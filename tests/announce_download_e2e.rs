//! End-to-end tests for `sift announce download` (Story 04).
//! Each test isolates `$HOME` to a tempdir, points cninfo PDF
//! downloads at a mockito server through the row's stdin-supplied
//! `url` field, and asserts on stdout / stderr / exit code / on-disk
//! artefacts.

use std::fs;
use std::path::Path;
use std::process::Output;
use std::time::Instant;

use assert_cmd::Command;
use mockito::Server;
use tempfile::TempDir;

/// A 2 KB faux-PDF payload — content does not matter, only that the
/// byte count round-trips through the `fetched N KB` stderr line.
fn faux_pdf() -> Vec<u8> {
    let mut v = Vec::with_capacity(2048);
    v.extend_from_slice(b"%PDF-1.4\n");
    v.resize(2048, b'A');
    v
}

fn ndjson_row(id: &str, url: &str) -> String {
    format!(
        r#"{{"id":"{id}","symbol":"600519.SH","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":2,"url":"{url}","source":"cninfo"}}
"#
    )
}

fn run_with_stdin(home: &Path, args: &[&str], stdin: &[u8]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("HOME", home)
        .env("SIFT_DOWNLOAD_DELAY_MS", "0") // default off; specific tests override
        .args(args)
        .write_stdin(stdin)
        .output()
        .expect("spawn sift")
}

fn run_with_stdin_and_delay(
    home: &Path,
    args: &[&str],
    stdin: &[u8],
    delay_ms: &str,
) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("HOME", home)
        .env("SIFT_DOWNLOAD_DELAY_MS", delay_ms)
        .args(args)
        .write_stdin(stdin)
        .output()
        .expect("spawn sift")
}

#[test]
fn first_download_writes_cache_and_target_and_logs_fetched() {
    let mut server = Server::new();
    let body = faux_pdf();
    let m = server
        .mock("GET", "/p/1219506510.PDF")
        .with_status(200)
        .with_body(body.clone())
        .expect(1)
        .create();
    let home = TempDir::new().unwrap();
    let out_dir = home.path().join("out");
    let url = format!("{}/p/1219506510.PDF", server.url());
    let stdin = ndjson_row("1219506510", &url);

    let out = run_with_stdin(
        home.path(),
        &[
            "announce",
            "download",
            "1219506510",
            "-o",
            out_dir.to_str().unwrap(),
        ],
        stdin.as_bytes(),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("fetched"), "stderr: {stderr}");
    assert!(stderr.contains("KB"), "stderr: {stderr}");
    assert!(stderr.contains("1219506510"), "stderr: {stderr}");

    // Target dir gets the copy.
    let target_pdf = out_dir.join("1219506510.pdf");
    assert!(target_pdf.is_file(), "missing target: {target_pdf:?}");
    assert_eq!(fs::read(&target_pdf).unwrap(), body);

    // Permanent cache also populated.
    let cache_pdf = home
        .path()
        .join(".sift/cache/announcements/1219506510.pdf");
    assert!(cache_pdf.is_file(), "missing cache: {cache_pdf:?}");
    assert_eq!(fs::read(&cache_pdf).unwrap(), body);

    m.assert();
}

#[test]
fn second_download_hits_cache_and_does_not_call_upstream() {
    // Seed the cache directly so we can run with a no-mock server and
    // be sure upstream is never contacted.
    let home = TempDir::new().unwrap();
    let cache_dir = home.path().join(".sift/cache/announcements");
    fs::create_dir_all(&cache_dir).unwrap();
    let pdf = faux_pdf();
    fs::write(cache_dir.join("1219506510.pdf"), &pdf).unwrap();

    let server = Server::new();
    // No mocks created — mockito returns 501 if asked, which would
    // surface as a Network error if the cache path were not taken.
    let out_dir = home.path().join("out");
    let url = format!("{}/p/1219506510.PDF", server.url());
    let stdin = ndjson_row("1219506510", &url);

    let out = run_with_stdin(
        home.path(),
        &[
            "announce",
            "download",
            "1219506510",
            "-o",
            out_dir.to_str().unwrap(),
        ],
        stdin.as_bytes(),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("cached"), "stderr: {stderr}");
    let target_pdf = out_dir.join("1219506510.pdf");
    assert!(target_pdf.is_file());
    assert_eq!(fs::read(&target_pdf).unwrap(), pdf);
}

