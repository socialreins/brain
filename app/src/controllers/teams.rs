//! Teams, members and invitations, for the signed-in user (session cookie).
//!
//! Every route acts inside the caller's own memberships; a team, member or invitation outside
//! them is `404 {"error":"not_found"}`. A cookie request may name its team by code, in the
//! `X-Team` header or in `?team=` (see `InTeam`). Without one it acts in the current team:
//! `users.current_team_id` when the user still belongs to it, else their oldest membership. A
//! user with no team gets a personal one with its default set, on sign-in or on first use.
use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, SqlErr, Statement,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use socialreins_core::{
    email,
    team::{self, Role},
    token,
};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, CurrentUser, FromWeb},
    waitlist::{error, Error, Reply},
};
use crate::{
    models::{social_sets, team_invitations, team_user, teams, users},
    rate_limit::{daily_refund, daily_reserve, reserve_in},
    state::AppState,
};

const INVITATION_DAYS: i64 = 7;
/// Daily caps (`rate_limit::daily_allow`).
const TEAMS_PER_DAY: i64 = 10;
const INVITES_PER_TEAM_EMAIL: i64 = 3;
const INVITES_PER_EMAIL: i64 = 20;
const INVITES_PER_TEAM: i64 = 50;
const INVITES_PER_INVITER: i64 = 50;
const SLUG_CHANGES_PER_TEAM: i64 = 5;
/// Keeps slug `$1` for team `$2` alone for 30 days from now (`team_slug_holds`).
const HOLD: &str = "INSERT INTO team_slug_holds (slug, team_id) VALUES ($1, $2) \
    ON CONFLICT (slug) DO UPDATE SET team_id = excluded.team_id, released_at = now()";

#[derive(Serialize, ToSchema)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    /// The team's address in the web app (`/t/<code>/...`): 6 lowercase characters that never
    /// change. Not a secret; it gives no access.
    pub code: String,
    /// An address the team chose (`PATCH /team`), accepted wherever the code is; `null` when it
    /// has none. Unlike the code it may change.
    pub slug: Option<String>,
    /// Posts of this team may have a public page (`PUT /posts/{id}/page`).
    pub public_pages: bool,
    /// Public pages offer a newsletter sign-up (`GET /subscribers`).
    pub newsletter: bool,
    /// Who the team's newsletters are from; `null` until set, and no newsletter is sent then.
    pub newsletter_sender: Option<Sender>,
    /// A member's scheduled posts wait for an owner or admin (`POST /posts/{id}/approve`).
    pub require_approval: bool,
    /// The caller's role: `owner`, `admin` or `member`.
    pub role: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Sender {
    /// The From name of the team's newsletters: 1 to 80 characters after trimming, one line,
    /// without `@`, `<`, `>`, `:` or `/` (so it cannot pass as an address or a link).
    pub name: String,
    /// The sender's postal address, shown under every newsletter as the law requires: 10 to
    /// 300 characters after trimming; it may have line breaks.
    pub address: String,
    /// The team's logo, shown above every newsletter in place of the name: a public `https`
    /// image link of at most 2000 characters, with a host and without a user name or password.
    /// `null`, an empty string or no `logo` at all means none. It is never fetched here, only
    /// put in the mail.
    pub logo: Option<String>,
}

/// The logo link as stored (the parsed URL written out, so it holds no white space or control
/// character), or `None` for one that is not a public `https` link.
fn logo(input: &str) -> Option<String> {
    let url = reqwest::Url::parse(input).ok()?;
    let public = url.scheme() == "https"
        && url.host_str().is_some_and(|h| !h.is_empty())
        && url.username().is_empty()
        && url.password().is_none();
    let url = String::from(url);
    (public && url.len() <= 2000).then_some(url)
}

/// The sender details as stored, or the field `422 invalid_sender` names.
fn sender(input: Sender) -> Result<Sender, &'static str> {
    let name = input.name.trim();
    let bad = |c: char| c.is_control() || "@<>:/".contains(c);
    if !(1..=80).contains(&name.chars().count()) || name.chars().any(bad) {
        return Err("name");
    }
    let address = input.address.replace("\r\n", "\n");
    let address = address.trim();
    if !(10..=300).contains(&address.chars().count())
        || address.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err("address");
    }
    let logo = match input.logo.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(link) => Some(logo(link).ok_or("logo")?),
    };
    Ok(Sender {
        name: name.to_owned(),
        address: address.to_owned(),
        logo,
    })
}

