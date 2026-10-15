//! **Finding Jellyfin on the LAN without typing an address.** Every Jellyfin server listens on UDP
//! 7359 and answers the datagram `Who is JellyfinServer?` with a small JSON object naming itself
//! (`{"Address":"http://192.168.1.10:8096","Id":"…","Name":"…"}`) — the same discovery the official
//! clients use. One broadcast, a short window for answers, deduplicated by server id.
//!
//! A server can disable discovery (Networking → "Enable auto discovery"); then this finds nothing
//! and the sign-in falls back to an address the person enters.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const PORT: u16 = 7359;
const PROBE: &[u8] = b"Who is JellyfinServer?";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub id: String,
    pub name: String,
    pub address: String,
}

/// Parse one discovery answer; `None` for anything that is not one.
pub fn parse(datagram: &[u8]) -> Option<Found> {
    let v: Value = serde_json::from_slice(datagram).ok()?;
    let address = v.get("Address")?.as_str()?.trim_end_matches('/').to_owned();
    if !(address.starts_with("http://") || address.starts_with("https://")) {
        return None;
    }
    Some(Found {
        id: v.get("Id").and_then(Value::as_str).unwrap_or("").to_owned(),
        name: v.get("Name").and_then(Value::as_str).unwrap_or("Jellyfin").to_owned(),
        address,
    })
}

/// Broadcast once to `targets` (normally `255.255.255.255`) and collect answers for `window`.
pub fn discover_on(targets: &[(&str, u16)], window: Duration) -> Vec<Found> {
    let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else { return Vec::new() };
    let _ = sock.set_broadcast(true);
    for t in targets {
        let _ = sock.send_to(PROBE, *t);
    }
    let deadline = Instant::now() + window;
    let mut out: Vec<Found> = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        let _ = sock.set_read_timeout(Some(left));
        let Ok((n, _)) = sock.recv_from(&mut buf) else { break };
        if let Some(f) = parse(&buf[..n]) {
            if !out.iter().any(|o| (o.id == f.id && !f.id.is_empty()) || o.address == f.address) {
                out.push(f);
            }
        }
    }
    out
}

/// UDP discovery: the limited broadcast, and the directed broadcast of this television's own /24
/// (some network stacks and routers drop `255.255.255.255` but pass the subnet's own).
pub fn discover(window: Duration) -> Vec<Found> {
    let directed = local_ipv4().map(|ip| {
        let o = ip.octets();
        format!("{}.{}.{}.255", o[0], o[1], o[2])
    });
    let mut targets: Vec<(&str, u16)> = vec![("255.255.255.255", PORT)];
    if let Some(d) = directed.as_deref() {
        targets.push((d, PORT));
    }
    discover_on(&targets, window)
}

/// This television's own LAN address: the source address the kernel would use to reach the
/// internet. A UDP `connect` sends nothing; it only selects the route.
pub fn local_ipv4() -> Option<std::net::Ipv4Addr> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(("1.1.1.1", 53)).ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// The JSON body of a plain-HTTP GET, read with short deadlines. Only for the subnet scan below,
/// which talks to addresses that may answer nothing at all; real requests use libcurl.
fn probe_http(addr: std::net::SocketAddr, path: &str) -> Option<Value> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(350)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nAccept: application/json\r\n\r\n").as_bytes()).ok()?;
    let mut buf = Vec::new();
    let _ = s.take(64 * 1024).read_to_end(&mut buf);
    let at = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    serde_json::from_slice(&buf[at + 4..]).ok()
}

/// A Jellyfin answering `/System/Info/Public` at `ip:port`, as a [`Found`].
pub fn probe(ip: std::net::Ipv4Addr, port: u16) -> Option<Found> {
    let v = probe_http(std::net::SocketAddr::from((ip, port)), "/System/Info/Public")?;
    let id = v.get("Id")?.as_str()?.to_owned();
    let product = v.get("ProductName").and_then(Value::as_str).unwrap_or("Jellyfin Server");
    if !product.to_ascii_lowercase().contains("jellyfin") {
        return None;
    }
    Some(Found {
        id,
        name: v.get("ServerName").and_then(Value::as_str).unwrap_or("Jellyfin").to_owned(),
        address: format!("http://{ip}:{port}"),
    })
}

/// When UDP discovery is silent — a server in Docker without UDP 7359 published, discovery turned
/// off, or a television whose sandbox drops broadcast — knock on Jellyfin's default port across
/// this television's own /24. A handful of workers, a short connect deadline each: a few seconds
/// for the whole subnet, and nothing leaves the LAN.
pub fn scan_subnet(port: u16) -> Vec<Found> {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    let Some(me) = local_ipv4() else { return Vec::new() };
    let o = me.octets();
    let next = Arc::new(AtomicU32::new(1));
    let found: Arc<Mutex<Vec<Found>>> = Arc::new(Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for _ in 0..24 {
        let (next, found) = (next.clone(), found.clone());
        let h = plx_base::task::spawn("jellyfin-scan", move || loop {
            let n = next.fetch_add(1, Ordering::Relaxed);
            if n > 254 { break; }
            let ip = std::net::Ipv4Addr::new(o[0], o[1], o[2], n as u8);
            if ip == me { continue; }
            if let Some(f) = probe(ip, port) {
                if let Ok(mut g) = found.lock() { g.push(f); }
            }
        });
        workers.extend(h);
    }
    for w in workers {
        let _ = w.join();
    }
    let mut out = found.lock().map(|g| g.clone()).unwrap_or_default();
    out.sort_by(|a, b| a.address.cmp(&b.address));
    out.dedup_by(|a, b| a.id == b.id && !a.id.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_parses_and_noise_does_not() {
        let f = parse(br#"{"Address":"http://192.168.87.110:8096/","Id":"abc","Name":"Casa","EndpointAddress":null}"#).unwrap();
        assert_eq!(f, Found { id: "abc".into(), name: "Casa".into(), address: "http://192.168.87.110:8096".into() });
        assert!(parse(b"Who is JellyfinServer?").is_none());
        assert!(parse(br#"{"Address":"ftp://x"}"#).is_none());
    }

    #[test]
    fn a_jellyfin_answering_http_is_recognised_by_the_probe() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 512];
            let _ = c.read(&mut b);
            let body = r#"{"Id":"j1","ServerName":"Casa","ProductName":"Jellyfin Server"}"#;
            write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        });
        let f = probe(std::net::Ipv4Addr::LOCALHOST, port).unwrap();
        t.join().unwrap();
        assert_eq!((f.id.as_str(), f.address.as_str()), ("j1", &*format!("http://127.0.0.1:{port}")));
        assert!(probe(std::net::Ipv4Addr::LOCALHOST, 1).is_none(), "a closed port is no server");
    }

    #[test]
    fn a_local_responder_is_found_once() {
        let server = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], PROBE);
            let answer = br#"{"Address":"http://10.0.0.2:8096","Id":"x1","Name":"Lab"}"#;
            server.send_to(answer, from).unwrap();
            server.send_to(answer, from).unwrap();
        });
        let found = discover_on(&[("127.0.0.1", port)], Duration::from_millis(500));
        t.join().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Lab");
    }
}

#[cfg(test)]
mod scan_timing {
    #[test]
    #[ignore = "touches the real network; run by hand"]
    fn time_a_subnet_scan() {
        let t = std::time::Instant::now();
        let f = super::scan_subnet(8096);
        eprintln!("scan: {} found in {:?} (me={:?})", f.len(), t.elapsed(), super::local_ipv4());
    }
}
