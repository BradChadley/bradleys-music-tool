//! Tests of what becomes of a lossless download, with "keep lossless downloads as they are" on and off, on
//! silent made-up files in a scratch folder: never the real downloads or library, and nothing is played.
//! They need ffmpeg and ffprobe.
use super::flow_tests::{lab, scan, settle, Lab};
use super::import::{carry_plan, carry_tags, import_unit, ImportResult};
use super::lyrics_tests::{found, import_with, same_words, FOUND_TIMED, WORDS};
use super::model::{Action, Track};
use super::scan::{fate, Fate};
use super::tools;
use crate::test_tags::{edit_id3, set_lyrics};
use ::id3::TagLike;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::OggPictureStorage;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::{Accessor, TagExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A silent file of the given length with the given tags, in the format its name says (an MP3 at `kbps`).
fn quiet(l: &Lab, out: &Path, secs: f64, kbps: u32, tags: &[(&str, &str)]) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-nostdin", "-y", "-f", "lavfi", "-t", &format!("{secs}"), "-i", "anullsrc=r=44100:cl=stereo"]);
    match out.extension().and_then(|e| e.to_str()) {
        Some("flac") => c.args(["-c:a", "flac"]),
        Some("wav") => c.args(["-c:a", "pcm_s16le"]),
        Some("m4a") => c.args(["-c:a", "alac"]),
        Some("wv") => c.args(["-c:a", "wavpack"]),
        _ => c.args(["-c:a", "libmp3lame", "-b:a", &format!("{kbps}k"), "-id3v2_version", "3"]),
    };
    for (k, v) in tags {
        c.arg("-metadata").arg(format!("{k}={v}"));
    }
    c.arg(out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
}

/// An album of silent tracks in `dir` (a download, or a copy in the library): every track a little longer
/// than the one before, so no two hold the same sound.
fn album(l: &Lab, dir: &Path, name: &str, ext: &str, kbps: u32, titles: &[&str]) {
    for (i, title) in titles.iter().enumerate() {
        let n = (i + 1).to_string();
        quiet(l, &dir.join(format!("{:02} {title}.{ext}", i + 1)), 1.0 + 0.1 * i as f64, kbps, &[("artist", "Test Artist"), ("album", name), ("title", title), ("track", n.as_str()), ("date", "2020")]);
    }
    settle(dir);
}

/// A small picture of one color.
fn picture(l: &Lab, out: &Path, color: &str) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", &format!("color=c={color}:s=300x300"), "-frames:v", "1"]).arg(out);
    assert!(tools::run_limited(&mut c, 60));
}

/// A FLAC file's parts: (the block that says what the sound is, with its checksum; how many pictures it
/// holds; the sound itself, byte for byte). A file with anything in front of its "fLaC" mark is not one.
fn flac_parts(file: &Path) -> (Vec<u8>, usize, Vec<u8>) {
    let b = std::fs::read(file).unwrap();
    assert_eq!(&b[..4], b"fLaC", "{} does not start the way a FLAC file does", file.display());
    let (mut at, mut info, mut pictures) = (4usize, vec![], 0);
    loop {
        let (kind, last) = (b[at] & 0x7f, b[at] & 0x80 != 0);
        let len = ((b[at + 1] as usize) << 16) | ((b[at + 2] as usize) << 8) | b[at + 3] as usize;
        if kind == 0 {
            info = b[at + 4..at + 4 + len].to_vec();
        }
        if kind == 6 {
            pictures += 1;
        }
        at += 4 + len;
        if last {
            break;
        }
    }
    (info, pictures, b[at..].to_vec())
}

/// A file's tags as ffprobe names them, lower case.
fn tags(l: &Lab, file: &Path) -> HashMap<String, String> {
    super::probe::probe(&l.ctx.ffprobe, file).tags
}

/// A fingerprint of a file's sound once decoded, whatever format holds it.
fn sound(l: &Lab, file: &Path) -> String {
    let args: Vec<std::ffi::OsString> = ["-hide_banner", "-v", "error", "-nostdin", "-i"].iter().map(|s| s.into()).chain([file.as_os_str().to_os_string()]).chain(["-map", "0:a:0", "-c:a", "pcm_s32le", "-f", "md5", "-"].iter().map(|s| s.into())).collect();
    let out = tools::run_stdout(&l.ctx.ffmpeg, &args.iter().map(|a| a.as_os_str()).collect::<Vec<_>>());
    let text = String::from_utf8_lossy(&out).trim().to_string();
    assert!(text.starts_with("MD5="), "{} could not be decoded: {text}", file.display());
    text
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = super::util::walk_files(dir).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
    v.sort();
    v
}

fn lead(s: &str) -> u32 {
    s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)
}

