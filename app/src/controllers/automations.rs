//! Automations of the current team: trigger, conditions and actions (see `crate::automations`).
//! Members read them and their runs; owners and admins create, change, switch and delete them,
//! as with inboxes and sets.
use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::numbered::{self, AutomationId, ItemId, RunId, ID_PARAM};
use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    automation_misses::{self, Miss, Verdict},
    automations::{self as rules, Action, ActionResult, Conditions, Step},
    dry_run::{self, Outcome, Sample},
    inbox,
    models::{automation_runs, automations, inbox_items, social_accounts, team_user, teams},
    state::AppState,
    x_automation,
};

/// Most automations one team holds.
const MAX_AUTOMATIONS: u64 = 50;
const PAGE: u64 = 50;

#[derive(Serialize, ToSchema)]
pub struct Automation {
    pub id: Uuid,
    /// The automation's number, unique over every team: the address web pages use. Every `{id}`
    /// of an automation route takes it as well as `id`.
    pub number: i64,
    pub name: String,
    pub active: bool,
    /// When it was last switched on; items received before that never run it.
    pub activated_at: DateTime<FixedOffset>,
    /// `button_clicked`, `comment_received`, `dm_received`, `like_received`,
    /// `mention_received`, `new_follower`, `repost_received`, `story_mention` or `story_reply`.
    pub trigger: String,
    pub conditions: Conditions,
    /// Empty for a flow.
    pub actions: Vec<Action>,
    /// A flow's steps, in order; null for an automation with `actions`.
    pub steps: Option<Vec<Step>>,
    /// A public reply sent when a `send_dm` could not go (`dm_not_allowed`, `no_dm_access`)
    /// and no `reply_publicly` went out.
    pub fallback_public_reply: Option<String>,
    /// Minutes before it answers the same person again (1 to 10080); null: no cool-down.
    pub cooldown_minutes: Option<i32>,
    /// Most runs of this automation in a day (1 to 500); null: only the team's daily cap.
    pub daily_cap: Option<i32>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    /// How often it ran, by status; in the list and on `GET /automations/{id}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<RunStats>,
}

