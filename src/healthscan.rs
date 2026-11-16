//! The library check: what the Health page shows.
//!
//! Reads the whole music folder and never changes it: every track's tags (ffprobe, kept on disk so the
//! next check only reads files that changed), optionally every track's audio (the deep check: decode
//! errors, real length, FLAC checksum, the spectrum; `importer::quality`, also kept on disk), then the
//! rules below. What it finds is written to the app's own folder as
//! `{ When, Deep, Cancelled, Files, Albums, Artists, Unchecked, DeepLeft, ListsLeft, Issues: [ { S, C, A, L, P, D, K, F, X } ] }`
//! (the shape is described at the top of `health.rs`).
//!
//! It took the place of an older check kept outside the app. Where that one was wrong (the audit in
//! PLAN.md, "Health check audit"), this one differs on purpose:
//!   * which folders are albums comes from `library::album_paths`, the same reading every page uses
//!   * two tracks are "the same song twice" only with a title and the same length
//!   * "Same album twice?" is the app's own same-album rule, and never two different years
//!   * a file that cannot be read or decoded is remembered for a week, not tried again on every check
//!   * tag text is cut to a readable length before any rule sees it
//!   * one finding per kind and folder, so the key each is ignored by is its own
//!   * a half-finished import folder is only reported once nothing has written to it for a while
use crate::importer::quality::{self, Quality};
use crate::importer::util::{strip_year, write_atomic};
use crate::importer::recheck;
use crate::importer::tracklist::OurTrack;
use crate::importer::{rules, tools};
use crate::library;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const AUDIO_EXT: [&str; 15] = ["mp3", "m4a", "flac", "wav", "aif", "aiff", "ogg", "opus", "wma", "aac", "ape", "wv", "tta", "dsf", "dff"];
const LOSSLESS_EXT: [&str; 9] = ["flac", "wav", "aif", "aiff", "ape", "wv", "tta", "dsf", "dff"];
const IMAGE_EXT: [&str; 3] = ["jpg", "jpeg", "png"];
const JUNK_EXT: [&str; 7] = ["part", "tmp", "crdownload", "!ut", "!qb", "partial", "download"];
/// some programs cannot open paths over 260 characters; this leaves a little room
const MAX_PATH: usize = 240;
/// no rule needs more of a tag than this, and a tag can be megabytes of anything
const TAG_MAX: usize = 2000;
/// a file that could not be read or decoded is tried again after this long
const RETRY_SECS: u64 = 7 * 24 * 3600;
/// an import folder nothing has written to for this long was abandoned
pub const ABANDONED_SECS: u64 = 15 * 60;
const TAGS_VERSION: u32 = 1;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}
re!(LOSSLESS_CODEC, r"^(flac|alac|ape|wavpack|tta|mlp|truehd|wmalossless|pcm_.*|dsd_.*)$");
re!(EDITION_RX, r"(?i)(\s+-\s+(EP|Single)$)|(\s*[\(\[{][^\)\]}]*[\)\]}]\s*$)|([\s\-:,]*\b(disc|disk|cd)\s*\d+(\s*of\s*\d+)?\s*$)");
re!(LEAD_RX, r"^\s*(\d+)");
re!(TOTAL_RX, r"^\s*\d+\s*/\s*(\d+)");
re!(DISC_TRACK_RX, r"^(\d)-(\d{1,3})\D");
re!(DISC_DIR_RX, r"(?i)^(?:cd|disc|disk)[\s._-]*(\d{1,2})\b");
re!(QUIET_RX, r"(?i)silen(ce|t)|untitled|hidden|blank|\bnothing\b");
re!(NOISE_RX, r"\[[^\]\[]* @ [0-9a-fA-Fx]+\]\s*");
re!(VARIOUS_RX, r"(?i)\bvarious\b|^va$|compilation|soundtrack");

// ------------------------------------------------------------------ what a check is given and says

pub struct Options {
    pub library: PathBuf,
    /// also decode every track that has no saved result yet
    pub deep: bool,
    /// the Settings list of folders that are not artists
    pub skip_folders: String,
    pub alias_file: String,
    /// where the findings are written
    pub report: PathBuf,
    /// where the tags read, the files that failed and the uploader signatures are kept
    pub cache_dir: PathBuf,
    pub quality_cache: PathBuf,
    /// tags saved by the check the app had before its own, read (never written) for files this check has not read itself
    pub seed_tags: Option<PathBuf>,
    /// where each album's comparison with its official track list is kept, so it is only asked once a month
    pub tracklists: PathBuf,
    /// where the answers of the online lookups are kept
    pub lookups: PathBuf,
    /// where LRCLIB's answers about lyrics are kept (read by a check, never asked)
    pub lyrics_dir: PathBuf,
    /// false: no album is looked up online (what is already known about the track lists is still used)
    pub online: bool,
    /// check only these folders at the top of the library (artists', usually), because something in them
    /// changed: what is found there takes the place of what the report said about them, and the rest of the
    /// report stays as it was. Empty = the whole library.
    pub only: Vec<PathBuf>,
}

impl Options {
    /// A check of the library in Settings. Everything it writes is in the app's own folder (a test run's, with
    /// `MUSIC_RECS_DATA` set, is that folder).
    pub fn for_app(cfg: &crate::config::Config, deep: bool) -> Self {
        let cache_dir = crate::config::cache_dir();
        Options {
            library: PathBuf::from(&cfg.music_dir),
            deep,
            skip_folders: cfg.skip_folders.clone(),
            alias_file: cfg.alias_file.clone(),
            report: report_file(&cfg.music_dir),
            quality_cache: cache_dir.join("quality-cache.txt"),
            seed_tags: Some(cache_dir.join("library-tags-cache.txt")),
            tracklists: recheck::overlay_file(&cfg.music_dir),
            lookups: cache_dir.join("tracklists"),
            lyrics_dir: cache_dir.join("lyrics"),
            online: !std::env::args().any(|a| a == "--no-online"),
            only: vec![],
            cache_dir,
        }
    }
}

fn library_id(library: &str) -> String {
    let key = format!("{:x}", md5::compute(library.trim().trim_end_matches(['\\', '/']).to_lowercase().as_bytes()));
    key[..10].to_string()
}

/// Where the app keeps what its own check found for this library.
pub fn report_file(library: &str) -> PathBuf {
    crate::config::app_dir().join(format!("health-report-{}.json", library_id(library)))
}

pub const PHASES: [&str; 6] = ["Finding files", "Reading tags", "Checking audio", "Looking for problems", "Comparing track lists", "Done"];

#[derive(Default)]
pub struct Progress {
    /// index into `PHASES`
    pub phase: AtomicU8,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
    /// a report was written (a check stopped while still reading tags writes none)
    pub wrote: AtomicBool,
    pub issues: AtomicUsize,
    pub error: Mutex<String>,
}

impl Progress {
    fn enter(&self, phase: u8, total: usize) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        self.phase.store(phase, Ordering::Relaxed);
    }
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// One finding, in the report's shape.
#[derive(Clone, Debug)]
pub struct Found {
    pub s: u8,
    pub c: String,
    pub a: String,
    pub l: String,
    pub p: String,
    pub d: String,
    pub k: String,
    pub f: Vec<String>,
    pub x: BTreeMap<String, String>,
}

impl Found {
    fn fix(&mut self, kind: &str, files: Vec<String>) -> &mut Self {
        self.k = kind.to_string();
        self.f = files;
        self
    }
    fn fact(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        self.x.insert(name.to_string(), value.into());
        self
    }
    fn from_json(i: &Value) -> Option<Found> {
        let o = i.as_object()?;
        let s = |k: &str| o.get(k).map(text).unwrap_or_default();
        Some(Found {
            s: o.get("S").and_then(|v| v.as_u64()).filter(|s| (1..=3).contains(s)).unwrap_or(3) as u8,
            c: s("C"),
            a: s("A"),
            l: s("L"),
            p: s("P"),
            d: s("D"),
            k: s("K"),
            f: o.get("F").and_then(|f| f.as_array()).map(|a| a.iter().map(text).collect()).unwrap_or_default(),
            x: o.get("X").and_then(|x| x.as_object()).map(|x| x.iter().map(|(k, v)| (k.clone(), text(v))).collect()).unwrap_or_default(),
        })
    }
    fn json(&self) -> Value {
        json!({ "S": self.s, "C": self.c, "A": self.a, "L": self.l, "P": self.p, "D": self.d, "K": self.k, "F": self.f, "X": self.x })
    }
}

const REDOWNLOAD: [&str; 8] = ["Damaged or incomplete", "Upscaled MP3", "Fake lossless", "Unreadable file", "Low bitrate", "Low sample rate", "Missing tracks", "Minor glitches"];

#[derive(Default)]
struct Out {
    issues: Vec<Found>,
}

impl Out {
    fn add(&mut self, s: u8, c: &str, artist: &str, album: &str, path: &str, detail: String) -> &mut Found {
        let mut i = Found { s, c: c.to_string(), a: artist.to_string(), l: album.to_string(), p: path.to_string(), d: detail, k: String::new(), f: vec![], x: BTreeMap::new() };
        if REDOWNLOAD.contains(&c) {
            i.k = "redownload".into();
            i.x.insert("search".into(), search_words(artist, album, path));
        }
        self.issues.push(i);
        self.issues.last_mut().unwrap()
    }
}

/// What to type into Soulseek: artist and album without the year or edition, or the file's name for a loose track.
fn search_words(artist: &str, album: &str, path: &str) -> String {
    let a = if album.starts_with('(') { Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default() } else { strip_edition_all(&strip_year(album)) };
    format!("{artist} {a}").trim().to_string()
}

// ------------------------------------------------------------------ one file's tags

/// What ffprobe says about one file. The field names are the ones the tags were first saved under, so tags saved long ago can still be read.
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "PascalCase")]
pub struct Info {
    pub ok: bool,
    pub error: String,
    /// readable, but in an odd wrapper that some players cannot handle
    pub quirk: String,
    pub codec: String,
    pub sample_rate: u32,
    pub bits: u32,
    pub kbps: u32,
    pub art_w: u32,
    pub art_h: u32,
    pub duration: f64,
    pub has_art: bool,
    pub tags: BTreeMap<String, String>,
    /// when this was read (seconds since 1970): a failure is tried again after a week
    pub at: u64,
    /// how the file's lyrics stand, worked out once for each version of the file (None: not looked at yet,
    /// as for every file read before the check knew about lyrics)
    pub lyr: Option<Lyr>,
}

/// How a file's lyrics stand, as the check keeps it (see `crate::importer::lyrics`).
#[derive(Clone, Default, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default, rename_all = "PascalCase")]
pub struct Lyr {
    /// 0 none, 1 in the lyrics field, 2 only as timed lyrics in their own frame, 3 only in a tag no player reads
    pub place: u8,
    /// the words say the song has none ("Instrumental")
    pub instrumental: bool,
    /// nothing that could be a lyric is left beside the junk
    pub only: bool,
    /// the first few lines that are not lyrics: (line, why)
    pub junk: Vec<(String, String)>,
}

