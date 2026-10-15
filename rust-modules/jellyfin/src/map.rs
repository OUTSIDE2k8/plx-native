//! **Jellyfin `BaseItemDto` → Plex `Metadata`.** Pure functions over `serde_json::Value`, so every
//! rule here is a host test with a literal Jellyfin answer and nothing else.
//!
//! The target shape is the one `plex::models` deserialises (field names verified against its serde
//! renames) and that `tests/mock_pms.py` serves, since the app is known to work end to end against
//! that server. Anything the app does not read is not produced.
//!
//! Units: Jellyfin times are 100 ns ticks, Plex's are milliseconds (`duration`, `viewOffset`,
//! chapter and marker offsets) and Unix seconds (`addedAt`, `lastViewedAt`). Ratings: Jellyfin's
//! `CriticRating` is a 0–100 percentage, `CommunityRating` a 0–10 score; Plex's `rating` and
//! `audienceRating` are both 0–10.

use serde_json::{json, Map, Value};

use crate::ids::Interner;

pub const TICKS_PER_MS: i64 = 10_000;

/// What a mapping needs beyond the item itself.
pub struct Ctx<'a> {
    pub ids: &'a Interner,
    /// The section an item belongs to, when the caller knows it (a section listing). Otherwise the
    /// first view of the item's kind is used ([`Ctx::default_section`]).
    pub section: Option<(i64, String)>,
    /// `(section id, title)` of the first movies view and the first shows view.
    pub movies: Option<(i64, String)>,
    pub shows: Option<(i64, String)>,
}

impl Ctx<'_> {
    fn section_for(&self, kind: &str) -> Option<(i64, String)> {
        if let Some(s) = &self.section {
            return Some(s.clone());
        }
        match kind {
            "movie" => self.movies.clone(),
            "show" | "season" | "episode" => self.shows.clone(),
            _ => None,
        }
    }
}

/// Plex's `type` for a Jellyfin `Type`; `None` for kinds the app has no screen for.
pub fn plex_kind(jf_type: &str) -> Option<&'static str> {
    Some(match jf_type {
        "Movie" => "movie",
        "Series" => "show",
        "Season" => "season",
        "Episode" => "episode",
        "Video" | "MusicVideo" | "Trailer" => "movie",
        "BoxSet" => "collection",
        _ => return None,
    })
}

fn s<'v>(v: &'v Value, k: &str) -> &'v str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn i(v: &Value, k: &str) -> Option<i64> {
    let x = v.get(k)?;
    x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)).or_else(|| x.as_str()?.parse().ok())
}

