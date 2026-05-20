//! `fetch_stock_lists`: cninfo A-share + HK listings with on-disk
//! caching, 24 h TTL, and stale-fallback on upstream failure.
//!
//! The public entry [`fetch_stock_lists`] hard-wires the production
//! cache root and cninfo base URL as the F1 README decides. The
//! internal [`fetch_with`] is the testing hook — it accepts an
//! isolated cache root, override base URL, and an explicit warn sink
//! so unit tests do not have to manipulate `$HOME` or capture stderr.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::domain::market::Market;
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::cninfo::{self, CnInfoRow, StockLists};

const DEFAULT_BASE: &str = "https://www.cninfo.com.cn";

/// One cninfo endpoint: HTTP path + cache file name + label for
/// messaging. The three live together so adding a third endpoint is a
/// single-line addition.
struct EndpointSpec {
    /// Path appended to the base URL (e.g. `"/new/data/szse_stock.json"`).
    path: &'static str,
    /// Filename under `<cache_root>/cninfo/`.
    cache_file: &'static str,
    /// Short label woven into error / warn messages.
    label: &'static str,
}

const SZSE: EndpointSpec = EndpointSpec {
    path: "/new/data/szse_stock.json",
    cache_file: "szse_stock.json",
    label: "szse_stock",
};

const HKE: EndpointSpec = EndpointSpec {
    path: "/new/data/hke_stock.json",
    cache_file: "hke_stock.json",
    label: "hke_stock",
};

/// Resolve the cninfo base URL. The `SIFT_CNINFO_BASE` env var is a
/// test-only seam: integration tests under `tests/search_e2e.rs` point
/// it at a mockito server. End users never set it; the F1 README
/// intentionally does not document it.
fn cninfo_base() -> String {
    std::env::var("SIFT_CNINFO_BASE").unwrap_or_else(|_| DEFAULT_BASE.into())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SearchCacheOpts {
    /// When true, ignore the existing cache file (skip the freshness
    /// check) but still write the new response on success — so the
    /// next caller benefits from the refresh.
    pub no_cache: bool,
}

/// Read the on-disk cninfo cache and return a `code → zwjc` map
/// without ever triggering a network fetch. Used by F2 to back-fill
/// the security name for rows where the upstream source did not
/// supply one (sina returns blank names). Returns an empty map when
/// the cache files do not exist, are unreadable, or fail to parse —
/// the caller treats that as "no backfill available".
pub fn load_cached_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(root) = super::cache_root() else {
        return map;
    };
    load_cached_names_at(&root.join("cninfo"), &mut map);
    map
}

fn load_cached_names_at(cninfo_dir: &Path, map: &mut HashMap<String, String>) {
    for fname in ["szse_stock.json", "hke_stock.json"] {
        let path = cninfo_dir.join(fname);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(rows) = cninfo::parse_envelope(&bytes, "name-backfill") else {
            continue;
        };
        for r in rows {
            map.insert(r.code, r.zwjc);
        }
    }
}

/// Read the on-disk cninfo cache and return a `code → (Market, org_id)`
/// map without ever triggering a network fetch. Used by F3 to resolve
/// `Symbol → ResolvedSymbol` for the announcements query. Same
/// "missing means empty map" contract as [`load_cached_names`].
pub fn load_cached_org_ids() -> HashMap<String, (Market, String)> {
    let mut map = HashMap::new();
    let Ok(root) = super::cache_root() else {
        return map;
    };
    load_cached_org_ids_at(&root.join("cninfo"), &mut map);
    map
}

/// Test seam for [`load_cached_org_ids`]: read a specific directory
/// instead of `$HOME/.sift/cache/cninfo`.
pub(crate) fn load_cached_org_ids_at(
    cninfo_dir: &Path,
    map: &mut HashMap<String, (Market, String)>,
) {
    for (fname, market) in [
        ("szse_stock.json", Market::CnA),
        ("hke_stock.json", Market::Hk),
    ] {
        let path = cninfo_dir.join(fname);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(rows) = cninfo::parse_envelope(&bytes, "org-id-lookup") else {
            continue;
        };
        for r in rows {
            map.insert(r.code, (market, r.org_id));
        }
    }
}

/// Production entry: caches under `~/.sift/cache/cninfo/`; warnings go
/// to stderr.
pub fn fetch_stock_lists(
    http: &HttpClient,
    opts: SearchCacheOpts,
) -> Result<StockLists, SiftError> {
    let root = super::cache_root()?.join("cninfo");
    let base = cninfo_base();
    let mut warn = std::io::stderr();
    fetch_with(http, opts, &root, &base, &mut warn)
}

