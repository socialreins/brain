//! Post metrics: the likes, comments, shares and views of published targets, read from the
//! platforms that give them with the scopes an account already has (`SUPPORTED`; the root post
//! of a thread only), one latest row per target in `post_metrics`, no history.
//!
//! `tick` (every 15 minutes) reads the due targets, grouped by account and batched as each
//! platform allows. A batch is claimed first (`claim`, one statement, which is also the
//! once-a-minute limit of `POST /posts/{id}/metrics/refresh`), so two instances never read one
//! target twice and no connection is held while a platform is asked. A failed batch keeps the
//! old numbers, stores `error` and waits twice as long per failed try, at most a day; a post the
//! platform no longer has is `gone` and is not read again by the tick.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, EntityTrait, Statement,
    TransactionTrait, Value,
};
use serde_json::Value as Json;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{connectors::bluesky::PostError, models::social_accounts, publish, state::AppState};

/// The platforms whose counts are read. Threads, LinkedIn, X and TikTok would need scopes or
/// paid access the accounts do not have, so they are never called.
pub const SUPPORTED: [&str; 4] = ["bluesky", "facebook", "instagram", "youtube"];
pub const EVERY_SECS: u64 = 15 * 60;
const LOCK: i64 = 0x004d_4554_5249_4353;
/// No batch starts after this much of a round; what is left stays due.
const ROUND: Duration = Duration::from_secs(5 * 60);
/// Targets a round takes, the longest unread first.
const PER_TICK: usize = 500;
/// Accounts read at once.
const AT_ONCE: usize = 8;
const FACEBOOK_FIELDS: &str =
    "reactions.summary(total_count).limit(0),comments.summary(total_count).limit(0),shares";
const INSTAGRAM_FIELDS: &str = "like_count,comments_count";

/// When the tick reads a target again: never read, or last tried over an hour ago (12 hours once
/// the post is two days old), doubled per failed try up to a day. Not after 30 days.
const DUE: &str = "t.published_at > now() - interval '30 days' \
    AND (m.target_id IS NULL OR (m.error IS DISTINCT FROM 'gone' AND m.fetched_at <= now() - LEAST( \
      (CASE WHEN t.published_at > now() - interval '48 hours' THEN interval '1 hour' \
        ELSE interval '12 hours' END) * power(2, LEAST(m.attempts, 10)), interval '24 hours')))";

/// What a platform counts on one post; `None` is a number it does not give (or hides).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub likes: Option<i64>,
    pub comments: Option<i64>,
    pub shares: Option<i64>,
    pub views: Option<i64>,
}

/// A published target whose counts can be read.
pub struct Due {
    target: Uuid,
    account: Uuid,
    remote_id: String,
    /// Published over an hour ago, so a platform that does not know it has lost it.
    old: bool,
    /// Tried within the last minute.
    pub recent: bool,
}

fn sql(text: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, text, values)
}

/// The published targets of supported platforms that match `cond`, the longest unread first, at
/// most `PER_TICK`. An account that needs a reconnect is left out, as is a disconnected one.
async fn due(db: &impl ConnectionTrait, cond: &str, values: Vec<Value>) -> Result<Vec<Due>, DbErr> {
    let platforms = SUPPORTED.map(|p| format!("'{p}'")).join(", ");
    let text = format!(
        "SELECT t.id AS target, t.social_account_id AS account, t.remote_id, \
           t.published_at <= now() - interval '1 hour' AS old, \
           COALESCE(m.fetched_at > now() - interval '1 minute', false) AS recent \
         FROM post_targets t \
         JOIN posts p ON p.id = t.post_id \
         JOIN social_accounts a ON a.id = t.social_account_id \
         LEFT JOIN post_metrics m ON m.target_id = t.id \
         WHERE t.status = 'published' AND t.remote_id IS NOT NULL AND t.published_at IS NOT NULL \
           AND p.platform IN ({platforms}) \
           AND COALESCE(a.meta->>'reconnect', 'false') <> 'true' AND {cond} \
         ORDER BY m.fetched_at ASC NULLS FIRST, t.id LIMIT {PER_TICK}"
    );
    let rows = db.query_all_raw(sql(&text, values)).await?;
    rows.iter()
        .map(|r| {
            Ok(Due {
                target: r.try_get("", "target")?,
                account: r.try_get("", "account")?,
                remote_id: r.try_get("", "remote_id")?,
                old: r.try_get("", "old")?,
                recent: r.try_get("", "recent")?,
            })
        })
        .collect()
}

