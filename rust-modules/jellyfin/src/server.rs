//! **The loopback listener.** A deliberately small HTTP/1.1 server: the only client is this app's
//! own Plex layer (libcurl on the control plane, the socket stream on the media plane), on
//! `127.0.0.1`, so it speaks exactly what those send — a request line, headers, an optional
//! `Content-Length` body — and answers with `Content-Length` and keep-alive. No chunked requests, no
//! pipelining, no TLS: none of those can arrive from the two clients it serves.
//!
//! One worker per connection on the shared small-stack spawn path, capped at [`MAX_CONNECTIONS`]
//! (the app opens a handful: artwork workers, the hub fetch, a playback report); a connection past
//! the cap is answered `503` and closed rather than queued, which the app's retry paths handle.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::routes::{Answer, Bridge};

pub const MAX_CONNECTIONS: usize = 24;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1 << 20;
const IDLE: Duration = Duration::from_secs(30);

/// A running bridge: its port, for the boot path to register `http://127.0.0.1:<port>`.
pub struct Running {
    pub port: u16,
}

/// Bind `127.0.0.1:port` (0 = any free port) and serve `bridge` on a background thread.
pub fn serve(bridge: Arc<Bridge>, port: u16) -> std::io::Result<Running> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    let live = Arc::new(AtomicUsize::new(0));
    let spawned = plx_base::task::spawn("jellyfin-bridge", move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            if live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                let _ = respond(&stream, &Answer::Bytes(503, "text/plain", b"busy".to_vec()), false, false);
                continue;
            }
            live.fetch_add(1, Ordering::SeqCst);
            let (b, l) = (bridge.clone(), live.clone());
            let ok = plx_base::task::spawn_small("jellyfin-conn", move || {
                connection(&b, stream);
                l.fetch_sub(1, Ordering::SeqCst);
            });
            if !ok {
                live.fetch_sub(1, Ordering::SeqCst);
            }
        }
    });
    if spawned.is_none() {
        return Err(std::io::Error::other("bridge thread refused"));
    }
    plx_base::eventlog::log(&format!("jellyfin: bridge listening on loopback port {port}"));
    Ok(Running { port })
}

fn connection(bridge: &Bridge, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IDLE));
    let _ = stream.set_nodelay(true);
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    loop {
        let Some(req) = read_request(&mut reader) else { return };
        let answer = bridge.handle(&req.method, &req.target, &req.headers);
        let head_only = req.method == "HEAD";
        if respond(&stream, &answer, req.keep_alive, head_only).is_err() || !req.keep_alive {
            return;
        }
    }
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    keep_alive: bool,
}

fn read_request(r: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    let mut total = 0usize;
    // Tolerate stray CRLFs between keep-alive requests (RFC 9112 §2.2).
    loop {
        line.clear();
        let n = r.read_line(&mut line).ok()?;
        if n == 0 { return None; }
        total += n;
        if !line.trim().is_empty() { break; }
        if total > MAX_HEADER_BYTES { return None; }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1").to_owned();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        let n = r.read_line(&mut h).ok()?;
        total += n;
        if n == 0 || total > MAX_HEADER_BYTES { return None; }
        let h = h.trim_end();
        if h.is_empty() { break; }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    let len: usize = get("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY_BYTES { return None; }
    if len > 0 {
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).ok()?;
    }
    let conn = get("Connection").unwrap_or("").to_ascii_lowercase();
    let keep_alive = if version == "HTTP/1.0" { conn == "keep-alive" } else { conn != "close" };
    Some(Request { method, target, headers, keep_alive })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK", 206 => "Partial Content", 302 => "Found", 400 => "Bad Request",
        401 => "Unauthorized", 404 => "Not Found", 502 => "Bad Gateway",
        503 => "Service Unavailable", _ => "Status",
    }
}

fn respond(mut s: &TcpStream, a: &Answer, keep_alive: bool, head_only: bool) -> std::io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let (status, ctype, body, location): (u16, &str, Vec<u8>, Option<&str>) = match a {
        Answer::Json(st, v) => (*st, "application/json", serde_json::to_vec(v).unwrap_or_default(), None),
        Answer::Bytes(st, ct, b) => (*st, ct, b.clone(), None),
        Answer::Redirect(url) => (302, "text/plain", Vec::new(), Some(url.as_str())),
    };
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: {conn}\r\nX-Plex-Protocol: 1.0\r\n",
        reason(status), body.len());
    if let Some(loc) = location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    if !head_only {
        s.write_all(&body)?;
    }
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{fake::Fake, ClientIdentity, Upstream};

    fn bridge() -> Arc<Bridge> {
        let f = Fake::default()
            .on("GET", "/UserViews", 200, r#"{"Items":[{"Name":"Pelis","Id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","CollectionType":"movies"}]}"#);
        let up = Upstream::new("http://jf", "tok", "u", ClientIdentity { device_id: "d".into(),
            device_name: "tv".into(), version: "0".into() }, Box::new(f));
        Arc::new(Bridge::new(up, "srv", "Casa", "local"))
    }

    fn exchange(port: u16, raw: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn the_listener_answers_identity_without_a_token_and_libraries_only_with_one() {
        let run = serve(bridge(), 0).unwrap();
        let id = exchange(run.port, "GET /identity HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(id.starts_with("HTTP/1.1 200"), "{id}");
        assert!(id.contains("jellyfin-srv"));
        let denied = exchange(run.port, "GET /library/sections HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
        let ok = exchange(run.port,
            "GET /library/sections?X-Plex-Token=local HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("Pelis"), "{ok}");
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        let run = serve(bridge(), 0).unwrap();
        let both = exchange(run.port, concat!(
            "GET /identity HTTP/1.1\r\n\r\n",
            "GET /identity HTTP/1.1\r\nX-Plex-Token: local\r\nConnection: close\r\n\r\n"));
        assert_eq!(both.matches("HTTP/1.1 200").count(), 2, "{both}");
    }
}
