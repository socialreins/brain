//! Automations of the current team: trigger, conditions and actions (see `crate::automations`).
//! Members read them and their runs; owners and admins create, change, switch and delete them,
//! as with inboxes and sets.
use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    automations::{self as rules, Action, ActionResult, Conditions},
    inbox,
    models::{automation_runs, automations, teams},
    state::AppState,
};

/// Most automations one team holds.
const MAX_AUTOMATIONS: u64 = 50;
const PAGE: u64 = 50;

#[derive(Serialize, ToSchema)]
pub struct Automation {
    pub id: Uuid,
    pub name: String,
    pub active: bool,
    /// When it was last switched on; comments received before that never run it.
    pub activated_at: DateTime<FixedOffset>,
    /// `comment_received`.
    pub trigger: String,
    pub conditions: Conditions,
    pub actions: Vec<Action>,
    /// A public reply sent when a `send_dm` could not go (`dm_not_allowed`, `no_dm_access`)
    /// and no `reply_publicly` went out.
    pub fallback_public_reply: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    /// How often it ran; only in the list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<RunStats>,
}

#[derive(Serialize, ToSchema, Default)]
pub struct RunStats {
    pub count: i64,
    pub last_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutomationInput {
    /// 1 to 60 characters after trimming.
    pub name: String,
    /// `comment_received`.
    pub trigger: String,
    #[serde(default)]
    pub conditions: Conditions,
    /// 1 or 2 actions, at most one of each type.
    pub actions: Vec<Action>,
    /// Default `true`.
    #[serde(default = "yes")]
    pub active: bool,
    /// Needs a `send_dm` action; at most 300 graphemes; may hold `{name}`.
    #[serde(default)]
    pub fallback_public_reply: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, ToSchema)]
pub struct Run {
    pub id: Uuid,
    pub automation_id: Uuid,
    pub inbox_item_id: Uuid,
    /// `running`, `sent`, `failed` or `skipped`.
    pub status: String,
    /// Why not everything was sent: `dm_not_allowed`, `no_dm_access`, `rate_limited`,
    /// `daily_cap`, `reconnect_required`, `interrupted`, `bluesky_refused`, ...
    pub reason: Option<String>,
    pub results: Vec<ActionResult>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct RunPage {
    /// Newest first.
    pub runs: Vec<Run>,
    pub next_cursor: Option<Uuid>,
}

#[derive(Deserialize, IntoParams)]
pub struct RunQuery {
    /// `next_cursor` of the previous page.
    pub cursor: Option<Uuid>,
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

fn automation_json(a: automations::Model) -> Automation {
    Automation {
        id: a.id,
        name: a.name,
        active: a.active,
        activated_at: a.activated_at,
        trigger: a.trigger,
        conditions: serde_json::from_value(a.conditions).unwrap_or_default(),
        actions: serde_json::from_value(a.actions).unwrap_or_default(),
        fallback_public_reply: a.fallback_public_reply,
        created_by: a.created_by,
        created_at: a.created_at,
        updated_at: a.updated_at,
        runs: None,
    }
}

async fn find(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    id: Uuid,
) -> Result<Option<automations::Model>, DbErr> {
    automations::Entity::find_by_id(id)
        .filter(automations::Column::TeamId.eq(team_id))
        .one(db)
        .await
}

/// The input checked and normalized; `Err` holds the 422 reply.
async fn checked(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    mut input: AutomationInput,
) -> Result<Result<(String, AutomationInput), Response>, DbErr> {
    let Some(name) = team::name(&input.name) else {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name")));
    };
    input.fallback_public_reply = input
        .fallback_public_reply
        .map(|f| f.trim().to_owned())
        .filter(|f| !f.is_empty());
    let shape = rules::validate(
        &input.trigger,
        &mut input.conditions,
        &input.actions,
        input.fallback_public_reply.as_deref(),
    );
    if let Err(code) = shape {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, code)));
    }
    let c = &input.conditions;
    if !inbox::owned(db, team_id, c.social_set_id, &c.account_ids).await? {
        return Ok(Err(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_conditions",
        )));
    }
    Ok(Ok((name, input)))
}

