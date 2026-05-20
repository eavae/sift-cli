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
use std::io::Read;
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
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .user_agent(UA)
            .build();
        Self { agent }
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
        // The closure returns `Result<Response, ureq::Error>`; clippy
        // flags the `Err` size (~272 B, dominated by ureq's enum)
        // but the box-it-up workaround would only relocate the cost.
        // Allow locally since the loop runs ≤4 times.
        #[allow(clippy::result_large_err)]
        let r = || self.agent.get(url).call();
        self.with_retries("GET", url, r)
    }

    /// POST `application/x-www-form-urlencoded`. Body is the
    /// `&[(&str, &str)]` form pairs; encoding is handled by ureq.
    /// Shares the retry / `Retry-After` / 16 MiB body cap behaviour
    /// with [`HttpClient::get_bytes`].
    pub fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Vec<u8>, SiftError> {
        #[allow(clippy::result_large_err)]
        let r = || self.agent.post(url).send_form(form);
        self.with_retries("POST", url, r)
    }

    /// Shared retry / body-read loop for every HTTP verb. The
    /// `request` closure encapsulates which verb (and which body
    /// payload, for POST) to send on each attempt; everything else —
    /// retry codes, backoff sequence, `Retry-After` parsing, body
    /// cap — is handled once here.
    fn with_retries<F>(
        &self,
        verb: &str,
        url: &str,
        mut request: F,
    ) -> Result<Vec<u8>, SiftError>
    where
        F: FnMut() -> Result<ureq::Response, ureq::Error>,
    {
        let mut last_err: Option<SiftError> = None;
        // One initial attempt plus `BACKOFF_SECS.len()` retries. The
        // final iteration has no corresponding sleep slot, so
        // `BACKOFF_SECS.get(attempt)` is `None` there and we skip the
        // wait — this is the structural reason we tolerate an
        // `attempt` index that's one larger than the slice.
        for attempt in 0..=BACKOFF_SECS.len() {
            match request() {
                Ok(resp) => return read_body(resp),
                Err(ureq::Error::Status(code, resp)) if RETRY_CODES.contains(&code) => {
                    last_err = Some(SiftError::Network(format!("{verb} {url} -> {code}")));
                    if let Some(&base_wait) = BACKOFF_SECS.get(attempt) {
                        let wait = retry_after_secs(&resp).unwrap_or(base_wait);
                        if wait > 0 {
                            std::thread::sleep(Duration::from_secs(wait));
                        }
                    }
                }
                Err(ureq::Error::Status(code, _)) => {
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

fn retry_after_secs(resp: &ureq::Response) -> Option<u64> {
    resp.header("Retry-After")?.trim().parse().ok()
}

/// Drain the response body with a `MAX_BODY_BYTES` cap. Reading one
/// extra byte lets us distinguish "exactly at cap" (OK) from "over the
/// cap" (error) without trusting `Content-Length`.
fn read_body(resp: ureq::Response) -> Result<Vec<u8>, SiftError> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| SiftError::Network(format!("body read: {e}")))?;
    if buf.len() > MAX_BODY_BYTES {
        return Err(SiftError::Network(format!(
            "response body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(buf)
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
