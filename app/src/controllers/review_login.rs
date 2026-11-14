//! `GET /review-login/{token}`: signs an app-store reviewer (Meta App Review) into a fixed demo
//! account without email. Off unless `REVIEW_LOGIN_TOKEN` is set; unset or wrong token is the
//! same bare 404. The demo user is never an admin, and it signs in even when
//! `REGISTRATION_DISABLED` is on, because the operator configured it by setting the token.
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use sea_orm::{ActiveValue::Set, EntityTrait};
use socialreins_core::token;

use super::{
    auth::{find_user, internal, start_session},
    waitlist::{error, is_admin},
};
use crate::{models::users, rate_limit::ClientIp, state::AppState};

const DEMO_EMAIL: &str = "review@socialreins.com";
const DEMO_NAME: &str = "App Review Demo";

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found").into_response()
}

async fn review_login(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(secret): Path<String>,
) -> Response {
    let Some(want) = &app.review_login_token else {
        return not_found();
    };
    if !app.login_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    }
    // Hashing first makes the compared strings the same length whatever was sent.
    if !token::same(&token::hash(&secret), &token::hash(want)) {
        return not_found();
    }
    if is_admin(&app, DEMO_EMAIL) {
        tracing::warn!("review login refused: the demo email is in ADMIN_EMAILS");
        return not_found();
    }
    let user = match demo_user(&app).await {
        Ok(user) => user,
        Err(e) => return internal(e, "review user"),
    };
    // `start_session` gives the demo user its team, with the default social set.
    match start_session(&app, user.id).await {
        Ok(cookie) => (
            [
                (header::SET_COOKIE, cookie),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
            Redirect::to(&format!("{}/app", app.web_url)),
        )
            .into_response(),
        Err(e) => internal(e, "review session"),
    }
}

async fn demo_user(app: &AppState) -> Result<users::Model, sea_orm::DbErr> {
    if let Some(user) = find_user(app, DEMO_EMAIL).await? {
        return Ok(user);
    }
    let row = users::ActiveModel {
        id: Set(uuid::Uuid::now_v7()),
        email: Set(DEMO_EMAIL.into()),
        name: Set(DEMO_NAME.into()),
        ..Default::default()
    };
    // Two first calls at once: the second insert is a no-op.
    users::Entity::insert(row)
        .on_conflict_do_nothing_on([users::Column::Email])
        .exec_without_returning(&app.db)
        .await?;
    find_user(app, DEMO_EMAIL)
        .await?
        .ok_or_else(|| sea_orm::DbErr::RecordNotFound("demo user vanished after insert".into()))
}

/// Whether `user_id` is the demo user. `teams::change_member` refuses to change or remove it:
/// someone signed in through the link could otherwise make their own account an owner and take
/// the demo user out, and `close` would no longer find the team.
pub async fn is_demo(
    db: &impl sea_orm::ConnectionTrait,
    user_id: uuid::Uuid,
) -> Result<bool, sea_orm::DbErr> {
    let user = users::Entity::find_by_id(user_id).one(db).await?;
    Ok(user.is_some_and(|u| u.email == DEMO_EMAIL))
}

/// Run at start when `REVIEW_LOGIN_TOKEN` is unset: with the link switched off, what it handed
/// out goes too. A session from it lasts 30 days, and a passkey or API token made in one would
/// last for ever, so removing the variable alone would leave the demo team open to anyone who
/// once had the link.
pub async fn close(db: &sea_orm::DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    const DEMO: &str = "(SELECT id FROM users WHERE email = $1)";
    // Every team the demo user is in, whatever its role there: a token or invitation of theirs
    // is a way back in whoever made it, since anyone signed in as the demo user could have added
    // an admin first. The demo user cannot be taken out of a team (`is_demo`), so none is missed.
    let teams = format!("(SELECT team_id FROM team_user WHERE user_id IN {DEMO})");
    let bind =
        |sql: String| Statement::from_sql_and_values(DbBackend::Postgres, sql, [DEMO_EMAIL.into()]);
    for sql in [
        format!("DELETE FROM sessions WHERE user_id IN {DEMO}"),
        format!("DELETE FROM passkeys WHERE user_id IN {DEMO}"),
        format!(
            "UPDATE api_tokens SET revoked_at = now() \
             WHERE revoked_at IS NULL AND (created_by IN {DEMO} OR team_id IN {teams})"
        ),
        format!("DELETE FROM team_invitations WHERE invited_by IN {DEMO} OR team_id IN {teams}"),
    ] {
        db.execute_raw(bind(sql)).await?;
    }
    // Members are left alone: the operator may have joined the demo team to set it up. They
    // are told, because a member added through the link still has the team.
    let others = format!(
        "SELECT count(*) AS n FROM team_user WHERE team_id IN {teams} AND user_id NOT IN {DEMO}"
    );
    let n: i64 = match db.query_one_raw(bind(others)).await? {
        Some(row) => row.try_get("", "n")?,
        None => 0,
    };
    if n > 0 {
        tracing::warn!(
            members = n,
            "review login is off, but the demo team has other members: check its Team page"
        );
    }
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/review-login/{token}", get(review_login))
}
