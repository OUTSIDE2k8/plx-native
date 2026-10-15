//! **plx_jellyfin — the Jellyfin bridge.**
//!
//! PlxNative is a Plex client from the data layer up: 40-odd thousand lines of `plx_plex` speak the
//! Plex Media Server REST protocol, and every screen, the playback route planner and the track
//! memory are written against its models. Rather than fork all of that into a second client, this
//! crate runs a small server on the television's LOOPBACK that answers the PMS subset the app asks
//! for (`routes`) from a Jellyfin server (`upstream`), translating identifiers (`ids`) and shapes
//! (`map`) on the way. To the rest of the app a Jellyfin library is simply a Plex server at
//! `http://127.0.0.1:<port>`: home hubs, library grids, search, detail pages, direct play, the
//! progressive transcode, resume, Up Next and watched state all go through code paths that already
//! exist and are already tested against `tests/mock_pms.py`.
//!
//! Media never passes through this process: a direct-play part or a transcode start is a redirect to
//! Jellyfin's own stream URL, which the app's media plane follows to the server.
//!
//! The sign-in half lives here too (`upstream::Anonymous`: password or Quick Connect, and
//! `discovery` for the LAN broadcast Jellyfin answers), and so does the small JSON file that keeps
//! the result between boots (`config`).

pub mod config;
pub mod discovery;
pub mod ids;
pub mod map;
pub mod pairing;
pub mod routes;
pub mod server;
pub mod upstream;

use std::sync::Arc;

pub use config::Config;
pub use routes::Bridge;

/// Start the bridge for a signed-in `cfg` on a loopback port (0 = any free one). Returns the port
/// and the local token the app must present as `X-Plex-Token`, plus the machine identifier the
/// Plex layer will see.
pub struct Started {
    pub port: u16,
    pub local_token: String,
    pub machine_id: String,
    pub server_name: String,
}

pub fn start(cfg: &Config, port: u16) -> std::io::Result<Started> {
    let up = upstream::Upstream::new(&cfg.server, &cfg.token, &cfg.user_id, cfg.identity(),
        Box::new(upstream::CurlTransport));
    let bridge = Arc::new(Bridge::new(up, &cfg.server_id, &cfg.server_name, &cfg.local_token));
    let machine_id = bridge.machine_id();
    let running = server::serve(bridge, port)?;
    Ok(Started { port: running.port, local_token: cfg.local_token.clone(), machine_id,
        server_name: cfg.server_name.clone() })
}

/// The loopback port the bridge prefers. Fixed so the app's artwork cache keys (which include the
/// server origin) survive a reboot; any free port is taken when it is busy.
pub const PREFERRED_PORT: u16 = 32411;

/// The boot path's question: a saved, signed-in pairing, started. `None` means the sign-in screen
/// should pair this television first.
pub fn boot() -> Option<Started> {
    let cfg = Config::load()?;
    if !cfg.signed_in() {
        return None;
    }
    match start(&cfg, PREFERRED_PORT).or_else(|_| start(&cfg, 0)) {
        Ok(s) => {
            report_previous_crash(&cfg);
            Some(s)
        }
        Err(e) => {
            plx_base::eventlog::log(&format!("jellyfin: bridge did not start ({})", e.kind()));
            None
        }
    }
}

/// Sign out: forget the pairing (the server, the user and the token) on every candidate path.
pub fn sign_out() {
    Config::forget(&config::candidates());
}

/// **Where "Share crash reports" goes in this edition.** The upstream app reports to its author's
/// Sentry project; this build carries no such credentials, so the person's answer to the first-run
/// question decides instead whether evidence is left on THEIR OWN Jellyfin server (Dashboard →
/// Logs, `POST /ClientLog/Document`): the playback diagnostic, and a crash from the previous run.
/// The app installs its consent reader at boot; until it does, nothing is sent (fails closed).
static REPORT_GATE: std::sync::OnceLock<fn() -> bool> = std::sync::OnceLock::new();

pub fn set_report_gate(gate: fn() -> bool) {
    let _ = REPORT_GATE.set(gate);
}

pub fn reports_allowed() -> bool {
    REPORT_GATE.get().is_some_and(|g| g())
}

fn crash_log() -> std::path::PathBuf {
    plx_base::paths::in_runtime_dir("plxnative-crash.log")
}

/// Append the crash log's tail (when there is one) to a diagnostic document.
pub(crate) fn append_crash_tail(doc: &mut String) {
    if let Ok(bytes) = std::fs::read(crash_log()) {
        if !bytes.is_empty() {
            doc.push_str("\n== crash log (tail) ==\n");
            let tail = &bytes[bytes.len().saturating_sub(32 * 1024)..];
            doc.push_str(&plx_base::eventlog::redact_tokens(&String::from_utf8_lossy(tail)));
        }
    }
}

/// At boot, when the person allowed crash reports: if the crash log grew since the last report,
/// send it to the paired server's client logs. The crash log survives a relaunch (not a reboot), so
/// this is how a crash on a television nobody can attach to reaches someone who can read it.
pub fn report_previous_crash(cfg: &Config) {
    if !reports_allowed() {
        return;
    }
    let size = std::fs::metadata(crash_log()).map(|m| m.len()).unwrap_or(0);
    let mark = plx_base::paths::in_runtime_dir("jellyfin-crash-reported");
    let reported: u64 = std::fs::read_to_string(&mark).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if size == 0 || size <= reported {
        return;
    }
    let up = upstream::Upstream::new(&cfg.server, &cfg.token, &cfg.user_id, cfg.identity(),
        Box::new(upstream::CurlTransport));
    plx_base::task::spawn_small("jellyfin-crash-report", move || {
        let mut doc = String::from("PlxNative Jellyfin edition — crash from the previous run\n");
        append_crash_tail(&mut doc);
        if up.post_text("/ClientLog/Document", doc.as_bytes()).is_ok() {
            let _ = std::fs::write(&mark, size.to_string());
            plx_base::eventlog::log("jellyfin: previous crash reported to the server's logs");
        }
    });
}
