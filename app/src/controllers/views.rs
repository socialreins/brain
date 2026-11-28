//! Saved views: a person's named filters for a list page (`scope`: `comments` or `inbox`), with
//! at most one marked the default, which the page applies when it opens with no filters. Per
//! person and team: a view is never shown to another member and another team's id is `404`, the
//! same as one that does not exist. The page owns the meaning of `filters` (a map of short strings,
//! its own query); brain checks only its shape and size.
use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait, Value};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::state::AppState;

/// Most views a person keeps per team and page.
const MAX_VIEWS: i64 = 20;
const SCOPES: [&str; 2] = ["comments", "inbox"];
const MAX_KEYS: usize = 24;
const MAX_VALUE: usize = 600;

#[derive(Serialize, ToSchema)]
pub struct View {
    pub id: Uuid,
    pub scope: String,
    pub name: String,
    /// The page's filters as short strings, for example `{"network": "x", "state": "open"}`.
    #[schema(value_type = Object)]
    pub filters: serde_json::Value,
    /// Applied when the page opens with no filters; one at most per page.
    pub is_default: bool,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Deserialize, IntoParams)]
pub struct ViewQuery {
    /// `comments` or `inbox`.
    pub scope: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewInput {
    /// `comments` or `inbox`.
    pub scope: String,
    /// 1 to 60 characters after trimming; unique per page ignoring case.
    pub name: String,
    #[serde(default = "no_filters")]
    #[schema(value_type = Object)]
    pub filters: serde_json::Value,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewChange {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub filters: Option<serde_json::Value>,
    /// `true` makes it the default (any other default stops being one); `false` leaves the page
    /// with no default.
    #[serde(default)]
    pub is_default: Option<bool>,
}

fn no_filters() -> serde_json::Value {
    serde_json::json!({})
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn name_ok(name: &str) -> Option<String> {
    let name = name.trim();
    (1..=60)
        .contains(&name.chars().count())
        .then(|| name.to_owned())
}

/// `filters` as an object of at most 24 keys (lower case letters and `_`) holding strings of at
/// most 600 characters.
fn filters_ok(v: &serde_json::Value) -> bool {
    let Some(map) = v.as_object() else {
        return false;
    };
    map.len() <= MAX_KEYS
        && map.iter().all(|(k, v)| {
            (1..=32).contains(&k.len())
                && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                && v.as_str().is_some_and(|s| s.chars().count() <= MAX_VALUE)
        })
}

fn view_of(r: &sea_orm::QueryResult) -> Result<View, DbErr> {
    Ok(View {
        id: r.try_get("", "id")?,
        scope: r.try_get("", "scope")?,
        name: r.try_get("", "name")?,
        filters: r.try_get("", "filters")?,
        is_default: r.try_get("", "is_default")?,
        created_at: r.try_get("", "created_at")?,
        updated_at: r.try_get("", "updated_at")?,
    })
}

fn sql(text: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, text, values)
}

const COLS: &str = "id, scope, name, filters, is_default, created_at, updated_at";

/// Your saved views of one page, the default first, then by name.
#[utoipa::path(get, path = "/views", tag = "views", params(ViewQuery),
    responses((status = 200, body = [View]), (status = 401, body = Error),
        (status = 422, body = Error)))]
async fn list(
    State(app): State<AppState>,
    t: InTeam,
    q: Result<Query<ViewQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    if !SCOPES.contains(&q.scope.as_str()) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let stmt = sql(
        &format!(
            "SELECT {COLS} FROM saved_views WHERE team_id = $1 AND user_id = $2 AND scope = $3 \
             ORDER BY is_default DESC, lower(name), id"
        ),
        vec![t.team.id.into(), t.current.0.id.into(), q.scope.into()],
    );
    let found = async {
        app.db
            .query_all_raw(stmt)
            .await?
            .iter()
            .map(view_of)
            .collect::<Result<Vec<_>, DbErr>>()
    };
    match found.await {
        Ok(views) => Json(views).into_response(),
        Err(e) => internal(e, "views list"),
    }
}

fn is_unique(e: &DbErr) -> bool {
    e.to_string().contains("saved_views_name")
}

