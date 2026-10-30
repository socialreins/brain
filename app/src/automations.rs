//! Automations: a trigger, conditions and actions, stored as typed JSON. v1 has one trigger,
//! `comment_received`, run by the inbox tick on each new comment. A run row unique per
//! (automation, item) is inserted before anything is sent, together with a hit of the team's
//! daily cap, and a run is never retried, the same rules as publishing.
use sea_orm::{
    sea_query::Query, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, QueryFilter, QueryOrder, Statement, TransactionTrait, Value,
};
use std::ops::Range;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    connectors::bluesky::InboxError,
    controllers::connections::live_sets,
    inbox::{self, Via},
    models::{automations, inbox_items, social_account_social_set as membership, social_accounts},
    rate_limit::reserve_in,
    state::AppState,
};

pub const TRIGGERS: [&str; 1] = ["comment_received"];
/// Automation runs that act, per team per day (`rate_limit::reserve_in`).
pub const RUNS_PER_DAY: i64 = 500;
/// A run `running` this long was cut off (crash, restart); it fails and is not retried.
const STUCK_MINUTES: i32 = 15;
/// Longest `{name}` put into a text.
const MAX_NAME_CHARS: usize = 64;

pub fn quota_bucket(team_id: Uuid) -> String {
    format!("automation:team:{team_id}")
}

/// When an automation acts on a comment. Empty lists mean "any". At most one of
/// `social_set_id` and `account_ids`.
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
    /// Bluesky `at://` URIs; at most 100.
    #[serde(default)]
    pub post_remote_ids: Vec<String>,
    /// Any of these, case-insensitive, in the comment; at most 20 of 1 to 100 characters.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Only an author's first matching comment runs this automation.
    #[serde(default)]
    pub first_per_author: bool,
}

/// What an automation does. Texts may hold `{name}`: the author's display name, else handle.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// A DM to the comment's author, at most 1000 graphemes.
    SendDm { text: String },
    /// A reply under the comment, at most 300 graphemes.
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

/// Checks an automation's shape; the error code otherwise. Account and set ids are checked
/// against the team by the caller.
pub fn validate(
    trigger: &str,
    conditions: &mut Conditions,
    actions: &[Action],
    fallback: Option<&str>,
) -> Result<(), &'static str> {
    if !TRIGGERS.contains(&trigger) {
        return Err("invalid_trigger");
    }
    let c = conditions;
    let ids_ok = c.post_remote_ids.len() <= 100
        && c.post_remote_ids
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 300 && !p.chars().any(char::is_whitespace));
    if !ids_ok
        || c.account_ids.len() > 100
        || (c.social_set_id.is_some() && !c.account_ids.is_empty())
    {
        return Err("invalid_conditions");
    }
    c.keywords = inbox::keywords(&c.keywords).ok_or("invalid_conditions")?;
    c.account_ids.sort();
    c.account_ids.dedup();
    let mut kinds: Vec<&str> = actions.iter().map(Action::kind).collect();
    kinds.sort();
    kinds.dedup();
    if actions.is_empty() || kinds.len() != actions.len() {
        return Err("invalid_actions");
    }
    // Checked with every `{name}` at its longest, so a long name never makes it too long.
    let longest = |t: &str| t.replace("{name}", &"n".repeat(MAX_NAME_CHARS));
    for a in actions {
        let (via, text) = a.via_text();
        if !inbox::fits(via, &longest(text)) {
            return Err("invalid_text");
        }
    }
    if let Some(f) = fallback {
        let dm = actions.iter().any(|a| matches!(a, Action::SendDm { .. }));
        if !dm || !inbox::fits(Via::Public, &longest(f)) {
            return Err("invalid_fallback");
        }
    }
    Ok(())
}

/// What conditions look at in a comment.
pub struct Facts<'a> {
    pub account_id: Uuid,
    /// Live sets holding the account.
    pub account_sets: &'a [Uuid],
    pub parent: Option<&'a str>,
    pub root: Option<&'a str>,
    pub text: &'a str,
    /// No earlier run of this automation for the same author.
    pub first_from_author: bool,
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
    set_ok && account_ok && post_ok && keyword_ok && (!c.first_per_author || f.first_from_author)
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