#[test]
fn a_flac_album_stays_flac_with_its_tags_put_right_and_its_sound_untouched() {
    let mut l = lab("keep-flac");
    l.ctx.keep_lossless = true;
    // six tracks, so the tag editor's four workers each get a second file
    let dir = l.ctx.source.join("Test Artist - Kept (2021) [FLAC]");
    let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
    let credit = format!("{WORDS}\nLyrics provided by SomeSite.com");
    for (i, title) in titles.iter().enumerate() {
        let no = (i + 1).to_string();
        // what a tagger and a player leave in a FLAC: no album artist and no track total (the import adds
        // them), someone else's rating and play count, and things about the music that must stay
        let mut t = vec![
            ("ARTIST", "Test Artist"), ("ALBUM", "Kept"), ("DATE", "2021"), ("TRACKNUMBER", no.as_str()), ("RATING", "80"), ("FMPS_RATING", "0.8"), ("FMPS_PLAYCOUNT", "4"),
            ("BPM", "120"), ("ISRC", "USABC2100001"), ("REPLAYGAIN_TRACK_GAIN", "-7.5 dB"), ("MUSICBRAINZ_ALBUMID", "11111111-1111-1111-1111-111111111111"), ("COMPOSER", "Composer"),
        ];
        if i != 3 {
            t.push(("TITLE", title)); // (the fourth has no title: it gets the one in its file's name)
        }
        if i != 4 {
            t.push(("GENRE", "Rock")); // (the fifth has no genre: it gets the album's)
        }
        match i {
            0 => t.push(("COMMENT", "Recorded at home")),
            1 => t.extend([("COMMENT", "Visit https://x.example.com for more"), ("LYRICS", WORDS)]),
            2 => t.push(("UNSYNCEDLYRICS", credit.as_str())),
            _ => {}
        }
        quiet(&l, &dir.join(format!("0{no} {title}.flac")), 1.0 + 0.1 * i as f64, 0, &t);
    }
    // the fifth carries an advert in the comment field itself (ffmpeg writes a comment under FLAC's other
    // name for it), and the sixth a cover of its own
    edit_flac(&dir.join("05 Five.flac"), |f| {
        f.vorbis_comments_mut().unwrap().insert("COMMENT".into(), "Visit https://x.example.com for more".into());
    });
    picture(&l, &l.base.join("own.png"), "blue");
    edit_flac(&dir.join("06 Six.flac"), |f| { f.insert_picture(front_cover(&l.base.join("own.png")), None).unwrap(); });
    picture(&l, &dir.join("cover.jpg"), "red");
    settle(&dir);

    // the list: every track stays FLAC, and nothing is said to be converted
    let u = scan(&l, &dir);
    assert!(u.tracks.len() == 6 && u.tracks.iter().all(|t| fate(t, true) == Fate::StaysFlac) && u.tracks.iter().all(|t| fate(t, false) == Fate::Mp3));
    assert_eq!(super::lossless_words(&u, true, false), [("Stays FLAC".to_string(), false)]);

    let kept = [("One", found()), ("Three", found())];
    let lib = import_with(&l, &dir, &kept, super::lyrics::Choice::Replace);
    let mut want: Vec<String> = titles.iter().enumerate().map(|(i, t)| format!("0{} {t}.flac", i + 1)).collect();
    want.push("cover.jpg".into());
    assert_eq!(names(&lib), want, "six FLAC files and the cover, and nothing else");
    for (i, title) in titles.iter().enumerate() {
        let name = format!("0{} {title}.flac", i + 1);
        let (was, now) = (flac_parts(&dir.join(&name)), flac_parts(&lib.join(&name)));
        // the sound: the same bytes, described by the same block (which holds its checksum)
        assert!(was.0 == now.0 && was.2 == now.2 && !now.2.is_empty(), "{name}: the sound changed");
        assert_eq!(sound(&l, &dir.join(&name)), sound(&l, &lib.join(&name)), "{name}");
        // the cover is in the file, once, as a FLAC picture (the sixth keeps its own)
        assert_eq!(now.1, 1, "{name}: pictures");
        let t = tags(&l, &lib.join(&name));
        // what the import fixes
        assert_eq!((t["title"].as_str(), t["album_artist"].as_str(), t["genre"].as_str(), t["tracktotal"].as_str(), lead(&t["track"])), (*title, "Test Artist", "Rock", "6", i as u32 + 1), "{name}: {t:?}");
        // what is about the music stays as it came
        assert_eq!((t["artist"].as_str(), t["album"].as_str(), t["date"].as_str(), t["bpm"].as_str(), t["isrc"].as_str(), t["composer"].as_str()), ("Test Artist", "Kept", "2021", "120", "USABC2100001", "Composer"), "{name}: {t:?}");
        assert!(t["replaygain_track_gain"] == "-7.5 dB" && t["musicbrainz_albumid"].starts_with("1111"), "{name}: {t:?}");
        // the rating and the play count were another person's
        assert!(!t.keys().any(|k| k.contains("rating") || k.contains("playcount")), "{name} kept someone else's rating: {t:?}");
        // a comment stays; an advert goes, under either of the two names a FLAC has for a comment
        assert_eq!(t.get("comment").map(|s| s.as_str()), (i == 0).then_some("Recorded at home"), "{name}");
    }
    let lyrics = |name: &str| tags(&l, &lib.join(name)).get("lyrics").cloned().unwrap_or_default();
    // none: filled from LRCLIB; good ones: as they came; a credit under real words: LRCLIB's take their place
    assert_eq!(lyrics("01 One.flac"), FOUND_TIMED);
    assert!(same_words(&lyrics("02 Two.flac"), WORDS));
    assert_eq!(lyrics("03 Three.flac"), FOUND_TIMED);
    assert!(!tags(&l, &lib.join("03 Three.flac")).contains_key("unsyncedlyrics"), "the flagged words stayed beside the new ones");
    assert_eq!(lyrics("04 Four.flac"), "");
    // the sixth kept the cover it came with (a PNG), the others got the folder's
    let codec_of_picture = |name: &str| {
        let out = tools::run_stdout(&l.ctx.ffprobe, &["-v".as_ref(), "error".as_ref(), "-select_streams".as_ref(), "v:0".as_ref(), "-show_entries".as_ref(), "stream=codec_name".as_ref(), "-of".as_ref(), "csv=p=0".as_ref(), lib.join(name).as_os_str()]);
        String::from_utf8_lossy(&out).trim().to_string()
    };
    assert_eq!((codec_of_picture("06 Six.flac"), codec_of_picture("01 One.flac")), ("png".to_string(), "mjpeg".to_string()));

    // a compilation is marked in the FLAC's own tag, and nothing is put in front of the file
    let va = l.ctx.source.join("Various - Sampler (2021)");
    for (no, artist) in [("1", "First Band"), ("2", "Second Band")] {
        quiet(&l, &va.join(format!("0{no} Song {no}.flac")), 1.0, 0, &[("ARTIST", artist), ("ALBUMARTIST", "Various Artists"), ("ALBUM", "Sampler"), ("TITLE", "Song"), ("TRACKNUMBER", no), ("DATE", "2021")]);
    }
    settle(&va);
    let lib = import_with(&l, &va, &[], super::lyrics::Choice::Replace);
    for name in ["01 Song 1.flac", "02 Song 2.flac"] {
        let (was, now) = (flac_parts(&va.join(name)), flac_parts(&lib.join(name)));
        assert!(was.0 == now.0 && was.2 == now.2, "{name}: the sound changed");
        assert_eq!(tags(&l, &lib.join(name)).get("compilation").map(|s| s.as_str()), Some("1"), "{name}");
    }
}

