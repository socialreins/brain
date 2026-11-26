//! YouTube in the inbox: the comments on a channel's videos (and Shorts), polled into
//! `inbox_items` like the other networks', so filters, inboxes, contacts and automations work on
//! them unchanged. A top-level comment is a `comment` whose parent and root are the video; a reply
//! is a `comment` whose parent is the top-level comment and whose root is the video. The channel's
//! own comments are not stored.
//!
//! YouTube charges quota units, not money, and the Google project's 10,000 units a day (reset at
//! midnight Pacific time) are shared by every team and by uploads (1,600 each), so reading is
//! opt-in per channel (`youtube_inbox.enabled`) and held to what it was given:
//! - a read is 1 unit: one list call a poll (100 threads), plus one for a thread whose reply count
//!   changed and one for 50 video titles; the channel's `daily_units` (300 by default) bounds them,
//!   and reading stops, `paused_reason = budget`, until the Pacific day turns;
//! - all channels together stop at `YOUTUBE_INBOX_UNITS` a day (4,000 by default), so inbox reads
//!   never starve uploads; YouTube's own `quotaExceeded` pauses a channel the same way (`quota`);
//! - each channel's read is bounded in time (`READ_LIMIT`) and in pages: a sweep that runs out
//!   keeps its place (`page_token`, `pending_at`) and goes on at the next tick, and the cursor
//!   (`newest_at`) moves only when a sweep ends, so nothing is skipped. A failed round waits for
//!   the next interval and is never retried in a loop; five in a row pause the channel (`errors`).
//!
//! A first read goes back `inbox::FIRST_DAYS` and stores what it finds as already automated, so
//! no automation answers old comments. YouTube's developer policy allows stored API data 30 days
//! before it must be refreshed or deleted: `purge` deletes comments, thread counts, video titles
//! and contact names older than that.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration as Wait, Instant},
};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryResult, Statement, Value};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    automations,
    connectors::{
        bluesky::{InboxError, PostError},
        youtube::{is_quota, Comment, Thread, QUOTA_MESSAGE},
    },
    controllers,
    inbox::{self, Fresh},
    models::{inbox_items, social_accounts},
    state::AppState,
};

pub const DEFAULT_POLL_MINUTES: i32 = 30;
pub const MIN_POLL_MINUTES: i32 = 10;
pub const MAX_POLL_MINUTES: i32 = 1440;
/// Units a channel may spend reading in one Pacific day.
pub const DEFAULT_DAILY_UNITS: i32 = 300;
pub const MIN_DAILY_UNITS: i32 = 20;
pub const MAX_DAILY_UNITS: i32 = 2000;
/// What a reply, a moderation call or a delete costs.
pub const WRITE_UNITS: i32 = 50;
/// Most units a channel's replies and moderation may cost in a Pacific day (40 calls), so a member
/// toggling a button cannot spend the Google project's quota.
pub const MAX_WRITE_UNITS: i32 = 2000;
/// Pages of 100 threads one tick may read, and longest it may take, for one channel.
const MAX_PAGES: usize = 5;
const READ_LIMIT: Wait = Wait::from_secs(20);
/// Threads one tick may read in full (1 unit each) because their reply count changed.
const MAX_REPLY_READS: usize = 10;
/// A video title is read again after this many days (a title can change).
const VIDEO_DAYS: i64 = 7;
/// YouTube's rule for stored API data.
const KEEP_DAYS: i64 = 30;
/// Failed rounds in a row that pause the channel.
const MAX_ERRORS: i32 = 5;
/// A pause for errors (`paused_reason = errors`) ends by itself this many hours after it began: a
/// provider outage must not leave the inbox, and the automations on it, off until a click. The
/// next read then ends the pause again at once if it still fails (`MAX_ERRORS - 1` errors are
/// kept), so an outage costs one read per wait.
const RESUME_HOURS: i32 = 2;
/// Channels read at once across the process.
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));
static PURGED: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

/// The Pacific day YouTube's quota counts in.
const TODAY: &str = "(now() AT TIME ZONE 'America/Los_Angeles')::date";