#[derive(Serialize, ToSchema, Default, Debug, PartialEq, Eq)]
pub struct RunStats {
    pub sent: i64,
    pub skipped: i64,
    pub failed: i64,
    pub running: i64,
    /// Runs of a flow at a `wait` step.
    pub waiting: i64,
    pub total: i64,
    /// The same as `total`, kept for older clients.
    pub count: i64,
    pub last_at: Option<DateTime<FixedOffset>>,
    /// Why runs were skipped or failed, with how many each (`reason` of the run), most first.
    pub reasons: Vec<ReasonCount>,
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct ReasonCount {
    pub reason: String,
    pub count: i64,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AutomationInput {
    /// 1 to 60 characters after trimming.
    pub name: String,
    /// `button_clicked`, `comment_received`, `dm_received`, `like_received`,
    /// `mention_received`, `new_follower`, `repost_received`, `story_mention` (Instagram only, no keywords) or `story_reply` (see `Conditions`
    /// for the conditions each takes). `reply_publicly`, a fallback reply and `{page}` are for
    /// `comment_received` and `mention_received` only. `like_received` and `repost_received` (a
    /// like or repost of one of the account's posts, Bluesky only) take the scope and
    /// `post_remote_ids`, and only the actions that are no message and no tag: `follow_back`,
    /// `notify_team`, `assign` and `stop_automations`. `like_comment` (Facebook, Bluesky) and
    /// `repost_comment` (Bluesky) are for `comment_received` and `mention_received`.
    pub trigger: String,
    #[serde(default)]
    pub conditions: Conditions,
    /// 1 or 2 sends (`send_dm` or `ai_send_dm`, `reply_publicly` or `ai_reply_publicly`), at
    /// most one DM and one public reply, and beside them
    /// up to 3 `tag_contact`, each tag once, and once each `notify_team` (mails the owners and
    /// admins, 20 a day), `assign` (a member of the team), `hide_comment` (`comment_received`
    /// only; Facebook and Instagram), `like_comment` (Facebook, and Bluesky; on a comment or
    /// mention), `repost_comment` (Bluesky; on a comment or mention), `follow_back` (Bluesky) and
    /// `stop_automations` (the newer automations do not run on the item). A send or one of
    /// the others but a stop is needed. `like_received` and `repost_received` take no send and
    /// no tag. Either this or `steps`: both is `422 actions_and_steps`.
    #[serde(default)]
    pub actions: Option<Vec<Action>>,
    /// A flow instead of `actions`: 1 to 10 steps run in order, each a `send_dm` (with up to 3
    /// `buttons`), a `reply_publicly` (where the trigger takes one), an `ai_send_dm` or
    /// `ai_reply_publicly` (text written by AI from a `prompt`), a `wait` or a
    /// `tag_contact`, `notify_team`, `assign`, `hide_comment`, `like_comment`, `repost_comment`,
    /// `follow_back` or `stop_automations` as in `actions`. Never a wait first or last, and at least one send,
    /// mail or moderation step (`422 invalid_steps`).
    /// A send that does not go out ends the run.
    #[serde(default)]
    pub steps: Option<Vec<Step>>,
    /// Default `true`.
    #[serde(default = "yes")]
    pub active: bool,
    /// Needs a `send_dm` action; at most 300 graphemes; may hold `{name}` and
    /// `{page}` (set off by spaces; the automation does not run when the post has no live page).
    /// Not with `steps` (`422 invalid_fallback`).
    #[serde(default)]
    pub fallback_public_reply: Option<String>,
    /// Minutes, 1 to 10080: it does not answer the same person again within this time (the
    /// run is stored `skipped: cooldown`). Absent: no cool-down.
    #[serde(default)]
    pub cooldown_minutes: Option<u32>,
    /// 1 to 500: most runs of this automation in 24 hours (a run past it is stored `skipped:
    /// automation_cap`). Absent: only the team's daily cap.
    #[serde(default)]
    pub daily_cap: Option<u32>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, ToSchema)]
pub struct Run {
    pub id: Uuid,
    pub automation_id: Uuid,
    pub inbox_item_id: Uuid,
    /// The item's `number`; `null` once the item is gone.
    pub inbox_item_number: Option<i64>,
    /// `running`, `waiting` (a flow at a `wait` step), `sent`, `failed` or `skipped`. A flow's
    /// run is `sent` once every step went out, else it has the status of the send that ended it.
    pub status: String,
    /// Why not everything was sent: `dm_not_allowed`, `no_dm_access`, `rate_limited`,
    /// `no_public_page`, `daily_cap`, `reconnect_required`, `interrupted`, `bluesky_refused`, ...
    /// and, for a waiting run of a flow that was switched off or whose steps were changed,
    /// `automation_changed`.
    pub reason: Option<String>,
    pub results: Vec<ActionResult>,
    /// A flow's step last tried, from 0 (the `wait` itself while `waiting`); null for an
    /// automation with `actions`.
    pub step_index: Option<i32>,
    /// When a `waiting` run goes on; null otherwise.
    pub resume_at: Option<DateTime<FixedOffset>>,
    /// How often it was tried again after a rate limit refused a send (by itself up to 3
    /// times, 5, 10 and 15 minutes later, or by a person).
    pub retries: i32,
    /// A person may try it again (`POST /automations/{id}/runs/{run_id}/retry`): the platform
    /// rate limited a send, so nothing of it went out.
    pub can_retry: bool,
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
    /// Only runs with this status (`sent`, `skipped`, `failed`, `waiting` or `running`).
    pub status: Option<String>,
    /// Only runs with this reason (see `Run.reason`).
    pub reason: Option<String>,
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

fn run_json(
    r: automation_runs::Model,
    a: Option<&automations::Model>,
    items: &std::collections::HashMap<Uuid, i64>,
) -> Run {
    Run {
        can_retry: a.is_some_and(|a| rules::retryable(&r, a)),
        retries: r.retries,
        resume_at: r.resume_at.filter(|_| r.status == "waiting"),
        id: r.id,
        automation_id: r.automation_id,
        inbox_item_id: r.inbox_item_id,
        inbox_item_number: items.get(&r.inbox_item_id).copied(),
        status: r.status,
        reason: r.reason,
        results: serde_json::from_value(r.results).unwrap_or_default(),
        step_index: r.step_index,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

fn automation_json(a: automations::Model) -> Automation {
    Automation {
        id: a.id,
        number: a.number,
        name: a.name,
        active: a.active,
        activated_at: a.activated_at,
        trigger: a.trigger,
        conditions: serde_json::from_value(a.conditions).unwrap_or_default(),
        actions: serde_json::from_value(a.actions).unwrap_or_default(),
        steps: a.steps.and_then(|s| serde_json::from_value(s).ok()),
        fallback_public_reply: a.fallback_public_reply,
        cooldown_minutes: a.cooldown_minutes,
        daily_cap: a.daily_cap,
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
    db: &DatabaseConnection,
    (team_id, linkedin_on): (Uuid, Option<bool>),
    mut input: AutomationInput,
) -> Result<Result<(String, AutomationInput), Response>, DbErr> {
    let Some(name) = team::name(&input.name) else {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name")));
    };
    let in_range = |v: Option<u32>, max: u32| v.is_none_or(|v| (1..=max).contains(&v));
    if !in_range(input.cooldown_minutes, 10_080) {
        return Ok(Err(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_cooldown",
        )));
    }
    if !in_range(input.daily_cap, rules::RUNS_PER_DAY as u32) {
        return Ok(Err(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_daily_cap",
        )));
    }
    input.fallback_public_reply = input
        .fallback_public_reply
        .map(|f| f.trim().to_owned())
        .filter(|f| !f.is_empty());
    let fallback = input.fallback_public_reply.as_deref();
    let (trigger, conditions) = (input.trigger.as_str(), &mut input.conditions);
    let shape = match (&input.actions, &mut input.steps) {
        (Some(_), Some(_)) => Err("actions_and_steps"),
        (_, Some(_)) if fallback.is_some() => Err("invalid_fallback"),
        (_, Some(steps)) => rules::validate_steps(trigger, conditions, steps),
        (actions, None) => {
            let actions = actions.as_deref().unwrap_or_default();
            rules::validate(trigger, conditions, actions, fallback)
        }
    };
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
    let actions = input.actions.as_deref().unwrap_or_default();
    let steps = input.steps.as_deref();
    let assignees = actions
        .iter()
        .filter_map(|a| match a {
            Action::Assign { user_id } => Some(*user_id),
            _ => None,
        })
        .chain(steps.into_iter().flatten().filter_map(|s| match s {
            Step::Assign { user_id } => Some(*user_id),
            _ => None,
        }));
    for user in assignees {
        if team_user::Entity::find_by_id((team_id, user))
            .one(db)
            .await?
            .is_none()
        {
            return Ok(Err(reply(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_actions",
            )));
        }
    }
    if let Some(code) = scope_limits(
        db,
        (team_id, linkedin_on),
        &input.trigger,
        c,
        actions,
        steps,
    )
    .await?
    {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, code)));
    }
    // Limited to X accounts alone, its replies are held to what X takes (280, or 25,000 on a
    // paid plan).
    let fallback = input.fallback_public_reply.as_deref();
    let named = x_accounts(db, team_id, c, true).await?;
    let metas: Vec<&serde_json::Value> = named.iter().map(|a| &a.meta).collect();
    if x_automation::text_limits(&metas, actions, steps, fallback).is_err() {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_text")));
    }
    Ok(Ok((name, input)))
}

