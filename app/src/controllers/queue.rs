//! The posting queue of a social set: a weekly schedule of posting times in an IANA zone. Every
//! member reads and replaces it, as with posts; a set token only its own set's. `POST /posts`
//! and `POST /post-groups` with `queue: true` take the earliest free slot (`pick`).
use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use schemars::JsonSchema;
use sea_orm::{
    sea_query::OnConflict, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr,
    EntityTrait, QueryFilter, QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body},
    waitlist::{error, Error},
};
use crate::{
    models::{posts, queue_schedules, social_sets},
    state::AppState,
};

/// Most slots one set's schedule holds.
const MAX_SLOTS: usize = 50;
/// How many free slot times `GET` lists.
const NEXT: usize = 10;
const DAY: i32 = 1440;

#[derive(Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct Slot {
    /// 0 is Sunday, 6 Saturday.
    pub weekday: u8,
    /// `HH:MM`, 24-hour, in the schedule's zone.
    pub time: String,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct QueueInput {
    /// An IANA zone such as `Asia/Dhaka`; absent means `UTC`.
    pub time_zone: Option<String>,
    /// At most 50, each weekday and time once.
    #[serde(default)]
    pub slots: Vec<Slot>,
}

#[derive(Serialize, ToSchema)]
pub struct Queue {
    pub time_zone: String,
    /// By weekday, then time.
    pub slots: Vec<Slot>,
    /// The next 10 slot times that are in the future and hold no scheduled post of the set.
    pub next: Vec<DateTime<Utc>>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn invalid(code: &str) -> Response {
    reply(StatusCode::UNPROCESSABLE_ENTITY, code)
}

/// The free slot times after `now`, earliest first, at most `n`. A slot that does not exist on
/// a day (a spring-forward gap) is skipped; an ambiguous one (fall back) is its first occurrence.
fn upcoming(
    tz: Tz,
    slots: &[i32],
    now: DateTime<Utc>,
    taken: &HashSet<DateTime<Utc>>,
    n: usize,
) -> Vec<DateTime<Utc>> {
    let mut out = Vec::new();
    if slots.is_empty() {
        return out;
    }
    let today = now.with_timezone(&tz).date_naive();
    // Every week has a slot, so this many days always finds `n`, gaps and taken ones included.
    let days = 7 * 2 * (taken.len() + n + 1) as i64 + 14;
    for d in 0..days {
        let date = today + Duration::days(d);
        let weekday = date.weekday().num_days_from_sunday() as i32;
        for s in slots.iter().filter(|s| *s / DAY == weekday) {
            let m = (s % DAY) as u32;
            let Some(time) = NaiveTime::from_hms_opt(m / 60, m % 60, 0) else {
                continue;
            };
            let Some(at) = tz.from_local_datetime(&date.and_time(time)).earliest() else {
                continue;
            };
            let at = at.with_timezone(&Utc);
            if at > now && !taken.contains(&at) {
                out.push(at);
                if out.len() == n {
                    return out;
                }
            }
        }
    }
    out
}

/// The set's scheduled posts' times after `now`.
async fn taken(
    db: &impl ConnectionTrait,
    set: Uuid,
    now: DateTime<Utc>,
) -> Result<HashSet<DateTime<Utc>>, DbErr> {
    let times: Vec<Option<DateTime<chrono::FixedOffset>>> = posts::Entity::find()
        .select_only()
        .column(posts::Column::ScheduledAt)
        .filter(posts::Column::SocialSetId.eq(set))
        .filter(posts::Column::Status.eq("scheduled"))
        .filter(posts::Column::ScheduledAt.gt(now.fixed_offset()))
        .into_tuple()
        .all(db)
        .await?;
    Ok(times.into_iter().flatten().map(|t| t.to_utc()).collect())
}

fn stored_slots(row: &queue_schedules::Model) -> Vec<i32> {
    serde_json::from_value(row.slots.clone()).unwrap_or_default()
}

async fn schedule(db: &impl ConnectionTrait, set: Uuid) -> Result<Option<(Tz, Vec<i32>)>, DbErr> {
    let row = queue_schedules::Entity::find_by_id(set).one(db).await?;
    Ok(row.and_then(|r| Some((r.time_zone.parse().ok()?, stored_slots(&r)))))
}

/// Whether the set's queue has a slot.
pub(crate) async fn has_slots(db: &impl ConnectionTrait, set: Uuid) -> Result<bool, DbErr> {
    Ok(schedule(db, set).await?.is_some_and(|(_, s)| !s.is_empty()))
}

/// The earliest free slot of the set, under a transaction-level advisory lock per set, so two
/// queue calls never get the same one: the lock is held until `txn` commits the post. `None`
/// when the queue has no slot.
pub(crate) async fn pick(
    txn: &impl ConnectionTrait,
    set: Uuid,
) -> Result<Option<DateTime<Utc>>, DbErr> {
    let sql = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";
    let key = format!("queue:{set}");
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        [key.into()],
    ))
    .await?;
    let Some((tz, slots)) = schedule(txn, set).await? else {
        return Ok(None);
    };
    let now = Utc::now();
    let taken = taken(txn, set, now).await?;
    Ok(upcoming(tz, &slots, now, &taken, 1).first().copied())
}

