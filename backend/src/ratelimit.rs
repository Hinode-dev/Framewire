//! A minimal global rate limiter for failed room-code lookups.
//!
//! Deliberately not per-IP: this server may sit behind a reverse proxy
//! (see `server/docker-compose.yml`), so the TCP peer address usually isn't
//! the real client, and trusting `X-Forwarded-For` without a known-trusted
//! proxy chain is its own can of worms. A global limiter still meaningfully
//! throttles bulk room-code guessing, since legitimate viewers almost never
//! hit a wrong code in the first place — only a guesser racks up failures.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct FailureRateLimiter {
    max_failures: usize,
    window: Duration,
    failures: Mutex<VecDeque<Instant>>,
}

impl FailureRateLimiter {
    pub fn new(max_failures: usize, window: Duration) -> Self {
        Self {
            max_failures,
            window,
            failures: Mutex::new(VecDeque::new()),
        }
    }

    /// True if we're currently under the failure threshold, i.e. it's fine
    /// to let this attempt proceed to a real room-code lookup.
    pub fn is_allowed(&self) -> bool {
        let mut failures = self.failures.lock().unwrap();
        let cutoff = Instant::now() - self.window;
        while failures.front().is_some_and(|&t| t < cutoff) {
            failures.pop_front();
        }
        failures.len() < self.max_failures
    }

    /// Records a failed (wrong room code) lookup attempt.
    pub fn record_failure(&self) {
        self.failures.lock().unwrap().push_back(Instant::now());
    }
}
