//! Posts of the current team: every member writes them. One post per platform (Bluesky today);
//! a post goes to every account of its platform in its social set. Only `draft` and
//! `scheduled` posts change; saving with `scheduled_at` makes one `scheduled`, else `draft`.
//! Posts of a deleted set are hidden (and held back) until the set is restored. Every route
//! takes an API token too (`Actor`); a set token only sees and writes its set's posts.
use std::collections::HashMap;

use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait, DbBackend, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::post;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body},
    connections::live_sets,
    waitlist::{error, Error},
};
use crate::{
    models::{post_targets, posts, social_sets, teams},
    publish,
    state::AppState,
};

/// Most posts a team keeps that are not `deleted`.
const MAX_POSTS: u64 = 2000;
const STATUSES: [&str; 6] = [
    "draft",
    "scheduled",
    "queued",
    "published",
    "failed",
    "deleted",
];

#[derive(Serialize, ToSchema)]
pub struct Post {
    pub id: Uuid,
    pub social_set_id: Uuid,
    /// `bluesky`.
    pub platform: String,
    pub body: String,
    /// Public `https://` image URLs.
    pub media: Vec<String>,
    /// `draft`, `scheduled`, `queued`, `published`, `failed` or `deleted`.
    pub status: String,
    pub scheduled_at: Option<DateTime<FixedOffset>>,
    pub published_at: Option<DateTime<FixedOffset>>,
    /// Why it failed, naming each account that failed.
    pub error: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    pub targets: Vec<Target>,
}

/// One account a post went (or is going) to.
#[derive(Serialize, ToSchema)]
pub struct Target {
    pub id: Uuid,
    /// `null` once the account was disconnected.
    pub social_account_id: Option<Uuid>,
    /// `pending`, `publishing`, `published`, `failed` or `deleted`.
    pub status: String,
    /// The platform's id, e.g. a Bluesky `at://` URI.
    pub remote_id: Option<String>,
    /// The public link to the post.
    pub remote_url: Option<String>,
    pub error: Option<String>,
    pub published_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema)]
pub struct PostInput {
    /// A live set of the current team. Required, except with a set token, where absent means
    /// the token's set and any other set is `invalid_set`.
    pub social_set_id: Option<Uuid>,
    /// `bluesky` (the only platform that publishes today).
    pub platform: String,
    /// At most 300 graphemes on Bluesky.
    #[serde(default)]
    pub body: String,
    /// At most 4 public `https://` image URLs (JPEG, PNG, WebP or GIF, 1 MB each on Bluesky).
    #[serde(default)]
    pub media: Vec<String>,
    /// When to publish. Set: `scheduled`; absent or `null`: `draft`.
    pub scheduled_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, IntoParams)]
