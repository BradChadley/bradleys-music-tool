//! Tests of what happens to a download's tags and lyrics on import, on silent made-up files in a scratch
//! folder: never the real downloads or library, and nothing is played. They need ffmpeg, ffprobe and
//! PowerShell 7 (the app does too).
use super::flow_tests::{lab, scan, settle, Lab};
use super::id3::{self, Frame};
use super::import::{carry_plan, carry_tags, import_unit, ImportResult};
use super::model::Track;
use super::tools;
use std::path::{Path, PathBuf};

pub(super) const WORDS: &str = "The lamps along the water\nburn a little lower now\nand every boat that leaves the harbor\ntakes a piece of you somehow";

/// A one-second silent file with the given tags, in the format its name says.
fn silent(l: &Lab, out: &Path, tags: &[(&str, &str)]) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "1"]);
    match out.extension().and_then(|e| e.to_str()) {
        Some("flac") => c.args(["-c:a", "flac"]),
        Some("m4a") => c.args(["-c:a", "alac"]),
        _ => c.args(["-c:a", "libmp3lame", "-b:a", "320k", "-id3v2_version", "3"]),
    };
    for (k, v) in tags {
        c.arg("-metadata").arg(format!("{k}={v}"));
    }
    c.arg(out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
}

/// Changes a file's tags with the tag library, as a tagger or a player would have written them.
/// `script` works on `$f` (the open file) and `$id3` (its ID3v2 tag, for an MP3).
pub(super) fn taglib(l: &Lab, file: &Path, script: &str) {
    let dll = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tag-editor").join("TagLibSharp.dll");
    let ps = l.base.join(format!("tag-{:x}.ps1", super::scan::rand_id()));
    let text = format!(
        "$ErrorActionPreference = 'Stop'\nAdd-Type -Path '{}'\n$f = [TagLib.File]::Create('{}')\n$id3 = $f.GetTag([TagLib.TagTypes]::Id3v2, $false)\n{script}\n$f.Save(); $f.Dispose()\n",
        dll.display(),
        file.display()
    );
    std::fs::write(&ps, text).unwrap();
    let mut c = tools::command(&tools::pwsh().expect("PowerShell 7"), false);
    c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&ps).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 120), "the tag script failed for {}", file.display());
}

fn frames(file: &Path) -> Vec<Frame> {
    id3::frames(file).unwrap_or_else(|| panic!("{} has no tag", file.display()))
}
fn has(fr: &[Frame], id: &str) -> bool {
    fr.iter().any(|f| f.id == id)
}
fn text(fr: &[Frame], id: &str) -> String {
    fr.iter().find(|f| f.id == id).map(|f| f.text.clone()).unwrap_or_default()
}
/// The generic text tags a file has, by their names, lower case.
fn stand_ins(fr: &[Frame]) -> Vec<String> {
    let mut v: Vec<String> = fr.iter().filter(|f| f.id == "TXXX").map(|f| f.desc.to_lowercase()).collect();
    v.sort();
    v
}
pub(super) fn same_words(a: &str, b: &str) -> bool {
    a.split_whitespace().collect::<Vec<_>>() == b.split_whitespace().collect::<Vec<_>>()
}

fn import(l: &Lab, dir: &Path) -> PathBuf {
    import_with(l, dir, &[], super::lyrics::Choice::Replace)
}

pub(super) const FOUND_PLAIN: &str = "words from the library\nline two of them\nline three of them";
pub(super) const FOUND_TIMED: &str = "[00:00.10]words from the library\n[00:00.40]line two of them\n[00:00.70]line three of them";

