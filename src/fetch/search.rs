//! Search data-access coordinator.
//!
//! Owns the listing cache + cninfo fetch + org-id resolution. The
//! public entry points are:
//!
//! - [`load_all_listings`] — returns owned `(Market, CnInfoRow)` pairs;
//!   consumed by `sift search`.
//! - [`resolve_org_id`] — turns a user-supplied code into a
//!   [`ResolvedSymbol`]; consumed by `sift announce`'s symbol
//!   resolution path.
//! - [`cached_names`] — pure cache read returning a `code → 中文简称`
//!   map; consumed by `sift report` to back-fill the `name` column
//!   when the upstream source (sina) leaves it blank.
//!
//! Everything cninfo-specific (JSON envelope shape, endpoint paths,
//! base URL) is imported from `sources::cninfo`; everything
//! filesystem-shaped (atomic write, mtime TTL check, mtime
//! formatting) goes through [`crate::cache::file::FileCache`]. There
//! is no direct `std::fs::*` call against the cache root anywhere in
//! this file — that boundary is what makes the cache root
//! test-injectable.

use std::collections::HashMap;
use std::io::Write;

use crate::app::AppContext;
use crate::cache;
use crate::cache::file::FileCache;
use crate::domain::market::Market;
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::cninfo::{self, cninfo_base, CnInfoRow, ResolvedSymbol, StockLists};

// ---------------------------------------------------------------------------
// Endpoint metadata
// ---------------------------------------------------------------------------

/// Subdir under `<cache_root>/` where listing JSON envelopes live.
/// One definition point so the cache layout stays grep-able.
const LISTINGS_SUBDIR: &str = "cninfo";

