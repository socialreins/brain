//! Cloud billing: what a team's plan allows, and its prepaid credits.
//!
//! `BILLING_ENABLED` is off on a self-hosted install, so nothing is capped there. When it is on,
//! `effective` answers from the team's subscription; a team with no live subscription may do
//! nothing. Credits cover what costs money per use (`x` posts, `ai` calls). `credit_ledger` holds
//! signed amounts; a balance is their sum. `grant` and `spend` take a `reference`, an idempotency
//! key unique per team, kind and direction (a refund may reuse the ref of its spend), so a retry after a lost answer never counts twice.
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait, Value,
};
use uuid::Uuid;

/// What the credits pay for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A post to X, which we pay X for.
    X,
    /// A call to an AI model that runs on our key.
    Ai,
    /// A use of the plan's monthly AI allowance (`spend_ai`). Never sold or granted, and never a
    /// balance: only its uses this month are counted.
    AiPlan,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Ai => "ai",
            Self::AiPlan => "ai_plan",
        }
    }
}

/// What a team may use. `None` is unlimited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub accounts: Option<i64>,
    pub members: Option<i64>,
    /// Automations, broadcasts and newsletters.
    pub automation: bool,
    pub contacts: Option<i64>,
    /// Custom inboxes ("All" never counts).
    pub inboxes: Option<i64>,
    /// AI replies a calendar month (UTC) on our key before `ai` credits are used; `None` when
    /// nothing is metered (billing off).
    pub ai_monthly: Option<i64>,
}

impl Limits {
    pub fn unlimited() -> Self {
        Self {
            accounts: None,
            members: None,
            automation: true,
            contacts: None,
            inboxes: None,
            ai_monthly: None,
        }
    }

    /// A team without a live plan: nothing is allowed until it subscribes.
    pub fn none() -> Self {
        Self {
            accounts: Some(0),
            members: Some(0),
            automation: false,
            contacts: Some(0),
            inboxes: Some(0),
            ai_monthly: Some(0),
        }
    }
}

/// A subscription in one of these states keeps its plan's limits. A `past_due` team keeps working
/// while the provider retries the payment; `canceled` ends it.
pub fn live(status: &str) -> bool {
    matches!(status, "trialing" | "active" | "past_due")
}

fn stmt(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// A team's subscription with the limits of its plan.
pub struct Subscription {
    pub plan_id: String,
    pub plan_name: String,
    pub status: String,
    pub trial_ends_at: Option<chrono::DateTime<chrono::Utc>>,
    pub current_period_end: Option<chrono::DateTime<chrono::Utc>>,
    pub cancel_at_period_end: bool,
    /// `stripe`, or `comp` for a free plan an admin gave.
    pub provider: Option<String>,
    pub limits: Limits,
}

/// The team's subscription, live or not.
pub async fn subscription(
    db: &impl ConnectionTrait,
    team: Uuid,
) -> Result<Option<Subscription>, DbErr> {
    let row = db
        .query_one_raw(stmt(
            "SELECT s.status, s.trial_ends_at, s.current_period_end, s.cancel_at_period_end, s.provider,
                    p.id, p.name, p.max_accounts, p.max_members, p.automation, p.max_contacts,
                    p.max_inboxes, p.ai_monthly
             FROM subscriptions s JOIN plans p ON p.id = s.plan_id WHERE s.team_id = $1",
            vec![team.into()],
        ))
        .await?;
    let Some(r) = row else { return Ok(None) };
    let count = |c: &str| -> Result<Option<i64>, DbErr> {
        Ok(r.try_get::<Option<i32>>("", c)?.map(i64::from))
    };
    Ok(Some(Subscription {
        status: r.try_get("", "status")?,
        trial_ends_at: r.try_get("", "trial_ends_at")?,
        current_period_end: r.try_get("", "current_period_end")?,
        cancel_at_period_end: r.try_get("", "cancel_at_period_end")?,
        provider: r.try_get("", "provider")?,
        plan_id: r.try_get("", "id")?,
        plan_name: r.try_get("", "name")?,
        limits: Limits {
            accounts: count("max_accounts")?,
            members: count("max_members")?,
            automation: r.try_get("", "automation")?,
            contacts: count("max_contacts")?,
            inboxes: count("max_inboxes")?,
            ai_monthly: Some(r.try_get::<i32>("", "ai_monthly")?.into()),
        },
    }))
}

/// Whether the team has a plan that keeps it working (trial, paid or retrying a payment).
pub async fn has_plan(db: &impl ConnectionTrait, team: Uuid) -> Result<bool, DbErr> {
    Ok(subscription(db, team)
        .await?
        .is_some_and(|s| live(&s.status)))
}

/// What the team may use right now.
pub fn effective(enabled: bool, sub: Option<&Subscription>) -> Limits {
    match (enabled, sub) {
        (false, _) => Limits::unlimited(),
        (true, Some(s)) if live(&s.status) => s.limits.clone(),
        (true, _) => Limits::none(),
    }
}

/// What a plan can refuse. The answer names it (`plan_limit`, `limit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    Accounts,
    Members,
    Automation,
    Inboxes,
    /// No live plan at all: publishing waits for one.
    Plan,
}