/// Imports a download. `kept`: what LRCLIB "has" for its songs, by title (the answers are put where the
/// app keeps them, so nothing is asked: the tests never go online). `choice`: what is done with flagged lyrics.
pub(super) fn import_with(l: &Lab, dir: &Path, kept: &[(&str, super::lyrics::Answer)], choice: super::lyrics::Choice) -> PathBuf {
    let mut u = scan(l, dir);
    for (_, artist, title, secs) in super::scan::lyric_asks(&u) {
        if let Some((_, answer)) = kept.iter().find(|k| k.0 == title) {
            l.ctx.lyrics.keep(&artist, &title, secs, answer);
        }
    }
    u.lyric_choice = choice;
    u.include = true;
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    let r = import_unit(&l.ctx, &mut u, &|t, _| *said.borrow_mut() = t.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Ok, "{} / log: {}", said.borrow(), std::fs::read_to_string(&l.ctx.log_file).unwrap_or_default());
    l.ctx.library.join(&u.artist).join(&u.folder)
}

#[test]
fn a_converted_album_keeps_its_lyrics_and_the_rest_of_its_tags() {
    let l = lab("carry-flac");
    let dir = l.ctx.source.join("Test Artist - Carried (2019) [FLAC]");
    let common = [
        ("ARTIST", "Test Artist"), ("ALBUM", "Carried"), ("DATE", "2019"), ("GENRE", "Rock"), ("TRACKTOTAL", "2"), ("DISCNUMBER", "1"), ("DISCTOTAL", "2"),
        ("COMPOSER", "Composer"), ("BPM", "120"), ("ISRC", "USABC1900001"), ("LABEL", "Label"), ("COMPILATION", "1"), ("GROUPING", "Group"), ("ORIGINALDATE", "1999"),
        ("RATING", "80"), ("ARTISTSORT", "Artist, Test"), ("ALBUMARTISTSORT", "Artist, Test"), ("MUSICBRAINZ_ALBUMID", "11111111-1111-1111-1111-111111111111"),
        ("REPLAYGAIN_TRACK_GAIN", "-7.5 dB"), ("CONDUCTOR", "Conductor"), ("LYRICIST", "Lyricist"),
    ];
    let mut one = common.to_vec();
    one.extend([("TITLE", "One"), ("TRACKNUMBER", "1"), ("LYRICS", WORDS), ("COMMENT", "Recorded at home")]);
    silent(&l, &dir.join("01 One.flac"), &one);
    // the other way taggers name the lyrics, and a comment that is an advert (the import takes it out)
    let mut two = common.to_vec();
    two.extend([("TITLE", "Two"), ("TRACKNUMBER", "2"), ("UNSYNCEDLYRICS", WORDS), ("COMMENT", "Visit https://x.example.com for more")]);
    silent(&l, &dir.join("02 Two.flac"), &two);
    settle(&dir);

    let album = import(&l, &dir);
    for (name, comment) in [("01 One.mp3", true), ("02 Two.mp3", false)] {
        let fr = frames(&album.join(name));
        // the lyrics are in the lyrics field, where players read them
        assert!(same_words(&text(&fr, "USLT"), WORDS), "{name}: {:?}", text(&fr, "USLT"));
        // each of these is in its real place...
        for id in ["TBPM", "TIT1", "TCMP", "TSRC", "TPUB", "TSOP", "TPE3", "TEXT"] {
            assert!(has(&fr, id), "{name} lacks {id}: {:?}", fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>());
        }
        assert_eq!((text(&fr, "TBPM").as_str(), text(&fr, "TIT1").as_str(), text(&fr, "TSRC").as_str(), text(&fr, "TPUB").as_str()), ("120", "Group", "USABC1900001", "Label"));
        assert!(has(&fr, "TORY") || has(&fr, "TDOR"), "{name}: the original year");
        assert!(text(&fr, "TRCK").ends_with("/2") && text(&fr, "TPOS").ends_with("/2"), "{name}: {} {}", text(&fr, "TRCK"), text(&fr, "TPOS"));
        // ...and no longer in a tag that only has its name
        let left = stand_ins(&fr);
        for gone in ["uslt", "unsyncedlyrics", "lyrics", "comment", "bpm", "grouping", "compilation", "tracktotal", "disctotal", "isrc", "label", "artistsort", "originaldate", "conductor", "lyricist", "musicbrainz_albumid"] {
            assert!(!left.contains(&gone.to_string()), "{name} still has the stand-in '{gone}': {left:?}");
        }
        // what has no agreed place in an MP3 stays as it came, and is not turned into a guess
        assert!(left.contains(&"musicbrainz album id".to_string()) && left.contains(&"replaygain_track_gain".to_string()), "{left:?}");
        // the rating is whoever shared the album's, not the owner's: it does not come along
        assert!(!left.contains(&"rating".to_string()) && !has(&fr, "POPM"), "{name} kept someone else's rating: {left:?}");
        // a comment comes along; one the import took out as junk does not come back
        assert_eq!(has(&fr, "COMM"), comment, "{name}");
        // (the import changed the album artist, so the old album artist's sort name is not put in its real place)
        assert!(!has(&fr, "TSO2"), "{name}");
        // and the import's own fixes are still there
        assert_eq!(text(&fr, "TPE2"), "Various Artists", "{name}");
    }
}

