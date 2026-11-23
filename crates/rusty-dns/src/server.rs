//! The listeners (docs/design-authoritative-dns.md, section 2): UDP and TCP on the addresses given, authoritative only. Every question goes through the same steps: parse, rate limit (UDP), the answer engine, encode,
//! cut to what the client can take (UDP). The zones are a snapshot that is swapped as a whole when the data changes; a question reads the current one and never waits for the database.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::adapter::{encode, encode_error, parse, Parsed, Question};
use crate::answer::{answer, Answer, Query, Rcode};
use crate::limit::{Limits, RateLimiter, Verdict};
use crate::zone::{RType, Zones};

/// The zones the server answers from: swap in a new snapshot with [`ZoneHandle::set`].
#[derive(Clone, Default)]
pub struct ZoneHandle {
    current: Arc<RwLock<Arc<Zones>>>,
}

impl ZoneHandle {
    pub fn new(zones: Zones) -> ZoneHandle {
        ZoneHandle { current: Arc::new(RwLock::new(Arc::new(zones))) }
    }

    pub fn set(&self, zones: Zones) {
        if let Ok(mut w) = self.current.write() {
            *w = Arc::new(zones);
        }
    }

    pub fn get(&self) -> Arc<Zones> {
        self.current.read().map(|r| r.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub limits: Limits,
    /// TCP connections at once, and from one address.
    pub tcp_max: usize,
    pub tcp_per_ip: usize,
    /// A TCP connection is closed after this long without a whole question, and after this many questions.
    pub tcp_idle: Duration,
    pub tcp_queries: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config { limits: Limits::default(), tcp_max: 1000, tcp_per_ip: 20, tcp_idle: Duration::from_secs(5), tcp_queries: 50 }
    }
}

fn respond(zones: &Zones, q: &Question) -> Answer {
    // `ANY` and every other type go through the engine; a question about a name that is not ours is refused there.
    answer(zones, &Query { name: q.name.clone(), qtype: RType::from_code(q.qtype) })
}

/// The reply to one UDP packet, if any.
pub fn handle_udp(zones: &Zones, limiter: &RateLimiter, peer: IpAddr, packet: &[u8]) -> Option<Vec<u8>> {
    match parse(packet) {
        Parsed::Drop => None,
        Parsed::Error(id, e) => match limiter.check(peer) {
            Verdict::Answer => Some(encode_error(id, e)),
            _ => None,
        },
        Parsed::Query(q) => match limiter.check(peer) {
            Verdict::Answer => Some(encode(&q, &respond(zones, &q), Some(usize::from(q.udp_payload)))),
            Verdict::Drop => None,
            // Too small to hurt anyone whose address was forged: the question, no records, TC set.
            Verdict::Slip => {
                let empty = Answer { rcode: Rcode::NoError, authoritative: true, answers: Vec::new(), authority: Vec::new(), additional: Vec::new() };
                Some(encode(&q, &empty, Some(0)))
            }
        },
    }
}

/// Answer UDP questions on `socket` until `stop`.
pub async fn serve_udp(socket: UdpSocket, zones: ZoneHandle, limits: Limits, stop: CancellationToken) {
    let limiter = Arc::new(RateLimiter::new(limits));
    {
        let limiter = limiter.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_secs(60)) => limiter.sweep(Duration::from_secs(300)),
                }
            }
        });
    }
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            got = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = got else { continue };
                let Some(packet) = buf.get(..n) else { continue };
                if let Some(reply) = handle_udp(&zones.get(), &limiter, from.ip(), packet) {
                    let _ = socket.send_to(&reply, from).await;
                }
            }
        }
    }
}

