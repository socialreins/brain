//! A flow that sends a private reply to a comment, waits for the person to answer, and goes on
//! in their conversation: Meta takes one private reply per comment, and the answer opens a new
//! 24 hour window.
use serde_json::{json, Value};
use socialreins_brain::automations;

use serial_test::serial;

use super::support::*;

fn guide_flow(timeout: u32) -> Value {
    json!({"name": "Guide", "trigger": "comment_received",
        "conditions": {"keywords": ["guide"]},
        "steps": [
            {"type": "send_dm", "text": "Want the guide? Say yes."},
            {"type": "wait_for_reply", "minutes": timeout},
            {"type": "send_dm", "text": "Here it is, {name}."}]})
}

/// Makes the waiting run of `item` due, as its timeout would.
async fn due(e: &Env, item: uuid::Uuid) {
    e.sql(&format!(
        "UPDATE automation_runs SET resume_at = now() - interval '1 second' \
         WHERE inbox_item_id = '{item}' AND status = 'waiting'"
    ))
    .await;
}

/// A round of flows. Other tests share the database, so only this team's runs are looked at.
async fn tick(e: &Env) {
    automations::resume_due(&e.state, std::time::Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
#[serial]
async fn the_answer_resumes_the_flow_in_the_persons_conversation() {
    let e = env().await;
    let a = e.automation(guide_flow(60)).await;
    // Another automation on DMs: the answer a waiting flow takes must not start it.
    let hello = e
        .automation(json!({"name": "Hello", "trigger": "dm_received",
            "actions": [{"type": "send_dm", "text": "Hi, I am a bot"}]}))
        .await;
    let person = digits();
    let comment = e.item("comment", &person, "send the guide", 0).await;
    assert_eq!(e.run(&a, comment.id).await.unwrap().0, "waiting");
    // The first message is the private reply to the comment.
    let first = e.mock.to("/messages");
    assert_eq!(first.len(), 1);
    assert!(first[0].contains("comment_id"), "{first:?}");
    // Someone else writing does not answer it.
    e.item("dm", &digits(), "yes", 0).await;
    due(&e, comment.id).await;
    // (made due, but nobody answered: only a reply sets `reply_item_id`)
    e.sql(&format!(
        "UPDATE automation_runs SET resume_at = now() + interval '1 hour' WHERE inbox_item_id = '{}'",
        comment.id
    ))
    .await;
    tick(&e).await;
    assert_eq!(e.run(&a, comment.id).await.unwrap().0, "waiting");
    // The person answers: the flow goes on and sends to them, not to the comment.
    let reply = e.item("dm", &person, "yes please", 0).await;
    assert!(
        e.run(&hello, reply.id).await.is_none(),
        "a taken answer starts nothing else"
    );
    tick(&e).await;
    assert_eq!(e.run(&a, comment.id).await.unwrap().0, "sent");
    // The private reply, the stranger's "Hi" (their DM is nobody's answer, so Hello took it),
    // and the guide to the person in their own conversation.
    let sent = e.mock.to("/messages");
    assert_eq!(sent.len(), 3, "{sent:?}");
    let last = &sent[2];
    assert!(
        last.contains(&person) && !last.contains("comment_id"),
        "{sent:?}"
    );
    assert!(
        last.contains("RESPONSE") && last.contains("Here+it+is"),
        "{sent:?}"
    );
    // The same answer is not taken twice.
    tick(&e).await;
    assert_eq!(e.mock.to("/messages").len(), 3);
}

#[tokio::test]
#[serial]
async fn no_answer_ends_the_run_with_its_reason() {
    let e = env().await;
    let a = e.automation(guide_flow(30)).await;
    let comment = e.item("comment", &digits(), "the guide?", 0).await;
    assert_eq!(e.run(&a, comment.id).await.unwrap().0, "waiting");
    due(&e, comment.id).await;
    tick(&e).await;
    let (status, reason) = e.run(&a, comment.id).await.unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("no_reply"))
    );
    assert_eq!(
        e.mock.to("/messages").len(),
        1,
        "the second message never went out"
    );
}

#[tokio::test]
#[serial]
async fn a_dm_from_before_the_flow_started_is_no_answer() {
    let e = env().await;
    let a = e.automation(guide_flow(30)).await;
    let person = digits();
    let comment = e.item("comment", &person, "the guide?", 0).await;
    // A message that came earlier than the run (backfilled late) does not count.
    e.item("dm", &person, "hello", 120).await;
    due(&e, comment.id).await;
    tick(&e).await;
    assert_eq!(
        e.run(&a, comment.id).await.unwrap().1.as_deref(),
        Some("no_reply")
    );
}

