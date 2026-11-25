//! Cool-down, per-automation cap, the team's pause switch and the loop guard.
use serde_json::json;
use socialreins_brain::automations;

use super::support::*;

#[tokio::test]
async fn a_cool_down_lets_a_person_start_an_automation_once() {
    let e = env().await;
    let a = e
        .automation(reply_all(json!({"cooldown_minutes": 60})))
        .await;
    let first = e.item("comment", "777", "hi", 0).await;
    assert_eq!(e.run(&a, first.id).await.unwrap().0, "sent");
    // The same person again: nothing goes out, and the run log says why.
    let again = e.item("comment", "777", "hi again", 0).await;
    let (status, reason) = e.run(&a, again.id).await.unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("cooldown"))
    );
    // Someone else is not held back.
    let other = e.item("comment", "888", "hello", 0).await;
    assert_eq!(e.run(&a, other.id).await.unwrap().0, "sent");
    assert_eq!(e.mock.to("/comments").len(), 2);
    // Once the first run is older than the cool-down, the person may start it again.
    e.sql(&format!(
        "UPDATE automation_runs SET created_at = now() - interval '61 minutes' \
         WHERE inbox_item_id = '{}'",
        first.id
    ))
    .await;
    let later = e.item("comment", "777", "and again", 0).await;
    assert_eq!(e.run(&a, later.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn a_run_that_sent_nothing_does_not_start_a_cool_down() {
    let e = env().await;
    let a = e
        .automation(reply_all(json!({"cooldown_minutes": 60})))
        .await;
    // Meta's rate limit refuses the first reply: the run is `skipped`, nothing went out, so the
    // person is still free to try again.
    e.mock.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    let first = e.item("comment", "777", "hi", 0).await;
    assert_eq!(e.run(&a, first.id).await.unwrap().0, "skipped");
    e.mock.mode.store(0, std::sync::atomic::Ordering::SeqCst);
    let again = e.item("comment", "777", "hi again", 0).await;
    assert_eq!(e.run(&a, again.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn an_automation_stops_at_its_own_daily_cap_and_leaves_the_teams_hits() {
    let e = env().await;
    let a = e.automation(reply_all(json!({"daily_cap": 2}))).await;
    let b = e.automation(reply_all(json!({"name": "Other"}))).await;
    for author in ["1", "2"] {
        let i = e.item("comment", author, "hi", 0).await;
        assert_eq!(e.run(&a, i.id).await.unwrap().0, "sent");
        assert_eq!(e.run(&b, i.id).await.unwrap().0, "sent");
    }
    let third = e.item("comment", "3", "hi", 0).await;
    let (status, reason) = e.run(&a, third.id).await.unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("skipped", Some("automation_cap"))
    );
    // The other automation has no cap of its own, and the capped one took no team hit.
    assert_eq!(e.run(&b, third.id).await.unwrap().0, "sent");
    let hits = e.count_hits(&automations::quota_bucket(e.team)).await;
    assert_eq!(
        hits, 5,
        "four sends before the cap, then one by the other automation"
    );
}

#[tokio::test]
async fn pausing_stops_every_automation_and_holds_waiting_flows() {
    let e = env().await;
    let a = e
        .automation(reply_all(json!({"conditions": {"first_per_author": true}})))
        .await;
    let flow = e
        .automation(json!({"name": "Flow", "trigger": "comment_received",
            "steps": [{"type": "reply_publicly", "text": "one"}, {"type": "wait", "minutes": 1},
                      {"type": "reply_publicly", "text": "two"}]}))
        .await;
    let waiting = e.item("comment", "5", "hi", 0).await;
    assert_eq!(e.run(&flow, waiting.id).await.unwrap().0, "waiting");
    let sent_before = e.mock.sent();
    let res = e
        .patch("/v1/team", json!({"automations_paused": true}))
        .await;
    assert_eq!(res.status_code(), 200);
    let body = res.json::<serde_json::Value>();
    assert_eq!(body["automations_paused"], true);
    assert!(body["automations_paused_at"].is_string());
    let during = e.item("comment", "6", "hi", 0).await;
    for id in [&a, &flow] {
        let (status, reason) = e.run(id, during.id).await.unwrap();
        assert_eq!(
            (status.as_str(), reason.as_deref()),
            ("skipped", Some("paused"))
        );
    }
    // A waiting run is held, not ended, while paused.
    e.sql(&format!(
        "UPDATE automation_runs SET resume_at = now() - interval '1 minute' \
         WHERE inbox_item_id = '{}'",
        waiting.id
    ))
    .await;
    let due = automations::resume_due(&e.state, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(due, 0);
    assert_eq!(e.run(&flow, waiting.id).await.unwrap().0, "waiting");
    assert_eq!(e.mock.sent(), sent_before);
    // Switched on again: the held run goes on, and the person who came while paused is still
    // a first-time commenter for a first-comment automation.
    let res = e
        .patch("/v1/team", json!({"automations_paused": false}))
        .await;
    assert_eq!(res.json::<serde_json::Value>()["automations_paused"], false);
    let due = automations::resume_due(&e.state, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(due, 1);
    assert_eq!(e.run(&flow, waiting.id).await.unwrap().0, "sent");
    let after = e.item("comment", "6", "hello again", 0).await;
    assert_eq!(e.run(&a, after.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn items_from_any_connected_account_on_the_network_never_run_automations() {
    let e = env().await;
    let a = e.automation(reply_all(json!({}))).await;
    // Another team's Page, and one of this team's own.
    let other = env().await;
    let (one, two, three) = (digits(), digits(), digits());
    let mine = new_account(&e.state, &e.db, e.team, "facebook", &one).await;
    let theirs = new_account(&e.state, &e.db, other.team, "facebook", &two).await;
    for who in [&mine.external_id, &theirs.external_id] {
        let i = e.item("comment", who, "hello", 0).await;
        assert!(e.run(&a, i.id).await.is_none(), "{who}");
    }
    let stranger = e.item("comment", &three, "hello", 0).await;
    assert_eq!(e.run(&a, stranger.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn limits_are_checked() {
    let e = env().await;
    for bad in [
        json!({"cooldown_minutes": 0}),
        json!({"cooldown_minutes": 43201}),
        json!({"daily_cap": 0}),
        json!({"daily_cap": 501}),
    ] {
        let mut body = reply_all(bad);
        body["conditions"] = json!({"account_ids": [e.account.id]});
        let res = e.post("/v1/automations", body).await;
        assert_eq!(res.status_code(), 422);
        assert_eq!(res.json::<serde_json::Value>()["error"], "invalid_limits");
    }
}

#[tokio::test]
async fn two_items_by_one_person_at_once_start_one_run() {
    let e = env().await;
    let a = e
        .automation(reply_all(json!({"cooldown_minutes": 60})))
        .await;
    let (one, two) = tokio::join!(
        e.item("comment", "777", "hi", 0),
        e.item("comment", "777", "hi", 0)
    );
    let sent = [&one, &two]
        .into_iter()
        .map(|i| i.id)
        .map(|id| e.run(&a, id));
    let runs = futures_util::future::join_all(sent).await;
    let statuses: Vec<_> = runs.into_iter().map(|r| r.unwrap().0).collect();
    assert_eq!(
        statuses.iter().filter(|s| *s == "sent").count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(e.mock.to("/comments").len(), 1);
}

#[tokio::test]
async fn a_skip_that_never_ran_leaves_the_first_comment_and_the_cool_down_alone() {
    let e = env().await;
    let a = e
        .automation(reply_all(json!({"conditions": {"first_per_author": true},
            "cooldown_minutes": 60, "daily_cap": 1})))
        .await;
    let first = e.item("comment", "1", "hi", 0).await;
    assert_eq!(e.run(&a, first.id).await.unwrap().0, "sent");
    // Another person meets the automation's own cap, which also shows when the team's day is
    // used up too: the automation's reason wins.
    e.sql(&format!(
        "INSERT INTO rate_limit_hits (bucket) SELECT '{}' FROM generate_series(1, 500)",
        automations::quota_bucket(e.team)
    ))
    .await;
    let capped = e.item("comment", "2", "hi", 0).await;
    let (_, reason) = e.run(&a, capped.id).await.unwrap();
    assert_eq!(reason.as_deref(), Some("automation_cap"));
    // Room again: person 2 is still new to the automation, so their next comment is a first.
    e.sql(&format!(
        "DELETE FROM rate_limit_hits WHERE bucket IN ('automation:runs:{a}', '{}')",
        automations::quota_bucket(e.team)
    ))
    .await;
    let again = e.item("comment", "2", "hi again", 0).await;
    assert_eq!(e.run(&a, again.id).await.unwrap().0, "sent");
}

#[tokio::test]
async fn a_put_without_limits_keeps_them_and_null_clears_them() {
    let e = env().await;
    let a = e
        .automation(reply_all(json!({"cooldown_minutes": 30, "daily_cap": 5})))
        .await;
    let put = |body: serde_json::Value| async {
        let mut body = reply_all(body);
        body["conditions"] = json!({"account_ids": [e.account.id]});
        let res =
            e.s.put(&format!("/v1/automations/{a}"))
                .add_header("origin", WEB)
                .add_header("cookie", e.cookie.clone())
                .json(&body)
                .await;
        assert_eq!(res.status_code(), 200, "{}", res.text());
        res.json::<serde_json::Value>()
    };
    let kept = put(json!({"name": "Renamed"})).await;
    assert_eq!(
        (kept["cooldown_minutes"].clone(), kept["daily_cap"].clone()),
        (json!(30), json!(5))
    );
    let cleared = put(json!({"cooldown_minutes": null})).await;
    assert_eq!(
        (
            cleared["cooldown_minutes"].clone(),
            cleared["daily_cap"].clone()
        ),
        (json!(null), json!(5))
    );
}

#[tokio::test]
async fn a_test_reports_that_the_team_is_paused() {
    let e = env().await;
    let body = |_: ()| {
        let mut automation = reply_all(json!({}));
        automation["conditions"] = json!({"account_ids": [e.account.id]});
        json!({"automation": automation,
            "sample": {"account_id": e.account.id, "text": "hi", "author_name": "Ada"}})
    };
    let res = e.post("/v1/automations/test", body(())).await;
    assert_eq!(res.json::<serde_json::Value>()["blocks"], json!([]));
    e.patch("/v1/team", json!({"automations_paused": true}))
        .await;
    let res = e.post("/v1/automations/test", body(())).await;
    assert_eq!(res.json::<serde_json::Value>()["blocks"], json!(["paused"]));
}

#[tokio::test]
async fn an_author_id_on_another_network_is_not_one_of_our_accounts() {
    let e = env().await;
    let a = e.automation(reply_all(json!({}))).await;
    let id = digits();
    new_account(&e.state, &e.db, e.team, "bluesky", &id).await;
    let i = e.item("comment", &id, "hello", 0).await;
    assert_eq!(e.run(&a, i.id).await.unwrap().0, "sent");
}
