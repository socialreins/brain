//! `GET /review-login/{token}` against a real Postgres at `DATABASE_URL` (see README).
use axum_test::TestServer;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};
use serde_json::{json, Value};
use serial_test::serial;
use socialreins_brain::{
    config::Config,
    connect, controllers,
    models::{sessions, social_sets, team_invitations, team_user, users},
    state::AppState,
};

const EMAIL: &str = "review@socialreins.com";
const TOKEN: &str = "review-token-for-tests";

async fn app(edit: impl FnOnce(&mut Config)) -> (TestServer, DatabaseConnection) {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    edit(&mut config);
    let db = connect(&config).await.expect("database reachable");
    let app = controllers::router(AppState::new(db.clone(), &config), &config);
    (TestServer::new(app).unwrap(), db)
}

async fn enabled() -> (TestServer, DatabaseConnection) {
    app(|c| c.review_login_token = Some(TOKEN.into())).await
}

async fn demo(db: &DatabaseConnection) -> Option<users::Model> {
    users::Entity::find()
        .filter(users::Column::Email.eq(EMAIL))
        .one(db)
        .await
        .unwrap()
}

/// Sessions the demo user holds, 0 while it does not exist.
async fn demo_sessions(db: &DatabaseConnection) -> u64 {
    let Some(user) = demo(db).await else {
        return 0;
    };
    sessions::Entity::find()
        .filter(sessions::Column::UserId.eq(user.id))
        .count(db)
        .await
        .unwrap()
}

#[tokio::test]
#[serial]
async fn unset_or_wrong_token_is_a_plain_404_with_no_session() {
    let (off, _) = app(|c| c.review_login_token = None).await;
    let (on, _) = enabled().await;
    for res in [
        off.get(&format!("/review-login/{TOKEN}")).await,
        on.get("/review-login/wrong").await,
        on.get(&format!("/review-login/{TOKEN}x")).await,
    ] {
        assert_eq!(res.status_code(), 404);
        res.assert_json(&json!({"error": "not_found"}));
        assert!(res.maybe_header("set-cookie").is_none());
    }
}

