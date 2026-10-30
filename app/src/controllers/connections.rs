//! Connected social accounts of the current team. Members list them; owners and admins connect,
//! regroup and disconnect them. Tokens never leave brain: they are sealed at rest and absent from
//! every response. Every account stays in the team's Default set.
use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{
    sea_query::{OnConflict, Query, SelectStatement},
    ActiveValue::Set,
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, CurrentUser, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    connectors::bluesky::{self, Error as BlueskyError},
    models::{social_account_social_set as membership, social_accounts, social_sets},
    rate_limit::{daily_refund, daily_reserve, ClientIp},
    state::AppState,
};

/// Daily caps on Bluesky connect attempts (`rate_limit::daily_reserve`), failures included.
const BLUESKY_PER_USER: i64 = 30;
const BLUESKY_PER_DID: i64 = 10;
/// Failed passwords on one account (DID) across every team, so many teams cannot brute-force
/// it. Only failures stay counted, so a flood ages out within a day.
const BLUESKY_FAILURES_PER_DID: i64 = 20;

#[derive(Serialize, ToSchema)]
pub struct Connection {
    pub id: Uuid,
    /// `bluesky`, later `facebook`, `instagram`, `threads`, `linkedin`, `x`, ...
    pub provider: String,
    /// The platform's id for the account, e.g. a Bluesky DID.
    pub external_id: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub token_expires_at: Option<DateTime<FixedOffset>>,
    /// The app password may read and send DMs. On Bluesky that needs an app password made with
    /// "Allow access to your direct messages"; without it comments and mentions still arrive.
    /// Checked on connect and again on every inbox poll.
    pub dm_access: bool,
    /// The social sets holding the account; the team's Default set is always one of them.
    pub set_ids: Vec<Uuid>,
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct Provider {
    /// `bluesky`, `meta`, `threads`, `linkedin`, `x`, `tiktok` or `youtube`.
    pub provider: String,
    /// Whether this install has the provider's app credentials; Bluesky needs none.
    pub configured: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct BlueskyLogin {
    /// e.g. `alice.bsky.social`; a leading `@` is fine.
    pub handle: String,
    /// An app password from Bluesky settings, never the account password.
    pub app_password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AccountSets {
    /// Sets of the current team; must include its Default set.
    pub set_ids: Vec<Uuid>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn forbidden() -> Response {
    reply(StatusCode::FORBIDDEN, "forbidden")
}

fn bluesky_error(e: BlueskyError) -> Response {
    let status = match e {
        // Not 401: the web app reads that as a lost session.
        BlueskyError::InvalidHandle
        | BlueskyError::UnsupportedPds
        | BlueskyError::InvalidCredentials => StatusCode::UNPROCESSABLE_ENTITY,
        BlueskyError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        BlueskyError::Unreachable => StatusCode::BAD_GATEWAY,
    };
    reply(status, e.code())
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

/// Ids of every live (not deleted) set; links to deleted sets are kept for a restore but hidden.
pub(crate) fn live_sets() -> SelectStatement {
    Query::select()
        .column(social_sets::Column::Id)
        .from(social_sets::Entity)
        .and_where(social_sets::Column::DeletedAt.is_null())
        .to_owned()
}

/// Live set ids per account, for `accounts`.
async fn set_ids(
    db: &impl ConnectionTrait,
    accounts: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, DbErr> {
    let rows = membership::Entity::find()
        .filter(membership::Column::SocialAccountId.is_in(accounts.iter().copied()))
        .filter(membership::Column::SocialSetId.in_subquery(live_sets()))
        .order_by_asc(membership::Column::SocialSetId)
        .all(db)
        .await?;
    let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for r in rows {
        map.entry(r.social_account_id)
            .or_default()
            .push(r.social_set_id);
    }
    Ok(map)
}

fn connection_json(a: social_accounts::Model, set_ids: Vec<Uuid>) -> Connection {
    Connection {
        id: a.id,
        provider: a.provider,
        external_id: a.external_id,
        handle: a.handle,
        display_name: a.display_name,
        avatar_url: a.avatar_url,
        token_expires_at: a.token_expires_at,
        dm_access: a.dm_access,
        set_ids,
        created_at: a.created_at,
    }
}

async fn one_json(
    db: &impl ConnectionTrait,
    a: social_accounts::Model,
) -> Result<Connection, DbErr> {
    let mut sets = set_ids(db, &[a.id]).await?;
    let ids = sets.remove(&a.id).unwrap_or_default();
    Ok(connection_json(a, ids))
}

async fn default_set(db: &impl ConnectionTrait, team_id: Uuid) -> Result<Uuid, DbErr> {
    social_sets::Entity::find()
        .filter(social_sets::Column::TeamId.eq(team_id))
        .filter(social_sets::Column::IsDefault.eq(true))
        .one(db)
        .await?
        .map(|s| s.id)
        .ok_or_else(|| DbErr::RecordNotFound("team has no Default set".into()))
}

/// The current team's connected accounts, oldest first. Never includes tokens.
#[utoipa::path(get, path = "/connections", tag = "connections",
    responses((status = 200, body = [Connection]), (status = 401, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    let found = async {
        let rows = social_accounts::Entity::find()
            .filter(social_accounts::Column::TeamId.eq(t.team.id))
            .order_by_asc(social_accounts::Column::CreatedAt)
            .order_by_asc(social_accounts::Column::Id)
            .all(&app.db)
            .await?;
        let ids: Vec<Uuid> = rows.iter().map(|a| a.id).collect();
        let mut sets = set_ids(&app.db, &ids).await?;
        Ok::<_, DbErr>(
            rows.into_iter()
                .map(|a| {
                    let ids = sets.remove(&a.id).unwrap_or_default();
                    connection_json(a, ids)
                })
                .collect::<Vec<_>>(),
        )
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "connection list"),
    }
}

/// Every provider and whether this install can connect it.
#[utoipa::path(get, path = "/connections/providers", tag = "connections",
    responses((status = 200, body = [Provider]), (status = 401, body = Error)))]
async fn providers(State(app): State<AppState>, _: CurrentUser) -> Json<Vec<Provider>> {
    Json(
        app.providers
            .iter()
            .map(|&(p, configured)| Provider {
                provider: p.into(),
                configured,
            })
            .collect(),
    )
}

/// Connects a Bluesky account with an app password, or replaces the stored app password when
/// the team already has it (`200` instead of `201`). Owners and admins. Throttled like sign-in:
/// per IP and 5 a minute per user, 30 a day per user, 10 a day per account in one team, and 20
/// refused passwords a day per account across all teams (not counted against a team that
/// already holds the account). A wrong password and an account password
/// both answer `invalid_credentials`, so web should say "use an app password" there.
#[utoipa::path(post, path = "/connections/bluesky", tag = "connections", request_body = BlueskyLogin,
    responses((status = 201, body = Connection), (status = 200, body = Connection),
        (status = 401, body = Error), (status = 403, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 502, body = Error)))]
async fn connect_bluesky(
    _: FromWeb,
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    t: InTeam,
    Body(body): Body<BlueskyLogin>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let user_id = t.current.0.id;
    if !app.login_limiter.allow(ip) || !app.connect_limiter.hit(user_id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let Some(handle) = bluesky::handle(&body.handle) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_handle");
    };
    if body.app_password.is_empty() || body.app_password.len() > 256 {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_credentials");
    }
    // Counted before resolving, so lookups of handles that do not resolve use it up too.
    let user_cap = [(format!("bluesky:user:{user_id}"), BLUESKY_PER_USER)];
    let mut reserved = match daily_reserve(&app.db, &user_cap).await {
        Ok(Some(ids)) => ids,
        Ok(None) => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        Err(e) => return internal(e, "bluesky cap"),
    };
    // No credentials leave brain until the handle and its DID point at each other.
    let id = match app.bluesky.resolve(&handle).await {
        Ok(id) => id,
        Err(e) => {
            // An outage is not the caller's attempt.
            if e == BlueskyError::Unreachable {
                daily_refund(&app.db, &reserved).await;
            }
            return bluesky_error(e);
        }
    };
    // Caps key on the DID: one account can have many handles, and a handle can move.
    let did = id.did.clone();
    // Per team, so strangers failing on an account cannot lock its owner's team out.
    let did_cap = [(format!("bluesky:did:{}:{did}", t.team.id), BLUESKY_PER_DID)];
    match daily_reserve(&app.db, &did_cap).await {
        Ok(Some(ids)) => reserved.extend(ids),
        Ok(None) => {
            daily_refund(&app.db, &reserved).await;
            return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
        }
        Err(e) => {
            daily_refund(&app.db, &reserved).await;
            return internal(e, "bluesky cap");
        }
    }
    let held = social_accounts::Entity::find()
        .filter(social_accounts::Column::TeamId.eq(t.team.id))
        .filter(social_accounts::Column::Provider.eq("bluesky"))
        .filter(social_accounts::Column::ExternalId.eq(&did))
        .count(&app.db)
        .await;
    let held = match held {
        Ok(n) => n > 0,
        Err(e) => {
            daily_refund(&app.db, &reserved).await;
            return internal(e, "bluesky account lookup");
        }
    };
    // Reserved up front so parallel guesses cannot pass it, and given back unless the password
    // was refused. A team reconnecting an account it holds is exempt, so a failure flood from
    // elsewhere cannot keep it from replacing its own app password.
    let failure = if held {
        Vec::new()
    } else {
        let failure_cap = [(format!("bluesky:failed:{did}"), BLUESKY_FAILURES_PER_DID)];
        match daily_reserve(&app.db, &failure_cap).await {
            Ok(Some(ids)) => ids,
            Ok(None) => {
                daily_refund(&app.db, &reserved).await;
                return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
            }
            Err(e) => {
                daily_refund(&app.db, &reserved).await;
                return internal(e, "bluesky cap");
            }
        }
    };
    let verified = app.bluesky.connect(id, &handle, &body.app_password).await;
    if !matches!(verified, Err(BlueskyError::InvalidCredentials)) {
        daily_refund(&app.db, &failure).await;
    }
    let verified = match verified {
        Ok(a) => a,
        Err(e) => {
            // A refused password counts; an outage is not the caller's attempt.
            if e == BlueskyError::Unreachable {
                daily_refund(&app.db, &reserved).await;
            }
            return bluesky_error(e);
        }
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(t.team.id),
        provider: Set("bluesky".into()),
        external_id: Set(verified.did),
        handle: Set(Some(verified.handle)),
        display_name: Set(verified.display_name),
        avatar_url: Set(verified.avatar),
        access_token: Set(app.cipher.encrypt(body.app_password.as_bytes())),
        token_expires_at: Set(None),
        meta: Set(serde_json::json!({ "pds": verified.pds })),
        dm_access: Set(verified.dm_access),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    let saved = async {
        let txn = app.db.begin().await?;
        let set = default_set(&txn, t.team.id).await?;
        // A reconnect keeps the row (id, sets, creator) and takes the new password and profile.
        let upsert = OnConflict::columns([
            social_accounts::Column::TeamId,
            social_accounts::Column::Provider,
            social_accounts::Column::ExternalId,
        ])
        .update_columns([
            social_accounts::Column::Handle,
            social_accounts::Column::DisplayName,
            social_accounts::Column::AvatarUrl,
            social_accounts::Column::AccessToken,
            social_accounts::Column::Meta,
            social_accounts::Column::DmAccess,
            social_accounts::Column::InboxErrorAt,
            social_accounts::Column::UpdatedAt,
        ])
        .to_owned();
        let account = social_accounts::Entity::insert(row)
            .on_conflict(upsert)
            .exec_with_returning(&txn)
            .await?;
        let link = membership::ActiveModel {
            social_account_id: Set(account.id),
            social_set_id: Set(set),
        };
        membership::Entity::insert(link)
            .on_conflict_do_nothing()
            .exec_without_returning(&txn)
            .await?;
        let created = account.id == new_id;
        let json = one_json(&txn, account).await?;
        txn.commit().await.map(|_| (created, json))
    };
    match saved.await {
        Ok((true, json)) => (StatusCode::CREATED, Json(json)).into_response(),
        Ok((false, json)) => Json(json).into_response(),
        Err(e) => internal(e, "bluesky account save"),
    }
}

/// Replaces the sets an account belongs to. Owners and admins. Every id must be a live set of
/// the current team, and the Default set must stay.
#[utoipa::path(put, path = "/connections/{id}/sets", tag = "connections", request_body = AccountSets,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Connection), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn update_sets(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(body): Body<AccountSets>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let wanted: HashSet<Uuid> = body.set_ids.into_iter().collect();
    let done = async {
        let txn = app.db.begin().await?;
        // The row lock serializes concurrent updates of one account.
        let acct = social_accounts::Entity::find_by_id(id)
            .filter(social_accounts::Column::TeamId.eq(t.team.id))
            .lock_exclusive()
            .one(&txn)
            .await?;
        let Some(acct) = acct else {
            return Ok(Err(not_found()));
        };
        let sets = social_sets::Entity::find()
            .filter(social_sets::Column::TeamId.eq(t.team.id))
            .filter(social_sets::Column::DeletedAt.is_null())
            .filter(social_sets::Column::Id.is_in(wanted.iter().copied()))
            .all(&txn)
            .await?;
        if sets.len() != wanted.len() {
            return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_sets")));
        }
        if !sets.iter().any(|s| s.is_default) {
            let code = "default_set_required";
            return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, code)));
        }
        // Links to deleted sets stay, so restoring a set brings its accounts back.
        membership::Entity::delete_many()
            .filter(membership::Column::SocialAccountId.eq(id))
            .filter(membership::Column::SocialSetId.in_subquery(live_sets()))
            .exec(&txn)
            .await?;
        let links = sets.iter().map(|s| membership::ActiveModel {
            social_account_id: Set(id),
            social_set_id: Set(s.id),
        });
        membership::Entity::insert_many(links)
            .exec_without_returning(&txn)
            .await?;
        let json = one_json(&txn, acct).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(json))
    };
    match done.await {
        Ok(Ok(json)) => Json(json).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "connection sets"),
    }
}

/// Disconnects an account and deletes its stored token. Owners and admins, with a sign-in from
/// the last 10 minutes (else `403 reauth_required`).
#[utoipa::path(delete, path = "/connections/{id}", tag = "connections",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn disconnect(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    if !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let deleted = social_accounts::Entity::delete_many()
        .filter(social_accounts::Column::Id.eq(id))
        .filter(social_accounts::Column::TeamId.eq(t.team.id))
        .exec(&app.db)
        .await;
    match deleted {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "connection delete"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(providers))
        .routes(routes!(connect_bluesky))
        .routes(routes!(update_sets))
        .routes(routes!(disconnect))
}
