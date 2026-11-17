//! The lyric repairs and their Undo, on silent made-up files in a scratch folder: never the real library,
//! never online (LRCLIB's "answers" are put where the app keeps them), and nothing is played. They need
//! ffmpeg and ffprobe.
use super::*;
use crate::importer::id3;
use crate::test_tags::{edit_id3, personal, set_lyrics, timed};
use ::id3::{Tag, TagLike};
use ::id3::frame::{ExtendedText, Lyrics};
use crate::importer::lyrics::{Answer, Lookup};

const WORDS: &str = "The lamps along the water\nburn a little lower now\nand every boat that leaves the harbor\ntakes a piece of you somehow";
const PLAIN: &str = "words from the library\nline two of them\nline three of them";
const TIMED: &str = "[00:00.10]words from the library\n[00:00.40]line two of them\n[00:00.70]line three of them";

struct Lab {
    base: PathBuf,
    lib: PathBuf,
    root: PathBuf,
    tools: Tools,
}
impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn lab(name: &str) -> Lab {
    let base = std::env::temp_dir().join(format!("music-recs-lyricfix-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
    let (lib, root) = (base.join("lib"), base.join("undo"));
    std::fs::create_dir_all(&lib).unwrap();
    let lyrics = Lookup { dir: base.join("lyrics"), online: false, timed: true, agent: ureq::Agent::new_with_defaults() };
    let tools = Tools { ffmpeg: tools::find_tool("ffmpeg").unwrap(), ffprobe: tools::find_tool("ffprobe").unwrap(), quality_cache: None, online: false, lyrics: Some(lyrics) };
    Lab { base, lib, root, tools }
}

impl Lab {
    /// A one-second silent MP3 of the album "A\Album (2020)", with a player's tags.
    fn track(&self, no: u32, title: &str, edit: impl FnOnce(&mut Tag)) -> PathBuf {
        let out = self.lib.join("A").join("Album (2020)").join(format!("0{no} {title}.mp3"));
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let mut c = tools::command(&self.tools.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "1", "-c:a", "libmp3lame", "-b:a", "320k", "-id3v2_version", "3"]);
        c.args(["-metadata", "artist=A", "-metadata", "album=Album", "-metadata", &format!("title={title}"), "-metadata", &format!("track={no}"), "-metadata", "comment=Bought it"]);
        c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        assert!(tools::run_limited(&mut c, 60));
        // every file carries what a player or tagger would have left in it, lyrics or not
        edit_id3(&out, |tag| {
            personal(tag);
            edit(tag);
        });
        out
    }

    fn keep(&self, title: &str, answer: Answer) {
        self.tools.lyrics.as_ref().unwrap().keep("A", title, 1.0, &answer);
    }

    fn fix(&self, kind: &str, choice: &str, files: &[&Path]) -> Done {
        let ask = Ask {
            fix: kind.into(),
            cat: "x".into(),
            path: self.lib.join("A").join("Album (2020)").to_string_lossy().to_string(),
            files: files.iter().map(|f| f.to_string_lossy().to_string()).collect(),
            artist: "A".into(),
            album: "Album (2020)".into(),
            data: HashMap::new(),
            choice: choice.into(),
        };
        apply(&plan(&ask, &self.lib, &self.tools), "test", &self.lib, &self.root, &self.tools).unwrap()
    }

    /// Whether the undo backups hold a copy of any track (a lyrics-only change keeps none).
    fn kept_files(&self) -> usize {
        crate::importer::util::walk_files(&self.root).iter().filter(|p| p.extension().is_some_and(|e| e == "mp3")).count()
    }
}

fn lyrics(file: &Path) -> String {
    id3::frames(file).unwrap().iter().find(|f| f.id == "USLT").map(|f| f.text.replace("\r\n", "\n")).unwrap_or_default()
}
/// Every entry of the tag but the lyrics, by name.
fn others(file: &Path) -> Vec<String> {
    let mut v: Vec<String> = id3::frames(file).unwrap().iter().filter(|f| f.id != "USLT").map(|f| format!("{}:{}:{}", f.id, f.desc, f.text)).collect();
    v.sort();
    v
}
fn found() -> Answer {
    Answer::Found { plain: PLAIN.into(), timed: TIMED.into() }
}
#[test]
fn missing_lyrics_are_filled_without_keeping_a_copy_of_the_file_and_undo_takes_them_out_again() {
    let l = lab("fill");
    let (empty, good, unknown) = (l.track(1, "Empty", |_| {}), l.track(2, "Good", |tag| set_lyrics(tag, WORDS)), l.track(3, "Unknown", |_| {}));
    l.keep("Empty", found());
    l.keep("Good", found());
    let before = (others(&empty), std::fs::metadata(&empty).unwrap().len());

    let done = l.fix("lyrics-fill", "", &[&empty, &good, &unknown]);
    // the song without lyrics has LRCLIB's, timed; nothing else in its tag changed (the rating, the cue points...)
    assert_eq!(lyrics(&empty), TIMED);
    assert_eq!(others(&empty), before.0);
    assert!(before.0.iter().any(|f| f.starts_with("POPM")) && before.0.iter().any(|f| f.starts_with("GEOB")), "{:?}", before.0);
    // lyrics that were there are never written over, whatever LRCLIB has
    assert_eq!(lyrics(&good), WORDS);
    // a song nothing is known about stays as it is, and that is said
    assert_eq!(lyrics(&unknown), "");
    assert!(done.problems.len() == 1 && done.problems[0].contains("03 Unknown.mp3"), "{:?}", done.problems);
    // one change, and no copy of the old file anywhere in the undo backups
    assert_eq!((done.changed, l.kept_files()), (1, 0));

    // Undo: the lyrics are gone again and the rest is as it was
    let id = done.batch.unwrap();
    assert_eq!(undo(&l.root, &id).unwrap(), Vec::<String>::new());
    assert_eq!(lyrics(&empty), "");
    assert_eq!(others(&empty), before.0);
    assert!(undo(&l.root, &id).is_err(), "a repair is undone once");

    // lyrics somebody changed after the repair are theirs: Undo leaves them and says so
    let done = l.fix("lyrics-fill", "", &[&empty]);
    assert_eq!(lyrics(&empty), TIMED);
    edit_id3(&empty, |tag| set_lyrics(tag, "my own words now"));
    let said = undo(&l.root, &done.batch.unwrap()).unwrap();
    assert!(said.len() == 1 && said[0].contains("changed after the repair"), "{said:?}");
    assert_eq!(lyrics(&empty), "my own words now");
}

#[test]
fn flagged_lyrics_are_replaced_or_cleaned_and_undo_brings_the_old_words_back() {
    let l = lab("junk");
    let credit_text = format!("{WORDS}\nLyrics provided by SomeSite.com");
    let address = l.track(1, "Address", |tag| set_lyrics(tag, "www.t.me/pmedia_music"));
    let credit = l.track(2, "Credit", |tag| set_lyrics(tag, &credit_text));
    let other = l.track(3, "Other", |tag| set_lyrics(tag, &credit_text));
    l.keep("Address", Answer::Nothing);
    l.keep("Credit", found());

    // replace: LRCLIB's where it has them; where it has nothing and the field held only junk, the field is
    // emptied; a song LRCLIB was never asked about is left alone
    let done = l.fix("lyrics-junk", "", &[&address, &credit, &other]);
    assert_eq!((lyrics(&address).as_str(), lyrics(&credit).as_str()), ("", TIMED));
    assert_eq!(lyrics(&other), credit_text);
    assert!(done.problems.len() == 1 && done.problems[0].contains("03 Other.mp3"), "{:?}", done.problems);
    assert_eq!((done.changed, l.kept_files()), (2, 0));
    // Undo: the old words are back, junk and all
    assert_eq!(undo(&l.root, &done.batch.unwrap()).unwrap(), Vec::<String>::new());
    assert_eq!((lyrics(&address).as_str(), lyrics(&credit)), ("www.t.me/pmedia_music", credit_text.clone()));

    // "only cut the junk lines": the words stay, the credit goes, and LRCLIB is not needed
    let done = l.fix("lyrics-junk", "cut", &[&credit, &other]);
    assert_eq!((lyrics(&credit).as_str(), lyrics(&other).as_str()), (WORDS, WORDS));
    assert!(done.problems.is_empty(), "{:?}", done.problems);
    // lyrics that are fine are not touched by either repair
    let again = l.fix("lyrics-junk", "", &[&credit]);
    assert!(again.changed == 0 && again.batch.is_none());
}

#[test]
fn lyrics_no_player_reads_go_into_the_lyrics_field_and_undo_puts_the_file_back_whole() {
    let l = lab("unhide");
    let hidden = l.track(1, "Hidden", |tag| {
        tag.add_frame(ExtendedText { description: "USLT".into(), value: WORDS.into() });
    });
    let bytes = std::fs::read(&hidden).unwrap();
    assert_eq!(lyrics(&hidden), "");

    let done = l.fix("lyrics-unhide", "", &[&hidden]);
    assert!(done.problems.is_empty(), "{:?}", done.problems);
    assert_eq!(lyrics(&hidden), WORDS);
    assert!(!id3::frames(&hidden).unwrap().iter().any(|f| f.id == "TXXX" && f.desc == "USLT"), "the tag that hid them is gone");
    // the old words would be nowhere else, so the old file is kept whole, as for any other repair
    assert_eq!(l.kept_files(), 1);
    assert_eq!(undo(&l.root, &done.batch.unwrap()).unwrap(), Vec::<String>::new());
    assert_eq!(std::fs::read(&hidden).unwrap(), bytes);
}

/// Codex's review, H2: an import puts a new copy of the track in place while a lyrics repair is working on a
/// copy of the old one. The repair must see that its file changed and save nothing, so the import stays.
#[test]
fn a_lyrics_repair_never_saves_over_a_track_an_import_replaced_meanwhile() {
    let l = lab("import-meanwhile");
    let text = format!("{WORDS}\nLyrics provided by SomeSite.com");
    let file = l.track(1, "Overlap", |tag| set_lyrics(tag, &text));
    let album = file.parent().unwrap().to_path_buf();
    // the import's finished album: the same track, three seconds long
    let build = l.base.join("import-build");
    std::fs::create_dir_all(&build).unwrap();
    let new = build.join(file.file_name().unwrap());
    let mut c = tools::command(&l.tools.ffmpeg, false);
    c.args(["-hide_banner", "-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "3", "-c:a", "libmp3lame", "-b:a", "320k", "-metadata", "title=Overlap", "-metadata", "artist=A", "-metadata", "album=Album"]);
    c.arg(&new).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60));
    let imported = std::fs::read(&new).unwrap();
    let (root, library, installed, expected) = (l.root.clone(), l.lib.clone(), file.clone(), imported.clone());
    test_pause::AFTER_TAGS.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            let placed = crate::importer::import::place_album(&root, &library, "A", &build, &album, Some(&album)).unwrap();
            assert!(placed.problem.is_none() && placed.old_copy.is_some());
            assert_eq!(std::fs::read(&installed).unwrap(), expected);
        }))
    });
    let done = l.fix("lyrics-junk", "cut", &[&file]);
    // the repair saw the new file and saved nothing: the imported track is in the library, byte for byte
    assert_eq!(done.changed, 0, "{:?}", done.problems);
    assert!(done.problems.len() == 1 && done.problems[0].contains("changed by something else"), "{:?}", done.problems);
    assert!(std::fs::read(&file).unwrap() == imported, "the imported track was written over");
    // and the import's own Undo still brings the old album back
    let import = crate::undo::batches(&l.root).into_iter().find(|b| b.title.starts_with("Import: replaced A")).unwrap();
    assert_eq!(crate::undo::undo(&l.root, &import.id).unwrap(), Vec::<String>::new());
    assert_eq!(lyrics(&file), text);
}

