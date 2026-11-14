//! The DNS validation engine (plan U10, W1): for each record of the generated set it asks DNS what is published
//! and decides `ok`, `missing`, `mismatch` or `warning`, with the evidence it saw and fixed finding codes. It does
//! no I/O of its own: DNS is the [`DnsSource`] seam (a resolver in production, a zone in memory in tests), so the
//! rules are testable without a network and the same engine serves the CLI, the daily job and the web page.
//!
//! Judgement, not string comparison: an SPF record is `ok` when it **authorises our addresses** with the DNS
//! lookups RFC 7208 allows (not when it equals the generated text, so an operator may add a relay's `include:`); a
//! DKIM record when the published **key equals ours**; a DMARC record when it is **well formed** (a stricter policy
//! than the generated `p=none` is fine). A check that cannot be decided (a DNS failure, a macro the engine does not
//! expand) is a `warning`, never a verdict.
//!
//! Hostile answers are bounded: at most 32 TXT records per name are read, each cut at 8 KiB, an SPF record over
//! 4 KiB is refused, the SPF walk stops after 10 lookups and 2 void lookups, and one record check asks a bounded
//! number of questions.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;

use sha2::{Digest, Sha256};

use crate::dns::{DnsRecord, RecordType};
use crate::dns_state::{clean_seen, Observation, RecordKey, RecordStatus};

/// A lookup that did not give an answer (timeout, server failure). "No such record" is an empty `Ok`, not this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsFail {
    Temporary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MxEntry {
    pub preference: u16,
    /// Lower case, no trailing dot.
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvEntry {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    /// Lower case, no trailing dot.
    pub target: String,
}

/// Where DNS answers come from. Names are lower case without a trailing dot. A name that does not exist, or has no
/// record of the type, is `Ok` with an empty list; each TXT record is its character-strings joined.
pub trait DnsSource: Send + Sync {
    fn txt<'a>(&'a self, name: &'a str) -> impl Future<Output = Result<Vec<String>, DnsFail>> + Send + 'a;
    fn mx<'a>(&'a self, name: &'a str) -> impl Future<Output = Result<Vec<MxEntry>, DnsFail>> + Send + 'a;
    /// A and AAAA together.
    fn addresses<'a>(&'a self, name: &'a str) -> impl Future<Output = Result<Vec<IpAddr>, DnsFail>> + Send + 'a;
    /// The names the reverse zone gives for `ip`.
    fn ptr<'a>(&'a self, ip: IpAddr) -> impl Future<Output = Result<Vec<String>, DnsFail>> + Send + 'a;
    fn srv<'a>(&'a self, name: &'a str) -> impl Future<Output = Result<Vec<SrvEntry>, DnsFail>> + Send + 'a;
}

#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// Mail leaves through a relay: the reverse records of our own addresses are not our business and are not checked.
    pub via_relay: bool,
    /// RFC 7208 section 4.6.4.
    pub max_spf_lookups: u32,
    pub max_spf_void: u32,
}

impl Default for CheckOptions {
    fn default() -> CheckOptions {
        CheckOptions { via_relay: false, max_spf_lookups: 10, max_spf_void: 2 }
    }
}

/// The verdict on one expected record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordCheck {
    pub record: DnsRecord,
    pub status: RecordStatus,
    /// What DNS showed (evidence, cleaned and bounded when it is stored).
    pub seen: Option<String>,
    /// Fixed codes (`spf.too_many_lookups`, `dkim.key_differs`, ...): the UI maps each to a text; nothing here is free text.
    pub findings: Vec<&'static str>,
}

impl RecordCheck {
    /// The identity of the record in the drift state.
    pub fn key(&self) -> RecordKey {
        RecordKey { name: self.record.name.to_ascii_lowercase(), rtype: self.record.rtype.as_str().to_owned(), purpose: self.record.purpose.to_owned() }
    }

    /// A hash of what was expected: a changed record (a rotated key, a new address) starts its history over.
    pub fn expected_hash(&self) -> String {
        let mut h = Sha256::new();
        for part in [self.record.rtype.as_str(), &self.record.name.to_ascii_lowercase(), &self.record.value] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
        hex::encode(h.finalize())
    }

    pub fn observation(&self) -> Observation {
        Observation { key: self.key(), expected_hash: self.expected_hash(), status: self.status, seen: self.seen.as_deref().map(clean_seen) }
    }
}

// ------------------------------------------------------------------------------------------ plumbing

const MAX_TXT_RECORDS: usize = 32;
const MAX_TXT_BYTES: usize = 8192;
const MAX_SPF_BYTES: usize = 4096;
const MAX_MX_HOSTS: usize = 10;
const MAX_SEEN: usize = 300;

