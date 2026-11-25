//! Team slugs: the address a team picks for itself (`/t/<slug>/...`) beside its permanent code.
//! `PATCH /team/slug` changes it (owner, fresh sign-in), `GET /slugs/check` says whether one is
//! free, and `GET /public/team-slugs/{slug}` tells the web app where a retired one now lives.
//! Every slug a team gives up goes into `team_slug_history`: for 90 days no other team may take
//! it, and for 400 days its old links are redirected by the web app.
use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, DbBackend, DbErr, EntityTrait, QuerySelect, SqlErr,
    Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team::{self, Role};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, CurrentUser, FromWeb},
    teams::{team_by_code, team_json, InTeam, Team},
    waitlist::{error, Error},
};
use crate::{models::teams, rate_limit::ClientIp, state::AppState};

/// A team may give up a slug (change it or clear it) this many times in 30 days.
const CHANGES_PER_30_DAYS: i64 = 3;
/// Written when a team gives up slug `$1` (team `$2`), also when the team is deleted.
pub(super) const RETIRE: &str = "INSERT INTO team_slug_history (slug, team_id) VALUES ($1, $2)";

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn bad_slug(reason: &str) -> Response {
    let body = serde_json::json!({"error": "invalid_slug", "reason": reason});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

/// Whether `slug` cannot be taken by team `team` (`None`: one not made yet): it is some team's
/// code, another team's slug, or another team's slug given up in the last 90 days (a deleted
/// team's too). A team may take back its own.
pub(super) async fn taken(
    db: &impl ConnectionTrait,
    slug: &str,
    team: Option<Uuid>,
) -> Result<bool, DbErr> {
    let sql = "SELECT 1 FROM teams WHERE code = $1 \
               OR (lower(slug) = $1 AND id IS DISTINCT FROM $2::uuid) \
               UNION ALL SELECT 1 FROM team_slug_history WHERE slug = $1 \
               AND team_id IS DISTINCT FROM $2::uuid AND retired_at > now() - interval '90 days' \
               LIMIT 1";
    let values = [slug.into(), team.into()];
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, values);
    Ok(db.query_one_raw(stmt).await?.is_some())
}

#[derive(Deserialize, ToSchema)]
pub struct SlugChange {
    /// The new slug: 3 to 32 of `a-z`, `0-9` and `-` after trimming and lowercasing, a letter
    /// or digit at both ends, no `--`. `null` goes back to the team's code. Required.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    pub slug: Option<String>,
}

/// Sets or clears the current team's slug. Owner only, with a sign-in from the last 10 minutes
/// (else `403 reauth_required`). A bad slug is `422 invalid_slug` with a `reason` (`reserved`,
/// `length`, `characters` or `blocked`); one that is some team's code, another team's slug, or
/// one another team gave up in the last 90 days is `409 slug_taken`. A team gives up a slug
/// (changes or clears it) at most 3 times in 30 days, then `429 slug_changed_too_often`; sending
/// the slug it already has changes nothing and costs nothing. The slug given up stops naming the
/// team for API calls at once, but the web app redirects its links to the new address for 400
/// days.
#[utoipa::path(patch, path = "/team/slug", tag = "teams", request_body = SlugChange,
    responses((status = 200, body = Team), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
async fn change(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<SlugChange>,
) -> Response {
    if t.role != Role::Owner {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    if !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let slug = match body.slug.as_deref().map(team::slug).transpose() {
        Ok(slug) => slug,
        Err(reason) => return bad_slug(reason),
    };
    let saved = async {
        let txn = app.db.begin().await?;
        let id = t.team.id;
        // Locked, so the slug given up below is the one this change replaces.
        let Some(before) = teams::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&txn)
            .await?
        else {
            return Ok(Err((StatusCode::NOT_FOUND, "not_found")));
        };
        if before.slug == slug {
            return Ok(Ok(before));
        }
        if before.slug.is_some() {
            let sql = "SELECT count(*) AS n FROM team_slug_history WHERE team_id = $1 \
                       AND retired_at > now() - interval '30 days'";
            let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, [id.into()]);
            let n: i64 = match txn.query_one_raw(stmt).await? {
                Some(row) => row.try_get("", "n")?,
                None => 0,
            };
            if n >= CHANGES_PER_30_DAYS {
                return Ok(Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "slug_changed_too_often",
                )));
            }
        }
        if let Some(new) = &slug {
            if taken(&txn, new, Some(id)).await? {
                return Ok(Err((StatusCode::CONFLICT, "slug_taken")));
            }
        }
        if let Some(old) = &before.slug {
            let values = [old.into(), id.into()];
            let retire = Statement::from_sql_and_values(DbBackend::Postgres, RETIRE, values);
            txn.execute_raw(retire).await?;
        }
        let row = teams::ActiveModel {
            id: Set(id),
            slug: Set(slug),
            updated_at: Set(chrono::Utc::now().fixed_offset()),
            ..Default::default()
        };
        let team = teams::Entity::update(row).exec(&txn).await?;
        txn.commit().await.map(|()| Ok(team))
    };
    match saved.await {
        Ok(Ok(team)) => Json(team_json(&team, t.role)).into_response(),
        Ok(Err((status, code))) => reply(status, code),
        // Two teams took one slug at once: the unique index let the first through.
        Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
            reply(StatusCode::CONFLICT, "slug_taken")
        }
        Err(e) => internal(e, "team slug change"),
    }
}

