//! The rules for a relay that a **tenant** chooses (admin-web design, section 14.4). A tenant now decides where the outbound service connects,
//! so the destination is a server-side-request-forgery surface: these pure functions say what is acceptable. They do no I/O. The
//! caller checks the **resolved addresses** with [`is_public_address`] at every connection, not only when the relay is entered (a name that
//! pointed at a public address yesterday may point inside today).
//!
//! Refusal texts are fixed and never echo the input.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::config::RelayTls;
use crate::{Error, Result};

/// The ports a tenant may name: SMTP, SMTPS, submission and the usual alternative submission port.
pub const TENANT_RELAY_PORTS: [u16; 4] = [25, 465, 587, 2525];

/// Longest user name accepted for a relay login.
pub const MAX_RELAY_USERNAME: usize = 256;

fn invalid(why: &str) -> Error {
    Error::Invalid(why.into())
}

/// Is this address one a tenant's relay may be reached at? Only globally routable unicast addresses: not loopback, private (RFC 1918, ULA),
/// link-local (the cloud metadata address `169.254.169.254` included), CGNAT (100.64/10), documentation, benchmarking, reserved,
/// multicast or unspecified. IPv4 embedded in an IPv6 address (mapped, NAT64, 6to4) is judged by the IPv4 address.
pub fn is_public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

fn public_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_multicast()
        || a.is_broadcast()
        || a.is_documentation()
        || o[0] == 0 // "this network"
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 shared address space (CGNAT)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || o[0] >= 240) // 240.0.0.0/4 reserved
}

fn public_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    if let Some(v4) = a.to_ipv4_mapped() {
        return public_v4(v4);
    }
    // NAT64 well-known prefix 64:ff9b::/96 carries an IPv4 address in the last 32 bits.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    // 6to4 (2002::/16) carries one in bits 16..48.
    if s[0] == 0x2002 {
        return public_v4(Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8));
    }
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] & 0xfff0) == 0x3ff0 // 3fff::/20 documentation (RFC 9637)
        || (s[0] == 0x2001 && s[1] == 0) // Teredo
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0]) // discard-only 100::/64
        || (s[0] & 0xe000) != 0x2000) // outside 2000::/3 (global unicast) is not routable on the internet
}

/// A relay as a tenant may state it, after the checks. The password is not part of it (it is sealed separately).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRelay {
    /// Lower-case ASCII host name without a trailing dot, or a public IP literal.
    pub host: String,
    pub port: u16,
    pub tls: RelayTls,
    pub username: Option<String>,
}

/// Check what a tenant entered. `host` must be a DNS name of two or more labels (no `localhost`, no single-label name) or a public IP
/// literal; `port` one of [`TENANT_RELAY_PORTS`]; `tls = none` is allowed only without a login (a password never crosses a clear connection);
/// the user name is one printable line.
pub fn validate_tenant_relay(host: &str, port: u16, tls: RelayTls, username: Option<&str>) -> Result<TenantRelay> {
    if !TENANT_RELAY_PORTS.contains(&port) {
        return Err(invalid("the relay port must be 25, 465, 587 or 2525"));
    }
    let host = normalize_host(host)?;
    if let Some(u) = username {
        if u.is_empty() || u.len() > MAX_RELAY_USERNAME || u.chars().any(char::is_control) {
            return Err(invalid("the relay user name must be one printable line of at most 256 bytes"));
        }
        if tls == RelayTls::None {
            return Err(invalid("a relay with a login needs TLS (starttls or implicit)"));
        }
    }
    if tls == RelayTls::Implicit && port != 465 {
        // Implicit TLS on 25/587 would never speak SMTP first; refusing it catches a swapped setting at entry.
        return Err(invalid("implicit TLS belongs on port 465"));
    }
    Ok(TenantRelay { host, port, tls, username: username.map(str::to_owned) })
}

