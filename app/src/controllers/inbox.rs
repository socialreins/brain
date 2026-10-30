//! Inboxes and their items, for the current team. Members read inboxes, reply to items and
//! mark them done; owners and admins create, change and delete inboxes, as with sets. Every
//! team has an "All" inbox (made on first list) that cannot be changed or deleted.
use std::collections::HashMap;

use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{
    connectors::bluesky::{self, InboxError},
    inbox::{self, filter_sql, Filters, Via},
    models::{inbox_items, inbox_replies, inboxes, social_accounts, teams},
    state::AppState,
};

/// Most inboxes one team holds, "All" included.
const MAX_INBOXES: u64 = 50;
const PAGE: u64 = 50;
const MAX_PAGE: u64 = 100;

#[derive(Serialize, ToSchema)]
pub struct Inbox {
    pub id: Uuid,
    pub name: String,
    /// The team's "All" inbox: every item, cannot be changed or deleted.
    pub is_all: bool,
    pub filters: Filters,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxInput {
    /// 1 to 60 characters after trimming.
    pub name: String,
    #[serde(default)]
    pub filters: Filters,
}

#[derive(Serialize, ToSchema)]
pub struct Author {
    /// The platform's id, e.g. a Bluesky DID.
    pub id: String,
    pub handle: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// A reply sent from SocialReins, by a person or an automation.
#[derive(Serialize, ToSchema)]
pub struct Reply {
    pub id: Uuid,
    /// `public` (in the thread) or `dm`.
    pub kind: String,
    pub text: String,
    /// The platform's id: a Bluesky post `at://` URI or a chat message id.
    pub remote_id: Option<String>,
    /// The public link, for public replies.
    pub remote_url: Option<String>,
    /// The member who sent it; `null` for automations.
    pub sent_by: Option<Uuid>,
    pub automation_id: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct Item {
    pub id: Uuid,
    pub social_account_id: Uuid,
    pub platform: String,
    /// `comment` (a reply to the account), `mention` (a mention or quote) or `dm`.
    pub kind: String,
    pub remote_id: String,
    /// The public link, for comments and mentions.
    pub remote_url: Option<String>,
    /// The post this one answers.
    pub parent_remote_id: Option<String>,
    /// The thread's first post.
    pub root_remote_id: Option<String>,
    /// The DM conversation.
    pub convo_id: Option<String>,
    /// Our post the thread hangs off, when it was published through SocialReins.
    pub post_id: Option<Uuid>,
    pub author: Author,
    pub text: String,
    pub received_at: DateTime<FixedOffset>,
    /// `open` or `done`.
    pub state: String,
    /// When the last reply went out.
    pub replied_at: Option<DateTime<FixedOffset>>,
    /// Replies sent from SocialReins, oldest first.
    pub replies: Vec<Reply>,
}

#[derive(Serialize, ToSchema)]
pub struct ItemPage {
    /// Newest first.
    pub items: Vec<Item>,
    /// Pass as `cursor` for the next page; `null` on the last.
    pub next_cursor: Option<Uuid>,
}

#[derive(Deserialize, IntoParams)]
pub struct ItemQuery {
    /// `open` or `done`; both when absent.
    pub state: Option<String>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<Uuid>,
    /// 1 to 100, default 50.
    pub limit: Option<u64>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyInput {
    /// A public reply for comments and mentions (at most 300 graphemes), a DM in the
    /// conversation for DMs (at most 1000).
    pub text: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StateInput {
    /// `open` or `done`.
    pub state: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn forbidden() -> Response {
    reply(StatusCode::FORBIDDEN, "forbidden")
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn inbox_json(i: inboxes::Model) -> Inbox {
    Inbox {
        id: i.id,
        name: i.name,
        is_all: i.is_all,
        filters: serde_json::from_value(i.filters).unwrap_or_default(),
        created_at: i.created_at,
        updated_at: i.updated_at,
    }
}

fn url_of(remote_id: &str) -> Option<String> {
    bluesky::post_url(remote_id)
}

async fn items_json(
    db: &impl ConnectionTrait,
    rows: Vec<inbox_items::Model>,
) -> Result<Vec<Item>, DbErr> {
    let replies = inbox_replies::Entity::find()
        .filter(inbox_replies::Column::InboxItemId.is_in(rows.iter().map(|i| i.id)))
        .order_by_asc(inbox_replies::Column::CreatedAt)
        .order_by_asc(inbox_replies::Column::Id)
        .all(db)
        .await?;
    let mut by_item: HashMap<Uuid, Vec<Reply>> = HashMap::new();
    for r in replies {
        by_item.entry(r.inbox_item_id).or_default().push(Reply {
            id: r.id,
            remote_url: r.remote_id.as_deref().and_then(url_of),
            kind: r.kind,
            text: r.text,
            remote_id: r.remote_id,
            sent_by: r.sent_by,
            automation_id: r.automation_id,
            created_at: r.created_at,
        });
    }
    Ok(rows
        .into_iter()
        .map(|i| Item {
            replies: by_item.remove(&i.id).unwrap_or_default(),
            id: i.id,
            social_account_id: i.social_account_id,
            remote_url: url_of(&i.remote_id),
            platform: i.platform,
            kind: i.kind,
            remote_id: i.remote_id,
            parent_remote_id: i.parent_remote_id,
            root_remote_id: i.root_remote_id,
            convo_id: i.convo_id,
            post_id: i.post_id,
            author: Author {
                id: i.author_id,
                handle: i.author_handle,
                name: i.author_name,
                avatar_url: i.author_avatar,
            },
            text: i.text,
            received_at: i.received_at,
            state: i.state,
            replied_at: i.replied_at,
        })
        .collect())
}

async fn item_json(db: &impl ConnectionTrait, i: inbox_items::Model) -> Result<Item, DbErr> {
    let mut list = items_json(db, vec![i]).await?;
    list.pop()
        .ok_or_else(|| DbErr::RecordNotFound("item vanished".into()))
}

async fn find_inbox(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    id: Uuid,
) -> Result<Option<inboxes::Model>, DbErr> {
    inboxes::Entity::find_by_id(id)
        .filter(inboxes::Column::TeamId.eq(team_id))
        .one(db)
        .await
}

async fn find_item(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    id: Uuid,
) -> Result<Option<inbox_items::Model>, DbErr> {
    inbox_items::Entity::find_by_id(id)
        .filter(inbox_items::Column::TeamId.eq(team_id))
        .one(db)
        .await
}

/// The input's name and filters, checked; `Err` holds the 422 reply.
async fn checked(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    input: InboxInput,
) -> Result<Result<(String, Filters), Response>, DbErr> {
    let Some(name) = team::name(&input.name) else {
        return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name")));
    };
    let filters = match input.filters.checked() {
        Ok(f) => f,
        Err(code) => return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, code))),
    };
    if !inbox::owned(db, team_id, filters.social_set_id, &filters.account_ids).await? {
        return Ok(Err(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_filters",
        )));
    }
    Ok(Ok((name, filters)))
}

/// The team's inboxes, "All" first, then oldest first. Makes "All" when it is missing.
#[utoipa::path(get, path = "/inboxes", tag = "inbox",
    responses((status = 200, body = [Inbox]), (status = 401, body = Error)))]
async fn list(State(app): State<AppState>, t: InTeam) -> Response {
    let found = async {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO inboxes (id, team_id, name, is_all) VALUES ($1, $2, 'All', true) \
             ON CONFLICT (team_id) WHERE is_all DO NOTHING",
            [Uuid::now_v7().into(), t.team.id.into()],
        );
        app.db.execute_raw(stmt).await?;
        inboxes::Entity::find()
            .filter(inboxes::Column::TeamId.eq(t.team.id))
            .order_by_desc(inboxes::Column::IsAll)
            .order_by_asc(inboxes::Column::CreatedAt)
            .order_by_asc(inboxes::Column::Id)
            .all(&app.db)
            .await
    };
    match found.await {
        Ok(rows) => Json(rows.into_iter().map(inbox_json).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e, "inbox list"),
    }
}

