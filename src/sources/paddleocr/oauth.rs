//! OAuth-mode PaddleOCR backend (Baidu AI Platform).
//!
//! Baidu's `paddle-vl-parser` API is **asynchronous**:
//!
//! 1. Exchange `api_key` + `secret_key` for an `access_token`
//!    (30-day lifetime; cached in the client).
//! 2. POST the chunk PDF (base64) to the `task` endpoint → receive
//!    a `task_id`.
//! 3. Poll the `task/query` endpoint every few seconds until
//!    `status == "success"` (or `"failed"`).
//! 4. Download the `parse_result_url` JSON for per-page structured
//!    output (layouts + tables + images) and assemble one
//!    [`PageResult`] per input PDF page.
//!
//! Reference: `packages/shared/external/paddleocr_client.py` in
//! the design_assistant_backend repo (Python, async). This file is
//! the synchronous Rust port — the only material difference is
//! that we run the poll loop on the calling worker thread instead
//! of inside an event loop.
//!
//! ## Configuration
//!
//! Triggered when **both** `PADDLEOCR_API_KEY` and
//! `PADDLEOCR_SECRET_KEY` are set. The Baidu host (default
//! `https://aip.baidubce.com`) is overridable by
//! `SIFT_BAIDU_HOST` for tests against mockito.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine;
use serde_json::Value;

use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::paddleocr::{OcrClient, PageResult};

const DEFAULT_BAIDU_HOST: &str = "https://aip.baidubce.com";
const OAUTH_TOKEN_PATH: &str = "/oauth/2.0/token";
const API_BASE_PATH: &str = "/rest/2.0/brain/online/v2/paddle-vl-parser";

/// Refresh the cached access token this many seconds before it
/// actually expires. Mirrors the Python reference so test snapshots
/// of the two clients line up.
const TOKEN_REFRESH_BUFFER_SECS: u64 = 3600;

/// How long to wait before the first task-query poll. The endpoint
/// docs recommend 5–10s for a fresh task — extracting earlier
/// almost always returns `pending` and wastes a round-trip.
#[cfg(not(test))]
const POLL_INITIAL_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const POLL_INITIAL_DELAY: Duration = Duration::from_millis(0);

/// Delay between subsequent polls.
#[cfg(not(test))]
const POLL_INTERVAL: Duration = Duration::from_secs(3);
#[cfg(test)]
const POLL_INTERVAL: Duration = Duration::from_millis(0);

/// Hard upper bound on total polling time for one task. Acts as a
/// circuit breaker so a stuck task can't pin a worker forever.
#[cfg(not(test))]
const POLL_MAX_WAIT: Duration = Duration::from_secs(120);
#[cfg(test)]
const POLL_MAX_WAIT: Duration = Duration::from_secs(5);

/// OAuth-mode client. Holds the credentials, a borrow on the shared
/// HTTP client, and a `Mutex` token cache so the (cheap) refresh
/// path stays behind `&self`.
pub struct OAuthClient<'a> {
    http: &'a HttpClient,
    api_key: String,
    secret_key: String,
    host: String,
    token_cache: Mutex<Option<CachedToken>>,
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    /// Unix seconds at which we'll proactively refresh.
    expires_at: u64,
}

impl<'a> OAuthClient<'a> {
    /// Build an OAuth-mode client from the environment, or `None`
    /// when either required env var is unset / empty.
    pub fn from_env(http: &'a HttpClient) -> Option<Self> {
        let api_key = nonempty_env("PADDLEOCR_API_KEY")?;
        let secret_key = nonempty_env("PADDLEOCR_SECRET_KEY")?;
        let host = nonempty_env("SIFT_BAIDU_HOST")
            .unwrap_or_else(|| DEFAULT_BAIDU_HOST.to_owned())
            .trim_end_matches('/')
            .to_owned();
        Some(Self {
            http,
            api_key,
            secret_key,
            host,
            token_cache: Mutex::new(None),
        })
    }

