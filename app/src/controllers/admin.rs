//! The site admin console, `/v1/admin/*`: totals, users, teams, waitlist approval and the
//! sign-up rules. Only for a user in `ADMIN_EMAILS` signed in with the session cookie (API tokens
//! do not reach it: `CurrentUser` reads the cookie alone). Nothing here returns post bodies,
//! tokens or any credential. The waitlist list itself is `waitlist::admin_list`.
use std::collections::BTreeMap;

use axum::{
    extract::{rejection::QueryRejection, FromRequestParts, Path, Query, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{
    sea_query::Expr, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter,
    QueryResult, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, signup_rules, Body, CurrentUser, FromWeb},
    waitlist::{error, is_admin, like, AdminSignup, Error, Reply},
};
use crate::{config::signup_entry, models::waitlist_signups, state::AppState};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 200;
const MAX_QUERY_CHARS: usize = 200;
const MAX_SIGNUP_DOMAINS: usize = 50;

/// A user in `ADMIN_EMAILS` with a session cookie: `401 unauthorized` signed out, `403 forbidden`
/// for anyone else.
pub struct Admin(pub CurrentUser);

impl FromRequestParts<AppState> for Admin {
    type Rejection = Reply;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Reply> {
        let current = CurrentUser::from_request_parts(parts, app).await?;
        if !is_admin(app, &current.0.email) {
            return Err(error(StatusCode::FORBIDDEN, "forbidden"));
        }
        Ok(Self(current))
    }
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn stmt(sql: &str, values: impl IntoIterator<Item = sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

#[derive(Serialize, ToSchema)]
pub struct AccountTotals {
    pub total: i64,
    /// Connected accounts per provider, e.g. `{"bluesky": 3}`; a provider with none is absent.
    pub by_provider: BTreeMap<String, i64>,
}

#[derive(Serialize, ToSchema)]
pub struct WaitlistTotals {
    pub total: i64,
    pub verified: i64,
    pub approved: i64,
}

#[derive(Serialize, ToSchema)]
pub struct Overview {
    pub users: i64,
    /// Accounts created in the last 7 days.
    pub new_users_7d: i64,
    pub teams: i64,
    pub accounts: AccountTotals,
    /// Posts per status. `scheduled`, `published` and `failed` are always there; `draft`,
    /// `queued` and `deleted` only when a post has that status.
    pub posts: BTreeMap<String, i64>,
    pub waitlist: WaitlistTotals,
}

const OVERVIEW: &str = "SELECT (SELECT count(*) FROM users) AS users, \
    (SELECT count(*) FROM users WHERE created_at > now() - interval '7 days') AS new_users_7d, \
    (SELECT count(*) FROM teams) AS teams, \
    (SELECT count(*) FROM waitlist_signups) AS waitlist, \
    (SELECT count(*) FROM waitlist_signups WHERE verified_at IS NOT NULL) AS verified, \
    (SELECT count(*) FROM waitlist_signups WHERE approved_at IS NOT NULL) AS approved, \
    COALESCE((SELECT jsonb_object_agg(provider, n) FROM \
      (SELECT provider, count(*) AS n FROM social_accounts GROUP BY provider) a), '{}') AS accounts, \
    COALESCE((SELECT jsonb_object_agg(status, n) FROM \
      (SELECT status, count(*) AS n FROM posts GROUP BY status) p), '{}') AS posts";

/// A `{name: count}` JSON column.
fn counts(row: &QueryResult, col: &str) -> Result<BTreeMap<String, i64>, DbErr> {
    let value: serde_json::Value = row.try_get("", col)?;
    serde_json::from_value(value).map_err(|e| DbErr::Custom(format!("{col}: {e}")))
}

/// Totals for the console's first page. Counts only.
#[utoipa::path(get, path = "/admin/overview", tag = "admin", security(("session" = [])),
    responses((status = 200, body = Overview), (status = 401, body = Error),
        (status = 403, body = Error), (status = 500, body = Error)))]
async fn overview(State(app): State<AppState>, _: Admin) -> Response {
    let found = async {
        let row = app
            .db
            .query_one_raw(stmt(OVERVIEW, []))
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("overview".into()))?;
        let by_provider = counts(&row, "accounts")?;
        let mut posts = counts(&row, "posts")?;
        for status in ["scheduled", "published", "failed"] {
            posts.entry(status.into()).or_insert(0);
        }
        Ok::<_, DbErr>(Overview {
            users: row.try_get("", "users")?,
            new_users_7d: row.try_get("", "new_users_7d")?,
            teams: row.try_get("", "teams")?,
            accounts: AccountTotals {
                total: by_provider.values().sum(),
                by_provider,
            },
            posts,
            waitlist: WaitlistTotals {
                total: row.try_get("", "waitlist")?,
                verified: row.try_get("", "verified")?,
                approved: row.try_get("", "approved")?,
            },
        })
    };
    match found.await {
        Ok(overview) => Json(overview).into_response(),
        Err(e) => internal(e, "admin overview"),
    }
}