/// Creates an inbox. Owners and admins; at most 50 per team. Account and set ids must be the
/// team's.
#[utoipa::path(post, path = "/inboxes", tag = "inbox", request_body = InboxInput,
    responses((status = 201, body = Inbox), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(input): Body<InboxInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let made = async {
        let (name, filters) = match checked(&app.db, t.team.id, input).await? {
            Ok(v) => v,
            Err(res) => return Ok(Err(res)),
        };
        let txn = app.db.begin().await?;
        // The team row lock keeps concurrent creates under the cap.
        teams::Entity::find_by_id(t.team.id)
            .lock_exclusive()
            .one(&txn)
            .await?;
        let n = inboxes::Entity::find()
            .filter(inboxes::Column::TeamId.eq(t.team.id))
            .count(&txn)
            .await?;
        if n >= MAX_INBOXES {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_inboxes")));
        }
        let row = inboxes::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(t.team.id),
            name: Set(name),
            is_all: Set(false),
            filters: Set(serde_json::json!(filters)),
            ..Default::default()
        };
        let made = inboxes::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(inbox_json(made)))
    };
    match made.await {
        Ok(Ok(i)) => (StatusCode::CREATED, Json(i)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "inbox insert"),
    }
}

/// Replaces an inbox's name and filters. Owners and admins; "All" is `409 all_inbox`.
#[utoipa::path(put, path = "/inboxes/{id}", tag = "inbox", request_body = InboxInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Inbox), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn update(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<InboxInput>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let saved = async {
        let Some(found) = find_inbox(&app.db, t.team.id, id).await? else {
            return Ok(Err(not_found()));
        };
        if found.is_all {
            return Ok(Err(reply(StatusCode::CONFLICT, "all_inbox")));
        }
        let (name, filters) = match checked(&app.db, t.team.id, input).await? {
            Ok(v) => v,
            Err(res) => return Ok(Err(res)),
        };
        let row = inboxes::ActiveModel {
            id: Set(id),
            name: Set(name),
            filters: Set(serde_json::json!(filters)),
            updated_at: Set(Utc::now().fixed_offset()),
            ..Default::default()
        };
        let saved = inboxes::Entity::update(row).exec(&app.db).await?;
        Ok::<_, DbErr>(Ok(inbox_json(saved)))
    };
    match saved.await {
        Ok(Ok(i)) => Json(i).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "inbox update"),
    }
}

