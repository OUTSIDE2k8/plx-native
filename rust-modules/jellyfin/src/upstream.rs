//! **The Jellyfin side**: one signed-in user on one server, and the requests the bridge makes for it.
//!
//! The transport is a trait so the mapping and routing can be tested against canned answers; the
//! production transport is the app's own libcurl door (`plx_net::net::request`), the same one every
//! Plex request takes, so a Jellyfin behind HTTPS (a reverse proxy with a real certificate) works
//! with the television's trust store exactly as plex.tv does.
//!
//! **Two API generations.** Jellyfin 10.9 moved the per-user endpoints from `/Users/{id}/Items…` to
//! `/Items?userId=…`, `/UserViews`, `/UserItems/Resume` and friends, and 10.10/10.11 keep only the new
//! forms for some of them. [`Upstream::get_first`] asks the new path first and falls back to the
//! old one on a 404, so one bridge serves 10.8 through current servers without a version table.

use serde_json::Value;

/// What a request to the Jellyfin server came back as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 401/403: the token is no longer valid (revoked device, changed password).
    Unauthorized,
    /// Any other non-2xx status.
    Status(u16),
    /// Nothing answered, or the transfer failed.
    Unreachable,
    /// A 2xx whose body was not the JSON the caller asked for.
    Malformed,
}

/// One HTTP response: status and body. Headers are not needed by any caller (content types are
/// asked for explicitly, `format=Jpg`, `Stream.srt`).
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

pub trait Transport: Send + Sync {
    fn send(&self, method: &str, url: &str, headers: &[String], body: Option<&[u8]>) -> Option<Reply>;
}

/// The production transport: the app's libcurl door with the bulk deadlines (a library listing of a
/// big collection is not an API-sized answer).
pub struct CurlTransport;

impl Transport for CurlTransport {
    fn send(&self, method: &str, url: &str, headers: &[String], body: Option<&[u8]>) -> Option<Reply> {
        let r = plx_net::net::request(url, headers, method, body, plx_net::net::BULK, false,
            Some(64 << 20), None)?;
        Some(Reply { status: r.status, body: r.body })
    }
}

/// The identity this client presents to Jellyfin. `device_id` is stable per installation, so the
/// server's Devices page shows one entry for the television rather than one per boot.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    pub device_id: String,
    pub device_name: String,
    pub version: String,
}

impl ClientIdentity {
    /// The `Authorization: MediaBrowser …` header value, with or without a token.
    pub fn auth_header(&self, token: Option<&str>) -> String {
        let esc = |s: &str| s.replace('"', "'");
        let mut v = format!(
            "MediaBrowser Client=\"PlxNative\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            esc(&self.device_name), esc(&self.device_id), esc(&self.version));
        if let Some(t) = token {
            v.push_str(&format!(", Token=\"{}\"", esc(t)));
        }
        v
    }
}

/// A signed-in Jellyfin user on one server.
pub struct Upstream {
    /// Base URL with no trailing slash, e.g. `http://192.168.1.10:8096` or `https://jf.example/jellyfin`.
    pub base: String,
    pub token: String,
    pub user_id: String,
    pub identity: ClientIdentity,
    transport: Box<dyn Transport>,
}

pub fn normalize_base(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    if t.starts_with("http://") || t.starts_with("https://") {
        t.to_owned()
    } else {
        format!("http://{t}")
    }
}

impl Upstream {
    pub fn new(base: &str, token: &str, user_id: &str, identity: ClientIdentity,
        transport: Box<dyn Transport>) -> Self {
        Self { base: normalize_base(base), token: token.to_owned(), user_id: user_id.to_owned(),
            identity, transport }
    }

    fn headers(&self, json_body: bool) -> Vec<String> {
        let mut h = vec![
            format!("Authorization: {}", self.identity.auth_header(Some(&self.token))),
            format!("X-Emby-Token: {}", self.token),
            "Accept: application/json".to_owned(),
        ];
        if json_body {
            h.push("Content-Type: application/json".to_owned());
        }
        h
    }

    /// `path` is everything after the base: `/Items?userId=…`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// A URL the TELEVISION opens directly (media, images): it carries the token as `api_key`, the
    /// one credential form Jellyfin accepts on a plain GET with no headers.
    pub fn signed_url(&self, path: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{}{}{}api_key={}", self.base, path, sep, self.token)
    }

