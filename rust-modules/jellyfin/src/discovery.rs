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

pub fn discover(window: Duration) -> Vec<Found> {
    discover_on(&[("255.255.255.255", PORT)], window)
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
