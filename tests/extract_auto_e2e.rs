//! End-to-end tests for `sift extract --mode auto`.
//!
//! Two flavors of fixture PDFs:
//!
//! * **text-only** — built with `pdf_oxide::writer::DocumentBuilder`'s
//!   `.text(...)` API; scan_detect leaves every page alone.
//! * **scanned-style** — built with the same DocumentBuilder, but
//!   each page gets a full-page raster image embedded so that
//!   scan_detect's "max image bbox covers > 80% of page" rule
//!   trips. We use a 67-byte 1×1 RGBA PNG sized via `Rect::new` to
//!   blanket the page; pdf_oxide's reader recovers the placement
//!   bbox so the unit-test classifier behavior is preserved in the
//!   integration path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use pdf_oxide::geometry::Rect;
use pdf_oxide::writer::{DocumentBuilder, PageSize};
use tempfile::TempDir;

/// Encode a small JPEG via the `image` crate. JPEG (vs PNG) round-
/// trips through `pdf_oxide`'s writer → reader image extraction
/// reliably — PNGs survive the write step but the reader's
/// `extract_images` returns zero entries for them in 0.3.52,
/// presumably because writer-emitted PNG XObjects use an
/// encoding pdf_oxide's reader doesn't yet decode. Only the
/// on-page bbox matters for scan_detect; intrinsic resolution is
/// irrelevant, so 16×16 grey suffices.
fn tiny_jpeg() -> Vec<u8> {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ExtendedColorType, ImageEncoder};
    let mut out: Vec<u8> = Vec::new();
    let pixels = vec![128u8; 16 * 16];
    JpegEncoder::new(&mut out)
        .write_image(&pixels, 16, 16, ExtendedColorType::L8)
        .expect("encode 16×16 JPEG");
    out
}

/// Page kind for the mixed-fixture builder.
#[derive(Clone, Copy)]
enum PageKind<'a> {
    /// Pure-text page rendered via `.text(...)`.
    Text(&'a str),
    /// "Scanned" page — embeds [`ONE_PX_PNG`] sized to cover the
    /// whole Letter page (612 × 792 pt), no text. scan_detect's
    /// "max image bbox covers > 80%" rule will trip on this.
    Scanned,
}