/// One channel's row of `youtube_inbox`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub enabled: bool,
    pub poll_minutes: i32,
    pub daily_units: i32,
    pub units_today: i32,
    pub write_units_today: i32,
    pub newest_at: Option<DateTime<Utc>>,
    pub page_token: Option<String>,
    pub pending_at: Option<DateTime<Utc>>,
    pub paused_reason: Option<String>,
    pub last_read_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl Settings {
    /// What a channel that never opted in has.
    pub fn off() -> Self {
        Self {
            enabled: false,
            poll_minutes: DEFAULT_POLL_MINUTES,
            daily_units: DEFAULT_DAILY_UNITS,
            units_today: 0,
            write_units_today: 0,
            newest_at: None,
            page_token: None,
            pending_at: None,
            paused_reason: None,
            last_read_at: None,
            last_error: None,
        }
    }

    fn from_row(r: &QueryResult) -> Result<Self, DbErr> {
        Ok(Self {
            enabled: r.try_get("", "enabled")?,
            poll_minutes: r.try_get("", "poll_minutes")?,
            daily_units: r.try_get("", "daily_units")?,
            units_today: r.try_get("", "units_today")?,
            write_units_today: r.try_get("", "write_units_today")?,
            newest_at: r.try_get("", "newest_at")?,
            page_token: r.try_get("", "page_token")?,
            pending_at: r.try_get("", "pending_at")?,
            paused_reason: r.try_get("", "paused_reason")?,
            last_read_at: r.try_get("", "last_read_at")?,
            last_error: r.try_get("", "last_error")?,
        })
    }
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The channel's settings, with today's spending (a new Pacific day starts at 0 even before the
/// next tick says so), or `None` when it never opted in.
pub async fn load(db: &impl ConnectionTrait, account: Uuid) -> Result<Option<Settings>, DbErr> {
    let row = db
        .query_one_raw(sql(
            &format!(
                "SELECT enabled, poll_minutes, daily_units, \
                 CASE WHEN period < {TODAY} THEN 0 ELSE units_today END AS units_today, \
                 CASE WHEN period < {TODAY} THEN 0 ELSE write_units_today END AS write_units_today, \
                 newest_at, page_token, pending_at, \
                 CASE WHEN paused_reason IN ('budget', 'quota') AND period < {TODAY} \
                      THEN NULL ELSE paused_reason END AS paused_reason, \
                 last_read_at, last_error FROM youtube_inbox WHERE social_account_id = $1"
            ),
            vec![account.into()],
        ))
        .await?;
    row.as_ref().map(Settings::from_row).transpose()
}

/// What a person chose.
pub struct Choice {
    pub enabled: bool,
    pub poll_minutes: i32,
    pub daily_units: i32,
}

impl Choice {
    /// The error code when a value is out of range.
    pub fn checked(&self) -> Result<(), &'static str> {
        if !(MIN_POLL_MINUTES..=MAX_POLL_MINUTES).contains(&self.poll_minutes) {
            return Err("invalid_poll");
        }
        if !(MIN_DAILY_UNITS..=MAX_DAILY_UNITS).contains(&self.daily_units) {
            return Err("invalid_budget");
        }
        Ok(())
    }
}

/// Saves `c` for the channel. Switching on from off clears what the old state caused and reads at
/// the next tick; a raised budget lifts a budget pause. The cursor is kept, so a switch off and
/// on again reads only what is new.
pub async fn save(db: &impl ConnectionTrait, account: Uuid, c: &Choice) -> Result<(), DbErr> {
    db.execute_raw(sql(
        "INSERT INTO youtube_inbox (social_account_id, enabled, poll_minutes, daily_units) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (social_account_id) DO UPDATE SET \
           next_poll_at = CASE WHEN NOT youtube_inbox.enabled AND EXCLUDED.enabled THEN now() \
                          ELSE youtube_inbox.next_poll_at END, \
           errors = CASE WHEN NOT youtube_inbox.enabled AND EXCLUDED.enabled THEN 0 \
                    ELSE youtube_inbox.errors END, \
           page_token = CASE WHEN NOT youtube_inbox.enabled AND EXCLUDED.enabled THEN NULL \
                        ELSE youtube_inbox.page_token END, \
           pending_at = CASE WHEN NOT youtube_inbox.enabled AND EXCLUDED.enabled THEN NULL \
                        ELSE youtube_inbox.pending_at END, \
           paused_reason = CASE WHEN youtube_inbox.paused_reason = 'budget' \
                           AND EXCLUDED.daily_units > youtube_inbox.daily_units THEN NULL \
                           ELSE youtube_inbox.paused_reason END, \
           enabled = EXCLUDED.enabled, poll_minutes = EXCLUDED.poll_minutes, \
           daily_units = EXCLUDED.daily_units, updated_at = now()",
        vec![
            account.into(),
            c.enabled.into(),
            c.poll_minutes.into(),
            c.daily_units.into(),
        ],
    ))
    .await
    .map(|_| ())
}

/// Lets a paused channel read again at the next tick. It never reads past the day's budget.
pub async fn resume(db: &impl ConnectionTrait, account: Uuid) -> Result<(), DbErr> {
    db.execute_raw(sql(
        "UPDATE youtube_inbox SET paused_reason = NULL, errors = 0, last_error = NULL, \
         page_token = NULL, pending_at = NULL, next_poll_at = now(), updated_at = now() \
         WHERE social_account_id = $1",
        vec![account.into()],
    ))
    .await
    .map(|_| ())
}

/// What a pause means to a person.
pub fn pause_message(reason: &str, last_error: Option<&str>) -> String {
    match reason {
        "quota" => QUOTA_MESSAGE.into(),
        "budget" => "Today's read budget is used up (this channel's own, or the share of \
                     YouTube's quota that all channels together may use). Reading goes on after \
                     midnight Pacific time, or raise the budget."
            .into(),
        _ => format!(
            "YouTube failed {MAX_ERRORS} times in a row{}. It tries again by itself every {RESUME_HOURS} hours; check the channel, then resume.",
            last_error.map(|e| format!(" ({e})")).unwrap_or_default()
        ),
    }
}

/// Units every channel spent today (reads and writes): what `YOUTUBE_INBOX_UNITS` caps.
pub async fn shared_used(db: &impl ConnectionTrait) -> Result<i64, DbErr> {
    let r = db
        .query_one_raw(sql(
            &format!(
                "SELECT COALESCE(SUM(units_today + write_units_today), 0)::bigint AS n \
                 FROM youtube_inbox WHERE period = {TODAY}"
            ),
            vec![],
        ))
        .await?;
    r.map_or(Ok(0), |r| r.try_get("", "n"))
}

/// Reserves what one write costs the channel today, in one statement so two writes at once cannot
/// both pass the budget: `false` when the channel's `MAX_WRITE_UNITS` a Pacific day is spent. The
/// units stay spent when YouTube then refuses, since it charges a refused call too.
async fn reserve_write(db: &impl ConnectionTrait, account: Uuid) -> Result<bool, DbErr> {
    let stmt = sql(
        &format!(
            "INSERT INTO youtube_inbox (social_account_id, write_units_today) VALUES ($1, $2) \
             ON CONFLICT (social_account_id) DO UPDATE SET \
               write_units_today = (CASE WHEN youtube_inbox.period < {TODAY} THEN 0 \
                 ELSE youtube_inbox.write_units_today END) + $2, \
               units_today = CASE WHEN youtube_inbox.period < {TODAY} THEN 0 \
                 ELSE youtube_inbox.units_today END, \
               period = {TODAY} \
             WHERE (CASE WHEN youtube_inbox.period < {TODAY} THEN 0 \
               ELSE youtube_inbox.write_units_today END) + $2 <= $3 \
             RETURNING 1 AS ok"
        ),
        vec![account.into(), WRITE_UNITS.into(), MAX_WRITE_UNITS.into()],
    );
    Ok(db.query_one_raw(stmt).await?.is_some())
}

/// Why a round stopped.
enum Stop {
    Api(PostError),
    Db(DbErr),
    /// The day's budget has no room for another call.
    Budget,
}

impl From<PostError> for Stop {
    fn from(e: PostError) -> Self {
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
    units: i32,
    fresh: Vec<inbox_items::Model>,
    /// The newest top-level comment of this sweep, which becomes the cursor when it ends.
    pending: Option<DateTime<FixedOffset>>,
    /// Where an unfinished sweep goes on.
    resume: Option<String>,
    finished: bool,
    /// Read again at the next tick, not after the interval.
    soon: bool,
    videos: Vec<String>,
    reply_reads: usize,
    /// Units this round may spend: what is left of the channel's day and of every channel's share.
    limit: i32,
}

impl Round {
    /// Fails with `Budget` when the budgets cannot cover one more call.
    fn room(&self) -> Result<(), Stop> {
        if self.units >= self.limit {
            Err(Stop::Budget)
        } else {
            Ok(())
        }
    }
}

fn fresh_of(c: &Comment) -> Fresh {
    Fresh {
        kind: "comment",
        remote_id: c.id.clone(),
        remote_cid: None,
        parent: c.parent.clone(),
        root: (c.video.clone(), None),
        convo_id: None,
        author: c.author.clone(),
        text: c.text.clone(),
        at: c.at,
        permalink: c.url(),
        story: None,
        button_payload: None,
        attachments: Vec::new(),
    }
}

/// Reply counts last seen for these threads.
async fn seen(
    db: &impl ConnectionTrait,
    account: Uuid,
    threads: &[Thread],
) -> Result<HashMap<String, i32>, DbErr> {
    let ids: Vec<String> = threads.iter().map(|t| t.comment.id.clone()).collect();
    let rows = db
        .query_all_raw(sql(
            "SELECT thread_id, replies FROM youtube_threads \
             WHERE social_account_id = $1 AND thread_id = ANY($2)",
            vec![account.into(), ids.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("", "thread_id")?, r.try_get("", "replies")?)))
        .collect()
}

struct Sweep<'a> {
    app: &'a AppState,
    account: &'a social_accounts::Model,
    token: &'a str,
    s: &'a Settings,
    started: Instant,
    floor: DateTime<FixedOffset>,
    /// A first read: what it finds is history, already automated.
    first: bool,
}

impl Sweep<'_> {
    /// Stores one comment unless it is the channel's own or older than the first read goes.
    async fn keep(&self, round: &mut Round, c: &Comment) -> Result<(), DbErr> {
        if c.author.did == self.account.external_id || c.at < self.floor {
            return Ok(());
        }
        let stored =
            inbox::store_with(&self.app.db, self.account, fresh_of(c), (true, self.first)).await?;
        if let Some(item) = stored {
            if let Some(v) = &c.video {
                if !round.videos.contains(v) {
                    round.videos.push(v.clone());
                }
            }
            if !self.first {
                round.fresh.push(item);
            }
        }
        Ok(())
    }

    /// Stores a page of threads, oldest first, reading again (1 unit) the ones whose replies
    /// YouTube did not send in full and whose count changed.
    async fn threads(&self, round: &mut Round, page: &[Thread]) -> Result<(), Stop> {
        let known = seen(&self.app.db, self.account.id, page).await?;
        for t in page.iter().rev() {
            self.keep(round, &t.comment).await?;
            let mut replies = t.replies.clone();
            let total = t.total_replies as usize;
            let changed = known
                .get(&t.comment.id)
                .map_or(total > 0, |n| *n as usize != total);
            let mut complete = replies.len() >= total;
            if changed
                && !complete
                && round.reply_reads < MAX_REPLY_READS
                && self.started.elapsed() < READ_LIMIT
                && round.room().is_ok()
            {
                round.reply_reads += 1;
                round.units += 1; // YouTube charges a refused call too
                let video = t.comment.video.as_deref();
                replies = self
                    .app
                    .youtube
                    .replies(self.token, &t.comment.id, video)
                    .await?;
                complete = true;
            }
            for r in &replies {
                self.keep(round, r).await?;
            }
            if complete && changed {
                let stmt = sql(
                    "INSERT INTO youtube_threads (social_account_id, thread_id, replies) \
                     VALUES ($1, $2, $3) ON CONFLICT (social_account_id, thread_id) \
                     DO UPDATE SET replies = EXCLUDED.replies, seen_at = now()",
                    vec![
                        self.account.id.into(),
                        t.comment.id.clone().into(),
                        (total as i32).into(),
                    ],
                );
                self.app.db.execute_raw(stmt).await?;
            }
        }
        Ok(())
    }

    /// One sweep from the newest thread back to the cursor, or as far as time, pages and budget
    /// allow, in which case `round.resume` says where to go on.
    async fn read(&self, round: &mut Round) -> Result<(), Stop> {
        let cursor = self
            .s
            .newest_at
            .map_or(self.floor, |c| c.fixed_offset().max(self.floor));
        round.pending = self.s.pending_at.map(|p| p.fixed_offset());
        let mut page = self.s.page_token.clone();
        for _ in 0..MAX_PAGES {
            round.resume = page.clone();
            if self.started.elapsed() > READ_LIMIT {
                round.soon = true;
                return Ok(());
            }
            round.room()?;
            round.units += 1; // YouTube charges a refused call too
            let r = self
                .app
                .youtube
                .threads(self.token, &self.account.external_id, page.as_deref())
                .await?;
            if round.pending.is_none() {
                round.pending = r.items.first().map(|t| t.comment.at);
            }
            self.threads(round, &r.items).await?;
            let reached = r.items.last().is_none_or(|t| t.comment.at <= cursor);
            match r.next {
                Some(next) if !reached => page = Some(next),
                _ => {
                    round.resume = None;
                    round.finished = true;
                    return Ok(());
                }
            }
        }
        // More than a tick's pages: the rest goes on at the next tick.
        round.resume = page;
        round.soon = true;
        Ok(())
    }
}

