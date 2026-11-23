//! Inboxes and their items, for the current team. Members read inboxes, reply to items,
//! mark them done and assign them; owners and admins create, change and delete inboxes, as with sets. Every
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
    sea_query::Expr, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait,
    FromQueryResult, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Statement,
    TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use socialreins_core::team;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    billing::plan_limit,
    teams::{present, InTeam},
    waitlist::{error, Error},
};
use crate::{
    billing::{self, Limit},
    connectors::bluesky::{self, InboxError},
    inbox::{self, filter_sql, Filters, Via},
    models::{inbox_items, inbox_replies, inboxes, social_accounts, team_user, users},
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
    /// Its `open` items (follows left out); only in the list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_count: Option<i64>,
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

/// The member an item is assigned to.
#[derive(Serialize, ToSchema)]
pub struct Assignee {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize, ToSchema)]
pub struct Item {
    pub id: Uuid,
    pub social_account_id: Uuid,
    pub platform: String,
    /// `comment` (a reply to the account), `mention` (a mention or quote), `dm` or `follow` (a
    /// new Bluesky follower, stored `done`; listed only with `kind=follow`).
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
    /// A DM replying to one of the account's stories (Instagram).
    pub story_reply: bool,
    pub story_id: Option<String>,
    /// The story's media link, as Meta gave it (it expires).
    pub story_url: Option<String>,
    /// A DM that is a click on a quick reply button (Facebook, Instagram): its payload.
    pub button_payload: Option<String>,
    /// The author's contact (`GET /contacts/{id}`).
    pub contact_id: Option<Uuid>,
    /// The contact's tags, by name.
    pub contact_tags: Vec<String>,
    /// The member it is assigned to; `null` for nobody.
    pub assigned_to: Option<Assignee>,
    /// Only with `group=conversations`: the DM conversation this item is the latest of; `null`
    /// for a comment or mention.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Conversation>)]
    pub conversation: Option<Option<Conversation>>,
}

/// A DM conversation: every `dm` item of one account and contact that the inbox holds.
#[derive(Serialize, ToSchema)]
pub struct Conversation {
    pub count: i64,
    /// How many of them are `open`.
    pub open_count: i64,
    pub first_at: DateTime<FixedOffset>,
    pub last_at: DateTime<FixedOffset>,
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
    /// `comment`, `mention`, `dm` or `follow`, within the inbox's own filters; every kind but
    /// `follow` when absent.
    pub kind: Option<String>,
    /// Case-insensitive search in the text and the author's name and handle, 1 to 100
    /// characters after trimming.
    pub q: Option<String>,
    /// Only items whose author's contact has this tag.
    pub tag: Option<String>,
    /// Only items assigned to `me`, to nobody (`none`) or to the member with this user id.
    pub assigned: Option<String>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<Uuid>,
    /// 1 to 100, default 50.
    pub limit: Option<u64>,
    /// `conversations`: the DMs of one account and contact become one row, the latest, with a
    /// `conversation` object. `state=open` keeps a conversation with any open item, `done` one
    /// with none; `q` and `tag` match any of its items, `assigned` its latest.
    pub group: Option<String>,
}

