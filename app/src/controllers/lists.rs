//! Post lists: ordered groups of posts, such as a series of short videos. A post's 1-based
//! position in a list is its "Part N"; nothing is added to captions. A list is team-wide
//! (`social_set_id` null) or tied to one set, whose posts alone it then holds; a post can be in
//! several lists. A set token sees team-wide lists and its set's, and inside them only its set's
//! posts; positions are numbered over the posts the caller sees, so a deleted post (or one out
//! of reach) never leaves a gap. Item writes lock the list row and rewrite every position in one
//! transaction. Every route takes an API token too (`Actor`), and token writes take an optional
//! `Idempotency-Key` header (see `idempotency`).
use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use schemars::JsonSchema;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait, DbBackend, DbErr, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::{internal, Body},
    connections::live_sets,
    posts::{self, Post},
    waitlist::{error, Error},
};
use crate::{
    models::{post_lists, posts as post_rows, social_sets},
    state::AppState,
};

const IDEMPOTENCY: &str = "With an API token: 1 to 255 printable ASCII characters. A retry with the same key, path and body replays the first answer for 30 days";

#[derive(Serialize, ToSchema)]
pub struct PostList {
    pub id: Uuid,
    /// `null` for a team-wide list.
    pub social_set_id: Option<Uuid>,
    pub name: String,
    /// Platform playlist ids by platform. Reserved: always `{}` today.
    #[schema(value_type = Object)]
    pub remote_ids: serde_json::Value,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

/// A list with its posts in order.
#[derive(Serialize, ToSchema)]
pub struct ListDetail {
    #[serde(flatten)]
    pub list: PostList,
    pub items: Vec<Item>,
}

#[derive(Serialize, ToSchema)]
pub struct Item {
    pub post_id: Uuid,
    /// 1-based: the post is "Part N" of the list.
    pub position: i64,
    pub post: Post,
}

/// A list a post is in, as every post carries it.
#[derive(Serialize, ToSchema)]
pub struct ListRef {
    pub id: Uuid,
    pub name: String,
    pub position: i64,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct ListInput {
    /// 1 to 60 characters.
    pub name: String,
    /// A live set of the team; absent for a team-wide list. A set token must name its own set.
    pub social_set_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct ListName {
    pub name: String,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct AddPost {
    pub post_id: Uuid,
    /// 1-based; absent or past the end appends. A post already in the list moves.
    pub position: Option<i32>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct Order {
    /// Every post of the list the caller sees, each once, in the new order.
    pub post_ids: Vec<Uuid>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn invalid(code: &str) -> Response {
    reply(StatusCode::UNPROCESSABLE_ENTITY, code)
}

fn list_json(l: post_lists::Model) -> PostList {
    PostList {
        id: l.id,
        social_set_id: l.social_set_id,
        name: l.name,
        remote_ids: l.remote_ids,
        created_at: l.created_at,
        updated_at: l.updated_at,
    }
}

/// Lists the caller sees: live, of a live set (or team-wide), and for a set token team-wide or
/// its set's.
fn visible(a: &Actor) -> Condition {
    let team_wide = post_lists::Column::SocialSetId.is_null();
    let mut c = Condition::all()
        .add(post_lists::Column::TeamId.eq(a.team.id))
        .add(post_lists::Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(team_wide.clone())
                .add(post_lists::Column::SocialSetId.in_subquery(live_sets())),
        );
    if let Some(set) = a.set_id() {
        c = c.add(
            Condition::any()
                .add(team_wide)
                .add(post_lists::Column::SocialSetId.eq(set)),
        );
    }
    c
}

async fn find(
    db: &impl ConnectionTrait,
    a: &Actor,
    id: Uuid,
    lock: bool,
) -> Result<Option<post_lists::Model>, DbErr> {
    let sel = post_lists::Entity::find_by_id(id).filter(visible(a));
    if lock {
        sel.lock_exclusive().one(db).await
    } else {
        sel.one(db).await
    }
}

/// Whether an item's post is seen: its set is live, it is not `deleted`, it matches a set-tied
/// list, and it is in a set token's set (`$2`). Needs `p`, `s` and `l` joined.
const SEEN: &str = "s.deleted_at IS NULL AND p.status <> 'deleted' \
    AND (l.social_set_id IS NULL OR l.social_set_id = p.social_set_id) \
    AND ($2::uuid IS NULL OR p.social_set_id = $2)";

const JOINS: &str = "FROM post_list_items i JOIN post_lists l ON l.id = i.list_id \
    JOIN posts p ON p.id = i.post_id JOIN social_sets s ON s.id = p.social_set_id";

struct Placed {
    list_id: Uuid,
    name: String,
    post_id: Uuid,
    position: i64,
}

/// The seen items of list `list`, or of every live list holding one of `posts`, numbered from 1
/// over the seen items of each list. Ordered by list, then position.
async fn placed(
    db: &impl ConnectionTrait,
    a: &Actor,
    list: Option<Uuid>,
    posts: Option<&[Uuid]>,
) -> Result<Vec<Placed>, DbErr> {
    let sql = format!(
        "SELECT list_id, name, post_id, position FROM ( \
           SELECT i.list_id, l.name, l.created_at, i.post_id, \
             row_number() OVER (PARTITION BY i.list_id ORDER BY i.position) AS position \
           {JOINS} \
           WHERE l.team_id = $1 AND l.deleted_at IS NULL AND {SEEN} \
             AND ($3::uuid IS NULL OR i.list_id = $3) \
             AND ($4::jsonb IS NULL OR i.list_id IN (SELECT list_id FROM post_list_items \
               WHERE post_id IN (SELECT jsonb_array_elements_text($4)::uuid)))) x \
         WHERE $4::jsonb IS NULL OR post_id IN (SELECT jsonb_array_elements_text($4)::uuid) \
         ORDER BY created_at, list_id, position"
    );
    let posts = posts.map(|p| serde_json::json!(p));
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        [
            a.team.id.into(),
            a.set_id().into(),
            list.into(),
            posts.into(),
        ],
    );
    db.query_all_raw(stmt)
        .await?
        .iter()
        .map(|r| {
            Ok(Placed {
                list_id: r.try_get("", "list_id")?,
                name: r.try_get("", "name")?,
                post_id: r.try_get("", "post_id")?,
                position: r.try_get("", "position")?,
            })
        })
        .collect()
}

/// The lists each of `posts` is in, for the `lists` of the Post JSON. One query.
pub(crate) async fn lists_of(
    db: &impl ConnectionTrait,
    a: &Actor,
    posts: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ListRef>>, DbErr> {
    let mut map: HashMap<Uuid, Vec<ListRef>> = HashMap::new();
    if posts.is_empty() {
        return Ok(map);
    }
    for p in placed(db, a, None, Some(posts)).await? {
        map.entry(p.post_id).or_default().push(ListRef {
            id: p.list_id,
            name: p.name,
            position: p.position,
        });
    }
    Ok(map)
}

/// Every item of a list in stored order, with whether the caller sees it.
async fn order_of(
    db: &impl ConnectionTrait,
    a: &Actor,
    list: Uuid,
) -> Result<Vec<(Uuid, bool)>, DbErr> {
    let sql = format!(
        "SELECT i.post_id, ({SEEN}) AS seen {JOINS} WHERE i.list_id = $1 ORDER BY i.position"
    );
    let stmt =
        Statement::from_sql_and_values(DbBackend::Postgres, sql, [list.into(), a.set_id().into()]);
    db.query_all_raw(stmt)
        .await?
        .iter()
        .map(|r| Ok((r.try_get("", "post_id")?, r.try_get("", "seen")?)))
        .collect()
}

/// Replaces a list's items with `ids`, numbered from 1. Run under the list row lock.
async fn rewrite(db: &impl ConnectionTrait, list: Uuid, ids: &[Uuid]) -> Result<(), DbErr> {
    let pg = |sql: &str, v: Vec<sea_orm::Value>| {
        Statement::from_sql_and_values(DbBackend::Postgres, sql, v)
    };
    db.execute_raw(pg(
        "DELETE FROM post_list_items WHERE list_id = $1",
        vec![list.into()],
    ))
    .await?;
    db.execute_raw(pg(
        "INSERT INTO post_list_items (list_id, post_id, position) \
         SELECT $1, e.v::uuid, e.n::int FROM jsonb_array_elements_text($2) WITH ORDINALITY e(v, n)",
        vec![list.into(), serde_json::json!(ids).into()],
    ))
    .await?;
    db.execute_raw(pg(
        "UPDATE post_lists SET updated_at = now() WHERE id = $1",
        vec![list.into()],
    ))
    .await
    .map(|_| ())
}

/// Puts `post` at `position` (1-based over the items the caller sees; absent or past the end
/// appends) of list `list_id`, moving it when it is already there. Locks the list row, so run it
/// in a transaction. `404` for a list the caller does not see; `422 invalid_post` for a deleted
/// post; `422 invalid_set` when a set-tied list is of another set than the post.
pub(crate) async fn place(
    db: &impl ConnectionTrait,
    a: &Actor,
    list_id: Uuid,
    post: &post_rows::Model,
    position: Option<i32>,
) -> Result<Result<(), Response>, DbErr> {
    if position.is_some_and(|p| p < 1) {
        return Ok(Err(invalid("invalid_position")));
    }
    let Some(list) = find(db, a, list_id, true).await? else {
        return Ok(Err(not_found()));
    };
    if post.status == "deleted" {
        return Ok(Err(invalid("invalid_post")));
    }
    if list.social_set_id.is_some_and(|s| s != post.social_set_id) {
        return Ok(Err(invalid("invalid_set")));
    }
    let mut order = order_of(db, a, list.id).await?;
    order.retain(|(id, _)| *id != post.id);
    // The stored index of the item now seen at `position`, which the post goes before.
    let at = position.and_then(|p| {
        order
            .iter()
            .enumerate()
            .filter(|(_, (_, seen))| *seen)
            .nth(p as usize - 1)
            .map(|(i, _)| i)
    });
    let at = at.unwrap_or(order.len());
    order.insert(at, (post.id, true));
    let ids: Vec<Uuid> = order.into_iter().map(|(id, _)| id).collect();
    rewrite(db, list.id, &ids).await?;
    Ok(Ok(()))
}

/// The list with its seen posts in order.
async fn detail(
    db: &impl ConnectionTrait,
    a: &Actor,
    list: post_lists::Model,
) -> Result<ListDetail, DbErr> {
    let placed = placed(db, a, Some(list.id), None).await?;
    let ids: Vec<Uuid> = placed.iter().map(|p| p.post_id).collect();
    let rows = post_rows::Entity::find()
        .filter(post_rows::Column::Id.is_in(ids))
        .all(db)
        .await?;
    let mut by_id: HashMap<Uuid, Post> = posts::posts_json(db, a, rows)
        .await?
        .into_iter()
        .map(|p| (p.id, p))
        .collect();
    let items = placed
        .into_iter()
        .filter_map(|p| {
            by_id.remove(&p.post_id).map(|post| Item {
                post_id: p.post_id,
                position: p.position,
                post,
            })
        })
        .collect();
    Ok(ListDetail {
        list: list_json(list),
        items,
    })
}

/// `200` with the list's detail, read after a write.
async fn detail_reply(app: &AppState, a: &Actor, id: Uuid, what: &str) -> Response {
    let found = async {
        match find(&app.db, a, id, false).await? {
            Some(l) => detail(&app.db, a, l).await.map(Some),
            None => Ok(None),
        }
    };
    match found.await {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, what),
    }
}

/// The lists the caller sees, oldest first, without their posts.
#[utoipa::path(get, path = "/lists", tag = "lists",
    security(("session" = []), ("api_token" = [])),
    responses((status = 200, body = [PostList]), (status = 401, body = Error)))]
async fn list(State(app): State<AppState>, a: Actor) -> Response {
    list_lists(&app, &a).await
}

/// The body of `GET /lists`, shared with the MCP `list-lists` tool.
pub async fn list_lists(app: &AppState, a: &Actor) -> Response {
    let found = post_lists::Entity::find()
        .filter(visible(a))
        .order_by_asc(post_lists::Column::CreatedAt)
        .order_by_asc(post_lists::Column::Id)
        .all(&app.db)
        .await;
    match found {
        Ok(rows) => Json(rows.into_iter().map(list_json).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e, "list list"),
    }
}

/// Creates a list, team-wide or tied to one set. A set token must name its own set (else `422
/// invalid_set`).
#[utoipa::path(post, path = "/lists", tag = "lists",
    security(("session" = []), ("api_token" = [])), request_body = ListInput,
    params(("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 201, body = ListDetail), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn create(State(app): State<AppState>, a: Actor, Body(input): Body<ListInput>) -> Response {
    create_list(&app, &a, input).await
}

/// The body of `POST /lists`, shared with the MCP `create-list` tool.
pub async fn create_list(app: &AppState, a: &Actor, input: ListInput) -> Response {
    let Some(name) = team::name(&input.name) else {
        return invalid("invalid_name");
    };
    let set = match (a.set_id(), input.social_set_id) {
        (Some(own), Some(id)) if id == own => Some(own),
        (Some(_), _) => return invalid("invalid_set"),
        (None, set) => set,
    };
    let made = async {
        if let Some(id) = set {
            let live = social_sets::Entity::find_by_id(id)
                .filter(social_sets::Column::TeamId.eq(a.team.id))
                .filter(social_sets::Column::DeletedAt.is_null())
                .one(&app.db)
                .await?;
            if live.is_none() {
                return Ok(None);
            }
        }
        let row = post_lists::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(a.team.id),
            social_set_id: Set(set),
            name: Set(name),
            remote_ids: Set(serde_json::json!({})),
            ..Default::default()
        };
        let l = post_lists::Entity::insert(row)
            .exec_with_returning(&app.db)
            .await?;
        Ok::<_, DbErr>(Some(ListDetail {
            list: list_json(l),
            items: Vec::new(),
        }))
    };
    match made.await {
        Ok(Some(d)) => (StatusCode::CREATED, Json(d)).into_response(),
        Ok(None) => invalid("invalid_set"),
        Err(e) => internal(e, "list insert"),
    }
}

/// One list with its posts in order.
#[utoipa::path(get, path = "/lists/{id}", tag = "lists",
    security(("session" = []), ("api_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = ListDetail), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    get_list(&app, &a, id).await
}

/// The body of `GET /lists/{id}`, shared with the MCP `get-list` tool.
pub async fn get_list(app: &AppState, a: &Actor, id: Uuid) -> Response {
    detail_reply(app, a, id, "list read").await
}

/// A set token may change a team-wide list's items but not the list itself.
fn may_edit(a: &Actor, l: &post_lists::Model) -> bool {
    a.set_id().is_none() || l.social_set_id.is_some()
}

/// Renames a list. A set token cannot rename a team-wide list (`403`).
#[utoipa::path(patch, path = "/lists/{id}", tag = "lists",
    security(("session" = []), ("api_token" = [])), request_body = ListName,
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 200, body = ListDetail), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn rename(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<ListName>,
) -> Response {
    let Some(name) = team::name(&input.name) else {
        return invalid("invalid_name");
    };
    let done = async {
        let Some(l) = find(&app.db, &a, id, false).await? else {
            return Ok(Err(not_found()));
        };
        if !may_edit(&a, &l) {
            return Ok(Err(reply(StatusCode::FORBIDDEN, "forbidden")));
        }
        post_lists::Entity::update_many()
            .col_expr(post_lists::Column::Name, name.into())
            .col_expr(
                post_lists::Column::UpdatedAt,
                Utc::now().fixed_offset().into(),
            )
            .filter(post_lists::Column::Id.eq(id))
            .filter(visible(&a))
            .exec(&app.db)
            .await?;
        Ok::<_, DbErr>(Ok(()))
    };
    match done.await {
        Ok(Ok(())) => detail_reply(&app, &a, id, "list read").await,
        Ok(Err(res)) => res,
        Err(e) => internal(e, "list rename"),
    }
}

/// Deletes a list; its posts stay. A set token cannot delete a team-wide list (`403`).
#[utoipa::path(delete, path = "/lists/{id}", tag = "lists",
    security(("session" = []), ("api_token" = [])),
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    let done = async {
        let Some(l) = find(&app.db, &a, id, false).await? else {
            return Ok(Err(not_found()));
        };
        if !may_edit(&a, &l) {
            return Ok(Err(reply(StatusCode::FORBIDDEN, "forbidden")));
        }
        let now = Utc::now().fixed_offset();
        post_lists::Entity::update_many()
            .col_expr(post_lists::Column::DeletedAt, Some(now).into())
            .col_expr(post_lists::Column::UpdatedAt, now.into())
            .filter(post_lists::Column::Id.eq(id))
            .filter(visible(&a))
            .exec(&app.db)
            .await?;
        Ok::<_, DbErr>(Ok(()))
    };
    match done.await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "list delete"),
    }
}

