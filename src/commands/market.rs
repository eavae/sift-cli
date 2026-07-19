//! View layer for `sift market` — whole-market业绩报表 query.
//!
//! Fetch a report-date snapshot (`fetch::market`), best-effort ingest
//! the whole thing into the fact store (`service::facts`), then print a
//! window narrowed by `--where` / `--sort` / `--limit` / `--market`.
//! Narrowing affects the printout only — ingest always covers the full
//! snapshot. Friendly-name screening (roe, revenue …) is left to
//! `sift sql` over `v_facts`; `--where` here speaks raw EM columns.

use std::io::Write;

use time::OffsetDateTime;

use crate::app::AppContext;
use crate::domain::period::most_recent_filed;
use crate::domain::Period;
use crate::error::SiftError;
use crate::fetch::market::load_snapshot;
use crate::output::{io_err, query::render_query, Format};
use crate::service::facts;
use crate::sources::eastmoney_screen::MarketRow;

/// Default printed metric columns when the user names none.
const DEFAULT_COLS: [&str; 4] = [
    "TOTAL_OPERATE_INCOME",
    "PARENT_NETPROFIT",
    "WEIGHTAVG_ROE",
    "BASIC_EPS",
];

#[derive(clap::Args, Debug)]
pub struct MarketArgs {
    /// Report period (`2024A` / `2024Q3` / `YYYY-MM-DD`). Defaults to
    /// the most-recent filed period.
    #[arg(long)]
    pub period: Option<String>,
    /// Filter (printout only): `METRIC OP VALUE`, e.g.
    /// `WEIGHTAVG_ROE>15`. Repeatable; conditions are AND-ed. METRIC is
    /// a raw EM column name.
    #[arg(long = "where", value_name = "EXPR")]
    pub wheres: Vec<String>,
    /// Sort by this raw EM column (rows missing it sort last).
    #[arg(long)]
    pub sort: Option<String>,
    /// Sort descending (with `--sort`).
    #[arg(long)]
    pub desc: bool,
    /// Max rows to print. Ingest still covers the whole snapshot.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Narrow the printout to a board: main / star / gem / bj (default:
    /// all A-share).
    #[arg(long)]
    pub market: Option<String>,
    /// Extra raw EM columns to print (beyond the default headline set).
    #[arg(long = "show", value_name = "COL")]
    pub show: Vec<String>,
    /// Bypass the snapshot cache and refetch from upstream.
    #[arg(long)]
    pub no_cache: bool,
    /// Do not ingest the snapshot into the local fact store.
    #[arg(long)]
    pub no_ingest: bool,
}