/// A file's lyrics, read whole. The tags a check keeps are cut short, and a site's credit stands at the end of
/// the words: an MP3's tag is read itself, and another kind of file is asked again when its lyrics reach the cut.
pub fn lyric_of(ffprobe: &Path, path: &Path, info: &Info) -> crate::importer::lyrics::Lyric {
    use crate::importer::lyrics;
    let (mut field, hidden, timed) = lyrics::words(path, info.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    if field.len() >= TAG_MAX - 4 && crate::importer::id3::frames(path).is_none() {
        let whole = crate::importer::probe::probe(ffprobe, path);
        let (full, _, _) = lyrics::words(path, whole.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if full.len() > field.len() {
            field = full;
        }
    }
    lyrics::judge(&field, &hidden, timed)
}

/// The artist a song is asked about on LRCLIB: the track's own, else the album's, else the folder's.
pub fn lyric_artist_of(info: &Info, folder_artist: &str) -> String {
    [info.tag("artist"), info.album_artist(), folder_artist].iter().find(|a| !a.trim().is_empty()).map(|a| a.trim().to_string()).unwrap_or_default()
}

fn lyric_artist(t: &Track, folder_artist: &str) -> String {
    lyric_artist_of(&t.info, folder_artist)
}

fn lyr_of(ffprobe: &Path, path: &Path, info: &Info) -> Lyr {
    use crate::importer::lyrics::Place;
    let l = lyric_of(ffprobe, path, info);
    Lyr {
        place: match l.place {
            Place::None => 0,
            Place::Field => 1,
            Place::TimedFrame => 2,
            Place::Hidden => 3,
        },
        instrumental: l.junk.instrumental,
        only: l.junk.only,
        junk: l.junk.lines.iter().take(5).map(|(line, why)| (line.clone(), why.to_string())).collect(),
    }
}

impl Info {
    fn tag(&self, k: &str) -> &str {
        self.tags.get(k).map(|s| s.as_str()).unwrap_or("")
    }
    fn album_artist(&self) -> &str {
        let v = self.tag("album_artist");
        if v.is_empty() { self.tag("albumartist") } else { v }
    }
    fn failed(error: &str) -> Info {
        Info { error: error.to_string(), at: now(), ..Default::default() }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cut(s: &str) -> String {
    let t = s.trim();
    if t.len() <= TAG_MAX { t.to_string() } else { t.chars().take(TAG_MAX).collect() }
}

fn num(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn one_line(s: &str) -> String {
    let s = s.trim().replace(['\r', '\n'], " ");
    s.chars().take(600).collect()
}

fn probe_once(ffprobe: &Path, file: &Path, force: Option<&str>, secs: u64) -> Info {
    let mut c = tools::command(ffprobe, true);
    c.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]);
    if let Some(f) = force {
        c.args(["-f", f]);
    }
    c.args(crate::importer::tools::LOCAL_ONLY).arg("--").arg(file);
    let Some((out, err)): Option<(Vec<u8>, Vec<u8>)> = tools::capture_limited(&mut c, secs) else { return Info::failed("took too long to read") };
    let json: Value = match serde_json::from_slice(&out) {
        Ok(v) if !out.iter().all(|b| b.is_ascii_whitespace()) => v,
        _ => return Info::failed(&one_line(&String::from_utf8_lossy(&err))),
    };
    let mut info = Info { at: now(), ..Default::default() };
    let mut audio: Option<&Value> = None;
    for s in json["streams"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        match s["codec_type"].as_str() {
            Some("audio") if audio.is_none() => audio = Some(s),
            Some("video") => {
                info.has_art = true;
                info.art_w = info.art_w.max(num(&s["width"]) as u32);
                info.art_h = info.art_h.max(num(&s["height"]) as u32);
            }
            _ => {}
        }
    }
    let Some(a) = audio else {
        // ffprobe prints an empty answer for a file it cannot open, and says why beside it
        let why = one_line(&String::from_utf8_lossy(&err));
        return Info::failed(if why.is_empty() { "no audio stream in this file" } else { &why });
    };
    info.codec = text(&a["codec_name"]);
    info.sample_rate = num(&a["sample_rate"]) as u32;
    info.bits = num(&a["bits_per_raw_sample"]) as u32;
    if info.bits == 0 && a["sample_fmt"].as_str().is_some_and(|s| s.starts_with("u8")) {
        info.bits = 8;
    }
    let format = &json["format"];
    let mut rate = num(&a["bit_rate"]);
    if rate <= 0.0 {
        rate = num(&format["bit_rate"]);
    }
    info.kbps = (rate / 1000.0) as u32;
    info.duration = num(&format["duration"]);
    if info.duration <= 0.0 {
        info.duration = num(&a["duration"]);
    }
    for tags in [&format["tags"], &a["tags"]] {
        for (k, v) in tags.as_object().into_iter().flatten() {
            info.tags.entry(k.to_lowercase()).or_insert_with(|| cut(&text(v)));
        }
    }
    info.ok = true;
    info
}

/// What is really inside a file whose name says .mp3.
fn wrapper_name(file: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let inner = || -> Option<String> {
        let mut f = std::fs::File::open(file).ok()?;
        let mut h = [0u8; 10];
        f.read_exact(&mut h).ok()?;
        let pos = if &h[..3] == b"ID3" { 10 + ((h[6] as u64 & 0x7f) << 21 | (h[7] as u64 & 0x7f) << 14 | (h[8] as u64 & 0x7f) << 7 | (h[9] as u64 & 0x7f)) } else { 0 };
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut b = [0u8; 12];
        f.read_exact(&mut b).ok()?;
        if &b[..4] == b"RIFF" {
            return Some(format!("MP3 audio inside a WAV wrapper{}", if pos > 0 { " (with a tag stuck in front)" } else { "" }));
        }
        (&b[4..8] == b"ftyp").then(|| "MP3 audio inside an MP4 wrapper".to_string())
    };
    inner().unwrap_or_else(|| "MP3 audio in an unusual wrapper".to_string())
}

fn explain(err: &str, file: &Path) -> String {
    if err.contains("moov atom not found") {
        return "it's really an M4A/MP4 file, and incomplete (missing its index): it needs a new download".into();
    }
    if err.contains("Invalid data found") || err.contains("Failed to find two consecutive MPEG audio frames") {
        return format!("not valid audio data (corrupt, or not really a .{})", ext_of(file));
    }
    // ffmpeg's own words, without the "[mp3 @ 000001f3a2c4]" it starts each line with
    let plain: String = NOISE_RX.replace_all(err, "").trim().chars().take(160).collect();
    if plain.is_empty() { "can't be read".into() } else { plain }
}

/// Tries hard before calling a file unreadable: a longer second attempt, and for an .mp3, reading it as
/// MP3 whatever wrapper it claims to be in.
pub fn probe(ffprobe: &Path, file: &Path) -> Info {
    let mut info = probe_once(ffprobe, file, None, 30);
    if !info.ok && info.error == "took too long to read" {
        info = probe_once(ffprobe, file, None, 120);
    }
    if !info.ok && ext_of(file) == "mp3" {
        let mut forced = probe_once(ffprobe, file, Some("mp3"), 60);
        if forced.ok {
            forced.quirk = wrapper_name(file);
            return forced;
        }
    }
    if !info.ok {
        info.error = explain(&info.error, file);
    }
    info
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// The key a file's saved results are kept under: it changes when the file does. The same key the
/// tags were first saved under, so tags saved long ago can still be used.
fn file_key(full: &str, size: u64, ticks: u128) -> String {
    format!("{TAGS_VERSION}|{}|{size}|{ticks}", full.to_lowercase())
}

/// A file's modified time as .NET counts it (100 ns steps since the year 1).
fn ticks_of(m: &std::fs::Metadata) -> u128 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| 621_355_968_000_000_000u128 + d.as_secs() as u128 * 10_000_000 + (d.subsec_nanos() / 100) as u128).unwrap_or(0)
}

fn load_tags(path: &Path, into: &mut HashMap<String, Info>, seed: bool) {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else { return };
    let prefix = format!("{TAGS_VERSION}|");
    let fresh = now().saturating_sub(RETRY_SECS);
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        let line = line.strip_prefix('\u{feff}').unwrap_or(&line);
        let Some((key, body)) = line.split_once('\t') else { continue };
        if !key.starts_with(&prefix) || (seed && into.contains_key(key)) {
            continue;
        }
        let Ok(mut info) = serde_json::from_str::<Info>(body) else { continue };
        if seed {
            // the other check kept only good reads, with the tags whole
            info.tags = info.tags.iter().map(|(k, v)| (k.clone(), cut(v))).collect();
            info.at = now();
        } else if !info.ok && info.at < fresh {
            continue; // a failure from over a week ago: try the file again
        }
        into.insert(key.to_string(), info);
    }
}

fn save_tags(path: &Path, cache: &HashMap<String, Info>, keep: Option<&HashSet<String>>) {
    let mut out = String::new();
    for (k, v) in cache {
        if keep.is_some_and(|keep| !keep.contains(k)) {
            continue;
        }
        if let Ok(body) = serde_json::to_string(v) {
            out.push_str(k);
            out.push('\t');
            out.push_str(&body);
            out.push('\n');
        }
    }
    let _ = write_atomic(path, out.as_bytes());
}

/// Files whose audio could not be decoded, by file key: (when, why).
fn load_failed(path: &Path) -> HashMap<String, (u64, String)> {
    let fresh = now().saturating_sub(RETRY_SECS);
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            let (key, at, why) = (p.next()?, p.next()?.parse::<u64>().ok()?, p.next().unwrap_or(""));
            (at >= fresh).then(|| (key.to_string(), (at, why.to_string())))
        })
        .collect()
}

fn save_failed(path: &Path, failed: &HashMap<String, (u64, String)>) {
    let text: String = failed.iter().map(|(k, (at, why))| format!("{k}\t{at}\t{}\n", why.replace(['\t', '\r', '\n'], " "))).collect();
    let _ = write_atomic(path, text.as_bytes());
}

// ------------------------------------------------------------------ small readers

/// Width and height of a JPEG or PNG from its header (nothing is decoded). None for anything else, and for
/// a size no picture has.
pub fn image_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut h = [0u8; 26];
    f.read_exact(&mut h).ok()?;
    let sane = |w: u32, h: u32| (w > 0 && h > 0 && w <= 30_000 && h <= 30_000).then_some((w, h));
    if &h[..4] == b"\x89PNG" {
        return sane(u32::from_be_bytes([h[16], h[17], h[18], h[19]]), u32::from_be_bytes([h[20], h[21], h[22], h[23]]));
    }
    if h[0] != 0xFF || h[1] != 0xD8 {
        return None;
    }
    f.seek(SeekFrom::Start(2)).ok()?;
    let mut b = [0u8; 5];
    // a JPEG's size is in one of its first few parts; a file made of thousands of them is not a cover
    for _ in 0..2000 {
        f.read_exact(&mut b[..1]).ok()?;
        if b[0] != 0xFF {
            continue;
        }
        let mut marker = 0xFFu8;
        while marker == 0xFF {
            f.read_exact(&mut b[..1]).ok()?;
            marker = b[0];
        }
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        f.read_exact(&mut b[..2]).ok()?;
        let len = ((b[0] as i64) << 8) | b[1] as i64;
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            f.read_exact(&mut b).ok()?;
            return sane(((b[3] as u32) << 8) | b[4] as u32, ((b[1] as u32) << 8) | b[2] as u32);
        }
        f.seek(SeekFrom::Current(len - 2)).ok()?;
    }
    None
}

/// An MP3's own length header (the Xing, Info or VBRI frame count), in seconds; 0 if it has none. When what
/// plays matches it, the file is complete and players show the right length: a longer guess from ffprobe
/// then only comes from extra tags at the end of the file, not from anything wrong with the audio.
pub fn mp3_header_seconds(file: &Path) -> f64 {
    use std::io::{Read, Seek, SeekFrom};
    let inner = || -> Option<f64> {
        let mut f = std::fs::File::open(file).ok()?;
        let mut h = [0u8; 10];
        f.read_exact(&mut h).ok()?;
        let mut pos = 0u64;
        if &h[..3] == b"ID3" {
            pos = 10 + ((h[6] as u64 & 0x7f) << 21 | (h[7] as u64 & 0x7f) << 14 | (h[8] as u64 & 0x7f) << 7 | (h[9] as u64 & 0x7f)) + if h[5] & 0x10 != 0 { 10 } else { 0 };
        }
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut b = vec![0u8; 8192];
        let mut n = 0;
        while n < b.len() {
            match f.read(&mut b[n..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
        let be = |at: usize| (b[at] as u64) << 24 | (b[at + 1] as u64) << 16 | (b[at + 2] as u64) << 8 | b[at + 3] as u64;
        let mut i = 0;
        while i + 4 < n {
            if b[i] != 0xFF || (b[i + 1] & 0xE0) != 0xE0 {
                i += 1;
                continue;
            }
            let (ver, layer, sri, mode) = ((b[i + 1] >> 3) & 3, (b[i + 1] >> 1) & 3, (b[i + 2] >> 2) & 3, (b[i + 3] >> 6) & 3);
            if ver == 1 || layer != 1 || sri == 3 {
                i += 1;
                continue; // not a valid MPEG layer III frame header
            }
            let rates: [u32; 3] = if ver == 3 { [44100, 48000, 32000] } else if ver == 2 { [22050, 24000, 16000] } else { [11025, 12000, 8000] };
            let (rate, spf) = (rates[sri as usize] as f64, if ver == 3 { 1152.0 } else { 576.0 });
            let side = if ver == 3 { if mode == 3 { 17 } else { 32 } } else if mode == 3 { 9 } else { 17 };
            let x = i + 4 + side;
            let mut frames = 0u64;
            if x + 12 <= n && (&b[x..x + 4] == b"Xing" || &b[x..x + 4] == b"Info") {
                if b[x + 7] & 1 != 0 {
                    frames = be(x + 8);
                }
            } else if i + 36 + 18 <= n && &b[i + 36..i + 40] == b"VBRI" {
                frames = be(i + 36 + 14);
            }
            return Some(if frames > 0 { frames as f64 * spf / rate } else { 0.0 }); // only the first frame can carry the header
        }
        None
    };
    inner().unwrap_or(0.0)
}

// ------------------------------------------------------------------ names

/// Lowercase letters and digits, accents folded: how two spellings of a name are compared.
fn key(s: &str) -> String {
    crate::util::norm(s)
}

/// `key`, or the name itself in lower case when it has no letters or digits ("÷", "( )").
fn key_or_raw(s: &str) -> String {
    let k = key(s);
    if k.is_empty() { s.trim().to_lowercase() } else { k }
}

/// " - EP", "(Deluxe)", "[FLAC]" and disc markers like "Disc 2", "CD1", "(Disc 1 of 2)" taken off the end.
fn strip_edition_all(s: &str) -> String {
    let mut cur = s.trim().to_string();
    loop {
        let next = EDITION_RX.replace(&cur, "").trim().to_string();
        if next.is_empty() || next == cur {
            return cur;
        }
        cur = next;
    }
}

fn lead(s: &str) -> u32 {
    LEAD_RX.captures(s).and_then(|c| c[1].parse::<u32>().ok()).filter(|n| *n <= 9999).unwrap_or(0)
}

fn tag_total(s: &str) -> u32 {
    TOTAL_RX.captures(s).and_then(|c| c[1].parse::<u32>().ok()).filter(|n| *n <= 9999).unwrap_or(0)
}

fn time(s: f64) -> String {
    format!("{}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64)
}

fn estimate_kbps(hz: f64) -> u32 {
    if hz <= 17000.0 {
        128
    } else if hz <= 18000.0 {
        160
    } else if hz <= 19250.0 {
        192
    } else if hz <= 19750.0 {
        256
    } else {
        320
    }
}

fn khz(hz: f64) -> String {
    crate::importer::util::trim_num(hz / 1000.0)
}

/// "a; b; c; and 4 more"
pub(crate) fn list_of(items: &[String], max: usize) -> String {
    let mut s = items.iter().take(max).map(|i| i.as_str()).collect::<Vec<_>>().join("; ");
    if items.len() > max {
        s.push_str(&format!("; and {} more", items.len() - max));
    }
    s
}

/// The value that occurs most, the first of them on a tie.
fn most_common<'a>(values: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut order: Vec<(&str, usize)> = vec![];
    for v in values.filter(|v| !v.is_empty()) {
        match order.iter_mut().find(|e| e.0 == v) {
            Some(e) => e.1 += 1,
            None => order.push((v, 1)),
        }
    }
    let mut best: Option<(&str, usize)> = None;
    for e in order {
        if best.is_none_or(|b| e.1 > b.1) {
            best = Some(e);
        }
    }
    best.map(|b| b.0)
}

/// The first spelling of each distinct value, where `same` says what makes two values one.
fn distinct_by<'a>(values: impl Iterator<Item = &'a str>, same: impl Fn(&str) -> String) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    values.filter(|v| seen.insert(same(v))).collect()
}

// ------------------------------------------------------------------ the library as the check sees it

struct Track {
    path: PathBuf,
    full: String,
    name: String,
    /// the path from the library folder, part by part, the file's name last
    parts: Vec<String>,
    ext: String,
    size: u64,
    ticks: u128,
    info: Info,
    q: Option<Quality>,
    lossless: bool,
    disc: u32,
    no: u32,
}

impl Track {
    fn title(&self) -> &str {
        self.info.tag("title")
    }
}

#[derive(Default)]
struct Walked {
    /// (file, size, modified)
    audio: Vec<(PathBuf, u64, u128)>,
    other: Vec<PathBuf>,
    empty: Vec<PathBuf>,
    importing: Vec<PathBuf>,
}

fn walk(dir: &Path, depth: u32, w: &mut Walked, stop: &AtomicBool) {
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut any = false;
    for e in rd.flatten() {
        any = true;
        let Ok(kind) = e.file_type() else { continue };
        let p = e.path();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if e.file_name().to_string_lossy().to_lowercase().ends_with(".importing") {
                w.importing.push(p);
            } else if depth < 16 {
                walk(&p, depth + 1, w, stop);
            }
        } else if AUDIO_EXT.contains(&ext_of(&p).as_str()) && !is_repair_leftover(&p) {
            let m = e.metadata().ok();
            w.audio.push((p, m.as_ref().map(|m| m.len()).unwrap_or(0), m.as_ref().map(ticks_of).unwrap_or(0)));
        } else {
            w.other.push(p);
        }
    }
    if !any {
        w.empty.push(dir.to_path_buf());
    }
}

/// "Song.repairtmp.mp3": what a repair that was cut off leaves beside the track it was working on.
fn is_repair_leftover(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().contains(".repairtmp."))
}

