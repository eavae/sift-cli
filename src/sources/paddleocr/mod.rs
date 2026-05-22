//! PaddleOCR backends — two flavors share one trait.
//!
//! The fine-extraction orchestrator only needs to send a multi-page
//! PDF chunk and receive one [`PageResult`] per input page. Two
//! upstreams ship that capability today and pick credentials
//! differently, so we encapsulate each behind an [`OcrClient`]
//! impl and let [`build_client`] decide which one to spin up at
//! startup based on environment variables:
//!
//! | Mode | Env vars (both required) | Backend |
//! |------|--------------------------|---------|
//! | token | `PADDLEOCR_API_BASE`, `PADDLEOCR_API_TOKEN` | hosted PaddleOCR layout-parsing endpoint (synchronous) |
//! | oauth | `PADDLEOCR_API_KEY`, `PADDLEOCR_SECRET_KEY` | Baidu AI Platform paddle-vl-parser (async: submit → poll → download) |
//!
//! When both modes are configured, token wins — it's the faster
//! path (one round-trip vs OAuth + submit + 5–10s of polling). If
//! neither is configured, [`build_client`] raises
//! [`SiftError::OcrTokenMissing`] with a message that lists both
//! options so the user can pick.

mod oauth;
mod token;

use crate::error::SiftError;
use crate::http::HttpClient;

pub use oauth::OAuthClient;
pub use token::TokenClient;

/// Maximum pages a single OCR call accepts. The token backend caps
/// at 8 (upstream returns 413 for more); the OAuth backend has a
/// looser limit but we keep parity across modes so chunk planning
/// stays mode-agnostic.
pub const PADDLEOCR_BATCH_SIZE: usize = 8;

/// One page of OCR output, in input-page order.
///
/// `markdown` is the layout-parsing markdown body. Image references
/// inside `markdown` use whatever name keys the upstream picked;
/// the orchestrator renames each to `p<NN>-img<MM>.<ext>` and
/// rewrites the body before landing on disk.
///
/// `images` carries every binary asset the upstream attached,
/// already fetched / decoded to raw bytes. The pair is
/// `(upstream_name, bytes)` so the rename step can match on the
/// upstream key without round-tripping bytes through the disk.
#[derive(Debug, Clone, PartialEq)]
pub struct PageResult {
    pub markdown: String,
    pub images: Vec<(String, Vec<u8>)>,
}

/// Common interface every OCR backend implements. Trait objects
/// are passed by reference to [`std::thread::scope`] workers, so
/// each backend must be both `Send` and `Sync`.
pub trait OcrClient: Send + Sync {
    /// Submit a multi-page PDF (≤ [`PADDLEOCR_BATCH_SIZE`] pages)
    /// and return one [`PageResult`] per input page, in input
    /// order. A mismatched response length is **not** the trait's
    /// concern — the orchestrator validates and surfaces that.
    fn parse_batch(&self, pdf_bytes: &[u8]) -> Result<Vec<PageResult>, SiftError>;

    /// Short identifier for stderr ("token" / "oauth"). Surfaced
    /// in the `[fine]` budget line so users know which backend
    /// ran.
    fn name(&self) -> &'static str;
}

/// Pick an [`OcrClient`] from environment configuration. Token
/// mode wins when both pairs are set. Returns
/// [`SiftError::OcrTokenMissing`] when neither pair is configured —
/// the error message lists both options so users can pick from
/// one place rather than guessing per-mode.
pub fn build_client(http: &HttpClient) -> Result<Box<dyn OcrClient + '_>, SiftError> {
    if let Some(c) = TokenClient::from_env(http) {
        return Ok(Box::new(c));
    }
    if let Some(c) = OAuthClient::from_env(http) {
        return Ok(Box::new(c));
    }
    Err(SiftError::OcrTokenMissing)
}
