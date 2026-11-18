//! Newsletter sign-ups from public post pages. A team switches them on (`teams.newsletter`,
//! `PATCH /team`); a visitor of a live page posts an email to `POST /public/pages/{code}/subscribe`
//! and gets a confirmation link to the web app, which posts its token to
//! `POST /public/newsletter/verify` (never a state-changing GET: mail scanners open links). Owners
//! and admins read the list (`GET /subscribers`) and remove a row (`DELETE /subscribers/{id}`).
//! Nothing is sent to subscribers yet beyond the confirmation.
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Statement,
};
use serde::{Deserialize, Serialize};
use socialreins_core::{email, token};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, FromWeb},
    pages,
    teams::InTeam,
    waitlist::{error, rejected, Error, Status, VerifyBody},
};
use crate::{
    models::subscribers,
    rate_limit::{self, daily_refund, daily_reserve, ClientIp},
    state::AppState,
};

/// An unconfirmed email gets at most one confirmation email per hour, however often it signs up.
const RESEND_AFTER_HOURS: i64 = 1;
/// Confirmation emails per rolling 24 hours, since the form has no captcha.
const CONFIRMATIONS_PER_IP_PER_DAY: i64 = 20;
const CONFIRMATIONS_PER_TEAM_PER_DAY: i64 = 200;
const CONFIRMATIONS_PER_DAY: i64 = 500;
const CONFIRMATIONS_BUCKET: &str = "newsletter:confirm";
/// shortcut: the list stops at the newest 5000 rows, add paging once a team has more.
const LIST_LIMIT: u64 = 5000;

#[derive(Deserialize, ToSchema)]
pub struct Subscribe {
    pub email: String,
}

#[derive(Serialize, ToSchema)]
pub struct Confirmed {
    /// Always `verified`, also when the email already was.
    pub status: String,
    /// The name of the team the email is now subscribed to.
    pub team: String,
}

#[derive(Serialize, ToSchema)]
pub struct Subscriber {
    pub id: Uuid,
    pub email: String,
    /// Whether the emailed link was confirmed. Only these should ever be mailed.
    pub confirmed: bool,
    pub subscribed_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct Subscribers {
    /// Newest first, at most 5000.
    pub subscribers: Vec<Subscriber>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

/// Subscribes an email to the team of the live public page `code`. The reply is `200 ok` whether
/// or not the email was known; only a new or unconfirmed one is mailed a link (once an hour, 20 a
/// day per IP, 200 per team and 500 overall). `404 not_found` unless the page is live and its team
/// has the newsletter on. Shares the waitlist IP limit.
#[utoipa::path(post, path = "/public/pages/{code}/subscribe", tag = "posts",
    params(("code" = String, Path)), request_body = Subscribe,
    responses((status = 200, body = Status), (status = 400, body = Error),
        (status = 404, body = Error), (status = 413, body = Error), (status = 415, body = Error),
        (status = 422, body = Error), (status = 429, body = Error), (status = 500, body = Error)))]
