//! What Facebook and Instagram let a message do, as "Test this automation" plays it, and the
//! Instagram story mention trigger.
use serde_json::{json, Value};
use socialreins_brain::inbox::Attachment;

use super::support::*;

/// A test of `automation` on the env's account; the outcome.
async fn test(e: &Env, mut automation: Value, sample: Value) -> Value {
    automation["conditions"]["account_ids"] = json!([e.account.id]);
    let mut sample = sample;
    sample["account_id"] = json!(e.account.id);
    sample["text"] = json!("the guide?");
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": automation, "sample": sample}),
        )
        .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    res.json()
}

fn steps(out: &Value) -> Vec<(String, String, Option<String>)> {
    out["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["type"].as_str().unwrap().to_owned(),
                s["status"].as_str().unwrap().to_owned(),
                s["reason"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn private_reply(fallback: bool) -> Value {
    let mut a = json!({"name": "Guide", "trigger": "comment_received",
        "actions": [{"type": "send_dm", "text": "Here is the guide"}]});
    if fallback {
        a["fallback_public_reply"] = json!("Sent you a DM!");
    }
    a
}

#[tokio::test]
async fn a_private_reply_is_tested_against_the_seven_day_window_and_one_per_comment() {
    let e = env().await;
    let day = 24 * 60;
    let ok = test(&e, private_reply(true), json!({"age_minutes": 6 * day})).await;
    assert_eq!(steps(&ok), [("send_dm".into(), "would_send".into(), None)]);
    // Seven days old: Meta refuses the private reply, so the public fallback answers.
    let late = test(&e, private_reply(true), json!({"age_minutes": 7 * day})).await;
    assert_eq!(
        steps(&late),
        [
            (
                "send_dm".into(),
                "would_skip".into(),
                Some("outside_window".into())
            ),
            ("fallback_public_reply".into(), "would_send".into(), None),
        ]
    );
    // One private reply a comment: a second DM before the person answers is refused.
    let used = test(
        &e,
        private_reply(false),
        json!({"private_reply_sent": true}),
    )
    .await;
    assert_eq!(
        steps(&used),
        [(
            "send_dm".into(),
            "would_skip".into(),
            Some("already_replied".into())
        )]
    );
    // A flow that waits for the answer may send more, as a real run would.
    let flow = json!({"name": "Flow", "trigger": "comment_received", "steps": [
        {"type": "send_dm", "text": "Want it?"}, {"type": "wait_for_reply", "minutes": 60},
        {"type": "send_dm", "text": "Here"}]});
    let out = test(&e, flow, json!({})).await;
    assert_eq!(
        steps(&out).iter().map(|s| s.1.as_str()).collect::<Vec<_>>(),
        ["would_send", "would_wait", "would_send"]
    );
    assert_eq!(e.mock.sent(), 0);
}

#[tokio::test]
async fn a_message_is_tested_against_the_24_hour_window_and_the_human_agent_tag() {
    let dm = json!({"name": "Welcome", "trigger": "dm_received",
        "actions": [{"type": "send_dm", "text": "Hi"}]});
    let e = env().await;
    let ok = test(&e, dm.clone(), json!({"age_minutes": 23 * 60})).await;
    assert_eq!(steps(&ok)[0].1, "would_send");
    let late = test(&e, dm.clone(), json!({"age_minutes": 30 * 60})).await;
    assert_eq!(steps(&late)[0].2.as_deref(), Some("outside_window"));
    // Without Meta's Human Agent feature, not even a person could answer late.
    assert_eq!(late["notes"], json!(["human_agent_off"]));
    // With it, a person could, until 7 days: the automation still never does.
    let agent = env_with(Opts {
        human_agent: true,
        ..Opts::default()
    })
    .await;
    let late = test(&agent, dm.clone(), json!({"age_minutes": 30 * 60})).await;
    assert_eq!(steps(&late)[0].2.as_deref(), Some("outside_window"));
    assert_eq!(late["notes"], json!(["human_agent_by_hand"]));
    let gone = test(&agent, dm.clone(), json!({"age_minutes": 8 * 24 * 60})).await;
    assert_eq!(gone["notes"], json!([]));
    // A wait inside the flow eats the window: 23 hours old plus a 2 hour wait.
    let flow = json!({"name": "Flow", "trigger": "dm_received", "steps": [
        {"type": "wait", "minutes": 120}, {"type": "send_dm", "text": "Hi"}]});
    let mut flow = flow;
    // A flow never starts on a wait: put a tag first.
    flow["steps"] = json!([{"type": "tag_contact", "tag": "x"}, {"type": "wait", "minutes": 120},
        {"type": "send_dm", "text": "Hi"}]);
    let out = test(&e, flow, json!({"age_minutes": 23 * 60})).await;
    assert_eq!(steps(&out)[2].2.as_deref(), Some("outside_window"));
}

#[tokio::test]
async fn an_instagram_dm_over_1000_bytes_is_refused_in_a_test() {
    let mut e = env().await;
    e.instagram().await;
    let long = "é".repeat(501);
    assert_eq!(long.len(), 1002);
    let flow = json!({"name": "Long", "trigger": "dm_received",
        "actions": [{"type": "send_dm", "text": long}]});
    // Saved for this account it is refused outright; tried on a sample it shows the failure.
    let mut by_name = flow.clone();
    by_name["conditions"] = json!({"account_ids": [e.account.id]});
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": by_name, "sample": {"account_id": e.account.id, "text": "hi"}}),
        )
        .await;
    assert_eq!(res.json::<Value>()["error"], "invalid_text");
    let any = json!({"name": "Long", "trigger": "dm_received", "conditions": {},
        "actions": [{"type": "send_dm", "text": "é".repeat(500)}]});
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": any,
                "sample": {"account_id": e.account.id, "text": "hi", "author_name": "Ada"}}),
        )
        .await;
    assert_eq!(steps(&res.json::<Value>())[0].1, "would_send");
}

