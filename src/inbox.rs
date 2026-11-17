//! Lists other programs hand to this one. For now one: Cadence's list of the albums he plays, likes or
//! keeps in playlists that the library does not have (`inbox\cadence-missing-albums.json` in the data
//! folder). Cadence offers "Send the list to Bradley's Music Tool" only when the `inbox` folder is there,
//! so making that folder at start is how this app says it reads such lists.
//!
//! The file is Cadence's: it is read, never written or removed. Each Send replaces it. Its words are only
//! ever shown and used as search words, never as a path. What this app remembers about the list (albums
//! forgotten with the X, a list put away, the last list seen, albums already searched for) is in its own
//! file, `inbox-state.json`, so a new Send does not bring back what was forgotten.
//!
//! An album leaves the list by itself once the library has it: the music folder is read in the background
//! when a list arrives, after an import ends, and every ten minutes while a list is on show.
use crate::library::Library;
use crate::util::{album_key, artist_key};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The file Cadence writes.
pub const CADENCE_FILE: &str = "cadence-missing-albums.json";
/// The newest shape of the file this app knows.
const VERSION: i64 = 1;
/// What is too much to read: a larger file or a longer list is refused with one plain line.
const MOST_BYTES: u64 = 5 * 1024 * 1024;
const MOST_ALBUMS: usize = 5000;
/// Longer words are cut (an artist, an album, a song), and an album keeps at most this many songs.
const MOST_CHARS: usize = 300;
const MOST_SONGS: usize = 200;
/// How often the file is looked at, and the library read again while a list is on show.
const LOOK_EVERY: f64 = 2.0;
const LIBRARY_EVERY: f64 = 600.0;

/// One album on the list, as Cadence sent it (cut to size).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Wanted {
    pub artist: String,
    /// may be empty: songs whose album Cadence does not know
    pub album: String,
    pub songs: Vec<String>,
    pub plays: u64,
    pub liked: u64,
    pub playlists: u64,
    pub saved: bool,
    pub year: Option<i32>,
    pub tracks: Option<u64>,
}

impl Wanted {
    /// What the row shows as its name: the album, or the first song when no album is named.
    pub fn title(&self) -> &str {
        if self.album.is_empty() { self.songs.first().map(String::as_str).unwrap_or("") } else { &self.album }
    }

    /// What it is kept by when forgotten or searched for: the artist and the album (or the first song).
    pub fn key(&self) -> String {
        let what = if self.album.is_empty() { format!("song:{}", album_key(self.title())) } else { album_key(&self.album) };
        format!("{}|{what}", artist_key(&self.artist))
    }

    /// The words a click searches Soulseek for: the artist and the album without its edition words
    /// ("Deluxe Edition", "Remastered"), which most copies are not named with.
    pub fn search(&self) -> String {
        let title = if self.album.is_empty() { self.title().to_string() } else { crate::util::strip_edition(&self.album) };
        format!("{} {}", self.artist.trim(), title.trim()).trim().to_string()
    }

    /// The faint words after the artist: why it is on the list.
    pub fn why(&self) -> String {
        let mut out: Vec<String> = vec![];
        if self.saved {
            out.push("saved as an album".into());
        }
        if self.album.is_empty() {
            out.push(match self.songs.len() {
                0 | 1 => "a song with no album named".into(),
                n => format!("{n} songs with no album named"),
            });
        }
        if self.plays > 0 {
            out.push(if self.plays == 1 { "1 play".into() } else { format!("{} plays", commas(self.plays)) });
        }
        if self.liked > 0 {
            out.push(if self.liked == 1 { "1 liked song".into() } else { format!("{} liked songs", commas(self.liked)) });
        }
        if self.playlists > 0 {
            out.push(if self.playlists == 1 { "in 1 playlist".into() } else { format!("in {} playlists", commas(self.playlists)) });
        }
        out.join(" \u{b7} ")
    }

    /// What pointing at the row says: the songs that were wanted.
    pub fn songs_words(&self) -> Option<String> {
        const SHOWN: usize = 8;
        if self.songs.is_empty() {
            return None;
        }
        let mut words = format!("You wanted: {}", self.songs.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", "));
        if self.songs.len() > SHOWN {
            let more = self.songs.len() - SHOWN;
            words.push_str(&if more == 1 { ", and 1 more".to_string() } else { format!(", and {more} more") });
        }
        Some(words)
    }
}

fn commas(n: u64) -> String {
    crate::library_page::commas(n)
}

