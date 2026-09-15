//! Staying inside Modrinth's request budget.
//!
//! The documented limit is 300 requests per minute per IP, reported back on every response via
//! `X-Ratelimit-Limit`, `X-Ratelimit-Remaining` and `X-Ratelimit-Reset`.
//!
//! The important design point is that **throttling is the safety net, not the strategy**. The
//! endpoints that would otherwise dominate our request count take batches: one
//! `POST /v2/version_files/update` carries hundreds of hashes, so checking a 300-mod pack for
//! updates costs a handful of requests rather than 300. This gate exists to keep a pathological
//! case (a large collection, several servers on one box, a retry storm) from tripping a 429,
//! not to pace ordinary work.
//!
//! Time is injected so backoff can be tested in microseconds. Without that, the tests take
//! thirty real seconds and someone deletes them.

use std::time::Duration;

/// Injectable clock, so rate-limit and retry behaviour is testable without sleeping.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Milliseconds since an arbitrary fixed origin. Monotonic.
    fn now_ms(&self) -> u64;
}

/// Wall-clock implementation used in production.
#[derive(Debug)]
pub struct SystemClock {
    origin: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

/// What the server last told us about our budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub limit: u32,
    pub remaining: u32,
    /// When the window resets, on the injected clock's timescale.
    pub reset_at_ms: u64,
}

/// Tracks the remaining budget and says how long to wait before the next request.
#[derive(Debug)]
pub struct RateGate {
    budget: Option<Budget>,
    /// Below this many remaining requests we start pacing rather than gambling on a 429.
    low_water: u32,
    /// Set by a 429 with `Retry-After`; overrides everything until it passes.
    hold_until_ms: Option<u64>,
}

impl Default for RateGate {
    fn default() -> Self {
        Self::new()
    }
}

impl RateGate {
    pub fn new() -> Self {
        Self {
            budget: None,
            low_water: 20,
            hold_until_ms: None,
        }
    }

    pub fn with_low_water(mut self, n: u32) -> Self {
        self.low_water = n;
        self
    }

    pub fn budget(&self) -> Option<Budget> {
        self.budget
    }

    /// Record what a response said about our budget. Call this on **every** response,
    /// including errors — a 4xx still consumes quota.
    pub fn observe(
        &mut self,
        limit: Option<u32>,
        remaining: Option<u32>,
        reset_in_s: Option<u64>,
        now_ms: u64,
    ) {
        if let (Some(limit), Some(remaining)) = (limit, remaining) {
            self.budget = Some(Budget {
                limit,
                remaining,
                reset_at_ms: now_ms + reset_in_s.unwrap_or(60) * 1000,
            });
        }
    }

    /// Record a 429. `retry_after_s` comes from the `Retry-After` header when present.
    ///
    /// `Retry-After` is the server telling us exactly when to come back, so it becomes the
    /// authority on when the window reopens: the inferred budget is moved to expire with the
    /// hold rather than outliving it. Leaving a stale `reset_at` in place would keep us waiting
    /// after the server already said we could retry.
    pub fn throttled(&mut self, retry_after_s: Option<u64>, now_ms: u64) {
        let wait = retry_after_s.unwrap_or(60) * 1000;
        let until = now_ms + wait;
        self.hold_until_ms = Some(until);
        if let Some(b) = self.budget.as_mut() {
            b.remaining = 0;
            b.reset_at_ms = until;
        }
    }

    /// How long to wait before issuing the next request.
    pub fn delay_before_next(&self, now_ms: u64) -> Duration {
        // An explicit 429 hold outranks anything we inferred.
        if let Some(hold) = self.hold_until_ms
            && hold > now_ms
        {
            return Duration::from_millis(hold - now_ms);
        }
        let Some(b) = self.budget else {
            return Duration::ZERO;
        };
        if b.reset_at_ms <= now_ms {
            // The window has rolled over; whatever we recorded is stale.
            return Duration::ZERO;
        }
        let until_reset = b.reset_at_ms - now_ms;

        if b.remaining == 0 {
            // Nothing left: the only correct move is to wait out the window.
            return Duration::from_millis(until_reset);
        }
        if b.remaining <= self.low_water {
            // Running low: spread what is left across the rest of the window instead of
            // spending it immediately and eating a 429 at the end.
            return Duration::from_millis(until_reset / u64::from(b.remaining));
        }
        Duration::ZERO
    }

    /// Whether a request may go out right now.
    pub fn ready(&self, now_ms: u64) -> bool {
        self.delay_before_next(now_ms).is_zero()
    }
}

