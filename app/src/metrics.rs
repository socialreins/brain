//! Post metrics: the likes, comments, shares and views of published targets, read from the
//! platforms that give them with the scopes an account already has (`SUPPORTED`; the root post
//! of a thread only), one latest row per target in `post_metrics`, no history.
//!
//! `tick` (every 15 minutes) reads the due targets, grouped by account and batched as each
//! platform allows. A batch is claimed first (`claim`, one statement, which is also the
//! once-a-minute limit of `POST /posts/{id}/metrics/refresh`), so two instances never read one
//! target twice and no connection is held while a platform is asked. A failed batch keeps the
//! old numbers, stores `error` and waits twice as long per failed try, at most a day; a post the
//! platform does not show is `gone` and asked for again a week later, `GONE_TRIES` times. A
//! reconnect of the account clears both (`reset`), so a lost permission heals itself.
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

use crate::{
    connectors::{bluesky::PostError, meta::Uncounted},
    models::social_accounts,
    publish,
    state::AppState,
};

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
/// What a video node (a Reel) has of them: no reactions and no shares.
const FACEBOOK_VIDEO_FIELDS: &str =
    "likes.summary(total_count).limit(0),comments.summary(total_count).limit(0)";
const INSTAGRAM_FIELDS: &str = "like_count,comments_count";
/// Times a `gone` post is asked for again, a week apart.
const GONE_TRIES: i32 = 4;

/// When the tick reads a target again: never read, or last tried over an hour ago (12 hours once
/// the post is two days old), doubled per failed try up to a day; a `gone` one a week after its
/// last try, `GONE_TRIES` times. Not after 30 days.
fn due_now() -> String {
    format!(
        "t.published_at > now() - interval '30 days' AND (m.target_id IS NULL \
         OR (m.error = 'gone' AND m.attempts <= {GONE_TRIES} \
           AND m.fetched_at <= now() - interval '7 days') \
         OR (m.error IS DISTINCT FROM 'gone' AND m.fetched_at <= now() - LEAST( \
           (CASE WHEN t.published_at > now() - interval '48 hours' THEN interval '1 hour' \
             ELSE interval '12 hours' END) * power(2, LEAST(m.attempts, 10)), \
           interval '24 hours')))"
    )
}

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

/// Forgets the errors of an account's targets, `gone` included, so the next round reads them
/// again: called when the account is connected again, which is what mends a lost permission.
pub async fn reset(db: &impl ConnectionTrait, account: Uuid) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE post_metrics SET error = NULL, attempts = 0 WHERE error IS NOT NULL \
         AND target_id IN (SELECT id FROM post_targets WHERE social_account_id = $1)",
        vec![account.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
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

/// One post's counts, or why the platform would not give them for it alone.
type Read = Result<Counts, String>;

/// Meta's answer for `ids`, by id, one call each (Graph v26 has no `/?ids=`); a post that is not
/// there is left out. One that is refused is asked once more with `video` (the fields a video
/// node has, for a Reel); what is still refused is that post's error only. `Err` is a failure of
/// the account (token, rate limit, Meta down), where asking again would not help.
async fn meta_counts(
    app: &AppState,
    token: &str,
    ids: &[&str],
    (fields, video): (&str, Option<&str>),
    counts: fn(&Json) -> Counts,
) -> Result<HashMap<String, Read>, String> {
    let mut found = HashMap::new();
    for id in ids {
        let mut got = app.meta.counts(token, id, fields).await;
        if let (Err(Uncounted::Refused(_)), Some(video)) = (&got, video) {
            got = app.meta.counts(token, id, video).await;
        }
        match got {
            Ok(one) if one.is_object() => {
                found.insert((*id).to_owned(), Ok(counts(&one)));
            }
            Ok(_) | Err(Uncounted::Missing) => {}
            Err(Uncounted::Refused(msg)) => {
                found.insert((*id).to_owned(), Err(msg));
            }
            Err(Uncounted::Failed(e)) => return Err(why(e)),
        }
    }
    Ok(found)
}

/// The counts of the posts `ids` of one account, by remote id; a post the platform no longer
/// has is left out.
async fn fetch(
    app: &AppState,
    account: &social_accounts::Model,
    token: &str,
    ids: &[&str],
) -> Result<HashMap<String, Read>, String> {
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
                .filter_map(|p| Some((p["uri"].as_str()?.to_owned(), Ok(counts(p)))))
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
                .filter_map(|v| Some((v["id"].as_str()?.to_owned(), Ok(counts(&v["statistics"])))))
                .collect())
        }
        "facebook" => {
            let fields = (FACEBOOK_FIELDS, Some(FACEBOOK_VIDEO_FIELDS));
            meta_counts(app, token, ids, fields, |p| {
                let post = p["reactions"].is_object();
                let likes = if post { &p["reactions"] } else { &p["likes"] };
                Counts {
                    likes: num(&likes["summary"]["total_count"]),
                    comments: num(&p["comments"]["summary"]["total_count"]),
                    // Graph leaves `shares` out of a post nobody shared; a video has none.
                    shares: post.then(|| num(&p["shares"]["count"]).unwrap_or(0)),
                    views: None,
                }
            })
            .await
        }
        "instagram" => {
            meta_counts(app, token, ids, (INSTAGRAM_FIELDS, None), |m| Counts {
                // Left out when the account hides its like counts.
                likes: num(&m["like_count"]),
                comments: num(&m["comments_count"]),
                shares: None,
                views: None,
            })
            .await
        }
        _ => Err("unsupported".into()),
    }
}

/// Saves what a read of `batch` gave: the counts, `gone` for a post the platform does not show
/// (`not_found` while it is under an hour old, since a platform can be slow to show a new
/// post), or the error, of the post or of the whole read, with one more failed try. `attempts`
/// of a `gone` row counts the times it was found gone.
async fn record(
    db: &DatabaseConnection,
    batch: &[&Due],
    got: &Result<HashMap<String, Read>, String>,
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
            Ok(Some(Ok(c))) => sql(
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
                "UPDATE post_metrics SET fetched_at = now(), error = 'gone', attempts = \
                 CASE WHEN error = 'gone' THEN attempts + 1 ELSE 1 END WHERE target_id = $1",
                vec![d.target.into()],
            ),
            Ok(None) => failed(d, "not_found"),
            Ok(Some(Err(e))) | Err(e) => failed(d, e),
        };
        db.execute_raw(stmt).await?;
    }
    Ok(())
}

/// Reads the targets of one account, a batch at a time (25 for Bluesky, 50 for the others). An
/// account whose token cannot be had (it needs a reconnect, or its renewal failed) is skipped:
/// its targets are claimed all the same, so they wait their turn again (an hour, or 12) and do
/// not fill the front of every round while the account is out.
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
        return claim(&app.db, &rows).await.map(|_| ());
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
    let rows = due(&lock, &due_now(), vec![]).await?;
    lock.commit().await?;
    read(app, rows, Instant::now() + ROUND).await;
    Ok(())
}
