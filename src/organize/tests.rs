//! Organize, tried on small libraries made in a temp folder for each test. The tracks are tiny files whose
//! bytes are their tags (as JSON), read by a stand-in for the app's tag reading, so no ffprobe is needed and
//! a track's tags travel with it.
use super::*;
use std::path::{Path, PathBuf};

/// Faults a test can set for the next run on its own thread.
pub(super) mod fault {
    use std::cell::Cell;
    thread_local! {
        pub static CRASH_BEFORE_RENAME: Cell<bool> = const { Cell::new(false) };
        pub static CRASH_BEFORE_DONE: Cell<bool> = const { Cell::new(false) };
        pub static CRASH_MID_HOP: Cell<bool> = const { Cell::new(false) };
        pub static FAIL_RENAMES: Cell<u32> = const { Cell::new(0) };
    }
    pub fn crash_before_rename() -> bool {
        CRASH_BEFORE_RENAME.with(|c| c.replace(false))
    }
    pub fn crash_mid_hop() -> bool {
        CRASH_MID_HOP.with(|c| c.replace(false))
    }
    pub fn crash_before_done() -> bool {
        CRASH_BEFORE_DONE.with(|c| c.replace(false))
    }
    pub fn fail_rename() -> bool {
        FAIL_RENAMES.with(|c| {
            let n = c.get();
            if n > 0 {
                c.set(n - 1);
            }
            n > 0
        })
    }
}

struct Lab {
    base: PathBuf,
    lib: PathBuf,
}

impl Lab {
    fn new(name: &str) -> Lab {
        let base = std::env::temp_dir().join(format!("music-recs-organize-{name}-{}-{}", std::process::id(), rand_hex(6)));
        let lib = base.join("Music");
        std::fs::create_dir_all(&lib).unwrap();
        Lab { base, lib }
    }
    fn root(&self) -> PathBuf {
        crate::repair::backups_root(&self.lib)
    }
    fn p(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.lib.clone() } else { self.lib.join(rel) }
    }
    /// A track whose bytes are its tags.
    fn track(&self, rel: &str, artist: &str, album: &str, year: &str) {
        self.track_tags(rel, serde_json::json!({ "artist": artist, "album": album, "date": year }));
    }
    fn track_tags(&self, rel: &str, tags: serde_json::Value) {
        let p = self.p(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, tags.to_string()).unwrap();
    }
    fn file(&self, rel: &str, what: &str) {
        let p = self.p(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, what).unwrap();
    }
    fn preview(&self) -> Preview {
        self.preview_with(&PreviewOptions::default())
    }
    fn preview_with(&self, o: &PreviewOptions) -> Preview {
        preview(&self.lib, o, &reader, &|_, _| {}).unwrap()
    }
    fn run(&self, p: &Preview) -> Result<RunResult, RunError> {
        run(&self.lib, p, &Hooks { settled: &|_, _, _| {}, cancel: &|| false, still_there: &|| true, is_playing: &|_| false, progress: &|_, _, _| {} })
    }
    /// Where an album named `name` lands in the artist folder `artist` (a long name shortened as Organize does).
    fn placed(&self, artist: &str, name: &str) -> PathBuf {
        place_for(&self.p(artist), name)
    }
    fn why(&self, p: &Preview, rel: &str) -> String {
        p.left_alone.iter().find(|l| l.folder == self.p(rel)).map(|l| l.why.clone()).unwrap_or_else(|| format!("not left alone: {:?}", p.left_alone))
    }
    fn to_of(&self, p: &Preview, rel: &str) -> Option<PathBuf> {
        p.moves.iter().find(|m| m.from == self.p(rel)).map(|m| m.to.clone())
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // (read-only files a test made would stop the clean-up)
        for e in walk_all(&self.base) {
            if let Ok(m) = std::fs::metadata(&e) {
                let mut perm = m.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perm.set_readonly(false);
                let _ = std::fs::set_permissions(&e, perm);
            }
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn walk_all(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        out.push(e.path());
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            out.extend(walk_all(&e.path()));
        }
    }
    out
}

/// The stand-in tag reader: the file's bytes are its tags.
fn reader(p: &Path) -> Option<TrackTags> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Some(TrackTags { album_artist: s("album_artist"), artist: s("artist"), album: s("album"), date: s("date"), compilation: v["compilation"] == true })
}

/// Every file and folder under `dir`: path from it, size, modified time (folders too, so a folder made or
/// changed shows).
fn full_listing(dir: &Path) -> Vec<(String, u64, u128, bool)> {
    let mut out: Vec<(String, u64, u128, bool)> = walk_all(dir)
        .into_iter()
        .map(|p| {
            let m = std::fs::metadata(&p).unwrap();
            let t = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            (p.strip_prefix(dir).unwrap().to_string_lossy().to_string(), if m.is_dir() { 0 } else { m.len() }, t, m.is_dir())
        })
        .collect();
    out.sort();
    out
}

/// The files only, names exact, with sizes and times.
fn files_listing(dir: &Path) -> Vec<(String, u64, u128)> {
    full_listing(dir).into_iter().filter(|e| !e.3).map(|e| (e.0, e.1, e.2)).collect()
}

fn log_of(lab: &Lab, id: &str) -> Vec<serde_json::Value> {
    crate::undo::entries(&lab.root().join(id))
}

fn only_batch(lab: &Lab) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(lab.root()).unwrap().flatten().map(|e| e.path()).collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs[0].clone()
}

// ------------------------------------------------------------------ names

#[test]
fn organize_names_come_from_tags_with_the_importers_rules() {
    let t = |aa: &str, a: &str, al: &str, d: &str| Some(TrackTags { album_artist: aa.into(), artist: a.into(), album: al.into(), date: d.into(), compilation: false });
    assert_eq!(target_names(&[t("", "Radiohead", "In Rainbows", "2007-10-10")], "Various Artists"), Ok(("Radiohead".into(), "In Rainbows (2007)".into())));
    // no year: just the album
    assert_eq!(target_names(&[t("", "Radiohead", "In Rainbows", "")], "Various Artists"), Ok(("Radiohead".into(), "In Rainbows".into())));
    // a year already at the end is not doubled
    assert_eq!(target_names(&[t("", "X", "Live (1999)", "1999")], "Various Artists"), Ok(("X".into(), "Live (1999)".into())));
    // the album artist wins over the track artists
    assert_eq!(target_names(&[t("Massive Attack", "Massive Attack feat. Tracey Thorn", "Protection", "1994"), t("Massive Attack", "Massive Attack", "Protection", "1994")], "Various Artists").unwrap().0, "Massive Attack");
    // disc notes are one album
    assert_eq!(target_names(&[t("", "A", "Big (Disc 1)", "2001"), t("", "A", "Big (Disc 2)", "2001"), t("", "A", "Big [CD 3]", "2001")], "Various Artists").unwrap().1, "Big (2001)");
    // forbidden characters, trailing dots and spaces, reserved names
    assert_eq!(target_names(&[t("", "AC/DC", "What?: Live...", "")], "Various Artists"), Ok(("AC_DC".into(), "What_ - Live".into())));
    assert_eq!(target_names(&[t("", "CON", "nul", "")], "Various Artists"), Ok(("_CON".into(), "_nul".into())));
    assert_eq!(target_names(&[t("", "COM1 ", "Ends with a space ", "")], "Various Artists").unwrap().0, "_COM1");
    // what stays
    assert_eq!(target_names(&[t("", "A", "", "")], "Various Artists"), Err("missing_tags"));
    assert_eq!(target_names(&[t("", "A", "X", ""), None], "Various Artists"), Err("missing_tags"));
    assert_eq!(target_names(&[t("", "", "X", "")], "Various Artists"), Err("missing_tags"));
    assert_eq!(target_names(&[t("", "...", "X", "")], "Various Artists"), Err("missing_tags"));
    assert_eq!(target_names(&[t("", "A", "X", ""), t("", "B", "X", "")], "Various Artists"), Err("mixed_artists"));
    assert_eq!(target_names(&[t("A", "A", "X", ""), t("B", "B", "X", "")], "Various Artists"), Err("mixed_artists"));
    assert_eq!(target_names(&[t("Various Artists", "A", "X", ""), t("Various Artists", "B", "X", "")], "Various Artists"), Ok(("Various Artists".into(), "X".into())));
    assert_eq!(target_names(&[t("", "VA", "X", "")], "Various Artists"), Ok(("Various Artists".into(), "X".into())));
    let mut flagged = t("", "A", "X", "").unwrap();
    flagged.compilation = true;
    assert_eq!(target_names(&[Some(flagged)], "Compi: lations"), Ok(("Compi - lations".into(), "X".into())));
    assert_eq!(target_names(&[t("", "A", "X", ""), t("", "A", "Y", "")], "Various Artists"), Err("two_albums_one_folder"));
    assert_eq!(target_names(&[], "Various Artists"), Err("missing_tags"));
}