#[test]
fn an_m4a_made_into_an_mp3_keeps_its_lyrics() {
    let l = lab("carry-m4a");
    let dir = l.ctx.source.join("Test Artist - Apple (2020)");
    silent(&l, &dir.join("01 One.m4a"), &[("artist", "Test Artist"), ("album", "Apple"), ("title", "One"), ("track", "1"), ("date", "2020"), ("lyrics", WORDS), ("comment", "Bought it")]);
    settle(&dir);
    let fr = frames(&import(&l, &dir).join("01 One.mp3"));
    assert!(same_words(&text(&fr, "USLT"), WORDS));
    assert!(has(&fr, "COMM"));
    // nothing about the file it was made from is left in the tag
    assert_eq!(stand_ins(&fr), Vec::<String>::new());
}

#[test]
fn an_mp3_ffmpeg_had_to_rewrite_gets_back_what_ffmpeg_dropped() {
    // the import's fallback when the tag editor refuses a file: ffmpeg rewrites it, and loses the rating,
    // the play count, the timed lyrics, DJ data and the file's id, and hides the lyrics and the comment
    let l = lab("carry-mp3");
    let src = l.base.join("src.mp3");
    silent(&l, &src, &[("artist", "Test Artist"), ("album", "Kept"), ("title", "One"), ("track", "3/12")]);
    taglib(
        &l,
        &src,
        "$f.Tag.Lyrics = \"line one`nline two\"; $f.Tag.Comment = 'A comment'; $f.Tag.Grouping = 'Group'; $f.Tag.BeatsPerMinute = 120\n\
         $id3 = $f.GetTag([TagLib.TagTypes]::Id3v2, $true)\n\
         $p = [TagLib.Id3v2.PopularimeterFrame]::Get($id3, 'SomePlayer', $true); $p.Rating = 196; $p.PlayCount = 7\n\
         $u = [TagLib.Id3v2.UniqueFileIdentifierFrame]::Get($id3, 'http://example.org', $true); $u.Identifier = [TagLib.ByteVector]::FromString('abc', [TagLib.StringType]::Latin1)\n\
         $g = [TagLib.Id3v2.AttachmentFrame]::Get($id3, 'Cue points', [TagLib.PictureType]::NotAPicture, $true); $g.Type = [TagLib.PictureType]::NotAPicture; $g.MimeType = 'application/octet-stream'; $g.Data = [TagLib.ByteVector]::FromString('cue points', [TagLib.StringType]::Latin1)\n\
         $s = [TagLib.Id3v2.SynchronisedLyricsFrame]::Get($id3, '', 'eng', [TagLib.Id3v2.SynchedTextType]::Lyrics, $true); $s.Format = [TagLib.Id3v2.TimestampFormat]::AbsoluteMilliseconds; $s.Text = @([TagLib.Id3v2.SynchedText]::new(1000, 'line one'))",
    );
    let before = frames(&src);
    for id in ["USLT", "SYLT", "COMM", "POPM", "GEOB", "UFID", "TIT1", "TBPM"] {
        assert!(has(&before, id), "the made-up file lacks {id}");
    }
    // ffmpeg rewrites it the way the import does
    let out = l.base.join("out.mp3");
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(&src).args(["-map", "0:a:0", "-c:a", "copy", "-map_metadata", "0", "-metadata", "title=One (fixed)", "-id3v2_version", "3"]).arg(&out);
    assert!(tools::run_limited(&mut c, 60));
    let lost = frames(&out);
    assert!(!has(&lost, "USLT") && !has(&lost, "POPM") && !has(&lost, "GEOB") && !has(&lost, "SYLT") && !has(&lost, "UFID") && !has(&lost, "COMM"), "ffmpeg no longer loses these: the carry may not be needed");

    // the carry: what is about the music is back, the import's own change to the title stays
    let mut t = Track::default();
    t.set_new("title", "One (fixed)");
    let failed = carry_tags(&l.ctx, &[carry_plan(&out, &src, &t)], &l.ctx.work_dir);
    assert!(failed.is_empty(), "{failed:?}");
    let fr = frames(&out);
    for id in ["USLT", "SYLT", "COMM", "UFID", "TIT1", "TBPM"] {
        assert!(has(&fr, id), "{id} did not come back: {:?}", fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>());
    }
    // the rating, the play count and the DJ data were another person's: they are not brought back
    assert!(!has(&fr, "POPM") && !has(&fr, "GEOB"), "{:?}", fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>());
    assert!(same_words(&text(&fr, "USLT"), "line one line two"));
    assert_eq!(text(&fr, "TIT2"), "One (fixed)");
    assert_eq!(stand_ins(&fr), Vec::<String>::new());
    // a comment the import took out as junk stays out
    let out2 = l.base.join("out2.mp3");
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(&src).args(["-map", "0:a:0", "-c:a", "copy", "-map_metadata", "0", "-metadata", "comment=", "-id3v2_version", "3"]).arg(&out2);
    assert!(tools::run_limited(&mut c, 60));
    t.junk.push(("comment".into(), "A comment".into(), "web link".into()));
    assert!(carry_tags(&l.ctx, &[carry_plan(&out2, &src, &t)], &l.ctx.work_dir).is_empty());
    let fr = frames(&out2);
    assert!(!has(&fr, "COMM") && !has(&fr, "POPM") && has(&fr, "USLT"));
}

