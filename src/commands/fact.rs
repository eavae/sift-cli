//! View layer for `sift fact {set,rm}`. Reads args / stdin TSV, calls
//! `service::facts`, prints a short summary on stderr. No domain logic
//! and no direct `store` access here.

use std::io::{IsTerminal, Read};

use clap::Subcommand;

use crate::app::AppContext;
use crate::error::SiftError;
use crate::service::facts::{self, BatchOutcome, FactInput, FactRef};

#[derive(Subcommand, Debug)]
pub enum FactCmd {
    /// Upsert one fact (with `--symbol`) or a stdin TSV batch.
    Set(FactSetArgs),
    /// Delete one fact by its key.
    Rm(FactRmArgs),
}

#[derive(clap::Args, Debug)]
pub struct FactSetArgs {
    /// Symbol (`600519`, `sh600519`, `600519.CN-A`, `00700.hk`).
    /// Omit to read a `#`-header TSV batch from stdin.
    #[arg(long)]
    pub symbol: Option<String>,
    /// Period literal (`2024A` / `2024Q3` / `2024-09-30`). Required
    /// with `--symbol`.
    #[arg(long)]
    pub period: Option<String>,
    /// Raw metric key (stored verbatim; map to a std_key later).
    #[arg(long)]
    pub key: Option<String>,
    /// Numeric value (raw, unscaled). Accepts `1.5e9` etc.
    #[arg(long)]
    pub value: Option<String>,
    /// Source tag. Defaults to `manual`.
    #[arg(long, default_value = "manual")]
    pub source: String,
    /// Accumulation mode: cumulative / single / point / na.
    #[arg(long, default_value = "na")]
    pub qmode: String,
    /// Consolidation scope: consolidated / parent / na.
    #[arg(long, default_value = "na")]
    pub scope: String,
    /// Currency code (optional).
    #[arg(long)]
    pub currency: Option<String>,
    /// Disclosure date `YYYY-MM-DD` (optional).
    #[arg(long)]
    pub publish_date: Option<String>,
    /// Batch mode: write valid rows and skip invalid ones instead of
    /// the default all-or-nothing rollback.
    #[arg(long)]
    pub skip_invalid: bool,
}

#[derive(clap::Args, Debug)]
pub struct FactRmArgs {
    #[arg(long)]
    pub symbol: String,
    #[arg(long)]
    pub period: String,
    #[arg(long)]
    pub key: String,
    #[arg(long, default_value = "manual")]
    pub source: String,
    #[arg(long, default_value = "na")]
    pub qmode: String,
    #[arg(long, default_value = "na")]
    pub scope: String,
}

pub fn run(cmd: FactCmd, ctx: &AppContext) -> Result<(), SiftError> {
    match cmd {
        FactCmd::Set(args) => run_set(args, ctx),
        FactCmd::Rm(args) => run_rm(args, ctx),
    }
}

fn run_set(args: FactSetArgs, ctx: &AppContext) -> Result<(), SiftError> {
    if let Some(symbol) = args.symbol.as_deref() {
        let period = args.period.as_deref().ok_or_else(|| {
            SiftError::Parse("--period is required with --symbol".into())
        })?;
        let key = args.key.as_deref().ok_or_else(|| {
            SiftError::Parse("--key is required with --symbol".into())
        })?;
        let value = args.value.as_deref().ok_or_else(|| {
            SiftError::Parse("--value is required with --symbol".into())
        })?;
        let out = facts::set_one(
            ctx,
            &FactInput {
                symbol,
                period,
                key,
                value,
                source: &args.source,
                qmode: &args.qmode,
                scope: &args.scope,
                currency: args.currency.as_deref(),
                publish_date: args.publish_date.as_deref(),
            },
        )?;
        summarize(&out);
        return Ok(());
    }

    // Batch: read TSV from stdin (must not be a TTY).
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(SiftError::Parse(
            "provide --symbol for a single fact, or pipe a #header TSV batch on stdin".into(),
        ));
    }
    let mut buf = String::new();
    stdin
        .lock()
        .read_to_string(&mut buf)
        .map_err(|e| SiftError::Io(format!("read stdin: {e}")))?;
    let out = facts::ingest_tsv(ctx, &buf, !args.skip_invalid)?;
    summarize(&out);
    Ok(())
}

fn run_rm(args: FactRmArgs, ctx: &AppContext) -> Result<(), SiftError> {
    let n = facts::remove(
        ctx,
        &FactRef {
            symbol: &args.symbol,
            period: &args.period,
            key: &args.key,
            source: &args.source,
            qmode: &args.qmode,
            scope: &args.scope,
        },
    )?;
    eprintln!("[info] deleted {n} fact(s)");
    Ok(())
}

fn summarize(out: &BatchOutcome) {
    eprintln!("[info] wrote {} fact(s)", out.written);
    for (line, why) in &out.skipped {
        eprintln!("[warn] skipped row {line}: {why}");
    }
}
