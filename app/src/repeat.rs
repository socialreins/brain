//! Repeating posts. A `scheduled` post may carry a rule (`posts.repeat`); whatever takes it out
//! of `scheduled` to publish it (the tick's claim, or publish now) calls `spawn` in the same
//! transaction, which makes the next copy and clears the rule, so a post yields one copy at most.
//! Each copy is made in a savepoint: one that fails costs only itself, and the team's owners are
//! mailed that the series stopped (`post_mail::Kind::RepeatEnded`). Steps are taken in the set's
//! queue time zone (`queue_schedules`, UTC without one), so a daily 9:00 post stays at 9:00
//! across a daylight saving change.
use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Months, NaiveDate, NaiveDateTime, TimeZone, Utc,
};
use chrono_tz::Tz;
use schemars::JsonSchema;
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, DbErr,
    EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    controllers::posts::MAX_POSTS,
    models::{posts, queue_schedules},
    post_mail,
};

/// How a scheduled post repeats.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct Repeat {
    /// `day`, `week` or `month`. Steps keep the time of day in the set's queue time zone (UTC
    /// without one); a month keeps the day of the month, and a month without that day uses its
    /// last day.
    pub every: String,
    /// Every how many days, weeks or months: 1 to 12.
    pub interval: i32,
    /// `YYYY-MM-DD`: no copy is scheduled after the end of this date in the set's queue time
    /// zone (UTC without one). `null`: no end date.
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
    /// and its rule, none for the last copy of a count. `None` when the rule has ended. Steps
    /// are taken on the wall clock of `zone`.
    pub fn next(
        &self,
        from: DateTime<Utc>,
        now: DateTime<Utc>,
        zone: Tz,
    ) -> Option<(DateTime<Utc>, Option<Self>)> {
        let interval = u32::try_from(self.interval).ok()?;
        let from = from.with_timezone(&zone).naive_local();
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
        let (local, at) = (1..10_000)
            .map_while(step)
            .filter_map(|t| Some((t, on_clock(zone, t)?)))
            .find(|(_, t)| *t > now)?;
        let until = self
            .until
            .as_deref()
            .and_then(|d| d.parse::<NaiveDate>().ok());
        if until.is_some_and(|d| local.date() > d) {
            return None;
        }
        let rule = (self.remaining != Some(1)).then(|| Self {
            remaining: self.remaining.map(|n| n - 1),
            day: Some(day).filter(|d| *d != local.day()),
            ..self.clone()
        });
        Some((at, rule))
    }
}

/// The instant `local` names in `zone`: the earlier one when the clock goes back, an hour on when
/// it springs forward over it.
fn on_clock(zone: Tz, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    let at = |t: NaiveDateTime| zone.from_local_datetime(&t).earliest();
    at(local)
        .or_else(|| at(local + Duration::hours(1)))
        .map(|t| t.to_utc())
}

