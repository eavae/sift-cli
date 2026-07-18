//! Fetch coordination for `sift market` — whole-market业绩报表
//! snapshots with a record-cache layer.
//!
//! The **raw snapshot** (one `Vec<MarketRow>` per report date) is
//! cached in `records.duckdb` under [`Kind::MarketSnapshot`], with the
//! same three-bucket TTL the F2 report cache uses. Ingest into the
//! fact store is a separate concern owned by `service::facts`.

use time::OffsetDateTime;

use crate::app::AppContext;
use crate::cache::record::Kind;
use crate::cache::ttl::{bucket_for, is_fresh};
use crate::domain::Period;
use crate::error::SiftError;
use crate::sources::eastmoney_screen::{self, MarketRow};

/// Load the whole-market snapshot for `period`. On a fresh cache hit
/// the stored snapshot is returned without any HTTP; otherwise the
/// feed is paginated fresh and written back. `no_cache` forces a
/// refetch (the fresh snapshot is still cached).
pub fn load_snapshot(
    app: &AppContext,
    period: Period,
    no_cache: bool,
) -> Result<Vec<MarketRow>, SiftError> {
    let end = period.end_date();
    let key = fmt_date(end);

    if !no_cache {
        if let Some(rows) = cache_get(app, &key, end) {
            return Ok(rows);
        }
    }

    let rows = eastmoney_screen::fetch_snapshot(&app.http, &eastmoney_screen::base(), end)?;

    if let Some(rc) = app.records_cache.as_ref() {
        match serde_json::to_vec(&rows) {
            Ok(body) => rc.put(Kind::MarketSnapshot, &[], &key, &body),
            Err(e) => eprintln!("[warn] market snapshot cache write skipped: {e}"),
        }
    }
    Ok(rows)
}

/// Fresh-cache read: hit only when the entry exists, its `created_at`
/// is within the TTL bucket for `end`, and the body still decodes.
fn cache_get(app: &AppContext, key: &str, end: time::Date) -> Option<Vec<MarketRow>> {
    let rc = app.records_cache.as_ref()?;
    let entry = rc.get(Kind::MarketSnapshot, &[], key)?;
    let created = entry.created_at?;
    let bucket = bucket_for(end, OffsetDateTime::now_utc().date());
    if !is_fresh(created, bucket) {
        return None;
    }
    serde_json::from_slice(&entry.body).ok()
}

fn fmt_date(d: time::Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}
