//! The DNS records a domain needs (plan U10), as a pure function with no I/O: the single source of
//! truth for `rustyctl check`, the future web setup page and the generated documentation. The
//! output is structured; a zone-file line formatter and its parser (used by the round-trip tests)
//! are here too. Public keys only: nothing in this module touches private key material.

use std::fmt::Write as _;
use std::net::IpAddr;

use crate::config::valid_hostname;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    Mx,
    A,
    Aaaa,
    Txt,
    Srv,
    Ptr,
}

impl RecordType {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordType::Mx => "MX",
            RecordType::A => "A",
            RecordType::Aaaa => "AAAA",
            RecordType::Txt => "TXT",
            RecordType::Srv => "SRV",
            RecordType::Ptr => "PTR",
        }
    }

    fn parse(s: &str) -> Option<RecordType> {
        Some(match s {
            "MX" => RecordType::Mx,
            "A" => RecordType::A,
            "AAAA" => RecordType::Aaaa,
            "TXT" => RecordType::Txt,
            "SRV" => RecordType::Srv,
            "PTR" => RecordType::Ptr,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    Required,
    Recommended,
    Optional,
}

/// One record to publish. `name` is fully qualified with the trailing dot; `value` is the record
/// data without zone-file quoting (a TXT value is the joined text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRecord {
    pub name: String,
    pub rtype: RecordType,
    pub value: String,
    pub ttl_hint: u32,
    pub purpose: &'static str,
    pub required: Requirement,
}

/// The kind of a DKIM key: the `k=` tag of its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimKind {
    Rsa,
    Ed25519,
}

impl DkimKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DkimKind::Rsa => "rsa",
            DkimKind::Ed25519 => "ed25519",
        }
    }

    pub fn parse(s: &str) -> Option<DkimKind> {
        match s {
            "rsa" => Some(DkimKind::Rsa),
            "ed25519" => Some(DkimKind::Ed25519),
            _ => None,
        }
    }
}

/// The public half of a DKIM key (`p=` of the TXT record), published at `<selector>._domainkey.<domain>`.
#[derive(Debug, Clone, Copy)]
pub struct DkimPublic<'a> {
    pub selector: &'a str,
    pub kind: DkimKind,
    /// Base64 as published in `p=` (RSA: the SubjectPublicKeyInfo; ed25519: the raw 32-byte key).
    pub public_key_b64: &'a str,
}

/// Most keys one domain publishes (an ed25519 key and an RSA key, with room for a rotation in progress).
pub const MAX_DKIM_KEYS: usize = 4;

/// What the generator needs. `domain` and `mail_host` are A-label names without a trailing dot.
#[derive(Debug, Clone, Copy)]
pub struct DnsInputs<'a> {
    pub domain: &'a str,
    pub mail_host: &'a str,
    /// Addresses mail leaves from (and the mail host resolves to).
    pub outbound_ips: &'a [IpAddr],
    /// The domain's keys, one record each; empty: no key has been generated yet and a placeholder record says how.
    pub dkim: &'a [DkimPublic<'a>],
    /// SPF includes of a relay provider (`amazonses.com`): mail then leaves from the provider's addresses. At most
    /// [`MAX_SPF_INCLUDES`] (each costs one of the ten DNS lookups SPF allows).
    pub spf_includes: &'a [&'a str],
    /// Address for DMARC aggregate reports (optional).
    pub dmarc_rua: Option<&'a str>,
}

/// Most SPF includes the generated record names.
pub const MAX_SPF_INCLUDES: usize = 5;

/// A domain name as it may follow `include:` in an SPF record: labels of letters, digits, `-` and `_` (providers publish
/// `_spf.example.org`), no empty label, no trailing dot, nothing that could end the term or start another.
pub fn valid_spf_domain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// Can this address be published as the address mail leaves from? Not the unspecified, loopback, multicast, link-local or
/// broadcast address: nothing outside the host could reach those.
pub fn publishable_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => !(a.is_unspecified() || a.is_loopback() || a.is_multicast() || a.is_broadcast() || a.is_link_local()),
        IpAddr::V6(a) => !(a.is_unspecified() || a.is_loopback() || a.is_multicast() || a.segments().first().is_some_and(|s| s & 0xffc0 == 0xfe80)),
    }
}

