//! Posts of the current team: every member writes them. One post per platform (Bluesky,
//! Threads, LinkedIn, Facebook, Instagram, YouTube, X and TikTok today); a post goes to every account of its platform in its social set. Only `draft` and
//! `scheduled` posts change; saving with `scheduled_at` makes one `scheduled`, else `draft`.
//! Posts of a deleted set are hidden (and held back) until the set is restored. Every route
//! takes an API token too (`Actor`); a set token only sees and writes its set's posts. Token
//! writes take an optional `Idempotency-Key` header (see `idempotency`). `POST /post-groups`
//! creates one post per platform in one go, tied by a `group_id`; `POST /posts/{id}/retry` and
//! `POST /post-groups/{group_id}/retry` publish the targets that failed again.
use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    body::to_bytes,
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use schemars::JsonSchema;
use sea_orm::{
    sea_query::Query as Select, ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait,
    DatabaseTransaction, DbBackend, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::post;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body},
    connections::live_sets,
    idempotency::IDEMPOTENCY,
    lists::{self, lists_of, ListRef},
    waitlist::{error, Error},
};
use crate::{
    connectors::{tiktok, youtube},
    models::{
        post_targets, posts, social_account_social_set as membership, social_accounts, social_sets,
        teams,
    },
    publish,
    state::AppState,
};

/// Most posts a team keeps that are not `deleted`.
const MAX_POSTS: u64 = 2000;
const STATUSES: [&str; 6] = [
    "draft",
    "scheduled",
    "queued",
    "published",
    "failed",
    "deleted",
];

#[derive(Serialize, ToSchema)]
pub struct Post {
    pub id: Uuid,
    pub social_set_id: Uuid,
    /// `bluesky`.
    pub platform: String,
    pub body: String,
    /// Public `https://` image URLs; Google Drive share links work when the file is shared
    /// with anyone with the link. Downloaded when the post publishes (JPEG, PNG, WebP or GIF, at
    /// most 1 MB each on Bluesky) and deleted once it settles.
    pub media: Vec<String>,
    /// Follow-up posts, each a reply to the one before.
    pub thread: Vec<ThreadPart>,
    /// Text the same account comments on the post right after it is published (Facebook,
    /// Instagram, LinkedIn, YouTube); each target says how it went (`comment_status`).
    pub first_comment: Option<String>,
    /// Cover image URL for the post's one video.
    pub cover: Option<String>,
    /// TikTok settings of a TikTok post, when it has any.
    pub tiktok: Option<tiktok::Options>,
    /// YouTube settings of a YouTube post, when it has any; none (or no `privacy`) is private.
    pub youtube: Option<youtube::Options>,
    /// `draft`, `scheduled`, `queued`, `published`, `failed` or `deleted`.
    pub status: String,
    pub scheduled_at: Option<DateTime<FixedOffset>>,
    pub published_at: Option<DateTime<FixedOffset>>,
    /// Why it failed, naming each account that failed.
    pub error: Option<String>,
    pub created_by: Option<Uuid>,
    /// Ties the posts of one `POST /post-groups` call; `null` for a single post.
    pub group_id: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    pub targets: Vec<Target>,
    /// The lists the post is in, with its position (its "Part N") in each.
    pub lists: Vec<ListRef>,
}

/// One account a post went (or is going) to.
#[derive(Serialize, ToSchema)]
pub struct Target {
    pub id: Uuid,
    /// `null` once the account was disconnected.
    pub social_account_id: Option<Uuid>,
    /// `pending`, `publishing`, `published`, `failed` or `deleted`.
    pub status: String,
    /// The platform's id, e.g. a Bluesky `at://` URI.
    pub remote_id: Option<String>,
    /// The public link to the post.
    pub remote_url: Option<String>,
    pub error: Option<String>,
    pub published_at: Option<DateTime<FixedOffset>>,
    /// The thread posts published so far, in order; a failed target names the one that did
    /// not go out.
    pub parts: Vec<TargetPart>,
    /// The first comment: `null` when not tried (or the post has none), `posting`, `posted`
    /// or `failed`. A failed comment never fails the post and is not retried.
    pub comment_status: Option<String>,
    /// The platform's id of the comment once `posted`.
    pub comment_id: Option<String>,
    /// Why the comment failed.
    pub comment_error: Option<String>,
    /// Why the cover could not be set on the published video; a warning, the post is up.
    pub cover_error: Option<String>,
}

/// One follow-up post of a thread.
#[derive(Clone, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ThreadPart {
    /// The same limits as a post's body.
    #[serde(default)]
    pub body: String,
    /// The same limits as a post's media.
    #[serde(default)]
    pub media: Vec<String>,
}