#[derive(Serialize, ToSchema)]
pub struct Member {
    /// The user's id.
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub role: String,
    pub joined_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct Invitation {
    pub id: Uuid,
    pub email: String,
    /// `admin` or `member`.
    pub role: String,
    /// The inviting user's id, `null` once that account is gone.
    pub invited_by: Option<Uuid>,
    pub expires_at: DateTime<FixedOffset>,
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct InvitationPreview {
    pub team: String,
    /// The invited email; only the account with this email can accept.
    pub email: String,
    pub role: String,
    /// The inviting user's name, `null` once that account is gone.
    pub invited_by: Option<String>,
    pub expires_at: DateTime<FixedOffset>,
}

#[derive(Deserialize, ToSchema)]
pub struct TeamName {
    /// 1 to 60 characters after trimming, no control characters.
    pub name: String,
}

#[derive(Deserialize, ToSchema)]
pub struct TeamChange {
    /// 1 to 60 characters after trimming, no control characters. Absent: unchanged.
    pub name: Option<String>,
    /// Whether posts may have a public page. Absent: unchanged.
    pub public_pages: Option<bool>,
    /// Whether public pages offer a newsletter sign-up. Absent: unchanged.
    pub newsletter: Option<bool>,
    /// Whether a member's scheduled posts need an owner's or admin's approval before they
    /// publish. Switching it off releases the posts still waiting. Absent: unchanged.
    pub require_approval: Option<bool>,
    /// Who the team's newsletters are from, with the postal address. `null` clears it. Absent:
    /// unchanged.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Sender>)]
    pub newsletter_sender: Option<Option<Sender>>,
    /// The team's slug: 3 to 30 of `a-z`, `0-9` and `-` after trimming and lowercasing, a
    /// letter or digit at both ends, no `--`. `null` clears it. Absent: unchanged.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub slug: Option<Option<String>>,
}

/// A field that was sent, `null` included (an absent one stays `None` through `default`).
pub(super) fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Deserialize::deserialize(d).map(Some)
}

#[derive(Deserialize, ToSchema)]
pub struct SwitchTeam {
    pub team_id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct RoleChange {
    /// `owner`, `admin` or `member`. Only an owner grants or takes away `owner`.
    pub role: String,
}

#[derive(Deserialize, ToSchema)]
pub struct NewInvitation {
    pub email: String,
    /// `admin` or `member`.
    pub role: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, "not_found")
}

fn forbidden() -> Response {
    reply(StatusCode::FORBIDDEN, "forbidden")
}

fn role_of(row: &team_user::Model) -> Role {
    // The CHECK constraint keeps the column to the three roles.
    Role::parse(&row.role).unwrap_or(Role::Member)
}

pub(crate) fn team_json(team: &teams::Model, role: Role) -> Team {
    Team {
        id: team.id,
        name: team.name.clone(),
        code: team.code.clone(),
        slug: team.slug.clone(),
        public_pages: team.public_pages,
        newsletter: team.newsletter,
        newsletter_sender: team
            .newsletter_sender_name
            .clone()
            .zip(team.newsletter_sender_address.clone())
            .map(|(name, address)| Sender {
                name,
                address,
                logo: team.newsletter_sender_logo.clone(),
            }),
        require_approval: team.require_approval,
        role: role.as_str().into(),
    }
}

fn member_json(row: &team_user::Model, user: &users::Model) -> Member {
    Member {
        id: user.id,
        name: user.name.clone(),
        email: user.email.clone(),
        role: row.role.clone(),
        joined_at: row.created_at,
    }
}

fn invitation_json(i: team_invitations::Model) -> Invitation {
    Invitation {
        id: i.id,
        email: i.email,
        role: i.role,
        invited_by: i.invited_by,
        expires_at: i.expires_at,
        created_at: i.created_at,
    }
}

pub(super) async fn membership(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    user_id: Uuid,
) -> Result<Option<team_user::Model>, DbErr> {
    team_user::Entity::find_by_id((team_id, user_id))
        .one(db)
        .await
}

pub(crate) async fn set_current(
    db: &impl ConnectionTrait,
    user_id: Uuid,
    team_id: Uuid,
) -> Result<(), DbErr> {
    let row = users::ActiveModel {
        id: Set(user_id),
        current_team_id: Set(Some(team_id)),
        ..Default::default()
    };
    users::Entity::update(row).exec(db).await.map(|_| ())
}

/// Inserts a team under a code from `code`, asking for another while the code is taken (the
/// unique index decides, so this is safe inside a transaction) or is some team's slug.
pub async fn insert_team(
    db: &impl ConnectionTrait,
    name: &str,
    mut code: impl FnMut() -> String,
) -> Result<teams::Model, DbErr> {
    // Plain SQL: `ON CONFLICT DO NOTHING` returns no row for a taken code, without failing
    // the surrounding transaction.
    let sql = "INSERT INTO teams (id, name, code) SELECT $1::uuid, $2::text, $3::text \
               WHERE NOT EXISTS (SELECT 1 FROM teams WHERE lower(slug) = $3::text) \
               ON CONFLICT (code) DO NOTHING RETURNING *";
    loop {
        let values = [Uuid::now_v7().into(), name.into(), code().into()];
        let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, values);
        if let Some(team) = teams::Entity::find().from_raw_sql(stmt).one(db).await? {
            return Ok(team);
        }
    }
}

/// Makes a team with its default set, `user_id` as owner, and switches the user to it.
async fn create_team(
    db: &impl ConnectionTrait,
    user_id: Uuid,
    name: &str,
) -> Result<teams::Model, DbErr> {
    let team = insert_team(db, name, team::code).await?;
    let owner = team_user::ActiveModel {
        team_id: Set(team.id),
        user_id: Set(user_id),
        role: Set(Role::Owner.as_str().into()),
        ..Default::default()
    };
    team_user::Entity::insert(owner)
        .exec_without_returning(db)
        .await?;
    let set = social_sets::ActiveModel {
        id: Set(Uuid::now_v7()),
        team_id: Set(team.id),
        name: Set("All sets".into()),
        is_default: Set(true),
        ..Default::default()
    };
    social_sets::Entity::insert(set)
        .exec_without_returning(db)
        .await?;
    set_current(db, user_id, team.id).await?;
    Ok(team)
}

/// Gives `user_id` a personal team when they belong to none. The user row is locked first, so
/// concurrent sign-ins of one new user make one team, and a second call is a no-op.
pub(crate) async fn ensure_team(db: &DatabaseConnection, user_id: Uuid) -> Result<(), DbErr> {
    let txn = db.begin().await?;
    let Some(user) = users::Entity::find_by_id(user_id)
        .lock_exclusive()
        .one(&txn)
        .await?
    else {
        return Ok(());
    };
    let teams = team_user::Entity::find()
        .filter(team_user::Column::UserId.eq(user_id))
        .count(&txn)
        .await?;
    if teams == 0 {
        // The first word of the name ("Harun R. Rayhan" gives "Harun"), else the email's local part.
        let first =
            (user.name.split_whitespace().next()).filter(|w| w.chars().any(char::is_alphanumeric));
        let name: String = first
            .unwrap_or_else(|| user.email.split('@').next().unwrap_or_default())
            .chars()
            .take(team::MAX_NAME_CHARS)
            .collect();
        create_team(&txn, user_id, &name).await?;
    }
    txn.commit().await
}

