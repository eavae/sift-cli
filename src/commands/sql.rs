//! View layer for `sift sql` (read-only) and `sift _sql` (writable
//! escape hatch). Both just parse the one positional SQL string, call
//! the service, and render — no logic, no direct `store` access.

use crate::app::AppContext;
use crate::error::SiftError;
use crate::output::{query::render_query, Format};
use crate::service::facts::{self, SqlOutcome};

#[derive(clap::Args, Debug)]
pub struct SqlArgs {
    /// SQL to run. `sift sql` allows SELECT only; `sift _sql` allows
    /// any statement (see its help).
    #[arg(required = true)]
    pub query: String,
}

/// `sift sql` — read-only. Writes are rejected by the DuckDB
/// `READ_ONLY` connection.
pub fn run(args: SqlArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    let (cols, rows) = facts::query(ctx, &args.query)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_query(&mut handle, fmt, &cols, &rows)
}

/// `sift _sql` — writable escape hatch. SELECT-shaped statements print
/// their result set; DML/DDL print an affected-row count on stderr.
/// DuckDB still enforces CHECK / FK / NOT NULL.
pub fn run_write(args: SqlArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    match facts::execute(ctx, &args.query)? {
        SqlOutcome::Rows(cols, rows) => {
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            render_query(&mut handle, fmt, &cols, &rows)
        }
        SqlOutcome::Affected(n) => {
            eprintln!("[info] {n} row(s) affected");
            Ok(())
        }
    }
}