#[test]
fn organize_tags_are_read_from_the_names_ffprobe_gives() {
    let m: std::collections::BTreeMap<String, String> = [("albumartist", "Radiohead"), ("artist", "Thom"), ("album", "OK Computer"), ("date", "1997"), ("compilation", "1")].into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    let t = TrackTags::from_map(&m);
    assert_eq!((t.album_artist.as_str(), t.artist.as_str(), t.album.as_str(), t.date.as_str(), t.compilation), ("Radiohead", "Thom", "OK Computer", "1997", true));
}

// ------------------------------------------------------------------ preview and run

#[test]
fn organize_happy_path_moves_albums_with_their_sidecars_as_one_batch() {
    let l = Lab::new("happy");
    l.track(r"radiohead - in rainbows\01 15 Step.mp3", "Radiohead", "In Rainbows", "2007");
    l.track(r"radiohead - in rainbows\07 Reckoner.flac", "Radiohead", "In Rainbows", "2007");
    l.file(r"radiohead - in rainbows\cover.jpg", "picture");
    l.file(r"radiohead - in rainbows\In Rainbows.cue", "cue");
    l.file(r"radiohead - in rainbows\scans\back.png", "scan");
    l.track(r"Radiohead\OK Computer\01 Airbag.mp3", "Radiohead", "OK Computer", "1997");
    l.track(r"Radiohead\Kid A (2000)\01 Everything.mp3", "Radiohead", "Kid A", "2000");
    l.track(r"Thom\Eraser\01.mp3", "Thom Yorke", "The Eraser", "2006");
    let p = l.preview();
    assert_eq!((p.to_move, p.in_place, p.left), (3, 1, 0), "{p:?}");
    assert!(p.preview.starts_with("p-"));
    let rainbows = p.moves.iter().find(|m| m.from == l.p("radiohead - in rainbows")).unwrap();
    assert_eq!(rainbows.to, l.p(r"Radiohead\In Rainbows (2007)"));
    assert_eq!((rainbows.files, rainbows.words.as_str()), (2, "Radiohead \\ In Rainbows (2007)"));
    assert_eq!(rainbows.sidecars, vec!["cover.jpg", "In Rainbows.cue", r"scans\back.png"]);
    assert_eq!(l.to_of(&p, r"Thom\Eraser"), Some(l.p(r"Thom Yorke\The Eraser (2006)")));

    let res = l.run(&p).unwrap();
    assert_eq!(res.moved.len(), 3);
    assert!(res.left_alone.is_empty() && !res.stopped);
    assert_eq!(std::fs::read_to_string(l.p(r"Radiohead\In Rainbows (2007)\scans\back.png")).unwrap(), "scan");
    assert!(l.p(r"Radiohead\In Rainbows (2007)\07 Reckoner.flac").is_file() && l.p(r"Radiohead\OK Computer (1997)\01 Airbag.mp3").is_file());
    assert!(l.p(r"Thom Yorke\The Eraser (2006)\01.mp3").is_file());
    // the folders left empty are gone; the music folder and the backups stay
    assert!(!l.p("radiohead - in rainbows").exists() && !l.p("Thom").exists() && l.lib.is_dir());
    // one batch, kind organize, three albums; each move written before it and done after it
    let id = res.batch.clone().unwrap();
    let b = crate::undo::batches(&l.root());
    assert_eq!((b.len(), b[0].kind.as_str(), b[0].changes, b[0].title.as_str()), (1, "organize", 3, TITLE));
    let log = log_of(&l, &id);
    let kinds: Vec<&str> = log.iter().map(|e| e["Kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "moved").count(), 3);
    assert_eq!(kinds.iter().filter(|k| **k == "done").count(), 3);
    assert_eq!(kinds.iter().filter(|k| **k == "madedir").count(), 1, "{kinds:?}"); // Thom Yorke
    for (i, k) in kinds.iter().enumerate() {
        if *k == "done" {
            assert_eq!(kinds[i - 1], "moved");
            assert_eq!(log[i]["Step"], log[i - 1]["Step"]);
        }
    }
    // what library.moved and backup-moves.jsonl are given
    let j = moves_json(&res.moved);
    assert_eq!(j.len(), 3);
    assert_eq!(j[0]["folder"], true);
    assert!(j.iter().any(|m| m["from"] == l.p("radiohead - in rainbows").to_string_lossy().as_ref() && m["to"] == l.p(r"Radiohead\In Rainbows (2007)").to_string_lossy().as_ref()));
    // a second preview finds everything in place
    let again = l.preview();
    assert_eq!((again.to_move, again.in_place), (0, 4));
}

#[test]
fn organize_preview_touches_nothing() {
    let l = Lab::new("look");
    l.track(r"a - b\01.mp3", "A", "B", "2001");
    l.track(r"C\D\01.mp3", "C", "D", "");
    l.track(r"C\loose.mp3", "C", "D", "");
    l.file(r"C\D\cover.jpg", "x");
    l.track(r"E\F\01.mp3", "E", "F", "1999");
    let before = full_listing(&l.base);
    let p = l.preview();
    assert_eq!((p.to_move, p.in_place), (2, 1)); // C \ D is already where it belongs
    assert_eq!(full_listing(&l.base), before);
    assert!(!l.root().exists());
}

#[test]
fn organize_multi_disc_albums_move_as_one() {
    let l = Lab::new("discs");
    l.track(r"band - album\CD1\01.mp3", "Band", "Album", "1990");
    l.track(r"band - album\CD2\01.mp3", "Band", "Album", "1990");
    l.file(r"band - album\CD2\rip.log", "log");
    l.track(r"Band2\Big Album\Disc 1\01.mp3", "Band2", "Big Album (Disc 1)", "2001");
    l.track(r"Band2\Big Album\Disc 2\01.mp3", "Band2", "Big Album (Disc 2)", "2001");
    let p = l.preview();
    assert_eq!(p.to_move, 2, "{p:?}");
    let m = p.moves.iter().find(|m| m.from == l.p("band - album")).unwrap();
    assert_eq!((m.to.clone(), m.files, m.sidecars.clone()), (l.p(r"Band\Album (1990)"), 2, vec![r"CD2\rip.log".to_string()]));
    assert_eq!(l.to_of(&p, r"Band2\Big Album"), Some(l.p(r"Band2\Big Album (2001)")));
    l.run(&p).unwrap();
    assert!(l.p(r"Band\Album (1990)\CD2\01.mp3").is_file() && l.p(r"Band2\Big Album (2001)\Disc 2\01.mp3").is_file());
}

#[test]
fn organize_leaves_alone_what_it_cannot_place_and_says_why() {
    let l = Lab::new("left");
    l.track_tags(r"Various\Comp\01.mp3", serde_json::json!({"album_artist": "Various Artists", "artist": "A", "album": "Comp", "date": "2000"}));
    l.track_tags(r"Various\Comp\02.mp3", serde_json::json!({"album_artist": "Various Artists", "artist": "B", "album": "Comp", "date": "2000"}));
    l.track_tags(r"Flagged\Hits\01.mp3", serde_json::json!({"artist": "A", "album": "Hits", "compilation": true}));
    l.track(r"Mixed\Split\01.mp3", "A", "Split", "");
    l.track(r"Mixed\Split\02.mp3", "B", "Split", "");
    l.track(r"NoTags\Album\01.mp3", "NoTags", "", "");
    l.file(r"Broken\Album\01.mp3", "not tags at all");
    l.track(r"Loose\Album\01.mp3", "Loose", "Album", "2000");
    l.track(r"Loose\stray.mp3", "Loose", "Single", "");
    l.track(r"Removed\Album\01.mp3", "Removed", "Album", "2000");
    l.track(r"Two\Both\01.mp3", "Two", "One", "");
    l.track(r"Two\Both\02.mp3", "Two", "Other", "");
    // an album with another release kept inside it
    l.track(r"Feels\Feels (2005)\01.mp3", "Feels", "Feels", "2005");
    l.track(r"Feels\Feels (2005)\Live Bonus Disc\01.mp3", "Feels", "Live Bonus Disc", "2005");
    l.track("stray at the top.mp3", "X", "Y", "");
    let p = l.preview_with(&PreviewOptions { hidden: vec![l.p(r"Removed\Album\01.mp3")], ..Default::default() });
    assert_eq!(l.why(&p, r"Mixed\Split"), "mixed_artists");
    assert_eq!(l.why(&p, r"NoTags\Album"), "missing_tags");
    assert_eq!(l.why(&p, r"Broken\Album"), "missing_tags");
    assert_eq!(l.why(&p, "Loose"), "loose_tracks");
    assert_eq!(l.why(&p, ""), "loose_tracks"); // the music folder itself
    assert_eq!(l.why(&p, r"Removed\Album"), "removed");
    assert_eq!(l.why(&p, r"Two\Both"), "two_albums_one_folder");
    assert_eq!(l.why(&p, r"Feels\Feels (2005)"), "two_albums_one_folder");
    assert_eq!(l.why(&p, r"Feels\Feels (2005)\Live Bonus Disc"), "two_albums_one_folder");
    // only the loose artist's album folder moves, and the two marked compilations (into their own folder)
    assert_eq!(p.moves.iter().map(|m| m.from.clone()).collect::<Vec<_>>(), vec![l.p(r"Flagged\Hits"), l.p(r"Loose\Album"), l.p(r"Various\Comp")]);
    assert!(p.left_alone.iter().all(|x| !x.words.is_empty() && !x.words.contains('\u{2014}')));
    assert_eq!(p.left, p.left_alone.len());
    l.run(&p).unwrap();
    assert!(l.p(r"Loose\stray.mp3").is_file() && l.p(r"Loose\Album (2000)\01.mp3").is_file() && l.p(r"Various Artists\Comp (2000)\02.mp3").is_file() && l.p(r"Various Artists\Hits\01.mp3").is_file());
    assert!(l.p(r"Mixed\Split\02.mp3").is_file());
}

#[test]
fn organize_never_lands_two_albums_on_one_place_or_on_something_there() {
    let l = Lab::new("collide");
    l.track(r"copy one\01.mp3", "A", "Album", "2000");
    l.track(r"copy two\01.mp3", "A", "Album", "2000");
    l.track(r"B\old name\01.mp3", "B", "Album", "2000");
    l.file(r"B\Album (2000)\notes.txt", "not an album, but there");
    // the place is another album's folder: never moved into
    l.track(r"C\01.mp3", "Someone", "C", "");
    l.track(r"elsewhere\01.mp3", "C", "Inside", "");
    let p = l.preview();
    assert_eq!(l.why(&p, "copy one"), "collision");
    assert_eq!(l.why(&p, "copy two"), "collision");
    assert_eq!(l.why(&p, r"B\old name"), "target_exists");
    assert_eq!(l.why(&p, "elsewhere"), "target_exists");
    let before = files_listing(&l.lib);
    let res = l.run(&p).unwrap();
    // only "C" itself moves (to Someone \ C); nothing else
    assert_eq!(res.moved, vec![(l.p("C"), l.p(r"Someone\C"))]);
    assert_eq!(files_listing(&l.lib).len(), before.len());
    assert_eq!(std::fs::read_to_string(l.p(r"B\Album (2000)\notes.txt")).unwrap(), "not an album, but there");
}

#[test]
fn organize_cleans_names_for_windows_and_makes_those_folders() {
    let l = Lab::new("names");
    l.track(r"x\01.mp3", "AC/DC", "Back: Black...", "1980");
    l.track(r"y\01.mp3", "CON", "aux", "");
    l.track(r"z\01.mp3", "Trailing. ", "Dots. . .", "");
    let p = l.preview();
    assert_eq!(l.to_of(&p, "x"), Some(l.p(r"AC_DC\Back - Black (1980)")));
    assert_eq!(l.to_of(&p, "y"), Some(l.p(r"_CON\_aux")));
    assert_eq!(l.to_of(&p, "z"), Some(l.p(r"Trailing\Dots")));
    let res = l.run(&p).unwrap();
    assert_eq!(res.moved.len(), 3);
    for m in &p.moves {
        assert!(exists_exact(&m.to) && m.to.join("01.mp3").is_file(), "{}", m.to.display());
    }
}

#[test]
fn organize_a_folder_that_differs_only_in_case_punctuation_or_spacing_is_already_in_place() {
    let l = Lab::new("case");
    l.track(r"Radiohead\in rainbows (2007)\01.mp3", "Radiohead", "In Rainbows", "2007");
    l.track(r"100 gecs\1000 gecs (2019)\01.mp3", "100 gecs", "1000 Gecs", "2019");
    l.track(r"2 Chainz\B.O.A.T.S. II Me Time (2013)\01.mp3", "2 Chainz", "B.O.A.T.S. II: Me Time", "2013");
    l.track(r"Arcade Fire\Funeral  [Japanese Special Limited Edition] (2004)\01.mp3", "Arcade Fire", "Funeral", "2004");
    l.track(r"1000 Gecs\Tree Of Clues (2020)\01.mp3", "100 gecs", "1000 Gecs and the Tree of Clues", "2020");
    let p = l.preview();
    assert_eq!((p.in_place, p.to_move), (4, 1), "{:?}", p.moves);
    assert_eq!(l.to_of(&p, r"1000 Gecs\Tree Of Clues (2020)"), Some(l.placed(r"100 gecs", r"1000 Gecs and the Tree of Clues (2020)")));
}

#[test]
fn organize_moves_an_album_into_a_folder_named_like_its_own_and_back() {
    let l = Lab::new("inside");
    // an album lying at the top, in a folder named after its artist
    l.track(r"Muse\01.mp3", "Muse", "Absolution", "2003");
    l.track(r"Muse\02.mp3", "Muse", "Absolution", "2003");
    l.track(r"muse - origin\01.mp3", "Muse", "Origin", "2001");
    let p = l.preview();
    assert_eq!(l.to_of(&p, "Muse"), Some(l.p(r"Muse\Absolution (2003)")));
    // the other album would go into a folder that is an album: it waits for another run
    assert_eq!(l.why(&p, "muse - origin"), "target_exists");
    let before = files_listing(&l.lib);
    let res = l.run(&p).unwrap();
    assert_eq!(res.moved.len(), 1);
    assert!(l.p(r"Muse\Absolution (2003)\02.mp3").is_file());
    assert!(std::fs::read_dir(&l.lib).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".organizing-")));
    // the next run places the other one
    let res2 = l.run(&l.preview()).unwrap();
    assert_eq!(res2.moved, vec![(l.p("muse - origin"), l.p(r"Muse\Origin (2001)"))]);
    // both undone, newest first
    crate::repair::undo(&l.root(), res2.batch.as_ref().unwrap()).unwrap();
    assert_eq!(crate::repair::undo(&l.root(), res.batch.as_ref().unwrap()).unwrap(), Vec::<String>::new());
    assert_eq!(files_listing(&l.lib), before);
}