/// A list as it was read.
#[derive(Clone, Debug, PartialEq)]
pub struct List {
    /// "Cadence"
    pub from: String,
    /// when it was written, as Cadence wrote it (UTC, ISO 8601); compared as written
    pub written: String,
    /// most missed first
    pub albums: Vec<Wanted>,
}

/// Cuts a string to `MOST_CHARS` letters and takes away control characters.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MOST_CHARS).collect::<String>().trim().to_string()
}

fn count(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f > 0.0).map(|f| f.min(1e12) as u64)).unwrap_or(0),
        // an array where a count was expected: how many it holds
        Value::Array(a) => a.len() as u64,
        _ => 0,
    }
}

/// Reads a list from the file's bytes. Unknown fields are passed over; an album with no artist and no
/// name is left out. The error is a plain sentence for the card.
pub fn parse(bytes: &[u8]) -> Result<List, String> {
    if bytes.len() as u64 > MOST_BYTES {
        return Err("The list from Cadence is over 5 MB, too large to read.".into());
    }
    let doc: Value = serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)).map_err(|_| "The list from Cadence could not be read. Send it again from Cadence.".to_string())?;
    if doc.get("kind").and_then(Value::as_str).is_some_and(|k| k != "missing-albums") {
        return Err("The file from Cadence is not a list of albums this app knows.".into());
    }
    let version = doc.get("version").and_then(Value::as_i64).unwrap_or(1);
    if version > VERSION {
        return Err("This list is from a newer Cadence. Update Bradley's Music Tool to read it.".into());
    }
    let Some(albums) = doc.get("albums").and_then(Value::as_array) else {
        return Err("The list from Cadence has no albums in it.".into());
    };
    if albums.len() > MOST_ALBUMS {
        return Err(format!("The list from Cadence has {} albums, more than the 5,000 this app reads.", commas(albums.len() as u64)));
    }
    let text = |a: &Value, k: &str| a.get(k).and_then(Value::as_str).map(clean).unwrap_or_default();
    let mut out = Vec::with_capacity(albums.len());
    for a in albums {
        let songs: Vec<String> = a.get("songs").and_then(Value::as_array).map(|s| s.iter().filter_map(Value::as_str).map(clean).filter(|s| !s.is_empty()).take(MOST_SONGS).collect()).unwrap_or_default();
        let w = Wanted {
            artist: text(a, "artist"),
            album: text(a, "album"),
            songs,
            plays: a.get("plays").map(count).unwrap_or(0),
            liked: a.get("liked").map(count).unwrap_or(0),
            playlists: a.get("playlists").map(count).unwrap_or(0),
            saved: a.get("saved").and_then(Value::as_bool).unwrap_or(false),
            year: a.get("year").and_then(Value::as_i64).filter(|y| (1000..=9999).contains(y)).map(|y| y as i32),
            tracks: a.get("tracks").and_then(Value::as_u64).filter(|t| *t > 0 && *t < 10_000),
        };
        if w.artist.is_empty() || w.title().is_empty() {
            continue;
        }
        out.push(w);
    }
    Ok(List { from: text(&doc, "from"), written: text(&doc, "written"), albums: out })
}

/// When the list was written, in this PC's time.
pub fn written_at(list: &List) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::DateTime::parse_from_rfc3339(&list.written).ok().map(|t| t.with_timezone(&chrono::Local))
}

/// What this app remembers about the lists it was sent.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// albums taken off with the X, for good (`Wanted::key`)
    #[serde(default)]
    pub forgotten: HashSet<String>,
    /// the `written` of a list put away with "Forget the list": it stays away until a newer one comes
    #[serde(default)]
    pub put_away: String,
    /// the `written` of the newest list seen, so a newer one opens the Downloads page once
    #[serde(default)]
    pub seen: String,
    /// albums a row's click has searched for
    #[serde(default)]
    pub searched: HashSet<String>,
    /// the time and size of the last file that could not be read (shown once, like a newer list)
    #[serde(default)]
    pub refused: String,
    /// when the list in `seen` was first read (UTC, RFC 3339), for the answer Cadence reads
    #[serde(default)]
    pub read_at: String,
}

/// The file this app writes beside Cadence's list, for Cadence to read: which list was read and when, and
/// how far each album has come. Written at every start (so Cadence knows this app answers, even with no
/// list yet), whenever an album's step changes, and when the app closes.
pub const ANSWER_FILE: &str = "cadence-missing-albums.answer.json";