/// One TCP connection: questions of two-byte length and a packet, answered the same way, until it idles, asks too much or closes.
async fn tcp_connection(mut s: TcpStream, zones: ZoneHandle, cfg: Config) {
    for _ in 0..cfg.tcp_queries {
        let mut len = [0u8; 2];
        match tokio::time::timeout(cfg.tcp_idle, s.read_exact(&mut len)).await {
            Ok(Ok(_)) => {}
            _ => return,
        }
        let n = usize::from(u16::from_be_bytes(len));
        if n < 12 {
            return;
        }
        let mut packet = vec![0u8; n];
        if !matches!(tokio::time::timeout(cfg.tcp_idle, s.read_exact(&mut packet)).await, Ok(Ok(_))) {
            return;
        }
        let reply = match parse(&packet) {
            Parsed::Drop => return,
            Parsed::Error(id, e) => encode_error(id, e),
            Parsed::Query(q) => encode(&q, &respond(&zones.get(), &q), None),
        };
        let Ok(rl) = u16::try_from(reply.len()) else { return };
        let mut out = Vec::with_capacity(reply.len() + 2);
        out.extend_from_slice(&rl.to_be_bytes());
        out.extend_from_slice(&reply);
        if s.write_all(&out).await.is_err() {
            return;
        }
    }
}

/// Accept TCP connections on `listener` until `stop`, with the caps of `cfg`.
pub async fn serve_tcp(listener: TcpListener, zones: ZoneHandle, cfg: Config, stop: CancellationToken) {
    let all = Arc::new(Semaphore::new(cfg.tcp_max));
    let per_ip: Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>> = Arc::default();
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            got = listener.accept() => {
                let Ok((stream, from)) = got else { continue };
                let Ok(permit) = all.clone().try_acquire_owned() else { continue };
                {
                    let Ok(mut m) = per_ip.lock() else { continue };
                    let n = m.entry(from.ip()).or_insert(0);
                    if *n >= cfg.tcp_per_ip {
                        continue;
                    }
                    *n += 1;
                }
                let (zones, cfg, per_ip) = (zones.clone(), cfg.clone(), per_ip.clone());
                let ip = from.ip();
                tokio::spawn(async move {
                    tcp_connection(stream, zones, cfg).await;
                    if let Ok(mut m) = per_ip.lock() {
                        if let Some(n) = m.get_mut(&ip) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                m.remove(&ip);
                            }
                        }
                    }
                    drop(permit);
                });
            }
        }
    }
}

