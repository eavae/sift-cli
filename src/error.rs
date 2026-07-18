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
    /// stdout's read end closed mid-write (EPIPE — e.g. `sift bars …
    /// | head -2`). Rust ignores SIGPIPE, so the write surfaces as an
    /// `io::Error` instead; `main` maps this variant to a **silent**
    /// exit 0, the Unix convention for pipe truncation.
    #[error("broken pipe")]
    BrokenPipe,
    /// Every applicable source either errored or panicked. Carries one
    /// `(name, message)` per source so the user can see which upstream
    /// broke and how. Exit code 3 (network category).
    #[error("all sources failed:\n{}", format_source_failures(.0))]
    AllSourcesFailed(Vec<(String, String)>),
    /// No registered source supports this query (e.g. `--scope parent`
    /// on a non-A-share market). Distinct from `AllSourcesFailed` so
    /// the user knows the request was never even attempted.
    #[error("no applicable source for {0}")]
    NoApplicableSource(String),
    /// User-supplied symbol is not in the cninfo listings cache (and
    /// a refresh did not find it either). Distinct from `Parse` (the
    /// shape of the input was fine) and `NotFound` (search miss) —
    /// this is "we know the cninfo universe and your code is not in
    /// it". Exit code 1 — the data source is healthy, the input just
    /// does not correspond to a known issuer.
    #[error("symbol {0:?} not in cninfo listings; check the code or wait for the list refresh")]
    MissingOrgId(String),
    /// `sift extract --mode fine` / `--mode auto` was invoked but
    /// no PaddleOCR credentials are configured. We raise this
    /// **before** any HTTP request so a misconfigured run doesn't
    /// waste a round-trip. Exit code 1 — same family as `Parse` /
    /// `Io` since the user can fix it locally; not `Network`
    /// because no request was attempted. Two ways to fix it,
    /// either token or OAuth — both pairs need to be set for the
    /// chosen mode (see `sources::paddleocr`).
    #[error(
        "PaddleOCR not configured. Set either (PADDLEOCR_API_BASE + PADDLEOCR_API_TOKEN) \
         or (PADDLEOCR_API_KEY + PADDLEOCR_SECRET_KEY) in the environment"
    )]
    OcrTokenMissing,
}

fn format_source_failures(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(name, msg)| format!("  {name}: {msg}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl SiftError {
    /// Exit-code mapping for the binary.
    pub fn exit_code(&self) -> i32 {
        match self {
            // Pipe truncation is a success-path exit (Unix convention);
            // `main` also skips printing the error for this variant.
            SiftError::BrokenPipe => 0,
            SiftError::Internal(_)
            | SiftError::Parse(_)
            | SiftError::Io(_)
            | SiftError::NoApplicableSource(_)
            | SiftError::MissingOrgId(_)
            | SiftError::OcrTokenMissing => 1,
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
        assert_eq!(SiftError::MissingOrgId("x".into()).exit_code(), 1);
        assert_eq!(SiftError::OcrTokenMissing.exit_code(), 1);
        assert_eq!(SiftError::BrokenPipe.exit_code(), 0);
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
