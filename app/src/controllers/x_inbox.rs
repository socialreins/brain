//! The X inbox's settings and the actions on an X item (like, repost, hide a reply). Reading X
//! costs money, so `PUT /connections/{id}/x-inbox` is where a team turns it on for one account
//! and sets how often it is read and what it may spend a month (`x_inbox`). Owners and admins
//! change settings; every member can read them and act on items.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::numbered::{ItemId, ID_PARAM};
use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    connectors::x::inbox::Granted,
    models::{inbox_items, social_accounts},
    state::AppState,
    x_inbox::{self, Action, Choice, Settings},
};

#[derive(Serialize, ToSchema)]
pub struct Limits {
    pub min_poll_minutes: i32,
    pub max_poll_minutes: i32,
    pub default_poll_minutes: i32,
    pub min_budget_usd: f64,
    pub max_budget_usd: f64,
    pub default_budget_usd: f64,
}

#[derive(Serialize, ToSchema)]
pub struct Estimate {
    /// A new mention or reply: the post and, once a day, its author.
    pub per_mention_usd: f64,
    /// A new DM: the event and, once a day, its sender.
    pub per_dm_usd: f64,
    /// A month in which nothing new comes: free for mentions, about this for DMs.
    pub idle_month_usd: f64,
    /// This month's spending carried to the month's end, never over the budget.
    pub projected_month_usd: f64,
}

/// An X account's inbox settings and state.
#[derive(Serialize, ToSchema)]
pub struct XInbox {
    /// Off until a person turns it on: nothing is read, and nothing spent, before that.
    pub enabled: bool,
    pub read_dms: bool,
    /// Minutes between reads.
    pub poll_minutes: i32,
    pub budget_usd: f64,
    /// Counted against the budget this calendar month (UTC).
    pub spent_usd: f64,
    /// Why polling stopped: `credits`, `usage_capped`, `budget` or `errors`; empty when it runs.
    pub paused: Option<String>,
    /// What the pause means and what to do.
    pub message: Option<String>,
    pub last_error: Option<String>,
    pub last_read_at: Option<DateTime<Utc>>,
    /// The token was granted DM permission: false for an account connected before the inbox
    /// existed, which reconnects to read and send DMs.
    pub can_read_dms: bool,
    pub can_like: bool,
    pub can_hide: bool,
    /// X refused the account's token: connect it again.
    pub reconnect: bool,
    /// With billing on, the team's X credits pay too: 1 per reply, DM or action, and 1 for
    /// every `reads_per_credit` items read.
    pub credits: bool,
    pub reads_per_credit: i32,
    pub estimate: Estimate,
    pub limits: Limits,
}

/// What a person sets. Missing fields keep their value (the defaults for an account that never
/// had any).
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct XInboxInput {
    pub enabled: Option<bool>,
    pub read_dms: Option<bool>,
    /// 15 to 1440.
    pub poll_minutes: Option<i32>,
    /// $0.10 to $100 a month for this account.
    pub budget_usd: Option<f64>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionInput {
    /// `like`, `repost`, `hide` (a reply to our post) or `unhide`.
    pub action: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn usd(micros: i64) -> f64 {
    micros as f64 / 1e6
}

fn json(account: &social_accounts::Model, s: &Settings, billing: bool) -> XInbox {
    let g = Granted::of(account.meta["scope"].as_str());
    let e = x_inbox::estimate(s, Utc::now());
    XInbox {
        enabled: s.enabled,
        read_dms: s.read_dms,
        poll_minutes: s.poll_minutes,
        budget_usd: usd(s.budget_micros),
        spent_usd: usd(s.spent_micros),
        paused: s.paused_reason.clone(),
        message: s
            .paused_reason
            .as_deref()
            .map(|r| x_inbox::pause_message(r, s.last_error.as_deref())),
        last_error: s.last_error.clone(),
        last_read_at: s.last_read_at,
        can_read_dms: g.dms,
        can_like: g.like,
        can_hide: g.hide,
        reconnect: account.meta["reconnect"].as_bool() == Some(true),
        credits: billing,
        reads_per_credit: x_inbox::READS_PER_CREDIT,
        estimate: Estimate {
            per_mention_usd: e.per_mention_usd,
            per_dm_usd: e.per_dm_usd,
            idle_month_usd: e.idle_month_usd,
            projected_month_usd: e.projected_month_usd,
        },
        limits: Limits {
            min_poll_minutes: x_inbox::MIN_POLL_MINUTES,
            max_poll_minutes: x_inbox::MAX_POLL_MINUTES,
            default_poll_minutes: x_inbox::DEFAULT_POLL_MINUTES,
            min_budget_usd: usd(x_inbox::MIN_BUDGET_MICROS),
            max_budget_usd: usd(x_inbox::MAX_BUDGET_MICROS),
            default_budget_usd: usd(x_inbox::DEFAULT_BUDGET_MICROS),
        },
    }
}

async fn account_of(
    app: &AppState,
    team: Uuid,
    id: Uuid,
) -> Result<Option<social_accounts::Model>, sea_orm::DbErr> {
    social_accounts::Entity::find_by_id(id)
        .filter(social_accounts::Column::TeamId.eq(team))
        .filter(social_accounts::Column::Provider.eq("x"))
        .one(&app.db)
        .await
}

/// The X inbox settings of an X account: off, 60 minutes and $2 a month until set. Any member.
#[utoipa::path(get, path = "/connections/{id}/x-inbox", tag = "inbox",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = XInbox), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, Path(id): Path<Uuid>) -> Response {
    let found = async {
        let Some(account) = account_of(&app, t.team.id, id).await? else {
            return Ok(None);
        };
        let s = x_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        Ok::<_, sea_orm::DbErr>(Some(json(&account, &s, app.billing_enabled)))
    };
    match found.await {
        Ok(Some(j)) => Json(j).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "x inbox read"),
    }
}