/// One cninfo endpoint: HTTP path + cache filename + label for
/// messaging. The three live together so adding a third endpoint is a
/// single-line addition. `cache_file` is the basename only; the full
/// [`FileCache`] key is composed as `<LISTINGS_SUBDIR>/<cache_file>`.
struct EndpointSpec {
    /// Path appended to the base URL (e.g. `"/new/data/szse_stock.json"`).
    path: &'static str,
    /// Basename under `<cache_root>/cninfo/`.
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

impl EndpointSpec {
    /// Compose the `FileCache` key for this endpoint.
    fn key(&self) -> String {
        format!("{LISTINGS_SUBDIR}/{}", self.cache_file)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SearchCacheOpts {
    /// When true, ignore the existing cache file (skip the freshness
    /// check) but still write the new response on success — so the
    /// next caller benefits from the refresh.
    pub no_cache: bool,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Load every cninfo listing (A-share + HK), respecting the 24h
/// on-disk cache TTL. `no_cache=true` bypasses the cache and forces
/// a fresh network fetch (used by `sift search --no-cache`).
///
/// Returns owned `(Market, CnInfoRow)` pairs so the caller can
/// filter/sort by reference without going through the `StockLists`
/// wrapper. The market tag is set per-source-file: `szse_stock.json`
/// → `Market::CnA` (covers SH / SZ / BJ / B-share / CDR despite the
/// file name); `hke_stock.json` → `Market::Hk`.
pub fn load_all_listings(
    ctx: &AppContext,
    no_cache: bool,
) -> Result<Vec<(Market, CnInfoRow)>, SiftError> {
    let lists = list_stocks(ctx, SearchCacheOpts { no_cache })?;
    let mut out: Vec<(Market, CnInfoRow)> =
        Vec::with_capacity(lists.cn_a.len() + lists.hk.len());
    out.extend(lists.cn_a.into_iter().map(|r| (Market::CnA, r)));
    out.extend(lists.hk.into_iter().map(|r| (Market::Hk, r)));
    Ok(out)
}

/// Resolve a single user-supplied code to a [`ResolvedSymbol`].
///
/// Three-step strategy:
/// 1. Read the on-disk listing cache via [`cached_org_ids`] — if the code
///    is present, return immediately (zero network).
/// 2. Cache miss → call [`list_stocks`] with default opts (24h TTL,
///    stale fallback). cninfo's name index is small + fast so this is
///    cheaper than refusing and asking the user to run
///    `sift search` first.
/// 3. Re-read the cache. Still missing → `SiftError::MissingOrgId`,
///    which exits 1 with a hint pointing at the code itself.
pub fn resolve_org_id(ctx: &AppContext, code: &str) -> Result<ResolvedSymbol, SiftError> {
    if let Some((market, org_id)) = cached_org_ids(ctx).get(code) {
        return Ok(ResolvedSymbol {
            code: code.into(),
            org_id: org_id.clone(),
            market: *market,
        });
    }
    let _ = list_stocks(ctx, SearchCacheOpts::default())?;
    if let Some((market, org_id)) = cached_org_ids(ctx).get(code) {
        return Ok(ResolvedSymbol {
            code: code.into(),
            org_id: org_id.clone(),
            market: *market,
        });
    }
    Err(SiftError::MissingOrgId(code.into()))
}

/// Read the on-disk cninfo cache and return a `code → zwjc` map
/// without ever triggering a network fetch. Used by `sift report` to
/// back-fill the security name for rows where the upstream source did not
/// supply one (sina returns blank names). Returns an empty map when
/// the file cache is disabled, the listing files do not exist, are
/// unreadable, or fail to parse — the caller treats that as "no
/// backfill available".
pub fn cached_names(ctx: &AppContext) -> HashMap<String, String> {
    let Some(files) = ctx.file_cache.as_ref() else {
        return HashMap::new();
    };
    cached_names_from(files)
}

fn cached_names_from(files: &FileCache) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for fname in ["szse_stock.json", "hke_stock.json"] {
        let key = format!("{LISTINGS_SUBDIR}/{fname}");
        let Some(bytes) = files.read(&key) else {
            continue;
        };
        let Ok(rows) = cninfo::parse_envelope(&bytes, "name-backfill") else {
            continue;
        };
        for r in rows {
            map.insert(r.code, r.zwjc);
        }
    }
    map
}

/// Read the on-disk cninfo cache and return a `code → (Market, org_id)`
/// map without ever triggering a network fetch. Same "missing means
/// empty map" contract as [`cached_names`].
pub(crate) fn cached_org_ids(ctx: &AppContext) -> HashMap<String, (Market, String)> {
    let Some(files) = ctx.file_cache.as_ref() else {
        return HashMap::new();
    };
    cached_org_ids_from(files)
}

/// Test seam: read from an explicit [`FileCache`] (tempdir-backed in
/// tests) instead of `ctx.file_cache`.
pub(crate) fn cached_org_ids_from(files: &FileCache) -> HashMap<String, (Market, String)> {
    let mut map = HashMap::new();
    for (fname, market) in [
        ("szse_stock.json", Market::CnA),
        ("hke_stock.json", Market::Hk),
    ] {
        let key = format!("{LISTINGS_SUBDIR}/{fname}");
        let Some(bytes) = files.read(&key) else {
            continue;
        };
        let Ok(rows) = cninfo::parse_envelope(&bytes, "org-id-lookup") else {
            continue;
        };
        for r in rows {
            map.insert(r.code, (market, r.org_id));
        }
    }
    map
}

// ---------------------------------------------------------------------------
// list_stocks — cache + cninfo with stale-fallback
// ---------------------------------------------------------------------------

/// Production entry: caches under `<file_cache.root>/cninfo/`;
/// warnings go to stderr. When `ctx.file_cache` is `None` (no
/// `$HOME`), this errors out — search needs a cache root to function
/// (every fetch path expects to write back).
pub fn list_stocks(ctx: &AppContext, opts: SearchCacheOpts) -> Result<StockLists, SiftError> {
    let files = ctx
        .file_cache
        .as_ref()
        .ok_or_else(|| SiftError::Io("cache root unresolved; set $HOME".into()))?;
    let base = cninfo_base();
    let mut warn = std::io::stderr();
    fetch_with(&ctx.http, opts, files, &base, &mut warn)
}

fn fetch_with(
    http: &HttpClient,
    opts: SearchCacheOpts,
    files: &FileCache,
    base_url: &str,
    warn: &mut dyn Write,
) -> Result<StockLists, SiftError> {
    // Per-endpoint independence: a SZSE outage does not invalidate an
    // otherwise healthy HKE cache, and vice versa.
    let cn_a = load_one(http, files, base_url, &SZSE, &opts, warn)?;
    let hk = load_one(http, files, base_url, &HKE, &opts, warn)?;
    Ok(StockLists { cn_a, hk })
}

fn load_one(
    http: &HttpClient,
    files: &FileCache,
    base_url: &str,
    ep: &EndpointSpec,
    opts: &SearchCacheOpts,
    warn: &mut dyn Write,
) -> Result<Vec<CnInfoRow>, SiftError> {
    let key = ep.key();
    let url = format!("{base_url}{}", ep.path);

    // Fast path: fresh cache + no `--no-cache` request. A corrupt
    // cache (parse failure) falls through to a refetch rather than
    // bubbling the parse error, since the upstream is the source of
    // truth and a refetch is cheap.
    if !opts.no_cache && files.is_fresh(&key, cache::CACHE_TTL_SEARCH_SECS) {
        if let Some(bytes) = files.read(&key) {
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
            // Cache write failure is non-fatal: the user already has
            // their data this call; only the next call pays the
            // re-fetch. Matches the project-wide warn-and-continue policy
            // (e.g. `cache::record::RecordCache::put_many`).
            if let Err(e) = files.write(&key, &bytes) {
                let _ = writeln!(
                    warn,
                    "[warn] cninfo {}: cache write failed ({e}); next call will re-fetch",
                    ep.label,
                );
            }
            Ok(rows)
        }
        Err(net_err) => stale_fallback(files, &key, ep.label, net_err, warn),
    }
}

/// Try to recover from an upstream failure by serving the existing
/// cache (no matter how stale) and emitting a `[warn]` line. Returns
/// the original `net_err` when there is no readable / parseable cache.
fn stale_fallback(
    files: &FileCache,
    key: &str,
    label: &str,
    net_err: SiftError,
    warn: &mut dyn Write,
) -> Result<Vec<CnInfoRow>, SiftError> {
    let Some(bytes) = files.read(key) else {
        return Err(net_err);
    };
    let Ok(rows) = cninfo::parse_envelope(&bytes, label) else {
        return Err(net_err);
    };
    let when = files.mtime_str(key);
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

    fn expire(files: &FileCache, key: &str) {
        let past = SystemTime::now() - Duration::from_secs(25 * 3600);
        filetime::set_file_mtime(
            files.path(key),
            filetime::FileTime::from_system_time(past),
        )
        .unwrap();
    }

    /// Run `fetch_with` against `server` as the cninfo base. Returns
    /// the parsed lists plus the captured warn-sink bytes. The cache
    /// lives in `tmp` (a per-test tempdir) so tests are fully
    /// isolated from `$HOME`.
    fn call(
        opts: SearchCacheOpts,
        files: &FileCache,
        server: &mockito::Server,
    ) -> Result<(StockLists, Vec<u8>), SiftError> {
        let mut warn = Vec::<u8>::new();
        let lists = fetch_with(
            &HttpClient::new(),
            opts,
            files,
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

    /// Helper: build a FileCache rooted at `tmp` (so `cninfo/...` keys
    /// land under `tmp/cninfo/...`).
    fn make_files(tmp: &TempDir) -> FileCache {
        FileCache::open(tmp.path().to_path_buf())
    }

    #[test]
    fn first_fetch_writes_both_files_and_returns_rows() {
        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);

        let (lists, warn) = call(SearchCacheOpts::default(), &files, &server).unwrap();

        assert_eq!(lists.cn_a.len(), 1);
        assert_eq!(lists.cn_a[0].code, "600519");
        assert_eq!(lists.hk.len(), 1);
        assert_eq!(lists.hk[0].code, "00700");
        assert!(files.exists("cninfo/szse_stock.json"));
        assert!(files.exists("cninfo/hke_stock.json"));
        assert!(warn.is_empty(), "no warn on cold fetch");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn cache_hit_skips_http() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        files.write("cninfo/szse_stock.json", SAMPLE_SZSE.as_bytes()).unwrap();
        files.write("cninfo/hke_stock.json", SAMPLE_HKE.as_bytes()).unwrap();
        // No mocks: an unmatched mockito request answers 501, which is
        // not retried, so any HTTP call would surface as Err and fail
        // the test below.
        let server = mockito::Server::new();

        let (lists, warn) = call(SearchCacheOpts::default(), &files, &server).unwrap();

        assert_eq!(lists.cn_a[0].code, "600519");
        assert_eq!(lists.hk[0].code, "00700");
        assert!(warn.is_empty());
    }

    #[test]
    fn no_cache_skips_read_but_writes_fresh() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        files.write("cninfo/szse_stock.json", OLD_SZSE.as_bytes()).unwrap();
        files.write("cninfo/hke_stock.json", SAMPLE_HKE.as_bytes()).unwrap();

        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);

        let (lists, _warn) = call(SearchCacheOpts { no_cache: true }, &files, &server).unwrap();

        assert_eq!(lists.cn_a[0].code, "600519");
        let on_disk = files.read("cninfo/szse_stock.json").unwrap();
        assert!(String::from_utf8_lossy(&on_disk).contains("600519"));
        m1.assert();
        m2.assert();
    }

    #[test]
    fn ttl_expired_triggers_refetch() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        files.write("cninfo/szse_stock.json", OLD_SZSE.as_bytes()).unwrap();
        files.write("cninfo/hke_stock.json", SAMPLE_HKE.as_bytes()).unwrap();
        expire(&files, "cninfo/szse_stock.json");
        expire(&files, "cninfo/hke_stock.json");

        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);

