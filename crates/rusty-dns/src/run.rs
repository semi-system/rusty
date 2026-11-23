//! The name server as a process (docs/design-authoritative-dns.md, section 2): it reads the hosted domains from the database, builds their zones, serves them, and reads again every few seconds. When the database cannot
//! be read it keeps answering from the last good zones, for at most `max_stale_secs`; after that it answers `REFUSED` for everything rather than serve data that may have been withdrawn.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusty_core::config::{DnsConfig, StartupConfig};
use rusty_core::dns::{records_with_facts, DnsRecord, MailFacts, RecordType, Requirement};
use rusty_core::{Error, Result};
use rusty_store::{DnsZoneRepo, ExtraRecord, HostedDomain, Store};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::build::{build_zone, ZoneSpec};
use crate::limit::Limits;
use crate::server::{start as start_listeners, Config as ServerConfig, ZoneHandle};
use crate::zone::{Zones};

/// An extra record of a tenant as a record to publish: the name is made absolute inside the domain.
fn extra_record(domain: &str, e: &ExtraRecord) -> Option<DnsRecord> {
    let name = if e.name == "@" { domain.to_owned() } else { format!("{}.{domain}", e.name) };
    let rtype = match e.rtype.as_str() {
        "A" => RecordType::A,
        "AAAA" => RecordType::Aaaa,
        "TXT" => RecordType::Txt,
        "MX" => RecordType::Mx,
        "SRV" => RecordType::Srv,
        // A CNAME is added to the zone directly: the generator of the records for mail has no such type.
        _ => return None,
    };
    Some(DnsRecord { name, rtype, value: e.value.clone(), ttl_hint: e.ttl, purpose: "A record of the domain's own", required: Requirement::Optional })
}

/// The zones of the hosted domains for `cfg`, with `serial`.
pub fn build_zones(hosted: &[HostedDomain], cfg: &DnsConfig, serial: u32) -> Zones {
    let mut zones = Zones::new();
    for d in hosted {
        let keys: Vec<(&str, &str, &str)> = d.dkim.iter().map(|(s, a, k)| (s.as_str(), a.as_str(), k.as_str())).collect();
        let facts = MailFacts { mail_host: &cfg.mail_host, public_ips: &cfg.public_ips, spf_include: d.spf_include.as_deref() };
        // A domain whose records cannot be generated (a bad key in the database) is left out: better no zone than a wrong one.
        let Ok((mut records, _)) = records_with_facts(&d.name, &facts, &keys) else { continue };
        let mut cnames = Vec::new();
        for e in &d.extra {
            if e.rtype == "CNAME" {
                cnames.push(e);
            } else if let Some(r) = extra_record(&d.name, e) {
                records.push(r);
            }
        }
        let spec = ZoneSpec { origin: &d.name, name_servers: &cfg.name_servers, hostmaster: &cfg.hostmaster, serial, ttl: cfg.ttl };
        let mut zone = build_zone(&spec, &records);
        for e in cnames {
            let name = if e.name == "@" { d.name.clone() } else { format!("{}.{}", e.name, d.name) };
            zone.add(&name, e.ttl, crate::zone::Rdata::Cname(e.value.trim_end_matches('.').to_ascii_lowercase()));
        }
        zones.insert(zone);
    }
    zones
}

fn now_secs() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
}

/// What a started name server gives back.
pub struct Running {
    pub udp: Vec<SocketAddr>,
    pub tcp: Vec<SocketAddr>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Read the zones once and serve them; the first read must work (a name server that has never had data has nothing to keep answering from).
pub async fn start(cfg: &StartupConfig, store: &Store, stop: CancellationToken) -> Result<Running> {
    let Some(dns) = cfg.dns.clone() else { return Err(Error::Invalid("the dns service needs a [dns] section".into())) };
    let repo: Arc<dyn DnsZoneRepo> = store.repos().dns_zone.clone();
    let mut serial = now_secs();
    let first = build_zones(&repo.hosted().await?, &dns, serial);
    tracing::info!(zones = first.len(), "hosted zones read");
    let handle = ZoneHandle::new(first.clone());
    let server = ServerConfig {
        limits: Limits { per_second: f64::from(dns.rate_per_second), burst: f64::from(dns.burst), slip: dns.slip, max_prefixes: 100_000 },
        tcp_max: dns.tcp_max,
        tcp_per_ip: dns.tcp_per_ip,
        ..ServerConfig::default()
    };
    let (mut udp, mut tcp, mut tasks) = (Vec::new(), Vec::new(), Vec::new());
    for addr in &dns.listen {
        let (u, t, ut, tt) = start_listeners(*addr, handle.clone(), server.clone(), stop.clone()).await.map_err(|e| Error::storage_with("cannot bind the name server address", e))?;
        udp.push(u);
        tcp.push(t);
        tasks.push(ut);
        tasks.push(tt);
    }
    // The heartbeat of the name server, for the Monitor page of the admin web.
    {
        let beat = rusty_svc::HeartbeatService::new(store.repos().heartbeat.clone(), "dns", env!("CARGO_PKG_VERSION")).with_detail(format!("{{\"listeners\":{}}}", dns.listen.len()));
        let stop = stop.clone();
        tokio::spawn(async move { beat.run_until(stop).await });
    }
    let reload = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut current = first;
            let mut last_good = Instant::now();
            let mut stale = false;
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_secs(dns.reload_secs)) => {}
                }
                match repo.hosted().await {
                    Ok(hosted) => {
                        last_good = Instant::now();
                        let candidate = build_zones(&hosted, &dns, serial);
                        if candidate != current || stale {
                            // The serial grows with every change (a secondary, or a resolver that asks for the SOA, can tell the zone changed).
                            serial = serial.max(now_secs()).wrapping_add(u32::from(candidate != current));
                            let next = build_zones(&hosted, &dns, serial);
                            tracing::info!(zones = next.len(), serial, "hosted zones changed");
                            handle.set(next.clone());
                            current = next;
                            stale = false;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "the hosted zones could not be read; the last ones are served");
                        if !stale && last_good.elapsed() > Duration::from_secs(dns.max_stale_secs) {
                            tracing::error!("the hosted zones are older than max_stale_secs: refusing every question until the database answers");
                            handle.set(Zones::new());
                            stale = true;
                        }
                    }
                }
            }
        })
    };
    let task = tokio::spawn(async move {
        for t in tasks {
            let _ = t.await;
        }
        let _ = reload.await;
    });
    Ok(Running { udp, tcp, task })
}

