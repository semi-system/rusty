//! `Cidr`: an address block (`10.0.0.0/8`, `2001:db8::/32`, or a single address) for the lists the configuration names, such as the proxies that may
//! speak the PROXY protocol. Small on purpose: parse, normalise, test membership. No dependency.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// The block of `addr` with `prefix` bits; the host bits are cleared (`10.1.2.3/8` is `10.0.0.0/8`). `None` for a prefix too long for the family.
    pub fn new(addr: IpAddr, prefix: u8) -> Option<Cidr> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        (prefix <= max).then(|| Cidr { network: mask(addr, prefix), prefix })
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Is `ip` in the block? An IPv4-mapped IPv6 address (`::ffff:10.0.0.1`) counts as its IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.network.is_ipv4() && mask(ip, self.prefix) == self.network
    }

    /// The block holds every address of its family (`/0`).
    pub fn is_everything(&self) -> bool {
        self.prefix == 0
    }
}

fn mask(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let bits = u32::from(a);
            let m = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
            IpAddr::V4((bits & m).into())
        }
        IpAddr::V6(a) => {
            let bits = u128::from(a);
            let m = if prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(prefix.min(128))) };
            IpAddr::V6((bits & m).into())
        }
    }
}

impl FromStr for Cidr {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Cidr, &'static str> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => {
                if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                    return Err("an address block needs a prefix length of digits");
                }
                (a, Some(p.parse::<u8>().map_err(|_| "the prefix length is too large")?))
            }
            None => (s, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| "not an IP address")?;
        let prefix = prefix.unwrap_or(if addr.is_ipv4() { 32 } else { 128 });
        Cidr::new(addr, prefix).ok_or("the prefix length is too large for the address family")
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Cidr, D::Error> {
        let s = String::deserialize(d)?;
        // The text of a bad value is never echoed (it could be pasted by mistake from somewhere else).
        s.parse().map_err(|why: &str| de::Error::custom(why))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Cidr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_contain_exactly_their_addresses() {
        let b = c("10.0.0.0/8");
        assert!(b.contains(ip("10.0.0.1")) && b.contains(ip("10.255.255.255")));
        assert!(!b.contains(ip("11.0.0.0")) && !b.contains(ip("9.255.255.255")) && !b.contains(ip("::1")));
        assert!(c("192.0.2.7").contains(ip("192.0.2.7")) && !c("192.0.2.7").contains(ip("192.0.2.8")), "no prefix is one address");
        let v6 = c("2001:db8::/32");
        assert!(v6.contains(ip("2001:db8:ffff::1")) && !v6.contains(ip("2001:db9::1")) && !v6.contains(ip("10.0.0.1")));
        assert!(c("::1").contains(ip("::1")) && !c("::1").contains(ip("::2")));
    }

    #[test]
    fn host_bits_are_cleared_and_the_text_is_normalised() {
        assert_eq!(c("10.9.8.7/8").to_string(), "10.0.0.0/8");
        assert_eq!(c("10.9.8.7/8"), c("10.0.0.0/8"));
        assert_eq!(c("2001:db8:1::5/32").to_string(), "2001:db8::/32");
    }

    #[test]
    fn an_ipv4_mapped_address_is_its_ipv4_address() {
        assert!(c("10.0.0.0/8").contains(ip("::ffff:10.1.2.3")));
        assert!(!c("10.0.0.0/8").contains(ip("::ffff:11.1.2.3")));
        assert!(c("127.0.0.0/8").contains(ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn the_edges_of_the_prefix_length() {
        assert!(c("0.0.0.0/0").is_everything() && c("::/0").is_everything() && !c("10.0.0.0/8").is_everything());
        assert!(c("0.0.0.0/0").contains(ip("203.0.113.1")) && !c("0.0.0.0/0").contains(ip("::1")));
        assert!(c("10.0.0.1/32").contains(ip("10.0.0.1")) && c("::1/128").prefix() == 128);
    }

    #[test]
    fn bad_text_is_refused() {
        for bad in ["", "10.0.0.0/33", "::/129", "10.0.0.0/", "10.0.0.0/-1", "10.0.0.0/08", "10.0.0.0/8/8", "10.0.0/8", "example.org", "10.0.0.0/ 8", " 10.0.0.0/8", "10.0.0.0/999", "1.2.3.4/x"] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad:?}");
        }
    }
}
