//! The conditions and actions every network shares, on Facebook against the mock Graph API.
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

use super::support::*;

/// A comment automation that replies, with `conditions` added.
fn when(conditions: Value) -> Value {
    reply_all(json!({"conditions": conditions}))
}

#[tokio::test]
async fn link_and_sentiment_conditions_are_checked() {
    let e = env().await;
    for bad in [
        json!({"sentiment": ["joke"]}),
        json!({"sentiment": ["question"]}),
    ] {
        let mut body = when(bad.clone());
        body["conditions"]["account_ids"] = json!([e.account.id]);
        let res = e.post("/v1/automations", body).await;
        assert_eq!(res.status_code(), 422, "{bad}");
        assert_eq!(res.json::<Value>()["error"], "invalid_conditions", "{bad}");
    }
    // No text to read in a follow or a button tap.
    for trigger in ["new_follower", "button_clicked"] {
        let mut body = reply_all(
            json!({"trigger": trigger, "actions": [{"type": "send_dm", "text": "x"}],
            "conditions": {"has_link": true, "payloads": ["a"]}}),
        );
        body["conditions"]["account_ids"] = json!([e.account.id]);
        let res = e.post("/v1/automations", body).await;
        assert_eq!(res.status_code(), 422, "{trigger}");
    }
    let mut body = when(json!({"sentiment": ["positive", "negative"]}));
    body["conditions"]["account_ids"] = json!([e.account.id]);
    let made = e.post("/v1/automations", body).await.json::<Value>();
    assert_eq!(
        made["conditions"]["sentiment"],
        json!(["negative", "positive"])
    );
}

#[tokio::test]
async fn a_link_condition_tells_spam_from_talk() {
    let e = env().await;
    let spam = e.automation(when(json!({"has_link": true}))).await;
    let talk = e.automation(when(json!({"has_link": false}))).await;
    let link = e
        .item("comment", "1", "win big at casino-deals.xyz now", 0)
        .await;
    let plain = e.item("comment", "2", "love this, thanks @cafe", 0).await;
    assert!(e.run(&spam, link.id).await.is_some());
    assert!(e.run(&talk, link.id).await.is_none());
    assert!(e.run(&spam, plain.id).await.is_none());
    assert!(e.run(&talk, plain.id).await.is_some());
}

#[tokio::test]
async fn a_sentiment_is_read_by_the_model_last_and_counted() {
    let e = env_with(Opts {
        ai_key: true,
        ..Opts::default()
    })
    .await;
    let a = e
        .automation(when(
            json!({"sentiment": ["negative"], "keywords": ["price"]}),
        ))
        .await;
    *e.mock.ai_reply.lock().unwrap() = "Negative".into();
    // The keyword fails first: the model is never asked.
    let off = e.item("comment", "1", "nice", 0).await;
    assert!(e.run(&a, off.id).await.is_none());
    assert_eq!(e.mock.ai_calls.load(Ordering::SeqCst), 0);
    let asked = e.item("comment", "2", "what a terrible price", 0).await;
    assert_eq!(e.run(&a, asked.id).await.unwrap().0, "sent");
    assert_eq!(e.mock.ai_calls.load(Ordering::SeqCst), 1);
    // Read as something else: no run.
    *e.mock.ai_reply.lock().unwrap() = "positive".into();
    let angry = e.item("comment", "3", "the price is lovely", 0).await;
    assert!(e.run(&a, angry.id).await.is_none());
    // The model fails: the run log says so, and nothing is sent.
    *e.mock.ai_reply.lock().unwrap() = String::new();
    let empty = e.item("comment", "4", "price?", 0).await;
    let (status, reason) = e.run(&a, empty.id).await.unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("ai_empty"))
    );
    assert_eq!(e.mock.to("/comments").len(), 1);
    // A test of it names what the model read, and counts as an AI call.
    *e.mock.ai_reply.lock().unwrap() = "negative".into();
    let mut automation = when(json!({"sentiment": ["negative"]}));
    automation["conditions"]["account_ids"] = json!([e.account.id]);
    let res = e
        .post(
            "/v1/automations/test",
            json!({"automation": automation,
                "sample": {"account_id": e.account.id, "text": "how much?"}}),
        )
        .await;
    let out = res.json::<Value>();
    assert_eq!(
        (out["matched"].clone(), out["sentiment"].clone()),
        (json!(true), json!("negative"))
    );
}

#[tokio::test]
async fn a_sentiment_without_any_key_is_skipped_with_its_reason() {
    let e = env().await;
    let a = e.automation(when(json!({"sentiment": ["negative"]}))).await;
    let i = e.item("comment", "1", "price?", 0).await;
    let (status, reason) = e.run(&a, i.id).await.unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("no_ai_key"))
    );
    // That skip never ran: the same person's next comment is still new to the automation.
    let a2 = e
        .automation(when(
            json!({"first_per_author": true, "sentiment": ["negative"]}),
        ))
        .await;
    let first = e.item("comment", "2", "price?", 0).await;
    let again = e.item("comment", "2", "price??", 0).await;
    assert_eq!(
        e.run(&a2, first.id).await.unwrap().1.as_deref(),
        Some("no_ai_key")
    );
    assert_eq!(
        e.run(&a2, again.id).await.unwrap().1.as_deref(),
        Some("no_ai_key")
    );
}