#[derive(Deserialize, IntoParams)]
pub struct CheckQuery {
    /// The slug to check, as typed: it is trimmed and lowercased first.
    pub slug: String,
    /// The team (code or slug) the slug is for, one the caller belongs to, so that its own
    /// current slug and the ones it gave up read right. Absent: a team not made yet.
    pub for_team: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SlugCheck {
    /// The slug as it would be stored.
    pub slug: String,
    pub available: bool,
    /// Why not: `reserved`, `length`, `characters`, `blocked`, `taken`, or `current` (it is
    /// already the team's slug). `null` when available.
    pub reason: Option<String>,
}

/// Whether a slug can be taken now, for the live check beside the field. Signed in; at most 120
/// calls a minute per user. It tells what a `PATCH /team/slug` or `POST /teams` would answer
/// (a race still can lose, then that call says `409 slug_taken`).
#[utoipa::path(get, path = "/slugs/check", tag = "teams", params(CheckQuery),
    responses((status = 200, body = SlugCheck), (status = 401, body = Error),
        (status = 404, body = Error), (status = 422, body = Error), (status = 429, body = Error)))]
async fn check(
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    q: Result<Query<CheckQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    if !app.slug_check_limiter.hit(user.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let shown = q.slug.trim().to_ascii_lowercase();
    let answer = |reason: Option<&str>| {
        Json(SlugCheck {
            slug: shown.clone(),
            available: reason.is_none(),
            reason: reason.map(str::to_owned),
        })
        .into_response()
    };
    let found = async {
        let own = match q.for_team.as_deref() {
            Some(named) => match team_by_code(&app.db, user.id, named).await? {
                Some((t, _)) => Some(t),
                None => return Ok(None),
            },
            None => None,
        };
        if own
            .as_ref()
            .is_some_and(|t| t.slug.as_deref() == Some(&shown))
        {
            return Ok(Some(Some("current")));
        }
        if let Err(reason) = team::slug(&shown) {
            return Ok(Some(Some(reason)));
        }
        let own = own.map(|t| t.id);
        Ok::<_, DbErr>(Some(taken(&app.db, &shown, own).await?.then_some("taken")))
    };
    match found.await {
        Ok(Some(reason)) => answer(reason),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "slug check"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct SlugAddress {
    /// What the retired slug's team is called now: its current slug, or its code.
    pub address: String,
}

/// Where the links of a slug a team gave up lead now, for the web app's redirect. Public, and
/// `404` for a slug that is a team's code or current slug, one given up more than 400 days ago,
/// one never used, and one whose team was deleted. Per-IP limit of 600 calls a minute.
#[utoipa::path(get, path = "/public/team-slugs/{slug}", tag = "teams",
    params(("slug" = String, Path)),
    responses((status = 200, body = SlugAddress,
            headers(("Cache-Control" = String, description = "`public, max-age=60`"))),
        (status = 404, body = Error), (status = 429, body = Error)))]
async fn address(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(slug): Path<String>,
) -> Response {
    if !app.slug_lookup_limiter.allow(ip) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let slug = slug.to_ascii_lowercase();
    if slug.len() > 64 {
        return reply(StatusCode::NOT_FOUND, "not_found");
    }
    // The newest retirement wins: the slug may have passed from one team to another. A slug a
    // team holds now, or a code, is that team's address, not a redirect.
    let sql = "SELECT COALESCE(t.slug, t.code) AS address FROM team_slug_history h \
               JOIN teams t ON t.id = h.team_id WHERE h.slug = $1 \
               AND h.retired_at > now() - interval '400 days' \
               AND NOT EXISTS (SELECT 1 FROM teams c WHERE c.code = $1 OR lower(c.slug) = $1) \
               ORDER BY h.retired_at DESC LIMIT 1";
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, [slug.into()]);
    let found = async {
        match app.db.query_one_raw(stmt).await? {
            Some(row) => row.try_get::<String>("", "address").map(Some),
            None => Ok(None),
        }
    };
    match found.await {
        Ok(Some(address)) => (
            [(header::CACHE_CONTROL, "public, max-age=60")],
            Json(SlugAddress { address }),
        )
            .into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "slug address"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(change))
        .routes(routes!(check))
        .routes(routes!(address))
}
