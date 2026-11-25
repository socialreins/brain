//! X in the inbox: mentions and replies to the account's posts, and (when asked for) direct
//! messages, polled into `inbox_items` like Bluesky's, so filters, inboxes, contacts and
//! automations work on them unchanged. Replies and DMs go out through `inbox::send`, once.
//!
//! Reading X costs money (pay-per-use credits, billed per post, user or DM event returned), so
//! everything is opt-in per account (`x_inbox.enabled`, off by default) and spending is held to
//! what the team set:
//! - the poll is slow (`poll_minutes`, 60 by default, at least 15) and asks X only for what is
//!   new: mentions after `mentions_since_id`, so a quiet account's poll returns, and costs,
//!   nothing; DMs have no such cursor, so one event is asked for first, and more only when it is
//!   new;
//! - each account has a monthly budget (`budget_micros`, $2 by default); what X returned is
//!   counted against it and polling stops, `paused_reason = budget`, when it is used up, until the
//!   month turns or the budget is raised;
//! - when X says the app is out of credits, or its usage cap is reached, polling stops
//!   (`credits`, `usage_capped`) until a person resumes it. No refusal is ever retried in a loop:
//!   a failed round waits for the next interval, and five in a row pause it (`errors`).
//!
//! With `BILLING_ENABLED` the team pays in its `x` credits too: 1 per reply, DM or action, and 1
//! for every `READS_PER_CREDIT` items read. Amounts here are millionths of a US dollar.
use std::sync::LazyLock;

use chrono::{DateTime, Datelike, Duration, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryResult, Statement, Value};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    automations, billing,
    connectors::{
        bluesky::{Author, InboxError, PostError},
        x::{
            inbox::{
                ApiError, DmEvent, DmTo, Granted, Mention, Mentions, DM_READ_MICROS,
                POST_READ_MICROS, USER_READ_MICROS,
            },
            status_url,
        },
    },
    controllers,
    inbox::{self, Fresh, Via},
    models::{inbox_items, social_accounts},
    state::AppState,
};

pub const DEFAULT_POLL_MINUTES: i32 = 60;
pub const MIN_POLL_MINUTES: i32 = 15;
pub const MAX_POLL_MINUTES: i32 = 1440;
/// $2 a month for each account unless the team sets another.
pub const DEFAULT_BUDGET_MICROS: i64 = 2_000_000;
pub const MIN_BUDGET_MICROS: i64 = 100_000;
pub const MAX_BUDGET_MICROS: i64 = 100_000_000;
/// With billing on: items read for one `x` credit (a credit is $0.05, three items at most
/// $0.06 with their users, and a user is billed once a day).
pub const READS_PER_CREDIT: i32 = 3;
/// A first read (or one after a long pause) goes back at most this far, and takes at most
/// `FIRST_ITEMS`.
const FIRST_DAYS: i64 = 7;
const FIRST_ITEMS: u32 = 20;
/// Pages one read of one kind may take.
const MAX_PAGES: usize = 3;
/// Failed rounds in a row that pause the account.
const MAX_ERRORS: i32 = 5;
/// What one call asking for the smallest page (5 posts and their authors) may cost: below this
/// left of the budget, polling stops.
const MIN_CALL_MICROS: i64 = 5 * (POST_READ_MICROS + USER_READ_MICROS);
/// Accounts read at once across the process.
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

/// One account's row of `x_inbox`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub enabled: bool,
    pub read_dms: bool,
    pub poll_minutes: i32,
    pub budget_micros: i64,
    pub spent_micros: i64,
    pub units_owed: i32,
    pub mentions_since_id: Option<String>,
    pub dm_since_id: Option<String>,
    pub next_poll_at: DateTime<Utc>,
    pub paused_reason: Option<String>,
    pub last_read_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub errors: i32,
}

impl Settings {
    /// What an account that never opted in has.
    pub fn off() -> Self {
        Self {
            enabled: false,
            read_dms: false,
            poll_minutes: DEFAULT_POLL_MINUTES,
            budget_micros: DEFAULT_BUDGET_MICROS,
            spent_micros: 0,
            units_owed: 0,
            mentions_since_id: None,
            dm_since_id: None,
            next_poll_at: Utc::now(),
            paused_reason: None,
            last_read_at: None,
            last_error: None,
            errors: 0,
        }
    }

