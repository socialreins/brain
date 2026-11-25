//! Connect a Pinterest account through the shared OAuth flow (`oauth.rs`):
//! `GET /connections/pinterest/redirect`, then `GET /connect/pinterest/callback` (path registered
//! with Pinterest, fixed) swaps the code for an access token (30 days) and a refresh token (60
//! days, renewed with every refresh), reads the account the consent picked and saves it as a
//! `pinterest` account in that team's default set. The refresh token is kept sealed in `meta`;
//! `fresh_token` renews the access token under a lock on the account row whenever a call is
//! about to be made with one near its end, and a daily job renews the ones that would otherwise
//! sit unused until their refresh token ran out. The account's default board (the one a pin
//! goes to when the post names none) is `meta.default_board_id`, kept across a reconnect.
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use sea_orm::{
    sea_query, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait,
    QueryFilter, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body, FromWeb},
    connections::save_account,
    oauth::{self, Callback},
    teams::InTeam,
    waitlist::{error, Error, Reply},
};
use crate::{
    connectors::{
        bluesky::PostError,
        pinterest::{Board, Profile, Token},
    },
    models::{social_account_social_set as membership, social_accounts},
    state::AppState,
};

const PROVIDER: &str = "pinterest";
/// An access token with less than this left is renewed before a call. A pin with a video can take
/// several minutes, so this leaves room for it.
const RENEW_WITHIN: Duration = Duration::hours(1);
/// The daily job renews a token with this much left or less, so an account nobody posts to
/// still renews well inside its refresh token's 60 days.
const JOB_RENEW_WITHIN: Duration = Duration::days(10);
const REFRESH_LOCK: i64 = 0x0050_494e_5452_4546;

/// `GET /connections/pinterest/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.pinterest.configured());
    oauth::start(&app, t, how, |uri, state| {
        app.pinterest.authorize_url(uri, state)
    })
    .await
}

fn seal(app: &AppState, refresh_token: &str) -> String {
    URL_SAFE_NO_PAD.encode(app.cipher.encrypt(refresh_token.as_bytes()))
}

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn db_failed(e: DbErr) -> PostError {
    tracing::error!(error = %e, "pinterest token save failed");
    PostError::Failed("The Pinterest token could not be renewed. Try again.".into())
}

