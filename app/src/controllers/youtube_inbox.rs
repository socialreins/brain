//! The YouTube inbox's settings. Reading comments uses YouTube quota (the Google project's
//! 10,000 units a day are shared by every team), so `PUT /connections/{id}/youtube-inbox` is where
//! a team turns it on for one channel and sets how often it is read and how many units a day it
//! may spend (`youtube_inbox`). Owners and admins change settings; every member can read them.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    models::social_accounts,
    state::AppState,
    youtube_inbox::{self, Choice, Settings},
};

#[derive(Serialize, ToSchema)]
pub struct Limits {
    pub min_poll_minutes: i32,
    pub max_poll_minutes: i32,
    pub default_poll_minutes: i32,
    pub min_daily_units: i32,
    pub max_daily_units: i32,
    pub default_daily_units: i32,
}

/// A YouTube channel's inbox settings and state.
#[derive(Serialize, ToSchema)]
pub struct YouTubeInbox {
    /// Off until a person turns it on: nothing is read, and no quota used, before that.
    pub enabled: bool,
    /// Minutes between reads.
    pub poll_minutes: i32,
    /// Quota units this channel may spend reading in one Pacific day.
    pub daily_units: i32,
    /// Spent reading today (Pacific time).
    pub units_today: i32,
    /// Spent today on replies, moderation and deletes (50 units each): shown, not limited.
    pub write_units_today: i32,
    /// Why polling stopped: `budget`, `quota` or `errors`; empty when it runs.
    pub paused: Option<String>,
    /// What the pause means and what to do.
    pub message: Option<String>,
    pub last_error: Option<String>,
    pub last_read_at: Option<DateTime<Utc>>,
    /// YouTube refused the channel's token: connect it again.
    pub reconnect: bool,
    /// The channel was connected without the permission comments need (`youtube.force-ssl`):
    /// connect it again. False when it has it, or when the grant was not recorded.
    pub reconnect_for_comments: bool,
    /// Every channel together has used the share of YouTube's quota the inbox may use today, so
    /// nothing is read until midnight Pacific time.
    pub shared_exhausted: bool,
    /// What reading costs: about this many units a day when nothing new comes.
    pub idle_units_per_day: i32,
    /// What a reply, a moderation call or a delete costs.
    pub write_units: i32,
    pub limits: Limits,
}

/// What a person sets. Missing fields keep their value (the defaults for a channel that never
/// had any).
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct YouTubeInboxInput {
    pub enabled: Option<bool>,
    /// 10 to 1440.
    pub poll_minutes: Option<i32>,
    /// 20 to 2000.
    pub daily_units: Option<i32>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn no_comment_scope(account: &social_accounts::Model) -> bool {
    account.meta["scope"]
        .as_str()
        .is_some_and(|s| !s.split(' ').any(|g| g.ends_with("/youtube.force-ssl")))
}

async fn json(
    app: &AppState,
    account: &social_accounts::Model,
    s: &Settings,
) -> Result<YouTubeInbox, sea_orm::DbErr> {
    Ok(YouTubeInbox {
        enabled: s.enabled,
        poll_minutes: s.poll_minutes,
        daily_units: s.daily_units,
        units_today: s.units_today,
        write_units_today: s.write_units_today,
        paused: s.paused_reason.clone(),
        message: s
            .paused_reason
            .as_deref()
            .map(|r| youtube_inbox::pause_message(r, s.last_error.as_deref())),
        last_error: s.last_error.clone(),
        last_read_at: s.last_read_at,
        reconnect: account.meta["reconnect"].as_bool() == Some(true),
        reconnect_for_comments: no_comment_scope(account),
        shared_exhausted: youtube_inbox::shared_used(&app.db).await? >= app.youtube.inbox_units,
        idle_units_per_day: 1440 / s.poll_minutes.max(1),
        write_units: youtube_inbox::WRITE_UNITS,
        limits: Limits {
            min_poll_minutes: youtube_inbox::MIN_POLL_MINUTES,
            max_poll_minutes: youtube_inbox::MAX_POLL_MINUTES,
            default_poll_minutes: youtube_inbox::DEFAULT_POLL_MINUTES,
            min_daily_units: youtube_inbox::MIN_DAILY_UNITS,
            max_daily_units: youtube_inbox::MAX_DAILY_UNITS,
            default_daily_units: youtube_inbox::DEFAULT_DAILY_UNITS,
        },
    })
}

async fn account_of(
    app: &AppState,
    team: Uuid,
    id: Uuid,
) -> Result<Option<social_accounts::Model>, sea_orm::DbErr> {
    social_accounts::Entity::find_by_id(id)
        .filter(social_accounts::Column::TeamId.eq(team))
        .filter(social_accounts::Column::Provider.eq("youtube"))
        .one(&app.db)
        .await
}

/// The YouTube inbox settings of a channel: off, every 30 minutes and 300 units a day until set.
/// Any member.
#[utoipa::path(get, path = "/connections/{id}/youtube-inbox", tag = "inbox",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = YouTubeInbox), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, Path(id): Path<Uuid>) -> Response {
    let found = async {
        let Some(account) = account_of(&app, t.team.id, id).await? else {
            return Ok(None);
        };
        let s = youtube_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        Ok::<_, sea_orm::DbErr>(Some(json(&app, &account, &s).await?))
    };
    match found.await {
        Ok(Some(j)) => Json(j).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "youtube inbox read"),
    }
}

