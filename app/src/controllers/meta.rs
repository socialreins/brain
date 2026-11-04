//! Connect Facebook Pages and the Instagram professional accounts linked to them, with one Meta
//! login through the shared OAuth flow (`oauth.rs`): `GET /connections/meta/redirect`, then
//! `GET /connect/meta/callback` (path registered with Meta, fixed) swaps the code for a
//! long-lived user token and reads the granted Pages. Accounts the team already has get their
//! Page token at once. When anything is left to choose, the Pages (with their tokens) are kept
//! sealed for 15 minutes and the browser lands on `?pick=meta`; `/v1/connections/meta/pick`
//! then lists the choices (ids, names and pictures, never tokens), saves the picked ones, or
//! drops the pick. Else it lands on `?connected=meta`.
use std::collections::HashSet;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::Utc;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter,
    QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    connections::{save_account, Connection},
    oauth::{self, Callback},
    teams::InTeam,
    waitlist::{error, Error, Reply},
};
use crate::{
    connectors::{
        bluesky::PostError,
        meta::{IgAccount, Page},
    },
    models::social_accounts,
    state::AppState,
};

const PROVIDER: &str = "meta";

/// One Page or Instagram account to choose.
#[derive(Serialize, ToSchema)]
pub struct Choice {
    /// The Page's or Instagram account's id.
    pub id: String,
    /// The Page's name, or the Instagram username.
    pub name: String,
    pub picture_url: Option<String>,
}

/// What a Meta login granted that the team does not have yet.
#[derive(Serialize, ToSchema)]
pub struct Choices {
    pub facebook: Vec<Choice>,
    pub instagram: Vec<Choice>,
}

/// The ids to connect, from `GET /connections/meta/pick`.
#[derive(Deserialize, ToSchema)]
pub struct Picked {
    #[serde(default)]
    pub facebook: Vec<String>,
    #[serde(default)]
    pub instagram: Vec<String>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

/// `GET /connections/meta/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.meta.configured());
    oauth::start(&app, t, how, |uri, state| {
        app.meta.authorize_url(uri, state)
    })
    .await
}

/// The team's Facebook and Instagram accounts, as `(provider, external id)`.
async fn held(
    db: &impl ConnectionTrait,
    team_id: Uuid,
) -> Result<HashSet<(String, String)>, DbErr> {
    let rows: Vec<(String, String)> = social_accounts::Entity::find()
        .select_only()
        .column(social_accounts::Column::Provider)
        .column(social_accounts::Column::ExternalId)
        .filter(social_accounts::Column::TeamId.eq(team_id))
        .filter(social_accounts::Column::Provider.is_in(["facebook", "instagram"]))
        .into_tuple()
        .all(db)
        .await?;
    Ok(rows.into_iter().collect())
}

fn has(held: &HashSet<(String, String)>, provider: &str, id: &str) -> bool {
    held.contains(&(provider.to_owned(), id.to_owned()))
}

/// How long a webhook subscription may take before the connect goes on without it.
const SUBSCRIBE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Subscribes the Page to the app's webhooks, so its comments and messages (and its Instagram
/// account's) arrive. Never fails the connect: the reason, when it did not work, is logged and
/// kept as `meta.webhook_error` on the account.
async fn subscribe(app: &AppState, page: &Page) -> Option<String> {
    let call = app.meta.subscribe_page(&page.id, &page.access_token);
    let why = match tokio::time::timeout(SUBSCRIBE_WAIT, call).await {
        Ok(Ok(())) => return None,
        Ok(Err(PostError::Reconnect)) => "Webhook: Meta refused the Page's token, or the \
            pages_manage_metadata permission is missing."
            .to_owned(),
        Ok(Err(PostError::Failed(why))) => why,
        Err(_) => "Webhook: Meta took too long.".to_owned(),
    };
    tracing::warn!(page = %page.id, why, "meta webhook subscription failed");
    Some(why)
}

/// After a Facebook or Instagram account is disconnected: takes the app's webhook subscription
/// off its Page, unless another account (in any team) still uses that Page. Best effort: a
/// failure is only logged.
pub async fn unsubscribe(app: &AppState, gone: &social_accounts::Model) {
    let page = match gone.provider.as_str() {
        "facebook" => gone.external_id.as_str(),
        "instagram" => gone.meta["page_id"].as_str().unwrap_or_default(),
        _ => return,
    };
    let Some(token) = app
        .cipher
        .decrypt(&gone.access_token)
        .and_then(|b| String::from_utf8(b).ok())
    else {
        return;
    };
    let used = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT EXISTS (SELECT 1 FROM social_accounts WHERE (provider = 'facebook' \
         AND external_id = $1) OR (provider = 'instagram' AND meta->>'page_id' = $1)) AS used",
        [page.into()],
    );
    match app.db.query_one_raw(used).await {
        Ok(Some(r)) if r.try_get("", "used") == Ok(false) => {}
        Ok(_) => return,
        Err(e) => return tracing::warn!(error = %e, "meta page use check failed"),
    }
    let call = app.meta.unsubscribe_page(page, &token);
    match tokio::time::timeout(SUBSCRIBE_WAIT, call).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(page, error = ?e, "meta webhook unsubscribe failed"),
        Err(_) => tracing::warn!(page, "meta webhook unsubscribe timed out"),
    }
}

