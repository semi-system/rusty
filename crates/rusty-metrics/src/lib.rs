//! Prometheus metrics (docs/design-metrics.md): a small registry of counters, gauges produced by collectors at scrape time, the text format, the `ServiceEvents` implementation that counts
//! what the listeners report, and a listener of its own that serves `GET /metrics` to the allowed scrapers only.
//!
//! Nothing here is a third-party exposition library: the format is a few lines, and every name and label key is a fixed string of the code. Label VALUES come from fixed sets as well
//! (a listener name, a result word): an address, a user or a domain is never a label, so the cardinality is bounded by the code, not by the traffic.

#![forbid(unsafe_code)]

mod federate;
pub use federate::{Federation, Target};

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusty_core::cidr::Cidr;
use rusty_core::{Error, Result};
use rusty_http::{handler, HttpConfig, HttpHandler, Method, Request, Response, Router};
use rusty_svc::{Accept, BoundListener, DirectTcp, ListenerSpec, RejectReason, ServiceEvent, ServiceEvents, TracingEvents};
use tokio_util::sync::CancellationToken;

/// How long a collector (a read of the database, say) may take before the scrape goes on without it.
pub const COLLECTOR_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        }
    }
}

/// One value of a metric: its name, help text, kind, labels and number.
#[derive(Debug, Clone)]
pub struct Sample {
    name: &'static str,
    help: &'static str,
    kind: Kind,
    labels: Vec<(&'static str, String)>,
    value: f64,
}

impl Sample {
    /// The text format of these samples alone (what a scrape adds for them): for tests of a collector.
    pub fn dump(samples: &[Sample]) -> String {
        encode(samples)
    }

    pub fn new(name: &'static str, help: &'static str, kind: Kind, labels: &[(&'static str, &str)], value: f64) -> Sample {
        Sample { name, help, kind, labels: labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(), value }
    }
}

/// Something that produces gauges when the registry is scraped (the admin web reads the heartbeats and the queue this way). A failure is counted and its text is not exposed.
#[async_trait]
pub trait Collector: Send + Sync {
    fn name(&self) -> &'static str;
    async fn collect(&self) -> std::result::Result<Vec<Sample>, String>;
}

type CounterKey = (&'static str, Vec<(&'static str, String)>);

/// The counters of one service process, the collectors that add gauges, and the facts of the build.
pub struct Registry {
    service: &'static str,
    version: String,
    started: Instant,
    counters: Mutex<BTreeMap<CounterKey, u64>>,
    collectors: Mutex<Vec<Arc<dyn Collector>>>,
    federation: Mutex<Option<Federation>>,
    timeout: Duration,
}

/// The help text of every counter this crate knows by name; a counter of another name gets a generic text.
fn help_of(name: &str) -> &'static str {
    match name {
        "rusty_connections_accepted_total" => "Connections accepted, by listener.",
        "rusty_connections_rejected_total" => "Connections refused before a session started, by listener and reason.",
        "rusty_connections_closed_total" => "Connections that ended, by listener.",
        "rusty_session_panics_total" => "Sessions that ended in a panic, by listener.",
        "rusty_auth_attempts_total" => "Authentication attempts, by listener and result.",
        "rusty_filter_results_total" => "Inbound scan results, by scanner and result (error: the scanner could not be asked).",
        "rusty_scrape_errors_total" => "Collectors that failed or were too slow during a scrape, by collector.",
        _ => "A counter of this service.",
    }
}

impl Registry {
    pub fn new(service: &'static str, version: &str) -> Registry {
        Registry { service, version: version.to_owned(), started: Instant::now(), counters: Mutex::new(BTreeMap::new()), collectors: Mutex::new(Vec::new()), federation: Mutex::new(None), timeout: COLLECTOR_TIMEOUT }
    }

    pub fn with_collector_timeout(mut self, timeout: Duration) -> Registry {
        self.timeout = timeout;
        self
    }

    /// Make this registry the central endpoint: a scrape also reads the targets and serves their samples with `service` and `instance` labels.
    pub fn set_federation(&self, f: Federation) {
        *self
            .federation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(f);
    }

    pub fn add_collector(&self, c: Arc<dyn Collector>) {
        self.collectors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(c);
    }

