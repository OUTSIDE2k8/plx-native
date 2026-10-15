//! **The Plex Media Server surface, answered from Jellyfin.** One function per PMS route the app
//! requests (the set `tests/mock_pms.py` answers, which is the set the app is known to need), each
//! translating to the Jellyfin calls that hold the same facts.
//!
//! Three kinds of answer: JSON (`MediaContainer`), bytes (artwork and subtitle sidecars, fetched
//! here because the app's control plane does not follow redirects), and a REDIRECT for media — a
//! direct-play part or a progressive transcode is a `302` to Jellyfin's own stream URL, which the
//! app's media plane follows (`stream_redirect` for plaintext, `curlio` for HTTPS). The media bytes
//! therefore never pass through this process: the television's demuxer reads the server directly,
//! exactly as it would read a PMS part.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::ids::{self, Interned, Interner};
use crate::map::{self, Ctx, TICKS_PER_MS};
use crate::upstream::{enc, Error, Upstream};

/// What a route answers.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Json(u16, Value),
    Bytes(u16, &'static str, Vec<u8>),
    Redirect(String),
}

fn ok(mc: Map<String, Value>) -> Answer {
    Answer::Json(200, container(mc))
}

/// `{"MediaContainer": {…, "size": n}}` — `size` counts whichever list the container carries.
pub fn container(mut mc: Map<String, Value>) -> Value {
    let size = ["Metadata", "Directory", "Hub", "SearchResult"].iter()
        .find_map(|k| mc.get(*k).and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0);
    mc.insert("size".into(), json!(size));
    mc.entry("allowSync").or_insert(json!(false));
    mc.entry("identifier").or_insert(json!("com.plexapp.plugins.library"));
    json!({ "MediaContainer": Value::Object(mc) })
}

fn mc() -> Map<String, Value> {
    Map::new()
}

fn with(mut m: Map<String, Value>, k: &str, v: impl Into<Value>) -> Map<String, Value> {
    m.insert(k.into(), v.into());
    m
}

/// A parsed request: path segments, query (last value wins, percent-decoded) and headers.
pub struct Req<'a> {
    pub method: &'a str,
    pub path: String,
    pub segs: Vec<String>,
    pub q: HashMap<String, String>,
    pub headers: &'a [(String, String)],
}

pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => { out.push(v); i += 3; }
                    Err(_) => { out.push(b'%'); i += 1; }
                }
            }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl<'a> Req<'a> {
    pub fn parse(method: &'a str, target: &str, headers: &'a [(String, String)]) -> Self {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut q = HashMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            q.insert(decode(k), decode(v));
        }
        let path = decode(path);
        let segs = path.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
        Self { method, path, segs, q, headers }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// A query value, or the same-named header (PMS accepts paging and tokens either way).
    pub fn param(&self, name: &str) -> Option<&str> {
        self.q.get(name).map(String::as_str).or_else(|| self.header(name))
    }

    fn int(&self, name: &str) -> Option<i64> {
        self.param(name)?.trim().parse().ok()
    }

    fn flag(&self, name: &str) -> bool {
        matches!(self.param(name), Some("1") | Some("true"))
    }

    /// The paging window, when the request asked for one (`X-Plex-Container-Start`/`-Size`).
    fn window(&self) -> Option<(i64, i64)> {
        let start = self.int("X-Plex-Container-Start")?;
        Some((start.max(0), self.int("X-Plex-Container-Size").unwrap_or(50).clamp(0, 500)))
    }
}

/// What the bridge remembers about a playback in progress, so the reports Jellyfin wants (start,
/// progress, stop, each with the same session id) can be made from Plex's stateless timeline calls.
#[derive(Clone, Debug)]
struct Playing {
    source: String,
    session: String,
    method: &'static str,
    started: bool,
}

pub struct Bridge {
    pub up: Upstream,
    pub ids: Interner,
    pub server_id: String,
    pub server_name: String,
    /// The secret the app must present (`X-Plex-Token`). The listener is loopback-only, but other
    /// apps on the television share that loopback.
    pub local_token: String,
    views: Mutex<Option<(Instant, Vec<Value>)>>,
    queues: Mutex<(i64, HashMap<i64, Vec<Value>>)>,
    playing: Mutex<HashMap<String, Playing>>,
    /// The audio/subtitle stream ids the app last chose per part (`PUT /library/parts/{id}`).
    selection: Mutex<HashMap<i64, (Option<i64>, Option<i64>)>>,
    /// The last requests the app made (method, path, status), newest last, for the playback
    /// diagnostic ([`Bridge::schedule_diagnostic`]). Never a query: a token can ride one.
    recent: Mutex<std::collections::VecDeque<String>>,
    /// When the last diagnostic was scheduled (seconds since the bridge started), so a burst of
    /// playback requests uploads one.
    last_diag: std::sync::atomic::AtomicU64,
    started: Instant,
}

const LIST_FIELDS: &str = "PrimaryImageAspectRatio,SortName,Overview,ProductionYear,PremiereDate,DateCreated,OfficialRating,CommunityRating,CriticRating,Genres,ChildCount,RecursiveItemCount,ParentId,MediaSources";
const DETAIL_FIELDS: &str = "PrimaryImageAspectRatio,SortName,Overview,ProductionYear,PremiereDate,DateCreated,OfficialRating,CommunityRating,CriticRating,Genres,ChildCount,RecursiveItemCount,ParentId,People,Studios,Taglines,MediaSources,MediaStreams,Chapters,ProductionLocations,ProviderIds";
const IMAGES: &str = "&EnableImageTypes=Primary,Backdrop,Thumb&ImageTypeLimit=1&EnableUserData=true";

impl Bridge {
    pub fn new(up: Upstream, server_id: &str, server_name: &str, local_token: &str) -> Self {
        Self {
            up, ids: Interner::new(), server_id: server_id.to_owned(), server_name: server_name.to_owned(),
            local_token: local_token.to_owned(), views: Mutex::new(None),
            queues: Mutex::new((1, HashMap::new())), playing: Mutex::new(HashMap::new()),
            selection: Mutex::new(HashMap::new()),
            recent: Mutex::new(std::collections::VecDeque::new()),
            last_diag: std::sync::atomic::AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    pub fn machine_id(&self) -> String {
        format!("jellyfin-{}", self.server_id)
    }

    fn u(&self) -> &str {
        &self.up.user_id
    }

    // ---- views and context --------------------------------------------------------------------

    /// The user's movie and show views, cached for a minute (every listing needs them for its
    /// section id and title, and they change only when an administrator adds a library).
    fn views(&self) -> Result<Vec<Value>, Error> {
        if let Ok(g) = self.views.lock() {
            if let Some((at, v)) = g.as_ref() {
                if at.elapsed() < Duration::from_secs(60) {
                    return Ok(v.clone());
                }
            }
        }
        let u = self.u();
        let v = self.up.get_first(&[format!("/UserViews?userId={u}"), format!("/Users/{u}/Views")])?;
        let items = v["Items"].as_array().cloned().unwrap_or_default();
        let ctx = self.ctx(None, None, None);
        let kept: Vec<Value> = items.into_iter().filter(|x| map::section(&ctx, x).is_some()).collect();
        if let Ok(mut g) = self.views.lock() {
            *g = Some((Instant::now(), kept.clone()));
        }
        Ok(kept)
    }

    fn ctx(&self, section: Option<(i64, String)>, movies: Option<(i64, String)>,
        shows: Option<(i64, String)>) -> Ctx<'_> {
        Ctx { ids: &self.ids, section, movies, shows }
    }

    /// A context with the default movie/show sections filled from the views.
    fn base_ctx(&self) -> Ctx<'_> {
        let views = self.views().unwrap_or_default();
        let first = |ct: &[&str]| views.iter()
            .find(|v| ct.contains(&v["CollectionType"].as_str().unwrap_or("")))
            .map(|v| (ids::fold(v["Id"].as_str().unwrap_or("")), v["Name"].as_str().unwrap_or("").to_owned()));
        let movies = first(&["movies", "homevideos", "musicvideos"]);
        let shows = first(&["tvshows"]);
        self.ctx(None, movies, shows)
    }