/// Saves `page` as a Facebook account, or with `ig` its Instagram account (on the Page's token,
/// `meta.page_id` naming the Page); a reconnect takes the new token and profile.
async fn save(
    app: &AppState,
    (team_id, user_id): (Uuid, Uuid),
    page: &Page,
    ig: Option<&IgAccount>,
) -> Result<Connection, DbErr> {
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let webhook_error = subscribe(app, page).await;
    let (provider, external_id, handle, name, picture, mut meta) = match ig {
        None => (
            "facebook",
            &page.id,
            None,
            page.name.clone(),
            &page.picture_url,
            serde_json::json!({}),
        ),
        Some(ig) => (
            "instagram",
            &ig.id,
            ig.username.clone(),
            ig.username.clone().unwrap_or_else(|| page.name.clone()),
            &ig.picture_url,
            serde_json::json!({ "page_id": page.id }),
        ),
    };
    if let Some(why) = webhook_error {
        meta["webhook_error"] = why.into();
    }
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(provider.into()),
        external_id: Set(external_id.clone()),
        handle: Set(handle),
        display_name: Set(Some(name)),
        avatar_url: Set(picture.clone()),
        access_token: Set(app.cipher.encrypt(page.access_token.as_bytes())),
        // A Page token from a long-lived user token does not expire.
        token_expires_at: Set(None),
        meta: Set(meta),
        dm_access: Set(false),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    let refresh = [
        social_accounts::Column::Handle,
        social_accounts::Column::DisplayName,
        social_accounts::Column::AvatarUrl,
        social_accounts::Column::AccessToken,
        social_accounts::Column::TokenExpiresAt,
        social_accounts::Column::Meta,
        social_accounts::Column::UpdatedAt,
    ];
    let (_, json) = save_account(&app.db, (team_id, new_id), row, refresh).await?;
    Ok(json)
}

/// Refreshes the accounts the team already has from `pages`, then keeps `pages` sealed for the
/// picker when any is left to choose. Returns whether one is.
async fn refresh_and_hold(
    app: &AppState,
    (team_id, user_id): (Uuid, Uuid),
    pages: &[Page],
) -> Result<bool, DbErr> {
    let held = held(&app.db, team_id).await?;
    let mut open = false;
    for p in pages {
        if has(&held, "facebook", &p.id) {
            save(app, (team_id, user_id), p, None).await?;
        } else {
            open = true;
        }
        if let Some(ig) = &p.instagram {
            if has(&held, "instagram", &ig.id) {
                save(app, (team_id, user_id), p, Some(ig)).await?;
            } else {
                open = true;
            }
        }
    }
    if !open {
        return Ok(false);
    }
    let sealed = serde_json::to_vec(pages).map_err(|e| DbErr::Custom(e.to_string()))?;
    app.db
        .execute_unprepared("DELETE FROM meta_picks WHERE expires_at < now()")
        .await?;
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO meta_picks (team_id, user_id, pages, expires_at) \
         VALUES ($1, $2, $3, now() + interval '15 minutes') \
         ON CONFLICT (team_id, user_id) DO UPDATE \
         SET pages = EXCLUDED.pages, expires_at = EXCLUDED.expires_at",
        [
            team_id.into(),
            user_id.into(),
            app.cipher.encrypt(&sealed).into(),
        ],
    );
    app.db.execute_raw(stmt).await?;
    Ok(true)
}

/// `GET /connect/meta/callback`: Meta sends the browser here with `code` and `state`, or with
/// `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.meta.configured());
    let (team_id, user_id, code) = match oauth::finish(&app, &headers, q, how).await {
        Ok(found) => found,
        Err(landing) => return *landing,
    };
    let land = |params: &[(&str, &str)]| oauth::landed(&app, PROVIDER, params);
    let fail = |code| land(&[("connect_error", code), ("provider", PROVIDER)]);
    let fetched = async {
        let uri = oauth::redirect_uri(&app, PROVIDER);
        let token = app.meta.exchange(&code, &uri).await?;
        app.meta.pages(&token).await
    };
    let pages = match fetched.await {
        Ok(pages) => pages,
        Err(PostError::Failed(why)) => {
            tracing::warn!(why, "meta connect failed");
            return fail("meta_failed");
        }
        Err(PostError::Reconnect) => {
            tracing::warn!("meta refused the new token");
            return fail("meta_failed");
        }
    };
    if pages.is_empty() {
        return fail("no_pages");
    }
    match refresh_and_hold(&app, (team_id, user_id), &pages).await {
        Ok(true) => land(&[("pick", PROVIDER)]),
        Ok(false) => land(&[("connected", PROVIDER)]),
        Err(e) => {
            tracing::error!(error = %e, "meta accounts save failed");
            fail("internal")
        }
    }
}

