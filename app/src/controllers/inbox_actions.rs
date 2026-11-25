//! What a person does from a Bluesky inbox item beyond replying (`bluesky_actions`): like,
//! repost, follow, mute, block, hide a reply, delete our reply, report. Confirmed in the body and
//! logged. Members like, repost, follow and mute; owners and admins block, hide, delete a reply
//! and report.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, FromQueryResult, QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::error,
};
use crate::{
    bluesky_actions::{self as actions, Action, Extra, Viewer},
    connectors::bluesky::InboxError,
    models::{inbox_items, inbox_replies, social_accounts},
    state::AppState,
};

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionInput {
    pub action: Action,
    /// Must be `true`: the person was asked first.
    #[serde(default)]
    pub confirm: bool,
    /// `delete_reply`: the reply (`replies[].id` of the item) to delete.
    pub reply_id: Option<Uuid>,
    /// `report`: `spam`, `violation`, `misleading`, `sexual`, `rude` or `other`.
    pub reason_type: Option<String>,
    /// `report`: what to tell the moderators, up to 2000 characters.
    pub reason: Option<String>,
    /// `report`: report the author's account instead of the post.
    #[serde(default)]
    pub account: bool,
}

#[derive(Serialize, ToSchema)]
pub struct ActionDone {
    pub action: Action,
    /// The record made on Bluesky (a like, repost, follow or block), or a report's id.
    pub remote_id: Option<String>,
}

#[derive(Serialize, ToSchema, FromQueryResult)]
pub struct LogEntry {
    pub id: Uuid,
    pub action: String,
    /// `done` or `failed`.
    pub status: String,
    /// What it acted on: a post or an account.
    pub target: String,
    /// The error code of a failed one.
    pub error: Option<String>,
    pub done_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct Actions {
    /// The account's live state toward the item's post and author; `null` when Bluesky could
    /// not be read (`viewer_error` says why).
    pub viewer: Option<Viewer>,
    pub viewer_error: Option<&'static str>,
    /// The reply is hidden from our thread, by the last hide or unhide made here.
    pub hidden: bool,
    /// Newest first.
    pub log: Vec<LogEntry>,
}

async fn find(
    app: &AppState,
    team: Uuid,
    id: Uuid,
) -> Result<Option<(inbox_items::Model, social_accounts::Model)>, sea_orm::DbErr> {
    let Some(item) = inbox_items::Entity::find_by_id(id)
        .filter(inbox_items::Column::TeamId.eq(team))
        .one(&app.db)
        .await?
    else {
        return Ok(None);
    };
    let account = social_accounts::Entity::find_by_id(item.social_account_id)
        .one(&app.db)
        .await?;
    Ok(account.map(|a| (item, a)))
}

fn failed(e: &InboxError) -> Response {
    let status = match e {
        InboxError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    let body = json!({ "error": e.code(), "message": e.message() });
    (status, Json(body)).into_response()
}

/// The account's live state toward an item's post and author, and what was done to it from here,
/// newest first. Every member.
#[utoipa::path(get, path = "/inbox-items/{id}/actions", tag = "inbox",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Actions), (status = 401, body = crate::controllers::waitlist::Error),
        (status = 404, body = crate::controllers::waitlist::Error)))]
async fn list(State(app): State<AppState>, t: InTeam, Path(id): Path<Uuid>) -> Response {
    let (item, account) = match find(&app, t.team.id, id).await {
        Ok(Some(found)) => found,
        Ok(None) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "inbox item read"),
    };
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT id, action, status, target, error, done_by, created_at FROM inbox_actions \
         WHERE inbox_item_id = $1 ORDER BY created_at DESC, id DESC LIMIT 100",
        [id.into()],
    );
    let rows = match app.db.query_all_raw(stmt).await {
        Ok(rows) => rows,
        Err(e) => return internal(e, "inbox actions read"),
    };
    let log: Result<Vec<LogEntry>, _> = rows
        .iter()
        .map(|r| LogEntry::from_query_result(r, ""))
        .collect();
    let Ok(log) = log else {
        return internal("unreadable row", "inbox actions read");
    };
    let hidden = log
        .iter()
        .find(|l| l.status == "done" && matches!(l.action.as_str(), "hide_reply" | "unhide_reply"))
        .is_some_and(|l| l.action == "hide_reply");
    let (viewer, viewer_error) = if item.platform == "bluesky" {
        match actions::viewer(&app, &account, &item).await {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e.code())),
        }
    } else {
        (None, Some("unsupported_platform"))
    };
    Json(Actions {
        viewer,
        viewer_error,
        hidden,
        log,
    })
    .into_response()
}

