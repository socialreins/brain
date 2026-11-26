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
//!
//! `ai_send_dm` and `ai_reply_publicly`, actions and steps, send a text an AI model writes from
//! the owner's prompt and the item's text (`ai_send`, `crate::ai`). They are sends like the others:
//! the same claim, daily cap and run results, which name the provider and model.
//!
//! A send a platform refused for rate limiting sent nothing, so it is safe to try again: a flow
//! parks and tries the step again in 5, 10 and 15 minutes (`retry_later`, at most `MAX_RETRIES`),
//! and a person may retry a run that ended that way (`retry`) when nothing else of it went out.
//! The same for an AI step whose provider did not answer (a timeout, a 429 or a 5xx: nothing was sent and
//! the credit is given back; the new try pays under a new reference, `ai_reference`).
//! Every other failure is final, because it may have been sent.
//!
//! `tag_contact`, an action and a step, sends nothing: it adds a tag to the contact of the
//! item's author (`inbox::add_tag`) and takes no hit of the daily cap. An automation still needs
//! a send beside it.
//!
//! A Bluesky like or repost of one of our posts is a `like` or `repost` item (stored `done`,
//! with no contact) and runs `like_received` and `repost_received`. Nobody is behind those to
//! message or tag, so they take only the actions that do something on the account or in the
//! team. `like_comment` (also on Bluesky), `repost_comment` and `follow_back` go through
//! `automation_effects::bluesky`, the inbox buttons' own code. Why an automation did not run on
//! an item is kept by `automation_misses`.
use sea_orm::{
    sea_query::Query, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait, Value,
};
use std::{
    ops::Range,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    ai, automation_effects, automation_misses,
    billing::{self, AiSpent},
    bluesky_actions,
    connectors::{bluesky::InboxError, meta::Moderate},
    controllers::ai_key::key_of,
    controllers::{connections::live_sets, pages},
    inbox::{self, Button, Via},
    models::{
        automation_runs, automations, inbox_items, social_account_social_set as membership,
        social_accounts,
    },
    rate_limit::reserve_in,
    state::AppState,
};

pub const TRIGGERS: [&str; 9] = [
    "button_clicked",
    "comment_received",
    "dm_received",
    "like_received",
    "mention_received",
    "new_follower",
    "repost_received",
    "story_mention",
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
/// Most `tag_contact` actions beside an automation's sends.
const MAX_TAG_ACTIONS: usize = 3;
/// How often a run is tried again by itself after a rate limit, and in all with a person's tries.
const MAX_RETRIES: i32 = 3;
const MAX_MANUAL_RETRIES: i32 = 10;
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
/// Runs stored `skipped` for a safety reason (`paused`, `own_account`, `connected_account`,
/// `cooldown`, `automation_cap`, the team's `daily_cap`, or a question that was not judged to be
/// one or could not be judged) did nothing: they are not an author's first run and never count
/// toward a cool-down or an automation's cap. So is any run skipped before it did or tried
/// anything (no results). SQL on a run aliased `r`.
const NOT_ACTED: &str = "(r.status = 'skipped' AND (r.results = '[]'::jsonb OR \
    COALESCE(r.reason, '') IN ('paused', 'own_account', 'connected_account', 'cooldown', \
    'automation_cap', 'daily_cap', 'no_question', 'question_cap', 'no_ai_key', \
    'no_ai_credits', 'ai_failed')))";
/// Longest `{name}` put into a text.
pub(crate) const MAX_NAME_CHARS: usize = 64;
/// What `{page}` counts as when a text is checked.
// shortcut: an address over 100 characters (a `WEB_URL` over 85) can make a checked text too
// long to send, which fails that action; derive this from `WEB_URL` if one ever is.
const MAX_PAGE_CHARS: usize = 100;

pub fn quota_bucket(team_id: Uuid) -> String {
    format!("automation:team:{team_id}")
}

/// When an automation acts on an item. Empty lists mean "any". At most one of
/// `social_set_id` and `account_ids`. Every trigger takes the scope (`social_set_id` or
/// `account_ids`); `keywords` all but `new_follower`, `button_clicked`, `like_received` and
/// `repost_received`; `post_remote_ids` only `comment_received`, `like_received` and
/// `repost_received`; `first_per_author` only `comment_received` and `mention_received`; `first_message` only `dm_received`; `payloads`
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
    /// Weekdays and hours it may act in, in a time zone. Absent: always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    /// Only a message that asks a question. One with a `?` counts as one; any other is judged by
    /// an AI model, paid like an AI reply. Not `new_follower` or `button_clicked`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub question: bool,
    /// Only a message written in one of these languages (ISO 639-1 codes of `LANGUAGES`, at most
    /// 10), detected from its text; a text too short or mixed to tell never matches. Not
    /// `new_follower` or `button_clicked`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// `true`: only text with a link, domain or email in it (spam); `false`: only text without
    /// one. Absent: either. Not for `new_follower` or `button_clicked`, which carry no text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_link: Option<bool>,
    /// Only text an AI model reads as one of these moods: `positive` or `negative` (text that is
    /// neither, or that it cannot tell, never matches). One reading counts as an AI reply (the
    /// team's key, else the plan's allowance, then credits), made last, once everything else
    /// matched; at most 500 a team a day. Not for `new_follower` or `button_clicked`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sentiment: Vec<String>,
}

/// When an automation may act: on these weekdays, in these hours of the day, in `zone`. An
/// overnight window has `from_hour` after `to_hour`.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct When {
    /// An IANA time zone name such as `Asia/Dhaka`.
    pub zone: String,
    /// 0 (Monday) to 6 (Sunday); empty: every day.
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// The first hour it may act, 0 to 23; with `to_hour`, or both absent: all day.
    #[serde(default)]
    pub from_hour: Option<u8>,
    /// The hour it stops, 1 to 24, not the same as `from_hour`: 9 to 17 acts from 9:00 to 16:59.
    #[serde(default)]
    pub to_hour: Option<u8>,
}

impl When {
    fn valid(&self) -> bool {
        let hours = match (self.from_hour, self.to_hour) {
            (None, None) => true,
            (Some(f), Some(t)) => f <= 23 && (1..=24).contains(&t) && f != t,
            _ => false,
        };
        hours
            && self.zone.parse::<chrono_tz::Tz>().is_ok()
            && self.weekdays.len() <= 7
            && self.weekdays.iter().all(|d| *d <= 6)
            && (self.from_hour.is_some() || !self.weekdays.is_empty())
    }

    /// `wrong_weekday` or `outside_hours` when `now` is not a time it may act.
    fn mismatch(&self, now: chrono::DateTime<chrono::Utc>) -> Option<&'static str> {
        use chrono::{Datelike, Timelike};
        let local = now.with_timezone(&self.zone.parse::<chrono_tz::Tz>().ok()?);
        let hour = local.hour() as u8;
        let (overnight, to) = match (self.from_hour, self.to_hour) {
            (Some(f), Some(t)) => (f >= t, t),
            _ => (false, 0),
        };
        // An overnight window belongs to the day it starts: Friday 22 to 6 runs into Saturday
        // morning, and those small hours are Friday's.
        let day = if overnight && hour < to {
            local.weekday().pred()
        } else {
            local.weekday()
        };
        if !self.weekdays.is_empty() && !self.weekdays.contains(&(day.num_days_from_monday() as u8))
        {
            return Some("wrong_weekday");
        }
        let (Some(from), Some(to)) = (self.from_hour, self.to_hour) else {
            return None;
        };
        let inside = if from < to {
            (from..to).contains(&hour)
        } else {
            hour >= from || hour < to
        };
        (!inside).then_some("outside_hours")
    }
}

/// Languages a `languages` condition may name: the ISO 639-1 code and `whatlang`'s name for it.
pub const LANGUAGES: [(&str, whatlang::Lang); 23] = {
    use whatlang::Lang::*;
    [
        ("ar", Ara),
        ("bn", Ben),
        ("de", Deu),
        ("en", Eng),
        ("es", Spa),
        ("fa", Pes),
        ("fr", Fra),
        ("he", Heb),
        ("hi", Hin),
        ("id", Ind),
        ("it", Ita),
        ("ja", Jpn),
        ("ko", Kor),
        ("nl", Nld),
        ("pl", Pol),
        ("pt", Por),
        ("ru", Rus),
        ("th", Tha),
        ("tr", Tur),
        ("uk", Ukr),
        ("ur", Urd),
        ("vi", Vie),
        ("zh", Cmn),
    ]
};

/// The least confidence (0 to 1) whatlang must have in a language for it to count. Its own
/// `is_reliable` wants 0.8, which a plain English sentence of a dozen words does not reach.
// shortcut: a very short text can be called the wrong language above this; raise it, or add the
// platform's own language tag (X sends one with each post) when that shows up in real use.
const MIN_LANGUAGE_CONFIDENCE: f64 = 0.3;
/// The fewest letters a text needs for its language to be judged: "ok" or "lol" can be any.
const MIN_LANGUAGE_LETTERS: usize = 12;

/// The language of `text` as one of `LANGUAGES`' codes; `None` when it is another one or the
/// detector is not sure (a very short or mixed text).
pub fn language_of(text: &str) -> Option<&'static str> {
    // Handles, links and hashtags say nothing of the language, and a few letters are too few.
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|w| !w.starts_with(['@', '#']) && !w.contains("://"))
        .collect();
    let text = words.join(" ");
    if text.chars().filter(|c| c.is_alphabetic()).count() < MIN_LANGUAGE_LETTERS {
        return None;
    }
    let info = whatlang::detect(&text).filter(|i| i.confidence() >= MIN_LANGUAGE_CONFIDENCE)?;
    LANGUAGES
        .iter()
        .find(|(_, l)| *l == info.lang())
        .map(|(code, _)| *code)
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
    /// A DM as `SendDm`, its text written by AI from `prompt` (1 to 2000 characters, no `{name}` or
    /// `{page}`), the item's text and the person's name. It uses the team's own AI key, else
    /// ours, paid on Cloud from the plan's monthly allowance, then 1 `ai` credit. A reply that is
    /// empty, too long, repeats the prompt or holds a link or @handle the prompt does not is not
    /// sent.
    AiSendDm { prompt: String },
    /// A reply under the comment or mention as `ReplyPublicly`, written by AI as `AiSendDm`.
    AiReplyPublicly { prompt: String },
    /// Adds `tag` (1 to 30 characters, no commas; trimmed and lowercased) to the contact of
    /// the item's author. Every trigger takes it. It sends nothing and costs no daily hit, and
    /// does nothing when the contact has the tag or holds 20 already.
    TagContact {
        #[serde(deserialize_with = "tidy_tag")]
        tag: String,
    },
    /// Mails the team's owners and admins about the item (who, what they wrote, a link to the
    /// inbox); at most 20 mails a day per team, then `skipped: notify_cap`. Every trigger.
    NotifyTeam,
    /// Assigns the item to a member of the team, unless it has an assignee already.
    Assign { user_id: Uuid },
    /// Hides the comment on Facebook, Instagram or Threads (a moderation action: nobody but its
    /// author and the account sees it any more; on Threads only a reply directly under a post of
    /// the account, else `skipped: threads_nested`). `comment_received` only; elsewhere or on
    /// another network it is `skipped: not_supported`.
    HideComment,
    /// Likes the comment as the Page on Facebook, or from the account on Bluesky, where it is
    /// a comment or a mention; `skipped: not_supported` elsewhere.
    LikeComment,
    /// Reposts the comment or mention from the account (Bluesky only; `skipped: not_supported`
    /// elsewhere).
    RepostComment,
    /// Follows the item's author from the account (Bluesky only, every trigger; nothing is done
    /// to someone it follows already or to an account of the team).
    FollowBack,
    /// Once this automation has started on an item, the team's later automations (newer ones)
    /// do not run on it.
    StopAutomations,
}

/// A tag as contacts keep one, trimmed and lowercased; `validate` refuses what is still not one.
fn tidy_tag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    String::deserialize(d).map(|t| t.trim().to_lowercase())
}

fn tag_ok(tag: &str) -> bool {
    inbox::tag(tag).as_deref() == Some(tag)
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SendDm { .. } => "send_dm",
            Self::ReplyPublicly { .. } => "reply_publicly",
            Self::AiSendDm { .. } => "ai_send_dm",
            Self::AiReplyPublicly { .. } => "ai_reply_publicly",
            Self::TagContact { .. } => "tag_contact",
            Self::NotifyTeam => "notify_team",
            Self::Assign { .. } => "assign",
            Self::HideComment => "hide_comment",
            Self::LikeComment => "like_comment",
            Self::RepostComment => "repost_comment",
            Self::FollowBack => "follow_back",
            Self::StopAutomations => "stop_automations",
        }
    }

    /// How a send goes out and its text (an AI send's prompt); `None` for anything that is not
    /// a send.
    pub(crate) fn via_text(&self) -> Option<(Via, &str)> {
        match self {
            Self::SendDm { text } => Some((Via::Dm, text)),
            Self::ReplyPublicly { text } => Some((Via::Public, text)),
            Self::AiSendDm { prompt } => Some((Via::Dm, prompt)),
            Self::AiReplyPublicly { prompt } => Some((Via::Public, prompt)),
            _ => None,
        }
    }

    pub(crate) fn ai(&self) -> bool {
        matches!(self, Self::AiSendDm { .. } | Self::AiReplyPublicly { .. })
    }

    /// An action that reaches someone or something outside SocialReins (a send, a mail, a
    /// moderation call): it takes a hit of the team's daily cap.
    pub(crate) fn outward(&self) -> bool {
        !matches!(
            self,
            Self::TagContact { .. } | Self::Assign { .. } | Self::StopAutomations
        )
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
    /// A DM written by AI, as `Action::AiSendDm`. No buttons.
    AiSendDm { prompt: String },
    /// A public reply written by AI, as `Action::AiReplyPublicly`.
    AiReplyPublicly { prompt: String },
    /// The run stops here for 1 to 10080 minutes (7 days). Never the first or last step.
    Wait { minutes: u32 },
    /// The run stops here until the person writes again, or `minutes` (1 to 10080) pass, when
    /// it ends `skipped: no_reply`. Never the first or last step. A DM step after it goes out
    /// in the conversation their answer came in, which on Facebook and Instagram opens a new
    /// 24 hour window (so a comment's one private reply may be followed by more messages). An
    /// answer a waiting run takes does not start other automations.
    WaitForReply { minutes: u32 },
    /// Adds `tag` to the contact of the item's author, as `Action::TagContact` does, and goes
    /// on whatever came of it. Not a send: a flow needs a send step beside it.
    TagContact {
        #[serde(deserialize_with = "tidy_tag")]
        tag: String,
    },
    /// As `Action::NotifyTeam`.
    NotifyTeam,
    /// As `Action::Assign`.
    Assign { user_id: Uuid },
    /// As `Action::HideComment`.
    HideComment,
    /// As `Action::LikeComment`.
    LikeComment,
    /// As `Action::RepostComment`.
    RepostComment,
    /// As `Action::FollowBack`.
    FollowBack,
    /// As `Action::StopAutomations`.
    StopAutomations,
}

