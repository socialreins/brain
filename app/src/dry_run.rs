//! "Test this automation": what an automation would do with a sample item, without sending
//! anything and without storing an inbox item or a run. It uses the same matching
//! (`automations::mismatches`), network rules (`network_limits`), `{name}` and `{page}` filling
//! and AI step (`automations::ai_write`, the same key choice, allowance and checks) as a real
//! run. Only the send, the run row and the daily cap hit are left out. A sample is always from a
//! stranger: the guard against the team's own accounts is not part of it.
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    automations::{self as rules, Action, ActionResult, Conditions, Facts, Step, RUNS_PER_DAY},
    controllers::pages,
    inbox::{self, Button, Via},
    models::{inbox_items, social_accounts},
    state::AppState,
};

/// Tests a team may run per day; each may call the AI model once per AI step.
pub const TESTS_PER_DAY: i64 = 100;
/// Longest sample text, in characters.
pub const MAX_SAMPLE_CHARS: usize = 2000;

pub fn quota_bucket(team_id: Uuid) -> String {
    format!("automation_test:team:{team_id}")
}

/// The item to try: what a stranger would send to `account`.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    /// A connected account of the team that can run automations.
    pub account_id: Uuid,
    /// The comment, mention or message, 1 to 2000 characters.
    pub text: String,
    /// The author's display name, which `{name}` and AI steps use ("there" when absent).
    #[serde(default)]
    pub author_name: Option<String>,
    /// The post the sample is under (a Bluesky `at://` URI or a Facebook or Instagram post id):
    /// what the `post_remote_ids` condition looks at, and which public page `{page}` is.
    #[serde(default)]
    pub post_remote_id: Option<String>,
    /// For a `button_clicked` trigger: the payload of the button clicked.
    #[serde(default)]
    pub button_payload: Option<String>,
}

/// One action or step as it would go.
#[derive(Serialize, ToSchema, Debug, Default, PartialEq, Eq)]
pub struct PlannedStep {
    /// `send_dm`, `reply_publicly`, `ai_send_dm`, `ai_reply_publicly`, `fallback_public_reply`,
    /// `tag_contact` or `wait`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `would_send`, `would_skip`, `would_fail`, `would_tag`, `would_wait` or `not_reached` (an
    /// earlier send of a flow did not go out, so the run ends before it).
    pub status: String,
    /// Why it would not go out, as a run's result gives it: `no_dm_access`, `too_long`,
    /// `no_ai_key`, `no_ai_credits`, `ai_failed`, `ai_empty`, `ai_too_long`, `ai_link`,
    /// `ai_leak`, `no_public_page`.
    pub reason: Option<String>,
    /// The final text, with `{name}` and `{page}` filled in or written by AI.
    pub text: Option<String>,
    /// A flow's buttons under a DM.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<Button>,
    /// A `wait`: how long the run would stop.
    pub wait_minutes: Option<u32>,
    /// A `tag_contact`: the tag.
    pub tag: Option<String>,
    /// An AI step: `anthropic` or `openai`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// An AI step: the model that wrote the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub trigger: String,
    /// The sample meets every condition.
    pub matched: bool,
    /// Why not, when it does not: `wrong_set`, `wrong_account`, `wrong_post`, `no_keyword` or
    /// `button_not_named`. (A sample is always its author's first comment or message, so the
    /// first-only conditions pass.)
    pub mismatches: Vec<String>,
    /// Rules that would stop it: `no_public_page` (a text holds `{page}` and the post has no
    /// live page: the automation does not run, so there are no steps), and the Facebook and
    /// Instagram rules of `network_limits` that make a send fail (`meta_one_private_reply`,
    /// `meta_reply_window`; the steps are still listed, as a run would go as far as it can), and
    /// `paused` (the team paused its automations: no steps).
    pub blocks: Vec<String>,
    pub steps: Vec<PlannedStep>,
}