/// Does one action from an item, confirmed (`confirm: true`, else `422 confirm_required`) and
/// logged. `403` when a member asks for what only owners and admins do; `422` with `error` and
/// `message` when it cannot be done (`not_applicable`, `not_our_thread`, `own_account`,
/// `invalid_reason`, `too_many_hidden`, and the Bluesky errors of a reply) or `429`. At most 60 a
/// minute per user, shared with thread reads.
#[utoipa::path(post, path = "/inbox-items/{id}/actions", tag = "inbox", request_body = ActionInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = ActionDone), (status = 401, body = crate::controllers::waitlist::Error),
        (status = 403, body = crate::controllers::waitlist::Error), (status = 404, body = crate::controllers::waitlist::Error),
        (status = 422, body = crate::controllers::waitlist::Error), (status = 429, body = crate::controllers::waitlist::Error)))]
async fn act(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<ActionInput>,
) -> Response {
    if input.action.manages() && !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    if !input.confirm {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "confirm_required");
    }
    if input
        .reason
        .as_ref()
        .is_some_and(|r| r.chars().count() > 2000)
    {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_reason");
    }
    let (item, account) = match find(&app, t.team.id, id).await {
        Ok(Some(found)) => found,
        Ok(None) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "inbox item read"),
    };
    let reply_row = match input.reply_id {
        Some(rid) => match inbox_replies::Entity::find_by_id(rid)
            .filter(inbox_replies::Column::InboxItemId.eq(item.id))
            .one(&app.db)
            .await
        {
            Ok(Some(r)) => Some(r),
            Ok(None) => return reply(StatusCode::NOT_FOUND, "not_found"),
            Err(e) => return internal(e, "inbox reply read"),
        },
        None => None,
    };
    if !app.live_limiter.hit(t.current.0.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    // A report is made once per target: a second click must not file it again.
    if input.action == Action::Report {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 FROM inbox_actions WHERE inbox_item_id = $1 AND action = 'report' \
             AND status = 'done' AND detail->>'account' = $2",
            [item.id.into(), input.account.to_string().into()],
        );
        match app.db.query_one_raw(stmt).await {
            Ok(Some(_)) => return reply(StatusCode::CONFLICT, "already_reported"),
            Ok(None) => {}
            Err(e) => return internal(e, "inbox actions read"),
        }
    }
    let extra = Extra {
        reply: reply_row.as_ref(),
        reason_type: input.reason_type.as_deref(),
        reason: input.reason.as_deref(),
        account: input.account,
    };
    let outcome = actions::perform(&app, &account, &item, input.action, &extra).await;
    if outcome == Err(InboxError::Reconnect) {
        crate::expiry::flag_reconnect(&app.db, account.id).await;
    }
    // What was said to the moderators stays out of the log: only its length and kind.
    let detail = json!({
        "account": input.account,
        "reply_id": input.reply_id,
        "reason_type": input.reason_type,
        "reason_chars": input.reason.as_ref().map(|r| r.chars().count()),
    });
    if let Err(e) = actions::log(&app, &item, t.current.0.id, input.action, &outcome, detail).await
    {
        // The action happened (or not); the log is what failed.
        tracing::error!(error = %e, item = %item.id, "inbox action not logged");
    }
    match outcome {
        Ok(remote_id) => Json(ActionDone {
            action: input.action,
            remote_id,
        })
        .into_response(),
        Err(e) => failed(&e),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list, act))
}
