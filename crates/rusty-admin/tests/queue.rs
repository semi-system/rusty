//! The outbound queue of a domain and the message trace (docs/design-queue.md): what the administrator sees of the jobs, the four actions and the states they apply to, and the delivery log.

mod common;

use common::*;
use rusty_store::{JobQueue, NewLogRow};
use serde_json::json;

fn payload(to: &str) -> serde_json::Value {
    json!({ "blob": "ab".repeat(32), "envelope_from": "alice@one.test", "recipients": [to], "destination": to.rsplit_once('@').unwrap().1, "sender_domain": "one.test" })
}

#[tokio::test]
async fn a_queued_job_is_listed_held_released_and_deleted_and_the_actions_keep_to_their_states() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    let dom = r.dir.find_domain("one.test").await.unwrap().unwrap().id;
    let jobs = JobQueue::with_repo(r.store.repos().jobs.clone());
    let a = jobs.enqueue("outbound", Some(dom), &payload("bob@far.example"), None).await.unwrap();
    jobs.enqueue("outbound", Some(dom), &payload("carol@other.example"), None).await.unwrap();

    let list = c.get("/api/v1/domains/one.test/queue").await;
    assert_eq!(list.status, 200, "{}", list.body);
    conforms(&list.json(), "QueueList");
    assert_eq!(list.json()["items"].as_array().unwrap().len(), 2);
    let first = &list.json()["items"][1];
    assert_eq!((first["state"].clone(), first["recipients"].clone(), first["destination"].clone()), (json!("waiting"), json!(["bob@far.example"]), json!("far.example")));
    // Search by recipient narrows it.
    let found = c.get("/api/v1/domains/one.test/queue?q=CAROL").await;
    assert_eq!(found.json()["items"].as_array().unwrap().len(), 1);

    let base = format!("/api/v1/domains/one.test/queue/{}", a.0);
    assert_eq!(c.post_empty(&format!("{base}/hold")).await.status, 200);
    let held = c.get("/api/v1/domains/one.test/queue?state=held").await;
    assert_eq!(held.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(held.json()["items"][0]["next_attempt_at"], json!(null), "a held job has no next attempt");
    assert_eq!(c.get("/api/v1/domains/one.test/queue?state=waiting").await.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(c.post_empty(&format!("{base}/release")).await.status, 200);
    assert_eq!(c.get("/api/v1/domains/one.test/queue?state=held").await.json()["items"].as_array().unwrap().len(), 0);
    // Retry is for what was given up: a waiting job answers wrong_state.
    let retry = c.post_empty(&format!("{base}/retry")).await;
    assert_eq!((retry.status, retry.json()["error"].clone()), (409, json!("wrong_state")));
    assert_eq!(c.mutate("DELETE", &base, None).await.status, 200);
    assert_eq!(c.mutate("DELETE", &base, None).await.status, 409, "already gone");
    assert_eq!(c.get("/api/v1/domains/one.test/queue?state=bogus").await.status, 400);
}

#[tokio::test]
async fn the_trace_shows_the_delivery_log_of_the_domain_and_keeps_to_it() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@two.test", true).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    let dom = r.dir.find_domain("one.test").await.unwrap().unwrap().id;
    let row = |to: &str, event: &'static str, detail: &str| NewLogRow {
        domain: dom,
        job_id: Some(7),
        sender: "alice@one.test".into(),
        recipient: to.into(),
        event,
        remote: Some("mx.far.example".into()),
        status: Some("2.0.0".into()),
        detail: (!detail.is_empty()).then(|| detail.to_owned()),
    };
    // The worker writes with its own role; the admin web only reads.
    r.outbound.repos().queue.log(&[row("bob@far.example", "deferred", "greylisted, try again later"), row("bob@far.example", "delivered", ""), row("carol@other.example", "bounced", "no such user")]).await.unwrap();
    let all = c.get("/api/v1/domains/one.test/trace").await;
    assert_eq!(all.status, 200, "{}", all.body);
    conforms(&all.json(), "Trace");
    let items = all.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["recipient"], "carol@other.example", "newest first");
    let bob = c.get("/api/v1/domains/one.test/trace?q=Bob%40far").await;
    assert_eq!(bob.json()["items"].as_array().unwrap().len(), 2);
    assert_eq!(bob.json()["items"][1]["event"], "deferred");
    // Another tenant's administrator cannot see it, and does not learn that the domain exists.
    let t = Client::sign_in(r.addr, "tenant@two.test").await;
    assert_eq!(t.get("/api/v1/domains/one.test/trace").await.status, 404);
    assert_eq!(t.get("/api/v1/domains/one.test/queue").await.status, 404);
    assert_eq!(t.post_empty("/api/v1/domains/one.test/queue/1/hold").await.status, 404);
}

