//! End-to-end tests for `sift announce list` and `sift announce show`.
//! Each test spawns the actual binary (`assert_cmd`), points it at a
//! temporary `$HOME` (so `~/.sift/cache/cninfo/*` is isolated) and an
//! injected cninfo mock (`SIFT_CNINFO_BASE`), and asserts on
//! stdout / stderr / exit code.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use mockito::Server;
use tempfile::TempDir;

const SAMPLE_SZSE: &str = r#"{"stockList":[
    {"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"},
    {"code":"000858","zwjc":"五粮液","pinyin":"wly","category":"A股","orgId":"gssz0000858"}
]}"#;

const SAMPLE_HKE: &str = r#"{"stockList":[
    {"code":"00700","zwjc":"腾讯控股","pinyin":"txkg","category":"港股","orgId":"gshk0000700"}
]}"#;

/// Pre-populate `<home>/.sift/cache/cninfo/{szse,hke}_stock.json` so
/// `resolve_org_id` finds a cache hit and never tries to fetch.
fn seed_cninfo_cache(home: &Path) {
    let dir = home.join(".sift").join("cache").join("cninfo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("szse_stock.json"), SAMPLE_SZSE).unwrap();
    std::fs::write(dir.join("hke_stock.json"), SAMPLE_HKE).unwrap();
}

fn run_sift_with_stdin(home: &Path, base: &str, args: &[&str], stdin: &[u8]) -> Output {
    Command::cargo_bin("sift")
        .unwrap()
        .env("HOME", home)
        .env("SIFT_CNINFO_BASE", base)
        .args(args)
        .write_stdin(stdin)
        .output()
        .expect("spawn sift")
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

/// One cninfo-shaped announcement row used by the mock bodies below.
fn ann_json(id: &str, code: &str, time_ms: i64, column: &str, title: &str) -> String {
    format!(
        r#"{{
            "announcementId":"{id}",
            "secCode":"{code}",
            "secName":"贵州茅台",
            "announcementTime":{time_ms},
            "announcementTitle":"{title}",
            "adjunctUrl":"finalpage/2024-04-03/{id}.PDF",
            "adjunctSize":3481,
            "columnId":"{column}"
        }}"#
    )
}

fn one_row_body(id: &str, code: &str, time_ms: i64, column: &str, title: &str) -> String {
    format!(
        r#"{{"announcements":[{}],"hasMore":false}}"#,
        ann_json(id, code, time_ms, column, title)
    )
}

