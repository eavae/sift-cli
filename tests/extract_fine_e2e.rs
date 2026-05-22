//! End-to-end tests for `sift extract --mode fine`. Generates a
//! text-only fixture PDF at runtime via `pdf_oxide`'s
//! `DocumentBuilder`, mocks the PaddleOCR endpoint via `mockito`,
//! and asserts on stdout / stderr / exit code / cache state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use pdf_oxide::writer::{DocumentBuilder, PageSize};
use tempfile::TempDir;

fn make_pdf(dir: &Path, name: &str, page_bodies: &[&str]) -> PathBuf {
    let mut builder = DocumentBuilder::new();
    for body in page_bodies {
        builder
            .page(PageSize::Letter)
            .font("Helvetica", 14.0)
            .at(72.0, 720.0)
            .text(body)
            .done();
    }
    let bytes = builder.build().expect("pdf_oxide build");
    let path = dir.join(name);
    fs::write(&path, &bytes).unwrap();
    path
}

type PageStub<'a> = (&'a str, &'a [(&'a str, &'a [u8])]);

/// Build a mock PaddleOCR response with one `layoutParsingResults`
/// entry per item — each `(text, [(name, bytes)])` pair.
fn ocr_body(pages: &[PageStub<'_>]) -> String {
    let results: Vec<serde_json::Value> = pages
        .iter()
        .map(|(text, images)| {
            let mut imgs = serde_json::Map::new();
            for (name, bytes) in *images {
                imgs.insert(name.to_string(), serde_json::Value::String(B64.encode(bytes)));
            }
            serde_json::json!({
                "markdown": {
                    "text": text,
                    "images": serde_json::Value::Object(imgs),
                }
            })
        })
        .collect();
    serde_json::json!({ "layoutParsingResults": results }).to_string()
}

/// Spawn `sift` under the token-mode backend. `token = None`
/// removes both vars so the preflight raises `OcrTokenMissing`;
/// `token = Some(...)` sets both `PADDLEOCR_API_BASE` and
/// `PADDLEOCR_API_TOKEN` so the token backend selects.
fn run_sift(home: &Path, base: &str, token: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::cargo_bin("sift").unwrap();
    cmd.env("HOME", home)
        // Always wipe the OAuth pair so a developer's `~/.zshrc` doesn't
        // accidentally select that backend during local `cargo test`.
        .env_remove("PADDLEOCR_API_KEY")
        .env_remove("PADDLEOCR_SECRET_KEY")
        .env_remove("SIFT_BAIDU_HOST");
    match token {
        Some(t) => {
            cmd.env("PADDLEOCR_API_BASE", base)
                .env("PADDLEOCR_API_TOKEN", t);
        }
        None => {
            cmd.env_remove("PADDLEOCR_API_BASE")
                .env_remove("PADDLEOCR_API_TOKEN");
        }
    }
    cmd.args(args).output().expect("spawn sift")
}

/// Plant a PDF under `home/.sift/cache/announcements/<id>.pdf` to
/// simulate a prior `sift announce download <id>`.
fn plant_announcement_pdf(home: &Path, id: &str, page_bodies: &[&str]) -> PathBuf {
    let cache_dir = home.join(".sift/cache/announcements");
    fs::create_dir_all(&cache_dir).unwrap();
    let pdf_path = cache_dir.join(format!("{id}.pdf"));
    let tmp = TempDir::new().unwrap();
    let built = make_pdf(tmp.path(), "x.pdf", page_bodies);
    fs::copy(&built, &pdf_path).unwrap();
    pdf_path
}

#[test]
fn missing_token_fails_before_any_http_call() {
    let mut server = mockito::Server::new();
    let m = server
        .mock("POST", "/layout-parsing")
        .expect(0)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "1001", &["a", "b"]);

    let out = run_sift(
        home.path(),
        &server.url(),
        None,
        &["extract", "1001", "--pages", "1-2", "--mode", "fine"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("PADDLEOCR_API_BASE"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("PADDLEOCR_API_TOKEN"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("PADDLEOCR_API_KEY"),
        "stderr: {stderr}"
    );
    m.assert();
}

#[test]
fn single_batch_end_to_end_writes_per_page_md_cache() {
    let mut server = mockito::Server::new();
    let body = ocr_body(&[
        ("# Page 1 OCR\nhello", &[]),
        ("# Page 2 OCR\nworld", &[]),
        ("# Page 3 OCR\n!", &[]),
    ]);
    let m = server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(body)
        .expect(1)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "2001", &["p1", "p2", "p3"]);

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "2001", "--pages", "1-3", "--mode", "fine"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stdout.contains("hello"));
    assert!(stdout.contains("world"));
    assert!(
        stderr.contains("[fine] backend=token cached 0 / API 3 pages (1 batch)"),
        "stderr: {stderr}"
    );
    // Per-page md landed in the cache.
    for n in 1..=3 {
        let p = home
            .path()
            .join(format!(".sift/cache/announcements/2001/fine/p{n}.md"));
        assert!(p.exists(), "missing cache file: {p:?}");
    }
    m.assert();
}

#[test]
fn second_run_hits_cache_and_makes_zero_api_calls() {
    let mut server = mockito::Server::new();
    let body = ocr_body(&[
        ("first page md", &[]),
        ("second page md", &[]),
    ]);
    server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(body.clone())
        .expect(1) // exactly one API call across both runs
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "3001", &["p1", "p2"]);

    // First run — primes the cache.
    let out1 = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "3001", "--pages", "1-2", "--mode", "fine"],
    );
    assert!(out1.status.success());

    // Second run — should be cache-only, zero API hits.
    let out2 = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "3001", "--pages", "1-2", "--mode", "fine"],
    );
    assert!(out2.status.success());
    let stderr2 = String::from_utf8(out2.stderr).unwrap();
    let stdout2 = String::from_utf8(out2.stdout).unwrap();
    assert!(
        stderr2.contains("[fine] backend=token cached 2 / API 0 pages (0 batches)"),
        "stderr: {stderr2}"
    );
    assert!(stdout2.contains("first page md"));
    assert!(stdout2.contains("second page md"));
}

