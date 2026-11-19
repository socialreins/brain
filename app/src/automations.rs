//! Automations: a trigger, conditions and actions, stored as typed JSON. Each new inbox item runs
//! the automations of the trigger its kind stands for (`trigger_of`): `comment_received`,
//! `mention_received`, `dm_received`, `story_reply` (an Instagram DM replying to a story, which
//! never runs `dm_received` ones), `button_clicked` (a click on a quick reply button, which
//! never runs them either) and `new_follower` (a Bluesky follow, once per follower per
//! account ever). A run row unique per
//! (automation, item) is inserted before anything is sent, together with a hit of the team's
//! daily cap, and a run is never retried, the same rules as publishing.
//!
//! An automation holds `actions`, all sent at once, or `steps`, a flow (`Step`): sends and
//! waits in order. A flow's run claims each step before it is sent (`claim_step`, which also
//! takes the send's hit of the daily cap), so a step runs at most once; a `wait` parks the run
//! as `waiting` and `tick` takes it up again once `resume_at` has come. The first send that
//! does not go out ends the run. A run keeps the hash of the steps it started on, and every
//! claim and park checks it (`UNCHANGED`): once the automation is off, gone or has other
//! steps, the run ends `skipped: automation_changed` instead of going on.
use sea_orm::{
    sea_query::Query, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, QueryFilter, QueryOrder, Statement, TransactionTrait, Value,
};
use std::{
    ops::Range,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    connectors::bluesky::InboxError,
    controllers::{connections::live_sets, pages},
    inbox::{self, Button, Via},
    models::{
        automation_runs, automations, inbox_items, social_account_social_set as membership,
        social_accounts,
    },
    rate_limit::reserve_in,
    state::AppState,
};

pub const TRIGGERS: [&str; 6] = [
    "button_clicked",
    "comment_received",
    "dm_received",
    "mention_received",
    "new_follower",
    "story_reply",
];
/// Automation runs that act, per team per day (`rate_limit::reserve_in`).
pub const RUNS_PER_DAY: i64 = 500;
/// A run `running` this long was cut off (crash, restart); it fails and is not retried.
const STUCK_MINUTES: i32 = 15;
/// Most steps in a flow, and the longest `wait` (7 days).
pub const MAX_STEPS: usize = 10;
pub const MAX_WAIT_MINUTES: u32 = 10_080;
const MAX_BUTTONS: usize = 3;
/// Waiting runs one `tick` takes up.
const RESUME_BATCH: i64 = 100;
const FLOW_LOCK: i64 = 0x0046_4c4f_5753_5449;
/// How long one round starts runs it took up; the rest go back to `waiting` for the next
/// round. Well under `STUCK_MINUTES`, so a run queued behind slow sends is never called stuck.
pub const ROUND: Duration = Duration::from_secs(300);
/// Whether run `r` may go on: its automation is on and still has the steps the run started on,
/// which are also the steps in hand (`$4`). The md5 of jsonb text is the same for equal JSON.
const UNCHANGED: &str = "r.steps_hash = md5($4::jsonb::text) AND EXISTS (SELECT 1 \
    FROM automations a WHERE a.id = r.automation_id AND a.active \
    AND md5(a.steps::text) = r.steps_hash)";
const CHANGED: &str = "automation_changed";
/// Longest `{name}` put into a text.
const MAX_NAME_CHARS: usize = 64;
/// What `{page}` counts as when a text is checked.
// shortcut: an address over 100 characters (a `WEB_URL` over 85) can make a checked text too
// long to send, which fails that action; derive this from `WEB_URL` if one ever is.
const MAX_PAGE_CHARS: usize = 100;

pub fn quota_bucket(team_id: Uuid) -> String {
    format!("automation:team:{team_id}")
}

/// When an automation acts on an item. Empty lists mean "any". At most one of
/// `social_set_id` and `account_ids`. Every trigger takes the scope (`social_set_id` or
/// `account_ids`); `keywords` all but `new_follower` and `button_clicked`; `post_remote_ids` and
/// `first_per_author` only `comment_received`; `first_message` only `dm_received`; `payloads`
/// only `button_clicked`, which needs them.
#[derive(Serialize, Deserialize, ToSchema, Default, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Conditions {
    /// Comments on accounts in this live set of the team.
    #[serde(default)]
    pub social_set_id: Option<Uuid>,
    /// Comments on these accounts of the team, at most 100.
    #[serde(default)]
    pub account_ids: Vec<Uuid>,
    /// Comments under these posts (the post replied to, or the thread's first post), e.g.
    /// Bluesky `at://` URIs or Facebook and Instagram post ids; at most 100.
    #[serde(default)]
    pub post_remote_ids: Vec<String>,
    /// Any of these, case-insensitive, in the comment; at most 20 of 1 to 100 characters.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Only an author's first matching comment runs this automation.
    #[serde(default)]
    pub first_per_author: bool,
    /// Only a DM that is the first one ever stored from its author on the account (a welcome).
    #[serde(default)]
    pub first_message: bool,
    /// `button_clicked` only, and required there: a click on a button with any of these
    /// payloads; 1 to 20, each 1 to 64 characters of `a-z`, `0-9`, `_` and `-`.
    #[serde(default)]
    pub payloads: Vec<String>,
}

/// What an automation does. Texts may hold `{name}`, the commenter's display name ("there"
/// when it is not plainly a name), and `{page}`, the address of the public page of the post the
/// comment is under, counted as 100 characters and written with a space (or the start) before
/// it. An automation whose action holds `{page}` does not run, and records no run, on a comment
/// that is not under one of the team's posts or whose post has no live page.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// A DM to the item's author, at most 1000 graphemes: in the same conversation for a DM, a
    /// new one for a follower. On Facebook and Instagram, for a comment, it is the private reply
    /// to the comment, once per comment and within 7 days.
    SendDm { text: String },
    /// A reply under the comment or mention, at most 300 graphemes.
    ReplyPublicly { text: String },
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SendDm { .. } => "send_dm",
            Self::ReplyPublicly { .. } => "reply_publicly",
        }
    }

    fn via_text(&self) -> (Via, &str) {
        match self {
            Self::SendDm { text } => (Via::Dm, text),
            Self::ReplyPublicly { text } => (Via::Public, text),
        }
    }
}