#[tokio::test]
async fn moderation_notice_assignment_and_stop_actions_run() {
    let e = env().await;
    let owner: uuid::Uuid = {
        let sql = format!("SELECT user_id FROM team_user WHERE team_id = '{}'", e.team);
        let r =
            e.db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
                .await
                .unwrap()
                .unwrap();
        r.try_get("", "user_id").unwrap()
    };
    // Spam: hide it, tell the team, assign it, and let no later automation answer.
    let spam = e
        .automation(reply_all(
            json!({"name": "Spam", "conditions": {"has_link": true},
            "actions": [{"type": "hide_comment"}, {"type": "notify_team"},
                        {"type": "assign", "user_id": owner}, {"type": "stop_automations"}]}),
        ))
        .await;
    let thanks = e.automation(reply_all(json!({"name": "Thanks"}))).await;
    let item = e.item("comment", "1", "free money at scam.xyz", 0).await;
    let (status, _) = e.run(&spam, item.id).await.unwrap();
    assert_eq!(status, "sent");
    assert!(e.run(&thanks, item.id).await.is_none(), "stopped");
    // The hide went to Graph as the comment's own call, nothing was said publicly.
    let hides = e.mock.to(&item.remote_id);
    assert_eq!(hides.len(), 1);
    assert!(hides[0].contains("is_hidden=true"), "{hides:?}");
    assert!(e.mock.to("/comments").is_empty());
    assert_eq!(
        e.count_hits(&format!("automation:notice:{}", e.team)).await,
        1
    );
    let assigned =
        e.db.query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT assigned_to FROM inbox_items WHERE id = '{}'",
                item.id
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        assigned.try_get::<uuid::Uuid>("", "assigned_to").unwrap(),
        owner
    );
    // A normal comment still gets the thanks.
    let fine = e.item("comment", "2", "lovely", 0).await;
    assert_eq!(e.run(&thanks, fine.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn a_moderation_action_the_item_does_not_take_is_skipped() {
    let e = env().await;
    let like = e
        .automation(reply_all(json!({"actions": [{"type": "like_comment"}]})))
        .await;
    let item = e.item("comment", "1", "hi", 0).await;
    assert_eq!(e.run(&like, item.id).await.unwrap().0, "sent");
    assert!(e.mock.to("/likes").len() == 1);
    // A DM trigger takes no comment action, and nobody outside the team can be assigned.
    for (trigger, action) in [
        ("dm_received", json!({"type": "hide_comment"})),
        (
            "comment_received",
            json!({"type": "assign", "user_id": uuid::Uuid::now_v7()}),
        ),
    ] {
        let mut body = reply_all(json!({"trigger": trigger, "actions": [action]}));
        body["conditions"] = json!({"account_ids": [e.account.id]});
        let res = e.post("/v1/automations", body).await;
        assert_eq!(res.status_code(), 422);
        assert_eq!(res.json::<Value>()["error"], "invalid_actions");
    }
}

#[tokio::test]
async fn a_stop_holds_when_the_sweep_runs_the_item_again_and_the_model_is_not_asked_twice() {
    let e = env_with(Opts {
        ai_key: true,
        ..Opts::default()
    })
    .await;
    let stopper = e
        .automation(reply_all(json!({"name": "First", "conditions": {"sentiment": ["negative"]},
            "actions": [{"type": "reply_publicly", "text": "Answer"}, {"type": "stop_automations"}]})))
        .await;
    let later = e.automation(reply_all(json!({"name": "Later"}))).await;
    *e.mock.ai_reply.lock().unwrap() = "negative".into();
    let item = e.item("comment", &digits(), "how much?", 0).await;
    assert!(e.run(&later, item.id).await.is_none());
    assert_eq!(e.mock.ai_calls.load(Ordering::SeqCst), 1);
    // The sweep runs it again: the stop still holds, and nothing is read again.
    socialreins_brain::automations::run(&e.state, &e.account, &item)
        .await
        .unwrap();
    assert!(e.run(&later, item.id).await.is_none());
    assert_eq!(e.run(&stopper, item.id).await.unwrap().0, "sent");
    assert_eq!(e.mock.ai_calls.load(Ordering::SeqCst), 1);
    assert_eq!(e.mock.to("/comments").len(), 1);
}

#[tokio::test]
async fn a_person_in_a_cool_down_is_not_read_by_the_model() {
    let e = env_with(Opts {
        ai_key: true,
        ..Opts::default()
    })
    .await;
    let a = e
        .automation(reply_all(json!({"conditions": {"sentiment": ["negative"]},
            "cooldown_minutes": 60})))
        .await;
    *e.mock.ai_reply.lock().unwrap() = "negative".into();
    let who = digits();
    let first = e.item("comment", &who, "how much?", 0).await;
    assert_eq!(e.run(&a, first.id).await.unwrap().0, "sent");
    let again = e.item("comment", &who, "how much?!", 0).await;
    assert_eq!(
        e.run(&a, again.id).await.unwrap().1.as_deref(),
        Some("cooldown")
    );
    assert_eq!(e.mock.ai_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_flow_names_each_one_off_action_once() {
    let e = env().await;
    let mut body = json!({"name": "Flow", "trigger": "comment_received",
        "conditions": {"account_ids": [e.account.id]},
        "steps": [{"type": "notify_team"}, {"type": "notify_team"},
                  {"type": "reply_publicly", "text": "x"}]});
    body["steps"][0] = json!({"type": "notify_team"});
    let res = e.post("/v1/automations", body).await;
    assert_eq!(res.json::<Value>()["error"], "invalid_steps");
}