/// Sets the settings. Owners and admins. `409 reconnect_required` when turning it on for a
/// channel YouTube refused, `422 reconnect_for_comments` for one connected without the comments
/// permission, `422` `invalid_poll` (10 to 1440 minutes) or `invalid_budget` (20 to 2000 units).
#[utoipa::path(put, path = "/connections/{id}/youtube-inbox", tag = "inbox",
    request_body = YouTubeInboxInput,
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = YouTubeInbox), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<YouTubeInboxInput>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let done = async {
        let Some(account) = account_of(&app, t.team.id, id).await? else {
            return Ok(reply(StatusCode::NOT_FOUND, "not_found"));
        };
        let was = youtube_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        let choice = Choice {
            enabled: input.enabled.unwrap_or(was.enabled),
            poll_minutes: input.poll_minutes.unwrap_or(was.poll_minutes),
            daily_units: input.daily_units.unwrap_or(was.daily_units),
        };
        if let Err(code) = choice.checked() {
            return Ok(reply(StatusCode::UNPROCESSABLE_ENTITY, code));
        }
        if choice.enabled && no_comment_scope(&account) {
            return Ok(reply(
                StatusCode::UNPROCESSABLE_ENTITY,
                "reconnect_for_comments",
            ));
        }
        if choice.enabled && !was.enabled && account.meta["reconnect"].as_bool() == Some(true) {
            return Ok(reply(StatusCode::CONFLICT, "reconnect_required"));
        }
        youtube_inbox::save(&app.db, id, &choice).await?;
        let s = youtube_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        Ok::<_, sea_orm::DbErr>(Json(json(&app, &account, &s).await?).into_response())
    };
    done.await
        .unwrap_or_else(|e| internal(e, "youtube inbox save"))
}

/// Starts reading again after a pause (`errors`, or a `budget` or `quota` pause before the day
/// turns). Owners and admins. It reads at the next minute and never past the day's budget.
#[utoipa::path(post, path = "/connections/{id}/youtube-inbox/resume", tag = "inbox",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = YouTubeInbox), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn resume(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let done = async {
        let Some(account) = account_of(&app, t.team.id, id).await? else {
            return Ok(None);
        };
        youtube_inbox::resume(&app.db, id).await?;
        let s = youtube_inbox::load(&app.db, id)
            .await?
            .unwrap_or_else(Settings::off);
        Ok::<_, sea_orm::DbErr>(Some(json(&app, &account, &s).await?))
    };
    match done.await {
        Ok(Some(j)) => Json(j).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "youtube inbox resume"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(show, update))
        .routes(routes!(resume))
}