/// The newest change to anything in a folder (and the folder itself), in seconds since 1970.
fn newest_change(dir: &Path, depth: u32) -> u64 {
    let at = |m: std::io::Result<std::fs::Metadata>| m.ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let mut newest = at(std::fs::metadata(dir));
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        newest = newest.max(if is_dir && depth < 6 { newest_change(&e.path(), depth + 1) } else { at(e.metadata()) });
    }
    newest
}

/// How long it has been since anything in a folder changed, in seconds.
pub fn quiet_seconds(dir: &Path) -> u64 {
    now().saturating_sub(newest_change(dir, 0))
}

/// An artist's folder: "Ween", or "Dubstep\Skream" inside a genre folder.
struct Root {
    dir: PathBuf,
    name: String,
    /// its album folders as `library::album_paths` reads them (paths from `dir`, lower case)
    albums: HashSet<String>,
    /// a folder of compilations ("Various Artists"): rules about one artist and one track list do not apply
    collection: bool,
}

/// The tracks of one album, or the tracks lying loose in an artist's folder.
struct Unit {
    artist: String,
    album: String,
    folder: PathBuf,
    tracks: Vec<usize>,
    collection: bool,
    loose: bool,
    root: Option<usize>,
}

struct Shape {
    roots: Vec<Root>,
    units: Vec<Unit>,
}

/// Which album every track belongs to. `genres`: the top folders that hold artists, not albums (lower case).
fn shape(library: &Path, tracks: &[Track], genres: &HashSet<String>, compilations: &HashSet<String>) -> Shape {
    let mut s = Shape { roots: vec![], units: vec![] };
    let mut root_of: HashMap<String, usize> = HashMap::new();
    let mut unit_of: HashMap<String, usize> = HashMap::new();
    // a folder directly in a genre folder: an album of its own (true), or an artist
    let mut direct_album: HashMap<String, bool> = HashMap::new();
    for (ti, t) in tracks.iter().enumerate() {
        let p = &t.parts;
        if p.len() < 2 {
            continue; // a file in the library folder itself: reported on its own
        }
        let top = p[0].to_lowercase();
        let in_genre = genres.contains(&top);
        // the artist's folder, and the folders between it and the file
        let (root_rel, inner): (Vec<&String>, &[String]) = if !in_genre {
            (vec![&p[0]], &p[1..p.len() - 1])
        } else if p.len() == 2 {
            (vec![&p[0]], &[])
        } else {
            let sub = library.join(&p[0]).join(&p[1]);
            let direct = *direct_album.entry(format!("{top}\\{}", p[1].to_lowercase())).or_insert_with(|| {
                let (own, subs) = library::look(&sub);
                own > 0 && subs.iter().all(|n| library::is_disc_like(n))
            });
            if direct {
                let u = *unit_of.entry(sub.to_string_lossy().to_lowercase()).or_insert_with(|| {
                    s.units.push(Unit { artist: p[0].clone(), album: p[1].clone(), folder: sub.clone(), tracks: vec![], collection: true, loose: false, root: None });
                    s.units.len() - 1
                });
                s.units[u].tracks.push(ti);
                continue;
            }
            (vec![&p[0], &p[1]], &p[2..p.len() - 1])
        };
        let rel = root_rel.iter().map(|x| x.as_str()).collect::<Vec<_>>().join("\\");
        let ri = *root_of.entry(rel.to_lowercase()).or_insert_with(|| {
            let dir = library.join(&rel);
            let albums = library::album_paths(&dir).into_iter().map(|a| a.to_lowercase()).collect();
            let name = root_rel.last().unwrap().to_string();
            s.roots.push(Root { dir, name, albums, collection: compilations.contains(&top) });
            s.roots.len() - 1
        });
        let root = &s.roots[ri];
        // the deepest album folder the file is in; a folder the library's reading does not list is its own album
        let depth = (1..=inner.len()).rev().find(|&j| root.albums.contains(&inner[..j].join("\\").to_lowercase())).unwrap_or(inner.len().min(1));
        let folder = inner[..depth].iter().fold(root.dir.clone(), |d, part| d.join(part));
        let (artist, collection) = (root.name.clone(), root.collection);
        let u = *unit_of.entry(format!("{}|{}", folder.to_string_lossy().to_lowercase(), depth == 0)).or_insert_with(|| {
            s.units.push(Unit { artist, album: if depth == 0 { String::new() } else { inner[depth - 1].clone() }, folder, tracks: vec![], collection, loose: depth == 0, root: Some(ri) });
            s.units.len() - 1
        });
        s.units[u].tracks.push(ti);
    }
    s
}

// ------------------------------------------------------------------ the rules

struct Rules<'a> {
    tracks: &'a [Track],
    /// "field|value" -> how many artist folders carry it
    spread: HashMap<String, usize>,
    /// tag artist -> folder artist, both as `key` makes them
    aliases: HashMap<String, String>,
    /// LRCLIB's answers kept from earlier imports and repairs, and the names of the songs there is one for
    kept: crate::importer::lyrics::Lookup,
    asked: HashSet<String>,
    out: Out,
}

/// "3 of 12 tracks checked", or the track's own name when one track was all there was to check.
fn who(hit: &[&Track], measured: usize, kind: &str) -> String {
    if measured == 1 { hit[0].name.clone() } else { format!("{} of {measured} {kind}tracks checked", hit.len()) }
}