/// The health check: one UDP question to the server; any answer at all is healthy.
pub async fn healthy(addr: SocketAddr, within: Duration) -> bool {
    let ip: IpAddr = if addr.ip().is_unspecified() { IpAddr::from([127, 0, 0, 1]) } else { addr.ip() };
    let target = SocketAddr::new(ip, addr.port());
    let Ok(s) = UdpSocket::bind("127.0.0.1:0").await else { return false };
    if s.send_to(&crate::adapter::probe_question(), target).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    matches!(tokio::time::timeout(within, s.recv_from(&mut buf)).await, Ok(Ok(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::{answer, Query, Rcode};
    use crate::zone::{RType, Rdata};

    fn cfg() -> DnsConfig {
        let text = "[store]\nurl = \"postgres://rusty_dns@db:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n[dns]\nname_servers = [\"ns1.platform.example\", \"ns2.platform.example\"]\nhostmaster = \"hostmaster.platform.example\"\nmail_host = \"mail.platform.example\"\npublic_ips = [\"192.0.2.25\"]\n";
        StartupConfig::from_toml_str_for(rusty_core::config::Role::Dns, text).unwrap().dns.unwrap()
    }

    fn hosted() -> Vec<HostedDomain> {
        vec![HostedDomain {
            name: "hosted.test".into(),
            dkim: vec![("s1".into(), "ed25519".into(), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into())],
            spf_include: Some("_spf.relay.example".into()),
            extra: vec![
                ExtraRecord { id: 1, name: "www".into(), rtype: "CNAME".into(), value: "hosted.test.".into(), ttl: 300 },
                ExtraRecord { id: 2, name: "@".into(), rtype: "TXT".into(), value: "site-verification=abc".into(), ttl: 3600 },
                ExtraRecord { id: 3, name: "blog".into(), rtype: "A".into(), value: "192.0.2.80".into(), ttl: 60 },
                ExtraRecord { id: 4, name: "bad".into(), rtype: "A".into(), value: "not an address".into(), ttl: 60 },
            ],
        }]
    }

    fn ask(z: &Zones, name: &str, t: RType) -> crate::answer::Answer {
        answer(z, &Query { name: name.into(), qtype: t })
    }

    #[test]
    fn the_zones_carry_the_generated_records_the_relay_include_and_the_tenants_own_records() {
        let z = build_zones(&hosted(), &cfg(), 5);
        assert_eq!(z.len(), 1);
        assert_eq!(ask(&z, "hosted.test", RType::Ns).answers.len(), 2);
        let spf = ask(&z, "hosted.test", RType::Txt);
        let texts: Vec<String> = spf.answers.iter().filter_map(|r| if let Rdata::Txt(t) = &r.rdata { Some(t.concat()) } else { None }).collect();
        assert!(texts.iter().any(|t| t.starts_with("v=spf1") && t.contains("include:_spf.relay.example")), "{texts:?}");
        assert!(texts.iter().any(|t| t == "site-verification=abc"), "the tenant's own TXT sits beside the generated one");
        assert_eq!(ask(&z, "s1._domainkey.hosted.test", RType::Txt).answers.len(), 1);
        let www = ask(&z, "www.hosted.test", RType::A);
        assert!(www.answers.len() == 1 && matches!(www.answers[0].rdata, Rdata::Cname(ref t) if t == "hosted.test"), "the tenant's CNAME stands (the apex has no address of its own)");
    }

    #[test]
    fn a_bad_extra_record_is_left_out_and_a_domain_that_is_not_hosted_is_refused() {
        let z = build_zones(&hosted(), &cfg(), 5);
        assert_eq!(ask(&z, "bad.hosted.test", RType::A).rcode, Rcode::NxDomain);
        assert_eq!(ask(&z, "blog.hosted.test", RType::A).answers.len(), 1);
        assert_eq!(ask(&z, "plain.test", RType::A).rcode, Rcode::Refused);
        assert_eq!(build_zones(&[], &cfg(), 1).len(), 0);
    }
}

