//! Exponential backoff with jitter for store errors, and the one classification
//! rule every loop in this crate needs before it decides to back off at all: a lost
//! CAS race (`412 Precondition Failed`) is the mechanism working as designed
//! (AGENTS.md invariant 4, `docs/architecture.md`'s failure-mode table), not a
//! failure to escalate. Backing off after a normal CAS loss would slow down the
//! *next* legitimate attempt on the roster for no reason — the retry there is
//! supposed to be immediate, driven by a fresh read, not throttled.
//!
//! [`XorShift`] is reused by [`crate::presence`] for an unrelated purpose (staggering
//! how often each daemon rebuilds the roster) — see that module's doc.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ctxlake_store::StoreError;

/// True if `err` should push a [`Backoff`] forward. A precondition failure is CAS
/// contention, not a fault — see the module doc. Everything else (network errors,
/// 5xx, a malformed body) is a real problem the store side is having, and repeated
/// hammering only makes an outage worse.
pub fn should_backoff(err: &StoreError) -> bool {
    !err.is_precondition_failed()
}

/// Supplies the random component of the jitter. A trait, not a bare RNG type, so
/// tests can inject a deterministic source and assert the exponential *shape*
/// (`exp_cap`) separately from the randomness layered on top of it.
pub trait JitterSource: fmt::Debug {
    /// A duration uniformly distributed in `[0, cap)`. `cap == Duration::ZERO` must
    /// return `Duration::ZERO` rather than panicking on a `% 0`.
    fn uniform_up_to(&mut self, cap: Duration) -> Duration;
}

/// A tiny xorshift64* generator seeded from the wall clock.
///
/// Not `rand`: jitter here exists only to keep a fleet of daemons that all started
/// retrying at the same instant (a shared store outage ending) from hammering the
/// store again in lockstep — it does not need to resist an adversary, only avoid
/// looking like a fixed delay. Adding a real RNG crate for that would be a
/// dependency AGENTS.md's house rule says to justify, and there is nothing here that
/// justifies it over 15 lines of xorshift.
#[derive(Debug, Clone)]
pub struct XorShift(u64);

impl XorShift {
    pub fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        // xorshift64* is undefined at seed 0; OR in a bit so a clock that reads back
        // as exactly 0 (frozen test clocks, some CI sandboxes) never wedges it.
        Self(nanos | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl JitterSource for XorShift {
    fn uniform_up_to(&mut self, cap: Duration) -> Duration {
        if cap.is_zero() {
            return Duration::ZERO;
        }
        let cap_nanos = cap.as_nanos().min(u64::MAX as u128).max(1) as u64;
        Duration::from_nanos(self.next_u64() % cap_nanos)
    }
}

/// The exponential cap for `attempt` (0-indexed): `base * 2^attempt`, clamped to
/// `max`. Pure and deterministic on purpose — [`Backoff`]'s jitter is layered on top
/// of this, and testing the shape (does it double, does it clamp) is cleanest done
/// without any randomness in the way.
fn exp_cap(base: Duration, max: Duration, attempt: u32) -> Duration {
    match 1u32.checked_shl(attempt) {
        Some(mult) => base.saturating_mul(mult).min(max),
        None => max, // shifted out entirely — we're already well past the cap.
    }
}

/// Full-jitter exponential backoff (Marc Brooker / AWS's "Exponential Backoff and
/// Jitter"): sleep a *uniformly random* duration in `[0, cap)` rather than sleeping
/// `cap` itself, so retries desynchronize instead of all landing on the store at
/// once every time the exponent doubles.
#[derive(Debug)]
pub struct Backoff<J: JitterSource = XorShift> {
    base: Duration,
    max: Duration,
    attempt: u32,
    jitter: J,
}

impl Backoff<XorShift> {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self::with_source(base, max, XorShift::seeded())
    }
}

impl<J: JitterSource> Backoff<J> {
    pub fn with_source(base: Duration, max: Duration, jitter: J) -> Self {
        Self {
            base,
            max,
            attempt: 0,
            jitter,
        }
    }

