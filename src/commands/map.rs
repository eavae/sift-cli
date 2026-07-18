//! View layer for `sift map {set,ls,rm}` — the agent-maintained
//! `(source, raw_key) → std_key` mappings applied at query time by
//! `v_facts`. Args / stdin in, service out, render.

use clap::Subcommand;

use crate::app::AppContext;
use crate::commands::metric::{read_stdin_batch, summarize};
use crate::error::SiftError;
use crate::output::{query::render_query, Format};
use crate::service::metrics;

#[derive(Subcommand, Debug)]
pub enum MapCmd {
    /// Set one mapping (`--source S RAW_KEY STD_KEY`) or a stdin TSV batch.
    Set(MapSetArgs),
    /// List mappings as TSV (round-trips into `map set`).
    Ls(MapLsArgs),
    /// Delete one mapping.
    Rm(MapRmArgs),
}

#[derive(clap::Args, Debug)]
pub struct MapSetArgs {
    /// Source tag the raw key comes from (e.g. `eastmoney`). Required
    /// for the single-row form.
    #[arg(long)]
    pub source: Option<String>,
    /// Raw upstream key. Omit both positionals to read a
    /// `#source\traw_key\tstd_key` TSV batch from stdin.
    pub raw_key: Option<String>,
    /// Target standard key (must already exist via `metric add`).
    pub std_key: Option<String>,
    /// Batch mode: write valid rows, skip invalid ones.
    #[arg(long)]
    pub skip_invalid: bool,
}

#[derive(clap::Args, Debug)]
pub struct MapLsArgs {
    /// Only show mappings for this source.
    #[arg(long)]
    pub source: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct MapRmArgs {
    #[arg(long)]
    pub source: String,
    pub raw_key: String,
}

pub fn run(cmd: MapCmd, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    match cmd {
        MapCmd::Set(args) => run_set(args, ctx),
        MapCmd::Ls(args) => run_ls(args, ctx, fmt),
        MapCmd::Rm(args) => {
            let n = metrics::map_remove(ctx, &args.source, &args.raw_key)?;
            eprintln!("[info] deleted {n} mapping(s)");
            Ok(())
        }
    }
}

fn run_set(args: MapSetArgs, ctx: &AppContext) -> Result<(), SiftError> {
    match (args.raw_key.as_deref(), args.std_key.as_deref()) {
        (Some(raw_key), Some(std_key)) => {
            let source = args.source.as_deref().ok_or_else(|| {
                SiftError::Parse("--source is required for a single mapping".into())
            })?;
            let out = metrics::map_set_one(ctx, source, raw_key, std_key)?;
            summarize(&out, "mapping");
            Ok(())
        }
        (None, None) => {
            let batch = read_stdin_batch()?;
            let out = metrics::ingest_map_tsv(ctx, &batch, !args.skip_invalid)?;
            summarize(&out, "mapping");
            Ok(())
        }
        _ => Err(SiftError::Parse(
            "give both RAW_KEY and STD_KEY for a single mapping, or neither for a stdin batch".into(),
        )),
    }
}

fn run_ls(args: MapLsArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    let (cols, rows) = metrics::list_map(ctx, args.source.as_deref())?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_query(&mut handle, fmt, &cols, &rows)
}
