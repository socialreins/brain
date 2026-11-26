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
    posts::{self as posts_api, PostId, ThreadPart, ID_PARAM},
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
    /// Whether the page offers a newsletter sign-up (`POST /public/pages/{code}/subscribe`).
    pub newsletter: bool,
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

/// The page with `code`, its team and its published posts, when the public route serves it:
/// the page is enabled, the team has public pages on and a post of it is published.
pub(super) async fn live(
    db: &impl ConnectionTrait,
    code: String,
) -> Result<Option<(post_pages::Model, teams::Model, Vec<posts::Model>)>, DbErr> {
    let page = post_pages::Entity::find()
        .filter(post_pages::Column::Code.eq(code))
        .filter(post_pages::Column::Enabled.eq(true))
        .filter(post_pages::Column::TakenDownAt.is_null())
        .one(db)
        .await?;
    let Some(page) = page else { return Ok(None) };
    let team = teams::Entity::find_by_id(page.team_id)
        .filter(teams::Column::PublicPages.eq(true))
        .one(db)
        .await?;
    let Some(team) = team else { return Ok(None) };
    let rows = published(db, team.id, page.subject).await?;
    Ok((!rows.is_empty()).then_some((page, team, rows)))
}

async fn page_json(
    db: &impl ConnectionTrait,
    team: &teams::Model,
    subject: Uuid,
    row: Option<post_pages::Model>,
) -> Result<Page, DbErr> {
    let on = team.public_pages
        && row
            .as_ref()
            .is_some_and(|r| r.enabled && r.taken_down_at.is_none());
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

/// The address of the public page of a post (`{web_url}/p/{code}`), when the public route
/// serves it now: the page is enabled, the team has public pages on and a post of it is
/// published in a live set.
pub async fn address(
    db: &impl ConnectionTrait,
    web_url: &str,
    team_id: Uuid,
    post_id: Uuid,
) -> Result<Option<String>, DbErr> {
    let post = posts::Entity::find_by_id(post_id)
        .filter(posts::Column::TeamId.eq(team_id))
        .one(db)
        .await?;
    let Some(post) = post else { return Ok(None) };
    let subject = subject(&post);
    let page = post_pages::Entity::find()
        .filter(post_pages::Column::Subject.eq(subject))
        .filter(post_pages::Column::Enabled.eq(true))
        .filter(post_pages::Column::TakenDownAt.is_null())
        .one(db)
        .await?;
    let Some(page) = page else { return Ok(None) };
    let on = teams::Entity::find_by_id(team_id)
        .filter(teams::Column::PublicPages.eq(true))
        .one(db)
        .await?
        .is_some();
    if !on || published(db, team_id, subject).await?.is_empty() {
        return Ok(None);
    }
    let base = web_url.trim_end_matches('/');
    Ok(Some(format!("{base}/p/{}", page.code)))
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
    security(("session" = []), ("api_token" = [])), params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Page), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, PostId(id): PostId) -> Response {
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
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Page), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error),
        (status = 422, body = Error)))]
async fn save(
    State(app): State<AppState>,
    a: Actor,
    PostId(id): PostId,
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
        let Some((page, team, mut rows)) = live(db, code).await? else {
            return Ok(None);
        };
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
            newsletter: team.newsletter,
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

const REASONS: [&str; 4] = ["phishing", "spam", "illegal", "other"];
const MAX_REPORT_CHARS: usize = 1000;
/// Reports a day: from one IP address, about one page, and in all.
const REPORTS_PER_IP: i64 = 10;
const REPORTS_PER_PAGE: i64 = 20;
const REPORTS_PER_DAY: i64 = 200;

#[derive(Deserialize, ToSchema)]
pub struct NewReport {
    /// `phishing`, `spam`, `illegal` or `other`.
    pub reason: String,
    /// What is wrong, at most 1000 characters. Optional.
    pub message: Option<String>,
    /// Where an answer may go. Optional.
    pub email: Option<String>,
    /// The Turnstile widget's token; required when `GET /auth/providers` gives a
    /// `turnstile_site_key`, else ignored.
    pub turnstile_token: Option<String>,
}

/// Public: reports the page with `code` to the operators ("Report this page" on `/p/<code>`). The
/// report is stored and mailed to `CONTACT_TO`; an admin can then take the page down
/// (`PUT /admin/pages/{code}/takedown`). `404` unless the page is live. `422 invalid_reason`,
/// `invalid_message`, `invalid_email`; `429` past 5 an hour for an address (the contact form's
/// limit). Past the daily caps (10 a day for an address, 20 for a page, 200 in all) the report is
/// still stored and shown to admins, only the mail is not sent.
#[utoipa::path(post, path = "/public/pages/{code}/report", tag = "posts",
    params(("code" = String, Path)), request_body = NewReport,
    responses((status = 202, body = serde_json::Value), (status = 400, body = Error),
        (status = 404, body = Error), (status = 413, body = Error), (status = 415, body = Error),
        (status = 422, body = Error), (status = 429, body = Error), (status = 500, body = Error),
        (status = 503, body = Error)))]