impl Limit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Members => "members",
            Self::Automation => "automation",
            Self::Inboxes => "inboxes",
            Self::Plan => "plan",
        }
    }
}

/// The `DbErr::Custom` text that `save_account` fails with when a new account would pass the
/// plan's limit; callers answer it as a limit, not as a fault.
pub const ACCOUNT_LIMIT: &str = "plan_limit:accounts";

pub fn is_account_limit(e: &DbErr) -> bool {
    matches!(e, DbErr::Custom(m) if m == ACCOUNT_LIMIT)
}

/// Like `ACCOUNT_LIMIT`, for an invitation that would take the last seat too many.
pub const MEMBER_LIMIT: &str = "plan_limit:members";

pub fn is_member_limit(e: &DbErr) -> bool {
    matches!(e, DbErr::Custom(m) if m == MEMBER_LIMIT)
}

/// What the team may use now (everything when billing is off).
pub async fn limits_of(
    enabled: bool,
    db: &impl ConnectionTrait,
    team: Uuid,
) -> Result<Limits, DbErr> {
    if !enabled {
        return Ok(Limits::unlimited());
    }
    let sub = subscription(db, team).await?;
    Ok(effective(enabled, sub.as_ref()))
}

/// Whether the team has no seat left for one more member. `pending` counts open invitations
/// as seats, except the one for `except_email`, which a new invitation to it would replace.
pub async fn members_full(
    enabled: bool,
    db: &impl ConnectionTrait,
    team: Uuid,
    pending: Option<&str>,
) -> Result<bool, DbErr> {
    let Some(max) = limits_of(enabled, db, team).await?.members else {
        return Ok(false);
    };
    let row = db
        .query_one_raw(stmt(
            "SELECT (SELECT count(*) FROM team_user WHERE team_id = $1)
                  + (SELECT CASE WHEN $2 THEN count(*) ELSE 0 END
                     FROM team_invitations WHERE team_id = $1 AND expires_at > now()
                       AND email <> $3) AS n",
            vec![
                team.into(),
                pending.is_some().into(),
                pending.unwrap_or_default().into(),
            ],
        ))
        .await?;
    let n: i64 = row.map_or(Ok(0), |r| r.try_get("", "n"))?;
    Ok(n >= max)
}

/// The team's balance of one kind of credit.
pub async fn balance(db: &impl ConnectionTrait, team: Uuid, kind: Kind) -> Result<i64, DbErr> {
    let row = db
        .query_one_raw(stmt(
            "SELECT COALESCE(SUM(amount), 0)::bigint AS n FROM credit_ledger
             WHERE team_id = $1 AND kind = $2",
            vec![team.into(), kind.as_str().into()],
        ))
        .await?;
    row.map_or(Ok(0), |r| r.try_get("", "n"))
}

