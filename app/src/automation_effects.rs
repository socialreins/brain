//! What an automation does beside sending a text, on Bluesky and in the team: like the item,
//! repost it, follow its author back, assign it to a member, and email the team about it. Each
//! goes through the same code as the inbox's own buttons (`bluesky_actions::perform`, so the
//! same live-state check, the same refusals and a row in `inbox_actions`), is claimed like a
//! send (one daily hit, once per item), and answers an `ActionResult` instead of an error: a
//! refusal ends as `skipped` or `failed`, never stops the other actions.
use sea_orm::{
    ConnectionTrait, DbBackend, DbErr, EntityTrait, QuerySelect, Statement, TransactionTrait,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    automations::ActionResult,
    bluesky_actions::{self, Action, Extra},
    connectors::bluesky::InboxError,
    models::{automations, inbox_items, social_accounts, team_user},
    rate_limit,
    state::AppState,
};

/// Notifications to the team an automation may send per team per day; each goes to every owner
/// and admin.
pub const MAILS_PER_DAY: i64 = 20;
/// Longest item text put into a mail.
const MAIL_CHARS: usize = 300;

/// One thing an automation does on an item beyond a send or a tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    Like,
    Repost,
    FollowBack,
    Assign(Uuid),
    Notify,
}

impl Effect {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Like => "like_item",
            Self::Repost => "repost_item",
            Self::FollowBack => "follow_back",
            Self::Assign(_) => "assign_item",
            Self::Notify => "notify_team",
        }
    }

    /// Acts on the item's own post: only a comment or a mention has one.
    pub fn on_post(self) -> bool {
        matches!(self, Self::Like | Self::Repost)
    }

    /// Goes to Bluesky's servers, so only a Bluesky account can do it.
    pub fn bluesky_only(self) -> bool {
        matches!(self, Self::Like | Self::Repost | Self::FollowBack)
    }

    fn action(self) -> Option<Action> {
        match self {
            Self::Like => Some(Action::Like),
            Self::Repost => Some(Action::Repost),
            Self::FollowBack => Some(Action::Follow),
            Self::Assign(_) | Self::Notify => None,
        }
    }
}