#[tokio::test]
#[serial]
async fn the_shape_of_a_flow_that_waits_for_a_reply_is_checked() {
    let e = env().await;
    let post = |steps: Value| {
        let mut body = json!({"name": "Flow", "trigger": "comment_received", "steps": steps,
            "conditions": {"account_ids": [e.account.id]}});
        body["steps"] = steps;
        e.post("/v1/automations", body)
    };
    let dm = |t: &str| json!({"type": "send_dm", "text": t});
    let wait = |m: u32| json!({"type": "wait_for_reply", "minutes": m});
    for (steps, code) in [
        (json!([wait(5), dm("a")]), "invalid_steps"),
        (json!([dm("a"), wait(5)]), "invalid_steps"),
        (json!([dm("a"), wait(0), dm("b")]), "invalid_steps"),
        (json!([dm("a"), wait(10081), dm("b")]), "invalid_steps"),
        // Two private replies to one comment, with no answer between them.
        (
            json!([dm("a"), {"type": "wait", "minutes": 5}, dm("b")]),
            "meta_one_private_reply",
        ),
        // A plain wait after the answer that runs past the 24 hour window.
        (
            json!([dm("a"), wait(60), {"type": "wait", "minutes": 1500}, dm("b")]),
            "meta_reply_window",
        ),
    ] {
        let res = post(steps.clone()).await;
        assert_eq!(res.status_code(), 422, "{steps}");
        assert_eq!(res.json::<Value>()["error"], code, "{steps}");
    }
    // After an answer more messages are fine, however many.
    let ok = post(json!([dm("a"), wait(60), dm("b"), dm("c")])).await;
    assert_eq!(ok.status_code(), 201, "{}", ok.text());
}

#[tokio::test]
#[serial]
async fn a_test_lists_the_wait_and_goes_on_as_if_answered() {
    let e = env().await;
    let mut automation = guide_flow(60);
    automation["conditions"]["account_ids"] = json!([e.account.id]);
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": automation,
                "sample": {"account_id": e.account.id, "text": "the guide?", "author_name": "Ada"}}),
        )
        .await;
    let out = res.json::<Value>();
    let kinds: Vec<_> = out["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["type"].as_str().unwrap(), s["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("send_dm", "would_send"),
            ("wait_for_reply", "would_wait"),
            ("send_dm", "would_send")
        ]
    );
    assert_eq!(out["steps"][2]["text"], "Here it is, Ada.");
    assert_eq!(e.mock.sent(), 0);
}

#[tokio::test]
#[serial]
async fn a_plain_wait_after_the_answer_keeps_sending_in_the_answers_conversation() {
    let e = env().await;
    let a = e
        .automation(json!({"name": "Long", "trigger": "comment_received",
            "steps": [{"type": "send_dm", "text": "Want it?"},
                      {"type": "wait_for_reply", "minutes": 60},
                      {"type": "send_dm", "text": "Here"},
                      {"type": "wait", "minutes": 5},
                      {"type": "send_dm", "text": "Anything else?"}]}))
        .await;
    let person = digits();
    let comment = e.item("comment", &person, "the guide?", 0).await;
    e.item("dm", &person, "yes", 0).await;
    tick(&e).await;
    due(&e, comment.id).await;
    tick(&e).await;
    assert_eq!(e.run(&a, comment.id).await.unwrap().0, "sent");
    let sent = e.mock.to("/messages");
    assert_eq!(sent.len(), 3, "{sent:?}");
    // Neither later message is a second private reply to the comment.
    assert!(
        sent[1..]
            .iter()
            .all(|m| !m.contains("comment_id") && m.contains(&person)),
        "{sent:?}"
    );
}

#[tokio::test]
#[serial]
async fn the_sweep_taking_an_answer_again_starts_nothing_else() {
    let e = env().await;
    let a = e.automation(guide_flow(60)).await;
    let hello = e
        .automation(json!({"name": "Hello", "trigger": "dm_received",
            "actions": [{"type": "send_dm", "text": "Hi"}]}))
        .await;
    let person = digits();
    let comment = e.item("comment", &person, "the guide?", 0).await;
    let reply = e.item("dm", &person, "yes", 0).await;
    // A crash before `automated_at`: the sweep runs the answer once more.
    automations::run(&e.state, &e.account, &reply)
        .await
        .unwrap();
    assert!(e.run(&hello, reply.id).await.is_none());
    tick(&e).await;
    assert_eq!(e.run(&a, comment.id).await.unwrap().0, "sent");
}

#[tokio::test]
#[serial]
async fn an_answer_that_is_gone_ends_the_run_instead_of_a_second_private_reply() {
    let e = env().await;
    let a = e.automation(guide_flow(60)).await;
    let person = digits();
    let comment = e.item("comment", &person, "the guide?", 0).await;
    let reply = e.item("dm", &person, "yes", 0).await;
    e.sql(&format!(
        "DELETE FROM inbox_items WHERE id = '{}'",
        reply.id
    ))
    .await;
    tick(&e).await;
    let (status, why) = e.run(&a, comment.id).await.unwrap();
    assert_eq!(
        (status.as_str(), why.as_deref()),
        ("skipped", Some("reply_gone"))
    );
    assert_eq!(e.mock.to("/messages").len(), 1);
}