/// Bind both transports on one address and serve them; returns the bound addresses (port 0 asks for any) and the tasks.
pub async fn start(addr: SocketAddr, zones: ZoneHandle, cfg: Config, stop: CancellationToken) -> std::io::Result<(SocketAddr, SocketAddr, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> {
    let udp = UdpSocket::bind(addr).await?;
    let tcp = TcpListener::bind(SocketAddr::new(addr.ip(), if addr.port() == 0 { 0 } else { addr.port() })).await?;
    let (ua, ta) = (udp.local_addr()?, tcp.local_addr()?);
    let u = tokio::spawn(serve_udp(udp, zones.clone(), cfg.limits, stop.clone()));
    let t = tokio::spawn(serve_tcp(tcp, zones, cfg, stop));
    Ok((ua, ta, u, t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::MAX_UDP_PAYLOAD;
    use crate::zone::{Rdata, Soa, Zone};
    use crate::adapter::testkit::{question as q_bytes, reply, Reply, FORMERR, REFUSED};
    use std::net::Ipv4Addr;

    fn zones() -> Zones {
        let soa = Soa { mname: "ns1.example.org".into(), rname: "hostmaster.example.org".into(), serial: 1, refresh: 3600, retry: 600, expire: 604_800, minimum: 300 };
        let mut z = Zone::new("example.org", soa, 3600);
        z.add("mail.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 25)));
        for i in 0..40 {
            z.add("big.example.org", 300, Rdata::Txt(vec![format!("{i:0>200}")]));
        }
        let mut zs = Zones::new();
        zs.insert(z);
        zs
    }

    fn question(id: u16, name: &str, t: u16) -> Vec<u8> {
        q_bytes(id, name, t)
    }

    async fn udp_ask(addr: SocketAddr, packet: &[u8]) -> Option<Reply> {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(packet, addr).await.unwrap();
        let mut buf = vec![0u8; 4096];
        match tokio::time::timeout(Duration::from_millis(500), s.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(reply(&buf[..n])),
            _ => None,
        }
    }

    async fn tcp_ask(addr: SocketAddr, packet: &[u8]) -> Reply {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut out = (packet.len() as u16).to_be_bytes().to_vec();
        out.extend_from_slice(packet);
        s.write_all(&out).await.unwrap();
        let mut len = [0u8; 2];
        s.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; usize::from(u16::from_be_bytes(len))];
        s.read_exact(&mut body).await.unwrap();
        reply(&body)
    }

    #[tokio::test]
    async fn udp_and_tcp_answer_the_same_question_and_a_big_answer_is_cut_over_udp_only() {
        let stop = CancellationToken::new();
        let handle = ZoneHandle::new(zones());
        let (ua, ta, u, t) = start("127.0.0.1:0".parse().unwrap(), handle.clone(), Config::default(), stop.clone()).await.unwrap();
        let m = udp_ask(ua, &question(11, "mail.example.org.", 1)).await.expect("an answer");
        assert_eq!((m.id, m.is_response, m.answers), (11, true, 1));
        let m = tcp_ask(ta, &question(12, "mail.example.org.", 1)).await;
        assert_eq!((m.id, m.answers, m.authoritative), (12, 1, true));
        let cut = udp_ask(ua, &question(13, "big.example.org.", 16)).await.unwrap();
        assert!(cut.truncated && cut.answers == 0);
        assert!(cut.size <= usize::from(MAX_UDP_PAYLOAD));
        let whole = tcp_ask(ta, &question(14, "big.example.org.", 16)).await;
        assert_eq!((whole.truncated, whole.answers), (false, 40));
        // The zones are swapped as a whole: the next question sees the new data.
        let mut next = zones();
        let mut z = next.find("example.org").unwrap().clone();
        z.add("new.example.org", 60, Rdata::A(Ipv4Addr::new(192, 0, 2, 99)));
        next.insert(z);
        handle.set(next);
        let m = udp_ask(ua, &question(15, "new.example.org.", 1)).await.unwrap();
        assert_eq!(m.answers, 1);
        // Never recursive: a name that is not ours is refused.
        let m = udp_ask(ua, &question(16, "example.net.", 1)).await.unwrap();
        assert_eq!(m.rcode, REFUSED);
        stop.cancel();
        let _ = tokio::join!(u, t);
    }

    #[tokio::test]
    async fn a_flood_from_one_prefix_is_dropped_and_slipped_and_tcp_is_not_limited_that_way() {
        let stop = CancellationToken::new();
        let cfg = Config { limits: Limits { per_second: 0.001, burst: 3.0, slip: 2, max_prefixes: 100 }, ..Config::default() };
        let (ua, ta, u, t) = start("127.0.0.1:0".parse().unwrap(), ZoneHandle::new(zones()), cfg, stop.clone()).await.unwrap();
        let mut answered = 0;
        let mut slipped = 0;
        let mut silent = 0;
        for i in 0..12 {
            match udp_ask(ua, &question(100 + i, "mail.example.org.", 1)).await {
                Some(m) if m.truncated => slipped += 1,
                Some(_) => answered += 1,
                None => silent += 1,
            }
        }
        assert_eq!(answered, 3, "the burst");
        assert!(slipped >= 3 && silent >= 3, "{slipped} slipped, {silent} silent");
        let m = tcp_ask(ta, &question(7, "mail.example.org.", 1)).await;
        assert_eq!(m.answers, 1, "the client that was limited over UDP is served over TCP");
        stop.cancel();
        let _ = tokio::join!(u, t);
    }

    #[tokio::test]
    async fn a_connection_is_closed_when_it_idles_and_what_is_not_a_question_gets_no_answer_or_an_error() {
        let stop = CancellationToken::new();
        let cfg = Config { tcp_idle: Duration::from_millis(150), ..Config::default() };
        let (ua, ta, u, t) = start("127.0.0.1:0".parse().unwrap(), ZoneHandle::new(zones()), cfg, stop.clone()).await.unwrap();
        let mut s = TcpStream::connect(ta).await.unwrap();
        let mut b = [0u8; 1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), s.read(&mut b)).await.unwrap().unwrap(), 0, "closed by the server after the idle time");
        assert!(udp_ask(ua, &[1]).await.is_none(), "a packet too short for an id: no answer");
        let m = udp_ask(ua, &[0xab, 0xcd, 0x01, 0x00, 0xff, 0xff, 0, 0, 0, 0, 0, 0]).await.unwrap();
        assert_eq!((m.id, m.rcode), (0xabcd, FORMERR));
        stop.cancel();
        let _ = tokio::join!(u, t);
    }
}
