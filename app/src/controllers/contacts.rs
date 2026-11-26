//! Contacts of the current team: the people who wrote to its accounts, one per platform and
//! author id, made and kept up by the inbox (`inbox::store`). Every member reads them and writes
//! their notes and tags, as with inbox items; owners and admins delete one. `opted_out` keeps a
//! contact out of broadcasts (`crate::broadcasts`). Like the inbox,
//! these routes take a session only.
use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, FixedOffset, SecondsFormat};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter, QueryOrder,
    QueryResult, QuerySelect, Statement, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::numbered::{ContactId, ID_PARAM};
use super::{
    auth::{internal, Body, FromWeb},
    inbox::{items_json, tags_of, Item},
    teams::{present, InTeam},
    waitlist::{error, Error},
};
use crate::{
    inbox::{self, MAX_TAGS},
    models::inbox_items,
    state::AppState,
};

const PAGE: u64 = 50;
const MAX_PAGE: u64 = 100;
const MAX_NOTES: usize = 2000;
/// Items in a contact's `recent`.
const RECENT: u64 = 20;

#[derive(Serialize, ToSchema)]
pub struct Contact {
    pub id: Uuid,
    /// The contact's number, unique over every team: the address web pages use. Every `{id}` of a
    /// contact route takes it as well as `id`.
    pub number: i64,
    pub platform: String,
    /// The platform's id of the person, e.g. a Bluesky DID.
    pub external_id: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// By name.
    pub tags: Vec<String>,
    pub notes: Option<String>,
    /// Never sent a broadcast: they wrote `stop` or `unsubscribe` (`start` clears it), or a
    /// member set it. Automations do not look at it. It is kept for the person, so it outlives
    /// a deleted contact.
    pub opted_out: bool,
    /// When their oldest and newest stored inbox items came.
    pub first_seen_at: DateTime<FixedOffset>,
    pub last_seen_at: DateTime<FixedOffset>,
    /// Their inbox items, of every kind.
    pub items_count: i64,
}

#[derive(Serialize, ToSchema)]
pub struct ContactDetail {
    #[serde(flatten)]
    pub contact: Contact,
    /// Their 20 latest inbox items, newest first.
    pub recent: Vec<Item>,
}

#[derive(Serialize, ToSchema)]
pub struct ContactPage {
    /// Newest `last_seen_at` first.
    pub items: Vec<Contact>,
    /// Pass as `cursor` for the next page, as it is; `null` on the last.
    pub next_cursor: Option<String>,
}