    pub fn raw(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Reply, Error> {
        let r = self.transport
            .send(method, &self.url(path), &self.headers(body.is_some()), body)
            .ok_or(Error::Unreachable)?;
        match r.status {
            200..=299 => Ok(r),
            401 | 403 => Err(Error::Unauthorized),
            s => Err(Error::Status(s)),
        }
    }

    /// A plain-text upload (the client-log document).
    pub fn post_text(&self, path: &str, body: &[u8]) -> Result<(), Error> {
        let mut h = self.headers(false);
        h.push("Content-Type: text/plain".to_owned());
        let r = self.transport.send("POST", &self.url(path), &h, Some(body)).ok_or(Error::Unreachable)?;
        match r.status {
            200..=299 => Ok(()),
            401 | 403 => Err(Error::Unauthorized),
            s => Err(Error::Status(s)),
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, Error> {
        let r = self.raw("GET", path, None)?;
        serde_json::from_slice(&r.body).map_err(|_| Error::Malformed)
    }

    /// The first of `paths` that is not a 404 (see the module doc on API generations).
    pub fn get_first(&self, paths: &[String]) -> Result<Value, Error> {
        let mut last = Error::Status(404);
        for p in paths {
            match self.get(p) {
                Err(Error::Status(404)) => last = Error::Status(404),
                other => return other,
            }
        }
        Err(last)
    }

    /// A write whose answer is not read beyond its status (playback reports, played state).
    pub fn send_json(&self, method: &str, path: &str, body: &Value) -> Result<(), Error> {
        let bytes = serde_json::to_vec(body).map_err(|_| Error::Malformed)?;
        self.raw(method, path, Some(&bytes)).map(|_| ())
    }

    /// [`Self::send_json`] over the new path, then the legacy one on a 404.
    pub fn send_first(&self, method: &str, paths: &[String], body: Option<&Value>) -> Result<(), Error> {
        let mut last = Error::Status(404);
        for p in paths {
            let r = match body {
                Some(b) => self.send_json(method, p, b),
                None => self.raw(method, p, None).map(|_| ()),
            };
            match r {
                Err(Error::Status(404)) => last = Error::Status(404),
                other => return other,
            }
        }
        Err(last)
    }
}

/// Unauthenticated calls a sign-in makes before there is a user: public info, password sign-in,
/// Quick Connect. Separate from [`Upstream`] because there is no token yet.
pub struct Anonymous<'a> {
    pub base: String,
    pub identity: &'a ClientIdentity,
    pub transport: &'a dyn Transport,
}

/// A successful sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub token: String,
    pub user_id: String,
    pub user_name: String,
    pub server_id: String,
}

impl Anonymous<'_> {
    fn headers(&self, json_body: bool) -> Vec<String> {
        let mut h = vec![
            format!("Authorization: {}", self.identity.auth_header(None)),
            "Accept: application/json".to_owned(),
        ];
        if json_body {
            h.push("Content-Type: application/json".to_owned());
        }
        h
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Error> {
        let bytes = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        let r = self.transport
            .send(method, &format!("{}{}", normalize_base(&self.base), path),
                &self.headers(body.is_some()), bytes.as_deref())
            .ok_or(Error::Unreachable)?;
        match r.status {
            200..=299 => {
                if r.body.is_empty() { return Ok(Value::Null); }
                serde_json::from_slice(&r.body).map_err(|_| Error::Malformed)
            }
            401 | 403 => Err(Error::Unauthorized),
            s => Err(Error::Status(s)),
        }
    }

    /// `/System/Info/Public`: proves a Jellyfin answers at this base and names it.
    pub fn public_info(&self) -> Result<(String, String, String), Error> {
        let v = self.call("GET", "/System/Info/Public", None)?;
        let id = v["Id"].as_str().ok_or(Error::Malformed)?.to_owned();
        Ok((id, v["ServerName"].as_str().unwrap_or("Jellyfin").to_owned(),
            v["Version"].as_str().unwrap_or("").to_owned()))
    }

    pub fn sign_in(&self, user: &str, password: &str) -> Result<Session, Error> {
        let body = serde_json::json!({ "Username": user, "Pw": password });
        let v = self.call("POST", "/Users/AuthenticateByName", Some(&body))?;
        session_of(&v)
    }

    /// Start Quick Connect: the code to show and the secret to poll with. `Status(401)` from a
    /// server where the administrator turned Quick Connect off reads as [`Error::Unauthorized`].
    pub fn quick_connect_start(&self) -> Result<(String, String), Error> {
        let v = self.call("POST", "/QuickConnect/Initiate", None)?;
        let code = v["Code"].as_str().ok_or(Error::Malformed)?.to_owned();
        let secret = v["Secret"].as_str().ok_or(Error::Malformed)?.to_owned();
        Ok((code, secret))
    }

    /// `Ok(None)` while the code waits for approval.
    pub fn quick_connect_poll(&self, secret: &str) -> Result<Option<Session>, Error> {
        let v = self.call("GET", &format!("/QuickConnect/Connect?secret={}", enc(secret)), None)?;
        if !v["Authenticated"].as_bool().unwrap_or(false) {
            return Ok(None);
        }
        let body = serde_json::json!({ "Secret": secret });
        let s = self.call("POST", "/Users/AuthenticateWithQuickConnect", Some(&body))?;
        session_of(&s).map(Some)
    }
}

fn session_of(v: &Value) -> Result<Session, Error> {
    Ok(Session {
        token: v["AccessToken"].as_str().ok_or(Error::Malformed)?.to_owned(),
        user_id: v["User"]["Id"].as_str().ok_or(Error::Malformed)?.to_owned(),
        user_name: v["User"]["Name"].as_str().unwrap_or("").to_owned(),
        server_id: v["ServerId"].as_str().unwrap_or("").to_owned(),
    })
}

/// Percent-encode a query value (RFC 3986 unreserved kept).
pub fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

#[cfg(test)]
pub mod fake {
    //! A scripted transport: each route is a (method, path-prefix) → (status, body) pair, and every
    //! request is recorded so a test can assert what reached the server.
    use super::{Reply, Transport};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct Fake {
        pub routes: Vec<(String, String, u16, Vec<u8>)>,
        pub seen: Mutex<Vec<(String, String, Option<Vec<u8>>)>>,
    }