/// A thread post published on one account.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct TargetPart {
    pub remote_id: String,
    pub remote_url: Option<String>,
    pub published_at: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct PostInput {
    /// A live set of the current team. Required, except with a set token, where absent means
    /// the token's set and any other set is `invalid_set`.
    pub social_set_id: Option<Uuid>,
    /// `bluesky`, `threads`, `linkedin`, `facebook`, `instagram`, `youtube`, `x` or `tiktok` (the platforms that publish today).
    /// A post goes to every account of its platform in its set (one target each), not to a
    /// chosen one: to post to one account, give it a set of its own.
    pub platform: String,
    /// At most 300 graphemes on Bluesky, 500 characters on Threads, 3000 on LinkedIn, 63206 on Facebook, 2200 on
    /// Instagram, 280 on X as X counts them (a link is 23, a CJK character or emoji 2). On YouTube the first line is the video's title (required, at most 100 characters)
    /// and the rest its description (at most 5000 bytes); neither may hold `<` or `>`
    /// (`invalid_text`). On TikTok at most 2200 characters, the video's caption.
    #[serde(default)]
    pub body: String,
    /// A PDF is a LinkedIn document: it must be the post's only media item, and the file's bytes
    /// decide (not the URL's extension). LinkedIn shows it as a swipeable document titled by the
    /// body's first non-empty line (at most 100 characters, else `Document`), and refuses files
    /// over 300 pages with its own reason on the target. Any other platform fails the target
    /// (`Image N is a PDF...`) before posting, and a PDF next to other media fails it too. At most
    /// 100 MB.
    ///
    /// Public `https://` media URLs: on Bluesky at most 4 images (JPEG, PNG, WebP or GIF, 1 MB
    /// each), on Threads at most 20 JPEG or PNG images or MP4 or MOV videos, on LinkedIn at most 20 JPEG, PNG or GIF images or one MP4 video, on Facebook at most
    /// 10 JPEG, PNG or GIF images or one MP4 or MOV video (a Reel), on Instagram 1 to 20 JPEG
    /// images or MP4 or MOV videos (`media_required` without one), on YouTube exactly one MP4 or
    /// MOV video (`media_required` without one), on X at most 4 JPEG, PNG or WebP images, or one
    /// GIF, or one MP4 or MOV video, on TikTok one MP4 or MOV video or 1 to 35 JPEG or WebP
    /// photos (`media_required` without one).
    #[serde(default)]
    pub media: Vec<String>,
    /// Follow-up posts of a thread (at most 25), each a reply to the one before and checked
    /// like `body` and `media`. Platforms that cannot post threads yet answer
    /// `thread_not_supported`; a bad part answers its 1-based `part` with the error.
    #[serde(default)]
    pub thread: Vec<ThreadPart>,
    /// A comment the account posts on the post right after it is published, e.g. for sources.
    /// Only on Facebook (at most 8000 characters), Instagram (2200), LinkedIn (1250) and
    /// YouTube (10000); other platforms answer `first_comment_not_supported`, and a longer one
    /// `first_comment_too_long`. Blank counts as absent. It is posted once and never retried: a
    /// failed comment leaves the post `published` and shows as the target's `comment_error`.
    /// Taking the post down removes it with the comment.
    pub first_comment: Option<String>,
    /// A cover image for the post's one video (a public https link to a JPEG or PNG, Google
    /// Drive share links included, fetched at publish time like `media`). Only on Instagram
    /// (JPEG, at most 8 MB), Facebook (JPEG or PNG, 10 MB) and YouTube (JPEG or PNG, 2 MB);
    /// other platforms answer `cover_not_supported` (TikTok can only pick a frame), a post
    /// without exactly one media item `cover_needs_one_video`, and a bad link `invalid_cover`.
    /// Blank counts as absent. A cover that cannot be downloaded or is not the right type, or
    /// a media item that is not a video, fails the target before anything is posted. On
    /// Facebook and YouTube the cover is set after the video is live: a refusal (YouTube needs
    /// a verified channel) leaves the post `published` and shows as the target's `cover_error`.
    pub cover: Option<String>,
    /// TikTok's per-post settings, as its Content Sharing Guidelines ask the user to choose
    /// them: the visibility (required, one of the account's `privacy_level_options` from
    /// `GET /connections/{id}/tiktok`), comments, duets and stitches (all off unless turned on),
    /// the commercial content disclosure and the AI-generated flag. Branded content cannot be
    /// `SELF_ONLY`. Other platforms answer `tiktok_not_supported`; invalid settings answer
    /// `tiktok_privacy_required`, `invalid_tiktok_privacy`, `invalid_tiktok_disclosure`,
    /// `tiktok_disclosure_incomplete` or `tiktok_branded_content_private`. A TikTok post saved
    /// without them (`POST /posts`, `PUT /posts/{id}` or `POST /post-groups`) is `422
    /// tiktok_settings_required`, so an update cannot remove them either. Posts saved before
    /// this rule without settings stay readable and still go out with the most public level
    /// the account is offered and every interaction off.
    pub tiktok: Option<tiktok::Options>,
    /// YouTube's per-post settings: `privacy` is the video's visibility, `private` (the
    /// default, also without `youtube`), `unlisted` or `public`. Other platforms answer
    /// `youtube_not_supported`, another value `invalid_youtube_privacy`. Until Google audits the
    /// API project, YouTube keeps every upload private whatever is chosen.
    pub youtube: Option<youtube::Options>,
    /// When to publish. Set: `scheduled`; absent or `null`: `draft`.
    pub scheduled_at: Option<DateTime<FixedOffset>>,
    /// A list to put the post in (moving it there when it already is).
    pub list_id: Option<Uuid>,
    /// With `list_id`: the 1-based position; absent appends.
    pub position: Option<i32>,
}

#[derive(Deserialize, IntoParams, JsonSchema)]
pub struct PostQuery {
    /// The posts of one set; the Default set, which holds every account, gives every set's.
    pub social_set_id: Option<Uuid>,
    /// One of the post statuses.
    pub status: Option<String>,
    /// `scheduled_at` at or after this time (drafts without a time drop out).
    pub from: Option<DateTime<FixedOffset>>,
    /// `scheduled_at` before this time.
    pub to: Option<DateTime<FixedOffset>>,
    /// The posts of one post group.
    pub group_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct PostGroupInput {
    /// As in `POST /posts`.
    pub social_set_id: Option<Uuid>,
    /// The platforms to post to, one post each, each going to every account of that platform
    /// in the set (one target per account). Absent: every platform brain publishes to with a
    /// connected account in the set (`422 no_accounts` when there is none), which includes
    /// TikTok, so then `tiktok` is required too.
    pub platforms: Option<Vec<String>>,
    /// Each post's body, unless its override has one.
    #[serde(default)]
    pub body: String,
    /// Each post's media, unless its override has some.
    #[serde(default)]
    pub media: Vec<String>,
    /// Each post's thread, unless its override has one.
    #[serde(default)]
    pub thread: Vec<ThreadPart>,
    /// Each post's first comment, unless its override has one (`""` for none). Platforms that
    /// cannot post one answer `first_comment_not_supported` unless they override it with `""`.
    /// Without `platforms` the group covers every connected platform, so a group with a
    /// `first_comment` fails with `first_comment_not_supported` naming the first platform that
    /// cannot take it (Bluesky, Threads, TikTok, X): pass `platforms`.
    pub first_comment: Option<String>,
    /// Each post's cover, unless its override has one (`""` for none). Platforms without a
    /// cover answer `cover_not_supported` unless they override it with `""`, so pass
    /// `platforms` or override.
    pub cover: Option<String>,
    /// The TikTok post's settings, unless its override has some. Other platforms get none. A
    /// group with a TikTok post and no `tiktok.privacy_level` (here or in the override) is
    /// `422 {"error":"tiktok_settings_required","platform":"tiktok"}`: the visibility has no
    /// default, and brain never picks the most public one for you.
    pub tiktok: Option<tiktok::Options>,
    /// The YouTube post's settings, unless its override has some. Other platforms get none.
    /// Without them the video is private.
    pub youtube: Option<youtube::Options>,
    /// As in `POST /posts`, for every post.
    pub scheduled_at: Option<DateTime<FixedOffset>>,
    /// A list to put every post in, in platform order.
    pub list_id: Option<Uuid>,
    /// With `list_id`: the first post's 1-based position, the others right after; absent
    /// appends.
    pub position: Option<i32>,
    /// Per-platform body or media; a platform outside the group is `422 invalid_override`.
    #[serde(default)]
    pub overrides: BTreeMap<String, Override>,
}

/// One platform's own body, media, thread or first comment in a post group.
#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct Override {
    pub body: Option<String>,
    pub media: Option<Vec<String>>,
    pub thread: Option<Vec<ThreadPart>>,
    pub first_comment: Option<String>,
    pub cover: Option<String>,
    pub tiktok: Option<tiktok::Options>,
    pub youtube: Option<youtube::Options>,
}

