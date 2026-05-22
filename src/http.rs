//! Synchronous HTTP client shared by every data source.
//!
//! Wraps `ureq::Agent` with the F1-README-mandated retry policy:
//! exponential backoff `1 → 2 → 4` seconds for status codes
//! `429 / 500 / 502 / 503 / 504`, up to 3 retries (= 4 total requests).
//! Other 4xx and transport-level failures return immediately.
//!
//! The retry parameters live as `const`s — F1 does not expose them as
//! flags or config-file knobs.

use crate::error::SiftError;
use std::time::Duration;

const UA: &str = concat!("sift/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Status codes that trigger a backoff retry. Anything else (other
/// 4xx, transport errors, DNS failures) returns immediately.
const RETRY_CODES: &[u16] = &[429, 500, 502, 503, 504];

/// Backoff sequence. Production: `[1, 2, 4]` seconds. Tests: zero
/// across the board so the retry-exhaustion case does not stretch CI
/// by 7 seconds. The cfg switch is the simplest knob the story
/// prescribed.
#[cfg(not(test))]
const BACKOFF_SECS: &[u64] = &[1, 2, 4];
#[cfg(test)]
const BACKOFF_SECS: &[u64] = &[0, 0, 0];

/// Hard cap on response body size. cninfo's two endpoints return well
/// under 2 MiB each; anything above 16 MiB is treated as a misbehaving
/// upstream rather than a legitimate payload.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

pub struct HttpClient {
    agent: ureq::Agent,
}

impl HttpClient {
    pub fn new() -> Self {
        // ureq 3 builds the `Agent` from a `Config`; the timeouts now
        // split between `connect` (TCP/TLS handshake) and
        // `recv_response` (headers) + `recv_body` (body). We give the
        // same 30 s budget to both response phases for parity with
        // ureq 2's single `timeout_read` knob.
        let config = ureq::Agent::config_builder()
            .user_agent(UA)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(READ_TIMEOUT))
            .timeout_recv_body(Some(READ_TIMEOUT))
            // ureq 3 surfaces non-2xx status codes as Ok(response) by
            // default and lets the caller decide; we keep that
            // behaviour explicit so the retry loop can inspect the
            // status without unwrapping an error variant.
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }

    /// GET with automatic retries. Returns the raw response body bytes;
    /// deserialization is the caller's job (cninfo returns JSON, EM
    /// returns JSONP-wrapped, sina returns pseudo-JS — each source
    /// peels its own envelope).
    ///
    /// On a `Retry-After` header (`429`/`503`), the integer-second
    /// value is honored *in place of* the next backoff slot — we do
    /// not stack them. The header still counts against the retry
    /// budget.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SiftError> {
        self.with_retries("GET", url, || self.agent.get(url).call())
    }

    /// POST `application/x-www-form-urlencoded`. Body is the
    /// `&[(&str, &str)]` form pairs; encoding is handled by ureq.
    /// Shares the retry / `Retry-After` / 16 MiB body cap behaviour
    /// with [`HttpClient::get_bytes`].
    pub fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Vec<u8>, SiftError> {
        // ureq 3 `send_form` expects `IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>`;
        // `&[(&str, &str)]` iterates as `&(&str, &str)` so we copy the
        // tuple to get an owning `(&str, &str)` iterator. The lifetimes
        // are tied to `form` which outlives this expression.
        self.with_retries("POST", url, || {
            self.agent.post(url).send_form(form.iter().copied())
        })
    }

    /// POST a JSON body with a custom `Authorization` header. Used by
    /// `sources::paddleocr` to drive the PaddleOCR layout-parsing
    /// endpoint (which expects `Authorization: token <…>` rather than
    /// the more usual `Bearer`). Shares the retry / `Retry-After` /
    /// body-cap behavior with the other verbs.
    pub fn post_json_with_auth(
        &self,
        url: &str,
        body: &serde_json::Value,
        auth_header: &str,
    ) -> Result<Vec<u8>, SiftError> {
        self.with_retries("POST", url, || {
            self.agent
                .post(url)
                .header("authorization", auth_header)
                .header("content-type", "application/json")
                .send_json(body)
        })
    }

    /// Shared retry / body-read loop for every HTTP verb. The
    /// `request` closure encapsulates which verb (and which body
    /// payload, for POST) to send on each attempt; everything else —
    /// retry codes, backoff sequence, `Retry-After` parsing, body
    /// cap — is handled once here.
    ///
    /// In ureq 3 every non-transport response (4xx, 5xx) arrives as
    /// `Ok(Response)` because we set `http_status_as_error(false)` —
    /// the loop classifies by `resp.status()` instead of by error
    /// variant, which is more uniform than ureq 2's
    /// `Error::Status(code, resp)`.
    fn with_retries<F>(
        &self,
        verb: &str,
        url: &str,
        mut request: F,
    ) -> Result<Vec<u8>, SiftError>
    where
        F: FnMut() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    {
        let mut last_err: Option<SiftError> = None;
        // One initial attempt plus `BACKOFF_SECS.len()` retries. The
        // final iteration has no corresponding sleep slot, so
        // `BACKOFF_SECS.get(attempt)` is `None` there and we skip the
        // wait — this is the structural reason we tolerate an
        // `attempt` index that's one larger than the slice.
        for attempt in 0..=BACKOFF_SECS.len() {
            match request() {
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    if (200..300).contains(&code) {
                        return read_body(resp);
                    }
                    if RETRY_CODES.contains(&code) {
                        last_err = Some(SiftError::Network(format!("{verb} {url} -> {code}")));
                        if let Some(&base_wait) = BACKOFF_SECS.get(attempt) {
                            let wait = retry_after_secs(&resp).unwrap_or(base_wait);
                            if wait > 0 {
                                std::thread::sleep(Duration::from_secs(wait));
                            }
                        }
                        continue;
                    }
                    return Err(SiftError::Network(format!("{verb} {url} -> {code}")));
                }
                Err(e) => return Err(SiftError::Network(e.to_string())),
            }
        }
        Err(last_err.expect("retry loop only exits via Ok or a populated last_err"))
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

