//! Newsletter sign-ups from public post pages. A team switches them on (`teams.newsletter`,
//! `PATCH /team`); a visitor of a live page posts an email to `POST /public/pages/{code}/subscribe`
//! and gets a confirmation link to the web app, which posts its token to
//! `POST /public/newsletter/verify` (never a state-changing GET: mail scanners open links). Owners
//! and admins read the list (`GET /subscribers`) and remove a row (`DELETE /subscribers/{id}`).
//! `POST /public/newsletter/unsubscribe` takes the token of a newsletter's unsubscribe links
//! (`crate::newsletters`) and sets `unsubscribed_at`, which keeps the address out of every send
//! until the person signs up again and confirms.
use axum::{
    extract::{rejection::QueryRejection, FromRequest, Path, Query, Request, State},
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
/// A safety valve only: far above what the per-IP and per-team caps let one attacker use up.
const CONFIRMATIONS_PER_DAY: i64 = 5000;
const CONFIRMATIONS_BUCKET: &str = "newsletter:confirm";
/// shortcut: the list stops at the newest 5000 rows, add paging once a team has more.
const LIST_LIMIT: u64 = 5000;

#[derive(Deserialize, ToSchema)]
pub struct Subscribe {
    pub email: String,
    /// The Turnstile widget's token; required when `GET /auth/providers` gives a
    /// `turnstile_site_key`, else ignored.
    pub turnstile_token: Option<String>,
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
    /// When the person unsubscribed, else `null`. Never mailed while set; signing up again on
    /// a public page and confirming clears it.
    pub unsubscribed_at: Option<DateTime<FixedOffset>>,
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
/// day per IP, 200 per team and 5000 overall). `404 not_found` unless the page is live and its team
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
    if !app.waitlist_limiter.allow(&app.db, ip).await {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let body = match Json::<Subscribe>::from_request(request, &()).await {
        Ok(Json(body)) => body,
        Err(r) => return rejected(&r).into_response(),
    };
    let Some(address) = email::normalize(&body.email) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email");
    };
    let token = body.turnstile_token.as_deref();
    if let Err(r) = crate::turnstile::check(app.turnstile.as_ref(), token, ip).await {
        return r.into_response();
    }
    let team = match pages::live(&app.db, code.clone()).await {
        Ok(Some((_, team, _))) if team.newsletter => team,
        Ok(_) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "newsletter page read"),
    };
    // The caps are counted before a row or a token is made, so a flood adds no rows and a blocked
    // attempt leaves the link already sent working. A claim that sends nothing gives its hit back.
    let ip_key = ip.map_or_else(|| "unknown".into(), |ip| rate_limit::bucket(ip).to_string());
    let reserved = match reserve(&app, &ip_key, team.id).await {
        Ok(Some(ids)) => ids,
        Ok(None) => {
            tracing::warn!("newsletter confirmation not sent: daily cap reached");
            return Json(serde_json::json!({"status": "ok"})).into_response();
        }
        Err(e) => return internal(e, "newsletter cap check"),
    };
    let secret = match claim(&app, team.id, &address, &code).await {
        Ok(Some(secret)) => secret,
        Ok(None) => {
            daily_refund(&app.db, &reserved).await;
            return Json(serde_json::json!({"status": "ok"})).into_response();
        }
        Err(e) => {
            daily_refund(&app.db, &reserved).await;
            return internal(e, "newsletter subscribe");
        }
    };
    // A failed mail keeps the row and the ok reply; the error carries no email or token.
    let sending = crate::mail::Sending::begin();
    tokio::spawn(async move {
        let _sending = sending;
        send_confirmation(&app, &reserved, &address, &secret).await
    });
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Adds the email as unconfirmed, or gives an unconfirmed one (or one that unsubscribed, who
/// must confirm again) a new link token unless one went out in the last hour, and returns the
/// token to mail. One statement checks and stamps, so
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
         WHERE (subscribers.verified_at IS NULL OR subscribers.unsubscribed_at IS NOT NULL) \
         AND (subscribers.verify_sent_at IS NULL \
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

/// Reserves one hit in each daily cap (per IP, per team, overall); `None` when one is reached.
async fn reserve(
    app: &AppState,
    ip: &str,
    team_id: Uuid,
) -> Result<Option<Vec<i64>>, sea_orm::DbErr> {
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
    daily_reserve(&app.db, &caps).await
}

/// Mails the link. Unsent (mail failed), the hits are given back and the claim is undone, so the
/// next sign-up of this email may try again; logs carry no email or token. The mail names no
/// team: the name is the team's own text, and this is our sender.
async fn send_confirmation(app: &AppState, reserved: &[i64], address: &str, secret: &str) {
    let url = format!("{}/newsletter/verify?token={secret}", app.web_url);
    let text = format!(
        "Confirm that you want to get a newsletter from a page on SocialReins:\n{url}\n\n\
         If you did not sign up, ignore this email.\n"
    );
    if let Err(e) = app
        .mailer
        .send(
            address,
            "Confirm your newsletter subscription",
            text,
            &app.web_url,
        )
        .await
    {
        tracing::error!(error = %e, "newsletter confirmation not sent");
        daily_refund(&app.db, reserved).await;
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
/// A link mailed after the person unsubscribed also subscribes them again.
/// `422 invalid_link` for a missing, malformed or unknown token. Shares the waitlist IP limit.
#[utoipa::path(post, path = "/public/newsletter/verify", tag = "posts", request_body = VerifyBody,
    responses((status = 200, body = Confirmed), (status = 400, body = Error),
        (status = 413, body = Error), (status = 415, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 500, body = Error)))]
async fn verify(State(app): State<AppState>, ClientIp(ip): ClientIp, request: Request) -> Response {
    if !app.waitlist_limiter.allow(&app.db, ip).await {
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
    // Only a link from after the unsubscribe clears it: an older mail's link does not.
    let sql = "UPDATE subscribers s SET verified_at = COALESCE(s.verified_at, now()), \
               unsubscribed_at = CASE WHEN s.verify_sent_at > s.unsubscribed_at THEN NULL \
               ELSE s.unsubscribed_at END \
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

#[derive(Deserialize)]
struct UnsubscribeQuery {
    token: Option<String>,
}

/// Unsubscribes the address a newsletter's unsubscribe link belongs to, for good until the
/// person signs up and confirms again. The token comes in the body (`{"token"}`, from the web
/// page `{WEB_URL}/newsletter/unsubscribe?token=...` the mail links to, whose button posts it)
/// or in the query, where the body is ignored: that is the one-click POST of the mail's
/// `List-Unsubscribe` header (RFC 8058). There is no `GET`: mail scanners open links. The
/// answer is `200 ok` for any token, known or not. 120 a minute per IP.
#[utoipa::path(post, path = "/public/newsletter/unsubscribe", tag = "posts",
    params(("token" = Option<String>, Query, description = "The token, for a one-click POST")),
    request_body = Option<VerifyBody>,
    responses((status = 200, body = Status), (status = 400, body = Error),
        (status = 413, body = Error), (status = 415, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 500, body = Error)))]
async fn unsubscribe(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    query: Result<Query<UnsubscribeQuery>, QueryRejection>,
    request: Request,
) -> Response {
    if !app.unsubscribe_limiter.allow(&app.db, ip).await {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let secret = match query.ok().and_then(|q| q.0.token) {
        Some(secret) => secret,
        None => match Json::<VerifyBody>::from_request(request, &()).await {
            Ok(Json(body)) => body.token.unwrap_or_default(),
            Err(r) => return rejected(&r).into_response(),
        },
    };
    if secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        let sql = "UPDATE subscribers SET unsubscribed_at = COALESCE(unsubscribed_at, now()) \
                   WHERE unsubscribe_token_hash = $1";
        let stmt =
            Statement::from_sql_and_values(DbBackend::Postgres, sql, [token::hash(&secret).into()]);
        if let Err(e) = app.db.execute_raw(stmt).await {
            return internal(e, "newsletter unsubscribe");
        }
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
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
                    unsubscribed_at: r.unsubscribed_at,
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
        .routes(routes!(unsubscribe))
        .routes(routes!(list))
        .routes(routes!(remove))
}
