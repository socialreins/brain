//! Analytics of the current team: the metrics `metrics.rs` keeps for published posts, summed
//! over a range of days. Any member reads it, and so does a token (a set token for its set).
use std::collections::BTreeMap;

use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration, NaiveDate, NaiveTime, Utc};
use schemars::JsonSchema;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::internal,
    posts::{self, Post},
    waitlist::{error, Error},
};
use crate::{metrics::SUPPORTED, models::posts as post_rows, state::AppState};

/// Most days one call covers.
const MAX_DAYS: i64 = 366;
const TOP: usize = 10;

#[derive(Deserialize, IntoParams, JsonSchema)]
pub struct AnalyticsQuery {
    /// The posts of one set; absent or the default set, which holds every account, gives every
    /// set's.
    pub social_set_id: Option<Uuid>,
    /// The first day, `YYYY-MM-DD` in UTC, by when a post was published. Default: 29 days
    /// before `to`.
    pub from: Option<String>,
    /// The last day, `YYYY-MM-DD` in UTC. Default: today. At most 366 days from `from`.
    pub to: Option<String>,
}

/// Sums over posts; a number a platform does not give counts as 0.
#[derive(Clone, Copy, Default, Serialize, ToSchema)]
pub struct Totals {
    /// Published posts, on every platform.
    pub posts: i64,
    pub likes: i64,
    pub comments: i64,
    pub shares: i64,
    pub views: i64,
}

#[derive(Serialize, ToSchema)]
pub struct PlatformTotals {
    pub platform: String,
    #[serde(flatten)]
    pub totals: Totals,
    /// `false`: the platform gives no metrics, so its numbers are 0.
    pub supported: bool,
}

#[derive(Serialize, ToSchema)]
pub struct DayTotals {
    /// `YYYY-MM-DD` in UTC.
    pub date: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Serialize, ToSchema)]
pub struct Analytics {
    /// The platforms metrics are read from.
    pub supported: Vec<String>,
    pub totals: Totals,
    /// Every platform with a published post in the range, by name.
    pub by_platform: Vec<PlatformTotals>,
    /// Every day of the range, oldest first, by when the post was published.
    pub by_day: Vec<DayTotals>,
    /// Up to 10 posts with the most likes, comments and shares together, the most first; a
    /// post with none is left out.
    pub top_posts: Vec<Post>,
}

fn invalid_range() -> Response {
    error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_range").into_response()
}

/// The first and last day asked for; `None` for a date that is not one, a range that ends
/// before it starts or one over `MAX_DAYS`.
fn range(q: &AnalyticsQuery) -> Option<(NaiveDate, NaiveDate)> {
    let day = |s: &String| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    let to = match &q.to {
        Some(s) => day(s)?,
        None => Utc::now().date_naive(),
    };
    let from = match &q.from {
        Some(s) => day(s)?,
        None => to - Duration::days(29),
    };
    let days = (to - from).num_days() + 1;
    (1..=MAX_DAYS).contains(&days).then_some((from, to))
}

/// Totals, per platform and per day, and the top posts of a range. Dates are UTC days of the
/// post's `published_at`; the default is the last 30 days. `422 invalid_range` for a date that
/// is not `YYYY-MM-DD`, a `to` before `from` or more than 366 days.
#[utoipa::path(get, path = "/analytics", tag = "analytics",
    security(("session" = []), ("api_token" = [])), params(AnalyticsQuery),
    responses((status = 200, body = Analytics), (status = 401, body = Error),
        (status = 422, body = Error)))]
async fn show(
    State(app): State<AppState>,
    a: Actor,
    q: Result<Query<AnalyticsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query").into_response();
    };
    get_analytics(&app, &a, q).await
}

