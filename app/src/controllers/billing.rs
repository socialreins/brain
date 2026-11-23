//! A team's plan and credits, read-only for now. Any member may read them (the app needs the
//! plan to show Upgrade); the payment provider writes them through its webhook, which comes
//! with the checkout. With `BILLING_ENABLED` off, `GET /team/billing` says `enabled: false` and
//! everything is allowed.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{auth::internal, teams::InTeam, waitlist::Error};
use crate::{
    billing::{self, Kind, Limit, Limits},
    state::AppState,
};

#[derive(Serialize, ToSchema)]
pub struct PlanLimits {
    /// Connected accounts allowed; `null` is unlimited.
    pub accounts: Option<i64>,
    /// Members allowed; `null` is unlimited.
    pub members: Option<i64>,
    /// The inbox, automations, broadcasts and newsletters.
    pub automation: bool,
    /// Contacts the automations may keep; `null` is unlimited.
    pub contacts: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct Usage {
    pub accounts: i64,
    pub members: i64,
}

#[derive(Serialize, ToSchema)]
pub struct Plan {
    pub id: String,
    pub name: String,
    /// `trialing`, `active`, `past_due` or `canceled`.
    pub status: String,
    pub trial_ends_at: Option<DateTime<Utc>>,
    pub current_period_end: Option<DateTime<Utc>>,
    /// The plan ends at the period's end and does not renew.
    pub cancel_at_period_end: bool,
    /// A free plan an admin gave: nothing to pay, nothing to manage in Stripe.
    pub comped: bool,
}

#[derive(Serialize, ToSchema)]
pub struct Billing {
    /// `false` on a self-hosted install: nothing is capped.
    pub enabled: bool,
    /// The team's subscription; `null` when it has none.
    pub plan: Option<Plan>,
    /// What the team may use now. With billing on and no live plan, nothing.
    pub limits: PlanLimits,
    pub usage: Usage,
}

#[derive(Serialize, ToSchema)]
pub struct Credits {
    /// Credits for posts to X.
    pub x: i64,
    /// Credits for AI calls that run on our key.
    pub ai: i64,
}

fn limits(l: Limits) -> PlanLimits {
    PlanLimits {
        accounts: l.accounts,
        members: l.members,
        automation: l.automation,
        contacts: l.contacts,
    }
}

async fn count(app: &AppState, table: &str, team: uuid::Uuid) -> Result<i64, sea_orm::DbErr> {
    // `table` is one of two literals below, never input.
    let sql = format!("SELECT count(*)::bigint AS n FROM {table} WHERE team_id = $1");
    let row = app
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            vec![team.into()],
        ))
        .await?;
    row.map_or(Ok(0), |r| r.try_get("", "n"))
}

/// `402 {"error": "plan_limit", "limit": ...}`: the plan does not allow this.
pub(crate) fn plan_limit(limit: Limit) -> Response {
    let body = serde_json::json!({"error": "plan_limit", "limit": limit.as_str()});
    (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response()
}

/// `Some(reply)` when the team's plan has no automation (the inbox, automations, broadcasts and
/// newsletters); `None` when it does, or billing is off.
pub(crate) async fn needs_automation(app: &AppState, team: uuid::Uuid) -> Option<Response> {
    match billing::limits_of(app.billing_enabled, &app.db, team).await {
        Ok(l) if l.automation => None,
        Ok(_) => Some(plan_limit(Limit::Automation)),
        Err(e) => Some(internal(e, "plan read")),
    }
}

/// `Some(402 plan_limit "plan")` when billing is on and the team has no live plan: publishing
/// now or retrying waits for one. `None` otherwise.
pub(crate) async fn needs_plan(app: &AppState, team: uuid::Uuid) -> Option<Response> {
    if !app.billing_enabled {
        return None;
    }
    match billing::has_plan(&app.db, team).await {
        Ok(true) => None,
        Ok(false) => Some(plan_limit(Limit::Plan)),
        Err(e) => Some(internal(e, "plan read")),
    }
}

/// The current team's plan, what it allows and what it uses. Any member.
#[utoipa::path(get, path = "/team/billing", tag = "billing",
    responses((status = 200, body = Billing), (status = 401, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam) -> Response {
    let sub = match billing::subscription(&app.db, t.team.id).await {
        Ok(s) => s,
        Err(e) => return internal(e, "billing read"),
    };
    let (accounts, members) = match (
        count(&app, "social_accounts", t.team.id).await,
        count(&app, "team_user", t.team.id).await,
    ) {
        (Ok(a), Ok(m)) => (a, m),
        (Err(e), _) | (_, Err(e)) => return internal(e, "billing usage"),
    };
    Json(Billing {
        enabled: app.billing_enabled,
        limits: limits(billing::effective(app.billing_enabled, sub.as_ref())),
        plan: sub.map(|s| Plan {
            id: s.plan_id,
            name: s.plan_name,
            status: s.status,
            trial_ends_at: s.trial_ends_at,
            current_period_end: s.current_period_end,
            cancel_at_period_end: s.cancel_at_period_end,
            comped: s.provider.as_deref() == Some("comp"),
        }),
        usage: Usage { accounts, members },
    })
    .into_response()
}

/// The current team's credit balances. Any member.
#[utoipa::path(get, path = "/team/credits", tag = "billing",
    responses((status = 200, body = Credits), (status = 401, body = Error)))]
async fn credits(State(app): State<AppState>, t: InTeam) -> Response {
    let read = |kind| billing::balance(&app.db, t.team.id, kind);
    match (read(Kind::X).await, read(Kind::Ai).await) {
        (Ok(x), Ok(ai)) => Json(Credits { x, ai }).into_response(),
        (Err(e), _) | (_, Err(e)) => internal(e, "credits read"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(show))
        .routes(routes!(credits))
}