    impl Fake {
        pub fn on(mut self, method: &str, path_prefix: &str, status: u16, body: &str) -> Self {
            self.routes.push((method.into(), path_prefix.into(), status, body.as_bytes().to_vec()));
            self
        }

        pub fn on_bytes(mut self, method: &str, path_prefix: &str, status: u16, body: &[u8]) -> Self {
            self.routes.push((method.into(), path_prefix.into(), status, body.to_vec()));
            self
        }
    }

    impl Transport for Fake {
        fn send(&self, method: &str, url: &str, _h: &[String], body: Option<&[u8]>) -> Option<Reply> {
            let path = url.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_default();
            self.seen.lock().unwrap().push((method.into(), path.clone(), body.map(<[u8]>::to_vec)));
            // Longest matching prefix wins, so a specific route can shadow a general one.
            self.routes.iter()
                .filter(|(m, p, ..)| m == method && path.starts_with(p.as_str()))
                .max_by_key(|(_, p, ..)| p.len())
                .map(|(_, _, s, b)| Reply { status: *s, body: b.clone() })
                .or(Some(Reply { status: 404, body: Vec::new() }))
        }
    }

    impl Transport for std::sync::Arc<Fake> {
        fn send(&self, method: &str, url: &str, h: &[String], body: Option<&[u8]>) -> Option<Reply> {
            (**self).send(method, url, h, body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> ClientIdentity {
        ClientIdentity { device_id: "dev1".into(), device_name: "LG \"C9\"".into(), version: "0.8.0".into() }
    }

    #[test]
    fn the_auth_header_names_the_device_and_never_breaks_its_quoting() {
        let h = ident().auth_header(Some("tok"));
        assert!(h.starts_with("MediaBrowser Client=\"PlxNative\""));
        assert!(h.contains("Device=\"LG 'C9'\""));
        assert!(h.ends_with("Token=\"tok\""));
        assert!(!ident().auth_header(None).contains("Token"));
    }

    #[test]
    fn a_bare_host_becomes_an_http_base_and_trailing_slashes_go() {
        assert_eq!(normalize_base("192.168.1.2:8096/"), "http://192.168.1.2:8096");
        assert_eq!(normalize_base("https://jf.example/jellyfin/"), "https://jf.example/jellyfin");
    }

    #[test]
    fn get_first_falls_back_on_404_and_stops_on_anything_else() {
        let f = fake::Fake::default()
            .on("GET", "/Users/u/Views", 200, r#"{"Items":[]}"#)
            .on("GET", "/Items/x", 401, "");
        let up = Upstream::new("http://h", "t", "u", ident(), Box::new(f));
        let v = up.get_first(&["/UserViews?userId=u".into(), "/Users/u/Views".into()]).unwrap();
        assert!(v["Items"].is_array());
        assert_eq!(up.get_first(&["/Items/x".into(), "/Users/u/Views".into()]), Err(Error::Unauthorized));
    }

    #[test]
    fn quick_connect_reports_waiting_then_a_session() {
        let waiting = fake::Fake::default()
            .on("GET", "/QuickConnect/Connect", 200, r#"{"Authenticated":false}"#);
        let id = ident();
        let a = Anonymous { base: "h:8096".into(), identity: &id, transport: &waiting };
        assert_eq!(a.quick_connect_poll("s3"), Ok(None));
        let done = fake::Fake::default()
            .on("GET", "/QuickConnect/Connect", 200, r#"{"Authenticated":true}"#)
            .on("POST", "/Users/AuthenticateWithQuickConnect", 200,
                r#"{"AccessToken":"T","ServerId":"S","User":{"Id":"U","Name":"jordi"}}"#);
        let a = Anonymous { base: "h:8096".into(), identity: &id, transport: &done };
        let s = a.quick_connect_poll("s3").unwrap().unwrap();
        assert_eq!((s.token.as_str(), s.user_id.as_str(), s.server_id.as_str()), ("T", "U", "S"));
    }

    #[test]
    fn signed_urls_carry_the_token_as_api_key() {
        let up = Upstream::new("http://h:8096", "tok", "u", ident(), Box::new(fake::Fake::default()));
        assert_eq!(up.signed_url("/Videos/a/stream?static=true"), "http://h:8096/Videos/a/stream?static=true&api_key=tok");
        assert_eq!(up.signed_url("/Videos/a/stream"), "http://h:8096/Videos/a/stream?api_key=tok");
    }
}
