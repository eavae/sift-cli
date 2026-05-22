//! End-to-end tests for `sift bars` (story-02 single-symbol plus
//! story-03 multi-symbol). The single-symbol tests are at the top;
//! multi-symbol (grouped layout, failure tolerance, all-failure)
//! tests come after.

use std::process::Output;

use assert_cmd::Command;
use mockito::{Matcher, Server, ServerGuard};

fn em_body(code: &str, lines: &[&str]) -> String {
    let arr = lines
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"rc":0,"data":{{"code":"{code}","name":"X","klt":101,"klines":[{arr}]}}}}"#,
    )
}

fn run_bars(server: &ServerGuard, args: &[&str]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_EM_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .arg("bars")
        .args(args)
        .output()
        .expect("spawn sift")
}

#[test]
fn default_table_renders_aligned_columns() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "1.600519".into()))
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1.0,2.0,3.0,0.1"],
        ))
        .create();

    let out = run_bars(&server, &["600519", "--limit", "1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("symbol"));
    assert!(lines[1].contains("2024-01-15"));
    // OCHL → OHLC: open=10, high=20, low=8, close=15
    assert!(lines[1].contains("10.00"));
    assert!(lines[1].contains("20.00"));
    assert!(lines[1].contains("15.00"));
    assert!(lines[1].contains("8.00"));
    // hands → shares: 100 × 100 = 10000
    assert!(lines[1].contains("10000"));
}

#[test]
fn tsv_format_emits_hash_prefix_header() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1.0,2.0,3.0,0.1"],
        ))
        .create();
    // Use `--format=value` to keep the global flag from getting
    // tangled with bars' own `--start` / `--limit` parsing order.
    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_EM_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .args(["--format=tsv", "bars", "600519", "--limit", "1"])
        .output()
        .expect("spawn sift");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stdout.starts_with("#symbol\tdate\t"), "header: {stdout:?}");
}

#[test]
fn json_format_is_soft_rejected_without_internal_codename() {
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["--format", "json", "bars", "600519"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("`sift bars`"), "stderr: {stderr:?}");
    assert!(!stderr.contains("F5"), "stderr leaks codename: {stderr:?}");
}

#[test]
fn limit_conflicts_with_start_at_clap_level() {
    // Clap's `conflicts_with` triggers exit code 2 (clap usage error).
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["bars", "600519", "--limit", "5", "--start", "2024-01-01"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn multi_symbol_table_renders_grouped_layout() {
    let mut server = Server::new();
    let _a = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "1.600519".into()))
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1,2,3,0.1"],
        ))
        .create();
    let _b = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "0.000858".into()))
        .with_status(200)
        .with_body(em_body(
            "000858",
            &["2024-01-15,85,86,87,84,200,2000,1.5,1.0,1.5,0.2"],
        ))
        .create();

    let out = run_bars(&server, &["600519", "000858", "--limit", "1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Two group headers, in input order: 600519 first, then 000858.
    let pos600 = stdout.find("── 600519.CN-A").expect("600519 header present");
    let pos858 = stdout.find("── 000858.CN-A").expect("000858 header present");
    assert!(pos600 < pos858, "input order preserved:\n{stdout}");
    // The data rows under each group must not include the symbol
    // literal — that column got hoisted into the group header.
    let data_line = stdout.lines().find(|l| l.starts_with("2024-01-15")).unwrap();
    assert!(!data_line.contains("600519"), "data line leaks symbol: {data_line:?}");
}

#[test]
fn multi_symbol_tsv_stays_flat_with_symbol_column() {
    let mut server = Server::new();
    let _a = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "1.600519".into()))
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1,2,3,0.1"],
        ))
        .create();
    let _b = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "0.000858".into()))
        .with_status(200)
        .with_body(em_body(
            "000858",
            &["2024-01-15,85,86,87,84,200,2000,1.5,1.0,1.5,0.2"],
        ))
        .create();

    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_EM_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .args(["--format=tsv", "bars", "600519", "000858", "--limit", "1"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Flat layout: header carries `symbol`, one row per symbol.
    assert!(stdout.starts_with("#symbol\tdate\t"), "TSV header: {stdout:?}");
    let data_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect();
    assert_eq!(data_lines.len(), 2);
    assert!(data_lines.iter().any(|l| l.starts_with("600519.CN-A\t")));
    assert!(data_lines.iter().any(|l| l.starts_with("000858.CN-A\t")));
}

#[test]
fn multi_symbol_partial_failure_keeps_stdout_clean() {
    let mut server = Server::new();
    let _bad = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "0.999999".into()))
        .with_status(404)
        .create();
    let _ok = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::UrlEncoded("secid".into(), "1.600519".into()))
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1,2,3,0.1"],
        ))
        .create();

    let out = run_bars(&server, &["999999", "600519", "--limit", "1"]);
    assert!(out.status.success(), "partial failure should exit 0");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!stdout.contains("[warn]"), "stdout dirty: {stdout:?}");
    assert!(stdout.contains("── 600519.CN-A"), "missing success group: {stdout:?}");
    assert!(stderr.contains("[warn] bars 999999"), "stderr missing warn: {stderr:?}");
}

#[test]
fn multi_symbol_all_failing_returns_exit_3() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::Any)
        .with_status(404)
        .create();
    let out = run_bars(&server, &["999999", "888888", "--limit", "1"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "all-failure stdout must be empty");
}

#[test]
fn hk_symbol_uses_lmt_param() {
    let mut server = Server::new();
    let m = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("secid".into(), "116.00700".into()),
            Matcher::UrlEncoded("lmt".into(), "1000000".into()),
        ]))
        .with_status(200)
        .with_body(em_body(
            "00700",
            &["2024-01-15,500,510,520,490,100,1000,1.0,2.0,3.0,0.1"],
        ))
        .expect(1)
        .create();

    let out = run_bars(&server, &["00700", "--limit", "1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    m.assert();
}
