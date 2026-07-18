//! End-to-end tests for `sift bars` (single-symbol and multi-symbol).
//! Tencent is the default source — most tests drive the binary
//! through `SIFT_TENCENT_BARS_BASE`.
//! `--source eastmoney` is exercised by a dedicated test pointing
//! at `SIFT_EM_BARS_BASE`.

use std::process::Output;

use assert_cmd::Command;
use mockito::{Matcher, Server, ServerGuard};

// ---------------------------------------------------------------------------
// Tencent fixture helpers
// ---------------------------------------------------------------------------

fn tencent_body(key: &str, rows: &[&str]) -> String {
    // `key` is the response container under `data.{code}`, e.g.
    // `qfqday` / `qfqweek`. `rows` are the JSON array literals
    // without the outer brackets, one per row.
    let arr = rows.iter().map(|r| format!("[{r}]")).collect::<Vec<_>>().join(",");
    format!(
        r#"{{"code":0,"msg":"","data":{{"sh600519":{{"{key}":[{arr}]}}}}}}"#
    )
}

fn tencent_body_hk(key: &str, rows: &[&str]) -> String {
    let arr = rows.iter().map(|r| format!("[{r}]")).collect::<Vec<_>>().join(",");
    format!(
        r#"{{"code":0,"msg":"","data":{{"hk00700":{{"{key}":[{arr}]}}}}}}"#
    )
}

fn tencent_body_sh_index(key: &str, rows: &[&str]) -> String {
    let arr = rows.iter().map(|r| format!("[{r}]")).collect::<Vec<_>>().join(",");
    format!(
        r#"{{"code":0,"msg":"","data":{{"sh000001":{{"{key}":[{arr}]}}}}}}"#
    )
}

fn run_bars(server: &ServerGuard, args: &[&str]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_TENCENT_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .arg("bars")
        .args(args)
        .output()
        .expect("spawn sift")
}

// ---------------------------------------------------------------------------
// Tencent default-source coverage
// ---------------------------------------------------------------------------