fn fetch_with(
    http: &HttpClient,
    opts: SearchCacheOpts,
    root: &Path,
    base_url: &str,
    warn: &mut dyn Write,
) -> Result<StockLists, SiftError> {
    // Per-endpoint independence: a SZSE outage does not invalidate an
    // otherwise healthy HKE cache, and vice versa.
    let cn_a = load_one(http, root, base_url, &SZSE, &opts, warn)?;
    let hk = load_one(http, root, base_url, &HKE, &opts, warn)?;
    Ok(StockLists { cn_a, hk })
}

fn load_one(
    http: &HttpClient,
    root: &Path,
    base_url: &str,
    ep: &EndpointSpec,
    opts: &SearchCacheOpts,
    warn: &mut dyn Write,
) -> Result<Vec<CnInfoRow>, SiftError> {
    let path: PathBuf = root.join(ep.cache_file);
    let url = format!("{base_url}{}", ep.path);

    // Fast path: fresh cache + no `--no-cache` request. A corrupt
    // cache (parse failure) falls through to a refetch rather than
    // bubbling the parse error, since the upstream is the source of
    // truth and a refetch is cheap.
    if !opts.no_cache && super::is_fresh(&path, super::CACHE_TTL_SEARCH_SECS) {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(rows) = cninfo::parse_envelope(&bytes, ep.label) {
                return Ok(rows);
            }
        }
    }

    match http.get_bytes(&url) {
        Ok(bytes) => {
            // Parse first, write only on success — a schema break must
            // never overwrite a (still-valid) prior snapshot.
            let rows = cninfo::parse_envelope(&bytes, ep.label)?;
            super::atomic_write(&path, &bytes)?;
            Ok(rows)
        }
        Err(net_err) => stale_fallback(&path, ep.label, net_err, warn),
    }
}

