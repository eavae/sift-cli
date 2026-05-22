pub mod announce;
pub mod financial_render;
pub mod render;
pub mod tabular;

pub use render::{render, RenderRow};

use crate::error::SiftError;

/// Convert a [`std::io::Error`] into [`SiftError::Internal`] with a
/// consistent prefix. Shared by every output module so `io: …`
/// messages look the same everywhere.
pub(crate) fn io_err(e: std::io::Error) -> SiftError {
    SiftError::Internal(format!("io: {e}"))
}

/// Output format. `Table` is the default but is **not** a clap value
/// — `--format table` is rejected by [`crate::cli::parse_user_format`]
/// with a hint pointing back to "omit the flag for the default table".
///
/// `Json` emits **NDJSON** (one JSON object per line, no enclosing
/// array) so downstream consumers can stream-parse with `jq -c` or
/// `serde_json::Deserializer::from_reader`. The user-visible name is
/// just `json` because that's what `--format json` says; the
/// "newline-delimited" detail is an internal property of the writer,
/// not something the user-facing CLI should advertise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Table,
    Tsv,
    Json,
}