    /// The delay to sleep before the next retry. Advances the attempt counter, so
    /// calling this repeatedly without an intervening [`Self::reset`] climbs the
    /// exponential ladder.
    pub fn next_delay(&mut self) -> Duration {
        let cap = exp_cap(self.base, self.max, self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        self.jitter.uniform_up_to(cap)
    }

    /// Call after a successful operation. A run of failures must not leave the
    /// *next*, unrelated failure waiting out a fully climbed backoff it had no part
    /// in causing.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::Error as OsError;

    #[test]
    fn precondition_failed_never_backs_off() {
        let err: StoreError = OsError::Precondition {
            path: "live/roster.json".into(),
            source: "etag mismatch".into(),
        }
        .into();
        assert!(
            !should_backoff(&err),
            "a lost CAS race must not trigger backoff"
        );
    }

    #[test]
    fn a_real_store_error_does_back_off() {
        let err: StoreError = OsError::Generic {
            store: "test",
            source: "connection reset".into(),
        }
        .into();
        assert!(
            should_backoff(&err),
            "a genuine store error must trigger backoff"
        );
    }

    #[test]
    fn exp_cap_doubles_each_attempt_until_clamped() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(10);
        assert_eq!(exp_cap(base, max, 0), Duration::from_millis(100));
        assert_eq!(exp_cap(base, max, 1), Duration::from_millis(200));
        assert_eq!(exp_cap(base, max, 2), Duration::from_millis(400));
        assert_eq!(exp_cap(base, max, 3), Duration::from_millis(800));
        // 100ms * 2^7 = 12.8s > 10s max.
        assert_eq!(exp_cap(base, max, 7), max);
        // Nowhere near overflowing u32's shift range, but must still clamp, not panic.
        assert_eq!(exp_cap(base, max, 63), max);
    }

    /// A jitter source that always returns the cap itself, so `Backoff`'s ladder can
    /// be asserted exactly instead of only "somewhere under the cap."
    #[derive(Debug)]
    struct AlwaysCap;
    impl JitterSource for AlwaysCap {
        fn uniform_up_to(&mut self, cap: Duration) -> Duration {
            cap
        }
    }

    #[test]
    fn backoff_climbs_the_exact_exponential_ladder_with_a_deterministic_source() {
        let mut b =
            Backoff::with_source(Duration::from_millis(50), Duration::from_secs(5), AlwaysCap);
        assert_eq!(b.next_delay(), Duration::from_millis(50));
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.attempt(), 3);
    }

    #[test]
    fn reset_returns_to_the_base_delay() {
        let mut b =
            Backoff::with_source(Duration::from_millis(50), Duration::from_secs(5), AlwaysCap);
        b.next_delay();
        b.next_delay();
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        b.reset();
        assert_eq!(
            b.next_delay(),
            Duration::from_millis(50),
            "a successful operation must not leave the next failure paying an old ladder's price"
        );
    }

    #[test]
    fn real_jitter_never_exceeds_the_cap_and_eventually_uses_the_full_range() {
        let mut b = Backoff::new(Duration::from_millis(10), Duration::from_millis(80));
        let mut saw_small = false;
        let mut saw_large = false;
        for _ in 0..500 {
            let d = b.next_delay();
            assert!(
                d < Duration::from_millis(80),
                "jitter must never reach, let alone exceed, the cap: {d:?}"
            );
            if d < Duration::from_millis(20) {
                saw_small = true;
            }
            if d > Duration::from_millis(60) {
                saw_large = true;
            }
        }
        assert!(
            saw_small && saw_large,
            "500 samples across a climbing cap should span both ends of the range"
        );
    }

    #[test]
    fn zero_cap_never_panics_and_returns_zero() {
        let mut x = XorShift::seeded();
        assert_eq!(x.uniform_up_to(Duration::ZERO), Duration::ZERO);
    }
}
