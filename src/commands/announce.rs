//! `sift announce {types | list | show | download}`.
//!
//! Thin CLI layer: dispatches the subcommand, owns the per-command
//! `*Args` shapes, threads stdin context + format choice through
//! to the fetch + render layers. All cache I/O, network calls and
//! fallback policy live in [`crate::fetch::announce`]; rendering
//! lives in [`crate::output::announce`]; stdin parsing + clap value
//! parsers + dict / symbol resolvers are inlined in this file as
//! private helpers (announce-specific, never reused).
//!
//! This file deliberately does not import `crate::cache::record`,
//! `crate::cache::file`, or `crate::sources::cninfo::Announcements` —
//! every PDF / record / network operation goes through
//! [`AnnounceResolver`] (which holds the cache references off
//! [`crate::app::AppContext`]). The forbidden-imports grep in
//! project convention enforces this.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use time::format_description::well_known::Iso8601;
use time::Date;

use crate::app::AppContext;
use crate::domain::announcement::{categories, lookup, AnnouncementRow};
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::fetch::announce::{AnnounceResolver, StdinContext};
use crate::output::announce as announce_view;
use crate::output::tabular::render_tabular;
use crate::output::{self, Format};
use crate::fetch::search::resolve_org_id;
use crate::sources::cninfo::ResolvedSymbol;

// ===========================================================================
// CLI surface
// ===========================================================================

#[derive(Subcommand, Debug)]
pub enum AnnounceCmd {
    #[command(
        about = "Print the 中文 `--type` values understood by `announce list`",
        after_long_help = "Examples:\n  \
                           sift announce types\n  \
                           sift announce types --format tsv | awk -F'\\t' '!/^#/ {print $1}'    # bare 中文 list"
    )]
    Types,
    #[command(
        about = "List announcements for one or more symbols, or scan the whole market by date",
        after_long_help = "Examples:\n  \
                           sift announce list 600519 --type 年报 --limit 5\n  \
                           sift announce list 600519 00700 --start 2024-01-01 --end 2024-06-30\n  \
                           sift announce list --start 2025-04-01 --end 2025-04-30 --keyword 减持 --limit 100   # whole-market scan\n  \
                           sift announce list 600519 --type 定期报告 --start 2023-01-01 --end 2025-12-31      # aggregate 4 sub-types\n  \
                           sift announce list 600519 --type 年报 --format json | sift announce download <id> -o ./pdfs"
    )]
    List(ListArgs),
    #[command(
        about = "Show metadata for a single announcement (reads NDJSON from stdin for un-cached ids)",
        long_about = "Show metadata for a single announcement.\n\n\
                      cninfo has no by-id metadata endpoint, so for ids not already in the local record \
                      cache `show` reads NDJSON rows from stdin. The typical pipeline is to feed it \
                      `announce list --format json` output.",
        after_long_help = "Examples:\n  \
                           sift announce list 600519 --format json | sift announce show 1219506510\n  \
                           sift announce show 1219506510 --format json     # cache hit only; no stdin needed"
    )]
    Show(ShowArgs),
    #[command(
        about = "Download announcement PDFs to a local directory",
        long_about = "Download announcement PDFs to a local directory.\n\n\
                      URL context for un-cached ids comes from stdin NDJSON (same pipeline as `show`).",
        after_long_help = "Examples:\n  \
                           sift announce list 600519 --type 年报 --format json | sift announce download 1219506510 -o ./pdfs\n  \
                           sift announce list 600519 --type 年报 --limit 5 --format json | sift announce download $(... | jq -r .id) -o ./pdfs   # batch"
    )]
    Download(DownloadArgs),
}

/// `sift announce list [symbol]...`. Symbols are optional so callers
/// can scan the whole market via `--start` / `--end`; the missing-both
/// case is rejected at runtime (Internal, exit 1) with a hint.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Stock codes (`600519`, `sh600519`, `00700`, …). Leave empty when
    /// scanning the whole market by date.
    pub symbols: Vec<String>,

    /// Announcement type (中文). Use `sift announce types` for the
    /// full list. The aggregate value `定期报告` triggers four cninfo
    /// sub-queries (年报 / 半年报 / 一季报 / 三季报) and merges results.
    #[arg(long = "type", value_parser = type_value_parser(), hide_possible_values = true)]
    pub r#type: Option<String>,

    /// Free-text keyword. Passed through to cninfo's `searchkey`.
    #[arg(long)]
    pub keyword: Option<String>,

    /// Inclusive start date (`YYYY-MM-DD`).
    #[arg(long, value_parser = parse_iso_date)]
    pub start: Option<Date>,

    /// Inclusive end date (`YYYY-MM-DD`).
    #[arg(long, value_parser = parse_iso_date)]
    pub end: Option<Date>,

    /// Maximum rows to return. With `--type 定期报告` each sub-query
    /// caps at this value and the merged result is then truncated.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
}