/// The team's X accounts an automation with `c` acts on. With `named_only`, those it names, and
/// only when it names X accounts alone (a mixed scope is left to fail item by item); else every X
/// account of its set or of the team too.
async fn x_accounts(
    db: &DatabaseConnection,
    team_id: Uuid,
    c: &Conditions,
    named_only: bool,
) -> Result<Vec<social_accounts::Model>, DbErr> {
    if named_only && c.account_ids.is_empty() {
        return Ok(Vec::new());
    }
    let all = social_accounts::Entity::find()
        .filter(social_accounts::Column::TeamId.eq(team_id))
        .filter(social_accounts::Column::Provider.eq("x"))
        .all(db)
        .await?;
    if named_only {
        let on_x = all.iter().filter(|a| c.account_ids.contains(&a.id)).count();
        if on_x != c.account_ids.len() {
            return Ok(Vec::new());
        }
    }
    let mut out = Vec::new();
    for a in all {
        let named = c.account_ids.is_empty() || c.account_ids.contains(&a.id);
        let in_set = match c.social_set_id {
            Some(s) => rules::live_set_ids(db, a.id).await?.contains(&s),
            None => true,
        };
        if named && in_set {
            out.push(a);
        }
    }
    Ok(out)
}

/// `rules::network_limits` for an automation limited to accounts by name; `None` otherwise.
async fn scope_limits(
    db: &impl ConnectionTrait,
    (team_id, linkedin_on): (Uuid, Option<bool>),
    trigger: &str,
    c: &Conditions,
    actions: &[Action],
    steps: Option<&[Step]>,
) -> Result<Option<&'static str>, DbErr> {
    if c.account_ids.is_empty() {
        return Ok(None);
    }
    let named: Vec<(String, serde_json::Value)> = social_accounts::Entity::find()
        .select_only()
        .column(social_accounts::Column::Provider)
        .column(social_accounts::Column::Meta)
        .filter(social_accounts::Column::TeamId.eq(team_id))
        .filter(social_accounts::Column::Id.is_in(c.account_ids.iter().copied()))
        .into_tuple()
        .all(db)
        .await?;
    let providers: Vec<&str> = named.iter().map(|(p, _)| p.as_str()).collect();
    if let Some(why) = rules::network_limits(trigger, &providers, actions, steps).err() {
        return Ok(Some(why));
    }
    // A LinkedIn account that cannot read comments yet (no approval, a personal profile, a Page
    // without the scopes) could never fire it: said plainly, before it is switched on.
    let blocked = named
        .iter()
        .filter(|(p, _)| p == "linkedin")
        .filter_map(|(_, meta)| {
            linkedin_on.map(|on| crate::linkedin_inbox::capability_of(on, meta))
        })
        .find(|c| *c != crate::linkedin_inbox::Capability::Ready);
    Ok(blocked.map(crate::linkedin_inbox::Capability::code))
}

