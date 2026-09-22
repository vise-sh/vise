//! The schedule seam: the one place that knows how a cron string turns into
//! wall-clock occurrences. Everything else in the routine feature treats
//! scheduling as a black box and calls [`next_after`] / [`validate_min_interval`],
//! so the choice of cron crate never leaks past this module.
//!
//! Occurrences are computed in the routine's IANA timezone and returned in UTC.
//! Interpreting the cron at wall-clock time (not UTC) is deliberate: a partner
//! who writes "0 2 * * *" means 2am *their* time year-round, so the UTC instant
//! must shift across DST. `chrono-tz` gives us that; `croner` walks the fields.

use anyhow::{Context, anyhow};
use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;

/// Sub-floor schedules cost real money (each occurrence spawns a session), so we
/// refuse anything that would fire more than four times an hour.
const MIN_INTERVAL: Duration = Duration::minutes(15);

/// The next occurrence of a standard 5-field cron expression, strictly after
/// `after`, interpreted in `timezone` (an IANA name) and returned in UTC.
///
/// Errors on a malformed cron expression or an unknown timezone.
pub fn next_after(
    cron: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| anyhow!("unknown timezone {timezone:?}"))?;
    let parsed = Cron::new(cron)
        .parse()
        .with_context(|| format!("invalid cron {cron:?}"))?;

    // Search from the wall-clock instant so DST is applied at the time the user
    // wrote; `false` makes the match strictly after `after`, never equal to it.
    let local_after = after.with_timezone(&tz);
    let next = parsed
        .find_next_occurrence(&local_after, false)
        .with_context(|| format!("no next occurrence for {cron:?}"))?;
    Ok(next.with_timezone(&Utc))
}

/// Rejects schedules that fire more often than every [`MIN_INTERVAL`].
///
/// Rather than parse cron step syntax ourselves, we sweep a bounded time window:
/// starting at a fixed epoch, we take every occurrence within one day plus one
/// hour and check *every* adjacent gap against the floor. A ~25h window
/// guarantees we observe every intra-day gap — including the wrap from the last
/// active hour of one day to the first active hour of the next. Day-of-week /
/// day-of-month fields only change *which days* fire, never the intra-day gap
/// structure, so this window is sufficient for any minute/hour pattern. (A small
/// fixed-count sample is *not* sufficient: for sparse, non-adjacent hour lists
/// the smallest gap can land past the sampled prefix and slip through.)
///
/// Sampling is in UTC, which has no DST transitions, so the interval math is
/// DST-independent — it's the use of UTC (not the specific epoch) that avoids
/// DST ambiguity. A hard iteration cap keeps a pathological pattern bounded.
pub fn validate_min_interval(cron: &str) -> anyhow::Result<()> {
    let parsed = Cron::new(cron)
        .parse()
        .with_context(|| format!("invalid cron {cron:?}"))?;

    // A fixed epoch in UTC; UTC has no DST transitions, so gaps measured here
    // hold regardless of any partner timezone.
    let epoch = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    // One full day plus one hour: enough to see every intra-day gap, including
    // the wrap into the next day's first active hour.
    let window_end = epoch + Duration::hours(25);

    let mut previous: Option<DateTime<Utc>> = None;
    for occurrence in parsed
        .iter_after(epoch)
        .take(4000)
        .take_while(|o| o.with_timezone(&Utc) <= window_end)
    {
        let occurrence = occurrence.with_timezone(&Utc);
        if let Some(prev) = previous
            && occurrence - prev < MIN_INTERVAL
        {
            return Err(anyhow!(
                "cron {cron:?} fires more often than every {} minutes",
                MIN_INTERVAL.num_minutes()
            ));
        }
        previous = Some(occurrence);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    #[test]
    fn next_after_computes_daily_utc() {
        // "every day at 02:00 America/New_York". 2026-01-10 is EST (UTC-5),
        // so 02:00 local == 07:00 UTC.
        let after = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let next = super::next_after("0 2 * * *", "America/New_York", after).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 10, 7, 0, 0).unwrap());
    }

    #[test]
    fn next_after_is_strictly_after() {
        let after = Utc.with_ymd_and_hms(2026, 1, 10, 7, 0, 0).unwrap();
        let next = super::next_after("0 2 * * *", "America/New_York", after).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 11, 7, 0, 0).unwrap());
    }

    #[test]
    fn rejects_bad_cron() {
        assert!(super::next_after("not a cron", "UTC", Utc::now()).is_err());
    }

    #[test]
    fn rejects_bad_timezone() {
        assert!(super::next_after("0 2 * * *", "Mars/Phobos", Utc::now()).is_err());
    }

    #[test]
    fn rejects_sub_floor_interval() {
        assert!(super::validate_min_interval("* * * * *").is_err());
        assert!(super::validate_min_interval("*/5 * * * *").is_err());
        assert!(super::validate_min_interval("*/15 * * * *").is_ok());
        assert!(super::validate_min_interval("0 2 * * *").is_ok());
        // Legitimately sparse schedules must still be accepted (guard against
        // over-rejection now that we sweep a full window).
        assert!(super::validate_min_interval("0,30 * * * *").is_ok());
    }

    #[test]
    fn rejects_sub_floor_across_sparse_hours() {
        // 22:55 -> 23:00 is a 5-minute gap that a small fixed-count sample misses.
        assert!(super::validate_min_interval("0,35,55 16,22,23 * * *").is_err());
    }
}