pub(crate) fn normalize_host(host: &str) -> Result<String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return Err(invalid("the relay host must be an ASCII DNS name or a public IP address"));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public_address(ip) { Ok(ip.to_string()) } else { Err(invalid("the relay host is not a public address")) };
    }
    // Anything that looks like a number or a bracketed literal but did not parse as an address is not a name either.
    if host.starts_with('[') || host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err(invalid("the relay host must be an ASCII DNS name or a public IP address"));
    }
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if labels.len() < 2 || !labels.iter().all(label_ok) {
        return Err(invalid("the relay host must be an ASCII DNS name of at least two labels or a public IP address"));
    }
    // Names that only ever mean "inside": refused before any lookup (the resolved addresses are checked as well).
    let last = labels[labels.len() - 1];
    if last.bytes().all(|b| b.is_ascii_digit()) {
        // No top-level domain is numeric: this is a mangled address ("0x7f.1"), which some resolvers would read as one.
        return Err(invalid("the relay host must be an ASCII DNS name or a public IP address"));
    }
    if matches!(last, "local" | "localhost" | "internal" | "localdomain" | "lan" | "home" | "corp" | "intranet" | "invalid" | "test" | "example") {
        return Err(invalid("the relay host is a name that is not on the public internet"));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_addresses_are_accepted() {
        for s in ["8.8.8.8", "1.1.1.1", "203.0.114.1", "74.125.0.1", "2606:4700:4700::1111", "2a00:1450:4001::1"] {
            assert!(is_public_address(ip(s)), "{s}");
        }
    }

    #[test]
    fn everything_inside_or_unroutable_is_refused() {
        for s in [
            "0.0.0.0", "0.1.2.3", "10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.1", "127.0.0.1", "127.255.255.254", "169.254.169.254",
            "169.254.0.1", "100.64.0.1", "100.127.255.255", "192.0.0.1", "192.0.2.1", "198.51.100.1", "203.0.113.5", "198.18.0.1", "198.19.255.255",
            "224.0.0.1", "239.1.1.1", "240.0.0.1", "255.255.255.255", "::", "::1", "fe80::1", "fc00::1", "fd12:3456::1", "ff02::1", "2001:db8::1",
            "2001::1", "100::1", "::ffff:10.0.0.1", "::ffff:127.0.0.1", "::ffff:169.254.169.254", "64:ff9b::a00:1", "64:ff9b::7f00:1", "2002:0a00:0001::1",
            "2002:7f00:1::1", "3fff::1", "4000::1", "5f00::1",
        ] {
            assert!(!is_public_address(ip(s)), "{s}");
        }
    }

    #[test]
    fn the_edges_of_the_private_ranges_are_exact() {
        assert!(is_public_address(ip("172.15.255.255")));
        assert!(!is_public_address(ip("172.16.0.0")));
        assert!(!is_public_address(ip("172.31.255.255")));
        assert!(is_public_address(ip("172.32.0.0")));
        assert!(is_public_address(ip("100.63.255.255")));
        assert!(!is_public_address(ip("100.64.0.0")));
        assert!(!is_public_address(ip("100.127.255.255")));
        assert!(is_public_address(ip("100.128.0.0")));
        assert!(is_public_address(ip("198.17.255.255")));
        assert!(!is_public_address(ip("198.18.0.0")));
        assert!(is_public_address(ip("198.20.0.0")));
        assert!(is_public_address(ip("::ffff:8.8.8.8")));
        assert!(is_public_address(ip("64:ff9b::808:808")));
        assert!(is_public_address(ip("2002:0808:0808::1")));
    }

    #[test]
    fn a_valid_relay_is_normalised() {
        let r = validate_tenant_relay("SMTP.Example-Mail.NET.", 587, RelayTls::Starttls, Some("user@x")).unwrap();
        assert_eq!(r.host, "smtp.example-mail.net");
        assert_eq!(r.port, 587);
        let r = validate_tenant_relay("8.8.8.8", 465, RelayTls::Implicit, None).unwrap();
        assert_eq!(r.host, "8.8.8.8");
        assert!(validate_tenant_relay("smtp.mail.net", 25, RelayTls::None, None).is_ok());
        assert!(validate_tenant_relay("smtp.mail.net", 2525, RelayTls::Starttls, None).is_ok());
    }

    #[test]
    fn only_the_four_ports_are_allowed() {
        for p in [0, 22, 80, 110, 143, 443, 993, 3306, 5432, 6379, 8080, 65535] {
            assert!(validate_tenant_relay("smtp.mail.net", p, RelayTls::Starttls, None).is_err(), "{p}");
        }
    }

    #[test]
    fn inside_names_and_addresses_are_refused_at_entry() {
        for h in [
            "localhost", "intranet", "mail.local", "mail.internal", "relay.lan", "a.localhost", "10.0.0.5", "127.0.0.1", "169.254.169.254", "[::1]", "::1",
            "fd00::1", "192.168.0.1", "100.64.1.1", "0x7f.1", "2130706433", "1.2.3", "smtp.example", "", ".", "..", "a..b.net", "-a.net", "a-.net",
            "sm tp.net", "smtp.münchen.de", "smtp_x.net", "a.b.c.d.e.f", "[smtp.net]",
        ] {
            let r = validate_tenant_relay(h, 587, RelayTls::Starttls, None);
            if h == "a.b.c.d.e.f" {
                assert!(r.is_ok(), "{h} is a valid six-label name");
            } else {
                assert!(r.is_err(), "{h:?}");
            }
        }
        assert!(validate_tenant_relay(&format!("{}.net", "a".repeat(64)), 587, RelayTls::Starttls, None).is_err());
        assert!(validate_tenant_relay(&format!("{}.net", "a".repeat(63)), 587, RelayTls::Starttls, None).is_ok());
        assert!(validate_tenant_relay(&format!("{}.net", "a.".repeat(130)), 587, RelayTls::Starttls, None).is_err());
    }

    #[test]
    fn a_login_never_crosses_a_clear_connection() {
        assert!(validate_tenant_relay("smtp.mail.net", 587, RelayTls::None, Some("u")).is_err());
        assert!(validate_tenant_relay("smtp.mail.net", 587, RelayTls::Starttls, Some("u")).is_ok());
        assert!(validate_tenant_relay("smtp.mail.net", 465, RelayTls::Implicit, Some("u")).is_ok());
    }

    #[test]
    fn implicit_tls_needs_port_465() {
        assert!(validate_tenant_relay("smtp.mail.net", 587, RelayTls::Implicit, None).is_err());
        assert!(validate_tenant_relay("smtp.mail.net", 25, RelayTls::Implicit, None).is_err());
        assert!(validate_tenant_relay("smtp.mail.net", 465, RelayTls::Starttls, None).is_ok());
    }

    #[test]
    fn the_user_name_is_one_printable_line() {
        for u in ["", "a\nb", "a\0b", "a\rb"] {
            assert!(validate_tenant_relay("smtp.mail.net", 587, RelayTls::Starttls, Some(u)).is_err(), "{u:?}");
        }
        assert!(validate_tenant_relay("smtp.mail.net", 587, RelayTls::Starttls, Some(&"u".repeat(256))).is_ok());
        assert!(validate_tenant_relay("smtp.mail.net", 587, RelayTls::Starttls, Some(&"u".repeat(257))).is_err());
    }

    #[test]
    fn refusals_never_echo_the_input() {
        let secret = "TOPSECRET-host-1234.internal";
        let e = validate_tenant_relay(secret, 587, RelayTls::Starttls, Some("TOPSECRET-user")).unwrap_err().to_string();
        assert!(!e.contains("TOPSECRET"), "{e}");
        let e = validate_tenant_relay("smtp.mail.net", 587, RelayTls::None, Some("TOPSECRET-user")).unwrap_err().to_string();
        assert!(!e.contains("TOPSECRET"), "{e}");
    }
}
