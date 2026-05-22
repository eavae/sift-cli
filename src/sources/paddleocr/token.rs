//! Token-mode PaddleOCR backend: one HTTP round-trip per chunk.
//!
//! Hits the hosted `layout-parsing` endpoint with a base64-encoded
//! multi-page PDF and `Authorization: token <…>`. The endpoint
//! responds with `result.layoutParsingResults[i]` in input page
//! order — one `markdown.text` per page, plus an optional
//! `markdown.images` map whose values are either URLs (production)
//! or inline base64 (test fixtures).
//!
//! ## Configuration
//!
//! Triggered when **both** `PADDLEOCR_API_BASE` and
//! `PADDLEOCR_API_TOKEN` are set. The base must be a fully-qualified
//! URL without the `/layout-parsing` suffix; the client appends it.

use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine;
use serde_json::Value;

use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::paddleocr::{OcrClient, PageResult};

/// Token-mode client. Stores credentials + a borrow on the shared
/// HTTP client; no interior state (every request is self-contained).
pub struct TokenClient<'a> {
    http: &'a HttpClient,
    base: String,
    token: String,
}

impl<'a> TokenClient<'a> {
    /// Build a token-mode client from the environment, or `None`
    /// when either required env var is unset / empty. The caller
    /// (`paddleocr::build_client`) chains this with the OAuth
    /// fallback.
    pub fn from_env(http: &'a HttpClient) -> Option<Self> {
        let base = nonempty_env("PADDLEOCR_API_BASE")?;
        let token = nonempty_env("PADDLEOCR_API_TOKEN")?;
        Some(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token,
        })
    }
}

impl OcrClient for TokenClient<'_> {
    fn parse_batch(&self, pdf_bytes: &[u8]) -> Result<Vec<PageResult>, SiftError> {
        let body = serde_json::json!({
            "file": B64_STANDARD.encode(pdf_bytes),
            "fileType": 0,
            "useDocOrientationClassify": false,
            "useDocUnwarping": false,
            "useChartRecognition": false,
        });
        let url = format!("{}/layout-parsing", self.base);
        let auth = format!("token {}", self.token);
        let resp = self.http.post_json_with_auth(&url, &body, &auth)?;
        decode_response(self.http, &resp)
    }

    fn name(&self) -> &'static str {
        "token"
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Decode the PaddleOCR response. Tolerates two top-level shapes
/// the upstream has shipped (`{ "result": { ... } }` vs flat
/// `{ "layoutParsingResults": [...] }`) so a minor protocol bump
/// doesn't break sift; each entry must carry `markdown.text` —
/// `images` is optional and missing is treated as an empty map.
///
/// `markdown.images` values are **URLs** in production (each
/// pointing at a hosted thumbnail of the rendered page asset), so
/// we follow up with one GET per image. We also accept inline
/// base64 strings so unit tests can avoid spinning up a second
/// mockito route per image — see [`fetch_or_decode_image`].
fn decode_response(http: &HttpClient, bytes: &[u8]) -> Result<Vec<PageResult>, SiftError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| {
        SiftError::Internal(format!("paddleocr decode: not JSON: {e}"))
    })?;
    let results = v
        .get("result")
        .and_then(|r| r.get("layoutParsingResults"))
        .or_else(|| v.get("layoutParsingResults"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SiftError::Internal(
                "paddleocr decode: missing `layoutParsingResults` array".into(),
            )
        })?;

    let mut out = Vec::with_capacity(results.len());
    for (i, entry) in results.iter().enumerate() {
        let md_obj = entry.get("markdown").ok_or_else(|| {
            SiftError::Internal(format!(
                "paddleocr decode: result[{i}] missing `markdown`"
            ))
        })?;
        let text = md_obj
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SiftError::Internal(format!(
                    "paddleocr decode: result[{i}].markdown missing `text`"
                ))
            })?
            .to_owned();

        let images = match md_obj.get("images") {
            Some(Value::Object(map)) => {
                let mut imgs = Vec::with_capacity(map.len());
                for (name, val) in map {
                    let s = val.as_str().ok_or_else(|| {
                        SiftError::Internal(format!(
                            "paddleocr decode: result[{i}].markdown.images.{name} is not a string"
                        ))
                    })?;
                    let raw = fetch_or_decode_image(http, name, s, i)?;
                    imgs.push((name.clone(), raw));
                }
                imgs
            }
            _ => Vec::new(),
        };

        out.push(PageResult {
            markdown: text,
            images,
        });
    }
    Ok(out)
}

