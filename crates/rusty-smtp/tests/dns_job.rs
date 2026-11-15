//! The `dns.check` job: the worker generates the records of a domain, checks them against a DNS source and records the observations as drift state.
mod common;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rusty_core::config::OutboundConfig;
use rusty_core::dns::{records_with_facts, DnsRecord, MailFacts, RecordType};
use rusty_core::dns_check::{record_hash, DnsFail, DnsSource, MxEntry, SrvEntry};
use rusty_smtp::outbound::dns_job::{enqueue_dns_check, DnsCheckWorker};
use rusty_store::DnsStore;
use rusty_svc::Service;
use tokio_util::sync::CancellationToken;

const IPS: [IpAddr; 1] = [IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5))];

#[derive(Default)]
struct Zone {
    txt: BTreeMap<String, Vec<String>>,
    mx: BTreeMap<String, Vec<MxEntry>>,
    addrs: BTreeMap<String, Vec<IpAddr>>,
    ptr: BTreeMap<IpAddr, Vec<String>>,
}

impl DnsSource for Zone {
    async fn txt(&self, name: &str) -> Result<Vec<String>, DnsFail> {
        Ok(self.txt.get(name).cloned().unwrap_or_default())
    }
    async fn mx(&self, name: &str) -> Result<Vec<MxEntry>, DnsFail> {
        Ok(self.mx.get(name).cloned().unwrap_or_default())
    }
    async fn addresses(&self, name: &str) -> Result<Vec<IpAddr>, DnsFail> {
        Ok(self.addrs.get(name).cloned().unwrap_or_default())
    }
    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, DnsFail> {
        Ok(self.ptr.get(&ip).cloned().unwrap_or_default())
    }
    async fn srv(&self, _: &str) -> Result<Vec<SrvEntry>, DnsFail> {
        Ok(Vec::new())
    }
}

fn facts() -> MailFacts<'static> {
    MailFacts { mail_host: "mail.example.net", public_ips: &IPS, spf_include: None }
}

/// A zone that publishes exactly what the generator says for `example.org` (no DKIM key yet).
fn published(records: &[DnsRecord]) -> Zone {
    let mut z = Zone::default();
    for r in records {
        let name = r.name.trim_end_matches('.').to_ascii_lowercase();
        match r.rtype {
            RecordType::Mx => {
                let (p, h) = r.value.split_once(' ').unwrap();
                z.mx.entry(name).or_default().push(MxEntry { preference: p.parse().unwrap(), host: h.trim_end_matches('.').to_owned() });
            }
            RecordType::A | RecordType::Aaaa => z.addrs.entry(name).or_default().push(r.value.parse().unwrap()),
            RecordType::Txt if !name.starts_with('<') => z.txt.entry(name).or_default().push(r.value.clone()),
            _ => {}
        }
    }
    z.ptr.insert(IPS[0], vec!["mail.example.net".into()]);
    z
}

fn cfg() -> OutboundConfig {
    OutboundConfig { public_ips: IPS.to_vec(), ..OutboundConfig::default() }
}

async fn run_job(b: &Backend, zone: Zone) {
    let (ctx, _blobs) = make_context(b).await;
    let dns = DnsStore::with_repo(b.store.repos().dns.clone());
    let worker = DnsCheckWorker::new(dns, cfg(), "mail.example.net", Arc::new(zone)).with_poll(Duration::from_millis(50));
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let (ctx, token) = (ctx.clone(), token.clone());
        async move { Box::new(worker).run(ctx, token).await }
    });
    let id = b.dir.find_domain("example.org").await.unwrap().unwrap().id;
    enqueue_dns_check(&ctx.jobs, id, "example.org").await.unwrap();
    for _ in 0..100 {
        if b.fx.scalar("SELECT count(*) FROM jobs WHERE kind = 'dns.check' AND state = 'done'", &[]).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_published_domain_is_recorded_ok_and_an_unpublished_one_missing() {
    for_each_backend(|b| async move {
        let id = b.dir.create_domain("example.org").await.unwrap();
        let (records, _) = records_with_facts("example.org", &facts(), &[]).unwrap();
        run_job(&b, published(&records)).await;
        let dns = DnsStore::with_repo(b.store.repos().dns.clone());
        let states = dns.states(id).await.unwrap();
        assert!(!states.is_empty(), "the observations were recorded");
        // The required records are ok, each state is keyed like the record it describes, and the placeholder of the missing DKIM key is not recorded.
        for r in records.iter().filter(|r| !r.name.starts_with('<') && r.required == rusty_core::dns::Requirement::Required) {
            let st = states.iter().find(|s| s.key.name == r.name.to_ascii_lowercase() && s.key.rtype == r.rtype.as_str() && s.key.purpose == r.purpose).unwrap_or_else(|| panic!("{} {}", r.name, r.purpose));
            assert_eq!(st.status.as_str(), "ok", "{} {}", r.name, r.purpose);
            assert_eq!(st.expected_hash, record_hash(r));
        }
        assert!(states.iter().all(|s| !s.key.name.starts_with('<')));
        // A second check against an empty zone: the same records are now missing.
        b.fx.run("DELETE FROM jobs", &[]).await;
        run_job(&b, Zone::default()).await;
        let states = dns.states(id).await.unwrap();
        assert!(states.iter().any(|s| s.status.as_str() == "missing"), "{:?}", states.iter().map(|s| s.status.as_str()).collect::<Vec<_>>());
    })
    .await;
}
