//! The data of the hosted zones, as the answer engine reads it: names are lower case, fully qualified and without the trailing dot (`example.org`, `_dmarc.example.org`); a zone is the records of one domain
//! and the SOA that goes with them. There are no wildcards and no delegations inside a zone: what is not written is not there.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// The record types the server knows. Anything else asked for is answered as "no data" (the name exists, that type does not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RType {
    A,
    Aaaa,
    Ns,
    Cname,
    Soa,
    Mx,
    Txt,
    Srv,
    Caa,
    /// `ANY` (255): never a stored type, only a question.
    Any,
    /// A type this server does not hold (its number).
    Other(u16),
}

impl RType {
    /// The number on the wire.
    pub fn code(self) -> u16 {
        match self {
            RType::A => 1,
            RType::Ns => 2,
            RType::Cname => 5,
            RType::Soa => 6,
            RType::Mx => 15,
            RType::Txt => 16,
            RType::Aaaa => 28,
            RType::Srv => 33,
            RType::Caa => 257,
            RType::Any => 255,
            RType::Other(n) => n,
        }
    }

    pub fn from_code(n: u16) -> RType {
        match n {
            1 => RType::A,
            2 => RType::Ns,
            5 => RType::Cname,
            6 => RType::Soa,
            15 => RType::Mx,
            16 => RType::Txt,
            28 => RType::Aaaa,
            33 => RType::Srv,
            257 => RType::Caa,
            255 => RType::Any,
            n => RType::Other(n),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Soa {
    pub mname: String,
    pub rname: String,
    pub serial: u32,
    pub refresh: u32,
    pub retry: u32,
    pub expire: u32,
    /// The negative-caching time of the zone (RFC 2308).
    pub minimum: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rdata {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ns(String),
    Cname(String),
    Soa(Soa),
    Mx { preference: u16, host: String },
    /// The strings of one TXT record, each at most 255 bytes (the wire format's limit; the zone builder splits a longer value).
    Txt(Vec<String>),
    Srv { priority: u16, weight: u16, port: u16, target: String },
    Caa { flags: u8, tag: String, value: String },
}

impl Rdata {
    pub fn rtype(&self) -> RType {
        match self {
            Rdata::A(_) => RType::A,
            Rdata::Aaaa(_) => RType::Aaaa,
            Rdata::Ns(_) => RType::Ns,
            Rdata::Cname(_) => RType::Cname,
            Rdata::Soa(_) => RType::Soa,
            Rdata::Mx { .. } => RType::Mx,
            Rdata::Txt(_) => RType::Txt,
            Rdata::Srv { .. } => RType::Srv,
            Rdata::Caa { .. } => RType::Caa,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Lower case, fully qualified, no trailing dot.
    pub name: String,
    pub ttl: u32,
    pub rdata: Rdata,
}

/// The lower-case, dot-less form of a name: what every comparison here uses (DNS names are case-insensitive).
pub fn normal(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// One hosted domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub origin: String,
    pub soa: Soa,
    /// The SOA's own time to live (also the time negative answers are cached for, when smaller than `soa.minimum`).
    pub soa_ttl: u32,
    /// Records by name, in a stable order.
    records: BTreeMap<String, Vec<Record>>,
}

impl Zone {
    /// A zone with its SOA and no other record. The name servers (`NS` at the apex) are added like any record.
    pub fn new(origin: &str, soa: Soa, soa_ttl: u32) -> Zone {
        let origin = normal(origin);
        let mut z = Zone { origin: origin.clone(), soa: soa.clone(), soa_ttl, records: BTreeMap::new() };
        z.records.entry(origin.clone()).or_default().push(Record { name: origin, ttl: soa_ttl, rdata: Rdata::Soa(soa) });
        z
    }

    /// Add a record. A name outside the zone is refused (`false`); so is a second `CNAME` for a name, a `CNAME` next to other data, and other data next to a `CNAME` (RFC 1034 section 3.6.2).
    pub fn add(&mut self, name: &str, ttl: u32, rdata: Rdata) -> bool {
        let name = normal(name);
        if !in_zone(&name, &self.origin) {
            return false;
        }
        let here = self.records.entry(name.clone()).or_default();
        let has_cname = here.iter().any(|r| r.rdata.rtype() == RType::Cname);
        let adding_cname = rdata.rtype() == RType::Cname;
        if (adding_cname && !here.is_empty()) || (!adding_cname && has_cname) {
            if here.is_empty() {
                self.records.remove(&name);
            }
            return false;
        }
        let rdata = match rdata {
            Rdata::Ns(h) => Rdata::Ns(normal(&h)),
            Rdata::Cname(h) => Rdata::Cname(normal(&h)),
            Rdata::Mx { preference, host } => Rdata::Mx { preference, host: normal(&host) },
            Rdata::Srv { priority, weight, port, target } => Rdata::Srv { priority, weight, port, target: normal(&target) },
            other => other,
        };
        let r = Record { name, ttl, rdata };
        if !here.contains(&r) {
            here.push(r);
        }
        true
    }

    /// The records at a name.
    pub fn at(&self, name: &str) -> &[Record] {
        self.records.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether the name has records, or has a name below it that does (an "empty non-terminal": it exists, with no data).
    pub fn exists(&self, name: &str) -> bool {
        if self.records.contains_key(name) {
            return true;
        }
        let below = format!(".{name}");
        self.records.keys().any(|k| k.ends_with(&below))
    }

    /// Every record of the zone.
    pub fn all(&self) -> impl Iterator<Item = &Record> {
        self.records.values().flatten()
    }
}

/// Whether `name` is `origin` or a name below it (at a label boundary).
pub fn in_zone(name: &str, origin: &str) -> bool {
    name == origin || name.strip_suffix(origin).is_some_and(|rest| rest.ends_with('.'))
}

/// All the hosted zones.
#[derive(Debug, Clone, Default)]
pub struct Zones {
    zones: BTreeMap<String, Zone>,
}

impl Zones {
    pub fn new() -> Zones {
        Zones::default()
    }

    pub fn insert(&mut self, zone: Zone) {
        self.zones.insert(zone.origin.clone(), zone);
    }

    pub fn len(&self) -> usize {
        self.zones.len()
    }

    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    /// The zone that holds `name`: the one with the longest origin that `name` is in (`mail.example.org` is in `example.org`, unless `mail.example.org` is a zone of its own).
    pub fn find(&self, name: &str) -> Option<&Zone> {
        let mut candidate = name;
        loop {
            if let Some(z) = self.zones.get(candidate) {
                return Some(z);
            }
            match candidate.split_once('.') {
                Some((_, rest)) => candidate = rest,
                None => return None,
            }
        }
    }
}
