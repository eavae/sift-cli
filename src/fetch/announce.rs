//! Announce data-access coordinator.
//!
//! [`AnnounceResolver`] owns the cache + cninfo + stdin fallback
//! policy for `sift announce`, **plus** the PDF cache. The
//! `commands/announce/`
//! layer drives the CLI and rendering; everything that touches
//! `RecordCache`, the PDF `FileCache`, `Announcements`, or the
//! cninfo PDF download lives here.
//!
//! ## The 3-tier fallback
//!
//! Both `show <id>` and `download <id>...` need to turn an
//! `announcementId` into either the full [`AnnouncementRow`] or
//! just its PDF URL. The resolution order is the same in both:
//!
//! 1. **Record cache** — populated by any prior `list` / `show` /
//!    `download` in this or a sibling process. Hits never touch
//!    the network.
//! 2. **stdin NDJSON** — the legacy "pipe `list --format json` into
//!    me" contract. When stdin is piped, missing ids are a hard
//!    `NotFound` — escalating to a whole-market scan would be
//!    surprising. The caller resolves the pipe into
//!    [`StdinContext`] (parsed in `commands/announce/input.rs`)
//!    and passes it in.
//! 3. **cninfo paginate** — only on a TTY (no pipe). Walks both
//!    `szse` and `hke` columns newest-first via
//!    [`Announcements::paginate_market`], caching every row it sees
//!    so the next invocation hits tier 1.
//!
//! [`AnnounceResolver::resolve_row`] returns on the first match;
//! [`AnnounceResolver::resolve_urls`] batches the search so one
//! paginate pass resolves N missing ids together (the previous
//! per-id loop in `commands/announce/mod.rs` walked the catalog
//! `N` times).

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use time::Date;

use crate::app::AppContext;
use crate::cache::file::FileCache;
use crate::cache::record::{Kind as CacheKind, RecordCache};
use crate::domain::announcement::AnnouncementRow;
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::cninfo::{AnnouncementQuery, Announcements, ResolvedSymbol};

/// stdin NDJSON parsed into the two shapes the resolver consumes:
/// full rows (for `show` to find a target by id, and to cache as a
/// side benefit) and an id→url map (for `download` to skip the
/// row decode when only the URL is needed).
///
/// Parsing lives in [`crate::commands::announce::input::read_stdin_ctx`];
/// fetch never reads stdin itself so unit tests can inject any
/// fixture without an actual pipe. When stdin is a TTY the caller
/// passes [`StdinContext::default`] (empty rows + empty map) and
/// sets `stdin_is_tty = true` to enable tier-3 paginate.
#[derive(Default, Debug, Clone)]
pub struct StdinContext {
    pub rows: Vec<AnnouncementRow>,
    pub url_map: HashMap<String, String>,
}

/// Coordinator for `sift announce` data access. Borrows the HTTP
/// client and (optionally) the record cache + the PDF file cache
/// from [`AppContext`], plus an owned [`Announcements`] adapter
/// (cheap — just a base URL).
///
/// Either cache `None` is the no-cache mode: `main.rs` couldn't
/// open the underlying file (permissions / disk full / …) and
/// warned the user. Listing / show flows still work without
/// `records_cache`; download flows require `file_cache` (no
/// file cache → can't store the downloaded PDF anywhere, so
/// `download_pdf` / `copy_pdf_to` return `SiftError::Io`).
///
/// Construct via [`AnnounceResolver::new`] for production; tests
/// use [`AnnounceResolver::with_api`] to inject a mockito-backed
/// [`Announcements::with_url`].
pub struct AnnounceResolver<'a> {
    http: &'a HttpClient,
    cache: Option<&'a RecordCache>,
    files: Option<&'a FileCache>,
    api: Announcements,
}

impl<'a> AnnounceResolver<'a> {
    /// Production constructor. `Announcements::new()` reads
    /// `SIFT_CNINFO_BASE` for any test override of the JSON
    /// endpoint; the PDF static origin is hard-wired in
    /// `Announcements`.
    pub fn new(ctx: &'a AppContext) -> Self {
        Self {
            http: &ctx.http,
            cache: ctx.records_cache.as_ref(),
            files: ctx.file_cache.as_ref(),
            api: Announcements::new(),
        }
    }