/// Run counts by status and the last run, per automation of the team (or of `only` that one).
async fn run_stats(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    only: Option<Uuid>,
) -> Result<std::collections::HashMap<Uuid, RunStats>, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT automation_id, COUNT(*) AS total, \
             COUNT(*) FILTER (WHERE status = 'sent') AS sent, \
             COUNT(*) FILTER (WHERE status = 'skipped') AS skipped, \
             COUNT(*) FILTER (WHERE status = 'failed') AS failed, \
             COUNT(*) FILTER (WHERE status = 'running') AS running, \
             COUNT(*) FILTER (WHERE status = 'waiting') AS waiting, \
             MAX(created_at) AS last_at FROM automation_runs \
             WHERE team_id = $1 AND ($2::uuid IS NULL OR automation_id = $2) \
             GROUP BY automation_id",
            [team_id.into(), only.into()],
        ))
        .await?;
    let mut stats = rows
        .iter()
        .map(|r| {
            let id: Uuid = r.try_get("", "automation_id")?;
            let total = r.try_get("", "total")?;
            let stats = RunStats {
                sent: r.try_get("", "sent")?,
                skipped: r.try_get("", "skipped")?,
                failed: r.try_get("", "failed")?,
                running: r.try_get("", "running")?,
                waiting: r.try_get("", "waiting")?,
                total,
                count: total,
                last_at: r.try_get("", "last_at")?,
                reasons: Vec::new(),
            };
            Ok((id, stats))
        })
        .collect::<Result<std::collections::HashMap<_, _>, DbErr>>()?;
    let why = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT automation_id, reason, COUNT(*) AS n FROM automation_runs \
             WHERE team_id = $1 AND ($2::uuid IS NULL OR automation_id = $2) \
             AND status IN ('skipped', 'failed') AND reason IS NOT NULL GROUP BY automation_id, reason ORDER BY n DESC, reason",
            [team_id.into(), only.into()],
        ))
        .await?;
    for r in &why {
        let id: Uuid = r.try_get("", "automation_id")?;
        if let Some(s) = stats.get_mut(&id) {
            s.reasons.push(ReasonCount {
                reason: r.try_get("", "reason")?,
                count: r.try_get("", "n")?,
            });
        }
    }
    Ok(stats)
}

/// The team's automations, oldest first, each with its run counts by status and last run.
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
        let mut stats = run_stats(&app.db, t.team.id, None).await?;
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

/// One automation, with its run counts by status and last run.
#[utoipa::path(get, path = "/automations/{id}", tag = "automations", params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, AutomationId(id): AutomationId) -> Response {
    let found = async {
        let Some(a) = find(&app.db, t.team.id, id).await? else {
            return Ok(None);
        };
        let runs = run_stats(&app.db, t.team.id, Some(id)).await?.remove(&id);
        Ok::<_, DbErr>(Some(Automation {
            runs: Some(runs.unwrap_or_default()),
            ..automation_json(a)
        }))
    };
    match found.await {
        Ok(Some(a)) => Json(a).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation read"),
    }
}