#[derive(Deserialize, IntoParams)]
pub struct ContactQuery {
    /// Case-insensitive search in the handle and display name, 1 to 100 characters after
    /// trimming.
    pub q: Option<String>,
    /// Only contacts with this tag.
    pub tag: Option<String>,
    /// e.g. `bluesky`.
    pub platform: Option<String>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
    /// 1 to 100, default 50.
    pub limit: Option<u64>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContactChange {
    /// At most 2000 characters; `null` or blank clears them. Absent: unchanged.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub notes: Option<Option<String>>,
    /// Replaces the set: at most 20, each 1 to 30 characters after trimming and lowercasing,
    /// no commas or control characters. Absent: unchanged.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Keeps the contact out of every broadcast. Every member sets it; only owners and admins
    /// clear it. Absent: unchanged.
    #[serde(default)]
    pub opted_out: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct TagCount {
    pub tag: String,
    /// Contacts of the team that have it.
    pub count: i64,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

/// `422 {"error": "invalid_contact", "field": field}`.
fn invalid(field: &str) -> Response {
    let body = json!({"error": "invalid_contact", "field": field});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

fn stmt(sql: String, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

const SELECT: &str = "SELECT c.*, EXISTS (SELECT 1 FROM contact_opt_outs o \
    WHERE o.team_id = c.team_id AND o.platform = c.platform \
    AND o.external_id = c.external_id) AS opted_out, (SELECT count(*) FROM inbox_items i \
    WHERE i.contact_id = c.id) AS items_count FROM contacts c WHERE c.team_id = $1";

/// The rows of `SELECT` as contacts, with their tags.
async fn contacts_json(
    db: &impl ConnectionTrait,
    rows: Vec<QueryResult>,
) -> Result<Vec<Contact>, DbErr> {
    let ids = rows
        .iter()
        .map(|r| r.try_get::<Uuid>("", "id"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut tags = tags_of(db, ids.iter().copied()).await?;
    rows.iter()
        .zip(ids)
        .map(|(r, id)| {
            Ok(Contact {
                id,
                number: r.try_get("", "number")?,
                platform: r.try_get("", "platform")?,
                external_id: r.try_get("", "external_id")?,
                handle: r.try_get("", "handle")?,
                display_name: r.try_get("", "display_name")?,
                avatar_url: r.try_get("", "avatar_url")?,
                tags: tags.remove(&id).unwrap_or_default(),
                notes: r.try_get("", "notes")?,
                opted_out: r.try_get("", "opted_out")?,
                first_seen_at: r.try_get("", "first_seen_at")?,
                last_seen_at: r.try_get("", "last_seen_at")?,
                items_count: r.try_get("", "items_count")?,
            })
        })
        .collect()
}

async fn find(db: &impl ConnectionTrait, team: Uuid, id: Uuid) -> Result<Option<Contact>, DbErr> {
    let sql = format!("{SELECT} AND c.id = $2");
    let rows = db
        .query_all_raw(stmt(sql, vec![team.into(), id.into()]))
        .await?;
    Ok(contacts_json(db, rows).await?.pop())
}

/// Where a page ended: a contact's `last_seen_at` as it was then, and its id. The cursor holds
/// both, so a contact seen again between two pages moves no page boundary.
type Seen = (DateTime<FixedOffset>, Uuid);

fn cursor_of(c: &Contact) -> String {
    let at = c.last_seen_at.to_rfc3339_opts(SecondsFormat::Micros, true);
    URL_SAFE_NO_PAD.encode(format!("{at}|{}", c.id))
}

fn seen_of(cursor: &str) -> Option<Seen> {
    let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
    let (at, id) = raw.split_once('|')?;
    Some((DateTime::parse_from_rfc3339(at).ok()?, id.parse().ok()?))
}

/// What narrows a page of contacts.
#[derive(Default)]
pub struct Narrow<'a> {
    pub q: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub platform: Option<&'a str>,
    /// The `last_seen_at` and id of the last contact of the page before.
    pub cursor: Option<Seen>,
}

/// The SQL for one page of a team's contacts, newest `last_seen_at` first; fetches `limit + 1`
/// rows to tell whether another page follows.
pub fn contacts_sql(team: Uuid, n: &Narrow<'_>, limit: u64) -> Statement {
    let mut values: Vec<Value> = vec![team.into()];
    let mut sql = String::from(SELECT);
    if let Some(q) = n.q {
        values.push(format!("%{}%", inbox::escape_like(q)).into());
        let p = values.len();
        sql += &format!(" AND (c.handle ILIKE ${p} OR c.display_name ILIKE ${p})");
    }
    if let Some(tag) = n.tag {
        values.push(tag.to_owned().into());
        sql += &format!(
            " AND EXISTS (SELECT 1 FROM contact_tags t WHERE t.contact_id = c.id \
             AND t.tag = ${})",
            values.len()
        );
    }
    if let Some(platform) = n.platform {
        values.push(platform.to_owned().into());
        sql += &format!(" AND c.platform = ${}", values.len());
    }
    if let Some((seen, id)) = n.cursor {
        values.extend([seen.into(), id.into()]);
        let p = values.len();
        sql += &format!(" AND (c.last_seen_at, c.id) < (${}, ${p})", p - 1);
    }
    values.push(((limit + 1) as i64).into());
    sql += &format!(
        " ORDER BY c.last_seen_at DESC, c.id DESC LIMIT ${}",
        values.len()
    );
    stmt(sql, values)
}

/// The team's contacts, newest `last_seen_at` first, a page at a time.
#[utoipa::path(get, path = "/contacts", tag = "contacts", params(ContactQuery),
    responses((status = 200, body = ContactPage), (status = 401, body = Error),
        (status = 422, body = Error)))]
async fn list(
    State(app): State<AppState>,
    t: InTeam,
    q: Result<Query<ContactQuery>, QueryRejection>,
) -> Response {
    let bad = || reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    let Ok(Query(q)) = q else {
        return bad();
    };
    let limit = q.limit.unwrap_or(PAGE);
    let search = q.q.as_deref().map(str::trim);
    let search_ok = search.is_none_or(|s| (1..=100).contains(&s.chars().count()));
    let tag = q.tag.as_deref().map(inbox::tag);
    let tag_ok = tag.as_ref().is_none_or(Option::is_some);
    let platform_ok = q
        .platform
        .as_deref()
        .is_none_or(|p| inbox::PLATFORMS.contains(&p));
    let cursor = q.cursor.as_deref().map(seen_of);
    let cursor_ok = cursor.is_none_or(|c| c.is_some());
    if !search_ok || !tag_ok || !platform_ok || !cursor_ok || !(1..=MAX_PAGE).contains(&limit) {
        return bad();
    }
    let tag = tag.flatten();
    let narrow = Narrow {
        q: search,
        tag: tag.as_deref(),
        platform: q.platform.as_deref(),
        cursor: cursor.flatten(),
    };
    let found = async {
        let mut rows = app
            .db
            .query_all_raw(contacts_sql(t.team.id, &narrow, limit))
            .await?;
        let more = rows.len() as u64 > limit;
        rows.truncate(limit as usize);
        let items = contacts_json(&app.db, rows).await?;
        let next_cursor = more.then(|| items.last().map(cursor_of)).flatten();
        Ok::<_, DbErr>(ContactPage { items, next_cursor })
    };
    match found.await {
        Ok(page) => Json(page).into_response(),
        Err(e) => internal(e, "contact list"),
    }
}

/// One contact with its 20 latest inbox items.
#[utoipa::path(get, path = "/contacts/{id}", tag = "contacts", params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = ContactDetail), (status = 401, body = Error),
        (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam, ContactId(id): ContactId) -> Response {
    let found = async {
        let Some(contact) = find(&app.db, t.team.id, id).await? else {
            return Ok(None);
        };
        let rows = inbox_items::Entity::find()
            .filter(inbox_items::Column::TeamId.eq(t.team.id))
            .filter(inbox_items::Column::ContactId.eq(id))
            .order_by_desc(inbox_items::Column::ReceivedAt)
            .order_by_desc(inbox_items::Column::Id)
            .limit(RECENT)
            .all(&app.db)
            .await?;
        let recent = items_json(&app.db, rows, app.meta.human_agent).await?;
        Ok::<_, DbErr>(Some(ContactDetail { contact, recent }))
    };
    match found.await {
        Ok(Some(c)) => Json(c).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "contact read"),
    }
}

/// The tags of a change, checked: trimmed, lowercased, each once, at most 20.
fn checked_tags(raw: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let t = inbox::tag(t)?;
        if !out.contains(&t) {
            out.push(t);
        }
    }
    (out.len() <= MAX_TAGS).then_some(out)
}

/// Sets a contact's notes, replaces its tags, opts it out of broadcasts, or any of them. Every
/// member, but only owners and admins opt a contact back in (`opted_out: false`, else `403
/// forbidden`). `422 invalid_contact` names the bad `field` (`notes` or `tags`).
#[utoipa::path(patch, path = "/contacts/{id}", tag = "contacts", request_body = ContactChange,
    params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 200, body = Contact), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn change(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    ContactId(id): ContactId,
    Body(input): Body<ContactChange>,
) -> Response {
    // Opting someone back in widens who a broadcast reaches.
    if input.opted_out == Some(false) && !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let notes = input.notes.map(|n| n.filter(|n| !n.trim().is_empty()));
    if matches!(&notes, Some(Some(n)) if n.chars().count() > MAX_NOTES) {
        return invalid("notes");
    }
    let tags = match input.tags.as_deref().map(checked_tags) {
        Some(None) => return invalid("tags"),
        Some(Some(tags)) => Some(tags),
        None => None,
    };
    let saved = async {
        let txn = app.db.begin().await?;
        // The row lock `inbox::add_tag` takes too, so the set stays within its cap.
        let lock = "SELECT 1 FROM contacts WHERE id = $1 AND team_id = $2 FOR UPDATE";
        let key: Vec<Value> = vec![id.into(), t.team.id.into()];
        if txn.query_one_raw(stmt(lock.into(), key)).await?.is_none() {
            return Ok(None);
        }
        if let Some(notes) = notes {
            let sql = "UPDATE contacts SET notes = $2 WHERE id = $1";
            txn.execute_raw(stmt(sql.into(), vec![id.into(), notes.into()]))
                .await?;
        }
        if let Some(out) = input.opted_out {
            let sql = if out {
                "INSERT INTO contact_opt_outs (team_id, platform, external_id) \
                 SELECT team_id, platform, external_id FROM contacts WHERE id = $1 \
                 ON CONFLICT DO NOTHING"
            } else {
                "DELETE FROM contact_opt_outs o USING contacts c WHERE c.id = $1 \
                 AND o.team_id = c.team_id AND o.platform = c.platform \
                 AND o.external_id = c.external_id"
            };
            txn.execute_raw(stmt(sql.into(), vec![id.into()])).await?;
        }
        if let Some(tags) = tags {
            let sql = "DELETE FROM contact_tags WHERE contact_id = $1";
            txn.execute_raw(stmt(sql.into(), vec![id.into()])).await?;
            for tag in tags {
                let sql = "INSERT INTO contact_tags (contact_id, tag) VALUES ($1, $2)";
                txn.execute_raw(stmt(sql.into(), vec![id.into(), tag.into()]))
                    .await?;
            }
        }
        txn.commit().await?;
        find(&app.db, t.team.id, id).await
    };
    match saved.await {
        Ok(Some(c)) => Json(c).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "contact change"),
    }
}