/// How far an album on the list has come, as the card shows it and the answer tells Cadence.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    Searching,
    /// the search ended with this many copies
    Found(usize),
    /// a copy is on its way: files done of files
    Arriving(usize, usize),
    /// arrived and on the import list, not imported yet
    Ready,
    Imported,
    /// arrived; where it stands on the import list is not known
    Arrived,
    /// searched once; that search is no longer kept
    Searched,
    /// taken off the list here with the X
    SetAside,
}

impl Stage {
    /// The words at a row's end.
    pub fn words(&self) -> String {
        match self {
            Stage::Searching => "searching".into(),
            Stage::Found(0) => "no copies found".into(),
            Stage::Found(1) => "1 copy found".into(),
            Stage::Found(n) => format!("{} copies found", commas(*n as u64)),
            Stage::Arriving(done, of) => format!("arriving, {done} of {of}"),
            Stage::Ready => "ready to import".into(),
            Stage::Imported => "imported".into(),
            Stage::Arrived => "arrived".into(),
            Stage::Searched => "searched".into(),
            Stage::SetAside => "set aside".into(),
        }
    }

    /// Its name in the answer file.
    fn name(&self) -> &'static str {
        match self {
            Stage::Searching => "searching",
            Stage::Found(_) => "found",
            Stage::Arriving(..) => "arriving",
            Stage::Ready => "ready",
            Stage::Imported => "imported",
            Stage::Arrived => "arrived",
            Stage::Searched => "searched",
            Stage::SetAside => "set-aside",
        }
    }
}

/// The answer for a list (or none yet) and the albums that have a step, in the list's order. The
/// artist, album and first song are Cadence's own words as read, so it can match them to its rows.
/// `updated` is when this was true (UTC, RFC 3339).
pub fn answer(list: Option<&List>, read_at: &str, steps: &[(Wanted, Stage)], updated: &str) -> Vec<u8> {
    let albums: Vec<Value> = steps
        .iter()
        .map(|(w, s)| {
            let song = if w.album.is_empty() { w.songs.first().cloned().unwrap_or_default() } else { String::new() };
            let mut a = serde_json::json!({ "artist": w.artist, "album": w.album, "song": song, "stage": s.name(), "words": s.words() });
            match s {
                Stage::Found(n) => a["copies"] = (*n).into(),
                Stage::Arriving(done, of) => {
                    a["done"] = (*done).into();
                    a["of"] = (*of).into();
                }
                _ => {}
            }
            a
        })
        .collect();
    let doc = serde_json::json!({
        "from": "Bradley's Music Tool", "kind": "missing-albums-answer", "version": 1,
        "list": list.map(|l| l.written.as_str()).unwrap_or(""), "read": if list.is_some() { read_at } else { "" },
        "updated": updated, "albums": albums,
    });
    serde_json::to_vec_pretty(&doc).unwrap_or_default()
}

fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl State {
    fn file() -> PathBuf {
        crate::config::app_dir().join("inbox-state.json")
    }
    pub fn load_from(path: &Path) -> State {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save_to(&self, path: &Path) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = crate::util::write_atomic(path, &b);
        }
    }
}

/// Is this album in the library? By the artist's folder (or a spelling of it in the artist name list) and
/// the album's name. Songs with no album named are had when every one of them is.
pub fn owned(lib: &Library, w: &Wanted) -> bool {
    let k = lib.key_for(&w.artist);
    if w.album.is_empty() {
        return !w.songs.is_empty() && w.songs.iter().all(|s| lib.owns(&k, s, true));
    }
    lib.owns(&k, &w.album, false)
}

/// The rows to show: the list less what was forgotten and what the library has (when it has been read).
pub fn shown<'a>(list: &'a List, state: &State, lib: Option<&Library>) -> Vec<&'a Wanted> {
    if !state.put_away.is_empty() && state.put_away == list.written {
        return vec![];
    }
    list.albums.iter().filter(|w| !state.forgotten.contains(&w.key()) && !lib.is_some_and(|l| owned(l, w))).collect()
}

/// What the music folder is read with.
#[derive(Clone, PartialEq, Default)]
pub struct Folders {
    pub music: String,
    pub skip: String,
    pub aliases: String,
}