impl Step {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SendDm { .. } => "send_dm",
            Self::ReplyPublicly { .. } => "reply_publicly",
            Self::AiSendDm { .. } => "ai_send_dm",
            Self::AiReplyPublicly { .. } => "ai_reply_publicly",
            Self::Wait { .. } => "wait",
            Self::WaitForReply { .. } => "wait_for_reply",
            Self::TagContact { .. } => "tag_contact",
            Self::NotifyTeam => "notify_team",
            Self::Assign { .. } => "assign",
            Self::HideComment => "hide_comment",
            Self::LikeComment => "like_comment",
            Self::RepostComment => "repost_comment",
            Self::FollowBack => "follow_back",
            Self::StopAutomations => "stop_automations",
        }
    }

    /// How a send step goes out, its text (an AI step's prompt) and its buttons; `None` for a
    /// wait or a tag.
    pub(crate) fn sends(&self) -> Option<(Via, &str, &[Button])> {
        match self {
            Self::SendDm { text, buttons } => Some((Via::Dm, text, buttons)),
            Self::ReplyPublicly { text } => Some((Via::Public, text, &[])),
            Self::AiSendDm { prompt } => Some((Via::Dm, prompt, &[])),
            Self::AiReplyPublicly { prompt } => Some((Via::Public, prompt, &[])),
            _ => None,
        }
    }

    /// The step as an action, when it is one that takes no time.
    pub(crate) fn effect(&self) -> Option<Action> {
        Some(match self {
            Self::TagContact { tag } => Action::TagContact { tag: tag.clone() },
            Self::NotifyTeam => Action::NotifyTeam,
            Self::Assign { user_id } => Action::Assign { user_id: *user_id },
            Self::HideComment => Action::HideComment,
            Self::LikeComment => Action::LikeComment,
            Self::RepostComment => Action::RepostComment,
            Self::FollowBack => Action::FollowBack,
            Self::StopAutomations => Action::StopAutomations,
            _ => return None,
        })
    }

    /// As `Action::outward`; a wait is not.
    pub(crate) fn outward(&self) -> bool {
        self.sends().is_some() || self.effect().is_some_and(|a| a.outward())
    }

    pub(crate) fn ai(&self) -> bool {
        matches!(self, Self::AiSendDm { .. } | Self::AiReplyPublicly { .. })
    }
}

/// An AI send's prompt: 1 to 2000 characters, and no `{name}` or `{page}`, which it never fills
/// in (the model is given the person's name as data instead).
fn prompt_ok(prompt: &str) -> bool {
    !prompt.trim().is_empty()
        && prompt.chars().count() <= ai::MAX_PROMPT_CHARS
        && !prompt.contains("{page}")
        && !prompt.contains("{name}")
}

/// The trigger a new item runs: by its kind, and a button click and a story reply apart from
/// other DMs.
pub fn trigger_of(item: &inbox_items::Model) -> Option<&'static str> {
    match item.kind.as_str() {
        "comment" => Some("comment_received"),
        "mention" => Some("mention_received"),
        "dm" if item.button_payload.is_some() => Some("button_clicked"),
        "dm" if item.story_reply => Some("story_reply"),
        // Instagram: someone @mentioned the account in their story.
        "dm" if story_mention(item) => Some("story_mention"),
        "dm" => Some("dm_received"),
        "follow" => Some("new_follower"),
        "like" => Some("like_received"),
        "repost" => Some("repost_received"),
        _ => None,
    }
}

/// Whether the DM is Instagram's notice that someone mentioned the account in a story (an
/// attachment of kind `story_mention`).
pub fn story_mention(item: &inbox_items::Model) -> bool {
    item.platform == "instagram"
        && item.attachments.as_array().is_some_and(|a| {
            a.iter()
                .any(|x| x["kind"].as_str() == Some("story_mention"))
        })
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
    let mention = trigger == "mention_received";
    let like = activity(trigger);
    let fits_trigger = (comment || like || c.post_remote_ids.is_empty())
        && (comment || mention || !c.first_per_author)
        && (trigger == "dm_received" || !c.first_message)
        && (!(button || like || matches!(trigger, "new_follower" | "story_mention"))
            || c.keywords.is_empty())
        && (button != c.payloads.is_empty());
    // A follow, a like, a repost, a story mention and a click carry no text to read.
    let textless = button || like || matches!(trigger, "new_follower" | "story_mention");
    let text_ok = !(textless && (c.has_link.is_some() || !c.sentiment.is_empty()))
        && c.sentiment.len() <= SENTIMENTS.len()
        && c.sentiment.iter().all(|i| SENTIMENTS.contains(&i.as_str()));
    let ids_ok = c.post_remote_ids.len() <= 100
        && c.post_remote_ids
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 300 && !p.chars().any(char::is_whitespace));
    if !fits_trigger
        || !text_ok
        || !ids_ok
        || c.account_ids.len() > 100
        || (c.social_set_id.is_some() && !c.account_ids.is_empty())
        || c.payloads.len() > 20
        || !c.payloads.iter().all(|p| payload_ok(p))
    {
        return Err("invalid_conditions");
    }
    let text_trigger = !textless;
    let mut codes: Vec<&str> = c.languages.iter().map(String::as_str).collect();
    codes.sort_unstable();
    codes.dedup();
    if c.when.as_ref().is_some_and(|w| !w.valid())
        || ((c.question || !c.languages.is_empty()) && !text_trigger)
        || c.languages.len() > 10
        || codes.len() != c.languages.len()
        || !codes
            .iter()
            .all(|l| LANGUAGES.iter().any(|(code, _)| code == l))
    {
        return Err("invalid_conditions");
    }
    if let Some(w) = &mut c.when {
        w.weekdays.sort_unstable();
        w.weekdays.dedup();
    }
    c.languages.sort();
    c.keywords = inbox::keywords(&c.keywords).ok_or("invalid_conditions")?;
    c.account_ids.sort();
    c.account_ids.dedup();
    c.payloads.sort();
    c.payloads.dedup();
    c.sentiment.sort();
    c.sentiment.dedup();
    Ok(())
}

/// What `sentiment` may name; a text read as anything else never matches.
pub const SENTIMENTS: [&str; 2] = ["negative", "positive"];