/// A finding and how bad it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sev {
    Bad,
    Warn,
    Info,
}

struct Verdict {
    seen: Option<String>,
    findings: Vec<(Sev, &'static str)>,
}

impl Verdict {
    fn new(seen: Option<String>) -> Verdict {
        Verdict { seen, findings: Vec::new() }
    }

    fn add(&mut self, sev: Sev, code: &'static str) {
        if !self.findings.iter().any(|(_, c)| *c == code) {
            self.findings.push((sev, code));
        }
    }

    fn finish(self, record: &DnsRecord) -> RecordCheck {
        let status = if self.findings.iter().any(|(s, _)| *s == Sev::Bad) {
            RecordStatus::Mismatch
        } else if self.findings.iter().any(|(s, _)| *s == Sev::Warn) {
            RecordStatus::Warning
        } else {
            RecordStatus::Ok
        };
        RecordCheck { record: record.clone(), status, seen: self.seen, findings: self.findings.into_iter().map(|(_, c)| c).collect() }
    }
}

fn done(record: &DnsRecord, status: RecordStatus, seen: Option<String>, code: Option<&'static str>) -> RecordCheck {
    RecordCheck { record: record.clone(), status, seen, findings: code.into_iter().collect() }
}

fn short(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_SEEN).collect()
}

fn bare(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// The answers of one check, remembered: the same name is asked once however many records need it.
struct Net<'s, S: DnsSource> {
    src: &'s S,
    txt: HashMap<String, Result<Vec<String>, DnsFail>>,
    mx: HashMap<String, Result<Vec<MxEntry>, DnsFail>>,
    addrs: HashMap<String, Result<Vec<IpAddr>, DnsFail>>,
}