/// `sift announce show <id>`. Reads NDJSON rows from stdin (the
/// expected pipe is `sift announce list … --format json | sift
/// announce show <id>`) because cninfo has no by-id metadata API.
#[derive(Args, Debug)]
pub struct ShowArgs {
    /// `announcementId`, e.g. `1219506510`.
    pub id: String,
}

/// `sift announce download <id>... -o <dir>`. `-o` is required: this
/// command lands files on disk and silently writing to `$PWD` would
/// surprise pipeline users. URL context for un-cached ids comes from
/// stdin NDJSON (same convention as [`ShowArgs`]).
#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// One or more `announcementId`s.
    #[arg(required = true)]
    pub ids: Vec<String>,
    /// Target directory (created if missing).
    #[arg(short = 'o', long = "output", required = true)]
    pub output: PathBuf,
}

pub fn run(cmd: AnnounceCmd, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    match cmd {
        AnnounceCmd::Types => run_types(fmt),
        AnnounceCmd::List(args) => run_list(args, ctx, fmt),
        AnnounceCmd::Show(args) => run_show(args, ctx, fmt),
        AnnounceCmd::Download(args) => run_download(args, ctx),
    }
}

// ===========================================================================
// Input helpers — clap value parsers
// ===========================================================================

/// Build the `--type` value parser at clap-Command construction time.
/// Using `PossibleValuesParser` gives us two things for free: the full
/// 27-entry list in `--help`, and a friendly `[possible values: …]`
/// footer on the rejection error (clap exit code 2).
fn type_value_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        categories()
            .iter()
            .map(|c| c.zh.as_str())
            .collect::<Vec<_>>(),
    )
}

/// `YYYY-MM-DD` parser shared by `--start` / `--end`. Returns the
/// error as a `String` so clap renders it inline with the offending
/// argument name (exit code 2).
fn parse_iso_date(s: &str) -> Result<Date, String> {
    Date::parse(s, &Iso8601::DATE).map_err(|e| format!("invalid date {s:?}: {e}"))
}

// ===========================================================================
// Input helpers — dict / symbol resolution
// ===========================================================================

/// Look up the cninfo `category_*` key for a 中文 type name. Returns
/// `None` for aggregate entries (they have no `category` field) and
/// for entries not in the dictionary; the caller is expected to have
/// pre-validated via [`type_value_parser`] so `None` here usually
/// signals an aggregate.
fn lookup_category(zh: &str) -> Option<String> {
    lookup(zh).and_then(|c| c.category.clone())
}

/// Translate the user's `--type` (already PossibleValuesParser-validated)
/// into one or more cninfo `category_*` keys. An aggregate entry fans
/// out into its constituents; absent `--type` becomes a single empty
/// key meaning "no category filter".
fn expand_categories(zh: Option<&str>) -> Result<Vec<String>, SiftError> {
    let Some(zh) = zh else {
        return Ok(vec![String::new()]);
    };
    // Validated by clap, so a None here would be a programmer error.
    let cat = lookup(zh).ok_or_else(|| {
        SiftError::Internal(format!("dictionary unexpectedly missing {zh:?}"))
    })?;
    if let Some(aggs) = &cat.aggregates {
        eprintln!("[info] {zh} = {} 次子查询", aggs.len());
        return aggs
            .iter()
            .map(|agg_zh| {
                lookup_category(agg_zh).ok_or_else(|| {
                    SiftError::Internal(format!("aggregate target {agg_zh:?} has no category"))
                })
            })
            .collect();
    }
    Ok(vec![cat.category.clone().unwrap_or_default()])
}