/// The team's automations, oldest first, each with its run count and last run.
#[utoipa::path(get, path = "/automations", tag = "automations",
    responses((status = 200, body = [Automation]), (status = 401, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    let found = async {
        let rows = automations::Entity::find()
            .filter(automations::Column::TeamId.eq(t.team.id))
            .order_by_asc(automations::Column::CreatedAt)
            .order_by_asc(automations::Column::Id)
            .all(&app.db)
            .await?;
        let stats = app
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT automation_id, COUNT(*) AS count, MAX(created_at) AS last_at \
                 FROM automation_runs WHERE team_id = $1 GROUP BY automation_id",
                [t.team.id.into()],
            ))
            .await?;
        let mut stats = stats
            .iter()
            .map(|r| {
                let id: Uuid = r.try_get("", "automation_id")?;
                let count = r.try_get("", "count")?;
                let last_at = r.try_get("", "last_at")?;
                Ok((id, RunStats { count, last_at }))
            })
            .collect::<Result<std::collections::HashMap<_, _>, DbErr>>()?;
        Ok::<_, DbErr>(
            rows.into_iter()
                .map(|a| {
                    let runs = stats.remove(&a.id).unwrap_or_default();
                    Automation {
                        runs: Some(runs),
                        ..automation_json(a)
                    }
                })
                .collect::<Vec<_>>(),
        )
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "automation list"),
    }
}

/// One automation.
#[utoipa::path(get, path = "/automations/{id}", tag = "automations", params(("id" = Uuid, Path)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, Path(id): Path<Uuid>) -> Response {
    match find(&app.db, t.team.id, id).await {
        Ok(Some(a)) => Json(automation_json(a)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation read"),
    }
}

/// Creates an automation; it acts on comments received from now on. Owners and admins; at
/// most 50 per team. Unknown fields, triggers, condition keys and action types are refused
/// (`422 invalid_body`, `invalid_trigger`, `invalid_conditions`, `invalid_actions`,
/// `invalid_text`, `invalid_fallback`).
#[utoipa::path(post, path = "/automations", tag = "automations", request_body = AutomationInput,
    responses((status = 201, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<AutomationInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let made = async {
        let (name, input) = match checked(&app.db, t.team.id, input).await? {
            Ok(v) => v,
            Err(res) => return Ok(Err(res)),
        };
        let txn = app.db.begin().await?;
        teams::Entity::find_by_id(t.team.id)
            .lock_exclusive()
            .one(&txn)
            .await?;
        let n = automations::Entity::find()
            .filter(automations::Column::TeamId.eq(t.team.id))
            .count(&txn)
            .await?;
        if n >= MAX_AUTOMATIONS {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_automations")));
        }
        let row = automations::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(t.team.id),
            name: Set(name),
            active: Set(input.active),
            trigger: Set(input.trigger),
            conditions: Set(serde_json::json!(input.conditions)),
            actions: Set(serde_json::json!(input.actions)),
            fallback_public_reply: Set(input.fallback_public_reply),
            created_by: Set(Some(t.current.0.id)),
            ..Default::default()
        };
        let a = automations::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(automation_json(a)))
    };
    match made.await {
        Ok(Ok(a)) => (StatusCode::CREATED, Json(a)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "automation insert"),
    }
}

/// Sets `active`, moving `activated_at` to now when it switches on. `None` when not found.
async fn switch(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    id: Uuid,
    active: bool,
) -> Result<Option<automations::Model>, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE automations SET active = $3, updated_at = now(), \
         activated_at = CASE WHEN $3 AND NOT active THEN now() ELSE activated_at END \
         WHERE id = $1 AND team_id = $2 RETURNING *",
        [id.into(), team_id.into(), active.into()],
    );
    automations::Entity::find().from_raw_sql(stmt).one(db).await
}