/// One step of a flow. Texts take `{name}` and `{page}` as an `Action`'s do.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// A DM as `Action::SendDm` sends it, with up to 3 buttons under it: quick replies on
    /// Facebook and Instagram, numbered lines after the text ("1. Label") on Bluesky.
    SendDm {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        buttons: Vec<Button>,
    },
    /// A reply under the comment or mention, at most 300 graphemes.
    ReplyPublicly { text: String },
    /// The run stops here for 1 to 10080 minutes (7 days). Never the first or last step.
    Wait { minutes: u32 },
}

impl Step {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SendDm { .. } => "send_dm",
            Self::ReplyPublicly { .. } => "reply_publicly",
            Self::Wait { .. } => "wait",
        }
    }

    /// How a send step goes out, its text and its buttons; `None` for a wait.
    fn sends(&self) -> Option<(Via, &str, &[Button])> {
        match self {
            Self::SendDm { text, buttons } => Some((Via::Dm, text, buttons)),
            Self::ReplyPublicly { text } => Some((Via::Public, text, &[])),
            Self::Wait { .. } => None,
        }
    }
}

/// The trigger a new item runs: by its kind, and a button click and a story reply apart from
/// other DMs.
pub fn trigger_of(item: &inbox_items::Model) -> Option<&'static str> {
    match item.kind.as_str() {
        "comment" => Some("comment_received"),
        "mention" => Some("mention_received"),
        "dm" if item.button_payload.is_some() => Some("button_clicked"),
        "dm" if item.story_reply => Some("story_reply"),
        "dm" => Some("dm_received"),
        "follow" => Some("new_follower"),
        _ => None,
    }
}

/// A button payload: 1 to 64 characters of `a-z`, `0-9`, `_` and `-`.
fn payload_ok(p: &str) -> bool {
    (1..=64).contains(&p.len())
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Checks the trigger and that the conditions are ones it takes, and normalizes them.
fn check_conditions(trigger: &str, c: &mut Conditions) -> Result<(), &'static str> {
    if !TRIGGERS.contains(&trigger) {
        return Err("invalid_trigger");
    }
    let comment = trigger == "comment_received";
    let button = trigger == "button_clicked";
    let fits_trigger = (comment || (c.post_remote_ids.is_empty() && !c.first_per_author))
        && (trigger == "dm_received" || !c.first_message)
        && (!(button || trigger == "new_follower") || c.keywords.is_empty())
        && (button != c.payloads.is_empty());
    let ids_ok = c.post_remote_ids.len() <= 100
        && c.post_remote_ids
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 300 && !p.chars().any(char::is_whitespace));
    if !fits_trigger
        || !ids_ok
        || c.account_ids.len() > 100
        || (c.social_set_id.is_some() && !c.account_ids.is_empty())
        || c.payloads.len() > 20
        || !c.payloads.iter().all(|p| payload_ok(p))
    {
        return Err("invalid_conditions");
    }
    c.keywords = inbox::keywords(&c.keywords).ok_or("invalid_conditions")?;
    c.account_ids.sort();
    c.account_ids.dedup();
    c.payloads.sort();
    c.payloads.dedup();
    Ok(())
}

/// A public reply needs a thread to go under, and `{page}` is the page of the post a comment or
/// mention hangs off: DMs, clicks and follows have neither.
fn public(trigger: &str) -> bool {
    matches!(trigger, "comment_received" | "mention_received")
}

/// A text with every `{name}` and `{page}` at its longest, so a long name or address never
/// makes a checked text too long.
fn longest(t: &str) -> String {
    t.replace("{name}", &"n".repeat(MAX_NAME_CHARS))
        .replace("{page}", &"p".repeat(MAX_PAGE_CHARS))
}

/// Checks a flow's shape, as `validate` checks actions, and trims its button labels: 1 to 10
/// steps, no wait first or last (so at least one send), each step one the trigger takes.
pub fn validate_steps(
    trigger: &str,
    conditions: &mut Conditions,
    steps: &mut [Step],
) -> Result<(), &'static str> {
    check_conditions(trigger, conditions)?;
    let wait = |s: Option<&Step>| matches!(s, Some(Step::Wait { .. }));
    if steps.is_empty() || steps.len() > MAX_STEPS || wait(steps.first()) || wait(steps.last()) {
        return Err("invalid_steps");
    }
    for step in steps {
        match step {
            Step::Wait { minutes } if !(1..=MAX_WAIT_MINUTES).contains(minutes) => {
                return Err("invalid_steps");
            }
            Step::ReplyPublicly { .. } if !public(trigger) => return Err("invalid_steps"),
            Step::SendDm { buttons, .. } => {
                for b in buttons.iter_mut() {
                    b.label = b.label.trim().to_owned();
                }
                let fine = |b: &Button| {
                    (1..=20).contains(&b.label.chars().count())
                        && !b.label.chars().any(char::is_control)
                        && payload_ok(&b.payload)
                };
                if buttons.len() > MAX_BUTTONS || !buttons.iter().all(fine) {
                    return Err("invalid_steps");
                }
            }
            _ => {}
        }
        if let Some((via, text, buttons)) = step.sends() {
            // Checked with the labels as lines, the longest form it goes out in.
            let fits = inbox::fits(via, &longest(&inbox::numbered(text, buttons)));
            let paged = !public(trigger) && text.contains("{page}");
            if paged || !fits || !page_stands_alone(text) {
                return Err("invalid_text");
            }
        }
    }
    Ok(())
}

/// Checks an automation's shape; the error code otherwise. Account and set ids are checked
/// against the team by the caller.
pub fn validate(
    trigger: &str,
    conditions: &mut Conditions,
    actions: &[Action],
    fallback: Option<&str>,
) -> Result<(), &'static str> {
    check_conditions(trigger, conditions)?;
    let public = public(trigger);
    if !public
        && actions
            .iter()
            .any(|a| matches!(a, Action::ReplyPublicly { .. }))
    {
        return Err("invalid_actions");
    }
    if !public && fallback.is_some() {
        return Err("invalid_fallback");
    }
    if !public && actions.iter().any(|a| a.via_text().1.contains("{page}")) {
        return Err("invalid_text");
    }
    let mut kinds: Vec<&str> = actions.iter().map(Action::kind).collect();
    kinds.sort();
    kinds.dedup();
    if actions.is_empty() || kinds.len() != actions.len() {
        return Err("invalid_actions");
    }
    for a in actions {
        let (via, text) = a.via_text();
        if !inbox::fits(via, &longest(text)) || !page_stands_alone(text) {
            return Err("invalid_text");
        }
    }
    if let Some(f) = fallback {
        let dm = actions.iter().any(|a| matches!(a, Action::SendDm { .. }));
        if !dm || !inbox::fits(Via::Public, &longest(f)) || !page_stands_alone(f) {
            return Err("invalid_fallback");
        }
    }
    Ok(())
}