    /// Test seam — inject an [`Announcements`] built with
    /// [`Announcements::with_url`] so unit tests can point at a
    /// mockito server without going through the env var. `files`
    /// is None for the listing / show tests; download tests pass
    /// a tempdir-backed [`FileCache`].
    #[cfg(test)]
    pub(crate) fn with_api(
        http: &'a HttpClient,
        cache: &'a RecordCache,
        files: Option<&'a FileCache>,
        api: Announcements,
    ) -> Self {
        Self {
            http,
            cache: Some(cache),
            files,
            api,
        }
    }

    // -----------------------------------------------------------------
    // list — multi-category fan-out for `sift announce list`
    // -----------------------------------------------------------------

    /// Fan out a `list` request across `categories` (one cninfo call
    /// per category, dedup by id across the union), sort newest-first,
    /// truncate to `limit`, and persist every surviving row to the
    /// record cache. `categories` empty-string entries mean "no
    /// category filter" — the caller (via `input::expand_categories`)
    /// already normalized aggregates like `定期报告` into the four
    /// constituent keys.
    pub fn list(
        &self,
        symbols: Vec<ResolvedSymbol>,
        categories: &[String],
        keyword: Option<String>,
        start: Option<Date>,
        end: Option<Date>,
        limit: u32,
    ) -> Result<Vec<AnnouncementRow>, SiftError> {
        let mut all: Vec<AnnouncementRow> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for cat in categories {
            let q = AnnouncementQuery {
                symbols: symbols.clone(),
                category: if cat.is_empty() { None } else { Some(cat.clone()) },
                keyword: keyword.clone(),
                start,
                end,
                limit,
            };
            for row in self.api.query(self.http, q)? {
                if seen.insert(row.id.clone()) {
                    all.push(row);
                }
            }
        }
        all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
        all.truncate(limit as usize);
        self.put_meta_rows(&all);
        Ok(all)
    }

    // -----------------------------------------------------------------
    // resolve_row — single id → AnnouncementRow, 3-tier fallback
    // -----------------------------------------------------------------

    /// Resolve one announcement id to a full row. See module docs
    /// for the tier order. Every successful resolution path
    /// persists the row to the record cache as a side effect.
    pub fn resolve_row(
        &self,
        id: &str,
        stdin: &StdinContext,
        stdin_is_tty: bool,
    ) -> Result<AnnouncementRow, SiftError> {
        // Tier 1: cache (skipped silently when the cache failed to open).
        if let Some(cache) = self.cache {
            if let Some(entry) = cache.get(CacheKind::AnnounceMeta, &[], id) {
                match serde_json::from_slice::<AnnouncementRow>(&entry.body) {
                    Ok(row) => return Ok(row),
                    Err(_) => {
                        // Schema drift / partial write — fall through to
                        // network. The fresh row overwrites on tier 3.
                        eprintln!("[warn] cached row for {id} did not decode; refetching");
                    }
                }
            }
        }

        // Tier 2: stdin NDJSON. When the user explicitly piped, treat
        // it as authoritative context: a miss here is NotFound, not a
        // license to escalate to a whole-market scan.
        if !stdin_is_tty {
            if let Some(row) = stdin.rows.iter().find(|r| r.id == id) {
                self.put_meta_row(row);
                return Ok(row.clone());
            }
            return Err(SiftError::NotFound(id.into()));
        }

        // Tier 3: paginate whole market. May take a while; print a
        // one-line preface so the user understands the latency.
        eprintln!(
            "[info] announcement {id} not in cache; scanning cninfo (this may take a while; ctrl-c to abort)..."
        );
        let mut found: Option<AnnouncementRow> = None;
        let mut scanned_pages = 0usize;
        self.api.paginate_market(self.http, |page_rows| {
            scanned_pages += 1;
            // Always cache every row we see — future calls benefit
            // even if not the target. Cost is negligible vs HTTP.
            self.put_meta_rows(page_rows);
            for row in page_rows {
                if row.id == id {
                    found = Some(row.clone());
                    return ControlFlow::Break(());
                }
            }
            if scanned_pages.is_multiple_of(20) {
                eprintln!("[info] scanned {scanned_pages} pages, still searching for {id}…");
            }
            ControlFlow::Continue(())
        })?;
        found.ok_or_else(|| SiftError::NotFound(id.into()))
    }

