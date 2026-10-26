//! Fixed-window per-client-IP counters, plus the client IP extractor they key on.
//! shortcut: in-process memory, so each replica counts on its own; move to Postgres or Redis
//! once brain runs more than one instance.
use std::{
    collections::HashMap,
    convert::Infallible,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{extract::FromRequestParts, http::request::Parts};

/// Above this many tracked IPs, expired windows are dropped so memory stays bounded.
const PRUNE_AT: usize = 10_000;

pub struct RateLimiter {
    cap: u32,
    window: Duration,
    hits: Mutex<HashMap<Option<IpAddr>, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn per_minute(cap: u32) -> Self {
        Self {
            cap,
            window: Duration::from_secs(60),
            hits: Mutex::default(),
        }
    }

    /// Counts one hit for `ip`; false once it is over the cap for the current window.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if hits.len() > PRUNE_AT {
            hits.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = hits.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.cap
    }
}

/// The rightmost `X-Forwarded-For` value, which Railway's edge (the one trusted proxy) appends.
/// `None` without the header (local runs), so those requests share one bucket.
pub struct ClientIp(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let ip = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok());
        Ok(Self(ip))
    }
}

#[cfg(test)]
mod tests {
    use super::RateLimiter;

    #[test]
    fn caps_per_ip() {
        let l = RateLimiter::per_minute(2);
        let a = Some("203.0.113.1".parse().unwrap());
        assert!(l.allow(a) && l.allow(a));
        assert!(!l.allow(a));
        assert!(l.allow(Some("203.0.113.2".parse().unwrap())));
    }
}