enum Part<'a> {
    Send {
        kind: &'static str,
        via: Via,
        template: &'a str,
        ai: bool,
        buttons: &'a [Button],
    },
    Tag(&'a str),
    Wait(u32),
}

/// The checked automation as the parts a run goes through, in order.
fn parts<'a>(actions: &'a [Action], steps: Option<&'a [Step]>) -> Vec<Part<'a>> {
    match steps {
        Some(steps) => steps
            .iter()
            .filter_map(|s| match (s.sends(), s) {
                (Some((via, template, buttons)), _) => Some(Part::Send {
                    kind: s.kind(),
                    via,
                    template,
                    ai: s.ai(),
                    buttons,
                }),
                (None, Step::Wait { minutes }) => Some(Part::Wait(*minutes)),
                (None, Step::TagContact { tag }) => Some(Part::Tag(tag)),
                (None, _) => None,
            })
            .collect(),
        None => actions
            .iter()
            .filter_map(|a| match (a.via_text(), a) {
                (Some((via, template)), _) => Some(Part::Send {
                    kind: a.kind(),
                    via,
                    template,
                    ai: a.ai(),
                    buttons: &[],
                }),
                (None, Action::TagContact { tag }) => Some(Part::Tag(tag)),
                (None, _) => None,
            })
            .collect(),
    }
}

fn step(kind: &str, status: &str) -> PlannedStep {
    PlannedStep {
        kind: kind.into(),
        status: status.into(),
        ..Default::default()
    }
}

/// An AI step's end (`ActionResult`) as a planned step.
fn ended(r: ActionResult) -> PlannedStep {
    let status = if r.status == "failed" {
        "would_fail"
    } else {
        "would_skip"
    };
    PlannedStep {
        reason: r.reason,
        provider: r.provider,
        model: r.model,
        ..step(&r.kind, status)
    }
}

/// The trigger's item: what the sample is, stored nowhere.
fn item_of(
    trigger: &str,
    account: &social_accounts::Model,
    s: &Sample,
    post_id: Option<Uuid>,
) -> inbox_items::Model {
    let kind = match trigger {
        "comment_received" => "comment",
        "mention_received" => "mention",
        "new_follower" => "follow",
        _ => "dm",
    };
    inbox_items::Model {
        id: Uuid::now_v7(),
        team_id: account.team_id,
        social_account_id: account.id,
        platform: account.provider.clone(),
        kind: kind.into(),
        remote_id: "sample".into(),
        remote_cid: None,
        parent_remote_id: s.post_remote_id.clone(),
        root_remote_id: s.post_remote_id.clone(),
        root_remote_cid: None,
        convo_id: None,
        post_id,
        author_id: "sample-author".into(),
        author_handle: None,
        author_name: s.author_name.clone().filter(|n| !n.trim().is_empty()),
        author_avatar: None,
        // A follow carries no text.
        text: if kind == "follow" {
            String::new()
        } else {
            s.text.clone()
        },
        received_at: chrono::Utc::now().fixed_offset(),
        state: "open".into(),
        replied_at: None,
        permalink: None,
        story_reply: trigger == "story_reply",
        story_id: None,
        story_url: None,
        button_payload: s
            .button_payload
            .clone()
            .filter(|_| trigger == "button_clicked"),
        contact_id: None,
        assigned_to: None,
        automated_at: None,
        attachments: serde_json::json!([]),
        moderation: serde_json::json!({}),
        created_at: chrono::Utc::now().fixed_offset(),
    }
}

/// How many more sends the team's daily cap takes; takes no hit.
async fn cap_room(db: &DatabaseConnection, team: Uuid) -> Result<i64, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT count(*) AS n FROM rate_limit_hits \
         WHERE bucket = $1 AND created_at > now() - interval '1 day'",
        [rules::quota_bucket(team).into()],
    );
    let n: i64 = match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "n")?,
        None => 0,
    };
    Ok((RUNS_PER_DAY - n).max(0))
}