#[test]
fn other_lossless_kinds_become_flac_with_the_same_sound_and_their_tags_carried() {
    let mut l = lab("to-flac");
    l.ctx.keep_lossless = true;
    // six tracks of three kinds (so the carry step's workers each get a second file): WAV, Apple Lossless
    // and WavPack, each tagged the way its own taggers do it
    let dir = l.ctx.source.join("Test Artist - Poured (2022)");
    let kinds = ["wav", "m4a", "wv", "wav", "m4a", "wv"];
    let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
    picture(&l, &l.base.join("own.jpg"), "blue");
    for (i, (ext, title)) in kinds.iter().zip(titles).enumerate() {
        let file = dir.join(format!("0{} {title}.{ext}", i + 1));
        quiet(&l, &file, 1.0 + 0.1 * i as f64, 0, &[]);
        carry_fixture(&file, ext, title, i as u32 + 1, &l.base.join("own.jpg"));
    }
    picture(&l, &dir.join("cover.jpg"), "red");
    settle(&dir);

    let u = scan(&l, &dir);
    assert!(u.tracks.len() == 6 && u.tracks.iter().all(|t| fate(t, true) == Fate::Flac), "{:?}", u.warnings);
    assert_eq!(super::lossless_words(&u, true, false), [("Becomes FLAC".to_string(), true)]);

    let lib = import_with(&l, &dir, &[], super::lyrics::Choice::Replace);
    let mut want: Vec<String> = titles.iter().enumerate().map(|(i, t)| format!("0{} {t}.flac", i + 1)).collect();
    want.push("cover.jpg".into());
    assert_eq!(names(&lib), want, "six FLAC files and the cover, and nothing else");
    for (i, (ext, title)) in kinds.iter().zip(titles).enumerate() {
        let (from, name) = (dir.join(format!("0{} {title}.{ext}", i + 1)), format!("0{} {title}.flac", i + 1));
        let p = super::probe::probe(&l.ctx.ffprobe, &lib.join(&name));
        assert_eq!(p.codec, "flac", "{name}");
        // the same sound, sample for sample
        assert_eq!(sound(&l, &from), sound(&l, &lib.join(&name)), "{name}: the sound changed");
        assert_eq!(flac_parts(&lib.join(&name)).1, 1, "{name}: pictures");
        let t = &p.tags;
        assert_eq!((t["title"].as_str(), t["artist"].as_str(), t["album_artist"].as_str(), t["album"].as_str(), t["genre"].as_str(), lead(&t["track"]), t["tracktotal"].as_str()), (title, "Test Artist", "Test Artist", "Poured", "Rock", i as u32 + 1, "6"), "{name}: {t:?}");
        // the year is where a FLAC keeps it (a WavPack file names it another way, which ffmpeg leaves as it is)
        assert!(t["date"].starts_with("2022") && !t.contains_key("year"), "{name}: {t:?}");
        // the rest of the tags came along, each under FLAC's name for it
        assert!(same_words(&t["lyrics"], "line one line two") && t["comment"] == "Bought it" && t["composer"] == "Composer", "{name}: {t:?}");
        assert!(t.get("bpm").or(t.get("tempo")).is_some_and(|b| b == "120"), "{name}: {t:?}");
        // nothing under another kind of tag's name, nothing about the file it was made from, no second album artist
        for gone in ["lyrics-eng", "tbpm", "tsrc", "major_brand", "minor_version", "compatible_brands", "album artist"] {
            assert!(!t.contains_key(gone), "{name} still has '{gone}': {t:?}");
        }
        assert!(!t.keys().any(|k| k.contains("rating")), "{name} kept someone else's rating: {t:?}");
    }
}