fn sql(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// `done`, or `skipped`/`failed` with a `reason`.
pub fn result(
    kind: &str,
    status: &str,
    reason: Option<&str>,
    remote_id: Option<String>,
) -> ActionResult {
    ActionResult {
        kind: kind.into(),
        status: status.into(),
        reason: reason.map(Into::into),
        remote_id,
        ..Default::default()
    }
}

/// Does `effect` on `item`, once. Never panics and never errors: what went wrong is the result.
pub async fn perform(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    a: &automations::Model,
    effect: Effect,
) -> ActionResult {
    let done = match effect {
        Effect::Like | Effect::Repost | Effect::FollowBack => {
            Ok(bluesky(app, account, item, a, effect).await)
        }
        Effect::Assign(user) => assign(&app.db, item, user).await,
        Effect::Notify => notify(app, item, a).await,
    };
    done.unwrap_or_else(|e| {
        tracing::error!(error = %e, item = %item.id, kind = effect.kind(), "automation action failed");
        result(effect.kind(), "failed", Some("internal"), None)
    })
}

async fn bluesky(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    a: &automations::Model,
    effect: Effect,
) -> ActionResult {
    let kind = effect.kind();
    let Some(action) = effect.action() else {
        return result(kind, "failed", Some("internal"), None);
    };
    if account.provider != "bluesky" {
        // Refused before anything is called or logged: it is no Bluesky item.
        return result(kind, "skipped", Some("unsupported_platform"), None);
    }
    // One attempt at a time per item and action, as the inbox's own buttons: a second automation
    // or a click at the same moment must not read "not liked yet" and make a second record.
    let Ok(txn) = app.db.begin().await else {
        return result(kind, "failed", Some("internal"), None);
    };
    let lock = sql(
        "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 2)) AS ok",
        vec![format!("{}:{}", item.id, action.as_str()).into()],
    );
    let held = match txn.query_one_raw(lock).await {
        Ok(Some(r)) => r.try_get("", "ok").unwrap_or(false),
        _ => false,
    };
    if !held {
        return result(kind, "skipped", Some("action_in_progress"), None);
    }
    let outcome = bluesky_actions::perform(app, account, item, action, &Extra::default()).await;
    if outcome == Err(InboxError::Reconnect) {
        crate::expiry::flag_reconnect(&app.db, account.id).await;
    }
    let detail = json!({ "automation": a.id });
    if let Err(e) = bluesky_actions::log(app, item, None, action, &outcome, detail).await {
        tracing::error!(error = %e, item = %item.id, "automation action not logged");
    }
    let _ = txn.commit().await;
    match outcome {
        Ok(remote) => result(kind, "done", None, remote),
        // Nothing to do there, not a fault: the item is no place for it, or Bluesky is busy.
        Err(e @ (InboxError::Invalid(_) | InboxError::RateLimited | InboxError::PostGone)) => {
            result(kind, "skipped", Some(e.code()), None)
        }
        Err(e) => result(kind, "failed", Some(e.code()), None),
    }
}

/// Assigns the item to `user` when they are still a member and nobody has it yet: an automation
/// never takes an item from the person who has it.
async fn assign(
    db: &sea_orm::DatabaseConnection,
    item: &inbox_items::Model,
    user: Uuid,
) -> Result<ActionResult, DbErr> {
    let kind = Effect::Assign(user).kind();
    let txn = db.begin().await?;
    // Held to the commit, so a removal of the member waits and then clears this too.
    let member = team_user::Entity::find_by_id((item.team_id, user))
        .lock_shared()
        .one(&txn)
        .await?;
    if member.is_none() {
        return Ok(result(kind, "skipped", Some("no_member"), None));
    }
    let stmt = sql(
        "UPDATE inbox_items SET assigned_to = $2 WHERE id = $1 AND team_id = $3 \
         AND assigned_to IS NULL",
        vec![item.id.into(), user.into(), item.team_id.into()],
    );
    let n = txn.execute_raw(stmt).await?.rows_affected();
    txn.commit().await?;
    Ok(match n {
        1 => result(kind, "done", None, None),
        _ => result(kind, "skipped", Some("already_assigned"), None),
    })
}

/// The people `notify` mails: the team's owners and admins, by address.
pub async fn recipients(db: &impl ConnectionTrait, team: Uuid) -> Result<Vec<String>, DbErr> {
    let rows = db
        .query_all_raw(sql(
            "SELECT DISTINCT u.email FROM team_user m JOIN users u ON u.id = m.user_id \
             WHERE m.team_id = $1 AND m.role IN ('owner', 'admin') ORDER BY u.email",
            vec![team.into()],
        ))
        .await?;
    rows.iter().map(|r| r.try_get("", "email")).collect()
}

pub fn mail_bucket(team: Uuid) -> String {
    format!("automation-mail:team:{team}")
}

/// What kind of item to tell the team about, in words.
fn noun(kind: &str) -> &'static str {
    match kind {
        "comment" => "comment",
        "mention" => "mention",
        "dm" => "message",
        "follow" => "new follower",
        "like" => "like",
        _ => "repost",
    }
}

/// The item's text as one quoted line: a person wrote it, so it must never turn into a link
/// button (a line starting with a link) or a heading in the HTML version of the mail.
fn quoted(text: &str) -> String {
    // One line, and no control or direction-changing characters a link could hide behind.
    let hidden =
        |c: char| c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let text: String = text
        .chars()
        .filter(|c| !hidden(*c) || c.is_whitespace())
        .collect();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = flat.chars().take(MAIL_CHARS).collect();
    let dots = if flat.chars().count() > MAIL_CHARS {
        "..."
    } else {
        ""
    };
    format!("\u{201c}{cut}{dots}\u{201d}")
}