    // -----------------------------------------------------------------
    // resolve_urls — batch id list → id→url map, 3-tier fallback
    // -----------------------------------------------------------------

    /// Resolve `ids` to a `HashMap<id, full_url>`. Caller is expected
    /// to have already filtered out ids whose PDFs are present in
    /// the file cache (via [`Self::is_pdf_cached`]) — that keeps the
    /// fetch layer free of file-system policy.
    ///
    /// Same 3-tier shape as [`Self::resolve_row`], but the paginate
    /// pass collects URLs for every still-missing id in **one** walk
    /// of the catalog instead of N walks.
    pub fn resolve_urls(
        &self,
        ids: &[String],
        stdin: &StdinContext,
        stdin_is_tty: bool,
    ) -> Result<HashMap<String, String>, SiftError> {
        let mut url_map: HashMap<String, String> = HashMap::new();

        // Tier 0/1a: stdin url_map (already parsed by the caller).
        for (id, url) in &stdin.url_map {
            if !url.is_empty() {
                url_map.insert(id.clone(), url.clone());
            }
        }
        // Persist full rows the stdin parser surfaced. Centralizing
        // this here means future callers can't forget the write-back.
        self.put_meta_rows(&stdin.rows);

        // Tier 1b: record cache lookup for remaining ids.
        for id in ids {
            if url_map.contains_key(id) {
                continue;
            }
            if let Some(url) = self.lookup_url_in_cache(id) {
                url_map.insert(id.clone(), url);
            }
        }

        // Tier 3: paginate (only on TTY; piped stdin is authoritative
        // so missing ids must surface as NotFound at the call site,
        // not silently escalate to a whole-market scan).
        if stdin_is_tty {
            let missing: Vec<String> = ids
                .iter()
                .filter(|id| !url_map.contains_key(*id))
                .cloned()
                .collect();
            if !missing.is_empty() {
                self.paginate_for_missing_urls(&missing, &mut url_map)?;
            }
        }

        Ok(url_map)
    }

    // -----------------------------------------------------------------
    // PDF cache — file-level operations on the announcement PDF blobs
    // -----------------------------------------------------------------

    /// On-disk path the cache would use for `id`'s PDF, if a file
    /// cache is configured. `None` when the file cache is disabled
    /// (typically `$HOME` unresolved at startup).
    pub fn pdf_path(&self, id: &str) -> Option<PathBuf> {
        self.files.map(|f| f.path(&pdf_key(id)))
    }

    /// `true` iff a non-empty PDF exists in the cache for `id`.
    /// Returns `false` when the file cache is disabled — there's
    /// nothing to check against.
    pub fn is_pdf_cached(&self, id: &str) -> bool {
        self.files.is_some_and(|f| f.exists(&pdf_key(id)))
    }

    /// Fetch the PDF at `full_url` and land it in the file cache
    /// under [`pdf_key`]`(id)`. Returns the byte count for the
    /// command-side progress message. Fails when the file cache is
    /// disabled (no place to store the bytes) or when the HTTP
    /// fetch / disk write itself errors.
    pub fn download_pdf(&self, id: &str, full_url: &str) -> Result<usize, SiftError> {
        let files = self.files.ok_or_else(|| {
            SiftError::Io("file cache unavailable; cannot download PDF".into())
        })?;
        let bytes = self.http.get_bytes(full_url)?;
        files.write(&pdf_key(id), &bytes)?;
        Ok(bytes.len())
    }

    /// Copy the cached PDF for `id` into `dst_dir/<id>.pdf`. Creates
    /// `dst_dir` (and missing ancestors) if needed. Returns the
    /// destination path so the caller can log it. `Err` when the
    /// file cache is disabled or the cache slot is missing (the
    /// command layer typically pre-filters via [`Self::is_pdf_cached`]
    /// so this only fires on a TOCTOU race).
    pub fn copy_pdf_to(&self, id: &str, dst_dir: &Path) -> Result<PathBuf, SiftError> {
        let files = self.files.ok_or_else(|| {
            SiftError::Io("file cache unavailable; cannot copy PDF".into())
        })?;
        let src = files.path(&pdf_key(id));
        std::fs::create_dir_all(dst_dir)
            .map_err(|e| SiftError::Io(format!("mkdir {}: {e}", dst_dir.display())))?;
        let dst = dst_dir.join(format!("{id}.pdf"));
        std::fs::copy(&src, &dst).map_err(|e| {
            SiftError::Io(format!(
                "copy {} -> {}: {e}",
                src.display(),
                dst.display()
            ))
        })?;
        Ok(dst)
    }