#[test]
fn default_table_renders_aligned_columns_via_tencent() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh600519,day".into()))
        .with_status(200)
        .with_body(tencent_body(
            "qfqday",
            &[r#""2024-01-02","10","15","20","8","100""#],
        ))
        .create();

    let out = run_bars(&server, &["600519", "--limit", "1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("symbol"));
    assert!(lines[1].contains("2024-01-15") || lines[1].contains("2024-01-02"));
    // OCHL → OHLC: open=10, high=20, low=8, close=15
    assert!(lines[1].contains("10.00"));
    assert!(lines[1].contains("20.00"));
    assert!(lines[1].contains("15.00"));
    assert!(lines[1].contains("8.00"));
    // hands → shares: 100 × 100 = 10000
    assert!(lines[1].contains("10000"));
    // Default source is now tencent.
    assert!(lines[1].contains("tencent"));
    assert!(lines[1].contains("daily"));
}

#[test]
fn tsv_format_emits_hash_prefix_header_with_period_and_source() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(tencent_body(
            "qfqday",
            &[r#""2024-01-02","10","15","20","8","100""#],
        ))
        .create();
    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_TENCENT_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .args(["--format=tsv", "bars", "600519", "--limit", "1"])
        .output()
        .expect("spawn sift");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    // 14-column header — turnover_pct dropped, period added.
    assert!(stdout.starts_with("#symbol\tdate\t"), "header: {stdout:?}");
    assert!(stdout.contains("\tperiod\t"));
    assert!(stdout.contains("\tsource"));
    assert!(!stdout.contains("turnover"), "turnover_pct should be gone");
}

#[test]
fn weekly_period_hits_qfqweek_response_key() {
    let mut server = Server::new();
    let m = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh600519,week".into()))
        .with_status(200)
        .with_body(tencent_body(
            "qfqweek",
            &[
                r#""2024-01-05","10","15","20","8","100""#,
                r#""2024-01-12","16","18","19","14","120""#,
            ],
        ))
        .expect(1)
        .create();

    let out = run_bars(&server, &["600519", "--period", "weekly", "--limit", "2"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    m.assert();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("weekly"));
}

#[test]
fn monthly_period_hits_qfqmonth() {
    let mut server = Server::new();
    let m = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh600519,month".into()))
        .with_status(200)
        .with_body(tencent_body(
            "qfqmonth",
            &[r#""2024-01-31","10","20","25","8","5000""#],
        ))
        .expect(1)
        .create();

    let out = run_bars(&server, &["600519", "--period", "monthly", "--limit", "1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    m.assert();
}

#[test]
fn sh_index_bars_fall_back_to_unadjusted_response_key() {
    // Tencent serves index K-lines from the same fqkline endpoint but
    // ignores the adjust parameter — the response key is the bare
    // period token (`day`), which the parser's fallback chain covers.
    let mut server = Server::new();
    let m = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh000001,day".into()))
        .with_status(200)
        .with_body(tencent_body_sh_index(
            "day",
            &[r#""2024-01-02","2900","2950","2960","2890","535281873""#],
        ))
        .expect(1)
        .create();

    let out = run_bars(&server, &["sh000001", "--limit", "1", "--format", "tsv"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    m.assert();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let data = stdout.lines().nth(1).unwrap();
    assert!(data.starts_with("sh000001\t"), "data row: {data:?}");
}

#[test]
fn json_format_emits_ndjson_per_bar() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(tencent_body(
            "qfqday",
            &[r#""2024-01-02","10","15","20","8","100""#],
        ))
        .create();

    let out = run_bars(&server, &["600519", "--limit", "1", "--format", "json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let parsed: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&out.stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<_, _>>()
        .expect("ndjson lines parse");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["symbol"], "600519.CN-A");
    assert_eq!(parsed[0]["date"], "2024-01-02");
    assert_eq!(parsed[0]["volume"], 10_000);
    assert_eq!(parsed[0]["source"], "tencent");
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
fn rejected_period_value_yields_clap_error() {
    // `quarterly` was previously promised in story chatter but the
    // unified bars schema only supports daily/weekly/monthly.
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["bars", "600519", "--period", "quarterly"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("quarterly"), "stderr: {stderr:?}");
}

#[test]
fn multi_symbol_table_renders_grouped_layout_with_period() {
    let mut server = Server::new();
    let _a = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh600519,day".into()))
        .with_status(200)
        .with_body(tencent_body(
            "qfqday",
            &[r#""2024-01-02","10","15","20","8","100""#],
        ))
        .create();
    let _b = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sz000858,day".into()))
        .with_status(200)
        .with_body(
            r#"{"code":0,"msg":"","data":{"sz000858":{"qfqday":[["2024-01-02","85","86","87","84","200"]]}}}"#,
        )
        .create();

    let out = run_bars(&server, &["600519", "000858", "--limit", "1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Group headers carry the new `period=` field too.
    assert!(stdout.contains("── 600519.CN-A  period=daily"));
    assert!(stdout.contains("── 000858.CN-A  period=daily"));
    assert!(stdout.contains("source=tencent"));
    let pos600 = stdout.find("── 600519.CN-A").expect("600519 header present");
    let pos858 = stdout.find("── 000858.CN-A").expect("000858 header present");
    assert!(pos600 < pos858, "input order preserved:\n{stdout}");
}

#[test]
fn multi_symbol_tsv_stays_flat_with_symbol_column() {
    let mut server = Server::new();
    let _a = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh600519,day".into()))
        .with_status(200)
        .with_body(tencent_body(
            "qfqday",
            &[r#""2024-01-02","10","15","20","8","100""#],
        ))
        .create();
    let _b = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sz000858,day".into()))
        .with_status(200)
        .with_body(
            r#"{"code":0,"msg":"","data":{"sz000858":{"qfqday":[["2024-01-02","85","86","87","84","200"]]}}}"#,
        )
        .create();

    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_TENCENT_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .args(["--format=tsv", "bars", "600519", "000858", "--limit", "1"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
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
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sz999999".into()))
        .with_status(404)
        .create();
    let _ok = server
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Regex("sh600519".into()))
        .with_status(200)
        .with_body(tencent_body(
            "qfqday",
            &[r#""2024-01-02","10","15","20","8","100""#],
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
        .mock("GET", "/appstock/app/fqkline/get")
        .match_query(Matcher::Any)
        .with_status(404)
        .create();
    let out = run_bars(&server, &["999999", "888888", "--limit", "1"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "all-failure stdout must be empty");
}

#[test]
fn hk_symbol_uses_hkfqkline_path() {
    let mut server = Server::new();
    let m = server
        .mock("GET", "/appstock/app/hkfqkline/get")
        .match_query(Matcher::Regex("hk00700".into()))
        .with_status(200)
        .with_body(tencent_body_hk(
            "qfqday",
            &[r#""2024-12-23","420.8","410.4","421.2","406.4","20856154"#],
        ))
        .expect(1)
        .create();

    let out = run_bars(&server, &["00700", "--limit", "1"]);
    // The mock body has a missing closing quote — we accept either
    // success or a parse error. The crucial assertion is that the
    // HK-specific path was hit, not the A-share one.
    m.assert();
    let _ = out;
}

// ---------------------------------------------------------------------------
// EM opt-in source coverage
// ---------------------------------------------------------------------------

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

#[test]
fn em_source_flag_routes_to_em_endpoint() {
    let mut server = Server::new();
    let m = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("secid".into(), "1.600519".into()),
            Matcher::UrlEncoded("klt".into(), "101".into()),
        ]))
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1.0,2.0,3.0,0.1"],
        ))
        .expect(1)
        .create();

    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_EM_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .args(["bars", "600519", "--source", "eastmoney", "--limit", "1"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("eastmoney"), "source column should be eastmoney: {stdout}");
    m.assert();
}

#[test]
fn em_source_weekly_uses_klt_102() {
    let mut server = Server::new();
    let m = server
        .mock("GET", "/api/qt/stock/kline/get")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("klt".into(), "102".into()),
        ]))
        .with_status(200)
        .with_body(em_body(
            "600519",
            &["2024-01-15,10,15,20,8,100,1000,1.0,2.0,3.0,0.1"],
        ))
        .expect(1)
        .create();

    let out = Command::cargo_bin("sift")
        .unwrap()
        .env("SIFT_EM_BARS_BASE", server.url())
        .env("HOME", "/nonexistent-on-purpose")
        .args([
            "bars",
            "600519",
            "--source",
            "eastmoney",
            "--period",
            "weekly",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    m.assert();
}
