//! RSS and Atom feeds of a team (`/v1/feeds`): each posts its new items to the platforms of one
//! social set (`crate::feeds`). A feed posts on its own, so only owners, admins and full tokens
//! write them; every member and a read-only token read. A set token sees and writes its own
//! set's feeds only. Creates take a per-team advisory lock, so the cap holds.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use schemars::JsonSchema;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body},
    idempotency::IDEMPOTENCY,
    posts::accounts_in,
    waitlist::{error, Error},
};
use crate::{
    feeds::{self as reader, Item},
    models::{feeds, social_sets},
    publish,
    state::AppState,
};

/// Most feeds a team keeps.
pub const MAX_FEEDS: u64 = 20;
const MAX_URL: usize = 2048;
const MAX_TEMPLATE: usize = 2000;
const DEFAULT_TEMPLATE: &str = "{title}\n{link}";

#[derive(Serialize, ToSchema)]
pub struct Feed {
    pub id: Uuid,
    pub url: String,
    pub social_set_id: Uuid,
    /// The platforms each new item is posted to.
    pub platforms: Vec<String>,
    pub template: String,
    /// `draft`, `queue` or `now`.
    pub mode: String,
    /// `false`: not checked. Set by `PUT`, and by brain after 10 failed checks in a row.
    pub active: bool,
    pub last_checked_at: Option<DateTime<FixedOffset>>,
    /// Why the last check failed, or what it could not post; `null` when all went well.
    pub last_error: Option<String>,
    /// Items the feed has made posts of.
    pub items_posted: i32,
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct FeedInput {
    /// Required: a public `https://` URL of an RSS 2.0 or Atom feed, at most 2048 characters,
    /// with no user or password in it.
    #[serde(default)]
    pub url: String,
    /// Required: a live set of the team; a set token's own.
    pub social_set_id: Option<Uuid>,
    /// Required, not empty: platforms the set has an account for. `tiktok`, `youtube` and `pinterest` are
    /// `422 platform_not_supported` (an item has no video or image).
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Each post's text, 1 to 2000 characters, default `{title}\n{link}`. `{title}`, `{link}`
    /// and `{summary}` (the item's description as plain text, at most 300 characters) are
    /// filled in per item; over a platform's limit the summary is cut first, then the title,
    /// never the link.
    pub template: Option<String>,
    /// Required: `draft` (posts wait as drafts), `queue` (the set's next free queue slot, a
    /// draft when the queue has none) or `now` (published by the next scheduler round).
    #[serde(default)]
    pub mode: String,
    /// `PUT` only, default unchanged: `false` stops the checks, `true` starts them again.
    pub active: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FeedId {
    pub id: Uuid,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn forbidden() -> Response {
    reply(StatusCode::FORBIDDEN, "forbidden")
}

/// `422 {"error": "invalid_feed", "field": field}`.
fn invalid(field: &str) -> Response {
    let body = json!({"error": "invalid_feed", "field": field});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

fn unreadable(reason: &str) -> Response {
    let body = json!({"error": "feed_unreadable", "reason": reason});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

fn feed_json(f: feeds::Model) -> Feed {
    Feed {
        id: f.id,
        url: f.url,
        social_set_id: f.social_set_id,
        platforms: serde_json::from_value(f.platforms).unwrap_or_default(),
        template: f.template,
        mode: f.mode,
        active: f.active,
        last_checked_at: f.last_checked_at,
        last_error: f.last_error,
        items_posted: f.items_posted,
        created_at: f.created_at,
    }
}

/// The caller's feeds: the team's, a set token's own set's.
fn mine(a: &Actor) -> sea_orm::Select<feeds::Entity> {
    let all = feeds::Entity::find().filter(feeds::Column::TeamId.eq(a.team.id));
    match a.set_id() {
        Some(set) => all.filter(feeds::Column::SocialSetId.eq(set)),
        None => all,
    }
}

async fn find(
    db: &impl ConnectionTrait,
    a: &Actor,
    id: Uuid,
) -> Result<Option<feeds::Model>, DbErr> {
    mine(a).filter(feeds::Column::Id.eq(id)).one(db).await
}

/// A request body that passed every check.
struct Valid {
    url: String,
    set: Uuid,
    platforms: Vec<String>,
    template: String,
    mode: String,
}

async fn checked(
    app: &AppState,
    a: &Actor,
    input: &FeedInput,
) -> Result<Result<Valid, Response>, DbErr> {
    if input.url.len() > MAX_URL || !app.feeds.allows(&input.url) {
        return Ok(Err(invalid("url")));
    }
    let Some(set) = input.social_set_id.or(a.set_id()) else {
        return Ok(Err(invalid("social_set_id")));
    };
    let live = social_sets::Entity::find_by_id(set)
        .filter(social_sets::Column::TeamId.eq(a.team.id))
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(&app.db)
        .await?;
    if live.is_none() || a.set_id().is_some_and(|own| own != set) {
        return Ok(Err(invalid("social_set_id")));
    }
    let mut platforms = input.platforms.clone();
    let mut seen = std::collections::HashSet::new();
    platforms.retain(|p| seen.insert(p.clone()));
    if let Some(p) = platforms
        .iter()
        .find(|p| matches!(p.as_str(), "tiktok" | "youtube" | "pinterest"))
    {
        let body = json!({"error": "platform_not_supported", "platform": p});
        return Ok(Err(
            (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
        ));
    }
    let accounts = accounts_in(&app.db, a.team.id, set).await?;
    let connected = |p: &String| {
        publish::publisher(app, p).is_some() && accounts.iter().any(|(provider, _)| provider == p)
    };
    if platforms.is_empty() || !platforms.iter().all(connected) {
        return Ok(Err(invalid("platforms")));
    }
    let template = input
        .template
        .clone()
        .unwrap_or_else(|| DEFAULT_TEMPLATE.into());
    if template.trim().is_empty() || template.chars().count() > MAX_TEMPLATE {
        return Ok(Err(invalid("template")));
    }
    if !matches!(input.mode.as_str(), "draft" | "queue" | "now") {
        return Ok(Err(invalid("mode")));
    }
    Ok(Ok(Valid {
        url: input.url.clone(),
        set,
        platforms,
        template,
        mode: input.mode.clone(),
    }))
}

/// The team's feeds, oldest first.
#[utoipa::path(get, path = "/feeds", tag = "feeds",
    security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = [Feed]), (status = 401, body = Error)))]
async fn index(State(app): State<AppState>, a: Actor) -> Response {
    list_feeds(&app, &a).await
}

/// The body of `GET /feeds`, shared with the MCP `list-feeds` tool.
pub async fn list_feeds(app: &AppState, a: &Actor) -> Response {
    let rows = mine(a)
        .order_by_asc(feeds::Column::CreatedAt)
        .order_by_asc(feeds::Column::Id)
        .all(&app.db)
        .await;
    match rows {
        Ok(rows) => Json(rows.into_iter().map(feed_json).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e, "feed list"),
    }
}

/// Adds a feed (owner, admin or a full token). The feed is read once: one that cannot be read or
/// parsed is `422 {"error":"feed_unreadable","reason"}`, and the items it holds now are marked
/// seen, so only later ones are posted. `422 {"error":"invalid_feed","field"}` names a bad
/// field, `422 {"error":"platform_not_supported","platform"}` TikTok, YouTube or Pinterest, and a 21st feed
/// is `422 too_many_feeds`.
#[utoipa::path(post, path = "/feeds", tag = "feeds",
    security(("session" = []), ("api_token" = [])), request_body = FeedInput,
    params(("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 201, body = Feed), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn create(State(app): State<AppState>, a: Actor, Body(input): Body<FeedInput>) -> Response {
    add_feed(&app, &a, input).await
}

async fn count(db: &impl ConnectionTrait, team: Uuid) -> Result<u64, DbErr> {
    feeds::Entity::find()
        .filter(feeds::Column::TeamId.eq(team))
        .count(db)
        .await
}

fn too_many() -> Response {
    reply(StatusCode::UNPROCESSABLE_ENTITY, "too_many_feeds")
}

/// The body of `POST /feeds`, shared with the MCP `add-feed` tool.
pub async fn add_feed(app: &AppState, a: &Actor, input: FeedInput) -> Response {
    if !a.role.manages() {
        return forbidden();
    }
    let team = a.team.id;
    let done = async {
        let v = match checked(app, a, &input).await? {
            Ok(v) => v,
            Err(res) => return Ok(res),
        };
        // Before the feed is asked for, so a full team calls nothing.
        if count(&app.db, team).await? >= MAX_FEEDS {
            return Ok(too_many());
        }
        let items: Vec<Item> = match app.feeds.items(&v.url).await {
            Ok(items) => items,
            Err(reason) => return Ok(unreadable(&reason)),
        };
        let txn = app.db.begin().await?;
        let lock = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";
        let key = format!("feeds:{team}");
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            lock,
            [key.into()],
        ))
        .await?;
        if count(&txn, team).await? >= MAX_FEEDS {
            return Ok(too_many());
        }
        let row = feeds::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(team),
            social_set_id: Set(v.set),
            url: Set(v.url),
            platforms: Set(json!(v.platforms)),
            template: Set(v.template),
            mode: Set(v.mode),
            last_checked_at: Set(Some(Utc::now().fixed_offset())),
            created_by: Set(a.user_id),
            ..Default::default()
        };
        let feed = feeds::Entity::insert(row).exec_with_returning(&txn).await?;
        reader::seed(&txn, feed.id, &items).await?;
        txn.commit().await?;
        Ok::<_, DbErr>((StatusCode::CREATED, Json(feed_json(feed))).into_response())
    };
    done.await.unwrap_or_else(|e| internal(e, "feed insert"))
}