/// A like or repost of one of our posts: no text, no contact and no one to message.
fn activity(trigger: &str) -> bool {
    matches!(trigger, "like_received" | "repost_received")
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
/// steps, no wait first or last, at least one send, each step one the trigger takes.
pub fn validate_steps(
    trigger: &str,
    conditions: &mut Conditions,
    steps: &mut [Step],
) -> Result<(), &'static str> {
    check_conditions(trigger, conditions)?;
    let wait = |s: Option<&Step>| matches!(s, Some(Step::Wait { .. } | Step::WaitForReply { .. }));
    // At least one step that does something outside (a send, a mail, a moderation call).
    let sends = steps.iter().any(Step::outward);
    // As in `validate`, each of these once.
    let once = |f: fn(&Step) -> bool| steps.iter().filter(|s| f(s)).count() <= 1;
    if !once(|s| matches!(s, Step::NotifyTeam))
        || !once(|s| matches!(s, Step::Assign { .. }))
        || !once(|s| matches!(s, Step::HideComment))
        || !once(|s| matches!(s, Step::LikeComment))
        || !once(|s| matches!(s, Step::RepostComment))
        || !once(|s| matches!(s, Step::FollowBack))
        || !once(|s| matches!(s, Step::StopAutomations))
    {
        return Err("invalid_steps");
    }
    // Nobody to message or tag behind a like or repost: it would repeat for every one.
    if activity(trigger)
        && steps
            .iter()
            .any(|s| s.sends().is_some() || matches!(s, Step::TagContact { .. }))
    {
        return Err("invalid_steps");
    }
    if !sends || steps.len() > MAX_STEPS || wait(steps.first()) || wait(steps.last()) {
        return Err("invalid_steps");
    }
    for step in steps {
        match step {
            // Nobody sent anything to wait for an answer to behind a like or repost.
            Step::WaitForReply { .. } if activity(trigger) => return Err("invalid_steps"),
            Step::Wait { minutes } | Step::WaitForReply { minutes }
                if !(1..=MAX_WAIT_MINUTES).contains(minutes) =>
            {
                return Err("invalid_steps");
            }
            Step::ReplyPublicly { .. } | Step::AiReplyPublicly { .. } if !public(trigger) => {
                return Err("invalid_steps");
            }
            Step::TagContact { tag } if !tag_ok(tag) => return Err("invalid_steps"),
            Step::HideComment if trigger != "comment_received" => {
                return Err("invalid_steps");
            }
            Step::LikeComment | Step::RepostComment if !public(trigger) => {
                return Err("invalid_steps");
            }
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
        if let (true, Some((_, prompt, _))) = (step.ai(), step.sends()) {
            if !prompt_ok(prompt) {
                return Err("invalid_text");
            }
        } else if let Some((via, text, buttons)) = step.sends() {
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

/// Networks whose inbox can fire an automation.
pub const AUTOMATED: [&str; 7] = [
    "bluesky",
    "facebook",
    "instagram",
    "linkedin",
    "threads",
    "x",
    "youtube",
];
/// Meta allows a message to a person only within 24 hours of their last one.
const META_WINDOW_MINUTES: u32 = 24 * 60;
/// And a private reply to a comment only within 7 days of it.
const META_PRIVATE_REPLY_MINUTES: u32 = 7 * 24 * 60;

/// What the networks of the accounts an automation is limited to allow, checked when it is
/// limited to accounts by name (`providers`, one per account). An account on a network with no
/// inbox can never fire it (`unsupported_account`). Only when every account is Facebook or
/// Instagram, the rules there are checked too, as a flow that breaks them could never finish:
/// a comment gets one private reply, so one DM (`meta_one_private_reply`); a DM trigger's DM
/// must go within 24 hours of the person's message (`meta_reply_window`); an Instagram DM is at
/// most 1000 bytes (`invalid_text`). When every account is X, only the triggers X has can fire
/// (`x_unsupported_trigger`): a comment (a reply to our post), a mention and a DM, never a follow,
/// a story reply or a button tap. X has no private reply and no messaging window: a DM goes
/// when the person follows the account or has DMs open, else it ends `skipped: dm_not_allowed`
/// and the fallback public reply answers. When every account is Bluesky, a story reply and a
/// button tap never come (`bluesky_unsupported_trigger`; its buttons are numbered lines the
/// person answers with a DM keyword). Bluesky has no private reply either: a DM goes only to
/// someone who follows the account or allows DMs from anyone, and a welcome DM to a new
/// follower normally does, as they follow us (unless they closed their DMs). When every account
/// is Threads, only a reply and a mention can fire it (`threads_unsupported_trigger`), no DM may
/// be sent (`threads_no_dm`: Threads has no messages and no private reply), a comment is not liked
/// (`threads_no_like`; hiding a reply directly under our post works), and a public reply
/// holds 500 characters with an emoji counted as its UTF-8 bytes (`invalid_text`). A mixed scope
/// is left to fail item by item: a DM to a Threads item is skipped (`threads_no_dm`) and the
/// fallback public reply answers. When every account is YouTube, only a comment can fire it (`youtube_unsupported_trigger`: no mentions, DMs,
/// follows, story replies or buttons) and only public replies can answer (`youtube_no_dm`: YouTube
/// has no DM and no private reply). When every account is LinkedIn, the same: only a comment can
/// fire it (`linkedin_unsupported_trigger`), no DM may be sent (`linkedin_no_dm`: LinkedIn gives
/// apps no messaging) and a comment cannot be hidden (`linkedin_no_hide`: only deleted, which an
/// automation never does); a reply holds 1250 characters (an AI reply; an automation's own text
/// stays within the shared 300). Whether the account can read comments at all is its
/// capability (`linkedin_inbox::capability`), checked where accounts are known. A mixed scope
/// is left to fail item by item.
pub fn network_limits(
    trigger: &str,
    providers: &[&str],
    actions: &[Action],
    steps: Option<&[Step]>,
) -> Result<(), &'static str> {
    if providers.iter().any(|p| !AUTOMATED.contains(p)) {
        return Err("unsupported_account");
    }
    // Likes and reposts of our posts, and reposting and following back, are Bluesky only (reposting
    // also X). A
    // scope with a Bluesky account in it is left to fail item by item, as a mixed one is.
    let no_bluesky = !providers.is_empty() && !providers.contains(&"bluesky");
    if no_bluesky && activity(trigger) {
        return Err("bluesky_only_trigger");
    }
    // Reposting is also done on X (`x_react`); following back only on Bluesky.
    let only_x = !providers.is_empty() && providers.iter().all(|p| *p == "x");
    let bluesky_action = |a: &Action| {
        matches!(a, Action::FollowBack) || (!only_x && matches!(a, Action::RepostComment))
    };
    let bluesky_step =
        |s: &Step| matches!(s, Step::FollowBack) || (!only_x && matches!(s, Step::RepostComment));
    if no_bluesky
        && (actions.iter().any(bluesky_action)
            || steps.unwrap_or_default().iter().any(bluesky_step))
    {
        return Err("bluesky_only_action");
    }
    if !providers.is_empty()
        && providers.iter().all(|p| *p == "x")
        && matches!(
            trigger,
            "new_follower" | "story_reply" | "story_mention" | "button_clicked"
        )
    {
        return Err("x_unsupported_trigger");
    }
    // Threads has replies and mentions only: no follow, story or button, and no DM at all.
    if !providers.is_empty() && providers.iter().all(|p| *p == "threads") {
        if !matches!(trigger, "comment_received" | "mention_received") {
            return Err("threads_unsupported_trigger");
        }
        let dm = actions
            .iter()
            .any(|a| a.via_text().is_some_and(|s| s.0 == Via::Dm))
            || steps.is_some_and(|s| s.iter().any(|x| x.sends().is_some_and(|s| s.0 == Via::Dm)));
        if dm {
            return Err("threads_no_dm");
        }
        // Threads can hide a reply but has no like.
        let like = actions.iter().any(|a| matches!(a, Action::LikeComment))
            || steps.is_some_and(|s| s.iter().any(|x| matches!(x, Step::LikeComment)));
        if like {
            return Err("threads_no_like");
        }
        // 500 characters, an emoji counted as its UTF-8 bytes, with a name at its widest.
        let widest = |t: &str| {
            t.replace("{name}", &"\u{10348}".repeat(MAX_NAME_CHARS))
                .replace("{page}", &"p".repeat(MAX_PAGE_CHARS))
        };
        let texts = actions
            .iter()
            .filter_map(|a| a.via_text().filter(|_| !a.ai()))
            .chain(
                steps
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|s| s.sends().filter(|_| !s.ai()).map(|(v, t, _)| (v, t))),
            );
        if texts
            .into_iter()
            .any(|(_, t)| !inbox::fits_on("threads", Via::Public, &widest(t)))
        {
            return Err("invalid_text");
        }
    }
    if !providers.is_empty() && providers.iter().all(|p| *p == "youtube") {
        if trigger != "comment_received" {
            return Err("youtube_unsupported_trigger");
        }
        let dm_step = steps
            .unwrap_or_default()
            .iter()
            .any(|s| matches!(s, Step::SendDm { .. } | Step::AiSendDm { .. }));
        let dm_action = actions
            .iter()
            .any(|a| matches!(a, Action::SendDm { .. } | Action::AiSendDm { .. }));
        if dm_step || dm_action {
            return Err("youtube_no_dm");
        }
    }
    // LinkedIn: comments only, answered in public (or liked); no messaging, no hiding.
    if !providers.is_empty() && providers.iter().all(|p| *p == "linkedin") {
        if trigger != "comment_received" {
            return Err("linkedin_unsupported_trigger");
        }
        let dm = actions
            .iter()
            .any(|a| a.via_text().is_some_and(|s| s.0 == Via::Dm))
            || steps.is_some_and(|s| s.iter().any(|x| x.sends().is_some_and(|s| s.0 == Via::Dm)));
        if dm {
            return Err("linkedin_no_dm");
        }
        let hide = actions.iter().any(|a| matches!(a, Action::HideComment))
            || steps.is_some_and(|s| s.iter().any(|x| matches!(x, Step::HideComment)));
        if hide {
            return Err("linkedin_no_hide");
        }
    }
    if !providers.is_empty()
        && providers.iter().all(|p| *p == "bluesky")
        && matches!(trigger, "story_reply" | "story_mention" | "button_clicked")
    {
        return Err("bluesky_unsupported_trigger");
    }
    // Facebook and Instagram bring no follows and no mentions of the account: they never fire.
    if matches!(trigger, "new_follower" | "mention_received")
        && !providers.is_empty()
        && providers
            .iter()
            .all(|p| matches!(*p, "facebook" | "instagram"))
    {
        return Err("meta_unsupported_trigger");
    }
    // A story mention is Instagram's alone.
    if trigger == "story_mention"
        && !providers.is_empty()
        && providers.iter().all(|p| *p == "facebook")
    {
        return Err("facebook_unsupported_trigger");
    }
    if providers.is_empty()
        || !providers
            .iter()
            .all(|p| matches!(*p, "facebook" | "instagram"))
    {
        return Ok(());
    }
    let dms: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            Action::SendDm { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let mut waited = 0u32;
    // After a `wait_for_reply` the person has written: the next DMs are in a new 24 hour window,
    // no longer the comment's one private reply.
    let mut answered = false;
    let mut private_replies = dms.len();
    let mut all_dms = dms.clone();
    for step in steps.unwrap_or_default() {
        match step {
            Step::Wait { minutes } => waited += minutes,
            Step::WaitForReply { .. } => (waited, answered) = (0, true),
            Step::SendDm { text, .. } => {
                let dm_trigger = !public(trigger) && trigger != "new_follower";
                if (dm_trigger || answered) && waited >= META_WINDOW_MINUTES {
                    return Err("meta_reply_window");
                }
                if trigger == "comment_received"
                    && !answered
                    && waited >= META_PRIVATE_REPLY_MINUTES
                {
                    return Err("meta_reply_window");
                }
                private_replies += usize::from(!answered);
                all_dms.push(text);
            }
            _ => {}
        }
    }
    if trigger == "comment_received" && private_replies > 1 {
        return Err("meta_one_private_reply");
    }
    let dms = all_dms;
    // A name may be in any script: each of its letters counted at 4 bytes, the most UTF-8 takes.
    let widest = |t: &str| {
        t.replace("{name}", &"\u{10348}".repeat(MAX_NAME_CHARS))
            .replace("{page}", &"p".repeat(MAX_PAGE_CHARS))
    };
    let instagram = providers.contains(&"instagram");
    if instagram
        && dms
            .iter()
            .any(|t| !inbox::fits_on("instagram", Via::Dm, &widest(t)))
    {
        return Err("invalid_text");
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
            .any(|a| a.via_text().is_some_and(|s| s.0 == Via::Public))
    {
        return Err("invalid_actions");
    }
    if !public && fallback.is_some() {
        return Err("invalid_fallback");
    }
    let sends = || actions.iter().filter_map(Action::via_text);
    if !public && sends().any(|s| s.1.contains("{page}")) {
        return Err("invalid_text");
    }
    let once = |f: fn(&Action) -> bool| actions.iter().filter(|a| f(a)).count() <= 1;
    let comment_only = actions.iter().any(|a| matches!(a, Action::HideComment));
    let post_only = actions
        .iter()
        .any(|a| matches!(a, Action::LikeComment | Action::RepostComment));
    // A like or repost has nobody to message or tag.
    let nobody = activity(trigger)
        && actions
            .iter()
            .any(|a| a.via_text().is_some() || matches!(a, Action::TagContact { .. }));
    if (comment_only && trigger != "comment_received")
        || (post_only && !public)
        || nobody
        || !once(|a| matches!(a, Action::RepostComment))
        || !once(|a| matches!(a, Action::FollowBack))
        || !once(|a| matches!(a, Action::NotifyTeam))
        || !once(|a| matches!(a, Action::Assign { .. }))
        || !once(|a| matches!(a, Action::HideComment))
        || !once(|a| matches!(a, Action::LikeComment))
        || !once(|a| matches!(a, Action::StopAutomations))
    {
        return Err("invalid_actions");
    }
    // One or two sends, at most one of each type, and up to 3 tags, each once. Beside them
    // each other action once; one of them (or a send) is needed, tags and `stop_automations`
    // alone do nothing.
    let mut tags: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            Action::TagContact { tag } => Some(tag.as_str()),
            _ => None,
        })
        .collect();
    let (dms, tagged) = (sends().filter(|s| s.0 == Via::Dm).count(), tags.len());
    tags.sort();
    tags.dedup();
    let sent = sends().count();
    if !actions.iter().any(Action::outward)
        && !actions.iter().any(|a| matches!(a, Action::Assign { .. }))
        || dms > 1
        || sent - dms > 1
    {
        return Err("invalid_actions");
    }
    if tagged > MAX_TAG_ACTIONS || tags.len() != tagged || !tags.iter().all(|t| tag_ok(t)) {
        return Err("invalid_actions");
    }
    for action in actions {
        let fine = match action.via_text() {
            Some((_, prompt)) if action.ai() => prompt_ok(prompt),
            Some((via, text)) => inbox::fits(via, &longest(text)) && page_stands_alone(text),
            None => true,
        };
        if !fine {
            return Err("invalid_text");
        }
    }
    if let Some(f) = fallback {
        let dm = sends().any(|s| s.0 == Via::Dm);
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
    /// The moment the conditions on weekdays and hours are judged at.
    pub now: chrono::DateTime<chrono::Utc>,
}

/// Why `f` does not meet `c`: one code per condition that fails (`wrong_set`, `wrong_account`,
/// `wrong_post`, `no_keyword`, `not_first_from_author`, `not_first_message`, `wrong_weekday`,
/// `outside_hours`, `wrong_language`, `wrong_link`); empty when it does. `question` is judged
/// apart (`asks_question`), and `sentiment` (`read_sentiment`), as they may cost an AI call.
pub fn mismatches(c: &Conditions, f: &Facts<'_>) -> Vec<&'static str> {
    let text = f.text.to_lowercase();
    let post_ok = c.post_remote_ids.is_empty()
        || c.post_remote_ids
            .iter()
            .any(|p| Some(p.as_str()) == f.parent || Some(p.as_str()) == f.root);
    [
        (
            c.social_set_id.is_none_or(|s| f.account_sets.contains(&s)),
            "wrong_set",
        ),
        (
            c.account_ids.is_empty() || c.account_ids.contains(&f.account_id),
            "wrong_account",
        ),
        (post_ok, "wrong_post"),
        (
            c.keywords.is_empty() || c.keywords.iter().any(|k| text.contains(&k.to_lowercase())),
            "no_keyword",
        ),
        (
            !c.first_per_author || f.first_from_author,
            "not_first_from_author",
        ),
        (!c.first_message || f.first_message, "not_first_message"),
        (
            c.has_link.is_none_or(|want| want == ai::has_link(f.text)),
            "wrong_link",
        ),
    ]
    .into_iter()
    .filter_map(|(ok, why)| (!ok).then_some(why))
    .chain(c.when.as_ref().and_then(|w| w.mismatch(f.now)))
    .chain(
        (!c.languages.is_empty()
            && !language_of(f.text).is_some_and(|l| c.languages.iter().any(|x| x == l)))
        .then_some("wrong_language"),
    )
    .collect()
}

pub fn matches(c: &Conditions, f: &Facts<'_>) -> bool {
    mismatches(c, f).is_empty()
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
    // A DM a waiting flow was waiting for belongs to that flow: nothing else answers it.
    if !answers_waiting(&app.db, item).await? {
        run_matching(app, account, item).await?;
    }
    // Done to the end: the sweep leaves it alone from now on.
    app.db
        .execute_raw(sql(
            "UPDATE inbox_items SET automated_at = now() WHERE id = $1",
            vec![item.id.into()],
        ))
        .await
        .map(|_| ())
}

/// Brings forward the waiting runs of flows at a `wait_for_reply` that `item` answers: a DM
/// from the person the run was started for, on the same account, that came after the run began.
/// `tick` then takes them up. Whether it answered any.
async fn answers_waiting(
    db: &DatabaseConnection,
    item: &inbox_items::Model,
) -> Result<bool, DbErr> {
    if item.kind != "dm" {
        return Ok(false);
    }
    let stmt = sql(
        "UPDATE automation_runs r SET reply_item_id = $1, resume_at = now(), updated_at = now() \
         FROM inbox_items i WHERE i.id = r.inbox_item_id AND r.team_id = $2 \
         AND r.awaiting_reply AND r.status = 'waiting' AND r.reply_item_id IS NULL \
         AND i.social_account_id = $3 AND i.author_id = $4 AND i.platform = $5 \
         AND r.created_at <= $6",
        vec![
            item.id.into(),
            item.team_id.into(),
            item.social_account_id.into(),
            item.author_id.clone().into(),
            item.platform.clone().into(),
            item.received_at.into(),
        ],
    );
    if db.execute_raw(stmt).await?.rows_affected() > 0 {
        return Ok(true);
    }
    // Taken before (the sweep takes an item again after a crash): still nobody else's.
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM automation_runs WHERE reply_item_id = $1) AS taken",
        vec![item.id.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "taken"),
        None => Ok(false),
    }
}

/// Items automations never finished (a crash, a restart, a database error) are older than this
/// before the sweep takes them, so it never races the round that stored them.
const SWEEP_AFTER_MINUTES: i64 = 2;
/// Older items are left alone: a reply days late does more harm than none.
const SWEEP_WITHIN_HOURS: i64 = 24;
const SWEEP_BATCH: u64 = 50;

/// Runs automations again over recent items they never finished, oldest first. A run that did
/// happen is not repeated (`claim` is unique per automation and item).
pub async fn sweep(app: &AppState) -> Result<(), DbErr> {
    // Ages count from when brain stored the item (`created_at`), not the platform's time, so a
    // round still running over late items is never raced.
    let now = chrono::Utc::now();
    let items = inbox_items::Entity::find()
        .filter(inbox_items::Column::AutomatedAt.is_null())
        .filter(
            inbox_items::Column::CreatedAt
                .lt((now - chrono::Duration::minutes(SWEEP_AFTER_MINUTES)).fixed_offset()),
        )
        .filter(
            inbox_items::Column::CreatedAt
                .gt((now - chrono::Duration::hours(SWEEP_WITHIN_HOURS)).fixed_offset()),
        )
        .order_by_asc(inbox_items::Column::CreatedAt)
        .limit(SWEEP_BATCH)
        .all(&app.db)
        .await?;
    for item in items {
        let Some(account) = social_accounts::Entity::find_by_id(item.social_account_id)
            .one(&app.db)
            .await?
        else {
            continue;
        };
        let id = item.id;
        // A Meta item is filled in first (the author's name, a Reel's post), as the webhook does.
        let ran = if matches!(account.provider.as_str(), "facebook" | "instagram") {
            crate::meta_inbox::enrich_and_run(app, &account, item).await
        } else {
            run(app, &account, &item).await
        };
        // One second chance only: an item that fails again is given up, so it cannot hold the
        // batch for a day.
        if let Err(e) = ran {
            tracing::error!(error = %e, item = %id, "automation sweep failed, given up");
            app.db
                .execute_raw(sql(
                    "UPDATE inbox_items SET automated_at = now() WHERE id = $1",
                    vec![id.into()],
                ))
                .await?;
        }
    }
    Ok(())
}

/// The live sets that hold `account`.
pub(crate) async fn live_set_ids(
    db: &DatabaseConnection,
    account: Uuid,
) -> Result<Vec<Uuid>, DbErr> {
    let in_live = Query::select()
        .column(membership::Column::SocialSetId)
        .from(membership::Entity)
        .and_where(membership::Column::SocialAccountId.eq(account))
        .and_where(membership::Column::SocialSetId.in_subquery(live_sets()))
        .to_owned();
    let rows = db.query_all(&in_live).await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get("", "social_set_id").ok())
        .collect())
}