/// The post of ours the sample hangs off, as ingest finds it.
async fn post_of(
    db: &DatabaseConnection,
    account: Uuid,
    remote: Option<&str>,
) -> Result<Option<Uuid>, DbErr> {
    let Some(remote) = remote else {
        return Ok(None);
    };
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT post_id FROM post_targets WHERE social_account_id = $1 AND remote_id = $2 \
         LIMIT 1",
        [account.into(), remote.to_owned().into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "post_id"),
        None => Ok(None),
    }
}

/// What the checked automation would do with the sample. Spends AI like a real run when a step
/// writes with AI and the run would reach it.
pub async fn test(
    app: &AppState,
    account: &social_accounts::Model,
    (trigger, cond, actions, steps, fallback): (
        &str,
        &Conditions,
        &[Action],
        Option<&[Step]>,
        Option<&str>,
    ),
    sample: &Sample,
) -> Result<Outcome, DbErr> {
    let db = &app.db;
    let post_id = post_of(db, account.id, sample.post_remote_id.as_deref()).await?;
    let item = item_of(trigger, account, sample, post_id);
    let sets = rules::live_set_ids(db, account.id).await?;
    let facts = Facts {
        account_id: account.id,
        account_sets: &sets,
        parent: item.parent_remote_id.as_deref(),
        root: item.root_remote_id.as_deref(),
        text: &item.text,
        first_from_author: true,
        first_message: true,
    };
    let mut mismatches = rules::mismatches(cond, &facts);
    if trigger == "button_clicked"
        && !item
            .button_payload
            .as_ref()
            .is_some_and(|p| cond.payloads.contains(p))
    {
        mismatches.push("button_not_named");
    }
    let mut out = Outcome {
        trigger: trigger.into(),
        matched: mismatches.is_empty(),
        mismatches: mismatches.into_iter().map(Into::into).collect(),
        blocks: Vec::new(),
        steps: Vec::new(),
    };
    if !out.matched {
        return Ok(out);
    }
    let provider = [account.provider.as_str()];
    // `invalid_text` here is an estimate for the longest possible name; the real text is checked below.
    match rules::network_limits(trigger, &provider, actions, steps) {
        Err(code) if code != "invalid_text" => out.blocks.push(code.into()),
        _ => {}
    }
    let parts = parts(actions, steps);
    let holds = |t: &str| t.contains("{page}");
    let needs_page = parts
        .iter()
        .any(|p| matches!(p, Part::Send { template, .. } if holds(template)));
    let wants_page = needs_page || fallback.is_some_and(holds);
    let page = match item.post_id {
        Some(post) if wants_page => pages::address(db, &app.web_url, item.team_id, post).await?,
        _ => None,
    };
    if needs_page && page.is_none() {
        out.blocks.push("no_public_page".into());
    }
    if out.blocks.iter().any(|b| b == "no_public_page") {
        return Ok(out);
    }
    // A paused team sends nothing, so there is nothing to show beyond the reason.
    if rules::team_paused(db, item.team_id).await? {
        out.blocks.push("paused".into());
        return Ok(out);
    }
    let room = cap_room(db, item.team_id).await?;
    let walked = Walk {
        flow: steps.is_some(),
        fallback,
        page,
        room,
    };
    out.steps = walk(app, account, &item, &parts, walked).await;
    Ok(out)
}

/// Plays the parts as a run does: a flow stops at its first send that would not go out, a list
/// of actions tries each, then the fallback reply when a DM was refused and nothing replied.
struct Walk<'a> {
    flow: bool,
    fallback: Option<&'a str>,
    page: Option<String>,
    /// Sends the team's daily cap still takes; a send past it would be skipped `daily_cap`.
    room: i64,
}

