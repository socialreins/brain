//! Public pages of posts. A team switches them on (`teams.public_pages`, `PATCH /team`); then
//! `PUT /posts/{id}/page` gives a post a page: its text and media plus free `details`. The posts
//! of a group share one page (`post_pages.subject`). `GET /public/pages/{code}` serves it to
//! anyone once a post of it is `published`, and never shows ids, errors, first comments or
//! unpublished versions.
use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter,
    QueryOrder, Statement,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body},
    connections::live_sets,
    posts::{self as posts_api, ThreadPart},
    waitlist::{error, Error},
};
use crate::{
    models::{post_pages, post_targets, posts, teams},
    rate_limit::ClientIp,
    state::AppState,
};

const MAX_DETAILS: usize = 5000;
const CODE_CHARS: usize = 10;

#[derive(Serialize, ToSchema)]
pub struct Page {
    pub enabled: bool,
    pub details: String,
    /// The page's public address (`GET /public/pages/{code}`): 10 characters, made on the first
    /// save and never changed. `null` before it.
    pub code: Option<String>,
    /// Whether the public route serves the page now: it is enabled, the team has public pages
    /// on and a post of it is published.
    pub live: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct PageInput {
    pub enabled: bool,
    /// Free text shown on the page (resources, links). At most 5000 characters.
    pub details: String,
}

#[derive(Serialize, ToSchema)]
pub struct PageLink {
    pub platform: String,
    pub url: String,
}

#[derive(Serialize, ToSchema)]
pub struct PublicPage {
    /// The team's name.
    pub team: String,
    pub body: String,
    pub media: Vec<String>,
    pub thread: Vec<ThreadPart>,
    pub details: String,
    pub published_at: DateTime<FixedOffset>,
    /// Where the post is live: one per published target of the page's published posts.
    pub links: Vec<PageLink>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

/// What a post's page hangs on: its group, else the post itself.
fn subject(p: &posts::Model) -> Uuid {
    p.group_id.unwrap_or(p.id)
}

/// The published posts of `subject` in live sets, in the order of the posts list.
async fn published(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    subject: Uuid,
) -> Result<Vec<posts::Model>, DbErr> {
    posts::Entity::find()
        .filter(posts::Column::TeamId.eq(team_id))
        .filter(posts::Column::Status.eq("published"))
        .filter(
            Condition::any()
                .add(posts::Column::GroupId.eq(subject))
                .add(posts::Column::Id.eq(subject)),
        )
        .filter(posts::Column::SocialSetId.in_subquery(live_sets()))
        .order_by_asc(posts::Column::ScheduledAt)
        .order_by_asc(posts::Column::CreatedAt)
        .order_by_asc(posts::Column::Id)
        .all(db)
        .await
}

async fn page_json(
    db: &impl ConnectionTrait,
    team: &teams::Model,
    subject: Uuid,
    row: Option<post_pages::Model>,
) -> Result<Page, DbErr> {
    let on = team.public_pages && row.as_ref().is_some_and(|r| r.enabled);
    let live = on && !published(db, team.id, subject).await?.is_empty();
    let (enabled, details, code) = match row {
        Some(r) => (r.enabled, r.details, Some(r.code)),
        None => (false, String::new(), None),
    };
    Ok(Page {
        enabled,
        details,
        code,
        live,
    })
}

fn answer(found: Result<Option<Page>, DbErr>, what: &str) -> Response {
    match found {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, what),
    }
}

/// The public page of a post, or of its group: every post of a group answers the same. Before
/// the first save: `enabled` false, `details` empty and `code` null.
#[utoipa::path(get, path = "/posts/{id}/page", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = Page), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    let found = async {
        let Some(post) = posts_api::find(&app.db, &a, id).await? else {
            return Ok(None);
        };
        let subject = subject(&post);
        let row = post_pages::Entity::find()
            .filter(post_pages::Column::Subject.eq(subject))
            .one(&app.db)
            .await?;
        page_json(&app.db, &a.team, subject, row).await.map(Some)
    };
    answer(found.await, "post page read")
}