#[derive(Deserialize, IntoParams)]
pub struct ConversationQuery {
    /// 1 to 100, default 50.
    pub limit: Option<u64>,
    /// The id of the oldest item of the previous page, for the page before it.
    pub before: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct ConversationPage {
    /// Oldest first, each with its `replies`.
    pub items: Vec<Item>,
    /// Older items are left: ask again with `before` set to the first item's id.
    pub has_more: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyInput {
    /// A public reply for comments and mentions (at most 300 graphemes), a DM in the
    /// conversation for DMs (at most 1000).
    pub text: String,
    /// For a DM: once the reply went out, every open item of its conversation is marked `done`.
    #[serde(default)]
    pub conversation: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemChange {
    /// `open` or `done`. Absent: unchanged.
    #[serde(default)]
    pub state: Option<String>,
    /// The user id of a member of the team, or `null` for nobody. Absent: unchanged.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Uuid>)]
    pub assigned_to: Option<Option<Uuid>>,
    /// For a DM: the change reaches every DM item of its conversation. Ignored for other kinds.
    #[serde(default)]
    pub conversation: bool,
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
        open_count: None,
    }
}

fn url_of(remote_id: &str) -> Option<String> {
    bluesky::post_url(remote_id)
}

/// Each contact's tags, by name.
pub(super) async fn tags_of(
    db: &impl ConnectionTrait,
    contacts: impl IntoIterator<Item = Uuid>,
) -> Result<HashMap<Uuid, Vec<String>>, DbErr> {
    let mut values = Vec::new();
    let p = inbox::placeholders(&mut values, contacts.into_iter().map(Into::into));
    let mut tags: HashMap<Uuid, Vec<String>> = HashMap::new();
    if values.is_empty() {
        return Ok(tags);
    }
    let sql =
        format!("SELECT contact_id, tag FROM contact_tags WHERE contact_id IN ({p}) ORDER BY tag");
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, values);
    for r in db.query_all_raw(stmt).await? {
        let (id, tag) = (r.try_get("", "contact_id")?, r.try_get("", "tag")?);
        tags.entry(id).or_default().push(tag);
    }
    Ok(tags)
}

pub(super) async fn items_json(
    db: &impl ConnectionTrait,
    rows: Vec<inbox_items::Model>,
) -> Result<Vec<Item>, DbErr> {
    let tags = tags_of(db, rows.iter().filter_map(|i| i.contact_id)).await?;
    let names: HashMap<Uuid, String> = users::Entity::find()
        .filter(users::Column::Id.is_in(rows.iter().filter_map(|i| i.assigned_to)))
        .all(db)
        .await?
        .into_iter()
        .map(|u| (u.id, u.name))
        .collect();
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
            remote_url: i.permalink.or_else(|| url_of(&i.remote_id)),
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
            story_reply: i.story_reply,
            story_id: i.story_id,
            story_url: i.story_url,
            button_payload: i.button_payload,
            contact_tags: i
                .contact_id
                .and_then(|c| tags.get(&c).cloned())
                .unwrap_or_default(),
            contact_id: i.contact_id,
            assigned_to: i.assigned_to.and_then(|id| {
                let name = names.get(&id)?.clone();
                Some(Assignee { id, name })
            }),
            conversation: None,
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

/// The items of `item`'s conversation: every DM of its account and contact, or `item` alone when
/// it is no DM or has no contact.
fn conversation_of(item: &inbox_items::Model) -> sea_orm::Condition {
    let own = sea_orm::Condition::all().add(inbox_items::Column::TeamId.eq(item.team_id));
    match item.contact_id {
        Some(contact) if item.kind == "dm" => own
            .add(inbox_items::Column::Kind.eq("dm"))
            .add(inbox_items::Column::SocialAccountId.eq(item.social_account_id))
            .add(inbox_items::Column::ContactId.eq(contact)),
        _ => own.add(inbox_items::Column::Id.eq(item.id)),
    }
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

/// Each inbox's `open_count`, in one query that reads the team's open items once.
pub fn open_counts_sql(team_id: Uuid, inboxes: &[(Uuid, Filters)]) -> Statement {
    let mut values: Vec<Value> = vec![team_id.into()];
    let counts: Vec<String> = inboxes
        .iter()
        .enumerate()
        .map(|(n, (_, f))| {
            format!(
                "COUNT(*) FILTER (WHERE TRUE{}) AS c{n}",
                filter_sql(f, &mut values)
            )
        })
        .collect();
    let sql = format!(
        "SELECT {} FROM inbox_items i WHERE i.team_id = $1 AND i.state = 'open' \
         AND i.kind <> 'follow'",
        if counts.is_empty() {
            "1".into()
        } else {
            counts.join(", ")
        }
    );
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The team's inboxes, "All" first, then oldest first, each with its `open_count`. Makes "All"
/// when it is missing.
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
        let rows = inboxes::Entity::find()
            .filter(inboxes::Column::TeamId.eq(t.team.id))
            .order_by_desc(inboxes::Column::IsAll)
            .order_by_asc(inboxes::Column::CreatedAt)
            .order_by_asc(inboxes::Column::Id)
            .all(&app.db)
            .await?;
        let list: Vec<Inbox> = rows.into_iter().map(inbox_json).collect();
        let filtered: Vec<(Uuid, Filters)> =
            list.iter().map(|i| (i.id, i.filters.clone())).collect();
        let counts = app
            .db
            .query_one_raw(open_counts_sql(t.team.id, &filtered))
            .await?;
        list.into_iter()
            .enumerate()
            .map(|(n, i)| {
                let open = match &counts {
                    Some(r) => r.try_get("", &format!("c{n}"))?,
                    None => 0,
                };
                Ok(Inbox {
                    open_count: Some(open),
                    ..i
                })
            })
            .collect::<Result<Vec<_>, DbErr>>()
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "inbox list"),
    }
}

/// Creates an inbox. Owners and admins; at most 50 per team ("All" included), and with billing on
/// at most the plan's `inboxes` besides "All" (`402 plan_limit`). Account and set ids must be the
/// team's.
#[utoipa::path(post, path = "/inboxes", tag = "inbox", request_body = InboxInput,
    responses((status = 201, body = Inbox), (status = 401, body = Error),
        (status = 402, body = Error), (status = 403, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
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
        // The team row lock keeps concurrent creates under the caps.
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 FROM teams WHERE id = $1 FOR NO KEY UPDATE",
            [t.team.id.into()],
        ))
        .await?;
        let all = inboxes::Entity::find().filter(inboxes::Column::TeamId.eq(t.team.id));
        let custom = all.clone().filter(inboxes::Column::IsAll.eq(false));
        let limits = billing::limits_of(app.billing_enabled, &txn, t.team.id).await?;
        if let Some(max) = limits.inboxes {
            if custom.count(&txn).await? as i64 >= max {
                return Ok(Err(plan_limit(Limit::Inboxes)));
            }
        }
        if all.count(&txn).await? >= MAX_INBOXES {
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

/// What narrows one page of items beyond the inbox's filters.
#[derive(Default)]
pub struct Narrow<'a> {
    pub state: Option<&'a str>,
    pub kind: Option<&'a str>,
    /// Searched in the text and the author's name and handle.
    pub q: Option<&'a str>,
    /// A tag of the author's contact.
    pub tag: Option<&'a str>,
    /// Assigned to this member, or (`Some(None)`) to nobody.
    pub assigned: Option<Option<Uuid>>,
    pub cursor: Option<Uuid>,
}

/// The SQL for one page of an inbox's items, newest first; fetches `limit + 1` rows to tell
/// whether another page follows. Follows only when `kind` asks for them.
///
/// `grouped`: one row per DM conversation (the `dm` items of one account and contact), its
/// latest item, with `conv_count`, `conv_open` and `conv_first` over the items in the inbox's
/// scope; every other item is a group of its own. `state`, `q` and `assigned` then narrow the
/// groups, not the items.
pub fn items_sql(
    team_id: Uuid,
    filters: &Filters,
    n: &Narrow<'_>,
    limit: u64,
    grouped: bool,
) -> Statement {
    let Narrow {
        state,
        kind,
        q,
        tag,
        assigned,
        cursor,
    } = *n;
    let mut values: Vec<Value> = vec![team_id.into()];
    // What an item must be to be in scope, then what narrows the rows that are listed.
    let mut scope = filter_sql(filters, &mut values);
    let mut rest = String::new();
    let mut hit = String::from("TRUE");
    if let Some(kind) = kind {
        values.push(kind.to_owned().into());
        scope += &format!(" AND i.kind = ${}", values.len());
    } else {
        scope += " AND i.kind <> 'follow'";
    }
    if let Some(tag) = tag {
        values.push(tag.to_owned().into());
        scope += &format!(
            " AND EXISTS (SELECT 1 FROM contact_tags ct WHERE ct.contact_id = i.contact_id \
             AND ct.tag = ${})",
            values.len()
        );
    }
    match state {
        Some("open") if grouped => rest += " AND i.conv_open > 0",
        Some(_) if grouped => rest += " AND i.conv_open = 0",
        Some(state) => {
            values.push(state.to_owned().into());
            rest += &format!(" AND i.state = ${}", values.len());
        }
        None => {}
    }
    if let Some(q) = q {
        values.push(format!("%{}%", inbox::escape_like(q)).into());
        let p = values.len();
        hit = format!(
            "(i.text ILIKE ${p} OR i.author_name ILIKE ${p} OR i.author_handle ILIKE ${p})"
        );
        rest += &if grouped {
            " AND i.conv_hit".to_owned()
        } else {
            format!(" AND {hit}")
        };
    }
    match assigned {
        Some(Some(user)) => {
            values.push(user.into());
            rest += &format!(" AND i.assigned_to = ${}", values.len());
        }
        Some(None) => rest += " AND i.assigned_to IS NULL",
        None => {}
    }
    if let Some(c) = cursor {
        values.push(c.into());
        let n = values.len();
        rest += &format!(
            " AND (i.received_at, i.id) < \
             (SELECT c.received_at, c.id FROM inbox_items c WHERE c.id = ${n} AND c.team_id = $1)"
        );
    }
    values.push(((limit + 1) as i64).into());
    let from = if grouped {
        // shortcut: the windows read every item in the inbox's scope for each page; keep a
        // conversations table once a team's inbox is too large for that.
        format!(
            "(SELECT i.*, count(*) OVER w AS conv_count, \
             count(*) FILTER (WHERE i.state = 'open') OVER w AS conv_open, \
             min(i.received_at) OVER w AS conv_first, bool_or({hit}) OVER w AS conv_hit, \
             row_number() OVER (w ORDER BY i.received_at DESC, i.id DESC) AS conv_rank \
             FROM inbox_items i WHERE i.team_id = $1{scope} \
             WINDOW w AS (PARTITION BY \
             CASE WHEN i.kind = 'dm' AND i.contact_id IS NOT NULL THEN i.social_account_id END, \
             CASE WHEN i.kind = 'dm' AND i.contact_id IS NOT NULL THEN i.contact_id ELSE i.id END)) \
             i WHERE i.conv_rank = 1"
        )
    } else {
        format!("inbox_items i WHERE i.team_id = $1{scope}")
    };
    let sql = format!(
        "SELECT i.* FROM {from}{rest} ORDER BY i.received_at DESC, i.id DESC LIMIT ${}",
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
    let kind_ok = q
        .kind
        .as_deref()
        .is_none_or(|k| matches!(k, "comment" | "mention" | "dm" | "follow"));
    let search = q.q.as_deref().map(str::trim);
    let search_ok = search.is_none_or(|s| (1..=100).contains(&s.chars().count()));
    let tag = q.tag.as_deref().map(inbox::tag);
    let assigned = q.assigned.as_deref().map(|a| match a {
        "me" => Some(Some(t.current.0.id)),
        "none" => Some(None),
        id => id.parse().ok().map(Some),
    });
    let narrow_ok =
        tag.as_ref().is_none_or(Option::is_some) && assigned.is_none_or(|a| a.is_some());
    if !state_ok || !kind_ok || !search_ok || !narrow_ok || !(1..=MAX_PAGE).contains(&limit) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let (tag, assigned) = (tag.flatten(), assigned.flatten());
    let grouped = match q.group.as_deref() {
        None => false,
        Some("conversations") => true,
        Some(_) => return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query"),
    };
    let found = async {
        let Some(found) = find_inbox(&app.db, t.team.id, id).await? else {
            return Ok(None);
        };
        let filters: Filters = serde_json::from_value(found.filters).unwrap_or_default();
        let narrow = Narrow {
            state: q.state.as_deref(),
            kind: q.kind.as_deref(),
            q: search,
            tag: tag.as_deref(),
            assigned,
            cursor: q.cursor,
        };
        let stmt = items_sql(t.team.id, &filters, &narrow, limit, grouped);
        let mut found = app.db.query_all_raw(stmt).await?;
        let more = found.len() as u64 > limit;
        found.truncate(limit as usize);
        let mut rows = Vec::new();
        let mut conversations = Vec::new();
        for r in &found {
            let row = inbox_items::Model::from_query_result(r, "")?;
            if grouped {
                conversations.push(if row.kind == "dm" {
                    Some(Conversation {
                        count: r.try_get("", "conv_count")?,
                        open_count: r.try_get("", "conv_open")?,
                        first_at: r.try_get("", "conv_first")?,
                        last_at: row.received_at,
                    })
                } else {
                    None
                });
            }
            rows.push(row);
        }
        let next_cursor = more.then(|| rows.last().map(|r| r.id)).flatten();
        let mut items = items_json(&app.db, rows).await?;
        for (item, c) in items.iter_mut().zip(conversations) {
            item.conversation = Some(c);
        }
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

/// A DM item's conversation: every DM item of the same account and contact, each with its
/// `replies`. The newest page first, oldest first within it; `before` (the first item's id)
/// pages back. `422 not_a_conversation` for an item that is no DM.
#[utoipa::path(get, path = "/inbox-items/{id}/conversation", tag = "inbox",
    params(("id" = Uuid, Path), ConversationQuery),
    responses((status = 200, body = ConversationPage), (status = 401, body = Error),
        (status = 404, body = Error), (status = 422, body = Error)))]
async fn conversation(
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    q: Result<Query<ConversationQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    let limit = q.limit.unwrap_or(PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let found = async {
        let Some(item) = find_item(&app.db, t.team.id, id).await? else {
            return Ok(Err(not_found()));
        };
        if item.kind != "dm" {
            let code = "not_a_conversation";
            return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, code)));
        }
        let mut page = inbox_items::Entity::find().filter(conversation_of(&item));
        if let Some(before) = q.before {
            page = page.filter(Expr::cust_with_values(
                "(received_at, id) < (SELECT c.received_at, c.id FROM inbox_items c \
                 WHERE c.id = $1 AND c.team_id = $2)",
                [before, t.team.id],
            ));
        }
        let mut rows = page
            .order_by_desc(inbox_items::Column::ReceivedAt)
            .order_by_desc(inbox_items::Column::Id)
            .limit(limit + 1)
            .all(&app.db)
            .await?;
        let has_more = rows.len() as u64 > limit;
        rows.truncate(limit as usize);
        rows.reverse();
        let items = items_json(&app.db, rows).await?;
        Ok::<_, DbErr>(Ok(ConversationPage { items, has_more }))
    };
    match found.await {
        Ok(Ok(page)) => Json(page).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "inbox conversation"),
    }
}