    fn token_url(&self) -> String {
        format!(
            "{}{}?grant_type=client_credentials&client_id={}&client_secret={}",
            self.host, OAUTH_TOKEN_PATH, self.api_key, self.secret_key
        )
    }

    fn submit_url(&self, token: &str) -> String {
        format!("{}{}/task?access_token={}", self.host, API_BASE_PATH, token)
    }

    fn query_url(&self, token: &str) -> String {
        format!(
            "{}{}/task/query?access_token={}",
            self.host, API_BASE_PATH, token
        )
    }

    /// Return a non-expired access token, fetching one if needed.
    /// The cache is a `Mutex` so concurrent workers can share the
    /// same token across chunks instead of each refetching.
    fn get_access_token(&self) -> Result<String, SiftError> {
        let now = unix_secs();
        if let Some(cached) = self.token_cache.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            if now < cached.expires_at {
                return Ok(cached.token);
            }
        }
        let body = self.http.post_form(&self.token_url(), &[])?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| {
            SiftError::Internal(format!("baidu token decode: not JSON: {e}"))
        })?;
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            let desc = v
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or(err);
            return Err(SiftError::Network(format!("baidu OAuth: {desc}")));
        }
        let token = v
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SiftError::Internal("baidu token decode: missing access_token".into())
            })?
            .to_owned();
        let expires_in = v
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(2_592_000);
        let buffer = TOKEN_REFRESH_BUFFER_SECS.min(expires_in.saturating_sub(60));
        let cached = CachedToken {
            token: token.clone(),
            expires_at: now + expires_in.saturating_sub(buffer),
        };
        *self.token_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(cached);
        Ok(token)
    }

    /// Submit one task. Returns the upstream `task_id` to poll on.
    fn submit_task(&self, pdf_bytes: &[u8]) -> Result<String, SiftError> {
        let token = self.get_access_token()?;
        let b64 = B64_STANDARD.encode(pdf_bytes);
        let resp = self.http.post_form(
            &self.submit_url(&token),
            &[
                ("file_data", &b64),
                ("file_url", ""),
                ("file_name", "chunk.pdf"),
            ],
        )?;
        let v: Value = serde_json::from_slice(&resp).map_err(|e| {
            SiftError::Internal(format!("baidu submit decode: not JSON: {e}"))
        })?;
        check_baidu_error(&v, "submit")?;
        let task_id = v
            .get("result")
            .and_then(|r| r.get("task_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SiftError::Internal("baidu submit: missing result.task_id".into())
            })?
            .to_owned();
        Ok(task_id)
    }

    /// Poll until the task completes, fails, or we hit
    /// [`POLL_MAX_WAIT`]. Returns the `result` object (which carries
    /// `markdown_url` and `parse_result_url`).
    fn poll_task(&self, task_id: &str) -> Result<Value, SiftError> {
        std::thread::sleep(POLL_INITIAL_DELAY);
        let mut elapsed = POLL_INITIAL_DELAY;
        loop {
            let token = self.get_access_token()?;
            let resp = self.http.post_form(
                &self.query_url(&token),
                &[("task_id", task_id)],
            )?;
            let v: Value = serde_json::from_slice(&resp).map_err(|e| {
                SiftError::Internal(format!("baidu poll decode: not JSON: {e}"))
            })?;
            check_baidu_error(&v, "poll")?;
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            let status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
            match status {
                "success" => return Ok(result),
                "failed" => {
                    let task_err = result
                        .get("task_error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(SiftError::Network(format!(
                        "baidu task {task_id} failed: {task_err}"
                    )));
                }
                _ => {
                    // pending / processing → keep polling
                }
            }
            if elapsed >= POLL_MAX_WAIT {
                return Err(SiftError::Network(format!(
                    "baidu task {task_id} did not complete within {}s",
                    POLL_MAX_WAIT.as_secs()
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
            elapsed += POLL_INTERVAL;
        }
    }

    /// Fetch `parse_result_url` and assemble one [`PageResult`] per
    /// page in the response. Each page's `layouts[*].text` and
    /// `tables[*].markdown` are joined with blank lines; images
    /// come from `images[*].{layout_id, data_url}`, where `data_url`
    /// is either an inline `data:` URI (decoded) or a remote presigned
    /// URL (fetched) — see [`fetch_or_decode_data_url`].
    fn download_results(&self, task: &Value) -> Result<Vec<PageResult>, SiftError> {
        let parse_url = task
            .get("parse_result_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SiftError::Internal(
                    "baidu task result missing `parse_result_url`; \
                     per-page extraction unavailable"
                        .into(),
                )
            })?;
        let bytes = self.http.get_bytes(parse_url)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
            SiftError::Internal(format!("baidu parse_result decode: not JSON: {e}"))
        })?;
        let pages = v
            .get("pages")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                SiftError::Internal(
                    "baidu parse_result decode: missing `pages` array".into(),
                )
            })?;

        let mut out = Vec::with_capacity(pages.len());
        for (i, page) in pages.iter().enumerate() {
            let mut parts: Vec<String> = Vec::new();
            if let Some(layouts) = page.get("layouts").and_then(Value::as_array) {
                for layout in layouts {
                    if let Some(text) = layout.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(text.to_owned());
                        }
                    }
                }
            }
            if let Some(tables) = page.get("tables").and_then(Value::as_array) {
                for table in tables {
                    if let Some(md) = table.get("markdown").and_then(Value::as_str) {
                        if !md.is_empty() {
                            parts.push(md.to_owned());
                        }
                    }
                }
            }
            let mut images: Vec<(String, Vec<u8>)> = Vec::new();
            if let Some(imgs) = page.get("images").and_then(Value::as_array) {
                for img in imgs {
                    let layout_id = img
                        .get("layout_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let data_url = img
                        .get("data_url")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if layout_id.is_empty() || data_url.is_empty() {
                        continue;
                    }
                    let raw = fetch_or_decode_data_url(self.http, data_url, i, layout_id)?;
                    images.push((layout_id.to_owned(), raw));
                }
            }
            out.push(PageResult {
                markdown: parts.join("\n\n"),
                images,
            });
        }
        Ok(out)
    }
}