async fn run_matching(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<(), DbErr> {
    let db = &app.db;
    let Some(trigger) = trigger_of(item) else {
        return Ok(());
    };
    // Comments from before the account was connected (a first read goes back 7 days) never
    // run automations.
    if item.received_at < account.created_at {
        return Ok(());
    }
    // The automations this item is not for, or that were off when it came, say why.
    automation_misses::record_unreached(db, item.team_id, item.id, trigger, item.received_at).await;
    let autos = automations::Entity::find()
        .filter(automations::Column::TeamId.eq(item.team_id))
        .filter(automations::Column::Active.eq(true))
        .filter(automations::Column::Trigger.eq(trigger))
        .filter(automations::Column::ActivatedAt.lte(item.received_at))
        .order_by_asc(automations::Column::CreatedAt)
        .all(db)
        .await?;
    if autos.is_empty() {
        return Ok(());
    }
    // Nothing runs on an item by a connected account (the team's own, or another team's), so
    // two automated accounts cannot answer each other in a loop, and nothing runs while the team
    // has paused its automations. Each automation that would have run stores why it did not.
    let guard = loop_guard(db, item, account).await?;
    // A follower is welcomed once per account ever: an unfollow and a new follow stores a new
    // item (a new record) but runs nothing.
    let followed_before = if item.kind == "follow" {
        let stmt = sql(
            "SELECT EXISTS (SELECT 1 FROM inbox_items WHERE social_account_id = $1 \
             AND kind = 'follow' AND author_id = $2 AND id < $3) AS ok",
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
        false
    };
    let sets = live_set_ids(db, account.id).await?;
    // Set once an automation that stops the others has taken the item.
    let mut stopped = false;
    for a in autos {
        if stopped {
            automation_misses::record(db, (a.id, a.team_id, item.id), &["stopped_by_other"]).await;
            continue;
        }
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
                &format!(
                    "SELECT NOT EXISTS (SELECT 1 FROM automation_runs r \
                     JOIN inbox_items i ON i.id = r.inbox_item_id \
                     WHERE r.automation_id = $1 AND i.author_id = $2 AND i.id <> $3 \
                     AND NOT {NOT_ACTED}) AS ok"
                ),
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
            now: chrono::Utc::now(),
        };
        // A click runs the automations that name its button.
        let clicked = item.button_payload.as_ref();
        let named =
            trigger != "button_clicked" || clicked.is_some_and(|p| cond.payloads.contains(p));
        let mut why = mismatches(&cond, &facts);
        if !named {
            why.push("button_not_named");
        }
        if !why.is_empty() {
            automation_misses::record(db, (a.id, a.team_id, item.id), &why).await;
            continue;
        }
        if followed_before {
            automation_misses::record(db, (a.id, a.team_id, item.id), &["followed_before"]).await;
            continue;
        }
        // An action that needs the post's page does not run without one, and leaves no run:
        // it costs no daily hit, and the author's first comment under a post with a page
        // still counts as their first.
        let holds = |t: &str| t.contains("{page}");
        let needed = match &steps {
            Some(steps) => steps.iter().any(|x| x.sends().is_some_and(|x| holds(x.1))),
            None => actions
                .iter()
                .filter_map(Action::via_text)
                .any(|x| holds(x.1)),
        };
        let wanted = needed || a.fallback_public_reply.as_deref().is_some_and(holds);
        let page = match item.post_id {
            Some(post) if wanted => pages::address(db, &app.web_url, item.team_id, post).await?,
            _ => None,
        };
        if needed && page.is_none() {
            tracing::info!(automation = %a.id, item = %item.id, "no public page, not run");
            automation_misses::record(db, (a.id, a.team_id, item.id), &["no_public_page"]).await;
            continue;
        }
        // Read for each automation, so a pause during the round holds the rest back.
        let held = match guard {
            Some(why) => Some(why),
            None if team_paused(db, item.team_id).await? => Some("paused"),
            None => None,
        };
        if let Some(why) = held {
            skip_run(db, &a, item, why).await?;
            continue;
        }
        // Last of the conditions, as it may cost an AI call: only a message that got this far.
        if cond.question {
            // A cool-down or cap that already holds the run is no reason to pay for a judgment.
            if let Some(why) = limited(db, &a, item, Uuid::nil()).await? {
                skip_run(db, &a, item, why).await?;
                continue;
            }
            match asks_question(app, item, a.id).await {
                Ok(true) => {}
                Ok(false) => {
                    skip_run(db, &a, item, "no_question").await?;
                    continue;
                }
                Err(why) => {
                    skip_run(db, &a, item, why).await?;
                    continue;
                }
            }
        }
        // And the mood, read last of all: a model call too, so not for an item this automation
        // already has a run for (the sweep took it again), nor one a cool-down or cap holds, and
        // a team reads at most as many texts a day as it may start runs.
        if !cond.sentiment.is_empty() && !run_exists(db, &a, item).await? {
            if let Some(why) = limited(db, &a, item, Uuid::nil()).await? {
                skip_run(db, &a, item, why).await?;
                continue;
            }
            let cap = [(
                format!("automation:sentiment:{}", item.team_id),
                RUNS_PER_DAY,
            )];
            if crate::rate_limit::daily_reserve(db, &cap).await?.is_none() {
                skip_run(db, &a, item, "sentiment_cap").await?;
                continue;
            }
            let reference = format!("ai:sentiment:{}:{}", a.id, item.id);
            match read_sentiment(app, account, item, &reference).await {
                Ok(label) if cond.sentiment.iter().any(|i| i == label) => {}
                Ok(_) => continue,
                Err(r) => {
                    let why = r.reason.unwrap_or_else(|| "ai_failed".into());
                    skip_run(db, &a, item, &why).await?;
                    continue;
                }
            }
        }
        // The hit pays for the first outward action: a flow that starts on a tag has not
        // reached anyone yet.
        let sends = match &steps {
            Some(s) => s.first().is_some_and(Step::outward),
            None => actions.iter().any(Action::outward),
        };
        let stop = match &steps {
            Some(s) => s.iter().any(|x| matches!(x, Step::StopAutomations)),
            None => actions.iter().any(|x| matches!(x, Action::StopAutomations)),
        };
        let once = cond.first_per_author;
        let Some(run_id) = claim(db, &a, item, steps.is_some(), sends, once).await? else {
            // The sweep took the item again: this automation did take it before, and the newer
            // ones are still to leave it alone.
            if stop && ran(db, &a, item).await? {
                stopped = true;
            }
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
                reply: None,
            };
            flow.advance(run_id, 0, Vec::new()).await?;
        } else {
            let results = act(app, account, item, &a, &actions, page.as_deref(), run_id).await;
            finish(db, run_id, results).await?;
        }
        // This automation took the item: the newer ones leave it alone.
        if stop {
            stopped = true;
        }
    }
    Ok(())
}

/// Whether `a` has a run on `item`, whatever it came to.
async fn run_exists(
    db: &impl ConnectionTrait,
    a: &automations::Model,
    item: &inbox_items::Model,
) -> Result<bool, DbErr> {
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM automation_runs WHERE automation_id = $1 \
         AND inbox_item_id = $2) AS yes",
        vec![a.id.into(), item.id.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "yes"),
        None => Ok(false),
    }
}

/// Whether `a` took `item`: it has a run on it that did something (`NOT_ACTED`).
async fn ran(
    db: &impl ConnectionTrait,
    a: &automations::Model,
    item: &inbox_items::Model,
) -> Result<bool, DbErr> {
    let stmt = sql(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM automation_runs r WHERE r.automation_id = $1 \
             AND r.inbox_item_id = $2 AND NOT {NOT_ACTED}) AS yes"
        ),
        vec![a.id.into(), item.id.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "yes"),
        None => Ok(false),
    }
}

/// Why a connected account's item runs nothing: `own_account` when its author is another account
/// of the item's team, `connected_account` when it is an account of any other team.
async fn loop_guard(
    db: &DatabaseConnection,
    item: &inbox_items::Model,
    account: &social_accounts::Model,
) -> Result<Option<&'static str>, DbErr> {
    let accounts = social_accounts::Entity::find()
        .select_only()
        .column(social_accounts::Column::TeamId)
        .filter(social_accounts::Column::Provider.eq(&account.provider));
    // Threads names an author by username only, never by the numeric id its accounts are kept
    // under, so its accounts are found by handle.
    let accounts = if account.provider == "threads" {
        accounts.filter(sea_orm::sea_query::Expr::cust_with_values(
            "lower(handle) = lower($1)",
            [item.author_id.clone()],
        ))
    } else {
        accounts.filter(social_accounts::Column::ExternalId.eq(&item.author_id))
    };
    let teams: Vec<Uuid> = accounts.into_tuple().all(db).await?;
    Ok(match teams.contains(&item.team_id) {
        true => Some("own_account"),
        false if !teams.is_empty() => Some("connected_account"),
        false => None,
    })
}

/// Whether the team has paused all its automations (`teams.automations_paused`).
pub(crate) async fn team_paused(db: &impl ConnectionTrait, team: Uuid) -> Result<bool, DbErr> {
    let stmt = sql(
        "SELECT automations_paused AS paused FROM teams WHERE id = $1",
        vec![team.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "paused"),
        None => Ok(false),
    }
}

/// Stores a run that did nothing, `skipped` for a safety `reason` the run log shows. Once per
/// automation and item.
async fn skip_run(
    db: &DatabaseConnection,
    a: &automations::Model,
    item: &inbox_items::Model,
    reason: &str,
) -> Result<(), DbErr> {
    let stmt = sql(
        "INSERT INTO automation_runs (id, automation_id, inbox_item_id, team_id, status, reason) \
         VALUES ($1, $2, $3, $4, 'skipped', $5) ON CONFLICT (automation_id, inbox_item_id) DO NOTHING",
        vec![
            Uuid::now_v7().into(),
            a.id.into(),
            item.id.into(),
            a.team_id.into(),
            reason.into(),
        ],
    );
    db.execute_raw(stmt).await.map(|_| ())
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
    /// The DM that answered a `wait_for_reply`: DM steps after it go out in its conversation.
    reply: Option<&'a inbox_items::Model>,
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
                    return park(db, run, self.a, (*minutes, false), &results).await;
                }
                Step::WaitForReply { minutes } => {
                    return park(db, run, self.a, (*minutes, true), &results).await;
                }
                // Whatever came of it, the flow goes on.
                step @ (Step::TagContact { .. }
                | Step::NotifyTeam
                | Step::Assign { .. }
                | Step::HideComment
                | Step::LikeComment
                | Step::RepostComment
                | Step::FollowBack
                | Step::StopAutomations) => {
                    if let Some(action) = step.effect() {
                        let (app, account, item) = (self.app, self.account, self.item);
                        results.push(effect(app, account, item, self.a, &action).await);
                    }
                    if !self.step_on(run, &mut i, &results).await? {
                        return Ok(());
                    }
                    continue;
                }
                Step::SendDm { text, buttons } => (Via::Dm, text, buttons),
                Step::ReplyPublicly { text } => (Via::Public, text, &[]),
                Step::AiSendDm { prompt } => (Via::Dm, prompt, &[]),
                Step::AiReplyPublicly { prompt } => (Via::Public, prompt, &[]),
            };
            let kind = self.steps[i].kind();
            // A DM goes out where the person last wrote.
            let target = match (via, self.reply) {
                (Via::Dm, Some(reply)) => reply,
                _ => self.item,
            };
            let result = match with_page(template, self.page) {
                _ if self.steps[i].ai() => {
                    let reference = ai_reference(db, run, i).await?;
                    let (app, account, item) = (self.app, self.account, target);
                    ai_send(
                        app, account, item, self.a.id, kind, via, template, &reference,
                    )
                    .await
                }
                None => no_page(kind),
                Some(template) => {
                    let (text, filled) = render(&template, self.item.author_name.as_deref());
                    let from = (None, Some(self.a.id));
                    let (account, item) = (self.account, target);
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
            // A refused AI send is not tried again (a person decides: it costs a model call), but
            // a provider that gave no answer sent nothing and cost nothing.
            let again = if self.steps[i].ai() {
                result.unavailable
            } else {
                is_rate_limited(&result)
            };
            if again && retry_later(db, run, i, &results).await? {
                // Parked: step `i` is sent again later, and this refusal is not kept.
                return Ok(());
            }
            let stopped = (result.status != "sent").then(|| result.clone());
            results.push(result);
            if let Some(r) = stopped {
                return settle(db, run, &r.status, r.reason, &results).await;
            }
            if !self.step_on(run, &mut i, &results).await? {
                return Ok(());
            }
        }
    }

    /// Moves `i` to the next step and claims it; `false` when the run is over: that was the
    /// last step (the run ends `sent`), or the step must not run (`claim_step`).
    async fn step_on(
        &self,
        run: Uuid,
        i: &mut usize,
        results: &[ActionResult],
    ) -> Result<bool, DbErr> {
        let db = &self.app.db;
        *i += 1;
        let Some(next) = self.steps.get(*i) else {
            settle(db, run, "sent", None, results).await?;
            return Ok(false);
        };
        claim_step(db, run, self.a, *i, next.outward(), results).await
    }
}

/// Runs an action that is not a send, as a result.
async fn effect(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    a: &automations::Model,
    action: &Action,
) -> ActionResult {
    let done = |kind: &str, why: Option<&str>, failed: bool| ActionResult {
        kind: kind.into(),
        status: match (why, failed) {
            (None, _) => "done",
            (Some(_), true) => "failed",
            (Some(_), false) => "skipped",
        }
        .into(),
        reason: why.map(str::to_owned),
        ..Default::default()
    };
    let kind = action.kind();
    match action {
        Action::TagContact { tag } => tag_contact(&app.db, item, tag).await,
        Action::StopAutomations => done(kind, None, false),
        Action::NotifyTeam => match notify_team(app, item, a).await {
            Ok(why) => done(kind, why, why == Some("mail_failed")),
            Err(e) => {
                tracing::error!(error = %e, item = %item.id, "automation notice failed");
                done(kind, Some("internal"), true)
            }
        },
        Action::Assign { user_id } => match assign(&app.db, item, *user_id).await {
            Ok(why) => done(kind, why, false),
            Err(e) => {
                tracing::error!(error = %e, item = %item.id, "automation assign failed");
                done(kind, Some("internal"), true)
            }
        },
        Action::LikeComment if account.provider == "linkedin" => {
            if item.kind != "comment" {
                return done(kind, Some("not_supported"), false);
            }
            match crate::linkedin_inbox::like(app, account, item).await {
                Ok(()) => done(kind, None, false),
                Err(e) => {
                    let calm = matches!(e, InboxError::Invalid(_) | InboxError::RateLimited);
                    done(kind, Some(e.code()), !calm)
                }
            }
        }
        Action::HideComment | Action::LikeComment | Action::RepostComment
            if account.provider == "x" =>
        {
            x_react(app, account, item, action).await
        }
        // On Bluesky the inbox buttons' own code, with the same refusals.
        Action::LikeComment | Action::RepostComment | Action::FollowBack
            if account.provider == "bluesky" =>
        {
            let act = match action {
                Action::LikeComment => bluesky_actions::Action::Like,
                Action::RepostComment => bluesky_actions::Action::Repost,
                _ => bluesky_actions::Action::Follow,
            };
            automation_effects::bluesky(app, account, item, a, (kind, act)).await
        }
        Action::RepostComment | Action::FollowBack => done(kind, Some("not_supported"), false),
        Action::HideComment | Action::LikeComment => {
            let m = if matches!(action, Action::HideComment) {
                Moderate::Hide
            } else {
                Moderate::Like
            };
            // Threads can hide, not like: its reply is hidden through `manage_reply`.
            if account.provider == "threads" {
                if m != Moderate::Hide {
                    return done(kind, Some("threads_no_like"), false);
                }
                if item.kind != "comment" {
                    return done(kind, Some("not_supported"), false);
                }
                return match crate::threads_inbox::perform(app, account, item, Some(true)).await {
                    Ok(_) => done(kind, None, false),
                    Err(e) => {
                        let calm = matches!(
                            e,
                            InboxError::RateLimited
                                | InboxError::Invalid("not_our_thread" | "threads_nested")
                        );
                        done(kind, Some(e.code()), !calm)
                    }
                };
            }
            let meta = matches!(account.provider.as_str(), "facebook" | "instagram");
            if !meta || item.kind != "comment" {
                return done(kind, Some("not_supported"), false);
            }
            match crate::meta_inbox::moderate(app, account, item, m).await {
                Ok(Ok(())) => done(kind, None, false),
                Ok(Err(e)) => {
                    // Nothing was done for these; anything else is a refusal worth a look.
                    let calm = matches!(
                        e,
                        InboxError::RateLimited
                            | InboxError::Invalid("not_supported" | "not_found")
                    );
                    done(kind, Some(e.code()), !calm)
                }
                Err(e) => {
                    tracing::error!(error = %e, item = %item.id, "automation moderation failed");
                    done(kind, Some("internal"), true)
                }
            }
        }
        // A send never comes here.
        _ => done(kind, Some("not_supported"), false),
    }
}