/// Codex's review, H1: an MP3 with a second lyrics frame (a translation) and timed lyrics. A lyrics cleanup
/// takes out every lyrics frame; the light undo record holds one text, so Undo used to bring back only the
/// first. Such a file keeps its whole old copy, and Undo puts it back byte for byte.
#[test]
fn an_mp3_with_more_than_one_lyrics_frame_comes_back_whole_after_undo() {
    let l = lab("many-frames");
    let text = format!("{WORDS}\nLyrics provided by SomeSite.com");
    let file = l.track(1, "Multi", |tag| {
        set_lyrics(tag, &text);
        tag.add_frame(Lyrics { lang: "fra".into(), description: "translation".into(), text: "Une traduction a garder".into() });
        timed(tag, "timed original", &[(100, "The lamps"), (600, "along the water")]);
    });
    // one timed frame alone counts too
    let timed_only = l.track(2, "Timed", |tag| {
        set_lyrics(tag, &text);
        timed(tag, "t", &[(100, "The lamps")]);
    });
    let count = |f: &Path| {
        let f = id3::frames(f).unwrap();
        (f.iter().filter(|x| x.id == "USLT").count(), f.iter().filter(|x| x.id == "SYLT").count())
    };
    assert_eq!((count(&file), count(&timed_only)), ((2, 1), (1, 1)));
    let before = (std::fs::read(&file).unwrap(), std::fs::read(&timed_only).unwrap());
    let done = l.fix("lyrics-junk", "cut", &[&file, &timed_only]);
    assert!(done.problems.is_empty(), "{:?}", done.problems);
    assert_eq!(done.changed, 2);
    assert!(!lyrics(&file).contains("SomeSite"));
    // the whole old files are kept, so nothing of them is only in the undo record's words
    assert_eq!(l.kept_files(), 2);
    assert_eq!(undo(&l.root, &done.batch.unwrap()).unwrap(), Vec::<String>::new());
    assert_eq!(count(&file), (2, 1));
    assert!(std::fs::read(&file).unwrap() == before.0, "the file did not come back byte for byte");
    assert!(std::fs::read(&timed_only).unwrap() == before.1, "the file did not come back byte for byte");
}
