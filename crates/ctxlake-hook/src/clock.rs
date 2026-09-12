//! RFC3339 UTC timestamps from `SystemTime`, without a date/time dependency.
//!
//! The hook's dependency tree is frozen (AGENTS.md invariant 2). Pulling in `time` or
//! `chrono` — both already in the workspace for the store/CLI side — just to stamp
//! `emitted_at` on every event is exactly the kind of one-line convenience that erodes
//! the boundary CI enforces with `cargo tree`. A calendar conversion is ~15 lines.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current instant as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn now_rfc3339() -> String {
    // `UNIX_EPOCH` is always in the past on real clocks; a clock set before 1970
    // would make `duration_since` fail, and stamping `1970-01-01` is a saner fallback
    // than panicking a hook over a broken system clock.
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(since_epoch.as_secs(), since_epoch.subsec_millis())
}

fn format_rfc3339(epoch_secs: u64, millis: u32) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days-since-1970-01-01 -> proleptic Gregorian
/// (year, month, day). See <http://howardhinnant.github.io/date_algorithms.html>.
/// Reproduced by hand (not imported) — the crate that owns the good implementation
/// pulls in far more than one timestamp per event needs.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instant_formats_correctly() {
        // 2024-01-01T00:00:00Z is 1704067200 on any epoch converter — cross-checked
        // against `date -u -d @1704067200`.
        assert_eq!(format_rfc3339(1_704_067_200, 0), "2024-01-01T00:00:00.000Z");
    }

    #[test]
    fn millis_are_zero_padded() {
        assert_eq!(format_rfc3339(1_704_067_200, 5), "2024-01-01T00:00:00.005Z");
    }

    #[test]
    fn leap_day_round_trips() {
        // 2024-02-29T12:00:00Z = 1709208000. Leap-year handling is exactly what a
        // hand-rolled calendar routine gets wrong first.
        assert_eq!(format_rfc3339(1_709_208_000, 0), "2024-02-29T12:00:00.000Z");
    }

    #[test]
    fn now_rfc3339_is_well_formed() {
        let s = now_rfc3339();
        assert_eq!(s.len(), "2026-09-11T18:22:03.114Z".len(), "got: {s}");
        assert!(s.ends_with('Z'), "got: {s}");
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
    }
}