#[test]
fn every_file_of_a_long_album_is_edited_the_same_way() {
    // the tag editor works on four files at a time and uses each worker again; a worker's second file must
    // be treated as its first was (it once lost the table of what to carry, and failed from the fifth file on)
    let l = lab("carry-many");
    let mut plans = vec![];
    let mut outs = vec![];
    for i in 1..=9 {
        let src = l.base.join(format!("src-{i}.mp3"));
        silent(&l, &src, &[("artist", "Test Artist"), ("album", "Long"), ("title", "Song")]);
        taglib(&l, &src, "$f.Tag.Lyrics = \"line one`nline two\"; $f.Tag.BeatsPerMinute = 120; $f.Tag.Grouping = 'Group'");
        // ffmpeg rewrites it the way the import's fallback does, which hides the lyrics
        let out = l.base.join(format!("out-{i}.mp3"));
        let mut c = tools::command(&l.ctx.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(&src).args(["-map", "0:a:0", "-c:a", "copy", "-map_metadata", "0", "-id3v2_version", "3"]).arg(&out);
        assert!(tools::run_limited(&mut c, 60));
        assert!(!has(&frames(&out), "USLT"));
        plans.push(carry_plan(&out, &src, &Track::default()));
        outs.push(out);
    }
    let failed = carry_tags(&l.ctx, &plans, &l.ctx.work_dir);
    assert!(failed.is_empty(), "{failed:?}");
    for out in &outs {
        let fr = frames(out);
        assert!(same_words(&text(&fr, "USLT"), "line one line two") && has(&fr, "TIT1") && stand_ins(&fr).is_empty(), "{}: {:?} {:?}", out.display(), fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), stand_ins(&fr));
    }
}

