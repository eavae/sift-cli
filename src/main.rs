mod app;
mod cache;
mod cli;
mod commands;
mod db;
mod domain;
mod error;
mod fetch;
mod http;
mod output;
mod pdf;
mod sources;
mod service;
mod store;

use clap::{CommandFactory, FromArgMatches};

use crate::app::AppContext;
use crate::cache::file::FileCache;
use crate::cache::record::RecordCache;
use crate::cli::{Cli, Command};
use crate::error::SiftError;

fn main() {
    // clap's own argument errors (unknown flag, `--format table`, etc.)
    // are written to stderr and exited with code 2 by `parse()`; they do
    // **not** flow through `SiftError`.
    //
    // `--format` is `global = true` so it appears on every subcommand's
    // help — but `extract` emits Markdown and ignores the flag. To hide
    // it just on extract, we drive a `try_get_matches_from_mut` against
    // a no-arg input first (which forces clap to propagate global args
    // down into each subcommand's arg list) and *then* use
    // `mut_subcommand` + `mut_arg` to flip its `hide` bit. The
    // throwaway parse always fails and is discarded.
    let mut cmd = Cli::command();
    let _ = cmd.try_get_matches_from_mut(["sift"]);
    let cmd = cmd.mut_subcommand("extract", |c| c.mut_arg("format", |a| a.hide(true)));
    let matches = cmd.get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    let fmt = cli.format.unwrap_or(output::Format::Table);

    let result: Result<(), SiftError> = match cli.command {
        Command::Search(args) => run_search(args, fmt),
        Command::Report { cmd } => run_report(cmd, fmt),
        Command::Announce { cmd } => run_announce(cmd, fmt),
        Command::Extract(args) => run_extract(args, fmt),
        Command::Quote(args) => run_quote(args, fmt),
        Command::Bars(args) => run_bars(args, fmt),
        Command::Sql(args) => run_sql(args, fmt, false),
        Command::SqlWrite(args) => run_sql(args, fmt, true),
        Command::Fact { cmd } => run_fact(cmd, fmt),
        Command::Metric { cmd } => run_metric(cmd, fmt),
        Command::Map { cmd } => run_map(cmd, fmt),
        Command::Market(args) => run_market(args, fmt),
    };

    if let Err(e) = result {
        // Broken pipe (`sift … | head`) — the Unix convention is a
        // silent exit 0, no error banner.
        if matches!(e, SiftError::BrokenPipe) {
            std::process::exit(0);
        }
        eprintln!("sift: {e}");
        std::process::exit(e.exit_code());
    }
}

/// Open the filesystem cache rooted at `~/.sift/cache/`. `None` is
/// the warn-and-continue mode — every caller already guards on the
/// Option (fetch helpers return empty / propagate `SiftError::Io` as
/// needed). The only failure path here is `$HOME` resolution.
fn open_file_cache() -> Option<FileCache> {
    match cache::cache_root() {
        Ok(p) => Some(FileCache::open(p)),
        Err(e) => {
            eprintln!("[warn] disabling cache: {e}");
            None
        }
    }
}

/// Open `<root>/records.duckdb` best-effort. Financials and announce
/// metadata both live in this one DuckDB file (different `Kind`
/// namespaces in `cache_entries`). `None` is the warn-and-continue
/// mode — every caller already guards on the Option.
fn open_records_cache(file_cache: Option<&FileCache>) -> Option<RecordCache> {
    let files = file_cache?;
    match RecordCache::open_at(&files.path("records.duckdb")) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[warn] record cache unavailable: {e}; running without cache");
            None
        }
    }
}

/// Open `~/.sift/facts.duckdb` — the persistent, user-curated fact
/// store (distinct from the disposable `records.duckdb` cache). `None`
/// is the warn-and-continue mode: `$HOME` unresolvable or the DuckDB
/// open failed; every caller guards on the Option.
fn open_fact_store() -> Option<store::FactStore> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            eprintln!("[warn] disabling fact store: cannot resolve $HOME");
            return None;
        }
    };
    match store::FactStore::open(home.join(".sift").join("facts.duckdb")) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[warn] fact store unavailable: {e}");
            None
        }
    }
}

/// Build an [`AppContext`] with the HTTP client and optional caches /
/// fact store. `with_records` opens the DuckDB record cache (used by
/// `report`, `announce`, `extract`); `with_facts` opens the fact store
/// (used by `sql` / `fact`, and later `report`/`market` ingest).
fn build_app_context(with_records: bool, with_facts: bool) -> AppContext {
    let file_cache = open_file_cache();
    let records_cache = if with_records {
        open_records_cache(file_cache.as_ref())
    } else {
        None
    };
    let facts = if with_facts { open_fact_store() } else { None };
    AppContext {
        http: http::HttpClient::new(),
        file_cache,
        records_cache,
        facts,
    }
}

/// Build the AppContext for `sift search` and dispatch. The listing
/// cache is filesystem-only (atomic JSON files under
/// `<root>/cninfo/`), so the ctx carries no DuckDB handle here.
fn run_search(args: cli::SearchArgs, fmt: output::Format) -> Result<(), SiftError> {
    let ctx = build_app_context(false, false);
    commands::search::run(args, &ctx, fmt)
}