#[tokio::test]
async fn the_statistics_count_the_log_per_day_and_kind_for_the_domain_only() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "tenant@two.test", true).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    let dom = r.dir.find_domain("one.test").await.unwrap().unwrap().id;
    let row = |event: &'static str| NewLogRow { domain: dom, job_id: None, sender: "a@one.test".into(), recipient: "b@far.example".into(), event, remote: None, status: None, detail: None };
    r.outbound.repos().queue.log(&[row("delivered"), row("delivered"), row("deferred"), row("bounced")]).await.unwrap();
    let got = c.get("/api/v1/domains/one.test/stats").await;
    assert_eq!(got.status, 200, "{}", got.body);
    conforms(&got.json(), "Stats");
    let days = got.json()["days"].as_array().unwrap().clone();
    assert_eq!(days.len(), 1, "one day so far");
    let d = &days[0];
    assert_eq!((d["sent"].clone(), d["deferred"].clone(), d["failed"].clone(), d["received"].clone()), (json!(2), json!(1), json!(1), json!(0)));
    let t = Client::sign_in(r.addr, "tenant@two.test").await;
    assert_eq!(t.get("/api/v1/domains/one.test/stats").await.status, 404);
}

#[tokio::test]
async fn the_days_of_the_log_are_rolled_up_and_the_statistics_read_older_days_from_the_roll_up() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    assert_eq!(c.post("/api/v1/domains", json!({ "name": "one.test" })).await.status, 201);
    let dom = r.dir.find_domain("one.test").await.unwrap().unwrap().id;
    let row = |event: &'static str| NewLogRow { domain: dom, job_id: None, sender: "a@one.test".into(), recipient: "b@far.example".into(), event, remote: None, status: None, detail: None };
    let queue = r.outbound.repos().queue.clone();
    queue.log(&[row("delivered"), row("delivered"), row("bounced")]).await.unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    // Two days later the day of the lines is complete and old enough to be rolled up; the log may be gone by then, the counts are not.
    let later = now + 2 * 86_400;
    assert!(queue.roll_up_stats(later).await.unwrap() >= 2, "a row per kind of line");
    assert_eq!(queue.roll_up_stats(later).await.unwrap(), queue.roll_up_stats(later).await.unwrap(), "repeating changes nothing");
    let far = now + 40 * 86_400;
    let rows = queue.stats(dom, 0, far).await.unwrap();
    let sent: i64 = rows.iter().filter(|x| x.direction == "out" && x.event == "delivered").map(|x| x.count).sum();
    let failed: i64 = rows.iter().filter(|x| x.event == "bounced").map(|x| x.count).sum();
    assert_eq!((sent, failed), (2, 1), "read from the roll-up: {rows:?}");
    // Nothing is counted twice while the log still holds the day.
    let live = queue.stats(dom, 0, now).await.unwrap();
    assert_eq!(live.iter().filter(|x| x.event == "delivered").map(|x| x.count).sum::<i64>(), 2);
    // The endpoint takes a range of days and refuses nonsense.
    assert_eq!(c.get("/api/v1/domains/one.test/stats?days=90").await.status, 200);
    assert_eq!(c.get("/api/v1/domains/one.test/stats?days=0").await.status, 400);
    assert_eq!(c.get("/api/v1/domains/one.test/stats?days=366").await.status, 400);
    assert!(queue.purge_stats(now.div_euclid(86_400) + 100, 100).await.unwrap() >= 2, "rows past their retention are removed");
}
