//! The metrics registry, its text format, the event counters and the listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::cidr::Cidr;
use rusty_metrics::{serve, Collector, Kind, MetricsEvents, Registry, Sample};
use rusty_svc::{RejectReason, ServiceEvent, ServiceEvents};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

fn peer() -> SocketAddr {
    "192.0.2.7:4000".parse().unwrap()
}

fn cidr(s: &str) -> Cidr {
    s.parse().unwrap()
}

#[tokio::test]
async fn the_text_has_one_help_and_type_per_name_and_escapes_label_values() {
    let reg = Registry::new("imap", "1.2.3");
    reg.inc("rusty_filter_results_total", &[("scanner", "clamd"), ("result", "clean")]);
    reg.inc("rusty_filter_results_total", &[("scanner", "clamd"), ("result", "clean")]);
    reg.inc("rusty_filter_results_total", &[("scanner", "rspamd"), ("result", "we\"ird\\va\nlue")]);
    let text = reg.render().await;
    assert_eq!(text.matches("# TYPE rusty_filter_results_total counter").count(), 1, "{text}");
    assert_eq!(text.matches("# HELP rusty_filter_results_total ").count(), 1);
    assert!(text.contains("rusty_filter_results_total{result=\"clean\",scanner=\"clamd\"} 2\n"), "{text}");
    assert!(text.contains("result=\"we\\\"ird\\\\va\\nlue\""), "escaped: {text}");
    assert!(text.contains("rusty_build_info{service=\"imap\",version=\"1.2.3\"} 1\n"), "{text}");
    assert!(text.contains("# TYPE rusty_process_uptime_seconds gauge"));
    assert!(text.ends_with('\n'));
}

#[tokio::test]
async fn every_line_is_a_comment_or_a_valid_sample() {
    let reg = Registry::new("gc", "9");
    reg.inc("rusty_session_panics_total", &[("listener", "x")]);
    for line in reg.render().await.lines() {
        if line.starts_with('#') {
            assert!(line.starts_with("# HELP ") || line.starts_with("# TYPE "), "{line}");
            continue;
        }
        let name_end = line.find(['{', ' ']).expect("a sample has a value");
        let name = &line[..name_end];
        assert!(name.chars().next().unwrap().is_ascii_alphabetic() || name.starts_with('_'), "{line}");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'), "{line}");
        let value = line.rsplit(' ').next().unwrap();
        assert!(value.parse::<f64>().is_ok(), "{line}");
    }
}

#[tokio::test]
async fn service_events_become_counters_with_bounded_labels() {
    let reg = Arc::new(Registry::new("imap", "1"));
    let ev = MetricsEvents::new(reg.clone());
    ev.emit(ServiceEvent::Accepted { service: "imap-993", peer: peer() });
    ev.emit(ServiceEvent::Accepted { service: "imap-993", peer: "198.51.100.1:1".parse().unwrap() });
    ev.emit(ServiceEvent::Closed { service: "imap-993", peer: peer() });
    ev.emit(ServiceEvent::Rejected { service: "imap-993", peer: peer(), reason: RejectReason::PerIpLimit });
    ev.emit(ServiceEvent::SessionPanicked { service: "imap-993" });
    ev.emit(ServiceEvent::AuthResult { service: "imap-993", peer: peer(), ok: true });
    ev.emit(ServiceEvent::AuthResult { service: "imap-993", peer: peer(), ok: false });
    let text = reg.render().await;
    assert!(text.contains("rusty_connections_accepted_total{listener=\"imap-993\"} 2\n"), "{text}");
    assert!(text.contains("rusty_connections_closed_total{listener=\"imap-993\"} 1\n"));
    assert!(text.contains("rusty_connections_rejected_total{listener=\"imap-993\",reason=\"per_ip_limit\"} 1\n"));
    assert!(text.contains("rusty_session_panics_total{listener=\"imap-993\"} 1\n"));
    assert!(text.contains("rusty_auth_attempts_total{listener=\"imap-993\",result=\"ok\"} 1\n"));
    assert!(text.contains("rusty_auth_attempts_total{listener=\"imap-993\",result=\"failed\"} 1\n"));
    assert!(!text.contains("192.0.2.7") && !text.contains("198.51.100"), "no address is ever a label");
}