/// The user's current team and role, without creating one.
async fn pick_team(
    db: &DatabaseConnection,
    user: &users::Model,
) -> Result<Option<(teams::Model, Role)>, DbErr> {
    let current = match user.current_team_id {
        Some(id) => membership(db, id, user.id).await?,
        None => None,
    };
    let row = match current {
        Some(row) => Some(row),
        None => {
            team_user::Entity::find()
                .filter(team_user::Column::UserId.eq(user.id))
                .order_by_asc(team_user::Column::CreatedAt)
                .order_by_asc(team_user::Column::TeamId)
                .one(db)
                .await?
        }
    };
    let Some(row) = row else { return Ok(None) };
    if user.current_team_id != Some(row.team_id) {
        set_current(db, user.id, row.team_id).await?;
    }
    let team = teams::Entity::find_by_id(row.team_id).one(db).await?;
    Ok(team.map(|t| (t, role_of(&row))))
}

/// The user's current team and their role in it, making a personal team when they have none.
pub(crate) async fn current_team(
    db: &DatabaseConnection,
    user: &users::Model,
) -> Result<(teams::Model, Role), DbErr> {
    if let Some(found) = pick_team(db, user).await? {
        return Ok(found);
    }
    ensure_team(db, user.id).await?;
    pick_team(db, user)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("no team after ensure_team".into()))
}

/// The header a cookie request names its team in, by code or slug.
pub const TEAM_HEADER: &str = "x-team";

#[derive(Deserialize)]
struct NamedTeam {
    team: Option<String>,
}

/// The team code or slug the request names: the `X-Team` header, else `?team=` (a page navigation
/// cannot set a header). The header is only ever sent by the web app, since CORS allows it for
/// `CORS_ORIGINS` alone. A link can come from anywhere, so the query counts only when the
/// navigation came from the web app (`FromWeb`: `Origin`, else `Referer`); otherwise it is
/// ignored and the request acts in the current team, as it did before team codes.
async fn named_team(parts: &mut Parts, app: &AppState) -> Option<String> {
    if let Some(value) = parts.headers.get(TEAM_HEADER) {
        return Some(value.to_str().unwrap_or_default().to_owned());
    }
    let code = Query::<NamedTeam>::try_from_uri(&parts.uri).ok()?.0.team?;
    (FromWeb::from_request_parts(parts, app).await.ok()).map(|_| code)
}

/// The team with the code or slug `named` (any case) and the user's role in it; `None` when
/// there is no such team or the user is not in it, so the two look the same to the caller.
async fn team_by_code(
    db: &DatabaseConnection,
    user_id: Uuid,
    named: &str,
) -> Result<Option<(teams::Model, Role)>, DbErr> {
    let named = named.trim().to_ascii_lowercase();
    if team::parse_code(&named).is_none() && team::slug(&named).is_err() {
        return Ok(None);
    }
    // A slug never equals a code (`set_slug`, `insert_team`); the code would win if one did.
    let sql = "SELECT * FROM teams WHERE code = $1 OR lower(slug) = $1 \
               ORDER BY (code = $1) DESC LIMIT 1";
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, [named.into()]);
    let found = teams::Entity::find().from_raw_sql(stmt).one(db).await?;
    let Some(found) = found else { return Ok(None) };
    let row = membership(db, found.id, user_id).await?;
    Ok(row.map(|row| (found, role_of(&row))))
}

/// The signed-in user inside a team; `401` without a session. The team is the one the request
/// names by code or slug (`X-Team` header or `?team=`), checked against the user's memberships: `404
/// not_found` when they are not in it or it does not exist. A request that names none acts in
/// the user's current team, so API tokens and older clients are unaffected.
pub struct InTeam {
    pub current: CurrentUser,
    pub team: teams::Model,
    pub role: Role,
}

impl FromRequestParts<AppState> for InTeam {
    type Rejection = Reply;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Reply> {
        let current = CurrentUser::from_request_parts(parts, app).await?;
        let failed = |e: DbErr| {
            tracing::error!(error = %e, "team lookup failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        };
        let (team, role) = match named_team(parts, app).await {
            Some(code) => team_by_code(&app.db, current.0.id, &code)
                .await
                .map_err(failed)?
                .ok_or_else(|| error(StatusCode::NOT_FOUND, "not_found"))?,
            None => current_team(&app.db, &current.0).await.map_err(failed)?,
        };
        Ok(Self {
            current,
            team,
            role,
        })
    }
}

/// A live invitation for `email` exists, so a sign-in link may create the account even when
/// registration is closed.
/// Like `live_invitation`, the inviter must still be an owner or admin of the team.
pub(crate) async fn invited(db: &DatabaseConnection, email: &str) -> Result<bool, DbErr> {
    let sql = "SELECT EXISTS (SELECT 1 FROM team_invitations i JOIN team_user m \
               ON m.team_id = i.team_id AND m.user_id = i.invited_by \
               WHERE i.email = $1 AND i.expires_at > now() \
               AND m.role IN ('owner', 'admin')) AS ok";
    let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, [email.into()]);
    match db.query_one_raw(stmt).await? {
        Some(row) => row.try_get("", "ok"),
        None => Ok(false),
    }
}

/// Creates a team with its default set; the caller becomes its owner and switches to it. At
/// most 10 a day per user.
#[utoipa::path(post, path = "/teams", tag = "teams", request_body = TeamName,
    responses((status = 201, body = Team), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error), (status = 429, body = Error)))]