#[test]
fn with_the_setting_off_lossless_downloads_still_become_320_mp3() {
    let l = lab("off");
    assert!(!l.ctx.keep_lossless, "off unless Settings says otherwise");
    let dir = l.ctx.source.join("Test Artist - Squeezed (2020)");
    quiet(&l, &dir.join("01 One.flac"), 1.0, 0, &[("ARTIST", "Test Artist"), ("ALBUM", "Squeezed"), ("TITLE", "One"), ("TRACKNUMBER", "1"), ("DATE", "2020")]);
    quiet(&l, &dir.join("02 Two.wav"), 1.1, 0, &[("artist", "Test Artist"), ("album", "Squeezed"), ("title", "Two"), ("track", "2"), ("date", "2020")]);
    settle(&dir);
    let u = scan(&l, &dir);
    assert!(u.tracks.iter().all(|t| fate(t, false) == Fate::Mp3));
    assert_eq!(super::lossless_words(&u, false, false), [("All converted to MP3".to_string(), true)]);
    assert_eq!(super::lossless_words(&u, false, true), [("Becomes MP3 320".to_string(), true)]);
    let lib = import_with(&l, &dir, &[], super::lyrics::Choice::Replace);
    assert_eq!(names(&lib), ["01 One.mp3", "02 Two.mp3"]);
    for name in ["01 One.mp3", "02 Two.mp3"] {
        let p = super::probe::probe(&l.ctx.ffprobe, &lib.join(name));
        assert!(p.codec == "mp3" && (300..=330).contains(&p.kbps), "{name}: {} at {}kbps", p.codec, p.kbps);
    }
}

#[test]
fn a_flac_that_ffmpeg_had_to_rewrite_keeps_its_sound_and_its_tags() {
    // the import's fallback when the tag editor refuses a file: ffmpeg writes it again (the sound copied,
    // not encoded again), and the carry step follows
    let l = lab("rewrite-flac");
    let src = l.base.join("src.flac");
    quiet(&l, &src, 1.0, 0, &[("ARTIST", "Test Artist"), ("ALBUM", "Kept"), ("TITLE", "One"), ("TRACKNUMBER", "3"), ("LYRICS", WORDS), ("RATING", "80"), ("BPM", "120")]);
    let out = l.base.join("out.flac");
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(&src).args(["-map", "0:a:0", "-c:a", "copy", "-map_metadata", "0", "-metadata", "title=One (fixed)"]).arg(&out);
    assert!(tools::run_limited(&mut c, 60));
    let mut t = Track::default();
    t.set_new("title", "One (fixed)");
    let failed = carry_tags(&l.ctx, &[carry_plan(&out, &src, &t)]);
    assert!(failed.is_empty(), "{failed:?}");
    let (was, now) = (flac_parts(&src), flac_parts(&out));
    assert!(was.0 == now.0 && was.2 == now.2, "the sound changed");
    let t = tags(&l, &out);
    assert!(t["title"] == "One (fixed)" && same_words(&t["lyrics"], WORDS) && t["bpm"] == "120" && !t.contains_key("rating"), "{t:?}");
}