/// A failed target a retry left alone because it may already have posted.
#[derive(Clone, Serialize, ToSchema)]
pub struct SkippedTarget {
    pub target_id: Uuid,
    pub post_id: Uuid,
    pub social_account_id: Option<Uuid>,
    /// Always `outcome_unknown`.
    pub reason: String,
    /// What the target failed with, e.g. `Timed out; it may have posted, check the account.`
    pub error: Option<String>,
}

/// `POST /posts/{id}/retry`: the post, queued, and the failed targets left alone.
#[derive(Serialize, ToSchema)]
pub struct PostRetry {
    #[serde(flatten)]
    pub post: Post,
    /// Failed targets that may already have posted (a timeout, a run cut off, a platform that
    /// never said it was done). They were not re-queued: check the account on the platform.
    pub skipped_targets: Vec<SkippedTarget>,
}

/// `POST /post-groups/{group_id}/retry`: the group, queued, and the failed targets left alone.
#[derive(Serialize, ToSchema)]
pub struct GroupRetry {
    #[serde(flatten)]
    pub group: PostGroup,
    /// As in `PostRetry`.
    pub skipped_targets: Vec<SkippedTarget>,
}

#[derive(Serialize, ToSchema)]
pub struct PostGroup {
    pub group_id: Uuid,
    /// One post per platform, in the order they were given.
    pub posts: Vec<Post>,
    /// How many targets the group will have once everything has gone out: one per account of
    /// each post's platform in its set. A post that already has targets counts those, one that
    /// has none yet counts the accounts it will fan out to now. Every target has reported when
    /// that many targets (across `posts`) are `published`, `failed` or `deleted`.
    pub expected_targets: u64,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn editable(status: &str) -> bool {
    status == "draft" || status == "scheduled"
}

async fn targets_of(
    db: &impl ConnectionTrait,
    posts: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Target>>, DbErr> {
    let rows = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.is_in(posts.iter().copied()))
        .order_by_asc(post_targets::Column::CreatedAt)
        .order_by_asc(post_targets::Column::Id)
        .all(db)
        .await?;
    let mut map: HashMap<Uuid, Vec<Target>> = HashMap::new();
    for t in rows {
        map.entry(t.post_id).or_default().push(Target {
            id: t.id,
            social_account_id: t.social_account_id,
            status: t.status,
            remote_id: t.remote_id,
            remote_url: t.remote_url,
            error: t.error,
            published_at: t.published_at,
            parts: serde_json::from_value(t.parts).unwrap_or_default(),
            comment_status: t.comment_status,
            comment_id: t.comment_id,
            comment_error: t.comment_error,
            cover_error: t.cover_error,
        });
    }
    Ok(map)
}

fn post_json(p: posts::Model, targets: Vec<Target>, lists: Vec<ListRef>) -> Post {
    Post {
        id: p.id,
        social_set_id: p.social_set_id,
        platform: p.platform,
        body: p.body,
        media: serde_json::from_value(p.media).unwrap_or_default(),
        thread: serde_json::from_value(p.thread).unwrap_or_default(),
        first_comment: p.first_comment,
        cover: p.cover,
        tiktok: p.tiktok.and_then(|v| serde_json::from_value(v).ok()),
        youtube: p.youtube.and_then(|v| serde_json::from_value(v).ok()),
        status: p.status,
        scheduled_at: p.scheduled_at,
        published_at: p.published_at,
        error: p.error,
        created_by: p.created_by,
        group_id: p.group_id,
        created_at: p.created_at,
        updated_at: p.updated_at,
        targets,
        lists,
    }
}

/// The JSON of `rows`, in order, with their targets and lists batched.
pub(crate) async fn posts_json(
    db: &impl ConnectionTrait,
    a: &Actor,
    rows: Vec<posts::Model>,
) -> Result<Vec<Post>, DbErr> {
    let ids: Vec<Uuid> = rows.iter().map(|p| p.id).collect();
    let mut targets = targets_of(db, &ids).await?;
    let mut in_lists = lists_of(db, a, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            let t = targets.remove(&p.id).unwrap_or_default();
            let l = in_lists.remove(&p.id).unwrap_or_default();
            post_json(p, t, l)
        })
        .collect())
}

async fn one_json(db: &impl ConnectionTrait, a: &Actor, p: posts::Model) -> Result<Post, DbErr> {
    let mut one = posts_json(db, a, vec![p]).await?;
    Ok(one.remove(0))
}

/// Posts a set token may touch: its set's. Re-checked in writes, so a concurrent move of the
/// post to another set cannot widen the token's reach.
fn in_scope(a: &Actor) -> Condition {
    match a.set_id() {
        Some(set) => Condition::all().add(posts::Column::SocialSetId.eq(set)),
        None => Condition::all(),
    }
}

/// The caller's post `id`, unless its set is deleted or outside a set token's set.
pub(crate) async fn find(
    db: &impl ConnectionTrait,
    a: &Actor,
    id: Uuid,
) -> Result<Option<posts::Model>, DbErr> {
    let sel = posts::Entity::find_by_id(id)
        .filter(posts::Column::TeamId.eq(a.team.id))
        .filter(posts::Column::SocialSetId.in_subquery(live_sets()));
    sel.filter(in_scope(a)).one(db).await
}