/// Adds a post the caller sees (else `422 invalid_post`) at `position`, appending by default; a
/// post already in the list moves there. A set-tied list only takes its set's posts (`422
/// invalid_set`).
#[utoipa::path(post, path = "/lists/{id}/posts", tag = "lists",
    security(("session" = []), ("api_token" = [])), request_body = AddPost,
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 200, body = ListDetail), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn add(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<AddPost>,
) -> Response {
    add_post(&app, &a, id, input).await
}

/// The body of `POST /lists/{id}/posts`, shared with the MCP `add-post-to-list` tool.
pub async fn add_post(app: &AppState, a: &Actor, id: Uuid, input: AddPost) -> Response {
    let done = async {
        let txn = app.db.begin().await?;
        let Some(post) = posts::find(&txn, a, input.post_id).await? else {
            return Ok(Err(invalid("invalid_post")));
        };
        if let Err(res) = place(&txn, a, id, &post, input.position).await? {
            return Ok(Err(res));
        }
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(()))
    };
    match done.await {
        Ok(Ok(())) => detail_reply(app, a, id, "list read").await,
        Ok(Err(res)) => res,
        Err(e) => internal(e, "list add"),
    }
}

/// Takes a post the caller sees out of the list; the posts after it move up.
#[utoipa::path(delete, path = "/lists/{id}/posts/{post_id}", tag = "lists",
    security(("session" = []), ("api_token" = [])),
    params(("id" = Uuid, Path), ("post_id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn remove(
    State(app): State<AppState>,
    a: Actor,
    Path((id, post_id)): Path<(Uuid, Uuid)>,
) -> Response {
    remove_post(&app, &a, id, post_id).await
}

/// The body of `DELETE /lists/{id}/posts/{post_id}`, shared with the MCP
/// `remove-post-from-list` tool.
pub async fn remove_post(app: &AppState, a: &Actor, id: Uuid, post_id: Uuid) -> Response {
    let done = async {
        let txn = app.db.begin().await?;
        if find(&txn, a, id, true).await?.is_none() {
            return Ok(false);
        }
        let mut order = order_of(&txn, a, id).await?;
        let before = order.len();
        order.retain(|(p, seen)| !(*p == post_id && *seen));
        if order.len() == before {
            return Ok(false);
        }
        let ids: Vec<Uuid> = order.into_iter().map(|(p, _)| p).collect();
        rewrite(&txn, id, &ids).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(true)
    };
    match done.await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e, "list remove"),
    }
}

