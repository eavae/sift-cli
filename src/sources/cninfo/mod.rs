//! cninfo source — stock listings (F1 `search`) **and**
//! announcements query (F3 `announce list / download`).
//!
//! - [`listings`] — F1 listings schema + [`CnInfoRow`] / [`StockLists`] /
//!   [`parse_envelope`]. Field semantics pinned in the F1 README
//!   "数据源与协议".
//! - [`announcements`] — F3 paginated `POST /new/hisAnnouncement/query`
//!   with dedup, multi-column fan-out, and Beijing-timezone date
//!   conversion. [`resolve_org_id`] (this file) is the symbol-resolver
//!   used by callers before issuing a query.
//! - [`download`] — F3 PDF fetch + atomic write.

pub mod announcements;
pub mod download;
pub mod listings;

pub use announcements::{Announcements, AnnouncementQuery};
pub use download::download_pdf;
pub use listings::{CnInfoRow, StockLists};
pub(crate) use listings::parse_envelope;

use crate::cache::search::{fetch_stock_lists, load_cached_org_ids, SearchCacheOpts};
use crate::domain::market::Market;
use crate::error::SiftError;
use crate::http::HttpClient;

// ---------------------------------------------------------------------------
// Shared base URL
// ---------------------------------------------------------------------------

/// Default cninfo base URL. HTTPS — the site supports it and downstream
/// `http.get_bytes` follows redirects, but pinning HTTPS up-front saves
/// the round-trip and matches what a browser would do.
///
/// The PDF static site lives at a separate origin (`static.cninfo.com.cn`)
/// and is HTTP-only by upstream design (see Story 04 §3); it is **not**
/// affected by this base.
const DEFAULT_BASE: &str = "https://www.cninfo.com.cn";
pub(crate) const ANNOUNCEMENT_PATH: &str = "/new/hisAnnouncement/query";
pub(crate) const PAGE_SIZE: u32 = 30;

/// Resolved base URL for cninfo's JSON endpoints. `SIFT_CNINFO_BASE`
/// overrides the default for tests (mockito) and any future integration
/// harness. Single canonical resolver — both `sources::cninfo::*` and
/// `cache::search::*` route through here so the protocol stays
/// consistent across the crate.
pub(crate) fn cninfo_base() -> String {
    std::env::var("SIFT_CNINFO_BASE").unwrap_or_else(|_| DEFAULT_BASE.into())
}

/// User-supplied code paired with its cninfo `orgId` + inferred
/// market. Construct via [`resolve_org_id`] in the command layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSymbol {
    pub code: String,
    pub org_id: String,
    pub market: Market,
}

impl ResolvedSymbol {
    /// Exchange-suffixed display form: `600519.SH`, `00700.HK`.
    /// Derived from `org_id` prefix (cninfo's internal market tag).
    pub fn as_secucode(&self) -> String {
        let suffix = match self.market {
            Market::Hk => "HK",
            Market::Us => "US",
            Market::CnA => match self.org_id.as_str() {
                s if s.starts_with("gssh") => "SH",
                s if s.starts_with("gssz") => "SZ",
                s if s.starts_with("gfbj") || s.starts_with("gsbj") => "BJ",
                _ => "SH",
            },
        };
        format!("{}.{}", self.code, suffix)
    }
}

/// Resolve a single user-supplied code to a `ResolvedSymbol`.
///
/// Three-step strategy:
/// 1. Read the on-disk F1 cache via
///    [`crate::cache::search::load_cached_org_ids`] — if the code is
///    present, return immediately (zero network).
/// 2. Cache miss → call
///    [`crate::cache::search::fetch_stock_lists`] with default opts
///    (24h TTL, stale fallback). cninfo's name index is small + fast
///    so this is cheaper than refusing and asking the user to run
///    `sift search` first.
/// 3. Re-read the cache. Still missing → `SiftError::MissingOrgId`,
///    which exits 1 with a hint pointing at the code itself.
pub fn resolve_org_id(http: &HttpClient, code: &str) -> Result<ResolvedSymbol, SiftError> {
    if let Some((market, org_id)) = load_cached_org_ids().get(code) {
        return Ok(ResolvedSymbol {
            code: code.into(),
            org_id: org_id.clone(),
            market: *market,
        });
    }
    let _ = fetch_stock_lists(http, SearchCacheOpts::default())?;
    if let Some((market, org_id)) = load_cached_org_ids().get(code) {
        return Ok(ResolvedSymbol {
            code: code.into(),
            org_id: org_id.clone(),
            market: *market,
        });
    }
    Err(SiftError::MissingOrgId(code.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_symbol_as_secucode_per_org_id_prefix() {
        let maotai = ResolvedSymbol {
            code: "600519".into(),
            org_id: "gssh0600519".into(),
            market: Market::CnA,
        };
        let pingan_bank = ResolvedSymbol {
            code: "000001".into(),
            org_id: "gssz0000001".into(),
            market: Market::CnA,
        };
        let tencent = ResolvedSymbol {
            code: "00700".into(),
            org_id: "gshk0000700".into(),
            market: Market::Hk,
        };
        let bj = ResolvedSymbol {
            code: "832000".into(),
            org_id: "gfbj0832000".into(),
            market: Market::CnA,
        };
        assert_eq!(maotai.as_secucode(), "600519.SH");
        assert_eq!(pingan_bank.as_secucode(), "000001.SZ");
        assert_eq!(tencent.as_secucode(), "00700.HK");
        assert_eq!(bj.as_secucode(), "832000.BJ");
    }
}