pub struct PostQuery {
    pub social_set_id: Option<Uuid>,
    /// One of the post statuses.
    pub status: Option<String>,
    /// `scheduled_at` at or after this time (drafts without a time drop out).
    pub from: Option<DateTime<FixedOffset>>,
    /// `scheduled_at` before this time.
    pub to: Option<DateTime<FixedOffset>>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn editable(status: &str) -> bool {
    status == "draft" || status == "scheduled"
}

async fn targets_of(
    db: &impl ConnectionTrait,
    posts: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Target>>, DbErr> {
    let rows = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.is_in(posts.iter().copied()))
        .order_by_asc(post_targets::Column::CreatedAt)
        .order_by_asc(post_targets::Column::Id)
        .all(db)
        .await?;
    let mut map: HashMap<Uuid, Vec<Target>> = HashMap::new();
    for t in rows {
        map.entry(t.post_id).or_default().push(Target {
            id: t.id,
            social_account_id: t.social_account_id,
            status: t.status,
            remote_id: t.remote_id,
            remote_url: t.remote_url,
            error: t.error,
            published_at: t.published_at,
        });
    }
    Ok(map)
}

fn post_json(p: posts::Model, targets: Vec<Target>) -> Post {
    Post {
        id: p.id,
        social_set_id: p.social_set_id,
        platform: p.platform,
        body: p.body,
        media: serde_json::from_value(p.media).unwrap_or_default(),
        status: p.status,
        scheduled_at: p.scheduled_at,
        published_at: p.published_at,
        error: p.error,
        created_by: p.created_by,
        created_at: p.created_at,
        updated_at: p.updated_at,
        targets,
    }
}

async fn one_json(db: &impl ConnectionTrait, p: posts::Model) -> Result<Post, DbErr> {
    let mut targets = targets_of(db, &[p.id]).await?;
    let t = targets.remove(&p.id).unwrap_or_default();
    Ok(post_json(p, t))
}

/// Posts a set token may touch: its set's. Re-checked in writes, so a concurrent move of the
/// post to another set cannot widen the token's reach.
fn in_scope(a: &Actor) -> Condition {
    match a.set_id() {
        Some(set) => Condition::all().add(posts::Column::SocialSetId.eq(set)),
        None => Condition::all(),
    }
}

/// The caller's post `id`, unless its set is deleted or outside a set token's set.
async fn find(
    db: &impl ConnectionTrait,
    a: &Actor,
    id: Uuid,
) -> Result<Option<posts::Model>, DbErr> {
    let sel = posts::Entity::find_by_id(id)
        .filter(posts::Column::TeamId.eq(a.team.id))
        .filter(posts::Column::SocialSetId.in_subquery(live_sets()));
    sel.filter(in_scope(a)).one(db).await
}

/// Checks a post body for saving: platform rules, then a live set of the team the caller may
/// write to. Answers that set's id.
async fn checked(
    db: &impl ConnectionTrait,
    a: &Actor,
    input: &PostInput,
) -> Result<Result<Uuid, Response>, DbErr> {
    let invalid_set = || reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_set");
    if let Err(e) = post::validate(&input.platform, &input.body, &input.media) {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, e.code())));
    }
    // Another set than a set token's own answers like a missing one.
    let id = match (a.set_id(), input.social_set_id) {
        (Some(own), None) => own,
        (Some(own), Some(id)) if id == own => own,
        (None, Some(id)) => id,
        _ => return Ok(Err(invalid_set())),
    };
    let set = social_sets::Entity::find_by_id(id)
        .filter(social_sets::Column::TeamId.eq(a.team.id))
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(db)
        .await?;
    Ok(match set {
        Some(_) => Ok(id),
        None => Err(invalid_set()),
    })
}

/// `scheduled_at`, with a past time meaning now (as the old app did).
fn when(input: &PostInput) -> Option<DateTime<FixedOffset>> {
    let now = Utc::now().fixed_offset();
    input.scheduled_at.map(|t| t.max(now))
}

fn status_for(input: &PostInput) -> &'static str {
    if input.scheduled_at.is_some() {
        "scheduled"
    } else {
        "draft"
    }
}

/// The current team's posts, by `scheduled_at` (unscheduled last), at most 2000. For a calendar,
/// pass `from` and `to`.
#[utoipa::path(get, path = "/posts", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(PostQuery),
    responses((status = 200, body = [Post]), (status = 401, body = Error),
        (status = 422, body = Error)))]
async fn list(
    State(app): State<AppState>,
    a: Actor,
    q: Result<Query<PostQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    if q.status.as_deref().is_some_and(|s| !STATUSES.contains(&s)) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let found = async {
        let mut sel = posts::Entity::find()
            .filter(posts::Column::TeamId.eq(a.team.id))
            .filter(posts::Column::SocialSetId.in_subquery(live_sets()))
            .filter(in_scope(&a));
        if let Some(set) = q.social_set_id {
            sel = sel.filter(posts::Column::SocialSetId.eq(set));
        }
        if let Some(status) = &q.status {
            sel = sel.filter(posts::Column::Status.eq(status));
        }
        if let Some(from) = q.from {
            sel = sel.filter(posts::Column::ScheduledAt.gte(from));
        }
        if let Some(to) = q.to {
            sel = sel.filter(posts::Column::ScheduledAt.lt(to));
        }
        let rows = sel
            .order_by_asc(posts::Column::ScheduledAt)
            .order_by_asc(posts::Column::CreatedAt)
            .order_by_asc(posts::Column::Id)
            .limit(MAX_POSTS)
            .all(&app.db)
            .await?;
        let ids: Vec<Uuid> = rows.iter().map(|p| p.id).collect();
        let mut targets = targets_of(&app.db, &ids).await?;
        Ok::<_, DbErr>(
            rows.into_iter()
                .map(|p| {
                    let t = targets.remove(&p.id).unwrap_or_default();
                    post_json(p, t)
                })
                .collect::<Vec<_>>(),
        )
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "post list"),
    }
}