    fn from_row(r: &QueryResult) -> Result<Self, DbErr> {
        Ok(Self {
            enabled: r.try_get("", "enabled")?,
            read_dms: r.try_get("", "read_dms")?,
            poll_minutes: r.try_get("", "poll_minutes")?,
            budget_micros: r.try_get("", "budget_micros")?,
            spent_micros: r.try_get("", "spent_micros")?,
            units_owed: r.try_get("", "units_owed")?,
            mentions_since_id: r.try_get("", "mentions_since_id")?,
            dm_since_id: r.try_get("", "dm_since_id")?,
            next_poll_at: r.try_get("", "next_poll_at")?,
            paused_reason: r.try_get("", "paused_reason")?,
            last_read_at: r.try_get("", "last_read_at")?,
            last_error: r.try_get("", "last_error")?,
            errors: r.try_get("", "errors")?,
        })
    }
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The account's settings, with this month's spending (a new month starts at 0 even before the
/// next tick says so), or `None` when it never opted in.
pub async fn load(db: &impl ConnectionTrait, account: Uuid) -> Result<Option<Settings>, DbErr> {
    let row = db
        .query_one_raw(sql(
            "SELECT enabled, read_dms, poll_minutes, budget_micros, \
             CASE WHEN period < (date_trunc('month', now() AT TIME ZONE 'UTC'))::date \
                  THEN 0 ELSE spent_micros END AS spent_micros, \
             units_owed, mentions_since_id, dm_since_id, next_poll_at, \
             CASE WHEN paused_reason = 'budget' AND period < \
                  (date_trunc('month', now() AT TIME ZONE 'UTC'))::date \
                  THEN NULL ELSE paused_reason END AS paused_reason, \
             last_read_at, last_error, errors FROM x_inbox WHERE social_account_id = $1",
            vec![account.into()],
        ))
        .await?;
    row.as_ref().map(Settings::from_row).transpose()
}

/// What a person chose.
pub struct Choice {
    pub enabled: bool,
    pub read_dms: bool,
    pub poll_minutes: i32,
    pub budget_micros: i64,
}

impl Choice {
    /// The error code when a value is out of range.
    pub fn checked(&self) -> Result<(), &'static str> {
        if !(MIN_POLL_MINUTES..=MAX_POLL_MINUTES).contains(&self.poll_minutes) {
            return Err("invalid_poll");
        }
        if !(MIN_BUDGET_MICROS..=MAX_BUDGET_MICROS).contains(&self.budget_micros) {
            return Err("invalid_budget");
        }
        Ok(())
    }
}

/// Saves `c` for the account. Switching on from off (or changing the budget up) clears a pause
/// the old values caused and polls at the next tick; the read cursors are kept, so a switch off
/// and on again never reads the same items twice.
pub async fn save(db: &impl ConnectionTrait, account: Uuid, c: &Choice) -> Result<(), DbErr> {
    db.execute_raw(sql(
        "INSERT INTO x_inbox (social_account_id, enabled, read_dms, poll_minutes, budget_micros) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (social_account_id) DO UPDATE SET \
           next_poll_at = CASE WHEN NOT x_inbox.enabled AND EXCLUDED.enabled THEN now() \
                          ELSE x_inbox.next_poll_at END, \
           errors = CASE WHEN NOT x_inbox.enabled AND EXCLUDED.enabled THEN 0 \
                    ELSE x_inbox.errors END, \
           paused_reason = CASE WHEN x_inbox.paused_reason = 'budget' \
                           AND EXCLUDED.budget_micros > x_inbox.budget_micros THEN NULL \
                           ELSE x_inbox.paused_reason END, \
           enabled = EXCLUDED.enabled, read_dms = EXCLUDED.read_dms, \
           poll_minutes = EXCLUDED.poll_minutes, budget_micros = EXCLUDED.budget_micros, \
           updated_at = now()",
        vec![
            account.into(),
            c.enabled.into(),
            c.read_dms.into(),
            c.poll_minutes.into(),
            c.budget_micros.into(),
        ],
    ))
    .await
    .map(|_| ())
}

/// Lets a paused account poll again, at the next tick. It never reads more than the budget
/// left allows, so resuming cannot overspend.
pub async fn resume(db: &impl ConnectionTrait, account: Uuid) -> Result<(), DbErr> {
    db.execute_raw(sql(
        "UPDATE x_inbox SET paused_reason = NULL, errors = 0, last_error = NULL, \
         next_poll_at = now(), updated_at = now() WHERE social_account_id = $1",
        vec![account.into()],
    ))
    .await
    .map(|_| ())
}

/// What a pause means to a person.
pub fn pause_message(reason: &str, last_error: Option<&str>) -> String {
    match reason {
        "credits" => ApiError::Credits.message(),
        "usage_capped" => ApiError::UsageCap.message(),
        "budget" => "This month's read budget for the account is used up. Reading starts again \
                     on the 1st (UTC), or raise the budget."
            .into(),
        _ => format!(
            "X failed {MAX_ERRORS} times in a row{}. Check the account, then resume.",
            last_error.map(|e| format!(" ({e})")).unwrap_or_default()
        ),
    }
}

/// What reading and replying cost, for the settings page. Dollars.
pub struct Estimate {
    pub per_mention_usd: f64,
    pub per_dm_usd: f64,
    /// What polling costs when nothing new comes: DMs ask for their newest event (and its
    /// sender) once a day at most, mentions ask for nothing new and are free.
    pub idle_month_usd: f64,
    /// This month's spending carried to the month's end, never over the budget.
    pub projected_month_usd: f64,
}

fn usd(micros: i64) -> f64 {
    micros as f64 / 1e6
}

pub fn estimate(s: &Settings, now: DateTime<Utc>) -> Estimate {
    let (year, month) = (now.year(), now.month());
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_default();
    let next = chrono::NaiveDate::from_ymd_opt(year + i32::from(month == 12), month % 12 + 1, 1)
        .unwrap_or_default();
    let days = (next - first).num_days().max(1) as f64;
    let so_far = ((now.date_naive() - first).num_days() as f64 + 1.0).clamp(1.0, days);
    let projected = (s.spent_micros as f64 / so_far * days).min(s.budget_micros as f64);
    Estimate {
        per_mention_usd: usd(POST_READ_MICROS + USER_READ_MICROS),
        per_dm_usd: usd(DM_READ_MICROS + USER_READ_MICROS),
        idle_month_usd: if s.read_dms {
            usd(DM_READ_MICROS + USER_READ_MICROS) * 30.0
        } else {
            0.0
        },
        projected_month_usd: (projected / 1e3).round() / 1e3,
    }
}

/// `a` is a later X id than `b` (both plain digits, no leading zeros).
fn newer(a: &str, b: &str) -> bool {
    (a.len(), a) > (b.len(), b)
}

fn author_of(p: &crate::connectors::x::inbox::Person) -> Author {
    Author {
        did: p.id.clone(),
        handle: p.username.clone(),
        display_name: p.name.clone(),
        avatar: p.avatar.clone(),
    }
}

/// A reply to one of the account's posts is a `comment`; any other post that mentions it a
/// `mention`.
fn mention_fresh(account: &social_accounts::Model, m: &Mention) -> Fresh {
    let ours = m.in_reply_to_user.as_deref() == Some(account.external_id.as_str());
    Fresh {
        kind: if ours { "comment" } else { "mention" },
        remote_id: m.id.clone(),
        remote_cid: None,
        parent: m.replied_to.clone(),
        root: (m.conversation_id.clone(), None),
        convo_id: None,
        author: author_of(&m.author),
        text: m.text.clone(),
        at: m.at,
        permalink: Some(status_url(m.author.username.as_deref(), &m.id)),
        story: None,
        button_payload: None,
        attachments: m.attachments.clone(),
    }
}

fn dm_fresh(e: &DmEvent) -> Fresh {
    Fresh {
        kind: "dm",
        remote_id: e.id.clone(),
        remote_cid: None,
        parent: None,
        root: (None, None),
        convo_id: Some(e.conversation_id.clone()),
        author: author_of(&e.sender),
        text: e.text.clone(),
        at: e.at,
        permalink: None,
        story: None,
        button_payload: None,
        attachments: e.attachments.clone(),
    }
}

/// Why a round stopped.
enum Stop {
    Api(ApiError),
    Db(DbErr),
    /// The month's budget has no room for another call.
    Budget,
}

impl From<ApiError> for Stop {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<DbErr> for Stop {
    fn from(e: DbErr) -> Self {
        Self::Db(e)
    }
}

/// One round's progress, kept even when a later call fails.
#[derive(Default)]
struct Round {
    cost: i64,
    reads: i32,
    mentions_cursor: Option<String>,
    dm_cursor: Option<String>,
    fresh: Vec<inbox_items::Model>,
}

impl Round {
    /// Counts a call's cost; the room it leaves.
    fn pay(&mut self, micros: i64, reads: usize) {
        self.cost += micros;
        self.reads += reads as i32;
    }