#[test]
fn organize_long_names_are_shortened_and_too_long_paths_stay() {
    let l = Lab::new("long");
    let title = "A Very Long Album Title That Goes On And On Because Some Records Are Named Like This One Here, Truly";
    l.track(r"x\01.mp3", "Artist", title, "2000");
    let p = l.preview();
    let to = l.to_of(&p, "x").unwrap();
    let name = leaf(&to);
    assert!(name.ends_with('~') && name.chars().count() <= 120 && name.chars().count() < title.len(), "{name}");
    // a track whose path is near the limit where it is, and over it where it would go
    let room = MAX_PATH - utf16_len(&l.p(r"y\")) - 2;
    let track = format!("{}.mp3", "t".repeat(room - 4));
    l.track(&format!(r"y\{track}"), "Someone With A Much Longer Name", "And A Longer Album Name", "2001");
    assert!(utf16_len(&l.p(&format!(r"y\{track}"))) <= MAX_PATH);
    let p = l.preview();
    assert_eq!(l.why(&p, "y"), "too_long");
    l.run(&p).unwrap();
    assert!(l.p(&format!(r"y\{track}")).is_file());
}

#[test]
fn organize_leaves_read_only_files_and_never_clears_the_flag() {
    let l = Lab::new("readonly");
    l.track(r"x\01.mp3", "A", "B", "");
    l.file(r"x\cover.jpg", "c");
    let f = l.p(r"x\cover.jpg");
    let mut perm = std::fs::metadata(&f).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&f, perm).unwrap();
    let p = l.preview();
    assert_eq!(l.why(&p, "x"), "read_only");
    assert_eq!(p.left_alone[0].words, "Read-only: Cadence leaves read-only files alone.");
    l.run(&p).unwrap();
    assert!(f.is_file() && std::fs::metadata(&f).unwrap().permissions().readonly());
}

#[test]
fn organize_leaves_open_files_alone_at_the_preview_and_at_the_run() {
    use std::os::windows::fs::OpenOptionsExt;
    let l = Lab::new("open");
    l.track(r"x\01.mp3", "A", "X", "");
    l.track(r"y\01.mp3", "A", "Y", "");
    let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(l.p(r"x\01.mp3")).unwrap();
    let p = l.preview();
    assert_eq!(l.why(&p, "x"), "open");
    drop(held);
    // the file the player has loaded (open the way a player opens it, others may read) is not "open in
    // another program"
    let player = std::fs::OpenOptions::new().read(true).share_mode(1).open(l.p(r"x\01.mp3")).unwrap();
    assert_eq!(l.why(&l.preview(), "x"), "open");
    let p2 = l.preview_with(&PreviewOptions { playing: Some(l.p(r"x\01.mp3")), ..Default::default() });
    assert!(l.to_of(&p2, "x").is_some());
    drop(player);
    // opened after the preview: left at the run
    let p = l.preview();
    assert_eq!(p.to_move, 2);
    let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(l.p(r"y\01.mp3")).unwrap();
    let res = l.run(&p).unwrap();
    assert_eq!(res.moved, vec![(l.p("x"), l.p(r"A\X"))]);
    assert_eq!((res.left_alone[0].folder.clone(), res.left_alone[0].why.as_str()), (l.p("y"), "open"));
    drop(held);
    assert!(l.p(r"y\01.mp3").is_file());
}

#[test]
fn organize_leaves_an_album_that_changed_after_the_preview() {
    let l = Lab::new("changed");
    l.track(r"x\01.mp3", "A", "X", "");
    l.track(r"y\01.mp3", "A", "Y", "");
    l.track(r"z\01.mp3", "A", "Z", "");
    let p = l.preview();
    l.track(r"x\01.mp3", "A", "X Edited", ""); // its tags were edited
    l.file(r"y\new.txt", "added"); // a file added
    let res = l.run(&p).unwrap();
    assert_eq!(res.moved, vec![(l.p("z"), l.p(r"A\Z"))]);
    let whys: Vec<(PathBuf, String)> = res.left_alone.iter().map(|x| (x.folder.clone(), x.why.clone())).collect();
    assert_eq!(whys, vec![(l.p("x"), "changed".to_string()), (l.p("y"), "changed".to_string())]);
}

#[test]
fn organize_stop_finishes_the_album_in_hand_and_keeps_one_batch() {
    let l = Lab::new("stop");
    for a in ["a", "b", "c", "d"] {
        l.track(&format!(r"{a}\01.mp3"), "Band", &a.to_uppercase(), "");
    }
    let p = l.preview();
    let seen = std::cell::Cell::new(0);
    let res = run(&l.lib, &p, &Hooks { settled: &|_, _, _| {}, cancel: &|| seen.get() >= 2, still_there: &|| true, is_playing: &|_| false, progress: &|_, _, _| seen.set(seen.get() + 1) }).unwrap();
    assert!(res.stopped);
    assert_eq!(res.moved.len(), 2);
    assert_eq!(crate::undo::batches(&l.root())[0].changes, 2);
    // every album wholly at one place
    for m in &p.moves {
        let at_to = res.moved.iter().any(|x| x.0 == m.from);
        assert_eq!((m.to.join("01.mp3").is_file(), m.from.join("01.mp3").is_file()), (at_to, !at_to));
    }
}

#[test]
fn organize_the_music_folder_going_away_ends_with_gone_and_nothing_half_moved() {
    let l = Lab::new("gone");
    for a in ["a", "b", "c"] {
        l.track(&format!(r"{a}\01.mp3"), "Band", &a.to_uppercase(), "");
        l.file(&format!(r"{a}\cover.jpg"), "c");
    }
    let p = l.preview();
    let away = l.base.join("Music unplugged");
    let calls = std::cell::Cell::new(0);
    let progress = |_: usize, _: usize, _: &str| {
        calls.set(calls.get() + 1);
        if calls.get() == 2 {
            std::fs::rename(&l.lib, &away).unwrap(); // the drive is pulled out before the second album
        }
    };
    let err = run(&l.lib, &p, &Hooks { settled: &|_, _, _| {}, cancel: &|| false, still_there: &|| l.lib.is_dir(), is_playing: &|_| false, progress: &progress }).unwrap_err();
    let RunError::Gone { words, done } = err else { panic!("not gone: {err:?}") };
    assert_eq!(words, GONE_WORDS);
    assert_eq!(done.moved.len(), 1);
    let batch = done.batch.clone().unwrap();
    // in the folder as it now is: the first album wholly moved, the others wholly where they were
    let moved_rel = done.moved[0].1.strip_prefix(&l.lib).unwrap().to_path_buf();
    assert!(away.join(&moved_rel).join("01.mp3").is_file() && away.join(&moved_rel).join("cover.jpg").is_file());
    for m in p.moves.iter().filter(|m| m.from != done.moved[0].0) {
        let rel = m.from.strip_prefix(&l.lib).unwrap();
        assert!(away.join(rel).join("01.mp3").is_file() && away.join(rel).join("cover.jpg").is_file());
        assert!(!away.join(m.to.strip_prefix(&l.lib).unwrap()).exists());
    }
    // the drive comes back: what was done is in Repair history and undoes
    std::fs::rename(&away, &l.lib).unwrap();
    assert_eq!(crate::undo::batches(&l.root())[0].changes, 1);
    assert_eq!(crate::repair::undo(&l.root(), &batch).unwrap(), Vec::<String>::new());
    assert!(p.moves.iter().all(|m| m.from.join("01.mp3").is_file()));
}

#[test]
fn organize_does_the_playing_album_last_and_leaves_it_if_still_playing() {
    let l = Lab::new("playing");
    for a in ["a", "b", "c"] {
        l.track(&format!(r"{a}\01.mp3"), "Band", &a.to_uppercase(), "");
    }
    let p = l.preview();
    // "a" is playing at first and stops before its turn: it moves, last
    let asked = std::cell::Cell::new(0);
    let playing = |d: &Path| {
        asked.set(asked.get() + 1);
        d == l.p("a") && asked.get() <= 3
    };
    let res = run(&l.lib, &p, &Hooks { settled: &|_, _, _| {}, cancel: &|| false, still_there: &|| true, is_playing: &playing, progress: &|_, _, _| {} }).unwrap();
    assert_eq!(res.moved.last().unwrap().0, l.p("a"));
    assert_eq!(res.moved.len(), 3);
    // still playing at its turn: left, with the reason
    let l = Lab::new("playing2");
    l.track(r"a\01.mp3", "Band", "A", "");
    l.track(r"b\01.mp3", "Band", "B", "");
    let p = l.preview();
    let res = run(&l.lib, &p, &Hooks { settled: &|_, _, _| {}, cancel: &|| false, still_there: &|| true, is_playing: &|d| d == l.p("a"), progress: &|_, _, _| {} }).unwrap();
    assert_eq!(res.moved, vec![(l.p("b"), l.p(r"Band\B"))]);
    assert_eq!((res.left_alone[0].folder.clone(), res.left_alone[0].why.as_str()), (l.p("a"), "playing"));
}

#[test]
fn organize_a_rename_windows_refuses_leaves_the_album_and_no_record() {
    let l = Lab::new("refused");
    l.track(r"x\01.mp3", "New Artist", "X", "");
    let p = l.preview();
    fault::FAIL_RENAMES.with(|c| c.set(1));
    let res = l.run(&p).unwrap();
    assert!(res.moved.is_empty() && res.batch.is_none());
    assert_eq!(res.left_alone[0].why, "open");
    assert!(l.p(r"x\01.mp3").is_file() && !l.p("New Artist").exists()); // the folder made for it is gone again
    assert!(crate::undo::batches(&l.root()).is_empty());
}

// ------------------------------------------------------------------ crashes

#[test]
fn organize_a_crash_before_the_rename_is_settled_as_not_done() {
    let l = Lab::new("crash1");
    l.track(r"x\01.mp3", "New Artist", "X", "");
    let p = l.preview();
    fault::CRASH_BEFORE_RENAME.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    let dir = only_batch(&l);
    let log = crate::undo::entries(&dir);
    assert_eq!(log.last().unwrap()["Kind"], "moved"); // on the disk before the rename that never came
    assert!(l.p(r"x\01.mp3").is_file() && l.p("New Artist").is_dir());
    assert_eq!(status(&l.root()), None);
    let settled = recover_all(&[l.root()]);
    assert_eq!(settled.len(), 1);
    assert_eq!((settled[0].moved, settled[0].left), (0, 1));
    assert_eq!(crate::undo::entries(&dir).last().unwrap()["Kind"], "notdone");
    assert!(l.p(r"x\01.mp3").is_file() && !l.p("New Artist").exists()); // the folder made for it went
    assert_eq!(status(&l.root()), Some(settled[0].clone()));
    acknowledge(&l.root(), &settled[0].batch);
    assert_eq!(status(&l.root()), None);
    // settled once: nothing more to do, and the album can be organized again
    assert!(recover_all(&[l.root()]).is_empty());
    let res = l.run(&l.preview()).unwrap();
    assert_eq!(res.moved.len(), 1);
}

#[test]
fn organize_a_crash_after_the_rename_is_settled_as_done_and_undoes() {
    let l = Lab::new("crash2");
    l.track(r"x\01.mp3", "New Artist", "X", "");
    l.track(r"y\01.mp3", "New Artist", "Y", "");
    let p = l.preview();
    let before = files_listing(&l.lib);
    fault::CRASH_BEFORE_DONE.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    let dir = only_batch(&l);
    assert!(l.p(r"New Artist\X\01.mp3").is_file() && l.p(r"y\01.mp3").is_file());
    // a run settles it first, then goes on with what is left (the moved album is no longer where the preview saw it)
    let res = l.run(&p).unwrap();
    assert_eq!(res.moved, vec![(l.p("y"), l.p(r"New Artist\Y"))]);
    assert_eq!(res.left_alone[0].why, "changed");
    let first = crate::undo::entries(&dir);
    assert_eq!(first.iter().filter(|e| e["Kind"] == "done").count(), 1);
    assert_eq!(status(&l.root()).map(|s| (s.moved, s.left)), Some((1, 0)));
    // both batches undo; everything is back
    crate::repair::undo(&l.root(), res.batch.as_ref().unwrap()).unwrap();
    assert_eq!(crate::repair::undo(&l.root(), &leaf(&dir)).unwrap(), Vec::<String>::new());
    assert_eq!(files_listing(&l.lib), before);
    assert!(!l.p("New Artist").exists());
}

#[test]
fn organize_a_crash_before_the_last_rename_of_a_hop_puts_the_album_back() {
    // an album lying at the top in a folder named like its artist goes into a folder of that name: through
    // a temporary name, and the folder at its old name is made for it
    let l = Lab::new("crash3");
    l.track(r"A\01.mp3", "A", "X", "");
    let p = l.preview();
    assert_eq!(l.to_of(&p, "A"), Some(l.p(r"A\X")));
    fault::CRASH_BEFORE_RENAME.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    // caught at its temporary name, with the folder made for it at its old name
    assert!(l.p("A").is_dir() && !l.p(r"A\01.mp3").exists());
    let settled = recover_all(&[l.root()]);
    assert_eq!((settled[0].left, settled[0].stuck.len()), (1, 0));
    assert!(l.p(r"A\01.mp3").is_file());
    assert_eq!(std::fs::read_dir(&l.lib).unwrap().flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).count(), 1);
}