/// A tagged MP3 as a player leaves it: lyrics, a rating with a play count, DJ cue points, and DJ software's
/// own private entry beside another program's.
fn played_mp3(l: &Lab, file: &Path, tags: &[(&str, &str)]) {
    silent(l, file, tags);
    taglib(
        l,
        file,
        "$f.Tag.Lyrics = \"line one`nline two\"; $f.Tag.BeatsPerMinute = 120\n\
         $id3 = $f.GetTag([TagLib.TagTypes]::Id3v2, $true)\n\
         $p = [TagLib.Id3v2.PopularimeterFrame]::Get($id3, 'SomePlayer', $true); $p.Rating = 196; $p.PlayCount = 7\n\
         $g = [TagLib.Id3v2.AttachmentFrame]::Get($id3, 'Cue points', [TagLib.PictureType]::NotAPicture, $true); $g.Type = [TagLib.PictureType]::NotAPicture; $g.MimeType = 'application/octet-stream'; $g.Data = [TagLib.ByteVector]::FromString('cue points', [TagLib.StringType]::Latin1)\n\
         $d = [TagLib.Id3v2.PrivateFrame]::Get($id3, 'TRAKTOR4', $true); $d.PrivateData = [TagLib.ByteVector]::FromString('deck data', [TagLib.StringType]::Latin1)\n\
         $o = [TagLib.Id3v2.PrivateFrame]::Get($id3, 'SomeEncoder', $true); $o.PrivateData = [TagLib.ByteVector]::FromString('settings', [TagLib.StringType]::Latin1)\n\
         $r = [TagLib.Id3v2.UserTextInformationFrame]::Get($id3, 'FMPS_RATING', $true); $r.Text = @('0.8')",
    );
    let fr = frames(file);
    for id in ["USLT", "POPM", "GEOB", "TBPM"] {
        assert!(has(&fr, id), "the made-up file lacks {id}");
    }
    assert_eq!(fr.iter().filter(|f| f.id == "PRIV").count(), 2);
    assert!(stand_ins(&fr).contains(&"fmps_rating".to_string()));
}

#[test]
fn an_imported_mp3_loses_the_rating_and_dj_data_of_whoever_shared_it() {
    // the commonest import: an MP3 album whose audio is kept byte for byte and whose tags are edited in place
    let l = lab("drop-personal");
    let dir = l.ctx.source.join("Test Artist - Shared (2022)");
    played_mp3(&l, &dir.join("01 One.mp3"), &[("artist", "Test Artist"), ("album", "Shared"), ("title", "One"), ("track", "1"), ("date", "2022")]);
    settle(&dir);
    let fr = frames(&import(&l, &dir).join("01 One.mp3"));
    assert!(!has(&fr, "POPM") && !has(&fr, "PCNT") && !has(&fr, "GEOB"), "{:?}", fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>());
    assert!(!stand_ins(&fr).contains(&"fmps_rating".to_string()));
    // DJ software's private entry goes; another program's stays
    assert_eq!(fr.iter().filter(|f| f.id == "PRIV").count(), 1);
    // what is about the music stays
    assert!(same_words(&text(&fr, "USLT"), "line one line two") && text(&fr, "TBPM") == "120");
}

#[test]
fn a_tag_edit_that_is_not_an_import_keeps_the_rating() {
    // a Health repair goes through the same tag editor and never asks for the rating to go: in the library
    // the rating is the owner's
    let l = lab("keep-personal");
    let file = l.base.join("mine.mp3");
    played_mp3(&l, &file, &[("artist", "Test Artist"), ("album", "Mine"), ("title", "One"), ("track", "1")]);
    let plan = serde_json::json!({ "File": file.to_string_lossy(), "Set": { "title": "One (fixed)" }, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false });
    assert!(carry_tags(&l.ctx, &[plan], &l.ctx.work_dir).is_empty());
    let fr = frames(&file);
    assert_eq!(text(&fr, "TIT2"), "One (fixed)");
    assert!(has(&fr, "POPM") && has(&fr, "GEOB") && stand_ins(&fr).contains(&"fmps_rating".to_string()), "{:?}", fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>());
    assert_eq!(fr.iter().filter(|f| f.id == "PRIV").count(), 2);
}

