//! Analytics of the current team: the metrics `metrics.rs` keeps for published posts, summed
//! over a range of days. Any member reads it, and so does a token (a set token for its set).
use std::collections::{BTreeMap, HashMap};

use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration, NaiveDate, NaiveTime, Utc};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::internal,
    posts::{self, Post, MAX_POSTS},
    waitlist::{error, Error},
};
use crate::{
    metrics::SUPPORTED,
    models::{post_metrics, posts as post_rows},
    state::AppState,
};

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

/// The body of `GET /analytics`, shared with the MCP `get-analytics` tool.
pub async fn get_analytics(app: &AppState, a: &Actor, q: AnalyticsQuery) -> Response {
    let Some((from, to)) = range(&q) else {
        return invalid_range();
    };
    let found = async {
        let start = from.and_time(NaiveTime::MIN).and_utc();
        let end = (to + Duration::days(1)).and_time(NaiveTime::MIN).and_utc();
        let rows = posts::in_set(&app.db, a, q.social_set_id)
            .await?
            .filter(post_rows::Column::Status.eq("published"))
            .filter(post_rows::Column::PublishedAt.gte(start))
            .filter(post_rows::Column::PublishedAt.lt(end))
            .order_by_desc(post_rows::Column::PublishedAt)
            .order_by_asc(post_rows::Column::Id)
            .limit(MAX_POSTS)
            .all(&app.db)
            .await?;
        // A post's numbers are those of its targets together.
        let mut sums: HashMap<Uuid, Totals> = HashMap::new();
        let read = post_metrics::Entity::find()
            .filter(post_metrics::Column::TeamId.eq(a.team.id))
            .filter(post_metrics::Column::PostId.is_in(rows.iter().map(|p| p.id)))
            .all(&app.db)
            .await?;
        for m in read {
            let s = sums.entry(m.post_id).or_default();
            s.likes += m.likes.unwrap_or(0);
            s.comments += m.comments.unwrap_or(0);
            s.shares += m.shares.unwrap_or(0);
            s.views += m.views.unwrap_or(0);
        }
        let add = |to: &mut Totals, s: Totals| {
            to.posts += 1;
            to.likes += s.likes;
            to.comments += s.comments;
            to.shares += s.shares;
            to.views += s.views;
        };
        let mut totals = Totals::default();
        let mut platforms: BTreeMap<String, Totals> = BTreeMap::new();
        let mut days: BTreeMap<NaiveDate, Totals> = from
            .iter_days()
            .take_while(|d| *d <= to)
            .map(|d| (d, Totals::default()))
            .collect();
        for p in &rows {
            let s = sums.get(&p.id).copied().unwrap_or_default();
            add(&mut totals, s);
            add(platforms.entry(p.platform.clone()).or_default(), s);
            if let Some(day) = p.published_at.map(|t| t.to_utc().date_naive()) {
                add(days.entry(day).or_default(), s);
            }
        }
        let score = |id: &Uuid| sums.get(id).map_or(0, |s| s.likes + s.comments + s.shares);
        // The sort is stable, so equal posts stay newest first.
        let mut top: Vec<post_rows::Model> =
            rows.into_iter().filter(|p| score(&p.id) > 0).collect();
        top.sort_by_key(|p| std::cmp::Reverse(score(&p.id)));
        top.truncate(TOP);
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
