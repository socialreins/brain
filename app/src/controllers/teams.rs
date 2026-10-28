//! Teams, members and invitations, for the signed-in user (session cookie).
//!
//! Every route acts inside the caller's own memberships; a team, member or invitation outside
//! them is `404 {"error":"not_found"}`. The current team is `users.current_team_id` when the user
//! still belongs to it, else their oldest membership. A user with no team gets a personal one
//! with its Default set, on sign-in or on first use.
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, SqlErr, TransactionTrait,
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
    password::reauthenticated,
    waitlist::{error, Error, Reply},
};
use crate::{
    models::{social_sets, team_invitations, team_user, teams, users},
    rate_limit::daily_allow,
    state::AppState,
};

const INVITATION_DAYS: i64 = 7;
/// Daily caps (`rate_limit::daily_allow`).
const TEAMS_PER_DAY: i64 = 10;
const INVITES_PER_EMAIL: i64 = 3;
const INVITES_PER_TEAM: i64 = 50;
const INVITES_PER_INVITER: i64 = 50;

#[derive(Serialize, ToSchema)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    /// The caller's role: `owner`, `admin` or `member`.
    pub role: String,
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

#[derive(Deserialize, ToSchema)]
pub struct DeleteTeam {
    /// The account password; not needed within 10 minutes of signing in.
    pub password: Option<String>,
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

fn team_json(team: &teams::Model, role: Role) -> Team {
    Team {
        id: team.id,
        name: team.name.clone(),
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

async fn membership(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    user_id: Uuid,
) -> Result<Option<team_user::Model>, DbErr> {
    team_user::Entity::find_by_id((team_id, user_id))
        .one(db)
        .await
}

async fn set_current(db: &impl ConnectionTrait, user_id: Uuid, team_id: Uuid) -> Result<(), DbErr> {
    let row = users::ActiveModel {
        id: Set(user_id),
        current_team_id: Set(Some(team_id)),
        ..Default::default()
    };
    users::Entity::update(row).exec(db).await.map(|_| ())
}

/// Makes a team with its Default set, `user_id` as owner, and switches the user to it.
async fn create_team(
    db: &impl ConnectionTrait,
    user_id: Uuid,
    name: &str,
) -> Result<teams::Model, DbErr> {
    let team = teams::ActiveModel {
        id: Set(Uuid::now_v7()),
        name: Set(name.to_owned()),
        ..Default::default()
    };
    let team = teams::Entity::insert(team).exec_with_returning(db).await?;
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
        name: Set("Default".into()),
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
        let name: String = format!("{}'s team", user.name)
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

/// The signed-in user inside their current team; `401` without a session.
pub struct InTeam {
    pub current: CurrentUser,
    pub team: teams::Model,
    pub role: Role,
}

impl FromRequestParts<AppState> for InTeam {
    type Rejection = Reply;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Reply> {
        let current = CurrentUser::from_request_parts(parts, app).await?;
        let (team, role) = current_team(&app.db, &current.0).await.map_err(|e| {
            tracing::error!(error = %e, "current team lookup failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        })?;
        Ok(Self {
            current,
            team,
            role,
        })
    }
}

/// A live invitation for `email` exists, so a sign-in link may create the account even when
/// registration is closed.
pub(crate) async fn invited(db: &DatabaseConnection, email: &str) -> Result<bool, DbErr> {
    let live = team_invitations::Entity::find()
        .filter(team_invitations::Column::Email.eq(email))
        .filter(team_invitations::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
        .count(db)
        .await?;
    Ok(live > 0)
}

/// Creates a team with its Default set; the caller becomes its owner and switches to it. At
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
    match daily_allow(&app.db, &caps).await {
        Ok(true) => {}
        Ok(false) => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        Err(e) => return internal(e, "team cap"),
    }
    let made = async {
        let txn = app.db.begin().await?;
        let team = create_team(&txn, user.id, &name).await?;
        txn.commit().await.map(|_| team)
    };
    match made.await {
        Ok(team) => (StatusCode::CREATED, Json(team_json(&team, Role::Owner))).into_response(),
        Err(e) => internal(e, "team insert"),
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

/// The caller's current team.
#[utoipa::path(get, path = "/team", tag = "teams",
    responses((status = 200, body = Team), (status = 401, body = Error)))]
async fn show(t: InTeam) -> Response {
    Json(team_json(&t.team, t.role)).into_response()
}

/// Renames the current team. Owners and admins only.
#[utoipa::path(patch, path = "/team", tag = "teams", request_body = TeamName,
    responses((status = 200, body = Team), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error)))]
async fn rename(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<TeamName>,
) -> Response {
    if !t.role.manages() {
        return forbidden();
    }
    let Some(name) = team::name(&body.name) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name");
    };
    let row = teams::ActiveModel {
        id: Set(t.team.id),
        name: Set(name),
        updated_at: Set(Utc::now().fixed_offset()),
        ..Default::default()
    };
    match teams::Entity::update(row).exec(&app.db).await {
        Ok(team) => Json(team_json(&team, t.role)).into_response(),
        Err(e) => internal(e, "team rename"),
    }
}

/// Deletes the current team with its members, invitations and sets. Owner only, with a sign-in
/// from the last 10 minutes or the account password. The caller's last team cannot go.
#[utoipa::path(delete, path = "/team", tag = "teams", request_body = DeleteTeam,
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 409, body = Error), (status = 429, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<DeleteTeam>,
) -> Response {
    if t.role != Role::Owner {
        return forbidden();
    }
    match reauthenticated(&app, &t.current, body.password).await {
        Some(true) => {}
        Some(false) => return reply(StatusCode::FORBIDDEN, "reauth_required"),
        None => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
    }
    let user_id = t.current.0.id;
    let deleted = async {
        let txn = app.db.begin().await?;
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
            return Ok(false);
        }
        // TODO tokens: API tokens are a polymorphic relation with no cascading foreign key, so
        // delete this team's tokens here explicitly once they exist.
        teams::Entity::delete_by_id(t.team.id).exec(&txn).await?;
        txn.commit().await.map(|_| true)
    };
    match deleted.await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => reply(StatusCode::CONFLICT, "last_team"),
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

/// Changes a member's role. Owners and admins; only an owner grants or takes away `owner`, and
/// the last owner cannot be demoted.
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
    change_reply(change_member(&app.db, t.team.id, t.current.0.id, user_id, Some(role)).await)
}

/// Removes a member, or leaves the team when `user_id` is the caller. Removing someone else
/// needs owner or admin (only an owner removes an owner); the last owner cannot go.
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
    change_reply(change_member(&app.db, t.team.id, t.current.0.id, user_id, None).await)
}

