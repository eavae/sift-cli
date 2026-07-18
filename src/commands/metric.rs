//! View layer for `sift metric {add,ls,rm}` — the controlled
//! vocabulary of standard metric keys. Args / stdin in, service out,
//! render. No domain logic, no direct `store` access.

use std::io::{IsTerminal, Read};

use clap::Subcommand;

use crate::app::AppContext;
use crate::error::SiftError;
use crate::output::{query::render_query, Format};
use crate::service::metrics::{self, BatchOutcome};

#[derive(Subcommand, Debug)]
pub enum MetricCmd {
    /// Register one metric (with `std_key`) or a stdin TSV batch.
    Add(MetricAddArgs),
    /// List registered metrics as TSV (round-trips into `metric add`).
    Ls,
    /// Delete a metric (refused while mappings reference it unless `--cascade`).
    Rm(MetricRmArgs),
}

#[derive(clap::Args, Debug)]
pub struct MetricAddArgs {
    /// Standard key (e.g. `revenue`). Omit to read a
    /// `#std_key\tlabel\tunit_kind` TSV batch from stdin.
    pub std_key: Option<String>,
    /// Human label (e.g. 营业总收入).
    #[arg(long)]
    pub label: Option<String>,
    /// One of amount / ratio / per_share / shares / count / other.
    #[arg(long = "unit-kind", default_value = "amount")]
    pub unit_kind: String,
    /// Batch mode: write valid rows, skip invalid ones.
    #[arg(long)]
    pub skip_invalid: bool,
}

#[derive(clap::Args, Debug)]
pub struct MetricRmArgs {
    pub std_key: String,
    /// Also delete any mappings that reference this metric.
    #[arg(long)]
    pub cascade: bool,
}

pub fn run(cmd: MetricCmd, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    match cmd {
        MetricCmd::Add(args) => run_add(args, ctx),
        MetricCmd::Ls => run_ls(ctx, fmt),
        MetricCmd::Rm(args) => {
            let n = metrics::metric_remove(ctx, &args.std_key, args.cascade)?;
            eprintln!("[info] deleted {n} metric(s)");
            Ok(())
        }
    }
}

fn run_add(args: MetricAddArgs, ctx: &AppContext) -> Result<(), SiftError> {
    if let Some(std_key) = args.std_key.as_deref() {
        let out = metrics::metric_add_one(ctx, std_key, args.label.as_deref(), &args.unit_kind)?;
        summarize(&out, "metric");
        return Ok(());
    }
    let batch = read_stdin_batch()?;
    let out = metrics::ingest_metrics_tsv(ctx, &batch, !args.skip_invalid)?;
    summarize(&out, "metric");
    Ok(())
}

fn run_ls(ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    let (cols, rows) = metrics::list_metrics(ctx)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_query(&mut handle, fmt, &cols, &rows)
}

/// Shared stdin-batch reader for `metric` / `map` add paths.
pub(crate) fn read_stdin_batch() -> Result<String, SiftError> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(SiftError::Parse(
            "provide the value inline, or pipe a #header TSV batch on stdin".into(),
        ));
    }
    let mut buf = String::new();
    stdin
        .lock()
        .read_to_string(&mut buf)
        .map_err(|e| SiftError::Io(format!("read stdin: {e}")))?;
    Ok(buf)
}

/// Shared batch summary line for `metric` / `map`.
pub(crate) fn summarize(out: &BatchOutcome, what: &str) {
    eprintln!("[info] wrote {} {what}(s)", out.written);
    for (line, why) in &out.skipped {
        eprintln!("[warn] skipped row {line}: {why}");
    }
}
