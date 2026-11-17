//! The tag editor used by imports and Health repairs. Its fixture cases preserve the reviewed results
//! of the former editor comparison, including the deliberate fixes to damaged and uncommon tags.
//!
//! For every file in the plan: the tags are changed on a second copy beside it, the audio is checked to
//! be byte for byte the same (found by hand, never by the library that wrote the tags) and every change
//! to read back, and only then does the copy take the place of the file. Lyrics, ratings and DJ data are
//! kept (a download's rating, play count and DJ data are another person's and go when the plan says so).
//! It edits ONLY the files named in the plan. MP3 tags go through the `id3` crate, which keeps the file's
//! own ID3 version and every frame in it; FLAC, M4A and the rest through lofty.
mod bytes;
mod doc;
mod other;
#[cfg(test)]
mod compare_tests;

use doc::{Doc, JunkAt};
use id3::{Content, TagLike};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One file's instructions, as the plan gives them (the established plan format).
#[derive(Debug, Default, Clone)]
struct Step {
    file: PathBuf,
    set: Vec<(String, String)>,
    junk: Vec<(String, String)>,
    cover: Option<PathBuf>,
    cover_all: bool,
    vbr: bool,
    carry_from: Option<PathBuf>,
    skip: Vec<String>,
    lyrics: String,
    lyrics_mode: String,
    unhide: bool,
    exact: bool,
    only_if: Option<String>,
    drop_personal: bool,
}

impl Step {
    fn from(v: &Value) -> Step {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        let b = |k: &str| v[k].as_bool().unwrap_or(false);
        let text = |x: &Value| match x {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            o => o.to_string(),
        };
        Step {
            file: PathBuf::from(s("File").unwrap_or_default()),
            set: v["Set"].as_object().map(|m| m.iter().map(|(k, x)| (k.clone(), text(x))).collect()).unwrap_or_default(),
            junk: v["Junk"].as_array().map(|a| a.iter().filter_map(|j| Some((text(j.get(0)?), text(j.get(1)?)))).collect()).unwrap_or_default(),
            cover: s("Cover").filter(|c| !c.is_empty()).map(PathBuf::from),
            cover_all: v["CoverMode"].as_str() == Some("all"),
            vbr: b("Vbr"),
            carry_from: s("CarryFrom").filter(|c| !c.is_empty()).map(PathBuf::from),
            skip: v["Skip"].as_array().map(|a| a.iter().filter_map(|k| k.as_str()).filter(|k| !k.is_empty()).map(|k| k.to_lowercase()).collect()).unwrap_or_default(),
            lyrics: v["Lyrics"].as_str().unwrap_or("").to_string(),
            lyrics_mode: v["LyricsMode"].as_str().unwrap_or("").to_string(),
            unhide: b("Unhide"),
            exact: b("Exact"),
            only_if: v.get("OnlyIf").filter(|x| !x.is_null()).map(text),
            drop_personal: b("DropPersonal"),
        }
    }
}

/// Runs the plan (four files at a time) and answers per file:
/// `{ File, Ok, Why, Note, OldLyrics }`. "nothing needed changing" counts as Ok.
pub fn run(plan: &[Value], ffmpeg: &Path) -> Vec<Value> {
    let steps: Vec<Step> = plan.iter().map(Step::from).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let answers = std::sync::Mutex::new(vec![Value::Null; steps.len()]);
    std::thread::scope(|sc| {
        for _ in 0..4.min(steps.len()) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(s) = steps.get(i) else { break };
                let a = answer(s, ffmpeg);
                answers.lock().unwrap_or_else(|e| e.into_inner())[i] = a;
            });
        }
    });
    answers.into_inner().unwrap_or_else(|e| e.into_inner())
}

fn answer(s: &Step, ffmpeg: &Path) -> Value {
    let mut old: Option<String> = None;
    // (a panic in a library is an answer too, never a lost file)
    let why = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| edit(s, ffmpeg, &mut old))) {
        Ok(Ok(())) => String::new(),
        Ok(Err(w)) => w,
        Err(p) => p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "the tag editor failed".into()),
    };
    let ok = why.is_empty() || why == NOTHING;
    let mut note = String::new();
    if ok && s.vbr {
        note = match bytes::vbr_header(&s.file, ffmpeg) {
            None => "added the missing length header".into(),
            Some(v) => format!("length header not fixed: {v}"),
        };
    }
    let old = if why.is_empty() { old } else { None };
    serde_json::json!({ "File": s.file.to_string_lossy(), "Ok": ok, "Why": why, "Note": note, "OldLyrics": old })
}

