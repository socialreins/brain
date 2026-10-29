//! Social sets of the current team: groups of connected accounts. Members list them; owners and
//! admins create, rename, delete and restore them. The Default set holds every account and
//! cannot be deleted; it can be renamed. A deleted set keeps its account links and can be
//! restored for 30 days, then `scheduler` purges it.
use std::collections::HashMap;

use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, SqlErr, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    password::reauthenticated,
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    models::{social_account_social_set as membership, social_sets, teams},
    scheduler::RESTORE_DAYS,
    state::AppState,
};

/// Most live sets one team holds.
const MAX_LIVE_SETS: u64 = 50;

#[derive(Serialize, ToSchema)]
pub struct SocialSet {
    pub id: Uuid,
    pub name: String,
    /// The team's pinned Default set: holds every account, cannot be deleted.
    pub is_default: bool,
    pub account_ids: Vec<Uuid>,
    pub account_count: usize,
    pub created_at: DateTime<FixedOffset>,
    /// Set on deleted sets (`?deleted=true`); restorable for 30 days from then.
    pub deleted_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema)]
pub struct SetName {
    /// 1 to 60 characters after trimming, no control characters, unique among the team's live
    /// sets in any case.
    pub name: String,
}

#[derive(Deserialize, ToSchema)]
pub struct DeleteSet {
    /// The account password; not needed within 10 minutes of signing in.
    pub password: Option<String>,
}

#[derive(Deserialize, IntoParams)]
pub struct ListQuery {
    /// `true`: the deleted sets that can still be restored, instead of the live ones.
    #[serde(default)]
    pub deleted: bool,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn forbidden() -> Response {
    reply(StatusCode::FORBIDDEN, "forbidden")
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn name_taken(e: &DbErr) -> bool {
    matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_)))
}

fn restorable_since() -> DateTime<FixedOffset> {
    (Utc::now() - Duration::days(RESTORE_DAYS)).fixed_offset()
}

async fn sets_json(
    db: &impl ConnectionTrait,
    sets: Vec<social_sets::Model>,
) -> Result<Vec<SocialSet>, DbErr> {
    let links = membership::Entity::find()
        .filter(membership::Column::SocialSetId.is_in(sets.iter().map(|s| s.id)))
        .order_by_asc(membership::Column::SocialAccountId)
        .all(db)
        .await?;
    let mut accounts: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for l in links {
        accounts
            .entry(l.social_set_id)
            .or_default()
            .push(l.social_account_id);
    }
    Ok(sets
        .into_iter()
        .map(|s| {
            let account_ids = accounts.remove(&s.id).unwrap_or_default();
            SocialSet {
                id: s.id,
                name: s.name,
                is_default: s.is_default,
                account_count: account_ids.len(),
                account_ids,
                created_at: s.created_at,
                deleted_at: s.deleted_at,
            }
        })
        .collect())
}

async fn one_json(db: &impl ConnectionTrait, set: social_sets::Model) -> Result<SocialSet, DbErr> {
    let mut list = sets_json(db, vec![set]).await?;
    list.pop()
        .ok_or_else(|| DbErr::RecordNotFound("set vanished".into()))
}

/// The team's live set `id`.
async fn live_set(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    id: Uuid,
) -> Result<Option<social_sets::Model>, DbErr> {
    social_sets::Entity::find_by_id(id)
        .filter(social_sets::Column::TeamId.eq(team_id))
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(db)
        .await
}

/// Locks the team row, so concurrent creates and restores cannot pass the live set cap.
async fn live_count_locked(db: &impl ConnectionTrait, team_id: Uuid) -> Result<u64, DbErr> {
    teams::Entity::find_by_id(team_id)
        .lock_exclusive()
        .one(db)
        .await?;
    social_sets::Entity::find()
        .filter(social_sets::Column::TeamId.eq(team_id))
        .filter(social_sets::Column::DeletedAt.is_null())
        .count(db)
        .await
}

/// The current team's live sets, Default first, then oldest first, with their accounts. With
/// `?deleted=true`, the deleted sets that can still be restored, newest deletion first.
#[utoipa::path(get, path = "/social-sets", tag = "social-sets", params(ListQuery),
    responses((status = 200, body = [SocialSet]), (status = 401, body = Error),
        (status = 422, body = Error)))]
async fn list(
    State(app): State<AppState>,
    t: InTeam,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    let found = async {
        let base = social_sets::Entity::find().filter(social_sets::Column::TeamId.eq(t.team.id));
        let sets = if q.deleted {
            base.filter(social_sets::Column::DeletedAt.gt(restorable_since()))
                .order_by_desc(social_sets::Column::DeletedAt)
        } else {
            base.filter(social_sets::Column::DeletedAt.is_null())
                .order_by_desc(social_sets::Column::IsDefault)
                .order_by_asc(social_sets::Column::CreatedAt)
        }
        .order_by_asc(social_sets::Column::Id)
        .all(&app.db)
        .await?;
        sets_json(&app.db, sets).await
    };
    match found.await {
        Ok(sets) => Json(sets).into_response(),
        Err(e) => internal(e, "social set list"),
    }
}