    /// Count one more of the counter `name` with these labels.
    pub fn inc(&self, name: &'static str, labels: &[(&'static str, &str)]) {
        let mut key: Vec<(&'static str, String)> = labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect();
        key.sort();
        *self.counters.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry((name, key)).or_insert(0) += 1;
    }

    /// The text of a scrape: counters, the build facts, the uptime, then whatever the collectors give.
    pub async fn render(&self) -> String {
        let collectors: Vec<Arc<dyn Collector>> = self.collectors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let mut gathered: Vec<Sample> = Vec::new();
        for c in collectors {
            match tokio::time::timeout(self.timeout, c.collect()).await {
                Ok(Ok(samples)) => gathered.extend(samples),
                Ok(Err(_)) | Err(_) => {
                    // The reason stays in the log, never in the exposition.
                    tracing::warn!(collector = c.name(), "a collector failed or was too slow during a scrape");
                    self.inc("rusty_scrape_errors_total", &[("collector", c.name())]);
                }
            }
        }
        let mut all: Vec<Sample> = Vec::new();
        all.push(Sample::new("rusty_build_info", "The service and its version (always 1).", Kind::Gauge, &[("service", self.service), ("version", &self.version)], 1.0));
        all.push(Sample::new("rusty_process_uptime_seconds", "Seconds since this process started.", Kind::Gauge, &[], self.started.elapsed().as_secs_f64().floor()));
        for ((name, labels), n) in self.counters.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter() {
            all.push(Sample { name, help: help_of(name), kind: Kind::Counter, labels: labels.clone(), value: *n as f64 });
        }
        all.extend(gathered);
        let own = encode(&all);
        let federation = self
            .federation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match federation {
            Some(f) => f.merge_with(self.service, &own).await,
            None => own,
        }
    }
}

fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// The text format 0.0.4: one `# HELP` and `# TYPE` per name, then its samples with the labels in key order.
fn encode(samples: &[Sample]) -> String {
    let mut by_name: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        by_name.entry(s.name).or_default().push(s);
    }
    let mut out = String::new();
    for (name, group) in by_name {
        out.push_str(&format!("# HELP {name} {}\n# TYPE {name} {}\n", group[0].help, group[0].kind.word()));
        for s in group {
            if !s.value.is_finite() {
                continue;
            }
            let mut labels = s.labels.clone();
            labels.sort();
            out.push_str(name);
            if !labels.is_empty() {
                let body: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect();
                out.push('{');
                out.push_str(&body.join(","));
                out.push('}');
            }
            out.push(' ');
            if s.value.fract() == 0.0 && s.value.abs() < 1e15 {
                out.push_str(&format!("{}", s.value as i64));
            } else {
                out.push_str(&format!("{}", s.value));
            }
            out.push('\n');
        }
    }
    out
}

fn reason_word(r: RejectReason) -> &'static str {
    match r {
        RejectReason::TotalLimit => "total_limit",
        RejectReason::PerIpLimit => "per_ip_limit",
        RejectReason::HandshakeFailed => "handshake_failed",
    }
}

/// The `ServiceEvents` that counts what the listeners report and still writes the `tracing` events of the default implementation.
pub struct MetricsEvents {
    registry: Arc<Registry>,
    tracing: TracingEvents,
}

impl MetricsEvents {
    pub fn new(registry: Arc<Registry>) -> MetricsEvents {
        MetricsEvents { registry, tracing: TracingEvents }
    }
}

impl ServiceEvents for MetricsEvents {
    fn emit(&self, event: ServiceEvent<'_>) {
        // The counters carry the listener's name only: never the peer.
        match &event {
            ServiceEvent::Accepted { service, .. } => self.registry.inc("rusty_connections_accepted_total", &[("listener", service)]),
            ServiceEvent::Rejected { service, reason, .. } => self.registry.inc("rusty_connections_rejected_total", &[("listener", service), ("reason", reason_word(*reason))]),
            ServiceEvent::Closed { service, .. } => self.registry.inc("rusty_connections_closed_total", &[("listener", service)]),
            ServiceEvent::SessionPanicked { service } => self.registry.inc("rusty_session_panics_total", &[("listener", service)]),
            ServiceEvent::AuthResult { service, ok, .. } => self.registry.inc("rusty_auth_attempts_total", &[("listener", service), ("result", if *ok { "ok" } else { "failed" })]),
            _ => {}
        }
        self.tracing.emit(event);
    }
}

async fn metrics_get(registry: &Registry, req: Request) -> Response {
    if req.query.is_some() {
        return Response::error(400, "invalid");
    }
    Response::new(200).with_header("content-type", "text/plain; version=0.0.4; charset=utf-8").with_header("cache-control", "no-store").with_body(registry.render().await)
}

