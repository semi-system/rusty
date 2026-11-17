//! The monitor endpoint: platform administrators only, the status of a service computed on the server from the heartbeats (the browser never decides what "stale" means), the queue,
//! storage and DNS figures, and the contract shape of the response.
mod common;

use common::*;

async fn beat(r: &Rig, service: &str, instance: &str, status: &str, detail: &str) {
    // The services write their heartbeat as themselves; this test plays them with the operator's login, which names the service itself.
    r.outbound.repos().heartbeat.beat(service, instance, "1.2.3", 90, status, detail).await.unwrap();
}

fn service<'a>(monitor: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    monitor["services"].as_array().unwrap().iter().find(|s| s["service"] == name).unwrap_or_else(|| panic!("no {name} in {monitor}"))
}

#[tokio::test]
async fn only_a_platform_administrator_sees_the_monitor() {
    let r = rig().await;
    assert_eq!(send(r.addr, "GET", "/api/v1/monitor", &[], "").await.status, 401);
    make_admin(&r, "root@home.test", false).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    make_admin(&r, "boss@one.test", true).await;
    let boss = Client::sign_in(r.addr, "boss@one.test").await;
    let denied = boss.get("/api/v1/monitor").await;
    assert_eq!((denied.status, denied.json()["error"].clone()), (403, "forbidden".into()));
    let ok = root.get("/api/v1/monitor").await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(ok.header("cache-control").as_deref(), Some("no-store"));
    assert_eq!(root.get("/api/v1/monitor?x=1").await.status, 400, "no query");
}

#[tokio::test]
async fn a_service_is_ok_degraded_or_down_by_its_heartbeats_as_the_database_sees_their_age() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;

    // Nothing has beaten: every service is listed, and every one is down.
    let m = root.get("/api/v1/monitor").await.json();
    conforms(&m, "Monitor");
    let names: Vec<&str> = m["services"].as_array().unwrap().iter().map(|s| s["service"].as_str().unwrap()).collect();
    assert_eq!(names, ["smtp-in", "submission", "imap", "outbound", "gc", "admin"]);
    assert!(m["services"].as_array().unwrap().iter().all(|s| s["status"] == "down" && s["instances"].as_array().unwrap().is_empty()));

    // Two live replicas of imap: ok. One reports a problem: degraded.
    beat(&r, "imap", "imap-1", "ok", "{\"listeners\":2}").await;
    beat(&r, "imap", "imap-2", "ok", "{}").await;
    let m = root.get("/api/v1/monitor").await.json();
    conforms(&m, "Monitor");
    let imap = service(&m, "imap");
    assert_eq!(imap["status"], "ok");
    assert_eq!(imap["instances"].as_array().unwrap().len(), 2);
    let first = &imap["instances"][0];
    assert_eq!((first["instance"].as_str(), first["version"].as_str(), first["listeners"].as_i64(), first["stale"].as_bool()), (Some("imap-1"), Some("1.2.3"), Some(2), Some(false)));
    assert!(first["uptime_secs"].as_i64().unwrap() >= 90);
    assert!(imap["instances"][1]["listeners"].is_null(), "no listener count reported");
    beat(&r, "imap", "imap-2", "degraded", "{}").await;
    assert_eq!(service(&root.get("/api/v1/monitor").await.json(), "imap")["status"], "degraded");

    // One replica stops beating (its last beat is old by the database clock): degraded while the other lives, down when none does.
    r.fx.run("UPDATE service_heartbeats SET last_seen = last_seen - 100 WHERE instance = 'imap-1'", &[]).await;
    // The survivor has been up for hours: it did not start after the other one stopped, so it did not replace it.
    r.outbound.repos().heartbeat.beat("imap", "imap-2", "1.2.3", 20_000, "ok", "{}").await.unwrap();
    let m = root.get("/api/v1/monitor").await.json();
    let imap = service(&m, "imap");
    assert_eq!(imap["status"], "degraded", "{imap}");
    assert_eq!(imap["instances"][0]["stale"], true);
    assert_eq!(imap["instances"][0]["replaced"], false, "a replica that was lost is a fault");
    assert_eq!(imap["instances"][1]["stale"], false);
    // A task that started after the stale one stopped replaces it (an upgrade): the old row is an old version, and the service is ok.
    r.fx.run("DELETE FROM service_heartbeats WHERE instance = 'imap-2'", &[]).await;
    r.outbound.repos().heartbeat.beat("imap", "imap-3", "1.2.4", 10, "ok", "{}").await.unwrap();
    let imap = service(&root.get("/api/v1/monitor").await.json(), "imap").clone();
    assert_eq!(imap["instances"][0]["replaced"], true, "{imap}");
    assert_eq!(imap["status"], "ok", "an old version that was replaced is not a fault: {imap}");
    // Back to the earlier picture: the upgraded task goes away and the survivor beats again.
    r.fx.run("DELETE FROM service_heartbeats WHERE instance = 'imap-3'", &[]).await;
    r.outbound.repos().heartbeat.beat("imap", "imap-2", "1.2.3", 20_000, "ok", "{}").await.unwrap();
    r.fx.run("UPDATE service_heartbeats SET last_seen = last_seen - 100 WHERE instance = 'imap-2'", &[]).await;
    assert_eq!(service(&root.get("/api/v1/monitor").await.json(), "imap")["status"], "down");
}

#[tokio::test]
async fn the_queue_storage_and_dns_figures_are_platform_wide() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let m = root.get("/api/v1/monitor").await.json();
    assert_eq!(m["queue"], serde_json::json!({ "queued": 0, "running": 0, "failed": 0, "dead": 0, "oldest_due_secs": 0 }));
    assert_eq!(m["storage"], serde_json::json!([]));
    assert_eq!(m["dns_problem_domains"], 0);

    root.post("/api/v1/domains", serde_json::json!({ "name": "one.test" })).await;
    root.post("/api/v1/domains", serde_json::json!({ "name": "two.test" })).await;
    let now = r.fx.db_clock().await;
    let jobs = rusty_store::JobQueue::with_repo(r.outbound.repos().jobs.clone());
    for (domain, run_after) in [("one.test", now - 200), ("two.test", now - 50), ("two.test", now + 3600)] {
        let id = r.dir.find_domain(domain).await.unwrap().unwrap().id;
        jobs.enqueue_at("outbound.send", Some(id), &serde_json::json!({}), Some(run_after), now).await.unwrap();
    }
    let m = root.get("/api/v1/monitor").await.json();
    conforms(&m, "Monitor");
    assert_eq!(m["queue"]["queued"], 3);
    assert!((200..=205).contains(&m["queue"]["oldest_due_secs"].as_i64().unwrap()), "{}", m["queue"]);
}
