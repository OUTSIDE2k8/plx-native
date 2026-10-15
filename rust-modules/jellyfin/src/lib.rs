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
        Ok(s) => Some(s),
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
