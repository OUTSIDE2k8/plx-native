//! The router against a scripted Jellyfin: what each PMS route answers, and what it asked the
//! server for on the way.

use std::sync::Arc;

use serde_json::Value;

use super::*;
use crate::upstream::fake::Fake;
use crate::upstream::ClientIdentity;

const MOVIE_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SOURCE_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHOW_ID: &str = "cccccccccccccccccccccccccccccccc";
const EP1: &str = "dddddddddddddddddddddddddddddd01";
const EP2: &str = "dddddddddddddddddddddddddddddd02";
const MOVIES_VIEW: &str = "11111111111111111111111111111111";
const SHOWS_VIEW: &str = "22222222222222222222222222222222";

fn views() -> String {
    format!(r#"{{"Items":[
      {{"Name":"Pelis","Id":"{MOVIES_VIEW}","CollectionType":"movies"}},
      {{"Name":"Series","Id":"{SHOWS_VIEW}","CollectionType":"tvshows"}},
      {{"Name":"Música","Id":"33333333333333333333333333333333","CollectionType":"music"}}]}}"#)
}

fn movie() -> String {
    format!(r#"{{"Name":"Dune","Id":"{MOVIE_ID}","Type":"Movie","RunTimeTicks":93000000000,
      "UserData":{{"PlaybackPositionTicks":0,"Played":false}},
      "MediaSources":[{{"Id":"{SOURCE_ID}","Container":"matroska,webm","DefaultAudioStreamIndex":1,
        "MediaStreams":[{{"Type":"Video","Index":0,"Codec":"hevc","Width":3840,"Height":2160}},
                        {{"Type":"Audio","Index":1,"Codec":"truehd","Channels":8,"IsDefault":true}},
                        {{"Type":"Audio","Index":2,"Codec":"eac3","Channels":6}},
                        {{"Type":"Subtitle","Index":3,"Codec":"subrip","IsExternal":true}}]}}]}}"#)
}

fn episodes() -> String {
    format!(r#"{{"Items":[
      {{"Name":"One","Id":"{EP1}","Type":"Episode","SeriesId":"{SHOW_ID}","IndexNumber":1,"ParentIndexNumber":1}},
      {{"Name":"Two","Id":"{EP2}","Type":"Episode","SeriesId":"{SHOW_ID}","IndexNumber":2,"ParentIndexNumber":1}}]}}"#)
}