/// The caller's live set `id`: the team's, and a set token's own.
async fn set_of(app: &AppState, a: &Actor, id: Uuid) -> Result<bool, DbErr> {
    if a.set_id().is_some_and(|own| own != id) {
        return Ok(false);
    }
    let set = social_sets::Entity::find_by_id(id)
        .filter(social_sets::Column::TeamId.eq(a.team.id))
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(&app.db)
        .await?;
    Ok(set.is_some())
}

async fn queue_json(db: &impl ConnectionTrait, set: Uuid) -> Result<Queue, DbErr> {
    let row = queue_schedules::Entity::find_by_id(set).one(db).await?;
    let (zone, slots) = match &row {
        Some(r) => (r.time_zone.clone(), stored_slots(r)),
        None => ("UTC".to_owned(), Vec::new()),
    };
    let tz: Tz = zone.parse().unwrap_or(Tz::UTC);
    let now = Utc::now();
    let next = upcoming(tz, &slots, now, &taken(db, set, now).await?, NEXT);
    let slots = slots
        .iter()
        .map(|s| Slot {
            weekday: (s / DAY) as u8,
            time: format!("{:02}:{:02}", s % DAY / 60, s % 60),
        })
        .collect();
    Ok(Queue {
        time_zone: zone,
        slots,
        next,
    })
}

/// `HH:MM` as a minute of the day.
fn minute_of_day(time: &str) -> Option<i32> {
    let (h, m) = time.split_once(':')?;
    let digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(h) || !digits(m) {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// The set's queue: its zone (`UTC` until one is saved), its slots and the next 10 free slot
/// times.
#[utoipa::path(get, path = "/social-sets/{id}/queue", tag = "social-sets",
    params(("id" = Uuid, Path)), security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = Queue), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    get_queue(&app, &a, id).await
}

/// The body of `GET /social-sets/{id}/queue`, shared with the MCP `get-queue` tool.
pub async fn get_queue(app: &AppState, a: &Actor, id: Uuid) -> Response {
    let found = async {
        if !set_of(app, a, id).await? {
            return Ok(reply(StatusCode::NOT_FOUND, "not_found"));
        }
        Ok::<_, DbErr>(Json(queue_json(&app.db, id).await?).into_response())
    };
    found.await.unwrap_or_else(|e| internal(e, "queue read"))
}

/// Replaces the set's zone and slots. `422`: `invalid_time_zone` (not an IANA zone),
/// `invalid_slot` (a weekday over 6 or a time not `HH:MM`), `duplicate_slot`, `too_many_slots`
/// (over 50). Posts already scheduled keep their times.
#[utoipa::path(put, path = "/social-sets/{id}/queue", tag = "social-sets",
    params(("id" = Uuid, Path)), request_body = QueueInput,
    security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = Queue), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn replace(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(body): Body<QueueInput>,
) -> Response {
    set_queue(&app, &a, id, body).await
}

