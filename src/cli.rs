use clap::{Parser, Subcommand};

use crate::output::Format;

#[derive(Parser, Debug)]
#[command(
    name = "sift",
    version,
    about = "Pull CN A-share / HK stock data (reports, announcements, quotes, bars) and analyze it locally with SQL.",
    long_about = "Pull CN A-share / HK stock data — listings, reports, announcements, PDF extracts, quotes, bars — to stdout, \
                  and build a local financial fact store you can query with SQL.\n\n\
                  Output is Unix-friendly TSV / NDJSON by default; omit `--format` for a human-aligned table. \
                  All commands accept multiple symbols where it makes sense and degrade gracefully (per-symbol failures \
                  surface as `[warn]` lines on stderr while successful rows still reach stdout).\n\n\
                  `report` and `market` also feed a local DuckDB fact store at ~/.sift/facts.duckdb — query it with \
                  `sift sql`, curate it with `sift fact` / `metric` / `map`.",
    after_long_help = "Common tasks:\n  \
                       Find a symbol?             sift search 茅台\n  \
                       Pull a financial report?   sift report income 600519 --last 4 --unit yi\n  \
                       Browse announcements?      sift announce list 600519 --type 年报 --limit 5\n  \
                       PDF → Markdown?            sift extract 1219506510 --pages 1-20 --mode auto\n  \
                       Live quote / history?      sift quote 600519  |  sift bars 600519 --limit 30\n  \
                       Analyze locally with SQL?  sift report income 600519 --last 12   # ingests into the fact store\n                              \
                                                  sift sql \"SELECT period,value FROM v_facts WHERE symbol='600519.CN-A' AND raw_key='TOTAL_OPERATE_INCOME' ORDER BY period_end\"\n  \
                       Screen the whole market?   sift market --period 2024A --where 'WEIGHTAVG_ROE>15' --sort WEIGHTAVG_ROE --desc\n\n\
                       The fact store (~/.sift/facts.duckdb) is fed automatically by `report` and `market`; query it with \
                       `sift sql`, curate it with `sift fact` / `metric` / `map`.\n\n\
                       Run `sift <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output format: `tsv` | `json` (NDJSON — one object per line).
    /// Omit the flag for the default aligned table; `--format table`
    /// is rejected with a hint pointing back to that default.
    #[arg(long, global = true, value_parser = parse_user_format)]
    pub format: Option<Format>,

    #[command(subcommand)]
    pub command: Command,
}