/// Keeps the title and thumbnail of the videos that comments came in on (1 unit for 50), for the
/// context shown under a comment. A failure only costs the context.
async fn videos(app: &AppState, token: &str, round: &mut Round) -> Result<(), Stop> {
    if round.videos.is_empty() {
        return Ok(());
    }
    let rows = app
        .db
        .query_all_raw(sql(
            "SELECT video_id FROM youtube_videos WHERE video_id = ANY($1) \
             AND fetched_at > now() - make_interval(days => $2)",
            vec![round.videos.clone().into(), (VIDEO_DAYS as i32).into()],
        ))
        .await?;
    let have: Vec<String> = rows
        .iter()
        .filter_map(|r| r.try_get("", "video_id").ok())
        .collect();
    let need: Vec<&str> = round
        .videos
        .iter()
        .map(String::as_str)
        .filter(|v| !have.iter().any(|h| h == v))
        .collect();
    for ids in need.chunks(50) {
        round.room()?;
        round.units += 1; // YouTube charges a refused call too
        let found = app.youtube.videos(token, ids).await?;
        for v in found {
            let stmt = sql(
                "INSERT INTO youtube_videos (video_id, title, thumbnail) VALUES ($1, $2, $3) \
                 ON CONFLICT (video_id) DO UPDATE SET title = EXCLUDED.title, \
                 thumbnail = EXCLUDED.thumbnail, fetched_at = now()",
                vec![v.id.into(), v.title.into(), v.thumbnail.into()],
            );
            app.db.execute_raw(stmt).await?;
        }
    }
    Ok(())
}

