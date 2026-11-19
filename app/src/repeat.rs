//! Repeating posts. A `scheduled` post may carry a rule (`posts.repeat`); whatever takes it out
//! of `scheduled` to publish it (the tick's claim, or publish now) calls `spawn` in the same
//! transaction, which makes the next copy and clears the rule, so a post yields one copy at most.
//! Each copy is made in a savepoint: one that fails is logged and costs only itself.
use chrono::{DateTime, Datelike, Duration, FixedOffset, Months, NaiveDate, Utc};
use schemars::JsonSchema;
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, DbErr,
    EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{controllers::posts::MAX_POSTS, models::posts};

/// How a scheduled post repeats.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct Repeat {
    /// `day`, `week` or `month`. A month keeps the day of the month and the time in UTC; a
    /// month without that day uses its last day.
    pub every: String,
    /// Every how many days, weeks or months: 1 to 12.
    pub interval: i32,
    /// `YYYY-MM-DD`: no copy is scheduled after the end of this UTC date. `null`: no end date.
    pub until: Option<String>,
    /// How many more copies will be made after this post, 1 to 100: the copy carries one less,
    /// and the copy of a post with 1 carries no rule. `null`: no count limit.
    pub remaining: Option<i32>,
    /// The day of the month a monthly rule aims for, kept by a copy a short month moved
    /// earlier (the 31st, on a copy that landed on February 28). Never read from a request.
    #[serde(default, skip_serializing)]
    #[schema(ignore)]
    #[schemars(skip)]
    pub day: Option<u32>,
}

impl Repeat {
    /// The first field that is out of range.
    pub fn invalid(&self) -> Option<&'static str> {
        let until = self.until.as_deref().map(str::parse::<NaiveDate>);
        if !["day", "week", "month"].contains(&self.every.as_str()) {
            Some("every")
        } else if !(1..=12).contains(&self.interval) {
            Some("interval")
        } else if until.is_some_and(|d| d.is_err()) {
            Some("until")
        } else if self.remaining.is_some_and(|n| !(1..=100).contains(&n)) {
            Some("remaining")
        } else {
            None
        }
    }

    /// The rule as stored, `day` included.
    pub fn stored(&self) -> serde_json::Value {
        let mut v = serde_json::json!(self);
        if let Some(day) = self.day {
            v["day"] = day.into();
        }
        v
    }

    /// The copy a post at `from` yields: its time, the first step of the rule after `now`,
    /// and its rule, none for the last copy of a count. `None` when the rule has ended.
    pub fn next(
        &self,
        from: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, Option<Self>)> {
        let interval = u32::try_from(self.interval).ok()?;
        // The day a monthly rule aims for: the post's, or the one a short month cut it from.
        let clamped = (from + Duration::days(1)).day() == 1;
        let day = self.day.filter(|d| clamped && *d > from.day());
        let day = day.unwrap_or(from.day());
        let step = |k: u32| match self.every.as_str() {
            "month" => {
                let first = from.with_day(1)?;
                let month = first.checked_add_months(Months::new(k * interval))?;
                (1..=day).rev().find_map(|d| month.with_day(d))
            }
            unit => {
                let days = if unit == "week" { 7 } else { 1 };
                from.checked_add_signed(Duration::days(i64::from(k * interval * days)))
            }
        };
        let at = (1..10_000).map_while(step).find(|t| *t > now)?;
        let until = self
            .until
            .as_deref()
            .and_then(|d| d.parse::<NaiveDate>().ok());
        if until.is_some_and(|d| at.date_naive() > d) {
            return None;
        }
        let rule = (self.remaining != Some(1)).then(|| Self {
            remaining: self.remaining.map(|n| n - 1),
            day: Some(day).filter(|d| *d != at.day()),
            ..self.clone()
        });
        Some((at, rule))
    }
}

/// Makes the next copy of each post of `ids` that has a rule, and clears the rule. `txn` is the
/// transaction that takes the posts out of `scheduled`, so each yields its copy once. `from` is
/// the time the series counts from when the claim overwrote `scheduled_at` (publish now).
///
/// The copy is a new `scheduled` post with the source's content and approval, in no list and
/// with no page. Copies of one group share one new group id, derived from the old one.
pub async fn spawn(
    txn: &DatabaseTransaction,
    ids: &[Uuid],
    from: Option<DateTime<FixedOffset>>,
) -> Result<(), DbErr> {
    let rows = posts::Entity::find()
        .filter(posts::Column::Id.is_in(ids.iter().copied()))
        .filter(posts::Column::Repeat.is_not_null())
        .all(txn)
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    posts::Entity::update_many()
        .col_expr(
            posts::Column::Repeat,
            Expr::value(None::<serde_json::Value>),
        )
        .filter(posts::Column::Id.is_in(rows.iter().map(|p| p.id)))
        .exec(txn)
        .await?;
    let now = Utc::now();
    for p in rows {
        // A savepoint: a copy that fails is undone alone, and the claim goes on without it.
        let id = p.id;
        let point = txn.begin().await?;
        match copy(&point, p, from, now).await {
            Ok(()) => point.commit().await?,
            Err(e) => {
                tracing::warn!(error = %e, post = %id, "repeat copy failed: the series ends");
                point.rollback().await?;
            }
        }
    }
    Ok(())
}

