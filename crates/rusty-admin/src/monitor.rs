//! The monitor endpoint (docs/design-admin-monitor.md): one read for platform administrators that shows the services (from their heartbeats), the outbound queue, the storage checks
//! and the DNS problems. The status of a service is decided here, from the age of its heartbeats by the database clock; the browser only displays it.
//!
//! Nothing here is secret and nothing is a tenant's mail: the figures are counts and ages, the heartbeats are the services' own names, versions and uptimes.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_smtp::outbound_queue::OUTBOUND_JOB_KIND;
use rusty_store::HeartbeatRow;
use serde::Serialize;

use crate::api::{fail, AdminApp};
use crate::manage::platform_only;

/// The services of a deployment, in the order the page shows them.
pub const SERVICES: [&str; 6] = ["smtp-in", "submission", "imap", "outbound", "gc", "admin"];

/// An instance that has not beaten for this long (three beats of 15 seconds) is stale.
pub const STALE_AFTER_SECS: i64 = 45;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct InstanceJson {
    instance: String,
    version: String,
    uptime_secs: i64,
    age_secs: i64,
    stale: bool,
    status: String,
    /// The number of listeners the instance reported, when it did.
    listeners: Option<i64>,
}

#[derive(Serialize)]
struct ServiceJson {
    service: &'static str,
    status: &'static str,
    instances: Vec<InstanceJson>,
}

#[derive(Serialize)]
struct QueueJson {
    queued: i64,
    running: i64,
    failed: i64,
    dead: i64,
    oldest_due_secs: i64,
}

#[derive(Serialize)]
struct CountJson {
    status: String,
    count: i64,
}

#[derive(Serialize)]
struct MonitorJson {
    services: Vec<ServiceJson>,
    queue: QueueJson,
    storage: Vec<CountJson>,
    dns_problem_domains: i64,
}

/// `down`: no instance beats; `degraded`: one that does beat reports a problem, or another has stopped beating; `ok` otherwise.
pub(crate) fn service_status(instances: &[InstanceJsonView]) -> &'static str {
    let fresh: Vec<&InstanceJsonView> = instances.iter().filter(|i| !i.stale).collect();
    if fresh.is_empty() {
        "down"
    } else if fresh.iter().any(|i| i.status != "ok") || fresh.len() < instances.len() {
        "degraded"
    } else {
        "ok"
    }
}

/// What `service_status` looks at of an instance.
pub(crate) struct InstanceJsonView<'a> {
    pub stale: bool,
    pub status: &'a str,
}

fn instance(h: &HeartbeatRow) -> InstanceJson {
    let listeners = serde_json::from_str::<serde_json::Value>(&h.detail).ok().and_then(|v| v.get("listeners").and_then(serde_json::Value::as_i64));
    InstanceJson { instance: h.instance.clone(), version: h.version.clone(), uptime_secs: h.uptime_secs, age_secs: h.age_secs, stale: h.age_secs > STALE_AFTER_SECS, status: h.status.clone(), listeners }
}

/// Everything the monitor shows, read from the stores (the page and the metrics collector share it).
async fn snapshot(monitor: &dyn rusty_store::HeartbeatRepo, jobs: &rusty_store::JobQueue) -> rusty_core::Result<MonitorJson> {
    let rows = monitor.list().await?;
    let (counts, oldest_due_secs) = jobs.platform_figures(OUTBOUND_JOB_KIND).await?;
    let storage = monitor.storage_check_counts().await?;
    let dns_problem_domains = monitor.dns_problem_domains().await?;
    let services = SERVICES
        .iter()
        .map(|name| {
            let instances: Vec<InstanceJson> = rows.iter().filter(|h| h.service == *name).map(instance).collect();
            let views: Vec<InstanceJsonView> = instances.iter().map(|i| InstanceJsonView { stale: i.stale, status: &i.status }).collect();
            ServiceJson { service: name, status: service_status(&views), instances }
        })
        .collect();
    let n = |state: &str| counts.iter().find(|(s, _)| s == state).map_or(0, |(_, c)| *c);
    Ok(MonitorJson {
        services,
        queue: QueueJson { queued: n("queued"), running: n("running"), failed: n("failed"), dead: n("dead"), oldest_due_secs },
        storage: storage.into_iter().map(|(status, count)| CountJson { status, count }).collect(),
        dns_problem_domains,
    })
}