#[test]
fn sixteen_pages_split_into_two_equal_batches() {
    // Pick a page count that splits cleanly into two
    // 8-page chunks so the same mock response (also 8 entries)
    // fits both batches — eliminates request-order non-determinism
    // from the parallel dispatch.
    let mut server = mockito::Server::new();
    let eight = ocr_body(&[
        ("p1", &[]), ("p2", &[]), ("p3", &[]), ("p4", &[]),
        ("p5", &[]), ("p6", &[]), ("p7", &[]), ("p8", &[]),
    ]);
    let m = server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(eight)
        .expect(2)
        .create();
    let home = TempDir::new().unwrap();
    let body_strs: Vec<String> = (1..=16).map(|i| format!("page{i}")).collect();
    let bodies: Vec<&str> = body_strs.iter().map(String::as_str).collect();
    plant_announcement_pdf(home.path(), "4001", &bodies);

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "4001", "--pages", "1-16", "--mode", "fine"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("API 16 pages (2 batches)"),
        "stderr: {stderr}"
    );
    m.assert();
}

#[test]
fn local_pdf_skips_cache_and_announces_disabled() {
    let mut server = mockito::Server::new();
    let body = ocr_body(&[("local md", &[])]);
    server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(body)
        .create();
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let pdf = make_pdf(tmp.path(), "local.pdf", &["only"]);

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", pdf.to_str().unwrap(), "--pages", "1", "--mode", "fine"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("cache=disabled (local PDF"),
        "stderr: {stderr}"
    );
}

#[test]
fn no_cache_flag_re_runs_api_even_when_cached() {
    let mut server = mockito::Server::new();
    let body = ocr_body(&[("first md", &[])]);
    let m = server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(body)
        .expect(2) // once per invocation
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "5001", &["p1"]);

    // Prime.
    let out1 = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "5001", "--pages", "1", "--mode", "fine"],
    );
    assert!(out1.status.success());
    // Re-run with --no-cache; same one mock should fire a second
    // time.
    let out2 = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "5001", "--pages", "1", "--mode", "fine", "--no-cache"],
    );
    assert!(out2.status.success());
    m.assert();
}

#[test]
fn chunk_failure_exits_3_lists_failed_pages() {
    let mut server = mockito::Server::new();
    // Persistent 503 on /layout-parsing — every attempt (including
    // retries) hits the same mock. The retry budget is small, so
    // the chunk eventually gives up.
    let m = server
        .mock("POST", "/layout-parsing")
        .with_status(503)
        .expect_at_least(1)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "6001", &["a", "b"]);

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "6001", "--pages", "1-2", "--mode", "fine"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("page(s) failed"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("1,2"), "stderr: {stderr}");
    // No cache should have been written for the failed chunk.
    let p1 = home
        .path()
        .join(".sift/cache/announcements/6001/fine/p1.md");
    let p2 = home
        .path()
        .join(".sift/cache/announcements/6001/fine/p2.md");
    assert!(!p1.exists(), "p1 must not be cached");
    assert!(!p2.exists(), "p2 must not be cached");
    m.assert();
}