/// Resolve every raw user input to a `ResolvedSymbol` via the cninfo
/// search cache (3-step: cache → auto-fetch → MissingOrgId). Errors
/// out on the first unresolved code — the user almost certainly wants
/// to fix the typo before issuing the (possibly large) query.
fn resolve_all(ctx: &AppContext, raw: &[String]) -> Result<Vec<ResolvedSymbol>, SiftError> {
    raw.iter()
        .map(|u| {
            let sym = Symbol::parse(u)?;
            resolve_org_id(ctx, &sym.code)
        })
        .collect()
}

// ===========================================================================
// Input helpers — stdin NDJSON parsing
// ===========================================================================

/// Reconstruct an [`AnnouncementRow`] from a single NDJSON object.
/// Thin wrapper around `serde_json::from_value` — the heavy lifting
/// (ISO date parsing, forcing `format`/`source` to canonical
/// `&'static str` literals via `skip_deserializing`) lives in the
/// derived `Deserialize` impl on the struct. A missing required field
/// surfaces as `SiftError::Internal` so the user sees which key the
/// piped row was missing.
fn parse_announcement_value(v: &serde_json::Value) -> Result<AnnouncementRow, SiftError> {
    serde_json::from_value(v.clone())
        .map_err(|e| SiftError::Internal(format!("stdin row deserialize: {e}")))
}

/// Drain stdin NDJSON into a [`StdinContext`] for the fetch layer.
///
/// Two products from one pass over the pipe:
/// - `url_map` — id→url, the primary input for `download`'s URL
///   resolution. Empty / missing URL fields are skipped.
/// - `rows` — any line that also shapes a complete
///   [`AnnouncementRow`]. The resolver writes these back to the
///   record cache as a side benefit so a follow-up `show <id>` is
///   zero-network.
///
/// Lines that fail to parse, lack `id` / `url`, or carry an empty
/// `url` are silently skipped — keeping the command resilient to
/// logs / blank lines / `jq` projections that strip fields. The
/// fetch layer is the *only* writer to the cache; this function
/// stays a pure transform.
fn read_stdin_ctx<R: BufRead>(reader: R) -> Result<StdinContext, SiftError> {
    let mut url_map = HashMap::new();
    let mut rows: Vec<AnnouncementRow> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| SiftError::Internal(format!("stdin read: {e}")))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(trimmed) else {
            continue;
        };
        let id = v.get("id").and_then(|x| x.as_str());
        let url = v.get("url").and_then(|x| x.as_str());
        if let (Some(id), Some(url)) = (id, url) {
            if !url.is_empty() {
                url_map.insert(id.to_string(), url.to_string());
            }
        }
        if let Ok(row) = parse_announcement_value(&v) {
            rows.push(row);
        }
    }
    Ok(StdinContext { rows, url_map })
}

// ===========================================================================
// `types`
// ===========================================================================

fn run_types(fmt: Format) -> Result<(), SiftError> {
    let rows: Vec<announce_view::TypeRow> = categories()
        .iter()
        .map(announce_view::to_type_row)
        .collect();
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match fmt {
        // Default human-facing path: borderless `tabled` table. Two
        // characters of right padding give the same visual rhythm as
        // sift's other commands, which still hand-roll alignment.
        Format::Table => announce_view::render_tabled(&mut handle, &rows),
        // TSV / NDJSON stay on the generic RenderRow pipeline; their
        // semantics (tab-separated body, one JSON object per line)
        // are unaffected by the table-only restyling.
        Format::Tsv | Format::Json => output::render(&mut handle, fmt, &rows),
    }
}

// ===========================================================================
// `list`
// ===========================================================================

/// Validate the (post-clap) argument combination. Symbols + dates can
/// each be empty individually, but not both — that would broadcast to
/// every issuer for all time, which cninfo rejects and the user
/// almost certainly did not mean.
fn validate_list_args(args: &ListArgs) -> Result<(), SiftError> {
    if args.symbols.is_empty() && args.start.is_none() && args.end.is_none() {
        return Err(SiftError::Internal(
            "`announce list` needs <symbol>... or --start / --end (at least one)".into(),
        ));
    }
    Ok(())
}