/// Saves a view. Any member, for themselves. At most 20 per page (`409 too_many_views`); a name
/// already used on the page is `409 name_taken`.
#[utoipa::path(post, path = "/views", tag = "views", request_body = ViewInput,
    responses((status = 201, body = View), (status = 401, body = Error),
        (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<ViewInput>,
) -> Response {
    let (Some(name), true, true) = (
        name_ok(&input.name),
        SCOPES.contains(&input.scope.as_str()),
        filters_ok(&input.filters),
    ) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_view");
    };
    let made = async {
        let txn = app.db.begin().await?;
        // One person's saves wait for each other, so the cap and the default hold.
        txn.execute_raw(sql(
            "SELECT 1 FROM team_user WHERE team_id = $1 AND user_id = $2 FOR UPDATE",
            vec![t.team.id.into(), t.current.0.id.into()],
        ))
        .await?;
        let held = txn
            .query_one_raw(sql(
                "SELECT count(*) AS n FROM saved_views \
                 WHERE team_id = $1 AND user_id = $2 AND scope = $3",
                vec![
                    t.team.id.into(),
                    t.current.0.id.into(),
                    input.scope.clone().into(),
                ],
            ))
            .await?
            .map(|r| r.try_get::<i64>("", "n"))
            .transpose()?
            .unwrap_or(0);
        if held >= MAX_VIEWS {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_views")));
        }
        if input.is_default {
            txn.execute_raw(sql(
                "UPDATE saved_views SET is_default = false, updated_at = now() \
                 WHERE team_id = $1 AND user_id = $2 AND scope = $3 AND is_default",
                vec![
                    t.team.id.into(),
                    t.current.0.id.into(),
                    input.scope.clone().into(),
                ],
            ))
            .await?;
        }
        let row = txn
            .query_one_raw(sql(
                &format!(
                    "INSERT INTO saved_views (team_id, user_id, scope, name, filters, is_default) \
                     VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLS}"
                ),
                vec![
                    t.team.id.into(),
                    t.current.0.id.into(),
                    input.scope.into(),
                    name.into(),
                    input.filters.into(),
                    input.is_default.into(),
                ],
            ))
            .await?
            .ok_or_else(|| DbErr::Custom("no row returned".into()))?;
        let view = view_of(&row)?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(view))
    };
    match made.await {
        Ok(Ok(v)) => (StatusCode::CREATED, Json(v)).into_response(),
        Ok(Err(res)) => res,
        Err(e) if is_unique(&e) => reply(StatusCode::CONFLICT, "name_taken"),
        Err(e) => internal(e, "view insert"),
    }
}

/// Renames a view, replaces its filters, or sets or clears its default mark. Only the person's
/// own view in this team; any other id is `404`.
#[utoipa::path(put, path = "/views/{id}", tag = "views", request_body = ViewChange,
    params(("id" = Uuid, Path, description = "The view's id.")),
    responses((status = 200, body = View), (status = 401, body = Error),
        (status = 404, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<String>,
    Body(input): Body<ViewChange>,
) -> Response {
    let Ok(id) = id.parse::<Uuid>() else {
        return reply(StatusCode::NOT_FOUND, "not_found");
    };
    let name = input.name.as_deref().map(name_ok);
    if name == Some(None) || input.filters.as_ref().is_some_and(|f| !filters_ok(f)) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_view");
    }
    let saved = async {
        let txn = app.db.begin().await?;
        // The same lock as a save takes, so two changes of the default (or one and a save) cannot cross.
        txn.execute_raw(sql(
            "SELECT 1 FROM team_user WHERE team_id = $1 AND user_id = $2 FOR UPDATE",
            vec![t.team.id.into(), t.current.0.id.into()],
        ))
        .await?;
        let Some(found) = txn
            .query_one_raw(sql(
                "SELECT scope FROM saved_views WHERE id = $1 AND team_id = $2 AND user_id = $3 \
                 FOR UPDATE",
                vec![id.into(), t.team.id.into(), t.current.0.id.into()],
            ))
            .await?
        else {
            return Ok(Err(reply(StatusCode::NOT_FOUND, "not_found")));
        };
        let scope: String = found.try_get("", "scope")?;
        if input.is_default == Some(true) {
            txn.execute_raw(sql(
                "UPDATE saved_views SET is_default = false, updated_at = now() \
                 WHERE team_id = $1 AND user_id = $2 AND scope = $3 AND is_default AND id <> $4",
                vec![
                    t.team.id.into(),
                    t.current.0.id.into(),
                    scope.into(),
                    id.into(),
                ],
            ))
            .await?;
        }
        let row = txn
            .query_one_raw(sql(
                &format!(
                    "UPDATE saved_views SET name = COALESCE($2, name), \
                     filters = COALESCE($3, filters), is_default = COALESCE($4, is_default), \
                     updated_at = now() WHERE id = $1 RETURNING {COLS}"
                ),
                vec![
                    id.into(),
                    name.flatten().into(),
                    input.filters.into(),
                    input.is_default.into(),
                ],
            ))
            .await?
            .ok_or_else(|| DbErr::Custom("no row returned".into()))?;
        let view = view_of(&row)?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(view))
    };
    match saved.await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(res)) => res,
        Err(e) if is_unique(&e) => reply(StatusCode::CONFLICT, "name_taken"),
        Err(e) => internal(e, "view update"),
    }
}

/// Deletes a view. Only the person's own view in this team; any other id is `404`.
#[utoipa::path(delete, path = "/views/{id}", tag = "views",
    params(("id" = Uuid, Path, description = "The view's id.")),
    responses((status = 204), (status = 401, body = Error), (status = 404, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<String>,
) -> Response {
    let Ok(id) = id.parse::<Uuid>() else {
        return reply(StatusCode::NOT_FOUND, "not_found");
    };
    let stmt = sql(
        "DELETE FROM saved_views WHERE id = $1 AND team_id = $2 AND user_id = $3",
        vec![id.into(), t.team.id.into(), t.current.0.id.into()],
    );
    match app.db.execute_raw(stmt).await {
        Ok(r) if r.rows_affected() == 0 => reply(StatusCode::NOT_FOUND, "not_found"),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e, "view delete"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(update, destroy))
}