const TTL: u32 = 3600;

fn fq(name: &str) -> String {
    format!("{name}.")
}

/// Reverse-DNS name of an address (`in-addr.arpa` / `ip6.arpa`).
pub fn reverse_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            format!("{}.{}.{}.{}.in-addr.arpa.", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(a) => {
            let mut s = String::new();
            for b in a.octets().iter().rev() {
                let _ = write!(s, "{:x}.{:x}.", b & 0xf, b >> 4);
            }
            s.push_str("ip6.arpa.");
            s
        }
    }
}

fn valid_selector(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !s.starts_with(['-', '.'])
        && !s.ends_with(['-', '.'])
}

fn valid_b64(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn valid_mailbox(s: &str) -> bool {
    s.len() <= 254
        && s.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\' | b';' | b','))
        && s.matches('@').count() == 1
        && !s.starts_with('@')
        && !s.ends_with('@')
}

/// The records for one domain. `Invalid` for a name or key that could not be published.
pub fn records_for_domain(i: &DnsInputs<'_>) -> Result<Vec<DnsRecord>> {
    // Names come in WITHOUT a trailing dot (fq adds it): a dotted input would print `example.com..`.
    if !valid_hostname(i.domain) || !valid_hostname(i.mail_host) || i.domain.ends_with('.') || i.mail_host.ends_with('.') {
        return Err(Error::Invalid("invalid host name".into()));
    }
    if i.dkim.len() > MAX_DKIM_KEYS {
        return Err(Error::Invalid("too many DKIM keys".into()));
    }
    for (n, k) in i.dkim.iter().enumerate() {
        if !valid_selector(k.selector) || !valid_b64(k.public_key_b64) {
            return Err(Error::Invalid("invalid DKIM key".into()));
        }
        // DNS names are case-insensitive: two selectors that differ only in case are one name.
        if i.dkim.iter().take(n).any(|o| o.selector.eq_ignore_ascii_case(k.selector)) {
            return Err(Error::Invalid("two DKIM keys share a selector".into()));
        }
    }
    if i.dmarc_rua.is_some_and(|r| !valid_mailbox(r)) {
        return Err(Error::Invalid("invalid report address".into()));
    }
    if i.spf_includes.len() > MAX_SPF_INCLUDES {
        return Err(Error::Invalid("too many SPF includes".into()));
    }
    for (n, inc) in i.spf_includes.iter().enumerate() {
        if !valid_spf_domain(inc) || i.spf_includes.iter().take(n).any(|o| o.eq_ignore_ascii_case(inc)) {
            return Err(Error::Invalid("invalid SPF include".into()));
        }
    }
    let (domain, host) = (fq(i.domain), fq(i.mail_host));
    let mut out = vec![DnsRecord {
        name: domain.clone(),
        rtype: RecordType::Mx,
        value: format!("10 {host}"),
        ttl_hint: TTL,
        purpose: "Where mail for the domain is delivered",
        required: Requirement::Required,
    }];
    for ip in i.outbound_ips {
        out.push(DnsRecord {
            name: host.clone(),
            rtype: if ip.is_ipv4() { RecordType::A } else { RecordType::Aaaa },
            value: ip.to_string(),
            ttl_hint: TTL,
            purpose: "Address of the mail host",
            required: Requirement::Required,
        });
    }
    let mut spf = String::from("v=spf1 mx");
    for ip in i.outbound_ips {
        match ip {
            IpAddr::V4(a) => {
                let _ = write!(spf, " ip4:{a}");
            }
            IpAddr::V6(a) => {
                let _ = write!(spf, " ip6:{a}");
            }
        }
    }
    for inc in i.spf_includes {
        let _ = write!(spf, " include:{}", inc.to_ascii_lowercase());
    }
    spf.push_str(" -all");
    out.push(DnsRecord {
        name: domain.clone(),
        rtype: RecordType::Txt,
        value: spf,
        ttl_hint: TTL,
        purpose: "SPF: which hosts may send mail for the domain",
        required: Requirement::Required,
    });
    for k in i.dkim {
        out.push(DnsRecord {
            name: format!("{}._domainkey.{domain}", k.selector),
            rtype: RecordType::Txt,
            value: format!("v=DKIM1; k={}; p={}", k.kind.as_str(), k.public_key_b64),
            ttl_hint: TTL,
            purpose: "DKIM public key (receivers verify our signatures with it)",
            required: Requirement::Required,
        });
    }
    if i.dkim.is_empty() {
        out.push(DnsRecord {
            name: format!("<selector>._domainkey.{domain}"),
            rtype: RecordType::Txt,
            value: "run `rustyctl dkim generate` (ed25519) and/or `rustyctl dkim import` (RSA) to create the key and its record".to_owned(),
            ttl_hint: TTL,
            purpose: "DKIM public key (not generated yet)",
            required: Requirement::Recommended,
        });
    }
    let mut dmarc = String::from("v=DMARC1; p=none");
    if let Some(rua) = i.dmarc_rua {
        let _ = write!(dmarc, "; rua=mailto:{rua}");
    }
    out.push(DnsRecord {
        name: format!("_dmarc.{domain}"),
        rtype: RecordType::Txt,
        value: dmarc,
        ttl_hint: TTL,
        purpose: "DMARC policy (start with p=none and tighten later)",
        required: Requirement::Recommended,
    });
    for ip in i.outbound_ips {
        out.push(DnsRecord {
            name: reverse_name(*ip),
            rtype: RecordType::Ptr,
            value: host.clone(),
            ttl_hint: TTL,
            purpose: "Reverse DNS: set by the owner of the IP address (your provider), must match the mail host",
            required: Requirement::Required,
        });
    }
    for (service, port) in [("_submission._tcp", 587u16), ("_submissions._tcp", 465), ("_imaps._tcp", 993)] {
        out.push(DnsRecord {
            name: format!("{service}.{domain}"),
            rtype: RecordType::Srv,
            value: format!("0 1 {port} {host}"),
            ttl_hint: TTL,
            purpose: "Lets mail clients find the server (RFC 6186)",
            required: Requirement::Optional,
        });
    }
    Ok(out)
}

/// Longest string inside one TXT record (RFC 1035 character-string).
const TXT_CHUNK: usize = 255;

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A zone-file line: `name ttl IN TYPE value`. TXT values are quoted and split at 255 bytes.
pub fn zone_line(r: &DnsRecord) -> String {
    let data = if r.rtype == RecordType::Txt {
        let mut parts = Vec::new();
        let mut cur = String::new();
        for c in r.value.chars() {
            if cur.len() + c.len_utf8() > TXT_CHUNK {
                parts.push(quote(&cur));
                cur.clear();
            }
            cur.push(c);
        }
        parts.push(quote(&cur));
        parts.join(" ")
    } else {
        r.value.clone()
    };
    format!("{} {} IN {} {}", r.name, r.ttl_hint, r.rtype.as_str(), data)
}

/// A parsed zone line (the inverse of [`zone_line`], used to prove the output is well-formed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneLine {
    pub name: String,
    pub ttl: u32,
    pub rtype: RecordType,
    pub value: String,
}