impl<'s, S: DnsSource> Net<'s, S> {
    fn new(src: &'s S) -> Net<'s, S> {
        Net { src, txt: HashMap::new(), mx: HashMap::new(), addrs: HashMap::new() }
    }

    /// The TXT records of `name`: at most 32, each cut at 8 KiB on a character boundary.
    async fn txt(&mut self, name: &str) -> Result<Vec<String>, DnsFail> {
        if let Some(r) = self.txt.get(name) {
            return r.clone();
        }
        let r = self.src.txt(name).await.map(|v| {
            v.into_iter()
                .take(MAX_TXT_RECORDS)
                .map(|t| if t.len() > MAX_TXT_BYTES { t.chars().scan(0usize, |n, c| { *n += c.len_utf8(); (*n <= MAX_TXT_BYTES).then_some(c) }).collect() } else { t })
                .collect()
        });
        self.txt.insert(name.to_owned(), r.clone());
        r
    }

    async fn mx(&mut self, name: &str) -> Result<Vec<MxEntry>, DnsFail> {
        if let Some(r) = self.mx.get(name) {
            return r.clone();
        }
        let r = self.src.mx(name).await.map(|mut v| {
            v.truncate(MAX_MX_HOSTS * 2);
            v
        });
        self.mx.insert(name.to_owned(), r.clone());
        r
    }

    async fn addresses(&mut self, name: &str) -> Result<Vec<IpAddr>, DnsFail> {
        if let Some(r) = self.addrs.get(name) {
            return r.clone();
        }
        let r = self.src.addresses(name).await.map(|mut v| {
            v.truncate(32);
            v
        });
        self.addrs.insert(name.to_owned(), r.clone());
        r
    }
}

/// Check every record of `expected` against what DNS publishes. The result has one entry per record (the reverse
/// records are left out when mail goes through a relay), in the order of `expected`.
pub async fn check_records<S: DnsSource>(src: &S, expected: &[DnsRecord], opts: &CheckOptions) -> Vec<RecordCheck> {
    let mut net = Net::new(src);
    let ips: Vec<IpAddr> = expected.iter().filter(|r| matches!(r.rtype, RecordType::A | RecordType::Aaaa)).filter_map(|r| r.value.parse().ok()).collect();
    let mut out = Vec::new();
    for r in expected {
        let name = bare(&r.name);
        let check = match r.rtype {
            RecordType::Mx => check_mx(&mut net, r, &name).await,
            RecordType::A | RecordType::Aaaa => check_address(&mut net, r, &name).await,
            RecordType::Ptr => {
                if opts.via_relay {
                    continue;
                }
                check_ptr(&mut net, r).await
            }
            RecordType::Srv => check_srv(src, r, &name).await,
            RecordType::Txt => {
                if r.value.to_ascii_lowercase().starts_with("v=spf1") {
                    check_spf(&mut net, r, &name, &ips, opts).await
                } else if name.starts_with("_dmarc.") {
                    check_dmarc(&mut net, r, &name).await
                } else if name.contains("._domainkey.") || name.starts_with('<') {
                    check_dkim(&mut net, r, &name).await
                } else {
                    done(r, RecordStatus::Warning, None, Some("check.unsupported"))
                }
            }
        };
        out.push(check);
    }
    out
}

// ----------------------------------------------------------------------------------------------- MX

async fn check_mx<S: DnsSource>(net: &mut Net<'_, S>, r: &DnsRecord, name: &str) -> RecordCheck {
    let want = r.value.split_whitespace().nth(1).map(bare).unwrap_or_default();
    let Ok(found) = net.mx(name).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    if found.is_empty() {
        return done(r, RecordStatus::Missing, None, None);
    }
    let seen = short(&found.iter().map(|m| format!("{} {}", m.preference, m.host)).collect::<Vec<_>>().join(", "));
    if !found.iter().any(|m| m.host == want) {
        return done(r, RecordStatus::Mismatch, Some(seen), None);
    }
    let mut v = Verdict::new(Some(seen));
    match net.addresses(&want).await {
        Ok(a) if a.is_empty() => v.add(Sev::Warn, "mx.host_unresolvable"),
        Ok(_) => {}
        Err(_) => v.add(Sev::Warn, "dns.temporary"),
    }
    v.finish(r)
}

// ------------------------------------------------------------------------------------------- A, AAAA

async fn check_address<S: DnsSource>(net: &mut Net<'_, S>, r: &DnsRecord, name: &str) -> RecordCheck {
    let Ok(want) = r.value.parse::<IpAddr>() else { return done(r, RecordStatus::Warning, None, Some("check.unsupported")) };
    let Ok(found) = net.addresses(name).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    let same_family: Vec<&IpAddr> = found.iter().filter(|a| a.is_ipv4() == want.is_ipv4()).collect();
    if same_family.is_empty() {
        return done(r, RecordStatus::Missing, None, None);
    }
    let seen = short(&same_family.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "));
    let status = if same_family.contains(&&want) { RecordStatus::Ok } else { RecordStatus::Mismatch };
    done(r, status, Some(seen), None)
}

// ----------------------------------------------------------------------------------------------- PTR

/// The address a reverse name stands for (`10.2.0.192.in-addr.arpa.` or the nibble form of `ip6.arpa.`).
fn ip_from_reverse(name: &str) -> Option<IpAddr> {
    let n = bare(name);
    if let Some(rest) = n.strip_suffix(".in-addr.arpa") {
        let o: Vec<u8> = rest.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        if let [d, c, b, a] = o[..] {
            return Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
        }
        return None;
    }
    let rest = n.strip_suffix(".ip6.arpa")?;
    let nibbles: Vec<u8> = rest.split('.').map(|p| u8::from_str_radix(p, 16).ok().filter(|v| *v < 16)).collect::<Option<_>>()?;
    if nibbles.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, pair) in nibbles.chunks(2).enumerate() {
        if let (Some(slot), [lo, hi]) = (bytes.get_mut(15usize.saturating_sub(i)), pair) {
            *slot = hi << 4 | lo;
        }
    }
    Some(IpAddr::V6(Ipv6Addr::from(bytes)))
}

async fn check_ptr<S: DnsSource>(net: &mut Net<'_, S>, r: &DnsRecord) -> RecordCheck {
    let Some(ip) = ip_from_reverse(&r.name) else { return done(r, RecordStatus::Warning, None, Some("check.unsupported")) };
    let want = bare(&r.value);
    let Ok(names) = net.src.ptr(ip).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    let names: Vec<String> = names.iter().take(8).map(|n| bare(n)).collect();
    if names.is_empty() {
        return done(r, RecordStatus::Missing, None, None);
    }
    let seen = short(&names.join(", "));
    if !names.contains(&want) {
        return done(r, RecordStatus::Mismatch, Some(seen), None);
    }
    // Forward-confirmed: the name must point back to the address.
    let mut v = Verdict::new(Some(seen));
    match net.addresses(&want).await {
        Ok(a) if a.contains(&ip) => {}
        Ok(_) => v.add(Sev::Bad, "ptr.not_forward_confirmed"),
        Err(_) => v.add(Sev::Warn, "dns.temporary"),
    }
    v.finish(r)
}

// ----------------------------------------------------------------------------------------------- SRV

async fn check_srv<S: DnsSource>(src: &S, r: &DnsRecord, name: &str) -> RecordCheck {
    let parts: Vec<&str> = r.value.split_whitespace().collect();
    let (Some(port), Some(target)) = (parts.get(2).and_then(|p| p.parse::<u16>().ok()), parts.get(3)) else {
        return done(r, RecordStatus::Warning, None, Some("check.unsupported"));
    };
    let target = bare(target);
    let Ok(found) = src.srv(name).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    if found.is_empty() {
        return done(r, RecordStatus::Missing, None, None);
    }
    let seen = short(&found.iter().take(8).map(|s| format!("{} {} {} {}", s.priority, s.weight, s.port, s.target)).collect::<Vec<_>>().join(", "));
    let ok = found.iter().any(|s| s.port == port && s.target == target);
    done(r, if ok { RecordStatus::Ok } else { RecordStatus::Mismatch }, Some(seen), None)
}

// ---------------------------------------------------------------------------------------------- DKIM

/// The tags of a `tag=value; tag=value` record, names lower-cased, values trimmed.
fn tags(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn tag<'a>(t: &'a [(String, String)], name: &str) -> Option<&'a str> {
    t.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

async fn check_dkim<S: DnsSource>(net: &mut Net<'_, S>, r: &DnsRecord, name: &str) -> RecordCheck {
    if name.starts_with('<') {
        return done(r, RecordStatus::Warning, None, Some("dkim.not_generated"));
    }
    let want: String = tag(&tags(&r.value), "p").unwrap_or_default().split_whitespace().collect();
    let Ok(records) = net.txt(name).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    let candidates: Vec<&String> = records.iter().filter(|t| tag(&tags(t), "p").is_some() || t.trim_start().to_ascii_lowercase().starts_with("v=dkim1")).collect();
    match candidates.as_slice() {
        [] => done(r, RecordStatus::Missing, None, None),
        [one] => {
            let t = tags(one);
            let mut v = Verdict::new(Some(short(one)));
            if tag(&t, "v").is_some_and(|x| !x.eq_ignore_ascii_case("DKIM1")) {
                v.add(Sev::Bad, "dkim.syntax");
            }
            let published: String = tag(&t, "p").unwrap_or_default().split_whitespace().collect();
            if published.is_empty() {
                v.add(Sev::Bad, "dkim.revoked");
            } else if published != want {
                v.add(Sev::Bad, "dkim.key_differs");
            }
            if tag(&t, "t").is_some_and(|f| f.split(':').any(|x| x.trim().eq_ignore_ascii_case("y"))) {
                v.add(Sev::Warn, "dkim.testing");
            }
            let rsa = tag(&t, "k").is_none_or(|k| k.eq_ignore_ascii_case("rsa"));
            if rsa && !published.is_empty() && published.len() < 300 {
                v.add(Sev::Warn, "dkim.short_key");
            }
            v.finish(r)
        }
        many => {
            let mut v = Verdict::new(Some(short(many.first().map_or("", |s| s.as_str()))));
            v.add(Sev::Bad, "dkim.multiple_records");
            v.finish(r)
        }
    }
}

// ---------------------------------------------------------------------------------------------- DMARC

fn valid_report_uri(list: &str) -> bool {
    list.split(',').map(str::trim).all(|u| {
        u.strip_prefix("mailto:").is_some_and(|a| {
            let addr = a.split('!').next().unwrap_or_default();
            addr.matches('@').count() == 1 && !addr.starts_with('@') && !addr.ends_with('@') && !addr.contains(char::is_whitespace)
        })
    })
}

async fn check_dmarc<S: DnsSource>(net: &mut Net<'_, S>, r: &DnsRecord, name: &str) -> RecordCheck {
    let Ok(records) = net.txt(name).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    let is_dmarc = |t: &str| {
        let l = t.trim_start().to_ascii_lowercase();
        l.strip_prefix("v=dmarc1").is_some_and(|rest| rest.trim_start().is_empty() || rest.trim_start().starts_with(';'))
    };
    let candidates: Vec<&String> = records.iter().filter(|t| is_dmarc(t)).collect();
    let mut v;
    match candidates.as_slice() {
        [] => {
            if records.is_empty() {
                return done(r, RecordStatus::Missing, None, None);
            }
            v = Verdict::new(records.first().map(|s| short(s)));
            v.add(Sev::Bad, "dmarc.syntax");
            return v.finish(r);
        }
        [one] => {
            v = Verdict::new(Some(short(one)));
            let t = tags(one);
            let policy = |x: &str| matches!(x.to_ascii_lowercase().as_str(), "none" | "quarantine" | "reject");
            if !tag(&t, "p").is_some_and(policy) || tag(&t, "sp").is_some_and(|s| !policy(s)) {
                v.add(Sev::Bad, "dmarc.syntax");
            }
            if tag(&t, "pct").is_some_and(|p| p.parse::<u8>().map_or(true, |n| n > 100)) {
                v.add(Sev::Bad, "dmarc.syntax");
            }
            for k in ["adkim", "aspf"] {
                if tag(&t, k).is_some_and(|a| !matches!(a.to_ascii_lowercase().as_str(), "r" | "s")) {
                    v.add(Sev::Bad, "dmarc.syntax");
                }
            }
            if tag(&t, "rua").is_some_and(|u| !valid_report_uri(u)) {
                v.add(Sev::Warn, "dmarc.rua_invalid");
            }
        }
        many => {
            v = Verdict::new(many.first().map(|s| short(s)));
            v.add(Sev::Bad, "dmarc.multiple_records");
        }
    }
    v.finish(r)
}

// ------------------------------------------------------------------------------------------------ SPF

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mech {
    All,
    Include(String),
    A(Option<String>, u8, u8),
    Mx(Option<String>, u8, u8),
    Ptr,
    Ip4(Ipv4Addr, u8),
    Ip6(Ipv6Addr, u8),
    Exists(String),
}

struct Term {
    qualifier: char,
    mech: Mech,
}

struct Spf {
    terms: Vec<Term>,
    redirect: Option<String>,
    macros: bool,
}

fn domain_spec(s: &str) -> Result<(Option<String>, bool), &'static str> {
    if s.is_empty() {
        return Err("spf.syntax");
    }
    if s.contains('%') {
        return Ok((None, true));
    }
    if s.len() > 253 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')) {
        return Err("spf.syntax");
    }
    Ok((Some(s.trim_end_matches('.').to_ascii_lowercase()), false))
}