fn f(v: &Value, k: &str) -> Option<f64> {
    let x = v.get(k)?;
    x.as_f64().or_else(|| x.as_str()?.parse().ok())
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Seconds since the epoch for a Jellyfin ISO-8601 instant (`2023-04-05T12:34:56.1234567Z`).
/// Days-from-civil (Howard Hinnant's algorithm), so no date library is needed.
pub fn epoch(iso: &str) -> Option<i64> {
    let (date, time) = iso.split_once('T').unwrap_or((iso, "00:00:00"));
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>());
    let (y, m, dd) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let t: String = time.chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
    let mut tp = t.splitn(3, ':').map(|p| p.parse::<i64>().unwrap_or(0));
    let (hh, mm, ss) = (tp.next().unwrap_or(0), tp.next().unwrap_or(0), tp.next().unwrap_or(0));
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// `YYYY-MM-DD` of an ISO instant (Plex's `originallyAvailableAt`).
fn ymd(iso: &str) -> String {
    iso.get(..10).unwrap_or("").to_owned()
}

/// The bridge path of an image the app asks for later (`/photo/:/transcode?url=<this>` or directly).
/// Carries the tag so a changed image is a new URL and the app's cache key moves with it.
pub fn image_path(item: &str, kind: &str, tag: &str) -> String {
    format!("/library/metadata/{item}/{kind}/{tag}")
}

fn primary(v: &Value) -> Option<String> {
    let tag = v.get("ImageTags")?.get("Primary")?.as_str()?;
    Some(image_path(s(v, "Id"), "thumb", tag))
}

fn backdrop(v: &Value) -> Option<String> {
    if let Some(tag) = v.get("BackdropImageTags").and_then(|t| t.get(0)).and_then(Value::as_str) {
        return Some(image_path(s(v, "Id"), "art", tag));
    }
    // An episode or season has none of its own; Jellyfin names the item whose backdrop stands in.
    let parent = s(v, "ParentBackdropItemId");
    let tag = v.get("ParentBackdropImageTags").and_then(|t| t.get(0)).and_then(Value::as_str)?;
    (!parent.is_empty()).then(|| image_path(parent, "art", tag))
}

fn series_thumb(v: &Value) -> Option<String> {
    let id = s(v, "SeriesId");
    let tag = s(v, "SeriesPrimaryImageTag");
    (!id.is_empty() && !tag.is_empty()).then(|| image_path(id, "thumb", tag))
}

fn put(m: &mut Map<String, Value>, k: &str, v: impl Into<Value>) {
    m.insert(k.to_owned(), v.into());
}

fn put_str(m: &mut Map<String, Value>, k: &str, v: &str) {
    if !v.is_empty() {
        m.insert(k.to_owned(), Value::String(v.to_owned()));
    }
}

fn put_opt(m: &mut Map<String, Value>, k: &str, v: Option<impl Into<Value>>) {
    if let Some(v) = v {
        m.insert(k.to_owned(), v.into());
    }
}

/// People of one Jellyfin `Type`, as Plex tags. Person ids go through the interner so
/// `/library/people/{id}/media` can find them again.
fn people(ctx: &Ctx, v: &Value, jf_type: &str, limit: usize) -> Vec<Value> {
    let Some(list) = v.get("People").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .filter(|p| s(p, "Type") == jf_type)
        .take(limit)
        .map(|p| {
            let pid = s(p, "Id");
            let mut t = Map::new();
            put(&mut t, "id", ctx.ids.person(pid));
            put(&mut t, "tag", s(p, "Name"));
            put(&mut t, "tagKey", pid);
            put_str(&mut t, "role", s(p, "Role"));
            if let Some(tag) = p.get("PrimaryImageTag").and_then(Value::as_str) {
                put(&mut t, "thumb", image_path(pid, "thumb", tag));
            }
            Value::Object(t)
        })
        .collect()
}

fn genres(ctx: &Ctx, v: &Value) -> Vec<Value> {
    let Some(list) = v.get("GenreItems").and_then(Value::as_array) else {
        // Older servers send only the names.
        return v.get("Genres").and_then(Value::as_array).map(|g| g.iter()
            .filter_map(Value::as_str)
            .map(|name| json!({ "id": ctx.ids.genre(name), "tag": name }))
            .collect()).unwrap_or_default();
    };
    list.iter().map(|g| json!({ "id": ctx.ids.genre(s(g, "Id")), "tag": s(g, "Name") })).collect()
}

fn chapters(v: &Value, duration_ms: i64) -> Vec<Value> {
    let Some(list) = v.get("Chapters").and_then(Value::as_array) else { return Vec::new() };
    let starts: Vec<i64> = list.iter().map(|c| i(c, "StartPositionTicks").unwrap_or(0) / TICKS_PER_MS).collect();
    list.iter().enumerate().map(|(n, c)| {
        let end = starts.get(n + 1).copied().unwrap_or(duration_ms);
        json!({ "index": n as i64 + 1, "startTimeOffset": starts[n], "endTimeOffset": end,
                "tag": s(c, "Name") })
    }).collect()
}

/// Plex `Metadata` for one Jellyfin item. `None` for a kind the app has no page for (music, books,
/// folders), so a listing drops it rather than showing a card that opens nothing.
pub fn item(ctx: &Ctx, v: &Value) -> Option<Value> {
    let kind = plex_kind(s(v, "Type"))?;
    let id = s(v, "Id");
    let mut m = Map::new();
    put(&mut m, "ratingKey", id);
    put(&mut m, "key", format!("/library/metadata/{id}"));
    put(&mut m, "guid", format!("jellyfin://{kind}/{id}"));
    put(&mut m, "type", kind);
    put(&mut m, "title", s(v, "Name"));
    put_str(&mut m, "titleSort", if s(v, "SortName").is_empty() { s(v, "Name") } else { s(v, "SortName") });
    put_str(&mut m, "summary", s(v, "Overview"));
    put_opt(&mut m, "year", i(v, "ProductionYear"));
    put_str(&mut m, "contentRating", s(v, "OfficialRating"));
    if let Some(st) = v.get("Studios").and_then(|x| x.get(0)) {
        put_str(&mut m, "studio", s(st, "Name"));
    }
    if let Some(t) = v.get("Taglines").and_then(|x| x.get(0)).and_then(Value::as_str) {
        put_str(&mut m, "tagline", t);
    }
    let premiere = s(v, "PremiereDate");
    put_str(&mut m, "originallyAvailableAt", &ymd(premiere));
    let added = epoch(s(v, "DateCreated")).unwrap_or(0);
    put(&mut m, "addedAt", added);
    put(&mut m, "updatedAt", added);
    put_opt(&mut m, "thumb", primary(v));
    put_opt(&mut m, "art", backdrop(v));
    if let Some(cr) = f(v, "CriticRating") {
        put(&mut m, "rating", cr.round() / 10.0);
    }
    if let Some(cr) = f(v, "CommunityRating") {
        put(&mut m, "audienceRating", (cr * 10.0).round() / 10.0);
    }
    let duration = i(v, "RunTimeTicks").map(|t| t / TICKS_PER_MS);
    put_opt(&mut m, "duration", duration);
    if let Some((sid, title)) = ctx.section_for(kind) {
        put(&mut m, "librarySectionID", sid);
        put(&mut m, "librarySectionTitle", title);
        put(&mut m, "librarySectionKey", format!("/library/sections/{sid}"));
    }

    // Watch state.
    let ud = v.get("UserData").cloned().unwrap_or(Value::Null);
    if matches!(kind, "movie" | "episode") {
        if b(&ud, "Played") {
            put(&mut m, "viewCount", i(&ud, "PlayCount").unwrap_or(1).max(1));
        }
        let pos = i(&ud, "PlaybackPositionTicks").unwrap_or(0) / TICKS_PER_MS;
        if pos > 0 {
            put(&mut m, "viewOffset", pos);
        }
    }
    put_opt(&mut m, "lastViewedAt", epoch(s(&ud, "LastPlayedDate")));

    match kind {
        "show" => {
            let leaves = i(v, "RecursiveItemCount").unwrap_or(0);
            let unplayed = i(&ud, "UnplayedItemCount").unwrap_or(if b(&ud, "Played") { 0 } else { leaves });
            put(&mut m, "childCount", i(v, "ChildCount").unwrap_or(0));
            put(&mut m, "leafCount", leaves);
            put(&mut m, "viewedLeafCount", (leaves - unplayed).max(0));
        }
        "season" => {
            put_opt(&mut m, "index", i(v, "IndexNumber"));
            put(&mut m, "parentRatingKey", s(v, "SeriesId"));
            put(&mut m, "parentKey", format!("/library/metadata/{}", s(v, "SeriesId")));
            put_str(&mut m, "parentTitle", s(v, "SeriesName"));
            put_opt(&mut m, "parentThumb", series_thumb(v));
            let leaves = i(v, "ChildCount").or_else(|| i(v, "RecursiveItemCount")).unwrap_or(0);
            let unplayed = i(&ud, "UnplayedItemCount").unwrap_or(if b(&ud, "Played") { 0 } else { leaves });
            put(&mut m, "leafCount", leaves);
            put(&mut m, "viewedLeafCount", (leaves - unplayed).max(0));
            if !m.contains_key("thumb") {
                put_opt(&mut m, "thumb", series_thumb(v));
            }
        }
        "episode" => {
            put_opt(&mut m, "index", i(v, "IndexNumber"));
            put_opt(&mut m, "parentIndex", i(v, "ParentIndexNumber"));
            put(&mut m, "parentRatingKey", s(v, "SeasonId"));
            put(&mut m, "parentKey", format!("/library/metadata/{}", s(v, "SeasonId")));
            put_str(&mut m, "parentTitle", s(v, "SeasonName"));
            put(&mut m, "grandparentRatingKey", s(v, "SeriesId"));
            put(&mut m, "grandparentKey", format!("/library/metadata/{}", s(v, "SeriesId")));
            put_str(&mut m, "grandparentTitle", s(v, "SeriesName"));
            put_opt(&mut m, "grandparentThumb", series_thumb(v));
            if let Some(art) = backdrop(v) {
                put(&mut m, "grandparentArt", art);
            }
        }
        _ => {}
    }

    let g = genres(ctx, v);
    if !g.is_empty() { put(&mut m, "Genre", g); }
    for (plex, jf, n) in [("Director", "Director", 4), ("Writer", "Writer", 4), ("Role", "Actor", 30)] {
        let p = people(ctx, v, jf, n);
        if !p.is_empty() { put(&mut m, plex, p); }
    }
    if let Some(c) = v.get("ProductionLocations").and_then(Value::as_array) {
        let tags: Vec<Value> = c.iter().filter_map(Value::as_str).map(|t| json!({ "tag": t })).collect();
        if !tags.is_empty() { put(&mut m, "Country", tags); }
    }
    let ch = chapters(v, duration.unwrap_or(0));
    if !ch.is_empty() { put(&mut m, "Chapter", ch); }

    if let Some(sources) = v.get("MediaSources").and_then(Value::as_array) {
        let media: Vec<Value> = sources.iter().map(|src| media_of(ctx, id, src, duration)).collect();
        if !media.is_empty() { put(&mut m, "Media", media); }
    }
    Some(Value::Object(m))
}

/// Plex codec spellings for Jellyfin's (which are ffmpeg's): the app compares these against the
/// television's codec table and its own decoder list.
pub fn codec(jf: &str) -> String {
    match jf.to_ascii_lowercase().as_str() {
        "subrip" => "srt".into(),
        "webvtt" => "vtt".into(),
        "hdmv_pgs_subtitle" | "pgssub" => "pgs".into(),
        "dvd_subtitle" | "dvdsub" => "vobsub".into(),
        "mov_text" => "mov_text".into(),
        "h265" => "hevc".into(),
        "dca" => "dts".into(),
        "mpeg2video" => "mpeg2video".into(),
        other => other.into(),
    }
}

fn container_of(src: &Value) -> String {
    // Jellyfin reports what ffprobe names the demuxer: `mov,mp4,m4a,3gp,3g2,mj2` or `matroska,webm`.
    let raw = s(src, "Container").to_ascii_lowercase();
    let first = raw.split(',').next().unwrap_or("");
    match first {
        "matroska" | "webm" => "mkv".into(),
        "mov" | "mp4" | "m4v" => "mp4".into(),
        "mpegts" => "ts".into(),
        "" => "mkv".into(),
        other => other.into(),
    }
}

fn resolution(w: i64, h: i64) -> &'static str {
    if w >= 3200 || h >= 1800 { "4k" }
    else if w >= 1700 || h >= 1000 { "1080" }
    else if w >= 1200 || h >= 700 { "720" }
    else if h >= 560 { "576" }
    else if h >= 460 { "480" }
    else { "sd" }
}

