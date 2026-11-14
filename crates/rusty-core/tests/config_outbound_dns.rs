//! `[outbound] public_ips` and `[outbound.relay] spf_include`: what the DNS page and `rustyctl domain check` need to know to
//! write the SPF record and the address records (the addresses mail leaves from, and the relay provider's SPF include).

use rusty_core::config::StartupConfig;
use rusty_core::Error;

const BASE: &str = "blob_dir = \"/a/blobs\"
store = { url = \"postgres://rusty_app@db.internal:5432/rusty\", password_file = \"/run/secrets/pg\" }
";

fn parse(extra: &str) -> Result<StartupConfig, Error> {
    StartupConfig::from_toml_str(&format!("{BASE}{extra}"))
}

#[test]
fn public_ips_default_to_none_and_accept_v4_and_v6() {
    assert!(parse("").unwrap().outbound.public_ips.is_empty());
    let c = parse("[outbound]\npublic_ips = [\"203.0.113.5\", \"2001:db8::5\"]\n").unwrap();
    assert_eq!(c.outbound.public_ips, vec!["203.0.113.5".parse::<std::net::IpAddr>().unwrap(), "2001:db8::5".parse().unwrap()]);
}

#[test]
fn addresses_that_cannot_be_published_are_refused() {
    for bad in [
        "\"0.0.0.0\"",
        "\"127.0.0.1\"",
        "\"::1\"",
        "\"::\"",
        "\"224.0.0.1\"",
        "\"ff02::1\"",
        "\"169.254.1.1\"",
        "\"fe80::1\"",
        "\"255.255.255.255\"",
        "\"not an address\"",
        "\"203.0.113.5/24\"",
        "\"203.0.113.5\", \"203.0.113.5\"",
    ] {
        let r = parse(&format!("[outbound]\npublic_ips = [{bad}]\n"));
        assert!(matches!(r, Err(Error::Invalid(_))), "{bad}");
    }
    let nine: Vec<String> = (1..=9).map(|i| format!("\"203.0.113.{i}\"")).collect();
    assert!(parse(&format!("[outbound]\npublic_ips = [{}]\n", nine.join(", "))).is_err(), "at most 8 addresses");
}

#[test]
fn the_spf_include_of_a_relay_is_a_host_name() {
    let relay = "[outbound.relay]\nhost = \"smtp.example.net\"\nport = 587\n";
    let c = parse(&format!("{relay}spf_include = \"amazonses.com\"\n")).unwrap();
    assert_eq!(c.outbound.relay.unwrap().spf_include.as_deref(), Some("amazonses.com"));
    assert_eq!(parse(relay).unwrap().outbound.relay.unwrap().spf_include, None);
    for bad in ["", "a b", "include:x.example", "x.example.", "_spf.google.com/x", "x;y", "-bad.example"] {
        let r = parse(&format!("{relay}spf_include = \"{bad}\"\n"));
        assert!(matches!(r, Err(Error::Invalid(_))), "{bad:?}");
    }
}