/// Reorders the list. `post_ids` holds every post of the list the caller sees, each once (else
/// `422 invalid_order`); posts out of a set token's reach keep their places.
#[utoipa::path(put, path = "/lists/{id}/order", tag = "lists",
    security(("session" = []), ("api_token" = [])), request_body = Order,
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 200, body = ListDetail), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn reorder(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<Order>,
) -> Response {
    reorder_list(&app, &a, id, input).await
}

/// The body of `PUT /lists/{id}/order`, shared with the MCP `reorder-list` tool.
pub async fn reorder_list(app: &AppState, a: &Actor, id: Uuid, input: Order) -> Response {
    let done = async {
        let txn = app.db.begin().await?;
        if find(&txn, a, id, true).await?.is_none() {
            return Ok(Err(not_found()));
        }
        let order = order_of(&txn, a, id).await?;
        let mut seen: Vec<Uuid> = order.iter().filter(|o| o.1).map(|o| o.0).collect();
        let mut want = input.post_ids.clone();
        seen.sort_unstable();
        want.sort_unstable();
        if seen != want {
            return Ok(Err(invalid("invalid_order")));
        }
        // The seen slots take the new order; the rest stay put.
        let mut next = input.post_ids.into_iter();
        let ids: Vec<Uuid> = order
            .into_iter()
            .map(|(p, s)| if s { next.next().unwrap_or(p) } else { p })
            .collect();
        rewrite(&txn, id, &ids).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(()))
    };
    match done.await {
        Ok(Ok(())) => detail_reply(app, a, id, "list read").await,
        Ok(Err(res)) => res,
        Err(e) => internal(e, "list reorder"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(show, rename, destroy))
        .routes(routes!(add))
        .routes(routes!(remove))
        .routes(routes!(reorder))
}