#[test]
fn missing_output_flag_is_rejected_by_clap_exit_two() {
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["announce", "download", "1219506510"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("--output"), "stderr: {stderr}");
}

#[test]
fn empty_stdin_for_uncached_id_returns_internal_error() {
    // No stdin context + nothing in cache → cannot resolve URL.
    // Subprocess gets an empty stdin (assert_cmd defaults to a closed
    // stdin, which is *not* a TTY — the "stdin is TTY" hint test runs
    // in a separate path that we cannot easily fake here).
    let home = TempDir::new().unwrap();
    let out_dir = home.path().join("out");
    let out = run_with_stdin(
        home.path(),
        &[
            "announce",
            "download",
            "1219506510",
            "-o",
            out_dir.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(out.status.code(), Some(3), "expected Network exit 3");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("1219506510"), "stderr: {stderr}");
    assert!(stderr.contains("failed"), "stderr: {stderr}");
    assert!(!out_dir.exists(), "no copy should land");
}

#[test]
fn partial_failure_continues_batch_and_exits_three() {
    let mut server = Server::new();
    let body = faux_pdf();
    let _m1 = server
        .mock("GET", "/p/AAA.PDF")
        .with_status(200)
        .with_body(body.clone())
        .expect(1)
        .create();
    // Mock for "BBB" returns 404 — should not retry, should not stop the batch.
    let _m2 = server
        .mock("GET", "/p/BBB.PDF")
        .with_status(404)
        .expect(1)
        .create();
    let _m3 = server
        .mock("GET", "/p/CCC.PDF")
        .with_status(200)
        .with_body(body.clone())
        .expect(1)
        .create();
    let home = TempDir::new().unwrap();
    let out_dir = home.path().join("out");
    let url_a = format!("{}/p/AAA.PDF", server.url());
    let url_b = format!("{}/p/BBB.PDF", server.url());
    let url_c = format!("{}/p/CCC.PDF", server.url());
    let mut stdin = String::new();
    stdin.push_str(&ndjson_row("AAA", &url_a));
    stdin.push_str(&ndjson_row("BBB", &url_b));
    stdin.push_str(&ndjson_row("CCC", &url_c));

    let out = run_with_stdin(
        home.path(),
        &[
            "announce",
            "download",
            "AAA",
            "BBB",
            "CCC",
            "-o",
            out_dir.to_str().unwrap(),
        ],
        stdin.as_bytes(),
    );
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    // Three progress / failure lines.
    assert!(stderr.contains("[1/3]") && stderr.contains("AAA"));
    assert!(stderr.contains("[2/3]") && stderr.contains("BBB") && stderr.contains("failed"));
    assert!(stderr.contains("[3/3]") && stderr.contains("CCC"));
    // AAA and CCC must have landed.
    assert!(out_dir.join("AAA.pdf").is_file());
    assert!(out_dir.join("CCC.pdf").is_file());
    assert!(!out_dir.join("BBB.pdf").exists());
}

#[test]
fn polite_delay_kicks_in_for_batches_larger_than_four() {
    let mut server = Server::new();
    let body = faux_pdf();
    // 5 ids, each served by the same mock with `expect(5)`.
    let _m = server
        .mock("GET", mockito::Matcher::Regex(r"^/p/[A-Z]\.PDF$".into()))
        .with_status(200)
        .with_body(body.clone())
        .expect(5)
        .create();

    let home = TempDir::new().unwrap();
    let out_dir = home.path().join("out");
    let mut stdin = String::new();
    let ids = ["A", "B", "C", "D", "E"];
    for id in &ids {
        stdin.push_str(&ndjson_row(id, &format!("{}/p/{id}.PDF", server.url())));
    }

    // 50 ms × 4 inter-request gaps = ≥ 200 ms baseline; with overhead
    // we tolerate up to whatever the test runner takes. The point is
    // that without the delay this completes in well under 100 ms.
    let start = Instant::now();
    let mut args = vec!["announce", "download"];
    args.extend_from_slice(&ids);
    args.push("-o");
    args.push(out_dir.to_str().unwrap());
    let out = run_with_stdin_and_delay(home.path(), &args, stdin.as_bytes(), "50");
    let elapsed = start.elapsed();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // 4 inter-id gaps × 50 ms = 200 ms. CI slack is generous on the
    // upper bound; the lower bound is what we actually assert.
    assert!(
        elapsed.as_millis() >= 180,
        "elapsed {:?} too short — polite delay did not engage",
        elapsed
    );
    for id in &ids {
        assert!(out_dir.join(format!("{id}.pdf")).is_file());
    }
}

#[test]
fn small_batch_skips_polite_delay() {
    // 4 ids = threshold value (POLITE_BATCH_THRESHOLD); story decision
    // is "> 4 triggers", so 4 should not pace.
    let mut server = Server::new();
    let body = faux_pdf();
    let _m = server
        .mock("GET", mockito::Matcher::Regex(r"^/p/[A-Z]\.PDF$".into()))
        .with_status(200)
        .with_body(body)
        .expect(4)
        .create();

    let home = TempDir::new().unwrap();
    let out_dir = home.path().join("out");
    let mut stdin = String::new();
    let ids = ["A", "B", "C", "D"];
    for id in &ids {
        stdin.push_str(&ndjson_row(id, &format!("{}/p/{id}.PDF", server.url())));
    }

    let start = Instant::now();
    let mut args = vec!["announce", "download"];
    args.extend_from_slice(&ids);
    args.push("-o");
    args.push(out_dir.to_str().unwrap());
    // 500 ms delay configured — but with 4 ids it must not be invoked.
    let out = run_with_stdin_and_delay(home.path(), &args, stdin.as_bytes(), "500");
    let elapsed = start.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed.as_millis() < 1500,
        "elapsed {:?} suggests delay engaged for a 4-id batch",
        elapsed
    );
}