#[test]
fn organize_a_crash_in_the_middle_of_the_temporary_hop_renames_the_album_back() {
    let l = Lab::new("crash-hop");
    l.track(r"A\01.mp3", "A", "X", "");
    let p = l.preview();
    fault::CRASH_MID_HOP.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    // only the hop's first line is written, and the album sits at its temporary name
    let dir = only_batch(&l);
    let log = crate::undo::entries(&dir);
    assert_eq!(log.iter().filter(|e| e["Kind"] == "moved").count(), 1, "{log:?}");
    let tmp = PathBuf::from(log[0]["To"].as_str().unwrap());
    assert!(leaf(&tmp).contains(".organizing-") && tmp.is_dir() && !l.p("A").exists());
    let settled = recover_all(&[l.root()]);
    assert_eq!((settled[0].moved, settled[0].left, settled[0].stuck.len(), settled[0].newly_done.len()), (0, 1, 0, 0), "not done at its temporary name");
    assert!(l.p(r"A\01.mp3").is_file() && !tmp.exists());
    assert_eq!(crate::undo::entries(&dir).last().unwrap()["Kind"], "notdone");
}

#[test]
fn organize_an_album_that_cannot_go_back_from_its_temporary_name_stays_open_and_is_said() {
    let l = Lab::new("crash-hop-stuck");
    l.track(r"A\01.mp3", "A", "X", "");
    let p = l.preview();
    fault::CRASH_MID_HOP.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    let dir = only_batch(&l);
    let tmp = PathBuf::from(crate::undo::entries(&dir)[0]["To"].as_str().unwrap());
    // something new took its old name meanwhile: it cannot go back
    std::fs::create_dir_all(l.p("A")).unwrap();
    let settled = recover_all(&[l.root()]);
    assert_eq!(settled[0].stuck, vec![tmp.clone()]);
    assert!(tmp.is_dir());
    assert!(!crate::undo::entries(&dir).iter().any(|e| e["Kind"] == "notdone" || e["Kind"] == "done"), "the step stays open");
    // once the way is clear, the next settling puts it back
    std::fs::remove_dir(l.p("A")).unwrap();
    let again = recover_all(&[l.root()]);
    assert_eq!((again[0].left, again[0].stuck.len()), (1, 0));
    assert!(l.p(r"A\01.mp3").is_file() && !tmp.exists());
}

