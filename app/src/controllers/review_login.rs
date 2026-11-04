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

pub fn routes() -> Router<AppState> {
    Router::new().route("/review-login/{token}", get(review_login))
}