    fn room(&self, s: &Settings) -> Result<i64, Stop> {
        let left = s.budget_micros - s.spent_micros - self.cost;
        if left < MIN_CALL_MICROS {
            Err(Stop::Budget)
        } else {
            Ok(left)
        }
    }
}

async fn read_mentions(
    app: &AppState,
    account: &social_accounts::Model,
    token: &str,
    s: &Settings,
    round: &mut Round,
) -> Result<(), Stop> {
    let since = s.mentions_since_id.as_deref();
    let start = since
        .is_none()
        .then(|| Utc::now() - Duration::days(FIRST_DAYS));
    let (mut page, mut pages, mut found): (Option<String>, usize, Vec<Mention>) =
        (None, 0, Vec::new());
    loop {
        let left = round.room(s)?;
        let max = if since.is_none() {
            FIRST_ITEMS
        } else {
            ((left / (POST_READ_MICROS + USER_READ_MICROS)).clamp(5, 50)) as u32
        };
        let r: Mentions = app
            .x
            .mentions(
                token,
                &account.external_id,
                (since, start),
                max,
                page.as_deref(),
            )
            .await?;
        round.pay(r.cost_micros(), r.posts);
        if pages == 0 {
            round.mentions_cursor = r.newest_id.clone();
        }
        pages += 1;
        let more = r.next_token.clone();
        found.extend(r.items);
        // A first read is one page; a later one follows older pages while the budget allows.
        match more {
            Some(t) if since.is_some() && pages < MAX_PAGES => page = Some(t),
            _ => break,
        }
    }
    // Oldest first, so automations see them in the order they came.
    for m in found.iter().rev() {
        if m.author.id == account.external_id {
            continue;
        }
        let item = inbox::store(&app.db, account, mention_fresh(account, m)).await?;
        round.fresh.extend(item);
    }
    Ok(())
}

async fn read_dms(
    app: &AppState,
    account: &social_accounts::Model,
    token: &str,
    s: &Settings,
    round: &mut Round,
) -> Result<(), Stop> {
    let seen = s.dm_since_id.as_deref();
    round.room(s)?;
    // X has no `since_id` for DMs: ask for the newest event, and for more only if it is new.
    let probe = app.x.dm_events(token, 1, None).await?;
    round.pay(probe.cost_micros(), probe.events);
    let Some(top) = probe.items.first().map(|e| e.id.clone()) else {
        return Ok(());
    };
    if seen.is_some_and(|s| !newer(&top, s)) {
        return Ok(());
    }
    round.dm_cursor = Some(top);
    let floor = (Utc::now() - Duration::days(FIRST_DAYS)).fixed_offset();
    let (mut page, mut pages, mut found): (Option<String>, usize, Vec<DmEvent>) =
        (None, 0, Vec::new());
    'pages: loop {
        let left = round.room(s)?;
        let max = if seen.is_none() {
            FIRST_ITEMS
        } else {
            ((left / (DM_READ_MICROS + USER_READ_MICROS)).clamp(5, 50)) as u32
        };
        let r = app.x.dm_events(token, max, page.as_deref()).await?;
        round.pay(r.cost_micros(), r.events);
        for e in r.items {
            if seen.is_some_and(|s| !newer(&e.id, s)) || e.at < floor {
                break 'pages;
            }
            found.push(e);
        }
        pages += 1;
        match r.next_token {
            Some(t) if seen.is_some() && pages < MAX_PAGES => page = Some(t),
            _ => break,
        }
    }
    for e in found.iter().rev() {
        if e.sender.id == account.external_id {
            continue;
        }
        let item = inbox::store(&app.db, account, dm_fresh(e)).await?;
        round.fresh.extend(item);
    }
    Ok(())
}

/// Notes a problem without pausing or counting an error.
async fn note(db: &impl ConnectionTrait, id: Uuid, text: &str) -> Result<(), DbErr> {
    db.execute_raw(sql(
        "UPDATE x_inbox SET last_error = $2, updated_at = now() WHERE social_account_id = $1",
        vec![id.into(), text.into()],
    ))
    .await
    .map(|_| ())
}

async fn pause(
    db: &impl ConnectionTrait,
    id: Uuid,
    reason: &str,
    last_error: Option<&str>,
) -> Result<(), DbErr> {
    tracing::warn!(account = %id, reason, "x inbox paused");
    db.execute_raw(sql(
        "UPDATE x_inbox SET paused_reason = $2, last_error = $3, updated_at = now() \
         WHERE social_account_id = $1",
        vec![
            id.into(),
            reason.into(),
            last_error.map(str::to_owned).into(),
        ],
    ))
    .await
    .map(|_| ())
}

/// Reads one account's mentions and DMs, stores what is new, spends, and moves its cursors.
async fn poll(app: &AppState, id: Uuid) -> Result<(), DbErr> {
    let _slot = SLOTS.acquire().await.ok();
    let db = &app.db;
    let Some(account) = social_accounts::Entity::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    let Some(s) = load(db, id).await? else {
        return Ok(());
    };
    if !s.enabled || s.paused_reason.is_some() {
        return Ok(());
    }
    if app.billing_enabled {
        if !billing::has_plan(db, account.team_id).await? {
            return note(db, id, "The team's plan has ended, so X is not read.").await;
        }
        if billing::balance(db, account.team_id, billing::Kind::X).await? < 1 {
            return note(db, id, "The team has no X credits. Add some to read X.").await;
        }
    }
    let token = match controllers::x::fresh_token(app, &account).await {
        Ok(t) => t,
        Err(PostError::Reconnect | PostError::Denied) => {
            return note(db, id, &ApiError::Reconnect.message()).await
        }
        Err(PostError::Failed(why)) => return note(db, id, &why).await,
    };
    let granted = Granted::of(account.meta["scope"].as_str());
    let mut round = Round::default();
    let mut stop = read_mentions(app, &account, &token, &s, &mut round)
        .await
        .err();
    if stop.is_none() && s.read_dms && granted.dms {
        stop = read_dms(app, &account, &token, &s, &mut round).await.err();
    }
    // Credits for what was read: whole credits for each `READS_PER_CREDIT` items, the rest
    // carried. A team that cannot pay is noted and not read again until it can (the check above).
    let mut owed = s.units_owed + round.reads;
    if app.billing_enabled && owed >= READS_PER_CREDIT {
        let reference = format!("xread:{id}:{}", Uuid::now_v7());
        let credits = i64::from(owed / READS_PER_CREDIT);
        let kind = billing::Kind::X;
        match billing::spend(
            db,
            account.team_id,
            kind,
            credits,
            "x inbox read",
            Some(&reference),
        )
        .await?
        {
            billing::Spent::Short(_) => {
                note(db, id, "The team has no X credits. Add some to read X.").await?;
            }
            _ => owed %= READS_PER_CREDIT,
        }
    }
    let owed = if app.billing_enabled { owed } else { 0 };
    let ok = stop.is_none();
    db.execute_raw(sql(
        "UPDATE x_inbox SET spent_micros = (CASE WHEN period < \
           (date_trunc('month', now() AT TIME ZONE 'UTC'))::date THEN 0 ELSE spent_micros END) + $2, \
         period = (date_trunc('month', now() AT TIME ZONE 'UTC'))::date, \
         mentions_since_id = COALESCE($3, mentions_since_id), \
         dm_since_id = COALESCE($4, dm_since_id), units_owed = $5, \
         last_read_at = now(), updated_at = now(), \
         errors = CASE WHEN $6 THEN 0 ELSE errors END, \
         last_error = CASE WHEN $6 THEN NULL ELSE last_error END, \
         next_poll_at = now() + make_interval(mins => poll_minutes) \
         WHERE social_account_id = $1",
        vec![
            id.into(),
            round.cost.into(),
            round.mentions_cursor.clone().into(),
            round.dm_cursor.clone().into(),
            owed.into(),
            ok.into(),
        ],
    ))
    .await?;
    match stop {
        None => {}
        Some(Stop::Db(e)) => return Err(e),
        Some(Stop::Budget) => pause(db, id, "budget", None).await?,
        Some(Stop::Api(e)) => on_error(app, &account, e).await?,
    }
    // The lock-free sweep of `automations` picks these up again if this is cut short.
    for item in &round.fresh {
        if let Err(e) = automations::run(app, &account, item).await {
            tracing::error!(error = %e, item = %item.id, "x automations failed");
        }
    }
    Ok(())
}

/// What an X refusal does to polling. Nothing here retries: the next try is the next interval.
async fn on_error(
    app: &AppState,
    account: &social_accounts::Model,
    e: ApiError,
) -> Result<(), DbErr> {
    let (db, id, said) = (&app.db, account.id, e.message());
    match e {
        ApiError::Credits => pause(db, id, "credits", Some(&said)).await,
        ApiError::UsageCap => pause(db, id, "usage_capped", Some(&said)).await,
        ApiError::Reconnect | ApiError::Scope => {
            crate::expiry::flag_reconnect(db, id).await;
            note(db, id, &said).await
        }
        ApiError::RateLimited(at) => {
            let at = at.unwrap_or_else(|| Utc::now() + Duration::minutes(15));
            db.execute_raw(sql(
                "UPDATE x_inbox SET next_poll_at = GREATEST(next_poll_at, $2), last_error = $3, \
                 updated_at = now() WHERE social_account_id = $1",
                vec![id.into(), (at + Duration::minutes(1)).into(), said.into()],
            ))
            .await
            .map(|_| ())
        }
        ApiError::NotAllowed(_) | ApiError::Refused(_) | ApiError::Unreachable => {
            tracing::warn!(account = %id, why = %said, "x inbox read failed");
            let n = db
                .query_one_raw(sql(
                    "UPDATE x_inbox SET errors = errors + 1, last_error = $2, updated_at = now() \
                     WHERE social_account_id = $1 RETURNING errors",
                    vec![id.into(), said.clone().into()],
                ))
                .await?
                .map(|r| r.try_get::<i32>("", "errors"))
                .transpose()?
                .unwrap_or(0);
            if n >= MAX_ERRORS {
                pause(db, id, "errors", Some(&said)).await?;
            }
            Ok(())
        }
    }
}

/// One round: the new month's accounts start fresh, then every account that is on, not paused,
/// due, and not waiting for a reconnect is claimed (its next poll moves on first, so a crash
/// never makes it poll twice at once) and read, four at a time.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    app.db
        .execute_raw(sql(
            "UPDATE x_inbox SET period = (date_trunc('month', now() AT TIME ZONE 'UTC'))::date, \
             spent_micros = 0, \
             paused_reason = CASE WHEN paused_reason = 'budget' THEN NULL ELSE paused_reason END \
             WHERE period < (date_trunc('month', now() AT TIME ZONE 'UTC'))::date",
            vec![],
        ))
        .await?;
    let claimed = app
        .db
        .query_all_raw(sql(
            "UPDATE x_inbox SET next_poll_at = now() + make_interval(mins => poll_minutes) \
             WHERE social_account_id IN (SELECT x.social_account_id FROM x_inbox x \
               JOIN social_accounts a ON a.id = x.social_account_id \
               WHERE x.enabled AND x.paused_reason IS NULL AND x.next_poll_at <= now() \
               AND COALESCE(a.meta->>'reconnect', 'false') <> 'true' \
               ORDER BY x.next_poll_at LIMIT 20 FOR UPDATE OF x SKIP LOCKED) \
             RETURNING social_account_id",
            vec![],
        ))
        .await?;
    let reads: Vec<_> = claimed
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "social_account_id").ok())
        .map(|id| {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = poll(&app, id).await {
                    tracing::error!(error = %e, account = %id, "x inbox poll failed");
                }
            })
        })
        .collect();
    for read in reads {
        let _ = read.await;
    }
    Ok(())
}