pub(super) fn found() -> super::lyrics::Answer {
    super::lyrics::Answer::Found { plain: FOUND_PLAIN.into(), timed: FOUND_TIMED.into() }
}

/// An MP3 album whose five tracks carry lyrics in every state.
fn mixed_album(l: &Lab, folder: &str, album: &str) -> PathBuf {
    let dir = l.ctx.source.join(folder);
    for (no, title) in [(1, "Empty"), (2, "Good"), (3, "Address"), (4, "Credit"), (5, "Hidden")] {
        let n = no.to_string();
        silent(l, &dir.join(format!("0{no} {title}.mp3")), &[("artist", "Test Artist"), ("album", album), ("title", title), ("track", n.as_str()), ("date", "2021")]);
    }
    let words = WORDS.replace('\n', "`n");
    taglib(l, &dir.join("02 Good.mp3"), &format!("$f.Tag.Lyrics = \"{words}\""));
    taglib(l, &dir.join("03 Address.mp3"), "$f.Tag.Lyrics = 'www.t.me/pmedia_music'");
    taglib(l, &dir.join("04 Credit.mp3"), &format!("$f.Tag.Lyrics = \"{words}`nLyrics provided by SomeSite.com\""));
    taglib(l, &dir.join("05 Hidden.mp3"), &format!("$id3 = $f.GetTag([TagLib.TagTypes]::Id3v2, $true); $id3.SetUserTextAsString('USLT', \"{words}\")"));
    settle(&dir);
    dir
}

#[test]
fn the_list_knows_how_each_tracks_lyrics_stand() {
    use super::lyrics::Place;
    let l = lab("lyric-facts");
    let u = scan(&l, &mixed_album(&l, "Test Artist - Mixed (2021)", "Mixed"));
    let by = |title: &str| &u.tracks.iter().find(|t| t.old("title") == title).unwrap().lyric;
    assert!(by("Empty").place == Place::None && by("Empty").missing());
    assert!(by("Good").place == Place::Field && by("Good").there() && !by("Good").has_junk());
    assert!(by("Address").junk.only && by("Address").missing() && by("Address").junk.lines[0].1 == "web address");
    assert!(by("Credit").has_junk() && !by("Credit").junk.only && by("Credit").junk.lines == [("Lyrics provided by SomeSite.com".to_string(), "site credit")]);
    assert!(by("Hidden").place == Place::Hidden && !by("Hidden").missing());
    // LRCLIB is asked about the ones with none and the flagged ones, never about lyrics that are fine
    let asked: Vec<String> = super::scan::lyric_asks(&u).into_iter().map(|a| a.2).collect();
    assert_eq!(asked, ["Empty", "Address", "Credit"]);
}