/// One Jellyfin `MediaSource` as a Plex `Media` with its single `Part`.
pub fn media_of(ctx: &Ctx, item: &str, src: &Value, duration: Option<i64>) -> Value {
    let source = s(src, "Id");
    let container = container_of(src);
    let part_id = ctx.ids.part(item, source, &container);
    let streams_src = src.get("MediaStreams").and_then(Value::as_array).cloned().unwrap_or_default();
    let default_audio = i(src, "DefaultAudioStreamIndex");
    let default_sub = i(src, "DefaultSubtitleStreamIndex");
    let video = streams_src.iter().find(|x| s(x, "Type") == "Video");
    let audio = streams_src.iter()
        .find(|x| s(x, "Type") == "Audio" && Some(i(x, "Index").unwrap_or(-1)) == default_audio)
        .or_else(|| streams_src.iter().find(|x| s(x, "Type") == "Audio"));
    let (w, h) = video.map(|x| (i(x, "Width").unwrap_or(0), i(x, "Height").unwrap_or(0))).unwrap_or((0, 0));

    let streams: Vec<Value> = streams_src.iter()
        .filter_map(|x| stream_of(ctx, item, source, x, default_audio, default_sub))
        .collect();

    let mut m = Map::new();
    put(&mut m, "id", crate::ids::fold(&format!("{source}#media")));
    put_opt(&mut m, "duration", i(src, "RunTimeTicks").map(|t| t / TICKS_PER_MS).or(duration));
    put_opt(&mut m, "bitrate", i(src, "Bitrate").map(|b| b / 1000));
    if w > 0 {
        put(&mut m, "width", w);
        put(&mut m, "height", h);
        put(&mut m, "aspectRatio", ((w as f64 / h.max(1) as f64) * 100.0).round() / 100.0);
        put(&mut m, "videoResolution", resolution(w, h));
    }
    if let Some(v) = video {
        put(&mut m, "videoCodec", codec(s(v, "Codec")));
        put_str(&mut m, "videoProfile", &s(v, "Profile").to_ascii_lowercase());
        if let Some(fr) = f(v, "RealFrameRate").or_else(|| f(v, "AverageFrameRate")) {
            put(&mut m, "videoFrameRate", frame_rate_label(fr));
        }
    }
    if let Some(a) = audio {
        put(&mut m, "audioCodec", codec(s(a, "Codec")));
        put_opt(&mut m, "audioChannels", i(a, "Channels"));
    }
    put(&mut m, "container", container.clone());

    let mut part = Map::new();
    put(&mut part, "id", part_id);
    put(&mut part, "key", format!("/library/parts/{part_id}/0/file.{container}"));
    put_opt(&mut part, "duration", i(src, "RunTimeTicks").map(|t| t / TICKS_PER_MS).or(duration));
    put_str(&mut part, "file", s(src, "Path"));
    put_opt(&mut part, "size", i(src, "Size"));
    put(&mut part, "container", container);
    put(&mut part, "Stream", streams);
    put(&mut m, "Part", vec![Value::Object(part)]);
    Value::Object(m)
}