/// One post with its targets.
#[utoipa::path(get, path = "/posts/{id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = Post), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    let found = async {
        match find(&app.db, &a, id).await? {
            Some(p) => one_json(&app.db, p).await.map(Some),
            None => Ok(None),
        }
    };
    match found.await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "post read"),
    }
}

/// Creates a post; `scheduled` with `scheduled_at` (a past time means now), else `draft`. At
/// most 2000 per team.
#[utoipa::path(post, path = "/posts", tag = "posts",
    security(("session" = []), ("api_token" = [])), request_body = PostInput,
    responses((status = 201, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(State(app): State<AppState>, a: Actor, Body(input): Body<PostInput>) -> Response {
    let made = async {
        let set = match checked(&app.db, &a, &input).await? {
            Ok(set) => set,
            Err(res) => return Ok(Err(res)),
        };
        let txn = app.db.begin().await?;
        // The team row lock keeps concurrent creates under the cap.
        teams::Entity::find_by_id(a.team.id)
            .lock_exclusive()
            .one(&txn)
            .await?;
        let live = posts::Entity::find()
            .filter(posts::Column::TeamId.eq(a.team.id))
            .filter(posts::Column::Status.ne("deleted"))
            .count(&txn)
            .await?;
        if live >= MAX_POSTS {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_posts")));
        }
        let row = posts::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(a.team.id),
            social_set_id: Set(set),
            platform: Set(input.platform.clone()),
            body: Set(input.body.clone()),
            media: Set(serde_json::json!(input.media)),
            status: Set(status_for(&input).into()),
            scheduled_at: Set(when(&input)),
            created_by: Set(a.user_id),
            ..Default::default()
        };
        let p = posts::Entity::insert(row).exec_with_returning(&txn).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(post_json(p, Vec::new())))
    };
    match made.await {
        Ok(Ok(p)) => (StatusCode::CREATED, Json(p)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post insert"),
    }
}

/// Replaces a `draft` or `scheduled` post's set, platform, body, media and time; anything else
/// is `409 not_editable`.
#[utoipa::path(put, path = "/posts/{id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), request_body = PostInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<PostInput>,
) -> Response {
    let saved = async {
        let Some(found) = find(&app.db, &a, id).await? else {
            return Ok(Err(not_found()));
        };
        if !editable(&found.status) {
            return Ok(Err(reply(StatusCode::CONFLICT, "not_editable")));
        }
        let set = match checked(&app.db, &a, &input).await? {
            Ok(set) => set,
            Err(res) => return Ok(Err(res)),
        };
        // One conditional statement, so a tick claiming the post cannot interleave.
        let done = posts::Entity::update_many()
            .col_expr(posts::Column::SocialSetId, set.into())
            .col_expr(posts::Column::Platform, input.platform.clone().into())
            .col_expr(posts::Column::Body, input.body.clone().into())
            .col_expr(posts::Column::Media, serde_json::json!(input.media).into())
            .col_expr(posts::Column::Status, status_for(&input).into())
            .col_expr(posts::Column::ScheduledAt, when(&input).into())
            .col_expr(posts::Column::UpdatedAt, Utc::now().fixed_offset().into())
            .filter(posts::Column::Id.eq(id))
            .filter(posts::Column::Status.is_in(["draft", "scheduled"]))
            .filter(in_scope(&a))
            .exec(&app.db)
            .await?;
        if done.rows_affected == 0 {
            return Ok(Err(reply(StatusCode::CONFLICT, "not_editable")));
        }
        match find(&app.db, &a, id).await? {
            Some(p) => one_json(&app.db, p).await.map(Ok),
            None => Ok(Err(not_found())),
        }
    };
    match saved.await {
        Ok(Ok(p)) => Json(p).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post update"),
    }
}

