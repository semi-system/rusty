//! Edge behaviour suite: the go/no-go evidence for the HAProxy edge (plan task H2).
//!
//! Run (needs Docker, openssl, nc in the same environment as `cargo`):
//!   cargo test -p edge-tests -- --ignored --nocapture
//! `EDGE_ONLY=a,b` runs the cases whose name contains one of the substrings, `EDGE_KEEP=1`
//! leaves the stack running afterwards. The project is always torn down otherwise.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edge_tests::{
    banner_source, client_hello, poll, random_bytes, read_line, run, send_hello, write_report,
    EdgeUnderTest, HelloAnswer, Row, Status,
};

type Outcome = Result<(Status, String), String>;
type CaseFn = fn(&EdgeUnderTest, &mut Vec<Row>) -> Outcome;

const PASSTHROUGH: [(&str, u16, u16, &str); 4] = [
    ("smtp", 25, 10025, "220 fake SMTP ready"),
    ("submission", 587, 10587, "220 fake submission starttls"),
    ("imap", 143, 10143, "* OK fake IMAP starttls"),
    ("pop3", 110, 10110, "+OK fake POP3 starttls"),
];

fn settle() {
    // The per-IP stick tables use a 10 s rate window; wait it out between heavy cases.
    thread::sleep(Duration::from_secs(12));
}

fn pass(s: impl Into<String>) -> Outcome {
    Ok((Status::Pass, s.into()))
}

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(msg())
    }
}

fn host_source(edge: &EdgeUnderTest) -> Result<String, String> {
    let mut s = edge.connect(edge.ports.imap).map_err(|e| e.to_string())?;
    let line = read_line(&mut s).map_err(|e| e.to_string())?;
    banner_source(&line).ok_or_else(|| format!("no banner: {line:?}"))
}

fn spoof_header() -> Vec<u8> {
    let mut h = vec![0x0d, 0x0a, 0x0d, 0x0a, 0x00, 0x0d, 0x0a, 0x51, 0x55, 0x49, 0x54, 0x0a];
    h.extend([0x21, 0x11, 0x00, 0x0c, 6, 6, 6, 6, 7, 7, 7, 7, 0x30, 0x39, 0x00, 0x19]);
    h
}

// ---------------------------------------------------------------------------------------
// Pass-through
// ---------------------------------------------------------------------------------------

fn tcp_passthrough_preserves_bytes(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let mut detail = Vec::new();
    for (name, port, _, prefix) in PASSTHROUGH {
        let mut s = edge.connect(port).map_err(|e| format!("{name}: {e}"))?;
        let banner = read_line(&mut s).map_err(|e| e.to_string())?;
        ensure(banner.starts_with(prefix), || format!("{name}: banner {banner:?}"))?;
        for n in [1usize, 4093, 1 << 20] {
            let data = random_bytes(n);
            s.write_all(format!("ECHO {n}\r\n").as_bytes())
                .and_then(|()| s.write_all(&data))
                .map_err(|e| format!("{name}: write {n}: {e}"))?;
            let mut back = vec![0u8; n];
            s.read_exact(&mut back)
                .map_err(|e| format!("{name}: read {n}: {e}"))?;
            ensure(back == data, || format!("{name}: {n} bytes changed"))?;
        }
        detail.push(format!("{name}:{port} 1B,4093B,1MiB identical"));
    }
    pass(detail.join("; "))
}

fn starttls_upgrade_survives_the_edge(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let backend = edge.cert_der("backend.crt");
    let mut detail = Vec::new();
    for (name, port, _, _) in PASSTHROUGH {
        let mut tcp = edge.connect(port).map_err(|e| e.to_string())?;
        read_line(&mut tcp).map_err(|e| e.to_string())?;
        tcp.write_all(b"STARTTLS\r\n").map_err(|e| e.to_string())?;
        let reply = read_line(&mut tcp).map_err(|e| e.to_string())?;
        ensure(reply.starts_with("220 begin TLS"), || {
            format!("{name}: STARTTLS reply {reply:?}")
        })?;
        let mut tls = edge.tls_wrap(tcp, "be-imap", edge.tls_all())?;
        let peer = tls.conn.peer_certificates().map(<[_]>::to_vec).unwrap_or_default();
        ensure(peer.first() == backend.first(), || {
            format!("{name}: peer certificate is not the backend certificate")
        })?;
        tls.write_all(b"hello-through-edge")
            .map_err(|e| e.to_string())?;
        let mut back = [0u8; 18];
        tls.read_exact(&mut back).map_err(|e| e.to_string())?;
        ensure(&back == b"hello-through-edge", || format!("{name}: echo mismatch"))?;
        detail.push(format!(
            "{name}:{port} backend cert end to end, {:?}",
            tls.conn.protocol_version()
        ));
    }
    pass(detail.join("; "))
}

fn proxy_v2_reports_real_client_ip(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let probe = edge.start_probe()?;
    let host_src = host_source(edge)?;
    let mut detail = vec![format!("host client seen as {host_src}, probe client {}", probe.ip)];
    ensure(host_src != probe.ip, || "host and probe share an IP; test is blind".to_owned())?;
    for (name, _, internal, prefix) in PASSTHROUGH {
        let b = probe.banner(internal);
        ensure(b.starts_with(prefix), || format!("{name}: probe banner {b:?}"))?;
        ensure(banner_source(&b).as_deref() == Some(probe.ip.as_str()), || {
            format!("{name}: backend saw {b:?}, probe is {}", probe.ip)
        })?;
    }
    detail.push("pass-through: backend saw the probe's own IP on 25/587/143/110".to_owned());
    for (name, port) in [
        ("smtps", edge.ports.smtps),
        ("imaps", edge.ports.imaps),
        ("pop3s", edge.ports.pop3s),
    ] {
        let mut tls = edge.tls_connect(port, "mail.test")?;
        let line = read_line(&mut tls).map_err(|e| e.to_string())?;
        ensure(banner_source(&line).as_deref() == Some(host_src.as_str()), || {
            format!("{name}: backend saw {line:?}, host is {host_src}")
        })?;
    }
    detail.push("terminated 465/993/995: backend saw the same host IP".to_owned());
    pass(detail.join("; "))
}

