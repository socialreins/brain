//! Team assets for the composer: the media library (`/v1/media-library`, URLs kept for reuse) and
//! saved text snippets (`/v1/snippets`, captions and hashtag sets). Both belong to the team, not to
//! a set: any member reads and writes them, and so does a whole-team API token. A set-scoped
//! token sees nothing outside its set, so every route here is `403 team_token_required` for one
//! (`api_tokens::refuses_set_token`); a read-only token only reads (the `Actor` extractor and
//! `/mcp` refuse its writes). Each create runs under a per-team advisory lock, so the caps and
//! the duplicate checks hold under concurrent calls.
use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use schemars::JsonSchema;
use sea_orm::{
    sea_query::{Expr, Func},
    ActiveValue::Set,
    ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Statement, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use socialreins_core::{post::https_url, team::name_up_to};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::{refuses_set_token, Actor},
    auth::{internal, Body},
    idempotency::IDEMPOTENCY,
    waitlist::{error, like, Error},
};
use crate::{
    models::{media_library, snippets},
    state::AppState,
};

/// Most media items a team keeps.
pub const MAX_MEDIA: u64 = 1000;
/// Most snippets a team keeps, of all kinds.
pub const MAX_SNIPPETS: u64 = 500;
const MAX_MEDIA_NAME: usize = 100;
const MAX_SNIPPET_NAME: usize = 80;
const MAX_SNIPPET_BODY: usize = 5000;
/// Longest `q`, in characters: a whole URL fits.
const MAX_QUERY: usize = 2048;
const PAGE: u64 = 50;
const MAX_PAGE: u64 = 100;