/// The targets of post `id` a refresh may read, whatever their age.
pub async fn of_post(db: &DatabaseConnection, id: Uuid) -> Result<Vec<Due>, DbErr> {
    due(db, "p.id = $1", vec![id.into()]).await
}

/// Marks `batch` as tried now and returns the targets this call got: one tried within the last
/// minute (by another instance, or a refresh) is left to that try.
async fn claim(db: &DatabaseConnection, batch: &[Due]) -> Result<HashSet<Uuid>, DbErr> {
    // The ids are UUIDs read from the database, so they are safe to write into the statement.
    let ids: Vec<String> = batch.iter().map(|d| format!("'{}'", d.target)).collect();
    let text = format!(
        "INSERT INTO post_metrics (target_id, post_id, team_id, platform, fetched_at) \
         SELECT t.id, p.id, p.team_id, p.platform, now() FROM post_targets t \
         JOIN posts p ON p.id = t.post_id WHERE t.id IN ({}) \
         ON CONFLICT (target_id) DO UPDATE SET fetched_at = now() \
           WHERE post_metrics.fetched_at <= now() - interval '1 minute' \
         RETURNING target_id",
        ids.join(", ")
    );
    let rows = db.query_all_raw(sql(&text, vec![])).await?;
    rows.iter().map(|r| r.try_get("", "target_id")).collect()
}

fn why(e: PostError) -> String {
    match e {
        PostError::Reconnect => "reconnect".into(),
        PostError::Denied => "denied".into(),
        PostError::Failed(msg) => msg,
    }
}

/// A count as a platform writes it: a number, or (YouTube) a string of digits.
fn num(v: &Json) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

/// Meta's answer for `ids`, keyed by id. One missing object fails a whole `?ids=` call, so
/// then each id is asked on its own and the missing ones are left out.
async fn meta_counts(
    app: &AppState,
    token: &str,
    ids: &[&str],
    fields: &str,
) -> Result<Json, PostError> {
    if let Some(all) = app.meta.counts(token, ids, fields).await? {
        return Ok(all);
    }
    let mut found = serde_json::Map::new();
    if ids.len() > 1 {
        for id in ids {
            if let Some(Json::Object(one)) = app.meta.counts(token, &[id], fields).await? {
                found.extend(one);
            }
        }
    }
    Ok(Json::Object(found))
}

/// The counts of the posts `ids` of one account, by remote id; a post the platform no longer
/// has is left out.
async fn fetch(
    app: &AppState,
    account: &social_accounts::Model,
    token: &str,
    ids: &[&str],
) -> Result<HashMap<String, Counts>, String> {
    let by_id = |all: Json, counts: fn(&Json) -> Counts| -> HashMap<String, Counts> {
        let all = all.as_object().cloned().unwrap_or_default();
        all.iter().map(|(id, v)| (id.clone(), counts(v))).collect()
    };
    match account.provider.as_str() {
        "bluesky" => {
            let creds = publish::creds(account, token);
            let posts = app.bluesky.posts(&creds, ids).await;
            let posts = posts.map_err(|e| e.code().to_owned())?;
            let counts = |p: &Json| Counts {
                likes: num(&p["likeCount"]),
                comments: num(&p["replyCount"]),
                shares: Some(
                    num(&p["repostCount"]).unwrap_or(0) + num(&p["quoteCount"]).unwrap_or(0),
                ),
                views: None,
            };
            Ok(posts
                .iter()
                .filter_map(|p| Some((p["uri"].as_str()?.to_owned(), counts(p))))
                .collect())
        }
        "youtube" => {
            let videos = app.youtube.statistics(token, ids).await.map_err(why)?;
            let counts = |s: &Json| Counts {
                likes: num(&s["likeCount"]),
                comments: num(&s["commentCount"]),
                shares: None,
                views: num(&s["viewCount"]),
            };
            Ok(videos
                .iter()
                .filter_map(|v| Some((v["id"].as_str()?.to_owned(), counts(&v["statistics"]))))
                .collect())
        }
        "facebook" => {
            let all = meta_counts(app, token, ids, FACEBOOK_FIELDS).await;
            Ok(by_id(all.map_err(why)?, |p| Counts {
                likes: num(&p["reactions"]["summary"]["total_count"]),
                comments: num(&p["comments"]["summary"]["total_count"]),
                // Graph leaves `shares` out of a post nobody shared.
                shares: Some(num(&p["shares"]["count"]).unwrap_or(0)),
                views: None,
            }))
        }
        "instagram" => {
            let all = meta_counts(app, token, ids, INSTAGRAM_FIELDS).await;
            Ok(by_id(all.map_err(why)?, |m| Counts {
                // Left out when the account hides its like counts.
                likes: num(&m["like_count"]),
                comments: num(&m["comments_count"]),
                shares: None,
                views: None,
            }))
        }
        _ => Err("unsupported".into()),
    }
}

