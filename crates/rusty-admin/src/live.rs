//! Live updates (docs/design-live-updates.md): `GET /api/v1/events`, a server-sent event stream of **hints** that something changed.
//!
//! A hint names a topic (`storage:example.test`) and carries nothing else: the page answers it by asking the ordinary API again, so authorization and the isolation of tenants
//! stay in the endpoints and the stream cannot leak what the API would not show. The [`Hub`] keeps the subscribers of this process; a handler that changes something calls
//! [`AdminApp::announce`](crate::api::AdminApp::announce) and every stream that may see the topic gets a hint. A stream:
//!
//! * starts with `resync` (refresh everything you show) and ends after [`MAX_LIFETIME`] (the browser reconnects and gets `resync` again);
//! * has a bounded queue of hints; a stream that does not keep up is sent one `resync` instead of the hints it missed (hints are idempotent);
//! * looks at its session on every keep-alive **without renewing it** (an open stream does not keep a session alive) and ends when the session, the grant or the account is gone;
//! * is limited per session ([`MAX_PER_SESSION`]) and per process ([`MAX_STREAMS`]);
//! * for a platform administrator also ticks `monitor` every [`MONITOR_EVERY`] (the heartbeats change all the time; nothing announces them).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rusty_http::{handler, Method, Request, Response, Router};
use tokio::sync::mpsc;

use crate::api::{AdminApp, COOKIE};
use crate::auth::Scope;

/// Streams one session may hold at once (several tabs).
pub const MAX_PER_SESSION: usize = 4;
/// Streams this process serves at once.
pub const MAX_STREAMS: usize = 256;
/// A stream ends after this long; the browser reconnects by itself.
pub const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// A comment line keeps proxies and the edge from closing a quiet connection, and is when the session is looked at again.
pub const KEEP_ALIVE: Duration = Duration::from_secs(20);
/// How often a platform administrator's stream says `monitor` changed.
pub const MONITOR_EVERY: Duration = Duration::from_secs(5);
/// Hints a stream may have waiting; more than this and the stream is told to resynchronise instead.
const QUEUE: usize = 64;
/// A burst of hints is merged for this long before it is written.
const MERGE: Duration = Duration::from_millis(100);

/// What a page can watch. The domain is a name, lower case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Topic {
    /// The list of domains and their status.
    Domains,
    /// The administrators.
    Admins,
    /// Services, queue and storage figures of the whole installation (platform administrators only).
    Monitor,
    /// Accounts and aliases of a domain.
    Accounts(String),
    /// DNS records and DKIM keys of a domain.
    Dns(String),
    /// The relay and the queue figures of a domain.
    Outbound(String),
    /// The store and the pool of a domain.
    Storage(String),
}

impl Topic {
    pub fn name(&self) -> String {
        match self {
            Topic::Domains => "domains".into(),
            Topic::Admins => "admins".into(),
            Topic::Monitor => "monitor".into(),
            Topic::Accounts(d) => format!("accounts:{d}"),
            Topic::Dns(d) => format!("dns:{d}"),
            Topic::Outbound(d) => format!("outbound:{d}"),
            Topic::Storage(d) => format!("storage:{d}"),
        }
    }

    fn domain(&self) -> Option<&str> {
        match self {
            Topic::Accounts(d) | Topic::Dns(d) | Topic::Outbound(d) | Topic::Storage(d) => Some(d),
            _ => None,
        }
    }
}

/// Who a stream belongs to: every domain, or one (by name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    Platform,
    Domain(String),
}

impl Audience {
    /// A domain administrator is told about the list of domains and the administrators (the hint carries nothing; its own pages ask the API, which shows its own domain only), and about
    /// the topics of its own domain; never about another domain's, and never about `monitor`.
    fn sees(&self, topic: &Topic) -> bool {
        match (self, topic) {
            (Audience::Platform, _) => true,
            (Audience::Domain(_), Topic::Monitor) => false,
            (Audience::Domain(_), Topic::Domains | Topic::Admins) => true,
            (Audience::Domain(mine), t) => t.domain() == Some(mine.as_str()),
        }
    }
}

struct Sub {
    audience: Audience,
    tx: mpsc::Sender<Topic>,
    /// Set when a hint could not be queued: the stream sends `resync` instead.
    overflow: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    subs: HashMap<u64, Sub>,
    per_session: HashMap<String, usize>,
}

/// The subscribers of this process.
pub struct Hub {
    inner: Mutex<Inner>,
    keep_alive: Duration,
    monitor_every: Duration,
}

impl Default for Hub {
    fn default() -> Hub {
        Hub::timed(KEEP_ALIVE, MONITOR_EVERY)
    }
}

