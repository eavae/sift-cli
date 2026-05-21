//! `sift announce {types | list | show | download}`.
//!
//! Control layer: dispatches the subcommand, owns the per-command
//! `*Args` shapes and the orchestration that strings together input
//! parsing ([`input`]), cache I/O ([`crate::cache::announcements`])
//! and rendering ([`render`]). Every cache write and network call
//! lives in this file or in the source / cache layers it delegates
//! to — never in the input / render layers themselves.

mod input;
mod render;

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use time::Date;

use crate::cache::announcements;
use crate::cache::record::{Kind as CacheKind, RecordCache};
use crate::domain::announcement::AnnouncementRow;
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::output::tabular::render_tabular;
use crate::output::{self, Format};
use crate::sources::cninfo::{download_pdf, AnnouncementQuery, Announcements};

#[derive(Subcommand, Debug)]
pub enum AnnounceCmd {
    /// Print the 27 中文 `--type` values understood by `announce list`
    Types,
    /// List announcements for one or more symbols (Story 03)
    List(ListArgs),
    /// Show metadata for a single announcement (Story 03)
    Show(ShowArgs),
    /// Download PDFs to a local directory (Story 04)
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
    #[arg(long = "type", value_parser = input::type_value_parser())]
    pub r#type: Option<String>,

    /// Free-text keyword. Passed through to cninfo's `searchkey`.
    #[arg(long)]
    pub keyword: Option<String>,

    /// Inclusive start date (`YYYY-MM-DD`).
    #[arg(long, value_parser = input::parse_iso_date)]
    pub start: Option<Date>,

