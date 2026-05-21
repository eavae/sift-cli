//! End-to-end tests for `sift search`. Each test spawns the actual
//! binary (`assert_cmd`), points it at a temporary `$HOME` and an
//! injected cninfo mock (`SIFT_CNINFO_BASE`), and asserts on
//! stdout / stderr / exit code.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use mockito::Server;
use tempfile::TempDir;

const SAMPLE_SZSE: &str = r#"{"stockList":[
    {"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"},
    {"code":"000001","zwjc":"平安银行","pinyin":"payh","category":"A股","orgId":"gssz0000001"}
]}"#;

const SAMPLE_HKE: &str = r#"{"stockList":[
    {"code":"00700","zwjc":"腾讯控股","pinyin":"txkg","category":"港股","orgId":"gshk00700"}
]}"#;

fn mock_cninfo(server: &mut Server) -> (mockito::Mock, mockito::Mock) {
    let m_szse = server
        .mock("GET", "/new/data/szse_stock.json")
        .with_status(200)
        .with_body(SAMPLE_SZSE)
        .create();
    let m_hke = server
        .mock("GET", "/new/data/hke_stock.json")
        .with_status(200)
        .with_body(SAMPLE_HKE)
        .create();
    (m_szse, m_hke)
}

fn run_sift(home: &Path, base: &str, args: &[&str]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("HOME", home)
        .env("SIFT_CNINFO_BASE", base)
        .args(args)
        .output()
        .expect("spawn sift")
}

#[test]
fn default_format_emits_aligned_table() {
    let mut server = Server::new();
    let _mocks = mock_cninfo(&mut server);
    let home = TempDir::new().unwrap();

    let out = run_sift(home.path(), &server.url(), &["search", "茅台"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Header row in declared order.
    assert!(stdout.contains("code"));
    assert!(stdout.contains("name"));
    assert!(stdout.contains("market"));
    assert!(stdout.contains("board"));
    // Row content.
    assert!(stdout.contains("600519"));
    assert!(stdout.contains("贵州茅台"));
    // Market label uppercased for table output.
    assert!(stdout.contains("CN-A"));
    assert!(stdout.contains("sh-main"));
}

#[test]
fn tsv_format_emits_tab_separated_rows() {
    let mut server = Server::new();
    let _mocks = mock_cninfo(&mut server);
    let home = TempDir::new().unwrap();

    let out = run_sift(home.path(), &server.url(), &["search", "茅台", "--format", "tsv"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty(), "stdout empty");
    // Header: canonical `#col1\tcol2\t…` per the project-wide
    // `TabularView` convention. 6 columns → 5 tabs on every line.
    assert_eq!(lines[0], "#code\tname\tmarket\tboard\tcategory\torgId");
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            line.matches('\t').count(),
            5,
            "line {i} has wrong column count: {line:?}"
        );
    }
}

#[test]
fn json_format_emits_ndjson_per_hit() {
    let mut server = Server::new();
    let _mocks = mock_cninfo(&mut server);
    let home = TempDir::new().unwrap();

    let out = run_sift(home.path(), &server.url(), &["search", "茅台", "--format", "json"]);
    assert!(out.status.success());
    // Streaming parse — ensures no array wrapper and one object per line.
    let parsed: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&out.stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<_, _>>()
        .expect("ndjson lines parse");
    assert_eq!(parsed.len(), 1, "茅台 should hit exactly one row");
    let first = &parsed[0];
    assert_eq!(first["code"], "600519");
    assert_eq!(first["zwjc"], "贵州茅台");
    assert_eq!(first["orgId"], "gssh0600519");
    assert_eq!(first["market"], "cn-a");
    assert_eq!(first["board"], "sh-main");
    assert_eq!(first["category"], "A股");
    assert_eq!(first["source"], "cninfo");
}

#[test]
fn format_table_value_is_rejected_by_clap() {
    // No HTTP / HOME setup required — clap rejects before any handler runs.
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["search", "茅台", "--format", "table"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("invalid value 'table'"),
        "stderr: {stderr}"
    );
}

#[test]
fn no_match_exits_with_code_4_and_stderr_hint() {
    let mut server = Server::new();
    let _mocks = mock_cninfo(&mut server);
    let home = TempDir::new().unwrap();

    let out = run_sift(
        home.path(),
        &server.url(),
        &["search", "absolutely_not_present_query"],
    );
    assert_eq!(out.status.code(), Some(4));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no match"), "stderr: {stderr}");
}

#[test]
fn second_call_uses_cache_without_http() {
    let home = TempDir::new().unwrap();

    // First call: populate cache via a live mock, then drop the server.
    {
        let mut server = Server::new();
        let _mocks = mock_cninfo(&mut server);
        let out = run_sift(home.path(), &server.url(), &["search", "茅台"]);
        assert!(out.status.success());
    }

    assert!(home.path().join(".sift/cache/cninfo/szse_stock.json").exists());
    assert!(home.path().join(".sift/cache/cninfo/hke_stock.json").exists());

    // Second call: point at a dead host so any stray HTTP attempt
    // would fail fast. Cache is fresh so HTTP is never tried.
    let out = run_sift(home.path(), "http://127.0.0.1:1", &["search", "茅台"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!stderr.contains("[warn]"), "no stale warn on cache hit; stderr: {stderr}");
}

#[test]
fn no_cache_with_dead_upstream_falls_back_to_stale() {
    let home = TempDir::new().unwrap();

    // Populate cache via a live mock.
    {
        let mut server = Server::new();
        let _mocks = mock_cninfo(&mut server);
        let out = run_sift(home.path(), &server.url(), &["search", "茅台"]);
        assert!(out.status.success());
    }

    // `--no-cache` skips the freshness check and forces a refetch.
    // With upstream dead the loader falls back to the stale on-disk
    // copy and emits the `[warn]` line.
    let out = run_sift(
        home.path(),
        "http://127.0.0.1:1",
        &["search", "茅台", "--no-cache"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("[warn] cninfo"), "stderr: {stderr}");
}

#[test]
fn cache_directory_layout_is_fixed() {
    let mut server = Server::new();
    let _mocks = mock_cninfo(&mut server);
    let home = TempDir::new().unwrap();

    let out = run_sift(home.path(), &server.url(), &["search", "茅台"]);
    assert!(out.status.success());

    let cache_dir = home.path().join(".sift").join("cache").join("cninfo");
    let szse = cache_dir.join("szse_stock.json");
    let hke = cache_dir.join("hke_stock.json");
    assert!(szse.exists(), "{szse:?} missing");
    assert!(hke.exists(), "{hke:?} missing");
    // File should be non-empty (atomic_write completed).
    assert!(std::fs::metadata(&szse).unwrap().len() > 0);
    assert!(std::fs::metadata(&hke).unwrap().len() > 0);
}
