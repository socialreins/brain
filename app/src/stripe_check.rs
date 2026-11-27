//! Stripe against brain's books, read only. Subscription state reaches brain only through the
//! webhook, so a wrong signing secret or a disabled endpoint leaves it silently behind. Once a
//! day the operator digest (`operator_digest`) lists the live subscriptions Stripe holds
//! (`active`, `trialing`, `past_due`) and compares them with `subscriptions`; each difference
//! becomes a line of "Stripe mismatches": the team's slug and what differs, never an id, an
//! email or a card. Nothing is written or repaired here: resending the event from the Stripe
//! dashboard is the mend. Does nothing without `BILLING_ENABLED` and a Stripe key.
//! shortcut: a subscription changed in the last `GRACE_MINUTES` is left out (the webhook may be
//! on its way), and a mismatch that appears and mends inside a day is not seen.
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use uuid::Uuid;

use crate::{billing, state::AppState};

/// Stripe states read and compared: the ones that keep a plan (`billing::live`).
const STATES: [&str; 3] = ["active", "trialing", "past_due"];
/// A subscription made or changed more recently than this is not compared yet.
const GRACE_MINUTES: i64 = 30;

/// A subscription as Stripe holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub id: String,
    pub status: String,
    /// The plan its price belongs to, when it is one of ours.
    pub plan: Option<String>,
    /// The team brain put in its metadata.
    pub team: Option<Uuid>,
    pub created: DateTime<Utc>,
}

/// A row of `subscriptions` that came from Stripe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub id: String,
    pub status: String,
    pub plan: String,
    pub team: Uuid,
    pub updated: DateTime<Utc>,
}

/// One difference: the team (when known) and what differs, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub team: Option<Uuid>,
    pub what: String,
}

/// What differs between Stripe's live subscriptions and brain's rows, as of `now`.
pub fn compare(remote: &[Remote], local: &[Local], now: DateTime<Utc>) -> Vec<Mismatch> {
    let recent = now - Duration::minutes(GRACE_MINUTES);
    let by_id: HashMap<&str, &Local> = local.iter().map(|l| (l.id.as_str(), l)).collect();
    let live: HashSet<&str> = remote.iter().map(|r| r.id.as_str()).collect();
    let mut out = Vec::new();
    for r in remote.iter().filter(|r| r.created < recent) {
        let word = |what: String| Mismatch { team: r.team, what };
        match by_id.get(r.id.as_str()) {
            None => out.push(word(format!(
                "Stripe has it {}, brain has no row",
                r.status
            ))),
            Some(l) if l.updated >= recent => {}
            Some(l) => {
                if l.status != r.status {
                    out.push(Mismatch {
                        team: Some(l.team),
                        what: format!("Stripe has it {}, brain has {}", r.status, l.status),
                    });
                }
                if r.plan.as_deref().is_some_and(|p| p != l.plan) {
                    let plan = r.plan.as_deref().unwrap_or_default();
                    out.push(Mismatch {
                        team: Some(l.team),
                        what: format!("Stripe has plan {plan}, brain has {}", l.plan),
                    });
                }
            }
        }
    }
    for l in local {
        if billing::live(&l.status) && !live.contains(l.id.as_str()) && l.updated < recent {
            out.push(Mismatch {
                team: Some(l.team),
                what: format!("brain has it {}, Stripe has none live", l.status),
            });
        }
    }
    out
}

fn remote_of(app: &AppState, v: &serde_json::Value) -> Option<Remote> {
    Some(Remote {
        id: v["id"].as_str()?.to_owned(),
        status: v["status"].as_str()?.to_owned(),
        plan: v["items"]["data"][0]["price"]["id"]
            .as_str()
            .and_then(|p| app.stripe.plan_of(p))
            .map(str::to_owned),
        team: v["metadata"]["team_id"]
            .as_str()
            .and_then(|t| t.parse().ok()),
        created: DateTime::from_timestamp(v["created"].as_i64()?, 0)?,
    })
}