/// Spawn `sift` under the OAuth-mode backend. Sets
/// `PADDLEOCR_API_KEY` + `PADDLEOCR_SECRET_KEY` + the Baidu host
/// override; wipes the token pair so build_client can't fall back.
fn run_sift_oauth(home: &Path, host: &str, args: &[&str]) -> Output {
    let mut cmd = Command::cargo_bin("sift").unwrap();
    cmd.env("HOME", home)
        .env_remove("PADDLEOCR_API_BASE")
        .env_remove("PADDLEOCR_API_TOKEN")
        .env("PADDLEOCR_API_KEY", "ak-test")
        .env("PADDLEOCR_SECRET_KEY", "sk-test")
        .env("SIFT_BAIDU_HOST", host);
    cmd.args(args).output().expect("spawn sift")
}

#[test]
fn oauth_backend_runs_end_to_end_via_baidu_protocol() {
    // Stand up the full Baidu OAuth flow against mockito:
    //   POST /oauth/2.0/token         → access_token
    //   POST .../paddle-vl-parser/task          → task_id
    //   POST .../paddle-vl-parser/task/query    → status=success
    //   GET  parse_result_url                   → per-page JSON
    let mut server = mockito::Server::new();
    let token_body = serde_json::json!({
        "access_token": "baidu-token",
        "expires_in": 2_592_000_u64,
    })
    .to_string();
    let submit_body = serde_json::json!({
        "error_code": 0,
        "result": { "task_id": "task-99" }
    })
    .to_string();
    let parse_path = "/cdn/parse_99.json";
    let parse_url = format!("{}{}", server.url(), parse_path);
    let query_body = serde_json::json!({
        "error_code": 0,
        "result": {
            "status": "success",
            "markdown_url": "",
            "parse_result_url": parse_url,
        }
    })
    .to_string();
    let parse_body = serde_json::json!({
        "pages": [
            { "layouts": [{"text": "page1 layout1"}], "tables": [], "images": [] }
        ]
    })
    .to_string();
    // Every Baidu URL carries an `access_token` query — mockito
    // 1.x's path matcher won't match unless we declare we're OK
    // with arbitrary query strings.
    server
        .mock("POST", "/oauth/2.0/token")
        .match_query(mockito::Matcher::Any)
        .with_status(200)
        .with_body(token_body)
        .create();
    server
        .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task")
        .match_query(mockito::Matcher::Any)
        .with_status(200)
        .with_body(submit_body)
        .create();
    server
        .mock("POST", "/rest/2.0/brain/online/v2/paddle-vl-parser/task/query")
        .match_query(mockito::Matcher::Any)
        .with_status(200)
        .with_body(query_body)
        .create();
    server
        .mock("GET", parse_path)
        .with_status(200)
        .with_body(parse_body)
        .create();

    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "8001", &["one"]);

    let out = run_sift_oauth(
        home.path(),
        &server.url(),
        &["extract", "8001", "--pages", "1", "--mode", "fine"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stdout.contains("page1 layout1"), "stdout: {stdout}");
    assert!(
        stderr.contains("[fine] backend=oauth"),
        "stderr: {stderr}"
    );
    // OAuth path should also write the per-page cache file under
    // the announcementId — same layout as token mode.
    assert!(
        home.path()
            .join(".sift/cache/announcements/8001/fine/p1.md")
            .exists(),
        "cache file missing"
    );
}

#[test]
fn ocr_images_land_with_p_nn_naming_and_absolute_path_in_md() {
    let mut server = mockito::Server::new();
    let body = ocr_body(&[(
        "see image: ![](orig.png)",
        &[("orig.png", b"\x89PNG\r\nFAKE")],
    )]);
    server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(body)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "7001", &["p"]);

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "7001", "--pages", "1", "--mode", "fine"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The renamed image lives under the announcement's images dir.
    let img = home
        .path()
        .join(".sift/cache/announcements/7001/images/p1-img01.png");
    assert!(img.exists(), "image not landed: {img:?}");
    assert_eq!(std::fs::read(&img).unwrap(), b"\x89PNG\r\nFAKE");
    // The markdown reference was rewritten to the absolute path.
    let img_abs = std::fs::canonicalize(&img).unwrap();
    assert!(
        stdout.contains(&img_abs.display().to_string()),
        "stdout: {stdout}"
    );
}