/// Adds `amount` (more than 0) credits. `false` when `reference` was already used for a grant of
/// this kind by this team, so a webhook delivered twice grants once. A grant may reuse the
/// reference of a spend, which is how a refund names the spend it undoes.
pub async fn grant(
    db: &impl ConnectionTrait,
    team: Uuid,
    kind: Kind,
    amount: i64,
    reason: &str,
    reference: Option<&str>,
) -> Result<bool, DbErr> {
    if amount <= 0 {
        return Err(DbErr::Custom("a grant adds credits".into()));
    }
    let done = db
        .execute_raw(stmt(
            "INSERT INTO credit_ledger (id, team_id, kind, amount, reason, ref)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
            vec![
                Uuid::now_v7().into(),
                team.into(),
                kind.as_str().into(),
                amount.into(),
                reason.into(),
                reference.map(str::to_owned).into(),
            ],
        ))
        .await?;
    Ok(done.rows_affected() == 1)
}

/// How a spend ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Spent {
    /// The credits were taken.
    Taken,
    /// The same `reference` was spent before: nothing more was taken.
    Repeated,
    /// Not enough credits; this many are left.
    Short(i64),
}

/// Takes `amount` (more than 0) credits, never taking the balance below 0. The team row is
/// locked (`FOR NO KEY UPDATE`, which does not block rows that point at the team) for the check
/// and the insert, so two spends at once cannot both pass.
pub async fn spend(
    db: &DatabaseConnection,
    team: Uuid,
    kind: Kind,
    amount: i64,
    reason: &str,
    reference: Option<&str>,
) -> Result<Spent, DbErr> {
    if amount <= 0 {
        return Err(DbErr::Custom("a spend takes credits".into()));
    }
    let tx = db.begin().await?;
    tx.execute_raw(stmt(
        "SELECT id FROM teams WHERE id = $1 FOR NO KEY UPDATE",
        vec![team.into()],
    ))
    .await?;
    if let Some(r) = reference {
        let seen = tx
            .query_one_raw(stmt(
                "SELECT 1 AS one FROM credit_ledger
                 WHERE team_id = $1 AND kind = $2 AND ref = $3 AND amount < 0",
                vec![team.into(), kind.as_str().into(), r.into()],
            ))
            .await?;
        if seen.is_some() {
            tx.commit().await?;
            return Ok(Spent::Repeated);
        }
    }
    let have = balance(&tx, team, kind).await?;
    if have < amount {
        tx.commit().await?;
        return Ok(Spent::Short(have));
    }
    tx.execute_raw(stmt(
        "INSERT INTO credit_ledger (id, team_id, kind, amount, reason, ref)
         VALUES ($1, $2, $3, $4, $5, $6)",
        vec![
            Uuid::now_v7().into(),
            team.into(),
            kind.as_str().into(),
            (-amount).into(),
            reason.into(),
            reference.map(str::to_owned).into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(Spent::Taken)
}

/// The start of the calendar month (UTC) `now` is in, as SQL on the parameter `$n`.
fn month_of(n: usize) -> String {
    format!("date_trunc('month', ${n}::timestamptz AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'")
}

/// The team's AI replies on our key in the calendar month (UTC) of `now` that the allowance paid
/// for and did not give back.
pub async fn ai_used(
    db: &impl ConnectionTrait,
    team: Uuid,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<i64, DbErr> {
    let sql = format!(
        "SELECT count(*)::bigint AS n FROM credit_ledger l
         WHERE l.team_id = $1 AND l.kind = 'ai_plan' AND l.amount < 0
           AND l.created_at >= {m} AND l.created_at < {m} + interval '1 month'
           AND NOT EXISTS (SELECT 1 FROM credit_ledger b WHERE b.team_id = l.team_id
             AND b.kind = l.kind AND b.ref = l.ref AND b.amount > 0)",
        m = month_of(2)
    );
    let row = db
        .query_one_raw(stmt(&sql, vec![team.into(), now.into()]))
        .await?;
    row.map_or(Ok(0), |r| r.try_get("", "n"))
}

/// How a spend for one AI reply on our key ended.
#[derive(Debug, PartialEq, Eq)]
pub enum AiSpent {
    /// Paid from the monthly allowance (`Kind::AiPlan`) or an `ai` credit (`Kind::Ai`); a refund
    /// gives it back with `grant` of that kind under the same reference. Also the answer when
    /// the reference was paid before and not given back.
    Paid(Kind),
    /// The reference was paid and given back: that reply was given up, never written again.
    Refunded,
    /// The allowance is used up and no `ai` credit is left.
    Short,
}

/// Pays for one AI reply under `reference`: the plan's `monthly` allowance for the calendar
/// month (UTC) of `now` first, then an `ai` credit. Under the team row lock, as `spend`, so two
/// replies at once never both take the last one. A reference seen before is never paid twice,
/// and one given back is never paid again (`Refunded`).
pub async fn spend_ai(
    db: &DatabaseConnection,
    team: Uuid,
    monthly: i64,
    reference: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AiSpent, DbErr> {
    let tx = db.begin().await?;
    tx.execute_raw(stmt(
        "SELECT id FROM teams WHERE id = $1 FOR NO KEY UPDATE",
        vec![team.into()],
    ))
    .await?;
    let seen = tx
        .query_one_raw(stmt(
            "SELECT bool_or(amount > 0) AS back, min(kind) AS kind FROM credit_ledger
             WHERE team_id = $1 AND kind IN ('ai', 'ai_plan') AND ref = $2",
            vec![team.into(), reference.into()],
        ))
        .await?;
    if let Some(r) = seen {
        let back: Option<bool> = r.try_get("", "back")?;
        let kind: Option<String> = r.try_get("", "kind")?;
        let paid = match kind.as_deref() {
            Some("ai") => Some(Kind::Ai),
            Some(_) => Some(Kind::AiPlan),
            None => None,
        };
        match (back, paid) {
            (Some(true), _) => {
                tx.commit().await?;
                return Ok(AiSpent::Refunded);
            }
            (_, Some(kind)) => {
                tx.commit().await?;
                return Ok(AiSpent::Paid(kind));
            }
            _ => {}
        }
    }
    let kind = if ai_used(&tx, team, now).await? < monthly {
        Kind::AiPlan
    } else if balance(&tx, team, Kind::Ai).await? >= 1 {
        Kind::Ai
    } else {
        tx.commit().await?;
        return Ok(AiSpent::Short);
    };
    tx.execute_raw(stmt(
        "INSERT INTO credit_ledger (id, team_id, kind, amount, reason, ref, created_at)
         VALUES ($1, $2, $3, -1, 'ai reply', $4, $5)",
        vec![
            Uuid::now_v7().into(),
            team.into(),
            kind.as_str().into(),
            reference.into(),
            now.into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(AiSpent::Paid(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(status: &str) -> Subscription {
        Subscription {
            plan_id: "scheduler".into(),
            plan_name: "Scheduler".into(),
            status: status.into(),
            trial_ends_at: None,
            current_period_end: None,
            cancel_at_period_end: false,
            provider: None,
            limits: Limits {
                accounts: Some(10),
                members: Some(3),
                automation: false,
                contacts: None,
                inboxes: Some(1),
                ai_monthly: Some(0),
            },
        }
    }

    #[test]
    fn nothing_is_capped_when_billing_is_off() {
        assert_eq!(effective(false, None), Limits::unlimited());
        assert_eq!(
            effective(false, Some(&sub("canceled"))),
            Limits::unlimited()
        );
    }

    #[test]
    fn a_live_subscription_keeps_its_plan_and_the_rest_may_do_nothing() {
        for s in ["trialing", "active", "past_due"] {
            assert_eq!(effective(true, Some(&sub(s))).accounts, Some(10), "{s}");
        }
        assert_eq!(effective(true, Some(&sub("active"))).inboxes, Some(1));
        assert_eq!(effective(true, Some(&sub("canceled"))), Limits::none());
        assert_eq!(Limits::none().inboxes, Some(0));
        assert_eq!(Limits::unlimited().inboxes, None);
        assert_eq!(effective(true, None), Limits::none());
    }
}
