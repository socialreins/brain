//! API tokens: bearer secrets owned by a team, for servers such as Content Machine. Owners and
//! admins list, create and revoke them with a session. A token is whole-team or limited to one
//! social set; a set token sees nothing outside its set, answered as `404` like a missing record.
//! `Actor` is the extractor of the few routes that take a token as well as a session.
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{header, request::Parts, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::{
    team::{self, Role},
    token,
};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::{InTeam, Team},
    waitlist::{error, Error},
};
use crate::{
    models::{api_tokens, social_sets, teams},
    state::AppState,
};

/// Most live tokens one team holds.
const MAX_LIVE_TOKENS: u64 = 20;
const MAX_EXPIRY_DAYS: i64 = 3650;
/// Characters of random secret after the `sr_{team|set}_live_` prefix.
const SECRET_CHARS: usize = 48;
/// Random characters kept in `prefix_hint` after the type prefix, to tell tokens apart.
const HINT_CHARS: usize = 4;

#[derive(Serialize, ToSchema)]
pub struct ApiToken {
    pub id: Uuid,
    pub name: String,
    /// The set the token is limited to; `null` for a whole-team token.
    pub social_set_id: Option<Uuid>,
    /// The start of the secret, e.g. `sr_set_live_a1b2`, to tell tokens apart.
    pub prefix_hint: String,
    pub created_by: Option<Uuid>,
    pub last_used_at: Option<DateTime<FixedOffset>>,
    pub expires_at: Option<DateTime<FixedOffset>>,
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct NewTokenReply {
    pub token: ApiToken,
    /// The bearer secret. Shown once, never again.
    pub secret: String,
}

#[derive(Deserialize, ToSchema)]
pub struct NewToken {
    /// 1 to 60 characters after trimming.
    pub name: String,
    /// A live set of the current team; absent or `null` for a whole-team token.
    pub social_set_id: Option<Uuid>,
    /// 1 to 3650; absent or `null`: never expires.
    pub expires_in_days: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct SetRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize, ToSchema)]
pub struct Me {
    /// The team; a token acts as `admin`.
    pub team: Team,
    /// The set a token is limited to.
    pub social_set: Option<SetRef>,
    /// The token's name; `null` for a session.
    pub token_name: Option<String>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn forbidden() -> Response {
    reply(StatusCode::FORBIDDEN, "forbidden")
}

fn invalid_token() -> Response {
    reply(StatusCode::UNAUTHORIZED, "invalid_token")
}

fn token_json(t: api_tokens::Model) -> ApiToken {
    ApiToken {
        id: t.id,
        name: t.name,
        social_set_id: t.social_set_id,
        prefix_hint: t.prefix_hint,
        created_by: t.created_by,
        last_used_at: t.last_used_at,
        expires_at: t.expires_at,
        created_at: t.created_at,
    }
}

/// Not revoked and not expired.
fn live() -> Condition {
    Condition::all()
        .add(api_tokens::Column::RevokedAt.is_null())
        .add(
            Condition::any()
                .add(api_tokens::Column::ExpiresAt.is_null())
                .add(api_tokens::Column::ExpiresAt.gt(Utc::now().fixed_offset())),
        )
}

/// Who calls a token-capable route: a signed-in user in their current team (as `InTeam`), or an
/// API token, which acts as an admin of its team. A request with an `Authorization` header is
/// judged by that header alone, never by a cookie. Session writes need the web origin
/// (`FromWeb`); token requests carry no cookie, so they skip it.
pub struct Actor {
    pub team: teams::Model,
    pub role: Role,
    /// The signed-in user; `None` for a token.
    pub user_id: Option<Uuid>,
    pub token: Option<api_tokens::Model>,
}

impl Actor {
    /// The one set a set token is limited to.
    pub fn set_id(&self) -> Option<Uuid> {
        self.token.as_ref().and_then(|t| t.social_set_id)
    }

    /// The key of per-caller rate limits: the user, or the token.
    pub fn key(&self) -> Uuid {
        match (&self.token, self.user_id) {
            (Some(t), _) => t.id,
            (None, Some(u)) => u,
            (None, None) => self.team.id,
        }
    }
}

impl FromRequestParts<AppState> for Actor {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Response> {
        if let Some(auth) = parts.headers.get(header::AUTHORIZATION) {
            let secret = auth
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim)
                .filter(|s| s.starts_with("sr_"))
                .ok_or_else(invalid_token)?;
            let found = api_tokens::Entity::find()
                .filter(api_tokens::Column::TokenHash.eq(token::hash(secret)))
                .filter(live())
                .one(&app.db)
                .await
                .map_err(|e| internal(e, "api token lookup"))?;
            let row = found.ok_or_else(invalid_token)?;
            if let Some(wait) = app.api_token_limiter.wait(row.id) {
                let mut res = reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
                let secs = HeaderValue::from(wait.as_secs().max(1));
                res.headers_mut().insert(header::RETRY_AFTER, secs);
                return Err(res);
            }
            let team = teams::Entity::find_by_id(row.team_id)
                .one(&app.db)
                .await
                .map_err(|e| internal(e, "api token team"))?
                .ok_or_else(invalid_token)?;
            touch(app, &row).await;
            return Ok(Self {
                team,
                role: Role::Admin,
                user_id: None,
                token: Some(row),
            });
        }
        if !parts.method.is_safe() {
            FromWeb::from_request_parts(parts, app)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        let t = InTeam::from_request_parts(parts, app)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Self {
            team: t.team,
            role: t.role,
            user_id: Some(t.current.0.id),
            token: None,
        })
    }
}