fn frame_rate_label(fr: f64) -> String {
    match fr.round() as i64 {
        23 | 24 => "24p".into(),
        25 => "PAL".into(),
        29 | 30 => "NTSC".into(),
        50 => "50p".into(),
        59 | 60 => "60p".into(),
        n => format!("{n}p"),
    }
}

/// Three-letter ISO 639-2 code to two-letter BCP-47 tag, for the common languages (the app's track
/// memory and default-language rules match on either).
fn lang_tag(code: &str) -> &str {
    match code {
        "eng" => "en", "spa" => "es", "cat" => "ca", "fre" | "fra" => "fr", "ger" | "deu" => "de",
        "ita" => "it", "por" => "pt", "jpn" => "ja", "kor" => "ko", "chi" | "zho" => "zh",
        "rus" => "ru", "dut" | "nld" => "nl", "pol" => "pl", "swe" => "sv", "nor" => "no",
        "dan" => "da", "fin" => "fi", "ara" => "ar", "tur" => "tr", "heb" => "he", "hin" => "hi",
        "eus" | "baq" => "eu", "glg" => "gl", "ukr" => "uk", "bel" => "be", "ces" | "cze" => "cs",
        "ell" | "gre" => "el", "hun" => "hu", "ron" | "rum" => "ro", "tha" => "th", "vie" => "vi",
        other => other,
    }
}