async fn create(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    Body(body): Body<TeamName>,
) -> Response {
    let Some(name) = team::name(&body.name) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
    };
    let caps = [(format!("team:create:{}", user.id), TEAMS_PER_DAY)];
    let reserved = match daily_reserve(&app.db, &caps).await {
        Ok(Some(ids)) => ids,
        Ok(None) => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        Err(e) => return internal(e, "team cap"),
    };
    let made = async {
        let txn = app.db.begin().await?;
        let team = create_team(&txn, user.id, &name).await?;
        txn.commit().await.map(|_| team)
    };
    match made.await {
        Ok(team) => (StatusCode::CREATED, Json(team_json(&team, Role::Owner))).into_response(),
        Err(e) => {
            daily_refund(&app.db, &reserved).await;
            internal(e, "team insert")
        }
    }
}

/// The caller's teams, oldest membership first.
#[utoipa::path(get, path = "/teams", tag = "teams",
    responses((status = 200, body = [Team]), (status = 401, body = Error)))]
async fn list(State(app): State<AppState>, CurrentUser(user, _): CurrentUser) -> Response {
    if let Err(e) = current_team(&app.db, &user).await {
        return internal(e, "current team lookup");
    }
    let rows = team_user::Entity::find()
        .filter(team_user::Column::UserId.eq(user.id))
        .order_by_asc(team_user::Column::CreatedAt)
        .find_also_related(teams::Entity)
        .all(&app.db)
        .await;
    match rows {
        Ok(rows) => {
            let teams: Vec<Team> = rows
                .iter()
                .filter_map(|(m, t)| t.as_ref().map(|t| team_json(t, role_of(m))))
                .collect();
            Json(teams).into_response()
        }
        Err(e) => internal(e, "team list"),
    }
}

/// Switches the caller's current team to one they belong to.
#[utoipa::path(put, path = "/current-team", tag = "teams", request_body = SwitchTeam,
    responses((status = 200, body = Team), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn switch(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    Body(body): Body<SwitchTeam>,
) -> Response {
    let found = async {
        let Some(row) = membership(&app.db, body.team_id, user.id).await? else {
            return Ok(None);
        };
        let team = teams::Entity::find_by_id(row.team_id).one(&app.db).await?;
        set_current(&app.db, user.id, row.team_id).await?;
        Ok::<_, DbErr>(team.map(|t| team_json(&t, role_of(&row))))
    };
    match found.await {
        Ok(Some(team)) => Json(team).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "team switch"),
    }
}

/// The team the request names (see `InTeam`), else the caller's current team.
#[utoipa::path(get, path = "/team", tag = "teams",
    responses((status = 200, body = Team), (status = 401, body = Error)))]
async fn show(t: InTeam) -> Response {
    Json(team_json(&t.team, t.role)).into_response()
}

/// Renames the current team (`name`) or switches its public pages (`public_pages`) or their
/// newsletter sign-up (`newsletter`) or post approvals (`require_approval`), or sets or clears
/// its `slug` or who its newsletters are from (`newsletter_sender`, `422 invalid_sender` with
/// the bad `field`, `name`, `address` or `logo`; a change clears `tested_at` of the team's draft
/// newsletters). Owners and admins only. A bad slug is `422 invalid_slug` with a `reason`
/// (`length`, `characters` or `reserved`); one that is another team's slug or any team's code is
/// `409 slug_taken`; a team changes its slug at most 5 times a day (`429
/// slug_changed_too_often`). A slug given up (changed, cleared, or its team deleted) stops
/// naming the team at once and stays `409 slug_taken` for every other team for 30 days; the
/// team that had it may take it back any time.
#[utoipa::path(patch, path = "/team", tag = "teams", request_body = TeamChange,
    responses((status = 200, body = Team), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error),
        (status = 422, body = Error), (status = 429, body = Error)))]
