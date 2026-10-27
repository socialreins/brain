//! Passkeys (WebAuthn). The ceremonies run in the web app's pages, which post here: each
//! `start` stores the ceremony state server side for 5 minutes and returns its `challenge_id`
//! with the options for `navigator.credentials`; the matching `finish` uses it once.
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::Set, ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, SqlErr};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use socialreins_core::password;
use uuid::Uuid;
use webauthn_rs::prelude::{
    DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential,
};

use super::{
    auth::{internal, start_session, user_json, Body, CurrentUser, FromWeb},
    waitlist::error,
};
use crate::{
    models::{passkey_challenges, passkeys, users},
    rate_limit::ClientIp,
    state::AppState,
};

const CHALLENGE_MINUTES: i64 = 5;
const MAX_NAME_CHARS: usize = 64;

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

/// Stores ceremony `state` and returns its id. Expired rows are swept on the way.
async fn save_challenge(
    app: &AppState,
    user_id: Option<Uuid>,
    state: &impl Serialize,
) -> Result<Uuid, String> {
    let now = Utc::now().fixed_offset();
    passkey_challenges::Entity::delete_many()
        .filter(passkey_challenges::Column::ExpiresAt.lt(now))
        .exec(&app.db)
        .await
        .map_err(|e| e.to_string())?;
    let id = Uuid::now_v7();
    let row = passkey_challenges::ActiveModel {
        id: Set(id),
        user_id: Set(user_id),
        state: Set(serde_json::to_value(state).map_err(|e| e.to_string())?),
        expires_at: Set(now + Duration::minutes(CHALLENGE_MINUTES)),
    };
    passkey_challenges::Entity::insert(row)
        .exec_without_returning(&app.db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Deletes the challenge and returns its state when it was live and belongs to `user_id`
/// (`None` for a sign-in). Whoever deletes the row owns it, so a replay finds nothing.
async fn take_challenge<T: DeserializeOwned>(
    app: &AppState,
    id: Uuid,
    user_id: Option<Uuid>,
) -> Result<Option<T>, DbErr> {
    let Some(row) = passkey_challenges::Entity::delete_by_id(id)
        .exec_with_returning(&app.db)
        .await?
    else {
        return Ok(None);
    };
    let live = row.expires_at > Utc::now() && row.user_id == user_id;
    Ok(live
        .then(|| serde_json::from_value(row.state).ok())
        .flatten())
}

#[derive(Deserialize)]
struct Confirm {
    /// Lets a user with a password confirm it instead of signing in again.
    password: Option<String>,
}

/// `POST /auth/passkeys/register/start`: needs a session from the last 10 minutes or the
/// account password, so a stolen old cookie cannot add a passkey.
async fn register_start(
    _: FromWeb,
    State(app): State<AppState>,
    current: CurrentUser,
    Body(body): Body<Confirm>,
) -> Response {
    let user = &current.0;
    let confirmed = match (body.password, &user.password_hash) {
        (Some(given), Some(phc)) => {
            if !app.login_email_limiter.hit(user.email.clone()) {
                return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
            }
            let phc = phc.clone();
            tokio::task::spawn_blocking(move || password::verify(&given, &phc))
                .await
                .unwrap_or(false)
        }
        _ => false,
    };
    if !(confirmed || current.recent()) {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let existing = match own_passkeys(&app, user.id).await {
        Ok(rows) => rows,
        Err(e) => return internal(e, "passkey list"),
    };
    let exclude = existing
        .iter()
        .map(|p| p.credential_id.clone().into())
        .collect();
    let started =
        app.webauthn
            .start_passkey_registration(user.id, &user.email, &user.name, Some(exclude));
    let (mut options, state) = match started {
        Ok((options, state)) => (json!(options), state),
        Err(e) => return internal(e, "passkey registration start"),
    };
    // Ask for a discoverable credential so `login/start` needs no email. webauthn-rs leaves it
    // optional and does not check it on finish.
    options["publicKey"]["authenticatorSelection"]["residentKey"] = json!("required");
    options["publicKey"]["authenticatorSelection"]["requireResidentKey"] = json!(true);
    match save_challenge(&app, Some(user.id), &state).await {
        Ok(id) => Json(json!({"challenge_id": id, "options": options})).into_response(),
        Err(e) => internal(e, "passkey challenge insert"),
    }
}

#[derive(Deserialize)]
struct RegisterFinish {
    challenge_id: Uuid,
    credential: RegisterPublicKeyCredential,
    name: Option<String>,
}

/// `POST /auth/passkeys/register/finish`: verifies the new credential and stores it.
async fn register_finish(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    Body(body): Body<RegisterFinish>,
) -> Response {
    let name = body.name.as_deref().map(str::trim).unwrap_or_default();
    let name = if name.is_empty() { "Passkey" } else { name };
    if name.chars().count() > MAX_NAME_CHARS {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
    }
    let state: PasskeyRegistration =
        match take_challenge(&app, body.challenge_id, Some(user.id)).await {
            Ok(Some(state)) => state,
            Ok(None) => return reply(StatusCode::BAD_REQUEST, "invalid_challenge"),
            Err(e) => return internal(e, "passkey challenge delete"),
        };
    let passkey = match app
        .webauthn
        .finish_passkey_registration(&body.credential, &state)
    {
        Ok(passkey) => passkey,
        Err(e) => {
            tracing::info!(error = %e, "passkey registration refused");
            return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_passkey");
        }
    };
    let Ok(credential) = serde_json::to_value(&passkey) else {
        return internal("serialize failed", "passkey serialize");
    };
    let id = Uuid::now_v7();
    let row = passkeys::ActiveModel {
        id: Set(id),
        user_id: Set(user.id),
        credential_id: Set(passkey.cred_id().to_vec()),
        credential: Set(credential),
        name: Set(name.to_owned()),
        ..Default::default()
    };
    match passkeys::Entity::insert(row)
        .exec_with_returning(&app.db)
        .await
    {
        Ok(row) => (StatusCode::CREATED, Json(listed(&row))).into_response(),
        Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
            reply(StatusCode::CONFLICT, "passkey_exists")
        }
        Err(e) => internal(e, "passkey insert"),
    }
}

/// `POST /auth/passkeys/login/start`: options for a discoverable credential, no email needed.
async fn login_start(_: FromWeb, State(app): State<AppState>, ClientIp(ip): ClientIp) -> Response {
    if !app.login_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let (mut options, state) = match app.webauthn.start_discoverable_authentication() {
        Ok(started) => started,
        Err(e) => return internal(e, "passkey login start"),
    };
    // webauthn-rs asks for conditional (autofill) UI; leave that choice to the web page.
    options.mediation = None;
    match save_challenge(&app, None, &state).await {
        Ok(id) => Json(json!({"challenge_id": id, "options": options})).into_response(),
        Err(e) => internal(e, "passkey challenge insert"),
    }
}

#[derive(Deserialize)]
struct LoginFinish {
    challenge_id: Uuid,
    credential: PublicKeyCredential,
}

/// `POST /auth/passkeys/login/finish`: verifies the assertion and starts a session. Every
/// failure past the challenge is the same `401 {"error":"invalid_passkey"}`.
async fn login_finish(
    _: FromWeb,
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Body(body): Body<LoginFinish>,
) -> Response {
    if !app.login_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let state: DiscoverableAuthentication =
        match take_challenge(&app, body.challenge_id, None).await {
            Ok(Some(state)) => state,
            Ok(None) => return reply(StatusCode::BAD_REQUEST, "invalid_challenge"),
            Err(e) => return internal(e, "passkey challenge delete"),
        };
    let refused = || reply(StatusCode::UNAUTHORIZED, "invalid_passkey");
    let Ok((user_id, cred_id)) = app
        .webauthn
        .identify_discoverable_authentication(&body.credential)
    else {
        return refused();
    };
    let row = match passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id))
        .filter(passkeys::Column::CredentialId.eq(cred_id.to_vec()))
        .one(&app.db)
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return refused(),
        Err(e) => return internal(e, "passkey lookup"),
    };
    let Ok(mut passkey) = serde_json::from_value::<Passkey>(row.credential.clone()) else {
        return internal("stored passkey unreadable", "passkey decode");
    };
    let result = match app.webauthn.finish_discoverable_authentication(
        &body.credential,
        state,
        &[DiscoverableKey::from(&passkey)],
    ) {
        Ok(result) => result,
        Err(e) => {
            tracing::info!(error = %e, "passkey sign-in refused");
            return refused();
        }
    };
    let mut update = passkeys::ActiveModel {
        id: Set(row.id),
        last_used_at: Set(Some(Utc::now().fixed_offset())),
        ..Default::default()
    };
    // Stores the new signature counter and backup state.
    if passkey.update_credential(&result) == Some(true) {
        match serde_json::to_value(&passkey) {
            Ok(v) => update.credential = Set(v),
            Err(e) => return internal(e, "passkey serialize"),
        }
    }
    if let Err(e) = passkeys::Entity::update(update).exec(&app.db).await {
        return internal(e, "passkey update");
    }
    let user = match users::Entity::find_by_id(user_id).one(&app.db).await {
        Ok(Some(user)) => user,
        Ok(None) => return refused(),
        Err(e) => return internal(e, "user lookup"),
    };
    match start_session(&app, user.id).await {
        Ok(cookie) => ([(header::SET_COOKIE, cookie)], user_json(&user)).into_response(),
        Err(e) => internal(e, "session insert"),
    }
}

