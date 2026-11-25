//! Automations on X: what a run costs, how long its texts may be, and when X's money or the
//! team's credits being used up must stop it.
//!
//! X bills every post, user and DM event read, and every write (docs.x.com, read 2026-11-25).
//! A run's cost is the read that brought the item (`read_micros`) plus a write for each message
//! it sends and each like, repost or hide it does (`WRITE_MICROS`; the docs name the price of a
//! post and a DM, the others are taken as the same). The per-account monthly budget of the X
//! inbox (`x_inbox.budget_micros`) pays for reads and for these writes, so an account whose
//! budget is used up runs no automation until the 1st (UTC) or a higher budget. With billing on,
//! each write is also 1 `x` credit, and a team with none runs none. Nothing here calls X.
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::Serialize;
use socialreins_core::post::{x_weight, X_MAX_LONG_WEIGHT, X_MAX_WEIGHT};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    automations::{Action, Step, MAX_NAME_CHARS},
    billing,
    connectors::x::inbox::{DM_READ_MICROS, POST_READ_MICROS, USER_READ_MICROS},
    inbox::Via,
    models::social_accounts,
    state::AppState,
    x_inbox::{self, READS_PER_CREDIT},
};

/// A post or a DM created, in millionths of a US dollar. Likes, reposts and hides are taken as
/// the same price.
pub const WRITE_MICROS: i64 = 15_000;
/// The longest DM X takes, in characters.
const DM_MAX: usize = 10_000;

/// Why X stops an automation on an account, as a run's reason.
pub const NO_CREDITS: &str = "x_no_credits";
pub const USAGE_CAPPED: &str = "x_usage_capped";
pub const BUDGET: &str = "x_budget";

/// The longest reply an X account (its `meta`) may post: 25,000 with a paid X plan, else 280, as X
/// counts.
pub fn reply_limit(meta: &serde_json::Value) -> usize {
    if crate::controllers::x::x_long_posts(meta) {
        X_MAX_LONG_WEIGHT
    } else {
        X_MAX_WEIGHT
    }
}

/// `text` fits a reply (`Public`) or DM from `account`.
pub fn fits(meta: &serde_json::Value, via: Via, text: &str) -> bool {
    let some = !text.trim().is_empty();
    match via {
        Via::Public => some && x_weight(text) <= reply_limit(meta),
        Via::Dm => some && text.chars().count() <= DM_MAX,
    }
}

/// The texts of an automation as they would stand with the longest name and a link for `{page}`
/// (a link counts 23 on X): `invalid_text` when a reply (or the fallback reply) is over the
/// shortest limit of `accounts`, the X accounts it is limited to. A DM has room for 10,000.
pub fn text_limits(
    metas: &[&serde_json::Value],
    actions: &[Action],
    steps: Option<&[Step]>,
    fallback: Option<&str>,
) -> Result<(), &'static str> {
    let Some(limit) = metas.iter().map(|m| reply_limit(m)).min() else {
        return Ok(());
    };
    let widest = |t: &str| {
        t.replace("{name}", &"n".repeat(MAX_NAME_CHARS))
            .replace("{page}", "https://socialreins.com/p/abcdefgh")
    };
    // An AI step's text is written to fit, and what is held here is the owner's own.
    let texts = actions
        .iter()
        .filter(|a| !a.ai())
        .filter_map(Action::via_text)
        .chain(
            steps
                .unwrap_or_default()
                .iter()
                .filter(|s| !s.ai())
                .filter_map(Step::sends)
                .map(|(via, t, _)| (via, t)),
        );
    let over = texts
        .filter(|(via, _)| *via == Via::Public)
        .any(|(_, t)| x_weight(&widest(t)) > limit);
    let over = over || fallback.is_some_and(|f| x_weight(&widest(f)) > limit);
    if over {
        Err("invalid_text")
    } else {
        Ok(())
    }
}

/// What one run of an automation costs on X, with the trigger's read.
#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub struct Cost {
    /// Writes a run makes at most: each message, like, repost or hide.
    pub writes: u32,
    /// The read that brought the item: a post and its author (a mention or comment), or a DM
    /// event and its sender. A person is billed once a day, so a repeat visitor costs less.
    pub read_usd: f64,
    pub write_usd: f64,
    pub total_usd: f64,
    /// With billing on: team `x` credits for the same run (1 per write, 1 per 3 items read).
    pub credits: f64,
}

fn usd(micros: i64) -> f64 {
    micros as f64 / 1e6
}

/// The cost of one run of an automation on `trigger`. A fallback reply replaces a refused DM, so
/// it is not counted; an AI step costs the same X write as any other (the model is paid apart).
pub fn cost(trigger: &str, actions: &[Action], steps: Option<&[Step]>) -> Cost {
    let writes = match steps {
        Some(steps) => steps
            .iter()
            .filter(|s| {
                s.sends().is_some()
                    || matches!(s, Step::LikeComment | Step::HideComment | Step::Repost)
            })
            .count(),
        None => actions
            .iter()
            .filter(|a| {
                a.via_text().is_some()
                    || matches!(
                        a,
                        Action::LikeComment | Action::HideComment | Action::Repost
                    )
            })
            .count(),
    } as u32;
    priced(trigger, writes)
}