#[test]
fn a_tag_edit_of_a_flac_that_is_not_an_import_keeps_the_rating_and_the_sound() {
    // a Health repair goes through the same tag editor. Before the editor knew where a FLAC's sound starts
    // it refused every FLAC ("the audio data would have changed"); now it edits one, and only what was asked
    let l = lab("flac-repair");
    let file = l.base.join("mine.flac");
    quiet(&l, &file, 1.0, 0, &[("ARTIST", "Test Artist"), ("ALBUM", "Mine"), ("TITLE", "One"), ("TRACKNUMBER", "1"), ("RATING", "80"), ("LYRICS", WORDS)]);
    let was = flac_parts(&file);
    let plan = serde_json::json!({ "File": file.to_string_lossy(), "Set": { "title": "One (fixed)", "compilation": "1" }, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false });
    assert!(carry_tags(&l.ctx, &[plan]).is_empty());
    let now = flac_parts(&file);
    assert!(was.0 == now.0 && was.2 == now.2, "the sound changed");
    let t = tags(&l, &file);
    assert!(t["title"] == "One (fixed)" && t["compilation"] == "1" && t["rating"] == "80" && same_words(&t["lyrics"], WORDS), "{t:?}");
}

#[test]
fn a_cover_found_online_takes_the_place_of_the_one_in_a_flac() {
    // what the import asks of the tag editor when a cover was found online: it goes into every track, in
    // place of the front cover the file came with (the tests are offline, so the editor is asked directly)
    let l = lab("flac-cover");
    let file = l.base.join("one.flac");
    quiet(&l, &file, 1.0, 0, &[("ARTIST", "Test Artist"), ("ALBUM", "Covered"), ("TITLE", "One"), ("TRACKNUMBER", "1")]);
    picture(&l, &l.base.join("old.png"), "blue");
    picture(&l, &l.base.join("new.jpg"), "red");
    edit_flac(&file, |f| { f.insert_picture(front_cover(&l.base.join("old.png")), None).unwrap(); });
    let was = flac_parts(&file);
    assert_eq!(was.1, 1);
    let plan = serde_json::json!({ "File": file.to_string_lossy(), "Set": {}, "Junk": [], "Cover": l.base.join("new.jpg").to_string_lossy(), "CoverMode": "all", "Vbr": false, "DropPersonal": true });
    assert!(carry_tags(&l.ctx, &[plan]).is_empty());
    let now = flac_parts(&file);
    assert!(was.0 == now.0 && was.2 == now.2, "the sound changed");
    assert_eq!(now.1, 1, "one picture, not two");
    let out = tools::run_stdout(&l.ctx.ffprobe, &["-v".as_ref(), "error".as_ref(), "-select_streams".as_ref(), "v:0".as_ref(), "-show_entries".as_ref(), "stream=codec_name".as_ref(), "-of".as_ref(), "csv=p=0".as_ref(), file.as_os_str()]);
    assert_eq!(String::from_utf8_lossy(&out).trim(), "mjpeg", "the new cover is the one in the file");
}

#[test]
fn a_loose_flac_leaves_for_an_mp3_of_its_song_only_where_lossless_is_not_kept() {
    // "Get the full album": when the album arrives, the loose tracks it holds go to the Recycle Bin (a
    // test's is a folder). The library used to be all 320 MP3s, so a loose FLAC left for a 320 of its song
    let l = lab("loose-flac");
    let artist = l.ctx.library.join("Test Artist");
    let loose = artist.join("One.flac");
    quiet(&l, &loose, 1.0, 0, &[("artist", "Test Artist"), ("title", "One"), ("album", "Comet")]);
    let album_dir = artist.join("Comet (2020)");
    album(&l, &album_dir, "Comet", "mp3", 320, &["One", "Two"]);
    let files = [loose.to_string_lossy().to_string()];
    // lossless is kept: the FLAC is the better copy of the song, and stays
    let kept = crate::loose::recycle_keeping(&l.ctx.ffprobe, &l.ctx.library, &album_dir, &files, &l.ctx.recycle, true).unwrap();
    assert!(kept.moved.is_empty() && loose.is_file() && kept.stayed.len() == 1 && kept.stayed[0].contains("sounds worse"), "{:?}", kept.stayed);
    // and it does leave for a lossless copy of its song
    let flac_dir = artist.join("Comet (Deluxe) (2020)");
    album(&l, &flac_dir, "Comet", "flac", 0, &["One", "Two"]);
    let done = crate::loose::recycle_keeping(&l.ctx.ffprobe, &l.ctx.library, &flac_dir, &files, &l.ctx.recycle, true).unwrap();
    assert!(done.moved == files && !loose.exists(), "{:?}", done.stayed);
    // as it always was, with the setting off: it leaves for the 320 MP3
    std::fs::rename(l.base.join("bin").join("One.flac"), &loose).unwrap();
    let done = crate::loose::recycle_keeping(&l.ctx.ffprobe, &l.ctx.library, &album_dir, &files, &l.ctx.recycle, false).unwrap();
    assert!(done.moved == files && !loose.exists(), "{:?}", done.stayed);
}