/// Checks a post body for saving: platform rules, then a live set of the team the caller may
/// write to. Answers that set's id.
async fn checked(
    db: &impl ConnectionTrait,
    a: &Actor,
    input: &PostInput,
) -> Result<Result<Uuid, Response>, DbErr> {
    if let Err(e) = post::validate(&input.platform, &input.body, &input.media) {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, e.code())));
    }
    let parts: Vec<(&str, &[String])> = input
        .thread
        .iter()
        .map(|p| (p.body.as_str(), p.media.as_slice()))
        .collect();
    if let Err((e, part)) = post::validate_thread(&input.platform, &parts) {
        let mut body = serde_json::json!({ "error": e.code() });
        if let Some(n) = part {
            body["part"] = n.into();
        }
        return Ok(Err(
            (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
        ));
    }
    let comment = post::first_comment(input.first_comment.as_deref());
    if let Err(e) = post::validate_first_comment(&input.platform, comment) {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, e.code())));
    }
    let cover = post::cover(input.cover.as_deref());
    if let Err(e) = post::validate_cover(&input.platform, cover, &input.media) {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, e.code())));
    }
    let choice = input.tiktok.as_ref().map(|t| post::TikTokChoice {
        privacy_level: t.privacy_level.as_deref(),
        commercial_content: t.commercial_content,
        your_brand: t.your_brand,
        branded_content: t.branded_content,
    });
    if let Err(e) = post::validate_tiktok(&input.platform, choice) {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, e.code())));
    }
    let privacy = input.youtube.as_ref().map(|y| y.privacy.as_deref());
    if let Err(e) = post::validate_youtube(&input.platform, privacy) {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, e.code())));
    }
    // The visibility has no default: brain never posts at the most public level unasked. (Rows
    // saved before this still publish with the auto-picked level, and stay readable.)
    if input.platform == "tiktok" && input.tiktok.is_none() {
        return Ok(Err(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "tiktok_settings_required",
        )));
    }
    set_for(db, a, input.social_set_id).await
}

/// The live set of the team a write names (a set token's own when absent), if the caller may
/// write to it.
async fn set_for(
    db: &impl ConnectionTrait,
    a: &Actor,
    social_set_id: Option<Uuid>,
) -> Result<Result<Uuid, Response>, DbErr> {
    let invalid_set = || reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_set");
    // Another set than a set token's own answers like a missing one.
    let id = match (a.set_id(), social_set_id) {
        (Some(own), None) => own,
        (Some(own), Some(id)) if id == own => own,
        (None, Some(id)) => id,
        _ => return Ok(Err(invalid_set())),
    };
    let set = social_sets::Entity::find_by_id(id)
        .filter(social_sets::Column::TeamId.eq(a.team.id))
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(db)
        .await?;
    Ok(match set {
        Some(_) => Ok(id),
        None => Err(invalid_set()),
    })
}

/// `scheduled_at`, with a past time meaning now (as the old app did).
fn when(input: &PostInput) -> Option<DateTime<FixedOffset>> {
    let now = Utc::now().fixed_offset();
    input.scheduled_at.map(|t| t.max(now))
}

/// The first comment as saved: blank is none.
fn stored_comment(input: &PostInput) -> Option<String> {
    post::first_comment(input.first_comment.as_deref()).map(str::to_owned)
}

/// The TikTok settings as saved.
fn stored_tiktok(input: &PostInput) -> Option<serde_json::Value> {
    input
        .tiktok
        .as_ref()
        .and_then(|t| serde_json::to_value(t).ok())
}

/// The YouTube settings as saved.
fn stored_youtube(input: &PostInput) -> Option<serde_json::Value> {
    input
        .youtube
        .as_ref()
        .and_then(|y| serde_json::to_value(y).ok())
}

/// The cover as saved: blank is none.
fn stored_cover(input: &PostInput) -> Option<String> {
    post::cover(input.cover.as_deref()).map(str::to_owned)
}

fn status_for(input: &PostInput) -> &'static str {
    if input.scheduled_at.is_some() {
        "scheduled"
    } else {
        "draft"
    }
}

/// The current team's posts, by `scheduled_at` (unscheduled last), at most 2000. For a calendar,
/// pass `from` and `to`.
#[utoipa::path(get, path = "/posts", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(PostQuery),
    responses((status = 200, body = [Post]), (status = 401, body = Error),
        (status = 422, body = Error)))]
async fn list(
    State(app): State<AppState>,
    a: Actor,
    q: Result<Query<PostQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    list_posts(&app, &a, q).await
}

/// The body of `GET /posts`, shared with the MCP `list-posts` tool.
pub async fn list_posts(app: &AppState, a: &Actor, q: PostQuery) -> Response {
    if q.status.as_deref().is_some_and(|s| !STATUSES.contains(&s)) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let found = async {
        let mut sel = posts::Entity::find()
            .filter(posts::Column::TeamId.eq(a.team.id))
            .filter(posts::Column::SocialSetId.in_subquery(live_sets()))
            .filter(in_scope(a));
        if let Some(set) = q.social_set_id {
            // The Default set holds every account, so it lists every set's posts.
            let default = social_sets::Entity::find_by_id(set)
                .filter(social_sets::Column::TeamId.eq(a.team.id))
                .filter(social_sets::Column::IsDefault.eq(true))
                .count(&app.db)
                .await?
                > 0;
            if !default {
                sel = sel.filter(posts::Column::SocialSetId.eq(set));
            }
        }
        if let Some(status) = &q.status {
            sel = sel.filter(posts::Column::Status.eq(status));
        }
        if let Some(from) = q.from {
            sel = sel.filter(posts::Column::ScheduledAt.gte(from));
        }
        if let Some(to) = q.to {
            sel = sel.filter(posts::Column::ScheduledAt.lt(to));
        }
        if let Some(group) = q.group_id {
            sel = sel.filter(posts::Column::GroupId.eq(group));
        }
        let rows = sel
            .order_by_asc(posts::Column::ScheduledAt)
            .order_by_asc(posts::Column::CreatedAt)
            .order_by_asc(posts::Column::Id)
            .limit(MAX_POSTS)
            .all(&app.db)
            .await?;
        posts_json(&app.db, a, rows).await
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "post list"),
    }
}

/// One post with its targets.
#[utoipa::path(get, path = "/posts/{id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = Post), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    get_post(&app, &a, id).await
}

