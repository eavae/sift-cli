//! View layer for `sift sql`. Read-only by default; `--write` opts
//! into the writable escape hatch. Parses the one positional SQL
//! string, calls the service, renders — no logic, no direct `store`
//! access.

use crate::app::AppContext;
use crate::error::SiftError;
use crate::output::{query::render_query, Format};
use crate::service::facts::{self, SqlOutcome};

#[derive(clap::Args, Debug)]
pub struct SqlArgs {
    /// SQL to run. Read-only unless `--write` is given.
    #[arg(required = true)]
    pub query: String,
    /// Escape hatch: allow ANY statement (INSERT/UPDATE/DELETE/DDL),
    /// not just SELECT. CHECK / foreign-key / NOT NULL constraints are
    /// still enforced by DuckDB, so this can delete and fix rows but
    /// cannot insert invalid data; DDL (DROP/ALTER) is unrestricted.
    /// Dangerous — use it to repair the store, not for routine reads.
    #[arg(long)]
    pub write: bool,
}

/// `sift sql` — read-only by default; `--write` is the writable escape
/// hatch. Read path: writes are rejected by the DuckDB `READ_ONLY`
/// connection. Write path: SELECT-shaped statements print their result
/// set; DML/DDL print an affected-row count on stderr.
pub fn run(args: SqlArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    if !args.write {
        let (cols, rows) = facts::query(ctx, &args.query)?;
        return render(fmt, &cols, &rows);
    }
    match facts::execute(ctx, &args.query)? {
        SqlOutcome::Rows(cols, rows) => render(fmt, &cols, &rows),
        SqlOutcome::Affected(n) => {
            eprintln!("[info] {n} row(s) affected");
            Ok(())
        }
    }
}

fn render(fmt: Format, cols: &[String], rows: &[Vec<String>]) -> Result<(), SiftError> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_query(&mut handle, fmt, cols, rows)
}