async fn report(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(code): Path<String>,
    request: axum::extract::Request,
) -> Response {
    use axum::extract::FromRequest;
    if !app.contact_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let body = match Json::<NewReport>::from_request(request, &()).await {
        Ok(Json(body)) => body,
        Err(r) => return super::waitlist::rejected(&r).into_response(),
    };
    let invalid = |code| reply(StatusCode::UNPROCESSABLE_ENTITY, code);
    if !REASONS.contains(&body.reason.as_str()) {
        return invalid("invalid_reason");
    }
    let message = body.message.as_deref().map(str::trim).unwrap_or_default();
    if message.chars().count() > MAX_REPORT_CHARS
        || message
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return invalid("invalid_message");
    }
    let contact = match body
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        Some(e) => match socialreins_core::email::normalize(e) {
            Some(e) => Some(e),
            None => return invalid("invalid_email"),
        },
        None => None,
    };
    if let Err(r) =
        crate::turnstile::check(app.turnstile.as_ref(), body.turnstile_token.as_deref(), ip).await
    {
        return r.into_response();
    }
    let (page, team) = match live(&app.db, code.clone()).await {
        Ok(Some((page, team, _))) => (page, team),
        Ok(None) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "report page read"),
    };
    let sql = "INSERT INTO page_reports (id, page_id, reason, message, contact) \
               VALUES ($1, $2, $3, $4, $5)";
    let values = [
        Uuid::now_v7().into(),
        page.id.into(),
        body.reason.clone().into(),
        message.to_owned().into(),
        contact.clone().into(),
    ];
    let stored = app
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await;
    if let Err(e) = stored {
        return internal(e, "report insert");
    }
    // The report is stored whatever happens next, so a flood cannot silence real reports; the caps
    // only decide whether it is also mailed (the admin list always has it).
    let mut caps = vec![
        (format!("page-report:page:{}", page.id), REPORTS_PER_PAGE),
        ("page-report:all".to_owned(), REPORTS_PER_DAY),
    ];
    if let Some(ip) = ip {
        let key = format!("page-report:ip:{}", crate::rate_limit::bucket(ip));
        caps.push((key, REPORTS_PER_IP));
    }
    let mailed = match crate::rate_limit::daily_reserve(&app.db, &caps).await {
        Ok(slot) => slot.is_some(),
        Err(e) => {
            tracing::error!(error = %e, "page report cap not read");
            false
        }
    };
    if let (true, Some(to)) = (mailed, app.contact_to.clone()) {
        let text = format!(
            "Page: {}/p/{code}\nTeam: {}\nReason: {}\nFrom: {}\n\n{message}\n\nTake it down in the admin console: {}/admin/reports\n",
            app.web_url,
            team::inert(&team.name),
            body.reason,
            contact.as_deref().unwrap_or("not given"),
            app.web_url,
        );
        let subject = format!("Page report ({}): {code}", body.reason);
        let sent = app
            .mailer
            .send_reply_to(&to, contact.as_deref(), &subject, text);
        if let Err(e) = sent.await {
            // The report is stored, so the admin list has it.
            tracing::error!(error = %e, "page report mail not sent");
        }
    }
    (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response()
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(show, save))
        .routes(routes!(public))
        .routes(routes!(report))
}