async fn subscribe(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(code): Path<String>,
    request: Request,
) -> Response {
    // Counted before the body is read, so malformed requests use up the limit too.
    if !app.waitlist_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let body = match Json::<Subscribe>::from_request(request, &()).await {
        Ok(Json(body)) => body,
        Err(r) => return rejected(&r).into_response(),
    };
    let Some(address) = email::normalize(&body.email) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email");
    };
    let team = match pages::live(&app.db, code.clone()).await {
        Ok(Some((_, team, _))) if team.newsletter => team,
        Ok(_) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "newsletter page read"),
    };
    let secret = match claim(&app, team.id, &address, &code).await {
        Ok(secret) => secret,
        Err(e) => return internal(e, "newsletter subscribe"),
    };
    if let Some(secret) = secret {
        // A failed mail keeps the row and the ok reply; the error carries no email or token.
        let (team_id, name) = (team.id, team.name);
        tokio::spawn(async move {
            send_confirmation(&app, ip, team_id, &name, &address, &secret).await;
        });
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Adds the email as unconfirmed, or gives an unconfirmed one a new link token unless one went
/// out in the last hour, and returns the token to mail. One statement checks and stamps, so
/// concurrent sign-ups cannot both send; a resend replaces the token, so only the newest link works.
async fn claim(
    app: &AppState,
    team_id: Uuid,
    address: &str,
    code: &str,
) -> Result<Option<String>, sea_orm::DbErr> {
    let secret = token::new();
    let sql = format!(
        "INSERT INTO subscribers (id, team_id, email, page_code, verify_token_hash, \
         verify_sent_at) VALUES ($1, $2, $3, $4, $5, now()) \
         ON CONFLICT (team_id, email) DO UPDATE \
         SET verify_token_hash = EXCLUDED.verify_token_hash, verify_sent_at = now() \
         WHERE subscribers.verified_at IS NULL AND (subscribers.verify_sent_at IS NULL \
         OR subscribers.verify_sent_at < now() - interval '{RESEND_AFTER_HOURS} hour') \
         RETURNING id"
    );
    let values = [
        Uuid::now_v7().into(),
        team_id.into(),
        address.into(),
        code.into(),
        token::hash(&secret).into(),
    ];
    let row = app
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    Ok(row.map(|_| secret))
}

/// Mails the link within the daily caps; all are checked before any is counted. Unsent (a cap
/// reached or mail failed), the claim is undone so the next sign-up of this email may try again;
/// logs carry no email or token.
async fn send_confirmation(
    app: &AppState,
    ip: Option<std::net::IpAddr>,
    team_id: Uuid,
    team: &str,
    address: &str,
    secret: &str,
) {
    let ip = ip.map_or_else(|| "unknown".into(), |ip| rate_limit::bucket(ip).to_string());
    let caps = [
        (
            format!("{CONFIRMATIONS_BUCKET}:ip:{ip}"),
            CONFIRMATIONS_PER_IP_PER_DAY,
        ),
        (
            format!("{CONFIRMATIONS_BUCKET}:team:{team_id}"),
            CONFIRMATIONS_PER_TEAM_PER_DAY,
        ),
        (CONFIRMATIONS_BUCKET.to_owned(), CONFIRMATIONS_PER_DAY),
    ];
    let reserved = match daily_reserve(&app.db, &caps).await {
        Ok(Some(ids)) => ids,
        Ok(None) => {
            tracing::warn!("newsletter confirmation not sent: daily cap reached");
            return unclaim(app, secret).await;
        }
        Err(e) => {
            tracing::error!(error = %e, "newsletter confirmation cap check failed");
            return unclaim(app, secret).await;
        }
    };
    let url = format!("{}/newsletter/verify?token={secret}", app.web_url);
    let text = format!(
        "Confirm that you want to get the newsletter of {team}:\n{url}\n\n\
         If you did not sign up, ignore this email.\n"
    );
    let subject = format!("Confirm your subscription to {team}");
    if let Err(e) = app.mailer.send(address, &subject, text).await {
        tracing::error!(error = %e, "newsletter confirmation not sent");
        daily_refund(&app.db, &reserved).await;
        unclaim(app, secret).await;
    }
}

/// Clears `verify_sent_at` of the claim holding `secret`, so it does not hold back a resend.
async fn unclaim(app: &AppState, secret: &str) {
    let sql = "UPDATE subscribers SET verify_sent_at = NULL WHERE verify_token_hash = $1";
    let stmt =
        Statement::from_sql_and_values(DbBackend::Postgres, sql, [token::hash(secret).into()]);
    if let Err(e) = app.db.execute_raw(stmt).await {
        tracing::warn!(error = %e, "newsletter confirmation unclaim failed");
    }
}

/// Confirms an email with the token from its confirmation link, which opens
/// `{WEB_URL}/newsletter/verify?token=...`; the web page posts it here, so a mail scanner that
/// opens the link changes nothing. The first call sets the confirmation time, later ones keep it.
/// `422 invalid_link` for a missing, malformed or unknown token. Shares the waitlist IP limit.
#[utoipa::path(post, path = "/public/newsletter/verify", tag = "posts", request_body = VerifyBody,
    responses((status = 200, body = Confirmed), (status = 400, body = Error),
        (status = 413, body = Error), (status = 415, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 500, body = Error)))]
async fn verify(State(app): State<AppState>, ClientIp(ip): ClientIp, request: Request) -> Response {
    if !app.waitlist_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let secret = match Json::<VerifyBody>::from_request(request, &()).await {
        Ok(Json(body)) => body.token.unwrap_or_default(),
        Err(r) => return rejected(&r).into_response(),
    };
    let invalid = || reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_link");
    if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid();
    }
    let sql = "UPDATE subscribers s SET verified_at = COALESCE(s.verified_at, now()) \
               FROM teams t WHERE s.verify_token_hash = $1 AND t.id = s.team_id RETURNING t.name";
    let stmt =
        Statement::from_sql_and_values(DbBackend::Postgres, sql, [token::hash(&secret).into()]);
    match app.db.query_one_raw(stmt).await {
        Ok(Some(row)) => match row.try_get::<String>("", "name") {
            Ok(team) => Json(Confirmed {
                status: "verified".into(),
                team,
            })
            .into_response(),
            Err(e) => internal(e, "newsletter verify"),
        },
        Ok(None) => invalid(),
        Err(e) => internal(e, "newsletter verify"),
    }
}

/// The team's newsletter sign-ups, newest first. Owners and admins only.
#[utoipa::path(get, path = "/subscribers", tag = "teams",
    responses((status = 200, body = Subscribers), (status = 401, body = Error),
        (status = 403, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let rows = subscribers::Entity::find()
        .filter(subscribers::Column::TeamId.eq(t.team.id))
        .order_by_desc(subscribers::Column::CreatedAt)
        .order_by_desc(subscribers::Column::Id)
        .limit(LIST_LIMIT)
        .all(&app.db)
        .await;
    match rows {
        Ok(rows) => Json(Subscribers {
            subscribers: rows
                .into_iter()
                .map(|r| Subscriber {
                    id: r.id,
                    email: r.email,
                    confirmed: r.verified_at.is_some(),
                    subscribed_at: r.created_at,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => internal(e, "subscribers list"),
    }
}

/// Removes one subscriber of the team. Owners and admins only. The person may sign up again.
#[utoipa::path(delete, path = "/subscribers/{id}", tag = "teams",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn remove(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let deleted = subscribers::Entity::delete_many()
        .filter(subscribers::Column::TeamId.eq(t.team.id))
        .filter(subscribers::Column::Id.eq(id))
        .exec(&app.db)
        .await;
    match deleted {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "subscriber delete"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(subscribe))
        .routes(routes!(verify))
        .routes(routes!(list))
        .routes(routes!(remove))
}