async fn rename(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<TeamChange>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let mut row = teams::ActiveModel {
        id: Set(t.team.id),
        updated_at: Set(Utc::now().fixed_offset()),
        ..Default::default()
    };
    if let Some(name) = &body.name {
        let Some(name) = team::name(name) else {
            return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
        };
        row.name = Set(name);
    }
    if let Some(on) = body.public_pages {
        row.public_pages = Set(on);
    }
    if let Some(on) = body.newsletter {
        row.newsletter = Set(on);
    }
    if let Some(on) = body.require_approval {
        row.require_approval = Set(on);
    }
    let mut sender_changed = false;
    if let Some(from) = body.newsletter_sender {
        let from = match from.map(sender).transpose() {
            Ok(from) => from,
            Err(field) => {
                let body = serde_json::json!({"error": "invalid_sender", "field": field});
                return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
            }
        };
        let (name, address, logo) = match from {
            Some(s) => (Some(s.name), Some(s.address), s.logo),
            None => (None, None, None),
        };
        let team = &t.team;
        sender_changed = (&name, &address, &logo)
            != (
                &team.newsletter_sender_name,
                &team.newsletter_sender_address,
                &team.newsletter_sender_logo,
            );
        row.newsletter_sender_name = Set(name);
        row.newsletter_sender_address = Set(address);
        row.newsletter_sender_logo = Set(logo);
    }
    let slug = match body.slug.map(|s| s.as_deref().map(team::slug).transpose()) {
        Some(Err(reason)) => {
            let body = serde_json::json!({"error": "invalid_slug", "reason": reason});
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
        }
        // The slug the team already has is no change, and costs none of the day's five.
        Some(Ok(slug)) if slug != t.team.slug => Some(slug),
        _ => None,
    };
    let saved = async {
        let txn = app.db.begin().await?;
        if let Some(slug) = slug {
            let caps = [(format!("team:slug:{}", t.team.id), SLUG_CHANGES_PER_TEAM)];
            if reserve_in(&txn, &caps).await?.is_none() {
                return Ok(Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "slug_changed_too_often",
                )));
            }
            let raw = |sql: &str, values: [sea_orm::Value; 2]| {
                Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
            };
            let id = t.team.id;
            // Locked, so the slug held back below is the one this change replaces.
            let before = teams::Entity::find_by_id(id)
                .lock_exclusive()
                .one(&txn)
                .await?;
            if let Some(new) = &slug {
                let sql = "SELECT 1 FROM teams WHERE code = $1 OR (lower(slug) = $1 AND id <> $2) \
                    UNION ALL SELECT 1 FROM team_slug_holds WHERE slug = $1 AND team_id <> $2 \
                    AND released_at > now() - interval '30 days'";
                let taken = raw(sql, [new.clone().into(), id.into()]);
                if txn.query_one_raw(taken).await?.is_some() {
                    return Ok(Err((StatusCode::CONFLICT, "slug_taken")));
                }
                // Its own hold, or one that ran out.
                let sql = "DELETE FROM team_slug_holds WHERE slug = $1";
                let free =
                    Statement::from_sql_and_values(DbBackend::Postgres, sql, [new.clone().into()]);
                txn.execute_raw(free).await?;
            }
            if let Some(old) = before
                .and_then(|b| b.slug)
                .filter(|old| Some(old) != slug.as_ref())
            {
                txn.execute_raw(raw(HOLD, [old.into(), id.into()])).await?;
            }
            row.slug = Set(slug);
        }
        let team = teams::Entity::update(row).exec(&txn).await?;
        if sender_changed {
            // The test mail showed the old sender: drafts need a new one before they send.
            let sql = "UPDATE newsletters SET tested_at = NULL \
                       WHERE team_id = $1 AND status = 'draft'";
            let untest =
                Statement::from_sql_and_values(DbBackend::Postgres, sql, [t.team.id.into()]);
            txn.execute_raw(untest).await?;
        }
        txn.commit().await.map(|_| Ok(team))
    };
    match saved.await {
        Ok(Ok(team)) => Json(team_json(&team, t.role)).into_response(),
        Ok(Err((status, code))) => reply(status, code),
        // Two teams took one slug at once: the unique index let the first through.
        Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
            reply(StatusCode::CONFLICT, "slug_taken")
        }
        Err(e) => internal(e, "team rename"),
    }
}

/// Deletes the current team with its members, invitations and sets. Owner only, with a sign-in
/// from the last 10 minutes (else `403 reauth_required`). The caller's last team cannot go.
/// Any body is ignored.
#[utoipa::path(delete, path = "/team", tag = "teams",
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 409, body = Error)))]
async fn destroy(_: FromWeb, State(app): State<AppState>, t: InTeam) -> Response {
    if t.role != Role::Owner {
        return forbidden();
    }
    if !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let user_id = t.current.0.id;
    let deleted = async {
        let txn = app.db.begin().await?;
        // The team row lock orders this against role changes (see `change_member`), so the
        // owner check below cannot be stale.
        let locked = teams::Entity::find_by_id(t.team.id)
            .lock_exclusive()
            .one(&txn)
            .await?;
        let owner = membership(&txn, t.team.id, user_id).await?;
        if !owner.is_some_and(|m| role_of(&m) == Role::Owner) {
            return Ok(Some((StatusCode::FORBIDDEN, "forbidden")));
        }
        // Serializes this user's team deletes, so two at once cannot remove their last team.
        users::Entity::find_by_id(user_id)
            .lock_exclusive()
            .one(&txn)
            .await?;
        let teams = team_user::Entity::find()
            .filter(team_user::Column::UserId.eq(user_id))
            .count(&txn)
            .await?;
        if teams < 2 {
            return Ok(Some((StatusCode::CONFLICT, "last_team")));
        }
        // A deleted team's slug is held back like one it gave up: read under the lock, since a
        // change may have landed after this request loaded the team.
        if let Some(slug) = locked.and_then(|team| team.slug) {
            let values = [slug.into(), t.team.id.into()];
            let hold = Statement::from_sql_and_values(DbBackend::Postgres, HOLD, values);
            txn.execute_raw(hold).await?;
        }
        // API tokens go with the team through their cascading foreign key.
        teams::Entity::delete_by_id(t.team.id).exec(&txn).await?;
        txn.commit().await.map(|_| None)
    };
    match deleted.await {
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some((status, code))) => reply(status, code),
        Err(e) => internal(e, "team delete"),
    }
}

/// The current team's members, in the order they joined.
#[utoipa::path(get, path = "/team/members", tag = "teams",
    responses((status = 200, body = [Member]), (status = 401, body = Error)))]
async fn members(State(app): State<AppState>, t: InTeam) -> Response {
    let rows = team_user::Entity::find()
        .filter(team_user::Column::TeamId.eq(t.team.id))
        .order_by_asc(team_user::Column::CreatedAt)
        .find_also_related(users::Entity)
        .all(&app.db)
        .await;
    match rows {
        Ok(rows) => {
            let members: Vec<Member> = rows
                .iter()
                .filter_map(|(m, u)| u.as_ref().map(|u| member_json(m, u)))
                .collect();
            Json(members).into_response()
        }
        Err(e) => internal(e, "member list"),
    }
}