/// One feed.
#[utoipa::path(get, path = "/feeds/{id}", tag = "feeds", params(("id" = Uuid, Path)),
    security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = Feed), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    match find(&app.db, &a, id).await {
        Ok(Some(f)) => Json(feed_json(f)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "feed read"),
    }
}

/// Replaces a feed (owner, admin or a full token): the fields of `POST /feeds`, checked the same
/// way, and `active`. A new `url` is read once like a new feed's, and what it holds now is
/// never posted. Switching `active` back on starts the count of failed checks again.
#[utoipa::path(put, path = "/feeds/{id}", tag = "feeds", params(("id" = Uuid, Path)),
    security(("session" = []), ("api_token" = [])), request_body = FeedInput,
    responses((status = 200, body = Feed), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn update(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<FeedInput>,
) -> Response {
    if !a.role.manages() {
        return forbidden();
    }
    let done = async {
        let Some(old) = find(&app.db, &a, id).await? else {
            return Ok(not_found());
        };
        let v = match checked(&app, &a, &input).await? {
            Ok(v) => v,
            Err(res) => return Ok(res),
        };
        let active = input.active.unwrap_or(old.active);
        let mut row = feeds::ActiveModel {
            id: Set(id),
            social_set_id: Set(v.set),
            platforms: Set(json!(v.platforms)),
            template: Set(v.template),
            mode: Set(v.mode),
            active: Set(active),
            ..Default::default()
        };
        if active && !old.active {
            row.failures = Set(0);
        }
        // Read before the transaction opens, so no connection waits on the feed.
        let moved = if v.url != old.url {
            match app.feeds.items(&v.url).await {
                Ok(items) => Some(items),
                Err(reason) => return Ok(unreadable(&reason)),
            }
        } else {
            None
        };
        let txn = app.db.begin().await?;
        if let Some(items) = moved {
            reader::seed(&txn, id, &items).await?;
            row.url = Set(v.url);
            row.etag = Set(None);
            row.last_modified = Set(None);
            row.last_error = Set(None);
            row.failures = Set(0);
            row.last_checked_at = Set(Some(Utc::now().fixed_offset()));
        }
        let feed = feeds::Entity::update(row).exec(&txn).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Json(feed_json(feed)).into_response())
    };
    done.await.unwrap_or_else(|e| internal(e, "feed update"))
}

