//! Newsletters of the current team: one email to every confirmed subscriber who did not
//! unsubscribe (`crate::newsletters`). Owners and admins only, reads included, and like
//! broadcasts these routes take a session only.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, QueryResult, Statement, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use socialreins_core::token;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::numbered::{NewsletterId, ID_PARAM};
use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    newsletters::{self, Envelope, Start},
    rate_limit::{daily_refund, daily_reserve},
    state::AppState,
};

/// Newsletters a list holds.
// shortcut: no paging of the list; add a cursor once a team keeps more than 200.
const LIST: i64 = 200;

#[derive(Serialize, ToSchema)]
pub struct Counts {
    /// The recipients snapshotted at the start; 0 for a draft. What is not `sent`, `failed` or
    /// `pending` was skipped: the person unsubscribed or was removed meanwhile, or the
    /// newsletter was cancelled first.
    pub total: i64,
    pub sent: i64,
    /// Never tried again: the mail provider refused it, or the send was cut off.
    pub failed: i64,
    /// Not mailed yet, the ones going out right now included.
    pub pending: i64,
}

#[derive(Serialize, ToSchema)]
pub struct Newsletter {
    pub id: Uuid,
    /// The newsletter's number, unique over every team: the address web pages use. Every `{id}` of a
    /// newsletter route takes it as well as `id`.
    pub number: i64,
    pub subject: String,
    /// Plain text: a blank line ends a paragraph, `**text**` is bold, a bare `https://`
    /// address becomes a link. Nothing else is markup; HTML is shown as typed.
    pub body: String,
    /// `draft`, `sending`, `paused`, `sent` or `cancelled`.
    pub status: String,
    /// Only while `paused`: what the mail provider said, or that 5 sends in a row failed.
    pub pause_reason: Option<String>,
    /// When a test mail of this subject and body last went out; `null` after an edit.
    pub tested_at: Option<DateTime<FixedOffset>>,
    pub counts: Counts,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub started_at: Option<DateTime<FixedOffset>>,
    pub finished_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewsletterInput {
    /// 1 to 150 characters after trimming, one line.
    pub subject: String,
    /// 1 to 20000 characters after trimming, at most 20 links.
    pub body: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendInput {
    /// How many people the sender expects to mail: the team's confirmed subscribers who did
    /// not unsubscribe.
    pub confirm_total: u64,
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

fn invalid(field: &str) -> Response {
    let body = json!({"error": "invalid_newsletter", "field": field});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

fn stmt(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

const SELECT: &str = "SELECT n.*, c.* FROM newsletters n, LATERAL (SELECT count(*) AS total, \
    count(*) FILTER (WHERE r.status = 'sent') AS sent, \
    count(*) FILTER (WHERE r.status = 'failed') AS failed, \
    count(*) FILTER (WHERE r.status IN ('pending', 'sending', 'mailing')) AS pending \
    FROM newsletter_recipients r WHERE r.newsletter_id = n.id) c WHERE n.team_id = $1";

fn newsletter_json(r: &QueryResult) -> Result<Newsletter, DbErr> {
    Ok(Newsletter {
        id: r.try_get("", "id")?,
        number: r.try_get("", "number")?,
        subject: r.try_get("", "subject")?,
        body: r.try_get("", "body")?,
        status: r.try_get("", "status")?,
        pause_reason: r.try_get("", "pause_reason")?,
        tested_at: r.try_get("", "tested_at")?,
        counts: Counts {
            total: r.try_get("", "total")?,
            sent: r.try_get("", "sent")?,
            failed: r.try_get("", "failed")?,
            pending: r.try_get("", "pending")?,
        },
        created_by: r.try_get("", "created_by")?,
        created_at: r.try_get("", "created_at")?,
        started_at: r.try_get("", "started_at")?,
        finished_at: r.try_get("", "finished_at")?,
    })
}

async fn find(
    db: &impl ConnectionTrait,
    team: Uuid,
    id: Uuid,
) -> Result<Option<Newsletter>, DbErr> {
    let sql = format!("{SELECT} AND n.id = $2");
    let row = db
        .query_one_raw(stmt(&sql, vec![team.into(), id.into()]))
        .await?;
    row.as_ref().map(newsletter_json).transpose()
}

/// `find` as a response: the newsletter with `status`, or `404`.
async fn shown(app: &AppState, team: Uuid, id: Uuid, status: StatusCode) -> Response {
    match find(&app.db, team, id).await {
        Ok(Some(n)) => (status, Json(n)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "newsletter read"),
    }
}

/// `404` when the team has no newsletter `id`, else `409` with `code`: what a write answers
/// when its one statement matched nothing.
async fn refused(app: &AppState, team: Uuid, id: Uuid, code: &str) -> Response {
    match find(&app.db, team, id).await {
        Ok(Some(_)) => reply(StatusCode::CONFLICT, code),
        Ok(None) => not_found(),
        Err(e) => internal(e, "newsletter read"),
    }
}

/// The team's newsletters, newest first (the latest 200). Owners and admins. The
/// `x-total-count` header says how many there are in all, so a reader can tell what the cap
/// leaves out.
#[utoipa::path(get, path = "/newsletters", tag = "newsletters",
    responses((status = 200, body = [Newsletter],
        headers(("x-total-count" = u64, description = "Every newsletter of the team, whether shown or not."))),
        (status = 401, body = Error), (status = 403, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let sql = format!("{SELECT} ORDER BY n.created_at DESC, n.id DESC LIMIT $2");
    let found = async {
        let rows = app
            .db
            .query_all_raw(stmt(&sql, vec![t.team.id.into(), LIST.into()]))
            .await?;
        let list = rows
            .iter()
            .map(newsletter_json)
            .collect::<Result<Vec<_>, _>>()?;
        let row = app
            .db
            .query_one_raw(stmt(
                "SELECT count(*) AS total FROM newsletters WHERE team_id = $1",
                vec![t.team.id.into()],
            ))
            .await?;
        let total: i64 = row
            .map(|r| r.try_get("", "total"))
            .transpose()?
            .unwrap_or(0);
        Ok::<_, DbErr>((list, total))
    };
    match found.await {
        // A header, not a field: the body stays the array it always was, so a web app that
        // does not know it yet keeps working.
        Ok((list, total)) => ([("x-total-count", total.to_string())], Json(list)).into_response(),
        Err(e) => internal(e, "newsletter list"),
    }
}

/// Creates a draft. Owners and admins. `422 invalid_newsletter` names the bad `field`
/// (`subject` or `body`). Nothing is sent until `POST /newsletters/{id}/send`.
#[utoipa::path(post, path = "/newsletters", tag = "newsletters", request_body = NewsletterInput,
    responses((status = 201, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<NewsletterInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    if let Some(res) = super::billing::needs_automation(&app, t.team.id).await {
        return res;
    }
    let (subject, body) = match newsletters::checked(&input.subject, &input.body) {
        Ok(c) => c,
        Err(field) => return invalid(field),
    };
    let id = Uuid::now_v7();
    let sql = "INSERT INTO newsletters (id, team_id, subject, body, created_by) \
               VALUES ($1, $2, $3, $4, $5)";
    let values: Vec<Value> = vec![
        id.into(),
        t.team.id.into(),
        subject.into(),
        body.into(),
        t.current.0.id.into(),
    ];
    if let Err(e) = app.db.execute_raw(stmt(sql, values)).await {
        return internal(e, "newsletter create");
    }
    shown(&app, t.team.id, id, StatusCode::CREATED).await
}

/// One newsletter with its counts. Owners and admins.
#[utoipa::path(get, path = "/newsletters/{id}", tag = "newsletters", params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, NewsletterId(id): NewsletterId) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    shown(&app, t.team.id, id, StatusCode::OK).await
}

/// Replaces a draft. Owners and admins. A changed subject or body clears `tested_at`, so a
/// new test must go out before the send. `409 already_started` for anything but a draft.
#[utoipa::path(put, path = "/newsletters/{id}", tag = "newsletters",
    request_body = NewsletterInput, params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    NewsletterId(id): NewsletterId,
    Body(input): Body<NewsletterInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let (subject, body) = match newsletters::checked(&input.subject, &input.body) {
        Ok(c) => c,
        Err(field) => return invalid(field),
    };
    let sql = "UPDATE newsletters SET subject = $3, body = $4, tested_at = CASE \
               WHEN subject = $3 AND body = $4 THEN tested_at END \
               WHERE id = $1 AND team_id = $2 AND status = 'draft'";
    let values: Vec<Value> = vec![id.into(), t.team.id.into(), subject.into(), body.into()];
    match app.db.execute_raw(stmt(sql, values)).await {
        Ok(r) if r.rows_affected() == 1 => shown(&app, t.team.id, id, StatusCode::OK).await,
        Ok(_) => refused(&app, t.team.id, id, "already_started").await,
        Err(e) => internal(e, "newsletter update"),
    }
}

/// Deletes a draft or a finished newsletter with its recipients. Owners and admins.
/// `409 still_sending` while it sends or is paused: cancel it first.
#[utoipa::path(delete, path = "/newsletters/{id}", tag = "newsletters",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error), (status = 409, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    NewsletterId(id): NewsletterId,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let sql = "DELETE FROM newsletters WHERE id = $1 AND team_id = $2 \
               AND status NOT IN ('sending', 'paused')";
    let key: Vec<Value> = vec![id.into(), t.team.id.into()];
    match app.db.execute_raw(stmt(sql, key)).await {
        Ok(r) if r.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => refused(&app, t.team.id, id, "still_sending").await,
        Err(e) => internal(e, "newsletter delete"),
    }
}

/// Mails a draft to the caller's own account email, and to nobody else, exactly as a
/// subscriber would get it (its unsubscribe links unsubscribe nobody), then sets `tested_at`.
/// Owners and admins. `422 sender_required` until the team has its `newsletter_sender`,
/// `409 already_started` for anything but a draft, `429 too_many_tests` after 10 in 24 hours
/// per team, `502 mail_failed` when the mail did not go out.
#[utoipa::path(post, path = "/newsletters/{id}/test", tag = "newsletters",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error), (status = 502, body = Error)))]
async fn test(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    NewsletterId(id): NewsletterId,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let team = &t.team;
    let draft = match find(&app.db, team.id, id).await {
        Ok(Some(n)) if n.status == "draft" => n,
        Ok(Some(_)) => return reply(StatusCode::CONFLICT, "already_started"),
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "newsletter read"),
    };
    let sender = (
        &team.newsletter_sender_name,
        &team.newsletter_sender_address,
    );
    let (Some(sender_name), Some(sender_address)) = sender else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "sender_required");
    };
    let cap = [(
        newsletters::test_bucket(team.id),
        newsletters::TESTS_PER_DAY,
    )];
    let hits = match daily_reserve(&app.db, &cap).await {
        Ok(Some(hits)) => hits,
        Ok(None) => return reply(StatusCode::TOO_MANY_REQUESTS, "too_many_tests"),
        Err(e) => return internal(e, "newsletter test cap"),
    };
    let envelope = Envelope {
        subject: &draft.subject,
        body: &draft.body,
        sender_name,
        sender_address,
        sender_logo: team.newsletter_sender_logo.as_deref(),
        team: &team.name,
    };
    // A token no subscriber has: the links look and answer as real ones do.
    let sent = newsletters::mail(&app, &t.current.0.email, &token::new(), &envelope).await;
    if let Err(e) = sent {
        tracing::error!(error = %e, "newsletter test not sent");
        daily_refund(&app.db, &hits).await;
        return reply(StatusCode::BAD_GATEWAY, "mail_failed");
    }
    // Only for the text that went out: an edit made meanwhile needs its own test.
    let sql = "UPDATE newsletters SET tested_at = now() WHERE id = $1 AND team_id = $2 \
               AND status = 'draft' AND subject = $3 AND body = $4";
    let values: Vec<Value> = vec![
        id.into(),
        team.id.into(),
        draft.subject.into(),
        draft.body.into(),
    ];
    if let Err(e) = app.db.execute_raw(stmt(sql, values)).await {
        return internal(e, "newsletter tested");
    }
    shown(&app, team.id, id, StatusCode::OK).await
}

/// Starts a draft: every confirmed subscriber who did not unsubscribe becomes a recipient and
/// the newsletter is `sending`; each recipient is then mailed at most once, 50 a minute, and
/// never tried again. Owners and admins. `confirm_total` must be the number of those
/// subscribers at this moment, else `409 audience_changed` with the current `total`. In this
/// order: `409 already_started` for anything but a draft, `422 sender_required` without the
/// team's `newsletter_sender`, `422 test_required` unless a test went out since the last edit,
/// `422 no_recipients`, `422 too_many_recipients` over 5000, `409 audience_changed`,
/// `429 too_many_newsletters` when the team started one in the last 24 hours. Any error of
/// the mail provider, or 5 failed sends in a row, makes it `paused` with a `pause_reason`: it
/// then waits for `POST /newsletters/{id}/resume`. All teams together send 10000 newsletter
/// mails in 24 hours; past that, recipients wait as `pending`.
#[utoipa::path(post, path = "/newsletters/{id}/send", tag = "newsletters",
    request_body = SendInput, params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
async fn send(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    NewsletterId(id): NewsletterId,
    Body(input): Body<SendInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    if let Some(res) = super::billing::needs_automation(&app, t.team.id).await {
        return res;
    }
    let team = t.team.id;
    let started = match newsletters::start(&app.db, team, id, input.confirm_total).await {
        Ok(s) => s,
        Err(e) => return internal(e, "newsletter start"),
    };
    let (status, code) = match started {
        Start::Started => return shown(&app, team, id, StatusCode::OK).await,
        Start::NotFound => return not_found(),
        Start::AudienceChanged(total) => {
            let body = json!({"error": "audience_changed", "total": total});
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
        Start::AlreadyStarted => (StatusCode::CONFLICT, "already_started"),
        Start::SenderRequired => (StatusCode::UNPROCESSABLE_ENTITY, "sender_required"),
        Start::TestRequired => (StatusCode::UNPROCESSABLE_ENTITY, "test_required"),
        Start::NoRecipients => (StatusCode::UNPROCESSABLE_ENTITY, "no_recipients"),
        Start::TooManyRecipients => (StatusCode::UNPROCESSABLE_ENTITY, "too_many_recipients"),
        Start::TooManyToday => (StatusCode::TOO_MANY_REQUESTS, "too_many_newsletters"),
    };
    reply(status, code)
}

/// Cancels a sending or paused newsletter: recipients not mailed yet are skipped, the ones
/// mailed stay. Owners and admins. `409 not_sending` otherwise.
#[utoipa::path(post, path = "/newsletters/{id}/cancel", tag = "newsletters",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error)))]
async fn cancel(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    NewsletterId(id): NewsletterId,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match newsletters::cancel(&app.db, t.team.id, id).await {
        Ok(Some(true)) => shown(&app, t.team.id, id, StatusCode::OK).await,
        Ok(Some(false)) => reply(StatusCode::CONFLICT, "not_sending"),
        Ok(None) => not_found(),
        Err(e) => internal(e, "newsletter cancel"),
    }
}

/// Puts a paused newsletter back to `sending`, once whatever paused it is dealt with; the
/// failed recipients are not tried again. Owners and admins. `409 not_paused` otherwise.
#[utoipa::path(post, path = "/newsletters/{id}/resume", tag = "newsletters",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Newsletter), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error)))]
async fn resume(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    NewsletterId(id): NewsletterId,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match newsletters::resume(&app.db, t.team.id, id).await {
        Ok(Some(true)) => shown(&app, t.team.id, id, StatusCode::OK).await,
        Ok(Some(false)) => reply(StatusCode::CONFLICT, "not_paused"),
        Ok(None) => not_found(),
        Err(e) => internal(e, "newsletter resume"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(test))
        .routes(routes!(send))
        .routes(routes!(cancel))
        .routes(routes!(resume))
}