#[derive(Clone, Copy)]
enum Op {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

struct Cond {
    metric: String,
    op: Op,
    val: f64,
}

pub fn run(args: MarketArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    let period = match &args.period {
        Some(s) => Period::parse(s)?,
        None => most_recent_filed(OffsetDateTime::now_utc().date()),
    };
    let conds = args
        .wheres
        .iter()
        .map(|s| parse_cond(s))
        .collect::<Result<Vec<_>, _>>()?;

    let rows = load_snapshot(ctx, period, args.no_cache)?;

    // Best-effort ingest of the *whole* snapshot (before narrowing).
    if !args.no_ingest {
        match period.period_type() {
            Some(pt) => match facts::ingest_market(ctx, &rows, period.end_date().year(), pt) {
                Ok(Some(_)) => {}
                Ok(None) => eprintln!("[warn] 事实库不可用，跳过入库（不影响输出）"),
                Err(e) => eprintln!("[warn] facts 入库失败（不影响输出）: {e}"),
            },
            None => eprintln!("[warn] 非标准报告期，跳过入库"),
        }
    }

    // Narrow the printout: board → where(AND) → sort → limit.
    let mut view: Vec<&MarketRow> = rows
        .iter()
        .filter(|r| board_matches(&r.code, args.market.as_deref()))
        .filter(|r| conds.iter().all(|c| cond_matches(r, c)))
        .collect();

    if let Some(sort) = &args.sort {
        view.sort_by(|a, b| {
            let av = a.metrics.get(sort);
            let bv = b.metrics.get(sort);
            // Missing values sort last regardless of direction.
            match (av, bv) {
                (Some(x), Some(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });
        if args.desc {
            view.reverse();
        }
    }
    view.truncate(args.limit);

    let cols = printed_columns(&args, &conds);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    // JSON is typed straight from the row (numbers stay numbers); table
    // and TSV go through the stringly `render_query`. The EM screen
    // schema is fixed, so no per-cell type inference is needed.
    if let Format::Json = fmt {
        return render_market_json(&mut handle, &view, &cols);
    }

    let header: Vec<String> = std::iter::once("code".to_string())
        .chain(std::iter::once("name".to_string()))
        .chain(cols.iter().cloned())
        .collect();
    let body: Vec<Vec<String>> = view
        .iter()
        .map(|r| {
            let mut row = Vec::with_capacity(header.len());
            row.push(r.code.clone());
            row.push(r.name.clone());
            for c in &cols {
                row.push(r.metrics.get(c).map(fmt_num).unwrap_or_default());
            }
            row
        })
        .collect();

    render_query(&mut handle, fmt, &header, &body)
}

/// Typed NDJSON for `sift market`: `code` / `name` as strings, every
/// printed metric as a JSON **number** (or `null` when this issuer
/// lacks that column). The EM screen schema is fixed (a constant column
/// list, all `f64`), so we serialize straight from the typed row rather
/// than round-tripping through the stringly `render_query` path — no
/// type inference, no leading-zero hazards.
fn render_market_json<W: Write>(
    out: &mut W,
    view: &[&MarketRow],
    cols: &[String],
) -> Result<(), SiftError> {
    use serde_json::{Map, Value};
    for r in view {
        let mut obj = Map::with_capacity(cols.len() + 2);
        obj.insert("code".into(), Value::String(r.code.clone()));
        obj.insert("name".into(), Value::String(r.name.clone()));
        for c in cols {
            let v = r.metrics.get(c).map_or(Value::Null, |&n| json_number(n));
            obj.insert(c.clone(), v);
        }
        serde_json::to_writer(&mut *out, &Value::Object(obj))
            .map_err(|e| SiftError::Internal(format!("ndjson serialize: {e}")))?;
        out.write_all(b"\n").map_err(io_err)?;
    }
    Ok(())
}

/// `f64` → JSON number. Whole values in `i64` range serialize as
/// integers (`14523000000`, not `14523000000.0`); everything else stays
/// a float. Non-finite (NaN / ±inf) has no JSON number form → `null`.
fn json_number(n: f64) -> serde_json::Value {
    use serde_json::{Number, Value};
    if n.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&n) {
        Value::Number(Number::from(n as i64))
    } else {
        Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

/// Ordered, deduped metric columns to print: `--show`, then `--sort`,
/// then `--where` metrics; falling back to [`DEFAULT_COLS`] when the
/// user named none. Unknown column names are allowed (they simply
/// render empty) so the output is predictable.
fn printed_columns(args: &MarketArgs, conds: &[Cond]) -> Vec<String> {
    let mut want: Vec<&str> = Vec::new();
    want.extend(args.show.iter().map(String::as_str));
    if let Some(s) = &args.sort {
        want.push(s);
    }
    want.extend(conds.iter().map(|c| c.metric.as_str()));

    let mut cols: Vec<String> = Vec::new();
    for c in want {
        if !cols.iter().any(|x| x == c) {
            cols.push(c.to_string());
        }
    }
    if cols.is_empty() {
        cols = DEFAULT_COLS.iter().map(|s| s.to_string()).collect();
    }
    cols
}

/// Parse `METRIC OP VALUE` (longest operators first).
fn parse_cond(s: &str) -> Result<Cond, SiftError> {
    for (tok, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("!=", Op::Ne),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
    ] {
        if let Some(idx) = s.find(tok) {
            let metric = s[..idx].trim().to_string();
            let val = s[idx + tok.len()..].trim();
            if metric.is_empty() {
                break;
            }
            let val: f64 = val
                .parse()
                .map_err(|_| SiftError::Parse(format!("bad --where value in {s:?}")))?;
            return Ok(Cond { metric, op, val });
        }
    }
    Err(SiftError::Parse(format!(
        "bad --where {s:?}; expected METRIC OP VALUE (e.g. WEIGHTAVG_ROE>15)"
    )))
}

fn cond_matches(r: &MarketRow, c: &Cond) -> bool {
    let Some(&v) = r.metrics.get(&c.metric) else {
        return false; // missing metric never satisfies a filter
    };
    match c.op {
        Op::Gt => v > c.val,
        Op::Ge => v >= c.val,
        Op::Lt => v < c.val,
        Op::Le => v <= c.val,
        Op::Eq => v == c.val,
        Op::Ne => v != c.val,
    }
}

/// Board selector on the raw code prefix. `None` / unknown → keep all.
fn board_matches(code: &str, sel: Option<&str>) -> bool {
    let p3 = code.get(..3).unwrap_or("");
    match sel {
        None => true,
        Some("a-share") => true,
        Some("main") => matches!(p3, "600" | "601" | "603" | "605" | "000" | "001" | "002" | "003"),
        Some("star") => matches!(p3, "688" | "689"),
        Some("gem") => matches!(p3, "300" | "301"),
        Some("bj") => code.starts_with('8') || matches!(p3, "430" | "920"),
        Some(_) => true,
    }
}

fn fmt_num(v: &f64) -> String {
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cond_handles_all_operators() {
        assert!(matches!(parse_cond("WEIGHTAVG_ROE>15").unwrap().op, Op::Gt));
        assert!(matches!(parse_cond("BPS>=1.5").unwrap().op, Op::Ge));
        assert!(matches!(parse_cond("XSMLL<=30").unwrap().op, Op::Le));
        assert!(matches!(parse_cond("X!=0").unwrap().op, Op::Ne));
        let c = parse_cond("WEIGHTAVG_ROE >= 15.5").unwrap();
        assert_eq!(c.metric, "WEIGHTAVG_ROE");
        assert_eq!(c.val, 15.5);
        assert!(parse_cond("garbage").is_err());
    }

    #[test]
    fn board_matches_by_prefix() {
        assert!(board_matches("600519", Some("main")));
        assert!(board_matches("688981", Some("star")));
        assert!(!board_matches("600519", Some("star")));
        assert!(board_matches("300750", Some("gem")));
        assert!(board_matches("830799", Some("bj")));
        assert!(board_matches("600519", None));
        assert!(board_matches("600519", Some("a-share")));
    }

    #[test]
    fn default_columns_used_when_none_named() {
        let args = MarketArgs {
            period: None,
            wheres: vec![],
            sort: None,
            desc: false,
            limit: 50,
            market: None,
            show: vec![],
            no_cache: false,
            no_ingest: false,
        };
        assert_eq!(printed_columns(&args, &[]), DEFAULT_COLS);
    }

    #[test]
    fn na_and_amount_cols_are_disjoint() {
        use crate::sources::eastmoney_screen::{AMOUNT_COLS, NA_COLS};
        // Guards against a copy-paste duplicate across the two lists.
        for a in AMOUNT_COLS {
            assert!(!NA_COLS.contains(&a), "{a} in both lists");
        }
    }

    #[test]
    fn json_number_ints_have_no_fraction_floats_kept() {
        use serde_json::Value;
        assert_eq!(json_number(14523000000.0), Value::from(14523000000i64));
        assert_eq!(json_number(2.83), Value::from(2.83));
        assert_eq!(json_number(-49478429211.96), Value::from(-49478429211.96));
        // Non-finite has no JSON number form.
        assert_eq!(json_number(f64::NAN), Value::Null);
        assert_eq!(json_number(f64::INFINITY), Value::Null);
    }

    #[test]
    fn market_json_types_metrics_as_numbers_code_as_string() {
        let mut metrics = std::collections::HashMap::new();
        metrics.insert("WEIGHTAVG_ROE".to_string(), 15.5);
        metrics.insert("PARENT_NETPROFIT".to_string(), 100.0);
        let row = MarketRow {
            code: "000001".into(),
            name: "平安银行".into(),
            board_name: None,
            notice_date: None,
            metrics,
        };
        let cols = vec![
            "WEIGHTAVG_ROE".to_string(),
            "PARENT_NETPROFIT".to_string(),
            "ABSENT".to_string(),
        ];
        let mut buf = Vec::new();
        render_market_json(&mut buf, &[&row], &cols).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        // Zero-padded code stays a string, never coerced to a number.
        assert_eq!(v["code"], serde_json::Value::from("000001"));
        assert_eq!(v["name"], serde_json::Value::from("平安银行"));
        assert_eq!(v["WEIGHTAVG_ROE"], serde_json::Value::from(15.5));
        assert_eq!(v["PARENT_NETPROFIT"], serde_json::Value::from(100i64));
        // A column this issuer lacks → null, not "".
        assert_eq!(v["ABSENT"], serde_json::Value::Null);
    }
}