#[test]
fn show_reports_cached_yes_after_download_and_no_after_eviction() {
    // Seed the PDF cache directly so this test doesn't depend on the
    // download path (covered above).
    let home = TempDir::new().unwrap();
    let cache_dir = home.path().join(".sift/cache/announcements");
    fs::create_dir_all(&cache_dir).unwrap();
    let cache_pdf = cache_dir.join("1219506510.pdf");
    fs::write(&cache_pdf, b"%PDF-payload").unwrap();

    let stdin_row = ndjson_row("1219506510", "http://static.cninfo.com.cn/p/X.PDF");
    let out = run_with_stdin(
        home.path(),
        &["announce", "show", "1219506510"],
        stdin_row.as_bytes(),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("(yes)"), "stdout: {stdout}");

    // Remove the cached PDF — `show` must now report `(no)`.
    fs::remove_file(&cache_pdf).unwrap();
    let out = run_with_stdin(
        home.path(),
        &["announce", "show", "1219506510"],
        stdin_row.as_bytes(),
    );
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("(no)"), "stdout: {stdout}");
}

#[test]
fn atomic_write_replaces_lingering_tmp_residual() {
    // Confirm Story 04 §5.2 #11: a stray `<id>.pdf.tmp` from a prior
    // interrupted run does not block a subsequent download.
    let mut server = Server::new();
    let body = faux_pdf();
    let _m = server
        .mock("GET", "/p/Z.PDF")
        .with_status(200)
        .with_body(body.clone())
        .expect(1)
        .create();
    let home = TempDir::new().unwrap();
    let cache_dir = home.path().join(".sift/cache/announcements");
    fs::create_dir_all(&cache_dir).unwrap();
    let tmp_residual = cache_dir.join("Z.pdf.tmp");
    fs::write(&tmp_residual, b"garbage").unwrap();

    let out_dir = home.path().join("out");
    let url = format!("{}/p/Z.PDF", server.url());
    let stdin = ndjson_row("Z", &url);
    let out = run_with_stdin(
        home.path(),
        &[
            "announce",
            "download",
            "Z",
            "-o",
            out_dir.to_str().unwrap(),
        ],
        stdin.as_bytes(),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!tmp_residual.exists(), ".tmp should be renamed away");
    let cache_pdf = cache_dir.join("Z.pdf");
    assert_eq!(fs::read(&cache_pdf).unwrap(), body);
}
