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
    /// Every applicable source either errored or panicked. Carries one
    /// `(name, message)` per source so the user can see which upstream
    /// broke and how. Exit code 3 (network category).
    ///
    /// Constructed by [`crate::sources::financial_source::dispatch`]
    /// once Story 05 wires it through; until then this variant is only
    /// touched by the dispatcher's unit tests.
    #[allow(dead_code)]
    #[error("all sources failed:\n{}", format_source_failures(.0))]
    AllSourcesFailed(Vec<(String, String)>),
    /// No registered source supports this query (e.g. `--scope parent`
    /// on a non-A-share market). Distinct from `AllSourcesFailed` so
    /// the user knows the request was never even attempted.
    #[allow(dead_code)]
    #[error("no applicable source for {0}")]
    NoApplicableSource(String),
}

fn format_source_failures(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(name, msg)| format!("  {name}: {msg}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl SiftError {
    /// One-to-one with the exit-code table in the F1 README.
    pub fn exit_code(&self) -> i32 {
        match self {
            SiftError::Internal(_)
            | SiftError::Parse(_)
            | SiftError::Io(_)
            | SiftError::NoApplicableSource(_) => 1,
            SiftError::Network(_) | SiftError::AllSourcesFailed(_) => 3,
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
        assert_eq!(SiftError::NoApplicableSource("x".into()).exit_code(), 1);
        assert_eq!(SiftError::Network("x".into()).exit_code(), 3);
        assert_eq!(
            SiftError::AllSourcesFailed(vec![("a".into(), "x".into())]).exit_code(),
            3
        );
        assert_eq!(SiftError::NotFound("x".into()).exit_code(), 4);
    }

    #[test]
    fn all_sources_failed_display_includes_every_pair() {
        let e = SiftError::AllSourcesFailed(vec![
            ("eastmoney".into(), "HTTP 503".into()),
            ("sina".into(), "timeout".into()),
        ]);
        let s = e.to_string();
        assert!(s.contains("eastmoney"), "got: {s}");
        assert!(s.contains("HTTP 503"), "got: {s}");
        assert!(s.contains("sina"), "got: {s}");
        assert!(s.contains("timeout"), "got: {s}");
    }

    #[test]
    fn parse_display() {
        assert_eq!(
            SiftError::Parse("bad symbol".into()).to_string(),
            "parse error: bad symbol"
        );
    }
}