/// The list, kept up to date: the file looked at every two seconds, the library read in the background.
pub struct Inbox {
    dir: PathBuf,
    state_file: PathBuf,
    pub state: State,
    /// the file's time and size when it was last read
    mark: Option<(SystemTime, u64)>,
    looked_at: f64,
    /// the list, or why it cannot be read; None when there is no file
    pub list: Option<Result<List, String>>,
    /// the library as last read, and the music folder it was read from
    lib: Option<Arc<Library>>,
    lib_music: String,
    lib_for: Folders,
    lib_read_at: f64,
    lib_rx: Option<crossbeam_channel::Receiver<Option<Library>>>,
    /// the library has been read once for this list (until then no rows show, so none drop off in sight)
    pub ready: bool,
    /// "Show all" is on
    pub show_all: bool,
    /// the last answer written, less its time (so it is written again only when what it says changed)
    answered: Vec<u8>,
}

impl Inbox {
    /// Makes the `inbox` folder (Cadence looks for it) and reads what is remembered.
    pub fn new() -> Inbox {
        let data = crate::config::app_dir();
        let dir = data.join("inbox");
        let _ = std::fs::create_dir_all(&dir);
        let state_file = State::file();
        Inbox { dir, state: State::load_from(&state_file), state_file, mark: None, looked_at: -10.0, list: None, lib: None, lib_music: String::new(), lib_for: Folders::default(), lib_read_at: -1e9, lib_rx: None, ready: false, show_all: false, answered: vec![] }
    }

    fn save(&self) {
        self.state.save_to(&self.state_file);
    }

    /// True when the answer for Cadence can be worked out: no list, a list that cannot be read, or a list
    /// whose rows are known (the library has been read for it).
    pub fn can_answer(&self) -> bool {
        !matches!(&self.list, Some(Ok(_))) || self.ready
    }

    /// The albums on the list taken off here with the X, in the list's order.
    pub fn set_aside(&self) -> Vec<Wanted> {
        match &self.list {
            Some(Ok(list)) => list.albums.iter().filter(|w| self.state.forgotten.contains(&w.key())).cloned().collect(),
            _ => vec![],
        }
    }

    /// Writes the answer for Cadence (`ANSWER_FILE`) when what it says has changed, or `always` (at the
    /// close, so its time says when this app last had it open).
    pub fn answer_with(&mut self, steps: &[(Wanted, Stage)], always: bool) {
        let list = match &self.list {
            Some(Ok(l)) => Some(l),
            _ => None,
        };
        let key = answer(list, &self.state.read_at, steps, "");
        if !always && key == self.answered {
            return;
        }
        if crate::util::write_atomic(&self.dir.join(ANSWER_FILE), &answer(list, &self.state.read_at, steps, &utc_now())).is_ok() {
            self.answered = key;
        }
    }

    /// Looks at the file at most every two seconds and reads it again when it has changed. Returns true
    /// once for a list newer than any seen before (the page then opens Downloads).
    pub fn tick(&mut self, now: f64) -> bool {
        if now - self.looked_at < LOOK_EVERY {
            return false;
        }
        self.looked_at = now;
        let path = self.dir.join(CADENCE_FILE);
        let mark = std::fs::metadata(&path).ok().filter(|m| m.is_file()).map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        if mark == self.mark {
            return false;
        }
        self.mark = mark;
        let Some((_, len)) = mark else {
            self.list = None;
            return false;
        };
        self.list = Some(if len > MOST_BYTES {
            Err("The list from Cadence is over 5 MB, too large to read.".into())
        } else {
            match std::fs::read(&path) {
                Ok(b) => parse(&b),
                // (Cadence moving a new one into place this moment: looked at again in two seconds)
                Err(_) => {
                    self.mark = None;
                    return false;
                }
            }
        });
        // a new list: the library is read again before its rows show
        self.ready = false;
        self.lib_read_at = -1e9;
        match &self.list {
            Some(Ok(list)) if !list.written.is_empty() && list.written > self.state.seen => {
                self.state.seen = list.written.clone();
                self.state.read_at = utc_now();
                self.save();
                true
            }
            // a file that cannot be read is shown once too, so a Send never seems to do nothing; known by
            // its time and size, since what it says cannot be trusted
            Some(Err(_)) => {
                let (time, len) = mark.unwrap_or((SystemTime::UNIX_EPOCH, 0));
                let at = time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
                let key = format!("{at}:{len}");
                if key == self.state.refused {
                    return false;
                }
                self.state.refused = key;
                self.save();
                true
            }
            _ => false,
        }
    }