/// The body of `GET /posts/{id}`, shared with the MCP `get-post` tool.
pub async fn get_post(app: &AppState, a: &Actor, id: Uuid) -> Response {
    let found = async {
        match find(&app.db, a, id).await? {
            Some(p) => one_json(&app.db, a, p).await.map(Some),
            None => Ok(None),
        }
    };
    match found.await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "post read"),
    }
}

/// Creates a post; `scheduled` with `scheduled_at` (a past time means now), else `draft`. At
/// most 2000 per team.
#[utoipa::path(post, path = "/posts", tag = "posts",
    security(("session" = []), ("api_token" = [])), request_body = PostInput,
    params(("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 201, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(State(app): State<AppState>, a: Actor, Body(input): Body<PostInput>) -> Response {
    create_post(&app, &a, input).await
}

/// The body of `POST /posts`, shared with the MCP `create-post` tool.
pub async fn create_post(app: &AppState, a: &Actor, input: PostInput) -> Response {
    let made = async {
        let txn = app.db.begin().await?;
        let p = match insert(&txn, a, &input, None).await? {
            Ok(p) => p,
            Err(res) => return Ok(Err(res)),
        };
        txn.commit().await?;
        one_json(&app.db, a, p).await.map(Ok)
    };
    match made.await {
        Ok(Ok(p)) => (StatusCode::CREATED, Json(p)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post insert"),
    }
}

/// Checks and inserts one new post in `txn`, then puts it in its list. Every create runs this:
/// `POST /posts` and each post of a group. The caller commits.
async fn insert(
    txn: &DatabaseTransaction,
    a: &Actor,
    input: &PostInput,
    group_id: Option<Uuid>,
) -> Result<Result<posts::Model, Response>, DbErr> {
    let set = match checked(txn, a, input).await? {
        Ok(set) => set,
        Err(res) => return Ok(Err(res)),
    };
    // The team row lock keeps concurrent creates under the cap.
    teams::Entity::find_by_id(a.team.id)
        .lock_exclusive()
        .one(txn)
        .await?;
    let live = posts::Entity::find()
        .filter(posts::Column::TeamId.eq(a.team.id))
        .filter(posts::Column::Status.ne("deleted"))
        .count(txn)
        .await?;
    if live >= MAX_POSTS {
        return Ok(Err(reply(StatusCode::CONFLICT, "too_many_posts")));
    }
    let row = posts::ActiveModel {
        id: Set(Uuid::now_v7()),
        team_id: Set(a.team.id),
        social_set_id: Set(set),
        platform: Set(input.platform.clone()),
        body: Set(input.body.clone()),
        media: Set(serde_json::json!(input.media)),
        thread: Set(serde_json::json!(input.thread)),
        first_comment: Set(stored_comment(input)),
        cover: Set(stored_cover(input)),
        tiktok: Set(stored_tiktok(input)),
        youtube: Set(stored_youtube(input)),
        status: Set(status_for(input).into()),
        scheduled_at: Set(when(input)),
        created_by: Set(a.user_id),
        group_id: Set(group_id),
        ..Default::default()
    };
    let p = posts::Entity::insert(row).exec_with_returning(txn).await?;
    if let Some(list) = input.list_id {
        if let Err(res) = lists::place(txn, a, list, &p, input.position).await? {
            return Ok(Err(res));
        }
    }
    Ok(Ok(p))
}

/// Creates one post per platform of a set in one transaction, tied by a new `group_id`; each is
/// checked as in `POST /posts`. Any failure creates nothing and answers that post's error with
/// `"platform"` added, e.g. `422 {"error":"too_long","platform":"bluesky"}`.
#[utoipa::path(post, path = "/post-groups", tag = "posts",
    security(("session" = []), ("api_token" = [])), request_body = PostGroupInput,
    params(("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 201, body = PostGroup), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn create_group(
    State(app): State<AppState>,
    a: Actor,
    Body(input): Body<PostGroupInput>,
) -> Response {
    create_post_group(&app, &a, input).await
}

/// The body of `POST /post-groups`, shared with the MCP `create-post-group` tool.
pub async fn create_post_group(app: &AppState, a: &Actor, input: PostGroupInput) -> Response {
    let made = async {
        let set = match set_for(&app.db, a, input.social_set_id).await? {
            Ok(set) => set,
            Err(res) => return Ok(Err(res)),
        };
        let mut platforms = match &input.platforms {
            Some(p) => p.clone(),
            // Only platforms brain publishes to, so an account connected early breaks nothing.
            None => connected_platforms(&app.db, a.team.id, set)
                .await?
                .into_iter()
                .filter(|p| publish::publisher(app, p).is_some())
                .collect(),
        };
        let mut seen = HashSet::new();
        platforms.retain(|p| seen.insert(p.clone()));
        if platforms.is_empty() {
            return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "no_accounts")));
        }
        if input.overrides.keys().any(|p| !platforms.contains(p)) {
            return Ok(Err(reply(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_override",
            )));
        }
        let group = Uuid::now_v7();
        let txn = app.db.begin().await?;
        let mut rows = Vec::with_capacity(platforms.len());
        for (i, platform) in (0i32..).zip(platforms) {
            let o = input.overrides.get(&platform);
            let one = PostInput {
                social_set_id: Some(set),
                body: o
                    .and_then(|o| o.body.clone())
                    .unwrap_or_else(|| input.body.clone()),
                media: o
                    .and_then(|o| o.media.clone())
                    .unwrap_or_else(|| input.media.clone()),
                thread: o
                    .and_then(|o| o.thread.clone())
                    .unwrap_or_else(|| input.thread.clone()),
                first_comment: o
                    .and_then(|o| o.first_comment.clone())
                    .or_else(|| input.first_comment.clone()),
                cover: o
                    .and_then(|o| o.cover.clone())
                    .or_else(|| input.cover.clone()),
                tiktok: o
                    .and_then(|o| o.tiktok.clone())
                    .or_else(|| input.tiktok.clone().filter(|_| platform == "tiktok")),
                youtube: o
                    .and_then(|o| o.youtube.clone())
                    .or_else(|| input.youtube.clone().filter(|_| platform == "youtube")),
                platform,
                scheduled_at: input.scheduled_at,
                list_id: input.list_id,
                position: input.position.map(|p| p.saturating_add(i)),
            };
            match insert(&txn, a, &one, Some(group)).await? {
                Ok(p) => rows.push(p),
                Err(res) => return Ok(Err(naming(res, &one.platform).await)),
            }
        }
        txn.commit().await?;
        group_json(&app.db, a, group, rows).await.map(Ok)
    };
    match made.await {
        Ok(Ok(g)) => (StatusCode::CREATED, Json(g)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post group insert"),
    }
}

/// The group's posts with their targets, and how many targets it will have in all.
async fn group_json(
    db: &impl ConnectionTrait,
    a: &Actor,
    group_id: Uuid,
    rows: Vec<posts::Model>,
) -> Result<PostGroup, DbErr> {
    let posts = posts_json(db, a, rows).await?;
    let mut expected_targets = 0;
    for p in &posts {
        expected_targets += if p.targets.is_empty() {
            let in_set = Select::select()
                .column(membership::Column::SocialAccountId)
                .from(membership::Entity)
                .and_where(membership::Column::SocialSetId.eq(p.social_set_id))
                .to_owned();
            social_accounts::Entity::find()
                .filter(social_accounts::Column::TeamId.eq(a.team.id))
                .filter(social_accounts::Column::Provider.eq(&p.platform))
                .filter(social_accounts::Column::Id.in_subquery(in_set))
                .count(db)
                .await?
        } else {
            p.targets.len() as u64
        };
    }
    Ok(PostGroup {
        group_id,
        posts,
        expected_targets,
    })
}

/// The live posts of a group the caller may see, in creation order.
async fn group_rows(
    db: &impl ConnectionTrait,
    a: &Actor,
    group_id: Uuid,
) -> Result<Vec<posts::Model>, DbErr> {
    posts::Entity::find()
        .filter(posts::Column::GroupId.eq(group_id))
        .filter(posts::Column::TeamId.eq(a.team.id))
        .filter(posts::Column::SocialSetId.in_subquery(live_sets()))
        .filter(in_scope(a))
        .order_by_asc(posts::Column::CreatedAt)
        .order_by_asc(posts::Column::Id)
        .all(db)
        .await
}

/// One post group: its posts with their targets, and `expected_targets`, so a client can tell
/// when every target has reported. `404` for a group of another team, outside a set token's
/// set, or in a deleted set.
#[utoipa::path(get, path = "/post-groups/{group_id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("group_id" = Uuid, Path)),
    responses((status = 200, body = PostGroup), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show_group(State(app): State<AppState>, a: Actor, Path(group_id): Path<Uuid>) -> Response {
    let found = async {
        let rows = group_rows(&app.db, &a, group_id).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        group_json(&app.db, &a, group_id, rows).await.map(Some)
    };
    match found.await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "post group read"),
    }
}

