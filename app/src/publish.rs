//! Publishing: claim due posts, fan each out to its set's accounts, publish every target once,
//! settle the post; and take published posts down. Every step is a conditional update, so two
//! instances (or a tick and "publish now") never send a post twice. A publish is never retried:
//! a retry could post twice. Each target status change queues its webhook event in the same
//! statement (`webhooks::with_event`).
use std::{sync::LazyLock, time::Duration};

use tokio::sync::Semaphore;

use async_trait::async_trait;
use sea_orm::{
    sea_query::Query, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, QueryFilter, QuerySelect, Statement, TransactionTrait, Value,
};
use uuid::Uuid;

pub use crate::connectors::bluesky::PostError as PublishError;
use crate::{
    connectors::bluesky::{self, Bluesky},
    models::{
        post_targets, posts, social_account_social_set as membership, social_accounts, social_sets,
    },
    rate_limit::reserve_in,
    state::AppState,
    webhooks::with_event,
};

/// One publisher per platform.
#[async_trait]
pub trait Publisher: Send + Sync {
    /// Publishes and returns the platform's id for the post.
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[String],
    ) -> Result<String, PublishError>;

    async fn delete(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError>;

    /// The public link to a published post.
    fn url(&self, account: &social_accounts::Model, remote_id: &str) -> Option<String>;
}

fn pds(account: &social_accounts::Model) -> &str {
    account.meta["pds"].as_str().unwrap_or_default()
}

#[async_trait]
impl Publisher for Bluesky {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[String],
    ) -> Result<String, PublishError> {
        self.publish_post(pds(account), &account.external_id, secret, body, media)
            .await
    }

    async fn delete(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_post(pds(account), &account.external_id, secret, remote_id)
            .await
    }

    fn url(&self, _: &social_accounts::Model, remote_id: &str) -> Option<String> {
        bluesky::post_url(remote_id)
    }
}

/// The publisher for `platform`; `None` until its connector exists.
pub fn publisher<'a>(app: &'a AppState, platform: &str) -> Option<&'a dyn Publisher> {
    match platform {
        "bluesky" => Some(&app.bluesky),
        _ => None,
    }
}

/// Due posts claimed per tick; the rest wait for the next one.
const CLAIM_BATCH: i64 = 50;
/// Posts publishing at once, and targets publishing at once, across the process.
static POST_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
static TARGET_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
/// Posts a team publishes a day, scheduled or published now (`rate_limit::daily_reserve`).
pub const PUBLISHES_PER_DAY: i64 = 100;
const OVER_QUOTA: &str = "This team reached its limit of 100 published posts a day. It stays \
                          scheduled and goes out once the limit frees up, within a day.";

/// The daily publish bucket of a team.
pub fn quota_bucket(team_id: Uuid) -> String {
    format!("publish:team:{team_id}")
}

/// Longest one target may take to publish or delete.
const TARGET_TIMEOUT: Duration = Duration::from_secs(120);
/// A target `publishing` this long was cut off (crash, restart) and may have posted.
const STUCK_MINUTES: i64 = 15;
const PUBLISH_LOCK: i64 = 0x0050_5542_4c49_5348;

pub const RECONNECT: &str =
    "Bluesky refused the app password. Reconnect the account under Connections.";
const STUCK: &str = "Unknown, check the account: publishing stopped midway and it may have posted.";

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

async fn ids(db: &impl ConnectionTrait, stmt: Statement, col: &str) -> Result<Vec<Uuid>, DbErr> {
    let rows = db.query_all_raw(stmt).await?;
    rows.iter().map(|r| r.try_get("", col)).collect()
}

