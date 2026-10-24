//! Byte-rate limiter shared by any number of connections.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// A token bucket that allows short bursts of up to `BURST` worth of the
/// configured rate and then paces callers. A rate of 0 means unlimited and
/// costs a single atomic load per call.
#[derive(Debug)]
pub struct RateLimiter {
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

const BURST: f64 = 0.25;

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Account for `n` bytes, sleeping as long as needed to respect the rate.
    pub async fn acquire(&self, n: u64) {
        let wait = self.debit(n);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn debit(&self, n: u64) -> Duration {
        let rate = self.rate() as f64;
        if rate <= 0.0 {
            return Duration::ZERO;
        }
        let mut b = self.bucket.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.last = now;
        b.tokens = (b.tokens + elapsed * rate).min(rate * BURST);
        b.tokens -= n as f64;
        if b.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-b.tokens / rate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let l = RateLimiter::new(0);
        assert_eq!(l.debit(u64::MAX / 2), Duration::ZERO);
    }

    #[tokio::test]
    async fn paces_to_configured_rate() {
        let l = RateLimiter::new(1_000_000);
        let start = Instant::now();
        for _ in 0..10 {
            l.acquire(50_000).await;
        }
        // 500 KB at 1 MB/s is ~0.5s, minus at most the burst allowance.
        let secs = start.elapsed().as_secs_f64();
        assert!(secs > 0.2 && secs < 1.0, "took {secs}s");
    }
}