/// Whether every `{page}` can become a clean link: at the start or after whitespace or `(`,
/// and followed by the end, whitespace or closing punctuation the linker leaves out
/// (`socialreins_core::post::links`). "details:{page}" would send plain text, "{page}]" a
/// link that includes the bracket.
fn page_stands_alone(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find("{page}") {
        let before = rest[..at].chars().next_back();
        // The linker runs to the next whitespace, so all of that must be closing punctuation.
        let tail = rest[at + 6..].split_whitespace().next().unwrap_or("");
        let glued = !rest[at + 6..].starts_with(char::is_whitespace);
        let opens = before.is_none_or(|c| c.is_whitespace() || c == '(');
        let closes = !glued || tail.chars().all(|c| ".,;:!?)".contains(c));
        if !opens || !closes {
            return false;
        }
        rest = &rest[at + 6..];
    }
    true
}

/// What conditions look at in an item.
pub struct Facts<'a> {
    pub account_id: Uuid,
    /// Live sets holding the account.
    pub account_sets: &'a [Uuid],
    pub parent: Option<&'a str>,
    pub root: Option<&'a str>,
    pub text: &'a str,
    /// No earlier run of this automation for the same author.
    pub first_from_author: bool,
    /// No earlier DM item from the same author on the account.
    pub first_message: bool,
}

pub fn matches(c: &Conditions, f: &Facts<'_>) -> bool {
    let set_ok = c.social_set_id.is_none_or(|s| f.account_sets.contains(&s));
    let account_ok = c.account_ids.is_empty() || c.account_ids.contains(&f.account_id);
    let post_ok = c.post_remote_ids.is_empty()
        || c.post_remote_ids
            .iter()
            .any(|p| Some(p.as_str()) == f.parent || Some(p.as_str()) == f.root);
    let text = f.text.to_lowercase();
    let keyword_ok =
        c.keywords.is_empty() || c.keywords.iter().any(|k| text.contains(&k.to_lowercase()));
    set_ok
        && account_ok
        && post_ok
        && keyword_ok
        && (!c.first_per_author || f.first_from_author)
        && (!c.first_message || f.first_message)
}

/// The commenter's display name when it is plainly a name: letters, digits, spaces, `'`, `-`
/// and `_` only, so it cannot carry a link, domain or mention (`evil.com`, `@someone`, `a/b`),
/// cut to 64 characters. A name with other punctuation ("Ada R. Lovelace - ...") gives its
/// first word, unless anything in it could link or mention. Handles are never used: they are
/// domains.
fn safe_name(name: Option<&str>) -> Option<String> {
    let name = name?.trim();
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || c == ' ' || matches!(c, '\'' | '-' | '_'))
    };
    if plain(name) {
        return Some(name.chars().take(MAX_NAME_CHARS).collect());
    }
    let chars: Vec<char> = name.chars().collect();
    let dot = |c: char| matches!(c, '.' | '\u{3002}' | '\u{FF0E}' | '\u{FF61}');
    let linky = chars.iter().enumerate().any(|(i, &c)| {
        c.is_control()
            || matches!(c, '@' | '\u{FF20}' | '/' | ':' | '#')
            || (dot(c) && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric()))
    });
    let first = name.split_whitespace().next()?.trim_end_matches([',', ';']);
    (!linky && plain(first)).then(|| first.chars().take(MAX_NAME_CHARS).collect())
}