/// Marks an item `done` or `open` again (`state`), assigns it to a member of the team or to
/// nobody (`assigned_to`), or both; with `conversation: true` on a DM, every DM item of its
/// conversation in one statement. Every member. `422 invalid_state`, `invalid_assignee` (not
/// a member of the team), and `invalid_body` when neither field is sent.
#[utoipa::path(patch, path = "/inbox-items/{id}", tag = "inbox", request_body = ItemChange,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Item), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn change_item(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
    Body(input): Body<ItemChange>,
) -> Response {
    let unprocessable = |code| reply(StatusCode::UNPROCESSABLE_ENTITY, code);
    if input.state.is_none() && input.assigned_to.is_none() {
        return unprocessable("invalid_body");
    }
    if input
        .state
        .as_deref()
        .is_some_and(|s| s != "open" && s != "done")
    {
        return unprocessable("invalid_state");
    }
    let saved = async {
        let txn = app.db.begin().await?;
        // Held to the commit, so a removal of the member waits and then clears this too.
        if let Some(Some(user)) = input.assigned_to {
            let member = team_user::Entity::find_by_id((t.team.id, user))
                .lock_shared()
                .one(&txn)
                .await?;
            if member.is_none() {
                return Ok(Err(unprocessable("invalid_assignee")));
            }
        }
        let mut change = inbox_items::Entity::update_many();
        if input.conversation {
            let Some(item) = find_item(&txn, t.team.id, id).await? else {
                return Ok(Ok(None));
            };
            change = change.filter(conversation_of(&item));
        } else {
            change = change
                .filter(inbox_items::Column::Id.eq(id))
                .filter(inbox_items::Column::TeamId.eq(t.team.id));
        }
        if let Some(state) = input.state {
            change = change.col_expr(inbox_items::Column::State, state.into());
        }
        if let Some(user) = input.assigned_to {
            change = change.col_expr(inbox_items::Column::AssignedTo, Expr::value(user));
        }
        let done = change.exec(&txn).await?;
        txn.commit().await?;
        if done.rows_affected == 0 {
            return Ok(Ok(None));
        }
        Ok::<_, DbErr>(Ok(match find_item(&app.db, t.team.id, id).await? {
            Some(i) => Some(item_json(&app.db, i).await?),
            None => None,
        }))
    };
    match saved.await {
        Ok(Ok(Some(i))) => Json(i).into_response(),
        Ok(Ok(None)) => not_found(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "inbox item change"),
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
/// replies. The item's state is left alone, unless `conversation: true` on a DM: then every
/// open item of its conversation is `done` once the reply went out. Every member; at most 10 a minute per user, shared with publishing; `409
/// reply_in_progress` while another reply to the item is going out. Failures:
/// `422` with `error` (`reconnect_required`, `no_dm_access`, `dm_not_allowed`, `too_long`,
/// `not_repliable`, `bluesky_refused`, `bluesky_unreachable`; on Facebook and Instagram
/// `meta_reconnect_required`, `meta_failed`, `outside_window`, `dm_unavailable`) and a
/// `message`, or `429`.
#[utoipa::path(post, path = "/inbox-items/{id}/reply", tag = "inbox", request_body = ReplyInput,
    params(("id" = Uuid, Path)),
    responses((status = 201, body = Item), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
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
    if !inbox::fits_on(&item.platform, via, text) {
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
    // One reply per item at a time, so a double click cannot send twice: the lock lives in
    // this transaction, held across the send.
    let guard = async {
        let txn = app.db.begin().await?;
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 1)) AS ok",
            [item.id.to_string().into()],
        );
        let ok: bool = match txn.query_one_raw(stmt).await? {
            Some(r) => r.try_get("", "ok")?,
            None => false,
        };
        Ok::<_, DbErr>(ok.then_some(txn))
    };
    let guard = match guard.await {
        Ok(Some(txn)) => txn,
        Ok(None) => return reply(StatusCode::CONFLICT, "reply_in_progress"),
        Err(e) => return internal(e, "inbox reply lock"),
    };
    let sent = inbox::send(&app, &account, &item, via, text, &[], user, None).await;
    let _ = guard.commit().await;
    match sent {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return send_error(&e),
        Err(e) => return internal(e, "inbox reply"),
    }
    if input.conversation && item.kind == "dm" {
        let done = inbox_items::Entity::update_many()
            .filter(conversation_of(&item))
            .filter(inbox_items::Column::State.eq("open"))
            .col_expr(inbox_items::Column::State, "done".into())
            .exec(&app.db)
            .await;
        if let Err(e) = done {
            return internal(e, "inbox conversation done");
        }
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
        .routes(routes!(show_item, change_item))
        .routes(routes!(conversation))
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
        let n = Narrow {
            state: Some("open"),
            kind: Some("dm"),
            cursor: Some(Uuid::nil()),
            ..Narrow::default()
        };
        let stmt = items_sql(team, &f, &n, 50, false);
        assert_eq!(
            stmt.sql,
            "SELECT i.* FROM inbox_items i WHERE i.team_id = $1 AND i.kind IN ($2) \
             AND i.kind = $3 AND i.state = $4 AND (i.received_at, i.id) < (SELECT c.received_at, c.id \
             FROM inbox_items c WHERE c.id = $5 AND c.team_id = $1) \
             ORDER BY i.received_at DESC, i.id DESC LIMIT $6"
        );
        let values = stmt.values.unwrap().0;
        assert_eq!(values.len(), 6);
        assert_eq!(values[5], Value::from(51i64));
        // No kind: follows are left out. A search is one bound, escaped pattern.
        let n = Narrow {
            q: Some("50%_off"),
            ..Narrow::default()
        };
        let stmt = items_sql(team, &Filters::default(), &n, 10, false);
        assert_eq!(
            stmt.sql,
            "SELECT i.* FROM inbox_items i WHERE i.team_id = $1 AND i.kind <> 'follow' \
             AND (i.text ILIKE $2 OR i.author_name ILIKE $2 OR i.author_handle ILIKE $2) \
             ORDER BY i.received_at DESC, i.id DESC LIMIT $3"
        );
        assert_eq!(
            stmt.values.unwrap().0[1],
            Value::from("%50\\%\\_off%".to_owned())
        );
        // A contact tag and an assignee each narrow further.
        let n = Narrow {
            tag: Some("vip"),
            assigned: Some(None),
            ..Narrow::default()
        };
        let stmt = items_sql(team, &Filters::default(), &n, 10, false);
        assert!(stmt.sql.contains(
            " AND EXISTS (SELECT 1 FROM contact_tags ct WHERE ct.contact_id = i.contact_id \
             AND ct.tag = $2) AND i.assigned_to IS NULL ORDER BY"
        ));
        let n = Narrow {
            assigned: Some(Some(Uuid::nil())),
            ..Narrow::default()
        };
        let stmt = items_sql(team, &Filters::default(), &n, 10, false);
        assert!(stmt.sql.contains(" AND i.assigned_to = $2 ORDER BY"));
    }

    #[test]
    fn open_counts_are_one_query() {
        let f = Filters {
            kinds: vec!["dm".into()],
            ..Filters::default()
        };
        let stmt = open_counts_sql(
            Uuid::nil(),
            &[(Uuid::nil(), Filters::default()), (Uuid::nil(), f)],
        );
        assert_eq!(
            stmt.sql,
            "SELECT COUNT(*) FILTER (WHERE TRUE) AS c0, \
             COUNT(*) FILTER (WHERE TRUE AND i.kind IN ($2)) AS c1 \
             FROM inbox_items i WHERE i.team_id = $1 AND i.state = 'open' AND i.kind <> 'follow'"
        );
    }
}