/// Deletes an inbox (its items stay, in the other inboxes). Owners and admins; "All" is
/// `409 not_deletable`.
#[utoipa::path(delete, path = "/inboxes/{id}", tag = "inbox", params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error), (status = 409, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let found = match find_inbox(&app.db, t.team.id, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "inbox read"),
    };
    if found.is_all {
        return reply(StatusCode::CONFLICT, "not_deletable");
    }
    match inboxes::Entity::delete_by_id(id).exec(&app.db).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e, "inbox delete"),
    }
}

/// The SQL for one page of an inbox's items, newest first; fetches `limit + 1` rows to tell
/// whether another page follows.
pub fn items_sql(
    team_id: Uuid,
    filters: &Filters,
    state: Option<&str>,
    cursor: Option<Uuid>,
    limit: u64,
) -> Statement {
    let mut values: Vec<Value> = vec![team_id.into()];
    let mut sql = String::from("SELECT i.* FROM inbox_items i WHERE i.team_id = $1");
    sql += &filter_sql(filters, &mut values);
    if let Some(state) = state {
        values.push(state.to_owned().into());
        sql += &format!(" AND i.state = ${}", values.len());
    }
    if let Some(c) = cursor {
        values.push(c.into());
        let n = values.len();
        sql += &format!(
            " AND (i.received_at, i.id) < \
             (SELECT c.received_at, c.id FROM inbox_items c WHERE c.id = ${n} AND c.team_id = $1)"
        );
    }
    values.push(((limit + 1) as i64).into());
    sql += &format!(
        " ORDER BY i.received_at DESC, i.id DESC LIMIT ${}",
        values.len()
    );
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// An inbox's items, newest first, a page at a time.
#[utoipa::path(get, path = "/inboxes/{id}/items", tag = "inbox",
    params(("id" = Uuid, Path), ItemQuery),
    responses((status = 200, body = ItemPage), (status = 401, body = Error),
        (status = 404, body = Error), (status = 422, body = Error)))]
async fn items(
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    q: Result<Query<ItemQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    let limit = q.limit.unwrap_or(PAGE);
    let state_ok = q
        .state
        .as_deref()
        .is_none_or(|s| s == "open" || s == "done");
    if !state_ok || !(1..=MAX_PAGE).contains(&limit) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let found = async {
        let Some(found) = find_inbox(&app.db, t.team.id, id).await? else {
            return Ok(None);
        };
        let filters: Filters = serde_json::from_value(found.filters).unwrap_or_default();
        let stmt = items_sql(t.team.id, &filters, q.state.as_deref(), q.cursor, limit);
        let mut rows = inbox_items::Entity::find()
            .from_raw_sql(stmt)
            .all(&app.db)
            .await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(limit as usize);
        let next_cursor = more.then(|| rows.last().map(|r| r.id)).flatten();
        let items = items_json(&app.db, rows).await?;
        Ok::<_, DbErr>(Some(ItemPage { items, next_cursor }))
    };
    match found.await {
        Ok(Some(page)) => Json(page).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "inbox items"),
    }
}