/// Try to recover from an upstream failure by serving the existing
/// cache (no matter how stale) and emitting a `[warn]` line. Returns
/// the original `net_err` when there is no readable / parseable cache.
fn stale_fallback(
    path: &Path,
    label: &str,
    net_err: SiftError,
    warn: &mut dyn Write,
) -> Result<Vec<CnInfoRow>, SiftError> {
    let Ok(bytes) = std::fs::read(path) else { return Err(net_err) };
    let Ok(rows) = cninfo::parse_envelope(&bytes, label) else { return Err(net_err) };
    let when = super::format_mtime_utc(path);
    let _ = writeln!(
        warn,
        "[warn] cninfo {label}: upstream failed ({net_err}); using cache from {when}",
    );
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    const SAMPLE_SZSE: &str = r#"{"stockList":[{"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"}]}"#;
    const SAMPLE_HKE: &str = r#"{"stockList":[{"code":"00700","zwjc":"腾讯控股","pinyin":"txkg","category":"港股","orgId":"gshk00700"}]}"#;
    const OLD_SZSE: &str = r#"{"stockList":[{"code":"000000","zwjc":"old","pinyin":"old","category":"A股","orgId":"old"}]}"#;

    fn expire(path: &Path) {
        let past = SystemTime::now() - Duration::from_secs(25 * 3600);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(past)).unwrap();
    }

    /// Run `fetch_with` against `server` as the cninfo base. Returns
    /// the parsed lists plus the captured warn-sink bytes.
    fn call(
        opts: SearchCacheOpts,
        root: &Path,
        server: &mockito::Server,
    ) -> Result<(StockLists, Vec<u8>), SiftError> {
        let mut warn = Vec::<u8>::new();
        let lists = fetch_with(
            &HttpClient::new(),
            opts,
            root,
            &server.url(),
            &mut warn,
        )?;
        Ok((lists, warn))
    }

    /// Standard "both endpoints OK" mock setup. Returns the two mocks
    /// so the caller can `.assert()` on them.
    fn mock_ok(server: &mut mockito::Server) -> (mockito::Mock, mockito::Mock) {
        let szse = server
            .mock("GET", SZSE.path)
            .with_status(200)
            .with_body(SAMPLE_SZSE)
            .expect(1)
            .create();
        let hke = server
            .mock("GET", HKE.path)
            .with_status(200)
            .with_body(SAMPLE_HKE)
            .expect(1)
            .create();
        (szse, hke)
    }

    #[test]
    fn first_fetch_writes_both_files_and_returns_rows() {
        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");

        let (lists, warn) = call(SearchCacheOpts::default(), &root, &server).unwrap();

        assert_eq!(lists.cn_a.len(), 1);
        assert_eq!(lists.cn_a[0].code, "600519");
        assert_eq!(lists.hk.len(), 1);
        assert_eq!(lists.hk[0].code, "00700");
        assert!(root.join("szse_stock.json").exists());
        assert!(root.join("hke_stock.json").exists());
        assert!(warn.is_empty(), "no warn on cold fetch");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn cache_hit_skips_http() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("szse_stock.json"), SAMPLE_SZSE).unwrap();
        fs::write(root.join("hke_stock.json"), SAMPLE_HKE).unwrap();
        // No mocks: an unmatched mockito request answers 501, which is
        // not retried, so any HTTP call would surface as Err and fail
        // the test below.
        let server = mockito::Server::new();

        let (lists, warn) = call(SearchCacheOpts::default(), &root, &server).unwrap();

        assert_eq!(lists.cn_a[0].code, "600519");
        assert_eq!(lists.hk[0].code, "00700");
        assert!(warn.is_empty());
    }

    #[test]
    fn no_cache_skips_read_but_writes_fresh() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("szse_stock.json"), OLD_SZSE).unwrap();
        fs::write(root.join("hke_stock.json"), SAMPLE_HKE).unwrap();

        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);

        let (lists, _warn) = call(SearchCacheOpts { no_cache: true }, &root, &server).unwrap();

        assert_eq!(lists.cn_a[0].code, "600519");
        let on_disk = fs::read_to_string(root.join("szse_stock.json")).unwrap();
        assert!(on_disk.contains("600519"));
        m1.assert();
        m2.assert();
    }

    #[test]
    fn ttl_expired_triggers_refetch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");
        fs::create_dir_all(&root).unwrap();
        let szse_path = root.join("szse_stock.json");
        let hke_path = root.join("hke_stock.json");
        fs::write(&szse_path, OLD_SZSE).unwrap();
        fs::write(&hke_path, SAMPLE_HKE).unwrap();
        expire(&szse_path);
        expire(&hke_path);

        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);

        let (lists, _warn) = call(SearchCacheOpts::default(), &root, &server).unwrap();
        assert_eq!(lists.cn_a[0].code, "600519");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn stale_fallback_is_per_endpoint() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");
        fs::create_dir_all(&root).unwrap();
        // Pre-populate only the SZSE side with stale data; HKE comes fresh.
        let szse_path = root.join("szse_stock.json");
        fs::write(&szse_path, OLD_SZSE).unwrap();
        expire(&szse_path);

        let mut server = mockito::Server::new();
        // 1 initial + 3 retries = 4 attempts before giving up.
        let m1 = server
            .mock("GET", SZSE.path)
            .with_status(502)
            .expect(4)
            .create();
        let m2 = server
            .mock("GET", HKE.path)
            .with_status(200)
            .with_body(SAMPLE_HKE)
            .expect(1)
            .create();

        let (lists, warn) = call(SearchCacheOpts::default(), &root, &server).unwrap();

        assert_eq!(lists.cn_a[0].code, "000000", "stale data returned");
        assert_eq!(lists.hk[0].code, "00700", "HK is fresh");
        let msg = String::from_utf8(warn).unwrap();
        assert!(msg.contains("[warn] cninfo szse_stock"), "actual: {msg:?}");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn both_endpoints_fail_no_cache_returns_network_error() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");

        let mut server = mockito::Server::new();
        let m1 = server
            .mock("GET", SZSE.path)
            .with_status(502)
            .expect(4)
            .create();
        // /hke may not be reached because /szse fails first; allow any count.
        let _m2 = server
            .mock("GET", HKE.path)
            .with_status(502)
            .create();

        let err = call(SearchCacheOpts::default(), &root, &server).unwrap_err();
        assert!(matches!(err, SiftError::Network(_)), "got {err:?}");
        assert_eq!(err.exit_code(), 3);
        m1.assert();
    }

    #[test]
    fn schema_error_returns_internal_no_fallback_to_stale() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");
        fs::create_dir_all(&root).unwrap();
        let szse_path = root.join("szse_stock.json");
        fs::write(&szse_path, SAMPLE_SZSE).unwrap();
        expire(&szse_path); // force a refetch

        let mut server = mockito::Server::new();
        let m1 = server
            .mock("GET", SZSE.path)
            .with_status(200)
            .with_body(r#"{"foo":1}"#)
            .expect(1)
            .create();
        let _m2 = server
            .mock("GET", HKE.path)
            .with_status(200)
            .with_body(SAMPLE_HKE)
            .create();

        let err = call(SearchCacheOpts::default(), &root, &server).unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)), "got {err:?}");
        m1.assert();
    }

    #[test]
    fn corrupt_fresh_cache_falls_through_to_fetch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cninfo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("szse_stock.json"), b"not json").unwrap();
        fs::write(root.join("hke_stock.json"), SAMPLE_HKE).unwrap();

        let mut server = mockito::Server::new();
        let m1 = server
            .mock("GET", SZSE.path)
            .with_status(200)
            .with_body(SAMPLE_SZSE)
            .expect(1)
            .create();
        // /hke cache is valid, no HTTP expected.

        let (lists, _warn) = call(SearchCacheOpts::default(), &root, &server).unwrap();
        assert_eq!(lists.cn_a[0].code, "600519");
        let new_content = fs::read_to_string(root.join("szse_stock.json")).unwrap();
        assert!(new_content.contains("600519"));
        m1.assert();
    }
}