/// `a`, `mx`: `[:domain][/v4cidr][//v6cidr]`.
fn dual(rest: &str) -> Result<(Option<String>, bool, u8, u8), &'static str> {
    let (dom, cidr) = match rest.strip_prefix(':') {
        Some(r) => match r.find('/') {
            Some(i) => (Some(r.get(..i).unwrap_or_default()), r.get(i..).unwrap_or_default()),
            None => (Some(r), ""),
        },
        None if rest.is_empty() || rest.starts_with('/') => (None, rest),
        None => return Err("spf.syntax"),
    };
    let (mut c4, mut c6) = (32u8, 128u8);
    if !cidr.is_empty() {
        let (v4, v6) = match cidr.split_once("//") {
            Some((a, b)) => (a, Some(b)),
            None => (cidr, None),
        };
        if !v4.is_empty() {
            c4 = v4.strip_prefix('/').and_then(|n| n.parse().ok()).filter(|n| *n <= 32).ok_or("spf.syntax")?;
        }
        if let Some(b) = v6 {
            c6 = b.parse().ok().filter(|n| *n <= 128).ok_or("spf.syntax")?;
        }
    }
    let (d, m) = match dom {
        Some(d) => domain_spec(d)?,
        None => (None, false),
    };
    Ok((d, m, c4, c6))
}