/// The platforms of the team's accounts in `set`, by name.
async fn connected_platforms(
    db: &impl ConnectionTrait,
    team: Uuid,
    set: Uuid,
) -> Result<Vec<String>, DbErr> {
    let in_set = Select::select()
        .column(membership::Column::SocialAccountId)
        .from(membership::Entity)
        .and_where(membership::Column::SocialSetId.eq(set))
        .to_owned();
    social_accounts::Entity::find()
        .select_only()
        .column(social_accounts::Column::Provider)
        .distinct()
        .filter(social_accounts::Column::TeamId.eq(team))
        .filter(social_accounts::Column::Id.in_subquery(in_set))
        .order_by_asc(social_accounts::Column::Provider)
        .into_tuple()
        .all(db)
        .await
}

/// An error answer with `"platform"` added to its JSON body.
async fn naming(res: Response, platform: &str) -> Response {
    let status = res.status();
    let body = to_bytes(res.into_body(), usize::MAX).await.ok();
    let mut v: serde_json::Value = body
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({"error": "internal"}));
    v["platform"] = platform.into();
    (status, Json(v)).into_response()
}

/// Replaces a `draft` or `scheduled` post's set, platform, body, media, thread, first comment, cover and time; anything else
/// is `409 not_editable`.
#[utoipa::path(put, path = "/posts/{id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), request_body = PostInput,
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 200, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<PostInput>,
) -> Response {
    update_post(&app, &a, id, input).await
}

/// The body of `PUT /posts/{id}`, shared with the MCP `update-post` tool.
pub async fn update_post(app: &AppState, a: &Actor, id: Uuid, input: PostInput) -> Response {
    let saved = async {
        let Some(found) = find(&app.db, a, id).await? else {
            return Ok(Err(not_found()));
        };
        if !editable(&found.status) {
            return Ok(Err(reply(StatusCode::CONFLICT, "not_editable")));
        }
        let set = match checked(&app.db, a, &input).await? {
            Ok(set) => set,
            Err(res) => return Ok(Err(res)),
        };
        let txn = app.db.begin().await?;
        // One conditional statement, so a tick claiming the post cannot interleave.
        let done = posts::Entity::update_many()
            .col_expr(posts::Column::SocialSetId, set.into())
            .col_expr(posts::Column::Platform, input.platform.clone().into())
            .col_expr(posts::Column::Body, input.body.clone().into())
            .col_expr(posts::Column::Media, serde_json::json!(input.media).into())
            .col_expr(
                posts::Column::Thread,
                serde_json::json!(input.thread).into(),
            )
            .col_expr(posts::Column::FirstComment, stored_comment(&input).into())
            .col_expr(posts::Column::Cover, stored_cover(&input).into())
            .col_expr(posts::Column::Tiktok, stored_tiktok(&input).into())
            .col_expr(posts::Column::Youtube, stored_youtube(&input).into())
            .col_expr(posts::Column::Status, status_for(&input).into())
            .col_expr(posts::Column::ScheduledAt, when(&input).into())
            .col_expr(posts::Column::UpdatedAt, Utc::now().fixed_offset().into())
            .filter(posts::Column::Id.eq(id))
            .filter(posts::Column::Status.is_in(["draft", "scheduled"]))
            .filter(in_scope(a))
            .exec(&txn)
            .await?;
        if done.rows_affected == 0 {
            return Ok(Err(reply(StatusCode::CONFLICT, "not_editable")));
        }
        // A post moved to another set leaves the lists tied to its old set.
        let leave = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM post_list_items i USING post_lists l WHERE i.list_id = l.id \
             AND i.post_id = $1 AND l.social_set_id IS NOT NULL AND l.social_set_id <> $2",
            [id.into(), set.into()],
        );
        txn.execute_raw(leave).await?;
        if let Some(list) = input.list_id {
            let Some(p) = find(&txn, a, id).await? else {
                return Ok(Err(not_found()));
            };
            if let Err(res) = lists::place(&txn, a, list, &p, input.position).await? {
                return Ok(Err(res));
            }
        }
        txn.commit().await?;
        match find(&app.db, a, id).await? {
            Some(p) => one_json(&app.db, a, p).await.map(Ok),
            None => Ok(Err(not_found())),
        }
    };
    match saved.await {
        Ok(Ok(p)) => Json(p).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post update"),
    }
}

