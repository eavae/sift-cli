//! End-to-end tests for `sift extract --mode fast`. Each test
//! generates a small text-only fixture PDF at runtime via
//! `pdf_oxide`'s `DocumentBuilder` (so nothing binary is checked in),
//! points the binary at a temp `$HOME`, and asserts on
//! stdout / stderr / exit code.
//!
//! Scope: text-type PDF flow + the page-out-of-range guard + image
//! directory override + announcementId-cache-miss path. The
//! scan-detection + image-landing internals are covered by unit
//! tests (see `pdf::scan_detect` + `fetch::extract::tests`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use pdf_oxide::writer::{DocumentBuilder, PageSize};
use tempfile::TempDir;

/// Build a text-only N-page PDF using `pdf_oxide`'s document builder
/// and write it under `dir`. Each page body is "Page <i>: <body>" so
/// integration tests can assert on the rendered markdown.
fn make_text_pdf(dir: &Path, name: &str, page_bodies: &[&str]) -> PathBuf {
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

fn run_sift(home: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("HOME", home)
        .args(args)
        .output()
        .expect("spawn sift")
}

#[test]
fn local_pdf_emits_page_markers_and_info_header() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let pdf = make_text_pdf(
        tmp.path(),
        "three.pdf",
        &["hello sift one", "page two body", "third page text"],
    );

    let out = run_sift(home.path(), &["extract", pdf.to_str().unwrap(), "--pages", "1"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert!(stdout.contains("hello sift one"), "stdout: {stdout}");

    // The [info] header rows defined for story-02.
    assert!(stderr.contains("[info] file"), "stderr: {stderr}");
    assert!(stderr.contains("[info] pages       3"), "stderr: {stderr}");
    assert!(stderr.contains("[info] size"), "stderr: {stderr}");
    assert!(stderr.contains("[info] text_layer"), "stderr: {stderr}");
    assert!(stderr.contains("[info] hint"), "stderr: {stderr}");
}

#[test]
fn pages_out_of_range_returns_parse_error_exit_1() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let pdf = make_text_pdf(tmp.path(), "tiny.pdf", &["only one page"]);

    let out = run_sift(home.path(), &["extract", pdf.to_str().unwrap(), "--pages", "5"]);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("page 5 out of range"), "stderr: {stderr}");
    assert!(stderr.contains("PDF has 1 pages"), "stderr: {stderr}");
}

#[test]
fn missing_local_pdf_returns_io_error_exit_1() {
    let home = TempDir::new().unwrap();
    let out = run_sift(
        home.path(),
        &["extract", "./does-not-exist-anywhere.pdf", "--pages", "1"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("PDF not found"), "stderr: {stderr}");
}

#[test]
fn announcement_id_without_cached_pdf_suggests_download() {
    let home = TempDir::new().unwrap();
    let out = run_sift(home.path(), &["extract", "9999999"]);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("PDF not cached"), "stderr: {stderr}");
    assert!(
        stderr.contains("sift announce download"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("9999999"), "stderr: {stderr}");
}

#[test]
fn full_doc_extraction_emits_full_doc_warning_and_all_pages() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let pdf = make_text_pdf(
        tmp.path(),
        "two.pdf",
        &["first page text", "second page text"],
    );

    let out = run_sift(home.path(), &["extract", pdf.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();

    // Both page bodies land in stdout; the per-page text we
    // baked into the fixture is enough to verify the order.
    let first_pos = stdout.find("first page text").expect("page 1 body");
    let second_pos = stdout.find("second page text").expect("page 2 body");
    assert!(first_pos < second_pos, "page order: {stdout}");
    assert!(
        stderr.contains("[warn] extracting full document (2 pages)"),
        "stderr: {stderr}"
    );
}

#[test]
fn image_dir_override_is_advertised_for_local_pdf_default_only() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let pdf = make_text_pdf(tmp.path(), "x.pdf", &["only text"]);
    let img_dir = tmp.path().join("custom_images");

    let out = run_sift(
        home.path(),
        &[
            "extract",
            pdf.to_str().unwrap(),
            "--pages",
            "1",
            "--image-dir",
            img_dir.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    // The "image_dir" advisory line should only appear when the
    // user did NOT supply --image-dir. With an override present
    // we suppress it.
    assert!(
        !stderr.contains("[info] image_dir"),
        "should not advertise image_dir when user supplied it; stderr: {stderr}"
    );
}

#[test]
fn local_pdf_default_image_dir_is_advertised_on_stderr() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let pdf = make_text_pdf(tmp.path(), "y.pdf", &["only text two"]);

    // Run from the tmp dir so the default "./extracted_images" lands there
    // rather than polluting the repo root.
    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(tmp.path())
        .args(["extract", pdf.to_str().unwrap(), "--pages", "1"])
        .output()
        .expect("spawn sift");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("[info] image_dir"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("extracted_images"),
        "stderr: {stderr}"
    );
}