/// Reads a download and imports it whatever the list ticked, the way a person who ticks it does.
fn import_ticked(l: &Lab, dir: &Path) -> (ImportResult, String, PathBuf) {
    let mut u = scan(l, dir);
    u.include = true;
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    let r = import_unit(&l.ctx, &mut u, &|t, _| *said.borrow_mut() = t.to_string(), None, &|| {});
    let text = said.borrow().clone();
    (r, text, l.ctx.library.join(&u.artist).join(&u.folder))
}

#[test]
fn a_lossless_copy_is_offered_over_a_lossy_one_and_never_gives_way_to_one() {
    let mut l = lab("better");
    let artist = l.ctx.library.join("Test Artist");
    let two = ["One", "Two"];
    let three = ["One", "Two", "Three"];
    let exts = |dir: &Path| -> Vec<String> { names(dir).iter().map(|n| n.rsplit('.').next().unwrap().to_string()).collect() };

    // --- the library holds the album as MP3 320, and a FLAC copy arrives
    let rocket = artist.join("Rocket (2020)");
    album(&l, &rocket, "Rocket", "mp3", 320, &two);
    let flac = l.ctx.source.join("Test Artist - Rocket [FLAC]");
    album(&l, &flac, "Rocket", "flac", 0, &two);
    // as it always was: the FLAC would become a 320 MP3, which is what is there already
    let u = scan(&l, &flac);
    assert!(u.action == Action::Exists && !u.include, "{}", u.note);
    // lossless downloads are kept: it is the better copy, offered, and left for a person to tick
    l.ctx.keep_lossless = true;
    let u = scan(&l, &flac);
    assert!(u.action == Action::Replace && !u.include, "{:?} {}", u.action, u.note);
    assert!(u.note.contains("This copy is lossless: tick to replace it") && u.note.contains("Rocket (2020)") && u.note.contains("undo backups"), "{}", u.note);
    // a FLAC copy with fewer tracks than the MP3 album is not offered
    let short = l.ctx.source.join("Test Artist - Rocket (part) [FLAC]");
    album(&l, &short, "Rocket", "flac", 0, &["One"]);
    let u = scan(&l, &short);
    assert!(u.action == Action::Exists && !u.include, "{}", u.note);
    std::fs::remove_dir_all(&short).unwrap();
    // ticked and imported: the album is FLAC now, and the MP3 copy is in the undo backups
    let (r, said, at) = import_ticked(&l, &flac);
    assert_eq!(r, ImportResult::Ok, "{said}");
    assert_eq!((at.as_path(), names(&rocket)), (rocket.as_path(), vec!["01 One.flac".to_string(), "02 Two.flac".to_string()]));
    let kept: Vec<PathBuf> = super::util::walk_files(&l.ctx.repair_root).into_iter().filter(|p| p.extension().is_some_and(|e| e == "mp3")).collect();
    assert_eq!(kept.len(), 2, "the MP3 copy is in the undo backups");

    // --- the library holds the album as FLAC now, and an MP3 copy arrives: never offered, even with more tracks
    let mp3 = l.ctx.source.join("Test Artist - Rocket (320)");
    album(&l, &mp3, "Rocket", "mp3", 320, &three);
    for keep in [true, false] {
        l.ctx.keep_lossless = keep;
        let u = scan(&l, &mp3);
        assert!(u.action == Action::Exists && !u.include && u.note.contains("Already in your library"), "keep {keep}: {:?} {}", u.action, u.note);
    }
    // not even when Health asked for a better copy of the album: an MP3 is not one (where lossless is kept)
    l.ctx.keep_lossless = true;
    crate::replace::add(&l.ctx.replacements, &rocket, "Test Artist", "Rocket (2020)", "Missing tracks", &[]).unwrap();
    let mut u = scan(&l, &mp3);
    assert!(u.action == Action::Exists && !u.include && u.note.contains("is not lossless, and your copy is"), "{:?} {}", u.action, u.note);
    // pushed through all the same, the import refuses and the FLAC album is as it was
    u.action = Action::Replace;
    let said = std::cell::RefCell::new(String::new());
    let r = import_unit(&l.ctx, &mut u, &|t, _| *said.borrow_mut() = t.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Failed, "{}", said.borrow());
    assert!(said.borrow().contains("is not lossless, and your copy is. Your copy stays as it is"), "{}", said.borrow());
    assert_eq!(exts(&rocket), ["flac", "flac"]);
    // a FLAC copy with every track is the better copy Health asked for
    let full = l.ctx.source.join("Test Artist - Rocket (full) [FLAC]");
    album(&l, &full, "Rocket", "flac", 0, &three);
    let u = scan(&l, &full);
    assert!(u.action == Action::Replace && u.include && u.note.contains("which Health flagged"), "{:?} {}", u.action, u.note);
    // (with the setting off, what Health asked for is done as it always was)
    l.ctx.keep_lossless = false;
    assert_eq!(scan(&l, &mp3).action, Action::Replace);
    crate::replace::done(&l.ctx.replacements, &rocket);
    l.ctx.keep_lossless = true;

    // --- FLAC in the library and a FLAC copy arrives: the same tracks are already there; more tracks win
    assert_eq!(scan(&l, &flac).action, Action::Exists);
    let u = scan(&l, &full);
    assert!(u.action == Action::Replace && u.include && u.note.contains("your copy has 2 tracks, this has 3"), "{:?} {}", u.action, u.note);
    // (with the setting off the longer FLAC would become MP3, and lossless is not traded for that)
    l.ctx.keep_lossless = false;
    assert_eq!(scan(&l, &full).action, Action::Exists);
    l.ctx.keep_lossless = true;
    let (r, said, _) = import_ticked(&l, &full);
    assert_eq!(r, ImportResult::Ok, "{said}");
    assert_eq!(exts(&rocket), ["flac", "flac", "flac"]);

    // --- a poor MP3 copy in the library: a FLAC replaces it without being asked, as a 320 MP3 always did
    album(&l, &artist.join("Comet (2020)"), "Comet", "mp3", 128, &two);
    let comet = l.ctx.source.join("Test Artist - Comet [FLAC]");
    album(&l, &comet, "Comet", "flac", 0, &two);
    let u = scan(&l, &comet);
    assert!(u.action == Action::Replace && u.include && u.note.contains("your copy is 128kbps, this one is lossless"), "{:?} {}", u.action, u.note);
    l.ctx.keep_lossless = false;
    let u = scan(&l, &comet);
    assert!(u.action == Action::Replace && u.include && u.note.contains("your copy is 128kbps, this one will be 320kbps"), "{:?} {}", u.action, u.note);
    l.ctx.keep_lossless = true;

    // --- mixed: a download that is part FLAC and part MP3 is not a lossless copy, so it is not offered over
    // an MP3 album; and an album in the library that is part FLAC is treated as lossless (never replaced by
    // a copy of the same tracks)
    album(&l, &artist.join("Planet (2020)"), "Planet", "mp3", 320, &two);
    let planet = l.ctx.source.join("Test Artist - Planet");
    quiet(&l, &planet.join("01 One.flac"), 1.0, 0, &[("artist", "Test Artist"), ("album", "Planet"), ("title", "One"), ("track", "1"), ("date", "2020")]);
    quiet(&l, &planet.join("02 Two.mp3"), 1.1, 320, &[("artist", "Test Artist"), ("album", "Planet"), ("title", "Two"), ("track", "2"), ("date", "2020")]);
    settle(&planet);
    let u = scan(&l, &planet);
    assert!(u.action == Action::Exists && !u.include, "{:?} {}", u.action, u.note);
    let moon = artist.join("Moon (2020)");
    quiet(&l, &moon.join("01 One.flac"), 1.0, 0, &[("artist", "Test Artist"), ("album", "Moon"), ("title", "One"), ("track", "1"), ("date", "2020")]);
    quiet(&l, &moon.join("02 Two.mp3"), 1.1, 320, &[("artist", "Test Artist"), ("album", "Moon"), ("title", "Two"), ("track", "2"), ("date", "2020")]);
    let moon_dl = l.ctx.source.join("Test Artist - Moon [FLAC]");
    album(&l, &moon_dl, "Moon", "flac", 0, &two);
    let u = scan(&l, &moon_dl);
    assert!(u.action == Action::Exists && !u.include, "{:?} {}", u.action, u.note);
    // --- two downloads of one new album in one batch, a FLAC and a 320 MP3 with the same tracks: the FLAC is
    // the one left ticked (here it is the smaller of the two, silence being what it is, so size did not decide)
    let (nova_flac, nova_mp3) = (l.ctx.source.join("Test Artist - Nova [FLAC]"), l.ctx.source.join("Test Artist - Nova (320)"));
    album(&l, &nova_flac, "Nova", "flac", 0, &two);
    album(&l, &nova_mp3, "Nova", "mp3", 320, &two);
    let mut both = [scan(&l, &nova_mp3), scan(&l, &nova_flac)];
    assert!(both[1].bytes < both[0].bytes && both.iter().all(|u| u.include && u.action == Action::New));
    super::scan::resolve_batch_duplicates(&mut both, true);
    assert!(!both[0].include && both[1].include && both[0].warnings.iter().any(|w| w.contains("Nova [FLAC]") && w.contains("better copy")), "{:?}", both[0].warnings);
    std::fs::remove_dir_all(&nova_flac).unwrap();
    std::fs::remove_dir_all(&nova_mp3).unwrap();

    // a new album that is part FLAC and part MP3 comes in as it is: the FLAC stays FLAC, the MP3 stays MP3
    let dust = l.ctx.source.join("Test Artist - Dust");
    quiet(&l, &dust.join("01 One.flac"), 1.0, 0, &[("artist", "Test Artist"), ("album", "Dust"), ("title", "One"), ("track", "1"), ("date", "2020")]);
    quiet(&l, &dust.join("02 Two.mp3"), 1.1, 320, &[("artist", "Test Artist"), ("album", "Dust"), ("title", "Two"), ("track", "2"), ("date", "2020")]);
    settle(&dust);
    let u = scan(&l, &dust);
    assert_eq!(super::lossless_words(&u, true, false), [("1 stays FLAC".to_string(), false)]);
    assert_eq!(super::lossless_words(&u, false, true), [("1 of 2 becomes MP3 320".to_string(), true)]);
    let (r, said, at) = import_ticked(&l, &dust);
    assert_eq!(r, ImportResult::Ok, "{said}");
    assert_eq!(names(&at), ["01 One.flac", "02 Two.mp3"]);
}

