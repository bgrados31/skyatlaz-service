//! The rate limit of the Hypixel API.
//!
//! Every answer carries `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`. The service keeps the
//! state of the limit, so it never asks for more than the key is allowed to and can tell how long it has to wait
//! when the limit is used up.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the service knows about the rate limit of the key.
#[derive(Default)]
pub struct RateLimits {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// How many requests the key may do in the window.
    limit: u32,
    /// How many of them are left.
    remaining: u32,
    /// When the window ends and the limit is back.
    reset_at: Option<Instant>,
}

impl RateLimits {
    /// Reads the headers of an answer and remembers what they say.
    pub fn update(&self, limit: Option<u32>, remaining: Option<u32>, reset: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(limit) = limit {
            state.limit = limit;
        }
        if let Some(remaining) = remaining {
            state.remaining = remaining;
        }
        if let Some(reset) = reset {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or_default();
            let wait = Duration::from_secs(reset.saturating_sub(now));
            state.reset_at = Some(Instant::now() + wait);
        }
    }

    /// How long the service has to wait before the next request, if the limit is used up.
    pub fn exhausted(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.remaining > 0 {
            return None;
        }
        let reset_at = state.reset_at?;
        let now = Instant::now();
        if reset_at > now {
            Some(reset_at - now)
        } else {
            None
        }
    }

    /// How many requests the key may do in the window, and how many are left.
    pub fn state(&self) -> (u32, u32) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (state.limit, state.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_no_wait_while_requests_are_left() {
        let limits = RateLimits::default();

        limits.update(Some(300), Some(299), None);

        assert!(limits.exhausted().is_none());
        assert_eq!(limits.state(), (300, 299));
    }

    #[test]
    fn the_limit_is_exhausted_when_nothing_is_left() {
        let limits = RateLimits::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default();

        limits.update(Some(300), Some(0), Some(now + 60));

        let wait = limits.exhausted().expect("the service has to wait");
        assert!(wait.as_secs() >= 55, "the wait was {wait:?}");
    }

    #[test]
    fn a_window_that_is_over_does_not_hold_the_service_back() {
        let limits = RateLimits::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default();

        limits.update(Some(300), Some(0), Some(now - 10));

        assert!(limits.exhausted().is_none());
    }
}