/// A like, repost or hide on X: acts on the post of the item, a write X bills, so the account's
/// budget and the team's credits stop it as they stop a send (`x_automation::blocked`). `done`;
/// `skipped` with the reason when it did not apply or could not go; `failed` when X refused it.
async fn x_react(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    action: &Action,
) -> ActionResult {
    use crate::x_inbox::Action as Do;
    let kind = action.kind();
    let result = |status: &str, reason: Option<&str>| ActionResult {
        kind: kind.into(),
        status: status.into(),
        reason: reason.map(str::to_owned),
        ..Default::default()
    };
    // Hiding is for a reply to our post; a like or repost fits a comment or a mention.
    let hide = matches!(action, Action::HideComment);
    if !matches!(item.kind.as_str(), "comment" | "mention") || (hide && item.kind != "comment") {
        return result("skipped", Some("not_supported"));
    }
    let what = match action {
        Action::LikeComment => Do::Like,
        Action::HideComment => Do::Hide,
        _ => Do::Repost,
    };
    match crate::x_automation::blocked(app, account).await {
        Ok(Some(why)) => return result("skipped", Some(why)),
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, item = %item.id, "x budget not read");
            return result("failed", Some("internal"));
        }
    }
    match crate::x_inbox::act(app, account, item, what).await {
        Ok(()) => {
            if let Err(e) = crate::x_automation::spent(&app.db, account.id).await {
                tracing::error!(error = %e, item = %item.id, "x write not counted against the budget");
            }
            result("done", None)
        }
        Err(e @ (InboxError::RateLimited | InboxError::Invalid("not_repliable"))) => {
            result("skipped", Some(e.code()))
        }
        Err(e) => result("failed", Some(e.code())),
    }
}

/// Most mails `notify_team` sends a team a day.
const NOTICES_PER_DAY: i64 = 20;

/// Mails the team's owners and admins about `item`. `None` when a mail went out, else why not:
/// `notify_cap` (the day's limit), `mail_failed`.
async fn notify_team(
    app: &AppState,
    item: &inbox_items::Model,
    a: &automations::Model,
) -> Result<Option<&'static str>, DbErr> {
    let cap = [(
        format!("automation:notice:{}", item.team_id),
        NOTICES_PER_DAY,
    )];
    let Some(hits) = crate::rate_limit::daily_reserve(&app.db, &cap).await? else {
        return Ok(Some("notify_cap"));
    };
    let to = app
        .db
        .query_all_raw(sql(
            "SELECT u.email FROM team_user t JOIN users u ON u.id = t.user_id \
             WHERE t.team_id = $1 AND t.role IN ('owner', 'admin') ORDER BY u.email",
            vec![item.team_id.into()],
        ))
        .await?;
    // A stranger wrote the name and the text: only plain words go into the mail, so nothing in
    // it becomes a link.
    let plain = |t: &str, max: usize| -> String {
        let flat = socialreins_core::team::inert(t);
        flat.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(max)
            .collect()
    };
    let subject = format!("Automation \"{}\" ran", plain(&a.name, 60));
    // A like, repost or follow has no text.
    let said = match plain(&item.text, 200) {
        t if t.is_empty() => String::new(),
        t => format!("\n\nThe text: \"{t}\""),
    };
    let text = format!(
        "The automation \"{}\" ran on a {} on {}.{said}\n\nOpen it in the inbox:\n\n{}/app/inbox?item={}\n",
        plain(&a.name, 60),
        item.kind,
        crate::expiry::label(&item.platform),
        app.web_url,
        item.number,
    );
    let mut sent = false;
    for r in to {
        let email: String = r.try_get("", "email")?;
        match app
            .mailer
            .send(&email, &subject, text.clone(), &app.web_url)
            .await
        {
            Ok(()) => sent = true,
            Err(e) => tracing::warn!(error = %e, "automation notice not mailed"),
        }
    }
    if !sent {
        crate::rate_limit::daily_refund(&app.db, &hits).await;
    }
    Ok((!sent).then_some("mail_failed"))
}

/// Assigns the item to `user` unless it has an assignee. `None` when done, else why not:
/// `already_assigned`, `not_a_member`.
async fn assign(
    db: &DatabaseConnection,
    item: &inbox_items::Model,
    user: Uuid,
) -> Result<Option<&'static str>, DbErr> {
    let stmt = sql(
        "UPDATE inbox_items SET assigned_to = $2 WHERE id = $1 AND assigned_to IS NULL \
         AND EXISTS (SELECT 1 FROM team_user WHERE team_id = $3 AND user_id = $2)",
        vec![item.id.into(), user.into(), item.team_id.into()],
    );
    if db.execute_raw(stmt).await?.rows_affected() == 1 {
        return Ok(None);
    }
    let stmt = sql(
        "SELECT assigned_to IS NOT NULL AS taken FROM inbox_items WHERE id = $1",
        vec![item.id.into()],
    );
    let taken: bool = match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "taken")?,
        None => false,
    };
    Ok(Some(if taken {
        "already_assigned"
    } else {
        "not_a_member"
    }))
}

/// The instructions that make the model read an item instead of answering it.
const SENTIMENT_PROMPT: &str = "Do not write a reply to the person. Read their message and \
    answer with exactly one word: positive (thanks, praise or excitement), negative (a complaint, \
    anger, criticism or an insult) or neutral.";

/// Reads the item's mood, with the same key choice, allowance and checks an AI step has
/// (`ai_write`): `positive`, `negative` or `neutral`. `Err` holds the reason it could not be
/// read (`no_ai_key`, `no_ai_credits`, `ai_failed`, ...).
#[allow(clippy::result_large_err)] // the step's own result, used once
pub(crate) async fn read_sentiment(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    reference: &str,
) -> Result<&'static str, ActionResult> {
    let given = ("read_sentiment", Via::Dm, SENTIMENT_PROMPT);
    let written = ai_write(app, account, item, given, reference).await?;
    let word = written
        .text
        .split(|c: char| !c.is_alphabetic())
        .find(|w| !w.is_empty())
        .map(str::to_lowercase);
    Ok(match word.as_deref() {
        Some("positive") => "positive",
        Some("negative") => "negative",
        _ => "neutral",
    })
}

/// Adds `tag` to the contact of the item's author, as a result: `done` when it was added,
/// `skipped` when the contact has it already (`already_tagged`), holds 20 (`tag_limit`) or was
/// deleted (`no_contact`).
async fn tag_contact(
    db: &DatabaseConnection,
    item: &inbox_items::Model,
    tag: &str,
) -> ActionResult {
    let added = match item.contact_id {
        Some(contact) => inbox::add_tag(db, contact, tag).await,
        None => Ok(Some("no_contact")),
    };
    let (status, reason) = match added {
        Ok(None) => ("done", None),
        Ok(Some(why)) => ("skipped", Some(why.to_owned())),
        Err(e) => {
            tracing::error!(error = %e, item = %item.id, "contact tag failed");
            ("failed", Some("internal".to_owned()))
        }
    };
    ActionResult {
        kind: "tag_contact".into(),
        status: status.into(),
        reason,
        ..Default::default()
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
    // `paid`: a retry of a step whose first try took its hit.
    let claim = format!(
        "WITH was AS (SELECT retry_paid FROM automation_runs WHERE id = $1 FOR UPDATE) \
         UPDATE automation_runs r SET step_index = $2, results = $3, resume_at = NULL, \
         awaiting_reply = false, retry_paid = false, updated_at = now() FROM was \
         WHERE r.id = $1 AND r.status = 'running' \
         AND r.step_index = $2 - 1 AND {UNCHANGED} RETURNING was.retry_paid AS paid"
    );
    let Some(claimed) = txn.query_one_raw(sql(&claim, values)).await? else {
        txn.execute_raw(end).await?;
        txn.commit().await?;
        return Ok(false);
    };
    let paid: bool = claimed.try_get("", "paid")?;
    let cap = [(quota_bucket(a.team_id), RUNS_PER_DAY)];
    if sends && !paid && reserve_in(&txn, &cap).await?.is_none() {
        settle(&txn, run, "skipped", Some("daily_cap".into()), results).await?;
        txn.commit().await?;
        return Ok(false);
    }
    txn.commit().await?;
    Ok(true)
}

/// What an AI step pays under: `ai:{run}:{step}`, and once the step has been tried again
/// (`retry_later`) `ai:{run}:{step}:{tries}`, because the credit of a try that failed was given
/// back and a reference given back is never paid again.
async fn ai_reference(db: &DatabaseConnection, run: Uuid, i: usize) -> Result<String, DbErr> {
    let row = db
        .query_one_raw(sql(
            "SELECT retries FROM automation_runs WHERE id = $1",
            vec![run.into()],
        ))
        .await?;
    let tries: i32 = row
        .map(|r| r.try_get("", "retries"))
        .transpose()?
        .unwrap_or(0);
    Ok(match tries {
        0 => format!("ai:{run}:{i}"),
        n => format!("ai:{run}:{i}:{n}"),
    })
}

/// A send the platform refused for rate limiting: nothing went out.
fn is_rate_limited(r: &ActionResult) -> bool {
    r.status == "skipped" && r.reason.as_deref() == Some("rate_limited")
}

/// Parks a flow run to send step `i` again in 5, 10 or 15 minutes, with `results` as they stood
/// before it. `false` when the tries are used up or the run is no longer ours.
async fn retry_later(
    db: &DatabaseConnection,
    run: Uuid,
    i: usize,
    results: &[ActionResult],
) -> Result<bool, DbErr> {
    let stmt = sql(
        "UPDATE automation_runs SET status = 'waiting', step_index = $2 - 1, results = $3, \
         retries = retries + 1, retry_paid = true, \
         resume_at = now() + make_interval(mins => (retries + 1) * 5), \
         updated_at = now() WHERE id = $1 AND status = 'running' AND retries < $4",
        vec![
            run.into(),
            (i as i32).into(),
            serde_json::json!(results).into(),
            MAX_RETRIES.into(),
        ],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected() == 1)
}

/// Whether a person may try a run again: it ended `skipped` or `failed` only because the
/// platform rate limited a send, so nothing of that send, or of an action list, went out.
pub fn retryable(run: &automation_runs::Model, a: &automations::Model) -> bool {
    let results: Vec<ActionResult> =
        serde_json::from_value(run.results.clone()).unwrap_or_default();
    let sends = |r: &&ActionResult| !matches!(r.kind.as_str(), "tag_contact" | "stop_automations");
    let send = |r: &ActionResult| {
        matches!(
            r.kind.as_str(),
            "send_dm" | "reply_publicly" | "ai_send_dm" | "ai_reply_publicly"
        )
    };
    let refused = run.reason.as_deref() == Some("rate_limited")
        && match (run.step_index, results.last()) {
            // A flow: the last step is a send the platform refused; whatever came before went out and stays done.
            (Some(_), Some(last)) => send(last) && is_rate_limited(last),
            // An action list: nothing of it went out.
            (None, Some(_)) => results.iter().filter(sends).all(is_rate_limited),
            _ => false,
        };
    refused
        && a.active
        && matches!(run.status.as_str(), "skipped" | "failed")
        && run.retries < MAX_MANUAL_RETRIES
}

/// Tries a run again for a person (`retryable`). A flow goes back to `waiting` to send its last
/// step again at the next round; an action list is run again now. `Err` holds why not:
/// `not_retryable`, `paused`. The transition is one statement, so two clicks start one retry.
pub async fn retry(
    app: &AppState,
    a: &automations::Model,
    run: &automation_runs::Model,
) -> Result<Result<(), &'static str>, DbErr> {
    let db = &app.db;
    if !retryable(run, a) {
        return Ok(Err("not_retryable"));
    }
    if team_paused(db, a.team_id).await? {
        return Ok(Err("paused"));
    }
    let flow = run.step_index.is_some();
    let stmt = if flow {
        sql(
            "UPDATE automation_runs r SET status = 'waiting', reason = NULL, \
             step_index = r.step_index - 1, \
             results = r.results - (jsonb_array_length(r.results) - 1), \
             retries = r.retries + 1, retry_paid = true, resume_at = now(), updated_at = now() \
             WHERE r.id = $1 AND r.status IN ('skipped', 'failed') AND r.retries < $2 \
             AND r.reason = 'rate_limited' AND r.retries = $4 \
             AND r.steps_hash = md5($3::jsonb::text) AND EXISTS (SELECT 1 FROM automations a \
             WHERE a.id = r.automation_id AND a.active AND md5(a.steps::text) = r.steps_hash)",
            vec![
                run.id.into(),
                MAX_MANUAL_RETRIES.into(),
                a.steps.clone().into(),
                run.retries.into(),
            ],
        )
    } else {
        sql(
            "UPDATE automation_runs r SET status = 'running', reason = NULL, results = '[]', \
             retries = r.retries + 1, updated_at = now() \
             WHERE r.id = $1 AND r.status IN ('skipped', 'failed') AND r.retries < $2 \
             AND r.reason = 'rate_limited' AND r.retries = $3",
            vec![run.id.into(), MAX_MANUAL_RETRIES.into(), run.retries.into()],
        )
    };
    // A run again takes its hit of the team's daily cap, as the first try did (a flow's step
    // already did).
    let cap = [(quota_bucket(a.team_id), RUNS_PER_DAY)];
    let hits = if flow {
        Vec::new()
    } else {
        match crate::rate_limit::daily_reserve(db, &cap).await? {
            Some(ids) => ids,
            None => return Ok(Err("daily_cap")),
        }
    };
    if db.execute_raw(stmt).await?.rows_affected() != 1 {
        crate::rate_limit::daily_refund(db, &hits).await;
        return Ok(Err("not_retryable"));
    }
    if flow {
        return Ok(Ok(()));
    }
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
    let actions: Vec<Action> = serde_json::from_value(a.actions.clone()).unwrap_or_default();
    let (Some(item), Some(account)) = (item, account) else {
        settle(db, run.id, "skipped", Some(CHANGED.into()), &[]).await?;
        return Ok(Ok(()));
    };
    let holds = |t: &str| t.contains("{page}");
    let wanted = actions
        .iter()
        .filter_map(Action::via_text)
        .any(|x| holds(x.1))
        || a.fallback_public_reply.as_deref().is_some_and(holds);
    let page = match item.post_id {
        Some(post) if wanted => pages::address(db, &app.web_url, item.team_id, post).await?,
        _ => None,
    };
    let results = act(app, &account, &item, a, &actions, page.as_deref(), run.id).await;
    finish(db, run.id, results).await?;
    Ok(Ok(()))
}

