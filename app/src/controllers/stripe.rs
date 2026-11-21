//! Stripe for Cloud billing. `POST /v1/team/billing/checkout` and `/portal` send the owner to
//! Stripe; `POST /webhooks/stripe` is how Stripe tells brain what happened. The webhook is the
//! only thing that writes a team's subscription (`subscriptions`), and a team is only trusted
//! through the id brain put in the subscription's metadata, never through anything the browser
//! sends back.
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use socialreins_core::team::Role;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{billing, state::AppState};

/// Largest delivery taken; a subscription event is a few KB.
pub(crate) const MAX_BODY_BYTES: usize = 256 * 1024;
/// The free trial of a team's first subscription.
const TRIAL_DAYS: u32 = 7;

#[derive(Deserialize, ToSchema)]
pub struct CheckoutInput {
    /// `scheduler` or `automation`.
    pub plan: String,
}

#[derive(Serialize, ToSchema)]
pub struct Redirect {
    /// A Stripe page to send the browser to.
    pub url: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// `(status, customer id)` of the team's subscription row, if it has one.
async fn held(app: &AppState, team: Uuid) -> Result<Option<(String, Option<String>)>, DbErr> {
    let row = app
        .db
        .query_one_raw(stmt(
            "SELECT status, provider_customer_id FROM subscriptions WHERE team_id = $1",
            vec![team.into()],
        ))
        .await?;
    row.map(|r| {
        Ok((
            r.try_get("", "status")?,
            r.try_get("", "provider_customer_id")?,
        ))
    })
    .transpose()
}

/// The owner's checks every call here shares: only an owner, with a sign-in from the last 10
/// minutes, and only where billing is on and Stripe is set up.
fn gate(app: &AppState, t: &InTeam) -> Option<Response> {
    if t.role != Role::Owner {
        return Some(reply(StatusCode::FORBIDDEN, "forbidden"));
    }
    if !t.current.recent() {
        return Some(reply(StatusCode::FORBIDDEN, "reauth_required"));
    }
    if !app.billing_enabled || !app.stripe.configured() {
        return Some(reply(
            StatusCode::SERVICE_UNAVAILABLE,
            "billing_unavailable",
        ));
    }
    None
}

/// Starts a subscription: answers the Stripe Checkout page to send the browser to. The team's
/// first subscription has a 7-day trial; a team that had one before pays at once. `409
/// already_subscribed` while a plan is live (change it in the portal), `422 invalid_plan`,
/// `503 billing_unavailable` when billing or Stripe is off, `502 stripe_failed` when Stripe
/// refuses. Owners only, with a sign-in from the last 10 minutes (else `403 reauth_required`).
#[utoipa::path(post, path = "/team/billing/checkout", tag = "billing",
    request_body = CheckoutInput,
    responses((status = 200, body = Redirect), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error),
        (status = 502, body = Error), (status = 503, body = Error)))]
async fn checkout(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<CheckoutInput>,
) -> Response {
    if let Some(res) = gate(&app, &t) {
        return res;
    }
    let Some(price) = app.stripe.price_of(&body.plan) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_plan");
    };
    let sub = match held(&app, t.team.id).await {
        Ok(s) => s,
        Err(e) => return internal(e, "billing read"),
    };
    if sub
        .as_ref()
        .is_some_and(|(status, _)| billing::live(status))
    {
        return reply(StatusCode::CONFLICT, "already_subscribed");
    }
    let team = t.team.id.to_string();
    let web = &app.web_url;
    let mut form = vec![
        ("mode".to_owned(), "subscription".to_owned()),
        ("line_items[0][price]".to_owned(), price.to_owned()),
        ("line_items[0][quantity]".to_owned(), "1".to_owned()),
        ("client_reference_id".to_owned(), team.clone()),
        ("subscription_data[metadata][team_id]".to_owned(), team),
        ("allow_promotion_codes".to_owned(), "true".to_owned()),
        (
            "success_url".to_owned(),
            format!("{web}/billing?checkout=success"),
        ),
        (
            "cancel_url".to_owned(),
            format!("{web}/billing?checkout=canceled"),
        ),
    ];
    if sub.is_none() {
        let days = TRIAL_DAYS.to_string();
        form.push(("subscription_data[trial_period_days]".to_owned(), days));
    }
    match sub.and_then(|(_, customer)| customer) {
        Some(customer) => form.push(("customer".to_owned(), customer)),
        None => form.push(("customer_email".to_owned(), t.current.0.email.clone())),
    }
    match app.stripe.checkout(&form).await {
        Ok(url) => Json(Redirect { url }).into_response(),
        Err(e) => {
            tracing::error!(status = ?e.0, code = %e.1, "stripe checkout failed");
            reply(StatusCode::BAD_GATEWAY, "stripe_failed")
        }
    }
}