/// Retry policy for transient failures.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_ms: u64,
    pub cap_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_ms: 250,
            cap_ms: 30_000,
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff, capped. `attempt` is 0-based.
    ///
    /// Deterministic here; callers add jitter. Keeping the curve pure makes it assertable.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let ms = self
            .base_ms
            .saturating_mul(1u64 << attempt.min(20))
            .min(self.cap_ms);
        Duration::from_millis(ms)
    }

    pub fn should_retry(&self, attempt: u32, status: Option<u16>) -> bool {
        if attempt + 1 >= self.max_attempts {
            return false;
        }
        match status {
            // Transport-level failure: worth another go.
            None => true,
            // Throttling and transient server faults.
            Some(408 | 425 | 429) => true,
            Some(s) if (500..600).contains(&s) => true,
            // Any other 4xx is our fault and will fail identically next time.
            Some(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_budget_information_means_no_delay() {
        assert!(RateGate::new().ready(0));
    }

    #[test]
    fn a_healthy_budget_does_not_pace() {
        let mut g = RateGate::new();
        g.observe(Some(300), Some(250), Some(60), 0);
        assert_eq!(g.delay_before_next(0), Duration::ZERO);
    }

    #[test]
    fn running_low_spreads_the_remainder_over_the_window() {
        let mut g = RateGate::new().with_low_water(20);
        // 10 requests left, 30 seconds to go: one every 3 seconds.
        g.observe(Some(300), Some(10), Some(30), 0);
        assert_eq!(g.delay_before_next(0), Duration::from_millis(3000));
    }

    #[test]
    fn an_exhausted_budget_waits_out_the_window() {
        let mut g = RateGate::new();
        g.observe(Some(300), Some(0), Some(45), 0);
        assert_eq!(g.delay_before_next(0), Duration::from_millis(45_000));
        // And the wait shrinks as time passes.
        assert_eq!(g.delay_before_next(20_000), Duration::from_millis(25_000));
    }

    #[test]
    fn a_stale_window_stops_holding_us_back() {
        let mut g = RateGate::new();
        g.observe(Some(300), Some(0), Some(10), 0);
        assert!(!g.ready(5_000));
        assert!(g.ready(10_001), "the window has rolled over");
    }

    #[test]
    fn a_429_holds_for_exactly_retry_after() {
        let mut g = RateGate::new();
        g.observe(Some(300), Some(200), Some(60), 0);
        g.throttled(Some(34), 0);
        assert_eq!(g.delay_before_next(0), Duration::from_millis(34_000));
        // Once Retry-After elapses we may try again: the inferred budget must not outlive the
        // server's own instruction and keep us waiting for the original window.
        assert!(g.ready(34_001));
    }

    #[test]
    fn a_throttle_moves_the_inferred_window_to_match() {
        let mut g = RateGate::new();
        g.observe(Some(300), Some(200), Some(600), 0);
        g.throttled(Some(30), 0);
        let b = g.budget().unwrap();
        assert_eq!(b.remaining, 0);
        assert_eq!(
            b.reset_at_ms, 30_000,
            "the budget must expire with the hold, not at the stale reset"
        );
    }

    #[test]
    fn a_429_outranks_an_apparently_healthy_budget() {
        // The server's explicit instruction beats our own accounting, which may be stale or
        // may not reflect other processes sharing this IP.
        let mut g = RateGate::new();
        g.throttled(Some(30), 0);
        g.observe(Some(300), Some(299), Some(60), 0);
        assert_eq!(g.delay_before_next(0), Duration::from_millis(30_000));
    }

    #[test]
    fn a_429_without_retry_after_falls_back_to_a_full_window() {
        let mut g = RateGate::new();
        g.throttled(None, 0);
        assert_eq!(g.delay_before_next(0), Duration::from_millis(60_000));
    }

    #[test]
    fn backoff_grows_then_caps() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff(0), Duration::from_millis(250));
        assert_eq!(p.backoff(1), Duration::from_millis(500));
        assert_eq!(p.backoff(2), Duration::from_millis(1000));
        assert_eq!(p.backoff(100), Duration::from_millis(30_000), "must cap");
    }

    #[test]
    fn retries_transient_failures_only() {
        let p = RetryPolicy::default();
        for status in [None, Some(408), Some(429), Some(500), Some(503)] {
            assert!(p.should_retry(0, status), "{status:?} should retry");
        }
        // A 404 or a 401 will fail the same way next time.
        for status in [Some(400), Some(401), Some(403), Some(404), Some(410)] {
            assert!(!p.should_retry(0, status), "{status:?} should not retry");
        }
    }

    #[test]
    fn retries_stop_at_the_attempt_limit() {
        let p = RetryPolicy::default();
        assert!(p.should_retry(2, Some(500)));
        assert!(!p.should_retry(3, Some(500)), "4 attempts means 4, not 5");
    }

    #[test]
    fn error_responses_still_update_the_budget() {
        // A 404 consumed quota just like a 200 did.
        let mut g = RateGate::new();
        g.observe(Some(300), Some(5), Some(60), 0);
        assert_eq!(g.budget().unwrap().remaining, 5);
    }
}
