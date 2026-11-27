//! The X credit sweep: credits a failed target spent and never got back return once, and
//! credits of a post that may have gone out, a published one, or a recent spend do not. Needs
//! `DATABASE_URL` like the request tests.
use sea_orm::ConnectionTrait;
use serial_test::serial;
use socialreins_brain::{
    billing::{self, Kind},
    config::Config,
    connect,
    connectors::unsent,
    state::AppState,
    x_credit_sweep::sweep,
};
use uuid::Uuid;

struct Env {
    app: AppState,
    team: Uuid,
    set: Uuid,
}

async fn env(billing_on: bool) -> Env {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.billing_enabled = billing_on;
    let db = connect(&config).await.expect("database reachable");
    let (team, set) = (Uuid::now_v7(), Uuid::now_v7());
    let code: String = team
        .as_u128()
        .to_string()
        .bytes()
        .rev()
        .take(6)
        .map(|d| (b'p' + d % 10) as char)
        .collect();
    db.execute_unprepared(&format!(
        "INSERT INTO teams (id, name, code) VALUES ('{team}', 't', '{code}'); \
         INSERT INTO social_sets (id, team_id, name) VALUES ('{set}', '{team}', 's')"
    ))
    .await
    .unwrap();
    Env {
        app: AppState::new(db, &config),
        team,
        set,
    }
}

impl Env {
    /// A target in `status` with `error` and `remote_id`, and the spend of `credits` it made
    /// `age` ago (unreleased). Returns the target's spend reference.
    async fn spent(&self, status: &str, error: Option<&str>, remote: bool, age: &str) -> String {
        let (post, target) = (Uuid::now_v7(), Uuid::now_v7());
        let (error, remote) = (
            error.map_or("NULL".to_owned(), |e| {
                format!("'{}'", e.replace('\'', "''"))
            }),
            if remote { "'1234'" } else { "NULL" },
        );
        let reference = format!("{target}:{}", Uuid::now_v7());
        self.app
            .db
            .execute_unprepared(&format!(
                "INSERT INTO posts (id, team_id, social_set_id, platform, status) \
                 VALUES ('{post}', '{}', '{}', 'x', 'failed'); \
                 INSERT INTO post_targets (id, post_id, status, error, remote_id, updated_at) \
                 VALUES ('{target}', '{post}', '{status}', {error}, {remote}, \
                 now() - interval '{age}' + interval '1 minute'); \
                 INSERT INTO credit_ledger (id, team_id, kind, amount, reason, ref, created_at) \
                 VALUES (gen_random_uuid(), '{}', 'x', -2, 'x post', '{reference}', \
                 now() - interval '{age}')",
                self.team, self.set, self.team
            ))
            .await
            .unwrap();
        reference
    }

    async fn credits(&self) -> i64 {
        billing::balance(&self.app.db, self.team, Kind::X)
            .await
            .unwrap()
    }
}

#[tokio::test]
#[serial]
async fn only_credits_of_posts_that_certainly_sent_nothing_come_back_and_only_once() {
    let e = env(true).await;
    let clean = "X refused it (403). Nothing was posted.";
    let never = unsent("Publish: X could not be reached.".into(), true);
    let cases = [
        // (status, error, has a remote id, spent how long ago, comes back)
        ("failed", Some(clean), false, "3 hours", true),
        ("failed", Some(never.as_str()), false, "5 days", true),
        (
            "failed",
            Some("Timed out; it may have posted, check the account."),
            false,
            "3 hours",
            false,
        ),
        (
            "failed",
            Some("Publish: X could not be reached."),
            false,
            "3 hours",
            false,
        ),
        ("failed", None, false, "3 hours", false),
        ("failed", Some(clean), true, "3 hours", false),
        ("published", None, true, "3 hours", false),
        ("pending", Some(clean), false, "3 hours", false),
        ("failed", Some(clean), false, "30 minutes", false),
        ("failed", Some(clean), false, "10 days", false),
    ];
    let mut back = 0;
    for (status, error, remote, age, returns) in cases {
        e.spent(status, error, remote, age).await;
        back += if returns { 2 } else { 0 };
    }
    let owed = e.credits().await;
    assert_eq!(owed, -2 * cases.len() as i64);
    // Another test's rows are none of ours: only this team's balance is looked at.
    sweep(&e.app).await.unwrap();
    assert_eq!(e.credits().await, owed + back);
    // A second run, and a second instance racing it, give nothing more.
    let (a, b) = tokio::join!(sweep(&e.app), sweep(&e.app));
    a.unwrap();
    b.unwrap();
    assert_eq!(e.credits().await, owed + back);
}

/// A spend whose credits stayed spent (`release_x` marked it kept) is never taken for a lost
/// release, whatever a later attempt of the same target says; and only a target's latest spend
/// is judged.
#[tokio::test]
#[serial]
async fn a_kept_spend_and_an_older_attempt_are_left_alone() {
    let e = env(true).await;
    let clean = "X refused it (403). Nothing was posted.";
    let kept = e.spent("failed", Some(clean), false, "3 hours").await;
    e.app
        .db
        .execute_unprepared(&format!(
            "UPDATE credit_ledger SET reason = 'x post kept' WHERE ref = '{kept}'"
        ))
        .await
        .unwrap();
    // An older attempt of a target that has a newer spend.
    let old = e.spent("failed", Some(clean), false, "5 hours").await;
    let target = &old[..36];
    e.app
        .db
        .execute_unprepared(&format!(
            "INSERT INTO credit_ledger (id, team_id, kind, amount, reason, ref, created_at) \
             VALUES (gen_random_uuid(), '{}', 'x', -1, 'x post kept', '{target}:{}', now() - interval '4 hours')",
            e.team,
            Uuid::now_v7()
        ))
        .await
        .unwrap();
    // A target touched an hour after its spend (retried, then failed before spending) is not
    // judged: its error may be another attempt's.
    let later = e.spent("failed", Some(clean), false, "3 hours").await;
    e.app
        .db
        .execute_unprepared(&format!(
            "UPDATE post_targets SET updated_at = now() - interval '2 hours' WHERE id::text = '{}'",
            &later[..36]
        ))
        .await
        .unwrap();
    let before = e.credits().await;
    sweep(&e.app).await.unwrap();
    assert_eq!(e.credits().await, before);
}

#[tokio::test]
#[serial]
async fn credits_already_given_back_are_not_given_twice() {
    let e = env(true).await;
    let reference = e
        .spent(
            "failed",
            Some("X refused it (403). Nothing was posted."),
            false,
            "3 hours",
        )
        .await;
    billing::grant(
        &e.app.db,
        e.team,
        Kind::X,
        2,
        "x post unsent",
        Some(&reference),
    )
    .await
    .unwrap();
    let before = e.credits().await;
    sweep(&e.app).await.unwrap();
    assert_eq!(e.credits().await, before);
}

#[tokio::test]
#[serial]
async fn with_billing_off_nothing_is_touched() {
    let e = env(false).await;
    e.spent(
        "failed",
        Some("X refused it (403). Nothing was posted."),
        false,
        "3 hours",
    )
    .await;
    assert_eq!(sweep(&e.app).await.unwrap(), (0, 0));
    assert_eq!(e.credits().await, -2);
}