    /// The Jellyfin view behind a Plex section id: the interner first, then a fresh view list (a
    /// section id persisted by an earlier run).
    fn section_view(&self, sid: i64) -> Option<(String, String, String)> {
        if let Some(Interned::Section { id, kind }) = self.ids.get(sid) {
            let title = self.views().ok()?.iter().find(|v| v["Id"] == id.as_str())
                .and_then(|v| v["Name"].as_str()).unwrap_or("").to_owned();
            return Some((id, kind, title));
        }
        self.views().ok()?.iter().find(|v| ids::fold(v["Id"].as_str().unwrap_or("")) == sid).map(|v| {
            let id = v["Id"].as_str().unwrap_or("").to_owned();
            let kind = v["CollectionType"].as_str().unwrap_or("").to_owned();
            self.ids.section(&id, &kind);
            (id, kind, v["Name"].as_str().unwrap_or("").to_owned())
        })
    }

    fn map_items(&self, ctx: &Ctx, list: &Value) -> Vec<Value> {
        let rows = list.get("Items").and_then(Value::as_array).or_else(|| list.as_array());
        rows.map(|r| r.iter().filter_map(|x| map::item(ctx, x)).collect()).unwrap_or_default()
    }

    fn items(&self, query: &str) -> Result<Value, Error> {
        let u = self.u();
        self.up.get_first(&[format!("/Items?userId={u}&{query}"), format!("/Users/{u}/Items?{query}")])
    }

    fn one(&self, id: &str, detail: bool) -> Result<Option<Value>, Error> {
        if !ids::is_item_id(id) {
            return Ok(None);
        }
        let fields = if detail { DETAIL_FIELDS } else { LIST_FIELDS };
        let v = self.items(&format!("Ids={id}&Fields={fields}{IMAGES}"))?;
        Ok(v["Items"].get(0).cloned())
    }

    // ---- the router ---------------------------------------------------------------------------

    pub fn handle(&self, method: &str, target: &str, headers: &[(String, String)]) -> Answer {
        let r = Req::parse(method, target, headers);
        let token_ok = r.param("X-Plex-Token").is_some_and(|t| t == self.local_token);
        // `/identity` is unauthenticated on a real PMS too: the app proves the machine before it
        // spends a token on it.
        if !token_ok && r.path != "/identity" && r.path != "/" {
            return Answer::Json(401, json!({ "error": "unauthorized" }));
        }
        let answer = match self.route(&r) {
            Ok(a) => a,
            Err(Error::Unauthorized) => Answer::Json(401, json!({ "error": "jellyfin refused the token" })),
            Err(Error::Unreachable) => Answer::Json(503, json!({ "error": "jellyfin unreachable" })),
            Err(Error::Status(s)) => Answer::Json(if s == 404 { 404 } else { 502 }, container(mc())),
            Err(Error::Malformed) => Answer::Json(502, container(mc())),
        };
        self.note(&r, &answer);
        answer
    }

    /// Remember one request for the diagnostic, and schedule one when it was a playback request.
    fn note(&self, r: &Req, a: &Answer) {
        let status = match a { Answer::Json(s, _) | Answer::Bytes(s, ..) => *s, Answer::Redirect(_) => 302 };
        let keys: Vec<&str> = r.q.keys().map(String::as_str).filter(|k| !k.to_ascii_lowercase().contains("token")).collect();
        let line = format!("{:>7.1}s {} {} -> {status} [{}]", self.started.elapsed().as_secs_f32(), r.method, r.path,
            keys.join(","));
        if let Ok(mut g) = self.recent.lock() {
            if g.len() >= 300 { g.pop_front(); }
            g.push_back(line);
        }
        let playback = r.path.starts_with("/video/") || r.path.starts_with("/library/parts/")
            || r.path == "/:/timeline";
        if playback {
            self.schedule_diagnostic();
        }
    }

    /// **The playback diagnostic.** Twenty seconds after the app starts a playback, upload the
    /// bridge's recent requests and the tail of the app's event log to the Jellyfin server's own
    /// client-log store (`POST /ClientLog/Document`, Dashboard → Logs), so a playback that fails on
    /// a television nobody can attach a debugger to still leaves its evidence where its owner can
    /// read it. At most once a minute. Nothing leaves the household: it goes to the same server.
    fn schedule_diagnostic(&self) {
        use std::sync::atomic::Ordering;
        if !crate::reports_allowed() {
            return;
        }
        let now = self.started.elapsed().as_secs() + 61;
        let last = self.last_diag.load(Ordering::Relaxed);
        if now < last + 60 || self.last_diag.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        let up = Upstream::new(&self.up.base, &self.up.token, &self.up.user_id, self.up.identity.clone(),
            Box::new(crate::upstream::CurlTransport));
        let requests: Vec<String> = self.recent.lock().map(|g| g.iter().cloned().collect()).unwrap_or_default();
        plx_base::task::spawn_small("jellyfin-diag", move || {
            std::thread::sleep(Duration::from_secs(20));
            let mut doc = String::from("PlxNative Jellyfin bridge — playback diagnostic\n\n== bridge requests ==\n");
            for l in &requests { doc.push_str(l); doc.push('\n'); }
            crate::append_crash_tail(&mut doc);
            doc.push_str("\n== app event log (tail) ==\n");
            if let Ok(bytes) = std::fs::read(plx_base::eventlog::events_log()) {
                let tail = &bytes[bytes.len().saturating_sub(96 * 1024)..];
                doc.push_str(&plx_base::eventlog::redact_tokens(&String::from_utf8_lossy(tail)));
            }
            let r = up.post_text("/ClientLog/Document", doc.as_bytes());
            plx_base::eventlog::log(&format!("jellyfin: playback diagnostic uploaded to the server's logs ({})",
                if r.is_ok() { "ok" } else { "refused" }));
        });
    }