        let (lists, _warn) = call(SearchCacheOpts::default(), &files, &server).unwrap();
        assert_eq!(lists.cn_a[0].code, "600519");
        m1.assert();
        m2.assert();
    }

    #[test]
    fn stale_fallback_is_per_endpoint() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        // Pre-populate only the SZSE side with stale data; HKE comes fresh.
        files.write("cninfo/szse_stock.json", OLD_SZSE.as_bytes()).unwrap();
        expire(&files, "cninfo/szse_stock.json");

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

        let (lists, warn) = call(SearchCacheOpts::default(), &files, &server).unwrap();

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
        let files = make_files(&tmp);

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

        let err = call(SearchCacheOpts::default(), &files, &server).unwrap_err();
        assert!(matches!(err, SiftError::Network(_)), "got {err:?}");
        assert_eq!(err.exit_code(), 3);
        m1.assert();
    }

    #[test]
    fn schema_error_returns_internal_no_fallback_to_stale() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        files.write("cninfo/szse_stock.json", SAMPLE_SZSE.as_bytes()).unwrap();
        expire(&files, "cninfo/szse_stock.json"); // force a refetch

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

        let err = call(SearchCacheOpts::default(), &files, &server).unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)), "got {err:?}");
        m1.assert();
    }

    #[test]
    fn cache_write_failure_warns_and_still_returns_rows() {
        // Plant a file at the `cninfo` subdir so `atomic_write`'s
        // `mkdir -p` fails — the cache write becomes unrecoverable
        // without affecting the HTTP response. The fetch must still
        // succeed and emit a warn line per failed endpoint.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("cninfo"), b"in the way").unwrap();
        let files = make_files(&tmp);

        let mut server = mockito::Server::new();
        let (m1, m2) = mock_ok(&mut server);

        let (lists, warn) = call(SearchCacheOpts::default(), &files, &server).unwrap();
        // Rows came back from the network despite cache failure.
        assert_eq!(lists.cn_a[0].code, "600519");
        assert_eq!(lists.hk[0].code, "00700");

        let msg = String::from_utf8(warn).unwrap();
        assert!(
            msg.contains("[warn] cninfo szse_stock: cache write failed"),
            "missing SZSE warn: {msg:?}",
        );
        assert!(
            msg.contains("[warn] cninfo hke_stock: cache write failed"),
            "missing HKE warn: {msg:?}",
        );
        m1.assert();
        m2.assert();
    }

    #[test]
    fn corrupt_fresh_cache_falls_through_to_fetch() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        files.write("cninfo/szse_stock.json", b"not json").unwrap();
        files.write("cninfo/hke_stock.json", SAMPLE_HKE.as_bytes()).unwrap();

        let mut server = mockito::Server::new();
        let m1 = server
            .mock("GET", SZSE.path)
            .with_status(200)
            .with_body(SAMPLE_SZSE)
            .expect(1)
            .create();
        // /hke cache is valid, no HTTP expected.

        let (lists, _warn) = call(SearchCacheOpts::default(), &files, &server).unwrap();
        assert_eq!(lists.cn_a[0].code, "600519");
        let new_content = files.read("cninfo/szse_stock.json").unwrap();
        assert!(String::from_utf8_lossy(&new_content).contains("600519"));
        m1.assert();
    }

    #[test]
    fn cached_org_ids_reads_both_szse_and_hke_files() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        files
            .write(
                "cninfo/szse_stock.json",
                r#"{"stockList":[{"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"}]}"#.as_bytes(),
            )
            .unwrap();
        files
            .write(
                "cninfo/hke_stock.json",
                r#"{"stockList":[{"code":"00700","zwjc":"腾讯","pinyin":"tx","category":"港股","orgId":"gshk0000700"}]}"#.as_bytes(),
            )
            .unwrap();
        let map = cached_org_ids_from(&files);
        assert_eq!(map.len(), 2);
        assert_eq!(map["600519"], (Market::CnA, "gssh0600519".into()));
        assert_eq!(map["00700"], (Market::Hk, "gshk0000700".into()));
    }

    #[test]
    fn cached_org_ids_returns_empty_when_no_files() {
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        let map = cached_org_ids_from(&files);
        assert!(map.is_empty());
    }

    #[test]
    fn cached_names_returns_empty_when_file_cache_disabled() {
        // `ctx.file_cache = None` → `cached_names` returns empty map.
        // Drives the AppContext-level guard in the public function.
        let ctx = AppContext::default();
        assert!(cached_names(&ctx).is_empty());
    }

    // Keep the on-disk layout assertion: tests should also verify that
    // the cache files end up at `<root>/cninfo/<basename>` rather than
    // bare basenames. The standalone fs::read keeps this test path
    // separate from the FileCache helpers (so a future refactor of
    // FileCache's path scheme has to update this assertion explicitly).
    #[test]
    fn cache_layout_pins_cninfo_subdir() {
        let mut server = mockito::Server::new();
        let _ = mock_ok(&mut server);
        let tmp = TempDir::new().unwrap();
        let files = make_files(&tmp);
        let _ = call(SearchCacheOpts::default(), &files, &server).unwrap();
        // Paths land exactly at <tmp>/cninfo/<file>.
        assert!(fs::metadata(tmp.path().join("cninfo/szse_stock.json")).is_ok());
        assert!(fs::metadata(tmp.path().join("cninfo/hke_stock.json")).is_ok());
    }
}