async fn walk(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    parts: &[Part<'_>],
    Walk {
        flow,
        fallback,
        page,
        room,
    }: Walk<'_>,
) -> Vec<PlannedStep> {
    let mut sends = 0;
    let mut out: Vec<PlannedStep> = Vec::new();
    let mut stopped = false;
    for part in parts {
        if stopped {
            let (kind, tag, wait) = match part {
                Part::Send { kind, .. } => (*kind, None, None),
                Part::Tag(t) => ("tag_contact", Some(t.to_string()), None),
                Part::Wait(m) => ("wait", None, Some(*m)),
            };
            out.push(PlannedStep {
                tag,
                wait_minutes: wait,
                ..step(kind, "not_reached")
            });
            continue;
        }
        match part {
            Part::Tag(tag) => out.push(PlannedStep {
                tag: Some((*tag).to_owned()),
                ..step("tag_contact", "would_tag")
            }),
            Part::Wait(minutes) => out.push(PlannedStep {
                wait_minutes: Some(*minutes),
                ..step("wait", "would_wait")
            }),
            Part::Send {
                kind,
                via,
                template,
                ai,
                buttons,
            } => {
                sends += 1;
                let planned = if sends > room {
                    PlannedStep {
                        reason: Some("daily_cap".into()),
                        ..step(kind, "would_skip")
                    }
                } else {
                    let send = (*kind, *via, *template, *ai, *buttons);
                    plan_send(app, account, item, send, page.as_deref()).await
                };
                stopped = flow && planned.status != "would_send";
                out.push(planned);
            }
        }
    }
    let refused = |r: &PlannedStep| {
        matches!(r.kind.as_str(), "send_dm" | "ai_send_dm")
            && r.status != "would_send"
            && matches!(r.reason.as_deref(), Some("no_dm_access"))
    };
    let replied = |r: &PlannedStep| {
        matches!(r.kind.as_str(), "reply_publicly" | "ai_reply_publicly")
            && r.status == "would_send"
    };
    if let (false, Some(f), true, false) = (
        flow,
        fallback,
        out.iter().any(refused),
        out.iter().any(replied),
    ) {
        let kind = "fallback_public_reply";
        let planned = if sends + 1 > room {
            PlannedStep {
                reason: Some("daily_cap".into()),
                ..step(kind, "would_skip")
            }
        } else {
            let send = (kind, Via::Public, f, false, &[][..]);
            plan_send(app, account, item, send, page.as_deref()).await
        };
        out.push(planned);
    }
    out
}

/// One send: its final text, or why it would not go out. An AI step is not written when the
/// send could not go anyway (a run would spend the call first).
async fn plan_send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    (kind, via, template, ai, buttons): (&str, Via, &str, bool, &[Button]),
    page: Option<&str>,
) -> PlannedStep {
    let no_dm = via == Via::Dm && account.provider == "bluesky" && !account.dm_access;
    let skipped = |reason: &str| PlannedStep {
        reason: Some(reason.into()),
        ..step(kind, "would_skip")
    };
    if ai {
        if no_dm {
            return skipped("no_dm_access");
        }
        let reference = format!("ai:test:{}", Uuid::now_v7());
        return match rules::ai_write(app, account, item, (kind, via, template), &reference).await {
            Ok(w) => PlannedStep {
                text: Some(w.text),
                provider: Some(w.provider),
                model: w.model,
                ..step(kind, "would_send")
            },
            Err(r) => ended(r),
        };
    }
    let Some(template) = rules::with_page(template, page) else {
        return skipped("no_public_page");
    };
    let (text, _) = rules::render(&template, item.author_name.as_deref());
    // Facebook and Instagram send the buttons beside the text, Bluesky as numbered lines.
    let meta = matches!(account.provider.as_str(), "facebook" | "instagram");
    let sent = if meta {
        text.clone()
    } else {
        inbox::numbered(&text, buttons)
    };
    if !inbox::fits_on(&account.provider, via, &sent) {
        return PlannedStep {
            reason: Some("too_long".into()),
            text: Some(text),
            ..step(kind, "would_fail")
        };
    }
    if no_dm {
        return skipped("no_dm_access");
    }
    PlannedStep {
        text: Some(sent),
        buttons: if meta { buttons.to_vec() } else { Vec::new() },
        ..step(kind, "would_send")
    }
}