#[derive(Serialize, ToSchema)]
pub struct MediaItem {
    pub id: Uuid,
    pub url: String,
    pub name: String,
    pub created_at: DateTime<FixedOffset>,
    /// The member who added it; `null` when an API token did, or the user is gone.
    pub created_by: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct MediaPage {
    /// Newest first.
    pub items: Vec<MediaItem>,
    /// Pass as `cursor` for the next page; `null` on the last.
    pub next_cursor: Option<Uuid>,
}

#[derive(Deserialize, IntoParams, JsonSchema)]
#[into_params(parameter_in = Query)]
pub struct MediaQuery {
    /// Case-insensitive search in the name and the URL; blank means none. At most 2048
    /// characters.
    pub q: Option<String>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<Uuid>,
    /// 1 to 100, default 50.
    pub limit: Option<u64>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct MediaInput {
    /// A public `https://` URL, at most 2048 bytes, with no user or password in it.
    #[serde(default)]
    pub url: String,
    /// Trimmed, at most 100 characters; absent or blank: the URL's last path segment, decoded
    /// (the host when the path is empty), cut to 100 characters.
    pub name: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct MediaName {
    /// Trimmed, 1 to 100 characters.
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize, ToSchema)]
pub struct Snippet {
    pub id: Uuid,
    /// `caption`, `hashtags` or `reply` (a saved inbox reply).
    pub kind: String,
    pub name: String,
    pub body: String,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct SnippetList {
    /// By name, ignoring case.
    pub items: Vec<Snippet>,
}

#[derive(Deserialize, IntoParams, JsonSchema)]
#[into_params(parameter_in = Query)]
pub struct SnippetQuery {
    /// `caption`, `hashtags` or `reply`; all when absent.
    pub kind: Option<String>,
}

#[derive(Deserialize, ToSchema, JsonSchema)]
pub struct SnippetInput {
    /// Required: `caption`, `hashtags` or `reply` (a saved inbox reply).
    #[serde(default)]
    pub kind: String,
    /// Required: trimmed, 1 to 80 characters, unique per team and kind ignoring case.
    #[serde(default)]
    pub name: String,
    /// Required: 1 to 5000 characters, not blank, kept as sent.
    #[serde(default)]
    pub body: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

/// `422 {"error": code, "field": field}`.
fn invalid(code: &str, field: &str) -> Response {
    let body = json!({"error": code, "field": field});
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

fn media_json(m: media_library::Model) -> MediaItem {
    MediaItem {
        id: m.id,
        url: m.url,
        name: m.name,
        created_at: m.created_at,
        created_by: m.created_by,
    }
}

fn snippet_json(s: snippets::Model) -> Snippet {
    Snippet {
        id: s.id,
        kind: s.kind,
        name: s.name,
        body: s.body,
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

/// Holds the team's `what` lock until `txn` ends.
pub(super) async fn lock(txn: &impl ConnectionTrait, what: &str, team: Uuid) -> Result<(), DbErr> {
    let sql = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";
    let key = format!("{what}:{team}");
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        [key.into()],
    ))
    .await
    .map(|_| ())
}

/// `%XX` escapes decoded; a bad escape stays as it is, invalid UTF-8 becomes U+FFFD.
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = s
            .get(i + 1..i + 3)
            .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()));
        match hex.filter(|_| b[i] == b'%') {
            Some(h) => {
                out.push(u8::from_str_radix(h, 16).unwrap_or_default());
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The default name of an `https://` URL: its last non-empty path segment, decoded, without
/// control characters, trimmed and cut to 100 characters; the host when that leaves nothing.
fn default_name(url: &str) -> String {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    let host = path.split('/').next().unwrap_or_default();
    let last = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(host);
    let name: String = percent_decode(last)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let name: String = name.trim().chars().take(MAX_MEDIA_NAME).collect();
    let name = name.trim();
    if name.is_empty() {
        host.chars().take(MAX_MEDIA_NAME).collect()
    } else {
        name.to_owned()
    }
}

/// The team's media library, newest first, a page at a time; `q` searches names and URLs.
#[utoipa::path(get, path = "/media-library", tag = "library",
    security(("session" = []), ("api_token" = [])), params(MediaQuery),
    responses((status = 200, body = MediaPage), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn media_index(
    State(app): State<AppState>,
    a: Actor,
    q: Result<Query<MediaQuery>, QueryRejection>,
) -> Response {
    match q {
        Ok(Query(q)) => list_media(&app, &a, q).await,
        Err(_) => reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query"),
    }
}

/// The body of `GET /media-library`, shared with the MCP `list-media` tool.
pub async fn list_media(app: &AppState, a: &Actor, q: MediaQuery) -> Response {
    if let Some(res) = refuses_set_token(a) {
        return res;
    }
    let limit = q.limit.unwrap_or(PAGE);
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if !(1..=MAX_PAGE).contains(&limit) || search.is_some_and(|s| s.chars().count() > MAX_QUERY) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let mut values: Vec<Value> = vec![a.team.id.into()];
    let mut sql = String::from("SELECT m.* FROM media_library m WHERE m.team_id = $1");
    if let Some(s) = search {
        values.push(like(s).into());
        let p = values.len();
        sql += &format!(" AND (m.name ILIKE ${p} OR m.url ILIKE ${p})");
    }
    if let Some(c) = q.cursor {
        values.push(c.into());
        let p = values.len();
        sql += &format!(
            " AND (m.created_at, m.id) < \
             (SELECT c.created_at, c.id FROM media_library c WHERE c.id = ${p} AND c.team_id = $1)"
        );
    }
    values.push(((limit + 1) as i64).into());
    sql += &format!(
        " ORDER BY m.created_at DESC, m.id DESC LIMIT ${}",
        values.len()
    );
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, values);
    match media_library::Entity::find()
        .from_raw_sql(stmt)
        .all(&app.db)
        .await
    {
        Ok(mut rows) => {
            let more = rows.len() as u64 > limit;
            rows.truncate(limit as usize);
            let next_cursor = more.then(|| rows.last().map(|r| r.id)).flatten();
            let items = rows.into_iter().map(media_json).collect();
            Json(MediaPage { items, next_cursor }).into_response()
        }
        Err(e) => internal(e, "media list"),
    }
}

/// Adds a URL to the team's library. `422 invalid_media` names the bad `field`; a URL already
/// there is `409 already_in_library` with that item's `id`; over 1000 items is `422
/// library_full`.
#[utoipa::path(post, path = "/media-library", tag = "library",
    security(("session" = []), ("api_token" = [])), request_body = MediaInput,
    params(("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 201, body = MediaItem), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn media_create(
    State(app): State<AppState>,
    a: Actor,
    Body(input): Body<MediaInput>,
) -> Response {
    add_media(&app, &a, input).await
}

/// The body of `POST /media-library`, shared with the MCP `add-media` tool.
pub async fn add_media(app: &AppState, a: &Actor, input: MediaInput) -> Response {
    if let Some(res) = refuses_set_token(a) {
        return res;
    }
    if !https_url(&input.url) {
        return invalid("invalid_media", "url");
    }
    let name = match input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        None => default_name(&input.url),
        Some(n) => match name_up_to(n, MAX_MEDIA_NAME) {
            Some(n) => n,
            None => return invalid("invalid_media", "name"),
        },
    };
    let team = a.team.id;
    let done = async {
        let txn = app.db.begin().await?;
        lock(&txn, "media_library", team).await?;
        let mine = media_library::Column::TeamId.eq(team);
        let existing = media_library::Entity::find()
            .filter(mine.clone())
            .filter(media_library::Column::Url.eq(input.url.as_str()))
            .one(&txn)
            .await?;
        if let Some(m) = existing {
            let body = json!({"error": "already_in_library", "id": m.id});
            return Ok(Err((StatusCode::CONFLICT, Json(body)).into_response()));
        }
        let count = media_library::Entity::find()
            .filter(mine)
            .count(&txn)
            .await?;
        if count >= MAX_MEDIA {
            return Ok(Err(reply(StatusCode::UNPROCESSABLE_ENTITY, "library_full")));
        }
        let row = media_library::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(team),
            url: Set(input.url),
            name: Set(name),
            created_by: Set(a.user_id),
            ..Default::default()
        };
        let m = media_library::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(m))
    };
    match done.await {
        Ok(Ok(m)) => (StatusCode::CREATED, Json(media_json(m))).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "media insert"),
    }
}

/// Renames an item of the team's library.
#[utoipa::path(patch, path = "/media-library/{id}", tag = "library",
    security(("session" = []), ("api_token" = [])), request_body = MediaName,
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 200, body = MediaItem), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 422, body = Error)))]
async fn media_rename(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<MediaName>,
) -> Response {
    if let Some(res) = refuses_set_token(&a) {
        return res;
    }
    let Some(name) = name_up_to(&input.name, MAX_MEDIA_NAME) else {
        return invalid("invalid_media", "name");
    };
    let done = media_library::Entity::update_many()
        .col_expr(media_library::Column::Name, name.into())
        .filter(media_library::Column::Id.eq(id))
        .filter(media_library::Column::TeamId.eq(a.team.id))
        .exec_with_returning(&app.db)
        .await;
    match done.map(|rows| rows.into_iter().next()) {
        Ok(Some(m)) => Json(media_json(m)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "media rename"),
    }
}