/// The access token to call Pinterest with: the stored one, or, when it ends within an hour (or
/// has no end recorded), a new one (see `renew`). A refresh token Pinterest refuses, or none, is
/// `Reconnect`.
pub async fn fresh_token(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<String, PostError> {
    renew(app, account.id, RENEW_WITHIN).await.map(|r| r.0)
}

/// Locks the account row and reads it again rather than trusting a model that may be older than
/// a renewal, then renews its token unless it already ends later than `within` from now. Returns
/// the token and whether Pinterest was called. The new access token and the refresh token
/// Pinterest sends back (else the one spent) are saved in the transaction that holds the lock, so
/// two calls renewing at once spend the refresh token once.
async fn renew(app: &AppState, id: Uuid, within: Duration) -> Result<(String, bool), PostError> {
    let txn = app.db.begin().await.map_err(db_failed)?;
    let row = txn
        .query_one_raw(stmt(
            "SELECT access_token, token_expires_at, meta FROM social_accounts \
             WHERE id = $1 FOR UPDATE",
            vec![id.into()],
        ))
        .await
        .map_err(db_failed)?;
    // The account was disconnected meanwhile.
    let Some(row) = row else {
        return Err(PostError::Reconnect);
    };
    let sealed: Vec<u8> = row.try_get("", "access_token").map_err(db_failed)?;
    let expires: Option<DateTime<Utc>> = row.try_get("", "token_expires_at").map_err(db_failed)?;
    let meta: serde_json::Value = row.try_get("", "meta").map_err(db_failed)?;
    let live = expires.is_some_and(|t| t > Utc::now() + within);
    let stored = app
        .cipher
        .decrypt(&sealed)
        .and_then(|b| String::from_utf8(b).ok());
    if let (true, Some(token)) = (live, stored) {
        return Ok((token, false));
    }
    let Some(refresh) = meta["refresh_token"]
        .as_str()
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .and_then(|b| app.cipher.decrypt(&b))
        .and_then(|b| String::from_utf8(b).ok())
    else {
        return Err(PostError::Reconnect);
    };
    let token = app.pinterest.refresh(&refresh).await?;
    let next = token.refresh_token.clone().unwrap_or(refresh);
    txn.execute_raw(stmt(
        "UPDATE social_accounts SET access_token = $2, \
         meta = meta || jsonb_build_object('refresh_token', $4::text), \
         token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
         WHERE id = $1",
        vec![
            id.into(),
            app.cipher.encrypt(token.access_token.as_bytes()).into(),
            (token.expires_in as f64).into(),
            seal(app, &next).into(),
        ],
    ))
    .await
    .map_err(db_failed)?;
    txn.commit().await.map_err(db_failed)?;
    Ok((token.access_token, true))
}

/// Renews every Pinterest token with 10 days or less left (or none recorded) for accounts not
/// flagged for a reconnect. Returns how many were renewed; a refused refresh flags the account,
/// another failure is logged and retried at the next run. One instance at a time (0 while
/// another holds the lock).
pub async fn refresh_tokens(app: &AppState) -> Result<u64, DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(stmt(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![REFRESH_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let rows = app
        .db
        .query_all_raw(stmt(
            "SELECT id FROM social_accounts \
             WHERE provider = $1 AND meta->>'refresh_token' IS NOT NULL \
             AND COALESCE(meta->>'reconnect', 'false') <> 'true' \
             AND (token_expires_at IS NULL \
                  OR token_expires_at <= now() + make_interval(secs => $2))",
            vec![
                PROVIDER.into(),
                (JOB_RENEW_WITHIN.num_seconds() as f64).into(),
            ],
        ))
        .await?;
    let mut done = 0;
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        match renew(app, id, JOB_RENEW_WITHIN).await {
            Ok((_, true)) => done += 1,
            Ok((_, false)) | Err(PostError::Denied) => {}
            Err(PostError::Reconnect) => {
                tracing::warn!(account = %id, "pinterest refused the token refresh");
                crate::expiry::flag_reconnect(&app.db, id).await;
            }
            Err(PostError::Failed(why)) => {
                tracing::warn!(account = %id, why, "pinterest token refresh failed");
            }
        }
    }
    lock.commit().await?;
    Ok(done)
}

/// What the callback needs of Pinterest: the tokens and the account, or the error code to land with.
async fn fetch(app: &AppState, code: &str) -> Result<(Token, String, Profile), &'static str> {
    let exchanged = app
        .pinterest
        .exchange(code, &oauth::redirect_uri(app, PROVIDER))
        .await;
    let token = match exchanged {
        Ok(t) => t,
        Err(e) => {
            log(&e, "token swap");
            return Err("pinterest_failed");
        }
    };
    // The consent screen may grant fewer scopes; without all of them, posting would fail
    // later, so nothing is saved.
    if !token.has_all_scopes() {
        tracing::warn!("pinterest consent lacked a needed scope");
        return Err("missing_scopes");
    }
    // The code swap always brings one; without it the account could not be renewed.
    let Some(refresh) = token.refresh_token.clone() else {
        tracing::warn!("pinterest sent no refresh token");
        return Err("pinterest_failed");
    };
    match app.pinterest.profile(&token.access_token).await {
        Ok(me) => Ok((token, refresh, me)),
        Err(e) => {
            log(&e, "profile");
            Err("pinterest_failed")
        }
    }
}

fn log(e: &PostError, step: &str) {
    match e {
        PostError::Failed(why) => tracing::warn!(why, step, "pinterest connect failed"),
        PostError::Reconnect | PostError::Denied => {
            tracing::warn!(step, "pinterest refused the connect")
        }
    }
}

/// `GET /connect/pinterest/callback`: Pinterest sends the browser here with `code` and `state`,
/// or with `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.pinterest.configured());
    let (team_id, user_id, code) = match oauth::finish(&app, &headers, q, how).await {
        Ok(found) => found,
        Err(landing) => return *landing,
    };
    let fail = |code| {
        oauth::landed(
            &app,
            PROVIDER,
            &[("connect_error", code), ("provider", PROVIDER)],
        )
    };
    let (token, refresh, me) = match fetch(&app, &code).await {
        Ok(found) => found,
        Err(code) => return fail(code),
    };
    let mut meta = serde_json::json!({ "refresh_token": seal(&app, &refresh) });
    // A reconnect keeps the default board the user chose.
    let kept = match app
        .db
        .query_one_raw(stmt(
            "SELECT meta->>'default_board_id' AS id, meta->>'default_board_name' AS name \
             FROM social_accounts WHERE team_id = $1 AND provider = $2 AND external_id = $3",
            vec![team_id.into(), PROVIDER.into(), me.id.clone().into()],
        ))
        .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::error!(error = %e, "pinterest default board read failed");
            return fail("internal");
        }
    };
    if let Some(row) = kept {
        let id: Option<String> = row.try_get("", "id").ok().flatten();
        let name: Option<String> = row.try_get("", "name").ok().flatten();
        if let Some(id) = id {
            meta["default_board_id"] = id.into();
            meta["default_board_name"] = name.unwrap_or_default().into();
        }
    }
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(me.id),
        handle: Set(me.username),
        display_name: Set(me.display_name),
        avatar_url: Set(me.avatar_url),
        access_token: Set(app.cipher.encrypt(token.access_token.as_bytes())),
        token_expires_at: Set(Some(now + Duration::seconds(token.expires_in))),
        meta: Set(meta),
        dm_access: Set(false),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    // A reconnect keeps the row and takes the new tokens and profile, and clears the flag.
    let refresh = [
        social_accounts::Column::Handle,
        social_accounts::Column::DisplayName,
        social_accounts::Column::AvatarUrl,
        social_accounts::Column::AccessToken,
        social_accounts::Column::TokenExpiresAt,
        social_accounts::Column::Meta,
        social_accounts::Column::UpdatedAt,
    ];
    if let Err(e) = save_account(&app, (team_id, new_id), row, refresh).await {
        if crate::billing::is_account_limit(&e) {
            return fail("plan_limit");
        }
        tracing::error!(error = %e, "pinterest account save failed");
        return fail("internal");
    }
    oauth::landed(&app, PROVIDER, &[("connected", PROVIDER)])
}

