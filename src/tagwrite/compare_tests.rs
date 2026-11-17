//! Regression cases for the Rust editor, using the same made-up files and plans as the former
//! comparison kit. The checked-in expectations preserve the reviewed Rust results from stages 2 and 3.
//! Audio must stay unchanged; refused files must stay byte for byte unchanged. Needs ffmpeg and ffprobe.
use super::bytes;
use crate::importer::tools;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Kit {
    base: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

fn kit() -> Kit {
    let base = std::env::temp_dir().join(format!("music-recs-tagwrite-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
    for d in ["src", "rs"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    Kit { base, ffmpeg: tools::find_tool("ffmpeg").unwrap(), ffprobe: tools::find_tool("ffprobe").unwrap() }
}

impl Kit {
    /// A one-second silent file of the kind its name says, with ffmpeg's own tags.
    fn silent(&self, name: &str, args: &[&str], tags: &[(&str, &str)]) -> PathBuf {
        let out = self.base.join("src").join(name);
        let mut c = tools::command(&self.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=330:sample_rate=44100:duration=1", "-ac", "2"]);
        let ext = out.extension().unwrap().to_string_lossy().to_string();
        match ext.as_str() {
            "flac" => c.args(["-c:a", "flac"]),
            "m4a" => c.args(["-c:a", "alac"]),
            "ogg" => c.args(["-c:a", "libvorbis"]),
            "opus" => c.args(["-c:a", "libopus"]),
            "wv" => c.args(["-c:a", "wavpack"]),
            "wav" => c.args(["-c:a", "pcm_s16le"]),
            "aiff" => c.args(["-c:a", "pcm_s16be"]),
            _ => c.args(["-c:a", "libmp3lame", "-b:a", "128k"]),
        };
        c.args(args);
        for (k, v) in tags {
            c.arg("-metadata").arg(format!("{k}={v}"));
        }
        c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {name}");
        out
    }
    fn picture(&self, name: &str, size: &str) -> PathBuf {
        let out = self.base.join("src").join(name);
        let mut c = tools::command(&self.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("testsrc=size={size}"), "-frames:v", "1"]).arg(&out);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        assert!(tools::run_limited(&mut c, 60));
        out
    }
    /// A bare MP3 (no tags at all) to put hand-made tags on.
    fn bare_mp3(&self, name: &str, args: &[&str]) -> Vec<u8> {
        let p = self.silent(name, &[&["-id3v2_version", "0", "-write_id3v1", "0"][..], args].concat(), &[]);
        let b = std::fs::read(&p).unwrap();
        std::fs::remove_file(p).unwrap();
        b
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.base.join("src").join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
}

// ---------------------------------------------------------------- hand-made ID3 tags

fn frame(ver: u8, id: &str, body: &[u8]) -> Vec<u8> {
    let mut v = id.as_bytes().to_vec();
    match ver {
        2 => v.extend(&(body.len() as u32).to_be_bytes()[1..]),
        4 => v.extend(syncsafe(body.len())),
        _ => v.extend((body.len() as u32).to_be_bytes()),
    }
    if ver != 2 {
        v.extend([0, 0]);
    }
    v.extend(body);
    v
}
fn syncsafe(n: usize) -> [u8; 4] {
    [((n >> 21) & 0x7f) as u8, ((n >> 14) & 0x7f) as u8, ((n >> 7) & 0x7f) as u8, (n & 0x7f) as u8]
}
fn t(s: &str) -> Vec<u8> {
    [&[0u8][..], s.as_bytes()].concat()
}
fn u16t(s: &str) -> Vec<u8> {
    let mut v = vec![1u8, 0xff, 0xfe];
    for u in s.encode_utf16() {
        v.extend(u.to_le_bytes());
    }
    v
}
fn comm(lang: &str, desc: &str, text: &str) -> Vec<u8> {
    [&[0u8][..], lang.as_bytes(), desc.as_bytes(), b"\0", text.as_bytes()].concat()
}
fn txxx(desc: &str, value: &str) -> Vec<u8> {
    [&[0u8][..], desc.as_bytes(), b"\0", value.as_bytes()].concat()
}
fn tag(ver: u8, flags: u8, frames: &[Vec<u8>], padding: usize) -> Vec<u8> {
    let mut body: Vec<u8> = frames.concat();
    body.extend(vec![0u8; padding]);
    [&b"ID3"[..], &[ver, 0, flags], &syncsafe(body.len()), &body].concat()
}
fn v1(title: &str, comment: &str, track: u8, genre: u8) -> Vec<u8> {
    let mut b = b"TAG".to_vec();
    for (s, n) in [(title, 30), ("Some Artist", 30), ("Old Album", 30), ("1999", 4)] {
        let mut x = s.as_bytes().to_vec();
        x.resize(n, 0);
        b.extend(x);
    }
    let mut c = comment.as_bytes().to_vec();
    c.resize(28, 0);
    b.extend(c);
    b.extend([0, track, genre]);
    b
}
fn ape_tag(items: &[(&str, &str)]) -> Vec<u8> {
    let mut body = vec![];
    for (k, v) in items {
        body.extend((v.len() as u32).to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(k.as_bytes());
        body.push(0);
        body.extend(v.as_bytes());
    }
    let footer = |is_header: bool| {
        let mut f = b"APETAGEX".to_vec();
        f.extend(2000u32.to_le_bytes());
        f.extend(((body.len() + 32) as u32).to_le_bytes());
        f.extend((items.len() as u32).to_le_bytes());
        f.extend((0x8000_0000u32 | if is_header { 0x2000_0000 } else { 0 }).to_le_bytes());
        f.extend([0u8; 8]);
        f
    };
    [footer(true), body.clone(), footer(false)].concat()
}
/// The frames a real download carries: sort and mood, a junk comment, iTunes' notes, a kept TXXX, another
/// person's rating, play count and DJ data, Windows' own PRIV, an old volume frame, lyrics and a cover.
fn rich_frames(ver: u8, jpg: &[u8]) -> Vec<Vec<u8>> {
    let mut f = vec![
        frame(ver, "TIT2", &t("Old")),
        frame(ver, "TPE1", &t("Some Artist")),
        frame(ver, "TALB", &t("Old Album")),
        frame(ver, if ver == 4 { "TDRC" } else { "TYER" }, &t("1999")),
        frame(ver, "TRCK", &t("3")),
        frame(ver, "TCON", &t("(17)")),
        frame(ver, "TSOP", &t("Artist, Some")),
        frame(ver, "TSOA", &t("Album, Old")),
        frame(ver, "TSO2", &t("Various")),
        frame(ver, "TMOO", &t("calm")),
        frame(ver, "TPE3", &u16t("Dirigent Ödön")),
        frame(ver, "COMM", &comm("eng", "", "Ripped by XYZ")),
        frame(ver, "COMM", &comm("eng", "iTunNORM", " 00000A 00000B")),
        frame(ver, "TXXX", &txxx("CUSTOM", "keep me")),
        frame(ver, "TXXX", &txxx("FMPS_Rating", "0.8")),
        frame(ver, "TXXX", &txxx("MusicBrainz Album Id", "1234-abcd")),
        frame(ver, "POPM", &[&b"someone@example.com\0"[..], &[200u8], &[0, 0, 0, 7]].concat()),
        frame(ver, "PCNT", &[0, 0, 0, 9]),
        frame(ver, "PRIV", &[&b"TRAKTOR4\0"[..], b"cue points"].concat()),
        frame(ver, "PRIV", &[&b"WM/MediaClassPrimaryID\0"[..], &[1, 2, 3, 4]].concat()),
        frame(ver, "USLT", &comm("eng", "", "old words\nsecond line")),
        frame(ver, "APIC", &[&[0u8][..], b"image/jpeg\0", &[3u8], b"\0", jpg].concat()),
        frame(ver, "APIC", &[&[0u8][..], b"image/jpeg\0", &[4u8], b"back\0", jpg].concat()),
    ];
    if ver == 3 {
        f.push(frame(ver, "RVAD", &[1u8, 16, 0, 10, 0, 10, 0, 0, 0, 0]));
    }
    f
}

// ---------------------------------------------------------------- what is compared

/// Every tag the file has, as text keyed by name: ffprobe's view, and for an MP3 also each ID3v2 frame
/// as the app's own reader sees it, plus how many pictures.
fn tags(k: &Kit, p: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut c = tools::command(&k.ffprobe, false);
    c.args(["-v", "error", "-show_entries", "format_tags:stream_tags:stream=codec_type:stream_disposition=attached_pic", "-of", "json"]).arg(p);
    let o = c.stderr(std::process::Stdio::null()).output().unwrap();
    let j: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
    for (key, v) in j["format"]["tags"].as_object().into_iter().flatten() {
        out.insert(format!("probe {}", key.to_lowercase()), v.as_str().unwrap_or("").trim().replace("\r\n", "\n"));
    }
    let pics = j["streams"].as_array().map(|s| s.iter().filter(|s| s["disposition"]["attached_pic"].as_i64() == Some(1)).count()).unwrap_or(0);
    out.insert("pictures".into(), pics.to_string());
    for (i, s) in j["streams"].as_array().into_iter().flatten().enumerate() {
        for (key, v) in s["tags"].as_object().into_iter().flatten() {
            out.insert(format!("stream{i} {}", key.to_lowercase()), v.as_str().unwrap_or("").trim().to_string());
        }
    }
    if bytes::is_mp3(p) {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for f in crate::importer::id3::frames(p).unwrap_or_default() {
            let key = format!("{}[{}]", f.id, f.desc);
            let n = seen.entry(key.clone()).or_default();
            *n += 1;
            out.insert(format!("frame {key} #{n}"), f.text.trim().replace("\r\n", "\n"));
        }
        let b = std::fs::read(p).unwrap();
        out.insert("id3 version".into(), bytes::id3_at_start(&b).map(|(_, v, _)| format!("2.{v}")).unwrap_or_default());
        out.insert("id3v1".into(), String::from_utf8_lossy(bytes::id3v1_block(&b)).replace('\0', "."));
        out.insert("ape".into(), (!bytes::ape_block(&b, bytes::id3v1_block(&b).len()).is_empty()).to_string());
    }
    // The fixture maker's build number is not part of the tag editor's contract.
    let build = regex::Regex::new(r"(Lavf|Lavc)\d+(?:\.\d+)*").unwrap();
    for value in out.values_mut() {
        *value = build.replace_all(value, "$1").into_owned();
    }
    out
}

struct Case {
    name: &'static str,
    file: PathBuf,
    plan: Value,
}

#[test]
fn the_rust_editor_preserves_the_reviewed_fixture_results() {
    let k = kit();
    let jpg_path = k.picture("old.jpg", "64x64");
    let jpg = std::fs::read(&jpg_path).unwrap();
    let cover = k.picture("new.jpg", "300x300");
    let png = k.picture("new.png", "200x200");
    let cbr = k.bare_mp3("bare.mp3", &[]);
    let vbr = k.bare_mp3("bare-vbr.mp3", &["-q:a", "6", "-write_xing", "0"]);
    let full = |title: &str| {
        json!({ "Set": { "title": title, "artist": "Some Artist", "album": "New Album", "album_artist": "Various Artists", "track": "3", "trackcount": "12", "disc": "1", "disccount": "2", "year": "2001", "genre": "Rock", "compilation": "1" },
                "Junk": [["comment", "Ripped by XYZ"]], "Cover": cover.to_string_lossy(), "CoverMode": "all", "Vbr": false, "Lyrics": "line one\nline two", "LyricsMode": "set", "DropPersonal": true })
    };
    let health = |set: Value| json!({ "Set": set, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false });
    let mut cases: Vec<Case> = vec![];
    let mut add = |name: &'static str, file: PathBuf, plan: Value| cases.push(Case { name, file, plan });

    // ---- MP3: the tag kinds and states the script handles
    let rich23 = [tag(3, 0, &rich_frames(3, &jpg), 512), cbr.clone(), v1("Old", "Ripped by XYZ", 3, 17)].concat();
    add("mp3 v2.3, a download's full import", k.write("rich23.mp3", &rich23), full("One (fixed)"));
    add("mp3 v2.3, a Health title fix", k.write("rich23-health.mp3", &rich23), health(json!({ "title": "Fixed Title" })));
    add("mp3 v2.3, junk out only", k.write("rich23-junk.mp3", &rich23), json!({ "Set": {}, "Junk": [["comment", "Ripped by XYZ"], ["fmps_rating", "0.8"]], "Cover": null, "CoverMode": "missing", "Vbr": false }));
    add("mp3 v2.3, cover only where missing", k.write("rich23-cover.mp3", &rich23), json!({ "Set": {}, "Junk": [], "Cover": png.to_string_lossy(), "CoverMode": "missing", "Vbr": false }));
    let rich24 = [tag(4, 0, &rich_frames(4, &jpg), 256), cbr.clone()].concat();
    add("mp3 v2.4, a download's full import", k.write("rich24.mp3", &rich24), full("Vier"));
    // (older iTunes: v2.4-style frame sizes in a v2.3 tag; a frame over 127 bytes shows the difference)
    let long = "x".repeat(300);
    let itunes = [tag(3, 0, &[frame(4, "TIT2", &t("Old")), frame(4, "COMM", &comm("eng", "", &long)), frame(4, "TPE1", &t("Some Artist"))], 64), cbr.clone()].concat();
    add("mp3 v2.3 with v2.4-style sizes", k.write("itunes.mp3", &itunes), health(json!({ "title": "Fixed" })));
    let mut broken = tag(3, 0, &[frame(3, "TIT2", &t("Old")), frame(3, "TPE1", &t("Some Artist"))], 64);
    broken[10 + 7] = 200; // the title frame says it is longer than the tag
    add("mp3 with a damaged tag", k.write("damaged.mp3", &[broken, cbr.clone()].concat()), health(json!({ "title": "Fixed" })));
    let junk_only = tag(3, 0, &[frame(3, "COMM", &comm("eng", "", "Ripped by XYZ")), frame(3, "TSSE", &t("LAME"))], 0);
    add("mp3 with a second tag of junk", k.write("stacked.mp3", &[tag(3, 0, &rich_frames(3, &jpg)[..6], 64), junk_only, cbr.clone()].concat()), json!({ "Set": {}, "Junk": [["comment", "Ripped by XYZ"]], "Cover": null, "CoverMode": "missing", "Vbr": false }));
    let unique = tag(3, 0, &[frame(3, "TCOM", &t("Someone Else")), frame(3, "COMM", &comm("eng", "", "Ripped by XYZ"))], 0);
    add("mp3 with a second tag holding other info", k.write("stacked2.mp3", &[tag(3, 0, &rich_frames(3, &jpg)[..6], 64), unique, cbr.clone()].concat()), json!({ "Set": {}, "Junk": [["comment", "Ripped by XYZ"]], "Cover": null, "CoverMode": "missing", "Vbr": false }));
    add("mp3 with no tag at all", k.write("bare.mp3", &cbr), full("From Nothing"));
    let v22 = tag(2, 0, &[frame(2, "TT2", &t("Old")), frame(2, "TP1", &t("Some Artist")), frame(2, "TAL", &t("Old Album")), frame(2, "COM", &comm("eng", "", "Ripped by XYZ")), frame(2, "TYE", &t("1999"))], 64);
    add("mp3 v2.2", k.write("v22.mp3", &[v22, cbr.clone()].concat()), full("Two Two"));
    let unsync = tag(3, 0x80, &[frame(3, "TIT2", &t("Old")), frame(3, "TPE1", &t("Some Artist"))], 64);
    add("mp3 v2.3 marked unsynchronised", k.write("unsync.mp3", &[unsync, cbr.clone()].concat()), health(json!({ "album": "Fixed Album" })));
    let with_ape = [tag(3, 0, &rich_frames(3, &jpg)[..6], 64), cbr.clone(), ape_tag(&[("Title", "Old"), ("Comment", "Ripped by XYZ")]), v1("Old", "", 3, 17)].concat();
    add("mp3 with an APE tag and ID3v1", k.write("ape.mp3", &with_ape), health(json!({ "title": "Fixed", "artist": "New Artist" })));
    let v1_junk = [tag(3, 0, &rich_frames(3, &jpg)[..6], 64), cbr.clone(), v1("Old", "Visit www.example.com for more", 0, 255)].concat();
    add("mp3 with junk in its ID3v1 comment", k.write("v1junk.mp3", &v1_junk), json!({ "Set": {}, "Junk": [["comment", "Visit www.example.com for more music downloads"]], "Cover": null, "CoverMode": "missing", "Vbr": false }));
    add("mp3, the length header added", k.write("vbr.mp3", &[tag(3, 0, &rich_frames(3, &jpg)[..6], 64), vbr.clone()].concat()), json!({ "Set": { "title": "Long" }, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": true }));

    // ---- MP3 lyrics, the Health way
    let lyr = [tag(3, 0, &rich_frames(3, &jpg), 64), cbr.clone()].concat();
    add("mp3 lyrics set exactly", k.write("lyr-exact.mp3", &lyr), json!({ "Lyrics": "new words\nnew line", "LyricsMode": "set", "Exact": true }));
    add("mp3 lyrics only where missing (has some)", k.write("lyr-missing.mp3", &lyr), json!({ "Lyrics": "new words", "LyricsMode": "missing" }));
    add("mp3 lyrics cleared", k.write("lyr-clear.mp3", &lyr), json!({ "LyricsMode": "clear", "Exact": true }));
    add("mp3 lyrics undo, still as written", k.write("lyr-undo.mp3", &lyr), json!({ "LyricsMode": "clear", "OnlyIf": "old words\nsecond line", "Exact": true }));
    add("mp3 lyrics undo, changed since", k.write("lyr-undo2.mp3", &lyr), json!({ "LyricsMode": "clear", "OnlyIf": "other words", "Exact": true }));
    let hidden = [tag(3, 0, &[frame(3, "TIT2", &t("Old")), frame(3, "TXXX", &txxx("LYRICS", "hidden words\nline two")), frame(3, "TXXX", &txxx("USLT", "hidden words\nline two"))], 64), cbr.clone()].concat();
    add("mp3 lyrics in a hidden tag", k.write("hidden.mp3", &hidden), json!({ "Unhide": true }));

    // ---- FLAC and M4A
    let flac_tags: &[(&str, &str)] = &[("title", "Old"), ("artist", "Some Artist"), ("album", "Old Album"), ("comment", "Ripped by XYZ"), ("date", "1999-05-01"), ("track", "3/12"), ("MOOD", "calm"), ("RATING", "80"), ("LYRICS", "old words")];
    let flac = k.silent("rich.flac", &[], flac_tags);
    add("flac, a download's full import", flac.clone(), full("Eins"));
    add("flac, a Health title fix", copy_as(&flac, "rich-health.flac"), health(json!({ "title": "Fixed" })));
    add("flac, cover only where missing", copy_as(&flac, "rich-cover.flac"), json!({ "Set": {}, "Junk": [], "Cover": png.to_string_lossy(), "CoverMode": "missing", "Vbr": false }));
    let m4a = k.silent("rich.m4a", &[], &[("title", "Old"), ("artist", "Some Artist"), ("album", "Old Album"), ("comment", "Ripped by XYZ"), ("date", "1999"), ("track", "3"), ("lyrics", "old words")]);
    add("m4a, a download's full import", m4a.clone(), full("Apple"));
    add("m4a, a Health title fix", copy_as(&m4a, "rich-health.m4a"), health(json!({ "title": "Fixed" })));
    add("m4a lyrics set", copy_as(&m4a, "rich-lyrics.m4a"), json!({ "Lyrics": "new words", "LyricsMode": "set" }));

    // ---- other kinds Health can meet
    let other: &[(&str, &str)] = &[("title", "Old"), ("artist", "Some Artist"), ("comment", "Ripped by XYZ"), ("MOOD", "calm"), ("REPLAYGAIN_TRACK_GAIN", "-7.5 dB"), ("CUSTOMFIELD", "keep me"), ("LYRICS", "old words"), ("track", "3/12")];
    let ogg = k.silent("rich.ogg", &[], other);
    add("ogg, a Health fix", ogg, json!({ "Set": { "title": "Fixed", "album": "New Album" }, "Junk": [["comment", "Ripped by XYZ"]], "Cover": null, "CoverMode": "missing", "Vbr": false }));
    let opus = k.silent("rich.opus", &[], other);
    add("opus, a Health fix", opus, health(json!({ "title": "Fixed" })));
    let wv = k.silent("rich.wv", &[], other);
    add("wavpack, a Health fix", wv, health(json!({ "title": "Fixed", "year": "2001" })));
    let aiff = k.silent("rich.aiff", &["-write_id3v2", "1"], other);
    add("aiff, a Health fix", aiff.clone(), json!({ "Set": { "title": "Fixed", "album": "New Album" }, "Junk": [["comment", "Ripped by XYZ"]], "Cover": null, "CoverMode": "missing", "Vbr": false }));
    let aiff_flac = convert(&k, &aiff, "carry-aiff.flac", &["-c:a", "flac"]);
    add("carry: AIFF to FLAC", aiff_flac.clone(), json!({ "File": aiff_flac.to_string_lossy(), "CarryFrom": aiff.to_string_lossy(), "Skip": [], "Set": {}, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false, "DropPersonal": true }));

    // ---- a damaged entry inside a tag that reads whole (the frame list is fine, one frame's text is not)
    let bad_frame = [tag(3, 0, &[frame(3, "TIT2", &t("Old")), frame(3, "SYLT", &[0, b'e', b'n', b'g', 2, 1, b'x', 0, 0, 0, 0, 10]), frame(3, "TPE1", &t("Some Artist"))], 64), cbr.clone()].concat();
    add("mp3 with one damaged entry", k.write("badframe.mp3", &bad_frame), health(json!({ "title": "Fixed" })));

    // ---- carrying a converted file's tags over (what the importer asks after ffmpeg converts)
    let src_flac = k.silent(
        "carry-src.flac",
        &[],
        &[
            ("title", "Carried"), ("artist", "Some Artist"), ("album", "Carry Album"), ("album_artist", "Some Artist"), ("date", "2004"), ("track", "5"), ("TRACKTOTAL", "11"), ("disc", "1"), ("DISCTOTAL", "2"),
            ("comment", "a real comment"), ("LYRICS", "carried words\nline two"), ("TEMPO", "128"), ("GROUPING", "Set A"), ("CONDUCTOR", "Someone"), ("ISRC", "USAAA0400001"), ("LABEL", "Some Label"), ("INITIALKEY", "Am"), ("SUBTITLE", "Live"),
            ("ARTISTSORT", "Artist, Some"), ("ALBUMSORT", "Album, Carry"), ("MUSICBRAINZ_ALBUMID", "aaaa-bbbb"), ("MUSICBRAINZ_TRACKID", "cccc-dddd"), ("RELEASECOUNTRY", "US"), ("ORIGINALDATE", "1980-02-03"), ("LYRICIST", "Some Writer"),
            ("COMPILATION", "1"), ("RATING", "80"), ("FMPS_RATING", "0.8"),
        ],
    );
    let to_mp3 = convert(&k, &src_flac, "carry-out.mp3", &["-c:a", "libmp3lame", "-b:a", "128k", "-id3v2_version", "3"]);
    let carry = |out: &Path, skip: &[&str]| json!({ "File": out.to_string_lossy(), "CarryFrom": src_flac.to_string_lossy(), "Skip": skip, "Set": {}, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false, "DropPersonal": true });
    add("carry: FLAC to MP3", to_mp3.clone(), carry(&to_mp3, &["comment"]));
    let to_mp3b = copy_as(&to_mp3, "carry-out-lyrics.mp3");
    let mut p = carry(&to_mp3b, &["title"]);
    p["Lyrics"] = json!("found words");
    p["LyricsMode"] = json!("missing");
    add("carry: FLAC to MP3, lyrics already found", to_mp3b, p);
    let src_wav = k.silent("carry-src.wav", &["-write_id3v2", "1"], &[("title", "From Wav"), ("artist", "Some Artist"), ("album", "Wav Album"), ("date", "2010"), ("track", "2")]);
    let wav_flac = convert(&k, &src_wav, "carry-wav.flac", &["-c:a", "flac"]);
    add("carry: WAV to FLAC", wav_flac.clone(), json!({ "File": wav_flac.to_string_lossy(), "CarryFrom": src_wav.to_string_lossy(), "Skip": [], "Set": {}, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false, "DropPersonal": true }));
    let src_wv = k.silent("carry-src.wv", &[], &[("title", "From WavPack"), ("artist", "Some Artist"), ("album", "WV Album"), ("date", "2011"), ("track", "4/9"), ("album artist", "Some Artist")]);
    let wv_flac = convert(&k, &src_wv, "carry-wv.flac", &["-c:a", "flac"]);
    add("carry: WavPack to FLAC", wv_flac.clone(), json!({ "File": wv_flac.to_string_lossy(), "CarryFrom": src_wv.to_string_lossy(), "Skip": [], "Set": {}, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false, "DropPersonal": true }));
    let src_mp3 = k.write("carry-src.mp3", &[tag(3, 0, &[rich_frames(3, &jpg), vec![frame(3, "USLT", &comm("deu", "", "alte Worte")), frame(3, "SYLT", &[0, b'e', b'n', b'g', 2, 1, 0, b'x', 0, 0, 0, 0, 10])]].concat(), 64), cbr.clone()].concat());
    let mp3_mp3 = convert(&k, &src_mp3, "carry-mp3.mp3", &["-c:a", "copy", "-id3v2_version", "3"]);
    add("carry: MP3 rewritten by ffmpeg", mp3_mp3.clone(), json!({ "File": mp3_mp3.to_string_lossy(), "CarryFrom": src_mp3.to_string_lossy(), "Skip": ["comment"], "Set": {}, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false, "DropPersonal": true }));

    // ---- small ones
    add("a file that is gone", k.base.join("src").join("gone.mp3"), health(json!({ "title": "x" })));
    add("nothing to change", k.write("same.mp3", &[tag(3, 0, &rich_frames(3, &jpg)[..6], 64), cbr.clone()].concat()), health(json!({ "title": "Old" })));

    let mut plans = vec![];
    let place = |f: &Path| k.base.join("rs").join(f.file_name().unwrap());
    for c in &cases {
        let to = place(&c.file);
        if c.file.exists() {
            std::fs::copy(&c.file, &to).unwrap();
        }
        let mut plan = c.plan.clone();
        plan["File"] = json!(to.to_string_lossy());
        plans.push(plan);
    }
    let results = super::run(&plans, &k.ffmpeg);
    let mut actual = BTreeMap::new();
    for (case, result) in cases.iter().zip(&results) {
        let file = place(&case.file);
        let mut result = result.clone();
        result.as_object_mut().unwrap().remove("File");
        let seen = if file.exists() {
            if result["Ok"] == false || result["Why"] == "nothing needed changing" {
                assert_eq!(std::fs::read(&file).unwrap(), std::fs::read(&case.file).unwrap(), "{} changed an untouched file", case.name);
            }
            if !case.name.contains("length header") {
                assert_eq!(bytes::audio_hash(&file, &k.ffmpeg), bytes::audio_hash(&case.file, &k.ffmpeg), "{} changed the audio", case.name);
            }
            tags(&k, &file)
        } else {
            assert!(!case.file.exists(), "{} lost its file", case.name);
            BTreeMap::new()
        };
        actual.insert(case.name, json!({ "result": result, "tags": seen }));
    }
    let actual = serde_json::to_value(actual).unwrap();
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tagwrite-observed.json");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, serde_json::to_string_pretty(&actual).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixture_expectations.json")).unwrap();
    let wrong: Vec<_> = cases.iter().filter(|c| actual[c.name] != expected[c.name]).map(|c| c.name).collect();
    let _ = std::fs::remove_dir_all(&k.base);
    assert!(wrong.is_empty(), "Fixture results changed: {wrong:?}; observed results: {}", out.display());
    assert_eq!(actual.as_object().unwrap().len(), expected.as_object().unwrap().len(), "fixture cases were removed");
}

fn copy_as(p: &Path, name: &str) -> PathBuf {
    let to = p.with_file_name(name);
    std::fs::copy(p, &to).unwrap();
    to
}
/// ffmpeg converting a file as the importer does (its tags carried by ffmpeg, then by the editor).
fn convert(k: &Kit, from: &Path, name: &str, args: &[&str]) -> PathBuf {
    let out = k.base.join("src").join(name);
    let mut c = tools::command(&k.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(from).args(["-map", "0:a", "-map_metadata", "0"]).args(args).arg(&out);
    c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {name}");
    out
}