/// The next copy of `p` by its rule, if the rule yields one.
async fn copy(
    txn: &DatabaseTransaction,
    p: posts::Model,
    from: Option<DateTime<FixedOffset>>,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    let rule: Repeat = serde_json::from_value(p.repeat.clone().unwrap_or_default())
        .map_err(|e| DbErr::Custom(format!("stored repeat rule: {e}")))?;
    // TikTok wants a person to confirm every post.
    let next = from.or(p.scheduled_at).filter(|_| p.platform != "tiktok");
    let Some((at, rule)) = next.and_then(|at| rule.next(at.to_utc(), now)) else {
        return Ok(());
    };
    let live = posts::Entity::find()
        .filter(posts::Column::TeamId.eq(p.team_id))
        .filter(posts::Column::Status.ne("deleted"))
        .count(txn)
        .await?;
    if live >= MAX_POSTS {
        tracing::warn!(post = %p.id, "team at its post cap: repeat ends");
        return Ok(());
    }
    let copy = posts::ActiveModel {
        id: Set(Uuid::now_v7()),
        status: Set("scheduled".into()),
        scheduled_at: Set(Some(at.fixed_offset())),
        published_at: Set(None),
        error: Set(None),
        quota_reserved: Set(false),
        fanned_out: Set(false),
        group_id: Set(p.group_id.map(|g| Uuid::new_v5(&g, b"repeat"))),
        repeat: Set(rule.map(|r| r.stored())),
        created_at: Set(now.fixed_offset()),
        updated_at: Set(now.fixed_offset()),
        ..p.into_active_model().reset_all()
    };
    posts::Entity::insert(copy).exec(txn).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn rule(every: &str, interval: i32) -> Repeat {
        Repeat {
            every: every.into(),
            interval,
            until: None,
            remaining: None,
            day: None,
        }
    }

    #[test]
    fn steps_are_exact_and_skip_the_past() {
        let from = at("2026-03-10T09:30:00Z");
        let next = |r: &Repeat, now: &str| r.next(from, at(now)).map(|n| n.0);
        let soon = "2026-03-10T09:31:00Z";
        assert_eq!(
            next(&rule("day", 3), soon),
            Some(at("2026-03-13T09:30:00Z"))
        );
        assert_eq!(
            next(&rule("week", 2), soon),
            Some(at("2026-03-24T09:30:00Z"))
        );
        assert_eq!(
            next(&rule("month", 1), soon),
            Some(at("2026-04-10T09:30:00Z"))
        );
        // Claimed late: the first step still ahead, never one in the past.
        let late = "2026-03-20T00:00:00Z";
        assert_eq!(
            next(&rule("day", 3), late),
            Some(at("2026-03-22T09:30:00Z"))
        );
        assert_eq!(
            next(&rule("week", 1), late),
            Some(at("2026-03-24T09:30:00Z"))
        );
    }

    #[test]
    fn a_month_clamps_to_its_last_day_and_returns_to_the_day() {
        let r = rule("month", 1);
        let (feb, kept) = r
            .next(at("2027-01-31T08:00:00Z"), at("2027-01-31T08:00:01Z"))
            .unwrap();
        assert_eq!(feb, at("2027-02-28T08:00:00Z"));
        let kept = kept.unwrap();
        assert_eq!(kept.day, Some(31));
        assert_eq!(kept.stored()["day"], 31);
        let leap = r
            .next(at("2028-01-31T08:00:00Z"), at("2028-01-31T08:00:01Z"))
            .unwrap();
        assert_eq!(leap.0, at("2028-02-29T08:00:00Z"));
        // The copy on February 28 goes back to the 31st.
        let (mar, kept) = kept.next(feb, feb).unwrap();
        assert_eq!(mar, at("2027-03-31T08:00:00Z"));
        assert_eq!(kept.unwrap().day, None);
        // A post moved to another day follows that day.
        let moved = Repeat {
            day: Some(31),
            ..rule("month", 1)
        };
        let from = at("2027-02-10T08:00:00Z");
        assert_eq!(
            moved.next(from, from).unwrap().0,
            at("2027-03-10T08:00:00Z")
        );
    }

    #[test]
    fn remaining_and_until_end_the_rule() {
        let from = at("2026-03-10T09:30:00Z");
        let mut r = rule("day", 1);
        r.remaining = Some(2);
        let (_, copy) = r.next(from, from).unwrap();
        let copy = copy.unwrap();
        assert_eq!(copy.remaining, Some(1));
        // The last of the count is still made, with no rule of its own.
        let last = copy.next(from, from).unwrap();
        assert_eq!(last, (at("2026-03-11T09:30:00Z"), None));
        let mut r = rule("day", 1);
        r.until = Some("2026-03-11".into());
        assert!(r.next(from, from).is_some(), "the end date itself counts");
        r.until = Some("2026-03-10".into());
        assert_eq!(r.next(from, from), None);
    }

    #[test]
    fn fields_out_of_range_are_named() {
        let bad = |f: fn(&mut Repeat)| {
            let mut r = rule("day", 1);
            f(&mut r);
            r.invalid()
        };
        assert_eq!(rule("month", 12).invalid(), None);
        assert_eq!(bad(|r| r.every = "year".into()), Some("every"));
        assert_eq!(bad(|r| r.interval = 0), Some("interval"));
        assert_eq!(bad(|r| r.interval = 13), Some("interval"));
        assert_eq!(bad(|r| r.until = Some("2026-02-30".into())), Some("until"));
        assert_eq!(bad(|r| r.remaining = Some(0)), Some("remaining"));
        assert_eq!(bad(|r| r.remaining = Some(101)), Some("remaining"));
    }
}