/// One publishing round: fail cut-off targets, claim due posts, pick up posts left `queued`,
/// publish them all and wait. Skips while another instance holds the lock.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![PUBLISH_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    // One failing step is logged and the tick goes on; per-post work never aborts it.
    if let Err(e) = recover_stuck(&app.db).await {
        tracing::warn!(error = %e, "stuck target recovery failed");
    }
    // The quota is reserved per post in `publish_post`, together with marking it reserved.
    let mut due: Vec<Uuid> = claim_due(&app.db).await?.into_iter().map(|c| c.0).collect();
    // Posts left `queued` by a crash between claim and settle: fan-out and claims are
    // idempotent, so running them again only finishes the job.
    let stmt = sql(
        "SELECT id FROM posts WHERE status = 'queued' \
         AND updated_at < now() - make_interval(mins => $1) LIMIT $2",
        vec![(STUCK_MINUTES as i32).into(), CLAIM_BATCH.into()],
    );
    due.extend(ids(&app.db, stmt, "id").await?);
    let runs: Vec<_> = due
        .into_iter()
        .map(|id| {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = publish_post(&app, id).await {
                    tracing::error!(error = %e, post = %id, "publish failed");
                }
            })
        })
        .collect();
    for run in runs {
        let _ = run.await;
    }
    lock.commit().await
}

/// Claims up to 50 due `scheduled` posts, oldest first, `scheduled` to `queued`: only posts of
/// a live set, of a team under its daily publish cap. Returns `(post, team)`.
pub async fn claim_due(db: &DatabaseConnection) -> Result<Vec<(Uuid, Uuid)>, DbErr> {
    // A CTE runs once; a LIMIT inside `IN (...)` may be re-run and claim more.
    let stmt = sql(
        "WITH due AS ( \
           SELECT d.id FROM posts d \
           WHERE d.status = 'scheduled' AND d.scheduled_at <= now() \
           AND EXISTS (SELECT 1 FROM social_sets s \
                       WHERE s.id = d.social_set_id AND s.deleted_at IS NULL) \
           AND (SELECT count(*) FROM rate_limit_hits h \
                WHERE h.bucket = 'publish:team:' || d.team_id::text \
                AND h.created_at > now() - interval '1 day') < $1 \
           ORDER BY d.scheduled_at, d.id LIMIT $2 \
           FOR UPDATE SKIP LOCKED) \
         UPDATE posts p SET status = 'queued', updated_at = now() FROM due \
         WHERE p.id = due.id AND p.status = 'scheduled' \
         RETURNING p.id, p.team_id",
        vec![PUBLISHES_PER_DAY.into(), CLAIM_BATCH.into()],
    );
    let rows = db.query_all_raw(stmt).await?;
    rows.iter()
        .map(|r| Ok((r.try_get("", "id")?, r.try_get("", "team_id")?)))
        .collect()
}

pub enum Quota {
    Reserved,
    /// The team is at its daily cap.
    Over,
    /// The post changed meanwhile; nothing was reserved.
    Lost,
}

/// In one transaction: reserve a hit of the team's daily publish cap (unless the post already
/// holds one) and run `update`, which must set `quota_reserved = true` on exactly this post.
/// The hit is rolled back when the update touches nothing.
pub async fn reserve_and(
    db: &DatabaseConnection,
    team_id: Uuid,
    already: bool,
    update: Statement,
) -> Result<Quota, DbErr> {
    let txn = db.begin().await?;
    if !already {
        let cap = [(quota_bucket(team_id), PUBLISHES_PER_DAY)];
        if reserve_in(&txn, &cap).await?.is_none() {
            return Ok(Quota::Over);
        }
    }
    if txn.execute_raw(update).await?.rows_affected() != 1 {
        return Ok(Quota::Lost);
    }
    txn.commit().await?;
    Ok(Quota::Reserved)
}

/// Puts a claimed post back to `scheduled`, with `error` saying why. It reserves again when it
/// is next claimed.
async fn unclaim(db: &DatabaseConnection, id: Uuid, error: Option<&str>) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE posts SET status = 'scheduled', error = COALESCE($2, error), \
         quota_reserved = false, updated_at = now() \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), error.map(str::to_owned).into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Targets cut off mid-publish become `failed` and are never sent again; their posts settle.