#[derive(Deserialize, IntoParams)]
pub struct ListQuery {
    /// Case-insensitive substring to match, at most 200 characters.
    pub q: Option<String>,
    /// Rows per page, 1 to 200. 50 when absent.
    pub limit: Option<u64>,
    /// Rows to skip. 0 when absent.
    pub offset: Option<u64>,
}

/// The `LIKE` pattern, limit and offset of `q`; `None` is a `422 invalid_query`.
fn page(q: Result<Query<ListQuery>, QueryRejection>) -> Option<(String, i64, i64)> {
    let Query(q) = q.ok()?;
    let text = q.q.as_deref().unwrap_or_default().trim();
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if text.chars().count() > MAX_QUERY_CHARS || !(1..=MAX_LIMIT).contains(&limit) {
        return None;
    }
    let offset = i64::try_from(q.offset.unwrap_or(0)).ok()?;
    Some((like(text), limit.cast_signed(), offset))
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct UserTeam {
    pub id: Uuid,
    pub name: String,
    /// `owner`, `admin` or `member`.
    pub role: String,
}

#[derive(Serialize, ToSchema)]
pub struct AdminUser {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    /// In `ADMIN_EMAILS`.
    pub is_admin: bool,
    pub created_at: DateTime<FixedOffset>,
    /// When the newest session still stored began; `null` once signed out everywhere or
    /// expired. Sign-ins are not tracked beyond that.
    pub last_sign_in_at: Option<DateTime<FixedOffset>>,
    /// Oldest membership first.
    pub teams: Vec<UserTeam>,
}

#[derive(Serialize, ToSchema)]
pub struct AdminUsers {
    /// Users matching `q`, whatever the page.
    pub total: i64,
    /// Newest first.
    pub users: Vec<AdminUser>,
}

// shortcut: ILIKE '%q%' scans the table (here and in TEAMS), add a trigram index once there are
// tens of thousands of rows.
const USERS: &str = "SELECT u.id, u.email, u.name, u.created_at, \
    (SELECT max(s.created_at) FROM sessions s WHERE s.user_id = u.id) AS last_sign_in_at, \
    COALESCE((SELECT jsonb_agg(jsonb_build_object('id', t.id, 'name', t.name, 'role', m.role) \
        ORDER BY m.created_at, t.id) \
      FROM team_user m JOIN teams t ON t.id = m.team_id WHERE m.user_id = u.id), '[]') AS teams \
    FROM users u WHERE u.email ILIKE $1 OR u.name ILIKE $1 \
    ORDER BY u.created_at DESC, u.id DESC LIMIT $2 OFFSET $3";

async fn total(app: &AppState, sql: &str, pattern: &str) -> Result<i64, DbErr> {
    let row = app.db.query_one_raw(stmt(sql, [pattern.into()])).await?;
    row.map_or(Ok(0), |r| r.try_get("", "n"))
}

/// Every account with its teams. `q` matches the email or the name.
#[utoipa::path(get, path = "/admin/users", tag = "admin", security(("session" = [])),
    params(ListQuery),
    responses((status = 200, body = AdminUsers), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error), (status = 500, body = Error)))]