/// Creates an automation; it acts on items received from now on. Owners and admins; at
/// most 50 per team. Unknown fields, triggers, condition keys and action types are refused
/// (`422 invalid_body`, `invalid_trigger`, `invalid_conditions`, `invalid_actions`,
/// `invalid_text`, `invalid_fallback`, and for a flow `invalid_steps` and `actions_and_steps`).
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
    if let Some(res) = super::billing::needs_automation(&app, t.team.id).await {
        return res;
    }
    let made = async {
        let (name, input) = match checked(
            &app.db,
            (t.team.id, Some(app.linkedin.community_api)),
            input,
        )
        .await?
        {
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
            actions: Set(serde_json::json!(input.actions.unwrap_or_default())),
            steps: Set(input.steps.map(|s| serde_json::json!(s))),
            fallback_public_reply: Set(input.fallback_public_reply),
            cooldown_minutes: Set(input.cooldown_minutes.map(|v| v as i32)),
            daily_cap: Set(input.daily_cap.map(|v| v as i32)),
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
/// Switching off ends its waiting runs.
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
    let a = automations::Entity::find()
        .from_raw_sql(stmt)
        .one(db)
        .await?;
    if a.is_some() && !active {
        rules::cancel_waiting(db, id).await?;
    }
    Ok(a)
}

/// Replaces an automation. Owners and admins. Its runs stay; changing its steps (or switching
/// it off) ends its waiting runs as `skipped: automation_changed`.
#[utoipa::path(put, path = "/automations/{id}", tag = "automations",
    request_body = AutomationInput, params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
    Body(input): Body<AutomationInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    if input.active {
        if let Some(res) = super::billing::needs_automation(&app, t.team.id).await {
            return res;
        }
    }
    let saved = async {
        let (name, input) = match checked(
            &app.db,
            (t.team.id, Some(app.linkedin.community_api)),
            input,
        )
        .await?
        {
            Ok(v) => v,
            Err(res) => return Ok(Err(res)),
        };
        let steps = input.steps.map(|s| serde_json::json!(s));
        let txn = app.db.begin().await?;
        let Some(old) = find(&txn, t.team.id, id).await? else {
            return Ok(Err(not_found()));
        };
        if old.steps != steps {
            rules::cancel_waiting(&txn, id).await?;
        }
        let done = automations::Entity::update_many()
            .col_expr(automations::Column::Name, name.into())
            .col_expr(automations::Column::Trigger, input.trigger.into())
            .col_expr(
                automations::Column::Conditions,
                serde_json::json!(input.conditions).into(),
            )
            .col_expr(
                automations::Column::Actions,
                serde_json::json!(input.actions.unwrap_or_default()).into(),
            )
            .col_expr(automations::Column::Steps, steps.into())
            .col_expr(
                automations::Column::FallbackPublicReply,
                input.fallback_public_reply.into(),
            )
            .col_expr(
                automations::Column::CooldownMinutes,
                input.cooldown_minutes.map(|v| v as i32).into(),
            )
            .col_expr(
                automations::Column::DailyCap,
                input.daily_cap.map(|v| v as i32).into(),
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
    if active {
        if let Some(res) = super::billing::needs_automation(app, t.team.id).await {
            return res;
        }
        // One saved before a rule existed is checked again before it may run.
        let refused = async {
            let Some(a) = find(&app.db, t.team.id, id).await? else {
                return Ok(None);
            };
            let parsed = (
                serde_json::from_value::<Conditions>(a.conditions.clone()),
                serde_json::from_value::<Vec<Action>>(a.actions.clone()),
                a.steps
                    .clone()
                    .map(serde_json::from_value::<Vec<Step>>)
                    .transpose(),
            );
            let (Ok(c), Ok(actions), Ok(steps)) = parsed else {
                return Ok(None);
            };
            scope_limits(
                &app.db,
                (t.team.id, Some(app.linkedin.community_api)),
                &a.trigger,
                &c,
                &actions,
                steps.as_deref(),
            )
            .await
        };
        match refused.await {
            Ok(Some(code)) => return reply(StatusCode::UNPROCESSABLE_ENTITY, code),
            Ok(None) => {}
            Err(e) => return internal(e, "automation switch"),
        }
    }
    match switch(&app.db, t.team.id, id, active).await {
        Ok(Some(a)) => Json(automation_json(a)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation switch"),
    }
}

/// Switches an automation on; it acts on items received from now on. Owners and admins.
#[utoipa::path(post, path = "/automations/{id}/enable", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn enable(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
) -> Response {
    set_active(&app, &t, id, true).await
}

/// Switches an automation off. Owners and admins. Its waiting runs end as
/// `skipped: automation_changed`.
#[utoipa::path(post, path = "/automations/{id}/disable", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Automation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn disable(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
) -> Response {
    set_active(&app, &t, id, false).await
}

/// Deletes an automation and its runs, waiting ones included; replies it sent stay on their
/// items. Owners and admins.
#[utoipa::path(delete, path = "/automations/{id}", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
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

/// An automation's runs, newest first, 50 a page. At most 500 runs act per team a day (a flow
/// counts once per send step); past that a run is `skipped: daily_cap`.
#[utoipa::path(get, path = "/automations/{id}/runs", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM), RunQuery),
    responses((status = 200, body = RunPage), (status = 401, body = Error),
        (status = 404, body = Error), (status = 422, body = Error)))]