#[tokio::test]
async fn a_story_mention_runs_its_own_automations_only() {
    let mut e = env().await;
    e.instagram().await;
    let mention = e
        .automation(json!({"name": "Mention", "trigger": "story_mention",
            "actions": [{"type": "send_dm", "text": "Thanks for the mention, {name}!"}]}))
        .await;
    let dm = e
        .automation(json!({"name": "Hello", "trigger": "dm_received",
            "actions": [{"type": "send_dm", "text": "Hi"}]}))
        .await;
    let story =
        Attachment::new("story_mention", Some("https://cdn.example/story.jpg"), None).unwrap();
    let who = digits();
    let item = e.item_with("dm", &who, "", 0, vec![story]).await;
    assert_eq!(e.run(&mention, item.id).await.unwrap().0, "sent");
    assert!(e.run(&dm, item.id).await.is_none());
    let sent = e.mock.to("/messages");
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].contains(&who) && sent[0].contains("RESPONSE"),
        "{sent:?}"
    );
    // A plain DM never runs it.
    let plain = e.item("dm", &digits(), "hello", 0).await;
    assert!(e.run(&mention, plain.id).await.is_none());
    assert_eq!(e.run(&dm, plain.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn a_story_mention_automation_is_checked() {
    let e = env().await;
    let save = |body: Value| {
        let mut body = body;
        body["trigger"] = json!("story_mention");
        e.post("/v1/automations", body)
    };
    let dm = json!([{"type": "send_dm", "text": "Thanks"}]);
    // No text to look in, nothing public to reply under, and Instagram only.
    for (body, code) in [
        (
            json!({"name": "a", "actions": dm, "conditions": {"keywords": ["hi"]}}),
            "invalid_conditions",
        ),
        (
            json!({"name": "a", "actions": [{"type": "reply_publicly", "text": "x"}]}),
            "invalid_actions",
        ),
        (
            json!({"name": "a", "actions": dm, "conditions": {"account_ids": [e.account.id]}}),
            "facebook_unsupported_trigger",
        ),
    ] {
        let res = save(body).await;
        assert_eq!(res.status_code(), 422, "{}", res.text());
        assert_eq!(res.json::<Value>()["error"], code);
    }
    assert_eq!(
        save(json!({"name": "ok", "actions": dm}))
            .await
            .status_code(),
        201
    );
}

#[tokio::test]
async fn the_window_edges_and_the_text_check_come_in_meta_s_order() {
    let dm = json!({"name": "Welcome", "trigger": "dm_received",
        "actions": [{"type": "send_dm", "text": "Hi"}]});
    let e = env().await;
    // 1439 minutes is inside the 24 hours, 1440 is not.
    let inside = test(&e, dm.clone(), json!({"age_minutes": 1439})).await;
    assert_eq!(steps(&inside)[0].1, "would_send");
    let outside = test(&e, dm.clone(), json!({"age_minutes": 1440})).await;
    assert_eq!(steps(&outside)[0].2.as_deref(), Some("outside_window"));
    // A sample older than 30 days is refused.
    let mut automation = dm.clone();
    automation["conditions"] = json!({"account_ids": [e.account.id]});
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": automation,
                "sample": {"account_id": e.account.id, "text": "hi", "age_minutes": 43201}}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    // Too long is found before the window, as a real send does it: no human-agent note, no fallback.
    let mut ig = env().await;
    ig.instagram().await;
    let long = json!({"name": "Long", "trigger": "comment_received",
        "actions": [{"type": "send_dm", "text": "é".repeat(501)}],
        "fallback_public_reply": "Sent!"});
    let mut automation = long.clone();
    automation["conditions"] = json!({});
    let res = ig
        .post(
            "/v1/automations/test",
            json!({"automation": automation,
                "sample": {"account_id": ig.account.id, "text": "hi", "age_minutes": 8 * 24 * 60}}),
        )
        .await;
    let out = res.json::<Value>();
    assert_eq!(
        steps(&out)[0],
        (
            "send_dm".into(),
            "would_fail".into(),
            Some("too_long".into())
        )
    );
    assert_eq!(out["notes"], json!([]));
    assert_eq!(steps(&out).len(), 1);
}

#[tokio::test]
async fn story_mentions_and_follows_on_the_wrong_networks_are_refused() {
    let e = env().await;
    let dm = json!([{"type": "send_dm", "text": "Thanks"}]);
    let save = |trigger: &str, actions: Value, accounts: Vec<uuid::Uuid>| {
        e.post(
            "/v1/automations",
            json!({"name": "a", "trigger": trigger, "actions": actions,
                "conditions": {"account_ids": accounts}}),
        )
    };
    // Facebook has no story mentions, and Meta never brings a follow or a mention of the account.
    for (trigger, code) in [
        ("story_mention", "facebook_unsupported_trigger"),
        ("new_follower", "meta_unsupported_trigger"),
        ("mention_received", "meta_unsupported_trigger"),
    ] {
        let res = save(trigger, dm.clone(), vec![e.account.id]).await;
        assert_eq!(res.status_code(), 422, "{trigger}");
        assert_eq!(res.json::<Value>()["error"], code, "{trigger}");
    }
    // A text condition has nothing to read in a story mention.
    for bad in [json!({"question": true}), json!({"languages": ["en"]})] {
        let res = e
            .post(
                "/v1/automations",
                json!({"name": "a", "trigger": "story_mention", "actions": dm, "conditions": bad}),
            )
            .await;
        assert_eq!(res.status_code(), 422, "{bad}");
    }
    // Saved for no account in particular, a test on a Facebook account still shows the block.
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": {"name": "a", "trigger": "story_mention", "actions": dm, "conditions": {}},
                "sample": {"account_id": e.account.id, "text": "hi"}}),
        )
        .await;
    assert_eq!(
        res.json::<Value>()["blocks"],
        json!(["facebook_unsupported_trigger"])
    );
}