fn client_supplied_proxy_header_cannot_spoof_source_ip(
    edge: &EdgeUnderTest,
    _: &mut Vec<Row>,
) -> Outcome {
    let real = host_source(edge)?;
    let mut detail = Vec::new();
    for (name, port, _, prefix) in PASSTHROUGH {
        let mut s = edge.connect(port).map_err(|e| e.to_string())?;
        s.write_all(&spoof_header()).map_err(|e| e.to_string())?;
        let line = read_line(&mut s).map_err(|e| e.to_string())?;
        ensure(line.starts_with(prefix), || format!("{name}: {line:?}"))?;
        ensure(banner_source(&line).as_deref() == Some(real.as_str()), || {
            format!("{name}: backend saw {line:?}, real client is {real}")
        })?;
    }
    detail.push(format!("pass-through: spoofed 6.6.6.6 ignored, backend saw {real}"));
    for (name, port) in [("imaps", edge.ports.imaps), ("https", edge.ports.https)] {
        let mut data = spoof_header();
        data.extend(client_hello(b"mail.test"));
        let a = send_hello(edge, port, &data);
        ensure(a != HelloAnswer::ServerHello, || {
            format!("{name}: edge accepted a PROXY preamble before TLS")
        })?;
        detail.push(format!("{name}: PROXY preamble before TLS -> {a:?}"));
    }
    // Through HTTPS the backend reports the address it learned from the edge.
    let mut tls = edge.tls_connect(edge.ports.https, "web.test")?;
    tls.write_all(b"GET / HTTP/1.1\r\nHost: web.test\r\nX-Forwarded-For: 6.6.6.6\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let body = read_all(&mut tls);
    ensure(body.contains(&format!("source={real}")), || format!("https backend view: {body:?}"))?;
    detail.push("https: backend source is the real client despite X-Forwarded-For".to_owned());
    pass(detail.join("; "))
}

fn read_all(s: &mut impl Read) -> String {
    let mut buf = Vec::new();
    let _ = s.take(256 * 1024).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------------------------------------------------------------------------------------
// Termination
// ---------------------------------------------------------------------------------------

fn terminated() -> [&'static str; 4] {
    ["smtps", "imaps", "pop3s", "https"]
}

fn terminated_port(edge: &EdgeUnderTest, name: &str) -> u16 {
    match name {
        "smtps" => edge.ports.smtps,
        "imaps" => edge.ports.imaps,
        "pop3s" => edge.ports.pop3s,
        _ => edge.ports.https,
    }
}

fn terminated_ports_present_the_right_cert_by_sni(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let expect = [
        ("mail.test", Some("mail-a.pem")),
        ("imap.test", Some("mail-a.pem")),
        ("pop.test", Some("mail-a.pem")),
        ("smtp.test", Some("mail-a.pem")),
        ("other.test", Some("mail-b.pem")),
        ("web.test", Some("web.pem")),
        ("unknown.test", None),
    ];
    let mut detail = Vec::new();
    for name in terminated() {
        let port = terminated_port(edge, name);
        for (sni, file) in expect {
            let got = edge.presented_cert(port, Some(sni));
            match (file, got) {
                (Some(f), Ok(fp)) => ensure(fp == edge.file_fingerprint(f), || {
                    format!("{name}/{sni}: wrong certificate {fp}")
                })?,
                (None, Err(why)) => detail.push(format!("{name}/{sni}: refused ({why})")),
                (None, Ok(fp)) => return Err(format!("{name}/{sni}: unknown SNI got a certificate {fp}")),
                (Some(_), Err(why)) => return Err(format!("{name}/{sni}: handshake failed: {why}")),
            }
        }
        match edge.presented_cert(port, None) {
            Ok(fp) => return Err(format!("{name}: no SNI got a certificate {fp}")),
            Err(why) => detail.push(format!("{name}/no-SNI: refused ({why})")),
        }
    }
    detail.insert(
        0,
        "expected: known SNI -> matching cert, unknown SNI and no SNI -> handshake refused (strict-sni); verified on 465/993/995/443".to_owned(),
    );
    detail.truncate(4);
    pass(detail.join("; "))
}

fn sni_never_reaches_a_file_path(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let long = vec![b'a'; 5000];
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("traversal", b"../../etc/passwd".to_vec()),
        ("absolute", b"/etc/passwd".to_vec()),
        ("encoded", b"..%2f..%2fetc%2fpasswd".to_vec()),
        ("dotdot", b"..".to_vec()),
        ("wildcard", b"*.test".to_vec()),
        ("very-long", long),
        ("embedded-nul", b"mail\0.test".to_vec()),
        ("empty", Vec::new()),
    ];
    let control = send_hello(edge, edge.ports.imaps, &client_hello(b"mail.test"));
    ensure(control == HelloAnswer::ServerHello, || {
        format!("raw ClientHello control failed: {control:?}")
    })?;
    let mut detail = Vec::new();
    for (label, sni) in &cases {
        let a = send_hello(edge, edge.ports.imaps, &client_hello(sni));
        ensure(a != HelloAnswer::ServerHello, || format!("{label}: got a ServerHello"))?;
        detail.push(format!("{label}->{a:?}"));
    }
    let nul_prefix = send_hello(edge, edge.ports.imaps, &client_hello(b"mail.test\0.evil"));
    detail.push(format!("nul-after-valid-name->{nul_prefix:?}"));
    let after = send_hello(edge, edge.ports.imaps, &client_hello(b"mail.test"));
    ensure(after == HelloAnswer::ServerHello, || "edge unhealthy after SNI abuse".to_owned())?;
    let logs = edge.edge_logs().to_ascii_lowercase();
    ensure(!logs.contains("passwd") && !logs.contains("no such file"), || {
        "edge logs mention a file lookup".to_owned()
    })?;
    pass(format!("only crt-list names select certs; {}", detail.join(", ")))
}

fn override_cert(service: &str, cert: &str, client_ca: Option<&str>) -> String {
    let mut y = format!(
        "services:\n  {service}:\n    environment:\n      RUSTY_CERT: /certs/{cert}.crt\n      RUSTY_KEY: /certs/{cert}.key\n"
    );
    if let Some(ca) = client_ca {
        y.push_str(&format!("      RUSTY_CLIENT_CA: /certs/{ca}.crt\n"));
    }
    y
}

fn imaps_banner(edge: &EdgeUnderTest) -> Option<String> {
    let mut tls = edge.tls_connect(edge.ports.imaps, "imap.test").ok()?;
    let line = read_line(&mut tls).ok()?;
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn backend_link_is_verified_mtls(edge: &EdgeUnderTest, rows: &mut Vec<Row>) -> Outcome {
    ensure(imaps_banner(edge).is_some(), || "control: no banner with the good backend".to_owned())?;
    let scenarios = [
        ("wrong-ca", override_cert("be-imap", "backend-wrongca", None)),
        ("wrong-name", override_cert("be-imap", "backend-wrongname", None)),
        ("expired", override_cert("be-imap", "backend-expired", None)),
        (
            "backend-rejects-edge-client-cert",
            override_cert("be-imap", "backend", Some("other-ca")),
        ),
    ];
    let mut detail = Vec::new();
    let mut result = Ok(());
    for (label, yaml) in &scenarios {
        edge.recreate_backend("be-imap", yaml)?;
        let mut leaked = None;
        // The edge health check (2 s interval, rise/fall defaults) needs a few seconds to react;
        // no attempt at any time may ever receive backend data.
        let started = Instant::now();
        let mut attempts = 0;
        while started.elapsed() < Duration::from_secs(14) {
            attempts += 1;
            if let Some(b) = imaps_banner(edge) {
                leaked = Some(b);
                break;
            }
            thread::sleep(Duration::from_millis(900));
        }
        if let Some(b) = leaked {
            result = Err(format!("{label}: backend data reached the client: {b:?}"));
            break;
        }
        detail.push(format!("{label}: {attempts} attempts, no data"));
    }
    // Finding probe (not a pass/fail criterion): CN equals the server name, SAN list does not.
    if result.is_ok() {
        edge.recreate_backend("be-imap", &override_cert("be-imap", "backend-cnonly", None))?;
        let accepted = poll(Duration::from_secs(14), || imaps_banner(edge).is_some());
        rows.push(Row {
            name: "backend_cert_cn_fallback_probe".to_owned(),
            status: Status::Info,
            detail: if accepted {
                "FINDING: a backend certificate whose SAN list lacks the server name but whose subject CN equals it WAS accepted (CN fallback despite SAN present). Mitigation: the internal CA must only issue certificates whose SAN is authoritative".to_owned()
            } else {
                "backend certificate with matching CN but non-matching SAN was refused (SAN is authoritative)".to_owned()
            },
            secs: 0.0,
        });
    }
    edge.restore_backend("be-imap")?;
    let back = poll(Duration::from_secs(40), || imaps_banner(edge).is_some());
    result?;
    ensure(back, || "good backend did not come back through the edge".to_owned())?;
    let logs = edge.edge_logs();
    ensure(logs.contains("imap_tls/imap is DOWN"), || {
        "edge never logged the TLS backend as DOWN".to_owned()
    })?;
    pass(detail.join("; "))
}

fn proxy_v2_and_tls_to_backend_work_together(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let host_src = host_source(edge)?;
    let mut detail = Vec::new();
    for (name, port, expect) in [
        ("smtps", edge.ports.smtps, "220 fake submission tls"),
        ("imaps", edge.ports.imaps, "* OK fake IMAPS"),
        ("pop3s", edge.ports.pop3s, "+OK fake POP3S"),
    ] {
        let mut tls = edge.tls_connect(port, "mail.test")?;
        let line = read_line(&mut tls).map_err(|e| e.to_string())?;
        ensure(line.starts_with(expect), || format!("{name}: banner {line:?}"))?;
        ensure(banner_source(&line).as_deref() == Some(host_src.as_str()), || {
            format!("{name}: {line:?}")
        })?;
        tls.write_all(b"ping").map_err(|e| e.to_string())?;
        let mut b = [0u8; 4];
        tls.read_exact(&mut b).map_err(|e| e.to_string())?;
        ensure(&b == b"ping", || format!("{name}: echo"))?;
        detail.push(format!("{name}: PROXY v2 parsed by backend before its TLS handshake, mTLS verified, data flows"));
    }
    pass(detail.join("; "))
}

struct Probed {
    ok: bool,
    protocol: String,
    cipher: String,
    text: String,
}

fn probe_tls(edge: &EdgeUnderTest, port: u16, extra: &[&str]) -> Probed {
    let mut args = vec!["-servername", "mail.test", "-verify_hostname", "mail.test"];
    args.extend_from_slice(extra);
    let text = edge.s_client(port, &args);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    };
    let protocol = field("Protocol:");
    let cipher = field("Cipher    :");
    let new_line = text.lines().find(|l| l.starts_with("New,")).unwrap_or("");
    Probed {
        ok: new_line.contains("Cipher is") && !new_line.contains("(NONE)"),
        protocol,
        cipher,
        text,
    }
}

const LEGACY: [&str; 4] = ["-cipher", "ALL:@SECLEVEL=0", "-min_protocol", "SSLv3"];

fn legacy_probe_works(edge: &EdgeUnderTest) -> Result<(), String> {
    // Positive control: prove this openssl can negotiate TLS 1.1 at all, so that a refusal by
    // the edge is the edge's decision and not a client limitation.
    let cert = edge.generated.display().to_string();
    let mut server = Command::new("openssl")
        .args(["s_server", "-accept", "127.0.0.1:28443", "-www", "-quiet"])
        .args(["-cert", &format!("{cert}/mail-a.crt"), "-key", &format!("{cert}/mail-a.key")])
        .args(["-cipher", "ALL:@SECLEVEL=0", "-min_protocol", "TLSv1", "-max_protocol", "TLSv1.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    thread::sleep(Duration::from_millis(800));
    let out = run(Command::new("timeout")
        .args(["10", "openssl", "s_client", "-connect", "127.0.0.1:28443"])
        .args(["-cipher", "ALL:@SECLEVEL=0", "-min_protocol", "TLSv1", "-max_protocol", "TLSv1.1"])
        .stdin(Stdio::null()));
    let _ = server.kill();
    let _ = server.wait();
    let text = format!("{}{}", out.stdout, out.stderr);
    ensure(text.contains("TLSv1.1") || text.contains("TLSv1,") || text.contains("Protocol: TLSv1"), || {
        format!("legacy probe control failed (test would be vacuous): {}", text.lines().take(4).collect::<Vec<_>>().join(" | "))
    })
}

fn tls_versions_and_ciphers(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    legacy_probe_works(edge)?;
    let mut detail = Vec::new();
    for name in terminated() {
        let port = terminated_port(edge, name);
        for (label, max) in [("TLS1.0", "TLSv1"), ("TLS1.1", "TLSv1.1")] {
            let mut a: Vec<&str> = LEGACY.to_vec();
            a.extend(["-max_protocol", max]);
            let p = probe_tls(edge, port, &a);
            ensure(!p.ok, || format!("{name}: {label} accepted: {}", p.protocol))?;
        }
        let p12 = probe_tls(edge, port, &["-min_protocol", "TLSv1.2", "-max_protocol", "TLSv1.2"]);
        ensure(p12.ok, || format!("{name}: TLS1.2 refused: {}", p12.text.lines().take(3).collect::<Vec<_>>().join("|")))?;
        let p13 = probe_tls(edge, port, &["-min_protocol", "TLSv1.3"]);
        ensure(p13.ok, || format!("{name}: TLS1.3 refused"))?;
        // Ciphers outside the policy must fail even when the version is acceptable.
        for weak in ["ECDHE-ECDSA-AES128-SHA", "ECDHE-ECDSA-AES256-SHA384", "DHE-RSA-AES128-SHA"] {
            let p = probe_tls(edge, port, &["-min_protocol", "TLSv1.2", "-max_protocol", "TLSv1.2", "-cipher", &format!("{weak}:@SECLEVEL=0")]);
            ensure(!p.ok, || format!("{name}: weak cipher {weak} accepted"))?;
        }
        let p = probe_tls(edge, port, &["-min_protocol", "TLSv1.3", "-ciphersuites", "TLS_AES_128_CCM_SHA256"]);
        ensure(!p.ok, || format!("{name}: CCM suite accepted"))?;
        detail.push(format!("{name}: 1.0/1.1 refused, 1.2 ({}) and 1.3 ({}) ok, CBC/DHE/CCM refused", p12.cipher, p13.cipher));
    }
    // Pass-through ports: the TLS policy is the backend's (rustls: >= 1.2). STARTTLS on 25/587.
    for (name, port) in [("smtp", edge.ports.smtp), ("submission", edge.ports.submission)] {
        let base = ["-starttls", "smtp"];
        let mut old: Vec<&str> = base.to_vec();
        old.extend(LEGACY);
        old.extend(["-max_protocol", "TLSv1.1"]);
        let p = probe_tls(edge, port, &old);
        ensure(!p.ok, || format!("{name}: backend accepted TLS<=1.1 behind the edge"))?;
        let mut ok12: Vec<&str> = base.to_vec();
        ok12.extend(["-min_protocol", "TLSv1.2", "-max_protocol", "TLSv1.2"]);
        let p = probe_tls(edge, port, &ok12);
        ensure(p.ok, || format!("{name}: STARTTLS+TLS1.2 failed: {}", p.text.lines().take(3).collect::<Vec<_>>().join("|")))?;
        detail.push(format!("{name}(STARTTLS pass-through): <=1.1 refused by backend, 1.2 ok ({})", p.cipher));
    }
    detail.push("143/110 use the same fake STARTTLS code path (only 25/587 probed with openssl -starttls smtp)".to_owned());
    pass(detail.join("; "))
}

// ---------------------------------------------------------------------------------------
// Abuse controls
// ---------------------------------------------------------------------------------------

/// Open `n` connections; a plain port counts as accepted when the banner arrives, a TLS port
/// when a ServerHello arrives. Accepted connections are returned (held open).
fn open_many(edge: &EdgeUnderTest, port: u16, n: usize, tls: bool) -> (Vec<TcpStream>, usize) {
    let mut held = Vec::new();
    let mut rejected = 0;
    for _ in 0..n {
        let Ok(mut s) = edge.connect(port) else {
            rejected += 1;
            continue;
        };
        let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
        let accepted = if tls {
            let _ = s.write_all(&client_hello(b"mail.test"));
            let mut b = [0u8; 6];
            matches!(s.read(&mut b), Ok(6) if b[0] == 0x16 && b[5] == 2)
        } else {
            matches!(read_line(&mut s), Ok(l) if !l.is_empty())
        };
        if accepted {
            held.push(s);
        } else {
            rejected += 1;
        }
    }
    (held, rejected)
}

fn per_ip_connection_limit_caps_one_ip_not_another(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    settle();
    let probe = edge.start_probe()?;
    let mut detail = Vec::new();
    // (port, internal, tls, configured cap)
    for (name, port, internal, tls, cap) in [
        ("imap", edge.ports.imap, 10143, false, 50usize),
        ("pop3", edge.ports.pop3, 10110, false, 30),
        ("imaps", edge.ports.imaps, 10993, true, 50),
        ("smtps", edge.ports.smtps, 10465, true, 30),
    ] {
        let (held, rejected) = open_many(edge, port, cap + 20, tls);
        ensure(held.len() <= cap, || format!("{name}: {} connections accepted, cap is {cap}", held.len()))?;
        ensure(!held.is_empty() && rejected >= 15, || format!("{name}: accepted {} rejected {rejected}", held.len()))?;
        let mut other = String::new();
        if !tls {
            let b = probe.banner(internal);
            ensure(banner_source(&b).as_deref() == Some(probe.ip.as_str()), || {
                format!("{name}: second IP was affected by the first IP's flood: {b:?}")
            })?;
            other = ", second IP served normally".to_owned();
        }
        detail.push(format!("{name}: cap {cap}, accepted {}, rejected {rejected}{other}", held.len()));
        drop(held);
        thread::sleep(Duration::from_secs(2));
        // Slots must be released after the flood stops.
        let back = poll(Duration::from_secs(15), || {
            let (h, _) = open_many(edge, port, 1, tls);
            !h.is_empty()
        });
        ensure(back, || format!("{name}: no recovery after the flood"))?;
        settle();
    }
    pass(detail.join("; "))
}

fn per_ip_connection_rate_limit(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    settle();
    let probe = edge.start_probe()?;
    let mut accepted = 0;
    let mut rejected = 0;
    let start = Instant::now();
    for _ in 0..110 {
        match edge.connect(edge.ports.pop3) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
                if matches!(read_line(&mut s), Ok(l) if !l.is_empty()) {
                    accepted += 1;
                } else {
                    rejected += 1;
                }
            }
            Err(_) => rejected += 1,
        }
    }
    let took = start.elapsed();
    ensure(accepted <= 66, || format!("pop3 rate limit 60/10s: {accepted} accepted in {took:?}"))?;
    ensure(rejected >= 30, || format!("pop3: only {rejected} rejected"))?;
    let b = probe.banner(10110);
    ensure(banner_source(&b).as_deref() == Some(probe.ip.as_str()), || {
        format!("second IP affected by the rate limit: {b:?}")
    })?;
    let t = Instant::now();
    let recovered = poll(Duration::from_secs(30), || {
        let (h, _) = open_many(edge, edge.ports.pop3, 1, false);
        !h.is_empty()
    });
    ensure(recovered, || "pop3: limited IP never recovered".to_owned())?;
    pass(format!(
        "pop3 limit 60/10s: {accepted} accepted / {rejected} rejected of 110 in {took:.1?}; second IP unaffected; limited IP recovered after {:.0?}",
        t.elapsed()
    ))
}

fn many_idle_connections_bounded(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    settle();
    let mut all = Vec::new();
    let mut detail = Vec::new();
    let mut total = 0;
    for (name, port, tls, cap) in [
        ("imap", edge.ports.imap, false, 50usize),
        ("pop3", edge.ports.pop3, false, 30),
        ("imaps", edge.ports.imaps, true, 50),
        ("pop3s", edge.ports.pop3s, true, 30),
    ] {
        let (held, rejected) = open_many(edge, port, cap + 10, tls);
        ensure(held.len() <= cap, || format!("{name}: {} idle connections held, cap {cap}", held.len()))?;
        total += held.len();
        detail.push(format!("{name} {}/{}", held.len(), held.len() + rejected));
        all.push(held);
    }
    let probe = edge.start_probe()?;
    let b = probe.banner(10025);
    ensure(b.starts_with("220"), || format!("edge unusable for another client under idle load: {b:?}"))?;
    let id = edge.container(&edge.edge_service)?;
    let stats = run(Command::new("docker").args(["stats", "--no-stream", "--format", "cpu={{.CPUPerc}} mem={{.MemUsage}} pids={{.PIDs}}", &id])).stdout;
    drop(all);
    pass(format!(
        "{total} idle connections held (<= per-port caps: {}); other client served; edge under load: {}. Global maxconn (4096) NOT exercised: needs thousands of distinct source IPs",
        detail.join(", "),
        stats.trim()
    ))
}

fn tls_handshake_flood_bounded(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    settle();
    let ok = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = edge.tls_all();
    let started = Instant::now();
    let mut threads = Vec::new();
    for _ in 0..4 {
        let (ok, bad, stop, cfg) = (ok.clone(), bad.clone(), stop.clone(), cfg.clone());
        let addr = edge.addr(edge.ports.imaps);
        let name = "imap.test".to_owned();
        threads.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let done = (|| -> Result<(), ()> {
                    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|_| ())?;
                    tcp.set_read_timeout(Some(Duration::from_secs(2))).map_err(|_| ())?;
                    let sn = rustls::pki_types::ServerName::try_from(name.clone()).map_err(|_| ())?;
                    let mut conn = rustls::ClientConnection::new(cfg.clone(), sn).map_err(|_| ())?;
                    let mut tcp = tcp;
                    while conn.is_handshaking() {
                        conn.complete_io(&mut tcp).map_err(|_| ())?;
                    }
                    Ok(())
                })();
                if done.is_ok() {
                    ok.fetch_add(1, Ordering::Relaxed);
                } else {
                    bad.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    thread::sleep(Duration::from_secs(5));
    let id = edge.container(&edge.edge_service)?;
    let cpu = run(Command::new("docker").args(["stats", "--no-stream", "--format", "cpu={{.CPUPerc}} mem={{.MemUsage}}", &id])).stdout;
    thread::sleep(Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    let secs = started.elapsed().as_secs_f64();
    let (ok, bad) = (ok.load(Ordering::Relaxed), bad.load(Ordering::Relaxed));
    settle();
    ensure(ok <= 250, || format!("{ok} full handshakes in {secs:.1}s from one IP; the 100/10s limit did not bound it"))?;
    ensure(ok > 0, || "no handshake ever succeeded".to_owned())?;
    pass(format!(
        "one source IP, 4 client threads, {secs:.1}s on 993: {ok} full handshakes completed ({:.1}/s), {bad} refused/failed ({:.0}/s attempted); edge sample while flooding: {}",
        ok as f64 / secs,
        (ok + bad) as f64 / secs,
        cpu.trim()
    ))
}

fn slow_wait(mut s: TcpStream, limit: Duration) -> Duration {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let start = Instant::now();
    let mut b = [0u8; 256];
    loop {
        match s.read(&mut b) {
            Ok(0) => return start.elapsed(),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return start.elapsed(),
        }
        if start.elapsed() > limit {
            return start.elapsed();
        }
    }
}

fn slowloris_closed_within_timeout(edge: &EdgeUnderTest, rows: &mut Vec<Row>) -> Outcome {
    settle();
    let limit = edge.passthrough_idle + Duration::from_secs(40);
    let mut handles = Vec::new();
    // Pass-through: take the banner, then say nothing.
    for (name, port, _, _) in PASSTHROUGH {
        let mut s = edge.connect(port).map_err(|e| e.to_string())?;
        read_line(&mut s).map_err(|e| e.to_string())?;
        handles.push((name.to_owned(), format!("{port} pass-through idle"), edge.passthrough_idle, thread::spawn(move || slow_wait(s, limit))));
    }
    // Terminated: connect and send nothing / half a ClientHello.
    for (name, port) in [("smtps", edge.ports.smtps), ("imaps", edge.ports.imaps), ("pop3s", edge.ports.pop3s)] {
        let s = edge.connect(port).map_err(|e| e.to_string())?;
        handles.push((name.to_owned(), format!("{port} silent before handshake"), edge.terminated_idle, thread::spawn(move || slow_wait(s, limit))));
    }
    let mut half = edge.connect(edge.ports.imaps).map_err(|e| e.to_string())?;
    half.write_all(&client_hello(b"mail.test")[..40]).map_err(|e| e.to_string())?;
    handles.push(("imaps-half-hello".to_owned(), "993 half ClientHello".to_owned(), edge.terminated_idle, thread::spawn(move || slow_wait(half, limit))));
    // HTTP: partial request line / headers.
    let mut http = edge.connect(edge.ports.http).map_err(|e| e.to_string())?;
    http.write_all(b"GET / HTTP/1.1\r\nHost: web.test\r\n").map_err(|e| e.to_string())?;
    handles.push(("http".to_owned(), "80 partial request".to_owned(), Duration::from_secs(10), thread::spawn(move || slow_wait(http, limit))));
    let mut https = edge.tls_connect(edge.ports.https, "web.test")?;
    https.write_all(b"GET / HTTP/1.1\r\nHost: web.test\r\n").map_err(|e| e.to_string())?;
    handles.push(("https".to_owned(), "443 partial request".to_owned(), Duration::from_secs(10), thread::spawn(move || {
        let start = Instant::now();
        let mut b = [0u8; 512];
        let _ = https.sock.set_read_timeout(Some(Duration::from_secs(5)));
        loop {
            match https.read(&mut b) {
                Ok(0) => return start.elapsed(),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return start.elapsed(),
            }
            if start.elapsed() > limit {
                return start.elapsed();
            }
        }
    })));
    // Drip: one byte every 10 s of a ClientHello; informational (inactivity timeout resets).
    let mut drip = edge.connect(edge.ports.imaps).map_err(|e| e.to_string())?;
    let hello = client_hello(b"mail.test");
    let drip_thread = thread::spawn(move || {
        let start = Instant::now();
        for i in 0..12 {
            if drip.write_all(&hello[i..=i]).is_err() {
                return (start.elapsed(), true);
            }
            thread::sleep(Duration::from_secs(10));
        }
        (start.elapsed(), false)
    });
    let mut detail = Vec::new();
    let mut failures = Vec::new();
    for (name, what, configured, h) in handles {
        let took = h.join().map_err(|_| "slow client panicked".to_owned())?;
        let line = format!("{name} ({what}): closed after {:.0?} (configured bound {configured:?})", took);
        if took > configured + Duration::from_secs(8) {
            failures.push(line.clone());
        }
        detail.push(line);
    }
    let (drip_time, closed) = drip_thread.join().map_err(|_| "drip panicked".to_owned())?;
    rows.push(Row {
        name: "slowloris_drip_handshake_info".to_owned(),
        status: Status::Info,
        detail: format!(
            "1 byte of a ClientHello every 10 s on 993: {} after {drip_time:.0?}. HAProxy applies the 30 s timeout as INACTIVITY, so a dripping client is bounded only by the per-IP connection cap (30-50) and the service side",
            if closed { "connection closed" } else { "connection still open" }
        ),
        secs: drip_time.as_secs_f32(),
    });
    ensure(failures.is_empty(), || format!("not closed in time: {}", failures.join("; ")))?;
    pass(detail.join("; "))
}

fn health_check_removes_a_broken_backend(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let mut s = edge.connect(edge.ports.smtp).map_err(|e| e.to_string())?;
    ensure(read_line(&mut s).map_err(|e| e.to_string())?.starts_with("220"), || "control banner".to_owned())?;
    drop(s);
    edge.recreate_backend(
        "be-smtp",
        "services:\n  be-smtp:\n    command: [serve, silent, \"0.0.0.0:1025\"]\n",
    )?;
    let started = Instant::now();
    let removed = poll(Duration::from_secs(30), || {
        let Ok(mut s) = edge.connect(edge.ports.smtp) else { return true };
        let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
        // A silent-but-listening backend would hold this connection open without a banner; a
        // removed server makes the edge close it immediately.
        let t = Instant::now();
        let mut b = [0u8; 8];
        matches!(s.read(&mut b), Ok(0) | Err(_)) && t.elapsed() < Duration::from_millis(1200)
    });
    let took = started.elapsed();
    // Stay removed beyond the rise period: a TCP-only check would have re-admitted it by now.
    thread::sleep(Duration::from_secs(12));
    let still_removed = edge.connect(edge.ports.smtp).is_ok_and(|mut s| {
        let _ = s.set_read_timeout(Some(Duration::from_millis(1200)));
        let mut b = [0u8; 8];
        matches!(s.read(&mut b), Ok(0) | Err(_))
    });
    ensure(still_removed, || "mute backend was re-admitted by the health check".to_owned())?;
    let logs = edge.edge_logs();
    edge.restore_backend("be-smtp")?;
    let back = poll(Duration::from_secs(40), || {
        edge.connect(edge.ports.smtp)
            .ok()
            .and_then(|mut s| read_line(&mut s).ok())
            .is_some_and(|l| l.starts_with("220"))
    });
    ensure(removed, || "TCP-only-healthy but mute backend was not removed".to_owned())?;
    ensure(logs.contains("smtp_starttls/smtp is DOWN"), || "no DOWN log line".to_owned())?;
    ensure(back, || "backend not re-admitted after repair".to_owned())?;
    pass(format!("mute backend (accepts TCP, speaks nothing) removed after {took:.1?}; banner check re-admitted the repaired one"))
}

// ---------------------------------------------------------------------------------------
// Certificates
// ---------------------------------------------------------------------------------------

fn cert_replacement_does_not_drop_connections(edge: &EdgeUnderTest, rows: &mut Vec<Row>) -> Outcome {
    let live = edge.generated.join("mail-a.pem");
    let original = fs::read(&live).map_err(|e| e.to_string())?;
    let rotated = fs::read(edge.generated.join("mail-a-rotated.pem")).map_err(|e| e.to_string())?;
    let fp_old = edge.presented_cert(edge.ports.imaps, Some("imap.test"))?;
    ensure(fp_old == edge.file_fingerprint("mail-a.pem"), || "baseline cert mismatch".to_owned())?;
    let fp_new = edge.file_fingerprint("mail-a-rotated.pem");
    ensure(fp_old != fp_new, || "old and new certificate are identical".to_owned())?;

    let mut held = edge.tls_connect(edge.ports.imaps, "imap.test")?;
    read_line(&mut held).map_err(|e| e.to_string())?;
    let mut held_plain = edge.connect(edge.ports.imap).map_err(|e| e.to_string())?;
    read_line(&mut held_plain).map_err(|e| e.to_string())?;

    // Background prober: new plain connections during the reload window.
    let stop = Arc::new(AtomicBool::new(false));
    let fails = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let prober = {
        let (stop, fails, total) = (stop.clone(), fails.clone(), total.clone());
        let addr = edge.addr(edge.ports.smtp);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                total.fetch_add(1, Ordering::Relaxed);
                let ok = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
                    .ok()
                    .and_then(|mut s| {
                        s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
                        read_line(&mut s).ok()
                    })
                    .is_some_and(|l| l.starts_with("220"));
                if !ok {
                    fails.fetch_add(1, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };

    let swap = |bytes: &[u8]| -> Result<(), String> {
        let _ = fs::remove_file(&live);
        fs::write(&live, bytes).map_err(|e| e.to_string())
    };
    let result = (|| -> Result<String, String> {
        swap(&rotated)?;
        edge.reload_edge()?;
        let switched = poll(Duration::from_secs(15), || {
            edge.presented_cert(edge.ports.imaps, Some("imap.test")).is_ok_and(|f| f == fp_new)
        });
        ensure(switched, || "new certificate never became active after reload".to_owned())?;
        held.write_all(b"after-reload").map_err(|e| format!("held TLS connection: {e}"))?;
        let mut b = [0u8; 12];
        held.read_exact(&mut b).map_err(|e| format!("held TLS connection dropped by reload: {e}"))?;
        ensure(&b == b"after-reload", || "held TLS echo mismatch".to_owned())?;
        held_plain.write_all(b"ECHO 3\r\nabc").map_err(|e| e.to_string())?;
        let mut p = [0u8; 3];
        held_plain.read_exact(&mut p).map_err(|e| format!("held pass-through connection dropped by reload: {e}"))?;
        // Old certificate must be gone for every new connection, on every port.
        let mut stale = 0;
        for port in [edge.ports.imaps, edge.ports.smtps, edge.ports.pop3s, edge.ports.https] {
            for _ in 0..4 {
                if edge.presented_cert(port, Some("mail.test")).is_ok_and(|f| f == fp_old) {
                    stale += 1;
                }
            }
        }
        rows.push(Row {
            name: "old_cert_not_served_after_replacement".to_owned(),
            status: if stale == 0 { Status::Pass } else { Status::Fail },
            detail: format!("16 fresh handshakes on 465/993/995/443 after the reload: {stale} still presented the old certificate"),
            secs: 0.0,
        });
        // Tickets/session caches must not resurrect the old cert either (checked above by fresh handshakes).
        let workers = run(Command::new("docker").args(["top", &edge.container(&edge.edge_service)?])).stdout;
        let procs = workers.lines().filter(|l| l.contains("haproxy")).count();
        Ok(format!("{procs} haproxy processes visible after reload (old worker lingers while it still holds connections)"))
    })();
    // Always restore the original certificate and reload again; the held connections must survive
    // a second reload too.
    let restore = swap(&original).and_then(|()| edge.reload_edge());
    let back = poll(Duration::from_secs(15), || {
        edge.presented_cert(edge.ports.imaps, Some("imap.test")).is_ok_and(|f| f == fp_old)
    });
    stop.store(true, Ordering::Relaxed);
    let _ = prober.join();
    let still = held.write_all(b"second").is_ok() && {
        let mut b = [0u8; 6];
        held.read_exact(&mut b).is_ok()
    };
    let note = result?;
    restore?;
    ensure(back, || "original certificate not restored".to_owned())?;
    ensure(still, || "held connection dropped by the second reload".to_owned())?;
    rows.push(Row {
        name: "runtime_api_certificate_update".to_owned(),
        status: Status::Gap,
        detail: "not tested: the runtime API socket is deliberately not enabled (no admin socket anywhere); rotation is by reload (SIGUSR2 to the master)".to_owned(),
        secs: 0.0,
    });
    let f = fails.load(Ordering::Relaxed);
    let t = total.load(Ordering::Relaxed);
    pass(format!(
        "held TLS (993) and pass-through (143) connections survived two reloads; new certificate active, old one never served again; {f} of {t} new connections to 25 failed during the reload windows; {note}"
    ))
}

fn session_resumption_policy_as_configured(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let mut detail = Vec::new();
    let mut hard_fail = None;
    for (label, ver) in [("TLS1.2", "TLSv1.2"), ("TLS1.3", "TLSv1.3")] {
        let sess = format!("{}/session-{label}.pem", edge.generated.display());
        let cmd = |extra: &str| {
            format!(
                "sleep 2 | timeout 20 openssl s_client -connect {}:{} -servername imap.test -CAfile {} -min_protocol {ver} -max_protocol {ver} {extra} 2>&1",
                edge.host, edge.ports.imaps, edge.ca_path().display()
            )
        };
        let first = run(Command::new("sh").args(["-c", &cmd(&format!("-sess_out {sess}"))])).stdout;
        let ticket = first.contains("TLS session ticket:") || first.contains("New Session Ticket");
        let second = run(Command::new("sh").args(["-c", &cmd(&format!("-sess_in {sess}"))])).stdout;
        let reused = second.contains("Reused,");
        detail.push(format!("{label}: ticket issued={ticket}, resumption on reconnect={reused}"));
        // Policy: no stateless tickets (no long-lived ticket key to rotate). Resumption through
        // the bounded server-side session cache (tune.ssl.cachesize/lifetime) is allowed and
        // reported. TLS 1.3 always sends NewSessionTicket messages, which are stateful here.
        if label == "TLS1.2" && ticket {
            hard_fail = Some("TLS1.2 stateless ticket issued despite no-tls-tickets".to_owned());
        }
        let _ = fs::remove_file(&sess);
    }
    if let Some(f) = hard_fail {
        return Err(f);
    }
    Ok((Status::Pass, format!("policy: no stateless tickets, bounded stateful cache (20000 entries, 300 s); {}", detail.join("; "))))
}

// ---------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------

fn https_request(edge: &EdgeUnderTest, request: &[u8]) -> Result<String, String> {
    let mut tls = edge.tls_connect(edge.ports.https, "web.test")?;
    tls.write_all(request).map_err(|e| e.to_string())?;
    Ok(read_all(&mut tls))
}

fn status_line(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}

fn http_routes_by_host_unknown_host_404_no_backend_leak(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let real = host_source(edge)?;
    let ok = https_request(edge, b"GET / HTTP/1.1\r\nHost: web.test\r\nConnection: close\r\n\r\n")?;
    ensure(status_line(&ok).contains("200") && ok.contains(&format!("source={real}")), || format!("known host: {ok:?}"))?;
    let mut detail = vec![format!("web.test -> {}", status_line(&ok))];
    for (label, req) in [
        ("evil.test", "GET / HTTP/1.1\r\nHost: evil.test\r\nConnection: close\r\n\r\n"),
        ("no-host-http1.0", "GET / HTTP/1.0\r\n\r\n"),
        ("ip-host", "GET / HTTP/1.1\r\nHost: 10.0.0.1\r\nConnection: close\r\n\r\n"),
        ("web.test.evil.test", "GET / HTTP/1.1\r\nHost: web.test.evil.test\r\nConnection: close\r\n\r\n"),
    ] {
        let r = https_request(edge, req.as_bytes())?;
        let lower = r.to_ascii_lowercase();
        ensure(status_line(&r).contains("421"), || format!("{label}: {:?}", status_line(&r)))?;
        ensure(!lower.contains("source=") && !lower.contains("be-http") && !lower.contains("fake") && !lower.contains("rustls"), || {
            format!("{label}: backend detail leaked: {r:?}")
        })?;
        detail.push(format!("{label} -> {}", status_line(&r)));
    }
    // Plain HTTP port: only redirects for the known host; never reflects an arbitrary Host.
    let mut s = edge.connect(edge.ports.http).map_err(|e| e.to_string())?;
    s.write_all(b"GET /x?y=1 HTTP/1.1\r\nHost: web.test\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let r = read_all(&mut s);
    ensure(status_line(&r).contains("308") && r.contains("ocation: https://web.test/x?y=1"), || format!("redirect: {r:?}"))?;
    let mut s = edge.connect(edge.ports.http).map_err(|e| e.to_string())?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: evil.test\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let r = read_all(&mut s);
    ensure(status_line(&r).contains("421") && !r.contains("evil.test/"), || format!("port 80 reflected an unknown Host: {r:?}"))?;
    detail.push("80: known host -> 308 https, unknown host -> 421 (no reflected redirect)".to_owned());
    pass(detail.join("; "))
}

fn http_tls_reencrypted_with_mtls(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let get = || https_request(edge, b"GET / HTTP/1.1\r\nHost: web.test\r\nConnection: close\r\n\r\n");
    let r = get()?;
    ensure(status_line(&r).contains("200"), || format!("control: {r:?}"))?;
    // be-http only answers after a verified client certificate AND a PROXY v2 header, so 200 proves both.
    edge.recreate_backend("be-http", &override_cert("be-http", "backend-wrongca", None))?;
    let mut refused = None;
    let ok = poll(Duration::from_secs(30), || {
        let r = get().unwrap_or_default();
        let bad = r.contains("503") && !r.contains("source=");
        if bad {
            refused = Some(r);
        }
        bad
    });
    edge.restore_backend("be-http")?;
    let back = poll(Duration::from_secs(40), || get().is_ok_and(|r| status_line(&r).contains("200")));
    ensure(ok, || "backend with a certificate from the wrong CA still served traffic".to_owned())?;
    ensure(back, || "good backend did not come back".to_owned())?;
    let r = refused.unwrap_or_default();
    ensure(!r.to_ascii_lowercase().contains("be-http") && !r.contains("rustls"), || format!("503 leaks detail: {r:?}"))?;
    pass("200 only with verified mTLS + PROXY v2 to the backend; wrong-CA backend -> 503 without detail; recovered after repair")
}

fn http_oversized_header_and_body_rejected(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let mut detail = Vec::new();
    let big = "a".repeat(64 * 1024);
    let r = https_request(edge, format!("GET / HTTP/1.1\r\nHost: web.test\r\nX-Big: {big}\r\n\r\n").as_bytes()).unwrap_or_default();
    ensure(!status_line(&r).contains("200") && !r.contains("source="), || format!("64 KiB header: {:?}", status_line(&r)))?;
    detail.push(format!("64 KiB header -> {:?}", status_line(&r)));
    let mut many = String::from("GET / HTTP/1.1\r\nHost: web.test\r\n");
    for i in 0..300 {
        many.push_str(&format!("X-{i}: v\r\n"));
    }
    many.push_str("\r\n");
    let r = https_request(edge, many.as_bytes()).unwrap_or_default();
    ensure(!status_line(&r).contains("200") && !r.contains("source="), || format!("300 headers: {:?}", status_line(&r)))?;
    detail.push(format!("300 headers -> {:?}", status_line(&r)));
    let r = https_request(edge, b"POST /u HTTP/1.1\r\nHost: web.test\r\nContent-Length: 100000000\r\nConnection: close\r\n\r\nx").unwrap_or_default();
    ensure(status_line(&r).contains("413"), || format!("100 MB Content-Length: {:?}", status_line(&r)))?;
    detail.push(format!("Content-Length 100 MB -> {:?}", status_line(&r)));
    // Not enforced at the edge: chunked bodies. Observe and report, do not hide.
    let mut tls = edge.tls_connect(edge.ports.https, "web.test")?;
    let head = b"POST /u HTTP/1.1\r\nHost: web.test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    let _ = tls.write_all(head);
    let chunk = format!("{:x}\r\n{}\r\n", 65536, "b".repeat(65536));
    let mut sent = 0usize;
    for _ in 0..80 {
        if tls.write_all(chunk.as_bytes()).is_err() {
            break;
        }
        sent += 65536;
    }
    detail.push(format!("chunked upload: {} MiB sent before the connection ended (no size limit configured for chunked bodies; the service must enforce its own)", sent / (1 << 20)));
    pass(detail.join("; "))
}

// ---------------------------------------------------------------------------------------
// Hardening
// ---------------------------------------------------------------------------------------

fn inspect(id: &str, fmt: &str) -> String {
    run(Command::new("docker").args(["inspect", "-f", fmt, id])).stdout.trim().to_owned()
}

fn only_edge_publishes_ports(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let edge_id = edge.container(&edge.edge_service)?;
    let ids = edge.project_containers();
    ensure(ids.len() >= 6, || format!("only {} containers", ids.len()))?;
    for id in &ids {
        let bindings = inspect(id, "{{json .HostConfig.PortBindings}}");
        if *id == edge_id {
            continue;
        }
        ensure(bindings == "null" || bindings == "{}", || format!("{id} publishes ports: {bindings}"))?;
        let published = inspect(id, "{{json .NetworkSettings.Ports}}");
        ensure(!published.contains("HostPort\":\"") || published.contains("null"), || format!("{id}: {published}"))?;
    }
    let ports = run(Command::new("docker").args(["port", &edge_id])).stdout;
    let host_ports: BTreeSet<u16> = ports
        .lines()
        .filter_map(|l| l.rsplit(':').next()?.trim().parse().ok())
        .collect();
    let want: BTreeSet<u16> = [25, 80, 110, 143, 443, 465, 587, 993, 995].into();
    ensure(host_ports == want, || format!("edge publishes {host_ports:?}, expected {want:?}"))?;
    pass(format!("{} containers; only the edge publishes, exactly {want:?}", ids.len()))
}

fn edge_runs_read_only_nonroot_without_caps(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let fmt = "{{.HostConfig.ReadonlyRootfs}}|{{.Config.User}}|{{.HostConfig.CapDrop}}|{{.HostConfig.CapAdd}}|{{.HostConfig.SecurityOpt}}|{{.HostConfig.Privileged}}|{{.HostConfig.NetworkMode}}|{{.HostConfig.PidMode}}|{{.HostConfig.Memory}}|{{.HostConfig.PidsLimit}}";
    for id in edge.project_containers() {
        let v = inspect(&id, fmt);
        let f: Vec<&str> = v.split('|').collect();
        ensure(f.len() == 10, || format!("{id}: {v}"))?;
        ensure(f[0] == "true", || format!("{id}: rootfs writable"))?;
        ensure(f[1] == "1000:1000", || format!("{id}: user {}", f[1]))?;
        ensure(f[2].contains("ALL") && (f[3] == "[]" || f[3] == "<no value>" || f[3].is_empty()), || format!("{id}: caps drop={} add={}", f[2], f[3]))?;
        ensure(f[4].contains("no-new-privileges"), || format!("{id}: security-opt {}", f[4]))?;
        ensure(f[5] == "false" && f[6] != "host" && f[7].is_empty(), || format!("{id}: privileged/host/pid: {v}"))?;
        ensure(f[8] != "0" && f[9] != "<nil>" && f[9] != "0", || format!("{id}: limits {v}"))?;
    }
    let id = edge.container(&edge.edge_service)?;
    let out = run(Command::new("docker").args([
        "exec", &id, "sh", "-c",
        "id -u; grep -E '^Cap(Eff|Prm|Inh|Bnd|Amb)' /proc/1/status; grep NoNewPrivs /proc/1/status; touch /x 2>&1; touch /usr/local/etc/haproxy/x 2>&1; touch /run/rusty-certs/x 2>&1; true",
    ]));
    let t = out.stdout;
    ensure(t.lines().next() == Some("1000"), || format!("uid: {t}"))?;
    ensure(t.lines().filter(|l| l.starts_with("Cap")).all(|l| l.ends_with("0000000000000000")), || format!("capabilities: {t}"))?;
    ensure(t.contains("NoNewPrivs:\t1") || t.contains("NoNewPrivs: 1"), || format!("no-new-privs: {t}"))?;
    ensure(t.matches("Read-only file system").count() == 3, || format!("filesystem writable: {t}"))?;
    pass("every container: read-only rootfs, uid 1000, cap_drop ALL, no cap_add, no-new-privileges, unprivileged, not host net/pid, memory+pids limits; edge /proc: CapEff/Prm/Inh/Bnd/Amb all zero, NoNewPrivs 1, writes refused")
}

fn no_runtime_socket_mounted_anywhere(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    for id in edge.project_containers() {
        let m = inspect(&id, "{{range .Mounts}}{{.Source}}={{.Destination}};{{end}}");
        let low = m.to_ascii_lowercase();
        ensure(!low.contains("sock") && !low.contains("containerd") && !low.contains("/var/run/docker") && !low.contains("podman"), || format!("{id}: mounts {m}"))?;
    }
    let id = edge.container(&edge.edge_service)?;
    let socks = run(Command::new("docker").args(["exec", &id, "sh", "-c", "find / -xdev -type s 2>/dev/null; find /proc/net/unix -type f 2>/dev/null >/dev/null; grep -c '/' /proc/net/unix"])).stdout;
    ensure(socks.lines().take_while(|l| l.contains('/')).count() == 0, || format!("unix sockets in edge: {socks}"))?;
    let unix_paths = run(Command::new("docker").args(["exec", &id, "sh", "-c", "awk 'NR>1 && $8 != \"\" {print $8}' /proc/net/unix"])).stdout;
    ensure(unix_paths.trim().is_empty(), || format!("edge has unix sockets bound: {unix_paths}"))?;
    pass("no mount source/destination mentions a runtime socket in any container; the edge has no filesystem or bound unix socket (no stats, admin or master socket)")
}

fn stats_and_admin_not_reachable_from_outside(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let id = edge.container(&edge.edge_service)?;
    let tcp = run(Command::new("docker").args(["exec", &id, "sh", "-c", "cat /proc/net/tcp /proc/net/tcp6"])).stdout;
    let listening: BTreeSet<u16> = tcp
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // 0A = LISTEN; skip Docker's embedded DNS resolver on 127.0.0.11 (ephemeral port).
            if f.get(3) == Some(&"0A") && !f.get(1)?.starts_with("0B00007F:") {
                u16::from_str_radix(f.get(1)?.rsplit(':').next()?, 16).ok()
            } else {
                None
            }
        })
        .collect();
    let want: BTreeSet<u16> = [10025, 10080, 10110, 10143, 10443, 10465, 10587, 10993, 10995].into();
    ensure(listening == want, || format!("edge listens on {listening:?}, expected exactly {want:?}"))?;
    let mut closed = Vec::new();
    for p in [8404u16, 8080, 8000, 8888, 9000, 9090, 9999, 1936, 7000, 22, 2375, 5555] {
        if TcpStream::connect_timeout(&edge.addr(p), Duration::from_millis(700)).is_ok() {
            return Err(format!("port {p} answers on the host"));
        }
        closed.push(p);
    }
    let r = https_request(edge, b"GET /stats HTTP/1.1\r\nHost: evil.test\r\nConnection: close\r\n\r\n")?;
    ensure(status_line(&r).contains("421") && !r.to_ascii_lowercase().contains("haproxy"), || format!("/stats: {r:?}"))?;
    let r = https_request(edge, b"GET /haproxy?stats HTTP/1.1\r\nHost: web.test\r\nConnection: close\r\n\r\n")?;
    ensure(!r.to_ascii_lowercase().contains("statistics report"), || "HAProxy stats page served".to_owned())?;
    pass(format!("edge listens only on its nine service ports (from /proc/net/tcp); common admin/stats ports {closed:?} closed on the host; /stats and /haproxy?stats serve no stats page"))
}

fn no_key_material_in_logs_inspect_or_env(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let mut blob = String::new();
    for id in edge.project_containers() {
        blob.push_str(&run(Command::new("docker").args(["inspect", &id])).stdout);
        let l = run(Command::new("docker").args(["logs", &id]));
        blob.push_str(&l.stdout);
        blob.push_str(&l.stderr);
    }
    let edge_id = edge.container(&edge.edge_service)?;
    blob.push_str(&run(Command::new("docker").args(["exec", &edge_id, "sh", "-c", "env; tr '\\0' '\\n' < /proc/1/environ; cat /proc/1/cmdline"])).stdout);
    for img in [inspect(&edge_id, "{{.Image}}"), "rusty-fake-backend:h1".to_owned()] {
        blob.push_str(&run(Command::new("docker").args(["image", "inspect", &img])).stdout);
        blob.push_str(&run(Command::new("docker").args(["history", "--no-trunc", &img])).stdout);
    }
    ensure(!blob.contains("PRIVATE KEY"), || "PEM private key marker found in logs/inspect/env/image metadata".to_owned())?;
    let mut checked = 0;
    for entry in fs::read_dir(&edge.generated).map_err(|e| e.to_string())? {
        let p = entry.map_err(|e| e.to_string())?.path();
        if p.extension().is_some_and(|e| e == "key") {
            for line in fs::read_to_string(&p).map_err(|e| e.to_string())?.lines().filter(|l| !l.starts_with("-----") && l.len() > 20) {
                ensure(!blob.contains(line), || format!("key body of {} appears in logs/inspect/env", p.display()))?;
                checked += 1;
            }
        }
    }
    // The fake backend image must not carry keys either (scratch image: list its files).
    let fake = run(Command::new("sh").args(["-c", "c=$(docker create rusty-fake-backend:h1) && docker export $c | tar -t; docker rm $c >/dev/null"])).stdout;
    let files: Vec<&str> = fake.lines().filter(|l| !l.ends_with('/')).filter(|l| !matches!(*l, ".dockerenv" | "etc/hosts" | "etc/hostname" | "etc/resolv.conf" | "etc/mtab") && !l.starts_with("dev/") && !l.starts_with("proc/") && !l.starts_with("sys/")).collect();
    ensure(files == ["fake-backend"], || format!("fake backend image contains {files:?}"))?;
    let imgfind = run(Command::new("docker").args(["run", "--rm", "--network", "none", "--entrypoint", "sh", &inspect(&edge_id, "{{.Image}}"), "-c", "grep -rlE -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' /etc /usr /opt /home /root /var 2>/dev/null | head"])).stdout;
    ensure(imgfind.trim().is_empty(), || format!("keys inside the HAProxy image: {imgfind}"))?;
    pass(format!("no PEM private-key marker and none of {checked} key-body lines in docker inspect, logs, env, cmdline or image metadata; fake image holds only /fake-backend; HAProxy image has no key files"))
}

fn keys_only_on_readonly_tmpfs(edge: &EdgeUnderTest, _: &mut Vec<Row>) -> Outcome {
    let id = edge.container(&edge.edge_service)?;
    let m = inspect(&id, "{{range .Mounts}}{{.Type}}:{{.Destination}}:{{.RW}};{{end}}");
    let certs = m.split(';').find(|x| x.contains("/run/rusty-certs")).unwrap_or("");
    if certs.starts_with("tmpfs") && certs.ends_with("false") {
        return pass("keys on a read-only tmpfs");
    }
    Ok((
        Status::Gap,
        format!("keys reach the edge via a read-only BIND mount of the git-ignored test directory ({certs}), i.e. they exist on the host disk during tests. Delivery into a read-only tmpfs (certd/secret injection) is not part of H1/H2 and is not satisfied here"),
    ))
}

// ---------------------------------------------------------------------------------------

fn cases() -> Vec<(&'static str, CaseFn)> {
    vec![
        ("only_edge_publishes_ports", only_edge_publishes_ports),
        ("edge_runs_read_only_nonroot_without_caps", edge_runs_read_only_nonroot_without_caps),
        ("no_runtime_socket_mounted_anywhere", no_runtime_socket_mounted_anywhere),
        ("stats_and_admin_not_reachable_from_outside", stats_and_admin_not_reachable_from_outside),
        ("tcp_passthrough_preserves_bytes", tcp_passthrough_preserves_bytes),
        ("starttls_upgrade_survives_the_edge", starttls_upgrade_survives_the_edge),
        ("proxy_v2_reports_real_client_ip", proxy_v2_reports_real_client_ip),
        ("client_supplied_proxy_header_cannot_spoof_source_ip", client_supplied_proxy_header_cannot_spoof_source_ip),
        ("terminated_ports_present_the_right_cert_by_sni", terminated_ports_present_the_right_cert_by_sni),
        ("sni_never_reaches_a_file_path", sni_never_reaches_a_file_path),
        ("proxy_v2_and_tls_to_backend_work_together", proxy_v2_and_tls_to_backend_work_together),
        ("tls_versions_and_ciphers", tls_versions_and_ciphers),
        ("session_resumption_policy_as_configured", session_resumption_policy_as_configured),
        ("http_routes_by_host_unknown_host_404_no_backend_leak", http_routes_by_host_unknown_host_404_no_backend_leak),
        ("http_oversized_header_and_body_rejected", http_oversized_header_and_body_rejected),
        ("backend_link_is_verified_mtls", backend_link_is_verified_mtls),
        ("http_tls_reencrypted_with_mtls", http_tls_reencrypted_with_mtls),
        ("health_check_removes_a_broken_backend", health_check_removes_a_broken_backend),
        ("cert_replacement_does_not_drop_connections", cert_replacement_does_not_drop_connections),
        ("per_ip_connection_limit_caps_one_ip_not_another", per_ip_connection_limit_caps_one_ip_not_another),
        ("per_ip_connection_rate_limit", per_ip_connection_rate_limit),
        ("many_idle_connections_bounded", many_idle_connections_bounded),
        ("tls_handshake_flood_bounded", tls_handshake_flood_bounded),
        ("slowloris_closed_within_timeout", slowloris_closed_within_timeout),
        ("no_key_material_in_logs_inspect_or_env", no_key_material_in_logs_inspect_or_env),
        ("keys_only_on_readonly_tmpfs", keys_only_on_readonly_tmpfs),
    ]
}

struct Teardown<'a>(&'a EdgeUnderTest);

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        if std::env::var_os("EDGE_KEEP").is_none() {
            self.0.down();
        }
    }
}

#[test]
#[ignore = "needs Docker; run with --ignored"]
fn edge_suite() {
    let edge = EdgeUnderTest::haproxy();
    let _guard = Teardown(&edge);
    // Start from a clean slate; a previous aborted run must not leak into this one.
    edge.down();
    edge.up().expect("stack must come up");
    let only: Vec<String> = std::env::var("EDGE_ONLY")
        .map(|v| v.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let mut rows: Vec<Row> = Vec::new();
    for (name, f) in cases() {
        if !only.is_empty() && !only.iter().any(|o| name.contains(o.as_str())) {
            continue;
        }
        let t = Instant::now();
        let mut extra = Vec::new();
        let outcome = catch_unwind(AssertUnwindSafe(|| f(&edge, &mut extra)));
        let (status, detail) = match outcome {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => (Status::Fail, e),
            Err(_) => (Status::Fail, "panicked".to_owned()),
        };
        // A case that disturbed the stack must not poison the next one.
        if !edge.wait_edge_healthy(Duration::from_secs(60)) {
            rows.push(Row {
                name: format!("{name}:stack_health_after"),
                status: Status::Fail,
                detail: "edge unhealthy after the case".to_owned(),
                secs: 0.0,
            });
        }
        println!("[{}] {name} ({:.1?}) {detail}", status.label(), t.elapsed());
        rows.push(Row { name: name.to_owned(), status, detail, secs: t.elapsed().as_secs_f32() });
        for r in extra {
            println!("[{}] {} {}", r.status.label(), r.name, r.detail);
            rows.push(r);
        }
    }
    write_report(&edge.report_path, &edge.name, &rows);
    println!("---- report: {} ----", edge.report_path.display());
    let failed: Vec<&str> = rows.iter().filter(|r| r.status == Status::Fail).map(|r| r.name.as_str()).collect();
    assert!(failed.is_empty(), "failing edge behaviours: {failed:?}");
}