/// Outcome of a member change made under the team row lock.
enum Change {
    /// The member after a role change; `None` after a removal.
    Done(Option<Member>),
    NotFound,
    Forbidden,
    LastOwner,
}

/// Runs a role change (`to` is `Some`) or a removal (`None`) of `target` in `team_id`, by
/// `actor`. The team row is locked, so two changes at once cannot remove the last owner.
async fn change_member(
    db: &DatabaseConnection,
    team_id: Uuid,
    actor: Uuid,
    target: Uuid,
    to: Option<Role>,
) -> Result<Change, DbErr> {
    let txn = db.begin().await?;
    teams::Entity::find_by_id(team_id)
        .lock_exclusive()
        .one(&txn)
        .await?;
    let (Some(me), Some(them)) = (
        membership(&txn, team_id, actor).await?,
        membership(&txn, team_id, target).await?,
    ) else {
        return Ok(Change::NotFound);
    };
    let (mine, from) = (role_of(&me), role_of(&them));
    // The App Review demo user stays the owner of a team it owns, whoever asks (see `is_demo`).
    // In a team it was only invited to, it is a member like any other.
    if from == Role::Owner
        && to != Some(Role::Owner)
        && super::review_login::is_demo(&txn, target).await?
    {
        return Ok(Change::Forbidden);
    }
    let leaving = to.is_none() && actor == target;
    if !leaving && !(mine.manages() && mine.may_change(from, to)) {
        return Ok(Change::Forbidden);
    }
    if from == Role::Owner && to != Some(Role::Owner) {
        let owners = team_user::Entity::find()
            .filter(team_user::Column::TeamId.eq(team_id))
            .filter(team_user::Column::Role.eq(Role::Owner.as_str()))
            .count(&txn)
            .await?;
        if owners < 2 {
            return Ok(Change::LastOwner);
        }
    }
    // An invitation carries its inviter's rights: once they can no longer invite, theirs go.
    if to.is_none_or(|r| !r.manages()) {
        team_invitations::Entity::delete_many()
            .filter(team_invitations::Column::TeamId.eq(team_id))
            .filter(team_invitations::Column::InvitedBy.eq(target))
            .exec(&txn)
            .await?;
        // So does an API token they made: they may still hold its secret, and only someone
        // who manages the team may hold one.
        let revoke = "UPDATE api_tokens SET revoked_at = now() \
                      WHERE team_id = $1 AND created_by = $2 AND revoked_at IS NULL";
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            revoke,
            [team_id.into(), target.into()],
        ))
        .await?;
    }
    let done = match to {
        Some(role) => {
            let row = team_user::ActiveModel {
                team_id: Set(team_id),
                user_id: Set(target),
                role: Set(role.as_str().into()),
                ..Default::default()
            };
            let row = team_user::Entity::update(row).exec(&txn).await?;
            let user = users::Entity::find_by_id(target).one(&txn).await?;
            user.map(|u| member_json(&row, &u))
        }
        None => {
            // A stale current_team_id is harmless: `pick_team` skips teams the user left.
            team_user::Entity::delete_by_id((team_id, target))
                .exec(&txn)
                .await?;
            // Inbox items assigned to them are nobody's again. After the delete: an assignment
            // made meanwhile holds the membership row, so it is over and seen here.
            let clear = "UPDATE inbox_items SET assigned_to = NULL \
                         WHERE team_id = $1 AND assigned_to = $2";
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                clear,
                [team_id.into(), target.into()],
            ))
            .await?;
            None
        }
    };
    txn.commit().await?;
    Ok(Change::Done(done))
}

fn change_reply(change: Result<Change, DbErr>) -> Response {
    match change {
        Ok(Change::Done(Some(member))) => Json(member).into_response(),
        Ok(Change::Done(None)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Change::NotFound) => not_found(),
        Ok(Change::Forbidden) => forbidden(),
        Ok(Change::LastOwner) => reply(StatusCode::CONFLICT, "last_owner"),
        Err(e) => internal(e, "member change"),
    }
}

/// Changes a member's role. Owners and admins, with a sign-in from the last 10 minutes (else
/// `403 reauth_required`); only an owner grants or takes away `owner`, and the last owner
/// cannot be demoted.
#[utoipa::path(patch, path = "/team/members/{user_id}", tag = "teams", request_body = RoleChange,
    params(("user_id" = Uuid, Path)),
    responses((status = 200, body = Member), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 409, body = Error),
        (status = 422, body = Error)))]
async fn set_role(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(user_id): Path<Uuid>,
    Body(body): Body<RoleChange>,
) -> Response {
    let Some(role) = Role::parse(&body.role) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_role");
    };
    // A stolen old cookie must not hand out or take away rights.
    if t.role.manages() && !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    change_reply(change_member(&app.db, t.team.id, t.current.0.id, user_id, Some(role)).await)
}

/// Removes a member, or leaves the team when `user_id` is the caller. Removing someone else
/// needs owner or admin (only an owner removes an owner) and a sign-in from the last 10
/// minutes; the last owner cannot go.
#[utoipa::path(delete, path = "/team/members/{user_id}", tag = "teams",
    params(("user_id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error), (status = 409, body = Error)))]
async fn remove(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(user_id): Path<Uuid>,
) -> Response {
    // Leaving needs no fresh sign-in; removing someone else does.
    if user_id != t.current.0.id && t.role.manages() && !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    change_reply(change_member(&app.db, t.team.id, t.current.0.id, user_id, None).await)
}

/// Invites an email to the current team as `admin` or `member` and emails a link to
/// `{WEB_URL}/invitations/{token}`, valid 7 days, replacing a pending invitation of the same
/// email. Owners and admins; inviting an admin needs a sign-in from the last 10 minutes. At most 10 a minute and 50 a day per inviter, 50 a day per team,
/// 3 a day per email in one team and 20 a day per email overall.
#[utoipa::path(post, path = "/team/invitations", tag = "teams", request_body = NewInvitation,
    responses((status = 201, body = Invitation), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 502, body = Error)))]