fn build_pdf(dir: &Path, name: &str, kinds: &[PageKind<'_>]) -> PathBuf {
    let mut builder = DocumentBuilder::new();
    for k in kinds {
        match k {
            PageKind::Text(body) => {
                builder
                    .page(PageSize::Letter)
                    .font("Helvetica", 14.0)
                    .at(72.0, 720.0)
                    .text(body)
                    .done();
            }
            PageKind::Scanned => {
                // Cover the entire Letter page (612 × 792 pt).
                let jpg = tiny_jpeg();
                builder
                    .page(PageSize::Letter)
                    .image_from_bytes(&jpg, Rect::new(0.0, 0.0, 612.0, 792.0))
                    .expect("image_from_bytes")
                    .done();
            }
        }
    }
    let bytes = builder.build().expect("pdf_oxide build");
    let path = dir.join(name);
    fs::write(&path, &bytes).unwrap();
    path
}

/// Plant a PDF under `$HOME/.sift/cache/announcements/<id>.pdf`.
fn plant_announcement_pdf(home: &Path, id: &str, kinds: &[PageKind<'_>]) -> PathBuf {
    let dir = home.join(".sift/cache/announcements");
    fs::create_dir_all(&dir).unwrap();
    let pdf_path = dir.join(format!("{id}.pdf"));
    let tmp = TempDir::new().unwrap();
    let built = build_pdf(tmp.path(), "x.pdf", kinds);
    fs::copy(&built, &pdf_path).unwrap();
    pdf_path
}

/// Spawn `sift` with token-mode credentials wired to a mockito
/// server. `token = None` removes both vars so the token-mode
/// preflight raises.
fn run_sift(home: &Path, base: &str, token: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::cargo_bin("sift").unwrap();
    cmd.env("HOME", home)
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

/// Build a mock PaddleOCR `layoutParsingResults` response with one
/// entry per item.
fn ocr_body(texts: &[&str]) -> String {
    let results: Vec<serde_json::Value> = texts
        .iter()
        .map(|t| {
            serde_json::json!({
                "markdown": { "text": t, "images": {} }
            })
        })
        .collect();
    serde_json::json!({ "layoutParsingResults": results }).to_string()
}

// ---------------------------------------------------------------------
// auto-mode behavior
// ---------------------------------------------------------------------

#[test]
fn text_only_pdf_under_auto_makes_zero_api_calls() {
    let mut server = mockito::Server::new();
    let m = server
        .mock("POST", "/layout-parsing")
        .expect(0)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(
        home.path(),
        "100",
        &[
            PageKind::Text("alpha body text"),
            PageKind::Text("beta body text"),
            PageKind::Text("gamma body text"),
        ],
    );

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "100", "--pages", "1-3", "--mode", "auto"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stdout.contains("alpha body text"), "stdout: {stdout}");
    assert!(stdout.contains("beta body text"));
    assert!(stdout.contains("gamma body text"));
    assert!(
        stderr.contains("[info] auto       all pages have text layer"),
        "stderr: {stderr}",
    );
    m.assert();
}

#[test]
fn text_only_auto_succeeds_without_paddleocr_credentials() {
    // The key promise of auto: a text PDF should never demand the
    // OCR token. Mockito is here just to catch any stray HTTP.
    let mut server = mockito::Server::new();
    let m = server
        .mock("POST", "/layout-parsing")
        .expect(0)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(
        home.path(),
        "200",
        &[PageKind::Text("body text")],
    );

    let out = run_sift(
        home.path(),
        &server.url(),
        None, // no credentials
        &["extract", "200", "--mode", "auto"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("body text"));
    m.assert();
}

#[test]
fn missing_credentials_with_scanned_pages_names_the_pages() {
    let mut server = mockito::Server::new();
    let m = server
        .mock("POST", "/layout-parsing")
        .expect(0)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(
        home.path(),
        "300",
        &[
            PageKind::Text("text"),
            PageKind::Scanned,
            PageKind::Scanned,
        ],
    );

    let out = run_sift(
        home.path(),
        &server.url(),
        None,
        &["extract", "300", "--mode", "auto"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("auto detected"),
        "stderr should explain auto's diagnosis: {stderr}"
    );
    assert!(
        stderr.contains("PADDLEOCR_API_BASE"),
        "stderr should mention both credential options: {stderr}"
    );
    // The page list should list the scanned pages (2-3 in this fixture).
    assert!(stderr.contains("2-3"), "stderr should name pages: {stderr}");
    m.assert();
}

#[test]
fn mixed_pdf_escalates_only_scanned_pages_to_ocr() {
    let mut server = mockito::Server::new();
    // Two scanned pages in the fixture → one batch of 2 → one
    // OCR call producing two markdown bodies.
    let body = ocr_body(&["scanned page 2 OCR text", "scanned page 3 OCR text"]);
    let m = server
        .mock("POST", "/layout-parsing")
        .with_status(200)
        .with_body(body)
        .expect(1)
        .create();

    let home = TempDir::new().unwrap();
    plant_announcement_pdf(
        home.path(),
        "400",
        &[
            PageKind::Text("plain text first"),
            PageKind::Scanned,
            PageKind::Scanned,
        ],
    );

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "400", "--pages", "1-3", "--mode", "auto"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert!(stdout.contains("plain text first"));
    assert!(stdout.contains("scanned page 2 OCR text"));
    assert!(stdout.contains("scanned page 3 OCR text"));
    assert!(
        stderr.contains("[fine] auto: 2 scanned page(s) escalated to OCR (2-3)"),
        "stderr: {stderr}"
    );
    // Per-page OCR md should have landed in the cache.
    for n in [2, 3] {
        assert!(
            home.path()
                .join(format!(".sift/cache/announcements/400/fine/p{n}.md"))
                .exists(),
            "missing cache file for page {n}"
        );
    }
    m.assert();
}

#[test]
fn ocr_chunk_failure_falls_back_to_fast_output_for_those_pages() {
    let mut server = mockito::Server::new();
    // Persistent 503 — the single batch will exhaust retries and
    // the chunk fails entirely. auto should still print SOMETHING
    // (the fast body for those pages) and exit 3 because ALL
    // escalated pages failed.
    let m = server
        .mock("POST", "/layout-parsing")
        .with_status(503)
        .expect_at_least(1)
        .create();
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(
        home.path(),
        "500",
        &[PageKind::Scanned, PageKind::Scanned],
    );

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "500", "--mode", "auto"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("auto: page 1 OCR failed"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("auto: page 2 OCR failed"),
        "stderr: {stderr}"
    );
    m.assert();
    // No cache entries should have been written for the failed chunk.
    for n in [1, 2] {
        assert!(
            !home
                .path()
                .join(format!(".sift/cache/announcements/500/fine/p{n}.md"))
                .exists(),
            "page {n} cache should not exist after failure"
        );
    }
}

#[test]
fn ocr_includes_images_under_announcement_image_dir() {
    let mut server = mockito::Server::new();
    let body = serde_json::json!({
        "layoutParsingResults": [
            {
                "markdown": {
                    "text": "see image ![](orig.png)",
                    "images": { "orig.png": B64.encode(b"\x89PNG\r\nFAKE") },
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
    let home = TempDir::new().unwrap();
    plant_announcement_pdf(home.path(), "600", &[PageKind::Scanned]);

    let out = run_sift(
        home.path(),
        &server.url(),
        Some("tok"),
        &["extract", "600", "--mode", "auto"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let img = home
        .path()
        .join(".sift/cache/announcements/600/images/p1-img01.png");
    assert!(img.exists(), "image should land: {img:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let abs = std::fs::canonicalize(&img).unwrap();
    assert!(
        stdout.contains(&abs.display().to_string()),
        "stdout should reference absolute image path: {stdout}"
    );
}