/// Custom value parser for `--format`. Yields the internal
/// [`Format`] directly (no `UserFormat` indirection) and rejects
/// `--format table` with an actionable hint pointing back to the
/// "omit the flag" default behavior — a generic `[possible values:
/// tsv, json]` rejection would leave users wondering how to get the
/// table back.
///
/// Returning `Err(String)` from a clap `value_parser` triggers
/// clap's parse-error path → exit code 2, matching other clap-level
/// rejections.
pub fn parse_user_format(s: &str) -> Result<Format, String> {
    match s {
        "tsv" => Ok(Format::Tsv),
        "json" => Ok(Format::Json),
        "table" => {
            Err("table is the default — omit `--format` to get it".into())
        }
        other => Err(format!(
            "unknown format {other:?} — expected `tsv` or `json`"
        )),
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(
        about = "Fuzzy-search CN A-share + HK listings by code, name, or pinyin initials",
        after_long_help = "Examples:\n  \
                           sift search 茅台                                         # name substring\n  \
                           sift search 600 --limit 20                              # code prefix, expand result cap\n  \
                           sift search gzmt --limit 3 --no-cache                   # pinyin initials, bypass listing cache\n  \
                           sift search 银行 --format json | jq -r .code | xargs sift quote   # search → quote pipeline\n\n\
                           search resolves a name/pinyin/code to a symbol; every other command takes that symbol.\n\n\
                           See also:\n  \
                           sift report income 600519 --last 4    Financials for a hit\n  \
                           sift quote 600519                     Live price for a hit\n  \
                           sift announce list 600519             Filings for a hit"
    )]
    Search(SearchArgs),
    #[command(
        about = "Financial reports + key indicators (income / balance / cashflow / indicator / periods)",
        after_long_help = "Examples:\n  \
                           sift report income 600519 --last 4 --unit yi\n  \
                           sift report balance 600519 600036 --period 2024A,2023A --scope parent --format tsv\n  \
                           sift report indicator 600519 --start 2020 --end 2024 --annual --items ROE加权,EPS,毛利率\n  \
                           sift report cashflow 600519 --period 2024Q3 --source eastmoney    # pin upstream for repro\n  \
                           sift report periods 600519                                        # what's available?\n\n\
                           Fetched rows are ingested into the local fact store by default (raw values, best-effort; \
                           `--no-ingest` opts out) — so after a `report` you can analyze the same data with SQL.\n\n\
                           See also:\n  \
                           sift sql \"SELECT period,value FROM v_facts WHERE symbol='600519.CN-A' AND raw_key='TOTAL_OPERATE_INCOME' ORDER BY period_end\"\n                              \
                                                                          Query the rows this command just ingested\n  \
                           sift market --period 2024A                     Same data across the whole market, one period"
    )]
    Report {
        #[command(subcommand)]
        cmd: crate::commands::report::ReportCmd,
    },
    #[command(
        about = "Browse, fetch, and download CN A-share / HK announcements (list / show / download / types)",
        after_long_help = "Examples:\n  \
                           sift announce list 600519 --type 年报 --limit 5\n  \
                           sift announce list 600519 00700 --start 2024-01-01 --end 2024-06-30\n  \
                           sift announce list --start 2025-04-01 --end 2025-04-30 --keyword 减持 --limit 100\n  \
                           sift announce list 600519 --type 定期报告 --start 2023-01-01 --end 2025-12-31    # aggregate 4 sub-types\n  \
                           sift announce list 600519 --format json | sift announce download <id> -o ./pdfs\n\n\
                           announce deals in raw filing documents (PDFs). For parsed financial line items, use \
                           `report` instead; to turn a downloaded PDF into text, use `extract`.\n\n\
                           See also:\n  \
                           sift extract 1219506510 --pages 1-20 --mode auto    Turn a downloaded PDF into Markdown\n  \
                           sift report income 600519 --last 4                  Structured numbers instead of raw filings"
    )]
    Announce {
        #[command(subcommand)]
        cmd: crate::commands::announce::AnnounceCmd,
    },
    #[command(
        about = "Extract a PDF announcement (by id or local path) as Markdown",
        after_long_help = "Examples:\n  \
                           sift extract 1219506510 --pages 1-5                              # cached PDF, first 5 pages\n  \
                           sift extract ./report.pdf --pages 3,7,10-12 --mode auto          # local PDF, OCR-on-demand\n  \
                           sift extract 1219506510 --mode auto --pages 1-30 > report.md     # auto + redirect\n  \
                           sift announce download 1219506510 -o /tmp && sift extract 1219506510 --mode auto --pages 1-30\n\n\
                           Extracting by id needs the PDF cached first (`announce download <id>`); a local path \
                           works with no fetch. Output is prose/tables as Markdown — for structured financial \
                           figures use `report`, not text extraction.\n\n\
                           See also:\n  \
                           sift announce list 600519 --type 年报 --limit 5    Find the announcement id to extract\n  \
                           sift announce download 1219506510 -o /tmp          Cache the PDF before extract-by-id"
    )]
    Extract(crate::commands::extract::ExtractArgs),
    #[command(
        about = "Current-price snapshot for one or more symbols",
        after_long_help = "Examples:\n  \
                           sift quote 600519\n  \
                           sift quote 600519 00700 sh000001 --format tsv\n  \
                           sift search 银行 --limit 5 --format json | jq -r .code | xargs sift quote   # batch from search\n\n\
                           quote is a realtime snapshot — fetched fresh every call, never cached, and not written \
                           to the fact store (which holds report-period fundamentals, not prices).\n\n\
                           See also:\n  \
                           sift bars 600519 --limit 30    Historical price series instead of a single snapshot\n  \
                           sift search 银行               Resolve names to codes to feed quote"
    )]
    Quote(crate::commands::quote::QuoteArgs),
    #[command(
        about = "Historical OHLC bars (daily / weekly / monthly) for one or more symbols",
        after_long_help = "Examples:\n  \
                           sift bars 600519 --limit 30                                              # last 30 daily bars\n  \
                           sift bars 600519 00700 --period weekly --limit 52 --format tsv           # multi-symbol, 1 year weekly\n  \
                           sift bars 600519 --start 2024-01-01 --end 2024-12-31 --adjust pre        # explicit range, pre-adjusted\n  \
                           sift bars 600519 --period monthly --limit 24 --source eastmoney         # 2y monthly, EM upstream\n\n\
                           bars is price history (OHLC) — like `quote`, it is market data, not fundamentals, and \
                           does not feed the fact store. For financial statements over time use `report`.\n\n\
                           See also:\n  \
                           sift quote 600519                     Just the latest price\n  \
                           sift report income 600519 --last 12   Fundamentals over time (into the fact store)"
    )]
    Bars(crate::commands::bars::BarsArgs),
    #[command(
        about = "Query the local fact store (~/.sift/facts.duckdb); read-only unless --write",
        after_long_help = "Read-only by default. Examples:\n  \
                           sift sql \"SELECT symbol,value FROM v_facts WHERE key='roe' AND period='2024A' ORDER BY value DESC LIMIT 20\"\n  \
                           sift sql \"SELECT period_end,value FROM v_facts WHERE symbol='600519.CN-A' AND raw_key='TOTAL_OPERATE_INCOME' ORDER BY period_end\"\n\n\
                           --write is the escape hatch: run ANY statement (INSERT/UPDATE/DELETE/DDL). \
                           CHECK / foreign-key / NOT NULL are still enforced, so it can delete and fix \
                           but not insert invalid data; DDL (DROP/ALTER) is unrestricted. Dangerous:\n  \
                           sift sql --write \"DELETE FROM facts WHERE source='screen' AND fiscal_year<2015\"\n  \
                           sift sql --write \"UPDATE facts SET currency='CNY' WHERE currency IS NULL\"\n\n\
                           `sql` only sees what has been ingested. To fill the store first:\n\n\
                           See also:\n  \
                           sift report income 600519 --last 12    Fetch a report — auto-ingests into the store\n  \
                           sift market --period 2024A             Ingest a whole-market snapshot (source=screen)\n  \
                           sift metric add / sift map set         Standardize raw_key → std_key so v_facts.key is friendly"
    )]
    Sql(crate::commands::sql::SqlArgs),
    #[command(
        about = "Write facts into the local store: one via flags, or a #header TSV batch on stdin",
        after_long_help = "Examples:\n  \
                           sift fact set --symbol 600519.CN-A --period 2024A --key employee_comp --value 1.5e9\n  \
                           printf '#symbol\\tfiscal_year\\tperiod_type\\traw_key\\tvalue\\n600519.CN-A\\t2024\\tannual\\temployee_comp\\t1.5e9\\n' | sift fact set\n  \
                           sift fact rm --symbol 600519.CN-A --period 2024A --key employee_comp\n\n\
                           Values are stored raw (unscaled); period is split into fiscal_year + period_type. \
                           Omitted TSV columns default to source=manual, scope=na, qmode=na.\n\n\
                           See also:\n  \
                           sift sql \"SELECT * FROM v_facts WHERE source='manual'\"   Read back what you wrote\n  \
                           sift metric add employee_comp --unit-kind amount        Register a std_key for a custom fact"
    )]
    Fact {
        #[command(subcommand)]
        cmd: crate::commands::fact::FactCmd,
    },
    #[command(
        about = "Manage the standard-metric vocabulary (add / ls / rm)",
        after_long_help = "Examples:\n  \
                           sift metric add revenue --label 营业总收入 --unit-kind amount\n  \
                           printf '#std_key\\tlabel\\tunit_kind\\nroe\\t加权ROE\\tratio\\n' | sift metric add\n  \
                           sift metric ls --format tsv\n\n\
                           unit_kind is one of amount / ratio / per_share / shares / count / other. \
                           A metric must exist before any mapping can point at it (`map set` is strict).\n\n\
                           See also:\n  \
                           sift map set --source eastmoney TOTAL_OPERATE_INCOME revenue   Point a raw label at a std_key\n  \
                           sift sql \"SELECT DISTINCT raw_key FROM facts\"                   Find raw keys still needing a std_key"
    )]
    Metric {
        #[command(subcommand)]
        cmd: crate::commands::metric::MetricCmd,
    },
    #[command(
        about = "Manage raw_key → std_key mappings applied at query time (set / ls / rm)",
        after_long_help = "Examples:\n  \
                           sift map set --source eastmoney TOTAL_OPERATE_INCOME revenue\n  \
                           printf '#source\\traw_key\\tstd_key\\neastmoney\\tWEIGHTAVG_ROE\\troe\\n' | sift map set\n  \
                           sift map ls --source eastmoney --format tsv\n\n\
                           Mappings apply at query time via the v_facts.key column — adding one standardizes \
                           all matching history instantly, without rewriting facts. Strict: the std_key must \
                           already be registered.\n\n\
                           See also:\n  \
                           sift metric add revenue --label 营业总收入        Register the target std_key first\n  \
                           sift sql \"SELECT key,mapped FROM v_facts LIMIT 5\"   Confirm the mapping took effect"
    )]
    Map {
        #[command(subcommand)]
        cmd: crate::commands::map::MapCmd,
    },
    #[command(
        about = "Whole-market业绩报表 snapshot for one period (fetch → ingest → filter/print)",
        after_long_help = "Examples (quote --where so the shell does not read `>` as redirection):\n  \
                           sift market --period 2024A --limit 20\n  \
                           sift market --period 2024A --where 'WEIGHTAVG_ROE>15' --where 'XSMLL>30' --sort WEIGHTAVG_ROE --desc\n  \
                           sift market --period 2024A --market star --show TOTAL_OPERATE_INCOME,PARENT_NETPROFIT\n\n\
                           The whole snapshot is ingested into the fact store (source=screen), unaffected by \
                           --where/--limit (those narrow the printout only). `--where` speaks raw EM columns \
                           (WEIGHTAVG_ROE, TOTAL_OPERATE_INCOME, XSMLL, …); for friendly names (roe, revenue) \
                           map the columns and screen via `sift sql` over v_facts.\n\n\
                           See also:\n  \
                           sift sql \"SELECT symbol,name,value FROM v_facts WHERE key='roe' AND period='2024A' ORDER BY value DESC\"\n                              \
                                                                          Friendly-name screening after `map set … roe`\n  \
                           sift report income 600519 --last 8             Per-symbol detail once you have candidates"
    )]
    Market(crate::commands::market::MarketArgs),
}

