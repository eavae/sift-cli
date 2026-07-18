pub mod announce;
pub mod bars;
pub mod financial_render;
pub mod query;
pub mod render;
pub mod tabular;

pub use render::{render, RenderRow};

use crate::error::SiftError;

/// Convert a [`std::io::Error`] into the matching [`SiftError`]
/// variant. EPIPE (downstream closed the pipe, e.g. `| head`) maps
/// to [`SiftError::BrokenPipe`] so `main` can exit silently with 0;
/// everything else becomes `Internal` with a consistent `io: ` prefix
/// so messages look the same everywhere.
pub(crate) fn io_err(e: std::io::Error) -> SiftError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return SiftError::BrokenPipe;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_err_maps_broken_pipe_to_dedicated_variant() {
        let e = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Broken pipe");
        assert!(matches!(io_err(e), SiftError::BrokenPipe));
    }

    #[test]
    fn io_err_keeps_other_kinds_as_internal_with_prefix() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        match io_err(e) {
            SiftError::Internal(m) => assert!(m.starts_with("io: "), "msg: {m}"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
