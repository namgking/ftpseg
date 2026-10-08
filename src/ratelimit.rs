//! A shared token bucket, so `--limit` caps the whole download rather than
//! each connection separately.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

struct Bucket {
    tokens: f64,
    capacity: f64,
    last: Instant,
}

/// Global bandwidth limiter.
pub struct RateLimiter {
    rate: f64,
    bucket: Mutex<Bucket>,
}

impl RateLimiter {
    /// `bytes_per_sec` must be non-zero.
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        // Allow a small burst so a single large read is not chopped into
        // artificially tiny sleeps.
        let capacity = (rate / 4.0).max(64.0 * 1024.0);
        Self {
            rate,
            bucket: Mutex::new(Bucket {
                tokens: capacity,
                capacity,
                last: Instant::now(),
            }),
        }
    }

    /// Consume `n` bytes of budget, waiting until they are available.
    pub async fn acquire(&self, n: usize) {
        let mut wanted = n as f64;
        loop {
            let sleep_for = {
                let mut bucket = self.bucket.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(bucket.last).as_secs_f64();
                bucket.last = now;
                bucket.tokens = (bucket.tokens + elapsed * self.rate).min(bucket.capacity);

                if bucket.tokens >= wanted {
                    bucket.tokens -= wanted;
                    return;
                }
                let missing = wanted - bucket.tokens;
                // Take whatever is there, then wait for the rest.
                bucket.tokens = 0.0;
                wanted = missing;
                Duration::from_secs_f64(missing / self.rate)
            };
            tokio::time::sleep(sleep_for).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn small_requests_pass_immediately() {
        let limiter = RateLimiter::new(1024 * 1024);
        let start = Instant::now();
        limiter.acquire(1024).await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "a request inside the initial burst must not wait"
        );
    }

    #[tokio::test]
    async fn demand_beyond_the_burst_is_paced() {
        // The bucket starts with max(rate/4, 64 KiB) tokens, so asking for more
        // than that has to wait for the remainder at the configured rate.
        let limiter = RateLimiter::new(100_000);
        let start = Instant::now();
        limiter.acquire(65_536 + 50_000).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "50 KiB over the burst at 100 KB/s should take about 0.5s, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "pacing should not overshoot wildly, took {elapsed:?}"
        );
    }

    #[test]
    fn zero_rate_is_clamped() {
        let limiter = RateLimiter::new(0);
        assert!(limiter.rate >= 1.0);
    }
}