impl Rules<'_> {
    fn files(list: &[&Track]) -> Vec<String> {
        list.iter().map(|t| t.full.clone()).collect()
    }

    /// Checks that apply to any group of tracks, loose ones too: damage, fake or upscaled quality, bitrate, odd formats.
    fn quality(&mut self, artist: &str, album: &str, folder: &str, all: &[&Track], loose: bool) {
        // odd wrappers cannot be decoded the usual way, so they get no verdict on their audio
        let list: Vec<&Track> = all.iter().copied().filter(|t| t.info.quirk.is_empty()).collect();
        let (mut damaged, mut damaged_t, mut glitchy, mut length_off, mut length_t) = (vec![], vec![], vec![], vec![], vec![]);
        for t in &list {
            let Some(q) = &t.q else { continue };
            let dur = t.info.duration;
            let before = damaged.len();
            if t.lossless {
                if q.md5_status == "mismatch" {
                    damaged.push(format!("{} (checksum doesn't match: the audio has changed or is corrupt)", t.name));
                } else if dur > 5.0 && q.decoded_seconds < dur - 2.0 {
                    damaged.push(format!("{} (stops at {} of {})", t.name, time(q.decoded_seconds), time(dur)));
                } else if !q.decode_ok {
                    damaged.push(format!("{} (decode errors)", t.name));
                }
            } else {
                let mut very_short = dur > 5.0 && q.decoded_seconds < dur * 0.95 - 2.0;
                if very_short {
                    // the file's own length header agrees with what plays: complete, and players show it right
                    let hs = mp3_header_seconds(&t.path);
                    if hs > 0.0 && (hs - q.decoded_seconds).abs() <= 2.0 {
                        very_short = false;
                    }
                }
                if very_short && q.error_count > 0 {
                    damaged.push(format!("{} (stops at {} of {})", t.name, time(q.decoded_seconds), time(dur)));
                } else if very_short {
                    // no errors, only shorter than the header says: usually a missing VBR header
                    length_off.push(format!("{} (plays {}, header says {})", t.name, time(q.decoded_seconds), time(dur)));
                    length_t.push(*t);
                } else if q.error_count >= 10 {
                    damaged.push(format!("{} ({} decode errors, likely audible glitches)", t.name, q.error_count));
                } else if q.error_count >= 3 {
                    glitchy.push(t.name.clone());
                }
            }
            if damaged.len() > before {
                damaged_t.push(*t);
            }
        }
        if !damaged.is_empty() {
            let again = if damaged.len() == 1 { "This one needs a new download" } else { "These need a new download" };
            let i = self.out.add(1, "Damaged or incomplete", artist, album, folder, format!("{}. {again}", list_of(&damaged, 4)));
            i.f = Self::files(&damaged_t);
            with_tracks(i, &damaged_t, &list, loose);
        }
        if !glitchy.is_empty() {
            let d = if glitchy.len() == 1 { format!("{} has a few decode errors, usually inaudible", glitchy[0]) } else { format!("{} tracks have a few decode errors, usually inaudible: {}", glitchy.len(), list_of(&glitchy, 3)) };
            self.out.add(3, "Minor glitches", artist, album, folder, d);
        }
        if !length_off.is_empty() {
            let i = self.out.add(3, "Length mismatch", artist, album, folder, format!("{}. Usually a missing VBR header: plays fine, but seeking and the shown length can be off", list_of(&length_off, 3)));
            i.fix("vbr-header", Self::files(&length_t));
            with_tracks(i, &length_t, &list, loose);
        }

        // silence and dropouts
        let (mut silent, mut worst) = (vec![], 3u8);
        for t in &list {
            let Some(q) = t.q.as_ref().filter(|q| q.decoded_seconds > 5.0) else { continue };
            if QUIET_RX.is_match(&format!("{} {}", t.title(), t.name)) {
                continue; // quiet on purpose
            }
            if q.lead_silence >= q.decoded_seconds - 1.0 {
                silent.push(format!("{} (completely silent)", t.name));
                continue;
            }
            // Clean digital silence is part of the recording (hidden tracks, pauses). A broken download decodes
            // with errors, so silence only counts as a dropout when the same track also has decode errors.
            if q.error_count == 0 {
                continue;
            }
            if q.max_gap >= 2.0 && q.max_gap < 15.0 {
                let more = if q.gaps > 1 { format!(", {} silent stretches", q.gaps) } else { String::new() };
                silent.push(format!("{} ({:.0}s gap at {} next to decode errors{more})", t.name, q.max_gap, time(q.max_gap_at)));
                worst = 2;
            }
        }
        if !silent.is_empty() {
            let tail = if worst == 2 { ". Have a listen: gaps next to decode errors usually mean a broken download" } else { ". Usually intentional; have a listen if you don't expect it" };
            let search = format!("{artist} {}", if album.starts_with('(') { String::new() } else { strip_edition_all(&strip_year(album)) }).trim().to_string();
            let i = self.out.add(worst, "Silence or dropouts", artist, album, folder, format!("{}{tail}", list_of(&silent, 4)));
            i.k = "redownload".into();
            i.fact("search", search);
        }

        // loose tracks are unrelated singles, so each one is judged on its own
        if loose {
            for t in &list {
                self.judge(artist, album, &t.full, &[*t]);
            }
        } else {
            self.judge(artist, album, folder, &list);
        }

        // bitrate and sample rate
        let lossy: Vec<&Track> = list.iter().copied().filter(|t| !t.lossless).collect();
        if !lossy.is_empty() {
            let low: Vec<&Track> = lossy.iter().copied().filter(|t| t.info.kbps > 0 && t.info.kbps < 160).collect();
            let meh: Vec<&Track> = lossy.iter().copied().filter(|t| t.info.kbps >= 160 && t.info.kbps < 192).collect();
            if !low.is_empty() {
                let around = low.iter().map(|t| t.info.kbps).sum::<u32>() / low.len() as u32;
                let d = if list.len() == 1 { format!("{} is under 160kbps (around {around}kbps)", low[0].name) } else { format!("{} of {} tracks under 160kbps (around {around}kbps)", low.len(), list.len()) };
                self.out.add(2, "Low bitrate", artist, album, folder, d);
            } else if meh.len() > lossy.len() / 2 {
                let d = if list.len() == 1 { format!("{} is at 160-190kbps", meh[0].name) } else { format!("{} of {} tracks at 160-190kbps", meh.len(), list.len()) };
                self.out.add(3, "Low bitrate", artist, album, folder, d);
            }
        }
        let low_rate: Vec<&Track> = list.iter().copied().filter(|t| t.info.sample_rate > 0 && t.info.sample_rate < 44100).collect();
        if let Some(first) = low_rate.first() {
            let rate = crate::importer::util::trim_num(first.info.sample_rate as f64 / 1000.0);
            let d = if low_rate.len() == 1 { format!("{} is at {rate}kHz: dull-sounding source", first.name) } else { format!("{} tracks at {rate}kHz: dull-sounding source", low_rate.len()) };
            self.out.add(2, "Low sample rate", artist, album, folder, d);
        }

        // odd wrappers (MP3 inside WAV): fine audio, but some players will not open them
        let quirky: Vec<&Track> = all.iter().copied().filter(|t| !t.info.quirk.is_empty()).collect();
        if let Some(first) = quirky.first() {
            let subject = if all.len() == 1 { format!("{} is", first.name) } else { format!("{} of {} tracks are", quirky.len(), all.len()) };
            let d = format!("{subject} {}. The audio is fine, but some apps and phones can't play {}; converting to plain MP3 loses nothing", first.info.quirk, if quirky.len() == 1 { "it" } else { "them" });
            self.out.add(2, "Unusual file format", artist, album, folder, d).fix("unwrap", Self::files(&quirky));
        }

        // a name that says one format around another's audio
        let misnamed: Vec<&Track> = list.iter().copied().filter(|t| t.ext == "flac" && !t.info.codec.is_empty() && t.info.codec != "flac").collect();
        if let Some(first) = misnamed.first() {
            let d = if misnamed.len() == 1 {
                format!("{} is named .flac but holds {} audio", first.name, first.info.codec)
            } else {
                format!("{} tracks are named .flac but hold {} audio: {}", misnamed.len(), first.info.codec, list_of(&misnamed.iter().map(|t| t.name.clone()).collect::<Vec<_>>(), 3))
            };
            self.out.add(2, "Wrong file type", artist, album, folder, d).f = Self::files(&misnamed);
        }
    }

    /// Fake lossless and upscaled lossy files: the majority of the tracks that could be judged.
    fn judge(&mut self, artist: &str, album: &str, path: &str, list: &[&Track]) {
        let measured = |t: &&Track| t.q.as_ref().is_some_and(|q| q.measured);
        let q = |t: &Track| t.q.clone().unwrap_or_default();
        let lossless: Vec<&Track> = list.iter().copied().filter(|t| t.lossless).filter(measured).collect();
        if !lossless.is_empty() {
            let need = lossless.len().div_ceil(2).max(1);
            let strong: Vec<&Track> = lossless.iter().copied().filter(|t| q(t).cliff_db >= 28.0 && q(t).cutoff_hz > 0.0 && q(t).cutoff_hz <= 19500.0).collect();
            let mild: Vec<&Track> = lossless.iter().copied().filter(|t| q(t).cliff_db >= 25.0 && q(t).cutoff_hz > 0.0 && q(t).cutoff_hz <= 20500.0).collect();
            let hit = if strong.len() >= need { Some((1, strong)) } else if mild.len() >= need { Some((2, mild)) } else { None };
            if let Some((sev, hit)) = hit {
                let mut cuts: Vec<f64> = hit.iter().map(|t| q(t).cutoff_hz).collect();
                cuts.sort_by(|a, b| a.total_cmp(b));
                let cut = cuts[cuts.len() / 2];
                let d = format!("{} {} sharply at {}kHz: converted from a ~{}kbps MP3", who(&hit, lossless.len(), "lossless "), if lossless.len() == 1 { "stops" } else { "stop" }, khz(cut), estimate_kbps(cut));
                self.out.add(sev, "Fake lossless", artist, album, path, d).f = Self::files(&hit);
            }
        }
        // Upscaled lossy files. Genuine 320s stop at about 20kHz or above, and a cliff at 19kHz can be a normal
        // encoder choice, so only clearly lower cutoffs count: 250k+ stopping at 18.5kHz or below, 190-249k at 16.5kHz.
        let lossy: Vec<&Track> = list.iter().copied().filter(|t| !t.lossless && t.info.kbps >= 190).filter(measured).collect();
        if !lossy.is_empty() {
            let need = lossy.len().div_ceil(2).max(1);
            let up: Vec<&Track> = lossy.iter().copied().filter(|t| q(t).cliff_db >= 22.0 && q(t).cutoff_hz > 0.0 && q(t).cutoff_hz <= if t.info.kbps >= 250 { 18500.0 } else { 16500.0 }).collect();
            if up.len() >= need {
                let mut cuts: Vec<f64> = up.iter().map(|t| q(t).cutoff_hz).collect();
                cuts.sort_by(|a, b| a.total_cmp(b));
                let cut = cuts[cuts.len() / 2];
                let label = up.iter().map(|t| t.info.kbps).sum::<u32>() / up.len() as u32;
                let one = lossy.len() == 1;
                let d = format!(
                    "{} {} labelled ~{label}kbps but {} sharply at {}kHz: really ~{}kbps quality that was re-encoded bigger. A proper {label}k copy would sound better",
                    who(&up, lossy.len(), ""),
                    if one { "is" } else { "are" },
                    if one { "stops" } else { "stop" },
                    khz(cut),
                    estimate_kbps(cut)
                );
                self.out.add(if cut <= 17000.0 { 1 } else { 2 }, "Upscaled MP3", artist, album, path, d).f = Self::files(&up);
            }
        }
    }

    /// Ads, links and uploader signatures in the tags.
    fn junk(&mut self, artist: &str, album: &str, folder: &str, list: &[&Track]) {
        let mut examples: Vec<(String, String)> = vec![];
        let (mut dirty, mut fields, mut exact) = (vec![], vec![], vec![]);
        for t in list {
            let found = rules::find_junk_in(t.info.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), Some(&self.spread));
            if found.is_empty() {
                continue;
            }
            dirty.push(*t);
            for (field, value, why) in found {
                if !fields.contains(&field) {
                    fields.push(field.clone());
                }
                // the repair removes exactly these values; one that was cut short here is not the whole value
                if value.len() < TAG_MAX {
                    exact.push(json!([t.full, field, value]));
                }
                let shown = rules::short(&value.split_whitespace().collect::<Vec<_>>().join(" "), 60);
                let name = if field == "comments" { "comment".to_string() } else { field.replace('_', " ") };
                let example = format!("{name} '{shown}'");
                if !examples.iter().any(|e| e.0 == example) {
                    examples.push((example, why));
                }
            }
        }
        if dirty.is_empty() {
            return;
        }
        let shown: Vec<String> = examples.iter().map(|(e, why)| format!("{e} ({why})")).collect();
        let subject = if list.len() == 1 { dirty[0].name.clone() } else { format!("{} of {} tracks", dirty.len(), list.len()) };
        let i = self.out.add(3, "Junk in tags", artist, album, folder, format!("{subject}: {}. Players show this as clutter; safe to remove", list_of(&shown, 3)));
        i.fix("strip-junk", Self::files(&dirty)).fact("fields", fields.join("|")).fact("junk", Value::Array(exact).to_string());
    }

    /// Lyrics: kept where no player reads them, carrying lines that are not lyrics, or not there at all.
    fn lyrics(&mut self, artist: &str, album: &str, folder: &str, list: &[&Track]) {
        use crate::importer::lyrics::Answer;
        let of = |t: &&Track| t.info.lyr.clone().unwrap_or_default();
        let subject = |hit: &[&Track], have: &str, has: &str| match (list.len(), hit.len()) {
            (1, _) => format!("{} {has}", hit[0].name),
            (n, m) if n == m => format!("All {n} tracks {have}"),
            (n, 1) => format!("1 of {n} tracks {has}"),
            (n, m) => format!("{m} of {n} tracks {have}"),
        };
        // 1. lyrics a conversion left in a text tag that only has the field's name
        let hidden: Vec<&Track> = list.iter().copied().filter(|t| of(t).place == 3).collect();
        if !hidden.is_empty() {
            let d = format!("{} in a tag no player reads (left there when the album was converted). They can be put where players look", subject(&hidden, "keep their lyrics", "keeps its lyrics"));
            self.out.add(3, "Lyrics players cannot see", artist, album, folder, d).fix("lyrics-unhide", Self::files(&hidden));
        }
        // 2. lines that are not lyrics: a web address, a site's credit, markup, a placeholder
        let junky: Vec<&Track> = list.iter().copied().filter(|t| !of(t).junk.is_empty()).collect();
        if !junky.is_empty() {
            // (the lines themselves are a fact of their own, shown under the finding track by track)
            let lines: Vec<Value> = junky.iter().map(|t| (t, of(t))).map(|(t, l)| json!({ "file": t.name, "only": l.only, "lines": l.junk.iter().map(|j| j.0.clone()).collect::<Vec<_>>() })).collect();
            let d = subject(&junky, "have lines in their lyrics that are not lyrics", "has lines in its lyrics that are not lyrics");
            self.out.add(3, "Lyrics with junk in them", artist, album, folder, d).fix("lyrics-junk", Self::files(&junky)).fact("lines", Value::Array(lines).to_string());
        }
        // 3. no lyrics. Not a song that says it has none, and not one LRCLIB was asked about and has
        // nothing for (or calls an instrumental): there is nothing to be done about those
        let none: Vec<&Track> = list
            .iter()
            .copied()
            .filter(|t| {
                let l = of(t);
                if l.place != 0 || l.instrumental {
                    return false;
                }
                let (by, title, secs) = (lyric_artist(t, artist), t.title(), t.info.duration);
                let name = crate::importer::lyrics::key(&by, title, secs);
                !(self.asked.contains(&name) && matches!(self.kept.known(&by, title, secs), Some(Answer::Nothing | Answer::Instrumental)))
            })
            .collect();
        if !none.is_empty() {
            let d = if none.len() == list.len() && list.len() > 1 { "No track has lyrics".to_string() } else { subject(&none, "have no lyrics", "has no lyrics") };
            self.out.add(3, "No lyrics", artist, album, folder, d).fix("lyrics-fill", Self::files(&none));
        }
    }

    /// Text saved in the wrong character encoding.
    fn garbled(&mut self, artist: &str, album: &str, folder: &str, list: &[&Track]) {
        let (mut bad, mut bad_t): (Vec<String>, Vec<&Track>) = (vec![], vec![]);
        for t in list {
            for f in ["title", "artist", "album", "album_artist"] {
                let v = t.info.tag(f);
                if !v.is_empty() && rules::test_garbled(v) {
                    let example = format!("{} '{}'", f.replace('_', " "), rules::short(v, 40));
                    if !bad.contains(&example) {
                        bad.push(example);
                    }
                    bad_t.push(*t);
                    break;
                }
            }
        }
        if bad_t.is_empty() {
            return;
        }
        let subject = if list.len() == 1 { format!("{} has", bad_t[0].name) } else { format!("{} of {} tracks have", bad_t.len(), list.len()) };
        self.out.add(2, "Garbled text", artist, album, folder, format!("{subject} scrambled characters (saved in the wrong text encoding): {}", list_of(&bad, 3))).fix("garbled", Self::files(&bad_t));
    }

    fn album(&mut self, u: &Unit) {
        let (artist, album, folder) = (u.artist.as_str(), u.album.as_str(), u.folder.to_string_lossy().to_string());
        let folder = folder.as_str();
        let list: Vec<&Track> = u.tracks.iter().map(|&i| &self.tracks[i]).filter(|t| t.info.ok).collect();
        if list.is_empty() {
            return;
        }
        let no = |t: &Track| (t.disc, t.no);
        self.junk(artist, album, folder, &list);
        self.garbled(artist, album, folder, &list);
        self.lyrics(artist, album, folder, &list);
        self.quality(artist, album, folder, &list, false);

        let lossy: Vec<&Track> = list.iter().copied().filter(|t| !t.lossless).collect();
        let formats = distinct_by(list.iter().map(|t| t.ext.as_str()), |e| e.to_string());
        let rates: Vec<u32> = lossy.iter().map(|t| t.info.kbps).filter(|k| *k > 0).collect();
        if formats.len() > 1 {
            self.out.add(3, "Mixed quality", artist, album, folder, format!("Mix of formats: {}", formats.iter().map(|f| format!(".{f}")).collect::<Vec<_>>().join(", ")));
        } else if rates.len() > 1 {
            let (min, max) = (*rates.iter().min().unwrap(), *rates.iter().max().unwrap());
            if max - min >= 96 && min < 200 {
                self.out.add(3, "Mixed quality", artist, album, folder, format!("Bitrates range from {min} to {max}kbps: tracks may be from different sources"));
            }
        }

        // missing and duplicate tracks, disc by disc
        let mut discs: Vec<u32> = list.iter().filter(|t| no(t).1 > 0).map(|t| no(t).0).collect();
        discs.sort();
        discs.dedup();
        let several = list.iter().any(|t| t.disc != list[0].disc);
        let (mut gaps, mut gap_facts, mut twice, mut twice_groups) = (vec![], vec![], vec![], vec![]);
        for &disc in &discs {
            let on: Vec<&Track> = list.iter().copied().filter(|t| no(t).0 == disc && no(t).1 > 0).collect();
            let mut nums: Vec<u32> = on.iter().map(|t| no(t).1).collect();
            nums.sort();
            nums.dedup();
            let max = *nums.last().unwrap();
            let tagged = on.iter().map(|t| tag_total(t.info.tag("track"))).max().unwrap_or(0);
            let expect = max.max(if tagged > max && tagged <= max + 30 { tagged } else { 0 });
            // later discs are sometimes numbered on from the earlier ones (disc 2 = tracks 13 to 25)
            let first = if disc > 1 && nums[0] > 1 { nums[0] } else { 1 };
            let missing: Vec<u32> = (first..=expect).filter(|n| !nums.contains(n)).collect();
            let d = if several { format!("disc {disc} ") } else { String::new() };
            if !u.collection && !missing.is_empty() && missing.len() <= (expect as usize / 2).max(2) {
                let why = if tagged > max { format!(" (the tags say {tagged} tracks; you have up to {max})") } else { String::new() };
                let shown = missing.iter().take(12).map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
                gaps.push(format!("{d}track{} {shown} of {expect}{why}", if missing.len() > 1 { "s" } else { "" }));
                gap_facts.push(format!("{disc}:{}", missing.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",")));
            }
            // the same song twice: the same number, the same title (there has to be one) and the same length
            for n in &nums {
                let same: Vec<&Track> = on.iter().copied().filter(|t| no(t).1 == *n).collect();
                if same.len() < 2 {
                    continue;
                }
                let titles = distinct_by(same.iter().map(|t| t.title()), key_or_raw);
                let (short, long) = same.iter().fold((f64::MAX, 0f64), |(lo, hi), t| (lo.min(t.info.duration), hi.max(t.info.duration)));
                if titles.len() == 1 && !titles[0].trim().is_empty() && long - short <= 2.0 {
                    twice.push(format!("{d}track {n} ({})", same.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")));
                    twice_groups.push(same.iter().map(|t| t.full.as_str()).collect::<Vec<_>>().join("|"));
                }
            }
        }
        if !gaps.is_empty() {
            let i = self.out.add(2, "Missing tracks", artist, album, folder, format!("Missing {}", gaps.join("; ")));
            i.fact("missing", gap_facts.join(";")).fact("discs", discs.len().to_string());
            with_tracks(i, &[], &list, false);
        }
        if !twice.is_empty() {
            self.out.add(2, "Duplicate tracks", artist, album, folder, format!("Same song twice: {}", list_of(&twice, 3))).fix("dupe-tracks", vec![]).fact("groups", twice_groups.join("||"));
        }

        // tags
        let count = |f: &dyn Fn(&Track) -> bool| list.iter().filter(|t| f(t)).count();
        let (n_title, n_artist, n_album, n_track) = (count(&|t| t.title().is_empty()), count(&|t| t.info.tag("artist").is_empty()), count(&|t| t.info.tag("album").is_empty()), count(&|t| lead(t.info.tag("track")) == 0));
        let mut miss = vec![];
        for (n, what) in [(n_title, "a title"), (n_artist, "an artist"), (n_album, "an album"), (if u.collection { 0 } else { n_track }, "a track number")] {
            if n > 0 {
                miss.push(format!("{n} without {what}"));
            }
        }
        if !miss.is_empty() {
            self.out.add(2, "Missing tags", artist, album, folder, format!("Of {} tracks: {}", list.len(), miss.join(", "))).fix("missing-tags", Self::files(&list));
        }
        let no_year = list.iter().all(|t| t.info.tag("date").is_empty() && t.info.tag("year").is_empty());
        let no_genre = list.iter().all(|t| t.info.tag("genre").is_empty());
        if no_year || no_genre {
            let what = if no_year && no_genre { "year or genre" } else if no_year { "year" } else { "genre" };
            self.out.add(3, "Missing year or genre", artist, album, folder, format!("No {what} on any track")).fix("year-genre", Self::files(&list)).fact("year", if no_year { "1" } else { "0" }).fact("genre", if no_genre { "1" } else { "0" });
        }

        let aa: Vec<&str> = list.iter().map(|t| t.info.album_artist()).filter(|x| !x.is_empty()).collect();
        let aa_distinct = distinct_by(aa.iter().copied(), key_or_raw);
        // the album artist to settle on: the one that matches the folder, else the most common
        let pick = aa.iter().copied().find(|v| key(v) == key(artist)).or(most_common(aa.iter().copied())).unwrap_or(artist).to_string();
        let quoted = |v: &[&str]| v.iter().take(4).map(|x| format!("'{x}'")).collect::<Vec<_>>().join(" / ");
        if aa_distinct.len() > 1 {
            let d = format!("Album artist differs between tracks: {}. Players may split this album", quoted(&aa_distinct));
            self.out.add(2, "Mixed-up tags", artist, album, folder, d).fix("album-artist", Self::files(&list)).fact("values", aa_distinct.join("|")).fact("default", pick.clone());
        } else if !aa.is_empty() && aa.len() < list.len() {
            let d = format!("Album artist set on only {} of {} tracks. Players may split this album", aa.len(), list.len());
            self.out.add(2, "Mixed-up tags", artist, album, folder, d).fix("album-artist", Self::files(&list)).fact("values", aa_distinct.first().copied().unwrap_or(artist)).fact("default", pick.clone());
        }
        let album_tags = distinct_by(list.iter().map(|t| t.info.tag("album")).filter(|x| !x.is_empty()), |x| key_or_raw(&strip_edition_all(x)));
        if album_tags.len() > 1 && !u.collection {
            let usual = most_common(list.iter().map(|t| t.info.tag("album"))).unwrap_or("").to_string();
            let d = format!("Album name differs between tracks: {}", quoted(&album_tags));
            // its own name, apart from the album artist's "Mixed-up tags", so each can be ignored by itself
            self.out.add(2, "Mixed-up album name", artist, album, folder, d).fix("album-name", Self::files(&list)).fact("values", album_tags.join("|")).fact("default", usual);
        }

        // tagged as someone else (compilations and collaborations aside)
        let main = most_common(aa.iter().copied()).or(most_common(list.iter().map(|t| t.info.tag("artist")))).unwrap_or("");
        if !main.is_empty() && !u.collection {
            let (mut nm, nf) = (key(main), key(artist));
            if let Some(alias) = self.aliases.get(&nm) {
                nm = alias.clone();
            }
            let various = VARIOUS_RX.is_match(&format!("{artist} {main}"));
            if !various && !nm.is_empty() && !nf.is_empty() && !nm.contains(&nf) && !nf.contains(&nm) {
                self.out.add(3, "Tagged under another artist", artist, album, folder, format!("Tagged as '{main}' but filed under '{artist}'")).fix("artist-mismatch", Self::files(&list)).fact("tagged", main).fact("folder", artist);
            }
        }

        // cover art
        let with_art = list.iter().filter(|t| t.info.has_art).count();
        let (mut best_w, mut best_h) = (0u64, 0u64);
        for t in list.iter().filter(|t| t.info.has_art) {
            if t.info.art_w as u64 * t.info.art_h as u64 > best_w * best_h {
                (best_w, best_h) = (t.info.art_w as u64, t.info.art_h as u64);
            }
        }
        let mut folder_image = false;
        for img in crate::importer::util::walk_files(&u.folder).into_iter().filter(|p| IMAGE_EXT.contains(&ext_of(p).as_str())) {
            folder_image = true;
            if let Some((w, h)) = image_size(&img) {
                if w as u64 * h as u64 > best_w * best_h {
                    (best_w, best_h) = (w as u64, h as u64);
                }
            }
        }
        if with_art == 0 && !folder_image {
            self.out.add(2, "No cover art", artist, album, folder, "No cover in the tracks or the folder".into()).fix("cover", Self::files(&list)).fact("mode", "none");
        } else {
            if best_w > 0 && best_w.min(best_h) < 480 {
                let d = format!("Best cover is only {best_w}x{best_h} (500+ looks sharp on most screens)");
                self.out.add(3, "Small cover art", artist, album, folder, d).fix("cover", Self::files(&list)).fact("mode", "small").fact("size", format!("{best_w}x{best_h}"));
            }
            if with_art > 0 && with_art < list.len() {
                let d = format!("{with_art} of {} tracks have the cover embedded", list.len());
                self.out.add(3, "Cover on some tracks only", artist, album, folder, d).fix("cover", Self::files(&list)).fact("mode", "partial");
            } else if with_art == 0 {
                let d = "Cover is only a file in the folder, not embedded: phones and some players won't show it".to_string();
                self.out.add(3, "Cover on some tracks only", artist, album, folder, d).fix("cover", Self::files(&list)).fact("mode", "folder");
            }
        }
    }
}