    fn route(&self, r: &Req) -> Result<Answer, Error> {
        let s: Vec<&str> = r.segs.iter().map(String::as_str).collect();
        match (r.method, s.as_slice()) {
            (_, []) | (_, ["identity"]) => Ok(ok(with(with(with(with(mc(),
                "machineIdentifier", self.machine_id()), "friendlyName", self.server_name.clone()),
                "version", "1.41.0.0000-jellyfin"), "platform", "Jellyfin"))),
            (_, ["library", "sections"]) => self.sections(),
            (_, ["library", "sections", sid, "all"]) => self.section_all(r, sid),
            (_, ["library", "sections", sid, "recentlyAdded"]) => self.section_recent(r, sid),
            (_, ["library", "sections", sid, "firstCharacter"]) => self.first_characters(r, sid),
            (_, ["library", "sections", sid, "genre"]) => self.genres(sid),
            (_, ["library", "sections", _, _]) => Ok(ok(with(mc(), "Directory", json!([])))),
            (_, ["library", "metadata", id]) => self.metadata(r, id),
            (_, ["library", "metadata", id, "children"]) => self.children(r, id),
            (_, ["library", "metadata", id, "allLeaves"]) => self.all_leaves(id),
            (_, ["library", "metadata", id, "similar"]) => self.similar(r, id),
            (_, ["library", "metadata", id, "related"]) => self.related(id),
            (_, ["library", "metadata", id, kind @ ("thumb" | "art"), tag]) => self.image(id, kind, tag, r),
            (_, ["library", "metadata", id, kind @ ("thumb" | "art")]) => self.image(id, kind, "", r),
            (_, ["library", "metadata", _, _]) => Ok(ok(with(mc(), "Metadata", json!([])))),
            (_, ["library", "people", pid, "media"]) => self.person_media(r, pid),
            (_, ["library", "people", id, "thumb", tag]) => self.image(id, "thumb", tag, r),
            (_, ["library", "collections", _, "children"]) => Ok(ok(with(mc(), "Metadata", json!([])))),
            (_, ["library", "search"]) => self.library_search(r),
            (_, ["hubs"]) | (_, ["hubs", "promoted"]) => self.home_hubs(r),
            (_, ["hubs", "continueWatching"]) => self.continue_hub(r),
            (_, ["hubs", "continueWatching", "items"]) => self.continue_items(r),
            (_, ["hubs", "sections", sid]) => self.section_hubs(sid),
            (_, ["hubs", "search"]) => self.search_hubs(r),
            (_, ["photo", ":", "transcode"]) => self.photo(r),
            ("PUT", ["library", "parts", pid]) => self.select_tracks(r, pid),
            (_, ["library", "parts", pid, ..]) => self.part(pid),
            (_, ["library", "streams", sid]) => self.sidecar(sid),
            (_, ["video", ":", "transcode", "universal", "decision"]) => self.decision(r),
            (_, ["video", ":", "transcode", "universal", "start.mkv"]) => self.start(r),
            (_, ["video", ":", "transcode", "universal", "stop"]) => self.stop(r),
            (_, ["video", ":", "transcode", "universal", _]) => Ok(Answer::Json(404, container(mc()))),
            (_, [":", "timeline"]) => self.timeline(r),
            (_, [":", "scrobble"]) => self.played(r, true),
            (_, [":", "unscrobble"]) => self.played(r, false),
            (_, [":", "progress"]) => self.progress(r),
            (_, ["actions", "removeFromContinueWatching"]) => self.forget_position(r),
            (_, ["status", "sessions", "close"]) => Ok(ok(mc())),
            ("POST", ["playQueues"]) | ("GET", ["playQueues"]) => self.new_queue(r),
            (_, ["playQueues", qid]) => self.queue(qid),
            _ => {
                plx_base::eventlog::log(&format!("jellyfin: unanswered {} {}", r.method,
                    r.path.split('/').take(4).collect::<Vec<_>>().join("/")));
                Ok(ok(mc()))
            }
        }
    }

    // ---- libraries ----------------------------------------------------------------------------

    fn sections(&self) -> Result<Answer, Error> {
        let ctx = self.ctx(None, None, None);
        let dirs: Vec<Value> = self.views()?.iter().filter_map(|v| map::section(&ctx, v)).collect();
        Ok(ok(with(mc(), "Directory", dirs)))
    }

    fn plex_sort(sort: &str) -> (String, &'static str) {
        let (field, dir) = sort.split_once(':').unwrap_or((sort, ""));
        let by = match field {
            "addedAt" => "DateCreated,SortName",
            "originallyAvailableAt" => "PremiereDate,SortName",
            "year" => "ProductionYear,SortName",
            "rating" => "CriticRating,SortName",
            "audienceRating" => "CommunityRating,SortName",
            "lastViewedAt" => "DatePlayed,SortName",
            "random" => "Random",
            "duration" => "Runtime,SortName",
            "episode.addedAt" => "DateLastContentAdded,SortName",
            _ => "SortName",
        };
        (by.into(), if dir == "desc" { "Descending" } else { "Ascending" })
    }