/// Publishes a `draft` or `scheduled` post now: `scheduled_at` becomes now and the post is
/// claimed (`queued`) and sent in the background. `202` with the queued post; poll it for the
/// result. At most 10 publish or take-down calls a minute per user; with scheduled posts, at
/// most 100 published a day per team (`429 publish_quota`).
#[utoipa::path(post, path = "/posts/{id}/publish", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 202, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 429, body = Error)))]
async fn publish_now(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    if !app.post_limiter.hit(a.key()) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let claimed = async {
        let Some(post) = find(&app.db, &a, id).await? else {
            return Ok(Err(not_found()));
        };
        // The claim and its hit of the daily cap (shared with scheduled publishes) commit
        // together. `quota_reserved = $2` keeps a stale read from skipping the reserve.
        let claim = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE posts p SET status = 'queued', quota_reserved = true, \
             scheduled_at = now(), updated_at = now() \
             WHERE p.id = $1 AND p.status IN ('draft', 'scheduled') AND p.quota_reserved = $2 \
             AND ($3::uuid IS NULL OR p.social_set_id = $3) \
             AND EXISTS (SELECT 1 FROM social_sets s \
                         WHERE s.id = p.social_set_id AND s.deleted_at IS NULL)",
            [id.into(), post.quota_reserved.into(), a.set_id().into()],
        );
        match publish::reserve_and(&app.db, a.team.id, post.quota_reserved, claim).await? {
            publish::Quota::Reserved => {}
            publish::Quota::Over => {
                return Ok(Err(reply(StatusCode::TOO_MANY_REQUESTS, "publish_quota")))
            }
            publish::Quota::Lost => return Ok(Err(reply(StatusCode::CONFLICT, "not_editable"))),
        }
        match find(&app.db, &a, id).await? {
            Some(p) => one_json(&app.db, &a, p).await.map(Ok),
            None => Ok(Err(not_found())),
        }
    };
    match claimed.await {
        Ok(Ok(p)) => {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = publish::publish_post(&app, id).await {
                    tracing::error!(error = %e, post = %id, "publish now failed");
                }
            });
            (StatusCode::ACCEPTED, Json(p)).into_response()
        }
        Ok(Err(res)) => res,
        Err(e) => internal(e, "post publish"),
    }
}

/// Deletes a post. `draft` or `scheduled`: removed (`204`). `published`: taken down from every
/// account and kept as `deleted` (`200`); when an account cannot take it down, `422
/// {"error":"takedown_failed","message"}` says what to do, and a retry only tries what is still
/// up. `queued` and `failed`: `409 not_deletable`.
#[utoipa::path(delete, path = "/posts/{id}", tag = "posts",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 204), (status = 200, body = Post), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
async fn destroy(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    delete_post(&app, &a, id).await
}

/// The body of `DELETE /posts/{id}`, shared with the MCP `delete-post` tool.
pub async fn delete_post(app: &AppState, a: &Actor, id: Uuid) -> Response {
    let found = match find(&app.db, a, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "post read"),
    };
    match found.status.as_str() {
        "draft" | "scheduled" => {
            let gone = posts::Entity::delete_many()
                .filter(posts::Column::Id.eq(id))
                .filter(posts::Column::Status.is_in(["draft", "scheduled"]))
                .filter(in_scope(a))
                .exec(&app.db)
                .await;
            match gone {
                Ok(r) if r.rows_affected == 1 => {
                    app.media.release_post(&app.db, id).await;
                    StatusCode::NO_CONTENT.into_response()
                }
                Ok(_) => reply(StatusCode::CONFLICT, "not_deletable"),
                Err(e) => internal(e, "post delete"),
            }
        }
        "published" | "deleted" => {
            if found.status == "published" {
                if !app.post_limiter.hit(a.key()) {
                    return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
                }
                match publish::takedown(app, &found).await {
                    Ok(problems) if problems.is_empty() => {
                        app.media.release_post(&app.db, id).await;
                    }
                    // 422, never 502: a proxy may swap a 502 body for its own page.
                    Ok(problems) => {
                        let body = serde_json::json!({
                            "error": "takedown_failed",
                            "message": problems.join(" "),
                        });
                        return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
                    }
                    Err(e) => return internal(e, "post takedown"),
                }
            }
            match find(&app.db, a, id).await {
                Ok(Some(p)) => match one_json(&app.db, a, p).await {
                    Ok(p) => Json(p).into_response(),
                    Err(e) => internal(e, "post read"),
                },
                Ok(None) => not_found(),
                Err(e) => internal(e, "post read"),
            }
        }
        _ => reply(StatusCode::CONFLICT, "not_deletable"),
    }
}