/// What a later look at the real track lengths needs: the album's tags, how many tracks it has, and for each
/// affected track its file, title, disc, number, how long it really plays, what its header claims and its
/// decode errors.
fn with_tracks(i: &mut Found, affected: &[&Track], album: &[&Track], loose: bool) {
    if loose || album.is_empty() {
        return;
    }
    let artist = album.iter().map(|t| t.info.album_artist()).find(|x| !x.is_empty()).or(album.iter().map(|t| t.info.tag("artist")).find(|x| !x.is_empty())).unwrap_or(i.a.as_str()).to_string();
    let name = album.iter().map(|t| t.info.tag("album")).find(|x| !x.is_empty()).map(|s| s.to_string()).unwrap_or_else(|| strip_year(&i.l));
    let rows: Vec<Value> = affected
        .iter()
        .map(|t| {
            let title = if t.title().is_empty() { crate::importer::util::file_stem(&t.name) } else { t.title().to_string() };
            let (plays, errors) = t.q.as_ref().map(|q| (q.decoded_seconds, q.error_count)).unwrap_or((0.0, 0));
            json!([t.full, title, t.disc.to_string(), t.no.to_string(), format!("{plays:.1}"), format!("{:.1}", t.info.duration), errors.to_string()])
        })
        .collect();
    i.fact("lookupArtist", artist).fact("lookupAlbum", name).fact("trackCount", album.len().to_string()).fact("tracks", Value::Array(rows).to_string());
}

/// Each track's disc and number, read the way a person would: the tag, else the file's name ("2-03 Song"),
/// else the folder it is in ("CD2", "Disc 2 - Live").
fn number(t: &mut Track, album_depth: usize) {
    t.no = lead(t.info.tag("track"));
    t.disc = 0;
    if t.no == 0 {
        match DISC_TRACK_RX.captures(&t.name) {
            Some(c) => (t.disc, t.no) = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0)),
            None => t.no = lead(&t.name),
        }
    }
    if t.disc == 0 {
        t.disc = lead(t.info.tag("disc"));
    }
    if t.disc == 0 {
        let inside = &t.parts[album_depth.min(t.parts.len() - 1)..t.parts.len() - 1];
        t.disc = inside.iter().find_map(|d| DISC_DIR_RX.captures(d.trim()).and_then(|c| c[1].parse().ok())).unwrap_or(0);
    }
    if t.disc == 0 {
        t.disc = 1;
    }
    if t.no > 200 {
        t.no = 0; // "2014" at the start of a file name is a year, not a track number
    }
}

fn aliases_from(file: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in std::fs::read_to_string(file).unwrap_or_default().lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.starts_with('#') {
            continue;
        }
        if let Some((from, to)) = line.split_once('=') {
            let (f, t) = (key(from), key(to));
            if !f.is_empty() && !t.is_empty() {
                out.insert(f, t);
            }
        }
    }
    out
}

/// The name, in lower case, of the folder (or file) at the top of the library that holds this path: what
/// the report's counts are kept under (`Tops`), so a check of some folders can put its own in their place.
fn top_key(library: &Path, p: &Path) -> String {
    p.strip_prefix(library).ok().and_then(|r| r.components().next()).map(|c| c.as_os_str().to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Applies every rule to the library as read. Changes nothing on disk except the uploader signatures file.
/// Returns the findings, how many albums and artists there are, the albums to hold against their track
/// lists, the albums each artist with loose tracks has, and the top folder of each album and of each artist
/// counted (`top_key`).
fn find_issues(o: &Options, tracks: &mut [Track], walked: &Walked, tops: &[(String, PathBuf)], genres: &HashSet<String>, compilations: &HashSet<String>) -> (Vec<Found>, usize, usize, Vec<Job>, HashMap<String, Vec<Owned>>, (Vec<String>, Vec<String>)) {
    let lib = o.library.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
    let part = |p: &Path, i: usize| -> String {
        let rel: Vec<String> = p.strip_prefix(&o.library).map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect()).unwrap_or_default();
        if i + 1 < rel.len() { rel[i].clone() } else { String::new() }
    };
    let leaf = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let aliases = aliases_from(&o.alias_file);

    // how many artist folders carry each comment or "encoded by" text: an odd one on many is an uploader's signature
    let mut carried: HashMap<String, HashSet<&str>> = HashMap::new();
    for t in tracks.iter().filter(|t| t.info.ok && t.parts.len() >= 2) {
        for (f, v) in &t.info.tags {
            if let Some(k) = rules::signature_key(f, v) {
                carried.entry(k).or_default().insert(t.parts[0].as_str());
            }
        }
    }
    let signatures = o.cache_dir.join("junk-signatures.txt");
    let spread: HashMap<String, usize> = if !o.only.is_empty() {
        // one folder cannot say how far a signature has spread: what the last whole check counted stands
        std::fs::read_to_string(&signatures).unwrap_or_default().lines().filter_map(|l| l.split_once('\t')).filter_map(|(n, k)| Some((k.to_string(), n.trim().parse().ok()?))).collect()
    } else {
        let spread: HashMap<String, usize> = carried.into_iter().map(|(k, v)| (k, v.len())).collect();
        // kept for the importer, so new downloads carrying the same signatures are cleaned too
        let lines: String = spread.iter().filter(|(_, n)| **n >= 3).map(|(k, n)| format!("{n}\t{k}\n")).collect();
        let _ = write_atomic(&signatures, lines.as_bytes());
        spread
    };

    let mut sh = shape(&o.library, tracks, genres, compilations);
    // a folder nobody listed whose albums are mostly by other artists is a folder of compilations too
    for ri in 0..sh.roots.len() {
        if sh.roots[ri].collection {
            continue;
        }
        let mut mains: Vec<String> = vec![];
        for u in sh.units.iter().filter(|u| u.root == Some(ri) && !u.loose) {
            let of = |f: &dyn Fn(&Track) -> &str| most_common(u.tracks.iter().map(|&i| &tracks[i]).filter(|t| t.info.ok).map(|t| f(t))).map(key);
            if let Some(m) = of(&|t| t.info.album_artist()).or(of(&|t| t.info.tag("artist"))).filter(|m| !m.is_empty()) {
                if !mains.contains(&m) {
                    mains.push(m);
                }
            }
        }
        let nf = key(&sh.roots[ri].name);
        let others = mains.iter().filter(|m| !m.contains(&nf) && !nf.contains(m.as_str())).count();
        if mains.len() >= 3 && others >= 3 && others * 5 >= mains.len() * 3 {
            sh.roots[ri].collection = true;
            sh.units.iter_mut().filter(|u| u.root == Some(ri)).for_each(|u| u.collection = true);
        }
    }
    // disc and track numbers, now that each track's album folder is known
    for u in &sh.units {
        let depth = u.folder.strip_prefix(&o.library).map(|r| r.components().count()).unwrap_or(0);
        for &i in &u.tracks {
            number(&mut tracks[i], depth);
        }
    }
    let tracks: &[Track] = tracks;
    // what LRCLIB was asked before (by an import or a repair): only read, nothing is asked by a check
    let kept = crate::importer::lyrics::Lookup { dir: o.lyrics_dir.clone(), online: false, timed: true, agent: ureq::Agent::new_with_defaults() };
    let asked: HashSet<String> = std::fs::read_dir(&o.lyrics_dir).into_iter().flatten().flatten().filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string())).collect();
    let mut r = Rules { tracks, spread, aliases, kept, asked, out: Out::default() };

    // the same artist in two folders
    let mut by_name: Vec<(String, Vec<&(String, PathBuf)>)> = vec![];
    for top in tops.iter().filter(|t| !genres.contains(&t.0.to_lowercase()) && !compilations.contains(&t.0.to_lowercase())) {
        let k = key(&top.0);
        let k = r.aliases.get(&k).cloned().unwrap_or(k);
        if k.is_empty() {
            continue;
        }
        match by_name.iter_mut().find(|e| e.0 == k) {
            Some(e) => e.1.push(top),
            None => by_name.push((k, vec![top])),
        }
    }
    for (_, same) in by_name.iter().filter(|e| e.1.len() > 1) {
        let d = format!("Folders: {}", same.iter().map(|t| format!("'{}'", t.0)).collect::<Vec<_>>().join(" / "));
        let folders = same.iter().map(|t| t.1.to_string_lossy().to_string()).collect::<Vec<_>>().join("|");
        r.out.add(2, "Same artist in two folders", &same[0].0, "", &same[0].1.to_string_lossy(), d).fix("merge-artist", vec![]).fact("folders", folders);
    }

    // leftover and empty folders, junk files
    for d in &walked.importing {
        if now().saturating_sub(newest_change(d, 0)) < ABANDONED_SECS {
            continue; // an import may be writing to it right now
        }
        let full = d.to_string_lossy().to_string();
        let parent = d.parent().map(leaf).unwrap_or_default();
        r.out.add(1, "Leftover import folder", &parent, &leaf(d), &full, "Half-finished import (the importer was interrupted). Safe to remove; the original download was not removed.".into()).fix("remove", vec![full.clone()]);
    }
    for d in &walked.empty {
        let full = d.to_string_lossy().to_string();
        let top = d.strip_prefix(&o.library).ok().and_then(|r| r.components().next()).map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
        r.out.add(3, "Empty folder", &top, &leaf(d), &full, "Nothing in it".into()).fix("remove", vec![full.clone()]);
    }
    for f in &walked.other {
        let (ext, name) = (ext_of(f), leaf(f));
        let why = if is_repair_leftover(f) {
            "is left over from a repair that was cut off"
        } else if JUNK_EXT.contains(&ext.as_str()) || name.starts_with("~$") {
            "looks like an unfinished download or temp file"
        } else {
            continue;
        };
        let dir = f.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
        r.out.add(3, "Junk file", &part(f, 0), &part(f, 1), &dir, format!("'{name}' {why}")).fix("remove", vec![f.to_string_lossy().to_string()]);
    }

    // per file: unreadable, and paths too long for some programs
    let mut long: Vec<(String, Vec<&Track>)> = vec![];
    for t in tracks {
        let (artist, album) = (part(&t.path, 0), part(&t.path, 1));
        if !t.info.ok {
            let why = if t.info.error.is_empty() { "can't be read" } else { &t.info.error };
            r.out.add(1, "Unreadable file", &artist, &album, &t.full, format!("'{}': {why}", t.name));
            continue;
        }
        if t.full.encode_utf16().count() > MAX_PATH {
            let dir = t.path.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
            match long.iter_mut().find(|e| e.0 == dir) {
                Some(e) => e.1.push(t),
                None => long.push((dir, vec![t])),
            }
        }
    }
    for (dir, ts) in &long {
        let len = |t: &Track| t.full.encode_utf16().count();
        let worst = *ts.iter().max_by_key(|t| len(t)).unwrap();
        let d = if ts.len() == 1 {
            format!("'{}' path is {} characters; some programs can't open paths over 260", worst.name, len(worst))
        } else {
            format!("{} files here have paths over {MAX_PATH} characters (the longest, '{}', is {}); some programs can't open paths over 260", ts.len(), worst.name, len(worst))
        };
        r.out.add(3, "Path too long", &part(&ts[0].path, 0), &part(&ts[0].path, 1), dir, d).f = Rules::files(ts);
    }
    // the albums each artist with loose tracks has (for saying which album a loose track comes from)
    let mut owned: HashMap<String, Vec<Owned>> = HashMap::new();
    // files lying in the library folder itself belong to no artist
    let stray: Vec<&Track> = tracks.iter().filter(|t| t.parts.len() < 2 && t.info.ok).collect();
    if !stray.is_empty() {
        let names: Vec<String> = stray.iter().map(|t| t.name.clone()).collect();
        let d = if stray.len() == 1 { format!("{} isn't in an artist folder", names[0]) } else { format!("{} tracks aren't in an artist folder: {}", stray.len(), list_of(&names, 3)) };
        // each one's title, artist and album tag, as for loose tracks in an artist's folder; here the artist
        // is the file's own tag, since no folder says whose it is
        let artist_of = |t: &Track| [t.info.album_artist(), t.info.tag("artist")].into_iter().find(|a| !a.trim().is_empty()).unwrap_or("").trim().to_string();
        let rows: Vec<Value> = stray
            .iter()
            .map(|t| {
                let title = if t.title().is_empty() { crate::importer::scan::parse_file_name(&crate::importer::util::file_stem(&t.name), "").title.unwrap_or_default() } else { t.title().to_string() };
                json!([t.full, title, artist_of(t), t.info.tag("album"), format!("{:.1}", t.info.duration)])
            })
            .collect();
        // the folder each tagged artist has (or would get), and the albums in it with their songs
        let (mut dirs, mut theirs) = (serde_json::Map::new(), vec![]);
        for artist in stray.iter().map(|t| artist_of(t)).filter(|a| !a.is_empty()) {
            let ak = key(&artist);
            if ak.is_empty() || dirs.contains_key(&ak) {
                continue;
            }
            let root = sh.roots.iter().position(|root| key(&root.name) == ak);
            let dir = match root {
                Some(ri) => sh.roots[ri].dir.clone(),
                None => o.library.join(crate::importer::util::clean_name(&artist)),
            };
            dirs.insert(ak.clone(), Value::String(dir.to_string_lossy().to_string()));
            for a in sh.units.iter().filter(|a| !a.loose && root.is_some() && a.root == root) {
                theirs.push(Owned {
                    key: format!("{ak}|{}", crate::importer::util::album_key(&a.album)),
                    folder: a.folder.to_string_lossy().to_string(),
                    songs: a.tracks.iter().map(|&i| &tracks[i]).filter(|t| t.info.ok).map(|t| (song_name(t), t.info.duration)).filter(|s| !s.0.is_empty()).collect(),
                });
            }
        }
        r.out.add(3, "Loose tracks", "", "", &lib, d).fix("file-loose", Rules::files(&stray)).fact("loose", Value::Array(rows).to_string()).fact("artist-dirs", Value::Object(dirs).to_string());
        owned.insert(lib.to_lowercase(), theirs);
    }

    // album by album
    let (mut albums, mut jobs) = (0, vec![]);
    let mut album_tops: Vec<String> = vec![];
    for u in &sh.units {
        let folder = u.folder.to_string_lossy().to_string();
        if u.loose {
            let list: Vec<&Track> = u.tracks.iter().map(|&i| &tracks[i]).filter(|t| t.info.ok).collect();
            if list.is_empty() {
                continue;
            }
            let names: Vec<String> = list.iter().map(|t| t.name.clone()).collect();
            let d = if list.len() == 1 { format!("{} isn't in an album folder", names[0]) } else { format!("{} tracks aren't in an album folder: {}", list.len(), list_of(&names, 3)) };
            // what the page needs to say which album each one comes from: its file, title, artist and album tag
            let rows: Vec<Value> = list
                .iter()
                .map(|t| {
                    let title = if t.title().is_empty() { crate::importer::scan::parse_file_name(&crate::importer::util::file_stem(&t.name), "").title.unwrap_or_default() } else { t.title().to_string() };
                    let by = [t.info.album_artist(), t.info.tag("artist"), u.artist.as_str()].into_iter().find(|a| !a.is_empty()).unwrap_or("").to_string();
                    json!([t.full, title, by, t.info.tag("album"), format!("{:.1}", t.info.duration)])
                })
                .collect();
            r.out.add(3, "Loose tracks", &u.artist, "", &folder, d).fix("file-loose", Rules::files(&list)).fact("loose", Value::Array(rows).to_string());
            // the albums this artist has, and the songs in each: whether a loose track's album is here, and holds it
            let theirs = sh.units.iter().filter(|a| !a.loose && a.root == u.root && u.root.is_some()).map(|a| Owned {
                key: crate::importer::util::album_key(&a.album),
                folder: a.folder.to_string_lossy().to_string(),
                songs: a.tracks.iter().map(|&i| &tracks[i]).filter(|t| t.info.ok).map(|t| (song_name(t), t.info.duration)).filter(|s| !s.0.is_empty()).collect(),
            });
            owned.insert(folder.to_lowercase(), theirs.collect());
            r.quality(&u.artist, "(loose tracks)", &folder, &list, true);
            r.junk(&u.artist, "(loose tracks)", &folder, &list);
            r.garbled(&u.artist, "(loose tracks)", &folder, &list);
            continue;
        }
        if u.tracks.iter().any(|&i| tracks[i].info.ok) {
            albums += 1;
            album_tops.push(top_key(&o.library, &u.folder));
        }
        r.album(u);
        jobs.extend(job_of(u, tracks, &o.library));
    }

    // one artist, the same album twice: the app's own same-album rule, and never two different years
    for (ri, root) in sh.roots.iter().enumerate().filter(|(_, r)| !r.collection) {
        let mut groups: Vec<(String, Vec<&Unit>)> = vec![];
        for u in sh.units.iter().filter(|u| u.root == Some(ri) && !u.loose && !library::is_disc_like(&u.album)) {
            let k = crate::importer::util::album_key(&u.album);
            if k.is_empty() {
                continue;
            }
            match groups.iter_mut().find(|g| g.0 == k) {
                Some(g) => g.1.push(u),
                None => groups.push((k, vec![u])),
            }
        }
        for (_, same) in groups.iter().filter(|g| g.1.len() > 1) {
            // folders that name different years are different records that share a title
            let mut by_year: Vec<(Option<i32>, Vec<&Unit>)> = vec![];
            for u in same {
                let y = crate::util::folder_year(&u.album);
                match by_year.iter_mut().find(|e| e.0 == y || e.0.is_none() || y.is_none()) {
                    Some(e) => {
                        e.0 = e.0.or(y);
                        e.1.push(u);
                    }
                    None => by_year.push((y, vec![u])),
                }
            }
            for (_, twins) in by_year.iter_mut().filter(|e| e.1.len() > 1) {
                twins.sort_by_key(|u| u.album.to_lowercase());
                let names: Vec<String> = twins.iter().map(|u| format!("'{}' ({} tracks)", u.album, u.tracks.len())).collect();
                let folders = twins.iter().map(|u| u.folder.to_string_lossy().to_string()).collect::<Vec<_>>().join("|");
                r.out.add(2, "Same album twice?", &root.name, &twins[0].album, &root.dir.to_string_lossy(), format!("{}. Could be different editions; worth a look", names.join(" / "))).fix("pick-album", vec![]).fact("folders", folders);
            }
        }
    }

    let artists = sh.roots.len();
    let artist_tops: Vec<String> = sh.roots.iter().map(|root| top_key(&o.library, &root.dir)).collect();
    let mut issues = r.out.issues;
    issues.sort_by(|a, b| a.s.cmp(&b.s).then_with(|| a.a.to_lowercase().cmp(&b.a.to_lowercase())).then_with(|| a.l.to_lowercase().cmp(&b.l.to_lowercase())).then_with(|| a.c.cmp(&b.c)));
    (issues, albums, artists, jobs, owned, (album_tops, artist_tops))
}