/// The same figures as gauges for Prometheus (docs/design-metrics.md): the admin web is the one process that sees the whole platform.
fn samples_of(m: &MonitorJson) -> Vec<rusty_metrics::Sample> {
    use rusty_metrics::{Kind::Gauge, Sample};
    let mut out = Vec::new();
    for s in &m.services {
        out.push(Sample::new("rusty_service_up", "1 when at least one instance of the service beats, else 0.", Gauge, &[("service", s.service)], if s.status == "down" { 0.0 } else { 1.0 }));
        let live = s.instances.iter().filter(|i| !i.stale).count();
        out.push(Sample::new("rusty_service_instances", "Instances of a service by state (live: beating, stale: stopped beating).", Gauge, &[("service", s.service), ("state", "live")], live as f64));
        out.push(Sample::new("rusty_service_instances", "Instances of a service by state (live: beating, stale: stopped beating).", Gauge, &[("service", s.service), ("state", "stale")], (s.instances.len() - live) as f64));
    }
    for (state, n) in [("queued", m.queue.queued), ("running", m.queue.running), ("failed", m.queue.failed), ("dead", m.queue.dead)] {
        out.push(Sample::new("rusty_outbound_queue_jobs", "Jobs of the outbound queue by state.", Gauge, &[("state", state)], n as f64));
    }
    out.push(Sample::new("rusty_outbound_queue_oldest_due_seconds", "How long the oldest due queued job has waited, by the database clock.", Gauge, &[], m.queue.oldest_due_secs as f64));
    for c in &m.storage {
        out.push(Sample::new("rusty_storage_stores", "Blob stores of domains by the state of their last check.", Gauge, &[("status", &c.status)], c.count as f64));
    }
    out.push(Sample::new("rusty_dns_problem_domains", "Domains with a missing or mismatching DNS record.", Gauge, &[], m.dns_problem_domains as f64));
    out
}

/// The collector the admin web registers: it reads the monitor's figures at every scrape.
pub struct PlatformCollector {
    pub monitor: Arc<dyn rusty_store::HeartbeatRepo>,
    pub jobs: rusty_store::JobQueue,
}

#[async_trait::async_trait]
impl rusty_metrics::Collector for PlatformCollector {
    fn name(&self) -> &'static str {
        "platform"
    }
    async fn collect(&self) -> std::result::Result<Vec<rusty_metrics::Sample>, String> {
        snapshot(self.monitor.as_ref(), &self.jobs).await.map(|m| samples_of(&m)).map_err(|e| e.to_string())
    }
}

async fn monitor_get(app: &AdminApp, req: Request) -> Response {
    let (_session, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p) {
        return r;
    }
    if req.query.is_some() {
        return err(400, "invalid");
    }
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match snapshot(s.monitor.as_ref(), &s.jobs).await {
        Ok(body) => json(200, &body),
        Err(e) => fail(&e),
    }
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    router.route(
        Method::Get,
        "/api/v1/monitor",
        handler(move |r: Request| {
            let a = app.clone();
            async move { monitor_get(&a, r).await }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::{samples_of, service_status, CountJson, InstanceJson, InstanceJsonView, MonitorJson, QueueJson, ServiceJson};

    fn inst(name: &str, stale: bool) -> InstanceJson {
        InstanceJson { instance: name.into(), version: "1".into(), uptime_secs: 5, age_secs: if stale { 99 } else { 3 }, stale, status: "ok".into(), listeners: None }
    }

    #[test]
    fn the_gauges_follow_the_monitor_figures() {
        let m = MonitorJson {
            services: vec![
                ServiceJson { service: "imap", status: "degraded", instances: vec![inst("a", false), inst("b", true)] },
                ServiceJson { service: "gc", status: "down", instances: vec![] },
            ],
            queue: QueueJson { queued: 4, running: 1, failed: 0, dead: 2, oldest_due_secs: 77 },
            storage: vec![CountJson { status: "failed".into(), count: 3 }],
            dns_problem_domains: 5,
        };
        let text = rusty_metrics::Sample::dump(&samples_of(&m));
        for line in [
            "rusty_service_up{service=\"imap\"} 1",
            "rusty_service_up{service=\"gc\"} 0",
            "rusty_service_instances{service=\"imap\",state=\"live\"} 1",
            "rusty_service_instances{service=\"imap\",state=\"stale\"} 1",
            "rusty_outbound_queue_jobs{state=\"dead\"} 2",
            "rusty_outbound_queue_oldest_due_seconds 77",
            "rusty_storage_stores{status=\"failed\"} 3",
            "rusty_dns_problem_domains 5",
        ] {
            assert!(text.lines().any(|l| l == line), "{line} in {text}");
        }
    }

    fn v(stale: bool, status: &str) -> InstanceJsonView<'_> {
        InstanceJsonView { stale, status }
    }

    #[test]
    fn a_service_is_down_degraded_or_ok_by_its_instances() {
        assert_eq!(service_status(&[]), "down", "no instance at all");
        assert_eq!(service_status(&[v(true, "ok")]), "down", "the only one stopped beating");
        assert_eq!(service_status(&[v(false, "ok")]), "ok");
        assert_eq!(service_status(&[v(false, "ok"), v(false, "ok")]), "ok");
        assert_eq!(service_status(&[v(false, "degraded")]), "degraded", "it beats and reports a problem");
        assert_eq!(service_status(&[v(false, "ok"), v(true, "ok")]), "degraded", "a replica is lost, another lives");
        assert_eq!(service_status(&[v(true, "ok"), v(true, "degraded")]), "down");
    }
}