/// Runs cut off mid-send fail as `interrupted`; they are never sent again.
pub async fn recover_stuck(db: &DatabaseConnection) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE automation_runs SET status = 'failed', reason = 'interrupted', \
         updated_at = now() WHERE status = 'running' \
         AND updated_at < now() - make_interval(mins => $1)",
        vec![STUCK_MINUTES.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Runs every active `comment_received` automation of the item's team that was on when the
/// comment came.
pub async fn run(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<(), DbErr> {
    let db = &app.db;
    let autos = automations::Entity::find()
        .filter(automations::Column::TeamId.eq(item.team_id))
        .filter(automations::Column::Active.eq(true))
        .filter(automations::Column::Trigger.eq("comment_received"))
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
        );
        let (Ok(cond), Ok(actions)) = parsed else {
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
        let facts = Facts {
            account_id: account.id,
            account_sets: &sets,
            parent: item.parent_remote_id.as_deref(),
            root: item.root_remote_id.as_deref(),
            text: &item.text,
            first_from_author: first,
        };
        if !matches(&cond, &facts) {
            continue;
        }
        let Some(run_id) = claim(db, &a, item).await? else {
            continue;
        };
        let results = act(app, account, item, &a, &actions).await;
        finish(db, run_id, results).await?;
    }
    Ok(())
}

/// Inserts the run and reserves its hit of the daily cap in one transaction. `None` when the
/// item already has a run of this automation, or the cap is reached (the run is then stored
/// `skipped: daily_cap`).
async fn claim(
    db: &DatabaseConnection,
    a: &automations::Model,
    item: &inbox_items::Model,
) -> Result<Option<Uuid>, DbErr> {
    let txn = db.begin().await?;
    let id = Uuid::now_v7();
    let stmt = sql(
        "INSERT INTO automation_runs (id, automation_id, inbox_item_id, team_id, status) \
         VALUES ($1, $2, $3, $4, 'running') \
         ON CONFLICT (automation_id, inbox_item_id) DO NOTHING",
        vec![id.into(), a.id.into(), item.id.into(), a.team_id.into()],
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
    /// `send_dm`, `reply_publicly` or `fallback_public_reply`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `sent`, `failed` or `skipped`.
    pub status: String,
    /// Why it was not sent, e.g. `dm_not_allowed`, `no_dm_access`, `rate_limited`.
    pub reason: Option<String>,
    pub remote_id: Option<String>,
}

fn outcome(kind: &str, sent: Result<Result<String, InboxError>, DbErr>) -> ActionResult {
    let (status, reason, remote_id) = match sent {
        Ok(Ok(id)) => ("sent", None, Some(id)),
        Ok(Err(
            e @ (InboxError::DmNotAllowed | InboxError::NoDmAccess | InboxError::RateLimited),
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

/// Sends each action once, then the fallback reply when a DM could not go and nothing was
/// replied publicly.
async fn act(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    a: &automations::Model,
    actions: &[Action],
) -> Vec<ActionResult> {
    let name = item.author_name.as_deref();
    let mut results = Vec::new();
    for action in actions {
        let (via, template) = action.via_text();
        let (text, filled) = render(template, name);
        let sent = inbox::send(app, account, item, via, &text, &filled, None, Some(a.id)).await;
        results.push(outcome(
            action.kind(),
            sent.map(|r| r.map(|m| m.remote_id.unwrap_or_default())),
        ));
    }
    let dm_refused = results.iter().any(|r| {
        r.kind == "send_dm"
            && matches!(r.reason.as_deref(), Some("dm_not_allowed" | "no_dm_access"))
    });
    let replied = results
        .iter()
        .any(|r| r.kind == "reply_publicly" && r.status == "sent");
    if let (true, false, Some(f)) = (dm_refused, replied, &a.fallback_public_reply) {
        let (text, filled) = render(f, name);
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
    let stmt = sql(
        "UPDATE automation_runs SET status = $2, reason = $3, results = $4, updated_at = now() \
         WHERE id = $1 AND status = 'running'",
        vec![
            id.into(),
            status.into(),
            reason.into(),
            serde_json::json!(results).into(),
        ],
    );
    db.execute_raw(stmt).await.map(|_| ())
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
