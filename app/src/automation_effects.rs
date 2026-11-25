//! What an automation does on Bluesky beside sending a text: like the comment or mention, repost
//! it, follow its author back. Each goes through the same code as the inbox's own buttons
//! (`bluesky_actions::perform`: the same live-state check, the same refusals, and a row in
//! `inbox_actions` with nobody as `done_by`), under the same per-item lock, so an automation and
//! a click, or two automations, never make two records. The caller (`automations::effect`)
//! claims the action first, like a send, and turns what comes back into a result.
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
use serde_json::json;

use crate::{
    automations::ActionResult,
    bluesky_actions::{self, Action, Extra},
    connectors::bluesky::InboxError,
    models::{automations, inbox_items, social_accounts},
    state::AppState,
};

fn result(
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

/// Does `action` (`Like`, `Repost` or `Follow`) on `item` from a Bluesky `account`, once. Never
/// errors: what went wrong is the result, `skipped` when there was nothing to do there (the item
/// is no place for it, Bluesky is busy, the post is gone), `failed` otherwise.
pub async fn bluesky(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    a: &automations::Model,
    (kind, action): (&str, Action),
) -> ActionResult {
    let skipped = |why: &str| result(kind, "skipped", Some(why), None);
    if account.provider != "bluesky" {
        // Refused before anything is called or logged: it is no Bluesky item.
        return skipped("not_supported");
    }
    let Ok(txn) = app.db.begin().await else {
        return result(kind, "failed", Some("internal"), None);
    };
    let lock = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 2)) AS ok",
        [format!("{}:{}", item.id, action.as_str()).into()],
    );
    let held = match txn.query_one_raw(lock).await {
        Ok(Some(r)) => r.try_get("", "ok").unwrap_or(false),
        _ => false,
    };
    if !held {
        return skipped("action_in_progress");
    }
    // A fan who likes ten posts in a round is followed once: the AppView may not show the first
    // follow yet when the next like is looked at, so our own log is asked too.
    if action == Action::Follow {
        let done = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS n FROM inbox_actions WHERE social_account_id = $1 AND action = 'follow' \
             AND target = $2 AND status = 'done' AND created_at > now() - interval '1 day'",
            [account.id.into(), item.author_id.clone().into()],
        );
        if matches!(txn.query_one_raw(done).await, Ok(Some(_))) {
            return skipped("already_done");
        }
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
        Err(e @ (InboxError::Invalid(_) | InboxError::RateLimited | InboxError::PostGone)) => {
            skipped(e.code())
        }
        Err(e) => result(kind, "failed", Some(e.code()), None),
    }
}