/// Sets the settings. Owners and admins. `409 reconnect_required` when turning it on for an
/// account X refused; `422` `invalid_poll` (15 to 1440 minutes), `invalid_budget` ($0.10 to
/// $100), or `reconnect_for_dms` (DMs asked for on an account connected without DM permission).
#[utoipa::path(put, path = "/connections/{id}/x-inbox", tag = "inbox", request_body = XInboxInput,
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = XInbox), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<XInboxInput>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let done = async {
        let Some(account) = account_of(&app, t.team.id, id).await? else {
            return Ok(reply(StatusCode::NOT_FOUND, "not_found"));
        };
        let was = x_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        let budget = match input.budget_usd {
            Some(b) if !b.is_finite() => {
                return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_budget"))
            }
            Some(b) => (b * 1e6).round() as i64,
            None => was.budget_micros,
        };
        let choice = Choice {
            enabled: input.enabled.unwrap_or(was.enabled),
            read_dms: input.read_dms.unwrap_or(was.read_dms),
            poll_minutes: input.poll_minutes.unwrap_or(was.poll_minutes),
            budget_micros: budget,
        };
        if let Err(code) = choice.checked() {
            return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, code));
        }
        let g = Granted::of(account.meta["scope"].as_str());
        if choice.read_dms && !g.dms {
            return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, "reconnect_for_dms"));
        }
        if choice.enabled && !was.enabled && account.meta["reconnect"].as_bool() == Some(true) {
            return Ok(reply(StatusCode::CONFLICT, "reconnect_required"));
        }
        x_inbox::save(&app.db, id, &choice).await?;
        let s = x_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        Ok::<_, sea_orm::DbErr>(Json(json(&account, &s, app.billing_enabled)).into_response())
    };
    done.await.unwrap_or_else(|e| internal(e, "x inbox save"))
}

/// Starts polling again after a pause (`credits`, `usage_capped`, `errors`): add credits or raise
/// the cap in the X developer console first. Owners and admins. It reads at the next minute and
/// never past the month's budget.
#[utoipa::path(post, path = "/connections/{id}/x-inbox/resume", tag = "inbox",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = XInbox), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn resume(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let done = async {
        let Some(account) = account_of(&app, t.team.id, id).await? else {
            return Ok(None);
        };
        x_inbox::resume(&app.db, id).await?;
        let s = x_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        Ok::<_, sea_orm::DbErr>(Some(json(&account, &s, app.billing_enabled)))
    };
    match done.await {
        Ok(Some(j)) => Json(j).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "x inbox resume"),
    }
}

/// Likes, reposts, hides or shows again the post of an X comment or mention, once (X is not
/// asked twice). `204`. Every member; shares the reply rate limit; `409 action_in_progress` while another action on the item is going out. `422` with `error` and
/// `message` when X refuses (`x_reconnect_required` for an account connected without the
/// permission, `x_failed`, `rate_limited`, `not_repliable` for an item the action does not fit).
#[utoipa::path(post, path = "/inbox-items/{id}/x-action", tag = "inbox", request_body = ActionInput,
    security(("session" = []), ("api_token" = [])), params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 204), (status = 401, body = Error), (status = 404, body = Error),
        (status = 409, body = Error), (status = 422, body = Error), (status = 429, body = Error)))]
async fn act(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    ItemId(id): ItemId,
    Body(input): Body<ActionInput>,
) -> Response {
    let Some(action) = Action::parse(&input.action) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_action");
    };
    let found = async {
        let item = inbox_items::Entity::find_by_id(id)
            .filter(inbox_items::Column::TeamId.eq(t.team.id))
            .filter(inbox_items::Column::Platform.eq("x"))
            .one(&app.db)
            .await?;
        let Some(item) = item else { return Ok(None) };
        let account = social_accounts::Entity::find_by_id(item.social_account_id)
            .one(&app.db)
            .await?;
        Ok::<_, sea_orm::DbErr>(account.map(|a| (item, a)))
    };
    let (item, account) = match found.await {
        Ok(Some(found)) => found,
        Ok(None) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "x item read"),
    };
    if !app.post_limiter.hit(t.current.0.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    // One action per item at a time, so a double click cannot take two credits: the lock lives in
    // this transaction, held across the call.
    let guard = async {
        let txn = app.db.begin().await?;
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 2)) AS ok",
            [item.id.to_string().into()],
        );
        let ok: bool = match txn.query_one_raw(stmt).await? {
            Some(r) => r.try_get("", "ok")?,
            None => false,
        };
        Ok::<_, sea_orm::DbErr>(ok.then_some(txn))
    };
    let guard = match guard.await {
        Ok(Some(txn)) => txn,
        Ok(None) => return reply(StatusCode::CONFLICT, "action_in_progress"),
        Err(e) => return internal(e, "x action lock"),
    };
    let done = x_inbox::act(&app, &account, &item, action).await;
    let _ = guard.commit().await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            let status = match e {
                crate::connectors::bluesky::InboxError::RateLimited => {
                    StatusCode::TOO_MANY_REQUESTS
                }
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            let body = serde_json::json!({"error": e.code(), "message": e.message()});
            (status, Json(body)).into_response()
        }
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(show, update))
        .routes(routes!(resume))
        .routes(routes!(act))
}