/// `template` with each `{name}` replaced by the author's display name when safe (see
/// `safe_name`), else "there"; and the byte ranges filled in, which must never become links.
pub fn render(template: &str, name: Option<&str>) -> (String, Vec<Range<usize>>) {
    let name = safe_name(name).unwrap_or_else(|| "there".to_owned());
    let mut out = String::new();
    let mut filled = Vec::new();
    let mut parts = template.split("{name}").peekable();
    while let Some(part) = parts.next() {
        out.push_str(part);
        if parts.peek().is_some() {
            filled.push(out.len()..out.len() + name.len());
            out.push_str(&name);
        }
    }
    (out, filled)
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Runs cut off mid-send fail as `interrupted`; they are never sent again. A `waiting` run is
/// not one: only `running` ones are looked at, by when their step was last claimed
/// (`updated_at`). A run a round took up and never started (`resume_at` still set) sent
/// nothing for its next step, so it goes back to `waiting`.
pub async fn recover_stuck(db: &DatabaseConnection) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE automation_runs SET status = 'waiting', updated_at = now() \
         WHERE status = 'running' AND resume_at IS NOT NULL \
         AND updated_at < now() - make_interval(mins => $1)",
        vec![STUCK_MINUTES.into()],
    );
    db.execute_raw(stmt).await?;
    let stmt = sql(
        "UPDATE automation_runs SET status = 'failed', reason = 'interrupted', \
         updated_at = now() WHERE status = 'running' \
         AND updated_at < now() - make_interval(mins => $1)",
        vec![STUCK_MINUTES.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Runs every active automation of the item's team for the item's trigger (`trigger_of`) that
/// was on when the item came.
pub async fn run(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<(), DbErr> {
    let db = &app.db;
    let Some(trigger) = trigger_of(item) else {
        return Ok(());
    };
    let autos = automations::Entity::find()
        .filter(automations::Column::TeamId.eq(item.team_id))
        .filter(automations::Column::Active.eq(true))
        .filter(automations::Column::Trigger.eq(trigger))
        .filter(automations::Column::ActivatedAt.lte(item.received_at))
        .order_by_asc(automations::Column::CreatedAt)
        .all(db)
        .await?;
    // Comments from before the account was connected (a first read goes back 7 days) never
    // run automations.
    if autos.is_empty() || item.received_at < account.created_at {
        return Ok(());
    }
    // Comments by the team's own accounts never run them either, so two automated accounts
    // cannot answer each other in a loop.
    let own = social_accounts::Entity::find()
        .filter(social_accounts::Column::TeamId.eq(item.team_id))
        .filter(social_accounts::Column::ExternalId.eq(&item.author_id))
        .one(db)
        .await?;
    if own.is_some() {
        return Ok(());
    }
    // A follower is welcomed once per account ever: an unfollow and a new follow stores a new
    // item (a new record) but runs nothing.
    if item.kind == "follow" {
        let stmt = sql(
            "SELECT EXISTS (SELECT 1 FROM inbox_items WHERE social_account_id = $1 \
             AND kind = 'follow' AND author_id = $2 AND id < $3) AS ok",
            vec![
                account.id.into(),
                item.author_id.clone().into(),
                item.id.into(),
            ],
        );
        let again: bool = match db.query_one_raw(stmt).await? {
            Some(r) => r.try_get("", "ok")?,
            None => false,
        };
        if again {
            return Ok(());
        }
    }
    let in_live = Query::select()
        .column(membership::Column::SocialSetId)
        .from(membership::Entity)
        .and_where(membership::Column::SocialAccountId.eq(account.id))
        .and_where(membership::Column::SocialSetId.in_subquery(live_sets()))
        .to_owned();
    let rows = db.query_all(&in_live).await?;
    let sets: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.try_get("", "social_set_id").ok())
        .collect();
    for a in autos {
        let parsed = (
            serde_json::from_value::<Conditions>(a.conditions.clone()),
            serde_json::from_value::<Vec<Action>>(a.actions.clone()),
            a.steps.clone().map(serde_json::from_value::<Vec<Step>>),
        );
        let (Ok(cond), Ok(actions), Ok(steps)) = (parsed.0, parsed.1, parsed.2.transpose()) else {
            tracing::warn!(automation = %a.id, "automation json unreadable");
            continue;
        };
        let first = if cond.first_per_author {
            let stmt = sql(
                "SELECT NOT EXISTS (SELECT 1 FROM automation_runs r \
                 JOIN inbox_items i ON i.id = r.inbox_item_id \
                 WHERE r.automation_id = $1 AND i.author_id = $2 AND i.id <> $3) AS ok",
                vec![a.id.into(), item.author_id.clone().into(), item.id.into()],
            );
            match db.query_one_raw(stmt).await? {
                Some(r) => r.try_get("", "ok")?,
                None => false,
            }
        } else {
            true
        };
        // Ids are v7 UUIDs and ingest stores oldest first, so a smaller id came earlier.
        let first_message = if cond.first_message {
            let stmt = sql(
                "SELECT NOT EXISTS (SELECT 1 FROM inbox_items WHERE social_account_id = $1 \
                 AND kind = 'dm' AND author_id = $2 AND id < $3) AS ok",
                vec![
                    account.id.into(),
                    item.author_id.clone().into(),
                    item.id.into(),
                ],
            );
            match db.query_one_raw(stmt).await? {
                Some(r) => r.try_get("", "ok")?,
                None => false,
            }
        } else {
            true
        };
        let facts = Facts {
            account_id: account.id,
            account_sets: &sets,
            parent: item.parent_remote_id.as_deref(),
            root: item.root_remote_id.as_deref(),
            text: &item.text,
            first_from_author: first,
            first_message,
        };
        // A click runs the automations that name its button.
        let clicked = item.button_payload.as_ref();
        let named =
            trigger != "button_clicked" || clicked.is_some_and(|p| cond.payloads.contains(p));
        if !named || !matches(&cond, &facts) {
            continue;
        }
        // An action that needs the post's page does not run without one, and leaves no run:
        // it costs no daily hit, and the author's first comment under a post with a page
        // still counts as their first.
        let holds = |t: &str| t.contains("{page}");
        let needed = match &steps {
            Some(steps) => steps.iter().any(|x| x.sends().is_some_and(|x| holds(x.1))),
            None => actions.iter().any(|x| holds(x.via_text().1)),
        };
        let wanted = needed || a.fallback_public_reply.as_deref().is_some_and(holds);
        let page = match item.post_id {
            Some(post) if wanted => pages::address(db, &app.web_url, item.team_id, post).await?,
            _ => None,
        };
        if needed && page.is_none() {
            tracing::info!(automation = %a.id, item = %item.id, "no public page, not run");
            continue;
        }
        let Some(run_id) = claim(db, &a, item, steps.is_some()).await? else {
            continue;
        };
        if let Some(steps) = &steps {
            let page = page.as_deref();
            let flow = Flow {
                app,
                account,
                item,
                a: &a,
                steps,
                page,
            };
            flow.advance(run_id, 0, Vec::new()).await?;
            continue;
        }
        let results = act(app, account, item, &a, &actions, page.as_deref()).await;
        finish(db, run_id, results).await?;
    }
    Ok(())
}

/// A flow's run in progress.
struct Flow<'a> {
    app: &'a AppState,
    account: &'a social_accounts::Model,
    item: &'a inbox_items::Model,
    a: &'a automations::Model,
    steps: &'a [Step],
    /// The address `{page}` stands for, when the item's post has a live page.
    page: Option<&'a str>,
}

impl Flow<'_> {
    /// Runs the flow from step `i`, which the caller claimed for this run, until it ends or
    /// comes to a wait. `results` holds what the sends before `i` gave. The first send that
    /// does not go out ends the run with its status and reason; the steps after it never run.
    async fn advance(
        &self,
        run: Uuid,
        mut i: usize,
        mut results: Vec<ActionResult>,
    ) -> Result<(), DbErr> {
        let db = &self.app.db;
        loop {
            let (via, template, buttons): (Via, &str, &[Button]) = match &self.steps[i] {
                Step::Wait { minutes } => {
                    return park(db, run, self.a, *minutes, &results).await;
                }
                Step::SendDm { text, buttons } => (Via::Dm, text, buttons),
                Step::ReplyPublicly { text } => (Via::Public, text, &[]),
            };
            let kind = self.steps[i].kind();
            let result = match with_page(template, self.page) {
                None => no_page(kind),
                Some(template) => {
                    let (text, filled) = render(&template, self.item.author_name.as_deref());
                    let from = (None, Some(self.a.id));
                    let (account, item) = (self.account, self.item);
                    let sent = inbox::send_with(
                        self.app, account, item, via, &text, &filled, from, buttons,
                    );
                    let sent = sent.await;
                    outcome(
                        kind,
                        sent.map(|r| r.map(|m| m.remote_id.unwrap_or_default())),
                    )
                }
            };
            let stopped = (result.status != "sent").then(|| result.clone());
            results.push(result);
            if let Some(r) = stopped {
                return settle(db, run, &r.status, r.reason, &results).await;
            }
            i += 1;
            let Some(next) = self.steps.get(i) else {
                return settle(db, run, "sent", None, &results).await;
            };
            let sends = next.sends().is_some();
            if !claim_step(db, run, self.a, i, sends, &results).await? {
                return Ok(());
            }
        }
    }
}