    fn type_filter(plex_type: Option<&str>, collection_type: &str) -> &'static str {
        match plex_type {
            Some("1") => "Movie",
            Some("2") => "Series",
            Some("3") => "Season",
            Some("4") => "Episode",
            _ if collection_type == "tvshows" => "Series",
            _ => "Movie",
        }
    }

    fn section_all(&self, r: &Req, sid: &str) -> Result<Answer, Error> {
        let sid: i64 = sid.parse().unwrap_or(-1);
        let Some((view, ct, title)) = self.section_view(sid) else {
            return Ok(Answer::Json(404, container(with(mc(), "Metadata", json!([])))));
        };
        if r.param("type") == Some("18") {
            // Collections: Jellyfin keeps box sets in a view of their own; none per library.
            let mut m = with(mc(), "Metadata", json!([]));
            if r.flag("includeMeta") { m.insert("Meta".into(), sort_meta(sid, "18")); }
            return Ok(ok(m));
        }
        let kind = Self::type_filter(r.param("type"), &ct);
        let (by, order) = Self::plex_sort(r.param("sort").unwrap_or("titleSort"));
        let mut q = format!("ParentId={view}&Recursive=true&IncludeItemTypes={kind}&SortBy={by}&SortOrder={order}&Fields={LIST_FIELDS}{IMAGES}");
        if r.flag("unwatched") { q.push_str("&Filters=IsUnplayed"); }
        if r.flag("inProgress") { q.push_str("&Filters=IsResumable"); }
        if let Some(g) = r.int("genre").and_then(|g| self.ids.get(g)) {
            if let Interned::Genre { id } = g {
                if ids::is_item_id(&id) { q.push_str(&format!("&GenreIds={id}")); }
                else { q.push_str(&format!("&Genres={}", enc(&id))); }
            }
        }
        for key in ["actor", "director", "writer"] {
            if let Some(Interned::Person { id }) = r.int(key).and_then(|p| self.ids.get(p)) {
                q.push_str(&format!("&PersonIds={id}"));
            }
        }
        let window = r.window();
        if let Some((start, size)) = window {
            q.push_str(&format!("&StartIndex={start}&Limit={size}"));
        }
        let v = self.items(&q)?;
        let ctx = self.ctx(Some((sid, title)), None, None);
        let rows = self.map_items(&ctx, &v);
        let mut m = with(mc(), "Metadata", rows);
        if let Some((start, _)) = window {
            m.insert("totalSize".into(), json!(v["TotalRecordCount"].as_i64().unwrap_or(0)));
            m.insert("offset".into(), json!(start));
        }
        if r.flag("includeMeta") {
            m.insert("Meta".into(), sort_meta(sid, if kind == "Series" { "2" } else { "1" }));
        }
        Ok(ok(m))
    }

    fn section_recent(&self, r: &Req, sid: &str) -> Result<Answer, Error> {
        let sid: i64 = sid.parse().unwrap_or(-1);
        let Some((view, ct, title)) = self.section_view(sid) else {
            return Ok(ok(with(mc(), "Metadata", json!([]))));
        };
        let kind = if ct == "tvshows" { "Episode" } else { "Movie" };
        let (start, size) = r.window().unwrap_or((0, 50));
        let v = self.items(&format!("ParentId={view}&Recursive=true&IncludeItemTypes={kind}&SortBy=DateCreated,SortName&SortOrder=Descending&StartIndex={start}&Limit={size}&Fields={LIST_FIELDS}{IMAGES}"))?;
        let rows = self.map_items(&self.ctx(Some((sid, title)), None, None), &v);
        let mut m = with(mc(), "Metadata", rows);
        if r.window().is_some() {
            m.insert("totalSize".into(), json!(v["TotalRecordCount"].as_i64().unwrap_or(0)));
            m.insert("offset".into(), json!(start));
        }
        Ok(ok(m))
    }

    /// The A–Z rail: letter counts in the listing's own (title, ascending) order. Jellyfin has no
    /// such endpoint, so the names are read once and counted here.
    fn first_characters(&self, r: &Req, sid: &str) -> Result<Answer, Error> {
        let sid: i64 = sid.parse().unwrap_or(-1);
        let Some((view, ct, _)) = self.section_view(sid) else {
            return Ok(ok(with(mc(), "Directory", json!([]))));
        };
        if r.param("type") == Some("18") {
            return Ok(ok(with(mc(), "Directory", json!([]))));
        }
        let kind = Self::type_filter(r.param("type"), &ct);
        let v = self.items(&format!("ParentId={view}&Recursive=true&IncludeItemTypes={kind}&SortBy=SortName&SortOrder=Ascending&Fields=SortName&EnableImages=false&EnableUserData=false"))?;
        let mut counts: Vec<(String, i64)> = Vec::new();
        for x in v["Items"].as_array().into_iter().flatten() {
            let name = x["SortName"].as_str().or_else(|| x["Name"].as_str()).unwrap_or("");
            let c = name.chars().next().map(|c| c.to_uppercase().next().unwrap_or(c)).unwrap_or('#');
            let letter = if c.is_alphabetic() { c.to_string() } else { "#".into() };
            match counts.last_mut() {
                Some((l, n)) if *l == letter => *n += 1,
                _ => counts.push((letter, 1)),
            }
        }
        // A letter can recur after a differently-sorting run (accents); merge, keeping first order.
        let mut merged: Vec<(String, i64)> = Vec::new();
        for (l, n) in counts {
            if let Some(e) = merged.iter_mut().find(|(m, _)| *m == l) { e.1 += n; } else { merged.push((l, n)); }
        }
        let dirs: Vec<Value> = merged.into_iter()
            .map(|(l, n)| json!({ "key": l.to_lowercase(), "title": l, "size": n })).collect();
        Ok(ok(with(mc(), "Directory", dirs)))
    }

    fn genres(&self, sid: &str) -> Result<Answer, Error> {
        let sid: i64 = sid.parse().unwrap_or(-1);
        let Some((view, _, _)) = self.section_view(sid) else {
            return Ok(ok(with(mc(), "Directory", json!([]))));
        };
        let u = self.u();
        let v = self.up.get(&format!("/Genres?userId={u}&ParentId={view}&SortBy=SortName"))?;
        let dirs: Vec<Value> = v["Items"].as_array().into_iter().flatten().map(|g| {
            let gid = self.ids.genre(g["Id"].as_str().unwrap_or(""));
            json!({ "key": gid.to_string(), "title": g["Name"],
                    "fastKey": format!("/library/sections/{sid}/all?genre={gid}") })
        }).collect();
        Ok(ok(with(mc(), "Directory", dirs)))
    }

    // ---- items --------------------------------------------------------------------------------

    fn metadata(&self, r: &Req, ids_csv: &str) -> Result<Answer, Error> {
        let wanted: Vec<&str> = ids_csv.split(',').filter(|i| ids::is_item_id(i)).collect();
        if wanted.is_empty() {
            return Ok(Answer::Json(404, container(with(mc(), "Metadata", json!([])))));
        }
        let v = self.items(&format!("Ids={}&Fields={DETAIL_FIELDS}{IMAGES}", wanted.join(",")))?;
        let ctx = self.base_ctx();
        let mut rows = self.map_items(&ctx, &v);
        if rows.is_empty() {
            return Ok(Answer::Json(404, container(with(mc(), "Metadata", json!([])))));
        }
        for row in rows.iter_mut() {
            self.apply_selection(row);
        }
        if rows.len() == 1 {
            let kind = rows[0]["type"].as_str().unwrap_or("").to_owned();
            let id = rows[0]["ratingKey"].as_str().unwrap_or("").to_owned();
            if r.flag("includeOnDeck") && kind == "show" {
                if let Some(next) = self.next_up(Some(&id))?.into_iter().next() {
                    rows[0]["OnDeck"] = json!({ "Metadata": next });
                }
            }
            if r.flag("includeMarkers") && matches!(kind.as_str(), "movie" | "episode") {
                // Media segments are 10.10+ (and filled by a plugin); a server without them has no
                // markers, which is what the app shows for a PMS item without any.
                if let Ok(seg) = self.up.get(&format!("/MediaSegments/{id}")) {
                    let marks = map::markers(&seg);
                    if !marks.is_empty() { rows[0]["Marker"] = Value::Array(marks); }
                }
            }
        }
        Ok(ok(with(mc(), "Metadata", rows)))
    }

    /// Mark the stream the app last chose for each part `selected` (and its siblings not), as PMS
    /// does after `PUT /library/parts/{id}`.
    fn apply_selection(&self, row: &mut Value) {
        let Ok(sel) = self.selection.lock() else { return };
        for media in row["Media"].as_array_mut().into_iter().flatten() {
            for part in media["Part"].as_array_mut().into_iter().flatten() {
                let Some((audio, sub)) = part["id"].as_i64().and_then(|p| sel.get(&p)).copied() else { continue };
                for st in part["Stream"].as_array_mut().into_iter().flatten() {
                    let (kind, id) = (st["streamType"].as_i64(), st["id"].as_i64());
                    let pick = match kind { Some(2) => audio, Some(3) => sub, _ => None };
                    let Some(pick) = pick else { continue };
                    if let Some(obj) = st.as_object_mut() {
                        if Some(pick) == id { obj.insert("selected".into(), json!(true)); }
                        else { obj.remove("selected"); }
                    }
                }
            }
        }
    }

    fn children(&self, r: &Req, id: &str) -> Result<Answer, Error> {
        if !ids::is_item_id(id) {
            return Ok(ok(with(mc(), "Metadata", json!([]))));
        }
        let mut q = format!("ParentId={id}&SortBy=ParentIndexNumber,IndexNumber,SortName&SortOrder=Ascending&Fields={LIST_FIELDS},MediaSources{IMAGES}");
        let window = r.window();
        if let Some((start, size)) = window { q.push_str(&format!("&StartIndex={start}&Limit={size}")); }
        let v = self.items(&q)?;
        let rows = self.map_items(&self.base_ctx(), &v);
        let mut m = with(mc(), "Metadata", rows);
        if let Some((start, _)) = window {
            m.insert("totalSize".into(), json!(v["TotalRecordCount"].as_i64().unwrap_or(0)));
            m.insert("offset".into(), json!(start));
        }
        Ok(ok(m))
    }

    fn all_leaves(&self, id: &str) -> Result<Answer, Error> {
        if !ids::is_item_id(id) {
            return Ok(ok(with(mc(), "Metadata", json!([]))));
        }
        let u = self.u();
        let v = self.up.get(&format!("/Shows/{id}/Episodes?userId={u}&Fields={LIST_FIELDS}{IMAGES}"))?;
        Ok(ok(with(mc(), "Metadata", self.map_items(&self.base_ctx(), &v))))
    }

    fn similar_list(&self, id: &str, limit: i64) -> Result<Vec<Value>, Error> {
        if !ids::is_item_id(id) {
            return Ok(Vec::new());
        }
        let u = self.u();
        let v = self.up.get(&format!("/Items/{id}/Similar?userId={u}&Limit={limit}&Fields={LIST_FIELDS}{IMAGES}"))?;
        Ok(self.map_items(&self.base_ctx(), &v))
    }

    fn similar(&self, r: &Req, id: &str) -> Result<Answer, Error> {
        let (start, size) = r.window().unwrap_or((0, 24));
        let all = self.similar_list(id, (start + size).min(100))?;
        let page: Vec<Value> = all.iter().skip(start as usize).take(size as usize).cloned().collect();
        let mut m = with(mc(), "Metadata", page);
        if r.window().is_some() {
            m.insert("totalSize".into(), json!(all.len()));
            m.insert("offset".into(), json!(start));
        }
        Ok(ok(m))
    }

    fn related(&self, id: &str) -> Result<Answer, Error> {
        let rows = self.similar_list(id, 24)?;
        if rows.is_empty() {
            return Ok(ok(with(mc(), "Hub", json!([]))));
        }
        let kind = rows[0]["type"].as_str().unwrap_or("movie").to_owned();
        let hub = json!({ "title": "More Like This", "type": kind, "hubIdentifier": "related",
            "size": rows.len(), "more": false, "key": format!("/library/metadata/{id}/similar"),
            "Metadata": rows });
        Ok(ok(with(mc(), "Hub", vec![hub])))
    }

    fn person_media(&self, r: &Req, pid: &str) -> Result<Answer, Error> {
        let Some(Interned::Person { id }) = pid.parse::<i64>().ok().and_then(|p| self.ids.get(p)) else {
            return Ok(ok(with(mc(), "Metadata", json!([]))));
        };
        let mut q = format!("PersonIds={id}&Recursive=true&IncludeItemTypes=Movie,Series&SortBy=ProductionYear,SortName&SortOrder=Descending&Fields={LIST_FIELDS}{IMAGES}");
        let window = r.window();
        if let Some((start, size)) = window { q.push_str(&format!("&StartIndex={start}&Limit={size}")); }
        let v = self.items(&q)?;
        let mut m = with(mc(), "Metadata", self.map_items(&self.base_ctx(), &v));
        if let Some((start, _)) = window {
            m.insert("totalSize".into(), json!(v["TotalRecordCount"].as_i64().unwrap_or(0)));
            m.insert("offset".into(), json!(start));
        }
        Ok(ok(m))
    }

    // ---- hubs ---------------------------------------------------------------------------------

    fn resume(&self, parent: Option<&str>, limit: i64) -> Result<Vec<Value>, Error> {
        let u = self.u();
        let mut q = format!("Limit={limit}&MediaTypes=Video&Fields={LIST_FIELDS}{IMAGES}");
        if let Some(p) = parent { q.push_str(&format!("&ParentId={p}")); }
        let v = self.up.get_first(&[format!("/UserItems/Resume?userId={u}&{q}"),
            format!("/Users/{u}/Items/Resume?{q}")])?;
        Ok(self.map_items(&self.base_ctx(), &v))
    }

    fn next_up(&self, series: Option<&str>) -> Result<Vec<Value>, Error> {
        let u = self.u();
        let mut q = format!("/Shows/NextUp?userId={u}&Limit=24&Fields={LIST_FIELDS}{IMAGES}&EnableResumable=false");
        if let Some(sid) = series { q.push_str(&format!("&SeriesId={sid}")); }
        let v = self.up.get(&q)?;
        Ok(self.map_items(&self.base_ctx(), &v))
    }

    fn recent_in(&self, view: &str, ct: &str, sid: i64, title: &str, limit: i64) -> Result<Vec<Value>, Error> {
        let kind = if ct == "tvshows" { "Episode" } else { "Movie" };
        let v = self.items(&format!("ParentId={view}&Recursive=true&IncludeItemTypes={kind}&SortBy=DateCreated,SortName&SortOrder=Descending&Limit={limit}&Fields={LIST_FIELDS}{IMAGES}"))?;
        Ok(self.map_items(&self.ctx(Some((sid, title.to_owned())), None, None), &v))
    }

    fn home_hubs(&self, r: &Req) -> Result<Answer, Error> {
        let count = r.int("count").unwrap_or(12).clamp(1, 50);
        let mut hubs = Vec::new();
        let cw = if r.flag("excludeContinueWatching") { Vec::new() } else { self.resume(None, count)? };
        hubs.push(hub("home.continue", "Continue Watching", "mixed", "/hubs/continueWatching", cw, None));
        let deck = self.next_up(None).unwrap_or_default();
        if !deck.is_empty() {
            hubs.push(hub("home.ondeck", "On Deck", "episode", "/hubs/home/onDeck", deck, None));
        }
        let views = self.views()?;
        let movie_libs = views.iter().filter(|v| v["CollectionType"] != "tvshows").count();
        let show_libs = views.len() - movie_libs;
        for v in &views {
            let (id, ct, name) = (v["Id"].as_str().unwrap_or(""), v["CollectionType"].as_str().unwrap_or(""),
                v["Name"].as_str().unwrap_or(""));
            let sid = self.ids.section(id, ct);
            let rows = self.recent_in(id, ct, sid, name, count)?;
            if rows.is_empty() { continue; }
            let tv = ct == "tvshows";
            let ident = match (tv, if tv { show_libs } else { movie_libs }) {
                (false, 1) => "home.movies.recent".to_owned(),
                (true, 1) => "home.television.recent".to_owned(),
                (false, _) => format!("movie.recentlyadded.{sid}"),
                (true, _) => format!("tv.recentlyadded.{sid}"),
            };
            hubs.push(hub(&ident, &format!("Recently Added in {name}"), if tv { "episode" } else { "movie" },
                &format!("/library/sections/{sid}/recentlyAdded"), rows, Some((sid, name))));
        }
        Ok(ok(with(mc(), "Hub", hubs)))
    }

    fn continue_hub(&self, r: &Req) -> Result<Answer, Error> {
        let rows = self.resume(None, r.int("count").unwrap_or(12).clamp(1, 50))?;
        Ok(ok(with(mc(), "Hub", vec![hub("home.continue", "Continue Watching", "mixed",
            "/hubs/continueWatching", rows, None)])))
    }

    fn continue_items(&self, r: &Req) -> Result<Answer, Error> {
        let (start, size) = r.window().unwrap_or((0, 50));
        let rows = self.resume(None, (start + size).min(200))?;
        let total = rows.len();
        let page: Vec<Value> = rows.into_iter().skip(start as usize).take(size as usize).collect();
        let mut m = with(mc(), "Metadata", page);
        if r.window().is_some() {
            m.insert("totalSize".into(), json!(total));
            m.insert("offset".into(), json!(start));
        }
        Ok(ok(m))
    }

    fn section_hubs(&self, sid: &str) -> Result<Answer, Error> {
        let sid: i64 = sid.parse().unwrap_or(-1);
        let Some((view, ct, title)) = self.section_view(sid) else {
            return Ok(ok(with(mc(), "Hub", json!([]))));
        };
        let kind = if ct == "tvshows" { "show" } else { "movie" };
        let cw = self.resume(Some(&view), 12)?;
        let recent = self.recent_in(&view, &ct, sid, &title, 24)?;
        let mut hubs = vec![hub(&format!("{kind}.inprogress.{sid}"), "Continue Watching", kind,
            &format!("/hubs/sections/{sid}/continueWatching"), cw, Some((sid, &title)))];
        hubs.push(hub(&format!("{kind}.recentlyadded.{sid}"), "Recently Added",
            if ct == "tvshows" { "episode" } else { "movie" },
            &format!("/library/sections/{sid}/recentlyAdded"), recent, Some((sid, &title))));
        Ok(ok(with(mc(), "Hub", hubs)))
    }

    // ---- search -------------------------------------------------------------------------------

    fn search_rows(&self, query: &str, kinds: &str, limit: i64) -> Result<Vec<Value>, Error> {
        let term = query.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let v = self.items(&format!("searchTerm={}&Recursive=true&IncludeItemTypes={kinds}&Limit={limit}&Fields={LIST_FIELDS}{IMAGES}",
            enc(term)))?;
        Ok(self.map_items(&self.base_ctx(), &v))
    }

    fn search_hubs(&self, r: &Req) -> Result<Answer, Error> {
        let query = r.param("query").unwrap_or("");
        let limit = r.int("limit").unwrap_or(10).clamp(1, 50);
        let mut hubs = Vec::new();
        for (jf, plex, title) in [("Movie", "movie", "Movies"), ("Series", "show", "Shows"),
            ("Episode", "episode", "Episodes")] {
            let rows = self.search_rows(query, jf, limit)?;
            if !rows.is_empty() {
                hubs.push(hub(plex, title, plex, "", rows, None));
            }
        }
        Ok(ok(with(mc(), "Hub", hubs)))
    }

    fn library_search(&self, r: &Req) -> Result<Answer, Error> {
        let query = r.param("query").unwrap_or("");
        let limit = r.int("limit").unwrap_or(30).clamp(1, 100);
        let types = r.param("searchTypes").unwrap_or("movies,tv");
        let mut kinds = Vec::new();
        if types.contains("movie") { kinds.push("Movie"); }
        if types.contains("tv") || types.contains("show") { kinds.extend(["Series", "Episode"]); }
        if kinds.is_empty() { kinds.extend(["Movie", "Series"]); }
        let rows = self.search_rows(query, &kinds.join(","), limit)?;
        let results: Vec<Value> = rows.into_iter().map(|m| json!({ "score": 1.0, "Metadata": m })).collect();
        Ok(ok(with(mc(), "SearchResult", results)))
    }

    // ---- artwork ------------------------------------------------------------------------------

    fn image(&self, id: &str, kind: &str, tag: &str, r: &Req) -> Result<Answer, Error> {
        let w = r.int("width").unwrap_or(0).clamp(0, 3840);
        let h = r.int("height").unwrap_or(0).clamp(0, 2160);
        self.fetch_image(id, kind, tag, w, h)
    }

    fn fetch_image(&self, id: &str, kind: &str, tag: &str, w: i64, h: i64) -> Result<Answer, Error> {
        let which = if kind == "art" { "Backdrop/0" } else { "Primary" };
        if !ids::is_item_id(id) {
            return Ok(Answer::Bytes(404, "text/plain", Vec::new()));
        }
        let mut path = format!("/Items/{id}/Images/{which}?quality=90&format=Jpg");
        if !tag.is_empty() { path.push_str(&format!("&tag={}", enc(tag))); }
        if w > 0 { path.push_str(&format!("&fillWidth={w}")); }
        if h > 0 { path.push_str(&format!("&fillHeight={h}")); }
        let reply = self.up.raw("GET", &path, None)?;
        Ok(Answer::Bytes(200, sniff(&reply.body), reply.body))
    }

    /// `/photo/:/transcode?url=<a bridge image path>&width&height` — the app's resized-artwork door.
    fn photo(&self, r: &Req) -> Result<Answer, Error> {
        let url = r.param("url").unwrap_or("");
        // A full URL back to this bridge (the app sometimes absolutises an artwork path) is the
        // same image: keep only its path.
        let url = match url.split_once("://") {
            Some((_, rest)) => rest.find('/').map_or("", |at| &rest[at..]),
            None => url,
        };
        let path = url.split('?').next().unwrap_or("");
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let w = r.int("width").unwrap_or(0).clamp(0, 3840);
        let h = r.int("height").unwrap_or(0).clamp(0, 2160);
        match segs.as_slice() {
            ["library", "metadata", id, kind @ ("thumb" | "art"), tag]
            | ["library", "people", id, kind @ "thumb", tag] => self.fetch_image(id, kind, tag, w, h),
            // PMS also answers the tagless form, and the app builds it for a parent's artwork.
            ["library", "metadata", id, kind @ ("thumb" | "art")] => self.fetch_image(id, kind, "", w, h),
            other => {
                // The shape only (segment count and the first two words), never an id.
                plx_base::eventlog::log(&format!("jellyfin: no artwork for a {}-segment path under /{}",
                    other.len(), other.iter().take(2).copied().collect::<Vec<_>>().join("/")));
                Ok(Answer::Bytes(404, "text/plain", Vec::new()))
            }
        }
    }

    // ---- playback -----------------------------------------------------------------------------

    fn rating_key_of(r: &Req) -> Option<String> {
        let path = r.param("path").or_else(|| r.param("key")).or_else(|| r.param("ratingKey"))
            .or_else(|| r.param("uri"))?;
        let rk = path.rsplit('/').next()?.to_owned();
        ids::is_item_id(&rk).then_some(rk)
    }

    fn play_session(&self, item: &str, source: &str, method: &'static str) -> String {
        let mut g = match self.playing.lock() { Ok(g) => g, Err(_) => return String::new() };
        let entry = g.entry(item.to_owned()).or_insert_with(|| Playing {
            source: source.to_owned(),
            session: format!("{:x}", ids::fold(&format!("{item}{:?}", Instant::now()))),
            method,
            started: false,
        });
        entry.source = source.to_owned();
        entry.method = method;
        entry.session.clone()
    }

    /// The direct-play part: a redirect to Jellyfin's static stream of exactly that file.
    fn part(&self, pid: &str) -> Result<Answer, Error> {
        let Some(Interned::Part { item, source, container }) = pid.parse::<i64>().ok().and_then(|p| self.ids.get(p)) else {
            return Ok(Answer::Bytes(404, "text/plain", Vec::new()));
        };
        let session = self.play_session(&item, &source, "DirectPlay");
        Ok(Answer::Redirect(self.up.signed_url(&format!(
            "/Videos/{item}/stream.{container}?static=true&mediaSourceId={source}&playSessionId={session}&deviceId={}",
            enc(&self.up.identity.device_id)))))
    }

    fn select_tracks(&self, r: &Req, pid: &str) -> Result<Answer, Error> {
        let Ok(pid) = pid.parse::<i64>() else { return Ok(ok(mc())) };
        if let Ok(mut g) = self.selection.lock() {
            let e = g.entry(pid).or_insert((None, None));
            if let Some(a) = r.int("audioStreamID") { e.0 = Some(a); }
            if let Some(s) = r.int("subtitleStreamID") { e.1 = Some(s); }
        }
        Ok(ok(mc()))
    }

    fn sidecar(&self, sid: &str) -> Result<Answer, Error> {
        let Some(Interned::Stream { item, source, index, codec, .. }) = sid.parse::<i64>().ok().and_then(|s| self.ids.get(s)) else {
            return Ok(Answer::Bytes(404, "text/plain", Vec::new()));
        };
        let (fmt, mime) = match codec.as_str() {
            "ass" | "ssa" => ("ass", "text/x-ssa"),
            "vtt" => ("vtt", "text/vtt"),
            _ => ("srt", "application/x-subrip"),
        };
        let reply = self.up.raw("GET", &format!("/Videos/{item}/{source}/Subtitles/{index}/0/Stream.{fmt}"), None)?;
        Ok(Answer::Bytes(200, mime, reply.body))
    }

    /// The item as the decision answer carries it: its first media, with per-stream decisions.
    fn decided_item(&self, rk: &str) -> Result<Option<Value>, Error> {
        let Some(v) = self.one(rk, true)? else { return Ok(None) };
        let Some(mut row) = map::item(&self.base_ctx(), &v) else { return Ok(None) };
        self.apply_selection(&mut row);
        if let Some(media) = row["Media"].as_array_mut() {
            media.truncate(1);
        }
        Ok(Some(row))
    }

    fn decision(&self, r: &Req) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else {
            return Ok(ok(with(with(mc(), "generalDecisionCode", 2000), "generalDecisionText", "No item")));
        };
        let Some(mut row) = self.decided_item(&rk)? else {
            return Ok(ok(with(with(mc(), "generalDecisionCode", 2000), "generalDecisionText", "No such item")));
        };
        let direct = r.flag("directPlay");
        let plan = TranscodePlan::of(r);
        let audio_pick = r.int("audioStreamID");
        let Some(part) = row["Media"][0]["Part"].get_mut(0) else {
            return Ok(ok(with(with(mc(), "generalDecisionCode", 2000), "generalDecisionText", "No media")));
        };
        let mut any_transcode = false;
        if let Some(obj) = part.as_object_mut() {
            obj.insert("decision".into(), json!(if direct { "directplay" } else { "transcode" }));
        }
        let default_audio = part["Stream"].as_array().and_then(|s| s.iter()
            .find(|x| x["streamType"] == 2 && x["selected"] == true).and_then(|x| x["id"].as_i64()));
        let chosen_audio = audio_pick.or(default_audio);
        for st in part["Stream"].as_array_mut().into_iter().flatten() {
            let kind = st["streamType"].as_i64().unwrap_or(0);
            let codec = st["codec"].as_str().unwrap_or("").to_owned();
            let decision = if direct {
                "copy"
            } else {
                match kind {
                    1 if plan.video_copy => "copy",
                    1 => { any_transcode = true; st["codec"] = json!(plan.video_codec); "transcode" }
                    2 if Some(st["id"].as_i64().unwrap_or(-1)) != chosen_audio => continue,
                    2 if plan.audio_copy && plan.copyable_audio(&codec) => "copy",
                    2 => { any_transcode = true; st["codec"] = json!(plan.audio_codec); "transcode" }
                    _ => continue,
                }
            };
            st["decision"] = json!(decision);
        }
        let text = if direct { "Direct play OK" } else if any_transcode { "Transcode OK" } else { "Direct stream OK" };
        Ok(ok(with(with(with(with(with(mc(), "generalDecisionCode", 1000), "generalDecisionText", text),
            "mdeDecisionCode", 1000), "transcodeDecisionCode", 1000), "Metadata", vec![row])))
    }

    /// The progressive transcode: a redirect to Jellyfin's `stream.mkv` with the same codecs the
    /// decision promised, so the app's Load payload describes what arrives.
    fn start(&self, r: &Req) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else { return Ok(Answer::Bytes(404, "text/plain", Vec::new())) };
        let Some(v) = self.one(&rk, true)? else { return Ok(Answer::Bytes(404, "text/plain", Vec::new())) };
        let Some(src) = v["MediaSources"].get(0) else { return Ok(Answer::Bytes(404, "text/plain", Vec::new())) };
        let source = src["Id"].as_str().unwrap_or("").to_owned();
        let plan = TranscodePlan::of(r);
        let session = self.play_session(&rk, &source, "Transcode");
        let mut q = format!("/Videos/{rk}/stream.mkv?static=false&container=mkv&mediaSourceId={source}&playSessionId={session}&deviceId={}",
            enc(&self.up.identity.device_id));
        q.push_str(&format!("&videoCodec={}&audioCodec={}", plan.video_codec, plan.audio_codec));
        q.push_str(&format!("&allowVideoStreamCopy={}&allowAudioStreamCopy={}", plan.video_copy, plan.audio_copy));
        if let Some(kbps) = r.int("maxVideoBitrate").filter(|k| *k > 0) {
            q.push_str(&format!("&maxVideoBitrate={}", kbps * 1000));
        }
        if let Some((w, h)) = r.param("videoResolution").and_then(|res| res.split_once('x')) {
            q.push_str(&format!("&maxWidth={}&maxHeight={}", w.trim(), h.trim()));
        }
        if let Some(Interned::Stream { index, .. }) = r.int("audioStreamID").and_then(|a| self.ids.get(a)) {
            q.push_str(&format!("&audioStreamIndex={index}"));
        }
        match r.int("subtitleStreamID").filter(|s| *s > 0).and_then(|s| self.ids.get(s)) {
            Some(Interned::Stream { index, .. }) if r.param("subtitles") == Some("burn") =>
                q.push_str(&format!("&subtitleStreamIndex={index}&subtitleMethod=Encode")),
            _ => q.push_str("&subtitleStreamIndex=-1"),
        }
        if let Some(off) = r.param("offset").and_then(|o| o.parse::<f64>().ok()).filter(|o| *o > 0.0) {
            q.push_str(&format!("&startTimeTicks={}", (off * 10_000_000.0) as i64));
        }
        Ok(Answer::Redirect(self.up.signed_url(&q)))
    }

    fn stop(&self, r: &Req) -> Result<Answer, Error> {
        let session = r.param("session").unwrap_or("");
        let dev = enc(&self.up.identity.device_id);
        let _ = self.up.raw("DELETE", &format!("/Videos/ActiveEncodings?deviceId={dev}&playSessionId={}", enc(session)), None);
        Ok(ok(mc()))
    }

    /// `/:/timeline` — Plex's one stateless report, as Jellyfin's start/progress/stop trio.
    fn timeline(&self, r: &Req) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else { return Ok(ok(mc())) };
        let state = r.param("state").unwrap_or("playing");
        let ticks = r.int("time").unwrap_or(0).max(0) * TICKS_PER_MS;
        let (entry, first) = {
            let Ok(mut g) = self.playing.lock() else { return Ok(ok(mc())) };
            let Some(e) = g.get_mut(&rk) else { return Ok(ok(mc())) };
            let first = !e.started;
            e.started = true;
            (e.clone(), first)
        };
        let body = json!({
            "ItemId": rk, "MediaSourceId": entry.source, "PlaySessionId": entry.session,
            "PositionTicks": ticks, "IsPaused": state == "paused", "CanSeek": true,
            "PlayMethod": entry.method,
            "EventName": match state { "paused" => "pause", "stopped" => "stop", _ => "timeupdate" },
        });
        let path = match state {
            "stopped" => "/Sessions/Playing/Stopped",
            _ if first => "/Sessions/Playing",
            _ => "/Sessions/Playing/Progress",
        };
        let res = self.up.send_json("POST", path, &body);
        if state == "stopped" {
            if let Ok(mut g) = self.playing.lock() { g.remove(&rk); }
        }
        res.map(|_| ok(mc()))
    }

    fn played(&self, r: &Req, played: bool) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else { return Ok(ok(mc())) };
        let u = self.u();
        let method = if played { "POST" } else { "DELETE" };
        self.up.send_first(method, &[format!("/UserPlayedItems/{rk}?userId={u}"),
            format!("/Users/{u}/PlayedItems/{rk}")], None)?;
        Ok(ok(mc()))
    }

    fn set_position(&self, rk: &str, ms: i64) -> Result<(), Error> {
        let u = self.u();
        let body = json!({ "PlaybackPositionTicks": ms.max(0) * TICKS_PER_MS });
        self.up.send_first("POST", &[format!("/UserItems/{rk}/UserData?userId={u}"),
            format!("/Users/{u}/Items/{rk}/UserData")], Some(&body))
    }

    fn progress(&self, r: &Req) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else { return Ok(ok(mc())) };
        self.set_position(&rk, r.int("time").unwrap_or(0))?;
        Ok(ok(mc()))
    }

    fn forget_position(&self, r: &Req) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else { return Ok(ok(mc())) };
        self.set_position(&rk, 0)?;
        Ok(ok(mc()))
    }

    // ---- play queues --------------------------------------------------------------------------

    fn new_queue(&self, r: &Req) -> Result<Answer, Error> {
        let Some(rk) = Self::rating_key_of(r) else {
            return Ok(ok(with(mc(), "Metadata", json!([]))));
        };
        let Some(v) = self.one(&rk, false)? else {
            return Ok(ok(with(mc(), "Metadata", json!([]))));
        };
        let ctx = self.base_ctx();
        let mut rows: Vec<Value> = Vec::new();
        if r.flag("continuous") && v["Type"] == "Episode" {
            let series = v["SeriesId"].as_str().unwrap_or("");
            let u = self.u();
            if ids::is_item_id(series) {
                if let Ok(eps) = self.up.get(&format!("/Shows/{series}/Episodes?userId={u}&StartItemId={rk}&Limit=50&Fields={LIST_FIELDS}{IMAGES}")) {
                    rows = self.map_items(&ctx, &eps);
                }
            }
        }
        if rows.is_empty() {
            rows.extend(map::item(&ctx, &v));
        }
        for (n, row) in rows.iter_mut().enumerate() {
            row["playQueueItemID"] = json!(n as i64 + 1);
        }
        let id = {
            let Ok(mut g) = self.queues.lock() else { return Ok(ok(mc())) };
            let id = g.0;
            g.0 += 1;
            g.1.insert(id, rows.clone());
            // A television plays one thing at a time: keep the last few queues only.
            let stale: Vec<i64> = g.1.keys().copied().filter(|k| *k + 8 < id).collect();
            for k in stale { g.1.remove(&k); }
            id
        };
        Ok(ok(queue_container(id, rows)))
    }

    fn queue(&self, qid: &str) -> Result<Answer, Error> {
        let id: i64 = qid.parse().unwrap_or(0);
        let rows = self.queues.lock().ok().and_then(|g| g.1.get(&id).cloned()).unwrap_or_default();
        Ok(ok(queue_container(id, rows)))
    }
}