/// How often the names of `allowed_hosts` are resolved again (a task of a swarm service comes and goes).
pub const HOSTS_REFRESH: Duration = Duration::from_secs(5);

type IpSet = std::collections::HashSet<std::net::IpAddr>;

/// Who may connect: address blocks, and the addresses that DNS names resolve to right now (Docker's DNS: a service name, `tasks.<service>` in a swarm). A peer in neither is dropped before a byte
/// is read; before the first resolution, and while no name resolves, no named peer is let in.
struct AllowPeers {
    inner: Arc<dyn Accept>,
    cidrs: Vec<Cidr>,
    named: Arc<std::sync::RwLock<IpSet>>,
}

#[async_trait]
impl Accept for AllowPeers {
    async fn accept(&self, tcp: tokio::net::TcpStream, addr: SocketAddr) -> Result<(rusty_svc::BoxedStream, rusty_svc::PeerInfo)> {
        let ip = addr.ip();
        let named = self.named.read().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&ip);
        if !named && !self.cidrs.iter().any(|c| c.contains(ip)) {
            return Err(Error::Invalid("peer is not allowed".into()));
        }
        self.inner.accept(tcp, addr).await
    }

    fn per_ip_after_accept(&self) -> bool {
        self.inner.per_ip_after_accept()
    }
}

/// Resolve every name; a name that fails keeps what it gave last time (a lookup that hiccups must not lock the scraper out), a name that answers replaces it.
async fn refresh(names: &[String], last: &mut [IpSet], into: &std::sync::RwLock<IpSet>) {
    for (i, n) in names.iter().enumerate() {
        if let Ok(Ok(it)) = tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((n.as_str(), 0))).await {
            last[i] = it.map(|a| a.ip()).collect();
        }
    }
    *into.write().unwrap_or_else(std::sync::PoisonError::into_inner) = last.iter().flatten().copied().collect();
}

/// Serve `GET /metrics` on `addr` to the peers in `allowed` only (any other peer is dropped before a byte is read). The list must name one to sixteen blocks and none that holds every
/// address: the endpoint has no login. Returns the bound address and the task that serves until `shutdown`.
pub async fn serve(addr: SocketAddr, registry: Arc<Registry>, allowed: Vec<Cidr>, shutdown: CancellationToken) -> Result<(SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
    serve_allowing(addr, registry, allowed, Vec::new(), shutdown).await
}

/// As `serve`, and the peers may also be named: `hosts` are DNS names (at most eight) whose addresses are let in, resolved again every few seconds. Blocks, names or both; at least one.
pub async fn serve_allowing(addr: SocketAddr, registry: Arc<Registry>, allowed: Vec<Cidr>, hosts: Vec<String>, shutdown: CancellationToken) -> Result<(SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
    if (allowed.is_empty() && hosts.is_empty()) || allowed.len() > 16 || hosts.len() > 8 || allowed.iter().any(Cidr::is_everything) {
        return Err(Error::Invalid("the metrics listener needs allowed networks or host names (at most sixteen and eight) and no network may hold every address".into()));
    }
    let router = Router::new().route(
        Method::Get,
        "/metrics",
        handler({
            let registry = registry.clone();
            move |req: Request| {
                let registry = registry.clone();
                async move { metrics_get(&registry, req).await }
            }
        }),
    );
    let http = Arc::new(HttpHandler::new(router, HttpConfig::default()));
    let named = Arc::new(std::sync::RwLock::new(IpSet::new()));
    if !hosts.is_empty() {
        let mut last = vec![IpSet::new(); hosts.len()];
        refresh(&hosts, &mut last, &named).await;
        let (named, shutdown) = (named.clone(), shutdown.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(HOSTS_REFRESH) => refresh(&hosts, &mut last, &named).await,
                }
            }
        });
    }
    let accept: Arc<dyn Accept> = Arc::new(AllowPeers { inner: Arc::new(DirectTcp), cidrs: allowed, named });
    let bound = BoundListener::bind(ListenerSpec { name: "metrics", addr, max_connections: 16, max_per_ip: 8 }).await?;
    let local = bound.local_addr();
    // The listener's own events are not counted in the registry it serves.
    let task = tokio::spawn(async move { bound.serve(accept, http, Arc::new(TracingEvents), shutdown, Duration::from_secs(2)).await });
    Ok((local, task))
}
