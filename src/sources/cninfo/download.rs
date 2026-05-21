//! F3 PDF download: fetch one announcement attachment and write it to
//! disk atomically. Used by `sift announce download`.

use crate::error::SiftError;
use crate::http::HttpClient;

/// Fetch one PDF and write it to `dst` atomically.
///
/// `url` is the full HTTP URL stored in
/// [`crate::domain::announcement::AnnouncementRow::url`] (already
/// prefixed with `http://static.cninfo.com.cn/` by the announcements
/// parser). Returns the byte count written so the command layer can
/// report `fetched N KB`. Uses the shared `HttpClient::get_bytes` so
/// retries / `Retry-After` / 16 MiB body cap all behave the same as
/// for JSON endpoints — cninfo PDFs are well under 5 MB so reading
/// the full body into memory is fine.
///
/// Writes go through [`crate::cache::atomic_write`]: `<dst>.tmp` →
/// `rename`, so concurrent readers never see a half-written file.
pub fn download_pdf(
    http: &HttpClient,
    url: &str,
    dst: &std::path::Path,
) -> Result<usize, SiftError> {
    let bytes = http.get_bytes(url)?;
    crate::cache::atomic_write(dst, &bytes)?;
    Ok(bytes.len())
}
