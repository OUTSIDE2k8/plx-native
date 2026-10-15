//! **The saved Jellyfin sign-in.** One small JSON file: the server, the user, the access token
//! Jellyfin issued to this device, and the two identifiers this install mints once (the device id
//! Jellyfin lists the television under, and the loopback secret the app shows the bridge).
//!
//! Where it lives follows the session file's own search order (`plx_base::paths::session_candidates`
//! and its reasoning): the durable jail paths first, `/tmp` last because a reboot sweeps it. A
//! simulator keeps it in its own instance root so two never share one.
//!
//! The file holds a credential, so it is written owner-only and replaced atomically (sibling temp
//! file, then `rename`), and a sign-out deletes every candidate rather than only the one read.

use std::io::Write;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::upstream::ClientIdentity;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub server: String,
    pub server_id: String,
    pub server_name: String,
    pub user_id: String,
    pub user_name: String,
    pub token: String,
    pub device_id: String,
    pub local_token: String,
}

const FILE: &str = "jellyfin.json";

pub fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if cfg!(feature = "hostsim") {
        v.push(plx_base::paths::in_runtime_dir(FILE));
        return v;
    }
    let id = plx_base::paths::app_id();
    v.extend([
        PathBuf::from(format!("/media/developer/{id}-{FILE}")),
        PathBuf::from(format!("/media/internal/.{id}-{FILE}")),
        PathBuf::from(format!("/media/developer/temp/{id}-{FILE}")),
        plx_base::paths::in_runtime_dir(FILE),
    ]);
    v
}

/// 32 lowercase hex digits from the kernel's generator (or, should that fail, from the clock and
/// the process id — a device id only needs to be distinct, and the local token only needs to be
/// unguessable by other apps on this one television).
pub fn random_hex() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf)).is_ok();
    if !ok {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos()).unwrap_or(0) ^ (u128::from(std::process::id()) << 64);
        buf = t.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl Config {
    /// A fresh install's identity: no server yet, its own device id and loopback secret.
    pub fn fresh() -> Self {
        Self { device_id: random_hex(), local_token: random_hex(), ..Self::default() }
    }

    pub fn signed_in(&self) -> bool {
        !self.server.is_empty() && !self.token.is_empty() && !self.user_id.is_empty()
    }

    pub fn identity(&self) -> ClientIdentity {
        ClientIdentity {
            device_id: self.device_id.clone(),
            device_name: "LG webOS TV".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "server": self.server, "server_id": self.server_id, "server_name": self.server_name,
            "user_id": self.user_id, "user_name": self.user_name, "token": self.token,
            "device_id": self.device_id, "local_token": self.local_token,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let c = Self {
            server: s("server"), server_id: s("server_id"), server_name: s("server_name"),
            user_id: s("user_id"), user_name: s("user_name"), token: s("token"),
            device_id: s("device_id"), local_token: s("local_token"),
        };
        // A file holding only `{"server": "http://…:8096"}` (written by hand, through Dev Manager's
        // file browser, for a network where discovery cannot reach the server) is a valid start:
        // the identity this install would have minted anyway is minted now.
        let mut c = c;
        if c.device_id.is_empty() || c.local_token.is_empty() {
            if c.server.is_empty() {
                return None;
            }
            let fresh = Self::fresh();
            c.device_id = fresh.device_id;
            c.local_token = fresh.local_token;
        }
        Some(c)
    }

    /// The first candidate that holds a readable config.
    pub fn load() -> Option<Self> {
        Self::load_from(&candidates())
    }

    pub fn load_from(paths: &[PathBuf]) -> Option<Self> {
        paths.iter().find_map(|p| {
            let bytes = std::fs::read(p).ok()?;
            Self::from_json(&serde_json::from_slice(&bytes).ok()?)
        })
    }

    /// The saved config, or a fresh identity (not yet saved) when there is none.
    pub fn load_or_fresh() -> Self {
        Self::load().unwrap_or_else(Self::fresh)
    }

    /// Write to the first candidate that accepts it; returns where.
    pub fn save(&self) -> std::io::Result<PathBuf> {
        self.save_to(&candidates())
    }

    pub fn save_to(&self, paths: &[PathBuf]) -> std::io::Result<PathBuf> {
        let body = serde_json::to_vec_pretty(&self.to_json()).unwrap_or_default();
        let mut last = std::io::Error::other("no candidate");
        for p in paths {
            match write_private(p, &body) {
                Ok(()) => return Ok(p.clone()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Sign out: forget the server and token everywhere, keep the device identity.
    pub fn forget(paths: &[PathBuf]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn write_private(path: &PathBuf, body: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.is_dir() {
            return Err(std::io::Error::other("no such directory"));
        }
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_round_trips_through_its_file_and_is_private() {
        let dir = std::env::temp_dir().join(format!("jf-cfg-{}", random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = vec![dir.join("missing-dir").join("x.json"), dir.join("jellyfin.json")];
        let mut c = Config::fresh();
        c.server = "http://192.168.1.5:8096".into();
        c.token = "t".into();
        c.user_id = "u".into();
        let at = c.save_to(&paths).unwrap();
        assert_eq!(at, paths[1], "the first writable candidate wins");
        assert_eq!(Config::load_from(&paths), Some(c.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&at).unwrap().permissions().mode() & 0o777, 0o600);
        }
        Config::forget(&paths);
        assert_eq!(Config::load_from(&paths), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hand_written_server_only_file_is_a_valid_start() {
        let c = Config::from_json(&serde_json::json!({ "server": "http://192.168.1.5:8096" })).unwrap();
        assert_eq!(c.server, "http://192.168.1.5:8096");
        assert_eq!(c.device_id.len(), 32);
        assert!(!c.signed_in());
        assert!(Config::from_json(&serde_json::json!({})).is_none());
    }

    #[test]
    fn fresh_identities_differ_and_are_not_signed_in() {
        let (a, b) = (Config::fresh(), Config::fresh());
        assert_ne!(a.device_id, b.device_id);
        assert_eq!(a.local_token.len(), 32);
        assert!(!a.signed_in());
    }
}