/// Moves a `running` run from step `i - 1` to step `i` of `a.steps` and, for a send, takes its
/// hit of the daily cap, in one transaction. `false` when the step must not run: the run is no
/// longer there to go on (cut off, or taken by someone else), the automation is off, gone or
/// has other steps (the run then ends `skipped: automation_changed`), or the cap is reached
/// (`skipped: daily_cap`).
async fn claim_step(
    db: &DatabaseConnection,
    run: Uuid,
    a: &automations::Model,
    i: usize,
    sends: bool,
    results: &[ActionResult],
) -> Result<bool, DbErr> {
    let txn = db.begin().await?;
    let mut values: Vec<Value> = vec![
        run.into(),
        (i as i32).into(),
        serde_json::json!(results).into(),
    ];
    // Still ours to move but not to go on with: it was the automation that changed.
    let end = "UPDATE automation_runs SET status = 'skipped', reason = 'automation_changed', \
               results = $3, resume_at = NULL, updated_at = now() \
               WHERE id = $1 AND status = 'running' AND step_index = $2 - 1";
    let end = sql(end, values.clone());
    values.push(a.steps.clone().into());
    let claim = format!(
        "UPDATE automation_runs r SET step_index = $2, results = $3, resume_at = NULL, \
         updated_at = now() WHERE r.id = $1 AND r.status = 'running' \
         AND r.step_index = $2 - 1 AND {UNCHANGED}"
    );
    if txn.execute_raw(sql(&claim, values)).await?.rows_affected() != 1 {
        txn.execute_raw(end).await?;
        txn.commit().await?;
        return Ok(false);
    }
    let cap = [(quota_bucket(a.team_id), RUNS_PER_DAY)];
    if sends && reserve_in(&txn, &cap).await?.is_none() {
        settle(&txn, run, "skipped", Some("daily_cap".into()), results).await?;
        txn.commit().await?;
        return Ok(false);
    }
    txn.commit().await?;
    Ok(true)
}

/// Parks a run at its `wait` step: `waiting` until `minutes` from now. A run whose automation
/// changed since the step was claimed ends instead (`cancel_waiting` came before it parked).
async fn park(
    db: &DatabaseConnection,
    run: Uuid,
    a: &automations::Model,
    minutes: u32,
    results: &[ActionResult],
) -> Result<(), DbErr> {
    let park = format!(
        "UPDATE automation_runs r SET status = 'waiting', results = $3, updated_at = now(), \
         resume_at = now() + make_interval(mins => $2) WHERE r.id = $1 \
         AND r.status = 'running' AND {UNCHANGED}"
    );
    let values = vec![
        run.into(),
        (minutes as i32).into(),
        serde_json::json!(results).into(),
        a.steps.clone().into(),
    ];
    if db.execute_raw(sql(&park, values)).await?.rows_affected() == 1 {
        return Ok(());
    }
    settle(db, run, "skipped", Some(CHANGED.into()), results).await
}

/// Ends a `running` run.
async fn settle(
    db: &impl ConnectionTrait,
    run: Uuid,
    status: &str,
    reason: Option<String>,
    results: &[ActionResult],
) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE automation_runs SET status = $2, reason = $3, results = $4, resume_at = NULL, \
         updated_at = now() WHERE id = $1 AND status = 'running'",
        vec![
            run.into(),
            status.into(),
            reason.into(),
            serde_json::json!(results).into(),
        ],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Ends the automation's waiting runs: it was switched off or its steps changed (deleting it
/// deletes its runs). They are `skipped: automation_changed` and never go on. A run that is
/// `running` just then ends the same way at its next claim or park (`UNCHANGED`).
pub async fn cancel_waiting(db: &impl ConnectionTrait, automation: Uuid) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE automation_runs SET status = 'skipped', reason = 'automation_changed', \
         resume_at = NULL, updated_at = now() WHERE automation_id = $1 AND status = 'waiting'",
        vec![automation.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// One round of flows: takes up the waiting runs that are due. Skips while another instance
/// is taking its own. The lock and its transaction last for the claim only, never for a send.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![FLOW_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    let due = if locked {
        claim_due(&lock).await?
    } else {
        Vec::new()
    };
    lock.commit().await?;
    work(app, &due, ROUND).await;
    Ok(())
}

/// Moves up to 100 waiting runs whose time has come to `running`, in one statement, so two
/// callers never take the same run. `resume_at` stays until the run's next step is claimed:
/// it marks a run taken up and not started.
async fn claim_due(db: &impl ConnectionTrait) -> Result<Vec<automation_runs::Model>, DbErr> {
    let stmt = sql(
        "UPDATE automation_runs SET status = 'running', updated_at = now() \
         WHERE id IN (SELECT id FROM automation_runs WHERE status = 'waiting' \
         AND resume_at <= now() ORDER BY resume_at LIMIT $1 FOR UPDATE SKIP LOCKED) \
         RETURNING *",
        vec![RESUME_BATCH.into()],
    );
    automation_runs::Entity::find()
        .from_raw_sql(stmt)
        .all(db)
        .await
}

/// Goes on with each claimed run, one after another, for `budget`; the runs it did not get
/// to go back to `waiting` with their `resume_at`, for the next round.
async fn work(app: &AppState, due: &[automation_runs::Model], budget: Duration) {
    let started = Instant::now();
    for run in due {
        let done = if started.elapsed() < budget {
            resume(app, run).await
        } else {
            let stmt = sql(
                "UPDATE automation_runs SET status = 'waiting', updated_at = now() \
                 WHERE id = $1 AND status = 'running' AND resume_at IS NOT NULL",
                vec![run.id.into()],
            );
            app.db.execute_raw(stmt).await.map(|_| ())
        };
        if let Err(e) = done {
            tracing::error!(error = %e, run = %run.id, "automation run resume failed");
        }
    }
}

/// A round without the lock, with `budget` to start the runs it took; how many it took.
pub async fn resume_due(app: &AppState, budget: Duration) -> Result<usize, DbErr> {
    let due = claim_due(&app.db).await?;
    work(app, &due, budget).await;
    Ok(due.len())
}

