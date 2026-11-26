//! The `{id}` of a route that names a list, an automation, a broadcast, a newsletter, an inbox,
//! an inbox item or a contact: its `id` (a UUID) or its `number` (plain digits), as `PostId` does
//! for posts. A number that names nothing becomes the nil UUID, so the handler answers its usual
//! `404`; the handler also looks the row up in the caller's team, so another team's number is a
//! `404` too.
use std::collections::HashMap;

use axum::{
    extract::{FromRequestParts, Path},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
};
use schemars::JsonSchema;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::Deserialize;
use uuid::Uuid;

use super::{auth::internal, waitlist::error};
use crate::state::AppState;

pub(crate) const ID_PARAM: &str = "The `id` (UUID) or the `number` (an integer).";

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found").into_response()
}

/// The id of `table`'s row numbered `n`; the nil UUID when there is none. `table` is one of the
/// constants below, never input.
async fn by_number(db: &impl ConnectionTrait, table: &str, n: i64) -> Result<Uuid, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!("SELECT id FROM {table} WHERE number = $1"),
        [n.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "id"),
        None => Ok(Uuid::nil()),
    }
}

/// The numbers of the rows `ids` of `table`, by id; a row that is gone has none.
pub async fn numbers(
    db: &impl ConnectionTrait,
    table: &str,
    ids: impl IntoIterator<Item = Uuid>,
) -> Result<HashMap<Uuid, i64>, DbErr> {
    let ids: Vec<Uuid> = ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!(
            "SELECT id, number FROM {table} \
             WHERE id IN (SELECT jsonb_array_elements_text($1::jsonb)::uuid)"
        ),
        [serde_json::json!(ids).into()],
    );
    db.query_all_raw(stmt)
        .await?
        .iter()
        .map(|r| Ok((r.try_get("", "id")?, r.try_get("", "number")?)))
        .collect()
}

/// A row named by its `id` (a UUID string) or its `number` (an integer), as the MCP tools take it.
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Ref {
    Id(Uuid),
    Number(i64),
    /// A number sent as text, which some MCP clients do.
    Text(String),
}

impl Ref {
    /// The row's id in `table`; the nil UUID for a number that names none.
    pub async fn resolve(self, db: &impl ConnectionTrait, table: &str) -> Result<Uuid, DbErr> {
        match self {
            Self::Id(id) => Ok(id),
            Self::Number(n) => by_number(db, table, n).await,
            Self::Text(t) => by_number(db, table, t.trim().parse().unwrap_or(0)).await,
        }
    }
}

/// The path parameter `key` as an id: a UUID, or digits looked up in `table`.
// A `Response` is what an extractor's rejection is, as in `PostId`.
#[allow(clippy::result_large_err)]
async fn param(
    parts: &mut Parts,
    app: &AppState,
    key: &str,
    table: &str,
) -> Result<Uuid, Response> {
    let Path(all) = Path::<HashMap<String, String>>::from_request_parts(parts, app)
        .await
        .map_err(IntoResponse::into_response)?;
    let raw = all.get(key).ok_or_else(not_found)?;
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        let n = raw.parse().unwrap_or(0);
        by_number(&app.db, table, n)
            .await
            .map_err(|e| internal(e, "number lookup"))
    } else {
        Uuid::parse_str(raw).map_err(|_| not_found())
    }
}

macro_rules! numbered {
    ($($(#[$doc:meta])* $name:ident => $table:literal,)*) => {$(
        $(#[$doc])*
        pub struct $name(pub Uuid);

        impl FromRequestParts<AppState> for $name {
            type Rejection = Response;

            async fn from_request_parts(p: &mut Parts, app: &AppState) -> Result<Self, Response> {
                param(p, app, "id", $table).await.map(Self)
            }
        }
    )*};
}

numbered! {
    /// A list.
    ListId => "post_lists",
    /// An automation.
    AutomationId => "automations",
    /// A broadcast.
    BroadcastId => "broadcasts",
    /// A newsletter.
    NewsletterId => "newsletters",
    /// An inbox.
    InboxId => "inboxes",
    /// An inbox item.
    ItemId => "inbox_items",
    /// A contact.
    ContactId => "contacts",
}

/// A second path parameter that stays a UUID (`{run_id}`, `{post_id}`), next to one of the above.
macro_rules! plain {
    ($($name:ident => $key:literal,)*) => {$(
        pub struct $name(pub Uuid);

        impl FromRequestParts<AppState> for $name {
            type Rejection = Response;

            async fn from_request_parts(p: &mut Parts, app: &AppState) -> Result<Self, Response> {
                let Path(all) = Path::<HashMap<String, String>>::from_request_parts(p, app)
                    .await
                    .map_err(IntoResponse::into_response)?;
                let id = all.get($key).and_then(|v| Uuid::parse_str(v).ok());
                id.map(Self).ok_or_else(not_found)
            }
        }
    )*};
}

plain! {
    RunId => "run_id",
    PostParam => "post_id",
}
