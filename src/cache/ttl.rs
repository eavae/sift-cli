//! Three-bucket TTL policy for the financials cache.
//!
//! Per the F2 README "缓存策略" table:
//!
//! | Report age (period_end vs. today) | TTL    |
//! | --- | --- |
//! | > 365 days                        | None (永久) |
//! | 90–365 days                       | 30 days |
//! | < 90 days                         | 24 hours |
//!
//! The "永久" bucket recognises that a report older than a year is
//! essentially set in stone — restatements after that point are rare
//! enough that re-fetching adds zero signal.

use time::{Date, Duration, OffsetDateTime};

/// Which TTL bucket a `period_end` falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlBucket {
    /// `period_end` is < 90 days old. 24-hour TTL.
    Recent,
    /// `period_end` is 90–365 days old. 30-day TTL.
    Mid,
    /// `period_end` is > 365 days old. Effectively permanent.
    Old,
}

impl TtlBucket {
    /// `None` is the sentinel for "永久 / no expiry".
    pub const fn ttl(self) -> Option<Duration> {
        match self {
            TtlBucket::Recent => Some(Duration::hours(24)),
            TtlBucket::Mid => Some(Duration::days(30)),
            TtlBucket::Old => None,
        }
    }
}

/// Classify a report period by its age relative to `today`.
pub fn bucket_for(period_end: Date, today: Date) -> TtlBucket {
    let age = today - period_end;
    if age > Duration::days(365) {
        TtlBucket::Old
    } else if age > Duration::days(90) {
        TtlBucket::Mid
    } else {
        TtlBucket::Recent
    }
}

/// `true` iff a row written at `written_at` is still fresh under
/// `bucket`'s TTL. The `Old` bucket always returns true.
pub fn is_fresh(written_at: OffsetDateTime, bucket: TtlBucket) -> bool {
    let Some(ttl) = bucket.ttl() else {
        return true;
    };
    OffsetDateTime::now_utc() - written_at < ttl
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    #[test]
    fn bucket_for_classifies_by_age() {
        let today = d(2026, 5, 20);
        // > 365 days old → Old.
        assert_eq!(bucket_for(d(2020, 12, 31), today), TtlBucket::Old);
        assert_eq!(bucket_for(d(2024, 12, 31), today), TtlBucket::Old);
        // 90–365 days → Mid.
        assert_eq!(bucket_for(d(2025, 9, 30), today), TtlBucket::Mid);
        // < 90 days → Recent.
        assert_eq!(bucket_for(d(2026, 3, 31), today), TtlBucket::Recent);
        // Today exactly → Recent (age = 0).
        assert_eq!(bucket_for(today, today), TtlBucket::Recent);
    }

    #[test]
    fn ttl_table_matches_readme() {
        assert_eq!(TtlBucket::Recent.ttl(), Some(Duration::hours(24)));
        assert_eq!(TtlBucket::Mid.ttl(), Some(Duration::days(30)));
        assert_eq!(TtlBucket::Old.ttl(), None);
    }

    #[test]
    fn old_bucket_is_always_fresh() {
        let very_old = OffsetDateTime::now_utc() - Duration::days(365 * 5);
        assert!(is_fresh(very_old, TtlBucket::Old));
    }

    #[test]
    fn recent_bucket_expires_after_24h() {
        let just_now = OffsetDateTime::now_utc() - Duration::hours(1);
        assert!(is_fresh(just_now, TtlBucket::Recent));
        let yesterday_plus = OffsetDateTime::now_utc() - Duration::hours(25);
        assert!(!is_fresh(yesterday_plus, TtlBucket::Recent));
    }

    #[test]
    fn mid_bucket_expires_after_30_days() {
        let recent = OffsetDateTime::now_utc() - Duration::days(15);
        assert!(is_fresh(recent, TtlBucket::Mid));
        let stale = OffsetDateTime::now_utc() - Duration::days(31);
        assert!(!is_fresh(stale, TtlBucket::Mid));
    }
}
