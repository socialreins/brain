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
        api_tokens::{read_only_token, Actor},
        connections,
        library::{self, MediaInput, MediaQuery, SnippetInput, SnippetQuery},
        lists::{self, AddPost, ListInput, Order},
        posts,
        posts::{Decision, PostGroupInput, PostInput, PostQuery},
        queue::{self, QueueInput},
        social_sets::{self, ListQuery},
        waitlist::error,
        MAX_GROUP_BODY_BYTES,
    },
    repeat::Repeat,
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
struct Decide {
    id: Uuid,
    #[serde(flatten)]
    decision: Decision,
}

#[derive(Deserialize, JsonSchema)]
struct SetRepeat {
    /// The post.
    id: Uuid,
    #[serde(flatten)]
    rule: Repeat,
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
struct SetId {
    /// The social set.
    id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
struct SetQueue {
    /// The social set.
    id: Uuid,
    #[serde(flatten)]
    queue: QueueInput,
}

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

#[derive(Deserialize, JsonSchema)]
struct SaveSnippet {
    /// The snippet to replace; absent creates one.
    id: Option<Uuid>,
    #[serde(flatten)]
    snippet: SnippetInput,
}

#[derive(Deserialize, JsonSchema)]
struct SnippetId {
    id: Uuid,
}

/// A create tool's arguments: the route's body, and its `?validate_only`.
#[derive(Deserialize, JsonSchema)]
struct Create<T> {
    #[serde(flatten)]
    input: T,
    /// `true`: check the request and create nothing; the answer lists every problem the create
    /// would refuse, per platform. Allowed with a read-only token.
    #[serde(default)]
    validate_only: bool,
}

/// Whether a call of tool `name` writes: every tool not marked read-only, but for a create with
/// `validate_only: true`, which creates nothing.
fn writes(name: &str, v: &Value) -> bool {
    let validates =
        matches!(name, "create-post" | "create-post-group") && v["validate_only"] == json!(true);
    !validates && specs().into_iter().any(|s| s.0 == name && !s.3)
}

/// Name, description, argument schema and read-only hint of each tool.
fn specs() -> Vec<(&'static str, &'static str, Value, bool)> {
    fn schema<T: JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default()
    }
    vec![
        ("list-social-sets", "The team's live social sets with their account ids, the default set first; `deleted: true` lists deleted sets still restorable. A set token sees only its set. Same as GET /v1/social-sets.", schema::<ListQuery>(), true),
        ("list-social-accounts", "The team's connected social accounts with their set ids. Same as GET /v1/connections.", schema::<NoArgs>(), true),
        ("get-queue", "A social set's posting queue: its IANA time_zone, weekly slots (weekday 0 is Sunday, time HH:MM) and the next 10 free slot times. Same as GET /v1/social-sets/{id}/queue.", schema::<SetId>(), true),
        ("set-queue", "Replaces a social set's posting queue: time_zone (IANA, default UTC) and up to 50 slots. Same as PUT /v1/social-sets/{id}/queue.", schema::<SetQueue>(), false),
        ("list-posts", "The team's posts by scheduled_at, filtered by set, status, approval (pending, approved or rejected) and a from/to window on scheduled_at. Same as GET /v1/posts.", schema::<PostQuery>(), true),
        ("get-post", "One post with its targets. Same as GET /v1/posts/{id}.", schema::<PostId>(), true),
        ("create-post", "Creates a post: scheduled with scheduled_at, or at the earliest free slot of the set's queue with queue: true, else draft. With validate_only nothing is created and the answer lists every problem. Same as POST /v1/posts.", schema::<Create<PostInput>>(), false),
        ("create-post-group", "Creates one post per platform of a set (by default every platform with a connected account), with optional per-platform body and media overrides, tied by a group_id; queue: true schedules the whole group at one free slot of the set's queue; any invalid post creates nothing, unless on_invalid is skip, which leaves that platform out and lists it in skipped. With validate_only nothing is created and the answer lists every problem of every platform. Same as POST /v1/post-groups.", schema::<Create<PostGroupInput>>(), false),
        ("update-post", "Replaces a draft or scheduled post. Same as PUT /v1/posts/{id}.", schema::<UpdatePost>(), false),
        ("approve-post", "Approves a scheduled post waiting for approval (or rejected before), with an optional note; it publishes at its time, or on the next tick when that has passed. Whole-team or set token, not read-only. Same as POST /v1/posts/{id}/approve.", schema::<Decide>(), false),
        ("reject-post", "Rejects a scheduled post with an optional note: it is held back until approved. Same as POST /v1/posts/{id}/reject.", schema::<Decide>(), false),
        ("set-post-repeat", "Makes a scheduled post (and the other scheduled posts of its group) repeat every interval (1 to 12) day, week or month, until an optional YYYY-MM-DD date or for remaining (1 to 100) more copies in all: each time it publishes, the next copy is scheduled. Not for TikTok. Same as PUT /v1/posts/{id}/repeat.", schema::<SetRepeat>(), false),
        ("clear-post-repeat", "Stops a scheduled post (and the other scheduled posts of its group) repeating; the post itself stays scheduled. Same as DELETE /v1/posts/{id}/repeat.", schema::<PostId>(), false),
        ("delete-post", "Deletes a draft or scheduled post, or takes a published one down. Same as DELETE /v1/posts/{id}.", schema::<PostId>(), false),
        ("list-lists", "The team's post lists (ordered groups of posts, such as a series), without their posts. Same as GET /v1/lists.", schema::<NoArgs>(), true),
        ("create-list", "Creates a post list, team-wide or tied to one set. Same as POST /v1/lists.", schema::<ListInput>(), false),
        ("get-list", "One post list with its posts in order; a post's position is its Part N. Same as GET /v1/lists/{id}.", schema::<ListId>(), true),
        ("add-post-to-list", "Adds a post to a list at a 1-based position, appending by default; a post already there moves. Same as POST /v1/lists/{id}/posts.", schema::<AddToList>(), false),
        ("remove-post-from-list", "Takes a post out of a list; the post stays. Same as DELETE /v1/lists/{id}/posts/{post_id}.", schema::<RemoveFromList>(), false),
        ("reorder-list", "Reorders a list: post_ids holds every post of the list, each once. Same as PUT /v1/lists/{id}/order.", schema::<Reorder>(), false),
        ("list-media", "The team's media library (saved media URLs), newest first, a page at a time; q searches names and URLs. Same as GET /v1/media-library.", schema::<MediaQuery>(), true),
        ("add-media", "Adds an https media URL to the team's media library, named by its last path segment unless name is given. Same as POST /v1/media-library.", schema::<MediaInput>(), false),
        ("list-snippets", "The team's saved text snippets by name: kind caption or hashtags, both when absent. Same as GET /v1/snippets.", schema::<SnippetQuery>(), true),
        ("save-snippet", "Saves a caption or hashtags snippet: creates one, or with id replaces that one. Same as POST /v1/snippets and PUT /v1/snippets/{id}.", schema::<SaveSnippet>(), false),
        ("delete-snippet", "Deletes a saved snippet. Same as DELETE /v1/snippets/{id}.", schema::<SnippetId>(), false),
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
            .destructive(matches!(
                name,
                "delete-post" | "remove-post-from-list" | "delete-snippet"
            ))
            .open_world(false),
    );
    t
}

#[derive(Clone)]
pub struct McpServer {
    app: AppState,
}

/// The service mounted at `/mcp` (behind `auth::layer`): stateless, JSON responses, and the
/// largest `/v1` body limit (a post group's); each tool's service fn re-validates its input.
pub fn service(app: &AppState) -> StreamableHttpService<McpServer, NeverSessionManager> {
    let server = McpServer { app: app.clone() };
    let config = StreamableHttpServerConfig::default()
        // Every request needs a bearer token, which a rebinding page cannot add.
        .disable_allowed_hosts()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(MAX_GROUP_BODY_BYTES);
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
        "get-queue" => match args::<SetId>(v) {
            Some(q) => queue::get_queue(app, a, q.id).await,
            None => invalid("invalid_body"),
        },
        "set-queue" => match args::<SetQueue>(v) {
            Some(q) => queue::set_queue(app, a, q.id, q.queue).await,
            None => invalid("invalid_body"),
        },
        "list-posts" => match args(v) {
            Some(q) => posts::list_posts(app, a, q).await,
            None => invalid("invalid_query"),
        },
        "get-post" => match args::<PostId>(v) {
            Some(p) => posts::get_post(app, a, p.id).await,
            None => invalid("invalid_body"),
        },
        "create-post" => match args::<Create<PostInput>>(v) {
            Some(c) => posts::create_post(app, a, c.input, c.validate_only).await,
            None => invalid("invalid_body"),
        },
        "create-post-group" => match args::<Create<PostGroupInput>>(v) {
            Some(c) => posts::create_post_group(app, a, c.input, c.validate_only).await,
            None => invalid("invalid_body"),
        },
        "update-post" => match args::<UpdatePost>(v) {
            Some(u) => posts::update_post(app, a, u.id, u.post).await,
            None => invalid("invalid_body"),
        },
        "approve-post" | "reject-post" => match args::<Decide>(v) {
            Some(d) => {
                let approve = name == "approve-post";
                posts::decide_post(app, a, d.id, approve, d.decision.note).await
            }
            None => invalid("invalid_body"),
        },
        "set-post-repeat" => match args::<SetRepeat>(v) {
            Some(r) => posts::set_repeat(app, a, r.id, Some(r.rule)).await,
            None => invalid("invalid_body"),
        },
        "clear-post-repeat" => match args::<PostId>(v) {
            Some(p) => posts::set_repeat(app, a, p.id, None).await,
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
        "list-media" => match args(v) {
            Some(q) => library::list_media(app, a, q).await,
            None => invalid("invalid_query"),
        },
        "add-media" => match args(v) {
            Some(m) => library::add_media(app, a, m).await,
            None => invalid("invalid_body"),
        },
        "list-snippets" => match args(v) {
            Some(q) => library::list_snippets(app, a, q).await,
            None => invalid("invalid_query"),
        },
        "save-snippet" => match args::<SaveSnippet>(v) {
            Some(SaveSnippet {
                id: Some(id),
                snippet,
            }) => library::update_snippet(app, a, id, snippet).await,
            Some(SaveSnippet { id: None, snippet }) => {
                library::create_snippet(app, a, snippet).await
            }
            None => invalid("invalid_body"),
        },
        "delete-snippet" => match args::<SnippetId>(v) {
            Some(s) => library::delete_snippet(app, a, s.id).await,
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
        if actor.read_only() && writes(name, &v) {
            return Ok(to_result(read_only_token()).await.into());
        }
        match run(&self.app, actor, name, v).await {
            Some(res) => Ok(to_result(res).await.into()),
            None => Err(ErrorData::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            )),
        }
    }
}
