//! The run log: why runs were skipped, counted, and a retry that can never send twice.
use serde_json::{json, Value};
use socialreins_brain::automations;
use std::sync::atomic::Ordering;

use serial_test::serial;

use super::support::*;

async fn tick(e: &Env) {
    automations::resume_due(&e.state, std::time::Duration::from_secs(5))
        .await
        .unwrap();
}

async fn due(e: &Env, item: uuid::Uuid) {
    e.sql(&format!(
        "UPDATE automation_runs SET resume_at = now() - interval '1 second' \
         WHERE inbox_item_id = '{item}' AND status = 'waiting'"
    ))
    .await;
}

async fn runs(e: &Env, automation: &str, query: &str) -> Value {
    e.s.get(&format!("/v1/automations/{automation}/runs{query}"))
        .add_header("cookie", e.cookie.clone())
        .await
        .json()
}

fn retry_path(automation: &str, run: &str) -> String {
    format!("/v1/automations/{automation}/runs/{run}/retry")
}

#[tokio::test]
#[serial]
async fn skipped_runs_are_counted_by_reason_and_can_be_listed() {
    let e = env().await;
    let a = e.automation(reply_all(json!({"daily_cap": 1}))).await;
    e.item("comment", &digits(), "one", 0).await;
    e.item("comment", &digits(), "two", 0).await;
    e.item("comment", &digits(), "three", 0).await;
    let got =
        e.s.get(&format!("/v1/automations/{a}"))
            .add_header("cookie", e.cookie.clone())
            .await
            .json::<Value>();
    assert_eq!(got["runs"]["sent"], 1);
    assert_eq!(
        got["runs"]["reasons"],
        json!([{"reason": "automation_cap", "count": 2}])
    );
    let skipped = runs(&e, &a, "?reason=automation_cap").await;
    assert_eq!(skipped["runs"].as_array().unwrap().len(), 2);
    let sent = runs(&e, &a, "?status=sent").await;
    assert_eq!(sent["runs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn a_rate_limited_action_list_is_retried_by_a_person_once() {
    let e = env().await;
    let a = e.automation(reply_all(json!({}))).await;
    e.mock.mode.store(1, Ordering::SeqCst);
    let item = e.item("comment", &digits(), "hi", 0).await;
    let page = runs(&e, &a, "").await;
    let run = &page["runs"][0];
    assert_eq!(
        (run["status"].clone(), run["reason"].clone()),
        (json!("skipped"), json!("rate_limited"))
    );
    assert_eq!(run["can_retry"], true);
    let id = run["id"].as_str().unwrap().to_owned();
    // Still rate limited: it stays retryable, nothing went out.
    let res = e.post(&retry_path(&a, &id), json!({})).await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    assert_eq!(res.json::<Value>()["status"], "skipped");
    e.mock.mode.store(0, Ordering::SeqCst);
    let res = e.post(&retry_path(&a, &id), json!({})).await;
    assert_eq!(res.json::<Value>()["status"], "sent");
    assert_eq!(res.json::<Value>()["retries"], 2);
    // A second click, or a double click, cannot send again.
    let res = e.post(&retry_path(&a, &id), json!({})).await;
    assert_eq!(res.status_code(), 409);
    assert_eq!(res.json::<Value>()["error"], "not_retryable");
    let ok_posts = e.mock.to("/comments");
    // Three attempts reached the mock (two refused, one taken); exactly one reply was taken.
    assert_eq!(ok_posts.len(), 3);
    assert_eq!(e.run(&a, item.id).await.unwrap().0, "sent");
}

#[tokio::test]
#[serial]
async fn a_failure_that_may_have_gone_out_is_never_retried() {
    let e = env().await;
    let a = e.automation(reply_all(json!({}))).await;
    // Meta refuses with a policy code: it may or may not have acted, so it is final.
    e.mock.mode.store(2, Ordering::SeqCst);
    e.item("comment", &digits(), "hi", 0).await;
    let page = runs(&e, &a, "").await;
    let run = &page["runs"][0];
    assert_eq!(run["status"], "failed");
    assert_eq!(run["can_retry"], false);
    e.mock.mode.store(0, Ordering::SeqCst);
    let id = run["id"].as_str().unwrap();
    let res = e.post(&retry_path(&a, id), json!({})).await;
    assert_eq!(res.status_code(), 409);
    assert_eq!(e.mock.sent(), 1);
}

#[tokio::test]
#[serial]
async fn a_flow_tries_a_rate_limited_step_again_by_itself_and_never_twice() {
    let e = env().await;
    let a = e
        .automation(json!({"name": "Flow", "trigger": "comment_received",
            "steps": [{"type": "reply_publicly", "text": "one"},
                      {"type": "wait", "minutes": 1},
                      {"type": "reply_publicly", "text": "two"}]}))
        .await;
    let item = e.item("comment", &digits(), "hi", 0).await;
    assert_eq!(e.mock.to("/comments").len(), 1);
    e.mock.mode.store(1, Ordering::SeqCst);
    due(&e, item.id).await;
    tick(&e).await;
    // Refused: parked to try again, not ended.
    assert_eq!(e.run(&a, item.id).await.unwrap().0, "waiting");
    e.mock.mode.store(0, Ordering::SeqCst);
    due(&e, item.id).await;
    tick(&e).await;
    assert_eq!(e.run(&a, item.id).await.unwrap().0, "sent");
    // One taken reply for each step, and the refused attempt is the only extra call.
    assert_eq!(e.mock.to("/comments").len(), 3);
    let got = runs(&e, &a, "").await;
    assert_eq!(got["runs"][0]["retries"], 1);
    assert_eq!(got["runs"][0]["results"].as_array().unwrap().len(), 2);
}

#[tokio::test]
#[serial]
async fn a_flow_gives_up_after_three_tries_and_a_person_can_go_on() {
    let e = env().await;
    let a = e
        .automation(json!({"name": "Flow", "trigger": "comment_received",
            "steps": [{"type": "reply_publicly", "text": "one"}]}))
        .await;
    e.mock.mode.store(1, Ordering::SeqCst);
    let item = e.item("comment", &digits(), "hi", 0).await;
    for _ in 0..3 {
        assert_eq!(e.run(&a, item.id).await.unwrap().0, "waiting");
        due(&e, item.id).await;
        tick(&e).await;
    }
    // The fourth refusal is final.
    let (status, reason) = e.run(&a, item.id).await.unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("rate_limited"))
    );
    // A person retries it once Meta lets them through: it goes on at the next round.
    e.mock.mode.store(0, Ordering::SeqCst);
    let id = runs(&e, &a, "").await["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let res = e.post(&retry_path(&a, &id), json!({})).await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    assert_eq!(res.json::<Value>()["status"], "waiting");
    tick(&e).await;
    assert_eq!(e.run(&a, item.id).await.unwrap().0, "sent");
    assert_eq!(e.mock.to("/comments").len(), 5, "four refused, one taken");
}

#[tokio::test]
#[serial]
async fn a_retry_waits_for_the_team_to_resume_and_for_the_automation_to_be_on() {
    let e = env().await;
    let a = e.automation(reply_all(json!({}))).await;
    e.mock.mode.store(1, Ordering::SeqCst);
    e.item("comment", &digits(), "hi", 0).await;
    let id = runs(&e, &a, "").await["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    e.mock.mode.store(0, Ordering::SeqCst);
    e.patch("/v1/team", json!({"automations_paused": true}))
        .await;
    let res = e.post(&retry_path(&a, &id), json!({})).await;
    assert_eq!(
        (
            res.status_code().as_u16(),
            res.json::<Value>()["error"].clone()
        ),
        (409, json!("paused"))
    );
    e.patch("/v1/team", json!({"automations_paused": false}))
        .await;
    e.post(&format!("/v1/automations/{a}/disable"), json!({}))
        .await;
    let res = e.post(&retry_path(&a, &id), json!({})).await;
    assert_eq!(res.status_code(), 409);
    assert_eq!(e.mock.sent(), 1);
}

#[tokio::test]
#[serial]
async fn only_a_refused_send_is_retryable_not_a_run_cut_off_after_a_moderation_skip() {
    let e = env().await;
    let a = e
        .automation(json!({"name": "Flow", "trigger": "comment_received",
            "steps": [{"type": "hide_comment"}, {"type": "reply_publicly", "text": "hi"}]}))
        .await;
    let item = e.item("comment", &digits(), "hi", 0).await;
    // A run cut off mid-send, whose last result is a rate-limited moderation skip.
    e.sql(&format!(
        "UPDATE automation_runs SET status = 'failed', reason = 'interrupted', \
         results = '[{{\"type\": \"hide_comment\", \"status\": \"skipped\", \"reason\": \"rate_limited\"}}]' \
         WHERE inbox_item_id = '{}'",
        item.id
    ))
    .await;
    let page = runs(&e, &a, "").await;
    assert_eq!(page["runs"][0]["can_retry"], false);
    let id = page["runs"][0]["id"].as_str().unwrap();
    assert_eq!(
        e.post(&retry_path(&a, id), json!({})).await.status_code(),
        409
    );
}

#[tokio::test]
#[serial]
async fn two_clicks_at_once_retry_once() {
    let e = env().await;
    let a = e.automation(reply_all(json!({}))).await;
    e.mock.mode.store(1, Ordering::SeqCst);
    e.item("comment", &digits(), "hi", 0).await;
    let id = runs(&e, &a, "").await["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    e.mock.mode.store(0, Ordering::SeqCst);
    let path = retry_path(&a, &id);
    let (one, two) = tokio::join!(e.post(&path, json!({})), e.post(&path, json!({})));
    let mut codes = [one.status_code().as_u16(), two.status_code().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 409]);
    // One refused attempt, one taken: never two replies.
    let taken = e.mock.to("/comments").len() - 1;
    assert_eq!(taken, 1);
}

#[tokio::test]
#[serial]
async fn a_retry_does_not_cost_the_team_another_daily_hit() {
    let e = env().await;
    let a = e
        .automation(json!({"name": "Flow", "trigger": "comment_received",
            "steps": [{"type": "reply_publicly", "text": "one"}]}))
        .await;
    let bucket = automations::quota_bucket(e.team);
    e.mock.mode.store(1, Ordering::SeqCst);
    let item = e.item("comment", &digits(), "hi", 0).await;
    assert_eq!(e.count_hits(&bucket).await, 1);
    due(&e, item.id).await;
    tick(&e).await;
    e.mock.mode.store(0, Ordering::SeqCst);
    due(&e, item.id).await;
    tick(&e).await;
    assert_eq!(e.run(&a, item.id).await.unwrap().0, "sent");
    assert_eq!(
        e.count_hits(&bucket).await,
        1,
        "one send, one hit, however many tries"
    );
}