/// One stream's place in the hub; dropping it frees the place.
pub struct Subscription {
    hub: Arc<Hub>,
    id: u64,
    session: String,
    pub rx: mpsc::Receiver<Topic>,
    pub overflow: Arc<AtomicBool>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut g) = self.hub.inner.lock() {
            g.subs.remove(&self.id);
            if let Some(n) = g.per_session.get_mut(&self.session) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    g.per_session.remove(&self.session);
                }
            }
        }
    }
}

impl Hub {
    fn timed(keep_alive: Duration, monitor_every: Duration) -> Hub {
        Hub { inner: Mutex::new(Inner::default()), keep_alive, monitor_every }
    }

    pub fn new() -> Arc<Hub> {
        Arc::new(Hub::default())
    }

    /// A hub with other keep-alive and `monitor` intervals (tests: the real ones are 20 and 5 seconds).
    pub fn with_timing(keep_alive: Duration, monitor_every: Duration) -> Arc<Hub> {
        Arc::new(Hub::timed(keep_alive, monitor_every))
    }

    /// A place for a stream of `session`, or `None` when the session or the process is at its limit.
    pub fn subscribe(self: &Arc<Hub>, session: &str, audience: Audience) -> Option<Subscription> {
        let mut g = self.inner.lock().ok()?;
        if g.subs.len() >= MAX_STREAMS || g.per_session.get(session).copied().unwrap_or(0) >= MAX_PER_SESSION {
            return None;
        }
        g.next += 1;
        let id = g.next;
        let (tx, rx) = mpsc::channel(QUEUE);
        let overflow = Arc::new(AtomicBool::new(false));
        g.subs.insert(id, Sub { audience, tx, overflow: overflow.clone() });
        *g.per_session.entry(session.to_owned()).or_insert(0) += 1;
        Some(Subscription { hub: self.clone(), id, session: session.to_owned(), rx, overflow })
    }

    /// Tell every stream that may see `topic`. Never waits: a stream with a full queue is marked to resynchronise.
    pub fn announce(&self, topic: &Topic) {
        let Ok(g) = self.inner.lock() else { return };
        for sub in g.subs.values().filter(|s| s.audience.sees(topic)) {
            if sub.tx.try_send(topic.clone()).is_err() {
                sub.overflow.store(true, Ordering::SeqCst);
            }
        }
    }

    #[cfg(test)]
    fn streams(&self) -> usize {
        self.inner.lock().map(|g| g.subs.len()).unwrap_or(0)
    }
}

/// What a request changes, from its method and path (empty for a read). The topics are announced after the request succeeded.
pub fn topics_of(req: &Request) -> Vec<Topic> {
    if req.method == Method::Get || req.method == Method::Head {
        return Vec::new();
    }
    let seg: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
    // The domain of an address in a path segment, as the API names it.
    let of_address = |name: &str| req.param(name).and_then(|a| a.rsplit_once('@')).map(|(_, d)| d.to_ascii_lowercase());
    let of_domain = || req.param("d").map(str::to_ascii_lowercase);
    match seg.as_slice() {
        ["api", "v1", "domains"] => vec![Topic::Domains],
        ["api", "v1", "domains", _, "enable" | "disable"] => vec![Topic::Domains],
        ["api", "v1", "domains", _, "accounts" | "aliases"] => of_domain().map(Topic::Accounts).into_iter().collect(),
        ["api", "v1", "accounts", _, _] | ["api", "v1", "aliases", _] => of_address("a").map(Topic::Accounts).into_iter().collect(),
        ["api", "v1", "domains", _, "dkim"] | ["api", "v1", "domains", _, "dns", "check"] => of_domain().map(Topic::Dns).into_iter().collect(),
        ["api", "v1", "domains", _, "relay", ..] => of_domain().map(Topic::Outbound).into_iter().collect(),
        ["api", "v1", "domains", _, "storage", ..] => of_domain().map(Topic::Storage).into_iter().collect(),
        ["api", "v1", "admins", ..] => vec![Topic::Admins],
        _ => Vec::new(),
    }
}

fn frame(event: &str, data: &str) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn changed(topics: &HashSet<String>) -> Bytes {
    let mut names: Vec<&String> = topics.iter().collect();
    names.sort();
    // Topic names are made of letters, digits, dots, hyphens and a colon: nothing to escape.
    let list = names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(",");
    frame("changed", &format!("{{\"topics\":[{list}]}}"))
}