/// Parks a run at its `wait` step: `waiting` until `minutes` from now. A run whose automation
/// changed since the step was claimed ends instead (`cancel_waiting` came before it parked).
async fn park(
    db: &DatabaseConnection,
    run: Uuid,
    a: &automations::Model,
    (minutes, for_reply): (u32, bool),
    results: &[ActionResult],
) -> Result<(), DbErr> {
    let park = format!(
        "UPDATE automation_runs r SET status = 'waiting', results = $3, updated_at = now(), \
         resume_at = now() + make_interval(mins => $2), awaiting_reply = $5, \
         reply_item_id = CASE WHEN $5 THEN NULL ELSE r.reply_item_id END WHERE r.id = $1 \
         AND r.status = 'running' AND {UNCHANGED}"
    );
    let values = vec![
        run.into(),
        (minutes as i32).into(),
        serde_json::json!(results).into(),
        a.steps.clone().into(),
        for_reply.into(),
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
    // Every tenth minute, so the "why not" log stays small (a failure only delays it).
    if chrono::Timelike::minute(&chrono::Utc::now()).is_multiple_of(10) {
        if let Err(e) = automation_misses::prune(&app.db).await {
            tracing::warn!(error = %e, "automation misses not pruned");
        }
    }
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
    // A run parked to try its step again has the one before it (-1 before the first).
    let i = run.step_index.map_or(0, |i| (i + 1) as usize);
    let (Some(a), Some(item), Some(account), Some(next)) = (a, item, account, steps.get(i)) else {
        return settle(db, run.id, "skipped", Some(CHANGED.into()), &results).await;
    };
    // Parked for a reply and no reply came: it timed out. Else the answer is where DMs go now.
    let reply = match (run.awaiting_reply, run.reply_item_id) {
        (true, None) => {
            return settle(db, run.id, "skipped", Some("no_reply".into()), &results).await;
        }
        (_, Some(id)) => match inbox_items::Entity::find_by_id(id).one(db).await? {
            Some(reply) => Some(reply),
            // The answer was deleted: a DM would go to the comment, a second private reply.
            None => {
                return settle(db, run.id, "skipped", Some("reply_gone".into()), &results).await;
            }
        },
        _ => None,
    };
    let holds = steps
        .iter()
        .any(|s| s.sends().is_some_and(|s| s.1.contains("{page}")));
    let page = match item.post_id {
        Some(post) if holds => pages::address(db, &app.web_url, item.team_id, post).await?,
        _ => None,
    };
    // A paused team's waiting runs end here: nothing goes out while it is paused. They sent
    // before they waited, so unlike a run held back at the start they count as having run.
    if team_paused(db, a.team_id).await? {
        let why = Some("paused_waiting".into());
        return settle(db, run.id, "skipped", why, &results).await;
    }
    // Also where a run ends whose automation was switched off or given other steps since.
    if !claim_step(db, run.id, &a, i, next.outward(), &results).await? {
        return Ok(());
    }
    let flow = Flow {
        app,
        account: &account,
        item: &item,
        a: &a,
        steps: &steps,
        page: page.as_deref(),
        reply: reply.as_ref(),
    };
    flow.advance(run.id, i, results).await
}

/// Inserts the run and, when it `sends` first, reserves its hit of the daily cap in one
/// transaction. `None` when the item already has a run of this automation, or the cap is
/// reached (the run is then stored `skipped: daily_cap`). A `flow`'s run starts at step 0; a
/// send there is what the hit pays for, and a later one takes its own (`claim_step`).
async fn claim(
    db: &DatabaseConnection,
    a: &automations::Model,
    item: &inbox_items::Model,
    flow: bool,
    sends: bool,
    first_per_author: bool,
) -> Result<Option<Uuid>, DbErr> {
    let txn = db.begin().await?;
    let id = Uuid::now_v7();
    // Two items by one author run at once (the round and the sweep, two instances): the check
    // `run_matching` made is taken again under a lock on the automation and author, so only one
    // becomes the author's first.
    if first_per_author {
        let held = sql(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text || ':' || $2, 0))",
            vec![a.id.into(), item.author_id.clone().into()],
        );
        txn.execute_raw(held).await?;
        let taken = sql(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM automation_runs r \
                 JOIN inbox_items i ON i.id = r.inbox_item_id \
                 WHERE r.automation_id = $1 AND i.author_id = $2 AND i.id <> $3 \
                 AND NOT {NOT_ACTED}) AS taken"
            ),
            vec![a.id.into(), item.author_id.clone().into(), item.id.into()],
        );
        let taken: bool = match txn.query_one_raw(taken).await? {
            Some(r) => r.try_get("", "taken")?,
            None => false,
        };
        if taken {
            return Ok(None);
        }
    }
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
    if let Some(why) = limited(&txn, a, item, id).await? {
        let stmt = sql(
            "UPDATE automation_runs SET status = 'skipped', reason = $2 WHERE id = $1",
            vec![id.into(), why.into()],
        );
        txn.execute_raw(stmt).await?;
        txn.commit().await?;
        return Ok(None);
    }
    let cap = [(quota_bucket(a.team_id), RUNS_PER_DAY)];
    if sends && reserve_in(&txn, &cap).await?.is_none() {
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

/// Why run `id` of `a` (just inserted, in `txn`) must not act: `cooldown` when the automation
/// already ran for the same author within its `cooldown_minutes`, `automation_cap` when it already
/// ran `daily_cap` times in 24 hours. Each is checked under a lock, so two items at once cannot
/// both get past it.
async fn limited(
    txn: &impl ConnectionTrait,
    a: &automations::Model,
    item: &inbox_items::Model,
    id: Uuid,
) -> Result<Option<&'static str>, DbErr> {
    let exists = |sql_text: &str, values: Vec<Value>| {
        let stmt = sql(sql_text, values);
        async move {
            let row = txn.query_one_raw(stmt).await?;
            row.map_or(Ok(false), |r| r.try_get("", "hit"))
        }
    };
    if let Some(minutes) = a.cooldown_minutes {
        let lock = sql(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text || ':' || $2, 0))",
            vec![a.id.into(), item.author_id.clone().into()],
        );
        txn.execute_raw(lock).await?;
        let hit = exists(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM automation_runs r \
                 JOIN inbox_items i ON i.id = r.inbox_item_id \
                 WHERE r.automation_id = $1 AND i.author_id = $2 AND r.id <> $3 \
                 AND r.created_at > now() - make_interval(mins => $4) \
                 AND NOT {NOT_ACTED}) AS hit"
            ),
            vec![
                a.id.into(),
                item.author_id.clone().into(),
                id.into(),
                (minutes).into(),
            ],
        )
        .await?;
        if hit {
            return Ok(Some("cooldown"));
        }
    }
    if let Some(cap) = a.daily_cap {
        let lock = sql(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 1))",
            vec![a.id.into()],
        );
        txn.execute_raw(lock).await?;
        let hit = exists(
            &format!(
                "SELECT count(*) >= $3 AS hit FROM automation_runs r \
                 WHERE r.automation_id = $1 AND r.id <> $2 \
                 AND r.created_at > now() - interval '1 day' AND NOT {NOT_ACTED}"
            ),
            vec![a.id.into(), id.into(), i64::from(cap).into()],
        )
        .await?;
        if hit {
            return Ok(Some("automation_cap"));
        }
    }
    Ok(None)
}

/// One action's outcome, as stored in a run's `results`.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq, Default)]
pub struct ActionResult {
    /// `send_dm`, `reply_publicly`, `ai_send_dm`, `ai_reply_publicly`, `fallback_public_reply`
    /// or `tag_contact`. A flow's run
    /// holds one per send and tag step tried, in order; waits leave none.
    #[serde(rename = "type")]
    pub kind: String,
    /// `sent`, `failed` or `skipped`; a `tag_contact` is `done` when the tag was added and
    /// `skipped` (`already_tagged`, `tag_limit`) when there was nothing to add.
    pub status: String,
    /// Why it was not sent, e.g. `dm_not_allowed`, `no_dm_access`, `rate_limited`;
    /// `no_public_page` for a fallback reply holding `{page}` when the post has no live page; on Facebook
    /// and Instagram `outside_window` (a private reply works within 7 days of the comment),
    /// `already_replied` (once per comment) and `dm_unavailable`. For an AI send also
    /// `no_ai_key` (no team key and none of ours), `no_ai_credits` (the monthly allowance and the
    /// `ai` credits are used up), `ai_failed` (the provider gave no reply), `ai_empty`,
    /// `ai_too_long`, `ai_link` (a link, domain or @handle the prompt does not hold) and
    /// `ai_leak` (it repeats the prompt).
    pub reason: Option<String>,
    pub remote_id: Option<String>,
    /// An AI send: `anthropic` or `openai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// An AI send: the model that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// An AI step whose provider did not answer (timeout, 429, 5xx): nothing was sent and the
    /// credit is back, so a flow may try the step again (`advance`). Never stored or shown.
    #[serde(skip)]
    #[schema(ignore)]
    pub unavailable: bool,
}

fn outcome(kind: &str, sent: Result<Result<String, InboxError>, DbErr>) -> ActionResult {
    let (status, reason, remote_id) = match sent {
        Ok(Ok(id)) => ("sent", None, Some(id)),
        Ok(Err(
            e @ (InboxError::DmNotAllowed
            | InboxError::NoDmAccess
            | InboxError::RateLimited
            | InboxError::Invalid(
                "outside_window"
                | "already_replied"
                | "dm_unavailable"
                | "threads_no_dm"
                | "threads_replies_closed"
                | "youtube_no_dm"
                | "linkedin_no_dm"
                | "linkedin_needs_approval"
                | "linkedin_personal_profile"
                | "linkedin_needs_scope"
                | "linkedin_call_budget"
                | "x_no_credits"
                | "x_usage_capped"
                | "x_budget",
            )),
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
        ..Default::default()
    }
}

/// `template` with each `{page}` replaced by the page's address; `None` when it holds one and
/// there is no page.
pub(crate) fn with_page(template: &str, page: Option<&str>) -> Option<String> {
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
        ..Default::default()
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
    run: Uuid,
) -> Vec<ActionResult> {
    let name = item.author_name.as_deref();
    let fallback = a.fallback_public_reply.as_deref();
    let mut results = Vec::new();
    for action in actions {
        let Some((via, template)) = action.via_text() else {
            results.push(effect(app, account, item, a, action).await);
            continue;
        };
        if action.ai() {
            let (kind, reference) = (action.kind(), format!("ai:{run}:{}", action.kind()));
            results.push(ai_send(app, account, item, a.id, kind, via, template, &reference).await);
            continue;
        }
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
        matches!(r.kind.as_str(), "send_dm" | "ai_send_dm")
            && matches!(
                r.reason.as_deref(),
                Some(
                    "dm_not_allowed"
                        | "no_dm_access"
                        | "outside_window"
                        | "already_replied"
                        | "dm_unavailable"
                        | "threads_no_dm"
                        | "youtube_no_dm"
                        | "linkedin_no_dm"
                )
            )
    });
    let replied = results.iter().any(|r| {
        matches!(r.kind.as_str(), "reply_publicly" | "ai_reply_publicly") && r.status == "sent"
    });
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

/// What an AI step wrote: the text, the provider and the model.
pub(crate) struct Written {
    pub text: String,
    pub provider: String,
    pub model: Option<String>,
}

/// Whether the item's text asks a question: yes when it holds a question mark (no AI call), else
/// judged by an AI model with the same key choice and payment as an AI reply, once per
/// automation and item (`ai:q:...`, so a sweep never pays twice). `Err` is the reason the run is
/// stored `skipped`: `no_ai_key`, `no_ai_credits` or `ai_failed`.
pub(crate) async fn asks_question(
    app: &AppState,
    item: &inbox_items::Model,
    automation: Uuid,
) -> Result<bool, &'static str> {
    if item.text.contains(['?', '\u{ff1f}', '\u{61f}', '\u{bf}']) {
        return Ok(true);
    }
    if item.text.trim().is_empty() {
        return Ok(false);
    }
    // Judging costs, and a stranger writes as much as they like: the team judges so many a day.
    let cap = [(
        format!("automation-judge:team:{}", item.team_id),
        JUDGES_PER_DAY,
    )];
    match crate::rate_limit::daily_reserve(&app.db, &cap).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err("question_cap"),
        Err(e) => {
            tracing::error!(error = %e, item = %item.id, "question cap not read");
            return Err("ai_failed");
        }
    }
    let reference = format!("ai:q:{automation}:{}", item.id);
    let access = ai_pay(app, item, &reference).await;
    let AiAccess {
        provider,
        key,
        paid,
    } = access.map_err(|(_, reason)| reason)?;
    let (system, user) = ai::question_prompt(&item.text);
    let answer = app.ai.write(&provider, &key, &system, &user).await;
    // The first word, whatever dresses it: "Yes.", "**no**", "yes, because".
    let word = answer.as_ref().ok().and_then(|(a, _)| {
        let first = a
            .split(|c: char| !c.is_alphabetic())
            .find(|w| !w.is_empty());
        first.map(str::to_lowercase)
    });
    match word.as_deref() {
        Some("yes") => Ok(true),
        Some("no") => Ok(false),
        _ => {
            tracing::warn!(status = ?answer.err(), %provider, item = %item.id, "question not judged");
            ai_refund(app, item, paid, &reference, "ai question unjudged").await;
            Err("ai_failed")
        }
    }
}

/// Questions judged by AI, a team, a day.
const JUDGES_PER_DAY: i64 = 300;

/// Who pays for one AI call and with which key: the team's own key, free, else ours, which with
/// billing on is paid under `reference` from the plan's monthly allowance, then an `ai` credit.
struct AiAccess {
    provider: String,
    key: String,
    /// What was paid, to give back when nothing usable came of the call.
    paid: Option<billing::Kind>,
}

/// Picks the key and pays for the call before it is made. `Err` is the (status, reason) of the
/// step that ends there: `skipped` `no_ai_key` or `no_ai_credits`, `failed` `ai_failed` (the
/// reference was given back before) or `internal`.
async fn ai_pay(
    app: &AppState,
    item: &inbox_items::Model,
    reference: &str,
) -> Result<AiAccess, (&'static str, &'static str)> {
    let team_key = match key_of(app, item.team_id).await {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, item = %item.id, "ai key unreadable");
            return Err(("failed", "internal"));
        }
    };
    let metered = team_key.is_none() && app.billing_enabled;
    let ours = || app.ai.own_key.clone().map(|k| ("anthropic".to_owned(), k));
    let Some((provider, key)) = team_key.or_else(ours) else {
        return Err(("skipped", "no_ai_key"));
    };
    let mut paid = None;
    if metered {
        let monthly = billing::limits_of(true, &app.db, item.team_id)
            .await
            .map(|l| l.ai_monthly.unwrap_or(0));
        let spent = match monthly {
            Ok(monthly) => {
                billing::spend_ai(
                    &app.db,
                    item.team_id,
                    monthly,
                    reference,
                    chrono::Utc::now(),
                )
                .await
            }
            Err(e) => Err(e),
        };
        match spent {
            Ok(AiSpent::Paid(kind)) => paid = Some(kind),
            Ok(AiSpent::Short) => return Err(("skipped", "no_ai_credits")),
            Ok(AiSpent::Refunded) => return Err(("failed", "ai_failed")),
            Err(e) => {
                tracing::error!(error = %e, item = %item.id, "ai reply not paid");
                return Err(("failed", "internal"));
            }
        }
    }
    Ok(AiAccess {
        provider,
        key,
        paid,
    })
}