/// One album an artist has, as the loose tracks beside it are held against it.
pub struct Owned {
    /// what decides that two names are the same album (`album_key`)
    key: String,
    folder: String,
    /// its songs, as `song_name` writes them, and how long each is in seconds (0 = not known)
    songs: Vec<(String, f64)>,
}

/// A track's song, for telling whether two files hold the same one: the title tag, else the title in the
/// file's name, without case, punctuation or brackets.
fn song_name(t: &Track) -> String {
    let title = if t.title().is_empty() { crate::importer::scan::parse_file_name(&crate::importer::util::file_stem(&t.name), "").title.unwrap_or_default() } else { t.title().to_string() };
    crate::importer::tracklist::norm_title(&title)
}

/// The key a looked-up song's album is kept under, beside the albums' track lists.
fn song_key(artist: &str, title: &str) -> String {
    format!("song|{}|{}", crate::importer::util::norm(artist), crate::importer::tracklist::norm_title(title))
}

/// An answer about a song's album that still stands (asked within a month, under these rules).
fn song_answer<'a>(known: &'a serde_json::Map<String, Value>, artist: &str, title: &str) -> Option<&'a Value> {
    known.get(&song_key(artist, title)).filter(|e| {
        e["Rules"].as_u64() == Some(recheck::RULES) && e["When"].as_str().and_then(|w| chrono::DateTime::parse_from_rfc3339(w).ok()).is_some_and(|w| chrono::Local::now().signed_duration_since(w) < chrono::Duration::days(30))
    })
}

fn loose_rows(i: &Found) -> Vec<Vec<String>> {
    i.x.get("loose").and_then(|t| serde_json::from_str::<Vec<Vec<String>>>(t).ok()).unwrap_or_default().into_iter().filter(|r| r.len() >= 4).collect()
}

/// The loose tracks with no album tag that have not been looked up yet: (artist, title), each once.
fn songs_to_ask(issues: &[Found], known: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for row in issues.iter().filter(|i| i.c == "Loose tracks").flat_map(loose_rows) {
        if row[3].trim().is_empty() && !row[1].trim().is_empty() && !row[2].trim().is_empty() && song_answer(known, &row[2], &row[1]).is_none() && !out.iter().any(|o| song_key(&o.0, &o.1) == song_key(&row[2], &row[1])) {
            out.push((row[2].clone(), row[1].clone()));
        }
    }
    out
}

/// Says, for every "Loose tracks" finding, which album each loose file comes from: its album tag, else
/// what a lookup by artist and title found (`known`). Files of one album are one group, so the page has one
/// control per album. For each group: whether the artist has that album (its folder), and which of the loose
/// files' songs that album already holds (the same title and the same length, as `loose::same_length` has it). Written as the finding's "albums" fact:
/// `[ { Album, Source ("tag", "Apple Music", "MusicBrainz", "" = not known), Search, Folder, Files, Held } ]`.
fn loose_albums(issues: &mut [Found], owned: &HashMap<String, Vec<Owned>>, known: &serde_json::Map<String, Value>) {
    struct Group {
        key: String,
        album: String,
        source: String,
        files: Vec<String>,
        /// each file's song and its length in seconds (0 = not known)
        songs: Vec<(String, f64)>,
        /// for files lying in the library folder itself: the artist their tags name, and that artist's folder
        artist: String,
        artist_dir: String,
    }
    for i in issues.iter_mut().filter(|i| i.c == "Loose tracks" && i.x.contains_key("loose")) {
        // files in the library folder itself: each artist's folder, by the artist's key (see `find_issues`)
        let dirs: Option<serde_json::Map<String, Value>> = i.x.get("artist-dirs").and_then(|t| serde_json::from_str(t).ok());
        let mut groups: Vec<Group> = vec![];
        for row in loose_rows(i) {
            let (mut album, mut source) = (row[3].trim().to_string(), "tag".to_string());
            if album.is_empty() {
                let found = song_answer(known, &row[2], &row[1]);
                album = found.and_then(|e| e["Album"].as_str()).unwrap_or("").to_string();
                source = match (album.is_empty(), found) {
                    (true, Some(_)) => "not found".to_string(),
                    (true, None) => String::new(),
                    (false, _) => found.and_then(|e| e["Source"].as_str()).unwrap_or("").to_string(),
                };
            }
            let (artist, artist_key) = if dirs.is_some() { (row[2].trim().to_string(), key(&row[2])) } else { Default::default() };
            let key = match (album.is_empty(), dirs.is_some()) {
                (true, _) => String::new(),
                (false, false) => crate::importer::util::album_key(&album),
                // one group per artist and album: two artists may each have a "Greatest Hits"
                (false, true) => format!("{artist_key}|{}", crate::importer::util::album_key(&album)),
            };
            let artist_dir = dirs.as_ref().and_then(|d| d.get(&artist_key)).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let song = (crate::importer::tracklist::norm_title(&row[1]), row.get(4).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0));
            match groups.iter_mut().find(|g| g.key == key) {
                Some(g) => {
                    g.files.push(row[0].clone());
                    g.songs.push(song);
                }
                None => groups.push(Group { key, album, source, files: vec![row[0].clone()], songs: vec![song], artist, artist_dir }),
            }
        }
        let theirs = owned.get(&i.p.to_lowercase());
        let out: Vec<Value> = groups
            .iter()
            .map(|g| {
                let have = theirs.and_then(|t| t.iter().find(|o| !g.key.is_empty() && o.key == g.key));
                let held: Vec<&String> = g.files.iter().zip(&g.songs).filter(|(_, s)| !s.0.is_empty() && have.is_some_and(|o| o.songs.iter().any(|h| h.0 == s.0 && crate::loose::same_length(h.1, s.1)))).map(|(f, _)| f).collect();
                json!({
                    "Album": g.album, "Source": g.source, "Files": g.files, "Held": held,
                    "Search": if g.album.is_empty() { String::new() } else { format!("{} {}", if g.artist.is_empty() { i.a.as_str() } else { g.artist.as_str() }, strip_edition_all(&strip_year(&g.album))).trim().to_string() },
                    "Folder": have.map(|o| o.folder.as_str()).unwrap_or(""),
                    "Artist": g.artist, "ArtistDir": g.artist_dir,
                })
            })
            .collect();
        i.x.insert("albums".into(), Value::Array(out).to_string());
    }
}

// ------------------------------------------------------------------ the check

/// Runs `f` on every item, `threads` at a time, until done or stopped.
fn each<T: Sync>(items: &[T], threads: usize, stop: &AtomicBool, f: impl Fn(usize, &T) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, items.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() || stop.load(Ordering::Relaxed) {
                    break;
                }
                f(i, &items[i]);
            });
        }
    });
}

/// The whole check, on the calling thread. Returns the report it wrote; None when it was stopped before
/// every track's tags were read (nothing is written then, so the last full report stands).
pub fn run(o: &Options, p: &Progress, ffmpeg: &Path, ffprobe: &Path) -> Result<Option<Value>, String> {
    run_with(o, p, ffmpeg, ffprobe, None, None)
}

/// What looking up the album of a song (artist, title) answers: Some((album, where it was found)), with an
/// empty album when it was looked for and not found; None when there was no answer at all.
pub type SongAsk<'a> = &'a dyn Fn(&str, &str) -> Option<(String, String)>;