struct Fixed;

#[async_trait]
impl Collector for Fixed {
    fn name(&self) -> &'static str {
        "fixed"
    }
    async fn collect(&self) -> Result<Vec<Sample>, String> {
        Ok(vec![
            Sample::new("rusty_outbound_queue_jobs", "Jobs of the outbound queue by state.", Kind::Gauge, &[("state", "queued")], 3.0),
            Sample::new("rusty_outbound_queue_jobs", "Jobs of the outbound queue by state.", Kind::Gauge, &[("state", "dead")], 0.0),
        ])
    }
}

struct Broken;

#[async_trait]
impl Collector for Broken {
    fn name(&self) -> &'static str {
        "broken"
    }
    async fn collect(&self) -> Result<Vec<Sample>, String> {
        Err("the database is down".into())
    }
}

struct Slow;

#[async_trait]
impl Collector for Slow {
    fn name(&self) -> &'static str {
        "slow"
    }
    async fn collect(&self) -> Result<Vec<Sample>, String> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(vec![])
    }
}

#[tokio::test]
async fn collectors_add_gauges_and_a_failing_or_slow_one_does_not_break_the_scrape() {
    let reg = Registry::new("admin", "1").with_collector_timeout(Duration::from_millis(150));
    reg.add_collector(Arc::new(Fixed));
    reg.add_collector(Arc::new(Broken));
    reg.add_collector(Arc::new(Slow));
    let started = std::time::Instant::now();
    let text = reg.render().await;
    assert!(started.elapsed() < Duration::from_secs(3), "the slow collector was cut");
    assert!(text.contains("rusty_outbound_queue_jobs{state=\"queued\"} 3\n"), "{text}");
    assert_eq!(text.matches("# TYPE rusty_outbound_queue_jobs gauge").count(), 1);
    assert!(text.contains("rusty_scrape_errors_total{collector=\"broken\"} 1\n"), "{text}");
    assert!(text.contains("rusty_scrape_errors_total{collector=\"slow\"} 1\n"), "{text}");
    assert!(!text.contains("the database is down"), "the reason of a failure is not exposed");
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let _ = s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn only_an_allowed_peer_is_answered_and_only_on_the_metrics_path() {
    let reg = Arc::new(Registry::new("smtp-in", "2"));
    let stop = CancellationToken::new();
    let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), reg.clone(), vec![cidr("127.0.0.1/32")], stop.clone()).await.unwrap();
    let ok = get(addr, "/metrics").await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(ok.to_ascii_lowercase().contains("content-type: text/plain; version=0.0.4"), "{ok}");
    assert!(ok.contains("rusty_build_info{service=\"smtp-in\",version=\"2\"} 1"), "{ok}");
    assert!(get(addr, "/").await.starts_with("HTTP/1.1 404"));
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

    // A peer outside the list gets nothing: the connection is dropped before a byte is read.
    let stop = CancellationToken::new();
    let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), reg, vec![cidr("10.0.0.1/32")], stop.clone()).await.unwrap();
    assert_eq!(get(addr, "/metrics").await, "", "dropped");
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn an_empty_or_unbounded_allow_list_is_refused() {
    let reg = Arc::new(Registry::new("gc", "1"));
    assert!(serve("127.0.0.1:0".parse().unwrap(), reg.clone(), vec![], CancellationToken::new()).await.is_err());
    assert!(serve("127.0.0.1:0".parse().unwrap(), reg, vec![cidr("0.0.0.0/0")], CancellationToken::new()).await.is_err());
}