async fn runs(
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
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
        if let Some(status) = &q.status {
            sel = sel.filter(automation_runs::Column::Status.eq(status));
        }
        if let Some(reason) = &q.reason {
            sel = sel.filter(automation_runs::Column::Reason.eq(reason));
        }
        let automation = find(&app.db, t.team.id, id).await?;
        let mut rows = sel
            .order_by_desc(automation_runs::Column::Id)
            .limit(PAGE + 1)
            .all(&app.db)
            .await?;
        let more = rows.len() as u64 > PAGE;
        rows.truncate(PAGE as usize);
        let next_cursor = more.then(|| rows.last().map(|r| r.id)).flatten();
        let items =
            numbered::numbers(&app.db, "inbox_items", rows.iter().map(|r| r.inbox_item_id)).await?;
        let runs = rows
            .into_iter()
            .map(|r| run_json(r, automation.as_ref(), &items))
            .collect();
        Ok::<_, DbErr>(Some(RunPage { runs, next_cursor }))
    };
    match found.await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation runs"),
    }
}

/// Tries a run again after the platform rate limited a send (`Run.can_retry`), which sent
/// nothing, so it can never send twice: an action list is run again now, a flow sends its last
/// step again at the next round (within a minute; its `status` is `waiting`). Owners and
/// admins. `409 not_retryable` for any other run (a send that failed some other way may have
/// gone out), `409 paused` while the team's automations are paused.
#[utoipa::path(post, path = "/automations/{id}/runs/{run_id}/retry", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM), ("run_id" = Uuid, Path)),
    responses((status = 200, body = Run), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error)))]
async fn retry(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
    RunId(run_id): RunId,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let done = async {
        let Some(a) = find(&app.db, t.team.id, id).await? else {
            return Ok(Err(not_found()));
        };
        let run = automation_runs::Entity::find_by_id(run_id)
            .filter(automation_runs::Column::AutomationId.eq(id))
            .one(&app.db)
            .await?;
        let Some(run) = run else {
            return Ok(Err(not_found()));
        };
        match rules::retry(&app, &a, &run).await? {
            Ok(()) => {}
            Err(code) => return Ok(Err(reply(StatusCode::CONFLICT, code))),
        }
        let run = automation_runs::Entity::find_by_id(run_id)
            .one(&app.db)
            .await?;
        let items =
            numbered::numbers(&app.db, "inbox_items", run.iter().map(|r| r.inbox_item_id)).await?;
        Ok::<_, DbErr>(
            run.map(|r| run_json(r, Some(&a), &items))
                .ok_or_else(not_found),
        )
    };
    match done.await {
        Ok(Ok(run)) => Json(run).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "automation run retry"),
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestInput {
    /// The automation to try, in the shape `POST /automations` takes (checked the same way,
    /// saved nowhere), so an edit can be tried before it is saved. `active` is ignored.
    pub automation: AutomationInput,
    pub sample: Sample,
}

/// Tries an automation on a sample comment, mention or message from a stranger, as the account
/// in `sample.account_id`: whether it matches (or why not), any rule that would stop it, and
/// each action with its final text, AI text included. Nothing is sent and no inbox item or run
/// is stored. An AI step is written by the real model with the real key choice and counted
/// against the AI allowance like a run. At most 100 tests a team a day, an AI step counting as
/// one more each (`429 rate_limited`).
/// Owners and admins on the Automation plan.
#[utoipa::path(post, path = "/automations/test", tag = "automations", request_body = TestInput,
    responses((status = 200, body = Outcome), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error), (status = 429, body = Error)))]