async fn users(
    State(app): State<AppState>,
    _: Admin,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let Some((pattern, limit, offset)) = page(q) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    let found = async {
        let count = "SELECT count(*) AS n FROM users u WHERE u.email ILIKE $1 OR u.name ILIKE $1";
        let total = total(&app, count, &pattern).await?;
        let values = [pattern.into(), limit.into(), offset.into()];
        let rows = app.db.query_all_raw(stmt(USERS, values)).await?;
        let users = rows
            .iter()
            .map(|r| {
                let email: String = r.try_get("", "email")?;
                let teams: serde_json::Value = r.try_get("", "teams")?;
                Ok(AdminUser {
                    id: r.try_get("", "id")?,
                    is_admin: is_admin(&app, &email),
                    email,
                    name: r.try_get("", "name")?,
                    created_at: r.try_get("", "created_at")?,
                    last_sign_in_at: r.try_get("", "last_sign_in_at")?,
                    teams: serde_json::from_value(teams)
                        .map_err(|e| DbErr::Custom(format!("teams: {e}")))?,
                })
            })
            .collect::<Result<_, DbErr>>()?;
        Ok::<_, DbErr>(AdminUsers { total, users })
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "admin users"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct AdminTeam {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<FixedOffset>,
    pub members: i64,
    /// Connected social accounts.
    pub accounts: i64,
    /// Posts of any status but `deleted`.
    pub posts: i64,
    /// The owner's email (the longest-standing one when there are several).
    pub owner_email: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AdminTeams {
    /// Teams matching `q`, whatever the page.
    pub total: i64,
    /// Newest first.
    pub teams: Vec<AdminTeam>,
}

const TEAMS: &str = "SELECT t.id, t.name, t.created_at, \
    (SELECT count(*) FROM team_user m WHERE m.team_id = t.id) AS members, \
    (SELECT count(*) FROM social_accounts a WHERE a.team_id = t.id) AS accounts, \
    (SELECT count(*) FROM posts p WHERE p.team_id = t.id AND p.status <> 'deleted') AS posts, \
    (SELECT u.email FROM team_user m JOIN users u ON u.id = m.user_id \
      WHERE m.team_id = t.id AND m.role = 'owner' \
      ORDER BY m.created_at, u.id LIMIT 1) AS owner_email \
    FROM teams t WHERE t.name ILIKE $1 \
    ORDER BY t.created_at DESC, t.id DESC LIMIT $2 OFFSET $3";

/// Every team with its sizes. `q` matches the team name.
#[utoipa::path(get, path = "/admin/teams", tag = "admin", security(("session" = [])),
    params(ListQuery),
    responses((status = 200, body = AdminTeams), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error), (status = 500, body = Error)))]
async fn teams(
    State(app): State<AppState>,
    _: Admin,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let Some((pattern, limit, offset)) = page(q) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query");
    };
    let found = async {
        let count = "SELECT count(*) AS n FROM teams t WHERE t.name ILIKE $1";
        let total = total(&app, count, &pattern).await?;
        let values = [pattern.into(), limit.into(), offset.into()];
        let rows = app.db.query_all_raw(stmt(TEAMS, values)).await?;
        let teams = rows
            .iter()
            .map(|r| {
                Ok(AdminTeam {
                    id: r.try_get("", "id")?,
                    name: r.try_get("", "name")?,
                    created_at: r.try_get("", "created_at")?,
                    members: r.try_get("", "members")?,
                    accounts: r.try_get("", "accounts")?,
                    posts: r.try_get("", "posts")?,
                    owner_email: r.try_get("", "owner_email")?,
                })
            })
            .collect::<Result<_, DbErr>>()?;
        Ok::<_, DbErr>(AdminTeams { total, teams })
    };
    match found.await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e, "admin teams"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct Approval {
    pub signup: AdminSignup,
    /// `sent`: the email went out. `failed`: it did not, and the approval stands (it is not
    /// sent again). `already_approved`: approved before, nothing was sent now.
    pub mail: String,
}

/// Approves a waitlist signup: its exact email may then sign up whatever the sign-up rules say,
/// and is told so by email with a link to `{WEB_URL}/signup`. Approving again changes nothing
/// and sends nothing.
#[utoipa::path(post, path = "/admin/waitlist/{id}/approve", tag = "admin",
    security(("session" = [])), params(("id" = Uuid, Path,)),
    responses((status = 200, body = Approval), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 500, body = Error)))]
async fn approve(
    _: FromWeb,
    State(app): State<AppState>,
    _: Admin,
    Path(id): Path<Uuid>,
) -> Response {
    use waitlist_signups::{Column, Entity};
    let found = async {
        // One UPDATE claims the approval, so two calls at once send one email.
        let claimed = Entity::update_many()
            .col_expr(Column::ApprovedAt, Expr::value(Utc::now().fixed_offset()))
            .filter(Column::Id.eq(id))
            .filter(Column::ApprovedAt.is_null())
            .exec(&app.db)
            .await?;
        let row = Entity::find_by_id(id).one(&app.db).await?;
        Ok::<_, DbErr>((claimed.rows_affected == 1, row))
    };
    let (new, row) = match found.await {
        Ok((new, Some(row))) => (new, row),
        Ok((_, None)) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "waitlist approve"),
    };
    let mail = if new {
        let text = format!(
            "Good news: you are off the SocialReins waitlist and can now create your account.\n\n\
             Use this email address when you sign up.\n\nCreate my account:\n\n{}/signup\n\n\
             If you did not join the waitlist, ignore this email.\n",
            app.web_url
        );
        let subject = "You can now create your SocialReins account";
        match app.mailer.send(&row.email, subject, text).await {
            Ok(()) => "sent",
            Err(e) => {
                tracing::error!(error = %e, "waitlist approval mail not sent");
                "failed"
            }
        }
    } else {
        "already_approved"
    };
    Json(Approval {
        signup: row.into(),
        mail: mail.into(),
    })
    .into_response()
}