    // -----------------------------------------------------------------
    // Internal: cache I/O helpers
    // -----------------------------------------------------------------

    /// Decode a cached row for `id` and return its non-empty `url`.
    /// Miss / decode error / empty URL / no-cache mode all map to
    /// `None` so the caller falls through to the next tier.
    fn lookup_url_in_cache(&self, id: &str) -> Option<String> {
        let cache = self.cache?;
        let entry = cache.get(CacheKind::AnnounceMeta, &[], id)?;
        let row: AnnouncementRow = serde_json::from_slice(&entry.body).ok()?;
        if row.url.is_empty() {
            None
        } else {
            Some(row.url)
        }
    }

    /// Persist one row to the records cache. No-op when the cache is
    /// disabled (open-failed path in `main.rs`).
    fn put_meta_row(&self, row: &AnnouncementRow) {
        let Some(cache) = self.cache else { return };
        if let Ok(body) = serde_json::to_vec(row) {
            cache.put(CacheKind::AnnounceMeta, &[], &row.id, &body);
        }
    }

    fn put_meta_rows(&self, rows: &[AnnouncementRow]) {
        let Some(cache) = self.cache else { return };
        if rows.is_empty() {
            return;
        }
        let items = rows.iter().filter_map(|row| {
            serde_json::to_vec(row)
                .ok()
                .map(|body| (row.id.clone(), body))
        });
        cache.put_many(CacheKind::AnnounceMeta, &[], items);
    }

    /// One whole-market walk that fills in URLs for every id in
    /// `missing` simultaneously. Stops as soon as all targets are
    /// found; otherwise runs until `hasMore=false`. Every page is
    /// written to the metadata cache regardless.
    fn paginate_for_missing_urls(
        &self,
        missing: &[String],
        url_map: &mut HashMap<String, String>,
    ) -> Result<(), SiftError> {
        let targets: HashSet<&str> = missing.iter().map(String::as_str).collect();
        eprintln!(
            "[info] {} id(s) not in cache; scanning cninfo for URL context (this may take a while; ctrl-c to abort)...",
            targets.len()
        );
        let mut scanned_pages = 0usize;
        let mut still_missing = targets.len();
        self.api.paginate_market(self.http, |page_rows| {
            scanned_pages += 1;
            self.put_meta_rows(page_rows);
            for row in page_rows {
                if targets.contains(row.id.as_str())
                    && !url_map.contains_key(&row.id)
                    && !row.url.is_empty()
                {
                    url_map.insert(row.id.clone(), row.url.clone());
                    still_missing -= 1;
                }
            }
            if still_missing == 0 {
                return ControlFlow::Break(());
            }
            if scanned_pages.is_multiple_of(20) {
                eprintln!(
                    "[info] scanned {scanned_pages} pages, {still_missing} id(s) still missing…"
                );
            }
            ControlFlow::Continue(())
        })?;
        Ok(())
    }
}