#[test]
fn organize_a_torn_batch_record_is_written_again_from_its_log_and_listed() {
    let l = Lab::new("torn");
    l.track(r"x\01.mp3", "New Artist", "X", "");
    l.track(r"y\01.mp3", "New Artist", "Y", "");
    let p = l.preview();
    fault::CRASH_BEFORE_DONE.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    let dir = only_batch(&l);
    // a power cut tore the record
    std::fs::write(dir.join("batch.json"), "{\"Id\": \"2026").unwrap();
    assert!(crate::undo::batches(&l.root()).is_empty());
    let settled = recover_all(&[l.root()]);
    assert_eq!((settled.len(), settled[0].moved), (1, 1));
    let listed = crate::undo::batches(&l.root());
    assert_eq!((listed.len(), listed[0].kind.as_str(), listed[0].changes), (1, KIND, 1));
    // and its Undo works
    assert_eq!(crate::repair::undo(&l.root(), &leaf(&dir)).unwrap(), Vec::<String>::new());
    assert!(l.p(r"x\01.mp3").is_file());
}

#[test]
fn organize_a_batch_whose_drive_is_away_is_settled_later() {
    let l = Lab::new("crash4");
    l.track(r"x\01.mp3", "N", "X", "");
    let p = l.preview();
    fault::CRASH_BEFORE_RENAME.with(|c| c.set(true));
    assert!(l.run(&p).is_err());
    let away = l.base.join("away");
    std::fs::rename(&l.lib, &away).unwrap();
    assert!(recover_all(&[l.root()]).is_empty());
    std::fs::rename(&away, &l.lib).unwrap();
    assert_eq!(recover_all(&[l.root()]).len(), 1);
}