/// Answers the Stripe customer portal page, where the owner changes the card, switches plan or
/// cancels. `409 no_customer` before any subscription. Same checks as checkout.
#[utoipa::path(post, path = "/team/billing/portal", tag = "billing",
    responses((status = 200, body = Redirect), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 502, body = Error),
        (status = 503, body = Error)))]
async fn portal(_: FromWeb, State(app): State<AppState>, t: InTeam) -> Response {
    if let Some(res) = gate(&app, &t) {
        return res;
    }
    let customer = match held(&app, t.team.id).await {
        Ok(s) => s.and_then(|(_, customer)| customer),
        Err(e) => return internal(e, "billing read"),
    };
    let Some(customer) = customer else {
        return reply(StatusCode::CONFLICT, "no_customer");
    };
    let back = format!("{}/billing", app.web_url);
    match app.stripe.portal(&customer, &back).await {
        Ok(url) => Json(Redirect { url }).into_response(),
        Err(e) => {
            tracing::error!(status = ?e.0, code = %e.1, "stripe portal failed");
            reply(StatusCode::BAD_GATEWAY, "stripe_failed")
        }
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(checkout))
        .routes(routes!(portal))
}

/// Stripe's subscription status as ours; `None` for one that has not started (`incomplete`).
/// `unpaid` (retries ran out), `paused` and expired ones are not live.
fn status_of(stripe: &str) -> Option<&'static str> {
    match stripe {
        "trialing" => Some("trialing"),
        "active" => Some("active"),
        "past_due" => Some("past_due"),
        "canceled" | "unpaid" | "paused" | "incomplete_expired" => Some("canceled"),
        _ => None,
    }
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(v.as_i64()?, 0)
}

/// What a subscription event came to.
enum Outcome {
    Applied,
    /// Not ours to apply (no team, no plan, not started): nothing is stored, not even the event
    /// id, so sending it again from the Stripe dashboard works once the cause is mended.
    Ignored,
    /// A live subscription while the team already has another live one: the team is billed
    /// twice. The new one is kept out of the books and cancelled in Stripe.
    Extra(String),
}

/// Writes one subscription event. Which team: the id in the subscription's metadata, else the
/// team already holding that subscription. An event for no team, for a price that is no plan,
/// or older than the last one applied changes nothing.
async fn apply_subscription(
    app: &AppState,
    db: &impl ConnectionTrait,
    sub: &Value,
    at: DateTime<Utc>,
) -> Result<Outcome, DbErr> {
    let id = sub["id"].as_str().unwrap_or_default();
    let team = match sub["metadata"]["team_id"]
        .as_str()
        .and_then(|t| t.parse::<Uuid>().ok())
    {
        Some(team) => Some(team),
        None => db
            .query_one_raw(stmt(
                "SELECT team_id FROM subscriptions \
                 WHERE provider = 'stripe' AND provider_subscription_id = $1",
                vec![id.into()],
            ))
            .await?
            .map(|r| r.try_get("", "team_id"))
            .transpose()?,
    };
    let item = &sub["items"]["data"][0];
    let plan = item["price"]["id"]
        .as_str()
        .and_then(|p| app.stripe.plan_of(p));
    let status = sub["status"].as_str().and_then(status_of);
    let (Some(team), Some(plan), Some(status)) = (team, plan, status) else {
        tracing::warn!(subscription = id, "stripe subscription event ignored");
        return Ok(Outcome::Ignored);
    };
    let known = db
        .query_one_raw(stmt("SELECT 1 FROM teams WHERE id = $1", vec![team.into()]))
        .await?;
    if known.is_none() {
        tracing::warn!(subscription = id, "stripe subscription of an unknown team");
        return Ok(Outcome::Ignored);
    }
    // The team row is locked (a subscription row may not exist yet), so two events for one
    // team take turns and the second sees what the first wrote.
    db.execute_raw(stmt(
        "SELECT 1 FROM teams WHERE id = $1 FOR NO KEY UPDATE",
        vec![team.into()],
    ))
    .await?;
    let current = db
        .query_one_raw(stmt(
            "SELECT status, provider_subscription_id FROM subscriptions WHERE team_id = $1",
            vec![team.into()],
        ))
        .await?;
    if let Some(row) = current {
        let status_now: String = row.try_get("", "status")?;
        let sub_now: Option<String> = row.try_get("", "provider_subscription_id")?;
        if billing::live(&status_now)
            && billing::live(status)
            && sub_now.as_deref().is_some_and(|s| s != id)
        {
            tracing::error!(%team, subscription = id, "second live stripe subscription");
            return Ok(Outcome::Extra(id.to_owned()));
        }
    }
    // Stripe moved the period end into the item in newer API versions.
    let period_end = time(&sub["current_period_end"]).or_else(|| time(&item["current_period_end"]));
    let sql = "INSERT INTO subscriptions (team_id, plan_id, status, provider, provider_customer_id, \
                 provider_subscription_id, trial_ends_at, current_period_end, cancel_at_period_end, \
                 provider_event_at, updated_at) \
               VALUES ($1, $2, $3, 'stripe', $4, $5, $6, $7, $8, $9, now()) \
               ON CONFLICT (team_id) DO UPDATE SET plan_id = EXCLUDED.plan_id, \
                 status = EXCLUDED.status, provider = 'stripe', \
                 provider_customer_id = EXCLUDED.provider_customer_id, \
                 provider_subscription_id = EXCLUDED.provider_subscription_id, \
                 trial_ends_at = EXCLUDED.trial_ends_at, \
                 current_period_end = EXCLUDED.current_period_end, \
                 cancel_at_period_end = EXCLUDED.cancel_at_period_end, \
                 provider_event_at = EXCLUDED.provider_event_at, updated_at = now() \
               WHERE (subscriptions.provider_event_at IS NULL \
                      OR subscriptions.provider_event_at <= EXCLUDED.provider_event_at) \
                 AND (subscriptions.provider_subscription_id IS NOT DISTINCT FROM \
                        EXCLUDED.provider_subscription_id \
                      OR subscriptions.status NOT IN ('trialing', 'active', 'past_due'))";
    db.execute_raw(stmt(
        sql,
        vec![
            team.into(),
            plan.into(),
            status.into(),
            sub["customer"].as_str().map(str::to_owned).into(),
            id.into(),
            time(&sub["trial_end"]).into(),
            period_end.into(),
            sub["cancel_at_period_end"]
                .as_bool()
                .unwrap_or(false)
                .into(),
            at.into(),
        ],
    ))
    .await?;
    Ok(Outcome::Applied)
}

