//! The central endpoint (docs/design-metrics.md, "One place to scrape"): one process (the admin web) reads the `/metrics` of every other service and serves everything in one exposition, so the
//! operator's Prometheus has a single target and cannot miss a service.
//!
//! A target is a service name and a `host:port`; the host may resolve to many addresses (`tasks.rusty-imap` in a swarm lists every replica, a compose service name every container): each address
//! is one instance. Every sample read from an instance gets the labels `service` and `instance`, and `rusty_scrape_target_up{service,instance}` says whether the read worked (a service that
//! cannot be reached shows as `0`, never as nothing). The reads are plain HTTP/1.0 with a short time limit and a size limit, in parallel.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long one instance has to answer (connect, send, read).
pub const TARGET_TIMEOUT: Duration = Duration::from_secs(2);
/// The most a single instance may send; more is treated as a failed read.
pub const MAX_BODY: usize = 1 << 20;
/// The most instances one target may expand to.
pub const MAX_INSTANCES_PER_TARGET: usize = 64;

/// A service to read: its name (a label value) and the `host:port` of its metrics listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub service: String,
    pub address: String,
}

/// The targets and the time limit of a read.
#[derive(Debug, Clone)]
pub struct Federation {
    pub targets: Vec<Target>,
    pub timeout: Duration,
}

/// What one instance gave: its labels and the text, or `None` when the read failed.
struct Read {
    service: String,
    instance: String,
    text: Option<String>,
}

impl Federation {
    pub fn new(targets: Vec<Target>) -> Federation {
        Federation {
            targets,
            timeout: TARGET_TIMEOUT,
        }
    }

    /// Read every instance of every target and return the merged exposition, to be joined with `own` (the text of the process itself, labelled `own_service`).
    pub async fn merge_with(&self, own_service: &str, own: &str) -> String {
        let mut set = tokio::task::JoinSet::new();
        for t in &self.targets {
            let (t, timeout) = (t.clone(), self.timeout);
            set.spawn(async move { read_target(&t, timeout).await });
        }
        let mut reads: Vec<Read> = Vec::new();
        while let Some(done) = set.join_next().await {
            if let Ok(r) = done {
                reads.extend(r);
            }
        }
        reads.sort_by(|a, b| (&a.service, &a.instance).cmp(&(&b.service, &b.instance)));
        let mut families: BTreeMap<String, Family> = BTreeMap::new();
        add_text(&mut families, own_service, "local", own);
        for r in &reads {
            let up = if r.text.is_some() { "1" } else { "0" };
            let f = families.entry("rusty_scrape_target_up".into()).or_default();
            f.help.get_or_insert_with(|| {
                "Whether the last read of this instance's metrics worked (1) or not (0).".into()
            });
            f.kind.get_or_insert_with(|| "gauge".into());
            f.lines.push(format!(
                "rusty_scrape_target_up{{instance=\"{}\",service=\"{}\"}} {up}",
                escape(&r.instance),
                escape(&r.service)
            ));
            if let Some(text) = &r.text {
                add_text(&mut families, &r.service, &r.instance, text);
            }
        }
        let mut out = String::new();
        for (name, f) in families {
            if let Some(h) = &f.help {
                out.push_str(&format!("# HELP {name} {h}\n"));
            }
            if let Some(k) = &f.kind {
                out.push_str(&format!("# TYPE {name} {k}\n"));
            }
            for l in f.lines {
                out.push_str(&l);
                out.push('\n');
            }
        }
        out
    }
}

#[derive(Default)]
struct Family {
    help: Option<String>,
    kind: Option<String>,
    lines: Vec<String>,
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Add the samples of `text` to the families, each with `service` and `instance` labels; one HELP and one TYPE per family, the first seen wins.
fn add_text(families: &mut BTreeMap<String, Family>, service: &str, instance: &str, text: &str) {
    let inst = format!("instance=\"{}\"", escape(instance));
    let with_service = format!("{inst},service=\"{}\"", escape(service));
    for line in text.lines() {
        if line.len() > 4096 {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# HELP ") {
            if let Some((name, help)) = rest.split_once(' ') {
                families
                    .entry(name.to_owned())
                    .or_default()
                    .help
                    .get_or_insert_with(|| help.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            if let Some((name, kind)) = rest.split_once(' ') {
                families
                    .entry(name.to_owned())
                    .or_default()
                    .kind
                    .get_or_insert_with(|| kind.to_owned());
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            let end = line.find(['{', ' ']).unwrap_or(line.len());
            let (name, rest) = line.split_at(end);
            // A service must not name its own instance; `rusty_build_info` already names the service (the same one).
            if rest.contains("instance=\"") {
                continue;
            }
            let labels = if rest.contains("service=\"") {
                &inst
            } else {
                &with_service
            };
            let labelled = match rest.strip_prefix('{') {
                Some(r) if r.starts_with('}') => format!("{name}{{{labels}{r}"),
                Some(r) => format!("{name}{{{labels},{r}"),
                None => format!("{name}{{{labels}}}{rest}"),
            };
            families
                .entry(name.to_owned())
                .or_default()
                .lines
                .push(labelled);
        }
    }
}

async fn read_target(t: &Target, timeout: Duration) -> Vec<Read> {
    let addrs: Vec<SocketAddr> =
        match tokio::time::timeout(timeout, tokio::net::lookup_host(t.address.as_str())).await {
            Ok(Ok(it)) => {
                let mut v: Vec<SocketAddr> = it.collect();
                v.sort();
                v.dedup();
                v.truncate(MAX_INSTANCES_PER_TARGET);
                v
            }
            _ => Vec::new(),
        };
    if addrs.is_empty() {
        return vec![Read {
            service: t.service.clone(),
            instance: t.address.clone(),
            text: None,
        }];
    }
    let mut set = tokio::task::JoinSet::new();
    for a in addrs {
        let host = t.address.clone();
        set.spawn(async move { (a, fetch(a, &host, timeout).await) });
    }
    let mut out = Vec::new();
    while let Some(done) = set.join_next().await {
        if let Ok((a, text)) = done {
            out.push(Read {
                service: t.service.clone(),
                instance: a.to_string(),
                text,
            });
        }
    }
    out
}

/// `GET /metrics` over plain HTTP/1.0: the body of a `200`, or `None`.
async fn fetch(addr: SocketAddr, host: &str, timeout: Duration) -> Option<String> {
    let work = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(
            format!("GET /metrics HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .ok()?;
        let mut buf = Vec::new();
        s.take(MAX_BODY as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .ok()?;
        if buf.len() > MAX_BODY {
            return None;
        }
        let text = String::from_utf8(buf).ok()?;
        let (head, body) = text.split_once("\r\n\r\n")?;
        let status = head.lines().next()?;
        (status.starts_with("HTTP/1.") && status.split(' ').nth(1) == Some("200"))
            .then(|| body.to_owned())
    };
    tokio::time::timeout(timeout, work).await.ok().flatten()
}