async fn invite(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<NewInvitation>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let inviter = &t.current.0;
    if !app.invitation_limiter.hit(inviter.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let Some(email) = email::normalize(&body.email) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email");
    };
    let role = match Role::parse(&body.role) {
        Some(r @ (Role::Admin | Role::Member)) => r,
        _ => return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_role"),
    };
    if role == Role::Admin && !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let member = team_user::Entity::find()
        .filter(team_user::Column::TeamId.eq(t.team.id))
        .inner_join(users::Entity)
        .filter(users::Column::Email.eq(&email))
        .count(&app.db)
        .await;
    match member {
        Ok(0) => {}
        Ok(_) => return reply(StatusCode::CONFLICT, "already_member"),
        Err(e) => return internal(e, "member lookup"),
    }
    // Daily caps per address in this team, per address overall (a ceiling high enough that
    // throwaway teams cannot use it to block real invitations), per team and per inviter.
    let caps = [
        (
            format!("invite:team-email:{}:{email}", t.team.id),
            INVITES_PER_TEAM_EMAIL,
        ),
        (format!("invite:email:{email}"), INVITES_PER_EMAIL),
        (format!("invite:team:{}", t.team.id), INVITES_PER_TEAM),
        (format!("invite:user:{}", inviter.id), INVITES_PER_INVITER),
    ];
    let reserved = match daily_reserve(&app.db, &caps).await {
        Ok(Some(ids)) => ids,
        Ok(None) => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        Err(e) => return internal(e, "invitation cap"),
    };
    // Only a sent invitation counts against the caps.
    let res = send_invitation(&app, &t, email, role).await;
    if res.status() != StatusCode::CREATED {
        daily_refund(&app.db, &reserved).await;
    }
    res
}

/// Stores the invitation (replacing a pending one for the address) and emails its link; `201`
/// with the invitation, or the error reply.
async fn send_invitation(app: &AppState, t: &InTeam, email: String, role: Role) -> Response {
    let now = Utc::now().fixed_offset();
    let swept = team_invitations::Entity::delete_many()
        .filter(team_invitations::Column::ExpiresAt.lt(now))
        .exec(&app.db)
        .await;
    if let Err(e) = swept {
        return internal(e, "invitation cleanup");
    }
    let secret = token::new();
    let row = team_invitations::ActiveModel {
        id: Set(Uuid::now_v7()),
        team_id: Set(t.team.id),
        email: Set(email.clone()),
        role: Set(role.as_str().into()),
        token_hash: Set(token::hash(&secret)),
        invited_by: Set(Some(t.current.0.id)),
        expires_at: Set(now + Duration::days(INVITATION_DAYS)),
        ..Default::default()
    };
    // Re-inviting a pending address replaces its invitation, so the old link stops working.
    let replaced = async {
        let txn = app.db.begin().await?;
        if app.billing_enabled {
            // One invitation at a time per team, so two cannot both take the last seat.
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT 1 FROM teams WHERE id = $1 FOR NO KEY UPDATE",
                [t.team.id.into()],
            ))
            .await?;
            if crate::billing::members_full(true, &txn, t.team.id, Some(&email)).await? {
                return Err(DbErr::Custom(crate::billing::MEMBER_LIMIT.into()));
            }
        }
        team_invitations::Entity::delete_many()
            .filter(team_invitations::Column::TeamId.eq(t.team.id))
            .filter(team_invitations::Column::Email.eq(&email))
            .exec(&txn)
            .await?;
        let row = team_invitations::Entity::insert(row)
            .exec_with_returning(&txn)
            .await?;
        txn.commit().await.map(|_| row)
    };
    let row = match replaced.await {
        Ok(row) => row,
        Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
            return reply(StatusCode::CONFLICT, "already_invited")
        }
        Err(e) if crate::billing::is_member_limit(&e) => {
            return super::billing::plan_limit(crate::billing::Limit::Members)
        }
        Err(e) => return internal(e, "invitation insert"),
    };
    let url = format!("{}/invitations/{secret}", app.web_url);
    // Names are user input: quoted and made inert so they cannot pass as links, and kept out
    // of the subject.
    let text = format!(
        "{} invited you to join the team \"{}\" on SocialReins as {}.\n\nAccept the \
         invitation:\n\n{url}\n\nIt expires in {INVITATION_DAYS} days. If you did not expect it, \
         ignore this email.\n",
        team::inert(&t.current.0.name),
        team::inert(&t.team.name),
        role.as_str()
    );
    let subject = "You're invited to a team on SocialReins";
    if let Err(e) = app.mailer.send(&email, subject, text, &app.web_url).await {
        tracing::error!(error = %e, "invitation not sent");
        // The link was never delivered, so it should not stay usable.
        let undo = team_invitations::Entity::delete_by_id(row.id)
            .exec(&app.db)
            .await;
        if let Err(e) = undo {
            return internal(e, "invitation delete");
        }
        return reply(StatusCode::BAD_GATEWAY, "mail_failed");
    }
    (StatusCode::CREATED, Json(invitation_json(row))).into_response()
}

/// The current team's pending invitations, oldest first. Owners and admins.
#[utoipa::path(get, path = "/team/invitations", tag = "teams",
    responses((status = 200, body = [Invitation]), (status = 401, body = Error),
        (status = 403, body = Error)))]