#[tokio::test]
#[serial]
async fn right_token_signs_in_as_the_demo_user_and_reuses_it() {
    let (open, db) = enabled().await;
    // Sign-up closed: the operator-configured demo account still works.
    let (closed, _) = app(|c| {
        c.review_login_token = Some(TOKEN.into());
        c.registration_disabled = true;
        c.owner_email = Some("someone-else@example.com".into());
    })
    .await;
    let mut teams = vec![];
    for server in [&open, &closed] {
        let res = server.get(&format!("/review-login/{TOKEN}")).await;
        assert_eq!(res.status_code(), 303);
        assert_eq!(res.header("location"), "http://localhost:3000/app");
        let cookie = res.header("set-cookie").to_str().unwrap().to_owned();
        assert!(cookie.starts_with("sr_session=") && cookie.contains("HttpOnly"));
        let me = server
            .get("/auth/me")
            .add_header("cookie", cookie.split(';').next().unwrap())
            .await;
        me.assert_status_ok();
        let me = me.json::<Value>();
        assert_eq!(me["email"], EMAIL);
        assert_eq!(me["name"], "App Review Demo");
        assert_eq!(me["is_admin"], false);
        assert_eq!(me["current_team"]["role"], "owner");
        teams.push(me["current_team"]["id"].clone());
    }
    assert_eq!(teams[0], teams[1], "same team on the second sign-in");
    assert!(demo_sessions(&db).await >= 2);
    let user = demo(&db).await.unwrap();
    let memberships = team_user::Entity::find()
        .filter(team_user::Column::UserId.eq(user.id))
        .count(&db)
        .await
        .unwrap();
    assert_eq!(memberships, 1);
    let team_id: uuid::Uuid = teams[0].as_str().unwrap().parse().unwrap();
    let sets = social_sets::Entity::find()
        .filter(social_sets::Column::TeamId.eq(team_id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(sets.len(), 1);
    assert!(sets[0].is_default);
}

/// Taking `REVIEW_LOGIN_TOKEN` away must end what the link handed out, or anyone who once had
/// it keeps the demo team through a session, or an API token made in one.
#[tokio::test]
#[serial]
async fn switching_the_link_off_ends_its_sessions_and_tokens() {
    let (s, db) = enabled().await;
    let res = s.get(&format!("/review-login/{TOKEN}")).await;
    let cookie = res.header("set-cookie").to_str().unwrap().to_owned();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let made = s
        .post("/v1/api-tokens")
        .add_header("cookie", &cookie)
        .add_header("origin", "http://localhost:3000")
        .json(&json!({"name": "kept by a reviewer"}))
        .await;
    assert_eq!(made.status_code(), 201, "{}", made.text());
    let secret = made.json::<Value>()["secret"].as_str().unwrap().to_owned();
    let with_token = || {
        s.get("/v1/posts")
            .add_header("authorization", format!("Bearer {secret}"))
    };
    with_token().await.assert_status_ok();
    // An invitation sent from the demo account would let its holder join the team later.
    let invited = s
        .post("/v1/team/invitations")
        .add_header("cookie", &cookie)
        .add_header("origin", "http://localhost:3000")
        .json(&json!({"email": format!("{}@example.com", uuid::Uuid::now_v7()), "role": "admin"}))
        .await;
    assert_eq!(invited.status_code(), 201, "{}", invited.text());
    let user_id = demo(&db).await.unwrap().id;
    let invitations = || async {
        team_invitations::Entity::find()
            .filter(team_invitations::Column::InvitedBy.eq(user_id))
            .count(&db)
            .await
            .unwrap()
    };
    assert_eq!(invitations().await, 1);

    // A second team, where the demo user is only an admin and someone else made the token and
    // the invitation: its role there and who made them must not matter.
    use sea_orm::ConnectionTrait;
    db.execute_unprepared("DELETE FROM rate_limit_hits WHERE bucket LIKE 'team:create:%'")
        .await
        .unwrap();
    let by_demo = |path: &str, body: Value| {
        s.post(path)
            .add_header("cookie", &cookie)
            .add_header("origin", "http://localhost:3000")
            .json(&body)
    };
    let second = by_demo("/v1/teams", json!({"name": "Second"})).await;
    assert_eq!(second.status_code(), 201, "{}", second.text());
    let second = second.json::<Value>()["id"].as_str().unwrap().to_owned();
    let made = by_demo("/v1/api-tokens", json!({"name": "second team"})).await;
    assert_eq!(made.status_code(), 201, "{}", made.text());
    let other_secret = made.json::<Value>()["secret"].as_str().unwrap().to_owned();
    let invite = json!({"email": format!("{}@example.com", uuid::Uuid::now_v7()), "role": "admin"});
    let invited = by_demo("/v1/team/invitations", invite).await;
    assert_eq!(invited.status_code(), 201, "{}", invited.text());
    let other = uuid::Uuid::now_v7();
    db.execute_unprepared(&format!(
        "INSERT INTO users (id, email, name) VALUES ('{other}', '{other}@example.com', 'Other'); \
         INSERT INTO team_user (team_id, user_id, role) VALUES ('{second}', '{other}', 'owner'); \
         UPDATE team_user SET role = 'admin' WHERE team_id = '{second}' AND user_id = '{user_id}'; \
         UPDATE api_tokens SET created_by = '{other}' WHERE team_id = '{second}'; \
         UPDATE team_invitations SET invited_by = '{other}' WHERE team_id = '{second}'"
    ))
    .await
    .unwrap();
    let second_id: uuid::Uuid = second.parse().unwrap();
    let second_invitations = || async {
        team_invitations::Entity::find()
            .filter(team_invitations::Column::TeamId.eq(second_id))
            .count(&db)
            .await
            .unwrap()
    };
    assert_eq!(second_invitations().await, 1);
    let with_other = || {
        s.get("/v1/posts")
            .add_header("authorization", format!("Bearer {other_secret}"))
    };
    with_other().await.assert_status_ok();

    controllers::review_login::close(&db).await.unwrap();

    assert_eq!(second_invitations().await, 0);
    assert_eq!(with_other().await.status_code(), 401);
    db.execute_unprepared(&format!(
        "DELETE FROM teams WHERE id = '{second}'; DELETE FROM users WHERE id = '{other}'"
    ))
    .await
    .unwrap();
    assert_eq!(invitations().await, 0);
    assert_eq!(demo_sessions(&db).await, 0);
    let me = s.get("/auth/me").add_header("cookie", &cookie).await;
    assert_eq!(me.status_code(), 401);
    assert_eq!(with_token().await.status_code(), 401);
    assert!(demo(&db).await.is_some(), "the account and its team stay");
}

/// Someone signed in through the link must not be able to hand the demo team to their own
/// account and take the demo user out of it: `close` finds a team through the demo user.
#[tokio::test]
#[serial]
async fn the_demo_user_cannot_be_demoted_or_removed_from_its_team() {
    use sea_orm::ConnectionTrait;
    let (s, db) = enabled().await;
    let res = s.get(&format!("/review-login/{TOKEN}")).await;
    let cookie = res.header("set-cookie").to_str().unwrap().to_owned();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let by_demo = |method: axum::http::Method, path: String| {
        s.method(method, &path)
            .add_header("cookie", &cookie)
            .add_header("origin", "http://localhost:3000")
    };
    let me = by_demo(axum::http::Method::GET, "/auth/me".into()).await;
    let team = me.json::<Value>()["current_team"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let demo_id = demo(&db).await.unwrap().id;
    // The reviewer's own account joins as an admin, and the demo user makes it an owner.
    let other = uuid::Uuid::now_v7();
    db.execute_unprepared(&format!(
        "INSERT INTO users (id, email, name) VALUES ('{other}', '{other}@example.com', 'Other'); \
         INSERT INTO team_user (team_id, user_id, role) VALUES ('{team}', '{other}', 'admin')"
    ))
    .await
    .unwrap();
    let promoted = by_demo(
        axum::http::Method::PATCH,
        format!("/v1/team/members/{other}"),
    )
    .json(&json!({"role": "owner"}))
    .await;
    assert_eq!(promoted.status_code(), 200, "{}", promoted.text());

    // With a second owner the last-owner rule no longer protects the demo user: this one does.
    let path = format!("/v1/team/members/{demo_id}");
    for role in ["admin", "member"] {
        let demoted = by_demo(axum::http::Method::PATCH, path.clone())
            .json(&json!({"role": role}))
            .await;
        assert_eq!(demoted.status_code(), 403, "{role}");
    }
    let removed = by_demo(axum::http::Method::DELETE, path).await;
    assert_eq!(removed.status_code(), 403);
    let role: Option<String> = team_user::Entity::find()
        .filter(team_user::Column::UserId.eq(demo_id))
        .one(&db)
        .await
        .unwrap()
        .map(|m| m.role);
    assert_eq!(role.as_deref(), Some("owner"));

    // In a team it was only invited to, the demo user is a member like any other and may go.
    let guest = uuid::Uuid::now_v7();
    db.execute_unprepared(&format!(
        "INSERT INTO teams (id, name) VALUES ('{guest}', 'Guest'); \
         INSERT INTO team_user (team_id, user_id, role) VALUES ('{guest}', '{other}', 'owner'); \
         INSERT INTO team_user (team_id, user_id, role) VALUES ('{guest}', '{demo_id}', 'admin')"
    ))
    .await
    .unwrap();
    let switched = by_demo(axum::http::Method::PUT, "/v1/current-team".into())
        .json(&json!({"team_id": guest}))
        .await;
    assert!(switched.status_code().is_success(), "{}", switched.text());
    let left = by_demo(
        axum::http::Method::DELETE,
        format!("/v1/team/members/{demo_id}"),
    )
    .await;
    assert_eq!(left.status_code(), 204, "{}", left.text());
    db.execute_unprepared(&format!(
        "DELETE FROM teams WHERE id = '{guest}'; DELETE FROM users WHERE id = '{other}'"
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn refuses_when_the_demo_email_is_an_admin() {
    let (s, db) = app(|c| {
        c.review_login_token = Some(TOKEN.into());
        c.admin_emails = vec![EMAIL.into()];
    })
    .await;
    let before = demo_sessions(&db).await;
    let res = s.get(&format!("/review-login/{TOKEN}")).await;
    assert_eq!(res.status_code(), 404);
    assert!(res.maybe_header("set-cookie").is_none());
    assert_eq!(demo_sessions(&db).await, before, "no session was created");
}