/// A reply or an action costs the team one `x` credit with billing on: taken before the call,
/// and the reference to give back with, or `None` with billing off.
async fn hold(app: &AppState, team: Uuid, what: &str) -> Result<Option<String>, InboxError> {
    if !app.billing_enabled {
        return Ok(None);
    }
    let reference = format!("xsend:{what}:{}", Uuid::now_v7());
    match billing::spend(&app.db, team, billing::Kind::X, 1, what, Some(&reference)).await {
        Ok(billing::Spent::Taken | billing::Spent::Repeated) => Ok(Some(reference)),
        Ok(billing::Spent::Short(_)) => Err(InboxError::X(
            "Sending on X needs 1 credit and the team has none. Add X credits.".into(),
        )),
        Err(e) => {
            tracing::error!(error = %e, "x credit spend failed");
            Err(InboxError::X("This could not be sent. Try again.".into()))
        }
    }
}

async fn give_back(app: &AppState, team: Uuid, held: Option<String>) {
    if let Some(reference) = held {
        let kind = billing::Kind::X;
        if let Err(e) = billing::grant(&app.db, team, kind, 1, "x unsent", Some(&reference)).await {
            tracing::error!(error = %e, "x credit refund failed");
        }
    }
}

/// X's refusal as the inbox words it.
fn refused(e: ApiError, via: Via) -> InboxError {
    match e {
        ApiError::Reconnect | ApiError::Scope => InboxError::XReconnect,
        ApiError::RateLimited(_) => InboxError::RateLimited,
        ApiError::NotAllowed(_) if via == Via::Dm => InboxError::DmNotAllowed,
        other => InboxError::X(other.message()),
    }
}