/// Takes an item out of the team's library; posts that use its URL keep it.
#[utoipa::path(delete, path = "/media-library/{id}", tag = "library",
    security(("session" = []), ("api_token" = [])),
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn media_destroy(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    if let Some(res) = refuses_set_token(&a) {
        return res;
    }
    let done = media_library::Entity::delete_many()
        .filter(media_library::Column::Id.eq(id))
        .filter(media_library::Column::TeamId.eq(a.team.id))
        .exec(&app.db)
        .await;
    match done {
        Ok(r) if r.rows_affected > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "media delete"),
    }
}

/// The team's snippets by name, of one `kind` or all.
#[utoipa::path(get, path = "/snippets", tag = "library",
    security(("session" = []), ("api_token" = [])), params(SnippetQuery),
    responses((status = 200, body = SnippetList), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn snippet_index(
    State(app): State<AppState>,
    a: Actor,
    q: Result<Query<SnippetQuery>, QueryRejection>,
) -> Response {
    match q {
        Ok(Query(q)) => list_snippets(&app, &a, q).await,
        Err(_) => reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query"),
    }
}

fn kind_ok(kind: &str) -> bool {
    matches!(kind, "caption" | "hashtags" | "reply")
}

/// The body of `GET /snippets`, shared with the MCP `list-snippets` tool.
pub async fn list_snippets(app: &AppState, a: &Actor, q: SnippetQuery) -> Response {
    if let Some(res) = refuses_set_token(a) {
        return res;
    }
    if q.kind.as_deref().is_some_and(|k| !kind_ok(k)) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    }
    let mut find = snippets::Entity::find().filter(snippets::Column::TeamId.eq(a.team.id));
    if let Some(k) = q.kind {
        find = find.filter(snippets::Column::Kind.eq(k));
    }
    let lower = Func::lower(Expr::col(snippets::Column::Name));
    let found = find
        .order_by_asc(Expr::expr(lower))
        .order_by_asc(snippets::Column::Name)
        .order_by_asc(snippets::Column::Id)
        .all(&app.db)
        .await;
    match found {
        Ok(rows) => {
            let items = rows.into_iter().map(snippet_json).collect();
            Json(SnippetList { items }).into_response()
        }
        Err(e) => internal(e, "snippet list"),
    }
}