fn fake() -> Fake {
    Fake::default()
        .on("GET", "/UserViews", 200, &views())
        .on("GET", "/Items?userId=u&Ids=", 200, &format!(r#"{{"Items":[{}]}}"#, movie()))
        .on("GET", "/Items?userId=u&ParentId=", 200,
            &format!(r#"{{"TotalRecordCount":120,"Items":[{}]}}"#, movie()))
        .on("GET", "/UserItems/Resume", 200, &format!(r#"{{"Items":[{}]}}"#, movie()))
        .on("GET", "/Shows/NextUp", 200, &episodes())
        .on("GET", &format!("/Shows/{SHOW_ID}/Episodes"), 200, &episodes())
        .on_bytes("GET", "/Items/", 200, b"\xff\xd8\xff\xe0jpeg")
        .on("POST", "/Sessions/Playing", 204, "")
        .on("POST", "/UserPlayedItems/", 200, "{}")
        .on("POST", "/UserItems/", 200, "{}")
        .on("GET", &format!("/Videos/{MOVIE_ID}/{SOURCE_ID}/Subtitles/3/0/Stream.srt"), 200, "1\n00:00:01,000 --> 00:00:02,000\nhola\n")
}

fn bridge_with(f: Fake) -> (Bridge, Arc<Fake>) {
    let f = Arc::new(f);
    let up = Upstream::new("http://jf:8096", "TOK", "u",
        ClientIdentity { device_id: "dev".into(), device_name: "tv".into(), version: "0".into() },
        Box::new(f.clone()));
    (Bridge::new(up, "srv", "Casa", "L"), f)
}

fn get(b: &Bridge, target: &str) -> Answer {
    let sep = if target.contains('?') { '&' } else { '?' };
    b.handle("GET", &format!("{target}{sep}X-Plex-Token=L"), &[])
}

fn json(a: Answer) -> Value {
    match a {
        Answer::Json(200, v) => v["MediaContainer"].clone(),
        other => panic!("expected JSON 200, got {other:?}"),
    }
}

fn seen(f: &Fake) -> Vec<(String, String)> {
    f.seen.lock().unwrap().iter().map(|(m, p, _)| (m.clone(), p.clone())).collect()
}

#[test]
fn every_route_but_identity_wants_the_local_token() {
    let (b, _) = bridge_with(fake());
    assert!(matches!(b.handle("GET", "/library/sections", &[]), Answer::Json(401, _)));
    assert!(matches!(b.handle("GET", "/library/sections?X-Plex-Token=wrong", &[]), Answer::Json(401, _)));
    let hdr = [("X-Plex-Token".to_owned(), "L".to_owned())];
    assert!(matches!(b.handle("GET", "/library/sections", &hdr), Answer::Json(200, _)));
    let id = match b.handle("GET", "/identity", &[]) { Answer::Json(200, v) => v, o => panic!("{o:?}") };
    assert_eq!(id["MediaContainer"]["machineIdentifier"], "jellyfin-srv");
}

#[test]
fn sections_are_the_movie_and_show_views() {
    let (b, _) = bridge_with(fake());
    let mc = json(get(&b, "/library/sections"));
    let dirs = mc["Directory"].as_array().unwrap();
    assert_eq!(dirs.len(), 2, "music is left out");
    assert_eq!(dirs[0]["type"], "movie");
    assert_eq!(dirs[1]["type"], "show");
    assert_eq!(dirs[0]["key"], crate::ids::fold(MOVIES_VIEW).to_string());
}

#[test]
fn a_section_listing_pages_sorts_and_filters_upstream() {
    let (b, f) = bridge_with(fake());
    let sid = crate::ids::fold(MOVIES_VIEW);
    let mc = json(get(&b, &format!(
        "/library/sections/{sid}/all?type=1&sort=addedAt:desc&unwatched=1&X-Plex-Container-Start=40&X-Plex-Container-Size=20&includeMeta=1")));
    assert_eq!(mc["totalSize"], 120);
    assert_eq!(mc["offset"], 40);
    assert_eq!(mc["Metadata"][0]["librarySectionID"], sid);
    assert!(mc["Meta"]["Type"][0]["Sort"].as_array().unwrap().len() > 3);
    let (_, q) = seen(&f).into_iter().find(|(_, p)| p.contains("ParentId=")).unwrap();
    for want in [&format!("ParentId={MOVIES_VIEW}"), "IncludeItemTypes=Movie", "SortBy=DateCreated",
        "SortOrder=Descending", "Filters=IsUnplayed", "StartIndex=40", "Limit=20"] {
        assert!(q.contains(want), "{want} missing from {q}");
    }
}

#[test]
fn a_section_id_from_an_earlier_run_is_found_again() {
    let (b, _) = bridge_with(fake());
    // Nothing has listed sections in this process, so the interner has never seen the id.
    let sid = crate::ids::fold(SHOWS_VIEW);
    let mc = json(get(&b, &format!("/library/sections/{sid}/all")));
    assert_eq!(mc["Metadata"].as_array().unwrap().len(), 1);
}

#[test]
fn metadata_carries_media_and_the_next_episode_on_deck() {
    let (b, _) = bridge_with(fake());
    let mc = json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let m = &mc["Metadata"][0];
    assert_eq!(m["title"], "Dune");
    assert_eq!(m["Media"][0]["Part"][0]["Stream"].as_array().unwrap().len(), 4);
    assert!(matches!(get(&b, "/library/metadata/not-an-id"), Answer::Json(404, _)));
}

#[test]
fn a_part_redirects_to_the_static_stream_with_the_token_as_api_key() {
    let (b, _) = bridge_with(fake());
    let mc = json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let key = mc["Metadata"][0]["Media"][0]["Part"][0]["key"].as_str().unwrap().to_owned();
    match get(&b, &key) {
        Answer::Redirect(url) => {
            assert!(url.starts_with(&format!("http://jf:8096/Videos/{MOVIE_ID}/stream.mkv?static=true&mediaSourceId={SOURCE_ID}")), "{url}");
            assert!(url.ends_with("api_key=TOK"), "{url}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(get(&b, "/library/parts/999/0/file.mkv"), Answer::Bytes(404, ..)));
}

#[test]
fn the_direct_play_decision_copies_every_stream() {
    let (b, _) = bridge_with(fake());
    let mc = json(get(&b, &format!(
        "/video/:/transcode/universal/decision?path=/library/metadata/{MOVIE_ID}&directPlay=1&hasMDE=1")));
    assert_eq!(mc["generalDecisionCode"], 1000);
    let part = &mc["Metadata"][0]["Media"][0]["Part"][0];
    assert_eq!(part["decision"], "directplay");
    assert!(part["Stream"].as_array().unwrap().iter().all(|s| s["decision"] == "copy"));
}

#[test]
fn a_remux_copies_video_and_converts_audio_the_profile_cannot_take() {
    let (b, _) = bridge_with(fake());
    let extra = enc("add-transcode-target(type=videoProfile&container=mkv&videoCodec=hevc,h264&audioCodec=eac3,ac3)+add-direct-play-profile(type=videoProfile&container=mkv,mp4&videoCodec=h264,hevc&audioCodec=aac,ac3,eac3)");
    let mc = json(get(&b, &format!(
        "/video/:/transcode/universal/decision?path=/library/metadata/{MOVIE_ID}&directPlay=0&directStream=1&directStreamAudio=1&X-Plex-Client-Profile-Extra={extra}")));
    let st = mc["Metadata"][0]["Media"][0]["Part"][0]["Stream"].as_array().unwrap().clone();
    assert_eq!(mc["Metadata"][0]["Media"][0]["Part"][0]["decision"], "transcode");
    assert_eq!(st[0]["decision"], "copy");
    assert_eq!(st[1]["decision"], "transcode", "TrueHD is not in the direct-play audio list");
    assert_eq!(st[1]["codec"], "eac3");
}

#[test]
fn a_transcode_start_redirects_with_the_promised_codecs_and_offset() {
    let (b, _) = bridge_with(fake());
    json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let audio = crate::ids::stream_id(SOURCE_ID, 2);
    let extra = enc("add-transcode-target(type=videoProfile&videoCodec=hevc,h264&audioCodec=ac3)");
    match get(&b, &format!("/video/:/transcode/universal/start.mkv?path=/library/metadata/{MOVIE_ID}&directStream=0&audioStreamID={audio}&offset=125.5&maxVideoBitrate=20000&videoResolution=1920x1080&X-Plex-Client-Profile-Extra={extra}")) {
        Answer::Redirect(url) => {
            for want in ["/stream.mkv?static=false", "videoCodec=hevc", "audioCodec=ac3",
                "allowVideoStreamCopy=false", "audioStreamIndex=2", "startTimeTicks=1255000000",
                "maxVideoBitrate=20000000", "maxWidth=1920", "subtitleStreamIndex=-1", "api_key=TOK"] {
                assert!(url.contains(want), "{want} missing from {url}");
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn timeline_reports_start_then_progress_then_stop() {
    let (b, f) = bridge_with(fake());
    let mc = json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let key = mc["Metadata"][0]["Media"][0]["Part"][0]["key"].as_str().unwrap().to_owned();
    let _ = get(&b, &key);
    for state in ["playing", "paused", "stopped"] {
        json(get(&b, &format!("/:/timeline?ratingKey={MOVIE_ID}&key=/library/metadata/{MOVIE_ID}&state={state}&time=60000")));
    }
    let posts: Vec<(String, Option<Vec<u8>>)> = f.seen.lock().unwrap().iter()
        .filter(|(m, ..)| m == "POST").map(|(_, p, b)| (p.clone(), b.clone())).collect();
    let paths: Vec<&str> = posts.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["/Sessions/Playing", "/Sessions/Playing/Progress", "/Sessions/Playing/Stopped"]);
    let body: Value = serde_json::from_slice(posts[1].1.as_ref().unwrap()).unwrap();
    assert_eq!(body["PositionTicks"], 600_000_000);
    assert_eq!(body["IsPaused"], true);
    assert_eq!(body["PlayMethod"], "DirectPlay");
    // After a stop the next play is a new session.
    json(get(&b, &format!("/:/timeline?ratingKey={MOVIE_ID}&state=playing&time=0")));
    assert_eq!(f.seen.lock().unwrap().iter().filter(|(m, ..)| m == "POST").count(), 3,
        "a report for an item no part was opened for is not sent");
}

#[test]
fn scrobble_marks_the_item_played() {
    let (b, f) = bridge_with(fake());
    json(get(&b, &format!("/:/scrobble?key={MOVIE_ID}&identifier=com.plexapp.plugins.library")));
    assert!(seen(&f).iter().any(|(m, p)| m == "POST" && p == &format!("/UserPlayedItems/{MOVIE_ID}?userId=u")));
}

#[test]
fn home_hubs_hold_continue_watching_on_deck_and_per_library_recents() {
    let (b, _) = bridge_with(fake());
    let mc = json(get(&b, "/hubs?count=12"));
    let ids: Vec<&str> = mc["Hub"].as_array().unwrap().iter()
        .map(|h| h["hubIdentifier"].as_str().unwrap()).collect();
    assert_eq!(ids, ["home.continue", "home.ondeck", "home.movies.recent", "home.television.recent"]);
}

#[test]
fn photo_transcode_fetches_the_sized_jellyfin_image() {
    let (b, f) = bridge_with(fake());
    let url = enc(&format!("/library/metadata/{MOVIE_ID}/thumb/tagP"));
    match get(&b, &format!("/photo/:/transcode?width=300&height=450&minSize=1&url={url}")) {
        Answer::Bytes(200, ct, body) => { assert_eq!(ct, "image/jpeg"); assert!(!body.is_empty()); }
        other => panic!("{other:?}"),
    }
    let (_, p) = seen(&f).into_iter().find(|(_, p)| p.starts_with("/Items/")).unwrap();
    assert_eq!(p, format!("/Items/{MOVIE_ID}/Images/Primary?quality=90&format=Jpg&tag=tagP&fillWidth=300&fillHeight=450"));
}

#[test]
fn an_external_subtitle_is_served_from_jellyfin() {
    let (b, _) = bridge_with(fake());
    json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let sid = crate::ids::stream_id(SOURCE_ID, 3);
    match get(&b, &format!("/library/streams/{sid}")) {
        Answer::Bytes(200, "application/x-subrip", body) => assert!(String::from_utf8_lossy(&body).contains("hola")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_track_choice_is_reflected_in_the_next_metadata_read() {
    let (b, _) = bridge_with(fake());
    let mc = json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let pid = mc["Metadata"][0]["Media"][0]["Part"][0]["id"].as_i64().unwrap();
    let eac3 = crate::ids::stream_id(SOURCE_ID, 2);
    let _ = b.handle("PUT", &format!("/library/parts/{pid}?audioStreamID={eac3}&X-Plex-Token=L"), &[]);
    let mc = json(get(&b, &format!("/library/metadata/{MOVIE_ID}")));
    let st = mc["Metadata"][0]["Media"][0]["Part"][0]["Stream"].as_array().unwrap().clone();
    assert!(st[1].get("selected").is_none());
    assert_eq!(st[2]["selected"], true);
}

#[test]
fn a_continuous_queue_follows_the_show_from_the_episode() {
    let (b, f) = bridge_with(fake()
        .on("GET", "/Items?userId=u&Ids=", 200, &format!(r#"{{"Items":[{{"Name":"One","Id":"{EP1}","Type":"Episode","SeriesId":"{SHOW_ID}"}}]}}"#)));
    let mc = match b.handle("POST", &format!("/playQueues?type=video&continuous=1&uri=server%3A%2F%2Fx%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F{EP1}&X-Plex-Token=L"), &[]) {
        Answer::Json(200, v) => v["MediaContainer"].clone(),
        o => panic!("{o:?}"),
    };
    assert_eq!(mc["playQueueTotalCount"], 2);
    assert_eq!(mc["Metadata"][1]["playQueueItemID"], 2);
    assert!(seen(&f).iter().any(|(_, p)| p.contains(&format!("StartItemId={EP1}"))));
    let again = json(get(&b, &format!("/playQueues/{}", mc["playQueueID"])));
    assert_eq!(again["Metadata"].as_array().unwrap().len(), 2);
}

#[test]
fn a_revoked_token_is_a_401_not_an_empty_library() {
    let (b, _) = bridge_with(Fake::default().on("GET", "/UserViews", 401, ""));
    assert!(matches!(get(&b, "/library/sections"), Answer::Json(401, _)));
}

#[test]
fn search_hubs_group_by_kind() {
    let (b, f) = bridge_with(fake().on("GET", "/Items?userId=u&searchTerm=", 200, &format!(r#"{{"Items":[{}]}}"#, movie())));
    let mc = json(get(&b, "/hubs/search?query=du%20ne&limit=5"));
    assert_eq!(mc["Hub"][0]["hubIdentifier"], "movie");
    assert!(seen(&f).iter().any(|(_, p)| p.contains("searchTerm=du%20ne")));
}

#[test]
fn the_a_to_z_rail_counts_letters_in_title_order() {
    let names = r#"{"Items":[{"SortName":"alien"},{"SortName":"amelie"},{"SortName":"brazil"},{"SortName":"10 things"}]}"#;
    let (b, _) = bridge_with(fake().on("GET", "/Items?userId=u&ParentId=", 200, names));
    let sid = crate::ids::fold(MOVIES_VIEW);
    let mc = json(get(&b, &format!("/library/sections/{sid}/firstCharacter")));
    let d: Vec<(String, i64)> = mc["Directory"].as_array().unwrap().iter()
        .map(|x| (x["title"].as_str().unwrap().to_owned(), x["size"].as_i64().unwrap())).collect();
    assert_eq!(d, [("A".to_owned(), 2), ("B".to_owned(), 1), ("#".to_owned(), 1)]);
}

/// A card on a shelf is playable straight from its row: the detail page plays the row it was
/// opened from (`PlayIntent::Movie`), whose part key is empty unless the LISTING carried media.
#[test]
fn list_rows_carry_the_part_a_play_needs() {
    let (b, f) = bridge_with(fake());
    let mc = json(get(&b, "/hubs/continueWatching"));
    let key = mc["Hub"][0]["Metadata"][0]["Media"][0]["Part"][0]["key"].as_str().unwrap_or("");
    assert!(key.starts_with("/library/parts/"), "row part key: {key:?}");
    let (_, q) = seen(&f).into_iter().find(|(_, p)| p.starts_with("/UserItems/Resume")).unwrap();
    assert!(q.contains("MediaSources"), "{q}");
}