/// One item with the replies sent from SocialReins.
#[utoipa::path(get, path = "/inbox-items/{id}", tag = "inbox", params(("id" = Uuid, Path)),
    responses((status = 200, body = Item), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show_item(State(app): State<AppState>, t: InTeam, Path(id): Path<Uuid>) -> Response {
    let found = async {
        match find_item(&app.db, t.team.id, id).await? {
            Some(i) => item_json(&app.db, i).await.map(Some),
            None => Ok(None),
        }
    };
    match found.await {
        Ok(Some(i)) => Json(i).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "inbox item read"),
    }
}

/// Marks an item `done`, or `open` again. Every member.
#[utoipa::path(patch, path = "/inbox-items/{id}", tag = "inbox", request_body = StateInput,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Item), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn set_state(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<StateInput>,
) -> Response {
    if input.state != "open" && input.state != "done" {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_state");
    }
    let saved = async {
        let done = inbox_items::Entity::update_many()
            .col_expr(inbox_items::Column::State, input.state.clone().into())
            .filter(inbox_items::Column::Id.eq(id))
            .filter(inbox_items::Column::TeamId.eq(t.team.id))
            .exec(&app.db)
            .await?;
        if done.rows_affected == 0 {
            return Ok(None);
        }
        match find_item(&app.db, t.team.id, id).await? {
            Some(i) => item_json(&app.db, i).await.map(Some),
            None => Ok(None),
        }
    };
    match saved.await {
        Ok(Some(i)) => Json(i).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "inbox item state"),
    }
}

/// The reply for a send that did not happen: `429` when Bluesky rate limits, else `422`
/// (never 502: a proxy may swap a 502 body for its own page) with what to do in `message`.
fn send_error(e: &InboxError) -> Response {
    let status = match e {
        InboxError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    let body = serde_json::json!({ "error": e.code(), "message": e.message() });
    (status, Json(body)).into_response()
}

/// Replies to an item from its account, once (never retried): a public reply in the thread for
/// a comment or mention, a DM in the conversation for a DM. `201` with the item and its
/// replies. Every member; at most 10 a minute per user, shared with publishing. Failures:
/// `422` with `error` (`reconnect_required`, `no_dm_access`, `dm_not_allowed`, `too_long`,
/// `not_repliable`, `bluesky_refused`, `bluesky_unreachable`) and a `message`, or `429`.
#[utoipa::path(post, path = "/inbox-items/{id}/reply", tag = "inbox", request_body = ReplyInput,
    params(("id" = Uuid, Path)),
    responses((status = 201, body = Item), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error),
        (status = 429, body = Error)))]
async fn send_reply(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<ReplyInput>,
) -> Response {
    let item = match find_item(&app.db, t.team.id, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "inbox item read"),
    };
    let via = if item.kind == "dm" {
        Via::Dm
    } else {
        Via::Public
    };
    let text = input.text.trim();
    if !inbox::fits(via, text) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_text");
    }
    if !app.post_limiter.hit(t.current.0.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let account = match social_accounts::Entity::find_by_id(item.social_account_id)
        .one(&app.db)
        .await
    {
        Ok(Some(a)) => a,
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "account read"),
    };
    let user = Some(t.current.0.id);
    match inbox::send(&app, &account, &item, via, text, user, None).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return send_error(&e),
        Err(e) => return internal(e, "inbox reply"),
    }
    match find_item(&app.db, t.team.id, id).await {
        Ok(Some(i)) => match item_json(&app.db, i).await {
            Ok(i) => (StatusCode::CREATED, Json(i)).into_response(),
            Err(e) => internal(e, "inbox item read"),
        },
        Ok(None) => not_found(),
        Err(e) => internal(e, "inbox item read"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(update, destroy))
        .routes(routes!(items))
        .routes(routes!(show_item, set_state))
        .routes(routes!(send_reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_pages_filter_then_page() {
        let team = Uuid::now_v7();
        let f = Filters {
            kinds: vec!["dm".into()],
            ..Filters::default()
        };
        let stmt = items_sql(team, &f, Some("open"), Some(Uuid::nil()), 50);
        assert_eq!(
            stmt.sql,
            "SELECT i.* FROM inbox_items i WHERE i.team_id = $1 AND i.kind IN ($2) \
             AND i.state = $3 AND (i.received_at, i.id) < (SELECT c.received_at, c.id \
             FROM inbox_items c WHERE c.id = $4 AND c.team_id = $1) \
             ORDER BY i.received_at DESC, i.id DESC LIMIT $5"
        );
        let values = stmt.values.unwrap().0;
        assert_eq!(values.len(), 5);
        assert_eq!(values[4], Value::from(51i64));
    }
}