    /// Reads the library in the background when it is due: for a new list, when `again` (an import just
    /// ended), every ten minutes, or when the folders changed. Takes a reading that has ended.
    pub fn keep_library(&mut self, folders: &Folders, now: f64, again: bool, ctx: &egui::Context) {
        if let Some(rx) = &self.lib_rx {
            match rx.try_recv() {
                Ok(lib) => {
                    self.take_reading(lib);
                    self.lib_rx = None;
                    self.ready = true;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(_) => {
                    self.lib_rx = None;
                    self.ready = true;
                }
            }
        }
        if !matches!(&self.list, Some(Ok(l)) if !l.albums.is_empty()) {
            return;
        }
        let due = again || now - self.lib_read_at >= LIBRARY_EVERY || *folders != self.lib_for;
        if !due || folders.music.trim().is_empty() {
            if folders.music.trim().is_empty() {
                // no music folder: nothing is owned (a library read before says nothing about "none")
                self.lib = None;
                self.ready = true;
            }
            return;
        }
        self.lib_read_at = now;
        self.lib_for = folders.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.lib_rx = Some(rx);
        let (f, ctx) = (folders.clone(), ctx.clone());
        std::thread::spawn(move || {
            let lib = Library::scan(f.music.trim(), &f.skip, &f.aliases).ok().filter(|l| !l.artists.is_empty());
            let _ = tx.send(lib);
            ctx.request_repaint();
        });
    }

    /// Takes up a reading of the library (`lib_for`): None when it could not be read or held no artists.
    /// A drive that is unplugged keeps the last reading of that same folder; a reading of another folder says
    /// nothing about what this one holds, so it is dropped (albums only it had are not owned any more).
    fn take_reading(&mut self, lib: Option<Library>) {
        let asked = self.lib_for.music.clone();
        match lib {
            Some(lib) => {
                self.lib = Some(Arc::new(lib));
                self.lib_music = asked;
            }
            None if !crate::library::same_folder(&self.lib_music, &asked) => self.lib = None,
            None => {}
        }
    }

    /// The rows to show now (empty until the library has been read for this list).
    pub fn rows(&self) -> Vec<Wanted> {
        match (&self.list, self.ready) {
            (Some(Ok(list)), true) => shown(list, &self.state, self.lib.as_deref()).into_iter().cloned().collect(),
            _ => vec![],
        }
    }

    /// Takes one album off for good.
    pub fn forget(&mut self, w: &Wanted) {
        self.state.forgotten.insert(w.key());
        self.save();
    }

    /// Puts the whole list away until a newer one is sent.
    pub fn put_away(&mut self) {
        if let Some(Ok(list)) = &self.list {
            self.state.put_away = list.written.clone();
            self.show_all = false;
            self.save();
        }
    }

    /// A row was clicked: it is searched for (the search itself is the Downloads page's).
    pub fn searched(&mut self, w: &Wanted) {
        if self.state.searched.insert(w.key()) {
            self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(artist: &str, album: &str, songs: &[&str]) -> Value {
        serde_json::json!({ "artist": artist, "album": album, "songs": songs, "plays": 12, "liked": 1, "playlists": 2, "saved": false })
    }

    fn doc(albums: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "from": "Cadence", "kind": "missing-albums", "version": 1, "written": "2026-11-15T20:52:00.1234567Z", "albums": albums })).unwrap()
    }

