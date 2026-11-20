//! Broadcasts of the current team: one DM to every reachable contact with a tag
//! (`crate::broadcasts`). Members list and read them; owners and admins write, preview, send,
//! cancel and read recipients. Like contacts, these routes take a session only.
use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, QueryResult, Statement, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    automations::MAX_NAME_CHARS,
    broadcasts::{self, Preview, Resume, Start},
    inbox,
    state::AppState,
};

const MAX_NAME: usize = 80;
const MAX_TEXT: usize = 1000;
/// Broadcasts a list holds, and recipients a page.
// shortcut: no paging of the list; add a cursor once a team keeps more than 200.
const LIST: i64 = 200;
const PAGE: i64 = 100;
const RECIPIENT_STATUSES: [&str; 6] = [
    "pending",
    "sending",
    "delivering",
    "sent",
    "skipped",
    "failed",
];

#[derive(Serialize, ToSchema)]
pub struct Counts {
    /// The recipients snapshotted at the start; 0 for a draft.
    pub total: i64,
    pub sent: i64,
    pub skipped: i64,
    pub failed: i64,
    /// Not sent to yet, the ones going out right now included.
    pub pending: i64,
}

#[derive(Serialize, ToSchema)]
pub struct Broadcast {
    pub id: Uuid,
    pub name: String,
    /// `{name}` stands for the contact's display name ("there" without a usable one).
    pub text: String,
    pub tag: String,
    /// `bluesky`, `facebook` or `instagram`; `null` for all three.
    pub platform: Option<String>,
    /// `draft`, `sending`, `paused`, `sent` or `cancelled`.
    pub status: String,
    /// Only while `paused`: what the platform said, or that 5 sends in a row failed.
    pub pause_reason: Option<String>,
    pub counts: Counts,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub started_at: Option<DateTime<FixedOffset>>,
    pub finished_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BroadcastInput {
    /// 1 to 80 characters after trimming.
    pub name: String,
    /// 1 to 1000 characters after trimming, counting each `{name}` as 64.
    pub text: String,
    /// A contact tag, trimmed and lowercased.
    pub tag: String,
    /// `bluesky`, `facebook` or `instagram`; absent or `null` for all three.
    #[serde(default)]
    pub platform: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendInput {
    /// The `reachable` of the preview the sender saw.
    pub confirm_total: u64,
}

#[derive(Serialize, ToSchema)]
pub struct Recipient {
    pub id: Uuid,
    /// `null`, like the three fields after it, once the contact was deleted.
    pub contact_id: Option<Uuid>,
    pub display_name: Option<String>,
    pub handle: Option<String>,
    pub platform: Option<String>,
    /// `pending`, `sending` (claimed by a round), `delivering` (the send was started), `sent`,
    /// `skipped` or `failed`.
    pub status: String,
    /// Why it was not sent. Skipped: `opted_out`, `recently_messaged`, `outside_window`,
    /// `no_conversation`, `account_disconnected`, `untagged`, `dm_not_allowed`, `too_long`,
    /// `contact_gone`, `cancelled`.
    /// Failed: `interrupted` (cut off mid-send, never sent again), `internal`, or the
    /// platform's refusal, e.g. `rate_limited`, `meta_failed`, `bluesky_refused`.
    pub reason: Option<String>,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct RecipientPage {
    pub items: Vec<Recipient>,
    /// Pass as `cursor` for the next page, as it is; `null` on the last.
    pub next_cursor: Option<String>,
}

#[derive(Deserialize, IntoParams)]
pub struct RecipientQuery {
    /// `pending`, `sending`, `delivering`, `sent`, `skipped` or `failed`.
    pub status: Option<String>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<Uuid>,
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

fn stmt(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The input, normalized, or the field `422 invalid_broadcast` names.
fn checked(
    input: BroadcastInput,
) -> Result<(String, String, String, Option<String>), &'static str> {
    let name = input.name.trim();
    if !(1..=MAX_NAME).contains(&name.chars().count()) || name.chars().any(char::is_control) {
        return Err("name");
    }
    let text = input.text.trim();
    // A long name must not push a text past what a platform takes.
    let longest = text.replace("{name}", &"n".repeat(MAX_NAME_CHARS));
    if text.is_empty() || longest.chars().count() > MAX_TEXT {
        return Err("text");
    }
    let tag = inbox::tag(&input.tag).ok_or("tag")?;
    let platform_ok = |p: &String| broadcasts::PLATFORMS.contains(&p.as_str());
    if !input.platform.as_ref().is_none_or(platform_ok) {
        return Err("platform");
    }
    Ok((name.to_owned(), text.to_owned(), tag, input.platform))
}

fn invalid(field: &str) -> Response {
    let body = json!({"error": "invalid_broadcast", "field": field});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

const SELECT: &str = "SELECT b.*, n.* FROM broadcasts b, LATERAL (SELECT count(*) AS total, \
    count(*) FILTER (WHERE r.status = 'sent') AS sent, \
    count(*) FILTER (WHERE r.status = 'skipped') AS skipped, \
    count(*) FILTER (WHERE r.status = 'failed') AS failed, \
    count(*) FILTER (WHERE r.status IN ('pending', 'sending', 'delivering')) AS pending \
    FROM broadcast_recipients r WHERE r.broadcast_id = b.id) n WHERE b.team_id = $1";

fn broadcast_json(r: &QueryResult) -> Result<Broadcast, DbErr> {
    Ok(Broadcast {
        id: r.try_get("", "id")?,
        name: r.try_get("", "name")?,
        text: r.try_get("", "text")?,
        tag: r.try_get("", "tag")?,
        platform: r.try_get("", "platform")?,
        status: r.try_get("", "status")?,
        pause_reason: r.try_get("", "pause_reason")?,
        counts: Counts {
            total: r.try_get("", "total")?,
            sent: r.try_get("", "sent")?,
            skipped: r.try_get("", "skipped")?,
            failed: r.try_get("", "failed")?,
            pending: r.try_get("", "pending")?,
        },
        created_by: r.try_get("", "created_by")?,
        created_at: r.try_get("", "created_at")?,
        started_at: r.try_get("", "started_at")?,
        finished_at: r.try_get("", "finished_at")?,
    })
}

async fn find(db: &impl ConnectionTrait, team: Uuid, id: Uuid) -> Result<Option<Broadcast>, DbErr> {
    let sql = format!("{SELECT} AND b.id = $2");
    let row = db
        .query_one_raw(stmt(&sql, vec![team.into(), id.into()]))
        .await?;
    row.as_ref().map(broadcast_json).transpose()
}

/// `find` as a response: the broadcast with `status`, or `404`.
async fn shown(app: &AppState, team: Uuid, id: Uuid, status: StatusCode) -> Response {
    match find(&app.db, team, id).await {
        Ok(Some(b)) => (status, Json(b)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "broadcast read"),
    }
}

/// The team's broadcasts, newest first (the latest 200). Every member.
#[utoipa::path(get, path = "/broadcasts", tag = "broadcasts",
    responses((status = 200, body = [Broadcast]), (status = 401, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    let sql = format!("{SELECT} ORDER BY b.created_at DESC, b.id DESC LIMIT $2");
    let found = async {
        let rows = app
            .db
            .query_all_raw(stmt(&sql, vec![t.team.id.into(), LIST.into()]))
            .await?;
        rows.iter()
            .map(broadcast_json)
            .collect::<Result<Vec<_>, _>>()
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "broadcast list"),
    }
}

/// Creates a draft. Owners and admins. `422 invalid_broadcast` names the bad `field` (`name`,
/// `text`, `tag` or `platform`). Nothing is sent until `POST /broadcasts/{id}/send`.
#[utoipa::path(post, path = "/broadcasts", tag = "broadcasts", request_body = BroadcastInput,
    responses((status = 201, body = Broadcast), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<BroadcastInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let (name, text, tag, platform) = match checked(input) {
        Ok(c) => c,
        Err(field) => return invalid(field),
    };
    let id = Uuid::now_v7();
    let sql = "INSERT INTO broadcasts (id, team_id, name, text, tag, platform, created_by) \
               VALUES ($1, $2, $3, $4, $5, $6, $7)";
    let values: Vec<Value> = vec![
        id.into(),
        t.team.id.into(),
        name.into(),
        text.into(),
        tag.into(),
        platform.into(),
        t.current.0.id.into(),
    ];
    if let Err(e) = app.db.execute_raw(stmt(sql, values)).await {
        return internal(e, "broadcast create");
    }
    shown(&app, t.team.id, id, StatusCode::CREATED).await
}

/// One broadcast with its counts. Every member.
#[utoipa::path(get, path = "/broadcasts/{id}", tag = "broadcasts", params(("id" = Uuid, Path)),
    responses((status = 200, body = Broadcast), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, Path(id): Path<Uuid>) -> Response {
    shown(&app, t.team.id, id, StatusCode::OK).await
}

/// `404` when the team has no broadcast `id`, else `409` with `code`: what a write answers
/// when its one statement matched nothing.
async fn refused(app: &AppState, team: Uuid, id: Uuid, code: &str) -> Response {
    match find(&app.db, team, id).await {
        Ok(Some(_)) => reply(StatusCode::CONFLICT, code),
        Ok(None) => not_found(),
        Err(e) => internal(e, "broadcast read"),
    }
}

/// Replaces a draft. Owners and admins. `409 already_started` for anything but a draft.
#[utoipa::path(put, path = "/broadcasts/{id}", tag = "broadcasts", request_body = BroadcastInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Broadcast), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<BroadcastInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let (name, text, tag, platform) = match checked(input) {
        Ok(c) => c,
        Err(field) => return invalid(field),
    };
    let sql = "UPDATE broadcasts SET name = $3, text = $4, tag = $5, platform = $6 \
               WHERE id = $1 AND team_id = $2 AND status = 'draft'";
    let values: Vec<Value> = vec![
        id.into(),
        t.team.id.into(),
        name.into(),
        text.into(),
        tag.into(),
        platform.into(),
    ];
    match app.db.execute_raw(stmt(sql, values)).await {
        Ok(r) if r.rows_affected() == 1 => shown(&app, t.team.id, id, StatusCode::OK).await,
        Ok(_) => refused(&app, t.team.id, id, "already_started").await,
        Err(e) => internal(e, "broadcast update"),
    }
}

/// Deletes a draft or a finished broadcast with its recipients. Owners and admins.
/// `409 still_sending` while it sends or is paused: cancel it first.
#[utoipa::path(delete, path = "/broadcasts/{id}", tag = "broadcasts", params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error), (status = 409, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let sql = "DELETE FROM broadcasts WHERE id = $1 AND team_id = $2 \
               AND status NOT IN ('sending', 'paused')";
    let key: Vec<Value> = vec![id.into(), t.team.id.into()];
    match app.db.execute_raw(stmt(sql, key)).await {
        Ok(r) if r.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => refused(&app, t.team.id, id, "still_sending").await,
        Err(e) => internal(e, "broadcast delete"),
    }
}

async fn preview_of(app: &AppState, team: Uuid, id: Uuid) -> Result<Option<Preview>, DbErr> {
    let Some(b) = find(&app.db, team, id).await? else {
        return Ok(None);
    };
    let p = broadcasts::preview(&app.db, team, &b.tag, b.platform.as_deref()).await?;
    Ok(Some(p))
}

/// Who the broadcast would go to if it started now; nothing is sent. Owners and admins.
/// `total` is `reachable` plus the `by_reason` counts. A contact is reachable when it has the
/// tag, did not opt out, and the account it last wrote to is connected; on Facebook and
/// Instagram only within 24 hours of its last message to that account. Nobody is reachable
/// within 24 hours of a broadcast that went out to them (`recently_messaged`).
#[utoipa::path(post, path = "/broadcasts/{id}/preview", tag = "broadcasts",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Preview), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn preview(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match preview_of(&app, t.team.id, id).await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "broadcast preview"),
    }
}