/// Notes a problem without pausing or counting an error.
async fn note(db: &impl ConnectionTrait, id: Uuid, text: &str) -> Result<(), DbErr> {
    db.execute_raw(sql(
        "UPDATE youtube_inbox SET last_error = $2, updated_at = now() \
         WHERE social_account_id = $1",
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
    tracing::warn!(account = %id, reason, "youtube inbox paused");
    db.execute_raw(sql(
        "UPDATE youtube_inbox SET paused_reason = $2, last_error = $3, updated_at = now() \
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

/// What a YouTube refusal does to polling. Nothing retries: the next try is the next interval.
async fn on_error(db: &impl ConnectionTrait, id: Uuid, e: PostError) -> Result<(), DbErr> {
    match e {
        PostError::Reconnect => {
            crate::expiry::flag_reconnect(db, id).await;
            note(
                db,
                id,
                "YouTube refused the channel's token. Reconnect the channel.",
            )
            .await
        }
        PostError::Failed(why) if is_quota(&why) => pause(db, id, "quota", Some(&why)).await,
        PostError::Denied | PostError::Failed(_) => {
            let said = match &e {
                PostError::Failed(why) => why.clone(),
                _ => "YouTube did not allow reading comments. Reconnect the channel and accept \
                      every permission."
                    .into(),
            };
            tracing::warn!(account = %id, why = %said, "youtube inbox read failed");
            let n = db
                .query_one_raw(sql(
                    "UPDATE youtube_inbox SET errors = errors + 1, last_error = $2, \
                     updated_at = now() WHERE social_account_id = $1 RETURNING errors",
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

/// Reads one channel's comments, stores what is new, spends, and moves its cursor.
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
    let token = match controllers::youtube::fresh_token(app, &account).await {
        Ok(t) => t,
        Err(e) => return on_error(db, id, e).await,
    };
    // Every channel's reads together stay inside the shared share, whatever each one was given.
    let shared_left = app.youtube.inbox_units - shared_used(db).await?;
    if shared_left < 1 {
        return Ok(());
    }
    // A read after a long gap (switched off, or paused for days) is history too: whoever wrote in
    // between has waited too long for an automation to answer.
    let stale = s
        .last_read_at
        .is_none_or(|t| t < Utc::now() - Duration::minutes(i64::from(s.poll_minutes) * 2 + 10));
    let sweep = Sweep {
        app,
        account: &account,
        token: &token,
        s: &s,
        started: Instant::now(),
        floor: (Utc::now() - Duration::days(inbox::FIRST_DAYS)).fixed_offset(),
        first: s.newest_at.is_none() || (stale && s.page_token.is_none()),
    };
    let mut round = Round {
        limit: i64::from(s.daily_units - s.units_today)
            .min(shared_left)
            .clamp(0, i32::MAX.into()) as i32,
        ..Round::default()
    };
    let mut stop = sweep.read(&mut round).await.err();
    if stop.is_none() || matches!(stop, Some(Stop::Budget)) {
        // Titles are context: a failure here is noted, never retried, and does not undo the read.
        match videos(app, &token, &mut round).await {
            Err(Stop::Api(e @ PostError::Failed(_))) if stop.is_none() => {
                // Only the quota pauses the channel; a title that cannot be read costs the context.
                if matches!(&e, PostError::Failed(w) if is_quota(w)) {
                    stop = Some(Stop::Api(e));
                } else {
                    tracing::warn!(account = %id, error = ?e, "youtube video titles not read");
                }
            }
            Err(Stop::Db(e)) => return Err(e),
            _ => {}
        }
    }
    if matches!(stop, Some(Stop::Budget)) {
        round.soon = false;
    }
    let ok = stop.is_none();
    db.execute_raw(sql(
        &format!(
            "UPDATE youtube_inbox SET \
             units_today = (CASE WHEN period < {TODAY} THEN 0 ELSE units_today END) + $2, \
             write_units_today = CASE WHEN period < {TODAY} THEN 0 ELSE write_units_today END, \
             period = {TODAY}, \
             newest_at = CASE WHEN $3 THEN COALESCE($4, newest_at, now()) ELSE newest_at END, \
             page_token = $5, pending_at = CASE WHEN $3 THEN NULL ELSE $4 END, \
             last_read_at = now(), updated_at = now(), \
             errors = CASE WHEN $6 THEN 0 ELSE errors END, \
             last_error = CASE WHEN $6 THEN NULL ELSE last_error END, \
             next_poll_at = CASE WHEN $7 THEN now() + interval '1 minute' \
                            ELSE now() + make_interval(mins => poll_minutes) END \
             WHERE social_account_id = $1"
        ),
        vec![
            id.into(),
            round.units.into(),
            round.finished.into(),
            round.pending.into(),
            round.resume.clone().into(),
            ok.into(),
            round.soon.into(),
        ],
    ))
    .await?;
    match stop.take() {
        None => {}
        Some(Stop::Db(e)) => return Err(e),
        Some(Stop::Budget) => pause(db, id, "budget", None).await?,
        Some(Stop::Api(e)) => on_error(db, id, e).await?,
    }
    // Oldest first. The sweep of `automations` picks these up again if this is cut short.
    round.fresh.sort_by_key(|i| i.received_at);
    for item in &round.fresh {
        if let Err(e) = automations::run(app, &account, item).await {
            tracing::error!(error = %e, item = %item.id, "youtube automations failed");
        }
    }
    Ok(())
}

/// Deletes YouTube data kept past YouTube's 30 days: comments (their replies go with them),
/// thread counts, video titles, and the names of contacts not seen since (a contact with a tag or
/// a note stays, as a bare id).
pub async fn purge(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let old = format!("now() - interval '{KEEP_DAYS} days'");
    for stmt in [
        format!("DELETE FROM inbox_items WHERE platform = 'youtube' AND received_at < {old}"),
        format!("DELETE FROM youtube_threads WHERE seen_at < {old}"),
        format!("DELETE FROM youtube_videos WHERE fetched_at < {old}"),
        format!(
            "DELETE FROM contacts c WHERE platform = 'youtube' AND last_seen_at < {old} \
             AND notes IS NULL AND NOT EXISTS (SELECT 1 FROM contact_tags t WHERE t.contact_id = c.id)"
        ),
        format!(
            "UPDATE contacts SET handle = NULL, display_name = NULL, avatar_url = NULL \
             WHERE platform = 'youtube' AND last_seen_at < {old} \
             AND (handle IS NOT NULL OR display_name IS NOT NULL OR avatar_url IS NOT NULL)"
        ),
    ] {
        db.execute_unprepared(&stmt).await?;
    }
    Ok(())
}

/// One round: a new Pacific day's channels start fresh, YouTube data past its 30 days goes (once
/// an hour), then, while the shared budget lasts, every channel that is on, not paused, due, and
/// not waiting for a reconnect is claimed (its next poll moves on first, so a crash never makes
/// it poll twice at once) and read, four at a time.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    app.db
        .execute_raw(sql(
            &format!(
                "UPDATE youtube_inbox SET paused_reason = NULL, errors = {}, next_poll_at = now(), \
                 updated_at = now() WHERE paused_reason = 'errors' \
                 AND updated_at < now() - make_interval(hours => {RESUME_HOURS})",
                MAX_ERRORS - 1
            ),
            vec![],
        ))
        .await?;
    app.db
        .execute_raw(sql(
            &format!(
                "UPDATE youtube_inbox SET period = {TODAY}, units_today = 0, write_units_today = 0, \
                 paused_reason = CASE WHEN paused_reason IN ('budget', 'quota') THEN NULL \
                                 ELSE paused_reason END WHERE period < {TODAY}"
            ),
            vec![],
        ))
        .await?;
    let purge_due = {
        let mut last = PURGED.lock().unwrap_or_else(|e| e.into_inner());
        let due = last.is_none_or(|t| t.elapsed() > Wait::from_secs(3600));
        if due {
            *last = Some(Instant::now());
        }
        due
    };
    if purge_due {
        if let Err(e) = purge(&app.db).await {
            tracing::warn!(error = %e, "youtube data purge failed");
        }
    }
    if shared_used(&app.db).await? >= app.youtube.inbox_units {
        return Ok(());
    }
    let claimed = app
        .db
        .query_all_raw(sql(
            "UPDATE youtube_inbox SET next_poll_at = now() + make_interval(mins => poll_minutes) \
             WHERE social_account_id IN (SELECT y.social_account_id FROM youtube_inbox y \
               JOIN social_accounts a ON a.id = y.social_account_id \
               WHERE y.enabled AND y.paused_reason IS NULL AND y.next_poll_at <= now() \
               AND COALESCE(a.meta->>'reconnect', 'false') <> 'true' \
               ORDER BY y.next_poll_at LIMIT 20 FOR UPDATE OF y SKIP LOCKED) \
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
                    tracing::error!(error = %e, account = %id, "youtube inbox poll failed");
                }
            })
        })
        .collect();
    for read in reads {
        let _ = read.await;
    }
    Ok(())
}

// ---- What a person does to a comment: reply, hold, reject, delete our reply. Each is one call
// (50 units) and never retried; what it cost is counted on the channel (`reserve_write`).

/// YouTube's refusal as the inbox words it.
fn refused(e: PostError) -> InboxError {
    match e {
        PostError::Reconnect | PostError::Denied => InboxError::YouTubeReconnect,
        PostError::Failed(why) => InboxError::YouTube(why),
    }
}

async fn token_of(app: &AppState, account: &social_accounts::Model) -> Result<String, InboxError> {
    match controllers::youtube::fresh_token(app, account).await {
        Err(PostError::Reconnect) => {
            crate::expiry::flag_reconnect(&app.db, account.id).await;
            Err(InboxError::YouTubeReconnect)
        }
        other => other.map_err(refused),
    }
}

fn flag(item: &inbox_items::Model, key: &str) -> bool {
    item.moderation[key].as_bool() == Some(true)
}

/// `reserve_write` as the inbox words it.
async fn reserve(app: &AppState, account: &social_accounts::Model) -> Result<(), InboxError> {
    match reserve_write(&app.db, account.id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(InboxError::Invalid("youtube_write_budget")),
        Err(e) => {
            tracing::error!(error = %e, "youtube write budget failed");
            Err(InboxError::YouTube(
                "This could not be sent. Try again.".into(),
            ))
        }
    }
}

/// Replies publicly to `item` as the channel, once: under its top-level comment (YouTube replies
/// are one level deep, so a reply to a reply goes under the same top-level comment, and a reply
/// id is `<top-level id>.<suffix>`). A DM does not exist on YouTube. The id of what went out.
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    text: &str,
) -> Result<String, InboxError> {
    if item.kind != "comment" {
        return Err(InboxError::Invalid("not_repliable"));
    }
    let top = item.remote_id.split('.').next().unwrap_or(&item.remote_id);
    // A rejected comment, or a reply under one, is hidden for good: YouTube would refuse (and charge).
    let parent = app
        .db
        .query_one_raw(sql(
            "SELECT moderation FROM inbox_items WHERE social_account_id = $1 AND remote_id = $2",
            vec![account.id.into(), top.into()],
        ))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "youtube parent read failed");
            InboxError::YouTube("This could not be sent. Try again.".into())
        })?
        .and_then(|r| r.try_get::<serde_json::Value>("", "moderation").ok());
    if flag(item, "rejected") || parent.is_some_and(|m| m["rejected"].as_bool() == Some(true)) {
        return Err(InboxError::Invalid("rejected"));
    }
    let token = token_of(app, account).await?;
    reserve(app, account).await?;
    let sent = app.youtube.reply(&token, top, text).await;
    sent.map_err(refused)
}

/// What a person can do to a comment of a channel's video, besides reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Moderate {
    /// Hide it until the owner approves it (`heldForReview`).
    Hold,
    /// Show a held comment again.
    Publish,
    /// Hide it for good.
    Reject,
    /// Hide it for good and ban its author from the channel's comments.
    Ban,
}