async fn own_passkeys(app: &AppState, user_id: Uuid) -> Result<Vec<passkeys::Model>, DbErr> {
    passkeys::Entity::find()
        .filter(passkeys::Column::UserId.eq(user_id))
        .order_by_asc(passkeys::Column::CreatedAt)
        .all(&app.db)
        .await
}

fn listed(p: &passkeys::Model) -> serde_json::Value {
    json!({
        "id": p.id,
        "name": p.name,
        "created_at": p.created_at,
        "last_used_at": p.last_used_at,
    })
}

/// `GET /auth/passkeys`: the signed-in user's passkeys, oldest first.
async fn list(State(app): State<AppState>, CurrentUser(user, _): CurrentUser) -> Response {
    match own_passkeys(&app, user.id).await {
        Ok(rows) => Json(rows.iter().map(listed).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e, "passkey list"),
    }
}

/// `DELETE /auth/passkeys/{id}`: `204`, or `404` when it is not one of the user's passkeys.
async fn remove(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    Path(id): Path<Uuid>,
) -> Response {
    let deleted = passkeys::Entity::delete_many()
        .filter(passkeys::Column::Id.eq(id))
        .filter(passkeys::Column::UserId.eq(user.id))
        .exec(&app.db)
        .await;
    match deleted {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "passkey delete"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/passkeys", get(list))
        .route("/auth/passkeys/{id}", delete(remove))
        .route("/auth/passkeys/register/start", post(register_start))
        .route("/auth/passkeys/register/finish", post(register_finish))
        .route("/auth/passkeys/login/start", post(login_start))
        .route("/auth/passkeys/login/finish", post(login_finish))
}