/// Re-queues the retryable targets of the posts `ids` in one transaction and returns the posts
/// to publish. Retryable: `failed` targets of `published` or `failed` posts (never a
/// `published` one), those cut off midway through a thread continuing after the parts already out.
/// Nothing changes unless every post is settled: `409 publish_in_progress` while a post is
/// `queued` or a target is `pending` or `publishing`, `409 nothing_to_retry` without a
/// retryable target.
async fn requeue(
    app: &AppState,
    a: &Actor,
    ids: &[Uuid],
) -> Result<Result<(Vec<Uuid>, Vec<SkippedTarget>), Response>, DbErr> {
    let txn = app.db.begin().await?;
    // The rows are locked, so two retries (or a retry and a settle) cannot interleave.
    let rows = posts::Entity::find()
        .filter(posts::Column::Id.is_in(ids.iter().copied()))
        .filter(posts::Column::TeamId.eq(a.team.id))
        .filter(posts::Column::SocialSetId.in_subquery(live_sets()))
        .filter(in_scope(a))
        .order_by_asc(posts::Column::Id)
        .lock_exclusive()
        .all(&txn)
        .await?;
    if rows.len() != ids.len() {
        return Ok(Err(not_found()));
    }
    let targets = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.is_in(ids.iter().copied()))
        .all(&txn)
        .await?;
    let busy = rows.iter().any(|p| p.status == "queued")
        || targets
            .iter()
            .any(|t| t.status == "pending" || t.status == "publishing");
    if busy {
        return Ok(Err(reply(StatusCode::CONFLICT, "publish_in_progress")));
    }
    let mut again = Vec::new();
    let mut posts = Vec::new();
    let mut skipped = Vec::new();
    for t in &targets {
        let post = rows.iter().find(|p| p.id == t.post_id);
        if let Some(p) = post.filter(|p| p.status == "published" || p.status == "failed") {
            if publish::retryable(t, p) {
                again.push(t.id);
                posts.push(p.id);
            } else if publish::resendable(t, p) {
                skipped.push(SkippedTarget {
                    target_id: t.id,
                    post_id: p.id,
                    social_account_id: t.social_account_id,
                    reason: "outcome_unknown".into(),
                    error: t.error.clone(),
                });
            }
        }
    }
    posts.sort();
    posts.dedup();
    if again.is_empty() && skipped.is_empty() {
        return Ok(Err(reply(StatusCode::CONFLICT, "nothing_to_retry")));
    }
    if again.is_empty() {
        let body = serde_json::json!({
            "error": "outcome_unknown",
            "message": "Every failed target may already have posted. Check each account on the \
                        platform; nothing was sent again.",
            "targets": skipped,
        });
        return Ok(Err((StatusCode::CONFLICT, Json(body)).into_response()));
    }
    let now = Utc::now().fixed_offset();
    post_targets::Entity::update_many()
        .col_expr(post_targets::Column::Status, "pending".into())
        .col_expr(post_targets::Column::Error, Option::<String>::None.into())
        .col_expr(post_targets::Column::UpdatedAt, now.into())
        .filter(post_targets::Column::Id.is_in(again))
        .exec(&txn)
        .await?;
    posts::Entity::update_many()
        .col_expr(posts::Column::Status, "queued".into())
        .col_expr(posts::Column::UpdatedAt, now.into())
        .filter(posts::Column::Id.is_in(posts.iter().copied()))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(Ok((posts, skipped)))
}

/// Sends the posts `ids` (queued by `requeue`) in the background, each target once.
fn publish_in_background(app: &AppState, ids: Vec<Uuid>) {
    for id in ids {
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = publish::publish_post(&app, id).await {
                tracing::error!(error = %e, post = %id, "retry failed");
            }
        });
    }
}

/// Publishes a `published` or `failed` post's `failed` targets again; `202` with the queued
/// post, poll it for the result. A `published` target is never sent again, and no target is
/// added for an account that joined the set since the first publish. A target that failed
/// midway through a thread goes on after the parts already out; any other starts over, with the
/// first comment and cover as the post says. A failure that may already have posted (a
/// timeout, a run cut off, a platform that never said it was done) is not sent again: it is
/// listed in `skipped_targets`, and when nothing else failed the answer is `409
/// outcome_unknown` with those `targets` and a message to check the account on the platform.
/// `409 publish_in_progress` while the post is `queued` or a target is `pending` or
/// `publishing`, `409 nothing_to_retry` when no target failed. At most 10 publish, retry or
/// take-down calls a minute per user. A retry does not count against the daily publish cap
/// again. Webhooks: a target fires `post.target.failed` once, so a retry that fails again sends
/// no second one.
#[utoipa::path(post, path = "/posts/{id}/retry", tag = "posts",
    security(("session" = []), ("api_token" = [])),
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 202, body = PostRetry), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 429, body = Error)))]
async fn retry_post(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    if !app.post_limiter.hit(a.key()) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let done = async {
        if find(&app.db, &a, id).await?.is_none() {
            return Ok(not_found());
        }
        let (queued, skipped_targets) = match requeue(&app, &a, &[id]).await? {
            Ok(q) => q,
            Err(res) => return Ok(res),
        };
        let Some(p) = find(&app.db, &a, id).await? else {
            return Ok(not_found());
        };
        let p = one_json(&app.db, &a, p).await?;
        publish_in_background(&app, queued);
        let body = PostRetry {
            post: p,
            skipped_targets,
        };
        Ok::<_, DbErr>((StatusCode::ACCEPTED, Json(body)).into_response())
    };
    done.await.unwrap_or_else(|e| internal(e, "post retry"))
}

/// `POST /posts/{id}/retry` for every post of a group at once: all or nothing, so `409
/// publish_in_progress` when any post of the group is still going out. Posts with nothing to
/// retry stay as they are; `409 nothing_to_retry` when none has a failed target, `409
/// outcome_unknown` when the only failures may already have posted. `202` with the group and
/// the `skipped_targets`.
#[utoipa::path(post, path = "/post-groups/{group_id}/retry", tag = "posts",
    security(("session" = []), ("api_token" = [])),
    params(("group_id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 202, body = GroupRetry), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 429, body = Error)))]
async fn retry_group(
    State(app): State<AppState>,
    a: Actor,
    Path(group_id): Path<Uuid>,
) -> Response {
    if !app.post_limiter.hit(a.key()) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let done = async {
        let ids: Vec<Uuid> = group_rows(&app.db, &a, group_id)
            .await?
            .iter()
            .map(|p| p.id)
            .collect();
        if ids.is_empty() {
            return Ok(not_found());
        }
        let (queued, skipped_targets) = match requeue(&app, &a, &ids).await? {
            Ok(q) => q,
            Err(res) => return Ok(res),
        };
        let rows = group_rows(&app.db, &a, group_id).await?;
        let group = group_json(&app.db, &a, group_id, rows).await?;
        publish_in_background(&app, queued);
        let body = GroupRetry {
            group,
            skipped_targets,
        };
        Ok::<_, DbErr>((StatusCode::ACCEPTED, Json(body)).into_response())
    };
    done.await
        .unwrap_or_else(|e| internal(e, "post group retry"))
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(show, update, destroy))
        .routes(routes!(publish_now))
        .routes(routes!(retry_post))
}

/// `POST /post-groups`, apart so it can take a larger body than the rest of `/v1`.
pub fn groups_api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_group))
        .routes(routes!(show_group))
        .routes(routes!(retry_group))
}