/// One Jellyfin `MediaStream` as a Plex `Stream`; `None` for attachments and cover art.
pub fn stream_of(ctx: &Ctx, item: &str, source: &str, x: &Value,
    default_audio: Option<i64>, default_sub: Option<i64>) -> Option<Value> {
    let kind = match s(x, "Type") {
        "Video" => 1,
        "Audio" => 2,
        "Subtitle" => 3,
        _ => return None,
    };
    let index = i(x, "Index").unwrap_or(0);
    let external = b(x, "IsExternal");
    let c = codec(s(x, "Codec"));
    let id = ctx.ids.stream(item, source, index, &c, external);
    let mut m = Map::new();
    put(&mut m, "id", id);
    put(&mut m, "streamType", kind);
    put(&mut m, "index", index);
    put(&mut m, "codec", c);
    let lang = s(x, "Language");
    if !lang.is_empty() && lang != "und" {
        put(&mut m, "languageCode", lang);
        put(&mut m, "languageTag", lang_tag(lang));
        put_str(&mut m, "language", s(x, "Language"));
    }
    put_str(&mut m, "displayTitle", s(x, "DisplayTitle"));
    put_str(&mut m, "extendedDisplayTitle", s(x, "DisplayTitle"));
    put_str(&mut m, "title", s(x, "Title"));
    put_opt(&mut m, "bitrate", i(x, "BitRate").map(|b| b / 1000));
    if b(x, "IsDefault") { put(&mut m, "default", true); }
    if b(x, "IsForced") { put(&mut m, "forced", true); }
    if b(x, "IsHearingImpaired") { put(&mut m, "hearingImpaired", true); }
    match kind {
        1 => {
            put_opt(&mut m, "width", i(x, "Width"));
            put_opt(&mut m, "height", i(x, "Height"));
            put_opt(&mut m, "bitDepth", i(x, "BitDepth"));
            put_opt(&mut m, "frameRate", f(x, "RealFrameRate").or_else(|| f(x, "AverageFrameRate")));
            put_str(&mut m, "profile", &s(x, "Profile").to_ascii_lowercase());
            put_opt(&mut m, "level", i(x, "Level"));
            put_str(&mut m, "colorTrc", s(x, "ColorTransfer"));
            put_str(&mut m, "colorPrimaries", s(x, "ColorPrimaries"));
            put_str(&mut m, "colorSpace", s(x, "ColorSpace"));
            if let Some(cs) = chroma(s(x, "PixelFormat")) { put(&mut m, "chromaSubsampling", cs); }
            if let Some(p) = i(x, "DvProfile").filter(|p| *p > 0) {
                put(&mut m, "DOVIPresent", true);
                put(&mut m, "DOVIProfile", p);
                put_opt(&mut m, "DOVILevel", i(x, "DvLevel"));
                put_opt(&mut m, "DOVIBLCompatID", i(x, "DvBlSignalCompatibilityId"));
                put(&mut m, "DOVIBLPresent", i(x, "BlPresentFlag").unwrap_or(1) == 1);
                put(&mut m, "DOVIELPresent", i(x, "ElPresentFlag").unwrap_or(0) == 1);
                put(&mut m, "DOVIRPUPresent", i(x, "RpuPresentFlag").unwrap_or(1) == 1);
                put(&mut m, "DOVIVersion", format!("{}.{}",
                    i(x, "DvVersionMajor").unwrap_or(1), i(x, "DvVersionMinor").unwrap_or(0)));
            }
        }
        2 => {
            put_opt(&mut m, "channels", i(x, "Channels"));
            put_str(&mut m, "audioChannelLayout", s(x, "ChannelLayout"));
            put_opt(&mut m, "samplingRate", i(x, "SampleRate"));
            put_str(&mut m, "profile", &s(x, "Profile").to_ascii_lowercase());
            if Some(index) == default_audio { put(&mut m, "selected", true); }
        }
        _ => {
            if Some(index) == default_sub { put(&mut m, "selected", true); }
            if external {
                put(&mut m, "key", format!("/library/streams/{id}"));
                put(&mut m, "external", true);
            }
        }
    }
    Some(Value::Object(m))
}