/// The caller's published posts of the range, each with the numbers of its targets together.
/// `$4` is a set token's set and `$5` the set asked for; either may be null.
const SCOPED: &str = "WITH scoped AS (SELECT p.id, p.platform, p.published_at, \
    COALESCE(sum(m.likes), 0)::bigint AS likes, COALESCE(sum(m.comments), 0)::bigint AS comments, \
    COALESCE(sum(m.shares), 0)::bigint AS shares, COALESCE(sum(m.views), 0)::bigint AS views \
    FROM posts p LEFT JOIN post_metrics m ON m.post_id = p.id \
    WHERE p.team_id = $1 AND p.status = 'published' AND p.published_at >= $2 \
      AND p.published_at < $3 \
      AND p.social_set_id IN (SELECT id FROM social_sets WHERE deleted_at IS NULL) \
      AND ($4::uuid IS NULL OR p.social_set_id = $4) \
      AND ($5::uuid IS NULL OR p.social_set_id = $5) \
    GROUP BY p.id)";

/// The body of `GET /analytics`, shared with the MCP `get-analytics` tool. The sums are
/// Postgres's, per platform and day; only the top posts are loaded.
pub async fn get_analytics(app: &AppState, a: &Actor, q: AnalyticsQuery) -> Response {
    let Some((from, to)) = range(&q) else {
        return invalid_range();
    };
    let found = async {
        let start = from.and_time(NaiveTime::MIN).and_utc();
        let end = (to + Duration::days(1)).and_time(NaiveTime::MIN).and_utc();
        let set = posts::narrowed(&app.db, a, q.social_set_id).await?;
        let query = |rest: &str| {
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!("{SCOPED} {rest}"),
                [
                    a.team.id.into(),
                    start.into(),
                    end.into(),
                    a.set_id().into(),
                    set.into(),
                ],
            )
        };
        let sums = "SELECT platform, (published_at AT TIME ZONE 'UTC')::date AS day, \
            count(*) AS posts, sum(likes)::bigint AS likes, sum(comments)::bigint AS comments, \
            sum(shares)::bigint AS shares, sum(views)::bigint AS views \
            FROM scoped GROUP BY 1, 2";
        let mut totals = Totals::default();
        let mut platforms: BTreeMap<String, Totals> = BTreeMap::new();
        let mut days: BTreeMap<NaiveDate, Totals> = from
            .iter_days()
            .take_while(|d| *d <= to)
            .map(|d| (d, Totals::default()))
            .collect();
        for r in app.db.query_all_raw(query(sums)).await? {
            let s = Totals {
                posts: r.try_get("", "posts")?,
                likes: r.try_get("", "likes")?,
                comments: r.try_get("", "comments")?,
                shares: r.try_get("", "shares")?,
                views: r.try_get("", "views")?,
            };
            let platform: String = r.try_get("", "platform")?;
            let day: NaiveDate = r.try_get("", "day")?;
            for to in [
                &mut totals,
                platforms.entry(platform).or_default(),
                days.entry(day).or_default(),
            ] {
                to.posts += s.posts;
                to.likes += s.likes;
                to.comments += s.comments;
                to.shares += s.shares;
                to.views += s.views;
            }
        }
        let top = format!(
            "SELECT id FROM scoped WHERE likes + comments + shares > 0 \
             ORDER BY likes + comments + shares DESC, published_at DESC, id LIMIT {TOP}"
        );
        let mut ids = Vec::new();
        for r in app.db.query_all_raw(query(&top)).await? {
            ids.push(r.try_get::<Uuid>("", "id")?);
        }
        let mut top = post_rows::Entity::find()
            .filter(post_rows::Column::Id.is_in(ids.iter().copied()))
            .all(&app.db)
            .await?;
        top.sort_by_key(|p| ids.iter().position(|id| *id == p.id));
        Ok::<_, DbErr>(Analytics {
            supported: SUPPORTED.map(str::to_owned).to_vec(),
            totals,
            by_platform: platforms
                .into_iter()
                .map(|(platform, totals)| PlatformTotals {
                    supported: SUPPORTED.contains(&platform.as_str()),
                    platform,
                    totals,
                })
                .collect(),
            by_day: days
                .into_iter()
                .map(|(d, totals)| DayTotals {
                    date: d.to_string(),
                    totals,
                })
                .collect(),
            top_posts: posts::posts_json(&app.db, a, top).await?,
        })
    };
    match found.await {
        Ok(body) => Json(body).into_response(),
        Err(e) => internal(e, "analytics"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(show))
}