/// Starts a draft: everyone reachable right now becomes a recipient and the broadcast is
/// `sending`; each recipient is then sent to at most once, 20 a minute. Owners and admins.
/// `409 audience_changed` with the new `preview` when `confirm_total` is not the reachable
/// count of this moment, `409 already_started` for anything but a draft, `409 another_sending`
/// while another broadcast of the team sends, `422 too_many_recipients` over 1000,
/// `422 no_recipients` with nobody reachable, `429 too_many_broadcasts` after 3 starts in 24
/// hours. A send the platform refuses for more than the one person (a rate limit, a block, an
/// outage, a dead token), or 5 failed sends in a row, makes it `paused` with a `pause_reason`:
/// it then waits for `POST /broadcasts/{id}/resume`.
#[utoipa::path(post, path = "/broadcasts/{id}/send", tag = "broadcasts", request_body = SendInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Broadcast), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
async fn send(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<SendInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let team = t.team.id;
    let started = match broadcasts::start(&app.db, team, id, input.confirm_total).await {
        Ok(s) => s,
        Err(e) => return internal(e, "broadcast start"),
    };
    let (status, code) = match started {
        Start::Started => return shown(&app, team, id, StatusCode::OK).await,
        Start::NotFound => return not_found(),
        Start::AudienceChanged => {
            return match preview_of(&app, team, id).await {
                Ok(Some(p)) => {
                    let body = json!({"error": "audience_changed", "preview": p});
                    (StatusCode::CONFLICT, Json(body)).into_response()
                }
                Ok(None) => not_found(),
                Err(e) => internal(e, "broadcast preview"),
            }
        }
        Start::AlreadyStarted => (StatusCode::CONFLICT, "already_started"),
        Start::AnotherSending => (StatusCode::CONFLICT, "another_sending"),
        Start::NoRecipients => (StatusCode::UNPROCESSABLE_ENTITY, "no_recipients"),
        Start::TooManyRecipients => (StatusCode::UNPROCESSABLE_ENTITY, "too_many_recipients"),
        Start::TooManyToday => (StatusCode::TOO_MANY_REQUESTS, "too_many_broadcasts"),
    };
    reply(status, code)
}

