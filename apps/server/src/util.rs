use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hash a token using SHA-256 for storage.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Small fixed-window limiter for public auth entry points.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, Bucket>>>,
    max_requests: u32,
    window: Duration,
}

#[derive(Clone, Copy)]
struct Bucket {
    count: u32,
    reset_at: Instant,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            max_requests,
            window,
        }
    }

    pub fn auth_defaults() -> Self {
        Self::new(30, Duration::from_secs(60))
    }

    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self.inner.lock().expect("rate limiter mutex poisoned");
        buckets.retain(|_, bucket| bucket.reset_at > now);

        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            count: 0,
            reset_at: now + self.window,
        });
        if bucket.reset_at <= now {
            *bucket = Bucket {
                count: 0,
                reset_at: now + self.window,
            };
        }
        if bucket.count >= self.max_requests {
            return false;
        }
        bucket.count += 1;
        true
    }
}