async fn token_of(app: &AppState, account: &social_accounts::Model) -> Result<String, InboxError> {
    match controllers::x::fresh_token(app, account).await {
        Ok(t) => Ok(t),
        Err(PostError::Reconnect | PostError::Denied) => Err(InboxError::XReconnect),
        Err(PostError::Failed(why)) => Err(InboxError::X(why)),
    }
}

/// Sends `text` for `item`, once: a reply to its post (comments and mentions) or a DM (in its
/// conversation, else to its author). The id of what went out. A DM X refuses because the
/// person does not follow the account and has DMs closed is `DmNotAllowed`, which automations
/// end as `skipped` and may answer with a public reply. A failure that never reached X gives
/// the credit back; one that may have gone out does not.
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    via: Via,
    text: &str,
) -> Result<String, InboxError> {
    if via == Via::Public && !matches!(item.kind.as_str(), "comment" | "mention") {
        return Err(InboxError::Invalid("not_repliable"));
    }
    if via == Via::Dm && !Granted::of(account.meta["scope"].as_str()).dms {
        return Err(InboxError::XReconnect);
    }
    let token = token_of(app, account).await?;
    let held = hold(app, account.team_id, "x reply").await?;
    let sent = match via {
        Via::Public => app.x.reply(&token, text, &item.remote_id).await,
        Via::Dm => {
            let to = match item.convo_id.as_deref() {
                Some(c) => DmTo::Conversation(c),
                None => DmTo::Person(&item.author_id),
            };
            app.x.send_dm(&token, to, text).await
        }
    };
    match sent {
        Ok(id) => Ok(id),
        Err(e) => {
            if e != ApiError::Unreachable {
                give_back(app, account.team_id, held).await;
            }
            Err(refused(e, via))
        }
    }
}

