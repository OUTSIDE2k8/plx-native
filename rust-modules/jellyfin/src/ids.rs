//! **Identifier spaces.** Plex and Jellyfin disagree on what an identifier is, and the app keeps the
//! Plex shape everywhere: `ratingKey` is a string, but a library section, a media part, a stream and
//! a person tag are integers (`plex::models` reads them through `de_i64`).
//!
//! - A Jellyfin item id (32 hex digits) IS the `ratingKey`, verbatim: the app never parses one, and
//!   a verbatim id survives a restart with no table, which matters because the app remembers
//!   ratingKeys across boots (resume, track memory, the last library).
//! - Everything integer is a **48-bit fold of the Jellyfin id it stands for** ([`fold`]). Deterministic,
//!   so a section id the app persisted (favourite libraries, the last library opened) still names the
//!   same library after a reboot, and positive and well inside `i64`, so no reader's sign or width
//!   assumption can bite. 48 bits makes an accidental collision across one household's libraries,
//!   parts and people vanishingly unlikely, and a collision would only ever confuse two ids of the
//!   same kind.
//! - The way BACK from an integer is the [`Interner`]: every id the bridge hands out is recorded
//!   beside the Jellyfin id it folded, and a request naming an integer the bridge never handed out
//!   (it can only come from a previous run) is answered by the caller re-listing what it can fold
//!   (sections: the user's views) rather than by guessing.

use std::collections::HashMap;
use std::sync::Mutex;

/// The 48-bit, always-positive fold of a Jellyfin id (or any string): FNV-1a 64 of the lowercase
/// form, top 16 bits dropped, never zero (zero means "absent" to several Plex readers).
pub fn fold(id: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let v = (h & 0x0000_ffff_ffff_ffff) as i64;
    if v == 0 { 1 } else { v }
}

/// A stream's integer id: unique per (media source, stream index), so two items' first audio tracks
/// never share one, and stable across runs for the same file.
pub fn stream_id(media_source_id: &str, index: i64) -> i64 {
    fold(&format!("{media_source_id}#s{index}"))
}

/// What an integer the bridge handed out stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interned {
    /// A user view (Plex "library section"): its Jellyfin id and collection type.
    Section { id: String, kind: String },
    /// A media source of an item (Plex "part"): the item and the media source.
    Part { item: String, source: String, container: String },
    /// One stream of a media source.
    Stream { item: String, source: String, index: i64, codec: String, external: bool },
    /// A person (Plex role/director/writer tag).
    Person { id: String },
    /// A genre (Plex genre directory key).
    Genre { id: String },
}

/// The process-wide reverse table. Grows by one small entry per distinct id ever served; a
/// household library is thousands of rows, not millions, and the table dies with the process.
#[derive(Default)]
pub struct Interner {
    map: Mutex<HashMap<i64, Interned>>,
}

impl Interner {
    pub fn new() -> Self { Self::default() }

    /// Record `what` under its fold and return the integer.
    pub fn put(&self, key: i64, what: Interned) -> i64 {
        if let Ok(mut m) = self.map.lock() {
            m.insert(key, what);
        }
        key
    }

    pub fn get(&self, key: i64) -> Option<Interned> {
        self.map.lock().ok()?.get(&key).cloned()
    }

    pub fn section(&self, id: &str, kind: &str) -> i64 {
        self.put(fold(id), Interned::Section { id: id.to_owned(), kind: kind.to_owned() })
    }

    pub fn part(&self, item: &str, source: &str, container: &str) -> i64 {
        self.put(fold(source), Interned::Part {
            item: item.to_owned(), source: source.to_owned(), container: container.to_owned(),
        })
    }

    pub fn stream(&self, item: &str, source: &str, index: i64, codec: &str, external: bool) -> i64 {
        self.put(stream_id(source, index), Interned::Stream {
            item: item.to_owned(), source: source.to_owned(), index, codec: codec.to_owned(), external,
        })
    }

    pub fn person(&self, id: &str) -> i64 {
        self.put(fold(id), Interned::Person { id: id.to_owned() })
    }

    pub fn genre(&self, id: &str) -> i64 {
        self.put(fold(id), Interned::Genre { id: id.to_owned() })
    }
}

/// True for the shape of a Jellyfin item id (32 hex digits, or the dashed GUID form). The bridge
/// only forwards ids of this shape upstream, so a ratingKey can never smuggle a path segment.
pub fn is_item_id(s: &str) -> bool {
    s.len() <= 36
        && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        && s.bytes().filter(|b| *b != b'-').count() == 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fold_is_stable_positive_and_case_blind() {
        let a = fold("4E2B7F7C1A2B4C5D8E9F00112233AABB");
        assert_eq!(a, fold("4e2b7f7c1a2b4c5d8e9f00112233aabb"));
        assert!(a > 0 && a < (1 << 48));
        assert_ne!(a, fold("4e2b7f7c1a2b4c5d8e9f00112233aabc"));
    }

    #[test]
    fn stream_ids_differ_by_source_and_index() {
        assert_ne!(stream_id("a", 1), stream_id("a", 2));
        assert_ne!(stream_id("a", 1), stream_id("b", 1));
    }

    #[test]
    fn the_interner_answers_what_it_handed_out_and_nothing_else() {
        let t = Interner::new();
        let s = t.section("f137a2dd21bbc1b99aa5c0f6bf02a805", "movies");
        assert_eq!(t.get(s), Some(Interned::Section { id: "f137a2dd21bbc1b99aa5c0f6bf02a805".into(),
            kind: "movies".into() }));
        assert_eq!(t.get(s + 1), None);
    }

    #[test]
    fn only_hex_ids_pass() {
        assert!(is_item_id("f137a2dd21bbc1b99aa5c0f6bf02a805"));
        assert!(is_item_id("f137a2dd-21bb-c1b9-9aa5-c0f6bf02a805"));
        assert!(!is_item_id("../../System/Info"));
        assert!(!is_item_id("1234"));
        assert!(!is_item_id("f137a2dd21bbc1b99aa5c0f6bf02a80z"));
    }
}