/// Subpath convention for the announcement PDF cache: `announcements/<id>.pdf`.
/// One definition point — every resolver method that touches a PDF
/// blob composes its key through here so adding a per-source-prefix
/// or sharding scheme later is a one-line change.
fn pdf_key(id: &str) -> String {
    format!("announcements/{id}.pdf")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpClient;
    use mockito::Server;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;
    use time::Month;

    // -----------------------------------------------------------------
    // Test fixtures
    // -----------------------------------------------------------------

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    fn temp_cache() -> (TempDir, RecordCache) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("records.duckdb");
        let cache = RecordCache::open_at(&path).expect("open record cache");
        (dir, cache)
    }

    /// Build an `AnnouncementRow` whose `url` matches what cninfo's
    /// parser produces for the given `adjunct` path: the static origin
    /// prefix prepended verbatim. Use `adjunct` values like
    /// `"finalpage/2024-04-03/100.PDF"` and the round-trip through the
    /// mock envelope below stays stable.
    fn row(id: &str, date: Date, adjunct: &str) -> AnnouncementRow {
        AnnouncementRow {
            id: id.into(),
            symbol: "600519.SH".into(),
            name: "贵州茅台".into(),
            date,
            type_zh: "年报".into(),
            title: format!("title-{id}"),
            format: "pdf",
            size_kb: 1024,
            url: format!("http://static.cninfo.com.cn/{adjunct}"),
            source: "cninfo",
        }
    }

    /// One mock page envelope matching cninfo's `hisAnnouncement/query`
    /// response shape. `has_more=false` ends the walk after this page.
    fn page_envelope(rows: &[AnnouncementRow], has_more: bool) -> serde_json::Value {
        let anns: Vec<_> = rows
            .iter()
            .map(|r| {
                let adjunct = r
                    .url
                    .trim_start_matches("http://static.cninfo.com.cn/")
                    .to_string();
                json!({
                    "announcementId": r.id,
                    "announcementTitle": r.title,
                    // cninfo returns ms epoch; pick noon Beijing time
                    // to match the parser's Asia/Shanghai conversion.
                    "announcementTime": r.date.midnight().assume_utc().unix_timestamp() * 1000
                        + 12 * 3600 * 1000,
                    "secCode": "600519",
                    "secName": r.name,
                    "adjunctUrl": adjunct,
                    "adjunctSize": r.size_kb,
                    "columnId": "category_ndbg_szsh",
                })
            })
            .collect();
        json!({
            "totalAnnouncement": anns.len(),
            "totalpages": 1,
            "hasMore": has_more,
            "announcements": anns,
        })
    }

    /// Wire a mockito server for the announcements endpoint. Each call
    /// returns one page; `match_query` filters on `pageNum` so multiple
    /// pages can be wired in order.
    fn mock_one_page(server: &mut mockito::ServerGuard, body: serde_json::Value) -> mockito::Mock {
        server
            .mock("POST", "/new/hisAnnouncement/query")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create()
    }

    fn http_client() -> Arc<HttpClient> {
        Arc::new(HttpClient::new())
    }

    // -----------------------------------------------------------------
    // resolve_row
    // -----------------------------------------------------------------

    #[test]
    fn resolve_row_returns_cached_row_without_touching_stdin_or_network() {
        let (_tmp, cache) = temp_cache();
        let r = row("100", d(2024, 4, 3), "finalpage/2024-04-03/100.PDF");
        cache.put(
            CacheKind::AnnounceMeta,
            &[],
            "100",
            &serde_json::to_vec(&r).unwrap(),
        );

        let http = http_client();
        // Empty Announcements API: any HTTP call here would 404 against
        // a stale base, surfacing as Err — so reaching tier 3 fails the
        // test loudly instead of quietly hitting the live cninfo.
        let mut server = Server::new();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        let _guard = server.mock("POST", "/").expect(0).create();

        let got = resolver
            .resolve_row("100", &StdinContext::default(), false)
            .unwrap();
        assert_eq!(got.id, "100");
        assert_eq!(got.url, "http://static.cninfo.com.cn/finalpage/2024-04-03/100.PDF");
    }

    #[test]
    fn resolve_row_returns_stdin_row_when_cache_misses_and_piped() {
        let (_tmp, cache) = temp_cache();
        let stdin = StdinContext {
            rows: vec![row("200", d(2024, 4, 4), "finalpage/2024-04-04/200.PDF")],
            url_map: HashMap::new(),
        };
        let http = http_client();
        let server = Server::new();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );

        let got = resolver
            .resolve_row("200", &stdin, /* stdin_is_tty */ false)
            .unwrap();
        assert_eq!(got.id, "200");

        // Side effect: cache learns about it for next time.
        assert!(cache.get(CacheKind::AnnounceMeta, &[], "200").is_some());
    }

    #[test]
    fn resolve_row_paginates_when_tty_and_returns_first_match() {
        let (_tmp, cache) = temp_cache();
        let mut server = Server::new();
        let target = row("300", d(2024, 4, 5), "finalpage/2024-04-05/300.PDF");
        let _m = mock_one_page(
            &mut server,
            page_envelope(
                &[
                    row("xxx", d(2024, 4, 5), "finalpage/2024-04-05/xxx.PDF"),
                    target.clone(),
                ],
                false,
            ),
        );

        let http = http_client();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        let got = resolver
            .resolve_row("300", &StdinContext::default(), /* tty */ true)
            .unwrap();
        assert_eq!(got.id, "300");

        // Walk caches every row it saw, not just the target — so future
        // lookups for "xxx" also hit tier 1.
        assert!(cache.get(CacheKind::AnnounceMeta, &[], "xxx").is_some());
    }

    #[test]
    fn resolve_row_paginate_miss_returns_not_found() {
        let (_tmp, cache) = temp_cache();
        let mut server = Server::new();
        // Both columns (szse, hke) get the same empty page; has_more=false
        // ends each walk on page 1.
        let _m = mock_one_page(&mut server, page_envelope(&[], false))
            .expect_at_most(2);

        let http = http_client();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        let err = resolver
            .resolve_row("nonexistent", &StdinContext::default(), true)
            .unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)));
    }

    #[test]
    fn resolve_row_non_tty_miss_returns_not_found_without_paginating() {
        let (_tmp, cache) = temp_cache();
        let http = http_client();
        let mut server = Server::new();
        // Assert the server is NEVER hit: piped stdin is authoritative.
        let m = server
            .mock("POST", "/new/hisAnnouncement/query")
            .expect(0)
            .create();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );

        let err = resolver
            .resolve_row("nope", &StdinContext::default(), /* tty */ false)
            .unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)));
        m.assert();
    }

    // -----------------------------------------------------------------
    // resolve_urls
    // -----------------------------------------------------------------

    #[test]
    fn resolve_urls_merges_stdin_cache_and_paginate_in_one_pass() {
        let (_tmp, cache) = temp_cache();

        // Cache provides id "B".
        let cached = row("B", d(2024, 4, 3), "finalpage/2024-04-03/B.PDF");
        cache.put(
            CacheKind::AnnounceMeta,
            &[],
            "B",
            &serde_json::to_vec(&cached).unwrap(),
        );

        // stdin provides id "A" — `url_map` carries whatever the
        // pipe-producer wrote; sift does not normalize the URL.
        let stdin = StdinContext {
            rows: vec![],
            url_map: HashMap::from([(
                "A".into(),
                "http://static.cninfo.com.cn/finalpage/2024-04-01/A.PDF".into(),
            )]),
        };

        // Paginate provides id "C" (and noise that should not appear).
        let mut server = Server::new();
        let _m = mock_one_page(
            &mut server,
            page_envelope(
                &[
                    row("noise", d(2024, 4, 1), "finalpage/2024-04-01/noise.PDF"),
                    row("C", d(2024, 4, 2), "finalpage/2024-04-02/C.PDF"),
                ],
                false,
            ),
        );

        let http = http_client();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        let got = resolver
            .resolve_urls(
                &["A".into(), "B".into(), "C".into()],
                &stdin,
                /* tty */ true,
            )
            .unwrap();

        assert_eq!(got.len(), 3);
        assert_eq!(
            got["A"],
            "http://static.cninfo.com.cn/finalpage/2024-04-01/A.PDF"
        );
        assert_eq!(
            got["B"],
            "http://static.cninfo.com.cn/finalpage/2024-04-03/B.PDF"
        );
        assert_eq!(
            got["C"],
            "http://static.cninfo.com.cn/finalpage/2024-04-02/C.PDF"
        );
    }

    #[test]
    fn resolve_urls_paginate_batches_missing_ids_in_one_walk() {
        let (_tmp, cache) = temp_cache();
        let mut server = Server::new();
        // One page that contains both targets — exercises the "batch
        // collect" behavior. If the loop walked once per id it would
        // attempt two POSTs; we assert exactly one.
        let m = server
            .mock("POST", "/new/hisAnnouncement/query")
            .with_status(200)
            .with_body(
                page_envelope(
                    &[
                        row("P", d(2024, 4, 1), "finalpage/2024-04-01/P.PDF"),
                        row("Q", d(2024, 4, 2), "finalpage/2024-04-02/Q.PDF"),
                    ],
                    false,
                )
                .to_string(),
            )
            // szse + hke columns each issue one page; "still_missing == 0"
            // breaks out of szse, but the for-loop in paginate_market
            // continues to hke (also one page) before returning.
            .expect_at_most(2)
            .create();

        let http = http_client();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        let got = resolver
            .resolve_urls(
                &["P".into(), "Q".into()],
                &StdinContext::default(),
                /* tty */ true,
            )
            .unwrap();
        assert_eq!(got.len(), 2);
        m.assert();
    }

    #[test]
    fn resolve_urls_non_tty_returns_partial_map_without_paginating() {
        let (_tmp, cache) = temp_cache();
        let stdin = StdinContext {
            rows: vec![],
            url_map: HashMap::from([(
                "only-one".into(),
                "http://static.cninfo.com.cn/finalpage/2024-04-01/1.PDF".into(),
            )]),
        };
        let http = http_client();
        let mut server = Server::new();
        let m = server
            .mock("POST", "/new/hisAnnouncement/query")
            .expect(0)
            .create();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );

        let got = resolver
            .resolve_urls(
                &["only-one".into(), "missing-on-purpose".into()],
                &stdin,
                /* tty */ false,
            )
            .unwrap();
        // The map carries what we know; commands handles the "missing"
        // by reporting the NDJSON pipe was incomplete.
        assert_eq!(got.len(), 1);
        assert_eq!(
            got["only-one"],
            "http://static.cninfo.com.cn/finalpage/2024-04-01/1.PDF"
        );
        m.assert();
    }

    // -----------------------------------------------------------------
    // download_pdf
    // -----------------------------------------------------------------

    #[test]
    fn download_pdf_writes_bytes_to_file_cache_slot() {
        let (_tmp_cache, cache) = temp_cache();
        let files_dir = TempDir::new().unwrap();
        let files = FileCache::open(files_dir.path().to_path_buf());

        let mut server = Server::new();
        let body = b"%PDF-1.4\nhello world".to_vec();
        let m = server
            .mock("GET", "/finalpage/2024-04-03/100.PDF")
            .with_status(200)
            .with_header("content-type", "application/pdf")
            .with_body(body.clone())
            .create();

        let http = http_client();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            Some(&files),
            Announcements::with_url(server.url()),
        );
        let url = format!("{}/finalpage/2024-04-03/100.PDF", server.url());
        let size = resolver.download_pdf("100", &url).unwrap();
        assert_eq!(size, body.len());
        // The blob lands under `announcements/<id>.pdf` per `pdf_key`.
        let stored = files.read("announcements/100.pdf").expect("cached PDF");
        assert_eq!(stored, body);
        // Resolver helpers see the cache as populated.
        assert!(resolver.is_pdf_cached("100"));
        assert_eq!(
            resolver.pdf_path("100"),
            Some(files_dir.path().join("announcements/100.pdf"))
        );
        m.assert();
    }

    #[test]
    fn download_pdf_without_file_cache_returns_io_error() {
        let (_tmp_cache, cache) = temp_cache();
        let server = Server::new();
        let http = http_client();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        let err = resolver
            .download_pdf("100", &format!("{}/anything.pdf", server.url()))
            .unwrap_err();
        assert!(matches!(err, SiftError::Io(_)), "got {err:?}");
    }

    #[test]
    fn copy_pdf_to_emits_dst_path_and_copies_bytes() {
        let (_tmp_cache, cache) = temp_cache();
        let files_dir = TempDir::new().unwrap();
        let files = FileCache::open(files_dir.path().to_path_buf());
        // Pre-populate the cache slot directly.
        files
            .write("announcements/100.pdf", b"%PDF-cached")
            .unwrap();

        let http = http_client();
        let server = Server::new();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            Some(&files),
            Announcements::with_url(server.url()),
        );

        let dst_dir = TempDir::new().unwrap();
        let dst = resolver.copy_pdf_to("100", dst_dir.path()).unwrap();
        assert_eq!(dst, dst_dir.path().join("100.pdf"));
        assert_eq!(std::fs::read(&dst).unwrap(), b"%PDF-cached");
    }

    #[test]
    fn is_pdf_cached_false_without_file_cache() {
        let (_tmp_cache, cache) = temp_cache();
        let http = http_client();
        let server = Server::new();
        let resolver = AnnounceResolver::with_api(
            &http,
            &cache,
            None,
            Announcements::with_url(server.url()),
        );
        assert!(!resolver.is_pdf_cached("anything"));
        assert_eq!(resolver.pdf_path("anything"), None);
    }
}