/// Goes on with a run a round claimed, from the step after its wait.
pub async fn resume(app: &AppState, run: &automation_runs::Model) -> Result<(), DbErr> {
    let db = &app.db;
    let results: Vec<ActionResult> =
        serde_json::from_value(run.results.clone()).unwrap_or_default();
    let a = automations::Entity::find_by_id(run.automation_id)
        .one(db)
        .await?;
    let item = inbox_items::Entity::find_by_id(run.inbox_item_id)
        .one(db)
        .await?;
    let account = match &item {
        Some(i) => {
            social_accounts::Entity::find_by_id(i.social_account_id)
                .one(db)
                .await?
        }
        None => None,
    };
    let steps = a
        .as_ref()
        .and_then(|a| a.steps.clone())
        .and_then(|s| serde_json::from_value::<Vec<Step>>(s).ok())
        .unwrap_or_default();
    let i = run.step_index.map_or(0, |i| i as usize + 1);
    let (Some(a), Some(item), Some(account), Some(next)) = (a, item, account, steps.get(i)) else {
        return settle(db, run.id, "skipped", Some(CHANGED.into()), &results).await;
    };
    let holds = steps
        .iter()
        .any(|s| s.sends().is_some_and(|s| s.1.contains("{page}")));
    let page = match item.post_id {
        Some(post) if holds => pages::address(db, &app.web_url, item.team_id, post).await?,
        _ => None,
    };
    // Also where a run ends whose automation was switched off or given other steps since.
    if !claim_step(db, run.id, &a, i, next.sends().is_some(), &results).await? {
        return Ok(());
    }
    let flow = Flow {
        app,
        account: &account,
        item: &item,
        a: &a,
        steps: &steps,
        page: page.as_deref(),
    };
    flow.advance(run.id, i, results).await
}

/// Inserts the run and reserves its hit of the daily cap in one transaction. `None` when the
/// item already has a run of this automation, or the cap is reached (the run is then stored
/// `skipped: daily_cap`). A `flow`'s run starts at step 0, a send, which the hit pays for.
async fn claim(
    db: &DatabaseConnection,
    a: &automations::Model,
    item: &inbox_items::Model,
    flow: bool,
) -> Result<Option<Uuid>, DbErr> {
    let txn = db.begin().await?;
    let id = Uuid::now_v7();
    let stmt = sql(
        "INSERT INTO automation_runs (id, automation_id, inbox_item_id, team_id, status, \
         step_index, steps_hash) VALUES ($1, $2, $3, $4, 'running', $5, md5($6::jsonb::text)) \
         ON CONFLICT (automation_id, inbox_item_id) DO NOTHING",
        vec![
            id.into(),
            a.id.into(),
            item.id.into(),
            a.team_id.into(),
            flow.then_some(0i32).into(),
            a.steps.clone().into(),
        ],
    );
    if txn.execute_raw(stmt).await?.rows_affected() != 1 {
        return Ok(None);
    }
    let cap = [(quota_bucket(a.team_id), RUNS_PER_DAY)];
    if reserve_in(&txn, &cap).await?.is_none() {
        let stmt = sql(
            "UPDATE automation_runs SET status = 'skipped', reason = 'daily_cap' WHERE id = $1",
            vec![id.into()],
        );
        txn.execute_raw(stmt).await?;
        txn.commit().await?;
        return Ok(None);
    }
    txn.commit().await?;
    Ok(Some(id))
}

/// One action's outcome, as stored in a run's `results`.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct ActionResult {
    /// `send_dm`, `reply_publicly` or `fallback_public_reply`. A flow's run holds one per send
    /// step tried, in order; waits leave none.
    #[serde(rename = "type")]
    pub kind: String,
    /// `sent`, `failed` or `skipped`.
    pub status: String,
    /// Why it was not sent, e.g. `dm_not_allowed`, `no_dm_access`, `rate_limited`;
    /// `no_public_page` for a fallback reply holding `{page}` when the post has no live page; on Facebook
    /// and Instagram `outside_window` (a private reply works within 7 days of the comment),
    /// `already_replied` (once per comment) and `dm_unavailable`.
    pub reason: Option<String>,
    pub remote_id: Option<String>,
}

fn outcome(kind: &str, sent: Result<Result<String, InboxError>, DbErr>) -> ActionResult {
    let (status, reason, remote_id) = match sent {
        Ok(Ok(id)) => ("sent", None, Some(id)),
        Ok(Err(
            e @ (InboxError::DmNotAllowed
            | InboxError::NoDmAccess
            | InboxError::RateLimited
            | InboxError::Invalid("outside_window" | "already_replied" | "dm_unavailable")),
        )) => ("skipped", Some(e.code().to_owned()), None),
        Ok(Err(e)) => ("failed", Some(e.code().to_owned()), None),
        Err(e) => {
            tracing::error!(error = %e, "automation reply record failed");
            ("failed", Some("internal".to_owned()), None)
        }
    };
    ActionResult {
        kind: kind.into(),
        status: status.into(),
        reason,
        remote_id,
    }
}

/// `template` with each `{page}` replaced by the page's address; `None` when it holds one and
/// there is no page.
fn with_page(template: &str, page: Option<&str>) -> Option<String> {
    if !template.contains("{page}") {
        return Some(template.to_owned());
    }
    page.map(|p| template.replace("{page}", p))
}

fn no_page(kind: &str) -> ActionResult {
    ActionResult {
        kind: kind.into(),
        status: "skipped".into(),
        reason: Some("no_public_page".into()),
        remote_id: None,
    }
}