/// Cancels a sending or paused broadcast: recipients not sent to yet are `skipped`
/// (`cancelled`), the ones sent to stay. Owners and admins. `409 not_sending` otherwise.
#[utoipa::path(post, path = "/broadcasts/{id}/cancel", tag = "broadcasts",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Broadcast), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error)))]
async fn cancel(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match broadcasts::cancel(&app.db, t.team.id, id).await {
        Ok(Some(true)) => shown(&app, t.team.id, id, StatusCode::OK).await,
        Ok(Some(false)) => reply(StatusCode::CONFLICT, "not_sending"),
        Ok(None) => not_found(),
        Err(e) => internal(e, "broadcast cancel"),
    }
}

/// Puts a paused broadcast back to `sending`, once whatever paused it is dealt with. Owners
/// and admins. `409 not_paused` otherwise, `409 another_sending` while another broadcast of the
/// team sends.
#[utoipa::path(post, path = "/broadcasts/{id}/resume", tag = "broadcasts",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Broadcast), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error)))]
async fn resume(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match broadcasts::resume(&app.db, t.team.id, id).await {
        Ok(Resume::Resumed) => shown(&app, t.team.id, id, StatusCode::OK).await,
        Ok(Resume::NotFound) => not_found(),
        Ok(Resume::NotPaused) => reply(StatusCode::CONFLICT, "not_paused"),
        Ok(Resume::AnotherSending) => reply(StatusCode::CONFLICT, "another_sending"),
        Err(e) => internal(e, "broadcast resume"),
    }
}