// ------------------------------------------------------------------ undo

#[test]
fn organize_undo_puts_every_folder_back_and_removes_the_folders_it_made() {
    let l = Lab::new("undo");
    l.track(r"radiohead - in rainbows\01.mp3", "Radiohead", "In Rainbows", "2007");
    l.file(r"radiohead - in rainbows\cover.jpg", "c");
    l.track(r"Old\Shelf\Album\CD1\01.mp3", "Brand New", "Album", "2010");
    l.track(r"Old\Shelf\Album\CD2\01.mp3", "Brand New", "Album", "2010");
    l.track(r"Another\One\01.mp3", "Brand New", "One", "");
    let before = files_listing(&l.lib);
    let res = l.run(&l.preview()).unwrap();
    assert_eq!(res.moved.len(), 3);
    assert!(!l.p("Old").exists() && !l.p("Another").exists());
    let id = res.batch.unwrap();
    assert_eq!(crate::repair::undo(&l.root(), &id).unwrap(), Vec::<String>::new());
    assert_eq!(files_listing(&l.lib), before);
    assert!(!l.p("Radiohead").exists() && !l.p("Brand New").exists());
    assert!(crate::undo::batches(&l.root())[0].undone);
}

#[test]
fn organize_undo_leaves_an_album_whose_old_place_is_taken_and_says_so() {
    let l = Lab::new("undotaken");
    l.track(r"x\01.mp3", "A", "X", "");
    l.track(r"y\01.mp3", "A", "Y", "");
    let res = l.run(&l.preview()).unwrap();
    l.file(r"x\something new.txt", "new"); // the old place is taken
    let left = crate::repair::undo(&l.root(), res.batch.as_ref().unwrap()).unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].contains(&l.p("x").display().to_string()), "{left:?}");
    assert!(l.p(r"A\X\01.mp3").is_file() && l.p(r"y\01.mp3").is_file() && l.p(r"x\something new.txt").is_file());
    assert!(!crate::undo::batches(&l.root())[0].undone);
}

#[test]
fn organize_reads_genre_folders_as_holding_artists() {
    let l = Lab::new("genre");
    l.track(r"Dubstep\Skream\Skream! (2006)\01.mp3", "Skream", "Skream!", "2006");
    l.track(r"Dubstep\Skream\Outside\01.mp3", "Skream", "Outside", "2010");
    l.track(r"skream - live\01.mp3", "Skream", "Live", "2012");
    let p = l.preview_with(&PreviewOptions { skip_folders: "Dubstep, Various Artists".into(), ..Default::default() });
    assert_eq!(p.in_place, 1, "{p:?}");
    // the artist's folder inside the genre folder is where the artist's albums go
    assert_eq!(l.to_of(&p, r"Dubstep\Skream\Outside"), Some(l.p(r"Dubstep\Skream\Outside (2010)")));
    assert_eq!(l.to_of(&p, "skream - live"), Some(l.p(r"Dubstep\Skream\Live (2012)")));
}

// ------------------------------------------------------------------ compilations, and one album at a time

#[test]
fn organize_compilations_go_to_their_folder_and_unmarked_mixes_stay() {
    let l = Lab::new("comps");
    l.track_tags(r"downloads\Now 50\01.mp3", serde_json::json!({"album_artist": "Various Artists", "artist": "A", "album": "Now 50", "date": "2001"}));
    l.track_tags(r"downloads\Now 50\02.mp3", serde_json::json!({"album_artist": "Various Artists", "artist": "B", "album": "Now 50", "date": "2001"}));
    l.track_tags(r"downloads\Mixtape\01.mp3", serde_json::json!({"artist": "A", "album": "Mixtape", "compilation": true}));
    l.track_tags(r"downloads\Mixtape\02.mp3", serde_json::json!({"artist": "C", "album": "Mixtape", "compilation": true}));
    // several artists, not marked: stays
    l.track(r"downloads\Split\01.mp3", "A", "Split", "1999");
    l.track(r"downloads\Split\02.mp3", "B", "Split", "1999");
    let p = l.preview();
    assert_eq!(l.to_of(&p, r"downloads\Now 50"), Some(l.p(r"Various Artists\Now 50 (2001)")));
    assert_eq!(l.to_of(&p, r"downloads\Mixtape"), Some(l.p(r"Various Artists\Mixtape")));
    assert_eq!(l.why(&p, r"downloads\Split"), "mixed_artists");
    l.run(&p).unwrap();
    assert!(l.p(r"Various Artists\Now 50 (2001)\02.mp3").is_file() && l.p(r"downloads\Split\02.mp3").is_file());
    // in place under the default name
    assert_eq!(l.preview().in_place, 2);
    // a renamed compilations folder (cleaned by the Windows rules): the next preview moves them there
    let renamed = PreviewOptions { compilations: "Compilations: Mixed?".into(), ..Default::default() };
    let p = l.preview_with(&renamed);
    assert_eq!((p.to_move, p.in_place), (2, 0), "{p:?}");
    assert_eq!(l.to_of(&p, r"Various Artists\Now 50 (2001)"), Some(l.p(r"Compilations - Mixed_\Now 50 (2001)")));
    l.run(&p).unwrap();
    assert!(l.p(r"Compilations - Mixed_\Mixtape\01.mp3").is_file() && !l.p("Various Artists").exists());
    assert_eq!(l.preview_with(&renamed).in_place, 2);
    // a name that cannot be a folder falls back to the usual one
    assert_eq!((compilations_name(""), compilations_name(" .. "), compilations_name("CON")), ("Various Artists".into(), "Various Artists".into(), "_CON".into()));
}

#[test]
fn organize_can_look_at_one_album_only() {
    let l = Lab::new("only");
    l.track(r"new one\01.mp3", "Band", "New", "2024");
    l.track(r"old one\01.mp3", "Band", "Old", "1990");
    l.track(r"Loose\stray.mp3", "Loose", "x", "");
    l.track(r"Band\Taken (2000)\01.mp3", "Band", "Taken", "2000");
    l.track(r"taken too\01.mp3", "Band", "Taken", "2000");
    let read = std::sync::atomic::AtomicUsize::new(0);
    let counting = |p: &Path| {
        read.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        reader(p)
    };
    let only = |rel: &str| PreviewOptions { only: Some(l.p(rel)), ..Default::default() };
    let p = preview(&l.lib, &only("new one"), &counting, &|_, _| {}).unwrap();
    assert_eq!((p.to_move, p.left, p.in_place), (1, 0, 0), "{p:?}");
    assert_eq!(p.moves[0].to, l.p(r"Band\New (2024)"));
    // only its own tracks are read
    assert_eq!(read.load(std::sync::atomic::Ordering::Relaxed), 1);
    // its reason, when it stays; nothing about the rest of the library
    let p = preview(&l.lib, &only("taken too"), &reader, &|_, _| {}).unwrap();
    assert_eq!((p.to_move, p.left_alone.len(), p.left_alone[0].why.as_str()), (0, 1, "target_exists"));
    let p = preview(&l.lib, &only(r"Band\Taken (2000)"), &reader, &|_, _| {}).unwrap();
    assert_eq!((p.to_move, p.left, p.in_place), (0, 0, 1));
    // a folder that is not an album: nothing at all
    let p = preview(&l.lib, &only("nowhere"), &reader, &|_, _| {}).unwrap();
    assert_eq!((p.to_move, p.left, p.in_place), (0, 0, 0));
}