/// The task behind one open stream: writes frames into `out` until the client goes away, the session ends or the lifetime is over.
async fn run(app: Arc<AdminApp>, mut sub: Subscription, session_id: String, platform: bool, out: mpsc::Sender<Bytes>) {
    if out.send(frame("resync", "{}")).await.is_err() {
        return;
    }
    let deadline = tokio::time::Instant::now() + MAX_LIFETIME;
    let mut keep = tokio::time::interval_at(tokio::time::Instant::now() + app.live.keep_alive, app.live.keep_alive);
    let mut monitor = tokio::time::interval_at(tokio::time::Instant::now() + app.live.monitor_every, app.live.monitor_every);
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return,
            first = sub.rx.recv() => {
                let Some(first) = first else { return };
                // Merge a burst: whatever else arrives in a short while goes out in the same frame.
                let mut topics = HashSet::from([first.name()]);
                let merge = tokio::time::sleep(MERGE);
                tokio::pin!(merge);
                loop {
                    tokio::select! {
                        () = &mut merge => break,
                        more = sub.rx.recv() => match more {
                            Some(t) => { topics.insert(t.name()); }
                            None => return,
                        },
                    }
                }
                let write = if sub.overflow.swap(false, Ordering::SeqCst) { frame("resync", "{}") } else { changed(&topics) };
                if out.send(write).await.is_err() {
                    return;
                }
            }
            _ = monitor.tick(), if platform => {
                if sub.overflow.swap(false, Ordering::SeqCst) && out.send(frame("resync", "{}")).await.is_err() {
                    return;
                }
                if out.send(changed(&HashSet::from([Topic::Monitor.name()]))).await.is_err() {
                    return;
                }
            }
            _ = keep.tick() => {
                // Still the same administrator? Looked at without renewing the session.
                match app.auth.principal_peek(&session_id).await {
                    Ok(Some(_)) => {}
                    _ => return,
                }
                if out.send(Bytes::from_static(b": keep-alive\n\n")).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn events(app: Arc<AdminApp>, req: Request) -> Response {
    let (session, principal) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if req.query.is_some() {
        return Response::error(400, "invalid").with_header("cache-control", "no-store");
    }
    let Some(session_id) = req.cookie(COOKIE).map(str::to_owned) else { return Response::error(401, "unauthorized").with_header("cache-control", "no-store") };
    let audience = match principal.scope {
        Scope::Platform => Audience::Platform,
        // A domain administrator is an account of its domain: the domain is the one in its address.
        Scope::Domain(_) => match session.address.rsplit_once('@') {
            Some((_, d)) => Audience::Domain(d.to_ascii_lowercase()),
            None => return Response::error(404, "not_found").with_header("cache-control", "no-store"),
        },
    };
    let platform = audience == Audience::Platform;
    let Some(sub) = app.live.subscribe(&session_id, audience) else {
        return Response::error(429, "throttled").with_header("cache-control", "no-store").with_header("retry-after", "30");
    };
    let (out, body) = mpsc::channel::<Bytes>(8);
    tokio::spawn(run(app.clone(), sub, session_id, platform, out));
    Response::streaming(200, body)
        .with_header("content-type", "text/event-stream")
        .with_header("cache-control", "no-store")
        .with_header("x-accel-buffering", "no")
}

impl Hub {
    /// Tell every stream to refresh everything it shows (the database feed was away and what it announced meanwhile is lost). A stream whose queue is full already has that flag.
    pub fn resync_all(&self) {
        let Ok(g) = self.inner.lock() else { return };
        for sub in g.subs.values() {
            sub.overflow.store(true, Ordering::SeqCst);
            // The stream is woken by any hint: the flag turns it into a `resync`.
            let _ = sub.tx.try_send(Topic::Domains);
        }
    }
}

/// What the database announced, on its way from the feed's single task to the hub: the names of domains are looked up by a task of its own, never by the feed.
enum Heard {
    Change(rusty_store::AdminChange),
    Resync,
}

struct FeedSink(mpsc::UnboundedSender<Heard>);

impl rusty_store::AdminSink for FeedSink {
    fn changed(&self, change: rusty_store::AdminChange) {
        let _ = self.0.send(Heard::Change(change));
    }
    fn resync(&self) {
        let _ = self.0.send(Heard::Resync);
    }
    fn lost(&self) {
        tracing::warn!("the admin console's database feed lost its connection; it will retry (live updates of other processes' changes pause meanwhile)");
    }
}

/// Every domain's id and name (the directory has no lookup by id, and the whole list is short).
async fn domain_names(directory: &rusty_directory::Directory) -> std::collections::HashMap<i64, String> {
    let mut names = std::collections::HashMap::new();
    let mut after = None;
    loop {
        let Ok(page) = directory.list_domains(after, crate::manage::PAGE).await else { break };
        after = page.last().map(|d| d.id);
        let n = page.len();
        names.extend(page.into_iter().map(|d| (d.id.0, d.name)));
        if n < crate::manage::PAGE {
            break;
        }
    }
    names
}

/// The topics a change of the database concerns (a change of a domain that is gone has none).
fn topics_for(change: rusty_store::AdminChange, names: &std::collections::HashMap<i64, String>) -> Vec<Topic> {
    use rusty_store::AdminKind as K;
    let name = || change.domain.and_then(|id| names.get(&id)).cloned();
    match change.kind {
        K::Domains => vec![Topic::Domains],
        K::Admins => vec![Topic::Admins],
        K::Accounts => name().map(Topic::Accounts).into_iter().collect(),
        K::Dns => name().map(Topic::Dns).into_iter().collect(),
        K::Outbound => name().map(Topic::Outbound).into_iter().collect(),
        K::Storage => name().map(Topic::Storage).into_iter().collect(),
    }
}

/// Run the database feed for this process: what other processes write (a check the outbound service recorded, another replica's change, `rustyctl`) is announced to the open streams.
/// Without it the streams still carry what this process changes itself. Ends with `shutdown`.
pub fn spawn_feed(app: Arc<AdminApp>, feed: Arc<rusty_store::PgAdminFeed>, shutdown: tokio_util::sync::CancellationToken) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Heard>();
    let stop = shutdown.clone();
    tokio::spawn(async move {
        let _ = feed.run(Arc::new(FeedSink(tx)), stop).await;
    });
    tokio::spawn(async move {
        let mut names = std::collections::HashMap::new();
        loop {
            let heard = tokio::select! {
                () = shutdown.cancelled() => return,
                h = rx.recv() => match h { Some(h) => h, None => return },
            };
            match heard {
                Heard::Resync => {
                    names = domain_names(&app.directory).await;
                    app.live.resync_all();
                }
                Heard::Change(change) => {
                    // A domain that was created or renamed since the list was read: look again before giving up on its name.
                    if change.kind == rusty_store::AdminKind::Domains || change.domain.is_some_and(|id| !names.contains_key(&id)) {
                        names = domain_names(&app.directory).await;
                    }
                    for t in topics_for(change, &names) {
                        app.live.announce(&t);
                    }
                }
            }
        }
    });
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    router.route(Method::Get, "/api/v1/events", handler(move |r: Request| {
        let a = app.clone();
        async move { events(a, r).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: &str) -> Audience {
        Audience::Domain(n.into())
    }

    #[test]
    fn a_domain_administrator_sees_its_own_domain_and_the_shared_lists_and_nothing_else() {
        let a = d("one.test");
        assert!(a.sees(&Topic::Storage("one.test".into())));
        assert!(a.sees(&Topic::Accounts("one.test".into())));
        assert!(a.sees(&Topic::Domains) && a.sees(&Topic::Admins));
        assert!(!a.sees(&Topic::Storage("two.test".into())));
        assert!(!a.sees(&Topic::Dns("two.test".into())));
        assert!(!a.sees(&Topic::Monitor));
        // A name that merely contains another is not that domain.
        assert!(!d("one.test").sees(&Topic::Outbound("xone.test".into())));
        assert!(Audience::Platform.sees(&Topic::Monitor) && Audience::Platform.sees(&Topic::Dns("two.test".into())));
    }

    #[tokio::test]
    async fn the_limits_hold_and_a_dropped_subscription_frees_its_place() {
        let hub = Hub::new();
        let mut held = Vec::new();
        for _ in 0..MAX_PER_SESSION {
            held.push(hub.subscribe("s1", Audience::Platform).expect("within the limit"));
        }
        assert!(hub.subscribe("s1", Audience::Platform).is_none(), "one session, one more than allowed");
        assert!(hub.subscribe("s2", Audience::Platform).is_some(), "another session is not affected");
        held.pop();
        assert!(hub.subscribe("s1", Audience::Platform).is_some(), "a place was freed");
        drop(held);
        // Everything dropped: the hub is empty again.
        assert_eq!(hub.streams(), 0);
    }

    #[tokio::test]
    async fn the_process_limit_holds() {
        let hub = Hub::new();
        let mut held = Vec::new();
        for i in 0..MAX_STREAMS {
            held.push(hub.subscribe(&format!("s{i}"), Audience::Platform).unwrap());
        }
        assert!(hub.subscribe("another", Audience::Platform).is_none());
    }

    #[tokio::test]
    async fn a_slow_stream_is_marked_to_resynchronise_and_never_blocks_the_announcer() {
        let hub = Hub::new();
        let mut sub = hub.subscribe("s", Audience::Platform).unwrap();
        for _ in 0..(QUEUE + 10) {
            hub.announce(&Topic::Domains);
        }
        assert!(sub.overflow.load(Ordering::SeqCst), "what did not fit is a resync");
        let mut n = 0;
        while sub.rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, QUEUE, "the queue is bounded");
    }

    #[test]
    fn a_burst_is_one_frame_with_sorted_topics() {
        let t: HashSet<String> = ["storage:b.test", "accounts:a.test"].iter().map(|s| (*s).to_owned()).collect();
        let f = changed(&t);
        assert_eq!(&f[..], b"event: changed\ndata: {\"topics\":[\"accounts:a.test\",\"storage:b.test\"]}\n\n");
    }
}
