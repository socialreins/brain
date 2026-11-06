//! Connect a LinkedIn profile through the shared OAuth flow (`oauth.rs`):
//! `GET /connections/linkedin/redirect`, then `GET /connect/linkedin/callback` (path registered
//! with LinkedIn, fixed) swaps the code for a token (about 60 days), reads the member's OpenID
//! profile and saves it as a `linkedin` account in that team's Default set. A refresh token, which
//! only partner apps get, is kept sealed in `meta`; without one the account shows as expired
//! after its 60 days and the member signs in again.
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::Set, ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait};
use uuid::Uuid;

use super::{
    connections::save_account,
    oauth::{self, Callback},
    teams::InTeam,
    waitlist::Reply,
};
use crate::{
    connectors::{bluesky::PostError, linkedin::Token},
    models::social_accounts,
    state::AppState,
};

const PROVIDER: &str = "linkedin";
/// Advisory lock key held by the instance refreshing tokens.
const REFRESH_LOCK: i64 = 0x004c_4b44_5245_4652;

/// `GET /connections/linkedin/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.linkedin.configured());
    oauth::start(&app, t, how, |uri, state| {
        app.linkedin.authorize_url(uri, state)
    })
    .await
}

/// The `meta` of an account: its refresh token sealed with `ENCRYPTION_KEY`, when it has one.
fn meta_of(app: &AppState, token: &Token) -> serde_json::Value {
    match &token.refresh_token {
        Some(r) => {
            let sealed = URL_SAFE_NO_PAD.encode(app.cipher.encrypt(r.as_bytes()));
            serde_json::json!({ "refresh_token": sealed })
        }
        None => serde_json::json!({}),
    }
}

/// `GET /connect/linkedin/callback`: LinkedIn sends the browser here with `code` and `state`, or
/// with `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.linkedin.configured());
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
    let fetched = async {
        let token = app
            .linkedin
            .exchange(&code, &oauth::redirect_uri(&app, PROVIDER))
            .await?;
        let me = app.linkedin.me(&token.access_token).await?;
        Ok::<_, PostError>((token, me))
    };
    let (token, me) = match fetched.await {
        Ok(found) => found,
        Err(PostError::Failed(why)) => {
            tracing::warn!(why, "linkedin connect failed");
            return fail("linkedin_failed");
        }
        Err(PostError::Reconnect | PostError::Denied) => {
            tracing::warn!("linkedin refused the new token");
            return fail("linkedin_failed");
        }
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let name = me.name.filter(|n| !n.trim().is_empty());
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(me.sub),
        handle: Set(None),
        display_name: Set(name),
        avatar_url: Set(me.picture),
        access_token: Set(app.cipher.encrypt(token.access_token.as_bytes())),
        token_expires_at: Set(Some(now + Duration::seconds(token.expires_in))),
        meta: Set(meta_of(&app, &token)),
        dm_access: Set(false),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    // A reconnect keeps the row and takes the new token and profile.
    let refresh = [
        social_accounts::Column::DisplayName,
        social_accounts::Column::AvatarUrl,
        social_accounts::Column::AccessToken,
        social_accounts::Column::TokenExpiresAt,
        social_accounts::Column::Meta,
        social_accounts::Column::UpdatedAt,
    ];
    if let Err(e) = save_account(&app.db, (team_id, new_id), row, refresh).await {
        tracing::error!(error = %e, "linkedin account save failed");
        return fail("internal");
    }
    oauth::landed(&app, PROVIDER, &[("connected", PROVIDER)])
}

/// Renews every LinkedIn token with 14 days or less left (or already expired) for accounts that
/// hold a refresh token. A token changed meanwhile (a reconnect) is left as it is. Returns how
/// many were renewed; a refused refresh is logged and the account waits for a reconnect. One
/// instance at a time (0 while another holds the lock).
pub async fn refresh_tokens(app: &AppState) -> Result<u64, DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            [REFRESH_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT id, access_token, meta->>'refresh_token' AS refresh FROM social_accounts \
         WHERE provider = $1 AND meta->>'refresh_token' IS NOT NULL \
         AND token_expires_at <= now() + interval '14 days'",
        [PROVIDER.into()],
    );
    let unseal = |s: &[u8]| {
        app.cipher
            .decrypt(s)
            .and_then(|b| String::from_utf8(b).ok())
    };
    let mut done = 0;
    for row in app.db.query_all_raw(stmt).await? {
        let (id, sealed, refresh): (Uuid, Vec<u8>, String) = (
            row.try_get("", "id")?,
            row.try_get("", "access_token")?,
            row.try_get("", "refresh")?,
        );
        let Some(old) = URL_SAFE_NO_PAD
            .decode(&refresh)
            .ok()
            .and_then(|b| unseal(&b))
        else {
            tracing::warn!(account = %id, "linkedin refresh token unreadable");
            continue;
        };
        let token = match app.linkedin.refresh(&old).await {
            Ok(t) => t,
            Err(PostError::Failed(why)) => {
                tracing::warn!(account = %id, why, "linkedin token refresh failed");
                continue;
            }
            // A missing permission says nothing about the token: no flag.
            Err(PostError::Denied) => {
                tracing::warn!(account = %id, "linkedin denied the token refresh");
                continue;
            }
            Err(PostError::Reconnect) => {
                tracing::warn!(account = %id, "linkedin refused the token refresh");
                crate::expiry::flag_reconnect(&app.db, id).await;
                continue;
            }
        };
        // LinkedIn may send a new refresh token; else the old one stays.
        let meta = match token.refresh_token {
            Some(_) => meta_of(app, &token),
            None => serde_json::json!({ "refresh_token": refresh }),
        };
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE social_accounts SET access_token = $2, meta = $5, \
             token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
             WHERE id = $1 AND access_token = $4",
            [
                id.into(),
                app.cipher.encrypt(token.access_token.as_bytes()).into(),
                (token.expires_in as f64).into(),
                sealed.into(),
                meta.into(),
            ],
        );
        done += app.db.execute_raw(stmt).await?.rows_affected();
    }
    lock.commit().await?;
    Ok(done)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/linkedin/redirect", get(start))
        .route("/connect/linkedin/callback", get(callback))
}