/// Native fixture tags, independent of the carry code that the tests exercise.
fn front_cover(path: &Path) -> Picture {
    let mut picture = Picture::from_reader(&mut std::fs::File::open(path).unwrap()).unwrap();
    picture.set_pic_type(PictureType::CoverFront);
    picture
}

fn edit_flac(path: &Path, edit: impl FnOnce(&mut FlacFile)) {
    let mut file = FlacFile::read_from(&mut std::fs::File::open(path).unwrap(), ParseOptions::new()).unwrap();
    edit(&mut file);
    file.save_to_path(path, WriteOptions::new()).unwrap();
}

fn carry_fixture(path: &Path, ext: &str, title: &str, no: u32, cover: &Path) {
    match ext {
        "wav" => edit_id3(path, |tag| {
            tag.set_title(title);
            tag.set_artist("Test Artist");
            tag.set_album("Poured");
            tag.set_year(2022);
            tag.set_track(no);
            tag.set_genre("Rock");
            set_lyrics(tag, "line one\nline two");
            tag.add_frame(::id3::frame::Comment { lang: "eng".into(), description: String::new(), text: "Bought it".into() });
            tag.set_text("TBPM", "120");
            tag.set_text("TCOM", "Composer");
            tag.set_text("TSRC", "USABC2200001");
            tag.add_frame(::id3::frame::Popularimeter { user: "SomePlayer".into(), rating: 196, counter: 7 });
        }),
        "wv" => {
            let mut tag = lofty::ape::ApeTag::new();
            for (key, value) in [
                ("Title", title), ("Artist", "Test Artist"), ("Album", "Poured"), ("Year", "2022"),
                ("Track", &no.to_string()), ("Genre", "Rock"), ("Lyrics", "line one\nline two"),
                ("Comment", "Bought it"), ("BPM", "120"), ("Composer", "Composer"), ("ISRC", "USABC2200001"),
                ("Rating", "80"), ("Album Artist", "test artist"),
            ] {
                tag.insert(lofty::ape::ApeItem::new(key.into(), lofty::tag::ItemValue::Text(value.into())).unwrap());
            }
            tag.save_to_path(path, WriteOptions::new()).unwrap();
        }
        "m4a" => {
            use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
            let mut tag = Ilst::new();
            tag.set_title(title.into());
            tag.set_artist("Test Artist".into());
            tag.set_album("Poured".into());
            tag.insert(Atom::new(AtomIdent::Fourcc(*b"\xa9day"), AtomData::UTF8("2022".into())));
            tag.set_track(no);
            tag.set_genre("Rock".into());
            tag.set_comment("Bought it".into());
            tag.insert(Atom::new(AtomIdent::Fourcc(*b"\xa9lyr"), AtomData::UTF8("line one\nline two".into())));
            tag.insert(Atom::new(AtomIdent::Fourcc(*b"\xa9wrt"), AtomData::UTF8("Composer".into())));
            tag.insert(Atom::new(AtomIdent::Fourcc(*b"tmpo"), AtomData::SignedInteger(120)));
            tag.insert(Atom::new(AtomIdent::Freeform { mean: "com.apple.iTunes".into(), name: "ISRC".into() }, AtomData::UTF8("USABC2200001".into())));
            tag.insert_picture(front_cover(cover));
            tag.save_to_path(path, WriteOptions::new()).unwrap();
        }
        _ => panic!("unexpected fixture kind: {ext}"),
    }
}