/// A checked snippet: `(kind, name, body)`, or the field to name in `422 invalid_snippet`.
fn checked(input: SnippetInput) -> Result<(String, String, String), &'static str> {
    if !kind_ok(&input.kind) {
        return Err("kind");
    }
    let Some(name) = name_up_to(&input.name, MAX_SNIPPET_NAME) else {
        return Err("name");
    };
    if input.body.trim().is_empty() || input.body.chars().count() > MAX_SNIPPET_BODY {
        return Err("body");
    }
    Ok((input.kind, name, input.body))
}

/// Whether another snippet of the team (not `except`) has this kind and name, ignoring case.
async fn name_taken(
    db: &impl ConnectionTrait,
    team: Uuid,
    kind: &str,
    name: &str,
    except: Option<Uuid>,
) -> Result<bool, DbErr> {
    use sea_orm::sea_query::ExprTrait;
    let lower = Expr::expr(Func::lower(Expr::col(snippets::Column::Name)));
    let mut find = snippets::Entity::find()
        .filter(snippets::Column::TeamId.eq(team))
        .filter(snippets::Column::Kind.eq(kind))
        .filter(lower.eq(Func::lower(Expr::val(name))));
    if let Some(id) = except {
        find = find.filter(snippets::Column::Id.ne(id));
    }
    Ok(find.count(db).await? > 0)
}

fn name_taken_reply() -> Response {
    reply(StatusCode::CONFLICT, "snippet_name_taken")
}

/// Saves a snippet. `422 invalid_snippet` names the bad `field`; a name the team already has
/// for the kind is `409 snippet_name_taken`; over 500 snippets is `422 too_many_snippets`.
#[utoipa::path(post, path = "/snippets", tag = "library",
    security(("session" = []), ("api_token" = [])), request_body = SnippetInput,
    params(("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 201, body = Snippet), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error)))]
async fn snippet_create(
    State(app): State<AppState>,
    a: Actor,
    Body(input): Body<SnippetInput>,
) -> Response {
    create_snippet(&app, &a, input).await
}

/// The body of `POST /snippets`, shared with the MCP `save-snippet` tool.
pub async fn create_snippet(app: &AppState, a: &Actor, input: SnippetInput) -> Response {
    if let Some(res) = refuses_set_token(a) {
        return res;
    }
    let (kind, name, body) = match checked(input) {
        Ok(s) => s,
        Err(field) => return invalid("invalid_snippet", field),
    };
    let team = a.team.id;
    let done = async {
        let txn = app.db.begin().await?;
        lock(&txn, "snippets", team).await?;
        if name_taken(&txn, team, &kind, &name, None).await? {
            return Ok(Err(name_taken_reply()));
        }
        let count = snippets::Entity::find()
            .filter(snippets::Column::TeamId.eq(team))
            .count(&txn)
            .await?;
        if count >= MAX_SNIPPETS {
            return Ok(Err(reply(
                StatusCode::UNPROCESSABLE_ENTITY,
                "too_many_snippets",
            )));
        }
        let row = snippets::ActiveModel {
            id: Set(Uuid::now_v7()),
            team_id: Set(team),
            kind: Set(kind),
            name: Set(name),
            body: Set(body),
            ..Default::default()
        };
        let s = snippets::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(s))
    };
    match done.await {
        Ok(Ok(s)) => (StatusCode::CREATED, Json(snippet_json(s))).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "snippet insert"),
    }
}