async fn locals(app: &AppState) -> Result<Vec<Local>, DbErr> {
    let rows = app
        .db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT provider_subscription_id AS id, status, plan_id, team_id, updated_at \
             FROM subscriptions WHERE provider = 'stripe' AND provider_subscription_id IS NOT NULL"
                .to_owned(),
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(Local {
                id: r.try_get("", "id")?,
                status: r.try_get("", "status")?,
                plan: r.try_get("", "plan_id")?,
                team: r.try_get("", "team_id")?,
                updated: r.try_get("", "updated_at")?,
            })
        })
        .collect()
}

/// The differences found now, or `None` when there is nothing to check (billing off, or no
/// Stripe key). `Err` holds why Stripe or the database could not be read, in a few words.
pub async fn check(app: &AppState) -> Option<Result<Vec<Mismatch>, String>> {
    if !app.billing_enabled || !app.stripe.can_read() {
        return None;
    }
    Some(read(app).await)
}

async fn read(app: &AppState) -> Result<Vec<Mismatch>, String> {
    let mut remote = Vec::new();
    for status in STATES {
        let page = app
            .stripe
            .subscriptions(status)
            .await
            .map_err(|e| format!("Stripe could not be read ({})", e.1))?;
        remote.extend(page.iter().filter_map(|v| remote_of(app, v)));
    }
    let local = locals(app)
        .await
        .map_err(|_| "the database could not be read".to_owned())?;
    Ok(compare(&remote, &local, Utc::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes_ago: i64) -> DateTime<Utc> {
        Utc::now() - Duration::minutes(minutes_ago)
    }

    fn remote(id: &str, status: &str, plan: &str, team: Uuid, age: i64) -> Remote {
        Remote {
            id: id.into(),
            status: status.into(),
            plan: Some(plan.into()),
            team: Some(team),
            created: at(age),
        }
    }

    fn local(id: &str, status: &str, plan: &str, team: Uuid, age: i64) -> Local {
        Local {
            id: id.into(),
            status: status.into(),
            plan: plan.into(),
            team,
            updated: at(age),
        }
    }

    #[test]
    fn equal_books_have_no_mismatch() {
        let t = Uuid::now_v7();
        let r = [remote("sub_1", "active", "scheduler", t, 500)];
        let l = [local("sub_1", "active", "scheduler", t, 500)];
        assert!(compare(&r, &l, Utc::now()).is_empty());
    }

    #[test]
    fn each_kind_of_difference_is_named() {
        let (a, b, c, d) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let r = [
            remote("sub_a", "active", "scheduler", a, 500),
            remote("sub_b", "trialing", "automation", b, 500),
            remote("sub_c", "active", "scheduler", c, 500),
        ];
        let l = [
            local("sub_b", "active", "scheduler", b, 500),
            local("sub_c", "active", "scheduler", c, 500),
            local("sub_d", "past_due", "scheduler", d, 500),
            local("sub_e", "canceled", "scheduler", Uuid::now_v7(), 500),
        ];
        let m = compare(&r, &l, Utc::now());
        let said = |team: Uuid| -> Vec<&str> {
            m.iter()
                .filter(|x| x.team == Some(team))
                .map(|x| x.what.as_str())
                .collect()
        };
        assert_eq!(said(a), ["Stripe has it active, brain has no row"]);
        assert_eq!(
            said(b),
            [
                "Stripe has it trialing, brain has active",
                "Stripe has plan automation, brain has scheduler"
            ]
        );
        assert!(said(c).is_empty());
        assert_eq!(said(d), ["brain has it past_due, Stripe has none live"]);
        // An ended row Stripe no longer lists is right.
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn a_change_of_the_last_half_hour_is_left_for_the_webhook() {
        let t = Uuid::now_v7();
        let r = [
            remote("sub_new", "active", "scheduler", t, 5),
            remote("sub_old", "active", "scheduler", t, 500),
        ];
        let l = [
            local("sub_old", "trialing", "scheduler", t, 5),
            local("sub_gone", "active", "scheduler", t, 5),
        ];
        assert!(compare(&r, &l, Utc::now()).is_empty());
    }
}