#[test]
fn organize_one_album_cut_off_by_a_crash_is_settled_both_ways() {
    // (Keep the library organized files one album with a preview of it alone and a run: the same journal)
    for (n, fault_after_rename) in [(1, false), (2, true)] {
        let l = Lab::new(&format!("album-crash{n}"));
        l.track(r"arrived\01.mp3", "New Band", "First", "2024");
        l.track(r"Other\Album\01.mp3", "Other", "Album", "");
        let p = preview(&l.lib, &PreviewOptions { only: Some(l.p("arrived")), ..Default::default() }, &reader, &|_, _| {}).unwrap();
        assert_eq!(p.moves.len(), 1);
        if fault_after_rename {
            fault::CRASH_BEFORE_DONE.with(|c| c.set(true));
        } else {
            fault::CRASH_BEFORE_RENAME.with(|c| c.set(true));
        }
        assert!(l.run(&p).is_err());
        let dir = only_batch(&l);
        assert_eq!(crate::undo::batches(&l.root())[0].kind, "organize");
        // the next engine start
        let settled = recover_all(&[l.root()]);
        assert_eq!(settled.len(), 1);
        let last = crate::undo::entries(&dir).last().unwrap().clone();
        if fault_after_rename {
            // already at its new place: written as done, and Undo puts it back
            assert_eq!((last["Kind"].as_str(), settled[0].moved, settled[0].left), (Some("done"), 1, 0));
            assert!(l.p(r"New Band\First (2024)\01.mp3").is_file() && !l.p("arrived").exists());
            assert_eq!(crate::repair::undo(&l.root(), &leaf(&dir)).unwrap(), Vec::<String>::new());
            assert!(l.p(r"arrived\01.mp3").is_file() && !l.p("New Band").exists());
        } else {
            // still where it was: not done, and the folder made for it is gone
            assert_eq!((last["Kind"].as_str(), settled[0].moved, settled[0].left), (Some("notdone"), 0, 1));
            assert!(l.p(r"arrived\01.mp3").is_file() && !l.p("New Band").exists());
        }
        assert!(l.p(r"Other\Album\01.mp3").is_file());
        assert!(recover_all(&[l.root()]).is_empty());
    }
}

#[test]
fn organize_imports_file_compilations_and_kept_organized_albums_the_same_way() {
    use crate::importer::scan::{folder_artist, home_for};
    let l = Lab::new("importer");
    l.track(r"Band\Studio Albums\Old (1999)\01.mp3", "Band", "Old", "1999");
    l.track(r"Band\Studio Albums\Older (1998)\01.mp3", "Band", "Older", "1998");
    l.track(r"Band\Studio Albums\Oldest (1997)\01.mp3", "Band", "Oldest", "1997");
    let mut ctx = crate::importer::ctx::Ctx::new(l.lib.clone(), l.lib.clone(), true, PathBuf::new(), PathBuf::new());
    // the Music Tool's way (its defaults): compilations by the scan's name, a new album on the shelf, a
    // replacement beside the copy it replaces
    assert_eq!(folder_artist(&ctx, "Various Artists"), "Various Artists");
    assert_eq!(folder_artist(&ctx, "Band"), "Band");
    let old = l.p(r"Band\Studio Albums\Old (1999)");
    assert_eq!(home_for(&ctx, "Band", "New (2024)", 10, None), l.p(r"Band\Studio Albums"));
    assert_eq!(home_for(&ctx, "Band", "Old (1999)", 10, Some(&old)), l.p(r"Band\Studio Albums"));
    // Cadence's engine with Keep the library organized and its own compilations folder
    ctx.keep_organized = true;
    ctx.compilations_folder = "Compilations: Mine".into();
    assert_eq!(folder_artist(&ctx, "Various Artists"), "Compilations - Mine");
    assert_eq!(folder_artist(&ctx, "Band"), "Band");
    assert_eq!(home_for(&ctx, "Band", "New (2024)", 10, None), l.p("Band"));
    assert_eq!(home_for(&ctx, "Band", "Old (1999)", 10, Some(&old)), l.p("Band"));
    assert_eq!(home_for(&ctx, "Nobody Yet", "First", 10, None), l.p("Nobody Yet"));
}

// ------------------------------------------------------------------ the folder's own words, and marks

#[test]
fn organize_carries_the_folders_own_words_over_and_leaves_a_bracketed_year_out() {
    assert_eq!(own_words("[2013] Burials (Best Buy Exclusive)", "Burials"), vec!["(Best Buy Exclusive)".to_string()]);
    assert_eq!(own_words("Funeral  [Japanese Special   Limited Edition] (2004)", "Funeral"), vec!["[Japanese Special Limited Edition]".to_string()]);
    assert_eq!(own_words("Abbey Road (Remastered 2009) [FLAC] [24bit-96kHz] (Disc 1)", "Abbey Road"), vec!["(Remastered 2009)".to_string()]);
    // words the album tag already has are not said twice (case and punctuation aside), nor one group twice
    assert_eq!(own_words("Burials (Best Buy Exclusive)", "Burials [Best Buy exclusive]"), Vec::<String>::new());
    assert_eq!(own_words("X (UK Version) [uk version]", "X"), vec!["(UK Version)".to_string()]);
    assert_eq!(with_own_words("Burials (2013)", "2013", &["(Best Buy Exclusive)".into()]), "Burials (Best Buy Exclusive) (2013)");
    assert_eq!(with_own_words("Live", "", &["[EP]".into()]), "Live [EP]");

    let l = Lab::new("own-words");
    l.track(r"AFI\[2013] Burials (Best Buy Exclusive)\01.mp3", "AFI", "Burials", "2013");
    l.track(r"Arcade Fire - Japan\Funeral  [Japanese Special Limited Edition] (2004)\01.mp3", "Arcade Fire", "Funeral", "2004");
    l.track(r"Oasis\Definitely Maybe (UK Version)\01.mp3", "Oasis", "Definitely Maybe", "1994");
    l.track(r"Beach House\[2008] Devotion [EP]\01.mp3", "Beach House", "Devotion", "2008");
    l.track(r"Green Day\Dookie [2003]\01.mp3", "Green Day", "Dookie", "1994");
    l.track(r"Kanye West\Late Registration (Remastered 2004) [FLAC]\01.mp3", "Kanye West", "Late Registration", "2005");
    let p = l.preview();
    assert_eq!(l.to_of(&p, r"AFI\[2013] Burials (Best Buy Exclusive)"), Some(l.placed(r"AFI", r"Burials (Best Buy Exclusive) (2013)")));
    assert_eq!(l.to_of(&p, r"Arcade Fire - Japan\Funeral  [Japanese Special Limited Edition] (2004)"), Some(l.placed(r"Arcade Fire", r"Funeral [Japanese Special Limited Edition] (2004)")));
    assert_eq!(l.to_of(&p, r"Oasis\Definitely Maybe (UK Version)"), Some(l.placed(r"Oasis", r"Definitely Maybe (UK Version) (1994)")));
    assert_eq!(l.to_of(&p, r"Beach House\[2008] Devotion [EP]"), Some(l.placed(r"Beach House", r"Devotion [EP] (2008)")));
    assert_eq!(l.to_of(&p, r"Green Day\Dookie [2003]"), None, "its own year kept, it differs only in the bracket style: in place");
    assert_eq!(l.to_of(&p, r"Kanye West\Late Registration (Remastered 2004) [FLAC]"), Some(l.placed(r"Kanye West", r"Late Registration (Remastered 2004) (2005)")));
}

