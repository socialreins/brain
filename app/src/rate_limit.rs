//! Rate limits, the client IP extractor they key on, and the daily caps. A `RateLimiter` is a
//! fixed-window counter kept in Postgres (`rate_limit_windows`), one row per bucket, so every
//! replica counts the same window and a restart does not reset it. A check is one atomic
//! statement (an upsert that takes the row lock), so two replicas cannot both pass the cap. A
//! database error allows the request and logs a warning: a limiter that cannot be read must
//! not stop traffic. Daily caps (`daily_reserve`, `daily_refund`) are counted in
//! `rate_limit_hits`.
use std::{
    convert::Infallible,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::request::Parts,
};

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};

use crate::state::AppState;

/// A fixed-window counter for short bursts, shared through Postgres. Keyed by client network
/// by default (`allow`); `hit` and `wait` key on anything else, e.g. a user or a token id.
pub struct RateLimiter {
    prefix: String,
    cap: u32,
    window: Duration,
    /// Distinguishes this instance's address-less requests; see `allow`.
    no_address: String,
}

/// What a `RateLimiter` counts beside its prefix: the text a bucket is named by.
pub trait Key {
    fn key(&self) -> String;
}

impl Key for uuid::Uuid {
    fn key(&self) -> String {
        self.to_string()
    }
}

impl Key for IpAddr {
    fn key(&self) -> String {
        bucket(*self).to_string()
    }
}

/// The bucket key: an IPv4 address as is, an IPv6 address by its /64 (one subscriber's
/// allocation, so rotating addresses inside it does not escape the limit).
pub(crate) fn bucket(ip: IpAddr) -> IpAddr {
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
    pub async fn allow(&self, db: &DatabaseConnection, ip: Option<IpAddr>) -> bool {
        match ip {
            Some(ip) => self.check(db, &bucket(ip).to_string()).await.is_none(),
            // No client address (a server without socket info, which is a test server in
            // practice): there is nothing a second replica could key it by, so this instance
            // counts those on its own rather than sharing one bucket across every replica.
            None => self.check(db, &self.no_address).await.is_none(),
        }
    }
}

impl RateLimiter {
    /// `prefix` names the limit in the table (`<prefix>:<key>`): every limit gets its own.
    pub fn per_minute(prefix: impl Into<String>, cap: u32) -> Self {
        Self::new(prefix, cap, Duration::from_secs(60))
    }

    pub fn new(prefix: impl Into<String>, cap: u32, window: Duration) -> Self {
        Self {
            prefix: prefix.into(),
            cap,
            window,
            no_address: format!("no-address:{}", uuid::Uuid::now_v7().simple()),
        }
    }

    /// Counts one hit for `key`; false once it is over the cap for the current window.
    pub async fn hit(&self, db: &DatabaseConnection, key: impl Key) -> bool {
        self.wait(db, key).await.is_none()
    }

    /// `hit`, answering how long until the window resets when `key` is over the cap (for a
    /// `Retry-After` header), else `None`.
    pub async fn wait(&self, db: &DatabaseConnection, key: impl Key) -> Option<Duration> {
        self.check(db, &key.key()).await
    }

    /// One statement: insert the bucket's first hit, or move an expired window to now, or
    /// count this one in the window that is running. The upsert locks the row, so concurrent
    /// replicas take turns and the cap holds. A refused check still counts, but it never moves
    /// `window_start`, so it does not extend the window.
    async fn check(&self, db: &DatabaseConnection, key: &str) -> Option<Duration> {
        let bucket = format!("{}:{key}", self.prefix);
        let sql = "INSERT INTO rate_limit_windows AS w (bucket, window_start, hits) \
                   VALUES ($1, now(), 1) \
                   ON CONFLICT (bucket) DO UPDATE SET \
                     hits = CASE WHEN w.window_start <= now() - ($2 * interval '1 second') \
                                 THEN 1 ELSE w.hits + 1 END, \
                     window_start = CASE WHEN w.window_start <= now() - ($2 * interval '1 second') \
                                         THEN now() ELSE w.window_start END \
                   RETURNING hits, \
                     EXTRACT(EPOCH FROM \
                       (window_start + ($2 * interval '1 second') - now()))::float8 AS reset_in";
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [bucket.clone().into(), self.window.as_secs_f64().into()],
        );
        let row = match db.query_one_raw(stmt).await {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(error = %e, bucket = %bucket, "rate limit check failed, allowing");
                return None;
            }
        };
        let hits: i64 = match row.try_get("", "hits") {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, bucket = %bucket, "rate limit check unreadable, allowing");
                return None;
            }
        };
        prune(db).await;
        if hits <= i64::from(self.cap) {
            return None;
        }
        let reset_in: f64 = row.try_get("", "reset_in").unwrap_or_default();
        let left = if reset_in.is_finite() {
            reset_in.clamp(0.0, self.window.as_secs_f64())
        } else {
            0.0
        };
        Some(Duration::from_secs_f64(left))
    }
}

/// When `prune` last ran, in Unix seconds.
static LAST_PRUNE: AtomicI64 = AtomicI64::new(0);