#[derive(clap::Args, Debug)]
pub struct SearchArgs {
    /// Query: stock code, code prefix, Chinese name substring, or pinyin initials (e.g. `gzmt` for 贵州茅台)
    pub query: String,

    /// Maximum number of matches to return
    #[arg(long, default_value_t = 10)]
    pub limit: u32,

    /// Skip the local cache and force a fresh fetch of the listing
    #[arg(long)]
    pub no_cache: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_format_accepts_tsv_and_json() {
        assert_eq!(parse_user_format("tsv").unwrap(), Format::Tsv);
        assert_eq!(parse_user_format("json").unwrap(), Format::Json);
    }

    #[test]
    fn parse_user_format_rejects_table_with_omit_hint() {
        // The hint must mention both "default" and "omit" so any
        // future rephrase still steers the user to the right action.
        let err = parse_user_format("table").unwrap_err();
        assert!(err.contains("default"), "msg should explain why: {err}");
        assert!(err.contains("omit"), "msg should suggest action: {err}");
    }

    #[test]
    fn parse_user_format_rejects_unknown_listing_expected_values() {
        let err = parse_user_format("xml").unwrap_err();
        assert!(err.contains("xml"), "msg should echo the bad value: {err}");
        assert!(err.contains("tsv"), "msg should list tsv: {err}");
        assert!(err.contains("json"), "msg should list json: {err}");
    }
}