/// Gives back what `ai_pay` took, when nothing usable came of the call.
async fn ai_refund(
    app: &AppState,
    item: &inbox_items::Model,
    paid: Option<billing::Kind>,
    reference: &str,
    why: &str,
) {
    if let Some(kind) = paid {
        let back = billing::grant(&app.db, item.team_id, kind, 1, why, Some(reference));
        if let Err(e) = back.await {
            tracing::error!(error = %e, item = %item.id, "ai reply not given back");
        }
    }
}

/// Writes a reply with AI from the owner's `prompt` and the item's text, as `kind`. The team's
/// own key goes first, free and unmetered; else ours, which with billing on is paid under
/// `reference` from the plan's monthly allowance, then an `ai` credit (`billing::spend_ai`),
/// and given back when nothing usable came of the call. `Err` holds the result of a step that
/// ends there: `skipped` with `no_ai_key` (no key at all) or `no_ai_credits`; `failed` with
/// `ai_failed`, `ai_empty`, `ai_too_long`, `ai_link` or `ai_leak` (it repeats the prompt). A
/// reference given back before is never paid or written again (`ai_failed`). Neither key nor
/// prompt is logged.
#[allow(clippy::result_large_err)] // the step's own result, used once
pub(crate) async fn ai_write(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    (kind, via, prompt): (&str, Via, &str),
    reference: &str,
) -> Result<Written, ActionResult> {
    let ended = |status: &str, reason: &str| ActionResult {
        kind: kind.into(),
        status: status.into(),
        reason: Some(reason.into()),
        ..Default::default()
    };
    let AiAccess {
        provider,
        key,
        paid,
    } = match ai_pay(app, item, reference).await {
        Ok(a) => a,
        Err((status, reason)) => return Err(ended(status, reason)),
    };
    let public = via == Via::Public;
    let limit = ai::budget(&account.provider, via);
    let name = safe_name(item.author_name.as_deref());
    // A message of only a photo or sticker: the AI is told what came, not handed nothing.
    let said = if item.text.trim().is_empty() {
        let kinds: Vec<inbox::Attachment> =
            serde_json::from_value(item.attachments.clone()).unwrap_or_default();
        match kinds.first().map(|k| k.kind.as_str()) {
            Some(kind) => format!("[{kind}]"),
            None => String::new(),
        }
    } else {
        item.text.clone()
    };
    let (system, user) = ai::prompt(
        prompt,
        &said,
        name.as_deref(),
        &account.provider,
        public,
        limit,
    );
    let mut model = None;
    let mut unavailable = false;
    let text = match app.ai.write(&provider, &key, &system, &user).await {
        Err(status) => {
            tracing::warn!(?status, %provider, item = %item.id, "ai reply not written");
            // No answer, a rate limit, a server error or an overloaded model: it may be fine soon.
            unavailable = matches!(status, None | Some(408 | 425 | 429 | 500..=599));
            Err("ai_failed")
        }
        Ok((text, m)) => {
            model = Some(m.to_owned());
            let text = text.trim().to_owned();
            if text.is_empty() {
                Err("ai_empty")
            } else if ai::invents_link(&text, prompt) {
                Err("ai_link")
            } else if ai::repeats_prompt(&text, prompt) {
                Err("ai_leak")
            } else if !inbox::fits_for(account, via, &text) {
                Err("ai_too_long")
            } else {
                Ok(text)
            }
        }
    };
    match text {
        Ok(text) => Ok(Written {
            text,
            provider,
            model,
        }),
        Err(reason) => {
            ai_refund(app, item, paid, reference, "ai reply unsent").await;
            Err(ActionResult {
                provider: Some(provider),
                model,
                unavailable,
                ..ended("failed", reason)
            })
        }
    }
}

/// Writes a reply with AI (`ai_write`) and sends it as `kind`, once.
#[allow(clippy::too_many_arguments)]
async fn ai_send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    automation: Uuid,
    kind: &str,
    via: Via,
    prompt: &str,
    reference: &str,
) -> ActionResult {
    // YouTube and LinkedIn have no DM: skip before the AI call is paid for.
    if via == Via::Dm && matches!(account.provider.as_str(), "youtube" | "linkedin") {
        let why = match account.provider.as_str() {
            "youtube" => "youtube_no_dm",
            _ => "linkedin_no_dm",
        };
        return outcome(kind, Ok(Err(InboxError::Invalid(why))));
    }
    // X stops an automation that cannot pay a write before the model is paid for a text.
    if account.provider == "x" {
        let stopped = match crate::x_automation::blocked(app, account).await {
            Ok(why) => why,
            Err(e) => {
                tracing::error!(error = %e, item = %item.id, "x budget not read");
                Some("internal")
            }
        };
        if let Some(why) = stopped {
            let status = if why == "internal" {
                "failed"
            } else {
                "skipped"
            };
            return ActionResult {
                kind: kind.into(),
                status: status.into(),
                reason: Some(why.into()),
                ..Default::default()
            };
        }
    }
    let written = match ai_write(app, account, item, (kind, via, prompt), reference).await {
        Ok(w) => w,
        Err(ended) => return ended,
    };
    let from = (None, Some(automation));
    let sent = inbox::send_with(app, account, item, via, &written.text, &[], from, &[]).await;
    ActionResult {
        provider: Some(written.provider),
        model: written.model,
        ..outcome(
            kind,
            sent.map(|r| r.map(|m| m.remote_id.unwrap_or_default())),
        )
    }
}