fn chroma(pix: &str) -> Option<&'static str> {
    if pix.contains("420") { Some("4:2:0") }
    else if pix.contains("422") { Some("4:2:2") }
    else if pix.contains("444") { Some("4:4:4") }
    else { None }
}

/// Jellyfin media segments (10.10+: `/MediaSegments/{id}`) as Plex markers (Skip Intro / Credits).
pub fn markers(segments: &Value) -> Vec<Value> {
    let Some(list) = segments.get("Items").and_then(Value::as_array) else { return Vec::new() };
    let mut out: Vec<Value> = list.iter().filter_map(|x| {
        let kind = match s(x, "Type") {
            "Intro" | "Recap" => "intro",
            "Outro" => "credits",
            "Commercial" => "commercial",
            _ => return None,
        };
        let start = i(x, "StartTicks")? / TICKS_PER_MS;
        let end = i(x, "EndTicks")? / TICKS_PER_MS;
        Some(json!({ "type": kind, "startTimeOffset": start, "endTimeOffset": end }))
    }).collect();
    // Plex marks the LAST credits marker `final`: Up Next keys off it.
    if let Some(last) = out.iter_mut().rev().find(|m| m["type"] == "credits") {
        last["final"] = Value::Bool(true);
    }
    out
}

/// A Jellyfin user view (`CollectionType` `movies` / `tvshows`) as a Plex library section.
pub fn section(ctx: &Ctx, v: &Value) -> Option<Value> {
    let kind = match s(v, "CollectionType") {
        "movies" | "homevideos" | "musicvideos" => "movie",
        "tvshows" => "show",
        _ => return None,
    };
    let id = s(v, "Id");
    let key = ctx.ids.section(id, s(v, "CollectionType"));
    let mut m = Map::new();
    put(&mut m, "key", key.to_string());
    put(&mut m, "type", kind);
    put(&mut m, "title", s(v, "Name"));
    put(&mut m, "uuid", id);
    put(&mut m, "agent", if kind == "movie" { "tv.plex.agents.movie" } else { "tv.plex.agents.series" });
    put(&mut m, "scanner", if kind == "movie" { "Plex Movie" } else { "Plex TV Series" });
    put(&mut m, "language", "en-US");
    put_opt(&mut m, "thumb", primary(v));
    put_opt(&mut m, "art", primary(v));
    put(&mut m, "refreshing", false);
    put(&mut m, "allowSync", false);
    Some(Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{Interned, Interner};

    fn ctx(ids: &Interner) -> Ctx<'_> {
        Ctx { ids, section: None, movies: Some((11, "Pelis".into())), shows: Some((22, "Series".into())) }
    }

    const MOVIE: &str = r#"{
      "Name":"Blade Runner 2049","SortName":"blade runner 2049","Id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "Type":"Movie","ProductionYear":2017,"PremiereDate":"2017-10-04T00:00:00.0000000Z",
      "DateCreated":"2024-01-02T03:04:05.1234567Z","OfficialRating":"R","CriticRating":88,
      "CommunityRating":7.95,"RunTimeTicks":98400000000,"Overview":"K.",
      "Taglines":["More human"],"Studios":[{"Name":"Alcon","Id":"s1"}],
      "GenreItems":[{"Name":"Sci-Fi","Id":"g1"}],
      "People":[{"Name":"Ryan Gosling","Id":"p1","Role":"K","Type":"Actor","PrimaryImageTag":"pt"},
                {"Name":"Denis Villeneuve","Id":"p2","Type":"Director"}],
      "ImageTags":{"Primary":"tagP"},"BackdropImageTags":["tagB"],
      "UserData":{"PlaybackPositionTicks":6000000000,"PlayCount":0,"Played":false,
                  "LastPlayedDate":"2024-05-01T00:00:00Z"},
      "Chapters":[{"StartPositionTicks":0,"Name":"Start"},{"StartPositionTicks":3000000000,"Name":"Two"}],
      "MediaSources":[{"Id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","Container":"matroska,webm","Size":60000000000,
        "Path":"/media/br.mkv","Bitrate":45000000,"RunTimeTicks":98400000000,
        "DefaultAudioStreamIndex":1,"DefaultSubtitleStreamIndex":-1,
        "MediaStreams":[
          {"Type":"Video","Index":0,"Codec":"hevc","Width":3840,"Height":2160,"BitDepth":10,
           "Profile":"Main 10","RealFrameRate":23.976,"VideoRangeType":"DOVIWithHDR10","DvProfile":8,
           "DvLevel":6,"DvBlSignalCompatibilityId":1,"RpuPresentFlag":1,"ElPresentFlag":0,"BlPresentFlag":1,
           "DvVersionMajor":1,"DvVersionMinor":0,"ColorTransfer":"smpte2084","PixelFormat":"yuv420p10le",
           "DisplayTitle":"4K HEVC Dolby Vision"},
          {"Type":"Audio","Index":1,"Codec":"eac3","Channels":6,"Language":"spa","IsDefault":true,
           "DisplayTitle":"Spanish - Dolby Digital+ - 5.1"},
          {"Type":"Audio","Index":2,"Codec":"truehd","Channels":8,"Language":"eng"},
          {"Type":"Subtitle","Index":3,"Codec":"subrip","Language":"spa","IsForced":true},
          {"Type":"Subtitle","Index":4,"Codec":"subrip","Language":"eng","IsExternal":true},
          {"Type":"Attachment","Index":5,"Codec":"ttf"}
        ]}]
    }"#;

    #[test]
    fn a_movie_maps_to_the_fields_the_app_reads() {
        let ids = Interner::new();
        let v: Value = serde_json::from_str(MOVIE).unwrap();
        let m = item(&ctx(&ids), &v).unwrap();
        assert_eq!(m["ratingKey"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(m["type"], "movie");
        assert_eq!(m["year"], 2017);
        assert_eq!(m["duration"], 9_840_000);
        assert_eq!(m["viewOffset"], 600_000);
        assert!(m.get("viewCount").is_none(), "unwatched has no viewCount");
        assert_eq!(m["rating"], 8.8);
        assert_eq!(m["audienceRating"], 8.0);
        assert_eq!(m["librarySectionID"], 11);
        assert_eq!(m["thumb"], "/library/metadata/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/thumb/tagP");
        assert_eq!(m["art"], "/library/metadata/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/art/tagB");
        assert_eq!(m["originallyAvailableAt"], "2017-10-04");
        assert_eq!(m["tagline"], "More human");
        assert_eq!(m["Role"][0]["tag"], "Ryan Gosling");
        assert_eq!(m["Role"][0]["role"], "K");
        assert_eq!(m["Director"][0]["tag"], "Denis Villeneuve");
        assert_eq!(m["Chapter"][1]["startTimeOffset"], 300_000);
        assert_eq!(m["Chapter"][1]["endTimeOffset"], 9_840_000);
        let person = m["Role"][0]["id"].as_i64().unwrap();
        assert_eq!(ids.get(person), Some(Interned::Person { id: "p1".into() }));
    }

    #[test]
    fn media_streams_carry_codec_language_selection_and_dolby_vision() {
        let ids = Interner::new();
        let v: Value = serde_json::from_str(MOVIE).unwrap();
        let m = item(&ctx(&ids), &v).unwrap();
        let media = &m["Media"][0];
        assert_eq!(media["container"], "mkv");
        assert_eq!(media["videoCodec"], "hevc");
        assert_eq!(media["videoResolution"], "4k");
        assert_eq!(media["audioCodec"], "eac3");
        assert_eq!(media["videoFrameRate"], "24p");
        let part = &media["Part"][0];
        let pid = part["id"].as_i64().unwrap();
        assert_eq!(part["key"], format!("/library/parts/{pid}/0/file.mkv"));
        assert!(matches!(ids.get(pid), Some(Interned::Part { ref source, .. }) if source == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        let st = part["Stream"].as_array().unwrap();
        assert_eq!(st.len(), 5, "the attachment is dropped");
        assert_eq!(st[0]["DOVIProfile"], 8);
        assert_eq!(st[0]["DOVIBLCompatID"], 1);
        assert_eq!(st[0]["chromaSubsampling"], "4:2:0");
        assert_eq!(st[1]["selected"], true);
        assert_eq!(st[1]["languageTag"], "es");
        assert!(st[2].get("selected").is_none());
        assert_eq!(st[3]["codec"], "srt");
        assert_eq!(st[3]["forced"], true);
        assert_eq!(st[4]["external"], true);
        let sid = st[4]["id"].as_i64().unwrap();
        assert_eq!(st[4]["key"], format!("/library/streams/{sid}"));
    }

    #[test]
    fn an_episode_names_its_season_and_show() {
        let ids = Interner::new();
        let v = json!({"Name":"Pilot","Id":"e1","Type":"Episode","IndexNumber":1,"ParentIndexNumber":2,
            "SeasonId":"s1","SeasonName":"Season 2","SeriesId":"sh1","SeriesName":"Show",
            "SeriesPrimaryImageTag":"st","ParentBackdropItemId":"sh1","ParentBackdropImageTags":["bt"],
            "UserData":{"Played":true,"PlayCount":2}});
        let m = item(&ctx(&ids), &v).unwrap();
        assert_eq!(m["index"], 1);
        assert_eq!(m["parentIndex"], 2);
        assert_eq!(m["grandparentRatingKey"], "sh1");
        assert_eq!(m["grandparentThumb"], "/library/metadata/sh1/thumb/st");
        assert_eq!(m["art"], "/library/metadata/sh1/art/bt");
        assert_eq!(m["viewCount"], 2);
        assert_eq!(m["librarySectionID"], 22);
    }

    #[test]
    fn a_show_counts_its_watched_episodes() {
        let ids = Interner::new();
        let v = json!({"Name":"S","Id":"sh","Type":"Series","ChildCount":3,"RecursiveItemCount":30,
            "UserData":{"UnplayedItemCount":12}});
        let m = item(&ctx(&ids), &v).unwrap();
        assert_eq!((m["childCount"].as_i64(), m["leafCount"].as_i64(), m["viewedLeafCount"].as_i64()),
            (Some(3), Some(30), Some(18)));
    }

    #[test]
    fn music_and_folders_are_not_items() {
        let ids = Interner::new();
        assert!(item(&ctx(&ids), &json!({"Type":"MusicAlbum","Id":"x"})).is_none());
        assert!(item(&ctx(&ids), &json!({"Type":"Folder","Id":"x"})).is_none());
    }

    #[test]
    fn iso_instants_become_epoch_seconds() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2024-01-02T03:04:05.1234567Z"), Some(1_704_164_645));
        assert_eq!(epoch("2000-03-01"), Some(951_868_800));
        assert_eq!(epoch("garbage"), None);
    }

    #[test]
    fn segments_become_markers_with_the_last_credits_final() {
        let segs = json!({"Items":[{"Type":"Intro","StartTicks":300000000,"EndTicks":900000000},
            {"Type":"Outro","StartTicks":50_000_000_000_i64,"EndTicks":56_000_000_000_i64},
            {"Type":"Preview","StartTicks":1,"EndTicks":2}]});
        let m = markers(&segs);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["type"], "intro");
        assert_eq!(m[0]["startTimeOffset"], 30_000);
        assert_eq!(m[1]["final"], true);
    }

    #[test]
    fn only_movie_and_show_views_are_sections() {
        let ids = Interner::new();
        let c = ctx(&ids);
        assert!(section(&c, &json!({"Name":"Music","Id":"m","CollectionType":"music"})).is_none());
        let s = section(&c, &json!({"Name":"Series","Id":"v2","CollectionType":"tvshows"})).unwrap();
        assert_eq!(s["type"], "show");
        assert_eq!(ids.get(s["key"].as_str().unwrap().parse().unwrap()),
            Some(Interned::Section { id: "v2".into(), kind: "tvshows".into() }));
    }
}