    #[test]
    fn reads_the_file_as_cadence_writes_it() {
        let text = r#"{
  "from": "Cadence",
  "kind": "missing-albums",
  "version": 1,
  "written": "2026-11-16T01:52:07.1234567Z",
  "something new": { "a": 1 },
  "albums": [
    { "artist": "Amber Relay", "album": "Signal Garden (Deluxe Edition)", "songs": ["Static Bloom", "After the Dial"], "plays": 412, "liked": 4, "playlists": 5, "saved": false, "year": 2019, "tracks": 12, "mood": "new field" },
    { "artist": "North Pier", "album": "", "songs": ["Harbor Lights"], "plays": 96, "liked": 0, "playlists": 0, "saved": false },
    { "artist": "Coastal Drift", "album": "The Long Shore", "songs": [], "plays": 0, "liked": 0, "playlists": 0, "saved": true },
    { "artist": "", "album": "", "songs": [], "plays": 3, "liked": 0, "playlists": 0, "saved": false }
  ]
}"#;
        let list = parse(text.as_bytes()).unwrap();
        assert_eq!((list.from.as_str(), list.albums.len()), ("Cadence", 3), "the album with no names is left out, the unknown fields passed over");
        let a = &list.albums[0];
        assert_eq!((a.year, a.tracks), (Some(2019), Some(12)));
        assert_eq!(a.search(), "Amber Relay Signal Garden", "the edition words are not searched for");
        assert_eq!(a.why(), "412 plays \u{b7} 4 liked songs \u{b7} in 5 playlists");
        assert_eq!(a.songs_words().unwrap(), "You wanted: Static Bloom, After the Dial");
        let b = &list.albums[1];
        assert_eq!((b.title(), b.search().as_str(), b.why().as_str()), ("Harbor Lights", "North Pier Harbor Lights", "a song with no album named \u{b7} 96 plays"));
        assert_eq!(list.albums[2].why(), "saved as an album");
        assert_eq!(list.albums[2].songs_words(), None);
        // the time Cadence writes (seven figures after the second) is understood
        assert!(written_at(&list).is_some());
        // a byte-order mark in front is fine
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend_from_slice(text.as_bytes());
        assert_eq!(parse(&bom).unwrap(), list);
    }

    #[test]
    fn refuses_what_it_cannot_read_soundly() {
        let newer = serde_json::to_vec(&serde_json::json!({ "kind": "missing-albums", "version": 2, "albums": [] })).unwrap();
        assert!(parse(&newer).unwrap_err().contains("newer Cadence"));
        let other = serde_json::to_vec(&serde_json::json!({ "kind": "playlists", "version": 1, "albums": [] })).unwrap();
        assert!(parse(&other).is_err());
        assert!(parse(b"{ not json").is_err());
        assert!(parse(b"{}").is_err(), "no albums at all");
        let many = doc((0..5001).map(|n| album("A", &format!("Album {n}"), &[])).collect());
        assert!(parse(&many).unwrap_err().contains("5,000"));
        assert_eq!(parse(&doc((0..5000).map(|n| album("A", &format!("Album {n}"), &[])).collect())).unwrap().albums.len(), 5000);
        let big = vec![b' '; MOST_BYTES as usize + 1];
        assert!(parse(&big).unwrap_err().contains("5 MB"));
    }

    #[test]
    fn cuts_long_and_odd_words() {
        let long = "x".repeat(5000);
        let songs: Vec<String> = (0..500).map(|n| format!("Song {n}")).collect();
        let list = parse(&serde_json::to_vec(&serde_json::json!({ "kind": "missing-albums", "version": 1, "albums": [
            { "artist": format!("Name\u{0}\n{long}"), "album": "..\\..\\Windows", "songs": songs, "plays": -5, "liked": "many", "playlists": ["a", "b"], "saved": "yes", "year": 99999 }
        ] })).unwrap()).unwrap();
        let w = &list.albums[0];
        assert_eq!(w.artist.chars().count(), MOST_CHARS);
        assert!(!w.artist.contains('\0') && !w.artist.contains('\n'));
        assert_eq!(w.songs.len(), MOST_SONGS);
        assert_eq!((w.plays, w.liked, w.playlists, w.saved, w.year), (0, 0, 2, false, None));
        // a name that looks like a path is only words: it is searched for as written
        assert!(w.search().ends_with("..\\..\\Windows"));
        assert!(w.songs_words().unwrap().ends_with(", and 192 more"));
    }

    #[test]
    fn forgetting_and_putting_away_last() {
        let dir = std::env::temp_dir().join(format!("inbox-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("inbox-state.json");
        let list = parse(&doc(vec![album("Amber Relay", "Signal Garden", &[]), album("The Blue Hour", "Low Frequencies", &[]), album("North Pier", "", &["Harbor Lights"])])).unwrap();
        let mut state = State::default();
        assert_eq!(shown(&list, &state, None).len(), 3);
        state.forgotten.insert(list.albums[1].key());
        state.save_to(&file);
        let back = State::load_from(&file);
        assert_eq!(back, state);
        // forgotten by its names, however they are written next time ("&", "The", an edition)
        let again = parse(&doc(vec![album("Blue Hour", "Low Frequencies (Remastered)", &[]), album("north pier", "", &["Harbor Lights"])])).unwrap();
        assert_eq!(again.albums[0].key(), list.albums[1].key());
        assert_eq!(shown(&again, &back, None).len(), 1);
        // a list put away stays away; a newer one comes back
        let mut away = back.clone();
        away.put_away = list.written.clone();
        assert!(shown(&list, &away, None).is_empty());
        let mut newer = list.clone();
        newer.written = "2026-11-16T09:00:00Z".into();
        assert_eq!(shown(&newer, &away, None).len(), 2);
        // a state file that is not there or not whole is an empty one
        assert_eq!(State::load_from(&dir.join("none.json")), State::default());
        std::fs::write(&file, b"{ half").unwrap();
        assert_eq!(State::load_from(&file), State::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_the_library_has_leaves() {
        let dir = std::env::temp_dir().join(format!("inbox-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (folder, file) in [("Amber Relay\\Signal Garden (2019)", "01 - Static Bloom.mp3"), ("North Pier\\Shoreline", "03 - Harbor Lights.mp3"), ("Coastal Drift\\Other Album", "01 - Tide.mp3")] {
            let d = dir.join(folder);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(file), b"x").unwrap();
        }
        let lib = Library::scan(&dir.to_string_lossy(), "", "").unwrap();
        let list = parse(&doc(vec![
            album("Amber Relay", "Signal Garden (Deluxe Edition)", &[]),
            album("North Pier", "", &["Harbor Lights"]),
            album("North Pier", "", &["Harbor Lights", "Fog Signal"]),
            album("Coastal Drift", "The Long Shore", &[]),
            album("Hollow Pines", "Winter Quarters", &[]),
        ]))
        .unwrap();
        let left: Vec<&str> = shown(&list, &State::default(), Some(&lib)).iter().map(|w| w.title()).collect();
        // the album under another edition name and the one song on an album are had; a song list is had
        // only when every song is; another album by an artist who is there is not
        assert_eq!(left, ["Harbor Lights", "The Long Shore", "Winter Quarters"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_list_opens_the_page_once() {
        let dir = std::env::temp_dir().join(format!("inbox-tick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut inbox = Inbox { dir: dir.clone(), state_file: dir.join("inbox-state.json"), state: State::default(), mark: None, looked_at: -10.0, list: None, lib: None, lib_music: String::new(), lib_for: Folders::default(), lib_read_at: -1e9, lib_rx: None, ready: false, show_all: false, answered: vec![] };
        assert!(!inbox.tick(0.0), "no file, nothing to open");
        std::fs::write(dir.join(CADENCE_FILE), doc(vec![album("A", "B", &[])])).unwrap();
        assert!(!inbox.tick(1.0), "looked at every two seconds only");
        assert!(inbox.tick(2.5));
        assert!(!inbox.tick(5.0), "the same list again does not");
        // the same list read by a new start of the app: already seen
        let mut fresh = Inbox { state: State::load_from(&dir.join("inbox-state.json")), mark: None, looked_at: -10.0, ..inbox_like(&dir) };
        assert!(!fresh.tick(0.0));
        // a newer Send does (written a moment later, so the time or size differs)
        let mut newer = serde_json::from_slice::<Value>(&doc(vec![album("A", "B", &[]), album("C", "D", &[])])).unwrap();
        newer["written"] = "2026-11-16T08:00:00Z".into();
        std::fs::write(dir.join(CADENCE_FILE), serde_json::to_vec(&newer).unwrap()).unwrap();
        assert!(fresh.tick(3.0));
        // a list this app cannot read opens the page once too, to say why
        std::fs::write(dir.join(CADENCE_FILE), b"{ \"kind\": \"missing-albums\", \"version\": 7, \"albums\": [] }").unwrap();
        assert!(fresh.tick(9.0));
        assert!(fresh.list.as_ref().unwrap().is_err());
        assert!(!fresh.tick(12.0));
        let mut again = Inbox { state: State::load_from(&dir.join("inbox-state.json")), ..inbox_like(&dir) };
        assert!(!again.tick(0.0), "not again at the next start");        // the file going away takes the list away
        std::fs::remove_file(dir.join(CADENCE_FILE)).unwrap();
        assert!(!fresh.tick(15.0));
        assert!(fresh.list.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_answer_says_which_list_was_read_and_how_far_each_album_came() {
        let dir = std::env::temp_dir().join(format!("inbox-answer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut inbox = inbox_like(&dir);
        // no list yet: an answer can be given (it says this app answers), and says no list
        assert!(inbox.can_answer());
        inbox.answer_with(&[], false);
        let read = |dir: &Path| serde_json::from_slice::<Value>(&std::fs::read(dir.join(ANSWER_FILE)).unwrap()).unwrap();
        let first = read(&dir);
        assert_eq!((first["kind"].as_str(), first["version"].as_i64(), first["list"].as_str(), first["read"].as_str()), (Some("missing-albums-answer"), Some(1), Some(""), Some("")));
        // a list arrives: it is read, and no answer is given until its rows are known
        std::fs::write(dir.join(CADENCE_FILE), doc(vec![album("Amber Relay", "Signal Garden (Deluxe Edition)", &[]), album("North Pier", "", &["Harbor Lights", "Fog Signal"]), album("Coastal Drift", "The Long Shore", &[])])).unwrap();
        assert!(inbox.tick(0.0));
        assert!(!inbox.state.read_at.is_empty(), "when it was read is kept");
        assert!(!inbox.can_answer());
        inbox.ready = true;
        let list = match &inbox.list {
            Some(Ok(l)) => l.clone(),
            _ => panic!("the list should read"),
        };
        inbox.forget(&list.albums[2]);
        assert_eq!(inbox.set_aside(), vec![list.albums[2].clone()]);
        let steps = vec![(list.albums[0].clone(), Stage::Arriving(6, 10)), (list.albums[1].clone(), Stage::Found(14)), (list.albums[2].clone(), Stage::SetAside)];
        inbox.answer_with(&steps, false);
        let said = read(&dir);
        assert_eq!(said["list"].as_str(), Some(list.written.as_str()));
        assert_eq!(said["read"].as_str(), Some(inbox.state.read_at.as_str()));
        let albums = said["albums"].as_array().unwrap();
        // Cadence's own words, so it can match them: the edition kept, the first song for songs with no album
        assert_eq!((albums[0]["artist"].as_str(), albums[0]["album"].as_str(), albums[0]["stage"].as_str(), albums[0]["done"].as_u64(), albums[0]["of"].as_u64()), (Some("Amber Relay"), Some("Signal Garden (Deluxe Edition)"), Some("arriving"), Some(6), Some(10)));
        assert_eq!((albums[1]["album"].as_str(), albums[1]["song"].as_str(), albums[1]["stage"].as_str(), albums[1]["copies"].as_u64()), (Some(""), Some("Harbor Lights"), Some("found"), Some(14)));
        assert_eq!(albums[2]["stage"].as_str(), Some("set-aside"));
        // written again only when what it says changed, or at the close
        std::fs::remove_file(dir.join(ANSWER_FILE)).unwrap();
        inbox.answer_with(&steps, false);
        assert!(!dir.join(ANSWER_FILE).exists(), "nothing changed, nothing written");
        inbox.answer_with(&steps, true);
        assert!(dir.join(ANSWER_FILE).exists(), "at the close it is written");
        let mut later = steps.clone();
        later[0].1 = Stage::Ready;
        inbox.answer_with(&later, false);
        assert_eq!(read(&dir)["albums"][0]["stage"].as_str(), Some("ready"));
        // the words at a row's end
        assert_eq!([Stage::Found(0).words(), Stage::Found(1).words(), Stage::Found(1200).words(), Stage::Arriving(6, 10).words(), Stage::Ready.words()], ["no copies found", "1 copy found", "1,200 copies found", "arriving, 6 of 10", "ready to import"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn inbox_like(dir: &Path) -> Inbox {
        Inbox { dir: dir.to_path_buf(), state_file: dir.join("inbox-state.json"), state: State::default(), mark: None, looked_at: -10.0, list: None, lib: None, lib_music: String::new(), lib_for: Folders::default(), lib_read_at: -1e9, lib_rx: None, ready: false, show_all: false, answered: vec![] }
    }
}

#[cfg(test)]
mod library_change_tests {
    use super::*;

    #[test]
    fn a_reading_of_another_music_folder_is_not_kept() {
        // review finding: when the music folder was changed to one that could not be read, the old
        // folder's library stayed, so Cadence was told albums only the old folder had were owned
        let dir = std::env::temp_dir().join(format!("inbox-change-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let album = dir.join("old").join("Amber Relay").join("Signal Garden");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 - Static Bloom.mp3"), b"x").unwrap();
        let old = dir.join("old").to_string_lossy().to_string();
        let mut inbox = Inbox { dir: dir.clone(), state_file: dir.join("inbox-state.json"), state: State::default(), mark: None, looked_at: -10.0, list: None, lib: None, lib_music: String::new(), lib_for: Folders::default(), lib_read_at: -1e9, lib_rx: None, ready: false, show_all: false, answered: vec![] };
        inbox.lib_for.music = old.clone();
        inbox.take_reading(Library::scan(&old, "", "").ok());
        assert!(inbox.lib.is_some());
        // the same folder unplugged: the last reading of it stays
        inbox.lib_for.music = format!("{}\\", old.to_uppercase());
        inbox.take_reading(None);
        assert!(inbox.lib.is_some(), "an unplugged drive keeps what it held");
        // another folder that cannot be read: what the old one held is not owned any more
        inbox.lib_for.music = dir.join("new").to_string_lossy().to_string();
        inbox.take_reading(None);
        assert!(inbox.lib.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
