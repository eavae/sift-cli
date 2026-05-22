mod app;
mod cache;
mod cli;
mod commands;
mod domain;
mod error;
mod fetch;
mod http;
mod output;
mod pdf;
mod sources;

use clap::Parser;

use crate::app::AppContext;
use crate::cache::file::FileCache;
use crate::cache::record::RecordCache;
use crate::cli::{Cli, Command};
use crate::error::SiftError;

fn main() {
    // clap's own argument errors (unknown flag, `--format table`, etc.)
    // are written to stderr and exited with code 2 by `parse()`; they do
    // **not** flow through `SiftError`.
    let cli = Cli::parse();
    let fmt = cli.format.unwrap_or(output::Format::Table);

    let result: Result<(), SiftError> = match cli.command {
        Command::Search(args) => run_search(args, fmt),
        Command::Report { cmd } => run_report(cmd, fmt),
        Command::Announce { cmd } => run_announce(cmd, fmt),
        Command::Extract(args) => run_extract(args, fmt),
    };

    if let Err(e) = result {
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

/// Open `<root>/records.duckdb` best-effort. F2 financials + F3
/// announce metadata both live in this one DuckDB file (different
/// `Kind` namespaces in `cache_entries`). `None` is the warn-and-
/// continue mode — every caller already guards on the Option.
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

/// Build the AppContext for `sift search` and dispatch. The listing
/// cache is filesystem-only (atomic JSON files under
/// `<root>/cninfo/`), so the ctx carries no DuckDB handle here.
fn run_search(args: cli::SearchArgs, fmt: output::Format) -> Result<(), SiftError> {
    let ctx = AppContext {
        http: http::HttpClient::new(),
        file_cache: open_file_cache(),
        records_cache: None,
    };
    commands::search::run(args, &ctx, fmt)
}

/// Build the AppContext + ReportContext for `sift report` and dispatch.
/// The F2 source list is constructed inline (EastMoney + sina) — there's
/// no hidden global registry. AppContext and the sources Vec each live
/// in their own slot on this stack frame; `ReportContext { app, sources }`
/// is a borrowed bundle for ergonomic single-argument dispatch.
fn run_report(
    cmd: crate::commands::report::ReportCmd,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let file_cache = open_file_cache();
    let records_cache = open_records_cache(file_cache.as_ref());

    let app = AppContext {
        http: http::HttpClient::new(),
        file_cache,
        records_cache,
    };
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
    let file_cache = open_file_cache();
    let records_cache = open_records_cache(file_cache.as_ref());

    let ctx = AppContext {
        http: http::HttpClient::new(),
        file_cache,
        records_cache,
    };
    commands::announce::run(cmd, &ctx, fmt)
}

/// Build the AppContext for `sift extract`. Both cache slots are
/// opened: F3's PDF cache lives in `file_cache` (read for the
/// `[info] cached` line, written when story-02/04 fetch a missing
/// PDF), and `records_cache` is reserved for stories 02/03's
/// metadata enrichment from stdin / announce cache.
fn run_extract(
    args: crate::commands::extract::ExtractArgs,
    fmt: output::Format,
) -> Result<(), SiftError> {
    let file_cache = open_file_cache();
    let records_cache = open_records_cache(file_cache.as_ref());

    let ctx = AppContext {
        http: http::HttpClient::new(),
        file_cache,
        records_cache,
    };
    commands::extract::run(args, &ctx, fmt)
}