/// Build the AppContext + ReportContext for `sift report` and dispatch.
/// The source list is constructed inline (EastMoney + sina) — there's
/// no hidden global registry. AppContext and the sources Vec each live
/// in their own slot on this stack frame; `ReportContext { app, sources }`
/// is a borrowed bundle for ergonomic single-argument dispatch.
fn run_report(
    cmd: crate::commands::report::ReportCmd,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let app = build_app_context(true, true);
    let sources = vec![
        sources::eastmoney_financials::build(),
        sources::sina_financials::build(),
    ];
    let ctx = fetch::report::ReportContext {
        app: &app,
        sources: &sources,
    };
    commands::report::run(cmd, &ctx, fmt)
}

/// Build the AppContext for `sift announce` and dispatch the chosen
/// subcommand. Shares the same `records.duckdb` file with `report`.
fn run_announce(
    cmd: crate::commands::announce::AnnounceCmd,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let ctx = build_app_context(true, false);
    commands::announce::run(cmd, &ctx, fmt)
}

/// Build the AppContext for `sift extract`. Both cache slots are
/// opened: the announcement PDF cache lives in `file_cache` (read
/// for the `[info] cached` line, written when fetching a missing PDF),
/// and `records_cache` carries metadata enrichment from stdin /
/// announce cache.
fn run_extract(
    args: crate::commands::extract::ExtractArgs,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let ctx = build_app_context(true, false);
    commands::extract::run(args, &ctx, fmt)
}

/// Build a no-cache AppContext + QuoteContext for `sift quote` and
/// dispatch. Quote is a fresh-every-call data source per the F5
/// README "cache strategy" section: realtime snapshot data has
/// zero cache hit rate, so we do not even open the filesystem
/// cache root (`build_app_context(false)` would happily do that
/// for search). Both cache slots are explicitly `None`.
///
/// The source list is constructed inline mirroring `run_report`'s
/// pattern: one entry per registered upstream, stored in a `Vec`
/// owned by this stack frame and borrowed into the context bundle
/// passed down to the command.
fn run_quote(
    args: crate::commands::quote::QuoteArgs,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let app = AppContext {
        http: http::HttpClient::new(),
        file_cache: None,
        records_cache: None,
        facts: None,
    };
    let sources: Vec<Box<dyn sources::quote_source::QuoteSource>> = vec![
        sources::eastmoney::quote::build(),
    ];
    let ctx = fetch::quote::QuoteContext {
        app: &app,
        sources: &sources,
    };
    commands::quote::run(args, &ctx, fmt)
}

/// Build a no-cache AppContext + BarsContext for `sift bars` and
/// dispatch. Same caching rationale as `run_quote`. Tencent is
/// listed first because `--source tencent` is the default; EM is
/// available behind `--source eastmoney`.
fn run_bars(
    args: crate::commands::bars::BarsArgs,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let app = AppContext {
        http: http::HttpClient::new(),
        file_cache: None,
        records_cache: None,
        facts: None,
    };
    let sources: Vec<Box<dyn sources::bars_source::BarsSource>> = vec![
        sources::tencent::bars::build(),
        sources::eastmoney::bars::build(),
    ];
    let ctx = fetch::bars::BarsContext {
        app: &app,
        sources: &sources,
    };
    commands::bars::run(args, &ctx, fmt)
}

/// Build the AppContext for `sift sql` / `sift _sql` and dispatch.
/// Only the fact store is needed — no HTTP caches. `write == true`
/// routes to the `_sql` writable escape hatch.
fn run_sql(
    args: crate::commands::sql::SqlArgs,
    fmt: output::Format,
    write: bool,
) -> Result<(), SiftError> {
    let ctx = build_app_context(false, true);
    if write {
        commands::sql::run_write(args, &ctx, fmt)
    } else {
        commands::sql::run(args, &ctx, fmt)
    }
}

/// Build the AppContext for `sift fact {set,rm}` and dispatch. Only
/// the fact store is needed.
fn run_fact(
    cmd: crate::commands::fact::FactCmd,
    _fmt: output::Format,
) -> Result<(), SiftError> {
    let ctx = build_app_context(false, true);
    commands::fact::run(cmd, &ctx)
}

/// Build the AppContext for `sift metric {add,ls,rm}` and dispatch.
fn run_metric(
    cmd: crate::commands::metric::MetricCmd,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let ctx = build_app_context(false, true);
    commands::metric::run(cmd, &ctx, fmt)
}

/// Build the AppContext for `sift map {set,ls,rm}` and dispatch.
fn run_map(
    cmd: crate::commands::map::MapCmd,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let ctx = build_app_context(false, true);
    commands::map::run(cmd, &ctx, fmt)
}

/// Build the AppContext for `sift market` and dispatch. Needs both the
/// record cache (snapshot) and the fact store (ingest).
fn run_market(
    args: crate::commands::market::MarketArgs,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let ctx = build_app_context(true, true);
    commands::market::run(args, &ctx, fmt)
}