const NOTHING: &str = "nothing needed changing";

/// The second copy beside the file: "name.repairtmp.ext" (the real extension last, so its kind is known).
pub(crate) fn temp_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    path.with_file_name(format!("{stem}.repairtmp{ext}"))
}

/// Text compared for tag changes: runs of white space as one space, trimmed, and
/// with no regard to upper and lower case.
fn same(a: &str, b: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    n(a) == n(b)
}

/// Applies one step to its file, safely. Ok(()) when changed and checked; Err(why) otherwise ("nothing
/// needed changing" included). `old`: what the lyrics field held, when this changed it.
fn edit(s: &Step, ffmpeg: &Path, old: &mut Option<String>) -> Result<(), String> {
    let path = &s.file;
    if !path.exists() {
        return Err("file no longer exists".into());
    }
    if bytes::locked(path) {
        return Err("file is open in another program".into());
    }
    if !Doc::writable(path) {
        return Err("tags of this kind of file can't be changed here, so it was left alone".into());
    }
    let original = std::fs::read(path).map_err(|e| format!("the file could not be read ({e})"))?;
    if let (Some(dirty), _) = bytes::id3_clean(path, &original) {
        return Err(dirty);
    }
    let tmp = temp_path(path);
    let result = (|| {
        std::fs::write(&tmp, &original).map_err(|e| format!("a working copy could not be made ({e})"))?;
        let h1 = bytes::audio_hash(path, ffmpeg);
        if h1.is_none() {
            return Err("its tags are malformed, so it was left alone (its audio could not be located)".into());
        }
        let mut dropped_stacked = false;
        if !s.junk.is_empty() && bytes::is_mp3(path) {
            // junk in a second, stacked tag: that tag goes, but only if everything in it is junk or a copy of the main tag
            if let Some((start, len, frames)) = bytes::stacked_id3(&original) {
                let main = Doc::open(&tmp).map_err(malformed)?;
                let main_frames: Vec<&id3::Frame> = main.id3().map(|t| t.frames().collect()).unwrap_or_default();
                let main_text: Vec<String> = main_frames.iter().filter_map(|f| frame_text(f)).filter(|t| !t.is_empty()).collect();
                let main_ids: Vec<&str> = main_frames.iter().map(|f| f.id()).collect();
                let junk_vals: Vec<String> = s.junk.iter().map(|j| j.1.trim().to_lowercase()).collect();
                // players read the main tag and ignore a second one, so a field the main tag has adds nothing;
                // only a kind of field the main tag lacks entirely counts as worth keeping
                let unique: Vec<_> = frames
                    .iter()
                    .filter(|f| !f.text.is_empty() && !junk_vals.contains(&f.text.to_lowercase()) && !main_text.iter().any(|t| t.eq_ignore_ascii_case(&f.text)) && !main_ids.contains(&f.id.as_str()) && !["TSSE", "TENC", "COMM", "TXXX"].contains(&f.id.as_str()))
                    .collect();
                if !unique.is_empty() {
                    let some: Vec<String> = unique.iter().take(2).map(|f| format!("{} '{}'", f.id, short(&f.text, 30))).collect();
                    return Err(format!("it has a second tag holding other info ({}), so it was left alone", some.join(", ")));
                }
                let out = [&original[..start], &original[start + len..]].concat();
                std::fs::write(&tmp, out).map_err(|e| format!("a working copy could not be made ({e})"))?;
                dropped_stacked = true;
            }
        }
        let others = if s.exact {
            // a lyrics-only change that keeps no copy of the old file: it must be an MP3, and every other
            // entry of its tag is compared before and after
            if !bytes::is_mp3(path) {
                return Err("only an MP3's tag can be checked entry by entry, so it was left alone".into());
            }
            Some(other_frames(&Doc::open(path).map_err(malformed)?))
        } else {
            None
        };
        let mut tf = Doc::open(&tmp).map_err(malformed)?;
        let mut want: BTreeMap<String, String> = BTreeMap::new();
        let lyrics_before = tf.real_lyrics();
        if let Some(from) = &s.carry_from {
            for (k, v) in copy_tags(&mut tf, from, &s.skip, s.drop_personal)? {
                want.insert(k, v);
            }
            // ffmpeg writes a FLAC's track as "3/12" in one entry; a FLAC keeps the number and the total
            // apart, and some players read nothing else (the same for the disc)
            if tf.is_flac() {
                for (n, t, key, tkey) in [("track", "track total", "TRACKNUMBER", "trackcount"), ("disc", "disc total", "DISCNUMBER", "disccount")] {
                    if tf.vorbis_first(key).contains('/') {
                        let (no, of) = (tf.get(n), tf.get(t));
                        tf.set(t, &of);
                        tf.set(n, &no);
                        want.insert(n.into(), no_zero(&no));
                        want.insert(tkey.into(), no_zero(&of));
                    }
                }
            }
        }
        for (k, v) in &s.set {
            let field = match k.as_str() {
                "album_artist" => "album artist",
                "trackcount" => "track total",
                "disccount" => "disc total",
                "album" | "artist" | "title" | "track" | "disc" | "year" | "genre" | "compilation" => k.as_str(),
                _ => continue,
            };
            tf.set(field, v);
            want.insert(k.clone(), v.clone());
        }
        let mut removed = 0;
        if !s.junk.is_empty() {
            let pairs = tf.junk_pairs();
            let mut out: Vec<JunkAt> = vec![];
            for (field, value) in &s.junk {
                let field = if field == "comments" { "comment" } else { field.as_str() };
                for p in pairs.iter().filter(|p| p.key.eq_ignore_ascii_case(field)) {
                    // the exact value; the ID3v1 copy is cut to 28-30 characters, so it matches by its start
                    let v1 = p.at == JunkAt::V1Comment && p.value.len() >= 20 && value.starts_with(p.value.trim());
                    if same(&p.value, value) || v1 {
                        out.push(p.at.clone());
                        removed += 1;
                    }
                }
            }
            tf.remove_junk(&out);
        }
        if let Some(cover) = &s.cover {
            if s.cover_all || !tf.has_front() {
                let data = std::fs::read(cover).map_err(|e| format!("the cover picture could not be read ({e})"))?;
                tf.put_front(data, doc::mime_of(cover), &cover.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                want.insert("cover".into(), "1".into());
            }
        }
        // lyrics: out of a tag that hides them, then what the plan says (never over words that are there, unless told to)
        let mut stood = 0;
        // (the text tags a conversion left go while the fields still hold what was carried: each only when
        // its field holds the same value)
        if s.carry_from.is_some() {
            stood += remove_stand_ins(&mut tf, false);
        }
        if s.drop_personal {
            stood += tf.personal(true);
        }
        if s.unhide && tf.real_lyrics().is_empty() {
            let hidden = tf.hidden_lyrics();
            if !hidden.trim().is_empty() {
                tf.set_lyrics(&hidden);
                want.insert("lyrics".into(), hidden);
            }
        }
        if s.unhide {
            stood += remove_stand_ins(&mut tf, true);
        }
        // undoing a lyrics repair: only a file whose lyrics are still what the repair wrote is put back
        if let Some(only_if) = &s.only_if {
            let now = tf.real_lyrics();
            let target = if s.lyrics_mode == "clear" { "" } else { s.lyrics.as_str() };
            if same(&now, target) {
                return Err(NOTHING.into());
            }
            if !same(&now, only_if) {
                return Err("its lyrics were changed after the repair, so they were left as they are".into());
            }
        }
        let forget = |w: &mut BTreeMap<String, String>| {
            for k in ["carry lyrics", "frames USLT", "frames SYLT"] {
                w.remove(k);
            }
        };
        if s.lyrics_mode == "clear" {
            if !tf.real_lyrics().is_empty() {
                tf.clear_lyrics();
                want.insert("lyrics".into(), String::new());
                forget(&mut want);
            }
        } else if !s.lyrics.trim().is_empty() && (s.lyrics_mode == "set" || tf.real_lyrics().is_empty()) && !same(&tf.real_lyrics(), &s.lyrics) {
            if !tf.real_lyrics().is_empty() {
                tf.clear_lyrics(); // the old words go, with any timed copy of them
            }
            tf.set_lyrics(&s.lyrics);
            want.insert("lyrics".into(), s.lyrics.clone());
            forget(&mut want);
        }
        if want.is_empty() && removed == 0 && !dropped_stacked && stood == 0 {
            return Err(NOTHING.into());
        }
        if want.contains_key("lyrics") {
            *old = Some(lyrics_before.clone());
        }
        tf.save(&tmp)?;
        drop(tf);

        // --- the copy is checked before it replaces anything: the audio bytes must be exactly the same ---
        let h2 = bytes::audio_hash(&tmp, ffmpeg);
        if h1 != h2 {
            return Err("the audio data would have changed, so nothing was saved".into());
        }
        let mut chk = Doc::open(&tmp).map_err(|e| format!("the saved tags could not be read back ({e}), so nothing was saved"))?;
        if s.drop_personal && chk.personal(false) > 0 {
            return Err("another person's rating or DJ data would have stayed in it, so nothing was saved".into());
        }
        check(&chk, &want)?;
        if let Some(before) = others {
            if other_frames(&chk) != before {
                return Err("another entry of its tag would have changed, so nothing was saved".into());
            }
            let after = std::fs::read(&tmp).unwrap_or_default();
            if bytes::id3v1_block(&original) != bytes::id3v1_block(&after) {
                return Err("the old tag at its end would have changed, so nothing was saved".into());
            }
        }
        // the checked copy takes the place of the file it was made from (itself a copy the app made)
        std::fs::rename(&tmp, path).map_err(|e| format!("the file could not be replaced ({e})"))
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// The values of an ID3 text, apart: v2.4 keeps them apart with a zero byte, and ffmpeg ends a value
/// with one, which is not a value.
fn values(t: &str) -> Vec<&str> {
    t.split('\0').map(str::trim).filter(|s| !s.is_empty()).collect()
}

fn malformed(e: String) -> String {
    format!("its tags are malformed, so it was left alone ({e})")
}
fn short(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}...", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}
fn no_zero(s: &str) -> String {
    if s.is_empty() { "0".into() } else { s.to_string() }
}

/// The text of an ID3 frame that has text (text frames, comments, lyrics, TXXX values).
fn frame_text(f: &id3::Frame) -> Option<String> {
    Some(
        match f.content() {
            Content::Text(t) => values(t).join("/"),
            Content::ExtendedText(x) => values(&x.value).join("/"),
            Content::Comment(c) => c.text.clone(),
            Content::Lyrics(l) => l.text.clone(),
            _ => return None,
        }
        .trim()
        .to_string(),
    )
}

/// Every entry of an MP3's tag except the lyrics, as text that is equal exactly when the entries are: for
/// proving that a lyrics change touched nothing else.
fn other_frames(d: &Doc) -> Vec<String> {
    let Some(t) = d.id3() else { return vec![] };
    let mut v: Vec<String> = t
        .frames()
        .filter(|f| !matches!(f.id(), "USLT" | "SYLT"))
        .filter(|f| !matches!(f.content(), Content::ExtendedText(x) if doc::hidden_name(&x.description)))
        .map(|f| format!("{}:{:?}", f.id(), f.content()))
        .collect();
    v.sort();
    v
}

/// The original year an MP3 says, from either version's frame.
fn original_year(t: &id3::Tag) -> String {
    for id in ["TDOR", "TORY"] {
        if let Some(y) = t.get(id).and_then(|f| f.content().text()) {
            if let Some(m) = regex_year(y) {
                return m;
            }
        }
    }
    String::new()
}
fn regex_year(s: &str) -> Option<String> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(3)).find(|&i| b[i..i + 4].iter().all(u8::is_ascii_digit)).map(|i| s[i..i + 4].to_string())
}

/// The fields ffmpeg loses or hides when it converts a file, carried over by `copy_tags`: each with the
/// names (as ffprobe gives them) that, in the plan's Skip, mean the import set or took it out on purpose.
const CARRIERS: [(&str, &[&str]); 26] = [
    ("lyrics", &["lyrics"]),
    ("comment", &["comment", "comments", "description"]),
    ("bpm", &["bpm", "tbpm"]),
    ("grouping", &["grouping"]),
    ("conductor", &["conductor"]),
    ("isrc", &["isrc", "tsrc"]),
    ("publisher", &["publisher", "label", "organization"]),
    ("key", &["key", "initialkey"]),
    ("remixer", &["remixer"]),
    ("subtitle", &["subtitle"]),
    ("title sort", &["title"]),
    ("album sort", &["album"]),
    ("artist sort", &["artist"]),
    ("album artist sort", &["album_artist"]),
    ("composer sort", &["composer"]),
    ("track total", &["track"]),
    ("disc total", &["disc"]),
    ("musicbrainz artist", &[]),
    ("musicbrainz album", &[]),
    ("musicbrainz album artist", &[]),
    ("musicbrainz track", &[]),
    ("musicbrainz release group", &[]),
    ("musicbrainz disc", &[]),
    ("musicbrainz status", &[]),
    ("musicbrainz type", &[]),
    ("musicbrainz country", &[]),
];
/// The plain fields, carried too when the new file is a FLAC: ffmpeg misses some of them (the tag at the
/// end of an AIFF file, the year of a WavPack or Monkey's Audio file).
const PLAIN: [(&str, &[&str]); 9] = [
    ("title", &["title"]),
    ("artist", &["artist"]),
    ("album", &["album"]),
    ("album artist", &["album_artist"]),
    ("genre", &["genre"]),
    ("year", &["date", "year"]),
    ("track", &["track"]),
    ("disc", &["disc"]),
    ("composer", &["composer"]),
];

/// The frames of an MP3 that ffmpeg drops or turns into generic text tags when it rewrites the file, and
/// which field (as ffprobe names it) each stands for.
fn carried_frame(id: &str) -> Option<&'static str> {
    Some(match id {
        "USLT" | "SYLT" => "lyrics",
        "COMM" => "comment",
        "TIT1" => "grouping",
        "TIT3" => "subtitle",
        "TBPM" => "bpm",
        "TKEY" => "key",
        "TSRC" => "isrc",
        "TPUB" => "publisher",
        "TPE3" => "conductor",
        "TPE4" => "remixer",
        "TEXT" => "lyricist",
        "TSOP" => "artist",
        "TSOA" => "album",
        "TSOT" => "title",
        "TSO2" => "album_artist",
        "TSOC" => "composer",
        "TCMP" => "compilation",
        "POPM" | "PCNT" | "GEOB" | "UFID" | "TOLY" | "TOPE" | "TOAL" | "TORY" | "TDOR" | "TMOO" | "TLAN" | "TMED" | "TDRL" | "TIPL" | "TMCL" | "IPLS" | "RVA2" | "RVAD" | "MCDI" | "CHAP" | "CTOC" | "ETCO" => "",
        _ => return None,
    })
}

/// A carried field read from the file a conversion started from: lyrics from the field, else from a tag
/// that hides them; the comment from the field, else a FLAC's COMMENT or DESCRIPTION; the publisher from
/// the field, else a FLAC's LABEL.
fn carried(sf: &Doc, name: &str) -> String {
    match name {
        "lyrics" => {
            let l = sf.real_lyrics();
            if l.is_empty() { sf.hidden_lyrics() } else { l }
        }
        "comment" => {
            let c = sf.get("comment");
            if c.is_empty() && sf.is_flac() {
                for k in ["COMMENT", "DESCRIPTION"] {
                    let v = sf.vorbis_first(k);
                    if !v.trim().is_empty() {
                        return v.trim().to_string();
                    }
                }
            }
            c
        }
        "publisher" => {
            let p = sf.get("publisher");
            if p.is_empty() && sf.is_flac() { sf.vorbis_first("LABEL").trim().to_string() } else { p }
        }
        f => sf.get(f),
    }
}
/// Whether the new file already has a carried field ("" = no). Lyrics count only in the field players read.
fn has(tf: &Doc, name: &str) -> String {
    if name == "lyrics" { tf.real_lyrics() } else { tf.get(name) }
}

/// ID3 frames of the file a conversion started from, when it has an ID3 tag (an MP3, or a WAV, AIFF or
/// DSF file with one inside).
fn source_id3(from: &Path, sf: &Doc) -> Option<id3::Tag> {
    if let Some(t) = sf.id3() {
        return (t.frames().count() > 0).then(|| t.clone());
    }
    let ext = from.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ["wav", "aif", "aiff"].contains(&ext.as_str()) {
        return id3::Tag::read_from_path(from).ok();
    }
    None
}

/// Carries the tags of the file a conversion started from into the new file's real fields, only where the
/// new file has nothing there yet (what the import set on purpose stays). Returns what was carried, as
/// name -> the value to find there afterwards.
fn copy_tags(tf: &mut Doc, from: &Path, skip: &[String], drop_personal: bool) -> Result<BTreeMap<String, String>, String> {
    let mut want = BTreeMap::new();
    let skipped = |names: &[&str]| names.iter().any(|n| skip.iter().any(|s| s == n));
    let sf = Doc::open_source(from).map_err(|e| format!("the file it was converted from could not be read ({e})"))?;
    let sid3 = source_id3(from, &sf);
    // an MP3 made from an MP3: the frames themselves, as they are (a rating, play count, DJ data, timed
    // lyrics, lyrics in two languages...), where the new file has none of that kind
    if let (Some(tid3), Some(sid3)) = (tf.id3_mut(), sid3.as_ref()) {
        let have: Vec<String> = tid3.frames().map(|f| f.id().to_string()).collect();
        let have_text: Vec<String> = tid3.extended_texts().map(|x| x.description.to_lowercase()).collect();
        let have_owner: Vec<String> = tid3.frames().filter_map(|f| if let Content::Private(p) = f.content() { Some(p.owner_identifier.clone()) } else { None }).collect();
        for fr in sid3.frames() {
            if drop_personal && doc::personal_frame(fr) {
                continue;
            }
            let id = fr.id();
            let add = match fr.content() {
                Content::ExtendedText(x) => {
                    let d = x.description.to_lowercase();
                    !have_text.contains(&d) && !skip.contains(&d) && !x.value.trim().is_empty()
                }
                Content::Private(p) => !have_owner.contains(&p.owner_identifier),
                c => match carried_frame(id) {
                    Some(field) if !have.iter().any(|h| h == id) => {
                        let add = field.is_empty() || !skip.iter().any(|s| s == field);
                        // a comment the import took out as junk stays out; the sound-check notes players keep there come along
                        add || (id == "COMM" && matches!(c, Content::Comment(cm) if cm.description.starts_with("iTun")))
                    }
                    _ => false,
                },
            };
            if add {
                tid3.frames_vec_mut().push(fr.clone());
                let n = want.get(&format!("frames {id}")).and_then(|n: &String| n.parse::<u32>().ok()).unwrap_or(0);
                want.insert(format!("frames {id}"), (n + 1).to_string());
            }
        }
    }
    let mut list: Vec<(&str, &[&str])> = vec![];
    if tf.is_flac() {
        list.extend(PLAIN);
    }
    list.extend(CARRIERS);
    for (name, names) in list {
        if skipped(names) {
            continue;
        }
        let v = carried(&sf, name);
        if v.trim().is_empty() || !has(tf, name).trim().is_empty() {
            continue; // nothing to carry, or the new file already has it
        }
        tf.set(name, &v);
        want.insert(format!("carry {name}"), v.trim().to_string());
    }
    // what only an MP3's tag has a place for
    if tf.is_mp3() {
        let x = sf.vorbis();
        let comp = x.is_some_and(|x| x.get("COMPILATION").is_some_and(|c| c.trim() == "1")) || (!sf.is_flac() && sf.get("compilation") == "1") || sid3.as_ref().is_some_and(|t| t.get("TCMP").and_then(|f| f.content().text()).is_some_and(|c| c.trim() == "1"));
        if comp && tf.get("compilation").is_empty() && !skip.iter().any(|s| s == "compilation") {
            tf.set("compilation", "1");
            want.insert("carry compilation".into(), "1".into());
        }
        if x.is_some() {
            let mut orig = sf.vorbis_first("ORIGINALDATE");
            if orig.trim().is_empty() {
                orig = sf.vorbis_first("ORIGINALYEAR");
            }
            let by = sf.vorbis_first("LYRICIST").trim().to_string();
            let mp3 = tf.mp3().map(|m| m.version);
            let tid3 = tf.id3_mut().unwrap();
            if let Some(y) = regex_year(&orig) {
                // (the original year has a frame of its own in each version of the tag)
                if original_year(tid3).is_empty() {
                    let id = if mp3 == Some(id3::Version::Id3v24) { "TDOR" } else { "TORY" };
                    tid3.add_frame(id3::Frame::text(id, y.clone()));
                    want.insert("original year".into(), y);
                }
            }
            if !by.is_empty() && tid3.get("TEXT").and_then(|f| f.content().text()).is_none_or(|t| t.trim().is_empty()) {
                tid3.add_frame(id3::Frame::text("TEXT", by.clone()));
                want.insert("text TEXT".into(), by);
            }
        }
    }
    Ok(want)
}

/// After a carry: the generic text tags ffmpeg wrote are taken out, each only when the real field now
/// holds the same value (so nothing is lost if a field could not be written). `lyrics_only`: only the ones
/// that hide lyrics. Returns how many went.
fn remove_stand_ins(tf: &mut Doc, lyrics_only: bool) -> usize {
    let real_lyrics = tf.real_lyrics();
    let mut n = 0;
    if tf.is_flac() {
        // A FLAC made from a file with an MP3-style tag: what ffmpeg has no name for it leaves under that
        // tag's own four-letter names, and lyrics under "lyrics-" and their language. One made from a
        // WavPack or Monkey's Audio file keeps that tag's "Year" and "Album Artist". FLAC players read none.
        let Some(x) = tf.vorbis() else { return 0 };
        let mut keys: Vec<String> = x.items().map(|(k, _)| k.to_uppercase()).collect();
        keys.dedup();
        let mut gone: Vec<String> = vec![];
        for k in keys {
            let values: Vec<String> = x.get_all(&k).map(str::to_string).collect();
            if k == "USLT" || (k.starts_with("LYRICS-") && k.len() > 7) {
                if same(&real_lyrics, &values.join("\n")) {
                    gone.push(k);
                }
                continue;
            }
            if lyrics_only {
                continue;
            }
            let real = match k.as_str() {
                "TBPM" => tf.get("bpm"),
                "TIT1" => tf.get("grouping"),
                "TIT3" => tf.get("subtitle"),
                "TSRC" => tf.get("isrc"),
                "TPUB" => tf.get("publisher"),
                "TKEY" => tf.get("key"),
                "TPE3" => tf.get("conductor"),
                "TPE4" => tf.get("remixer"),
                "YEAR" => {
                    let d: Vec<char> = tf.vorbis_first("DATE").trim().chars().collect();
                    if d.len() >= 4 { d[..4].iter().collect() } else { String::new() }
                }
                "ALBUM ARTIST" => tf.vorbis_first("ALBUMARTIST"),
                _ => continue,
            };
            if !real.trim().is_empty() && same(&real, &values.join("/")) {
                gone.push(k);
            }
        }
        n = gone.len();
        for k in gone {
            tf.remove_vorbis(&k);
        }
        return n;
    }
    let fields: BTreeMap<&str, String> = ["comment", "bpm", "grouping", "track total", "disc total", "isrc", "publisher", "artist sort", "album artist sort", "album sort", "title sort", "composer sort", "conductor", "key", "subtitle", "remixer", "compilation"]
        .into_iter()
        .chain(doc::FIELDS.iter().copied().filter(|f| f.starts_with("musicbrainz")))
        .map(|f| (f, tf.get(f)))
        .collect();
    let Some(id3) = tf.id3_mut() else { return 0 };
    let orig = original_year(id3);
    let lyricist = id3.get("TEXT").and_then(|f| f.content().text()).map(|t| values(t).join("/")).unwrap_or_default();
    let before = id3.frames().count();
    id3.frames_vec_mut().retain(|fr| {
        let Content::ExtendedText(x) = fr.content() else { return true };
        let d = x.description.to_lowercase();
        let v = values(&x.value).join("/");
        if doc::hidden_name(&d) {
            return !same(&real_lyrics, &values(&x.value).join("\n"));
        }
        if lyrics_only {
            return true;
        }
        let f = |k: &str| fields.get(k).cloned().unwrap_or_default();
        let real = match d.as_str() {
            "major_brand" | "minor_version" | "compatible_brands" => v.clone(), // facts about the file it was converted from
            "comment" => f("comment"),
            "bpm" | "tbpm" => f("bpm"),
            "grouping" | "contentgroup" => f("grouping"),
            "compilation" => f("compilation"),
            "tracktotal" | "totaltracks" => f("track total"),
            "disctotal" | "totaldiscs" => f("disc total"),
            "isrc" => f("isrc"),
            "label" | "organization" | "publisher" => f("publisher"),
            "artistsort" | "performersort" => f("artist sort"),
            "albumartistsort" => f("album artist sort"),
            "albumsort" => f("album sort"),
            "titlesort" => f("title sort"),
            "composersort" => f("composer sort"),
            "originaldate" | "originalyear" => {
                if !orig.is_empty() && v.starts_with(&orig) { v.clone() } else { String::new() }
            }
            "conductor" => f("conductor"),
            "lyricist" => lyricist.clone(),
            "initialkey" | "key" => f("key"),
            "subtitle" => f("subtitle"),
            "remixer" => f("remixer"),
            "musicbrainz_artistid" => f("musicbrainz artist"),
            "musicbrainz_albumid" => f("musicbrainz album"),
            "musicbrainz_albumartistid" => f("musicbrainz album artist"),
            "musicbrainz_trackid" => f("musicbrainz track"),
            "musicbrainz_releasegroupid" => f("musicbrainz release group"),
            "musicbrainz_discid" => f("musicbrainz disc"),
            "musicbrainz_albumstatus" => f("musicbrainz status"),
            "musicbrainz_albumtype" => f("musicbrainz type"),
            "releasecountry" => f("musicbrainz country"),
            _ => return true,
        };
        real.trim().is_empty() || !same(&real, &v)
    });
    n += before - id3.frames().count();
    n
}

/// Every change the step made, read back from the saved copy. Err(why) for the first that did not stick.
fn check(chk: &Doc, want: &BTreeMap<String, String>) -> Result<(), String> {
    let first = |s: String| s.split('/').next().unwrap_or("").trim().to_string();
    for (k, w) in want {
        // what was carried over, and the lyrics: compared as text, however the line ends are written
        if k == "lyrics" {
            if !same(&chk.real_lyrics(), w) {
                return Err("the lyrics didn't stick, so nothing was saved".into());
            }
            continue;
        }
        if let Some(name) = k.strip_prefix("carry ") {
            if !same(&has(chk, name), w) {
                return Err(format!("the {name} didn't carry over, so nothing was saved"));
            }
            continue;
        }
        let cid3 = chk.id3();
        if k == "original year" {
            if cid3.map(original_year).as_deref() != Some(w.as_str()) {
                return Err("the original year didn't carry over, so nothing was saved".into());
            }
            continue;
        }
        if let Some(id) = k.strip_prefix("text ") {
            let now = cid3.and_then(|t| t.get(id)).and_then(|f| f.content().text()).unwrap_or("");
            if !same(now, w) {
                return Err(format!("the change to {k} didn't stick, so nothing was saved"));
            }
            continue;
        }
        if let Some(id) = k.strip_prefix("frames ") {
            let n = cid3.map(|t| t.frames().filter(|f| f.id() == id).count()).unwrap_or(0);
            if n < w.parse::<usize>().unwrap_or(0) {
                return Err(format!("the {id} entries didn't carry over, so nothing was saved"));
            }
            continue;
        }
        let (got, numeric) = match k.as_str() {
            "album" => (chk.get("album"), false),
            "album_artist" => (first(chk.get("album artist")), false),
            "artist" => (first(chk.get("artist")), false),
            "title" => (chk.get("title"), false),
            "genre" => (first(chk.get("genre")), false),
            "track" => (chk.get("track"), true),
            "disc" => (chk.get("disc"), true),
            "year" => (chk.get("year"), true),
            "trackcount" => (chk.get("track total"), true),
            "disccount" => (chk.get("disc total"), true),
            "cover" => ((if chk.picture_count() > 0 { "1" } else { "0" }).to_string(), false),
            "compilation" => ((if chk.get("compilation") == "1" { "1" } else { "0" }).to_string(), false),
            _ => continue,
        };
        let ok = if numeric { doc::number(&got) == doc::number(w) } else { got.trim().eq_ignore_ascii_case(w.trim()) };
        if !ok {
            return Err(format!("the change to {k} didn't stick, so nothing was saved"));
        }
    }
    Ok(())
}
