//! When a UTC day stops being expected to change.
//!
//! Nothing enforces immutability — a machine can always rewrite its own shard —
//! but a finalized day is one a reader may cache and a writer is not expected
//! to touch, so a rewrite after that point is reported rather than performed
//! silently. The window runs from the *end* of the UTC day, not its start,
//! because a user at `UTC-12` is still living inside a UTC day that ended
//! twelve hours ago; 48 hours past the end covers every real offset plus a log
//! flushed well after the fact.

use jiff::civil::Date;
use jiff::tz::TimeZone;

/// How long after a UTC day ends before its shard is considered settled.
pub const FINALIZE_AFTER_MS: i64 = 48 * 60 * 60 * 1000;

/// Whether `utc_date` is old enough that its shard should no longer change.
///
/// An unparseable date is never finalized: refusing to settle is recoverable,
/// while settling a day that does not exist would freeze a shard nothing can
/// correct.
pub fn is_finalized(utc_date: &str, now_ms: i64) -> bool {
    let Some(ended_at) = day_end_ms(utc_date) else {
        return false;
    };
    now_ms.saturating_sub(ended_at) > FINALIZE_AFTER_MS
}

/// Midnight UTC at the end of the given day, in epoch milliseconds.
fn day_end_ms(utc_date: &str) -> Option<i64> {
    let date: Date = utc_date.parse().ok()?;
    let next = date.tomorrow().ok()?;
    let zoned = next
        .to_datetime(jiff::civil::time(0, 0, 0, 0))
        .to_zoned(TimeZone::UTC)
        .ok()?;
    Some(zoned.timestamp().as_millisecond())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-17 ends at 2026-09-18T00:00Z; 48h later is 2026-09-20T00:00Z.
    const DAY_END: i64 = 1_789_689_600_000;

    #[test]
    fn a_day_that_has_just_ended_is_still_open() {
        assert!(!is_finalized("2026-09-17", DAY_END + 1_000));
    }

    #[test]
    fn a_day_is_still_open_at_exactly_the_window() {
        assert!(!is_finalized("2026-09-17", DAY_END + FINALIZE_AFTER_MS));
    }

    #[test]
    fn a_day_more_than_two_days_past_its_end_is_finalized() {
        assert!(is_finalized("2026-09-17", DAY_END + FINALIZE_AFTER_MS + 1));
    }

    /// The window counts from the end of the day, so a user twelve hours behind
    /// UTC is never told their current day has settled.
    #[test]
    fn today_is_never_finalized() {
        let midday = DAY_END - 12 * 60 * 60 * 1000;
        assert!(!is_finalized("2026-09-17", midday));
    }

    #[test]
    fn a_date_that_cannot_be_read_is_left_open() {
        assert!(!is_finalized(
            "not-a-date",
            DAY_END + 10 * FINALIZE_AFTER_MS
        ));
    }
}