fn parse_spf(text: &str) -> Result<Spf, &'static str> {
    if text.len() > MAX_SPF_BYTES {
        return Err("spf.too_long");
    }
    let mut it = text.split_whitespace();
    if !it.next().is_some_and(|v| v.eq_ignore_ascii_case("v=spf1")) {
        return Err("spf.syntax");
    }
    let mut spf = Spf { terms: Vec::new(), redirect: None, macros: false };
    for raw in it {
        let is_modifier = raw.find('=').is_some_and(|i| raw.get(..i).is_some_and(|n| n.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))));
        if is_modifier {
            let (name, value) = raw.split_once('=').unwrap_or((raw, ""));
            if name.eq_ignore_ascii_case("redirect") {
                if spf.redirect.is_some() {
                    return Err("spf.syntax");
                }
                match domain_spec(value)? {
                    (Some(d), _) => spf.redirect = Some(d),
                    (None, _) => spf.macros = true,
                }
            }
            continue;
        }
        let (qualifier, body) = match raw.chars().next() {
            Some(q @ ('+' | '-' | '~' | '?')) => (q, raw.get(1..).unwrap_or_default()),
            _ => ('+', raw),
        };
        let name_end = body.find([':', '/']).unwrap_or(body.len());
        let name = body.get(..name_end).unwrap_or_default().to_ascii_lowercase();
        let rest = body.get(name_end..).unwrap_or_default();
        let mech = match name.as_str() {
            "all" if rest.is_empty() => Mech::All,
            "include" | "exists" => {
                let spec = rest.strip_prefix(':').ok_or("spf.syntax")?;
                match domain_spec(spec)? {
                    (Some(d), _) => {
                        if name == "include" {
                            Mech::Include(d)
                        } else {
                            Mech::Exists(d)
                        }
                    }
                    (None, _) => {
                        spf.macros = true;
                        continue;
                    }
                }
            }
            "a" | "mx" => {
                let (d, m, c4, c6) = dual(rest)?;
                if m {
                    spf.macros = true;
                    continue;
                }
                if name == "a" {
                    Mech::A(d, c4, c6)
                } else {
                    Mech::Mx(d, c4, c6)
                }
            }
            "ptr" => Mech::Ptr,
            "ip4" => {
                let spec = rest.strip_prefix(':').ok_or("spf.syntax")?;
                let (addr, len) = match spec.split_once('/') {
                    Some((a, l)) => (a, l.parse::<u8>().ok().filter(|n| *n <= 32).ok_or("spf.syntax")?),
                    None => (spec, 32),
                };
                Mech::Ip4(addr.parse().map_err(|_| "spf.syntax")?, len)
            }
            "ip6" => {
                let spec = rest.strip_prefix(':').ok_or("spf.syntax")?;
                let (addr, len) = match spec.split_once('/') {
                    Some((a, l)) => (a, l.parse::<u8>().ok().filter(|n| *n <= 128).ok_or("spf.syntax")?),
                    None => (spec, 128),
                };
                Mech::Ip6(addr.parse().map_err(|_| "spf.syntax")?, len)
            }
            _ => return Err("spf.syntax"),
        };
        spf.terms.push(Term { qualifier, mech });
    }
    Ok(spf)
}