impl OcrClient for OAuthClient<'_> {
    fn parse_batch(&self, pdf_bytes: &[u8]) -> Result<Vec<PageResult>, SiftError> {
        let task_id = self.submit_task(pdf_bytes)?;
        let task = self.poll_task(&task_id)?;
        self.download_results(&task)
    }

    fn name(&self) -> &'static str {
        "oauth"
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Baidu's task endpoints return `{ error_code: N, error_msg }`
/// even on 200 — non-zero codes are upstream rejections that ureq
/// won't catch. Map them to `SiftError::Network` so the chunk
/// orchestrator surfaces them as a cause line.
fn check_baidu_error(v: &Value, stage: &str) -> Result<(), SiftError> {
    let code = v.get("error_code").and_then(Value::as_i64).unwrap_or(0);
    if code == 0 {
        return Ok(());
    }
    let msg = v
        .get("error_msg")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Err(SiftError::Network(format!(
        "baidu {stage} error_code {code}: {msg}"
    )))
}

/// Resolve one `images[*].data_url` to raw bytes. The field is
/// misnamed: small regions arrive as an inline `data:image/...;base64,`
/// URI, but figure/chart regions (e.g. `*-layout-11`) come back as a
/// remote presigned URL. Fetch the URLs via the shared [`HttpClient`];
/// decode the inline ones locally. Mirrors the token backend's
/// `fetch_or_decode_image`; errors carry the page + layout id so a
/// malformed response surfaces actionably.
fn fetch_or_decode_data_url(
    http: &HttpClient,
    s: &str,
    page_idx: usize,
    layout_id: &str,
) -> Result<Vec<u8>, SiftError> {
    let trimmed = s.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return http.get_bytes(trimmed).map_err(|e| {
            SiftError::Network(format!(
                "baidu parse_result image fetch: page[{page_idx}].images.{layout_id}: {e}"
            ))
        });
    }
    decode_data_url(trimmed, page_idx, layout_id)
}