pub async fn recover_stuck(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Cut off after the platform took it (the remote id is saved first): it is live, so it is
    // published, and stays takeable down.
    let stmt = sql(
        "SELECT id, post_id, remote_id FROM post_targets \
         WHERE status = 'publishing' AND remote_id IS NOT NULL \
         AND updated_at < now() - make_interval(mins => $1)",
        vec![(STUCK_MINUTES as i32).into()],
    );
    let mut posts = Vec::new();
    for row in db.query_all_raw(stmt).await? {
        let (id, post, remote): (Uuid, Uuid, String) = (
            row.try_get("", "id")?,
            row.try_get("", "post_id")?,
            row.try_get("", "remote_id")?,
        );
        // shortcut: Bluesky is the only publisher; use the platform's `url` once there are more.
        let url = bluesky::post_url(&remote);
        let stmt = sql(
            &with_event(
                "UPDATE post_targets SET status = 'published', remote_url = $2, error = NULL, \
                 published_at = COALESCE(published_at, now()), updated_at = now() \
                 WHERE id = $1 AND status = 'publishing'",
            ),
            vec![id.into(), url.into()],
        );
        db.execute_raw(stmt).await?;
        posts.push(post);
    }
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'failed', error = $1, updated_at = now() \
             WHERE status = 'publishing' AND remote_id IS NULL \
             AND updated_at < now() - make_interval(mins => $2)",
        ),
        vec![STUCK.into(), (STUCK_MINUTES as i32).into()],
    );
    posts.extend(ids(db, stmt, "post_id").await?);
    posts.sort();
    posts.dedup();
    for id in posts {
        if let Err(e) = settle(db, id).await {
            tracing::warn!(error = %e, post = %id, "settle after recovery failed");
        }
    }
    Ok(())
}

async fn fail_post(db: &DatabaseConnection, id: Uuid, msg: &str) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE posts SET status = 'failed', error = $2, updated_at = now() \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), msg.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Publishes a `queued` post: one target per account of its platform in its set, each
/// published at most once, then the post settles.
pub async fn publish_post(app: &AppState, id: Uuid) -> Result<(), DbErr> {
    let Ok(_slot) = POST_SLOTS.acquire().await else {
        return Ok(());
    };
    let db = &app.db;
    let Some(post) = posts::Entity::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    if post.status != "queued" {
        return Ok(());
    }
    // Whatever queued it (a tick, publish now, recovery), nothing publishes without its hit.
    if !post.quota_reserved {
        let mark = sql(
            "UPDATE posts SET quota_reserved = true \
             WHERE id = $1 AND status = 'queued' AND NOT quota_reserved",
            vec![id.into()],
        );
        let back = match reserve_and(db, post.team_id, false, mark).await {
            Ok(Quota::Reserved) => None,
            Ok(Quota::Over) => Some(Some(OVER_QUOTA)),
            Ok(Quota::Lost) => return Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, post = %id, "publish quota reserve failed");
                Some(None)
            }
        };
        if let Some(error) = back {
            return unclaim(db, id, error).await;
        }
    }
    let set_live = social_sets::Entity::find_by_id(post.social_set_id)
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(db)
        .await?
        .is_some();
    if !set_live {
        // The set was deleted after the claim: back to `scheduled`, to go out if it returns.
        return unclaim(db, id, None).await;
    }
    if publisher(app, &post.platform).is_none() {
        let msg = format!("Publishing to {} is not available yet.", post.platform);
        return fail_post(db, id, &msg).await;
    }
    let in_set = Query::select()
        .column(membership::Column::SocialAccountId)
        .from(membership::Entity)
        .and_where(membership::Column::SocialSetId.eq(post.social_set_id))
        .to_owned();
    let accounts: Vec<Uuid> = social_accounts::Entity::find()
        .select_only()
        .column(social_accounts::Column::Id)
        .filter(social_accounts::Column::TeamId.eq(post.team_id))
        .filter(social_accounts::Column::Provider.eq(&post.platform))
        .filter(social_accounts::Column::Id.in_subquery(in_set))
        .into_tuple()
        .all(db)
        .await?;
    for account in &accounts {
        let stmt = sql(
            "INSERT INTO post_targets (id, post_id, social_account_id, status) \
             VALUES ($1, $2, $3, 'pending') ON CONFLICT (post_id, social_account_id) DO NOTHING",
            vec![Uuid::now_v7().into(), id.into(), (*account).into()],
        );
        db.execute_raw(stmt).await?;
    }
    let targets = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(id))
        .all(db)
        .await?;
    if targets.is_empty() {
        let msg = format!("No {} account is in this post's set.", post.platform);
        return fail_post(db, id, &msg).await;
    }
    let runs: Vec<_> = targets
        .into_iter()
        .filter(|t| t.status == "pending")
        .map(|t| {
            let (app, post) = (app.clone(), post.clone());
            tokio::spawn(async move { publish_target(&app, &post, t.id).await })
        })
        .collect();
    for run in runs {
        match run.await {
            Ok(Err(e)) => tracing::error!(error = %e, post = %id, "target publish failed"),
            Err(e) => tracing::error!(error = %e, post = %id, "target task failed"),
            Ok(Ok(())) => {}
        }
    }
    settle(db, id).await
}