/// Creates a set. Owners and admins; at most 50 live sets per team.
#[utoipa::path(post, path = "/social-sets", tag = "social-sets", request_body = SetName,
    responses((status = 201, body = SocialSet), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<SetName>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let Some(name) = team::name(&body.name) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
    };
    let made = async {
        let txn = app.db.begin().await?;
        if live_count_locked(&txn, t.team.id).await? >= MAX_LIVE_SETS {
            return Ok(None);
        }
        let row = social_sets::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(t.team.id),
            name: Set(name),
            is_default: Set(false),
            ..Default::default()
        };
        let set = social_sets::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Some(set))
    };
    match made.await {
        Ok(Some(set)) => match one_json(&app.db, set).await {
            Ok(json) => (StatusCode::CREATED, Json(json)).into_response(),
            Err(e) => internal(e, "social set read"),
        },
        Ok(None) => reply(StatusCode::CONFLICT, "too_many_sets"),
        Err(e) if name_taken(&e) => reply(StatusCode::CONFLICT, "name_taken"),
        Err(e) => internal(e, "social set insert"),
    }
}

/// Renames a live set, the Default set included. Owners and admins.
#[utoipa::path(patch, path = "/social-sets/{id}", tag = "social-sets", request_body = SetName,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = SocialSet), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn rename(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(body): Body<SetName>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let Some(name) = team::name(&body.name) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
    };
    let renamed = async {
        // One statement, so a concurrent delete cannot slip between a check and the write.
        let done = social_sets::Entity::update_many()
            .col_expr(social_sets::Column::Name, name.into())
            .col_expr(
                social_sets::Column::UpdatedAt,
                Utc::now().fixed_offset().into(),
            )
            .filter(social_sets::Column::Id.eq(id))
            .filter(social_sets::Column::TeamId.eq(t.team.id))
            .filter(social_sets::Column::DeletedAt.is_null())
            .exec(&app.db)
            .await?;
        if done.rows_affected == 0 {
            return Ok(None);
        }
        match live_set(&app.db, t.team.id, id).await? {
            Some(set) => one_json(&app.db, set).await.map(Some),
            None => Ok(None),
        }
    };
    match renamed.await {
        Ok(Some(json)) => Json(json).into_response(),
        Ok(None) => not_found(),
        Err(e) if name_taken(&e) => reply(StatusCode::CONFLICT, "name_taken"),
        Err(e) => internal(e, "social set rename"),
    }
}

/// Deletes a set, restorable for 30 days. Owners and admins, with a sign-in from the last 10
/// minutes or the account password. The Default set cannot go (`409 default_set`).
#[utoipa::path(delete, path = "/social-sets/{id}", tag = "social-sets", request_body = DeleteSet,
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error), (status = 409, body = Error), (status = 429, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(body): Body<DeleteSet>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match live_set(&app.db, t.team.id, id).await {
        Ok(Some(s)) if s.is_default => return reply(StatusCode::CONFLICT, "default_set"),
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "social set lookup"),
    }
    match reauthenticated(&app, &t.current, body.password).await {
        Some(true) => {}
        Some(false) => return reply(StatusCode::FORBIDDEN, "reauth_required"),
        None => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
    }
    // TODO tokens: once API tokens exist, revoke the ones limited to this set here.
    let now = Utc::now().fixed_offset();
    let deleted = social_sets::Entity::update_many()
        .col_expr(social_sets::Column::DeletedAt, now.into())
        .col_expr(social_sets::Column::UpdatedAt, now.into())
        .filter(social_sets::Column::Id.eq(id))
        .filter(social_sets::Column::IsDefault.eq(false))
        .filter(social_sets::Column::DeletedAt.is_null())
        .exec(&app.db)
        .await;
    match deleted {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "social set delete"),
    }
}

/// Restores a set deleted within the last 30 days, with its accounts. Owners and admins. Its
/// name must be free again (`409 name_taken`) and the team under 50 live sets.
#[utoipa::path(post, path = "/social-sets/{id}/restore", tag = "social-sets",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = SocialSet), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error)))]
async fn restore(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let restored = async {
        let txn = app.db.begin().await?;
        let count = live_count_locked(&txn, t.team.id).await?;
        let found = social_sets::Entity::find_by_id(id)
            .filter(social_sets::Column::TeamId.eq(t.team.id))
            .filter(social_sets::Column::DeletedAt.gt(restorable_since()))
            .one(&txn)
            .await?;
        if found.is_none() {
            return Ok(Err(not_found()));
        }
        if count >= MAX_LIVE_SETS {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_sets")));
        }
        let row = social_sets::ActiveModel {
            id: Set(id),
            deleted_at: Set(None),
            updated_at: Set(Utc::now().fixed_offset()),
            ..Default::default()
        };
        let set = social_sets::Entity::update(row).exec(&txn).await?;
        let json = one_json(&txn, set).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(json))
    };
    match restored.await {
        Ok(Ok(json)) => Json(json).into_response(),
        Ok(Err(res)) => res,
        Err(e) if name_taken(&e) => reply(StatusCode::CONFLICT, "name_taken"),
        Err(e) => internal(e, "social set restore"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(rename, destroy))
        .routes(routes!(restore))
}