/// The caller's live pick in its current team, unsealed.
async fn pending(app: &AppState, t: &InTeam) -> Result<Option<Vec<Page>>, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pages FROM meta_picks WHERE team_id = $1 AND user_id = $2 \
         AND expires_at > now()",
        [t.team.id.into(), t.current.0.id.into()],
    );
    let Some(row) = app.db.query_one_raw(stmt).await? else {
        return Ok(None);
    };
    let sealed: Vec<u8> = row.try_get("", "pages")?;
    Ok(app
        .cipher
        .decrypt(&sealed)
        .and_then(|b| serde_json::from_slice(&b).ok()))
}

async fn drop_pick(app: &AppState, t: &InTeam) -> Result<(), DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM meta_picks WHERE team_id = $1 AND user_id = $2",
        [t.team.id.into(), t.current.0.id.into()],
    );
    app.db.execute_raw(stmt).await.map(|_| ())
}

/// The Pages and Instagram accounts the caller's Meta login granted that the team does not have
/// yet. Owners and admins; `404` when there is no pick or it is over 15 minutes old.
#[utoipa::path(get, path = "/connections/meta/pick", tag = "connections",
    responses((status = 200, body = Choices), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn choices(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let found = async {
        let Some(pages) = pending(&app, &t).await? else {
            return Ok(None);
        };
        let held = held(&app.db, t.team.id).await?;
        let facebook = pages
            .iter()
            .filter(|p| !has(&held, "facebook", &p.id))
            .map(|p| Choice {
                id: p.id.clone(),
                name: p.name.clone(),
                picture_url: p.picture_url.clone(),
            })
            .collect();
        let instagram = pages
            .iter()
            .filter_map(|p| Some((p, p.instagram.as_ref()?)))
            .filter(|(_, ig)| !has(&held, "instagram", &ig.id))
            .map(|(p, ig)| Choice {
                id: ig.id.clone(),
                name: ig.username.clone().unwrap_or_else(|| p.name.clone()),
                picture_url: ig.picture_url.clone(),
            })
            .collect();
        Ok::<_, DbErr>(Some(Choices {
            facebook,
            instagram,
        }))
    };
    match found.await {
        Ok(Some(c)) => Json(c).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "meta pick"),
    }
}

/// Connects the picked Pages as Facebook accounts and the picked Instagram accounts, each in the
/// team's Default set, then drops the pick. Owners and admins. `201` with the saved accounts;
/// `404` when the pick is gone, `422 nothing_picked` or `invalid_pick` (an id the login did not
/// grant).
#[utoipa::path(post, path = "/connections/meta/pick", tag = "connections", request_body = Picked,
    responses((status = 201, body = [Connection]), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn pick(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<Picked>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    if body.facebook.is_empty() && body.instagram.is_empty() {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "nothing_picked");
    }
    let done = async {
        let Some(pages) = pending(&app, &t).await? else {
            return Ok(Err(reply(StatusCode::NOT_FOUND, "not_found")));
        };
        let page_of = |id: &String| pages.iter().find(|p| &p.id == id);
        let ig_of = |id: &String| {
            pages.iter().find_map(|p| {
                let ig = p.instagram.as_ref().filter(|ig| &ig.id == id)?;
                Some((p, ig))
            })
        };
        let known = body.facebook.iter().all(|id| page_of(id).is_some())
            && body.instagram.iter().all(|id| ig_of(id).is_some());
        if !known {
            return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_pick")));
        }
        let who = (t.team.id, t.current.0.id);
        let mut saved = Vec::new();
        for p in body.facebook.iter().filter_map(page_of) {
            saved.push(save(&app, who, p, None).await?);
        }
        for (p, ig) in body.instagram.iter().filter_map(ig_of) {
            saved.push(save(&app, who, p, Some(ig)).await?);
        }
        drop_pick(&app, &t).await?;
        Ok::<_, DbErr>(Ok(saved))
    };
    match done.await {
        Ok(Ok(saved)) => (StatusCode::CREATED, Json(saved)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "meta pick save"),
    }
}

/// Drops the caller's pick, so its Page tokens are gone. Owners and admins; `204` either way.
#[utoipa::path(delete, path = "/connections/meta/pick", tag = "connections",
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error)))]
async fn cancel(_: FromWeb, State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    match drop_pick(&app, &t).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e, "meta pick delete"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/meta/redirect", get(start))
        .route("/connect/meta/callback", get(callback))
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(choices, pick, cancel))
}
