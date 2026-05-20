mod cache;
mod cli;
mod commands;
mod domain;
mod error;
mod http;
mod output;
mod sources;

use std::sync::Arc;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::error::SiftError;

fn main() {
    // clap's own argument errors (unknown flag, `--format table`, etc.)
    // are written to stderr and exited with code 2 by `parse()`; they do
    // **not** flow through `SiftError`.
    let cli = Cli::parse();
    let fmt = output::Format::from_user(cli.format);

    let result: Result<(), SiftError> = match cli.command {
        Command::Search(args) => commands::search::run(args, fmt),
        Command::Financials { cmd } => run_financials(cmd, fmt),
        Command::Announce { cmd } => commands::announce::run(cmd, fmt),
    };

    if let Err(e) = result {
        eprintln!("sift: {e}");
        std::process::exit(e.exit_code());
    }
}

/// Wire up the F2 source registry + cache once, then dispatch the
/// chosen `financials` subcommand.
fn run_financials(
    cmd: crate::commands::financials::FinancialsCmd,
    fmt: output::Format,
) -> Result<(), SiftError> {
    // Init the global source registry once per process. `init` panics
    // on a second call, which is the right behaviour for a binary
    // entry point.
    static REGISTRY_ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    REGISTRY_ONCE.get_or_init(|| {
        sources::financial_source::init(vec![
            sources::eastmoney_financials::build(),
            sources::sina_financials::build(),
        ]);
    });

    // Open the financial cache (best-effort; fall back to no-cache on failure).
    let cache = match cache::cache_root() {
        Ok(root) => {
            let path = root.join("financials.duckdb");
            match cache::financials::FinancialCache::open(&path) {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    eprintln!("[warn] disabling cache: {e}");
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("[warn] disabling cache: {e}");
            None
        }
    };

    let ctx = sources::financial_source::Context {
        http: http::HttpClient::new(),
        cache,
    };
    commands::financials::run(cmd, &ctx, fmt)
}