/// Parse a line produced by [`zone_line`]. TXT strings are unquoted and joined.
pub fn parse_zone_line(line: &str) -> Result<ZoneLine> {
    let bad = || Error::Invalid("not a zone line".into());
    let mut it = line.splitn(5, ' ');
    let name = it.next().ok_or_else(bad)?.to_owned();
    let ttl = it.next().ok_or_else(bad)?.parse::<u32>().map_err(|_| bad())?;
    if it.next() != Some("IN") {
        return Err(bad());
    }
    let rtype = RecordType::parse(it.next().ok_or_else(bad)?).ok_or_else(bad)?;
    let data = it.next().ok_or_else(bad)?;
    let value = if rtype == RecordType::Txt {
        let mut out = String::new();
        let mut chars = data.chars().peekable();
        loop {
            while chars.peek() == Some(&' ') {
                chars.next();
            }
            match chars.next() {
                None => break,
                Some('"') => {}
                Some(_) => return Err(bad()),
            }
            loop {
                match chars.next().ok_or_else(bad)? {
                    '\\' => out.push(chars.next().ok_or_else(bad)?),
                    '"' => break,
                    c => out.push(c),
                }
            }
        }
        out
    } else {
        data.to_owned()
    };
    Ok(ZoneLine { name, ttl, rtype, value })
}