fn in_v4(net: Ipv4Addr, len: u8, ip: Ipv4Addr) -> bool {
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len.min(32))) };
    u32::from(net) & mask == u32::from(ip) & mask
}

fn in_v6(net: Ipv6Addr, len: u8, ip: Ipv6Addr) -> bool {
    let mask = if len == 0 { 0 } else { u128::MAX << (128 - u32::from(len.min(128))) };
    u128::from(net) & mask == u128::from(ip) & mask
}

fn in_net(addr: IpAddr, c4: u8, c6: u8, ip: IpAddr) -> bool {
    match (addr, ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => in_v4(a, c4, b),
        (IpAddr::V6(a), IpAddr::V6(b)) => in_v6(a, c6, b),
        _ => false,
    }
}

/// Why the SPF walk could not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Lookups,
    Void,
    Temp,
    Syntax(&'static str),
}

struct Walk {
    lookups: u32,
    void: u32,
    macros: bool,
    all: Option<char>,
    redirected: bool,
    /// Every domain named by an `include:` or a `redirect=` anywhere in the tree (lower case), in the order met.
    named: Vec<String>,
}

/// The SPF records among `records`: those that start with `v=spf1` as a whole word.
fn spf_records(records: &[String]) -> Vec<&String> {
    records
        .iter()
        .filter(|t| {
            let l = t.trim_start().to_ascii_lowercase();
            l.strip_prefix("v=spf1").is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
        })
        .collect()
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The static count of RFC 7208 section 4.6.4: every mechanism and modifier that causes a DNS lookup, followed through
/// `include` and `redirect` recursively, whatever the order and whatever would match first. Stops as soon as the limit
/// is passed (a loop passes it).
fn walk<'a, S: DnsSource>(net: &'a mut Net<'_, S>, text: &'a str, top: bool, w: &'a mut Walk, opts: &'a CheckOptions, domain: &'a str) -> BoxFut<'a, Result<(), Stop>> {
    Box::pin(async move {
        let spf = parse_spf(text).map_err(Stop::Syntax)?;
        w.macros |= spf.macros;
        for t in &spf.terms {
            match &t.mech {
                Mech::All => {
                    if top {
                        w.all = Some(t.qualifier);
                    }
                }
                Mech::Ip4(..) | Mech::Ip6(..) => {}
                Mech::Ptr => count(w, opts)?,
                Mech::Include(d) => {
                    count(w, opts)?;
                    w.named.push(d.clone());
                    let records = net.txt(d).await.map_err(|_| Stop::Temp)?;
                    let found = spf_records(&records);
                    match found.as_slice() {
                        [one] => {
                            let one = (*one).clone();
                            walk(net, &one, false, w, opts, d).await?;
                        }
                        [] => return Err(Stop::Syntax("spf.include_missing")),
                        _ => return Err(Stop::Syntax("spf.syntax")),
                    }
                }
                Mech::A(d, ..) => {
                    count(w, opts)?;
                    void_check(net, d.as_deref().unwrap_or(domain), w, opts).await?;
                }
                Mech::Exists(d) => {
                    count(w, opts)?;
                    void_check(net, d, w, opts).await?;
                }
                Mech::Mx(d, ..) => {
                    count(w, opts)?;
                    let name = d.as_deref().unwrap_or(domain);
                    let hosts = net.mx(name).await.map_err(|_| Stop::Temp)?;
                    if hosts.is_empty() {
                        void(w, opts)?;
                    }
                }
            }
        }
        if let Some(target) = &spf.redirect {
            count(w, opts)?;
            w.redirected = true;
            w.named.push(target.clone());
            let records = net.txt(target).await.map_err(|_| Stop::Temp)?;
            let found = spf_records(&records);
            match found.as_slice() {
                [one] => {
                    let one = (*one).clone();
                    walk(net, &one, false, w, opts, target).await?;
                }
                [] => return Err(Stop::Syntax("spf.include_missing")),
                _ => return Err(Stop::Syntax("spf.syntax")),
            }
        }
        Ok(())
    })
}

fn count(w: &mut Walk, opts: &CheckOptions) -> Result<(), Stop> {
    w.lookups += 1;
    if w.lookups > opts.max_spf_lookups {
        return Err(Stop::Lookups);
    }
    Ok(())
}

fn void(w: &mut Walk, opts: &CheckOptions) -> Result<(), Stop> {
    w.void += 1;
    if w.void > opts.max_spf_void {
        return Err(Stop::Void);
    }
    Ok(())
}

/// A lookup that finds nothing is a "void lookup" (RFC 7208 section 4.6.4).
async fn void_check<S: DnsSource>(net: &mut Net<'_, S>, name: &str, w: &mut Walk, opts: &CheckOptions) -> Result<(), Stop> {
    let found = net.addresses(name).await.map_err(|_| Stop::Temp)?;
    if found.is_empty() {
        void(w, opts)?;
    }
    Ok(())
}

/// The first mechanism of `text` that matches `ip`, as RFC 7208 evaluates a record: its qualifier, or `None` when nothing
/// matches (and there is nothing to redirect to). Only called after [`walk`] has proved the record within the limits, so
/// the recursion is bounded by them; the depth guard is a second belt.
fn authorised<'a, S: DnsSource>(net: &'a mut Net<'_, S>, text: &'a str, domain: &'a str, ip: IpAddr, depth: u32) -> BoxFut<'a, Result<Option<char>, Stop>> {
    Box::pin(async move {
        if depth > 12 {
            return Err(Stop::Lookups);
        }
        let spf = parse_spf(text).map_err(Stop::Syntax)?;
        for t in &spf.terms {
            let hit = match &t.mech {
                Mech::All => true,
                Mech::Ip4(a, len) => matches!(ip, IpAddr::V4(b) if in_v4(*a, *len, b)),
                Mech::Ip6(a, len) => matches!(ip, IpAddr::V6(b) if in_v6(*a, *len, b)),
                Mech::Ptr => false,
                Mech::A(d, c4, c6) => {
                    let found = net.addresses(d.as_deref().unwrap_or(domain)).await.map_err(|_| Stop::Temp)?;
                    found.iter().any(|a| in_net(*a, *c4, *c6, ip))
                }
                Mech::Mx(d, c4, c6) => {
                    let hosts = net.mx(d.as_deref().unwrap_or(domain)).await.map_err(|_| Stop::Temp)?;
                    let mut any = false;
                    for h in hosts.iter().take(MAX_MX_HOSTS) {
                        let found = net.addresses(&h.host).await.map_err(|_| Stop::Temp)?;
                        if found.iter().any(|a| in_net(*a, *c4, *c6, ip)) {
                            any = true;
                            break;
                        }
                    }
                    any
                }
                Mech::Exists(d) => !net.addresses(d).await.map_err(|_| Stop::Temp)?.is_empty(),
                Mech::Include(d) => {
                    let records = net.txt(d).await.map_err(|_| Stop::Temp)?;
                    let found = spf_records(&records);
                    match found.as_slice() {
                        [one] => {
                            let one = (*one).clone();
                            authorised(net, &one, d, ip, depth + 1).await? == Some('+')
                        }
                        _ => return Err(Stop::Syntax("spf.include_missing")),
                    }
                }
            };
            if hit {
                return Ok(Some(t.qualifier));
            }
        }
        if let Some(target) = &spf.redirect {
            let records = net.txt(target).await.map_err(|_| Stop::Temp)?;
            let found = spf_records(&records);
            return match found.as_slice() {
                [one] => {
                    let one = (*one).clone();
                    authorised(net, &one, target, ip, depth + 1).await
                }
                _ => Err(Stop::Syntax("spf.include_missing")),
            };
        }
        Ok(None)
    })
}

async fn check_spf<S: DnsSource>(net: &mut Net<'_, S>, r: &DnsRecord, name: &str, ips: &[IpAddr], opts: &CheckOptions) -> RecordCheck {
    let Ok(records) = net.txt(name).await else { return done(r, RecordStatus::Warning, None, Some("dns.temporary")) };
    let found = spf_records(&records);
    let text = match found.as_slice() {
        [] => return done(r, RecordStatus::Missing, None, None),
        [one] => (*one).clone(),
        many => {
            let mut v = Verdict::new(many.first().map(|s| short(s)));
            v.add(Sev::Bad, "spf.multiple_records");
            return v.finish(r);
        }
    };
    let mut v = Verdict::new(Some(short(&text)));
    if text.len() > MAX_SPF_BYTES {
        v.add(Sev::Bad, "spf.too_long");
        return v.finish(r);
    }
    let mut w = Walk { lookups: 0, void: 0, macros: false, all: None, redirected: false, named: Vec::new() };
    match walk(net, &text, true, &mut w, opts, name).await {
        Err(Stop::Lookups) => v.add(Sev::Bad, "spf.too_many_lookups"),
        Err(Stop::Void) => v.add(Sev::Bad, "spf.too_many_void"),
        Err(Stop::Temp) => v.add(Sev::Warn, "dns.temporary"),
        Err(Stop::Syntax(code)) => v.add(Sev::Bad, code),
        Ok(()) => {
            if w.macros {
                v.add(Sev::Warn, "spf.macros_unsupported");
            } else {
                for ip in ips {
                    match authorised(net, &text, name, *ip, 0).await {
                        Ok(Some('+')) => {}
                        Ok(_) => v.add(Sev::Bad, "spf.ip_not_authorised"),
                        Err(Stop::Temp) => v.add(Sev::Warn, "dns.temporary"),
                        Err(Stop::Lookups | Stop::Void) => v.add(Sev::Bad, "spf.too_many_lookups"),
                        Err(Stop::Syntax(code)) => v.add(Sev::Bad, code),
                    }
                }
            }
            // A relay's provider sends our mail from its own addresses, which we cannot list: what can be checked is that its
            // include is named, directly or through another include, in the record that is published.
            let wanted: Vec<String> = r.value.split_whitespace().filter_map(|t| t.strip_prefix("include:")).map(str::to_ascii_lowercase).collect();
            if wanted.iter().any(|inc| !w.named.iter().any(|n| n == inc)) {
                v.add(Sev::Bad, "spf.provider_not_included");
            }
            // Nothing to authorise and nothing to find: an `ok` here would claim a check that was not made.
            if ips.is_empty() && wanted.is_empty() {
                v.add(Sev::Warn, "spf.not_verifiable");
            }
            match (w.all, w.redirected) {
                (Some('+'), _) => v.add(Sev::Bad, "spf.open"),
                (Some('?'), _) => v.add(Sev::Warn, "spf.weak_all"),
                (None, false) => v.add(Sev::Warn, "spf.no_all"),
                _ => {}
            }
        }
    }
    let _ = Sev::Info;
    v.finish(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::reverse_name;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn a_reverse_name_gives_back_its_address(a in any::<u32>(), b in any::<u128>()) {
            let v4 = IpAddr::V4(Ipv4Addr::from(a));
            let v6 = IpAddr::V6(Ipv6Addr::from(b));
            prop_assert_eq!(ip_from_reverse(&reverse_name(v4)), Some(v4));
            prop_assert_eq!(ip_from_reverse(&reverse_name(v6)), Some(v6));
        }

        #[test]
        fn cidr_matching_is_equality_of_the_leading_bits(a in any::<u32>(), b in any::<u32>(), len in 0u8..=32) {
            let keep = if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len)) };
            prop_assert_eq!(in_v4(Ipv4Addr::from(a), len, Ipv4Addr::from(b)), a & keep == b & keep);
        }

        #[test]
        fn cidr_matching_v6(a in any::<u128>(), b in any::<u128>(), len in 0u8..=128) {
            let keep = if len == 0 { 0 } else { u128::MAX << (128 - u32::from(len)) };
            prop_assert_eq!(in_v6(Ipv6Addr::from(a), len, Ipv6Addr::from(b)), a & keep == b & keep);
        }

        #[test]
        fn a_hostile_spf_text_never_panics(s in r"\PC{0,300}") {
            let _ = parse_spf(&s);
            let _ = parse_spf(&format!("v=spf1 {s}"));
        }
    }

    #[test]
    fn reverse_names_that_are_not_addresses_are_refused() {
        for bad in ["", "example.net.", "1.2.3.in-addr.arpa.", "1.2.3.4.5.in-addr.arpa.", "256.0.0.1.in-addr.arpa.", "g.0.ip6.arpa.", "1.ip6.arpa."] {
            assert_eq!(ip_from_reverse(bad), None, "{bad}");
        }
    }
}