async fn test(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<TestInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    if let Some(res) = super::billing::needs_automation(&app, t.team.id).await {
        return res;
    }
    let TestInput { automation, sample } = input;
    let text = sample.text.trim();
    // A follow, like or repost carries no text.
    let textless = matches!(
        automation.trigger.as_str(),
        "new_follower" | "like_received" | "repost_received"
    );
    if (text.is_empty() && !textless)
        || text.chars().count() > dry_run::MAX_SAMPLE_CHARS
        || sample.age_minutes > dry_run::MAX_SAMPLE_AGE_MINUTES
    {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_sample");
    }
    let sample = Sample {
        text: text.to_owned(),
        ..sample
    };
    let tried = async {
        let (_, input) = match checked(&app.db, (t.team.id, None), automation).await? {
            Ok(v) => v,
            Err(res) => return Ok(Err(res)),
        };
        let account = social_accounts::Entity::find_by_id(sample.account_id)
            .filter(social_accounts::Column::TeamId.eq(t.team.id))
            .one(&app.db)
            .await?;
        let Some(account) = account.filter(|a| rules::AUTOMATED.contains(&a.provider.as_str()))
        else {
            return Ok(Err(reply(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_sample",
            )));
        };
        // A hit for the test and one for each AI step, which spends like a run.
        let ai = input.actions.iter().flatten().filter(|a| a.ai()).count()
            + input.steps.iter().flatten().filter(|s| s.ai()).count()
            + usize::from(!input.conditions.sentiment.is_empty());
        let bucket = (dry_run::quota_bucket(t.team.id), dry_run::TESTS_PER_DAY);
        let caps = vec![bucket; 1 + ai];
        if crate::rate_limit::daily_reserve(&app.db, &caps)
            .await?
            .is_none()
        {
            return Ok(Err(reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited")));
        }
        // Run apart from the request: a dropped request must not stop it between an AI spend and
        // its refund.
        let app = app.clone();
        let task = tokio::spawn(async move {
            let given = (
                input.trigger.as_str(),
                &input.conditions,
                input.actions.as_deref().unwrap_or_default(),
                input.steps.as_deref(),
                input.fallback_public_reply.as_deref(),
            );
            dry_run::test(&app, &account, given, &sample).await
        });
        match task.await {
            Ok(outcome) => Ok::<_, DbErr>(Ok(outcome?)),
            Err(e) => Err(DbErr::Custom(e.to_string())),
        }
    };
    match tried.await {
        Ok(Ok(outcome)) => Json(outcome).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "automation test"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct MissList {
    /// Newest first, at most 50, kept 14 days.
    pub misses: Vec<Miss>,
}

/// Recent items this automation was not run on, with why: the conditions that failed
/// (`wrong_set`, `wrong_account`, `wrong_post`, `no_keyword`, `not_first_from_author`,
/// `not_first_message`, `button_not_named` and the time, language and link conditions),
/// `not_yet_on` (switched on after the item came), `followed_before` (a follower it welcomed
/// already), `no_public_page` and `stopped_by_other` (an earlier automation on the item stops the
/// others). Items for another trigger, or another account or set, are left out. Items it was held
/// back on for a safety reason (paused, cool-down, caps) are in `runs`, skipped, with that
/// reason. `GET /inbox-items/{id}/automations` has all of them for one item.
#[utoipa::path(get, path = "/automations/{id}/misses", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = MissList), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn misses(
    State(app): State<AppState>,
    t: InTeam,
    AutomationId(id): AutomationId,
) -> Response {
    let found = async {
        if find(&app.db, t.team.id, id).await?.is_none() {
            return Ok(None);
        }
        let misses = automation_misses::for_automation(&app.db, t.team.id, id).await?;
        Ok::<_, DbErr>(Some(MissList { misses }))
    };
    match found.await {
        Ok(Some(l)) => Json(l).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "automation misses"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct VerdictList {
    /// Oldest automation first; only those that were on when the item came and have a run or a
    /// recorded reason.
    pub automations: Vec<Verdict>,
}

/// What each automation did with an inbox item: `ran`, `failed` or `skipped` (a run, with its
/// reason) or `not_matched` (with the codes of why). Kept 14 days after the item.
#[utoipa::path(get, path = "/inbox-items/{id}/automations", tag = "automations",
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = VerdictList), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn item_verdicts(State(app): State<AppState>, t: InTeam, ItemId(id): ItemId) -> Response {
    let found = async {
        let item = inbox_items::Entity::find_by_id(id)
            .filter(inbox_items::Column::TeamId.eq(t.team.id))
            .one(&app.db)
            .await?;
        if item.is_none() {
            return Ok(None);
        }
        let automations = automation_misses::for_item(&app.db, t.team.id, id).await?;
        Ok::<_, DbErr>(Some(VerdictList { automations }))
    };
    match found.await {
        Ok(Some(l)) => Json(l).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "item automations"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct Estimate {
    /// The automation acts on at least one X account of the team.
    pub x: bool,
    /// What one run costs on X (see `Outcome::cost`); absent when `x` is false.
    pub cost: Option<x_automation::Cost>,
}

/// What one run of an automation costs on X, from its actions or steps and its trigger, without
/// checking or saving it, so an edit shows its price while it is typed. Owners and admins.
#[utoipa::path(post, path = "/automations/estimate", tag = "automations",
    request_body = AutomationInput,
    responses((status = 200, body = Estimate), (status = 401, body = Error),
        (status = 403, body = Error)))]
async fn estimate(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<AutomationInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    match x_accounts(&app.db, t.team.id, &input.conditions, false).await {
        Ok(x) if x.is_empty() => Json(Estimate {
            x: false,
            cost: None,
        })
        .into_response(),
        Ok(_) => {
            let actions = input.actions.as_deref().unwrap_or_default();
            let cost = x_automation::cost(&input.trigger, actions, input.steps.as_deref());
            Json(Estimate {
                x: true,
                cost: Some(cost),
            })
            .into_response()
        }
        Err(e) => internal(e, "automation estimate"),
    }
}

/// One LinkedIn account and whether automations can run on it.
#[derive(Serialize, ToSchema)]
pub struct LinkedInAccount {
    pub id: Uuid,
    /// `ready`, `linkedin_needs_approval` (LinkedIn has not approved the Community Management API
    /// for this app), `linkedin_personal_profile` (apps cannot read comments on a member's
    /// posts) or `linkedin_needs_scope` (a Page connected without the comment permissions).
    pub status: String,
    /// What it means and what to do; empty when ready.
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct LinkedInStatus {
    /// Whether this server says LinkedIn approved the Community Management API for the app
    /// (`LINKEDIN_COMMUNITY_API`). Off until then, and every account is `linkedin_needs_approval`.
    pub approved: bool,
    pub accounts: Vec<LinkedInAccount>,
}

/// Whether automations can run on each of the team's LinkedIn accounts. Automations on LinkedIn
/// answer and like comments on a Page's posts and send no messages; nothing is read or sent for
/// an account that is not `ready`.
#[utoipa::path(get, path = "/automations/linkedin", tag = "automations",
    responses((status = 200, body = LinkedInStatus), (status = 401, body = Error)))]
async fn linkedin(State(app): State<AppState>, t: InTeam) -> Response {
    let found = social_accounts::Entity::find()
        .filter(social_accounts::Column::TeamId.eq(t.team.id))
        .filter(social_accounts::Column::Provider.eq("linkedin"))
        .order_by_asc(social_accounts::Column::CreatedAt)
        .all(&app.db)
        .await;
    match found {
        Ok(rows) => Json(LinkedInStatus {
            approved: app.linkedin.community_api,
            accounts: rows
                .iter()
                .map(|a| {
                    let c = crate::linkedin_inbox::capability(&app, a);
                    LinkedInAccount {
                        id: a.id,
                        status: c.code().into(),
                        message: c.message().into(),
                    }
                })
                .collect(),
        })
        .into_response(),
        Err(e) => internal(e, "linkedin automations"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(linkedin))
        .routes(routes!(estimate))
        .routes(routes!(test))
        .routes(routes!(misses))
        .routes(routes!(item_verdicts))
        .routes(routes!(list, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(enable))
        .routes(routes!(disable))
        .routes(routes!(runs))
        .routes(routes!(retry))
}