/// Removes a feed (owner, admin or a full token). The posts it made stay.
#[utoipa::path(delete, path = "/feeds/{id}", tag = "feeds", params(("id" = Uuid, Path)),
    security(("session" = []), ("api_token" = [])),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    remove_feed(&app, &a, id).await
}

/// The body of `DELETE /feeds/{id}`, shared with the MCP `remove-feed` tool.
pub async fn remove_feed(app: &AppState, a: &Actor, id: Uuid) -> Response {
    if !a.role.manages() {
        return forbidden();
    }
    let done = async {
        let Some(feed) = find(&app.db, a, id).await? else {
            return Ok(not_found());
        };
        feeds::Entity::delete_by_id(feed.id).exec(&app.db).await?;
        Ok::<_, DbErr>(StatusCode::NO_CONTENT.into_response())
    };
    done.await.unwrap_or_else(|e| internal(e, "feed delete"))
}

/// Checks a feed now (owner, admin or a full token), as the scheduler does every 15 minutes,
/// and answers the feed as the check left it. At most once a minute per feed, the scheduler's
/// checks and the one a create makes included: sooner is `429 checked_recently`.
#[utoipa::path(post, path = "/feeds/{id}/check", tag = "feeds", params(("id" = Uuid, Path)),
    security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = Feed), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 429, body = Error)))]
async fn check(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    if !a.role.manages() {
        return forbidden();
    }
    let done = async {
        if find(&app.db, &a, id).await?.is_none() {
            return Ok(not_found());
        }
        let Some(feed) = reader::claim(&app.db, id, reader::CHECK_NOW_SECS, false).await? else {
            return Ok(reply(StatusCode::TOO_MANY_REQUESTS, "checked_recently"));
        };
        reader::check(&app, feed).await?;
        Ok::<_, DbErr>(match find(&app.db, &a, id).await? {
            Some(f) => Json(feed_json(f)).into_response(),
            None => not_found(),
        })
    };
    done.await.unwrap_or_else(|e| internal(e, "feed check"))
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(index, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(check))
}
