//! Timer jitter: spread the daemon's background POST timers (heartbeat, sync
//! backstop, billing refresh) by a uniform random fraction so many daemons
//! never beat the cloud in lockstep. A coalescing debounce (e.g. sync's
//! `DEBOUNCE`) is deliberately NOT jittered — jitter is for *independent*
//! periodic timers, not a burst-settling window.

use std::time::Duration;

/// Default jitter fraction applied across the daemon's background timers: ±10%.
pub const DEFAULT_FRAC: f64 = 0.1;

/// Returns a duration drawn uniformly from `[d·(1−frac), d·(1+frac)]`.
///
/// `frac` is clamped to `[0.0, 1.0]` so a caller can never jitter into a negative
/// duration. `frac == 0.0` (or `d` zero) is a no-op, returning `d` unchanged —
/// jitter never turns an already-degenerate cadence into a busy loop.
pub fn jittered(d: Duration, frac: f64) -> Duration {
    let frac = frac.clamp(0.0, 1.0);
    let base = d.as_secs_f64();
    if frac == 0.0 || base <= 0.0 {
        return d;
    }
    let lo = base * (1.0 - frac);
    let hi = base * (1.0 + frac);
    let secs = lo + fastrand::f64() * (hi - lo);
    Duration::from_secs_f64(secs.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jittered_stays_within_the_uniform_bounds() {
        let d = Duration::from_secs(100);
        for _ in 0..2000 {
            let j = jittered(d, 0.1);
            assert!(
                j >= Duration::from_secs_f64(90.0),
                "{j:?} below lower bound"
            );
            assert!(
                j <= Duration::from_secs_f64(110.0),
                "{j:?} above upper bound"
            );
        }
    }

    #[test]
    fn zero_fraction_is_a_no_op() {
        let d = Duration::from_secs(42);
        assert_eq!(jittered(d, 0.0), d);
    }

    #[test]
    fn zero_duration_is_a_no_op() {
        assert_eq!(jittered(Duration::ZERO, 0.1), Duration::ZERO);
    }

    #[test]
    fn fraction_above_one_is_clamped() {
        let d = Duration::from_secs(10);
        for _ in 0..500 {
            // frac=5.0 clamps to 1.0 ⇒ bounds are [0, 20]s, never panics/underflows.
            let j = jittered(d, 5.0);
            assert!(j <= Duration::from_secs(20));
        }
    }

    #[test]
    fn negative_fraction_is_clamped_to_zero() {
        let d = Duration::from_secs(30);
        assert_eq!(jittered(d, -1.0), d);
    }
}