#[test]
fn list_tty_table_renders_header_and_row() {
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    seed_cninfo_cache(home.path());
    let _m = server
        .mock("POST", "/new/hisAnnouncement/query")
        .with_status(200)
        .with_body(one_row_body(
            "1219506510",
            "600519",
            1_712_073_600_000,
            "category_ndbg_szsh",
            "2023年年度报告",
        ))
        .create();

    let out = run_sift(
        home.path(),
        &server.url(),
        &["announce", "list", "600519", "--type", "年报", "--limit", "5"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Header (tabled-rendered; field names in README order).
    let first = stdout.lines().next().unwrap();
    for col in ["id", "symbol", "name", "date", "type", "title", "size_kb"] {
        assert!(first.contains(col), "header missing {col}: {first}");
    }
    // Body content.
    assert!(stdout.contains("1219506510"));
    assert!(stdout.contains("600519.SH"));
    assert!(stdout.contains("2023年年度报告"));
}

#[test]
fn list_tsv_emits_ten_column_header_and_rows() {
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    seed_cninfo_cache(home.path());
    let _m = server
        .mock("POST", "/new/hisAnnouncement/query")
        .with_status(200)
        .with_body(one_row_body(
            "1219506510",
            "600519",
            1_712_073_600_000,
            "category_ndbg_szsh",
            "2023年年度报告",
        ))
        .create();

    let out = run_sift(
        home.path(),
        &server.url(),
        &[
            "--format", "tsv", "announce", "list", "600519", "--type", "年报",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    // 10 columns means 9 tabs per line. Tabular convention prefixes
    // the header line with `#` so downstream pandas/awk treat it as a
    // comment.
    assert_eq!(
        lines[0],
        "#id\tsymbol\tname\tdate\ttype\ttitle\tformat\tsize_kb\turl\tsource"
    );
    for line in &lines {
        assert_eq!(line.matches('\t').count(), 9, "row: {line:?}");
    }
}

#[test]
fn list_json_emits_ndjson_objects_per_announcement() {
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    seed_cninfo_cache(home.path());
    let _m = server
        .mock("POST", "/new/hisAnnouncement/query")
        .with_status(200)
        .with_body(one_row_body(
            "1219506510",
            "600519",
            1_712_073_600_000,
            "category_ndbg_szsh",
            "2023年年度报告",
        ))
        .create();

    let out = run_sift(
        home.path(),
        &server.url(),
        &[
            "--format", "json", "announce", "list", "600519", "--type", "年报",
        ],
    );
    assert!(out.status.success());
    let parsed: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&out.stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<_, _>>()
        .expect("ndjson parses");
    assert_eq!(parsed.len(), 1);
    let first = &parsed[0];
    assert_eq!(first["id"], "1219506510");
    assert_eq!(first["symbol"], "600519.SH");
    assert_eq!(first["type"], "年报");
    assert_eq!(first["source"], "cninfo");
    assert!(first["url"].as_str().unwrap().contains("static.cninfo"));
}

#[test]
fn list_unknown_type_exits_with_clap_arg_error_listing_all_values() {
    // No HTTP / HOME setup: clap rejects before the handler runs.
    let out = Command::cargo_bin("sift")
        .unwrap()
        .args(["announce", "list", "600519", "--type", "假分类"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("假分类"), "stderr: {stderr}");
    // PossibleValuesParser emits `[possible values: ...]` containing
    // every dictionary value; smoke-check a handful.
    for v in ["年报", "半年报", "一季报", "三季报", "定期报告"] {
        assert!(stderr.contains(v), "stderr missing {v}: {stderr}");
    }
}

#[test]
fn list_unresolved_symbol_after_auto_fetch_returns_missing_org_id() {
    // Empty cache forces a fetch; mock returns a list that does not
    // contain 999999, so resolve_org_id falls through to MissingOrgId.
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    let _m_szse = server
        .mock("GET", "/new/data/szse_stock.json")
        .with_status(200)
        .with_body(SAMPLE_SZSE)
        .create();
    let _m_hke = server
        .mock("GET", "/new/data/hke_stock.json")
        .with_status(200)
        .with_body(SAMPLE_HKE)
        .create();

    let out = run_sift(
        home.path(),
        &server.url(),
        &["announce", "list", "999999"],
    );
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("999999") && stderr.contains("not in cninfo"),
        "stderr: {stderr}"
    );
}

#[test]
fn list_empty_response_renders_just_header_with_exit_zero() {
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    seed_cninfo_cache(home.path());
    let _m = server
        .mock("POST", "/new/hisAnnouncement/query")
        .with_status(200)
        .with_body(r#"{"announcements":null,"hasMore":false}"#)
        .create();

    let out = run_sift(
        home.path(),
        &server.url(),
        &[
            "--format", "tsv", "announce", "list", "600519", "--type", "年报",
        ],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "header only; got: {stdout:?}");
    assert!(lines[0].starts_with("#id\t"), "got: {:?}", lines[0]);
}

#[test]
fn quarterly_aggregate_issues_four_sub_queries_and_emits_info_line() {
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    seed_cninfo_cache(home.path());
    // One mock per category — each returns a distinct row; merge should produce 4.
    for (cat, id, title) in [
        ("category_ndbg_szsh", "A1", "2023年报"),
        ("category_bndbg_szsh", "A2", "2023半年报"),
        ("category_yjdbg_szsh", "A3", "2024一季报"),
        ("category_sjdbg_szsh", "A4", "2023三季报"),
    ] {
        let pattern = format!(r"(^|&)category={cat}(&|$)");
        server
            .mock("POST", "/new/hisAnnouncement/query")
            .match_body(mockito::Matcher::Regex(pattern))
            .with_status(200)
            .with_body(one_row_body(
                id,
                "600519",
                1_712_073_600_000,
                cat,
                title,
            ))
            .expect(1)
            .create();
    }

    let out = run_sift(
        home.path(),
        &server.url(),
        &[
            "--format", "tsv", "announce", "list", "600519", "--type", "定期报告",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("[info]") && stderr.contains("4 次子查询"),
        "stderr: {stderr}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // 4 data rows + 1 header.
    assert_eq!(stdout.lines().count(), 5, "stdout: {stdout}");
    for id in ["A1", "A2", "A3", "A4"] {
        assert!(stdout.contains(id), "missing {id} in {stdout}");
    }
}

#[test]
fn list_then_show_pipeline_finds_row_and_reports_cache_no() {
    let mut server = Server::new();
    let home = TempDir::new().unwrap();
    seed_cninfo_cache(home.path());
    let _m = server
        .mock("POST", "/new/hisAnnouncement/query")
        .with_status(200)
        .with_body(one_row_body(
            "1219506510",
            "600519",
            1_712_073_600_000,
            "category_ndbg_szsh",
            "2023年年度报告",
        ))
        .create();

    let list_out = run_sift(
        home.path(),
        &server.url(),
        &[
            "--format", "json", "announce", "list", "600519", "--type", "年报",
        ],
    );
    assert!(list_out.status.success());

    // Pipe the NDJSON into `show`. cninfo base is irrelevant to show
    // but we still keep HOME isolated.
    let show_out = run_sift_with_stdin(
        home.path(),
        &server.url(),
        &["announce", "show", "1219506510"],
        &list_out.stdout,
    );
    assert!(
        show_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&show_out.stderr)
    );
    let stdout = String::from_utf8(show_out.stdout).unwrap();
    assert!(stdout.contains("id"));
    assert!(stdout.contains("1219506510"));
    assert!(stdout.contains("cached"));
    assert!(stdout.contains("(no)"));
    // The reported cache path lives under `~/.sift/cache/announcements/`.
    assert!(
        stdout.contains("announcements"),
        "expected cache path hint: {stdout}"
    );
}

#[test]
fn show_returns_not_found_when_id_missing_from_stdin() {
    let home = TempDir::new().unwrap();
    let ndjson = r#"{"id":"100","symbol":"600519.SH","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":0,"url":"","source":"cninfo"}
"#;
    let out = run_sift_with_stdin(
        home.path(),
        "http://127.0.0.1:1",
        &["announce", "show", "999"],
        ndjson.as_bytes(),
    );
    assert_eq!(out.status.code(), Some(4));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no match"), "stderr: {stderr}");
}