/// Publishes a `draft` or `scheduled` post now: `scheduled_at` becomes now and the post is
/// claimed (`queued`) and sent in the background. `202` with the queued post; poll it for the
/// result. At most 10 publish or take-down calls a minute per user; with scheduled posts, at
/// most 100 published a day per team (`429 publish_quota`).
#[utoipa::path(post, path = "/posts/{id}/publish", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 202, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 429, body = Error)))]
async fn publish_now(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    if !app.post_limiter.hit(a.key()) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let claimed = async {
        let Some(post) = find(&app.db, &a, id).await? else {
            return Ok(Err(not_found()));
        };
        // The claim and its hit of the daily cap (shared with scheduled publishes) commit
        // together. `quota_reserved = $2` keeps a stale read from skipping the reserve.
        let claim = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE posts p SET status = 'queued', quota_reserved = true, \
             scheduled_at = now(), updated_at = now() \
             WHERE p.id = $1 AND p.status IN ('draft', 'scheduled') AND p.quota_reserved = $2 \
             AND ($3::uuid IS NULL OR p.social_set_id = $3) \
             AND EXISTS (SELECT 1 FROM social_sets s \
                         WHERE s.id = p.social_set_id AND s.deleted_at IS NULL)",
            [id.into(), post.quota_reserved.into(), a.set_id().into()],
        );
        match publish::reserve_and(&app.db, a.team.id, post.quota_reserved, claim).await? {
            publish::Quota::Reserved => {}
            publish::Quota::Over => {
                return Ok(Err(reply(StatusCode::TOO_MANY_REQUESTS, "publish_quota")))
            }
            publish::Quota::Lost => return Ok(Err(reply(StatusCode::CONFLICT, "not_editable"))),
        }
        match find(&app.db, &a, id).await? {
            Some(p) => one_json(&app.db, p).await.map(Ok),
            None => Ok(Err(not_found())),
        }
    };
    match claimed.await {
        Ok(Ok(p)) => {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = publish::publish_post(&app, id).await {
                    tracing::error!(error = %e, post = %id, "publish now failed");
                }
            });
            (StatusCode::ACCEPTED, Json(p)).into_response()
        }
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post publish"),
    }
}

/// Deletes a post. `draft` or `scheduled`: removed (`204`). `published`: taken down from every
/// account and kept as `deleted` (`200`); when an account cannot take it down, `422
/// {"error":"takedown_failed","message"}` says what to do, and a retry only tries what is still
/// up. `queued` and `failed`: `409 not_deletable`.
#[utoipa::path(delete, path = "/posts/{id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 204), (status = 200, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
async fn destroy(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    let found = match find(&app.db, &a, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "post read"),
    };
    match found.status.as_str() {
        "draft" | "scheduled" => {
            let gone = posts::Entity::delete_many()
                .filter(posts::Column::Id.eq(id))
                .filter(posts::Column::Status.is_in(["draft", "scheduled"]))
                .filter(in_scope(&a))
                .exec(&app.db)
                .await;
            match gone {
                Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
                Ok(_) => reply(StatusCode::CONFLICT, "not_deletable"),
                Err(e) => internal(e, "post delete"),
            }
        }
        "published" | "deleted" => {
            if found.status == "published" {
                if !app.post_limiter.hit(a.key()) {
                    return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
                }
                match publish::takedown(&app, &found).await {
                    Ok(problems) if problems.is_empty() => {}
                    // 422, never 502: a proxy may swap a 502 body for its own page.
                    Ok(problems) => {
                        let body = serde_json::json!({
                            "error": "takedown_failed",
                            "message": problems.join(" "),
                        });
                        return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
                    }
                    Err(e) => return internal(e, "post takedown"),
                }
            }
            match find(&app.db, &a, id).await {
                Ok(Some(p)) => match one_json(&app.db, p).await {
                    Ok(p) => Json(p).into_response(),
                    Err(e) => internal(e, "post read"),
                },
                Ok(None) => not_found(),
                Err(e) => internal(e, "post read"),
            }
        }
        _ => reply(StatusCode::CONFLICT, "not_deletable"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(publish_now))
}