/// The cost of a run of `trigger` that makes `writes` writes.
pub fn priced(trigger: &str, writes: u32) -> Cost {
    let read = if trigger == "dm_received" {
        DM_READ_MICROS + USER_READ_MICROS
    } else {
        POST_READ_MICROS + USER_READ_MICROS
    };
    let write = i64::from(writes) * WRITE_MICROS;
    Cost {
        writes,
        read_usd: usd(read),
        write_usd: usd(write),
        total_usd: usd(read + write),
        credits: f64::from(writes) + 1.0 / f64::from(READS_PER_CREDIT),
    }
}

/// The reason an automation must not send or act on `account` now: the team has no `x` credits
/// (billing on), X said the app is out of credits or at its usage cap and a person has not
/// resumed it, or this month's budget could not pay one more write. `None` when it may. An
/// account with no X inbox settings has no budget and is never stopped by one.
pub async fn blocked(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<Option<&'static str>, DbErr> {
    if app.billing_enabled
        && billing::balance(&app.db, account.team_id, billing::Kind::X).await? < 1
    {
        return Ok(Some(NO_CREDITS));
    }
    let Some(s) = x_inbox::load(&app.db, account.id).await? else {
        return Ok(None);
    };
    Ok(match s.paused_reason.as_deref() {
        Some("credits") => Some(NO_CREDITS),
        Some("usage_capped") => Some(USAGE_CAPPED),
        _ if s.spent_micros + WRITE_MICROS > s.budget_micros => Some(BUDGET),
        _ => None,
    })
}

/// Counts one write against the account's monthly budget, after X took it.
pub async fn spent(db: &impl ConnectionTrait, account: Uuid) -> Result<(), DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE x_inbox SET spent_micros = (CASE WHEN period < \
         (date_trunc('month', now() AT TIME ZONE 'UTC'))::date THEN 0 ELSE spent_micros END) + $2, \
         period = (date_trunc('month', now() AT TIME ZONE 'UTC'))::date, updated_at = now() \
         WHERE social_account_id = $1",
        [account.into(), WRITE_MICROS.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_280_unless_the_plan_is_paid_and_a_dm_is_10000() {
        let free = serde_json::json!({});
        let paid = serde_json::json!({"plan": "paid"});
        let long = "a".repeat(281);
        assert!(fits(&free, Via::Public, &"a".repeat(280)));
        assert!(!fits(&free, Via::Public, &long));
        assert!(fits(&paid, Via::Public, &long));
        assert!(!fits(&paid, Via::Public, &"a".repeat(25_001)));
        assert!(fits(&free, Via::Dm, &"a".repeat(10_000)));
        assert!(!fits(&free, Via::Dm, &"a".repeat(10_001)));
        assert!(!fits(&free, Via::Dm, "  "));
    }

    #[test]
    fn text_limits_use_the_longest_name_and_the_shortest_account() {
        let free = serde_json::json!({});
        let paid = serde_json::json!({"plan": "paid"});
        let reply = |t: &str| Action::ReplyPublicly { text: t.into() };
        // 64 for the name and 217 more is 281.
        let near = format!("{{name}}{}", "a".repeat(216));
        assert_eq!(text_limits(&[&free], &[reply(&near)], None, None), Ok(()));
        let over = format!("{{name}}{}", "a".repeat(217));
        assert_eq!(
            text_limits(&[&free], &[reply(&over)], None, None),
            Err("invalid_text")
        );
        assert_eq!(text_limits(&[&paid], &[reply(&over)], None, None), Ok(()));
        assert_eq!(
            text_limits(&[&paid, &free], &[reply(&over)], None, None),
            Err("invalid_text")
        );
        assert_eq!(
            text_limits(&[&free], &[], None, Some(&"a".repeat(281))),
            Err("invalid_text")
        );
        // A DM is not held to the reply limit.
        let dm = Action::SendDm {
            text: "a".repeat(1000),
        };
        assert_eq!(text_limits(&[&free], &[dm], None, None), Ok(()));
    }

    #[test]
    fn a_run_costs_its_read_and_a_write_for_each_message_and_reaction() {
        let both = [
            Action::SendDm { text: "a".into() },
            Action::ReplyPublicly { text: "b".into() },
            Action::LikeComment,
            Action::TagContact { tag: "t".into() },
            Action::NotifyTeam,
        ];
        let c = cost("comment_received", &both, None);
        assert_eq!((c.writes, c.read_usd, c.write_usd), (3, 0.015, 0.045));
        assert!((c.total_usd - 0.06).abs() < 1e-9);
        assert!((c.credits - (3.0 + 1.0 / 3.0)).abs() < 1e-9);
        assert_eq!(cost("dm_received", &both[..1], None).read_usd, 0.02);
    }
}