/// Invites an email to the current team as `admin` or `member` and emails a link to
/// `{WEB_URL}/invitations/{token}`, valid 7 days, replacing a pending invitation of the same
/// email. Owners and admins. At most 10 a minute and 50 a day per inviter, 50 a day per team,
/// and 3 a day per email across all teams.
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
    // Daily caps per recipient (across all teams), per team and per inviter, so neither many
    // teams nor many inviters can flood one inbox.
    let caps = [
        (format!("invite:email:{email}"), INVITES_PER_EMAIL),
        (format!("invite:team:{}", t.team.id), INVITES_PER_TEAM),
        (format!("invite:user:{}", inviter.id), INVITES_PER_INVITER),
    ];
    match daily_allow(&app.db, &caps).await {
        Ok(true) => {}
        Ok(false) => return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        Err(e) => return internal(e, "invitation cap"),
    }
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
        invited_by: Set(Some(inviter.id)),
        expires_at: Set(now + Duration::days(INVITATION_DAYS)),
        ..Default::default()
    };
    // Re-inviting a pending address replaces its invitation, so the old link stops working.
    let replaced = async {
        let txn = app.db.begin().await?;
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
        Err(e) => return internal(e, "invitation insert"),
    };
    let url = format!("{}/invitations/{secret}", app.web_url);
    // Names are user input: quoted and made inert so they cannot pass as links, and kept out
    // of the subject.
    let text = format!(
        "{} invited you to join the team \"{}\" on SocialReins as {}.\n\nOpen this link to \
         accept:\n\n{url}\n\nIt expires in {INVITATION_DAYS} days. If you did not expect it, \
         ignore this email.\n",
        team::inert(&inviter.name),
        team::inert(&t.team.name),
        role.as_str()
    );
    let subject = "You're invited to a team on SocialReins";
    if let Err(e) = app.mailer.send(&email, subject, text).await {
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
