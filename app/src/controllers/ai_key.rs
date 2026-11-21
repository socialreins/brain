//! A team's own AI provider key (BYOK). AI steps in automations and AI replies use it before
//! any credit is spent (`key_of`). Owners and admins set, read and delete it; it is sealed
//! with the app cipher and never answered again, only its last 4 characters.
use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use chrono::{DateTime, FixedOffset};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::state::AppState;

#[derive(Serialize, ToSchema)]
pub struct AiKey {
    /// `anthropic` or `openai`.
    pub provider: String,
    /// The last 4 characters of the key.
    pub last4: String,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Deserialize, ToSchema)]
pub struct AiKeyInput {
    /// `anthropic` (key starts `sk-ant-`) or `openai` (key starts `sk-`).
    pub provider: String,
    /// The provider's API key, 20 to 300 visible ASCII characters.
    pub key: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Whether `key` looks like a `provider` key. Only a shape check: the key is not tried.
fn plausible(provider: &str, key: &str) -> bool {
    let prefix = match provider {
        "anthropic" => "sk-ant-",
        "openai" => "sk-",
        _ => return false,
    };
    (20..=300).contains(&key.len())
        && key.starts_with(prefix)
        && key.bytes().all(|b| b.is_ascii_graphic())
}

/// The team's key for the AI steps: the provider and the key in clear. `None` when the team
/// has none (the call then uses credits). The caller must never log or answer the key.
pub async fn key_of(app: &AppState, team: Uuid) -> Result<Option<(String, String)>, DbErr> {
    let row = app
        .db
        .query_one_raw(stmt(
            "SELECT provider, key FROM team_ai_keys WHERE team_id = $1",
            vec![team.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let provider: String = row.try_get("", "provider")?;
    let sealed: Vec<u8> = row.try_get("", "key")?;
    let key = app
        .cipher
        .decrypt(&sealed)
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| DbErr::Custom("ai key not readable".into()))?;
    Ok(Some((provider, key)))
}

/// The team's AI key, without the key itself; `404` when it has none. Owners and admins.
#[utoipa::path(get, path = "/team/ai-key", tag = "billing",
    responses((status = 200, body = AiKey), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let found = app
        .db
        .query_one_raw(stmt(
            "SELECT provider, last4, updated_at FROM team_ai_keys WHERE team_id = $1",
            vec![t.team.id.into()],
        ))
        .await;
    let read = |r: sea_orm::QueryResult| -> Result<AiKey, DbErr> {
        Ok(AiKey {
            provider: r.try_get("", "provider")?,
            last4: r.try_get("", "last4")?,
            updated_at: r.try_get("", "updated_at")?,
        })
    };
    match found.and_then(|r| r.map(read).transpose()) {
        Ok(Some(k)) => Json(k).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "ai key read"),
    }
}

/// Sets or replaces the team's AI key. `422 invalid_key` when the provider or the key's shape
/// is wrong. Owners and admins, with a sign-in from the last 10 minutes (else
/// `403 reauth_required`).
#[utoipa::path(put, path = "/team/ai-key", tag = "billing", request_body = AiKeyInput,
    responses((status = 200, body = AiKey), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn set(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<AiKeyInput>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    if !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let (provider, key) = (body.provider.trim(), body.key.trim());
    if !plausible(provider, key) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_key");
    }
    let last4 = &key[key.len() - 4..];
    let saved = app
        .db
        .query_one_raw(stmt(
            "INSERT INTO team_ai_keys (team_id, provider, key, last4) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (team_id) DO UPDATE SET provider = EXCLUDED.provider, \
             key = EXCLUDED.key, last4 = EXCLUDED.last4, updated_at = now() \
             RETURNING updated_at",
            vec![
                t.team.id.into(),
                provider.into(),
                app.cipher.encrypt(key.as_bytes()).into(),
                last4.into(),
            ],
        ))
        .await;
    match saved.map(|r| r.map(|r| r.try_get::<DateTime<FixedOffset>>("", "updated_at"))) {
        Ok(Some(Ok(updated_at))) => Json(AiKey {
            provider: provider.into(),
            last4: last4.into(),
            updated_at,
        })
        .into_response(),
        Ok(_) => internal("no row", "ai key upsert"),
        Err(e) => internal(e, "ai key upsert"),
    }
}

/// Deletes the team's AI key, `204`: AI steps then use credits. Owners and admins.
#[utoipa::path(delete, path = "/team/ai-key", tag = "billing",
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(_: FromWeb, State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let done = app
        .db
        .execute_raw(stmt(
            "DELETE FROM team_ai_keys WHERE team_id = $1",
            vec![t.team.id.into()],
        ))
        .await;
    match done {
        Ok(r) if r.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "ai key delete"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(show, set, destroy))
}

#[cfg(test)]
mod tests {
    use super::plausible;

    #[test]
    fn key_shapes() {
        let a = format!("sk-ant-{}", "x".repeat(30));
        assert!(plausible("anthropic", &a));
        assert!(plausible("openai", "sk-proj-abcdefghijklmnopqrstuvwxyz"));
        assert!(!plausible("openai", "abcdefghijklmnopqrstuvwxyz"));
        assert!(!plausible("anthropic", "sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!plausible("gemini", &a));
        assert!(!plausible("openai", "sk-short"));
        assert!(!plausible("openai", &format!("sk-{}", "é".repeat(20))));
        assert!(!plausible("openai", "sk-abcdefghij klmnopqrstuvwxyz"));
        assert!(!plausible("openai", &format!("sk-{}", "a".repeat(300))));
    }
}