fn run_list(args: ListArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    validate_list_args(&args)?;
    let symbols = resolve_all(ctx, &args.symbols)?;
    let cat_keys = expand_categories(args.r#type.as_deref())?;

    let resolver = AnnounceResolver::new(ctx);
    let all = resolver.list(symbols, &cat_keys, args.keyword, args.start, args.end, args.limit)?;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match fmt {
        Format::Table => announce_view::render_list_table(&mut handle, &all),
        // TSV goes through the project-wide tabular convention so the
        // header line is `#col1\tcol2\t…` and downstream pandas/awk
        // can treat it as a comment uniformly with every other sift
        // command.
        Format::Tsv => render_tabular(&mut handle, &announce_view::AnnouncementListTsvView(&all)),
        Format::Json => output::render(&mut handle, fmt, &all),
    }
}

// ===========================================================================
// `show`
// ===========================================================================

fn run_show(args: ShowArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let stdin_ctx = if stdin_is_tty {
        StdinContext::default()
    } else {
        read_stdin_ctx(stdin.lock())?
    };

    let resolver = AnnounceResolver::new(ctx);
    let row = resolver.resolve_row(&args.id, &stdin_ctx, stdin_is_tty)?;

    let cached_str = match resolver.pdf_path(&args.id) {
        Some(p) => format!(
            "{}  ({})",
            p.display(),
            if resolver.is_pdf_cached(&args.id) {
                "yes"
            } else {
                "no"
            }
        ),
        // File cache disabled (e.g. $HOME unresolved): show the
        // sentinel rather than a misleading relative path.
        None => "—  (no cache)".into(),
    };

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let projected = announce_view::ShowOutRow::from_parts(&row, &cached_str);
    match fmt {
        Format::Table => announce_view::render_show_kv(&mut handle, &row, &cached_str),
        Format::Tsv => render_tabular(&mut handle, &announce_view::ShowTsvView(&projected)),
        Format::Json => output::render(&mut handle, fmt, std::slice::from_ref(&projected)),
    }
}

// ===========================================================================
// `download`
// ===========================================================================

/// Default polite delay between successive downloads when the batch is
/// larger than [`POLITE_BATCH_THRESHOLD`]. Tunable via
/// `SIFT_DOWNLOAD_DELAY_MS` (test-only seam — not documented as a user
/// flag) so integration tests can drop it from 200 ms to a few ms.
const POLITE_DELAY_DEFAULT_MS: u64 = 200;

/// Batch size at which the polite delay kicks in. Single-digit batches
/// stay fast; only "spray cninfo with many requests" cases pay the
/// inter-request pause. The threshold matches Story 04 §3.
const POLITE_BATCH_THRESHOLD: usize = 4;

fn polite_delay_ms() -> u64 {
    std::env::var("SIFT_DOWNLOAD_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(POLITE_DELAY_DEFAULT_MS)
}

fn run_download(args: DownloadArgs, ctx: &AppContext) -> Result<(), SiftError> {
    let DownloadArgs { ids, output } = args;
    let total = ids.len();

    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let stdin_ctx = if stdin_is_tty {
        StdinContext::default()
    } else {
        read_stdin_ctx(stdin.lock())?
    };

    let resolver = AnnounceResolver::new(ctx);

    // Filter out ids that already have a PDF on disk — they don't need
    // URL resolution. Keeps the fetch layer free of file-system policy
    // and ensures a 5-id batch with 4 cached doesn't trigger a paginate
    // fallback for nothing.
    let needs_url: Vec<String> = ids
        .iter()
        .filter(|id| !resolver.is_pdf_cached(id))
        .cloned()
        .collect();

    let url_map = resolver.resolve_urls(&needs_url, &stdin_ctx, stdin_is_tty)?;

    let delay = polite_delay_ms();
    let mut failures: u32 = 0;
    for (i, id) in ids.iter().enumerate() {
        let idx = i + 1;
        if let Err(e) = download_one(&resolver, &output, idx, total, id, &url_map, stdin_is_tty) {
            eprintln!("[{idx}/{total}] {id} failed: {e}");
            failures += 1;
        }
        // Inter-request polite delay: only between requests, only when
        // the batch is large enough to warrant pacing.
        if idx < total && total > POLITE_BATCH_THRESHOLD && delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }

    if failures > 0 {
        Err(SiftError::Network(format!(
            "{failures} of {total} downloads failed"
        )))
    } else {
        Ok(())
    }
}

/// Resolve one id: PDF cache hit → copy; PDF cache miss → fetch via
/// the URL resolved upstream (stdin url_map, record cache, or
/// paginate fallback inside [`AnnounceResolver::resolve_urls`]) →
/// atomic write to PDF cache → copy to `dst_dir`.
fn download_one(
    resolver: &AnnounceResolver,
    dst_dir: &Path,
    idx: usize,
    total: usize,
    id: &str,
    url_map: &HashMap<String, String>,
    stdin_is_tty: bool,
) -> Result<(), SiftError> {
    if resolver.is_pdf_cached(id) {
        let dst = resolver.copy_pdf_to(id, dst_dir)?;
        eprintln!("[{idx}/{total}] {id}  cached  → {}", dst.display());
        return Ok(());
    }
    let url = url_map.get(id).ok_or_else(|| {
        if stdin_is_tty {
            SiftError::Internal(format!(
                "no url context for {id} after consulting record cache and a whole-market scan; \
                 try `sift announce list <symbol>` first or pipe `sift announce list … --format json` directly"
            ))
        } else {
            SiftError::Internal(format!(
                "stdin NDJSON did not include a row with id {id:?} (or its url field was empty)"
            ))
        }
    })?;
    let size = resolver.download_pdf(id, url)?;
    let dst = resolver.copy_pdf_to(id, dst_dir)?;
    eprintln!(
        "[{idx}/{total}] {id}  fetched {} KB  → {}",
        size / 1024,
        dst.display()
    );
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    fn default_list_args() -> ListArgs {
        ListArgs {
            symbols: vec![],
            r#type: None,
            keyword: None,
            start: None,
            end: None,
            limit: 20,
        }
    }

    // ---- arg validation -------------------------------------------------

    #[test]
    fn polite_delay_ms_reads_env_var_when_set() {
        // Using a temp env var requires single-threaded execution; this
        // test sets and unsets within the same call so global pollution
        // is short-lived. We tolerate the cross-test race because no
        // other test reads SIFT_DOWNLOAD_DELAY_MS.
        std::env::set_var("SIFT_DOWNLOAD_DELAY_MS", "7");
        assert_eq!(polite_delay_ms(), 7);
        std::env::remove_var("SIFT_DOWNLOAD_DELAY_MS");
        assert_eq!(polite_delay_ms(), POLITE_DELAY_DEFAULT_MS);
    }

    #[test]
    fn validate_list_args_rejects_no_symbols_and_no_dates() {
        let args = default_list_args();
        let err = validate_list_args(&args).unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("--start"));
    }

    #[test]
    fn validate_list_args_passes_with_only_symbols() {
        let args = ListArgs {
            symbols: vec!["600519".into()],
            ..default_list_args()
        };
        assert!(validate_list_args(&args).is_ok());
    }

    #[test]
    fn validate_list_args_passes_with_only_start() {
        let args = ListArgs {
            start: Some(d(2024, 4, 1)),
            ..default_list_args()
        };
        assert!(validate_list_args(&args).is_ok());
    }

    #[test]
    fn run_list_returns_internal_error_for_empty_inputs() {
        // `validate_list_args` is called before any HTTP — safe to
        // exercise the public entry point without a mock server.
        let ctx = AppContext::default();
        let err = run_list(default_list_args(), &ctx, Format::Tsv).unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
        assert_eq!(err.exit_code(), 1);
    }

    // ---- clap value parsers ---------------------------------------------

    #[test]
    fn parse_iso_date_accepts_yyyy_mm_dd() {
        let got = parse_iso_date("2024-04-03").unwrap();
        assert_eq!(got, d(2024, 4, 3));
    }

    #[test]
    fn parse_iso_date_rejects_garbage() {
        let err = parse_iso_date("not-a-date").unwrap_err();
        assert!(err.contains("invalid date"), "err: {err}");
    }

    // ---- dict lookups ---------------------------------------------------

    #[test]
    fn lookup_category_resolves_known_and_returns_none_for_unknown() {
        assert_eq!(
            lookup_category("年报").as_deref(),
            Some("category_ndbg_szsh")
        );
        assert!(lookup_category("不存在").is_none());
        // Aggregate entries have no `category` key — also `None`.
        assert!(lookup_category("定期报告").is_none());
    }

    #[test]
    fn expand_categories_none_yields_single_empty_no_filter() {
        assert_eq!(expand_categories(None).unwrap(), vec![String::new()]);
    }

    #[test]
    fn expand_categories_known_returns_single_key() {
        assert_eq!(
            expand_categories(Some("年报")).unwrap(),
            vec!["category_ndbg_szsh".to_string()]
        );
    }

    #[test]
    fn expand_categories_aggregate_fans_out_to_four_keys_in_readme_order() {
        let keys = expand_categories(Some("定期报告")).unwrap();
        assert_eq!(
            keys,
            vec![
                "category_ndbg_szsh".to_string(),  // 年报
                "category_bndbg_szsh".to_string(), // 半年报
                "category_yjdbg_szsh".to_string(), // 一季报
                "category_sjdbg_szsh".to_string(), // 三季报
            ]
        );
    }

    // ---- stdin parser ---------------------------------------------------

    #[test]
    fn read_stdin_ctx_collects_id_to_url_pairs() {
        let stdin = r#"{"id":"100","url":"http://x/100.PDF","title":"t"}
{"id":"101","url":"http://x/101.PDF"}
"#;
        let ctx = read_stdin_ctx(stdin.as_bytes()).unwrap();
        assert_eq!(ctx.url_map.len(), 2);
        assert_eq!(ctx.url_map["100"], "http://x/100.PDF");
        assert_eq!(ctx.url_map["101"], "http://x/101.PDF");
        // Neither line carried the full AnnouncementRow shape (missing
        // symbol/name/date/...), so no rows accumulate.
        assert!(ctx.rows.is_empty());
    }

    #[test]
    fn read_stdin_ctx_skips_rows_missing_url_or_id_or_malformed() {
        let stdin = "not json\n{\"id\":\"only-id\"}\n{\"url\":\"http://x/orphan.PDF\"}\n{\"id\":\"empty-url\",\"url\":\"\"}\n{\"id\":\"100\",\"url\":\"http://x/100.PDF\"}\n";
        let ctx = read_stdin_ctx(stdin.as_bytes()).unwrap();
        assert_eq!(ctx.url_map.len(), 1);
        assert_eq!(ctx.url_map["100"], "http://x/100.PDF");
    }

    #[test]
    fn read_stdin_ctx_extracts_full_rows_when_present() {
        let stdin = r#"{"id":"100","symbol":"600519.SH","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":12,"url":"http://u","source":"cninfo"}
{"id":"only-url","url":"http://y","title":"z"}
"#;
        let ctx = read_stdin_ctx(stdin.as_bytes()).unwrap();
        assert_eq!(ctx.url_map.len(), 2);
        // Only the first line deserialized into a full AnnouncementRow.
        assert_eq!(ctx.rows.len(), 1);
        assert_eq!(ctx.rows[0].id, "100");
        assert_eq!(ctx.rows[0].symbol, "600519.SH");
    }

    #[test]
    fn read_stdin_ctx_finds_row_by_id_among_many() {
        let stdin = r#"{"id":"100","symbol":"600519.SH","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":12,"url":"http://u","source":"cninfo"}
{"id":"101","symbol":"600519.SH","name":"y","date":"2024-04-04","type":"年报","title":"t2","format":"pdf","size_kb":34,"url":"http://v","source":"cninfo"}
"#;
        let ctx = read_stdin_ctx(stdin.as_bytes()).unwrap();
        // The resolver itself does the id lookup; the parser only
        // needs to expose both rows in `ctx.rows` for it to scan.
        let row = ctx.rows.iter().find(|r| r.id == "101").unwrap();
        assert_eq!(row.date, d(2024, 4, 4));
        assert_eq!(row.size_kb, 34);
    }

    #[test]
    fn read_stdin_ctx_skips_malformed_lines() {
        let stdin = "not json\n{\"id\":\"100\",\"symbol\":\"x\",\"name\":\"x\",\"date\":\"2024-04-03\",\"type\":\"年报\",\"title\":\"t\",\"format\":\"pdf\",\"size_kb\":0,\"url\":\"http://u\",\"source\":\"cninfo\"}\n";
        let ctx = read_stdin_ctx(stdin.as_bytes()).unwrap();
        assert_eq!(ctx.rows.len(), 1);
        assert_eq!(ctx.rows[0].id, "100");
        assert_eq!(ctx.url_map.len(), 1);
        assert_eq!(ctx.url_map["100"], "http://u");
    }
}
