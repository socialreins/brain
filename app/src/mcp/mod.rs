//! The MCP edge at `/mcp`: rmcp's streamable HTTP service, stateless, JSON responses. Each tool
//! runs the same service fn as its `/v1` route and answers that route's JSON as text; a `4xx` or
//! `5xx` is a tool result with `isError: true`. `auth` resolves the API token first.
pub mod auth;

use std::sync::Arc;

use axum::{
    body::to_bytes,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool as McpTool,
        ToolAnnotations,
    },
    service::RequestContext,
    transport::{
        streamable_http_server::session::never::NeverSessionManager, StreamableHttpServerConfig,
        StreamableHttpService,
    },
    ErrorData, RoleServer, ServerHandler,
};
use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{
    controllers::{
        api_tokens::Actor,
        connections,
        lists::{self, AddPost, ListInput, Order},
        posts,
        posts::{PostInput, PostQuery},
        social_sets::{self, ListQuery},
        waitlist::error,
        MAX_BODY_BYTES,
    },
    state::AppState,
};

#[derive(Deserialize, JsonSchema)]
struct PostId {
    id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
struct UpdatePost {
    id: Uuid,
    #[serde(flatten)]
    post: PostInput,
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
struct ListId {
    id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
struct AddToList {
    /// The list.
    id: Uuid,
    #[serde(flatten)]
    add: AddPost,
}

#[derive(Deserialize, JsonSchema)]
struct RemoveFromList {
    /// The list.
    id: Uuid,
    post_id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
struct Reorder {
    /// The list.
    id: Uuid,
    #[serde(flatten)]
    order: Order,
}

/// Name, description, argument schema and read-only hint of each tool.
fn specs() -> Vec<(&'static str, &'static str, Value, bool)> {
    fn schema<T: JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default()
    }
    vec![
        ("list-social-sets", "The team's live social sets with their account ids, Default first; `deleted: true` lists deleted sets still restorable. A set token sees only its set. Same as GET /v1/social-sets.", schema::<ListQuery>(), true),
        ("list-social-accounts", "The team's connected social accounts with their set ids. Same as GET /v1/connections.", schema::<NoArgs>(), true),
        ("list-posts", "The team's posts by scheduled_at, filtered by set, status and a from/to window on scheduled_at. Same as GET /v1/posts.", schema::<PostQuery>(), true),
        ("get-post", "One post with its targets. Same as GET /v1/posts/{id}.", schema::<PostId>(), true),
        ("create-post", "Creates a post: scheduled with scheduled_at, else draft. Same as POST /v1/posts.", schema::<PostInput>(), false),
        ("update-post", "Replaces a draft or scheduled post. Same as PUT /v1/posts/{id}.", schema::<UpdatePost>(), false),
        ("delete-post", "Deletes a draft or scheduled post, or takes a published one down. Same as DELETE /v1/posts/{id}.", schema::<PostId>(), false),
        ("list-lists", "The team's post lists (ordered groups of posts, such as a series), without their posts. Same as GET /v1/lists.", schema::<NoArgs>(), true),
        ("create-list", "Creates a post list, team-wide or tied to one set. Same as POST /v1/lists.", schema::<ListInput>(), false),
        ("get-list", "One post list with its posts in order; a post's position is its Part N. Same as GET /v1/lists/{id}.", schema::<ListId>(), true),
        ("add-post-to-list", "Adds a post to a list at a 1-based position, appending by default; a post already there moves. Same as POST /v1/lists/{id}/posts.", schema::<AddToList>(), false),
        ("remove-post-from-list", "Takes a post out of a list; the post stays. Same as DELETE /v1/lists/{id}/posts/{post_id}.", schema::<RemoveFromList>(), false),
        ("reorder-list", "Reorders a list: post_ids holds every post of the list, each once. Same as PUT /v1/lists/{id}/order.", schema::<Reorder>(), false),
    ]
}

fn to_mcp(
    (name, description, schema, read_only): (&'static str, &'static str, Value, bool),
) -> McpTool {
    let schema = match schema {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut t = McpTool::new(name, description, schema);
    t.annotations = Some(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(matches!(name, "delete-post" | "remove-post-from-list"))
            .open_world(false),
    );
    t
}

#[derive(Clone)]
pub struct McpServer {
    app: AppState,
}

/// The service mounted at `/mcp` (behind `auth::layer`): stateless, JSON responses, and the
/// `/v1` body limit.
pub fn service(app: &AppState) -> StreamableHttpService<McpServer, NeverSessionManager> {
    let server = McpServer { app: app.clone() };
    let config = StreamableHttpServerConfig::default()
        // Every request needs a bearer token, which a rebinding page cannot add.
        .disable_allowed_hosts()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(MAX_BODY_BYTES);
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

fn args<T: DeserializeOwned>(v: Value) -> Option<T> {
    serde_json::from_value(v).ok()
}

/// The `422` the REST route answers for a query or body it cannot read.
fn invalid(code: &str) -> Response {
    error(StatusCode::UNPROCESSABLE_ENTITY, code).into_response()
}

/// The REST answer of tool `name`; `None` for an unknown tool.
async fn run(app: &AppState, a: &Actor, name: &str, v: Value) -> Option<Response> {
    let res = match name {
        "list-social-sets" => match args(v) {
            Some(q) => social_sets::list_sets(app, a, q).await,
            None => invalid("invalid_query"),
        },
        "list-social-accounts" => connections::list_connections(app, a).await,
        "list-posts" => match args(v) {
            Some(q) => posts::list_posts(app, a, q).await,
            None => invalid("invalid_query"),
        },
        "get-post" => match args::<PostId>(v) {
            Some(p) => posts::get_post(app, a, p.id).await,
            None => invalid("invalid_body"),
        },
        "create-post" => match args(v) {
            Some(p) => posts::create_post(app, a, p).await,
            None => invalid("invalid_body"),
        },
        "update-post" => match args::<UpdatePost>(v) {
            Some(u) => posts::update_post(app, a, u.id, u.post).await,
            None => invalid("invalid_body"),
        },
        "delete-post" => match args::<PostId>(v) {
            Some(p) => posts::delete_post(app, a, p.id).await,
            None => invalid("invalid_body"),
        },
        "list-lists" => lists::list_lists(app, a).await,
        "create-list" => match args(v) {
            Some(l) => lists::create_list(app, a, l).await,
            None => invalid("invalid_body"),
        },
        "get-list" => match args::<ListId>(v) {
            Some(l) => lists::get_list(app, a, l.id).await,
            None => invalid("invalid_body"),
        },
        "add-post-to-list" => match args::<AddToList>(v) {
            Some(l) => lists::add_post(app, a, l.id, l.add).await,
            None => invalid("invalid_body"),
        },
        "remove-post-from-list" => match args::<RemoveFromList>(v) {
            Some(l) => lists::remove_post(app, a, l.id, l.post_id).await,
            None => invalid("invalid_body"),
        },
        "reorder-list" => match args::<Reorder>(v) {
            Some(l) => lists::reorder_list(app, a, l.id, l.order).await,
            None => invalid("invalid_body"),
        },
        _ => return None,
    };
    Some(res)
}

/// The REST body as text content; an empty `204` body reads `{"ok":true}`.
async fn to_result(res: Response) -> CallToolResult {
    let failed = res.status().is_client_error() || res.status().is_server_error();
    let text = match to_bytes(res.into_body(), usize::MAX).await {
        Ok(b) if b.is_empty() => json!({"ok": true}).to_string(),
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            tracing::error!(error = %e, "mcp tool body unreadable");
            json!({"error": "internal"}).to_string()
        }
    };
    let content = vec![ContentBlock::text(text)];
    if failed {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("socialreins", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = specs().into_iter().map(to_mcp).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<McpTool> {
        specs().into_iter().find(|s| s.0 == name).map(to_mcp)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let actor = context
            .extensions
            .get::<Parts>()
            .and_then(|p| p.extensions.get::<Actor>())
            .ok_or_else(|| ErrorData::internal_error("no caller", None))?;
        let v = Value::Object(request.arguments.unwrap_or_default());
        let name = request.name.as_ref();
        match run(&self.app, actor, name, v).await {
            Some(res) => Ok(to_result(res).await.into()),
            None => Err(ErrorData::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            )),
        }
    }
}