/// Records a token's use, at most once a minute; the filter keeps concurrent requests to one
/// write. A failure is logged, not answered: it only costs a stale `last_used_at`.
async fn touch(app: &AppState, row: &api_tokens::Model) {
    let now = Utc::now().fixed_offset();
    let stale = now - Duration::minutes(1);
    if row.last_used_at.is_some_and(|t| t >= stale) {
        return;
    }
    let touched = api_tokens::Entity::update_many()
        .col_expr(api_tokens::Column::LastUsedAt, now.into())
        .filter(api_tokens::Column::Id.eq(row.id))
        .filter(
            Condition::any()
                .add(api_tokens::Column::LastUsedAt.is_null())
                .add(api_tokens::Column::LastUsedAt.lt(stale)),
        )
        .exec(&app.db)
        .await;
    if let Err(e) = touched {
        tracing::warn!(error = %e, token = %row.id, "api token last_used_at not saved");
    }
}

/// The caller's team, and for a token its set and name.
#[utoipa::path(get, path = "/me", tag = "api-tokens",
    security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = Me), (status = 401, body = Error),
        (status = 429, body = Error)))]
async fn me(State(app): State<AppState>, a: Actor) -> Response {
    let set = match a.set_id() {
        Some(id) => match social_sets::Entity::find_by_id(id).one(&app.db).await {
            Ok(s) => s.map(|s| SetRef {
                id: s.id,
                name: s.name,
            }),
            Err(e) => return internal(e, "api token set"),
        },
        None => None,
    };
    Json(Me {
        team: Team {
            id: a.team.id,
            name: a.team.name.clone(),
            role: a.role.as_str().into(),
        },
        social_set: set,
        token_name: a.token.map(|t| t.name),
    })
    .into_response()
}

/// The current team's live API tokens, newest first, without their secrets. Owners and admins.
#[utoipa::path(get, path = "/api-tokens", tag = "api-tokens",
    responses((status = 200, body = [ApiToken]), (status = 401, body = Error),
        (status = 403, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let rows = api_tokens::Entity::find()
        .filter(api_tokens::Column::TeamId.eq(t.team.id))
        .filter(live())
        .order_by_desc(api_tokens::Column::CreatedAt)
        .order_by_desc(api_tokens::Column::Id)
        .all(&app.db)
        .await;
    match rows {
        Ok(rows) => Json(rows.into_iter().map(token_json).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e, "api token list"),
    }
}

/// Creates a token and answers its secret, once. Owners and admins, with a sign-in from the
/// last 10 minutes (else `403 reauth_required`). At most 20 live tokens per team.
#[utoipa::path(post, path = "/api-tokens", tag = "api-tokens", request_body = NewToken,
    responses((status = 201, body = NewTokenReply), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<NewToken>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    if !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let Some(name) = team::name(&body.name) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
    };
    let expires_at = match body.expires_in_days {
        None => None,
        Some(d) if (1..=MAX_EXPIRY_DAYS).contains(&d) => {
            Some((Utc::now() + Duration::days(d)).fixed_offset())
        }
        Some(_) => return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_expiry"),
    };
    let kind = if body.social_set_id.is_some() {
        "set"
    } else {
        "team"
    };
    let prefix = format!("sr_{kind}_live_");
    let secret = format!("{prefix}{}", &token::new()[..SECRET_CHARS]);
    let made = async {
        let txn = app.db.begin().await?;
        // The team row lock keeps concurrent creates under the cap.
        teams::Entity::find_by_id(t.team.id)
            .lock_exclusive()
            .one(&txn)
            .await?;
        if let Some(set) = body.social_set_id {
            // Shared lock: a concurrent set delete waits, then revokes this token too.
            let found = social_sets::Entity::find_by_id(set)
                .filter(social_sets::Column::TeamId.eq(t.team.id))
                .filter(social_sets::Column::DeletedAt.is_null())
                .lock_shared()
                .one(&txn)
                .await?;
            if found.is_none() {
                return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_set")));
            }
        }
        let count = api_tokens::Entity::find()
            .filter(api_tokens::Column::TeamId.eq(t.team.id))
            .filter(live())
            .count(&txn)
            .await?;
        if count >= MAX_LIVE_TOKENS {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_tokens")));
        }
        let row = api_tokens::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(t.team.id),
            social_set_id: Set(body.social_set_id),
            name: Set(name),
            token_hash: Set(token::hash(&secret)),
            prefix_hint: Set(secret[..prefix.len() + HINT_CHARS].to_owned()),
            created_by: Set(Some(t.current.0.id)),
            expires_at: Set(expires_at),
            ..Default::default()
        };
        let row = api_tokens::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await?;
        Ok::<_, sea_orm::DbErr>(Ok(row))
    };
    match made.await {
        Ok(Ok(row)) => {
            let body = NewTokenReply {
                token: token_json(row),
                secret,
            };
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Ok(Err(res)) => res,
        Err(e) => internal(e, "api token insert"),
    }
}

/// Revokes a token at once. Owners and admins.
#[utoipa::path(delete, path = "/api-tokens/{id}", tag = "api-tokens",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn revoke(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let done = api_tokens::Entity::update_many()
        .col_expr(
            api_tokens::Column::RevokedAt,
            Utc::now().fixed_offset().into(),
        )
        .filter(api_tokens::Column::Id.eq(id))
        .filter(api_tokens::Column::TeamId.eq(t.team.id))
        .filter(api_tokens::Column::RevokedAt.is_null())
        .exec(&app.db)
        .await;
    match done {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "api token revoke"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(me))
        .routes(routes!(list, create))
        .routes(routes!(revoke))
}