/// `run`, with the comparison of an album with its official track list, and the lookup of a loose song's
/// album, given from outside (the tests', which ask nobody).
pub fn run_with(o: &Options, p: &Progress, ffmpeg: &Path, ffprobe: &Path, judge: Option<&dyn Fn(&Job) -> Option<(recheck::Outcome, String)>>, song: Option<SongAsk>) -> Result<Option<Value>, String> {
    let started = std::time::Instant::now();
    let _ = std::fs::create_dir_all(&o.cache_dir);
    let id = library_id(&o.library.to_string_lossy());
    let (tags_file, failed_file) = (o.cache_dir.join(format!("health-tags-{id}.txt")), o.cache_dir.join(format!("health-undecodable-{id}.txt")));
    let skip: HashSet<String> = o.skip_folders.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();

    // ---- finding files
    p.enter(0, 0);
    let rd = std::fs::read_dir(&o.library).map_err(|e| crate::util::plain_err(format!("Can't read {}: {e}", o.library.display())))?;
    let (mut walked, mut tops, mut genres, mut compilations) = (Walked::default(), vec![], HashSet::new(), HashSet::new());
    let mut entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for e in entries {
        let (name, path) = (e.file_name().to_string_lossy().to_string(), e.path());
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_symlink() || name.starts_with('.') {
            continue;
        }
        // a check of some folders leaves everything else unread
        if !o.only.is_empty() && !o.only.iter().any(|only| only.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&name))) {
            continue;
        }
        if !kind.is_dir() {
            if AUDIO_EXT.contains(&ext_of(&path).as_str()) {
                let m = e.metadata().ok();
                walked.audio.push((path, m.as_ref().map(|m| m.len()).unwrap_or(0), m.as_ref().map(ticks_of).unwrap_or(0)));
            }
            continue;
        }
        if hidden_or_system(&e) || name.eq_ignore_ascii_case("Automatically Add to Apple Music") {
            continue;
        }
        if skip.contains(&name.to_lowercase()) {
            if library::is_genre_folder(&name) { &mut genres } else { &mut compilations }.insert(name.to_lowercase());
        }
        walk(&path, 0, &mut walked, &p.cancel);
        tops.push((name, path));
    }
    if p.stopped() {
        return Ok(None);
    }
    walked.audio.sort_by_key(|a| a.0.to_string_lossy().to_lowercase());

    // ---- reading tags
    p.enter(1, walked.audio.len());
    let mut cache: HashMap<String, Info> = HashMap::new();
    load_tags(&tags_file, &mut cache, false);
    if let Some(seed) = &o.seed_tags {
        load_tags(seed, &mut cache, true);
    }
    let cache = Mutex::new(cache);
    let infos: Vec<OnceLock<Info>> = walked.audio.iter().map(|_| OnceLock::new()).collect();
    let fresh = AtomicUsize::new(0);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    each(&walked.audio, cores.max(2), &p.cancel, |i, (path, size, ticks)| {
        let k = file_key(&path.to_string_lossy(), *size, *ticks);
        let have = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&k).cloned();
        // (a file read before the check knew about lyrics has its lyrics looked at now, once: a few small reads)
        let looked = have.as_ref().is_some_and(|i| !i.ok || i.lyr.is_some());
        let mut info = have.unwrap_or_else(|| if *size == 0 { Info::failed("empty file (0 bytes)") } else { probe(ffprobe, path) });
        if !looked {
            if info.ok {
                info.lyr = Some(lyr_of(ffprobe, path, &info));
            }
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            c.insert(k, info.clone());
            if (fresh.fetch_add(1, Ordering::Relaxed) + 1) % 2000 == 0 {
                save_tags(&tags_file, &*c, None); // a check that is cut off keeps what it read
            }
        }
        let _ = infos[i].set(info);
        p.done.fetch_add(1, Ordering::Relaxed);
    });
    let cache = cache.into_inner().unwrap_or_else(|e| e.into_inner());
    if p.stopped() {
        save_tags(&tags_file, &cache, None);
        return Ok(None);
    }
    let mut tracks: Vec<Track> = walked
        .audio
        .iter()
        .zip(infos)
        .map(|((path, size, ticks), info)| {
            let info = info.into_inner().unwrap_or_default();
            let ext = ext_of(path);
            let parts = path.strip_prefix(&o.library).map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect()).unwrap_or_else(|_| vec![path.to_string_lossy().to_string()]);
            Track {
                full: path.to_string_lossy().to_string(),
                name: path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                lossless: LOSSLESS_EXT.contains(&ext.as_str()) || LOSSLESS_CODEC.is_match(&info.codec),
                path: path.clone(),
                parts,
                ext,
                size: *size,
                ticks: *ticks,
                info,
                q: None,
                disc: 1,
                no: 0,
            }
        })
        .collect();
    // only this library's files as they are now: tags of files that are gone or changed are dropped
    let seen: HashSet<String> = tracks.iter().map(|t| file_key(&t.full, t.size, t.ticks)).collect();
    // (a check of one folder has not seen the rest: nothing is dropped then)
    save_tags(&tags_file, &cache, if !o.only.is_empty() { None } else { Some(&seen) });
    drop(cache);

    // ---- the audio: what earlier checks found, and for a full check every track that has no answer yet
    quality::load_cache(&o.quality_cache);
    let mut failed = load_failed(&failed_file);
    let decodable = |t: &Track| t.info.ok && t.info.quirk.is_empty();
    for t in tracks.iter_mut().filter(|t| decodable(t)) {
        t.q = quality::cached_by_size(&t.path, true);
    }
    let waiting = |t: &Track, failed: &HashMap<String, (u64, String)>| decodable(t) && t.q.is_none() && !failed.contains_key(&file_key(&t.full, t.size, t.ticks));
    if o.deep {
        let todo: Vec<usize> = (0..tracks.len()).filter(|&i| waiting(&tracks[i], &failed)).collect();
        p.enter(2, todo.len());
        let threads = (cores / 2).max(2);
        let mut since_save = 0;
        for batch in todo.chunks(threads * 2) {
            if p.stopped() {
                break;
            }
            let files: Vec<PathBuf> = batch.iter().map(|&i| tracks[i].path.clone()).collect();
            let lengths: Vec<f64> = batch.iter().map(|&i| tracks[i].info.duration).collect();
            let found = quality::check_files(ffmpeg, &files, &lengths, &vec![true; batch.len()], threads);
            for (&i, q) in batch.iter().zip(found) {
                if q.check_error.is_empty() {
                    tracks[i].q = Some(q);
                } else {
                    failed.insert(file_key(&tracks[i].full, tracks[i].size, tracks[i].ticks), (now(), q.check_error));
                }
            }
            p.done.fetch_add(batch.len(), Ordering::Relaxed);
            since_save += batch.len();
            if since_save >= 400 {
                since_save = 0;
                quality::save_cache(&o.quality_cache);
                save_failed(&failed_file, &failed);
            }
        }
        quality::save_cache(&o.quality_cache);
        save_failed(&failed_file, &failed);
    }
    let deep_left = tracks.iter().filter(|t| waiting(t, &failed)).count();
    let unchecked = tracks.iter().filter(|t| decodable(t) && t.q.is_none()).count() - deep_left;

    // ---- the rules
    p.enter(3, 0);
    let files = tracks.len();
    // the same counts for each folder at the top of the library, so that a later check of some folders can put
    // its own in their place and the header stays true: tracks, albums, artists, could not be checked, not
    // listened to yet, albums waiting for their track list (the last is added at each write)
    let mut counts: HashMap<String, [usize; 6]> = HashMap::new();
    for t in &tracks {
        let c = counts.entry(top_key(&o.library, &t.path)).or_default();
        c[0] += 1;
        if waiting(t, &failed) {
            c[4] += 1;
        } else if decodable(t) && t.q.is_none() {
            c[3] += 1;
        }
    }
    let (mut issues, albums, artists, jobs, owned, (album_tops, artist_tops)) = find_issues(o, &mut tracks, &walked, &tops, &genres, &compilations);
    drop(tracks);
    for (at, list) in [(1, &album_tops), (2, &artist_tops)] {
        for top in list {
            counts.entry(top.clone()).or_default()[at] += 1;
        }
    }
    // the top folders of the albums whose track list has not been compared yet (set before each write)
    let lists_waiting: std::cell::RefCell<Vec<String>> = Default::default();
    let write = |issues: &mut Vec<Found>| -> Result<Value, String> {
        issues.sort_by(|a, b| a.s.cmp(&b.s).then_with(|| a.a.to_lowercase().cmp(&b.a.to_lowercase())).then_with(|| a.l.to_lowercase().cmp(&b.l.to_lowercase())).then_with(|| a.c.cmp(&b.c)));
        p.issues.store(issues.len(), Ordering::Relaxed);
        let lists_left = lists_waiting.borrow().len();
        let mut tops = counts.clone();
        for top in lists_waiting.borrow().iter() {
            tops.entry(top.clone()).or_default()[5] += 1;
        }
        if !o.only.is_empty() {
            // a check of some folders: its findings and its counts take the place of what the report said
            // about those folders
            let report = merged(&o.report, &o.only, issues, &tops)?;
            write_atomic(&o.report, report.to_string().as_bytes()).map_err(|e| crate::util::plain_err(format!("The check's results could not be saved: {e}")))?;
            p.wrote.store(true, Ordering::Relaxed);
            return Ok(report);
        }
        let report = json!({
            "When": chrono::Local::now().to_rfc3339(),
            "By": "music-recs",
            "Deep": deep_left == 0 && files > 0,
            "Cancelled": false,
            "Files": files, "Albums": albums, "Artists": artists,
            "Unchecked": unchecked, "DeepLeft": deep_left, "ListsLeft": lists_left,
            "Tops": tops,
            "Seconds": started.elapsed().as_secs_f64(),
            "Issues": issues.iter().map(|i| i.json()).collect::<Vec<_>>(),
        });
        if let Some(dir) = o.report.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        write_atomic(&o.report, report.to_string().as_bytes()).map_err(|e| crate::util::plain_err(format!("The check's results could not be saved: {e}")))?;
        p.wrote.store(true, Ordering::Relaxed);
        Ok(report)
    };

    // ---- the official track lists: what is already known about each album, then the rest one by one
    let mut known = recheck::read_all(&o.tracklists);
    let month = chrono::Duration::days(30);
    let mut pending: Vec<(&Job, String)> = vec![];
    for j in &jobs {
        let sig = recheck::sig_of(Path::new(&j.folder));
        let fresh = known.get(&j.folder.to_lowercase()).filter(|e| {
            e["Sig"].as_str() == Some(sig.as_str())
                && e["Rules"].as_u64() == Some(recheck::RULES)
                && e["When"].as_str().and_then(|w| chrono::DateTime::parse_from_rfc3339(w).ok()).is_some_and(|w| chrono::Local::now().signed_duration_since(w) < month)
                // an answer from before the real lengths were kept is asked again for an album with a damaged
                // file (the lookups themselves are saved, so that is quick)
                && (e.get("Real").is_some() || !wants_lengths(&issues, &j.folder))
        });
        match fresh {
            Some(e) => lay(&mut issues, &j.folder, e),
            None => pending.push((j, sig)),
        }
    }
    // everything found so far is on the page while the lists are asked for (one album every few seconds)
    // (which of them have been answered since, by their place in `pending`)
    let mut answered_at: HashSet<usize> = HashSet::new();
    let still_waiting = |answered_at: &HashSet<usize>| -> Vec<String> { pending.iter().enumerate().filter(|(n, _)| !answered_at.contains(n)).map(|(_, (j, _))| top_key(&o.library, Path::new(&j.folder))).collect() };
    *lists_waiting.borrow_mut() = still_waiting(&answered_at);
    // which album each loose track comes from, with what is already known about the ones that have no album tag
    loose_albums(&mut issues, &owned, &known);
    let mut report = write(&mut issues)?;
    let songs = songs_to_ask(&issues, &known);
    // the sites are only asked when no answers were given from outside (a test's), and there is something to ask
    let asks_sites = judge.is_none() && song.is_none() && o.online && !p.stopped() && (!pending.is_empty() || !songs.is_empty());
    let sites = asks_sites.then(|| {
        let mut c = crate::importer::ctx::Ctx::new(o.library.clone(), o.library.clone(), false, ffmpeg.to_path_buf(), ffprobe.to_path_buf());
        c.lookup_cache = Some(o.lookups.clone());
        c
    });
    let (online, online_song);
    let judge: Option<&dyn Fn(&Job) -> Option<(recheck::Outcome, String)>> = match (judge, &sites) {
        (Some(j), _) => Some(j),
        (None, Some(c)) if !pending.is_empty() => {
            online = move |j: &Job| {
                let before = c.net_failures.load(Ordering::Relaxed);
                let (outcome, search) = recheck::judge_album(c, &j.artist, &j.album, &j.reads);
                // "no list" after a lookup that got no answer says nothing about the album: asked again next time
                (c.net_failures.load(Ordering::Relaxed) == before || !outcome.source.is_empty()).then_some((outcome, search))
            };
            Some(&online)
        }
        _ => None,
    };
    let song: Option<SongAsk> = match (song, &sites) {
        (Some(s), _) => Some(s),
        (None, Some(c)) => {
            online_song = move |artist: &str, title: &str| {
                let before = c.net_failures.load(Ordering::Relaxed);
                let found = crate::importer::lookup::album_of_song(c, artist, title);
                // "not found" after a lookup that got no answer is not a fact about the song
                (found.is_some() || c.net_failures.load(Ordering::Relaxed) == before).then(|| found.unwrap_or_default())
            };
            Some(&online_song)
        }
        _ => None,
    };
    if let Some(judge) = judge {
        p.enter(4, pending.len());
        let (mut unsaved, mut unanswered, mut wrote_at, mut changed) = (0, 0, std::time::Instant::now(), false);
        for (at, (j, sig)) in pending.iter().enumerate() {
            if p.stopped() || unanswered >= 5 {
                break; // five albums in a row with no answer: the connection is down, the rest wait for the next check
            }
            match judge(j) {
                Some((outcome, search)) => {
                    unanswered = 0;
                    let album = recheck::Album { artist: j.artist.clone(), album: j.album.clone(), path: j.folder.clone() };
                    let entry = json!({
                        "Path": j.folder, "Sig": sig, "When": chrono::Local::now().to_rfc3339(), "Rules": recheck::RULES,
                        "Kind": format!("{:?}", outcome.kind), "List": outcome.source, "Fit": outcome.fit,
                        "Drop": recheck::dropped(&outcome), "Issues": if outcome.fit { recheck::issues_of(&album, &outcome, &search) } else { vec![] },
                        "Real": outcome.real.iter().map(|r| json!([r.d, r.n, r.t, r.s])).collect::<Vec<_>>(),
                    });
                    lay(&mut issues, &j.folder, &entry);
                    known.insert(j.folder.to_lowercase(), entry);
                    (unsaved, changed) = (unsaved + 1, true);
                    answered_at.insert(at);
                    if unsaved >= 5 {
                        recheck::save(&o.tracklists, &known);
                        unsaved = 0;
                    }
                }
                None => unanswered += 1,
            }
            p.done.fetch_add(1, Ordering::Relaxed);
            if changed && wrote_at.elapsed().as_secs() >= 60 {
                *lists_waiting.borrow_mut() = still_waiting(&answered_at);
                report = write(&mut issues)?;
                (wrote_at, changed) = (std::time::Instant::now(), false);
            }
        }
        recheck::save(&o.tracklists, &known);
        if changed {
            // the albums that got no answer (stopped, or the connection went) wait for the next check
            *lists_waiting.borrow_mut() = still_waiting(&answered_at);
            report = write(&mut issues)?;
        }
    }
    // ---- loose tracks with no album tag: the album each song comes from, looked up by artist and title
    if let Some(song) = song {
        let (mut unanswered, mut learned) = (0, false);
        for (artist, title) in &songs {
            if p.stopped() || unanswered >= 5 {
                break;
            }
            match song(artist, title) {
                Some((album, source)) => {
                    unanswered = 0;
                    known.insert(song_key(artist, title), json!({ "Album": album, "Source": source, "When": chrono::Local::now().to_rfc3339(), "Rules": recheck::RULES }));
                    learned = true;
                }
                None => unanswered += 1,
            }
        }
        if learned {
            recheck::save(&o.tracklists, &known);
            loose_albums(&mut issues, &owned, &known);
            report = write(&mut issues)?;
        }
    }
    Ok(Some(report))
}