/// The body of `PUT /social-sets/{id}/queue`, shared with the MCP `set-queue` tool.
pub async fn set_queue(app: &AppState, a: &Actor, id: Uuid, input: QueueInput) -> Response {
    let zone = input.time_zone.unwrap_or_else(|| "UTC".into());
    if zone.parse::<Tz>().is_err() {
        return invalid("invalid_time_zone");
    }
    if input.slots.len() > MAX_SLOTS {
        return invalid("too_many_slots");
    }
    let mut slots = Vec::with_capacity(input.slots.len());
    for s in &input.slots {
        match minute_of_day(&s.time) {
            Some(m) if s.weekday <= 6 => slots.push(i32::from(s.weekday) * DAY + m),
            _ => return invalid("invalid_slot"),
        }
    }
    slots.sort_unstable();
    let before = slots.len();
    slots.dedup();
    if slots.len() != before {
        return invalid("duplicate_slot");
    }
    let saved = async {
        if !set_of(app, a, id).await? {
            return Ok(reply(StatusCode::NOT_FOUND, "not_found"));
        }
        let row = queue_schedules::ActiveModel {
            social_set_id: Set(id),
            team_id: Set(a.team.id),
            time_zone: Set(zone),
            slots: Set(serde_json::json!(slots)),
            updated_at: Set(Utc::now().fixed_offset()),
        };
        let upsert = OnConflict::column(queue_schedules::Column::SocialSetId)
            .update_columns([
                queue_schedules::Column::TimeZone,
                queue_schedules::Column::Slots,
                queue_schedules::Column::UpdatedAt,
            ])
            .to_owned();
        queue_schedules::Entity::insert(row)
            .on_conflict(upsert)
            .exec_without_returning(&app.db)
            .await?;
        Ok::<_, DbErr>(Json(queue_json(&app.db, id).await?).into_response())
    };
    saved.await.unwrap_or_else(|e| internal(e, "queue save"))
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(show, replace))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn at(weekday: i32, time: &str) -> i32 {
        weekday * DAY + minute_of_day(time).unwrap()
    }

    #[test]
    fn times_parse_as_hh_mm_only() {
        assert_eq!(minute_of_day("00:00"), Some(0));
        assert_eq!(minute_of_day("17:45"), Some(17 * 60 + 45));
        assert_eq!(minute_of_day("23:59"), Some(1439));
        for bad in [
            "24:00", "9:00", "09:60", "09:5", "0900", "aa:bb", "+9:00", "",
        ] {
            assert_eq!(minute_of_day(bad), None, "{bad}");
        }
    }

    #[test]
    fn past_and_taken_slots_are_skipped() {
        let tz: Tz = "Asia/Dhaka".parse().unwrap();
        // Wednesday 2026-11-18 10:00 in Dhaka (UTC+6).
        let now = utc("2026-11-18T04:00:00Z");
        let slots = [at(3, "09:00"), at(3, "17:45"), at(4, "09:00")];
        let taken = HashSet::from([utc("2026-11-18T11:45:00Z")]);
        let next = upcoming(tz, &slots, now, &taken, 3);
        assert_eq!(
            next,
            [
                utc("2026-11-19T03:00:00Z"),
                utc("2026-11-25T03:00:00Z"),
                utc("2026-11-25T11:45:00Z"),
            ]
        );
    }

    #[test]
    fn a_slot_in_the_spring_forward_gap_is_skipped() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // Saturday 2027-03-13; clocks go from 02:00 to 03:00 on Sunday 2027-03-14.
        let now = utc("2027-03-13T12:00:00Z");
        let slots = [at(0, "02:30"), at(0, "03:30")];
        let next = upcoming(tz, &slots, now, &HashSet::new(), 2);
        // 03:30 EDT is 07:30Z; the next 02:30 is a week on, EDT.
        assert_eq!(
            next,
            [utc("2027-03-14T07:30:00Z"), utc("2027-03-21T06:30:00Z")]
        );
    }

    #[test]
    fn an_ambiguous_fall_back_slot_is_its_first_occurrence() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // Clocks go from 02:00 EDT back to 01:00 EST on Sunday 2026-11-01.
        let now = utc("2026-10-31T12:00:00Z");
        let next = upcoming(tz, &[at(0, "01:30")], now, &HashSet::new(), 2);
        // 01:30 EDT is 05:30Z (not 06:30Z, the EST one).
        assert_eq!(
            next,
            [utc("2026-11-01T05:30:00Z"), utc("2026-11-08T06:30:00Z")]
        );
        // Once the first is past, the second 01:30 that day is not taken either.
        let later = utc("2026-11-01T06:00:00Z");
        let next = upcoming(tz, &[at(0, "01:30")], later, &HashSet::new(), 1);
        assert_eq!(next, [utc("2026-11-08T06:30:00Z")]);
    }

    #[test]
    fn a_full_horizon_still_ends() {
        let tz = Tz::UTC;
        let now = utc("2026-11-18T00:00:00Z");
        let all: HashSet<_> = upcoming(tz, &[at(1, "09:00")], now, &HashSet::new(), 100)
            .into_iter()
            .collect();
        let next = upcoming(tz, &[at(1, "09:00")], now, &all, 2);
        assert_eq!(next.len(), 2);
        assert!(next.iter().all(|t| !all.contains(t)));
    }
}
