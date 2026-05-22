//! End-to-end tests for `sift quote`. Spawns the actual binary via
//! `assert_cmd`, points it at a mockito-backed `SIFT_EM_QUOTE_BASE`,
//! and asserts on stdout / stderr / exit code.
//!
//! Coverage:
//! - default `Table` format and TSV format produce the documented columns
//! - `--format json` is soft-rejected with a user-facing message, exit 1
//! - multi-symbol partial failure: stdout stays clean, stderr warns
//!   trail stdout, exit 0
//! - multi-symbol all-failure: empty stdout, exit 3 (`AllSourcesFailed`)

use std::process::Output;

use assert_cmd::Command;
use mockito::{Matcher, Server, ServerGuard};

fn em_body(name: &str, code: &str) -> String {
    format!(
        r#"{{"rc":0,"data":{{
            "f43":132359,"f44":133299,"f45":132000,"f46":132100,
            "f47":12674,"f48":1680717318.0,
            "f57":"{code}","f58":"{name}","f60":133069,
            "f86":1747724400,"f169":-710,"f170":-53
        }}}}"#
    )
}

fn run_quote(server: &ServerGuard, args: &[&str]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_EM_QUOTE_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose") // force any cache code paths to fall back to None
        .arg("quote")
        .args(args)
        .output()
        .expect("spawn sift")
}

#[test]
fn default_table_format_outputs_aligned_columns() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/api/qt/stock/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "1.600519".into()))
        .with_status(200)
        .with_body(em_body("贵州茅台", "600519"))
        .create();

    let out = run_quote(&server, &["600519"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // First column of the header row must be `symbol`; columns are
    // padded with spaces, not prefixed with `#` (that is the TSV
    // form).
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("symbol"), "got header: {:?}", lines[0]);
    assert!(lines[1].contains("贵州茅台"), "data row: {:?}", lines[1]);
    assert!(lines[1].contains("1323.59"), "price ÷100: {:?}", lines[1]);
    assert!(lines[1].contains("eastmoney"));
}

#[test]
fn tsv_format_uses_hash_prefix_header_and_tab_separator() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/api/qt/stock/get")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(em_body("贵州茅台", "600519"))
        .create();

    let out = run_quote(&server, &["--format", "tsv", "--", "600519"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let first_line = stdout.lines().next().unwrap();
    assert!(first_line.starts_with("#symbol\t"), "TSV header: {first_line:?}");
    assert!(first_line.contains("\tname\t"));
    assert!(first_line.contains("\tsource"));
}

#[test]
fn json_format_is_soft_rejected_without_leaking_internal_codename() {
    // `--format` is a global option, so clap will not reject `json`
    // on the quote subcommand by itself; only the runtime check
    // inside `run()` rejects it.
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["--format", "json", "quote", "600519"])
        .output()
        .expect("spawn sift");
    // Exit code 1 (SiftError::Internal), not the clap-level 2.
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("`sift quote`"),
        "stderr must name the user-facing command: {stderr:?}",
    );
    assert!(stderr.contains("tsv"), "should mention supported format: {stderr:?}");
    assert!(!stderr.contains("F5"), "must not leak internal codename: {stderr:?}");
}

#[test]
fn partial_failure_keeps_stdout_clean_and_warns_after() {
    let mut server = Server::new();
    let _bad = server
        .mock("GET", "/api/qt/stock/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "0.999999".into()))
        .with_status(404)
        .create();
    let _ok = server
        .mock("GET", "/api/qt/stock/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "1.600519".into()))
        .with_status(200)
        .with_body(em_body("贵州茅台", "600519"))
        .create();

    let out = run_quote(&server, &["999999", "600519", "--format", "tsv"]);
    assert!(out.status.success(), "partial failure should exit 0: {out:?}");

    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();

    // stdout must contain no `[warn]` — data and diagnostics are
    // strictly separated streams.
    assert!(
        !stdout.contains("[warn]"),
        "stdout should not contain any [warn] line: {stdout:?}",
    );
    // stdout contains the successful symbol and not the failed one.
    assert!(stdout.contains("600519"));
    assert!(!stdout.contains("999999"));
    // stderr trails with the failure detail.
    assert!(stderr.contains("[warn] quote 999999"), "stderr: {stderr:?}");
}

#[test]
fn all_failures_produce_empty_stdout_and_exit_3() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/api/qt/stock/get")
        .match_query(Matcher::Any)
        .with_status(404)
        .create();

    let out = run_quote(&server, &["999999", "888888"]);
    // SiftError::AllSourcesFailed → exit code 3.
    assert_eq!(out.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "all-failure stdout must be empty: {:?}", out.stdout);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("all sources failed"), "stderr: {stderr:?}");
}