/// Sends each action once, then the fallback reply when a DM could not go and nothing was
/// replied publicly. `page` is the address `{page}` stands for; `run` never gets here without
/// one when an action holds `{page}`, so only a fallback reply can still lack it, and is then
/// skipped (`no_public_page`).
async fn act(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    a: &automations::Model,
    actions: &[Action],
    page: Option<&str>,
) -> Vec<ActionResult> {
    let name = item.author_name.as_deref();
    let fallback = a.fallback_public_reply.as_deref();
    let mut results = Vec::new();
    for action in actions {
        let (via, template) = action.via_text();
        // Filled in before `render`, outside its ranges: the address must become a link.
        let Some(template) = with_page(template, page) else {
            results.push(no_page(action.kind()));
            continue;
        };
        let (text, filled) = render(&template, name);
        let sent = inbox::send(app, account, item, via, &text, &filled, None, Some(a.id)).await;
        results.push(outcome(
            action.kind(),
            sent.map(|r| r.map(|m| m.remote_id.unwrap_or_default())),
        ));
    }
    let dm_refused = results.iter().any(|r| {
        r.kind == "send_dm"
            && matches!(
                r.reason.as_deref(),
                Some(
                    "dm_not_allowed"
                        | "no_dm_access"
                        | "outside_window"
                        | "already_replied"
                        | "dm_unavailable"
                )
            )
    });
    let replied = results
        .iter()
        .any(|r| r.kind == "reply_publicly" && r.status == "sent");
    if let (true, false, Some(f)) = (dm_refused, replied, fallback) {
        let Some(f) = with_page(f, page) else {
            results.push(no_page("fallback_public_reply"));
            return results;
        };
        let (text, filled) = render(&f, name);
        let sent = inbox::send(
            app,
            account,
            item,
            Via::Public,
            &text,
            &filled,
            None,
            Some(a.id),
        )
        .await;
        results.push(outcome(
            "fallback_public_reply",
            sent.map(|r| r.map(|m| m.remote_id.unwrap_or_default())),
        ));
    }
    results
}

/// The run's status from its results: `sent` when anything went, else `failed` when anything
/// failed, else `skipped`; the reason is the first one given.
pub fn summary(results: &[ActionResult]) -> (&'static str, Option<String>) {
    let status = if results.iter().any(|r| r.status == "sent") {
        "sent"
    } else if results.iter().any(|r| r.status == "failed") {
        "failed"
    } else {
        "skipped"
    };
    let reason = results.iter().find_map(|r| r.reason.clone());
    (status, reason)
}