#[test]
fn an_import_fills_missing_lyrics_and_never_writes_over_good_ones() {
    use super::lyrics::{Answer, Choice};
    let l = lab("lyric-fill");
    // LRCLIB "has" lyrics for every song here, the good one included: it must not matter
    let kept = [("Empty", found()), ("Good", found()), ("Address", Answer::Nothing), ("Credit", found()), ("Hidden", found())];
    let album = import_with(&l, &mixed_album(&l, "Test Artist - Mixed (2021)", "Mixed"), &kept, Choice::Replace);
    let lyrics = |name: &str| text(&frames(&album.join(name)), "USLT");
    // none: filled, with a time on every line
    assert_eq!(lyrics("01 Empty.mp3"), FOUND_TIMED);
    // good ones: exactly as they came
    assert!(same_words(&lyrics("02 Good.mp3"), WORDS));
    // nothing but an address, and LRCLIB has nothing: the field is emptied
    assert!(!has(&frames(&album.join("03 Address.mp3")), "USLT"));
    // a credit under real words, and the choice is to replace: LRCLIB's
    assert_eq!(lyrics("04 Credit.mp3"), FOUND_TIMED);
    // hidden in a tag no player reads: into the field, and the tag that hid them is gone
    let fr = frames(&album.join("05 Hidden.mp3"));
    assert!(same_words(&text(&fr, "USLT"), WORDS) && stand_ins(&fr).is_empty(), "{:?} / USLT {:?} / {:?}", stand_ins(&fr), text(&fr, "USLT"), fr.iter().map(|f| f.id.as_str()).collect::<Vec<_>>());

    // "Cut these lines": the words stay, the credit goes, LRCLIB's are not used
    let album = import_with(&l, &mixed_album(&l, "Test Artist - Cut (2021)", "Cut"), &kept, Choice::Cut);
    assert!(same_words(&text(&frames(&album.join("04 Credit.mp3")), "USLT"), WORDS));
    assert!(!has(&frames(&album.join("03 Address.mp3")), "USLT"));
    assert_eq!(text(&frames(&album.join("01 Empty.mp3")), "USLT"), FOUND_TIMED);

    // "Leave as they are": flagged lyrics come in untouched; missing ones are still filled
    let album = import_with(&l, &mixed_album(&l, "Test Artist - Kept (2021)", "Kept"), &kept, Choice::Keep);
    assert!(text(&frames(&album.join("04 Credit.mp3")), "USLT").contains("Lyrics provided by SomeSite.com"));
    assert_eq!(text(&frames(&album.join("03 Address.mp3")), "USLT"), "www.t.me/pmedia_music");
    assert_eq!(text(&frames(&album.join("01 Empty.mp3")), "USLT"), FOUND_TIMED);

    // nothing known about a song (offline, never asked; a fresh folder of answers): nothing is written
    let l = lab("lyric-offline");
    let album = import_with(&l, &mixed_album(&l, "Test Artist - Offline (2021)", "Offline"), &[], Choice::Keep);
    assert!(!has(&frames(&album.join("01 Empty.mp3")), "USLT"));
}

#[test]
fn a_converted_track_gets_lyrics_too_and_plain_words_when_that_is_the_setting() {
    use super::lyrics::Choice;
    let mut l = lab("lyric-flac");
    l.ctx.lyrics.timed = false;
    let dir = l.ctx.source.join("Test Artist - Lossless (2022) [FLAC]");
    let credit = format!("{WORDS}\nVisit www.somesite.com for more lyrics");
    for (no, title, lyrics) in [(1, "Empty", ""), (2, "Credit", credit.as_str()), (3, "Good", WORDS)] {
        let n = no.to_string();
        let mut tags = vec![("ARTIST", "Test Artist"), ("ALBUM", "Lossless"), ("TITLE", title), ("TRACKNUMBER", n.as_str()), ("DATE", "2022")];
        if !lyrics.is_empty() {
            tags.push(("LYRICS", lyrics));
        }
        silent(&l, &dir.join(format!("0{no} {title}.flac")), &tags);
    }
    settle(&dir);
    let album = import_with(&l, &dir, &[("Empty", found()), ("Good", found())], Choice::Replace);
    // filled with the plain words (the setting), in the lyrics field of the MP3 it became
    assert_eq!(text(&frames(&album.join("01 Empty.mp3")), "USLT"), FOUND_PLAIN);
    // flagged, to be replaced, but LRCLIB was never asked about it: the junk line is cut, the words stay
    let fr = frames(&album.join("02 Credit.mp3"));
    assert!(same_words(&text(&fr, "USLT"), WORDS), "{:?}", text(&fr, "USLT"));
    assert!(same_words(&text(&frames(&album.join("03 Good.mp3")), "USLT"), WORDS));
}