/// Removes a waitlist signup, `204`. An approved email that has not signed up yet loses its
/// approval with it.
#[utoipa::path(delete, path = "/admin/waitlist/{id}", tag = "admin",
    security(("session" = [])), params(("id" = Uuid, Path,)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error), (status = 500, body = Error)))]
async fn remove(
    _: FromWeb,
    State(app): State<AppState>,
    _: Admin,
    Path(id): Path<Uuid>,
) -> Response {
    let deleted = waitlist_signups::Entity::delete_by_id(id)
        .exec(&app.db)
        .await;
    match deleted {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "waitlist delete"),
    }
}

#[derive(Serialize, ToSchema)]
pub struct SignupSettings {
    /// `open`: anyone may sign up. `closed`: nobody new. `domains`: only the listed domains and
    /// addresses. `OWNER_EMAIL`, invited emails and approved waitlist emails pass in every mode.
    pub mode: String,
    /// Email domains and whole email addresses, lowercase. Only mode `domains` uses them (and
    /// `closed` on a server with no account and no `OWNER_EMAIL`, for the first sign-up).
    pub domains: Vec<String>,
    /// `saved` once set here, else `env`: `REGISTRATION_DISABLED` and `SIGNUP_DOMAINS` apply.
    pub source: String,
}

#[derive(Deserialize, ToSchema)]
pub struct NewSignupSettings {
    /// `open`, `closed` or `domains`.
    pub mode: String,
    /// At most 50 domains or email addresses; trimmed, lowercased and deduplicated. Mode
    /// `domains` needs at least one.
    #[serde(default)]
    pub domains: Vec<String>,
}

/// The sign-up rules in force.
#[utoipa::path(get, path = "/admin/settings/signup", tag = "admin", security(("session" = [])),
    responses((status = 200, body = SignupSettings), (status = 401, body = Error),
        (status = 403, body = Error), (status = 500, body = Error)))]
async fn signup_settings(State(app): State<AppState>, _: Admin) -> Response {
    match signup_rules(&app).await {
        Ok(rules) => Json(SignupSettings {
            mode: rules.mode,
            domains: rules.domains,
            source: if rules.saved { "saved" } else { "env" }.into(),
        })
        .into_response(),
        Err(e) => internal(e, "signup settings"),
    }
}

/// Saves the sign-up rules, which then replace `REGISTRATION_DISABLED` and `SIGNUP_DOMAINS`.
/// Needs a sign-in from the last 10 minutes (else `403 reauth_required`). `422 invalid_mode`
/// for an unknown mode, `422 invalid_domains` for an entry that is neither a domain nor an
/// email address, more than 50 entries, or mode `domains` with none.
#[utoipa::path(put, path = "/admin/settings/signup", tag = "admin", security(("session" = [])),
    request_body = NewSignupSettings,
    responses((status = 200, body = SignupSettings), (status = 400, body = Error),
        (status = 401, body = Error), (status = 403, body = Error), (status = 413, body = Error),
        (status = 415, body = Error), (status = 422, body = Error), (status = 500, body = Error)))]
async fn save_signup_settings(
    _: FromWeb,
    State(app): State<AppState>,
    Admin(current): Admin,
    Body(body): Body<NewSignupSettings>,
) -> Response {
    if !current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    if !["open", "closed", "domains"].contains(&body.mode.as_str()) {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_mode");
    }
    let invalid = || reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_domains");
    let mut domains = Vec::new();
    for raw in &body.domains {
        let Ok(entry) = signup_entry(&raw.trim().to_lowercase()) else {
            return invalid();
        };
        if !domains.contains(&entry) {
            domains.push(entry);
        }
    }
    if domains.len() > MAX_SIGNUP_DOMAINS || (body.mode == "domains" && domains.is_empty()) {
        return invalid();
    }
    let sql = "INSERT INTO signup_settings (mode, domains) VALUES ($1, $2) \
               ON CONFLICT (id) DO UPDATE \
               SET mode = EXCLUDED.mode, domains = EXCLUDED.domains, updated_at = now()";
    let values = [body.mode.clone().into(), serde_json::json!(domains).into()];
    match app.db.execute_raw(stmt(sql, values)).await {
        Ok(_) => Json(SignupSettings {
            mode: body.mode,
            domains,
            source: "saved".into(),
        })
        .into_response(),
        Err(e) => internal(e, "signup settings save"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(overview))
        .routes(routes!(users))
        .routes(routes!(teams))
        .routes(routes!(approve))
        .routes(routes!(remove))
        .routes(routes!(signup_settings, save_signup_settings))
}