async fn finish(
    db: &DatabaseConnection,
    id: Uuid,
    results: Vec<ActionResult>,
) -> Result<(), DbErr> {
    let (status, reason) = summary(&results);
    settle(db, id, status, reason, &results).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(sets: &'a [Uuid], text: &'a str) -> Facts<'a> {
        Facts {
            account_id: Uuid::nil(),
            account_sets: sets,
            parent: Some("at://me/app.bsky.feed.post/p1"),
            root: Some("at://me/app.bsky.feed.post/p0"),
            text,
            first_from_author: true,
            first_message: true,
        }
    }

    #[test]
    fn conditions_match_every_rule() {
        let set = Uuid::now_v7();
        let sets = [set];
        let any = Conditions::default();
        assert!(matches(&any, &facts(&[], "hello")));
        let kw = Conditions {
            keywords: vec!["GUIDE".into(), "link".into()],
            ..Conditions::default()
        };
        assert!(matches(&kw, &facts(&[], "send me the guide pls")));
        assert!(!matches(&kw, &facts(&[], "nice post")));
        let in_set = Conditions {
            social_set_id: Some(set),
            ..Conditions::default()
        };
        assert!(matches(&in_set, &facts(&sets, "x")));
        assert!(!matches(&in_set, &facts(&[], "x")));
        let accounts = Conditions {
            account_ids: vec![Uuid::now_v7()],
            ..Conditions::default()
        };
        assert!(!matches(&accounts, &facts(&[], "x")));
        for (post, want) in [
            ("at://me/app.bsky.feed.post/p0", true),
            ("at://me/app.bsky.feed.post/p1", true),
            ("at://me/app.bsky.feed.post/p9", false),
        ] {
            let c = Conditions {
                post_remote_ids: vec![post.into()],
                ..Conditions::default()
            };
            assert_eq!(matches(&c, &facts(&[], "x")), want, "{post}");
        }
        let first = Conditions {
            first_per_author: true,
            ..Conditions::default()
        };
        let mut again = facts(&[], "x");
        again.first_from_author = false;
        assert!(!matches(&first, &again));
        assert!(matches(&any, &again));
        let welcome = Conditions {
            first_message: true,
            ..Conditions::default()
        };
        assert!(matches(&welcome, &facts(&[], "hi")));
        let mut later = facts(&[], "hi");
        later.first_message = false;
        assert!(!matches(&welcome, &later));
        assert!(matches(&any, &later));
    }

    #[test]
    fn each_trigger_takes_its_own_conditions_and_actions() {
        let dm = || Action::SendDm {
            text: "Welcome {name}".into(),
        };
        let reply = || Action::ReplyPublicly {
            text: "Thanks".into(),
        };
        let ok = |t: &str, c: Conditions, a: &[Action], f: Option<&str>| {
            validate(t, &mut c.clone(), a, f)
        };
        let welcome = Conditions {
            first_message: true,
            keywords: vec!["hi".into()],
            ..Conditions::default()
        };
        assert_eq!(ok("dm_received", welcome.clone(), &[dm()], None), Ok(()));
        for t in [
            "comment_received",
            "mention_received",
            "story_reply",
            "new_follower",
        ] {
            assert_eq!(
                ok(t, welcome.clone(), &[dm()], None),
                Err("invalid_conditions"),
                "{t}"
            );
        }
        let kw = Conditions {
            keywords: vec!["hi".into()],
            ..Conditions::default()
        };
        assert_eq!(ok("story_reply", kw.clone(), &[dm()], None), Ok(()));
        assert_eq!(
            ok("mention_received", kw.clone(), &[reply(), dm()], Some("x")),
            Ok(())
        );
        assert_eq!(
            ok("new_follower", kw, &[dm()], None),
            Err("invalid_conditions")
        );
        assert_eq!(
            ok("new_follower", Conditions::default(), &[dm()], None),
            Ok(())
        );
        let post = Conditions {
            post_remote_ids: vec!["p".into()],
            ..Conditions::default()
        };
        let author = Conditions {
            first_per_author: true,
            ..Conditions::default()
        };
        for c in [post, author] {
            assert_eq!(ok("comment_received", c.clone(), &[dm()], None), Ok(()));
            assert_eq!(
                ok("mention_received", c, &[dm()], None),
                Err("invalid_conditions")
            );
        }
        for t in ["dm_received", "story_reply", "new_follower"] {
            let none = Conditions::default();
            assert_eq!(
                ok(t, none.clone(), &[reply()], None),
                Err("invalid_actions"),
                "{t}"
            );
            assert_eq!(
                ok(t, none.clone(), &[dm()], Some("x")),
                Err("invalid_fallback"),
                "{t}"
            );
            let paged = Action::SendDm {
                text: "see {page}".into(),
            };
            assert_eq!(ok(t, none, &[paged], None), Err("invalid_text"), "{t}");
        }
    }

    #[test]
    fn names_fill_placeholders() {
        let (text, filled) = render("Hi {name}, {name}!", Some(" Ada "));
        assert_eq!(text, "Hi Ada, Ada!");
        assert_eq!(filled, [3..6, 8..11]);
        assert_eq!(render("Hi {name}", Some("  ")).0, "Hi there");
        assert_eq!(render("Hi {name}", None).0, "Hi there");
        assert_eq!(
            render("Hi {name}", Some("José O'Neil-Ng")).0,
            "Hi José O'Neil-Ng"
        );
        assert_eq!(render("Hi {name}", Some(&"n".repeat(99))).0.len(), 3 + 64);
        assert_eq!(
            render("no placeholder", Some("Ada")),
            ("no placeholder".into(), vec![])
        );
    }

    #[test]
    fn punctuated_names_give_their_first_word() {
        let hi = |n: &str| render("Hi {name}", Some(n)).0;
        assert_eq!(
            hi("Harun R. Rayhan - \u{09b9}\u{09be}\u{09b0}\u{09c1}\u{09a8}"),
            "Hi Harun"
        );
        assert_eq!(hi("Ada (she/her)"), "Hi there");
        assert_eq!(hi("Dr. Ada"), "Hi there");
        assert_eq!(hi("Ada, PhD."), "Hi Ada");
    }

    #[test]
    fn names_that_could_link_or_mention_fall_back() {
        for bad in [
            "claim prize at evil.com",
            "https://evil.example/x",
            "@someone",
            "a/b",
            "evil。com",
            "line\nbreak",
            "tab\there",
            "Ada evil.com",
            "Ada @someone",
            "Ada #promo",
        ] {
            let (text, filled) = render("Hi {name}, see https://x.io", Some(bad));
            assert_eq!(text, "Hi there, see https://x.io", "{bad}");
            // No link is found inside the filled-in name.
            let links = socialreins_core::post::links(&text);
            assert!(links
                .iter()
                .all(|l| filled.iter().all(|r| l.end <= r.start || l.start >= r.end)));
        }
    }

    #[test]
    fn shapes_are_strict() {
        let dm = || Action::SendDm {
            text: "Here: https://x.io".into(),
        };
        let reply = || Action::ReplyPublicly {
            text: "Sent you a DM".into(),
        };
        let mut c = Conditions::default();
        assert_eq!(
            validate("comment_received", &mut c, &[dm(), reply()], Some("Hi")),
            Ok(())
        );
        assert_eq!(
            validate("follow", &mut c, &[dm()], None),
            Err("invalid_trigger")
        );
        assert_eq!(
            validate("comment_received", &mut c, &[], None),
            Err("invalid_actions")
        );
        assert_eq!(
            validate("comment_received", &mut c, &[dm(), dm()], None),
            Err("invalid_actions")
        );
        let long = Action::ReplyPublicly {
            text: "a".repeat(301),
        };
        assert_eq!(
            validate("comment_received", &mut c, &[long], None),
            Err("invalid_text")
        );
        assert_eq!(
            validate("comment_received", &mut c, &[reply()], Some("x")),
            Err("invalid_fallback")
        );
        let mut both = Conditions {
            social_set_id: Some(Uuid::nil()),
            account_ids: vec![Uuid::nil()],
            ..Conditions::default()
        };
        assert_eq!(
            validate("comment_received", &mut both, &[dm()], None),
            Err("invalid_conditions")
        );
        // `{page}` counts as 100 characters, in actions and in the fallback.
        let paged_text = |t: &str| Action::ReplyPublicly { text: t.into() };
        let paged = |n: usize| Action::ReplyPublicly {
            text: format!("{} {{page}}", "a".repeat(n - 1)),
        };
        assert_eq!(
            validate("comment_received", &mut c, &[paged(200)], None),
            Ok(())
        );
        assert_eq!(
            validate("comment_received", &mut c, &[paged(201)], None),
            Err("invalid_text")
        );
        let f = format!("{} {{page}}", "a".repeat(200));
        // `{page}` must be able to become a clean link.
        for (text, ok) in [
            ("{page}", true),
            ("See ({page}), {name}. {page}!", true),
            ("details:{page}", false),
            ("{page}]", false),
            ("{page}/{name}", false),
            ("{page}{page}", false),
            ("{page}.html", false),
            ("{page},{name}", false),
            ("{page}). Bye", true),
        ] {
            let got = validate("comment_received", &mut c, &[paged_text(text)], None);
            assert_eq!(got.is_ok(), ok, "{text}");
        }
        assert_eq!(
            validate("comment_received", &mut c, &[dm()], Some(&f)),
            Err("invalid_fallback")
        );
        assert_eq!(
            validate("comment_received", &mut c, &[dm()], Some(&f[1..])),
            Ok(())
        );
        let unknown = serde_json::json!([{"type": "send_dm", "text": "x", "extra": 1}]);
        assert!(serde_json::from_value::<Vec<Action>>(unknown).is_err());
        let wrong = serde_json::json!([{"type": "like"}]);
        assert!(serde_json::from_value::<Vec<Action>>(wrong).is_err());
        let typo = serde_json::json!({"keyword": ["x"]});
        assert!(serde_json::from_value::<Conditions>(typo).is_err());
    }

    #[test]
    fn runs_summarize_their_actions() {
        let r = |kind: &str, status: &str, reason: Option<&str>| ActionResult {
            kind: kind.into(),
            status: status.into(),
            reason: reason.map(Into::into),
            remote_id: None,
        };
        let fallback = [
            r("send_dm", "skipped", Some("dm_not_allowed")),
            r("fallback_public_reply", "sent", None),
        ];
        assert_eq!(summary(&fallback), ("sent", Some("dm_not_allowed".into())));
        let refused = [r("send_dm", "skipped", Some("no_dm_access"))];
        assert_eq!(summary(&refused), ("skipped", Some("no_dm_access".into())));
        let broke = [
            r("send_dm", "skipped", Some("rate_limited")),
            r("reply_publicly", "failed", Some("bluesky_refused")),
        ];
        assert_eq!(summary(&broke).0, "failed");
    }
}
