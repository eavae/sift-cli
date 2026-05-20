use thiserror::Error;

/// The single error type for the whole CLI. Exit-code mapping lives on
/// [`SiftError::exit_code`]; clap's own argument errors do **not** flow
/// through here — `Cli::parse()` writes them to stderr and exits with 2.
#[derive(Debug, Error)]
pub enum SiftError {
    #[error("network error: {0}")]
    Network(String),
    #[error("no match for query {0:?}")]
    NotFound(String),
    #[error("internal: {0}")]
    Internal(String),
    /// Upstream / string-parse inconsistency (e.g. an unexpected Symbol
    /// shape coming back from a data source). Distinct from clap's
    /// argument errors, which never reach `SiftError`.
    #[error("parse error: {0}")]
    Parse(String),
    /// Filesystem failure (cache directory creation, atomic write,
    /// missing `$HOME`, etc.). Distinct from `Network` so that local
    /// disk problems are not blamed on cninfo.
    #[error("io error: {0}")]
    Io(String),
}

impl SiftError {
    /// One-to-one with the exit-code table in the F1 README.
    pub fn exit_code(&self) -> i32 {
        match self {
            SiftError::Internal(_) | SiftError::Parse(_) | SiftError::Io(_) => 1,
            SiftError::Network(_) => 3,
            SiftError::NotFound(_) => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_matches_readme_table() {
        assert_eq!(SiftError::Internal("x".into()).exit_code(), 1);
        assert_eq!(SiftError::Parse("x".into()).exit_code(), 1);
        assert_eq!(SiftError::Io("x".into()).exit_code(), 1);
        assert_eq!(SiftError::Network("x".into()).exit_code(), 3);
        assert_eq!(SiftError::NotFound("x".into()).exit_code(), 4);
    }

    #[test]
    fn parse_display() {
        assert_eq!(
            SiftError::Parse("bad symbol".into()).to_string(),
            "parse error: bad symbol"
        );
    }
}
