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
use crate::{billing, state::AppState, stripe::PACKS};

/// Largest delivery taken; a subscription event is a few KB.
pub(crate) const MAX_BODY_BYTES: usize = 256 * 1024;
/// The free trial of a team's first subscription.
const TRIAL_DAYS: u32 = 7;

#[derive(Deserialize, ToSchema)]
pub struct CheckoutInput {
    /// `scheduler` or `automation`.
    pub plan: String,
    /// `year` (the default, two months free) or `month`.
    #[serde(default = "yearly")]
    pub interval: String,
}

fn yearly() -> String {
    "year".into()
}

#[derive(Deserialize, ToSchema)]
pub struct CreditsInput {
    /// `x100` (100 X posts) or `x500` (500 X posts).
    pub pack: String,
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

/// The key of the person who would get a trial, when they have not had one: `Some` means the
/// trial is on offer. Trials are per person (decision 0015), so a new team does not start another.
pub(crate) async fn trial_offer(app: &AppState, email: &str) -> Result<Option<String>, DbErr> {
    let Some(key) = socialreins_core::email::person_key(email) else {
        return Ok(None);
    };
    let used = app
        .db
        .query_one_raw(stmt(
            "SELECT 1 FROM trial_uses WHERE person_key = $1",
            vec![key.clone().into()],
        ))
        .await?;
    Ok(used.is_none().then_some(key))
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
/// first subscription has a 7-day trial, once for each person (a person who had a trial in any
/// team pays at once, and so does a team that had a subscription before). `409
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
    let Some(price) = app.stripe.price_of(&body.plan, &body.interval) else {
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
        // USD only (decision 0006): the price matches the pricing page, no conversion fee.
        ("adaptive_pricing[enabled]".to_owned(), "false".to_owned()),
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
        let offer = match trial_offer(&app, &t.current.0.email).await {
            Ok(o) => o,
            Err(e) => return internal(e, "trial read"),
        };
        if let Some(key) = offer {
            let days = TRIAL_DAYS.to_string();
            form.push(("subscription_data[trial_period_days]".to_owned(), days));
            // The webhook writes the trial down under this key once Stripe starts it.
            form.push(("subscription_data[metadata][trial_key]".to_owned(), key));
        }
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

/// Buys a pack of X credits: answers a Stripe Checkout page for a one-time payment. The credits
/// are granted when Stripe reports the payment (`checkout.session.completed`), once. `422
/// invalid_pack`, `503 billing_unavailable` when billing, Stripe or the pack prices are off, `502
/// stripe_failed`. Same checks as checkout.
#[utoipa::path(post, path = "/team/billing/credits", tag = "billing",
    request_body = CreditsInput,
    responses((status = 200, body = Redirect), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error), (status = 502, body = Error),
        (status = 503, body = Error)))]