/// The set's queue time zone, UTC without one (or with one chrono-tz does not know).
async fn zone(txn: &DatabaseTransaction, set: Uuid) -> Result<Tz, DbErr> {
    let row = queue_schedules::Entity::find_by_id(set).one(txn).await?;
    Ok(row
        .and_then(|r| r.time_zone.parse().ok())
        .unwrap_or(Tz::UTC))
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
                post_mail::queue(txn, id, post_mail::Kind::RepeatEnded).await?;
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
    let zone = zone(txn, p.social_set_id).await?;
    let Some((at, rule)) = next.and_then(|at| rule.next(at.to_utc(), now, zone)) else {
        return Ok(());
    };
    let live = posts::Entity::find()
        .filter(posts::Column::TeamId.eq(p.team_id))
        .filter(posts::Column::Status.ne("deleted"))
        .count(txn)
        .await?;
    if live >= MAX_POSTS {
        tracing::warn!(post = %p.id, "team at its post cap: repeat ends");
        return post_mail::queue(txn, p.id, post_mail::Kind::RepeatEnded).await;
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
        let next = |r: &Repeat, now: &str| r.next(from, at(now), Tz::UTC).map(|n| n.0);
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
            .next(
                at("2027-01-31T08:00:00Z"),
                at("2027-01-31T08:00:01Z"),
                Tz::UTC,
            )
            .unwrap();
        assert_eq!(feb, at("2027-02-28T08:00:00Z"));
        let kept = kept.unwrap();
        assert_eq!(kept.day, Some(31));
        assert_eq!(kept.stored()["day"], 31);
        let leap = r
            .next(
                at("2028-01-31T08:00:00Z"),
                at("2028-01-31T08:00:01Z"),
                Tz::UTC,
            )
            .unwrap();
        assert_eq!(leap.0, at("2028-02-29T08:00:00Z"));
        // The copy on February 28 goes back to the 31st.
        let (mar, kept) = kept.next(feb, feb, Tz::UTC).unwrap();
        assert_eq!(mar, at("2027-03-31T08:00:00Z"));
        assert_eq!(kept.unwrap().day, None);
        // A post moved to another day follows that day.
        let moved = Repeat {
            day: Some(31),
            ..rule("month", 1)
        };
        let from = at("2027-02-10T08:00:00Z");
        assert_eq!(
            moved.next(from, from, Tz::UTC).unwrap().0,
            at("2027-03-10T08:00:00Z")
        );
    }

    #[test]
    fn steps_keep_the_wall_clock_of_the_zone_across_daylight_saving() {
        let york: Tz = "America/New_York".parse().unwrap();
        // 9:00 in New York the day before clocks spring forward (14:00 UTC), then 9:00 EDT.
        let from = at("2026-03-07T14:00:00Z");
        let (next, _) = rule("day", 1).next(from, from, york).unwrap();
        assert_eq!(next, at("2026-03-08T13:00:00Z"));
        // In UTC the same rule keeps 14:00 UTC, an hour off the wall clock.
        let (utc, _) = rule("day", 1).next(from, from, Tz::UTC).unwrap();
        assert_eq!(utc, at("2026-03-08T14:00:00Z"));
        // Back in November, and a week rule too.
        let from = at("2026-10-31T13:00:00Z");
        let (next, _) = rule("week", 1).next(from, from, york).unwrap();
        assert_eq!(next, at("2026-11-07T14:00:00Z"));
        // 2:30 does not exist on the day clocks spring forward: an hour on.
        let from = at("2026-03-07T07:30:00Z");
        let (next, _) = rule("day", 1).next(from, from, york).unwrap();
        assert_eq!(next, at("2026-03-08T07:30:00Z"));
        // `until` is a date on the zone's calendar: 22:00 on the 8th in New York is the 9th in UTC.
        let from = at("2026-03-08T03:00:00Z");
        let mut r = rule("day", 1);
        r.until = Some("2026-03-08".into());
        assert_eq!(
            r.next(from, from, york).unwrap().0,
            at("2026-03-09T02:00:00Z")
        );
        assert_eq!(r.next(from, from, Tz::UTC), None);
    }

    #[test]
    fn remaining_and_until_end_the_rule() {
        let from = at("2026-03-10T09:30:00Z");
        let mut r = rule("day", 1);
        r.remaining = Some(2);
        let (_, copy) = r.next(from, from, Tz::UTC).unwrap();
        let copy = copy.unwrap();
        assert_eq!(copy.remaining, Some(1));
        // The last of the count is still made, with no rule of its own.
        let last = copy.next(from, from, Tz::UTC).unwrap();
        assert_eq!(last, (at("2026-03-11T09:30:00Z"), None));
        let mut r = rule("day", 1);
        r.until = Some("2026-03-11".into());
        assert!(
            r.next(from, from, Tz::UTC).is_some(),
            "the end date itself counts"
        );
        r.until = Some("2026-03-10".into());
        assert_eq!(r.next(from, from, Tz::UTC), None);
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