/// Saves what a read of `batch` gave: the counts, `gone` for a post the platform no longer has
/// (`not_found` while it is under an hour old, since a platform can be slow to show a new
/// post), or the error with one more failed try.
async fn record(
    db: &DatabaseConnection,
    batch: &[&Due],
    got: &Result<HashMap<String, Counts>, String>,
) -> Result<(), DbErr> {
    let failed = |d: &Due, error: &str| {
        sql(
            "UPDATE post_metrics SET error = $2, attempts = attempts + 1, fetched_at = now() \
             WHERE target_id = $1",
            vec![d.target.into(), error.into()],
        )
    };
    for d in batch {
        let stmt = match got.as_ref().map(|all| all.get(&d.remote_id)) {
            Ok(Some(c)) => sql(
                "UPDATE post_metrics SET likes = $2, comments = $3, shares = $4, views = $5, \
                 fetched_at = now(), error = NULL, attempts = 0 WHERE target_id = $1",
                vec![
                    d.target.into(),
                    c.likes.into(),
                    c.comments.into(),
                    c.shares.into(),
                    c.views.into(),
                ],
            ),
            Ok(None) if d.old => sql(
                "UPDATE post_metrics SET error = 'gone', fetched_at = now() WHERE target_id = $1",
                vec![d.target.into()],
            ),
            Ok(None) => failed(d, "not_found"),
            Err(e) => failed(d, e),
        };
        db.execute_raw(stmt).await?;
    }
    Ok(())
}

/// Reads the targets of one account, a batch at a time (25 for Bluesky, 50 for the others). An
/// account whose token cannot be had (it needs a reconnect, or its renewal failed) is skipped.
async fn read_account(
    app: &AppState,
    account: Uuid,
    rows: Vec<Due>,
    until: Instant,
) -> Result<(), DbErr> {
    let Some(account) = social_accounts::Entity::find_by_id(account)
        .one(&app.db)
        .await?
    else {
        return Ok(());
    };
    let Ok(token) = publish::credential(app, &account).await else {
        return Ok(());
    };
    let size = if account.provider == "bluesky" {
        25
    } else {
        50
    };
    for batch in rows.chunks(size) {
        if Instant::now() >= until {
            break;
        }
        let mine = claim(&app.db, batch).await?;
        let batch: Vec<&Due> = batch.iter().filter(|d| mine.contains(&d.target)).collect();
        if batch.is_empty() {
            continue;
        }
        let ids: Vec<&str> = batch.iter().map(|d| d.remote_id.as_str()).collect();
        let got = fetch(app, &account, &token, &ids).await;
        if let Err(e) = &got {
            tracing::warn!(error = %e, account = %account.id, "metrics read failed");
        }
        record(&app.db, &batch, &got).await?;
    }
    Ok(())
}

/// Reads and stores the counts of `rows`, `AT_ONCE` accounts at a time; no batch starts after
/// `until`.
pub async fn read(app: &AppState, rows: Vec<Due>, until: Instant) {
    let mut by_account: HashMap<Uuid, Vec<Due>> = HashMap::new();
    for d in rows {
        by_account.entry(d.account).or_default().push(d);
    }
    let mut running = tokio::task::JoinSet::new();
    for (account, rows) in by_account {
        if running.len() >= AT_ONCE {
            running.join_next().await;
        }
        let app = app.clone();
        running.spawn(async move {
            if let Err(e) = read_account(&app, account, rows, until).await {
                tracing::warn!(error = %e, account = %account, "metrics not saved");
            }
        });
    }
    // A read that panicked ends its own task only.
    while running.join_next().await.is_some() {}
}

/// One round: the due targets, read within `ROUND`. Skips while another instance holds the lock.
/// The lock is transaction-level, as in the other ticks, and covers only the read of what is
/// due: each batch is claimed before its platform is asked, so no connection waits on one.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    let rows = due(&lock, DUE, vec![]).await?;
    lock.commit().await?;
    read(app, rows, Instant::now() + ROUND).await;
    Ok(())
}