/// Decode an inline `data:` URL of the form `data:image/png;base64,XXXX`.
fn decode_data_url(s: &str, page_idx: usize, layout_id: &str) -> Result<Vec<u8>, SiftError> {
    let (_, after_comma) = s.trim().split_once(',').ok_or_else(|| {
        SiftError::Internal(format!(
            "baidu parse_result decode: page[{page_idx}].images.{layout_id}: \
             data_url has no comma"
        ))
    })?;
    B64_STANDARD.decode(after_comma.as_bytes()).map_err(|e| {
        SiftError::Internal(format!(
            "baidu parse_result decode: page[{page_idx}].images.{layout_id} base64: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Same pattern as token.rs — serialise tests that mutate the
    // four env vars used to pick a backend. Lives in this module
    // so the two test mutexes are independent (avoid cross-test
    // deadlocks when both files run concurrently in cargo test).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        prev_key: Option<String>,
        prev_secret: Option<String>,
        prev_host: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(host: &str, key: &str, secret: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prev_key = std::env::var("PADDLEOCR_API_KEY").ok();
            let prev_secret = std::env::var("PADDLEOCR_SECRET_KEY").ok();
            let prev_host = std::env::var("SIFT_BAIDU_HOST").ok();
            unsafe {
                std::env::set_var("PADDLEOCR_API_KEY", key);
                std::env::set_var("PADDLEOCR_SECRET_KEY", secret);
                std::env::set_var("SIFT_BAIDU_HOST", host);
            }
            Self {
                prev_key,
                prev_secret,
                prev_host,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_key {
                    Some(v) => std::env::set_var("PADDLEOCR_API_KEY", v),
                    None => std::env::remove_var("PADDLEOCR_API_KEY"),
                }
                match &self.prev_secret {
                    Some(v) => std::env::set_var("PADDLEOCR_SECRET_KEY", v),
                    None => std::env::remove_var("PADDLEOCR_SECRET_KEY"),
                }
                match &self.prev_host {
                    Some(v) => std::env::set_var("SIFT_BAIDU_HOST", v),
                    None => std::env::remove_var("SIFT_BAIDU_HOST"),
                }
            }
        }
    }

    /// Quick base64 of arbitrary bytes — used in fixture data
    /// URLs so the decoder sees realistic inputs.
    fn data_url(mime: &str, raw: &[u8]) -> String {
        format!("data:{};base64,{}", mime, B64_STANDARD.encode(raw))
    }

    fn token_body() -> String {
        serde_json::json!({
            "access_token": "baidu-token-abc",
            "expires_in": 2_592_000_u64,
        })
        .to_string()
    }

    fn submit_body(task_id: &str) -> String {
        serde_json::json!({
            "error_code": 0,
            "result": { "task_id": task_id }
        })
        .to_string()
    }

    fn query_body_success(parse_url: &str) -> String {
        serde_json::json!({
            "error_code": 0,
            "result": {
                "status": "success",
                "markdown_url": "",
                "parse_result_url": parse_url,
            }
        })
        .to_string()
    }

    /// Fixture spec: `(layouts_text, tables_md, (layout_id, data_url) images)`.
    type PageFixture<'a> = (&'a [&'a str], &'a [&'a str], &'a [(&'a str, &'a str)]);

    fn parse_result_body(pages: &[PageFixture<'_>]) -> String {
        let v: Vec<serde_json::Value> = pages
            .iter()
            .map(|(layouts, tables, images)| {
                let layouts_arr: Vec<_> = layouts
                    .iter()
                    .map(|t| serde_json::json!({"text": t}))
                    .collect();
                let tables_arr: Vec<_> = tables
                    .iter()
                    .map(|m| serde_json::json!({"markdown": m}))
                    .collect();
                let images_arr: Vec<_> = images
                    .iter()
                    .map(|(lid, durl)| {
                        serde_json::json!({"layout_id": lid, "data_url": durl})
                    })
                    .collect();
                serde_json::json!({
                    "layouts": layouts_arr,
                    "tables": tables_arr,
                    "images": images_arr,
                })
            })
            .collect();
        serde_json::json!({ "pages": v }).to_string()
    }

    #[test]
    fn from_env_returns_none_when_either_var_missing() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let pk = std::env::var("PADDLEOCR_API_KEY").ok();
        let ps = std::env::var("PADDLEOCR_SECRET_KEY").ok();
        unsafe {
            std::env::remove_var("PADDLEOCR_API_KEY");
            std::env::remove_var("PADDLEOCR_SECRET_KEY");
        }
        let http = HttpClient::new();
        assert!(OAuthClient::from_env(&http).is_none());
        unsafe { std::env::set_var("PADDLEOCR_API_KEY", "x"); }
        assert!(OAuthClient::from_env(&http).is_none(), "missing SECRET");
        unsafe { std::env::remove_var("PADDLEOCR_API_KEY"); }
        unsafe { std::env::set_var("PADDLEOCR_SECRET_KEY", "y"); }
        assert!(OAuthClient::from_env(&http).is_none(), "missing KEY");
        unsafe {
            match &pk {
                Some(v) => std::env::set_var("PADDLEOCR_API_KEY", v),
                None => std::env::remove_var("PADDLEOCR_API_KEY"),
            }
            match &ps {
                Some(v) => std::env::set_var("PADDLEOCR_SECRET_KEY", v),
                None => std::env::remove_var("PADDLEOCR_SECRET_KEY"),
            }
        }
    }

    #[test]
    fn full_happy_path_returns_per_page_results() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "ak", "sk");

        let m_token = server
            .mock("POST", "/oauth/2.0/token")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("grant_type".into(), "client_credentials".into()),
                mockito::Matcher::UrlEncoded("client_id".into(), "ak".into()),
                mockito::Matcher::UrlEncoded("client_secret".into(), "sk".into()),
            ]))
            .with_status(200)
            .with_body(token_body())
            .expect_at_least(1)
            .create();
        let m_submit = server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(submit_body("task-42"))
            .expect(1)
            .create();
        let parse_path = "/cdn/parse_42.json";
        let parse_url = format!("{}{}", server.url(), parse_path);
        let m_query = server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task/query")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(query_body_success(&parse_url))
            .expect_at_least(1)
            .create();
        let m_parse = server
            .mock("GET", parse_path)
            .with_status(200)
            .with_body(parse_result_body(&[
                (
                    &["hello"],
                    &["| col | col2 |"],
                    &[("L1", &data_url("image/png", b"fakepng"))],
                ),
                (&["second page"], &[], &[]),
            ]))
            .expect(1)
            .create();

        let http = HttpClient::new();
        let client = OAuthClient::from_env(&http).expect("env set above");
        let pages = client.parse_batch(b"%PDF stub").unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages[0].markdown.contains("hello"));
        assert!(pages[0].markdown.contains("| col | col2 |"));
        assert_eq!(pages[0].images.len(), 1);
        assert_eq!(pages[0].images[0].0, "L1");
        assert_eq!(pages[0].images[0].1, b"fakepng");
        assert_eq!(pages[1].markdown, "second page");
        m_token.assert();
        m_submit.assert();
        m_query.assert();
        m_parse.assert();
    }

    /// Figure/chart regions (`*-layout-11`) arrive as a remote presigned
    /// URL in `data_url`, not an inline `data:` URI. The decoder must
    /// fetch those rather than choke on the missing comma.
    #[test]
    fn remote_image_url_in_data_url_is_fetched() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "ak", "sk");

        server
            .mock("POST", "/oauth/2.0/token")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(token_body())
            .create();
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(submit_body("task-url"))
            .create();
        let img_path = "/bos/fig-layout-11.jpg";
        let img_url = format!("{}{}", server.url(), img_path);
        let m_img = server
            .mock("GET", img_path)
            .with_status(200)
            .with_body(b"\xff\xd8\xff JPEG bytes")
            .expect(1)
            .create();
        let parse_path = "/cdn/url.json";
        let parse_url = format!("{}{}", server.url(), parse_path);
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task/query")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(query_body_success(&parse_url))
            .create();
        server
            .mock("GET", parse_path)
            .with_status(200)
            .with_body(parse_result_body(&[(
                &["figure caption text"],
                &[],
                &[("remote-layout-11", &img_url)],
            )]))
            .create();

        let http = HttpClient::new();
        let client = OAuthClient::from_env(&http).expect("env set above");
        let pages = client.parse_batch(b"%PDF stub").unwrap();

        assert_eq!(pages.len(), 1);
        assert!(pages[0].markdown.contains("figure caption text"));
        assert_eq!(pages[0].images.len(), 1);
        assert_eq!(pages[0].images[0].0, "remote-layout-11");
        assert_eq!(pages[0].images[0].1, b"\xff\xd8\xff JPEG bytes");
        m_img.assert();
    }

    #[test]
    fn task_failed_status_propagates_as_network_error() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "ak", "sk");
        server
            .mock("POST", "/oauth/2.0/token")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(token_body())
            .create();
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(submit_body("task-x"))
            .create();
        let fail_body = serde_json::json!({
            "error_code": 0,
            "result": {
                "status": "failed",
                "task_error": "PDF could not be parsed",
            }
        })
        .to_string();
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task/query")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(fail_body)
            .create();

        let http = HttpClient::new();
        let client = OAuthClient::from_env(&http).unwrap();
        let err = client.parse_batch(b"x").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SiftError::Network(_)));
        assert!(msg.contains("task-x"), "msg: {msg}");
        assert!(msg.contains("PDF could not be parsed"), "msg: {msg}");
    }

    #[test]
    fn baidu_error_code_on_submit_surfaces_as_network() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "ak", "sk");
        server
            .mock("POST", "/oauth/2.0/token")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(token_body())
            .create();
        let err_body = serde_json::json!({
            "error_code": 6,
            "error_msg": "No permission to access data",
        })
        .to_string();
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(err_body)
            .create();

        let http = HttpClient::new();
        let client = OAuthClient::from_env(&http).unwrap();
        let err = client.parse_batch(b"x").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SiftError::Network(_)));
        assert!(msg.contains("error_code 6"), "msg: {msg}");
        assert!(msg.contains("No permission"), "msg: {msg}");
    }

    #[test]
    fn token_cache_avoids_a_second_oauth_call() {
        let mut server = mockito::Server::new();
        let _g = EnvGuard::set(&server.url(), "ak", "sk");
        let m_token = server
            .mock("POST", "/oauth/2.0/token")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(token_body())
            .expect(1) // exactly one even though we call twice
            .create();
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(submit_body("t1"))
            .create();
        let parse_path = "/cdn/p.json";
        let parse_url = format!("{}{}", server.url(), parse_path);
        server
            .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task/query")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(query_body_success(&parse_url))
            .create();
        server
            .mock("GET", parse_path)
            .with_status(200)
            .with_body(parse_result_body(&[(&["x"], &[], &[])]))
            .create();

        let http = HttpClient::new();
        let client = OAuthClient::from_env(&http).unwrap();
        // Two back-to-back submissions; second one should hit the
        // cached token instead of re-fetching.
        client.parse_batch(b"x").unwrap();
        client.parse_batch(b"y").unwrap();
        m_token.assert();
    }

    #[test]
    fn decode_data_url_extracts_base64_after_comma() {
        let bytes = decode_data_url("data:image/png;base64,aGVsbG8=", 0, "L1").unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn decode_data_url_rejects_malformed_input() {
        let err = decode_data_url("not-a-data-url", 0, "L1").unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
        assert!(err.to_string().contains("no comma"));
    }
}