async fn fail_target(db: &DatabaseConnection, id: Uuid, msg: &str) -> Result<(), DbErr> {
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'failed', error = $2, updated_at = now() \
             WHERE id = $1 AND status = 'publishing'",
        ),
        vec![id.into(), msg.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Claims a target (`pending` with no remote id, to `publishing`); false when another run has it.
pub async fn claim_target(db: &DatabaseConnection, id: Uuid) -> Result<bool, DbErr> {
    let stmt = sql(
        "UPDATE post_targets SET status = 'publishing', updated_at = now() \
         WHERE id = $1 AND status = 'pending' AND remote_id IS NULL",
        vec![id.into()],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected() == 1)
}

async fn publish_target(app: &AppState, post: &posts::Model, id: Uuid) -> Result<(), DbErr> {
    let Ok(_slot) = TARGET_SLOTS.acquire().await else {
        return Ok(());
    };
    let db = &app.db;
    if !claim_target(db, id).await? {
        return Ok(());
    }
    let target = post_targets::Entity::find_by_id(id).one(db).await?;
    let account = match target.and_then(|t| t.social_account_id) {
        Some(a) => social_accounts::Entity::find_by_id(a).one(db).await?,
        None => None,
    };
    let Some(account) = account else {
        return fail_target(db, id, "The account was disconnected.").await;
    };
    let Some(publisher) = publisher(app, &post.platform) else {
        return fail_target(db, id, "This platform cannot publish yet.").await;
    };
    let Some(secret) = app
        .cipher
        .decrypt(&account.access_token)
        .and_then(|b| String::from_utf8(b).ok())
    else {
        return fail_target(db, id, RECONNECT).await;
    };
    let media: Vec<String> = serde_json::from_value(post.media.clone()).unwrap_or_default();
    let sent = tokio::time::timeout(
        TARGET_TIMEOUT,
        publisher.publish(&account, &secret, &post.body, &media),
    )
    .await;
    let remote_id = match sent {
        Ok(Ok(remote_id)) => remote_id,
        Ok(Err(PublishError::Reconnect)) => return fail_target(db, id, RECONNECT).await,
        Ok(Err(PublishError::Failed(msg))) => return fail_target(db, id, &msg).await,
        Err(_) => {
            return fail_target(db, id, "Timed out; it may have posted, check the account.").await
        }
    };
    // The remote id first: whatever fails after this, the post is known to be live.
    let stmt = sql(
        "UPDATE post_targets SET remote_id = $2, updated_at = now() WHERE id = $1",
        vec![id.into(), remote_id.clone().into()],
    );
    db.execute_raw(stmt).await?;
    let url = publisher.url(&account, &remote_id);
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'published', remote_url = $2, error = NULL, \
             published_at = now(), updated_at = now() WHERE id = $1 AND status = 'publishing'",
        ),
        vec![id.into(), url.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Settles a `queued` post once no target is `pending` or `publishing`: `failed` when every
/// target failed, else `published`, with each failed account named in `error`. The post row
/// is locked, so concurrent settles agree.
pub async fn settle(db: &DatabaseConnection, id: Uuid) -> Result<(), DbErr> {
    let txn = db.begin().await?;
    let post = posts::Entity::find_by_id(id)
        .lock_exclusive()
        .one(&txn)
        .await?;
    if !post.is_some_and(|p| p.status == "queued") {
        return txn.commit().await;
    }
    let targets = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(id))
        .all(&txn)
        .await?;
    let open = targets
        .iter()
        .any(|t| t.status == "pending" || t.status == "publishing");
    if targets.is_empty() || open {
        return txn.commit().await;
    }
    let failed: Vec<&post_targets::Model> =
        targets.iter().filter(|t| t.status == "failed").collect();
    let mut errors = Vec::new();
    for t in &failed {
        let account = match t.social_account_id {
            Some(a) => social_accounts::Entity::find_by_id(a).one(&txn).await?,
            None => None,
        };
        let name = account
            .and_then(|a| a.handle)
            .map_or_else(|| "A disconnected account".to_owned(), |h| format!("@{h}"));
        errors.push(format!(
            "{name}: {}",
            t.error.as_deref().unwrap_or("failed")
        ));
    }
    let status = if failed.len() == targets.len() {
        "failed"
    } else {
        "published"
    };
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let stmt = sql(
        "UPDATE posts SET status = $2, error = $3, updated_at = now(), \
         published_at = CASE WHEN $2 = 'published' THEN now() ELSE published_at END \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), status.into(), error.into()],
    );
    txn.execute_raw(stmt).await?;
    txn.commit().await
}

/// Takes a `published` post down from every account it is still up on. Each target that comes
/// down is marked `deleted`, so a retry only tries the rest; the post becomes `deleted` once
/// none is left. Returns what to tell the user about each target that stayed up.
pub async fn takedown(app: &AppState, post: &posts::Model) -> Result<Vec<String>, DbErr> {
    let db = &app.db;
    let up = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(post.id))
        .filter(post_targets::Column::Status.eq("published"))
        .all(db)
        .await?;
    let mut problems = Vec::new();
    for t in up {
        let account = match t.social_account_id {
            Some(a) => social_accounts::Entity::find_by_id(a).one(db).await?,
            None => None,
        };
        let Some(account) = account else {
            problems.push(
                "A disconnected account: connect it again, or delete the post in its app.".into(),
            );
            continue;
        };
        let name = format!("@{}", account.handle.as_deref().unwrap_or("account"));
        let secret = app
            .cipher
            .decrypt(&account.access_token)
            .and_then(|b| String::from_utf8(b).ok());
        let (Some(publisher), Some(secret), Some(remote_id)) = (
            publisher(app, &post.platform),
            secret,
            t.remote_id.as_deref(),
        ) else {
            problems.push(format!(
                "{name}: reconnect the account under Connections, or delete the post in the app."
            ));
            continue;
        };
        let done = tokio::time::timeout(
            TARGET_TIMEOUT,
            publisher.delete(&account, &secret, remote_id),
        )
        .await;
        match done {
            Ok(Ok(())) => {
                let stmt = sql(
                    &with_event(
                        "UPDATE post_targets SET status = 'deleted', updated_at = now() \
                         WHERE id = $1 AND status = 'published'",
                    ),
                    vec![t.id.into()],
                );
                db.execute_raw(stmt).await?;
            }
            Ok(Err(PublishError::Reconnect)) => problems.push(format!(
                "{name}: {RECONNECT} Or delete the post in the Bluesky app."
            )),
            Ok(Err(PublishError::Failed(msg))) => problems.push(format!(
                "{name}: {msg} Delete the post in the Bluesky app if it is still up."
            )),
            Err(_) => problems.push(format!(
                "{name}: Bluesky did not answer in time. Try again, or delete it in the app."
            )),
        }
    }
    if problems.is_empty() {
        let stmt = sql(
            "UPDATE posts SET status = 'deleted', updated_at = now() \
             WHERE id = $1 AND status = 'published' AND NOT EXISTS \
             (SELECT 1 FROM post_targets WHERE post_id = $1 AND status = 'published')",
            vec![post.id.into()],
        );
        db.execute_raw(stmt).await?;
    }
    Ok(problems)
}