    /// Inclusive end date (`YYYY-MM-DD`).
    #[arg(long, value_parser = input::parse_iso_date)]
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

pub fn run(cmd: AnnounceCmd, fmt: Format) -> Result<(), SiftError> {
    match cmd {
        AnnounceCmd::Types => run_types(fmt),
        AnnounceCmd::List(args) => run_list(args, fmt),
        AnnounceCmd::Show(args) => run_show(args, fmt),
        AnnounceCmd::Download(args) => run_download(args),
    }
}

// ---------------------------------------------------------------------------
// `types`
// ---------------------------------------------------------------------------

fn run_types(fmt: Format) -> Result<(), SiftError> {
    let rows: Vec<render::TypeRow> = crate::domain::announcement::categories()
        .iter()
        .map(render::to_type_row)
        .collect();
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match fmt {
        // Default human-facing path: borderless `tabled` table. Two
        // characters of right padding give the same visual rhythm as
        // sift's other commands, which still hand-roll alignment.
        Format::Table => render::render_tabled(&mut handle, &rows),
        // TSV / NDJSON stay on the generic RenderRow pipeline; their
        // semantics (tab-separated body, one JSON object per line)
        // are unaffected by the table-only restyling.
        Format::Tsv | Format::Json => output::render(&mut handle, fmt, &rows),
    }
}

// ---------------------------------------------------------------------------
// `list`
// ---------------------------------------------------------------------------

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

fn run_list(args: ListArgs, fmt: Format) -> Result<(), SiftError> {
    validate_list_args(&args)?;
    let http = HttpClient::new();
    let symbols = input::resolve_all(&http, &args.symbols)?;
    let cat_keys = input::expand_categories(args.r#type.as_deref())?;
    let api = Announcements::new();

    let mut all: Vec<AnnouncementRow> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for cat in &cat_keys {
        let q = AnnouncementQuery {
            symbols: symbols.clone(),
            category: if cat.is_empty() { None } else { Some(cat.clone()) },
            keyword: args.keyword.clone(),
            start: args.start,
            end: args.end,
            limit: args.limit,
        };
        for row in api.query(&http, q)? {
            if seen.insert(row.id.clone()) {
                all.push(row);
            }
        }
    }
    all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
    all.truncate(args.limit as usize);

    // Side effect: cache every returned row by `announcementId` so
    // `sift announce show <id>` works zero-input later. Cache errors
    // log a [warn] inside `put` but never block the user-facing path.
    announcements::put_meta_rows(&all);

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match fmt {
        Format::Table => render::render_list_table(&mut handle, &all),
        // TSV goes through the project-wide tabular convention so the
        // header line is `#col1\tcol2\t…` and downstream pandas/awk
        // can treat it as a comment uniformly with every other sift
        // command.
        Format::Tsv => render_tabular(&mut handle, &render::AnnouncementListTsvView(&all)),
        Format::Json => output::render(&mut handle, fmt, &all),
    }
}

// ---------------------------------------------------------------------------
// `show`
// ---------------------------------------------------------------------------

fn run_show(args: ShowArgs, fmt: Format) -> Result<(), SiftError> {
    // Process-shared cache: opens the DuckDB file exactly once even
    // if the paginate fallback fires and writes hundreds of rows
    // across many page callbacks.
    let cache = RecordCache::shared();

    // Lookup order:
    //   1. Record cache — populated by any prior `list` / `download`.
    //   2. stdin NDJSON pipe — legacy contract, still honored. Hits
    //      from stdin get written back to cache as a side effect.
    //   3. Whole-market paginate-and-search — for users who typed an
    //      id without prior context. Walks cninfo newest-first,
    //      writing every row to cache as it goes, stopping at first
    //      hit (or `hasMore=false`).
    let row = lookup_announcement_row(cache, &args.id)?;

    let pdf_path = announcements::pdf_path(&args.id)?;
    let cached_str = format!(
        "{}  ({})",
        pdf_path.display(),
        if announcements::is_cached(&args.id) {
            "yes"
        } else {
            "no"
        }
    );

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let projected = render::ShowOutRow::from_parts(&row, &cached_str);
    match fmt {
        Format::Table => render::render_show_kv(&mut handle, &row, &cached_str),
        Format::Tsv => render_tabular(&mut handle, &render::ShowTsvView(&projected)),
        Format::Json => output::render(&mut handle, fmt, std::slice::from_ref(&projected)),
    }
}

/// Resolve an announcement row by id, walking the lookup chain
/// documented in [`run_show`]. Every code path writes the resolved
/// row back to `cache` so the next invocation hits in step 1.
fn lookup_announcement_row(
    cache: &RecordCache,
    id: &str,
) -> Result<AnnouncementRow, SiftError> {
    // 1. Cache hit.
    if let Some(entry) = cache.get(CacheKind::AnnounceMeta, &[], id) {
        if let Ok(row) = serde_json::from_slice::<AnnouncementRow>(&entry.body) {
            return Ok(row);
        }
        // Body decoded badly (schema drift, partial write). Fall
        // through to the network path; the fresh row will overwrite.
        eprintln!("[warn] cached row for {id} did not decode; refetching");
    }

    // 2. stdin NDJSON pipe. When the user explicitly piped a
    // stream, we respect it as authoritative context: a miss here
    // is a NotFound, *not* a license to escalate to a whole-market
    // scan. The escalation only happens when there's no pipe at all.
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let row = input::read_stdin_for_id(stdin.lock(), id)?;
        announcements::put_meta_row(&row);
        return Ok(row);
    }

    // 3. Whole-market paginate-and-search. May take a while; print
    // a one-line preface so the user understands the latency.
    eprintln!(
        "[info] announcement {id} not in cache; scanning cninfo (this may take a while; ctrl-c to abort)..."
    );
    let http = HttpClient::new();
    let mut found: Option<AnnouncementRow> = None;
    let mut scanned_pages = 0usize;
    Announcements::new().paginate_market(&http, |page_rows| {
        scanned_pages += 1;
        // Always cache every row we see — even if not the target,
        // future `show` calls benefit. Cost is negligible vs HTTP.
        announcements::put_meta_rows(page_rows);
        for row in page_rows {
            if row.id == id {
                found = Some(row.clone());
                return std::ops::ControlFlow::Break(());
            }
        }
        if scanned_pages.is_multiple_of(20) {
            eprintln!("[info] scanned {scanned_pages} pages, still searching for {id}…");
        }
        std::ops::ControlFlow::Continue(())
    })?;

    found.ok_or_else(|| SiftError::NotFound(id.into()))
}

// ---------------------------------------------------------------------------
// `download`
// ---------------------------------------------------------------------------

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

fn run_download(args: DownloadArgs) -> Result<(), SiftError> {
    let DownloadArgs { ids, output } = args;
    let total = ids.len();
    let http = HttpClient::new();

    // Read URL context from stdin once (NDJSON id→url). A TTY stdin
    // leaves the map empty; missing ids will then fall through to the
    // record cache + paginate fallback below, mirroring `show`'s
    // resolution chain.
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let (mut url_map, stdin_rows) = if stdin_is_tty {
        (std::collections::HashMap::new(), Vec::new())
    } else {
        let urls = input::read_stdin_url_map(stdin.lock())?;
        (urls.map, urls.rows)
    };
    // Persist any complete rows the stdin parser surfaced. The input
    // layer is side-effect-free by design; the cache write happens
    // here in the controller so `read_stdin_url_map` stays a pure
    // transform.
    announcements::put_meta_rows(&stdin_rows);

    // Round 2: for every id still missing a URL, look it up in the
    // record cache (populated by any prior `list` / `show`). This is
    // what makes `sift announce list 600519` then
    // `sift announce download <id> -o dist` work without a pipe.
    let cache = RecordCache::shared();
    for id in &ids {
        if url_map.contains_key(id) || announcements::is_cached(id) {
            continue;
        }
        if let Some(url) = lookup_url_from_record_cache(cache, id) {
            url_map.insert(id.clone(), url);
        }
    }

    // Round 3: still-missing ids on a TTY → paginate cninfo once,
    // collecting URLs for ALL of them in one whole-market scan. Mirrors
    // `show`'s paginate fallback but batches the search so a 5-id
    // download doesn't paginate 5 times.
    if stdin_is_tty {
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| {
                !url_map.contains_key(*id) && !announcements::is_cached(id)
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            paginate_for_missing_urls(&http, &missing, &mut url_map)?;
        }
    }

    let delay = polite_delay_ms();
    let mut failures: u32 = 0;
    for (i, id) in ids.iter().enumerate() {
        let idx = i + 1;
        if let Err(e) = download_one(&http, &output, idx, total, id, &url_map, stdin_is_tty) {
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
/// the URL already resolved upstream (stdin url_map, record cache, or
/// paginate fallback in `run_download`) → atomic write to PDF cache
/// → copy to `dst_dir`. Errors propagate up so the caller can tally
/// failures without short-circuiting the batch.
fn download_one(
    http: &HttpClient,
    dst_dir: &Path,
    idx: usize,
    total: usize,
    id: &str,
    url_map: &std::collections::HashMap<String, String>,
    stdin_is_tty: bool,
) -> Result<(), SiftError> {
    if announcements::is_cached(id) {
        let dst = announcements::copy_to(id, dst_dir)?;
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
    let cache_path = announcements::pdf_path(id)?;
    let size = download_pdf(http, url, &cache_path)?;
    let dst = announcements::copy_to(id, dst_dir)?;
    eprintln!(
        "[{idx}/{total}] {id}  fetched {} KB  → {}",
        size / 1024,
        dst.display()
    );
    Ok(())
}

/// Decode a cached `AnnouncementRow` for `id` and return its
/// non-empty `url`. Cache miss, decode error, or empty URL all map
/// to `None` so the caller falls through to the next resolution step.
fn lookup_url_from_record_cache(cache: &RecordCache, id: &str) -> Option<String> {
    let entry = cache.get(CacheKind::AnnounceMeta, &[], id)?;
    let row: AnnouncementRow = serde_json::from_slice(&entry.body).ok()?;
    if row.url.is_empty() {
        None
    } else {
        Some(row.url)
    }
}

/// Paginate cninfo's whole-market announcement stream once, collecting
/// URLs for every id in `missing` in a single pass. Stops early when
/// all targets are found; otherwise runs to `hasMore=false`. Caches
/// every row it sees as a side benefit — same shape as `show`'s
/// paginate fallback. Caller has already verified the targets aren't
/// in the PDF cache or `url_map`.
fn paginate_for_missing_urls(
    http: &HttpClient,
    missing: &[String],
    url_map: &mut std::collections::HashMap<String, String>,
) -> Result<(), SiftError> {
    let targets: HashSet<&str> = missing.iter().map(String::as_str).collect();
    eprintln!(
        "[info] {} id(s) not in cache; scanning cninfo for URL context (this may take a while; ctrl-c to abort)...",
        targets.len()
    );
    let mut scanned_pages = 0usize;
    let mut still_missing = targets.len();
    Announcements::new().paginate_market(http, |page_rows| {
        scanned_pages += 1;
        announcements::put_meta_rows(page_rows);
        for row in page_rows {
            if targets.contains(row.id.as_str())
                && !url_map.contains_key(&row.id)
                && !row.url.is_empty()
            {
                url_map.insert(row.id.clone(), row.url.clone());
                still_missing -= 1;
            }
        }
        if still_missing == 0 {
            return std::ops::ControlFlow::Break(());
        }
        if scanned_pages.is_multiple_of(20) {
            eprintln!(
                "[info] scanned {scanned_pages} pages, {still_missing} id(s) still missing…"
            );
        }
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        let err = run_list(default_list_args(), Format::Tsv).unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
        assert_eq!(err.exit_code(), 1);
    }
}