fn queue_container(id: i64, rows: Vec<Value>) -> Map<String, Value> {
    let total = rows.len();
    with(with(with(with(with(with(mc(), "playQueueID", id), "playQueueSelectedItemID", 1),
        "playQueueSelectedItemOffset", 0), "playQueueTotalCount", total), "playQueueVersion", 1),
        "Metadata", rows)
}

fn hub(ident: &str, title: &str, kind: &str, key: &str, rows: Vec<Value>, section: Option<(i64, &str)>) -> Value {
    let mut h = json!({ "hubIdentifier": ident, "title": title, "type": kind, "key": key,
        "size": rows.len(), "more": false, "Metadata": rows });
    if let Some((sid, name)) = section {
        h["librarySectionID"] = json!(sid);
        h["librarySectionTitle"] = json!(name);
    }
    h
}

fn sort_meta(sid: i64, plex_type: &str) -> Value {
    let (kind, title) = match plex_type { "18" => ("collection", "Collections"), "2" => ("show", "TV Shows"), _ => ("movie", "Movies") };
    let sorts = if plex_type == "18" {
        json!([{ "key": "titleSort", "defaultDirection": "asc", "descKey": "titleSort:desc", "title": "Title" }])
    } else {
        json!([
            { "key": "titleSort", "defaultDirection": "asc", "descKey": "titleSort:desc", "title": "Title" },
            { "key": "addedAt", "defaultDirection": "desc", "descKey": "addedAt:desc", "title": "Date Added" },
            { "key": "originallyAvailableAt", "defaultDirection": "desc", "descKey": "originallyAvailableAt:desc", "title": "Release Date" },
            { "key": "rating", "defaultDirection": "desc", "descKey": "rating:desc", "title": "Critic Rating" },
            { "key": "audienceRating", "defaultDirection": "desc", "descKey": "audienceRating:desc", "title": "Audience Rating" },
            { "key": "lastViewedAt", "defaultDirection": "desc", "descKey": "lastViewedAt:desc", "title": "Date Viewed" },
            { "key": "random", "defaultDirection": "asc", "descKey": "random:desc", "title": "Randomly" }
        ])
    };
    json!({ "Type": [{ "key": format!("/library/sections/{sid}/all?type={plex_type}"), "type": kind,
        "title": title, "active": true, "Sort": sorts }] })
}