async fn buy_credits(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<CreditsInput>,
) -> Response {
    if let Some(res) = gate(&app, &t) {
        return res;
    }
    if !app.stripe.packs_on() {
        return reply(StatusCode::SERVICE_UNAVAILABLE, "billing_unavailable");
    }
    let Some(price) = app.stripe.pack_price(&body.pack) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_pack");
    };
    let customer = match held(&app, t.team.id).await {
        Ok(s) => s.and_then(|(_, customer)| customer),
        Err(e) => return internal(e, "billing read"),
    };
    let team = t.team.id.to_string();
    let web = &app.web_url;
    let mut form = vec![
        ("mode".to_owned(), "payment".to_owned()),
        ("line_items[0][price]".to_owned(), price.to_owned()),
        ("line_items[0][quantity]".to_owned(), "1".to_owned()),
        ("client_reference_id".to_owned(), team.clone()),
        ("metadata[team_id]".to_owned(), team),
        ("metadata[pack]".to_owned(), body.pack.clone()),
        // On the payment too, so a refund or dispute (charge events) names the team and pack.
        (
            "payment_intent_data[metadata][team_id]".to_owned(),
            t.team.id.to_string(),
        ),
        (
            "payment_intent_data[metadata][pack]".to_owned(),
            body.pack.clone(),
        ),
        ("adaptive_pricing[enabled]".to_owned(), "false".to_owned()),
        (
            "success_url".to_owned(),
            format!("{web}/billing?credits=success"),
        ),
        (
            "cancel_url".to_owned(),
            format!("{web}/billing?credits=canceled"),
        ),
    ];
    match customer {
        Some(customer) => form.push(("customer".to_owned(), customer)),
        None => form.push(("customer_email".to_owned(), t.current.0.email.clone())),
    }
    match app.stripe.checkout(&form).await {
        Ok(url) => Json(Redirect { url }).into_response(),
        Err(e) => {
            tracing::error!(status = ?e.0, code = %e.1, "stripe credits checkout failed");
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
        .routes(routes!(buy_credits))
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
    /// Written; `true` when it ended a live plan (the owners are told).
    Applied(bool),
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
    let mut was_live = false;
    if let Some(row) = current {
        let status_now: String = row.try_get("", "status")?;
        let sub_now: Option<String> = row.try_get("", "provider_subscription_id")?;
        was_live = billing::live(&status_now);
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
                      OR subscriptions.status NOT IN ('trialing', 'active', 'past_due') \
                      OR (subscriptions.provider = 'comp' \
                          AND EXCLUDED.status IN ('trialing', 'active', 'past_due')))";
    // A comped team that pays anyway (a checkout opened before the comp) takes the paid plan;
    // a late event that ends some older subscription never ends the comp.
    let written = db
        .execute_raw(stmt(
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
        .await?
        .rows_affected()
        == 1;
    // A trial that really began is that person's one trial, whatever happens to the team later.
    if let (true, Some(key), Some(_)) = (
        written,
        sub["metadata"]["trial_key"]
            .as_str()
            .filter(|k| !k.is_empty()),
        time(&sub["trial_end"]),
    ) {
        db.execute_raw(stmt(
            "INSERT INTO trial_uses (person_key, team_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            vec![key.into(), team.into()],
        ))
        .await?;
    }
    Ok(Outcome::Applied(
        written && was_live && !billing::live(status),
    ))
}

/// A billing email to a team's owners, sent after the event is committed.
enum Notice {
    /// The trial ends at this time (`customer.subscription.trial_will_end`, 3 days before).
    TrialEnds(DateTime<Utc>),
    /// The first failed renewal (`invoice.payment_failed`); Stripe retries for 7 days.
    PaymentFailed,
    /// A live plan ended (cancelled, or the retries ran out).
    Ended,
}

/// The team whose subscription row holds this Stripe subscription id, only while `live` (when
/// asked): a mail goes to the team brain itself recorded, never to one named in the event.
async fn team_holding(
    db: &impl ConnectionTrait,
    sub: Option<&str>,
    live: bool,
) -> Result<Option<Uuid>, DbErr> {
    let Some(sub) = sub else { return Ok(None) };
    db.query_one_raw(stmt(
        "SELECT team_id FROM subscriptions WHERE provider = 'stripe' \
         AND provider_subscription_id = $1 \
         AND (NOT $2 OR status IN ('trialing', 'active', 'past_due'))",
        vec![sub.into(), live.into()],
    ))
    .await?
    .map(|r| r.try_get("", "team_id"))
    .transpose()
}

/// The team of a failed invoice's subscription, on its first attempt only: Stripe sends the
/// event for every retry, and the owner needs telling once.
async fn first_failure(db: &impl ConnectionTrait, invoice: &Value) -> Result<Option<Uuid>, DbErr> {
    if invoice["attempt_count"].as_i64() != Some(1) {
        return Ok(None);
    }
    // Newer API versions moved the subscription under `parent`.
    let sub = invoice["subscription"]
        .as_str()
        .or_else(|| invoice["parent"]["subscription_details"]["subscription"].as_str());
    // Only a live plan: "your team keeps working" would be wrong for one already ended.
    team_holding(db, sub, true).await
}

/// Mails the team's owners (billing is theirs). A failed send is logged, never retried: the
/// event is already taken.
async fn tell_owners(app: &AppState, team: Uuid, notice: Notice) {
    let found = async {
        let rows = app
            .db
            .query_all_raw(stmt(
                "SELECT u.email FROM team_user t JOIN users u ON u.id = t.user_id \
                 WHERE t.team_id = $1 AND t.role = 'owner' ORDER BY u.email",
                vec![team.into()],
            ))
            .await?;
        let plan = app
            .db
            .query_one_raw(stmt(
                "SELECT p.name FROM subscriptions s JOIN plans p ON p.id = s.plan_id WHERE s.team_id = $1",
                vec![team.into()],
            ))
            .await?
            .map(|r| r.try_get::<String>("", "name"))
            .transpose()?;
        let emails = rows
            .iter()
            .map(|r| r.try_get::<String>("", "email"))
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, DbErr>((emails, plan.unwrap_or_else(|| "your plan".into())))
    };
    let (to, plan) = match found.await {
        Ok(f) => f,
        Err(e) => return tracing::error!(error = %e, %team, "billing mail: owners not read"),
    };
    let billing = format!("{}/billing", app.web_url);
    let (subject, text) = match notice {
        Notice::TrialEnds(at) => (
            format!("Your SocialReins trial ends on {}", at.format("%Y-%m-%d")),
            format!(
                "Your free trial of {plan} ends on {}. After that, {plan} is charged to the card you \
                 gave at checkout, and renews until you cancel.\n\nTo change the plan or cancel before \
                 then, open Billing and choose Manage billing:\n\n{billing}\n",
                at.format("%Y-%m-%d")
            ),
        ),
        Notice::PaymentFailed => (
            "Your SocialReins payment failed".to_owned(),
            format!(
                "We couldn't charge your card for {plan}. Your team keeps working while the card is \
                 tried again over the next 7 days. If it still fails, the plan ends and scheduled \
                 posts stop publishing; nothing is deleted.\n\nTo update the card, open Billing and \
                 choose Manage billing:\n\n{billing}\n"
            ),
        ),
        Notice::Ended => (
            "Your SocialReins plan has ended".to_owned(),
            format!(
                "{plan} has ended for your team, so scheduled posts no longer publish. Nothing is \
                 deleted: your posts, accounts and settings are kept.\n\nChoose a plan again to \
                 pick up where you left off:\n\n{billing}\n"
            ),
        ),
    };
    for email in to {
        if let Err(e) = app
            .mailer
            .send(&email, &subject, text.clone(), &app.web_url)
            .await
        {
            tracing::warn!(error = %e, %team, "billing mail not sent");
        }
    }
}

/// Grants the credits of a paid one-time Checkout: the team and pack come only from the metadata
/// brain put on the session, and the session id keeps the grant to once. A subscription
/// checkout, or one not paid yet (an async payment follows with its own event), is taken and
/// changes nothing. `false` (ignored) for no known team or pack.
async fn apply_purchase(db: &impl ConnectionTrait, session: &Value) -> Result<bool, DbErr> {
    if session["mode"] != "payment" || session["payment_status"] != "paid" {
        return Ok(true);
    }
    let (Some(id), Some(team), Some(credits)) = (
        session["id"].as_str(),
        session["metadata"]["team_id"]
            .as_str()
            .and_then(|t| t.parse::<Uuid>().ok()),
        session["metadata"]["pack"]
            .as_str()
            .and_then(|p| PACKS.iter().find(|(id, _)| *id == p))
            .map(|(_, n)| *n),
    ) else {
        // Charged with nothing granted: a person must grant or refund it.
        tracing::error!(session = ?session["id"].as_str(), "paid stripe session with no team or pack");
        return Ok(false);
    };
    let known = db
        .query_one_raw(stmt("SELECT 1 FROM teams WHERE id = $1", vec![team.into()]))
        .await?;
    if known.is_none() {
        tracing::error!(%team, "paid stripe session for an unknown team");
        return Ok(false);
    }
    billing::grant(db, team, billing::Kind::X, credits, "x pack", Some(id)).await?;
    Ok(true)
}

/// `POST /webhooks/stripe`: `403` unless `Stripe-Signature` signs the raw body (timestamp within
/// 5 minutes), `404` while `STRIPE_WEBHOOK_SECRET` is unset. The event id is stored with the
/// change in one transaction, so a delivery seen before changes nothing and a failure is a
/// `500` that Stripe retries. Handled: `customer.subscription.created`, `.updated` and
/// `.deleted`, and `checkout.session.completed` and `.async_payment_succeeded` (credit packs).
/// The owners are mailed when a live plan ends, on `customer.subscription.trial_will_end` and on
/// the first `invoice.payment_failed` of an invoice. Every other event is taken and ignored.
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
        let paid = matches!(
            kind,
            "checkout.session.completed" | "checkout.session.async_payment_succeeded"
        );
        // shortcut: a refunded or disputed pack keeps its credits; a person takes them back with
        // the admin tools. Automate it if refunds become common.
        if matches!(kind, "charge.refunded" | "charge.dispute.created") {
            let meta = &event["data"]["object"]["metadata"];
            tracing::error!(kind, team = ?meta["team_id"].as_str(), pack = ?meta["pack"].as_str(),
                "stripe pack payment refunded or disputed: review its credits");
        }
        if fresh && paid && !apply_purchase(&txn, &event["data"]["object"]).await? {
            return Ok((None, None));
        }
        let object = &event["data"]["object"];
        let mut extra = None;
        let mut notice = None;
        if fresh && subscription {
            match apply_subscription(&app, &txn, object, at).await? {
                Outcome::Applied(ended) => {
                    if ended {
                        notice = team_holding(&txn, object["id"].as_str(), false)
                            .await?
                            .map(|t| (t, Notice::Ended));
                    }
                }
                // Dropping the transaction takes the event id back with it.
                Outcome::Ignored => return Ok::<_, DbErr>((None, None)),
                Outcome::Extra(sub) => extra = Some(sub),
            }
        }
        // A trial already cancelled is not charged: the plan-ended mail follows instead.
        let cancelling = object["cancel_at_period_end"].as_bool() == Some(true)
            || object["cancel_at"].as_i64().is_some();
        if fresh && kind == "customer.subscription.trial_will_end" && !cancelling {
            // A trial ended early by hand fires this too, with an end already past: no mail.
            if let Some(end) = time(&object["trial_end"]).filter(|end| *end > Utc::now()) {
                notice = team_holding(&txn, object["id"].as_str(), true)
                    .await?
                    .map(|t| (t, Notice::TrialEnds(end)));
            }
        }
        if fresh && kind == "invoice.payment_failed" {
            notice = first_failure(&txn, object)
                .await?
                .map(|t| (t, Notice::PaymentFailed));
        }
        txn.commit().await?;
        Ok((extra, notice))
    };
    match handled.await {
        Ok((extra, notice)) => {
            // After the commit, so a failed call leaves the event taken; it is logged for a
            // person to cancel (and refund) in Stripe.
            if let Some(sub) = extra {
                if let Err(e) = app.stripe.cancel(&sub).await {
                    tracing::error!(subscription = %sub, status = ?e.0, code = %e.1, "extra stripe subscription not cancelled");
                }
            }
            // In the background: a slow mail service must not hold Stripe's answer.
            if let Some((team, notice)) = notice {
                let app = app.clone();
                tokio::spawn(async move { tell_owners(&app, team, notice).await });
            }
            StatusCode::OK.into_response()
        }
        Err(e) => internal(e, "stripe webhook"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/stripe", post(receive))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
}