/// An account's boards, for the post editor.
#[derive(Serialize, ToSchema)]
pub struct Boards {
    pub boards: Vec<Board>,
    /// The board a pin goes to when the post names none; absent when none is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_board_id: Option<String>,
}

/// The team's Pinterest account `id`, within a set token's own set.
async fn find(
    app: &AppState,
    (team, set): (Uuid, Option<Uuid>),
    id: Uuid,
) -> Result<Option<social_accounts::Model>, DbErr> {
    let mut sel = social_accounts::Entity::find_by_id(id)
        .filter(social_accounts::Column::TeamId.eq(team))
        .filter(social_accounts::Column::Provider.eq(PROVIDER));
    if let Some(own) = set {
        let in_set = sea_query::Query::select()
            .column(membership::Column::SocialAccountId)
            .from(membership::Entity)
            .and_where(membership::Column::SocialSetId.eq(own))
            .to_owned();
        sel = sel.filter(social_accounts::Column::Id.in_subquery(in_set));
    }
    sel.one(&app.db).await
}

fn failure(e: PostError) -> Response {
    match e {
        PostError::Reconnect | PostError::Denied => {
            error(StatusCode::CONFLICT, "reconnect_required").into_response()
        }
        PostError::Failed(why) => {
            tracing::warn!(why, "pinterest call failed");
            error(StatusCode::BAD_GATEWAY, "pinterest_failed").into_response()
        }
    }
}