/// What a person can do to an X item besides reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Like,
    Repost,
    Hide,
    Unhide,
}

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "like" => Some(Self::Like),
            "repost" => Some(Self::Repost),
            "hide" => Some(Self::Hide),
            "unhide" => Some(Self::Unhide),
            _ => None,
        }
    }
}

/// Likes, reposts, hides or shows again the post of `item` (a comment or mention; hiding is for
/// replies to the account's own posts), once, never retried.
pub async fn act(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    action: Action,
) -> Result<(), InboxError> {
    let g = Granted::of(account.meta["scope"].as_str());
    let hiding = matches!(action, Action::Hide | Action::Unhide);
    if !matches!(item.kind.as_str(), "comment" | "mention") || (hiding && item.kind != "comment") {
        return Err(InboxError::Invalid("not_repliable"));
    }
    if (action == Action::Like && !g.like) || (hiding && !g.hide) {
        return Err(InboxError::XReconnect);
    }
    let token = token_of(app, account).await?;
    let held = hold(app, account.team_id, "x action").await?;
    let done = match action {
        Action::Like | Action::Repost => {
            let repost = action == Action::Repost;
            app.x
                .react(&token, &account.external_id, &item.remote_id, repost)
                .await
        }
        Action::Hide | Action::Unhide => {
            app.x
                .hide(&token, &item.remote_id, action == Action::Hide)
                .await
        }
    };
    match done {
        Ok(()) => Ok(()),
        Err(e) => {
            if e != ApiError::Unreachable {
                give_back(app, account.team_id, held).await;
            }
            Err(refused(e, Via::Public))
        }
    }
}
