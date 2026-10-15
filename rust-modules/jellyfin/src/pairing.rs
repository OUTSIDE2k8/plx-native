//! **Pairing a television with a Jellyfin server, with nothing typed on the remote.**
//!
//! 1. Find the server: the address a previous pairing saved, else Jellyfin's own LAN discovery
//!    ([`crate::discovery`]).
//! 2. Ask it for a Quick Connect code and show it on the sign-in screen (the same screen, QR card
//!    and code line the Plex sign-in draws). The QR opens the server's Quick Connect page on a
//!    phone; the person approves the code there, signed in as themselves.
//! 3. Poll until the code is approved, trade it for an access token, save [`Config`], and start the
//!    app again so the boot path finds a signed-in Jellyfin and goes straight to Home.
//!
//! The worker that drives these steps lives with the rest of the sign-in flow (`plx_session::auth`),
//! which already owns the screen's phases; this module is only the Jellyfin half of each step, so
//! it can be tested without a screen.

use std::sync::Mutex;
use std::time::Duration;

use crate::config::Config;
use crate::discovery;
use crate::upstream::{Anonymous, CurlTransport, Error, Session, Transport};

/// Why a pairing step could not go on, in the words the sign-in screen will need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// No Jellyfin answered discovery and none was saved.
    NoServer,
    /// The server answered but refuses Quick Connect (an administrator setting).
    QuickConnectOff,
    /// The server stopped answering mid-pairing.
    Unreachable,
    /// Anything else the server said.
    Refused(u16),
}

impl From<Error> for PairError {
    fn from(e: Error) -> Self {
        match e {
            Error::Unauthorized => PairError::QuickConnectOff,
            Error::Unreachable => PairError::Unreachable,
            Error::Status(s) => PairError::Refused(s),
            Error::Malformed => PairError::Refused(0),
        }
    }
}