/// `POST /webhooks/stripe`: `403` unless `Stripe-Signature` signs the raw body (timestamp within
/// 5 minutes), `404` while `STRIPE_WEBHOOK_SECRET` is unset. The event id is stored with the
/// change in one transaction, so a delivery seen before changes nothing and a failure is a
/// `500` that Stripe retries. Handled: `customer.subscription.created`, `.updated` and
/// `.deleted`; every other event is taken and ignored.
async fn receive(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !app.stripe.webhook_on() {
        return reply(StatusCode::NOT_FOUND, "not_found");
    }
    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !app.stripe.signed(&body, signature, Utc::now().timestamp()) {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        tracing::warn!("stripe webhook: signed body is not JSON");
        return StatusCode::OK.into_response();
    };
    let (Some(id), Some(kind)) = (event["id"].as_str(), event["type"].as_str()) else {
        return StatusCode::OK.into_response();
    };
    let at = time(&event["created"]).unwrap_or_else(Utc::now);
    let handled = async {
        let txn = app.db.begin().await?;
        let fresh = txn
            .execute_raw(stmt(
                "INSERT INTO stripe_events (id, type) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                vec![id.into(), kind.into()],
            ))
            .await?
            .rows_affected()
            == 1;
        let subscription = matches!(
            kind,
            "customer.subscription.created"
                | "customer.subscription.updated"
                | "customer.subscription.deleted"
        );
        let mut extra = None;
        if fresh && subscription {
            match apply_subscription(&app, &txn, &event["data"]["object"], at).await? {
                Outcome::Applied => {}
                // Dropping the transaction takes the event id back with it.
                Outcome::Ignored => return Ok::<_, DbErr>(None),
                Outcome::Extra(sub) => extra = Some(sub),
            }
        }
        txn.commit().await?;
        Ok(extra)
    };
    match handled.await {
        Ok(Some(sub)) => {
            // After the commit, so a failed call leaves the event taken; it is logged for a
            // person to cancel (and refund) in Stripe.
            if let Err(e) = app.stripe.cancel(&sub).await {
                tracing::error!(subscription = %sub, status = ?e.0, code = %e.1, "extra stripe subscription not cancelled");
            }
            StatusCode::OK.into_response()
        }
        Ok(None) => StatusCode::OK.into_response(),
        Err(e) => internal(e, "stripe webhook"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/stripe", post(receive))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
}