fn retry_after_secs(resp: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Drain the response body with a `MAX_BODY_BYTES` cap. ureq 3's
/// `Body::read_to_vec` takes care of the buffer; we layer the cap via
/// the configurable `Body::with_config().limit(...)` reader so an
/// oversized response is reported rather than truncated silently.
fn read_body(resp: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, SiftError> {
    let mut body = resp.into_body();
    // Read up to `MAX + 1` so we can distinguish "exactly at cap" (OK)
    // from "over the cap" (error) without trusting `Content-Length`.
    let bytes = body
        .with_config()
        .limit((MAX_BODY_BYTES + 1) as u64)
        .read_to_vec()
        .map_err(|e| SiftError::Network(format!("body read: {e}")))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(SiftError::Network(format!(
            "response body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_200_returns_body_and_makes_one_request() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/ok")
            .with_status(200)
            .with_body("hello")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client.get_bytes(&format!("{}/ok", server.url())).unwrap();
        assert_eq!(body, b"hello");
        m.assert();
    }

    #[test]
    fn retries_each_documented_code_up_to_max() {
        for code in [429u16, 500, 502, 503, 504] {
            let mut server = mockito::Server::new();
            // 1 initial + 3 retries = 4 total.
            let m = server
                .mock("GET", "/x")
                .with_status(code.into())
                .expect(4)
                .create();
            let client = HttpClient::new();
            let err = client
                .get_bytes(&format!("{}/x", server.url()))
                .unwrap_err();
            assert!(matches!(err, SiftError::Network(_)), "code={code}");
            m.assert();
        }
    }

    #[test]
    fn does_not_retry_on_403() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/forbidden")
            .with_status(403)
            .expect(1)
            .create();
        let client = HttpClient::new();
        let err = client
            .get_bytes(&format!("{}/forbidden", server.url()))
            .unwrap_err();
        assert!(matches!(err, SiftError::Network(_)));
        m.assert();
    }

    #[test]
    fn does_not_retry_on_404() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/missing")
            .with_status(404)
            .expect(1)
            .create();
        let client = HttpClient::new();
        let err = client
            .get_bytes(&format!("{}/missing", server.url()))
            .unwrap_err();
        assert!(matches!(err, SiftError::Network(_)));
        m.assert();
    }

    #[test]
    fn retry_after_header_is_honored() {
        // Two sequential mocks: first 429 with `Retry-After: 0` (exactly
        // one hit), then 200 (exactly one hit). Mockito 1.x routes the
        // second request to the second mock once the first's expect
        // count is satisfied — that confirms the retry actually fired.
        let mut server = mockito::Server::new();
        let m1 = server
            .mock("GET", "/r")
            .with_status(429)
            .with_header("Retry-After", "0")
            .expect(1)
            .create();
        let m2 = server
            .mock("GET", "/r")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client.get_bytes(&format!("{}/r", server.url())).unwrap();
        assert_eq!(body, b"ok");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn recovers_when_retry_eventually_succeeds() {
        let mut server = mockito::Server::new();
        let m1 = server
            .mock("GET", "/s")
            .with_status(502)
            .expect(2)
            .create();
        let m2 = server
            .mock("GET", "/s")
            .with_status(200)
            .with_body("done")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client.get_bytes(&format!("{}/s", server.url())).unwrap();
        assert_eq!(body, b"done");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn post_form_returns_body_and_sends_form_fields() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("POST", "/echo")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("a".into(), "1".into()),
                mockito::Matcher::UrlEncoded("b".into(), "2".into()),
            ]))
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client
            .post_form(&format!("{}/echo", server.url()), &[("a", "1"), ("b", "2")])
            .unwrap();
        assert_eq!(body, b"ok");
        m.assert();
    }

    #[test]
    fn post_form_retries_on_502_then_succeeds() {
        let mut server = mockito::Server::new();
        let m_fail = server
            .mock("POST", "/r")
            .with_status(502)
            .expect(2)
            .create();
        let m_ok = server
            .mock("POST", "/r")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client
            .post_form(&format!("{}/r", server.url()), &[("x", "1")])
            .unwrap();
        assert_eq!(body, b"ok");
        m_fail.assert();
        m_ok.assert();
    }

    #[test]
    fn post_json_with_auth_sends_header_and_body() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("POST", "/llm")
            .match_header("authorization", "token abc123")
            .match_header("content-type", "application/json")
            .match_body(mockito::Matcher::JsonString(
                r#"{"k":"v"}"#.to_string(),
            ))
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client
            .post_json_with_auth(
                &format!("{}/llm", server.url()),
                &serde_json::json!({"k": "v"}),
                "token abc123",
            )
            .unwrap();
        assert_eq!(body, b"ok");
        m.assert();
    }

    #[test]
    fn post_json_with_auth_retries_on_502_then_succeeds() {
        let mut server = mockito::Server::new();
        let m_fail = server
            .mock("POST", "/llm")
            .with_status(502)
            .expect(2)
            .create();
        let m_ok = server
            .mock("POST", "/llm")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create();
        let client = HttpClient::new();
        let body = client
            .post_json_with_auth(
                &format!("{}/llm", server.url()),
                &serde_json::json!({}),
                "token x",
            )
            .unwrap();
        assert_eq!(body, b"ok");
        m_fail.assert();
        m_ok.assert();
    }

    #[test]
    fn post_form_does_not_retry_on_404() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("POST", "/none")
            .with_status(404)
            .expect(1)
            .create();
        let client = HttpClient::new();
        let err = client
            .post_form(&format!("{}/none", server.url()), &[])
            .unwrap_err();
        assert!(matches!(err, SiftError::Network(_)));
        m.assert();
    }
}