/// Is this run pairing with Jellyfin rather than signing in to plex.tv? Set once by the boot path;
/// off by default so every test and tool that drives the Plex sign-in keeps doing so.
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn active() -> bool {
    ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_active(on: bool) {
    ACTIVE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// What the screen shows above the QR while a code is up: the server's address, the way the Plex
/// screen shows `plex.tv/link`.
static LINK: Mutex<String> = Mutex::new(String::new());

pub fn link_text() -> String {
    let s = LINK.lock().map(|g| g.clone()).unwrap_or_default();
    if s.is_empty() { "Jellyfin · Quick Connect".to_owned() } else { s }
}

fn set_link(base: &str) {
    let shown = base.trim_start_matches("http://").trim_start_matches("https://").to_owned();
    if let Ok(mut g) = LINK.lock() {
        *g = shown;
    }
}

/// The server to pair with: `(base url, server id, server name)`.
pub fn find_server(cfg: &Config, transport: &dyn Transport) -> Result<(String, String, String), PairError> {
    let identity = cfg.identity();
    let answer = |base: &str| {
        let anon = Anonymous { base: base.to_owned(), identity: &identity, transport };
        anon.public_info().ok().map(|(id, name, _version)| {
            set_link(base);
            (base.to_owned(), id, name)
        })
    };
    if !cfg.server.is_empty() {
        if let Some(found) = answer(&cfg.server) {
            return Ok(found);
        }
    }
    for _ in 0..2 {
        for f in discovery::discover(Duration::from_millis(1500)) {
            if let Some(found) = answer(&f.address) {
                return Ok(found);
            }
        }
    }
    // Names a home network commonly gives the server (a router's DNS, mDNS where the TV resolves it).
    for host in ["jellyfin", "jellyfin.lan", "jellyfin.local", "jellyfin.home"] {
        if let Some(found) = answer(&format!("http://{host}:8096")) {
            return Ok(found);
        }
    }
    // UDP discovery was silent: knock on Jellyfin's default ports across the local subnet.
    for port in [8096, 8920] {
        for f in discovery::scan_subnet(port) {
            plx_base::eventlog::log(&format!("jellyfin: found a server by subnet scan on port {port}"));
            if let Some(found) = answer(&f.address) {
                return Ok(found);
            }
        }
    }
    Err(PairError::NoServer)
}

/// Start Quick Connect: `(code, secret)`.
pub fn start_code(base: &str, cfg: &Config, transport: &dyn Transport) -> Result<(String, String), PairError> {
    let identity = cfg.identity();
    let anon = Anonymous { base: base.to_owned(), identity: &identity, transport };
    Ok(anon.quick_connect_start()?)
}

/// `Ok(None)` while the code waits for approval.
pub fn poll(base: &str, cfg: &Config, secret: &str, transport: &dyn Transport) -> Result<Option<Session>, PairError> {
    let identity = cfg.identity();
    let anon = Anonymous { base: base.to_owned(), identity: &identity, transport };
    match anon.quick_connect_poll(secret) {
        // A secret the server has forgotten (it restarted, or the code expired) is "start over".
        Err(Error::Status(404)) | Err(Error::Status(400)) => Err(PairError::Refused(404)),
        other => Ok(other?),
    }
}

/// Record a successful pairing. Returns the saved config.
pub fn complete(mut cfg: Config, base: &str, server_id: &str, server_name: &str, s: &Session) -> std::io::Result<Config> {
    cfg.server = base.to_owned();
    cfg.server_id = if s.server_id.is_empty() { server_id.to_owned() } else { s.server_id.clone() };
    cfg.server_name = server_name.to_owned();
    cfg.user_id = s.user_id.clone();
    cfg.user_name = s.user_name.clone();
    cfg.token = s.token.clone();
    cfg.save()?;
    Ok(cfg)
}

/// The production transport, for callers outside this crate.
pub fn transport() -> CurlTransport {
    CurlTransport
}

/// Replace this process with a fresh run of the same program and arguments, so the boot path sees
/// the new pairing. Returns only on failure (the screen then asks the person to reopen the app).
#[cfg(unix)]
pub fn restart_process() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = std::fs::read_link("/proc/self/exe")
        .or_else(|_| std::env::current_exe());
    let Ok(exe) = exe else { return std::io::Error::other("no executable path") };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    plx_base::eventlog::log("jellyfin: paired — restarting into the signed-in boot");
    std::process::Command::new(exe).args(args).exec()
}

#[cfg(not(unix))]
pub fn restart_process() -> std::io::Error {
    std::io::Error::other("restart not supported")
}

/// The Quick Connect page of the server's web client, for the QR a phone scans.
pub fn quick_connect_url(base: &str) -> String {
    format!("{}/web/#/quickconnect", base.trim_end_matches('/'))
}

/// A QR code as a PNG in the form the sign-in screen expects of Plex's: WHITE modules on a
/// TRANSPARENT ground (the screen tints the modules dark on its own white card).
pub fn qr_png(text: &str) -> Vec<u8> {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else { return Vec::new() };
    let scale = 8i32;
    let side = code.size() * scale;
    let mut rgba = vec![0u8; (side * side * 4) as usize];
    for y in 0..side {
        for x in 0..side {
            if code.get_module(x / scale, y / scale) {
                let o = ((y * side + x) * 4) as usize;
                rgba[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    png_rgba(side as u32, side as u32, &rgba)
}

/// A minimal PNG encoder: 8-bit RGBA, no filtering, zlib with STORED blocks. The QR is a few
/// hundred kilobytes uncompressed and decoded once, so compression would buy nothing here.
pub fn png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in rgba.chunks(w as usize * 4).take(h as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_in = tag.to_vec();
    crc_in.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::fake::Fake;

    #[test]
    fn the_qr_png_is_a_valid_png_of_white_modules_on_transparency() {
        let png = qr_png("http://192.168.87.110:8096/web/#/quickconnect");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        // IHDR: square, 8-bit RGBA.
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!(w, h);
        assert_eq!(&png[24..26], &[8, 6]);
        assert!(w >= 21 * 8);
    }

    #[test]
    fn known_checksums() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_saved_server_is_tried_before_discovery_and_names_the_link() {
        let mut cfg = Config::fresh();
        cfg.server = "http://10.0.0.9:8096".into();
        let f = Fake::default().on("GET", "/System/Info/Public", 200, r#"{"Id":"S1","ServerName":"Casa","Version":"10.10.7"}"#);
        let (base, id, name) = find_server(&cfg, &f).unwrap();
        assert_eq!((base.as_str(), id.as_str(), name.as_str()), ("http://10.0.0.9:8096", "S1", "Casa"));
        assert_eq!(link_text(), "10.0.0.9:8096");
    }

    #[test]
    fn quick_connect_off_is_its_own_error() {
        let f = Fake::default().on("POST", "/QuickConnect/Initiate", 401, "");
        assert_eq!(start_code("http://h", &Config::fresh(), &f), Err(PairError::QuickConnectOff));
    }
}