/// Resolve a `markdown.images` value to raw bytes. Production
/// PaddleOCR returns a hosted URL; tests sometimes inline base64 to
/// avoid a second mockito hop. Order:
///
/// 1. If the string looks like an `http(s)://` URL — GET it via
///    the shared [`HttpClient`] (retry / body-cap rules apply).
/// 2. Otherwise — treat as base64 and decode locally.
///
/// Either branch labels its error with the source index + image
/// name so a malformed PaddleOCR response surfaces actionably.
fn fetch_or_decode_image(
    http: &HttpClient,
    name: &str,
    value: &str,
    result_idx: usize,
) -> Result<Vec<u8>, SiftError> {
    let trimmed = value.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        http.get_bytes(trimmed).map_err(|e| {
            SiftError::Network(format!(
                "paddleocr image fetch: result[{result_idx}].markdown.images.{name}: {e}"
            ))
        })
    } else {
        B64_STANDARD.decode(trimmed.as_bytes()).map_err(|e| {
            SiftError::Internal(format!(
                "paddleocr decode: result[{result_idx}].markdown.images.{name}: \
                 value is neither URL nor base64 ({e})"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize every test that touches the four PaddleOCR env
    /// vars. `cargo test` runs `#[test]` functions in parallel
    /// within the same binary; without this lock two tests would
    /// race the env vars and mockito would see a request with the
    /// wrong (or missing) header.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        prev_token: Option<String>,
        prev_base: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(base: &str, token: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prev_token = std::env::var("PADDLEOCR_API_TOKEN").ok();
            let prev_base = std::env::var("PADDLEOCR_API_BASE").ok();
            unsafe {
                std::env::set_var("PADDLEOCR_API_BASE", base);
                std::env::set_var("PADDLEOCR_API_TOKEN", token);
            }
            Self {
                prev_token,
                prev_base,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_token {
                    Some(v) => std::env::set_var("PADDLEOCR_API_TOKEN", v),
                    None => std::env::remove_var("PADDLEOCR_API_TOKEN"),
                }
                match &self.prev_base {
                    Some(v) => std::env::set_var("PADDLEOCR_API_BASE", v),
                    None => std::env::remove_var("PADDLEOCR_API_BASE"),
                }
            }
        }
    }

    fn b64(s: &[u8]) -> String {
        B64_STANDARD.encode(s)
    }

    #[test]
    fn single_page_ok_returns_one_result() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok-abc");
        let body = serde_json::json!({
            "result": {
                "layoutParsingResults": [
                    {"markdown": {"text": "## Page 1\nhello"}}
                ]
            }
        })
        .to_string();
        let m = server
            .mock("POST", "/layout-parsing")
            .match_header("authorization", "token tok-abc")
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).expect("env set above");
        let pages = client.parse_batch(b"x").unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].markdown.contains("hello"));
        assert!(pages[0].images.is_empty());
        m.assert();
    }

    #[test]
    fn multi_page_preserves_input_order() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok");
        let body = serde_json::json!({
            "layoutParsingResults": [
                {"markdown": {"text": "first"}},
                {"markdown": {"text": "second"}},
                {"markdown": {"text": "third"}},
            ]
        })
        .to_string();
        server
            .mock("POST", "/layout-parsing")
            .with_status(200)
            .with_body(body)
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).unwrap();
        let pages = client.parse_batch(b"x").unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].markdown, "first");
        assert_eq!(pages[1].markdown, "second");
        assert_eq!(pages[2].markdown, "third");
    }

    #[test]
    fn images_map_urls_are_fetched_via_http() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok");
        let img1_path = "/cdn/img1.png";
        let img2_path = "/cdn/img2.png";
        let img1_url = format!("{}{}", server.url(), img1_path);
        let img2_url = format!("{}{}", server.url(), img2_path);
        let body = serde_json::json!({
            "layoutParsingResults": [
                {
                    "markdown": {
                        "text": "with image",
                        "images": {
                            "imgs/img1.png": img1_url,
                            "imgs/img2.png": img2_url,
                        }
                    }
                }
            ]
        })
        .to_string();
        server
            .mock("POST", "/layout-parsing")
            .with_status(200)
            .with_body(body)
            .create();
        server
            .mock("GET", img1_path)
            .with_status(200)
            .with_body(b"\x89PNG\r\nFAKE" as &[u8])
            .create();
        server
            .mock("GET", img2_path)
            .with_status(200)
            .with_body(b"binary" as &[u8])
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).unwrap();
        let pages = client.parse_batch(b"x").unwrap();
        assert_eq!(pages.len(), 1);
        let img1 = pages[0]
            .images
            .iter()
            .find(|(n, _)| n == "imgs/img1.png")
            .unwrap();
        assert_eq!(img1.1, b"\x89PNG\r\nFAKE");
        let img2 = pages[0]
            .images
            .iter()
            .find(|(n, _)| n == "imgs/img2.png")
            .unwrap();
        assert_eq!(img2.1, b"binary");
    }

    #[test]
    fn images_map_accepts_inline_base64_as_fallback() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok");
        let body = serde_json::json!({
            "layoutParsingResults": [
                {
                    "markdown": {
                        "text": "with image",
                        "images": { "img.png": b64(b"raw bytes here") }
                    }
                }
            ]
        })
        .to_string();
        server
            .mock("POST", "/layout-parsing")
            .with_status(200)
            .with_body(body)
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).unwrap();
        let pages = client.parse_batch(b"x").unwrap();
        assert_eq!(pages[0].images[0].1, b"raw bytes here");
    }

    #[test]
    fn http_4xx_propagates_as_network_no_retry() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok");
        let m = server
            .mock("POST", "/layout-parsing")
            .with_status(401)
            .expect(1)
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).unwrap();
        let err = client.parse_batch(b"x").unwrap_err();
        assert!(matches!(err, SiftError::Network(_)));
        m.assert();
    }

    #[test]
    fn http_5xx_retries_then_succeeds() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok");
        let m_fail = server
            .mock("POST", "/layout-parsing")
            .with_status(502)
            .expect(2)
            .create();
        let body = serde_json::json!({
            "layoutParsingResults": [{"markdown": {"text": "ok"}}]
        })
        .to_string();
        let m_ok = server
            .mock("POST", "/layout-parsing")
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).unwrap();
        let pages = client.parse_batch(b"x").unwrap();
        assert_eq!(pages.len(), 1);
        m_fail.assert();
        m_ok.assert();
    }

    #[test]
    fn schema_drift_returns_internal_error() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "tok");
        server
            .mock("POST", "/layout-parsing")
            .with_status(200)
            .with_body(r#"{"foo":1}"#)
            .create();
        let http = HttpClient::new();
        let client = TokenClient::from_env(&http).unwrap();
        let err = client.parse_batch(b"x").unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
        assert!(err.to_string().contains("paddleocr decode"));
    }

    #[test]
    fn from_env_returns_none_when_either_var_missing() {
        // Lock manually so we can mutate without going through
        // EnvGuard (which would try to set both).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_t = std::env::var("PADDLEOCR_API_TOKEN").ok();
        let prev_b = std::env::var("PADDLEOCR_API_BASE").ok();
        unsafe {
            std::env::remove_var("PADDLEOCR_API_TOKEN");
            std::env::remove_var("PADDLEOCR_API_BASE");
        }
        let http = HttpClient::new();
        assert!(TokenClient::from_env(&http).is_none());

        unsafe { std::env::set_var("PADDLEOCR_API_BASE", "https://x"); }
        assert!(TokenClient::from_env(&http).is_none(), "missing TOKEN");
        unsafe { std::env::remove_var("PADDLEOCR_API_BASE"); }

        unsafe { std::env::set_var("PADDLEOCR_API_TOKEN", "tok"); }
        assert!(TokenClient::from_env(&http).is_none(), "missing BASE");

        unsafe {
            match &prev_t {
                Some(v) => std::env::set_var("PADDLEOCR_API_TOKEN", v),
                None => std::env::remove_var("PADDLEOCR_API_TOKEN"),
            }
            match &prev_b {
                Some(v) => std::env::set_var("PADDLEOCR_API_BASE", v),
                None => std::env::remove_var("PADDLEOCR_API_BASE"),
            }
        }
    }
}