#[test]
fn organize_marks_each_move_so_cadence_can_group_them_and_counts_the_marks() {
    let l = Lab::new("marks");
    // the artist folder changes
    l.track(r"Andrew Jackson Jihad\Knife Man (2011)\01.mp3", "AJJ", "Knife Man", "2011");
    // the folder's name and the album tag share no words
    l.track(r"50 Cent\Animal Ambition\01.mp3", "50 Cent", "Don't Worry 'Bout It - Single", "2014");
    // only the way it is written
    l.track(r"Blink-182\[1999] Enema of the State\01.mp3", "Blink-182", "Enema of the State", "1999");
    l.track(r"radiohead - in rainbows\01.mp3", "Radiohead", "In Rainbows", "2007");
    let p = l.preview();
    let marks = |rel: &str| p.moves.iter().find(|m| m.from == l.p(rel)).map(|m| m.changes.clone()).unwrap_or_else(|| panic!("{rel} does not move: {:?}", p.left_alone));
    assert_eq!(marks(r"Andrew Jackson Jihad\Knife Man (2011)"), vec!["artist"]);
    assert_eq!(marks(r"50 Cent\Animal Ambition"), vec!["renamed"]);
    assert_eq!(l.to_of(&p, r"50 Cent\Animal Ambition"), Some(l.placed(r"50 Cent", r"Don't Worry 'Bout It - Single (2014)")));
    assert_eq!(marks(r"Blink-182\[1999] Enema of the State"), vec!["tidy"]);
    assert_eq!(marks("radiohead - in rainbows"), vec!["tidy"], "the artist is in its own folder's name");
    assert_eq!(p.counts, Counts { artist: 1, renamed: 1, tidy: 2 });
}

#[test]
fn organize_keeps_the_year_the_folders_name_gives_and_uses_the_tags_only_without_one() {
    let l = Lab::new("folder-year");
    // a reissue tagged with its reissue year: the folder's 1971 stays, and nothing moves
    l.track(r"Alice Coltrane\Journey in Satchidananda (1971)\01.mp3", "Alice Coltrane", "Journey in Satchidananda", "2009");
    // the year in front, the tags' 2006: the folder's 2010, moved to the end
    l.track(r"Lyrics Born\[2010] Home Style (Recordings)\01.mp3", "Lyrics Born", "Home Style", "2006");
    // no year in the folder: the tags'
    l.track(r"Beach House\Bloom\01.mp3", "Beach House", "Bloom", "2012");
    // a year inside other words is not the folder's year
    l.track(r"Kanye West\Late Registration (Remastered 2004)\01.mp3", "Kanye West", "Late Registration", "2005");
    let p = l.preview();
    assert_eq!((p.in_place, p.to_move), (1, 3), "{:?}", p.moves);
    assert_eq!(l.to_of(&p, r"Lyrics Born\[2010] Home Style (Recordings)"), Some(l.placed("Lyrics Born", "Home Style (Recordings) (2010)")));
    assert_eq!(l.to_of(&p, r"Beach House\Bloom"), Some(l.placed("Beach House", "Bloom (2012)")));
    assert_eq!(l.to_of(&p, r"Kanye West\Late Registration (Remastered 2004)"), Some(l.placed("Kanye West", "Late Registration (Remastered 2004) (2005)")));
    assert!(p.moves.iter().all(|m| !m.changes.contains(&"year".to_string())));
}

#[test]
fn organize_keeps_edition_words_outside_brackets_in_parentheses() {
    assert_eq!(edition_runs("Trouble (2004) DELUXE EDITION"), vec!["Deluxe Edition".to_string()]);
    assert_eq!(edition_runs("X 20th Anniversary Edition 1999"), vec!["20th Anniversary Edition".to_string()]);
    assert_eq!(edition_runs("21st Century Breakdown"), Vec::<String>::new());
    assert_eq!(own_words("Trouble (2004) DELUXE EDITION", "Trouble"), vec!["(Deluxe Edition)".to_string()]);
    // words the album tag already has are not said twice
    assert_eq!(own_words("Trouble Deluxe Edition", "Trouble (Deluxe Edition)"), Vec::<String>::new());

    let l = Lab::new("editions");
    l.track(r"Akon\Trouble (2004) DELUXE EDITION\01.mp3", "Akon", "Trouble", "2004");
    l.track(r"Other\X 20th Anniversary Edition (1999)\01.mp3", "Other", "X", "1999");
    l.track(r"Other\[1998] Y 20TH ANNIVERSARY EDITION\01.mp3", "Other", "Y", "1998");
    // two editions of one album, the same tags: different places
    l.track(r"Kelis\Tasty (2003)\01.mp3", "Kelis", "Tasty", "2003");
    l.track(r"Kelis\Tasty (2003) DELUXE EDITION\01.mp3", "Kelis", "Tasty", "2003");
    // no edition words: as before
    l.track(r"Beach House\Bloom\01.mp3", "Beach House", "Bloom", "2012");
    let p = l.preview();
    assert_eq!(l.to_of(&p, r"Akon\Trouble (2004) DELUXE EDITION"), Some(l.placed("Akon", "Trouble (Deluxe Edition) (2004)")));
    // its words are already in that order: only the parentheses differ, so it is already in place
    assert_eq!(l.to_of(&p, r"Other\X 20th Anniversary Edition (1999)"), None);
    assert_eq!(l.to_of(&p, r"Other\[1998] Y 20TH ANNIVERSARY EDITION"), Some(l.placed("Other", "Y (20th Anniversary Edition) (1998)")));
    assert_eq!(l.to_of(&p, r"Kelis\Tasty (2003)"), None, "in place");
    assert_eq!(l.to_of(&p, r"Kelis\Tasty (2003) DELUXE EDITION"), Some(l.placed("Kelis", "Tasty (Deluxe Edition) (2003)")));
    assert_eq!(l.to_of(&p, r"Beach House\Bloom"), Some(l.placed("Beach House", "Bloom (2012)")));
}

#[test]
fn organize_two_albums_that_would_still_share_a_place_both_stay() {
    let l = Lab::new("editions-collide");
    // a format note is left out and the bracketed year becomes the "(Year)": both aim at A\Album (2004)
    l.track(r"A\[2004] Album\01.mp3", "A", "Album", "2004");
    l.track(r"A\Album 2004 (FLAC)\01.mp3", "A", "Album", "2004");
    let p = l.preview();
    assert_eq!(p.to_move, 0, "{:?}", p.moves);
    assert_eq!((l.why(&p, r"A\[2004] Album"), l.why(&p, r"A\Album 2004 (FLAC)")), ("collision".to_string(), "collision".to_string()));
}

#[test]
fn organize_treats_curly_braces_as_brackets() {
    assert_eq!(own_words("(2013)The 20-20 Experience {Target Deluxe Edition}", "The 20_20 Experience"), vec!["(Target Deluxe Edition)".to_string()]);
    let l = Lab::new("braces");
    l.track(r"Justin Timberlake\(2013)The 20-20 Experience {Target Deluxe Edition}\01.mp3", "Justin Timberlake", "The 20/20 Experience", "2013");
    l.track(r"Justin Timberlake\(2006)FutureSex - LoveSounds {Deluxe Edition}\01.mp3", "Justin Timberlake", "FutureSex/LoveSounds", "2006");
    let p = l.preview();
    assert_eq!(l.to_of(&p, r"Justin Timberlake\(2013)The 20-20 Experience {Target Deluxe Edition}"), Some(l.placed("Justin Timberlake", "The 20_20 Experience (Target Deluxe Edition) (2013)")));
    assert_eq!(l.to_of(&p, r"Justin Timberlake\(2006)FutureSex - LoveSounds {Deluxe Edition}"), Some(l.placed("Justin Timberlake", "FutureSex_LoveSounds (Deluxe Edition) (2006)")));
}