/// The codecs and copy permissions a transcode request carries, read from the same query the app
/// sends a PMS (`X-Plex-Client-Profile-Extra`'s transcode target and direct-play profile).
struct TranscodePlan {
    video_copy: bool,
    audio_copy: bool,
    video_codec: String,
    audio_codec: String,
    dp_audio: Vec<String>,
}

impl TranscodePlan {
    fn of(r: &Req) -> Self {
        let extra = r.param("X-Plex-Client-Profile-Extra").unwrap_or("");
        let field = |section: &str, name: &str| -> Option<String> {
            let at = extra.find(section)?;
            let rest = &extra[at..];
            let end = rest.find(')').unwrap_or(rest.len());
            let body = &rest[..end];
            let key = format!("{name}=");
            let v = body.split('&').find_map(|kv| kv.strip_prefix(key.as_str()).or_else(|| {
                kv.split_once('(').and_then(|(_, t)| t.strip_prefix(key.as_str()))
            }))?;
            Some(v.to_owned())
        };
        let first = |v: Option<String>, dflt: &str| v.and_then(|v| v.split(',').next().map(str::to_owned))
            .filter(|c| !c.is_empty()).unwrap_or_else(|| dflt.to_owned());
        let video_codec = first(field("add-transcode-target", "videoCodec"), "hevc");
        let audio_codec = first(field("add-transcode-target", "audioCodec"), "ac3");
        let dp_audio = field("add-direct-play-profile", "audioCodec")
            .map(|v| v.split(',').map(str::to_owned).collect())
            .unwrap_or_else(|| ["aac", "ac3", "eac3", "mp3", "mp2", "flac", "opus", "vorbis"].iter().map(|s| s.to_string()).collect());
        Self {
            video_copy: r.flag("directStream"),
            audio_copy: r.flag("directStreamAudio") || r.flag("directStream"),
            video_codec, audio_codec, dp_audio,
        }
    }

    fn copyable_audio(&self, codec: &str) -> bool {
        self.dp_audio.iter().any(|c| c.eq_ignore_ascii_case(codec))
    }
}

/// The content type of an artwork body, from its magic number (the transport returns no headers).
fn sniff(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) { "image/png" }
    else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") { "image/webp" }
    else if b.starts_with(&[0xff, 0xd8]) { "image/jpeg" }
    else { "application/octet-stream" }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