/// Deletes a contact with its notes and tags. Owners and admins. An opt-out of broadcasts is
/// kept (the platform and the person's id on it), so a later contact of theirs is opted out. Its inbox items stay, without
/// a contact; a later item of the same person makes a new one.
#[utoipa::path(delete, path = "/contacts/{id}", tag = "contacts", params(("id" = String, Path, description = ID_PARAM)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    ContactId(id): ContactId,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let sql = "DELETE FROM contacts WHERE id = $1 AND team_id = $2";
    let key: Vec<Value> = vec![id.into(), t.team.id.into()];
    match app.db.execute_raw(stmt(sql.into(), key)).await {
        Ok(r) if r.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "contact delete"),
    }
}

/// The tags the team's contacts have, the most used first, then by name.
#[utoipa::path(get, path = "/contact-tags", tag = "contacts",
    responses((status = 200, body = [TagCount]), (status = 401, body = Error)))]
async fn tags(State(app): State<AppState>, t: InTeam) -> Response {
    let sql = "SELECT t.tag, count(*) AS count FROM contact_tags t \
               JOIN contacts c ON c.id = t.contact_id WHERE c.team_id = $1 \
               GROUP BY t.tag ORDER BY count DESC, t.tag";
    let found = async {
        let rows = app
            .db
            .query_all_raw(stmt(sql.into(), vec![t.team.id.into()]))
            .await?;
        rows.iter()
            .map(|r| {
                Ok(TagCount {
                    tag: r.try_get("", "tag")?,
                    count: r.try_get("", "count")?,
                })
            })
            .collect::<Result<Vec<_>, DbErr>>()
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "contact tags"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(show, change, destroy))
        .routes(routes!(tags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_pages_filter_then_page() {
        let n = Narrow {
            q: Some("50%_off"),
            tag: Some("vip"),
            platform: Some("bluesky"),
            cursor: seen_of(
                &URL_SAFE_NO_PAD.encode(format!("2026-01-02T03:04:05.000006Z|{}", Uuid::nil())),
            ),
        };
        assert!(n.cursor.is_some());
        assert_eq!(seen_of("x"), None);
        let got = contacts_sql(Uuid::nil(), &n, 50);
        assert!(got.sql.ends_with(
            " WHERE c.team_id = $1 AND (c.handle ILIKE $2 OR c.display_name ILIKE $2) \
             AND EXISTS (SELECT 1 FROM contact_tags t WHERE t.contact_id = c.id AND t.tag = $3) \
             AND c.platform = $4 AND (c.last_seen_at, c.id) < ($5, $6) \
             ORDER BY c.last_seen_at DESC, c.id DESC LIMIT $7"
        ));
        let values = got.values.unwrap().0;
        assert_eq!(values[1], Value::from("%50\\%\\_off%".to_owned()));
        assert_eq!(values[6], Value::from(51i64));
    }

    #[test]
    fn tag_sets_are_checked() {
        let raw = |t: &[&str]| t.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            checked_tags(&raw(&[" VIP ", "vip", "lead"])),
            Some(raw(&["vip", "lead"]))
        );
        assert_eq!(checked_tags(&raw(&["a,b"])), None);
        let many: Vec<String> = (0..21).map(|n| format!("t{n}")).collect();
        assert_eq!(checked_tags(&many), None);
        assert_eq!(checked_tags(&many[..20]).map(|t| t.len()), Some(20));
    }
}
