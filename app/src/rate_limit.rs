//! Fixed-window counters per client network, plus the client IP extractor they key on.
//! shortcut: in-process memory, so each replica counts on its own; move to Postgres or Redis
//! once brain runs more than one instance.
use std::{
    collections::HashMap,
    convert::Infallible,
    hash::Hash,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::request::Parts,
};

use crate::state::AppState;

/// Most networks tracked at once; past it the oldest window is evicted, so memory stays bounded.
const MAX_KEYS: usize = 10_000;

/// Keyed by client network by default; `RateLimiter<String>` keys on anything else, e.g. an email.
pub struct RateLimiter<K = Option<IpAddr>> {
    cap: u32,
    window: Duration,
    max_keys: usize,
    hits: Mutex<HashMap<K, (Instant, u32)>>,
}

/// The bucket key: an IPv4 address as is, an IPv6 address by its /64 (one subscriber's
/// allocation, so rotating addresses inside it does not escape the limit).
fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(v6.to_bits() & !((1u128 << 64) - 1))),
        },
        v4 => v4,
    }
}

impl RateLimiter {
    /// Counts one hit for `ip`; false once its network is over the cap for the current window.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        self.hit(ip.map(bucket))
    }
}

impl<K: Hash + Eq + Clone> RateLimiter<K> {
    pub fn per_minute(cap: u32) -> Self {
        Self::new(cap, Duration::from_secs(60), MAX_KEYS)
    }

    pub fn new(cap: u32, window: Duration, max_keys: usize) -> Self {
        Self {
            cap,
            window,
            max_keys,
            hits: Mutex::default(),
        }
    }

    /// Counts one hit for `key`; false once it is over the cap for the current window.
    pub fn hit(&self, key: K) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if !hits.contains_key(&key) && hits.len() >= self.max_keys {
            hits.retain(|_, (start, _)| now.duration_since(*start) < self.window);
            if hits.len() >= self.max_keys {
                let oldest = hits
                    .iter()
                    .min_by_key(|(_, (start, _))| *start)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    hits.remove(&k);
                }
            }
        }
        let entry = hits.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.cap
    }

    /// Forgets `key`, e.g. after a successful sign-in.
    pub fn clear(&self, key: &K) {
        self.hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    pub fn tracked(&self) -> usize {
        self.hits.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// The client address. With `TRUST_PROXY=true` (behind Railway's edge, the one trusted proxy)
/// it is the rightmost `X-Forwarded-For` value, which that proxy appends, falling back to the
/// socket peer when the header is missing or unparsable. Without it the header is ignored, so
/// a spoofed value changes nothing.
pub struct ClientIp(pub Option<IpAddr>);

fn forwarded_for(parts: &Parts) -> Option<IpAddr> {
    parts
        .headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse().ok())
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Infallible> {
        let forwarded = app.trust_proxy.then(|| forwarded_for(parts)).flatten();
        let peer = || {
            let info = parts.extensions.get::<ConnectInfo<SocketAddr>>();
            info.map(|c| c.0.ip())
        };
        let source = if forwarded.is_some() {
            "x-forwarded-for"
        } else {
            "socket"
        };
        let ip = forwarded.or_else(peer);
        tracing::debug!(client_ip = ?ip, source, "client ip");
        Ok(Self(ip))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, time::Duration};

    use super::RateLimiter;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn caps_per_ip() {
        let l: RateLimiter = RateLimiter::per_minute(2);
        assert!(l.allow(ip("203.0.113.1")) && l.allow(ip("203.0.113.1")));
        assert!(!l.allow(ip("203.0.113.1")));
        assert!(l.allow(ip("203.0.113.2")));
    }

    #[test]
    fn ipv6_in_one_slash_64_shares_a_bucket() {
        let l: RateLimiter = RateLimiter::per_minute(2);
        assert!(l.allow(ip("2001:db8:1:2::1")));
        assert!(l.allow(ip("2001:db8:1:2:ffff:ffff:ffff:ffff")));
        assert!(!l.allow(ip("2001:db8:1:2::abcd")));
        assert!(l.allow(ip("2001:db8:1:3::1")), "next /64 is its own bucket");
        assert_eq!(l.tracked(), 2);
    }

    #[test]
    fn clear_resets_a_key() {
        let l: RateLimiter<String> = RateLimiter::per_minute(1);
        let k = "a@example.com".to_string();
        assert!(l.hit(k.clone()) && !l.hit(k.clone()));
        l.clear(&k);
        assert!(l.hit(k));
    }

    #[test]
    fn map_never_grows_past_its_cap() {
        let l: RateLimiter = RateLimiter::new(1, Duration::from_secs(60), 3);
        for i in 0..50u8 {
            assert!(l.allow(ip(&format!("198.51.100.{i}"))));
        }
        assert_eq!(l.tracked(), 3);
        // The newest keys stay tracked; the oldest were evicted.
        assert!(!l.allow(ip("198.51.100.49")));
    }
}