/// Findings that can only be made with the whole library in view: a check of one folder neither makes nor
/// takes away one of these.
const WHOLE_LIBRARY: [&str; 1] = ["Same artist in two folders"];

/// The report on disk with everything it said about these folders replaced by what a check of those folders
/// alone has just found. The rest of the report and the date of the whole check stay as they were; the
/// counts follow, and `Updated` says when. An error when
/// there is no report of the app's own to change (a whole check has to come first).
fn merged(report: &Path, only: &[PathBuf], found: &[Found], counts: &HashMap<String, [usize; 6]>) -> Result<Value, String> {
    let mut root: Value = std::fs::read(report).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or("There is no report yet to bring up to date. Run a check first.")?;
    if root.get("By").and_then(|b| b.as_str()) != Some("music-recs") {
        return Err("There is no report yet to bring up to date. Run a check first.".into());
    }
    let folders: Vec<String> = only.iter().map(|f| f.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase()).collect();
    let inside = |p: &str| {
        let p = p.to_lowercase();
        folders.iter().any(|folder| p == *folder || p.strip_prefix(folder.as_str()).is_some_and(|rest| rest.starts_with(['\\', '/'])))
    };
    let whole = |c: &str| WHOLE_LIBRARY.contains(&c);
    let mut list: Vec<Value> = root["Issues"].as_array().cloned().unwrap_or_default();
    list.retain(|i| whole(i["C"].as_str().unwrap_or("")) || !inside(i["P"].as_str().unwrap_or("")));
    list.extend(found.iter().filter(|i| !whole(&i.c)).map(|i| i.json()));
    let sev = |v: &Value| v["S"].as_u64().unwrap_or(3);
    let low = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_lowercase();
    list.sort_by(|a, b| sev(a).cmp(&sev(b)).then_with(|| low(a, "A").cmp(&low(b, "A"))).then_with(|| low(a, "L").cmp(&low(b, "L"))).then_with(|| low(a, "C").cmp(&low(b, "C"))));
    root["Issues"] = Value::Array(list);
    // the header's facts: when the report was last brought up to date, and, where the whole check kept its
    // counts folder by folder, these folders' new counts in place of their old ones and the sums made again
    // (a report from before the counts were kept folder by folder keeps its sums until the next whole check)
    root["Updated"] = Value::String(chrono::Local::now().to_rfc3339());
    let sums: Option<Vec<u64>> = root.get_mut("Tops").and_then(|t| t.as_object_mut()).map(|tops| {
        for folder in only {
            let key = folder.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            tops.remove(&key);
            if let Some(c) = counts.get(&key) {
                tops.insert(key, json!(c));
            }
        }
        (0..6).map(|n| tops.values().map(|v| v[n].as_u64().unwrap_or(0)).sum()).collect()
    });
    if let Some(sums) = sums {
        for (name, n) in ["Files", "Albums", "Artists", "Unchecked", "DeepLeft", "ListsLeft"].iter().zip(sums) {
            root[*name] = json!(n);
        }
    }
    Ok(root)
}

/// One album as the comparison with its official track list wants it.
pub struct Job {
    pub artist: String,
    /// the album folder's name
    pub album: String,
    pub folder: String,
    pub reads: Vec<recheck::Read>,
}

/// The album's tracks for the comparison. None for a folder of compilations, an album with no readable
/// track, and one with more tracks than any single album has.
fn job_of(u: &Unit, tracks: &[Track], library: &Path) -> Option<Job> {
    use crate::importer::scan::{disc_from_path, lead_number, parse_file_name};
    let list: Vec<&Track> = u.tracks.iter().map(|&i| &tracks[i]).filter(|t| t.info.ok && t.info.quirk.is_empty()).collect();
    if u.collection || list.is_empty() || list.len() > 400 {
        return None;
    }
    let depth = u.folder.strip_prefix(library).map(|r| r.components().count()).unwrap_or(0);
    let reads = list
        .iter()
        .map(|t| {
            let inside = &t.parts[depth.min(t.parts.len() - 1)..];
            let parsed = parse_file_name(&crate::importer::util::file_stem(&t.name), "");
            let heard = t.q.as_ref().filter(|q| q.decoded_seconds > 0.0);
            recheck::Read {
                track: OurTrack {
                    file: t.full.clone(),
                    title: if t.title().is_empty() { parsed.title.clone().unwrap_or_default() } else { t.title().to_string() },
                    disc: lead_number(t.info.tag("disc")).or(disc_from_path(&inside.join("\\"))).or(parsed.disc).unwrap_or(0),
                    no: lead_number(t.info.tag("track")).or(parsed.track).unwrap_or(0),
                    secs: heard.map(|q| q.decoded_seconds).unwrap_or(t.info.duration),
                    errors: heard.map(|q| q.error_count).unwrap_or(0),
                },
                sub: if inside.len() > 1 { inside[0].clone() } else { String::new() },
                album_tag: t.info.tag("album").to_string(),
                artist_tags: [t.info.tag("album_artist"), t.info.tag("albumartist"), t.info.tag("artist")].into_iter().filter(|s| !s.is_empty()).map(|s| s.to_string()).collect(),
            }
        })
        .collect();
    Some(Job { artist: u.artist.clone(), album: u.album.clone(), folder: u.folder.to_string_lossy().to_string(), reads })
}

/// Lays what the comparison with the official track list found for one album over the check's own findings:
/// the kinds it judged better are taken out and its own are added. A list that names the missing songs
/// stands in for the gap in the track numbers.
fn lay(issues: &mut Vec<Found>, folder: &str, entry: &Value) {
    let theirs: Vec<Found> = entry["Issues"].as_array().map(|a| a.iter().filter_map(Found::from_json).collect()).unwrap_or_default();
    let mut drop: Vec<&str> = entry["Drop"].as_array().map(|a| a.iter().filter_map(|c| c.as_str()).collect()).unwrap_or_default();
    if theirs.iter().any(|f| f.c == "Incomplete album") {
        drop.push(recheck::GAPS);
    }
    let folder = folder.to_lowercase();
    issues.retain(|i| !(i.p.to_lowercase() == folder && drop.contains(&i.c.as_str())));
    issues.extend(theirs);
    second_look(issues, &folder, entry);
}

/// The findings the second look can say more about: the ones that carry their tracks' real playing times.
const SECOND_LOOK: [&str; 2] = ["Damaged or incomplete", "Length mismatch"];

fn wants_lengths(issues: &[Found], folder: &str) -> bool {
    let folder = folder.to_lowercase();
    issues.iter().any(|i| SECOND_LOOK.contains(&i.c.as_str()) && i.x.contains_key("tracks") && i.p.to_lowercase() == folder)
}

/// The second look at damaged files. The check knows how long each flagged track really plays and what its
/// own header claims, but a header is only a claim. Once the album has been matched with an official track
/// list, each of those tracks is held against the real song's length:
///   * it stops well short of the real song and has decode errors: "cut short" (Damaged or incomplete)
///   * it stops short but decodes cleanly: "possibly cut short" (worth a listen: it may be another version)
///   * it plays the whole song: only the length shown in players is wrong (Length mismatch, fixable in place)
/// `folder` is lower case. A track with no match on the list, and a lossless file with any decode error,
/// keep the finding as serious as it was.
fn second_look(issues: &mut Vec<Found>, folder: &str, entry: &Value) {
    use crate::importer::tracklist::{find_real_track, RealTrack};
    let real: Vec<RealTrack> = entry["Real"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| Some(RealTrack { d: r[0].as_u64()? as u32, n: r[1].as_u64()? as u32, t: r[2].as_str()?.to_string(), s: r[3].as_f64()? })).collect())
        .unwrap_or_default();
    let src = entry["List"].as_str().unwrap_or("");
    if real.is_empty() || src.is_empty() {
        return;
    }
    let mut touched = false;
    for i in issues.iter_mut().filter(|i| SECOND_LOOK.contains(&i.c.as_str()) && i.p.to_lowercase() == folder) {
        let Some(rows) = i.x.get("tracks").and_then(|t| serde_json::from_str::<Vec<Vec<String>>>(t).ok()).filter(|r| !r.is_empty() && r.iter().all(|row| row.len() >= 7)) else { continue };
        let (mut lines, mut worst, mut all_fine, mut checked) = (vec![], 3u8, true, 0);
        for row in &rows {
            let file = Path::new(&row[0]).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let num = |s: &String| s.trim().parse::<f64>().unwrap_or(0.0);
            let (plays, header, errors) = (num(&row[4]), num(&row[5]), num(&row[6]) as u32);
            let Some(rt) = find_real_track(&real, num(&row[2]) as u32, num(&row[3]) as u32, &row[1]) else {
                lines.push(format!("{file}: plays {} (no matching track on {src})", time(plays)));
                all_fine = false;
                worst = worst.min(i.s);
                continue;
            };
            checked += 1;
            let estimate = if header > rt.s + 10.0 { format!(" (the {} shown in players is only an estimate)", time(header)) } else { String::new() };
            let lossless = LOSSLESS_EXT.contains(&ext_of(Path::new(&row[0])).as_str());
            if plays < rt.s - 5.0 && errors > 0 {
                lines.push(format!("{file}: cut short, plays {} of the real {} ({src}) before breaking{estimate}", time(plays), time(rt.s)));
                (worst, all_fine) = (1, false);
            } else if plays < rt.s - 5.0 {
                // decodes cleanly but shorter than the real song: broken off, or simply another version (edit, live, demo)
                lines.push(format!("{file}: possibly cut short, plays {}, the {src} version is {} (could also be a different edit or version){estimate}", time(plays), time(rt.s)));
                (worst, all_fine) = (worst.min(2), false);
            } else if plays > rt.s + 10.0 && errors > 0 {
                lines.push(format!("{file}: garbled, plays {} of broken audio, but the real track is {} ({src}){estimate}", time(plays), time(rt.s)));
                (worst, all_fine) = (1, false);
            } else if errors >= 10 {
                lines.push(format!("{file}: full length ({}, real {}) but {errors} decode errors, likely audible glitches", time(plays), time(rt.s)));
                (worst, all_fine) = (1, false);
            } else if lossless {
                // a lossless file is only flagged when it fails its own exact check (checksum, sample count, a
                // decode error): full length does not make it sound, so it stays as serious as it was
                let why = if errors > 0 { format!("{errors} decode {} in a lossless file", if errors == 1 { "error" } else { "errors" }) } else { "the file does not pass its own check".to_string() };
                lines.push(format!("{file}: full length ({}, real {}) but {why}", time(plays), time(rt.s)));
                (worst, all_fine) = (worst.min(i.s), false);
            } else {
                lines.push(format!("{file}: plays the full {} (real track: {}, {src}), only the length shown in players is wrong", time(plays), time(rt.s)));
            }
        }
        let detail = list_of(&lines, 4);
        touched = true;
        if all_fine && checked > 0 {
            // every flagged track plays in full: a wrong length header, fixable without touching the audio
            (i.c, i.s, i.d) = ("Length mismatch".into(), 3, format!("{detail}. The fix adds the missing length header"));
            i.fix("vbr-header", rows.iter().map(|r| r[0].clone()).collect());
            i.x.remove("search");
            continue;
        }
        // the worst outcome decides: 1 = really damaged, 2 = possibly cut short, 3 = as it was
        let tail = match worst {
            1 if lines.len() == 1 => ". This one needs a new download",
            1 => ". These need a new download",
            2 => ". Have a listen, and get a new copy if it ends abruptly",
            _ => "",
        };
        i.d = format!("{detail}{tail}");
        if worst <= 2 {
            (i.c, i.s, i.k) = (if worst == 1 { "Damaged or incomplete" } else { "Possibly cut short" }.to_string(), worst, "redownload".into());
            if !i.x.contains_key("search") {
                let words = search_words(&i.a, &i.l, &i.p);
                i.fact("search", words);
            }
        }
    }
    if !touched {
        return;
    }
    // one finding per kind and folder (each is ignored by that pair): two that are now of one kind become one
    let mut n = 0;
    while n < issues.len() {
        let twin = (issues[n].p.to_lowercase() == folder).then(|| issues[..n].iter().position(|o| o.c == issues[n].c && o.p.to_lowercase() == folder)).flatten();
        match twin {
            Some(first) => {
                let later = issues.remove(n);
                let keep = &mut issues[first];
                keep.d = format!("{}; {}", keep.d.trim_end_matches('.'), later.d);
                keep.s = keep.s.min(later.s);
                for f in later.f {
                    if !keep.f.contains(&f) {
                        keep.f.push(f);
                    }
                }
            }
            None => n += 1,
        }
    }
}

fn hidden_or_system(e: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    e.metadata().is_ok_and(|m| m.file_attributes() & 0x6 != 0)
}

/// Starts a check in the background. `repaint` is called whenever there is something new to show.
pub fn start(o: Options, repaint: impl Fn() + Send + Sync + 'static) -> Result<Arc<Progress>, String> {
    let ffmpeg = tools::find_tool("ffmpeg")?;
    let ffprobe = tools::find_tool("ffprobe")?;
    let progress = Arc::new(Progress::default());
    let p = progress.clone();
    std::thread::spawn(move || {
        // the page shows the numbers moving without being asked to redraw for every file
        let (alive, tick) = (Arc::new(AtomicBool::new(true)), Arc::new(repaint));
        let (a, t) = (alive.clone(), tick.clone());
        std::thread::spawn(move || {
            while a.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(400));
                (*t)();
            }
        });
        if let Err(why) = run(&o, &p, &ffmpeg, &ffprobe) {
            *p.error.lock().unwrap_or_else(|e| e.into_inner()) = why;
        }
        p.phase.store(5, Ordering::Relaxed);
        // a check of the whole library has ended (the small one of a few folders is not worth a word)
        if o.only.is_empty() {
            crate::links::job_ended("health check");
        }
        p.finished.store(true, Ordering::Relaxed);
        alive.store(false, Ordering::Relaxed);
        (*tick)();
    });
    Ok(progress)
}

#[cfg(test)]
mod tests;