/// The boards of a connected Pinterest account (up to 1000, the ones its token may read: secret
/// boards are not listed) and its default board, read live. Any member; a set token only for an
/// account of its set.
#[utoipa::path(get, path = "/connections/{id}/pinterest/boards", tag = "connections",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = Boards), (status = 401, body = Error),
        (status = 404, body = Error), (status = 409, body = Error), (status = 502, body = Error)))]
async fn boards(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    let account = match find(&app, (a.team.id, a.set_id()), id).await {
        Ok(Some(account)) => account,
        Ok(None) => return error(StatusCode::NOT_FOUND, "not_found").into_response(),
        Err(e) => return internal(e, "pinterest account read"),
    };
    let listed = match fresh_token(&app, &account).await {
        Ok(token) => app.pinterest.boards(&token).await,
        Err(e) => Err(e),
    };
    match listed {
        Ok(boards) => Json(Boards {
            boards,
            default_board_id: account.meta["default_board_id"].as_str().map(str::to_owned),
        })
        .into_response(),
        Err(e) => failure(e),
    }
}

#[derive(Deserialize, ToSchema)]
pub struct DefaultBoard {
    /// One of the account's boards, or `null` to have none.
    pub default_board_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DefaultBoardSaved {
    pub default_board_id: Option<String>,
    pub default_board_name: Option<String>,
}

/// Sets the board a pin goes to when its post names none (`null` removes it). The board must be
/// one of the account's. Owners and admins.
#[utoipa::path(put, path = "/connections/{id}/pinterest", tag = "connections",
    request_body = DefaultBoard, params(("id" = Uuid, Path)),
    responses((status = 200, body = DefaultBoardSaved), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 502, body = Error)))]
async fn set_default(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(body): Body<DefaultBoard>,
) -> Response {
    if !t.role.manages() {
        return error(StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let account = match find(&app, (t.team.id, None), id).await {
        Ok(Some(account)) => account,
        Ok(None) => return error(StatusCode::NOT_FOUND, "not_found").into_response(),
        Err(e) => return internal(e, "pinterest account read"),
    };
    let chosen = match body.default_board_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(wanted) => {
            let found = match fresh_token(&app, &account).await {
                Ok(token) => app.pinterest.board(&token, wanted).await,
                Err(e) => Err(e),
            };
            match found {
                Ok(Some(board)) => Some(board),
                Ok(None) => {
                    return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_board").into_response()
                }
                Err(e) => return failure(e),
            }
        }
    };
    let saved = match &chosen {
        Some(b) => {
            app.db
                .execute_raw(stmt(
                    "UPDATE social_accounts SET meta = meta || jsonb_build_object(\
                     'default_board_id', $2::text, 'default_board_name', $3::text) WHERE id = $1",
                    vec![id.into(), b.id.clone().into(), b.name.clone().into()],
                ))
                .await
        }
        None => {
            app.db
                .execute_raw(stmt(
                    "UPDATE social_accounts SET meta = meta - 'default_board_id' \
                     - 'default_board_name' WHERE id = $1",
                    vec![id.into()],
                ))
                .await
        }
    };
    match saved {
        Ok(_) => Json(DefaultBoardSaved {
            default_board_id: chosen.as_ref().map(|b| b.id.clone()),
            default_board_name: chosen.map(|b| b.name),
        })
        .into_response(),
        Err(e) => internal(e, "pinterest default board"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(boards, set_default))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/pinterest/redirect", get(start))
        .route("/connect/pinterest/callback", get(callback))
}