/// Mails the team's owners and admins about the item: the kind of item, its text (cut) and a
/// link to it, nothing else about the person. At most `MAILS_PER_DAY` a team a day.
async fn notify(
    app: &AppState,
    item: &inbox_items::Model,
    a: &automations::Model,
) -> Result<ActionResult, DbErr> {
    let kind = Effect::Notify.kind();
    let cap = [(mail_bucket(item.team_id), MAILS_PER_DAY)];
    let Some(hits) = rate_limit::daily_reserve(&app.db, &cap).await? else {
        return Ok(result(kind, "skipped", Some("mail_cap"), None));
    };
    let read = async {
        let to = recipients(&app.db, item.team_id).await?;
        let code: String = app
            .db
            .query_one_raw(sql(
                "SELECT code FROM teams WHERE id = $1",
                vec![item.team_id.into()],
            ))
            .await?
            .map(|r| r.try_get("", "code"))
            .transpose()?
            .unwrap_or_default();
        Ok::<_, DbErr>((to, code))
    };
    let (to, code) = match read.await {
        Ok(r) => r,
        Err(e) => {
            rate_limit::daily_refund(&app.db, &hits).await;
            return Err(e);
        }
    };
    let link = format!("{}/t/{code}/inbox?item={}", app.web_url, item.id);
    let what = noun(&item.kind);
    let subject = format!("A new {what} ran one of your automations");
    let said = if item.text.trim().is_empty() {
        String::new()
    } else {
        format!("\n{}\n", quoted(&item.text))
    };
    let text = format!(
        "The automation \"{}\" ran on a new {what} on {}.\n{said}\nOpen it in your inbox:\n{link}\n",
        a.name.split_whitespace().collect::<Vec<_>>().join(" "),
        item.platform
    );
    let mut sent = 0;
    for email in &to {
        match app
            .mailer
            .send(email, &subject, text.clone(), &app.web_url)
            .await
        {
            Ok(()) => sent += 1,
            Err(e) => tracing::warn!(error = %e, item = %item.id, "automation mail not sent"),
        }
    }
    if sent == 0 {
        rate_limit::daily_refund(&app.db, &hits).await;
        return Ok(result(kind, "failed", Some("mail_failed"), None));
    }
    Ok(result(kind, "done", None, None))
}

/// Rows of `team_user` for a member, to tell whether `user` may still be assigned.
pub async fn is_member(db: &impl ConnectionTrait, team: Uuid, user: Uuid) -> Result<bool, DbErr> {
    Ok(team_user::Entity::find_by_id((team, user))
        .select_only()
        .column(team_user::Column::UserId)
        .into_tuple::<Uuid>()
        .one(db)
        .await?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_text_is_quoted_on_one_line_and_cut() {
        // A line that is only a link would become a button in the HTML mail: it is quoted.
        let q = quoted("https://evil.example/login\nsecond   line");
        assert_eq!(q, "\u{201c}https://evil.example/login second line\u{201d}");
        assert!(!q.starts_with("http"));
        let long = quoted(&"a".repeat(400));
        assert_eq!(long.chars().count(), 300 + 3 + 2);
        assert!(long.ends_with("...\u{201d}"));
    }

    #[test]
    fn effects_know_where_they_go() {
        let user = Uuid::nil();
        let all = [
            Effect::Like,
            Effect::Repost,
            Effect::FollowBack,
            Effect::Assign(user),
            Effect::Notify,
        ];
        let kinds: Vec<_> = all.iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            [
                "like_item",
                "repost_item",
                "follow_back",
                "assign_item",
                "notify_team"
            ]
        );
        assert_eq!(
            all.map(Effect::bluesky_only),
            [true, true, true, false, false]
        );
        assert_eq!(all.map(Effect::on_post), [true, true, false, false, false]);
        assert_eq!(Effect::FollowBack.action(), Some(Action::Follow));
        assert_eq!(Effect::Notify.action(), None);
    }
}