/// Replaces a snippet's kind, name and body, checked as on create.
#[utoipa::path(put, path = "/snippets/{id}", tag = "library",
    security(("session" = []), ("api_token" = [])), request_body = SnippetInput,
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 200, body = Snippet), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn snippet_update(
    State(app): State<AppState>,
    a: Actor,
    Path(id): Path<Uuid>,
    Body(input): Body<SnippetInput>,
) -> Response {
    update_snippet(&app, &a, id, input).await
}

/// The body of `PUT /snippets/{id}`, shared with the MCP `save-snippet` tool.
pub async fn update_snippet(app: &AppState, a: &Actor, id: Uuid, input: SnippetInput) -> Response {
    if let Some(res) = refuses_set_token(a) {
        return res;
    }
    let (kind, name, body) = match checked(input) {
        Ok(s) => s,
        Err(field) => return invalid("invalid_snippet", field),
    };
    let team = a.team.id;
    let done = async {
        let txn = app.db.begin().await?;
        lock(&txn, "snippets", team).await?;
        if name_taken(&txn, team, &kind, &name, Some(id)).await? {
            return Ok(Err(name_taken_reply()));
        }
        let rows = snippets::Entity::update_many()
            .col_expr(snippets::Column::Kind, kind.into())
            .col_expr(snippets::Column::Name, name.into())
            .col_expr(snippets::Column::Body, body.into())
            .col_expr(
                snippets::Column::UpdatedAt,
                Utc::now().fixed_offset().into(),
            )
            .filter(snippets::Column::Id.eq(id))
            .filter(snippets::Column::TeamId.eq(team))
            .exec_with_returning(&txn)
            .await?;
        let Some(s) = rows.into_iter().next() else {
            return Ok(Err(not_found()));
        };
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(s))
    };
    match done.await {
        Ok(Ok(s)) => Json(snippet_json(s)).into_response(),
        Ok(Err(res)) => res,
        Err(e) => internal(e, "snippet update"),
    }
}

/// Deletes a snippet; posts that used its text keep it.
#[utoipa::path(delete, path = "/snippets/{id}", tag = "library",
    security(("session" = []), ("api_token" = [])),
    params(("id" = Uuid, Path), ("Idempotency-Key" = Option<String>, Header, description = IDEMPOTENCY)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn snippet_destroy(State(app): State<AppState>, a: Actor, Path(id): Path<Uuid>) -> Response {
    delete_snippet(&app, &a, id).await
}

/// The body of `DELETE /snippets/{id}`, shared with the MCP `delete-snippet` tool.
pub async fn delete_snippet(app: &AppState, a: &Actor, id: Uuid) -> Response {
    if let Some(res) = refuses_set_token(a) {
        return res;
    }
    let done = snippets::Entity::delete_many()
        .filter(snippets::Column::Id.eq(id))
        .filter(snippets::Column::TeamId.eq(a.team.id))
        .exec(&app.db)
        .await;
    match done {
        Ok(r) if r.rows_affected > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "snippet delete"),
    }
}

/// `/media-library` routes; a set-scoped token is `403 team_token_required` on each.
pub fn media_api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(media_index, media_create))
        .routes(routes!(media_rename, media_destroy))
}

/// `/snippets` routes; a set-scoped token is `403 team_token_required` on each, and a
/// 5000-character body can pass 4 KiB.
pub fn snippets_api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(snippet_index, snippet_create))
        .routes(routes!(snippet_update, snippet_destroy))
}

#[cfg(test)]
mod tests {
    use super::{default_name, percent_decode};

    #[test]
    fn default_names_come_from_the_last_path_segment() {
        for (url, name) in [
            ("https://a.example/x/My%20Photo.jpg?w=1#t", "My Photo.jpg"),
            ("https://a.example/x/dir/", "dir"),
            ("https://a.example/", "a.example"),
            ("https://a.example", "a.example"),
            ("https://a.example/%E0%A6%AC.png", "\u{09ac}.png"),
            ("https://a.example/%00%01", "a.example"),
            ("https://a.example/100%", "100%"),
        ] {
            assert_eq!(default_name(url), name, "{url}");
        }
        let long = format!("https://a.example/{}", "b".repeat(300));
        assert_eq!(default_name(&long).chars().count(), 100);
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }
}