async fn invitations(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let rows = team_invitations::Entity::find()
        .filter(team_invitations::Column::TeamId.eq(t.team.id))
        .filter(team_invitations::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
        .order_by_asc(team_invitations::Column::CreatedAt)
        .all(&app.db)
        .await;
    match rows {
        Ok(rows) => Json(rows.into_iter().map(invitation_json).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e, "invitation list"),
    }
}

/// Withdraws an invitation of the current team. Owners and admins.
#[utoipa::path(delete, path = "/team/invitations/{id}", tag = "teams",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn revoke(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Path(id): Path<Uuid>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let deleted = team_invitations::Entity::delete_many()
        .filter(team_invitations::Column::Id.eq(id))
        .filter(team_invitations::Column::TeamId.eq(t.team.id))
        .exec(&app.db)
        .await;
    match deleted {
        Ok(r) if r.rows_affected == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => not_found(),
        Err(e) => internal(e, "invitation delete"),
    }
}

/// The unexpired invitation behind `secret`, while its inviter is still an owner or admin of
/// the team. One whose inviter lost that right (or their account) is deleted and not returned.
async fn live_invitation(
    db: &DatabaseConnection,
    secret: &str,
) -> Result<Option<team_invitations::Model>, DbErr> {
    let Some(inv) = team_invitations::Entity::find()
        .filter(team_invitations::Column::TokenHash.eq(token::hash(secret)))
        .filter(team_invitations::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let inviter = match inv.invited_by {
        Some(id) => membership(db, inv.team_id, id).await?,
        None => None,
    };
    if inviter.is_some_and(|m| role_of(&m).manages()) {
        return Ok(Some(inv));
    }
    team_invitations::Entity::delete_by_id(inv.id)
        .exec(db)
        .await?;
    Ok(None)
}

/// Public: what the invitation link offers. `404` when the token is unknown, used or expired.
#[utoipa::path(get, path = "/invitations/{token}", tag = "teams",
    params(("token" = String, Path)),
    responses((status = 200, body = InvitationPreview), (status = 404, body = Error)))]
async fn preview(State(app): State<AppState>, Path(secret): Path<String>) -> Response {
    let found = async {
        let Some(inv) = live_invitation(&app.db, &secret).await? else {
            return Ok(None);
        };
        let Some(team) = teams::Entity::find_by_id(inv.team_id).one(&app.db).await? else {
            return Ok(None);
        };
        let inviter = match inv.invited_by {
            Some(id) => users::Entity::find_by_id(id).one(&app.db).await?,
            None => None,
        };
        Ok::<_, DbErr>(Some(InvitationPreview {
            team: team.name,
            email: inv.email,
            role: inv.role,
            invited_by: inviter.map(|u| u.name),
            expires_at: inv.expires_at,
        }))
    };
    match found.await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "invitation lookup"),
    }
}

/// Accepts an invitation for the signed-in user, whose email must be the invited one: they join
/// with its role (an existing member keeps theirs) and switch to the team. Works once.
#[utoipa::path(post, path = "/invitations/{token}", tag = "teams",
    params(("token" = String, Path)),
    responses((status = 200, body = Team), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn accept(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    Path(secret): Path<String>,
) -> Response {
    let inv = match live_invitation(&app.db, &secret).await {
        Ok(Some(inv)) => inv,
        Ok(None) => return not_found(),
        Err(e) => return internal(e, "invitation lookup"),
    };
    // Both are stored lowercased; a mismatch leaves the invitation for the right account.
    if inv.email != user.email.to_lowercase() {
        return reply(StatusCode::FORBIDDEN, "email_mismatch");
    }
    // The invitation holds its own seat, so only members count here. Someone already in the team
    // takes none.
    match membership(&app.db, inv.team_id, user.id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            match crate::billing::members_full(app.billing_enabled, &app.db, inv.team_id, None)
                .await
            {
                Ok(false) => {}
                Ok(true) => return super::billing::plan_limit(crate::billing::Limit::Members),
                Err(e) => return internal(e, "plan read"),
            }
        }
        Err(e) => return internal(e, "membership read"),
    }
    let joined = async {
        let txn = app.db.begin().await?;
        // Whoever deletes the row owns the invitation, so it is used once.
        let taken = team_invitations::Entity::delete_many()
            .filter(team_invitations::Column::Id.eq(inv.id))
            .filter(team_invitations::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
            .exec(&txn)
            .await?;
        if taken.rows_affected != 1 {
            return Ok(None);
        }
        let row = team_user::ActiveModel {
            team_id: Set(inv.team_id),
            user_id: Set(user.id),
            role: Set(inv.role.clone()),
            ..Default::default()
        };
        team_user::Entity::insert(row)
            .on_conflict_do_nothing_on([team_user::Column::TeamId, team_user::Column::UserId])
            .exec_without_returning(&txn)
            .await?;
        set_current(&txn, user.id, inv.team_id).await?;
        let member = membership(&txn, inv.team_id, user.id).await?;
        let team = teams::Entity::find_by_id(inv.team_id).one(&txn).await?;
        txn.commit().await?;
        Ok::<_, DbErr>(team.zip(member).map(|(t, m)| team_json(&t, role_of(&m))))
    };
    match joined.await {
        Ok(Some(team)) => Json(team).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e, "invitation accept"),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(switch))
        .routes(routes!(show, rename, destroy))
        .routes(routes!(members))
        .routes(routes!(set_role, remove))
        .routes(routes!(invite, invitations))
        .routes(routes!(revoke))
        .routes(routes!(preview, accept))
}