/// A broadcast's recipients with how each ended, 100 a page in a fixed order. Owners and
/// admins. `status` narrows the page.
#[utoipa::path(get, path = "/broadcasts/{id}/recipients", tag = "broadcasts",
    params(("id" = Uuid, Path), RecipientQuery),
    responses((status = 200, body = RecipientPage), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn recipients(
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    q: Result<Query<RecipientQuery>, QueryRejection>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let status_ok = |s: &String| RECIPIENT_STATUSES.contains(&s.as_str());
    let q = match q {
        Ok(Query(q)) if q.status.as_ref().is_none_or(status_ok) => q,
        _ => return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query"),
    };
    let sql = "SELECT r.id, r.contact_id, r.status, r.reason, r.updated_at, c.display_name, \
               c.handle, c.platform FROM broadcast_recipients r \
               LEFT JOIN contacts c ON c.id = r.contact_id WHERE r.broadcast_id = $1 \
               AND ($2::text IS NULL OR r.status = $2) AND ($3::uuid IS NULL OR r.id > $3) \
               ORDER BY r.id LIMIT $4";
    let values: Vec<Value> = vec![
        id.into(),
        q.status.into(),
        q.cursor.into(),
        (PAGE + 1).into(),
    ];
    let found = async {
        if find(&app.db, t.team.id, id).await?.is_none() {
            return Ok(None);
        }
        let rows = app.db.query_all_raw(stmt(sql, values)).await?;
        let mut items = rows
            .iter()
            .map(|r| {
                Ok(Recipient {
                    id: r.try_get("", "id")?,
                    contact_id: r.try_get("", "contact_id")?,
                    display_name: r.try_get("", "display_name")?,
                    handle: r.try_get("", "handle")?,
                    platform: r.try_get("", "platform")?,
                    status: r.try_get("", "status")?,
                    reason: r.try_get("", "reason")?,
                    updated_at: r.try_get("", "updated_at")?,
                })
            })
            .collect::<Result<Vec<_>, DbErr>>()?;
        let more = items.len() as i64 > PAGE;
        items.truncate(PAGE as usize);
        let next_cursor = more
            .then(|| items.last().map(|r| r.id.to_string()))
            .flatten();
        Ok::<_, DbErr>(Some(RecipientPage { items, next_cursor }))
    };
    match found.await {
        Ok(Some(page)) => Json(page).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "broadcast recipients"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(preview))
        .routes(routes!(send))
        .routes(routes!(cancel))
        .routes(routes!(resume))
        .routes(routes!(recipients))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, text: &str, tag: &str, platform: Option<&str>) -> BroadcastInput {
        BroadcastInput {
            name: name.into(),
            text: text.into(),
            tag: tag.into(),
            platform: platform.map(str::to_owned),
        }
    }

    #[test]
    fn inputs_are_trimmed_and_checked() {
        let ok = checked(input(" Launch ", " Hi {name} ", " VIP ", Some("bluesky"))).unwrap();
        assert_eq!(ok.0, "Launch");
        assert_eq!(ok.1, "Hi {name}");
        assert_eq!(ok.2, "vip");
        assert_eq!(ok.3.as_deref(), Some("bluesky"));
        assert!(checked(input(&"n".repeat(80), &"a".repeat(1000), "vip", None)).is_ok());
        for (bad, field) in [
            (input(" ", "hi", "vip", None), "name"),
            (input(&"n".repeat(81), "hi", "vip", None), "name"),
            (input("n", " ", "vip", None), "text"),
            (input("n", &"a".repeat(1001), "vip", None), "text"),
            // 937 characters and a name of up to 64 pass 1000.
            (
                input("n", &format!("{}{{name}}", "a".repeat(937)), "vip", None),
                "text",
            ),
            (input("n", "hi", "a,b", None), "tag"),
            (input("n", "hi", "vip", Some("x")), "platform"),
        ] {
            assert_eq!(checked(bad).err(), Some(field));
        }
    }
}