/// Saves the page of a post (of its group, when it has one), whatever the post's status. The
/// first save makes its `code`. `422 details_too_long` over 5000 characters; `422
/// public_pages_off` for `enabled: true` while the team has public pages off.
#[utoipa::path(put, path = "/posts/{id}/page", tag = "posts",
    security(("session" = []), ("api_token" = [])), request_body = PageInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Page), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error),
        (status = 422, body = Error)))]
async fn save(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<PageInput>,
) -> Response {
    let post = match posts_api::find(&app.db, &a, id).await {
        Ok(Some(post)) => post,
        found => return answer(found.map(|_| None), "post page save"),
    };
    if input.details.chars().count() > MAX_DETAILS {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "details_too_long");
    }
    if input.enabled && !a.team.public_pages {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "public_pages_off");
    }
    let subject = subject(&post);
    // shortcut: a new code that is taken (one in 31^10 per page) fails the save with a 500 and
    // the next try draws another; loop like `teams::insert_team` if that ever shows up.
    let sql = "INSERT INTO post_pages (id, team_id, subject, code, details, enabled) \
               VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (subject) DO UPDATE \
               SET details = EXCLUDED.details, enabled = EXCLUDED.enabled, updated_at = now() \
               RETURNING *";
    let values = [
        Uuid::now_v7().into(),
        a.team.id.into(),
        subject.into(),
        team::random_code(CODE_CHARS).into(),
        input.details.into(),
        input.enabled.into(),
    ];
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, values);
    let saved = async {
        let row = post_pages::Entity::find()
            .from_raw_sql(stmt)
            .one(&app.db)
            .await?;
        page_json(&app.db, &a.team, subject, row).await.map(Some)
    };
    answer(saved.await, "post page save")
}

/// Public: the page with `code`, built from the first published post of it. `404` unless the
/// page is enabled, its team has public pages on and a post of it is published. Limited per IP.
#[utoipa::path(get, path = "/public/pages/{code}", tag = "posts",
    params(("code" = String, Path)),
    responses((status = 200, body = PublicPage,
            headers(("Cache-Control" = String, description = "`public, max-age=60`"))),
        (status = 404, body = Error), (status = 429, body = Error)))]
async fn public(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(code): Path<String>,
) -> Response {
    if !app.page_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let db = &app.db;
    let found = async {
        let page = post_pages::Entity::find()
            .filter(post_pages::Column::Code.eq(code))
            .filter(post_pages::Column::Enabled.eq(true))
            .one(db)
            .await?;
        let Some(page) = page else { return Ok(None) };
        let team = teams::Entity::find_by_id(page.team_id)
            .filter(teams::Column::PublicPages.eq(true))
            .one(db)
            .await?;
        let Some(team) = team else { return Ok(None) };
        let mut rows = published(db, team.id, page.subject).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let targets = post_targets::Entity::find()
            .filter(post_targets::Column::PostId.is_in(rows.iter().map(|p| p.id)))
            .filter(post_targets::Column::Status.eq("published"))
            .order_by_asc(post_targets::Column::CreatedAt)
            .order_by_asc(post_targets::Column::Id)
            .all(db)
            .await?;
        let platforms: HashMap<Uuid, &str> =
            rows.iter().map(|p| (p.id, p.platform.as_str())).collect();
        let links = targets
            .into_iter()
            .filter_map(|t| {
                Some(PageLink {
                    platform: (*platforms.get(&t.post_id)?).to_owned(),
                    url: t.remote_url?,
                })
            })
            .collect();
        let first = rows.swap_remove(0);
        Ok::<_, DbErr>(Some(PublicPage {
            team: team.name,
            body: first.body,
            media: serde_json::from_value(first.media).unwrap_or_default(),
            thread: serde_json::from_value(first.thread).unwrap_or_default(),
            details: page.details,
            published_at: first.published_at.unwrap_or(first.updated_at),
            links,
        }))
    };
    match found.await {
        Ok(Some(p)) => ([(header::CACHE_CONTROL, "public, max-age=60")], Json(p)).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "public page read"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(show, save))
        .routes(routes!(public))
}