/// The run's status from its sends (a `tag_contact` result says nothing of them): `sent` when
/// anything went, else `failed` when anything failed, else `skipped`; the reason is the first
/// one given.
pub fn summary(results: &[ActionResult]) -> (&'static str, Option<String>) {
    // A tag and `stop_automations` say nothing of what was done; a mail, an assignment or a
    // moderation call that was `done` counts as acting.
    let sends = || {
        results
            .iter()
            .filter(|r| !matches!(r.kind.as_str(), "tag_contact" | "stop_automations"))
    };
    let status = if sends().any(|r| matches!(r.status.as_str(), "sent" | "done")) {
        "sent"
    } else if sends().any(|r| r.status == "failed") {
        "failed"
    } else {
        "skipped"
    };
    let reason = sends().find_map(|r| r.reason.clone());
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
            now: chrono::Utc::now(),
        }
    }

    #[test]
    fn hours_and_weekdays_are_judged_in_the_zone() {
        use chrono::TimeZone;
        // 2026-11-23 is a Monday.
        let at = |h| {
            chrono::Utc
                .with_ymd_and_hms(2026, 11, 23, h, 30, 0)
                .unwrap()
        };
        let office = When {
            zone: "Asia/Dhaka".into(),
            weekdays: vec![0, 1],
            from_hour: Some(9),
            to_hour: Some(17),
        };
        assert!(office.valid());
        assert_eq!(office.mismatch(at(5)), None, "11:30 in Dhaka");
        assert_eq!(office.mismatch(at(12)), Some("outside_hours"));
        let tuesday = When {
            weekdays: vec![1],
            ..office.clone()
        };
        assert_eq!(tuesday.mismatch(at(5)), Some("wrong_weekday"));
        let night = When {
            zone: "UTC".into(),
            weekdays: vec![],
            from_hour: Some(22),
            to_hour: Some(6),
        };
        assert_eq!(night.mismatch(at(23)), None);
        assert_eq!(night.mismatch(at(5)), None);
        assert_eq!(night.mismatch(at(6)), Some("outside_hours"));
        // Friday 22 to 6 (2026-11-27 is a Friday) runs into Saturday morning, which is still
        // Friday's night, and not into Friday's own small hours.
        let friday_night = When {
            weekdays: vec![4],
            ..night.clone()
        };
        let sat = |h| chrono::Utc.with_ymd_and_hms(2026, 11, 28, h, 0, 0).unwrap();
        let fri = |h| chrono::Utc.with_ymd_and_hms(2026, 11, 27, h, 0, 0).unwrap();
        assert_eq!(friday_night.mismatch(fri(23)), None);
        assert_eq!(friday_night.mismatch(sat(3)), None);
        assert_eq!(friday_night.mismatch(fri(3)), Some("wrong_weekday"));
        assert_eq!(friday_night.mismatch(sat(23)), Some("wrong_weekday"));
        let all_day = When {
            from_hour: None,
            to_hour: None,
            weekdays: vec![0],
            ..night.clone()
        };
        assert_eq!(all_day.mismatch(at(12)), None);
        for bad in [
            When {
                zone: "Nope".into(),
                ..night.clone()
            },
            When {
                to_hour: None,
                ..night.clone()
            },
            When {
                to_hour: Some(22),
                ..night.clone()
            },
            When {
                to_hour: Some(25),
                ..night.clone()
            },
            When {
                from_hour: None,
                to_hour: None,
                ..night.clone()
            },
        ] {
            assert!(!bad.valid(), "{bad:?}");
        }
    }

    #[test]
    fn a_language_is_known_only_when_the_text_is_clear() {
        let en = "I would like to know where I can buy your new product in my town";
        assert_eq!(language_of(en), Some("en"));
        assert_eq!(
            language_of("আমি আপনার নতুন পণ্যটি কোথায় কিনতে পারি তা জানতে চাই"),
            Some("bn")
        );
        assert_eq!(language_of("Wo kann ich das kaufen?"), Some("de"));
        assert_eq!(language_of(""), None);
        // Too few letters, or only handles and links, say nothing.
        assert_eq!(language_of("ok thanks lol"), None);
        assert_eq!(
            language_of(&format!("@someone https://x.com/a/b {en}")),
            Some("en")
        );
        assert_eq!(
            language_of("@someone @another https://example.com/page"),
            None
        );
        let c = Conditions {
            languages: vec!["bn".into()],
            ..Default::default()
        };
        assert_eq!(mismatches(&c, &facts(&[], en)), vec!["wrong_language"]);
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
    fn a_dm_runs_the_most_specific_trigger_it_is() {
        let now = chrono::Utc::now().fixed_offset();
        let dm = |button: Option<&str>, story_reply: bool, mention: bool| inbox_items::Model {
            id: Uuid::now_v7(),
            number: 0,
            team_id: Uuid::nil(),
            social_account_id: Uuid::nil(),
            platform: "instagram".into(),
            kind: "dm".into(),
            remote_id: "m".into(),
            remote_cid: None,
            parent_remote_id: None,
            root_remote_id: None,
            root_remote_cid: None,
            convo_id: None,
            post_id: None,
            author_id: "a".into(),
            author_handle: None,
            author_name: None,
            author_avatar: None,
            text: String::new(),
            received_at: now,
            state: "open".into(),
            replied_at: None,
            permalink: None,
            story_reply,
            story_id: None,
            story_url: None,
            button_payload: button.map(str::to_owned),
            contact_id: None,
            assigned_to: None,
            automated_at: None,
            attachments: if mention {
                serde_json::json!([{"kind": "story_mention", "url": "https://x.example/s"}])
            } else {
                serde_json::json!([])
            },
            moderation: serde_json::json!({}),
            created_at: now,
        };
        assert_eq!(
            trigger_of(&dm(Some("yes"), true, true)),
            Some("button_clicked")
        );
        assert_eq!(trigger_of(&dm(None, true, true)), Some("story_reply"));
        assert_eq!(trigger_of(&dm(None, false, true)), Some("story_mention"));
        assert_eq!(trigger_of(&dm(None, false, false)), Some("dm_received"));
        // Only Instagram sends a story mention.
        let facebook = inbox_items::Model {
            platform: "facebook".into(),
            ..dm(None, false, true)
        };
        assert_eq!(trigger_of(&facebook), Some("dm_received"));
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
        for c in [post, author.clone()] {
            assert_eq!(ok("comment_received", c.clone(), &[dm()], None), Ok(()));
        }
        // A mention takes "first mention per person" but has no post to be under.
        assert_eq!(ok("mention_received", author, &[dm()], None), Ok(()));
        assert_eq!(
            ok(
                "mention_received",
                Conditions {
                    post_remote_ids: vec!["p".into()],
                    ..Conditions::default()
                },
                &[dm()],
                None
            ),
            Err("invalid_conditions")
        );
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
        // Tags ride along with a send: up to 3, each once, written as contacts keep them.
        let tag = |t: &str| -> Action {
            serde_json::from_value(serde_json::json!({"type": "tag_contact", "tag": t})).unwrap()
        };
        assert_eq!(tag("  VIP "), Action::TagContact { tag: "vip".into() });
        // Every trigger with a person behind it; a like or repost has no contact to tag.
        for t in TRIGGERS.into_iter().filter(|t| !activity(t)) {
            let mut c = Conditions {
                payloads: vec!["yes".into()],
                ..Conditions::default()
            };
            let c = if t == "button_clicked" {
                &mut c
            } else {
                &mut Conditions::default()
            };
            assert_eq!(
                validate(t, c, &[tag("a"), dm(), tag("b"), tag("c")], None),
                Ok(()),
                "{t}"
            );
        }
        for bad in [
            vec![tag("a")],
            vec![dm(), tag("a"), tag("a")],
            vec![dm(), tag("a"), tag("b"), tag("c"), tag("d")],
            vec![dm(), tag("a,b")],
            vec![dm(), tag(" ")],
            vec![dm(), tag(&"a".repeat(31))],
        ] {
            assert_eq!(
                validate("comment_received", &mut c, &bad, None),
                Err("invalid_actions"),
                "{bad:?}"
            );
        }
        let mut steps: Vec<Step> = serde_json::from_value(serde_json::json!([
            {"type": "tag_contact", "tag": " Lead "},
            {"type": "send_dm", "text": "Hi"},
            {"type": "wait", "minutes": 5},
            {"type": "tag_contact", "tag": "waited"}]))
        .unwrap();
        assert_eq!(steps[0], Step::TagContact { tag: "lead".into() });
        assert_eq!(validate_steps("new_follower", &mut c, &mut steps), Ok(()));
        steps.remove(1);
        assert_eq!(
            validate_steps("new_follower", &mut c, &mut steps),
            Err("invalid_steps"),
            "a flow needs a send"
        );
        let mut bad = vec![
            Step::TagContact { tag: "a,b".into() },
            Step::SendDm {
                text: "x".into(),
                buttons: vec![],
            },
        ];
        assert_eq!(
            validate_steps("dm_received", &mut c, &mut bad),
            Err("invalid_steps")
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
            ..Default::default()
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
        // A tag that was added does not make a run whose DM never went `sent`.
        let tagged = [
            r("tag_contact", "done", None),
            r("send_dm", "skipped", Some("no_dm_access")),
        ];
        assert_eq!(summary(&tagged), ("skipped", Some("no_dm_access".into())));
    }

    #[test]
    fn threads_takes_public_replies_to_replies_and_mentions_only() {
        let reply = |t: &str| Action::ReplyPublicly { text: t.into() };
        let dm = Action::SendDm { text: "hi".into() };
        let ok = ["comment_received", "mention_received"];
        for trigger in ok {
            assert_eq!(
                network_limits(trigger, &["threads"], &[reply("hi")], None),
                Ok(())
            );
            // No DM and no private reply on Threads, in actions or in a flow.
            assert_eq!(
                network_limits(trigger, &["threads"], std::slice::from_ref(&dm), None),
                Err("threads_no_dm")
            );
            let flow = [Step::SendDm {
                text: "hi".into(),
                buttons: vec![],
            }];
            assert_eq!(
                network_limits(trigger, &["threads"], &[], Some(&flow)),
                Err("threads_no_dm")
            );
        }
        for trigger in [
            "dm_received",
            "new_follower",
            "story_reply",
            "button_clicked",
        ] {
            assert_eq!(
                network_limits(trigger, &["threads"], &[], None),
                Err("threads_unsupported_trigger")
            );
        }
        // 500 UTF-8 bytes, a name at 4 bytes a letter: 126 emoji are over, and so is a long name.
        assert_eq!(
            network_limits(
                "comment_received",
                &["threads"],
                &[reply(&"😀".repeat(126))],
                None
            ),
            Err("invalid_text")
        );
        assert_eq!(
            network_limits(
                "comment_received",
                &["threads"],
                &[reply(&"a".repeat(500))],
                None
            ),
            Ok(())
        );
        // Mixed with another network, each item is judged when it comes.
        assert_eq!(
            network_limits("comment_received", &["threads", "bluesky"], &[dm], None),
            Ok(())
        );
        // A DM to a Threads item is skipped, not failed, so a fallback reply answers.
        let sent: Result<Result<String, InboxError>, DbErr> =
            Ok(Err(InboxError::Invalid("threads_no_dm")));
        assert_eq!(outcome("send_dm", sent).status, "skipped");
    }

    #[test]
    fn likes_and_reposts_are_triggers_with_nobody_to_message() {
        let now = chrono::Utc::now().fixed_offset();
        let item = |kind: &str| inbox_items::Model {
            id: Uuid::now_v7(),
            number: 0,
            team_id: Uuid::nil(),
            social_account_id: Uuid::nil(),
            platform: "bluesky".into(),
            kind: kind.into(),
            remote_id: "r".into(),
            remote_cid: None,
            parent_remote_id: None,
            root_remote_id: None,
            root_remote_cid: None,
            convo_id: None,
            post_id: None,
            author_id: "a".into(),
            author_handle: None,
            author_name: None,
            author_avatar: None,
            text: String::new(),
            received_at: now,
            state: "done".into(),
            replied_at: None,
            permalink: None,
            story_reply: false,
            story_id: None,
            story_url: None,
            button_payload: None,
            contact_id: None,
            assigned_to: None,
            automated_at: None,
            attachments: serde_json::json!([]),
            moderation: serde_json::json!({}),
            created_at: now,
        };
        assert_eq!(trigger_of(&item("like")), Some("like_received"));
        assert_eq!(trigger_of(&item("repost")), Some("repost_received"));
        let ok = |trigger: &str, actions: Vec<Action>| {
            validate(trigger, &mut Conditions::default(), &actions, None)
        };
        assert_eq!(ok("like_received", vec![Action::FollowBack]), Ok(()));
        assert_eq!(
            ok(
                "repost_received",
                vec![Action::FollowBack, Action::NotifyTeam]
            ),
            Ok(())
        );
        assert_eq!(ok("comment_received", vec![Action::RepostComment]), Ok(()));
        assert_eq!(ok("mention_received", vec![Action::LikeComment]), Ok(()));
        for (trigger, actions) in [
            // A like or repost has no post of its own to like or repost.
            ("like_received", vec![Action::LikeComment]),
            ("new_follower", vec![Action::RepostComment]),
            ("like_received", vec![Action::StopAutomations]),
            (
                "like_received",
                vec![Action::FollowBack, Action::FollowBack],
            ),
            // Nobody to message or tag behind a like: it would repeat for every one.
            ("like_received", vec![Action::SendDm { text: "hi".into() }]),
            (
                "repost_received",
                vec![Action::FollowBack, Action::TagContact { tag: "a".into() }],
            ),
        ] {
            assert_eq!(
                ok(trigger, actions.clone()),
                Err("invalid_actions"),
                "{trigger} {actions:?}"
            );
        }
        // A like names a post but has no words or "first".
        let mut c = Conditions {
            post_remote_ids: vec!["at://x/app.bsky.feed.post/1".into()],
            ..Conditions::default()
        };
        assert_eq!(check_conditions("like_received", &mut c), Ok(()));
        assert_eq!(
            check_conditions("dm_received", &mut c),
            Err("invalid_conditions")
        );
        for bad in [
            Conditions {
                keywords: vec!["a".into()],
                ..Conditions::default()
            },
            Conditions {
                first_per_author: true,
                ..Conditions::default()
            },
            Conditions {
                question: true,
                ..Conditions::default()
            },
        ] {
            assert_eq!(
                check_conditions("repost_received", &mut bad.clone()),
                Err("invalid_conditions")
            );
        }
        let flow = |trigger: &str, steps: &mut [Step]| {
            validate_steps(trigger, &mut Conditions::default(), steps)
        };
        assert_eq!(
            flow("like_received", &mut [Step::FollowBack, Step::NotifyTeam]),
            Ok(())
        );
        assert_eq!(
            flow("like_received", &mut [Step::RepostComment]),
            Err("invalid_steps")
        );
        assert_eq!(
            flow(
                "like_received",
                &mut [Step::FollowBack, Step::TagContact { tag: "a".into() }]
            ),
            Err("invalid_steps")
        );
        assert_eq!(
            flow(
                "comment_received",
                &mut [
                    Step::LikeComment,
                    Step::Wait { minutes: 5 },
                    Step::FollowBack
                ]
            ),
            Ok(())
        );
        assert_eq!(
            flow(
                "comment_received",
                &mut [Step::FollowBack, Step::FollowBack]
            ),
            Err("invalid_steps")
        );
        assert!(Step::FollowBack.outward() && !Step::StopAutomations.outward());
        // Nothing was sent to wait for an answer to.
        assert_eq!(
            flow(
                "like_received",
                &mut [
                    Step::FollowBack,
                    Step::WaitForReply { minutes: 5 },
                    Step::NotifyTeam
                ]
            ),
            Err("invalid_steps")
        );
    }

    #[test]
    fn network_limits_keep_likes_and_bluesky_actions_on_bluesky() {
        let none: [Action; 0] = [];
        for trigger in ["like_received", "repost_received"] {
            assert_eq!(network_limits(trigger, &["bluesky"], &none, None), Ok(()));
            assert_eq!(network_limits(trigger, &[], &none, None), Ok(()));
            // A mixed scope is left to fail item by item, a scope with no Bluesky is refused.
            assert_eq!(
                network_limits(trigger, &["bluesky", "facebook"], &none, None),
                Ok(())
            );
            for no in ["facebook", "instagram", "x"] {
                assert_eq!(
                    network_limits(trigger, &[no], &none, None),
                    Err("bluesky_only_trigger"),
                    "{no}"
                );
            }
        }
        for no in ["facebook", "x"] {
            for a in [Action::FollowBack, Action::RepostComment] {
                // X reposts too (`x_react`); only Bluesky follows back.
                let want = if no == "x" && a == Action::RepostComment {
                    Ok(())
                } else {
                    Err("bluesky_only_action")
                };
                assert_eq!(network_limits("comment_received", &[no], &[a], None), want);
            }
            let steps = [Step::FollowBack];
            assert_eq!(
                network_limits("comment_received", &[no], &none, Some(&steps)),
                Err("bluesky_only_action")
            );
        }
        // Like is also Facebook's, and a mail or an assignment goes to nobody's servers.
        let local = [Action::LikeComment, Action::NotifyTeam];
        assert_eq!(
            network_limits("comment_received", &["facebook"], &local, None),
            Ok(())
        );
        assert_eq!(
            network_limits(
                "comment_received",
                &["bluesky"],
                &[Action::FollowBack],
                None
            ),
            Ok(())
        );
    }

    #[test]
    fn network_limits_follow_what_each_network_allows() {
        let reply = |t: &str| Step::ReplyPublicly { text: t.into() };
        let dm = |t: &str| Step::SendDm {
            text: t.into(),
            buttons: vec![],
        };
        let wait = |m: u32| Step::Wait { minutes: m };
        let two = [dm("hi"), wait(5), dm("again")];
        let none: [Action; 0] = [];
        // Networks with no inbox can never fire it.
        assert_eq!(
            network_limits("comment_received", &["tiktok"], &none, Some(&[dm("hi")])),
            Err("unsupported_account")
        );
        // X has comments, mentions and DMs; no follow, story or button triggers, and no
        // Meta windows.
        for ok in ["comment_received", "mention_received", "dm_received"] {
            assert_eq!(
                network_limits(ok, &["x"], &none, Some(&two)),
                Ok(()),
                "{ok}"
            );
        }
        for no in ["new_follower", "story_reply", "button_clicked"] {
            assert_eq!(
                network_limits(no, &["x"], &none, Some(&[dm("hi")])),
                Err("x_unsupported_trigger"),
                "{no}"
            );
        }
        assert_eq!(
            network_limits("new_follower", &["x", "bluesky"], &none, None),
            Ok(())
        );
        // YouTube has comments only, answered in public.
        assert_eq!(
            network_limits(
                "comment_received",
                &["youtube"],
                &none,
                Some(&[reply("hi")])
            ),
            Ok(())
        );
        for no in [
            "mention_received",
            "dm_received",
            "new_follower",
            "story_reply",
            "button_clicked",
        ] {
            assert_eq!(
                network_limits(no, &["youtube"], &none, None),
                Err("youtube_unsupported_trigger"),
                "{no}"
            );
        }
        assert_eq!(
            network_limits(
                "comment_received",
                &["youtube"],
                &none,
                Some(&[reply("hi"), dm("hi")])
            ),
            Err("youtube_no_dm")
        );
        assert_eq!(
            network_limits(
                "comment_received",
                &["youtube"],
                &[Action::SendDm { text: "hi".into() }],
                None
            ),
            Err("youtube_no_dm")
        );
        // LinkedIn has comments only, answered in public: no DM, no hiding.
        assert_eq!(
            network_limits(
                "comment_received",
                &["linkedin"],
                &none,
                Some(&[reply("hi")])
            ),
            Ok(())
        );
        for no in [
            "mention_received",
            "dm_received",
            "new_follower",
            "button_clicked",
        ] {
            assert_eq!(
                network_limits(no, &["linkedin"], &none, None),
                Err("linkedin_unsupported_trigger"),
                "{no}"
            );
        }
        assert_eq!(
            network_limits("comment_received", &["linkedin"], &none, Some(&[dm("hi")])),
            Err("linkedin_no_dm")
        );
        assert_eq!(
            network_limits(
                "comment_received",
                &["linkedin"],
                &[Action::SendDm { text: "hi".into() }],
                None
            ),
            Err("linkedin_no_dm")
        );
        assert_eq!(
            network_limits(
                "comment_received",
                &["linkedin"],
                &[Action::HideComment],
                None
            ),
            Err("linkedin_no_hide")
        );
        // A mixed scope is left to fail item by item.
        assert_eq!(
            network_limits("dm_received", &["linkedin", "bluesky"], &none, None),
            Ok(())
        );
        // Bluesky has no story replies or button taps; a follow, a comment, a mention and a DM
        // are all its own, and a mixed scope is left to fail item by item.
        for no in ["story_reply", "button_clicked"] {
            assert_eq!(
                network_limits(no, &["bluesky"], &none, Some(&[dm("hi")])),
                Err("bluesky_unsupported_trigger"),
                "{no}"
            );
        }
        for ok in [
            "comment_received",
            "mention_received",
            "dm_received",
            "new_follower",
        ] {
            assert_eq!(network_limits(ok, &["bluesky"], &none, Some(&two)), Ok(()));
        }
        assert_eq!(
            network_limits("story_reply", &["bluesky", "instagram"], &none, None),
            Ok(())
        );
        assert_eq!(
            network_limits(
                "dm_received",
                &["bluesky", "tiktok"],
                &none,
                Some(&[dm("hi")])
            ),
            Err("unsupported_account")
        );
        // A comment gets one private reply on Facebook and Instagram, any number on Bluesky,
        // and a mixed scope is left to fail item by item.
        assert_eq!(
            network_limits("comment_received", &["facebook"], &none, Some(&two)),
            Err("meta_one_private_reply")
        );
        assert_eq!(
            network_limits("comment_received", &["bluesky"], &none, Some(&two)),
            Ok(())
        );
        assert_eq!(
            network_limits(
                "comment_received",
                &["bluesky", "instagram"],
                &none,
                Some(&two)
            ),
            Ok(())
        );
        assert_eq!(
            network_limits("dm_received", &["facebook"], &none, Some(&two)),
            Ok(())
        );
        // A DM trigger's message must go within 24 hours of the person's.
        let late = [dm("hi"), wait(23 * 60), wait(60), dm("later")];
        assert_eq!(
            network_limits("dm_received", &["instagram"], &none, Some(&late)),
            Err("meta_reply_window")
        );
        let soon = [dm("hi"), wait(23 * 60), dm("later")];
        assert_eq!(
            network_limits("dm_received", &["instagram"], &none, Some(&soon)),
            Ok(())
        );
        // Instagram counts a DM in bytes: 400 Bengali letters are 1200.
        let bengali = "\u{0995}".repeat(400);
        assert_eq!(
            network_limits("dm_received", &["instagram"], &none, Some(&[dm(&bengali)])),
            Err("invalid_text")
        );
        assert_eq!(
            network_limits("dm_received", &["facebook"], &none, Some(&[dm(&bengali)])),
            Ok(())
        );
        // A private reply 7 days after the comment is too late.
        let week = [
            Step::ReplyPublicly { text: "hi".into() },
            wait(10080),
            dm("later"),
        ];
        assert_eq!(
            network_limits("comment_received", &["facebook"], &none, Some(&week)),
            Err("meta_reply_window")
        );
        // A {name} may be 64 letters of 4 bytes: 760 ASCII bytes plus it is over 1000.
        let named = format!("Hi {{name}} {}", "a".repeat(760));
        assert_eq!(
            network_limits("dm_received", &["instagram"], &none, Some(&[dm(&named)])),
            Err("invalid_text")
        );
        for trigger in ["button_clicked", "story_reply"] {
            assert_eq!(
                network_limits(trigger, &["instagram"], &none, Some(&late)),
                Err("meta_reply_window")
            );
        }
        let action = [Action::SendDm {
            text: bengali.clone(),
        }];
        assert_eq!(
            network_limits("comment_received", &["instagram"], &action, None),
            Err("invalid_text")
        );
    }
}