impl Moderate {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hold" => Some(Self::Hold),
            "publish" => Some(Self::Publish),
            "reject" => Some(Self::Reject),
            "ban" => Some(Self::Ban),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Publish => "publish",
            Self::Reject => "reject",
            Self::Ban => "ban",
        }
    }
}

/// The actions that work on `item` now: hold or publish again, reject, and reject with a ban.
/// Nothing for a rejected comment (YouTube never shows it again) or an item that is no comment.
pub fn moderations(item: &inbox_items::Model) -> Vec<&'static str> {
    if item.platform != "youtube" || item.kind != "comment" || flag(item, "rejected") {
        return Vec::new();
    }
    vec![
        if flag(item, "held") {
            "publish"
        } else {
            "hold"
        },
        "reject",
        "ban",
    ]
}

/// Moderates a comment on YouTube and keeps what was done on the item (a rejected comment is also
/// `done`). The inner `Err` says why nothing was done.
pub async fn moderate(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    action: Moderate,
) -> Result<Result<(), InboxError>, DbErr> {
    // Only what changes something: holding a held comment would spend 50 units for nothing.
    if !moderations(item).contains(&action.as_str()) {
        return Ok(Err(InboxError::Invalid("not_supported")));
    }
    let token = match token_of(app, account).await {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = reserve(app, account).await {
        return Ok(Err(e));
    }
    let (status, ban, kept) = match action {
        Moderate::Hold => ("heldForReview", false, serde_json::json!({"held": true})),
        Moderate::Publish => ("published", false, serde_json::json!({"held": false})),
        Moderate::Reject => ("rejected", false, serde_json::json!({"rejected": true})),
        Moderate::Ban => (
            "rejected",
            true,
            serde_json::json!({"rejected": true, "banned": true}),
        ),
    };
    let done = app
        .youtube
        .moderate(&token, &item.remote_id, status, ban)
        .await;
    // A comment that is gone (its author deleted it) is as hidden as a rejected one. A ban needs
    // the comment, so it is never taken as done.
    let gone = matches!(&done, Err(PostError::Failed(w)) if w.contains("is gone on YouTube"))
        && action == Moderate::Reject;
    if let (Err(e), false) = (done, gone) {
        return Ok(Err(refused(e)));
    }
    let state = if action == Moderate::Reject || action == Moderate::Ban {
        ", state = 'done'"
    } else {
        ""
    };
    let stmt = sql(
        &format!(
            "UPDATE inbox_items SET moderation = moderation || $2::jsonb{state} WHERE id = $1"
        ),
        vec![item.id.into(), kept.into()],
    );
    app.db.execute_raw(stmt).await?;
    Ok(Ok(()))
}

/// Deletes a public reply of the channel that went out from this inbox (`reply` is one of the
/// item's replies): only its author, the channel, can. The reply is then gone from the item, and
/// the item is unanswered again when it was the last one.
pub async fn delete_reply(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    reply: Uuid,
) -> Result<Result<(), InboxError>, DbErr> {
    let found = app
        .db
        .query_one_raw(sql(
            "SELECT remote_id FROM inbox_replies WHERE id = $1 AND inbox_item_id = $2 \
             AND kind = 'public'",
            vec![reply.into(), item.id.into()],
        ))
        .await?;
    let remote: Option<String> = found.map_or(Ok(None), |r| r.try_get("", "remote_id"))?;
    let Some(remote) = remote else {
        return Ok(Err(InboxError::Invalid("reply_not_found")));
    };
    let token = match token_of(app, account).await {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = reserve(app, account).await {
        return Ok(Err(e));
    }
    let done = app.youtube.delete_comment(&token, &remote).await;
    if let Err(e) = done {
        return Ok(Err(refused(e)));
    }
    // One statement, so the reply and the item's answered mark never disagree.
    app.db
        .execute_raw(sql(
            "WITH d AS (DELETE FROM inbox_replies WHERE id = $1) \
             UPDATE inbox_items SET replied_at = (SELECT max(created_at) FROM inbox_replies \
             WHERE inbox_item_id = $2 AND id <> $1) WHERE id = $2",
            vec![reply.into(), item.id.into()],
        ))
        .await?;
    Ok(Ok(()))
}
