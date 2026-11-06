//! Connect a Threads profile through the shared OAuth flow (`oauth.rs`):
//! `GET /connections/threads/redirect`, then `GET /connect/threads/callback` (path registered with
//! Threads, fixed) swaps the code for a 60-day token, reads the profile and saves it as a
//! `threads` account in that team's Default set.
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::Set, ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait};
use uuid::Uuid;

use super::{
    connections::save_account,
    oauth::{self, Callback},
    teams::InTeam,
    waitlist::Reply,
};
use crate::{connectors::bluesky::PostError, models::social_accounts, state::AppState};

const PROVIDER: &str = "threads";
/// Advisory lock key held by the instance refreshing tokens.
const REFRESH_LOCK: i64 = 0x0054_4852_4541_4453;

/// `GET /connections/threads/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.threads.configured());
    oauth::start(&app, t, how, |uri, state| {
        app.threads.authorize_url(uri, state)
    })
    .await
}

/// `GET /connect/threads/callback`: Threads sends the browser here with `code` and `state`, or
/// with `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.threads.configured());
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
            .threads
            .exchange(&code, &oauth::redirect_uri(&app, PROVIDER))
            .await?;
        let me = app.threads.me(&token.access_token).await?;
        Ok::<_, PostError>((token, me))
    };
    let (token, me) = match fetched.await {
        Ok(found) => found,
        Err(PostError::Failed(why)) => {
            tracing::warn!(why, "threads connect failed");
            return fail("threads_failed");
        }
        Err(PostError::Reconnect) => {
            tracing::warn!("threads refused the new token");
            return fail("threads_failed");
        }
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let name = me.name.filter(|n| !n.trim().is_empty());
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(me.id),
        handle: Set(me.username.clone()),
        display_name: Set(name.or(me.username)),
        avatar_url: Set(me.threads_profile_picture_url),
        access_token: Set(app.cipher.encrypt(token.access_token.as_bytes())),
        token_expires_at: Set(Some(now + Duration::seconds(token.expires_in))),
        meta: Set(serde_json::json!({})),
        dm_access: Set(false),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    // A reconnect keeps the row and takes the new token and profile.
    let refresh = [
        social_accounts::Column::Handle,
        social_accounts::Column::DisplayName,
        social_accounts::Column::AvatarUrl,
        social_accounts::Column::AccessToken,
        social_accounts::Column::TokenExpiresAt,
        social_accounts::Column::UpdatedAt,
    ];
    if let Err(e) = save_account(&app.db, (team_id, new_id), row, refresh).await {
        tracing::error!(error = %e, "threads account save failed");
        return fail("internal");
    }
    oauth::landed(&app, PROVIDER, &[("connected", PROVIDER)])
}

/// Extends every Threads token with 30 days or less left and not yet expired by another 60
/// days. A token changed meanwhile (a reconnect) is left as it is. Returns how many were
/// extended; a refused refresh is logged and the account waits for a reconnect. One instance at
/// a time (0 while another holds the lock).
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
        "SELECT id, access_token FROM social_accounts WHERE provider = $1 \
         AND token_expires_at > now() AND token_expires_at <= now() + interval '30 days'",
        [PROVIDER.into()],
    );
    let mut done = 0;
    for row in app.db.query_all_raw(stmt).await? {
        let (id, sealed): (Uuid, Vec<u8>) =
            (row.try_get("", "id")?, row.try_get("", "access_token")?);
        let Some(old) = app
            .cipher
            .decrypt(&sealed)
            .and_then(|b| String::from_utf8(b).ok())
        else {
            tracing::warn!(account = %id, "threads token unreadable");
            continue;
        };
        let token = match app.threads.refresh(&old).await {
            Ok(t) => t,
            Err(PostError::Failed(why)) => {
                tracing::warn!(account = %id, why, "threads token refresh failed");
                continue;
            }
            Err(PostError::Reconnect) => {
                tracing::warn!(account = %id, "threads refused the token refresh");
                crate::expiry::flag_reconnect(&app.db, id).await;
                continue;
            }
        };
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE social_accounts SET access_token = $2, \
             token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
             WHERE id = $1 AND access_token = $4",
            [
                id.into(),
                app.cipher.encrypt(token.access_token.as_bytes()).into(),
                (token.expires_in as f64).into(),
                sealed.into(),
            ],
        );
        done += app.db.execute_raw(stmt).await?.rows_affected();
    }
    lock.commit().await?;
    Ok(done)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/threads/redirect", get(start))
        .route("/connect/threads/callback", get(callback))
}