fn bind(sql: &str, v: &str) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, [v.to_owned().into()])
}

/// Daily caps kept in Postgres (`rate_limit_hits`), so they hold across restarts and replicas,
/// cannot be evicted, and do not reset when what they count is deleted. Reserves one hit in
/// every `(bucket, cap)` when each has fewer than `cap` hits in the last 24 hours, and returns
/// the reserved row ids; `None` when one is at its cap. The buckets are locked in sorted order
/// for the count and insert, so concurrent requests cannot pass a cap. A caller whose action
/// then fails gives the hits back with `daily_refund`, so only completed actions count.
pub async fn daily_reserve(
    db: &DatabaseConnection,
    caps: &[(String, i64)],
) -> Result<Option<Vec<i64>>, DbErr> {
    let txn = db.begin().await?;
    let Some(ids) = reserve_in(&txn, caps).await? else {
        return Ok(None);
    };
    txn.commit().await?;
    prune(db).await;
    Ok(Some(ids))
}

/// `daily_reserve` inside the caller's transaction, so the hits commit or roll back with the
/// action they pay for.
pub async fn reserve_in(
    txn: &impl ConnectionTrait,
    caps: &[(String, i64)],
) -> Result<Option<Vec<i64>>, DbErr> {
    let mut caps = caps.to_vec();
    caps.sort();
    for (bucket, _) in &caps {
        let sql = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";
        txn.execute_raw(bind(sql, bucket)).await?;
    }
    for (bucket, cap) in &caps {
        let sql = "SELECT count(*) AS n FROM rate_limit_hits \
                   WHERE bucket = $1 AND created_at > now() - interval '1 day'";
        let n: i64 = match txn.query_one_raw(bind(sql, bucket)).await? {
            Some(row) => row.try_get("", "n")?,
            None => 0,
        };
        if n >= *cap {
            return Ok(None);
        }
    }
    let mut ids = Vec::with_capacity(caps.len());
    for (bucket, _) in &caps {
        let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1) RETURNING id";
        if let Some(row) = txn.query_one_raw(bind(sql, bucket)).await? {
            ids.push(row.try_get("", "id")?);
        }
    }
    Ok(Some(ids))
}

/// Gives back hits from `daily_reserve` after the action failed. Logged, not returned: a lost
/// refund only costs the caller one slot until it ages out.
pub async fn daily_refund(db: &DatabaseConnection, ids: &[i64]) {
    for &id in ids {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM rate_limit_hits WHERE id = $1",
            [id.into()],
        );
        if let Err(e) = db.execute_raw(stmt).await {
            tracing::warn!(error = %e, "rate limit refund failed");
        }
    }
}

/// Counts one event in `bucket` with no cap, to be counted later (`count_since`): a hit nobody
/// gives back. Logged, not returned: losing a count must not fail what it describes.
pub async fn note(db: &DatabaseConnection, bucket: &str) {
    let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1)";
    if let Err(e) = db.execute_raw(bind(sql, bucket)).await {
        tracing::warn!(error = %e, bucket, "event not counted");
    }
}

/// Hits in `bucket` made from `from` up to `to` (they are kept a day, `prune`).
pub async fn count_since(
    db: &DatabaseConnection,
    bucket: &str,
    (from, to): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
) -> Result<i64, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT count(*) AS n FROM rate_limit_hits WHERE bucket = $1 \
         AND created_at >= $2 AND created_at < $3",
        [bucket.to_owned().into(), from.into(), to.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(row) => row.try_get("", "n"),
        None => Ok(0),
    }
}

/// Deletes day-old hits and windows whose window began over a day ago (longer than any window
/// uses), at most once an hour per process, outside any caller's transaction.
async fn prune(db: &DatabaseConnection) {
    let now = chrono::Utc::now().timestamp();
    let last = LAST_PRUNE.load(Ordering::Relaxed);
    let due = now - last >= 3600;
    if due
        && LAST_PRUNE
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let hits = "DELETE FROM rate_limit_hits WHERE created_at < now() - interval '1 day'";
        if let Err(e) = db.execute_unprepared(hits).await {
            tracing::warn!(error = %e, "rate limit prune failed");
        }
        let windows =
            "DELETE FROM rate_limit_windows WHERE window_start < now() - interval '1 day'";
        if let Err(e) = db.execute_unprepared(windows).await {
            tracing::warn!(error = %e, "rate limit window prune failed");
        }
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
    use std::net::IpAddr;

    use super::bucket;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv4_and_its_mapped_ipv6_are_one_bucket() {
        assert_eq!(bucket(ip("203.0.113.7")), ip("203.0.113.7"));
        assert_eq!(bucket(ip("::ffff:203.0.113.7")), ip("203.0.113.7"));
    }

    #[test]
    fn ipv6_is_counted_by_its_slash_64() {
        let one = bucket(ip("2001:db8:1:2::1"));
        assert_eq!(one, bucket(ip("2001:db8:1:2:ffff:ffff:ffff:ffff")));
        assert_ne!(one, bucket(ip("2001:db8:1:3::1")));
    }
}