/// Replaces an automation. Owners and admins. Its runs stay.
#[utoipa::path(put, path = "/automations/{id}", tag = "automations",
    request_body = AutomationInput, params(("id" = Uuid, Path)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<AutomationInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let saved = async {
        let (name, input) = match checked(&app.db, t.team.id, input).await? {
            Ok(v) => v,
            Err(res) => return Ok(Err(res)),
        };
        let txn = app.db.begin().await?;
        let done = automations::Entity::update_many()
            .col_expr(automations::Column::Name, name.into())
            .col_expr(automations::Column::Trigger, input.trigger.into())
            .col_expr(
                automations::Column::Conditions,
                serde_json::json!(input.conditions).into(),
            )
            .col_expr(
                automations::Column::Actions,
                serde_json::json!(input.actions).into(),
            )
            .col_expr(
                automations::Column::FallbackPublicReply,
                input.fallback_public_reply.into(),
            )
            .filter(automations::Column::Id.eq(id))
            .filter(automations::Column::TeamId.eq(t.team.id))
            .exec(&txn)
            .await?;
        if done.rows_affected == 0 {
            return Ok(Err(not_found()));
        }
        let a = switch(&txn, t.team.id, id, input.active).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(a.map(automation_json).ok_or_else(not_found))
    };
    match saved.await {
        Ok(Ok(a)) => Json(a).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "automation update"),
    }
}

async fn set_active(app: &AppState, t: &InTeam, id: Uuid, active: bool) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match switch(&app.db, t.team.id, id, active).await {
        Ok(Some(a)) => Json(automation_json(a)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation switch"),
    }
}

/// Switches an automation on; it acts on comments received from now on. Owners and admins.
#[utoipa::path(post, path = "/automations/{id}/enable", tag = "automations",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn enable(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    set_active(&app, &t, id, true).await
}

/// Switches an automation off. Owners and admins.
#[utoipa::path(post, path = "/automations/{id}/disable", tag = "automations",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn disable(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    set_active(&app, &t, id, false).await
}

/// Deletes an automation and its runs; replies it sent stay on their items. Owners and admins.
#[utoipa::path(delete, path = "/automations/{id}", tag = "automations",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let gone = automations::Entity::delete_many()
        .filter(automations::Column::Id.eq(id))
        .filter(automations::Column::TeamId.eq(t.team.id))
        .exec(&app.db)
        .await;
    match gone {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "automation delete"),
    }
}

/// An automation's runs, newest first, 50 a page. At most 500 runs act per team a day; past
/// that a run is `skipped: daily_cap`.
#[utoipa::path(get, path = "/automations/{id}/runs", tag = "automations",
    params(("id" = Uuid, Path), RunQuery),
    responses((status = 200, body = RunPage), (status = 401, body = Error),
        (status = 404, body = Error), (status = 422, body = Error)))]
async fn runs(
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    q: Result<Query<RunQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    let found = async {
        if find(&app.db, t.team.id, id).await?.is_none() {
            return Ok(None);
        }
        // Run ids are v7 UUIDs, so they sort by creation.
        let mut sel =
            automation_runs::Entity::find().filter(automation_runs::Column::AutomationId.eq(id));
        if let Some(c) = q.cursor {
            sel = sel.filter(automation_runs::Column::Id.lt(c));
        }
        let mut rows = sel
            .order_by_desc(automation_runs::Column::Id)
            .limit(PAGE + 1)
            .all(&app.db)
            .await?;
        let more = rows.len() as u64 > PAGE;
        rows.truncate(PAGE as usize);
        let next_cursor = more.then(|| rows.last().map(|r| r.id)).flatten();
        let runs = rows
            .into_iter()
            .map(|r| Run {
                id: r.id,
                automation_id: r.automation_id,
                inbox_item_id: r.inbox_item_id,
                status: r.status,
                reason: r.reason,
                results: serde_json::from_value(r.results).unwrap_or_default(),
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect();
        Ok::<_, DbErr>(Some(RunPage { runs, next_cursor }))
    };
    match found.await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation runs"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(enable))
        .routes(routes!(disable))
        .routes(routes!(runs))
}
