//! Tests that read made-up downloads the way the app does, in a scratch folder: never the real downloads,
//! library, alias file or log. They need ffmpeg and ffprobe (the app does too).
use super::ctx::Ctx;
use super::import::{changed_since_scan, import_unit, ImportResult};
use super::model::{Action, Unit};
use super::scan::{build_unit, update_unit_action};
use super::tools;
use std::path::{Path, PathBuf};

pub(super) struct Lab {
    pub(super) base: PathBuf,
    pub(super) ctx: Ctx,
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

pub(super) fn lab(name: &str) -> Lab {
    let base = std::env::temp_dir().join(format!("music-recs-flow-{name}-{}-{:x}", std::process::id(), super::scan::rand_id()));
    let (dl, lib) = (base.join("dl"), base.join("lib"));
    std::fs::create_dir_all(&dl).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    let mut ctx = Ctx::new(dl, lib, true, tools::find_tool("ffmpeg").unwrap(), tools::find_tool("ffprobe").unwrap());
    // everything the importer writes goes to the scratch folder
    ctx.alias_file = base.join("aliases.txt");
    ctx.log_file = base.join("log.txt");
    ctx.pending_file = base.join("pending.txt");
    ctx.quality_cache = base.join("quality.txt");
    ctx.repair_root = base.join("undo");
    ctx.replacements = base.join("replacements.json");
    ctx.loose_wants = base.join("loose-albums.json");
    ctx.lyrics.dir = base.join("lyrics");
    // a test's Recycle Bin is the folder "bin" beside its library, so nothing of a test reaches the real one
    ctx.recycle = |p: &Path| {
        let bin = p.ancestors().find(|a| a.join("lib").is_dir() && a.join("dl").is_dir()).map(|a| a.join("bin")).ok_or("no test bin")?;
        std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
        std::fs::rename(p, bin.join(p.file_name().unwrap())).map_err(|e| e.to_string())
    };
    ctx.work_dir = base.join("work");
    std::fs::create_dir_all(&ctx.work_dir).unwrap();
    ctx.aliases.write().unwrap().clear();
    Lab { base, ctx }
}

/// A two-second tagged track.
fn tone(ctx: &Ctx, out: &Path, album: &str, no: u32, title: &str, kbps: u32) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut c = tools::command(&ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("sine=frequency={}:duration=2", 200 + no * 100)]);
    if out.extension().is_some_and(|e| e == "flac") {
        c.args(["-c:a", "flac"]);
    } else if out.extension().is_some_and(|e| e == "wav") {
        c.args(["-c:a", "pcm_s16le"]);
    } else {
        c.args(["-c:a", "libmp3lame", "-b:a", &format!("{kbps}k")]);
    }
    c.args(["-metadata", "artist=Test Artist", "-metadata", &format!("album={album}"), "-metadata", &format!("title={title}"), "-metadata", &format!("track={no}"), "-metadata", "date=2020"]);
    c.arg(out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
}

/// Makes the files look two hours old, so the download does not count as still arriving.
pub(super) fn settle(dir: &Path) {
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    for f in super::util::walk_files(dir) {
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(then).unwrap();
    }
}

fn download(l: &Lab, folder: &str, album: &str, tracks: &[(u32, &str)], kbps: u32) -> PathBuf {
    let dir = l.ctx.source.join(folder);
    for (no, title) in tracks {
        tone(&l.ctx, &dir.join(format!("{no:02} {title}.mp3")), album, *no, title, kbps);
    }
    settle(&dir);
    dir
}

pub(super) fn scan(l: &Lab, dir: &Path) -> Unit {
    build_unit(&l.ctx, dir).unwrap_or_else(|why| panic!("{} was skipped: {why}", dir.display()))
}

#[test]
fn an_edition_is_the_album_you_have_but_a_live_album_is_not() {
    let l = lab("near");
    let have = l.ctx.library.join("Test Artist").join("Rocket (2020)");
    tone(&l.ctx, &have.join("01 One.mp3"), "Rocket", 1, "One", 320);
    tone(&l.ctx, &have.join("02 Two.mp3"), "Rocket", 2, "Two", 320);

    // same album, another edition, no better: already there
    let deluxe = scan(&l, &download(&l, "Test Artist - Rocket (Deluxe Edition)", "Rocket (Deluxe Edition)", &[(1, "One"), (2, "Two")], 320));
    assert_eq!(deluxe.action, Action::Exists);
    assert_eq!(deluxe.existing.as_deref(), Some(have.as_path()));

    // a live album with more tracks: never a Replace of the studio album. Left unticked, and it says which album it is near
    let live = scan(&l, &download(&l, "Test Artist - Rocket (Live)", "Rocket (Live)", &[(1, "One"), (2, "Two"), (3, "Three")], 320));
    assert_eq!(live.action, Action::New);
    assert!(live.existing.is_none() && !live.include, "a near match must be left for a person to decide");
    assert!(live.note.contains("Rocket (2020)"), "{}", live.note);

    // an album with no near match is ticked as usual
    let other = scan(&l, &download(&l, "Test Artist - Comet", "Comet", &[(1, "One")], 320));
    assert!(other.action == Action::New && other.include && other.note.is_empty());
}

#[test]
fn two_discs_that_open_with_the_same_song_name_keep_both_songs() {
    // no disc tags: only the folders say which disc a track is on
    let l = lab("discs");
    let dir = l.ctx.source.join("Test Artist - Twins");
    for disc in ["CD1", "CD2"] {
        tone(&l.ctx, &dir.join(disc).join("01 Intro.mp3"), "Twins", 1, "Intro", 320);
        tone(&l.ctx, &dir.join(disc).join("02 Song.mp3"), "Twins", 2, "Song", 320);
    }
    settle(&dir);
    let u = scan(&l, &dir);
    assert_eq!(u.tracks.len(), 4, "{:?}", u.warnings);
    assert!(!u.warnings.iter().any(|w| w.contains("duplicate")), "{:?}", u.warnings);
    // the same song in two formats is still one song
    let pair = l.ctx.source.join("Test Artist - Pair");
    tone(&l.ctx, &pair.join("FLAC").join("01 One.flac"), "Pair", 1, "One", 0);
    tone(&l.ctx, &pair.join("MP3").join("01 One.mp3"), "Pair", 1, "One", 320);
    settle(&pair);
    assert_eq!(scan(&l, &pair).tracks.len(), 1);
}

#[test]
fn a_file_that_is_not_what_its_name_says_stays_out_of_the_library() {
    let l = lab("names");
    let dir = download(&l, "Test Artist - Comet", "Comet", &[(1, "One"), (2, "Two")], 320);
    // text with a song's name, junk with a cover's name (the largest "cover", so it used to be the one put
    // into every track), a playlist with a song's name, and one real picture
    std::fs::write(dir.join("03 Three.ogg"), "this is not music").unwrap();
    std::fs::write(dir.join("04 Four.flac"), "ffconcat version 1.0\nfile '01 One.mp3'\n").unwrap();
    std::fs::write(dir.join("cover.jpg"), vec![b'x'; 300_000]).unwrap();
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=300x300", "-frames:v", "1"]).arg(dir.join("back.png"));
    assert!(tools::run_limited(&mut c, 60));
    settle(&dir);
    assert!(super::images::is_picture(&dir.join("back.png")) && !super::images::is_picture(&dir.join("cover.jpg")) && !super::images::is_picture(&dir.join("03 Three.ogg")));

    // the list shows the two real tracks, unticked, and says what would be left out
    let mut u = scan(&l, &dir);
    assert_eq!(u.tracks.len(), 2);
    assert!(!u.include);
    assert!(u.warnings.iter().any(|w| w.contains("'03 Three.ogg'") && w.contains("'04 Four.flac'") && w.contains("not audio")), "{:?}", u.warnings);
    assert_eq!(u.folder_img.as_deref(), Some(dir.join("back.png").as_path()));

    // ticked and imported anyway: the music and the real picture come in, the rest does not
    u.include = true;
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    assert_eq!(import_unit(&l.ctx, &mut u, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {}), ImportResult::Ok, "{}", said.borrow());
    let mut names: Vec<String> = super::util::walk_files(&l.ctx.library).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
    names.sort();
    assert_eq!(names, ["01 One.mp3", "02 Two.mp3", "back.png"]);

    // a download with nothing real in it is not listed at all
    let empty = l.ctx.source.join("Nothing real");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join("01 Fake.wma"), "nope").unwrap();
    settle(&empty);
    assert!(build_unit(&l.ctx, &empty).is_err());
}

#[test]
fn tags_cannot_aim_a_replace_at_an_artists_folder_or_a_shelf() {
    let mut l = lab("aim");
    l.ctx.genre_folders = vec!["Dubstep".into()];
    // an artist kept inside a genre folder, with two poor copies; and an artist with a shelf of poor copies
    let in_genre = l.ctx.library.join("Dubstep").join("Test Artist");
    tone(&l.ctx, &in_genre.join("Meteor (2020)").join("01 One.mp3"), "Meteor", 1, "One", 96);
    let shelf = l.ctx.library.join("Other Artist").join("EPs and Singles");
    tone(&l.ctx, &shelf.join("[2019]Rocket").join("01 One.mp3"), "Rocket", 1, "One", 96);
    tone(&l.ctx, &shelf.join("[2020]Comet").join("01 One.mp3"), "Comet", 1, "One", 96);
    let before: Vec<PathBuf> = super::util::walk_files(&l.ctx.library);

    // a stranger's download, better by every number, whose tags name the genre folder as its artist and the
    // artist's folder as its album (no year, so the name is exactly the folder's)
    let mut u = scan(&l, &download(&l, "x", "Whatever", &[(1, "One"), (2, "Two"), (3, "Three")], 320));
    for (artist, folder, target) in [("Dubstep", "Test Artist", &in_genre), ("dubstep", "test artist", &in_genre), ("Other Artist", "EPs and Singles", &shelf)] {
        (u.artist, u.folder, u.include) = (artist.to_string(), folder.to_string(), true);
        update_unit_action(&l.ctx, &mut u);
        assert!(u.existing.is_none() && u.action == Action::New, "{artist} / {folder}: {:?} {}", u.existing, u.note);
        assert!(!super::scan::is_album_of(&l.ctx, artist, target), "{artist} / {folder}");
    }
    // and even told to replace it outright, the importer refuses and nothing in the library moves
    (u.artist, u.folder, u.action, u.existing) = ("Dubstep".to_string(), "Test Artist".to_string(), Action::Replace, Some(in_genre.clone()));
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = import_unit(&l.ctx, &mut u, &|_, _| {}, None, &|| {});
    assert_ne!(r, ImportResult::Ok);
    assert_eq!(super::util::walk_files(&l.ctx.library), before);
    assert!(super::util::walk_files(&l.ctx.repair_root).is_empty());

    // the albums themselves can still be replaced
    assert!(super::scan::is_album_of(&l.ctx, "Test Artist", &in_genre.join("Meteor (2020)")));
    assert!(super::scan::is_album_of(&l.ctx, "Other Artist", &shelf.join("[2019]Rocket")));
    let better = scan(&l, &download(&l, "Test Artist - Meteor", "Meteor", &[(1, "One"), (2, "Two")], 320));
    assert_eq!((better.action, better.existing.as_deref()), (Action::Replace, Some(in_genre.join("Meteor (2020)").as_path())), "{}", better.note);
}

#[test]
fn albums_on_shelves_and_in_genre_folders_are_found_replaced_in_place_and_filed_there() {
    let mut l = lab("shelf");
    l.ctx.genre_folders = vec!["Dubstep".into()];
    let artist = l.ctx.library.join("Test Artist");
    let shelf = artist.join("EPs and Singles");
    tone(&l.ctx, &shelf.join("[2019]Rocket").join("01 One.mp3"), "Rocket", 1, "One", 96);
    tone(&l.ctx, &shelf.join("[2020]Comet").join("01 One.mp3"), "Comet", 1, "One", 96);
    tone(&l.ctx, &artist.join("Studio Albums").join("[2018]Planet").join("01 One.mp3"), "Planet", 1, "One", 320);
    let in_genre = l.ctx.library.join("Dubstep").join("Test Artist").join("Meteor (2020)");
    tone(&l.ctx, &in_genre.join("01 One.mp3"), "Meteor", 1, "One", 320);

    // a much better copy of an EP that sits on the shelf: a Replace, and it will land on the shelf
    let mut better = scan(&l, &download(&l, "Test Artist - Rocket", "Rocket", &[(1, "One"), (2, "Two")], 320));
    assert_eq!(better.action, Action::Replace, "{}", better.note);
    assert_eq!(better.existing.as_deref(), Some(shelf.join("[2019]Rocket").as_path()));
    assert_eq!(better.home.as_deref(), Some(shelf.as_path()));
    // the shelf itself is not an album
    assert!(scan(&l, &download(&l, "Test Artist - EPs and Singles", "EPs and Singles", &[(1, "One"), (2, "Two"), (3, "Three"), (4, "Four")], 320)).existing.is_none());

    // a new EP goes on the shelf for EPs, a new album on the shelf for albums
    let ep = scan(&l, &download(&l, "Test Artist - Dust EP", "Dust EP", &[(1, "One"), (2, "Two"), (3, "Three"), (4, "Four")], 320));
    assert!(ep.action == Action::New && ep.home.as_deref() == Some(shelf.as_path()), "{:?}", ep.home);
    let single = scan(&l, &download(&l, "Test Artist - Spark", "Spark", &[(1, "One")], 320));
    assert_eq!(single.home.as_deref(), Some(shelf.as_path()));
    let album = scan(&l, &download(&l, "Test Artist - Galaxy", "Galaxy", &[(1, "One"), (2, "Two"), (3, "Three"), (4, "Four"), (5, "Five")], 320));
    assert_eq!(album.home.as_deref(), Some(artist.join("Studio Albums").as_path()));

    // an album the artist has inside a genre folder is found there
    let meteor = scan(&l, &download(&l, "Test Artist - Meteor", "Meteor", &[(1, "One")], 320));
    assert_eq!((meteor.action, meteor.existing.as_deref()), (Action::Exists, Some(in_genre.as_path())));

    // the Replace, carried out: the new copy is on the shelf, the old one in the undo backups, nothing beside the shelf
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    let r = import_unit(&l.ctx, &mut better, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Ok, "{}", said.borrow());
    let now_on_shelf = shelf.join(&better.folder);
    assert!(now_on_shelf.join("01 One.mp3").exists() && now_on_shelf.join("02 Two.mp3").exists(), "{}", now_on_shelf.display());
    assert!(!shelf.join("[2019]Rocket").exists() && !artist.join(&better.folder).exists());
    let kept: Vec<PathBuf> = super::util::walk_files(&l.ctx.repair_root).into_iter().filter(|p| p.extension().is_some_and(|e| e == "mp3")).collect();
    assert_eq!(kept.len(), 1, "the old copy is in the undo backups");
    assert!(kept[0].to_string_lossy().contains("[2019]Rocket"));
}

#[test]
fn two_tracks_with_one_name_stop_the_import() {
    let l = lab("clash");
    let dir = l.ctx.source.join("Test Artist - Clash");
    // two different songs (the same song in two formats is handled earlier: the scan keeps the better copy)
    // whose files would both become "01 Song.mp3"
    tone(&l.ctx, &dir.join("01 Song.flac"), "Clash", 1, "Alpha", 0);
    tone(&l.ctx, &dir.join("01 Song.wav"), "Clash", 2, "Beta", 0);
    settle(&dir);
    let mut u = scan(&l, &dir);
    assert_eq!(u.tracks.len(), 2, "both songs are kept by the scan");
    let said = std::cell::RefCell::new(String::new());
    let r = import_unit(&l.ctx, &mut u, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Failed);
    assert!(said.borrow().contains("Two tracks"), "{}", said.borrow());
    // nothing reached the library, and the download is whole
    assert!(!l.ctx.library.join("Test Artist").exists());
    assert!(dir.join("01 Song.wav").exists() && dir.join("01 Song.flac").exists());
}

#[test]
fn a_file_that_arrives_after_the_scan_is_noticed() {
    let l = lab("late");
    let dir = download(&l, "Test Artist - Late", "Late", &[(1, "One"), (2, "Two")], 320);
    let u = scan(&l, &dir);
    assert!(!changed_since_scan(&u));
    std::fs::write(dir.join("03 Three.mp3.part"), b"arriving").unwrap();
    assert!(changed_since_scan(&u), "a new file must keep the download out of the Recycle Bin");
    std::fs::remove_file(dir.join("03 Three.mp3.part")).unwrap();
    assert!(!changed_since_scan(&u));
    // a file that grew since the scan counts too
    std::fs::OpenOptions::new().append(true).open(dir.join("01 One.mp3")).and_then(|mut f| std::io::Write::write_all(&mut f, b"more")).unwrap();
    assert!(changed_since_scan(&u));
}

#[test]
fn editing_the_name_onto_an_album_you_have_is_worked_out_again() {
    let l = lab("edit");
    let have = l.ctx.library.join("Test Artist").join("Rocket (2020)");
    tone(&l.ctx, &have.join("01 One.mp3"), "Rocket", 1, "One", 320);
    let mut u = scan(&l, &download(&l, "Test Artist - Comet", "Comet", &[(1, "One")], 320));
    assert_eq!(u.action, Action::New);
    u.folder = "Rocket (2020)".into();
    update_unit_action(&l.ctx, &mut u);
    assert_eq!(u.action, Action::Exists, "the same quality as the copy in the library: skipped, not added again");
    assert_eq!(u.existing.as_deref(), Some(have.as_path()));
    // and back to a free name
    u.folder = "Comet (2020)".into();
    update_unit_action(&l.ctx, &mut u);
    assert!(u.action == Action::New && u.existing.is_none());
}

#[test]
fn tag_text_cannot_write_extra_lines_into_the_alias_file() {
    let l = lab("alias");
    for bad in ["Evil\nReal Artist = Hijacked", "a = b", "# comment", " #x", "tab\there", ""] {
        l.ctx.save_alias(bad, "Somebody");
        l.ctx.save_alias("Somebody Else", bad);
    }
    assert!(!l.ctx.alias_file.exists(), "nothing unsafe may reach the file: {:?}", std::fs::read_to_string(&l.ctx.alias_file));
    l.ctx.save_alias("Bjork", "Björk");
    assert_eq!(std::fs::read_to_string(&l.ctx.alias_file).unwrap().trim(), "Bjork = Björk");
    assert!(l.ctx.is_alias("bjork"));
}

#[test]
fn a_folder_the_app_fetched_is_read_at_once_and_any_other_waits() {
    let l = lab("handoff");
    let dir = l.ctx.source.join("Test Artist - Fresh");
    tone(&l.ctx, &dir.join("01 One.mp3"), "Fresh", 1, "One", 320);
    // written a moment ago: could still be arriving
    assert_eq!(build_unit(&l.ctx, &dir).err().as_deref(), Some("still downloading"));
    // the Soulseek side says every file of this folder has arrived
    l.ctx.settled.write().unwrap().insert("test artist - fresh".into());
    let u = scan(&l, &dir);
    assert!(u.action == Action::New && u.tracks.len() == 1);
}

#[test]
fn a_download_takes_the_place_of_an_album_health_flagged_only_if_it_is_sound() {
    let l = lab("replace");
    let have = l.ctx.library.join("Test Artist").join("Rocket (2020)");
    tone(&l.ctx, &have.join("01 One.mp3"), "Rocket", 1, "One", 320);
    tone(&l.ctx, &have.join("02 Two.mp3"), "Rocket", 2, "Two", 320);
    std::fs::write(have.join("the old copy.txt"), b"a mark to know the old folder by").unwrap();

    // the same album at the same quality: with nothing asked for, it is skipped as already there
    let dir = download(&l, "Test Artist - Rocket", "Rocket", &[(1, "One"), (2, "Two")], 320);
    assert_eq!(scan(&l, &dir).action, Action::Exists);

    // Health asked for a better copy of that album: now this download is its replacement
    crate::replace::add(&l.ctx.replacements, &have, "Test Artist", "Rocket (2020)", "Damaged or incomplete", &[]).unwrap();
    let mut u = scan(&l, &dir);
    assert_eq!(u.action, Action::Replace, "{}", u.note);
    assert!(u.note.contains("which Health flagged (damaged or incomplete)"), "{}", u.note);

    // a download with damage of its own is not a better copy: unticked, and refused even if pushed through
    let mut bad = u.clone();
    bad.verdict.damaged.push("01 One.mp3 (stops at 0:01 of 0:02)".into());
    update_unit_action(&l.ctx, &mut bad);
    assert!(bad.action == Action::Exists && !bad.include && bad.note.contains("has 1 damaged file itself"), "{}", bad.note);
    bad.action = Action::Replace;
    let said = std::cell::RefCell::new(String::new());
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = import_unit(&l.ctx, &mut bad, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Failed, "{}", said.borrow());
    assert!(said.borrow().contains("Your copy stays as it is"), "{}", said.borrow());
    assert!(have.join("the old copy.txt").exists() && crate::replace::wanted(&l.ctx.replacements, &have).is_some());

    // the sound download: the new album is in place, the old folder is whole in the undo backups, and
    // Health is no longer waiting for it
    let r = import_unit(&l.ctx, &mut u, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Ok, "{}", said.borrow());
    let now = l.ctx.library.join("Test Artist").join(&u.folder);
    assert!(now.join("01 One.mp3").exists() && !now.join("the old copy.txt").exists());
    let kept: Vec<PathBuf> = super::util::walk_files(&l.ctx.repair_root).into_iter().filter(|p| p.file_name().is_some_and(|n| n == "the old copy.txt")).collect();
    assert_eq!(kept.len(), 1, "the old copy is in the undo backups");
    assert!(crate::replace::wanted(&l.ctx.replacements, &have).is_none());
}

#[test]
fn only_the_track_health_asked_for_is_swapped_and_the_rest_of_the_album_stays() {
    let l = lab("onetrack");
    let have = l.ctx.library.join("Test Artist").join("Rocket (2020)");
    for (no, title) in [(1, "One"), (2, "Two"), (3, "Three")] {
        tone(&l.ctx, &have.join(format!("{no:02} {title}.mp3")), "Rocket", no, title, 96);
    }
    let bytes = |name: &str| std::fs::read(have.join(name)).unwrap();
    let before = [bytes("01 One.mp3"), bytes("02 Two.mp3"), bytes("03 Three.mp3")];
    let asked = have.join("02 Two.mp3");
    crate::replace::add(&l.ctx.replacements, &have, "Test Artist", "Rocket (2020)", "Low bitrate", &[asked.to_string_lossy().to_string()]).unwrap();
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    let in_backups = || -> Vec<PathBuf> { super::util::walk_files(&l.ctx.repair_root).into_iter().filter(|p| p.extension().is_some_and(|e| e == "mp3")).collect() };

    // a download that has no track 2: nothing in the library changes, and the album is still waited for
    let mut part = scan(&l, &download(&l, "Test Artist - Rocket (part)", "Rocket", &[(1, "One"), (3, "Three")], 320));
    assert_eq!(part.action, Action::Replace, "{}", part.note);
    let r = import_unit(&l.ctx, &mut part, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Failed, "{}", said.borrow());
    assert!(said.borrow().contains("no track 2") && said.borrow().contains("Nothing in the library was changed"), "{}", said.borrow());
    assert!(before == [bytes("01 One.mp3"), bytes("02 Two.mp3"), bytes("03 Three.mp3")]);
    assert_eq!(std::fs::read_dir(&have).unwrap().count(), 3);
    assert!(in_backups().is_empty() && crate::replace::wanted(&l.ctx.replacements, &have).is_some());

    // the whole album at 320: only track 2 is taken from it
    let mut u = scan(&l, &download(&l, "Test Artist - Rocket", "Rocket", &[(1, "One"), (2, "Two"), (3, "Three")], 320));
    assert_eq!(u.action, Action::Replace, "{}", u.note);
    assert!(u.note.contains("Replaces 1 track of 'Rocket (2020)'") && u.note.contains("the rest of the album stays"), "{}", u.note);
    let r = import_unit(&l.ctx, &mut u, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::OkKept, "{}", said.borrow());
    assert!(said.borrow().contains("Replaced 1 track"), "{}", said.borrow());
    assert!(bytes("01 One.mp3") == before[0] && bytes("03 Three.mp3") == before[2], "the tracks not asked for are untouched");
    assert!(bytes("02 Two.mp3").len() > before[1].len() * 2, "track 2 is the 320 kbps one now");
    assert_eq!(std::fs::read_dir(&have).unwrap().count(), 3, "nothing was added beside the album");
    assert_eq!(std::fs::read_dir(l.ctx.library.join("Test Artist")).unwrap().count(), 1, "no second album folder was made");
    // the old file is in the undo backups, byte for byte, and the entry has left the list
    let kept = in_backups();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].file_name().is_some_and(|n| n == "02 Two.mp3") && std::fs::read(&kept[0]).unwrap() == before[1]);
    assert!(crate::replace::wanted(&l.ctx.replacements, &have).is_none() && crate::replace::all(&l.ctx.replacements).is_empty());
    // and Repair history can put it back
    let batches = crate::undo::batches(&l.ctx.repair_root);
    assert_eq!(batches.len(), 1);
    crate::undo::undo(&l.ctx.repair_root, &batches[0].id).unwrap();
    assert!(bytes("02 Two.mp3") == before[1], "Undo brings the old track back");
}

#[test]
fn a_single_track_swap_never_changes_the_tracks_format() {
    let l = lab("oneflac");
    let have = l.ctx.library.join("Test Artist").join("Rocket (2020)");
    for (no, title) in [(1, "One"), (2, "Two"), (3, "Three")] {
        tone(&l.ctx, &have.join(format!("{no:02} {title}.flac")), "Rocket", no, title, 0);
    }
    let bytes = |name: &str| std::fs::read(have.join(name)).unwrap();
    let names = || -> Vec<String> { let mut v: Vec<String> = std::fs::read_dir(&have).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect(); v.sort(); v };
    let all = ["01 One.flac".to_string(), "02 Two.flac".to_string(), "03 Three.flac".to_string()];
    let before = [bytes("01 One.flac"), bytes("02 Two.flac"), bytes("03 Three.flac")];
    let asked = have.join("02 Two.flac");
    crate::replace::add(&l.ctx.replacements, &have, "Test Artist", "Rocket (2020)", "Damaged or incomplete", &[asked.to_string_lossy().to_string()]).unwrap();
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    let in_backups = || -> Vec<PathBuf> { super::util::walk_files(&l.ctx.repair_root).into_iter().filter(|p| p.extension().is_some_and(|e| e == "flac" || e == "mp3")).collect() };

    // an MP3 download of the FLAC album: the swap is refused and says why, every byte stays, still waited for
    let mut mp3 = scan(&l, &download(&l, "Test Artist - Rocket (mp3)", "Rocket", &[(1, "One"), (2, "Two"), (3, "Three")], 320));
    let r = import_unit(&l.ctx, &mut mp3, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Failed, "{}", said.borrow());
    assert!(said.borrow().contains("'02 Two.flac' is FLAC and track 2 of this download would come in as MP3") && said.borrow().contains("Nothing in the library was changed"), "{}", said.borrow());
    assert!(names() == all && before == [bytes("01 One.flac"), bytes("02 Two.flac"), bytes("03 Three.flac")]);
    assert!(in_backups().is_empty() && crate::replace::wanted(&l.ctx.replacements, &have).is_some());

    // a FLAC download: its own track 2 is swapped in as it is (not the MP3 the importer builds from it)
    let dir = l.ctx.source.join("Test Artist - Rocket (flac)");
    for (no, title) in [(1, "One"), (2, "Two"), (3, "Three")] {
        tone(&l.ctx, &dir.join(format!("{no:02} {title}.flac")), "Rocket", no + 4, title, 0); // (another pitch: other bytes)
    }
    // (the tone's number tag gave the pitch; the track numbers are set right again)
    for (no, title) in [(1u32, "One"), (2, "Two"), (3, "Three")] {
        let (f, tmp) = (dir.join(format!("{no:02} {title}.flac")), dir.join("tmp.flac"));
        let mut c = tools::command(&l.ctx.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(&f).args(["-c", "copy", "-metadata", &format!("track={no}")]).arg(&tmp);
        assert!(tools::run_limited(&mut c, 60));
        std::fs::rename(&tmp, &f).unwrap();
    }
    settle(&dir);
    let theirs = std::fs::read(dir.join("02 Two.flac")).unwrap();
    assert!(theirs != before[1]);
    let mut flac = scan(&l, &dir);
    let r = import_unit(&l.ctx, &mut flac, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_ne!(r, ImportResult::Failed, "{}", said.borrow());
    assert!(said.borrow().contains("Replaced 1 track"), "{}", said.borrow());
    assert_eq!(names(), all, "three FLAC tracks, and no MP3 beside them");
    assert!(bytes("01 One.flac") == before[0] && bytes("03 Three.flac") == before[2], "the tracks not asked for are untouched");
    assert!(bytes("02 Two.flac") == theirs, "track 2 is the download's own FLAC file");
    assert!(dir.join("02 Two.flac").is_file(), "the download keeps its file");
    let kept = in_backups();
    assert!(kept.len() == 1 && std::fs::read(&kept[0]).unwrap() == before[1], "{kept:?}");
    assert!(crate::replace::wanted(&l.ctx.replacements, &have).is_none());
    // and Repair history can put the old track back
    let batches = crate::undo::batches(&l.ctx.repair_root);
    assert_eq!(batches.len(), 1);
    crate::undo::undo(&l.ctx.repair_root, &batches[0].id).unwrap();
    assert!(names() == all && bytes("02 Two.flac") == before[1], "Undo brings the old track back");
}

/// A loose track in an artist's folder (no track number, as loose files often are).
fn loose_track(l: &Lab, name: &str, album: &str, title: &str) -> PathBuf {
    let out = l.ctx.library.join("Test Artist").join(name);
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:duration=2", "-c:a", "libmp3lame", "-b:a", "128k"]);
    c.args(["-metadata", "artist=Test Artist", "-metadata", &format!("title={title}")]);
    if !album.is_empty() {
        c.args(["-metadata", &format!("album={album}")]);
    }
    c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
    out
}

#[test]
fn loose_tracks_go_to_the_recycle_bin_once_the_full_album_is_imported_and_holds_them() {
    let l = lab("loose");
    let artist = l.ctx.library.join("Test Artist");
    tone(&l.ctx, &artist.join("Rocket (2020)").join("01 One.mp3"), "Rocket", 1, "One", 320);
    let (alpha, beta, stray) = (loose_track(&l, "Alpha.mp3", "Comet", "Alpha"), loose_track(&l, "Beta.mp3", "Comet", "Beta"), loose_track(&l, "Stray.mp3", "Comet", "Stray"));
    let other = loose_track(&l, "Zeta.mp3", "Meteor", "Zeta");
    let before = std::fs::read(&alpha).unwrap();
    let paths = |list: &[&PathBuf]| list.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>();
    super::import::KEEP_ORIGINALS.store(true, std::sync::atomic::Ordering::Relaxed);
    let said = std::cell::RefCell::new(String::new());
    let names_in = |dir: &Path| -> Vec<String> { let mut v: Vec<String> = super::util::walk_files(dir).into_iter().filter(|p| p.extension().is_some_and(|e| e == "mp3")).map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect(); v.sort(); v };
    let in_backups = || names_in(&l.ctx.repair_root);
    let in_bin = || names_in(&l.base.join("bin"));

    // with nothing asked for, an album arriving moves no loose track
    let mut meteor = scan(&l, &download(&l, "Test Artist - Meteor", "Meteor", &[(1, "Zeta"), (2, "Eta")], 320));
    assert_ne!(import_unit(&l.ctx, &mut meteor, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {}), ImportResult::Failed, "{}", said.borrow());
    assert!(other.is_file() && in_backups().is_empty() && in_bin().is_empty());

    // "Get the full album" was clicked for the three loose tracks of Comet
    crate::loose::add(&l.ctx.loose_wants, &artist, "Test Artist", "Comet", &paths(&[&alpha, &beta, &stray])).unwrap();

    // another album of the artist arrives: it is not that album, nothing moves
    let mut planet = scan(&l, &download(&l, "Test Artist - Planet", "Planet", &[(1, "Alpha"), (2, "Beta")], 320));
    assert_ne!(import_unit(&l.ctx, &mut planet, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {}), ImportResult::Failed, "{}", said.borrow());
    assert!(alpha.is_file() && beta.is_file() && in_backups().is_empty() && in_bin().is_empty());
    assert!(crate::loose::wanted(&l.ctx.loose_wants, &artist, "Comet").is_some());

    // a download of that album that fails (two tracks would get one name): nothing reaches the library, and
    // no loose file moves
    let broken = l.ctx.source.join("Test Artist - Comet (broken)");
    tone(&l.ctx, &broken.join("01 Song.flac"), "Comet", 1, "Alpha", 0);
    tone(&l.ctx, &broken.join("01 Song.wav"), "Comet", 2, "Beta", 0);
    settle(&broken);
    let mut bad = scan(&l, &broken);
    assert_eq!(import_unit(&l.ctx, &mut bad, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {}), ImportResult::Failed, "{}", said.borrow());
    assert!(alpha.is_file() && beta.is_file() && stray.is_file() && in_bin().is_empty());
    assert_eq!(crate::loose::wanted(&l.ctx.loose_wants, &artist, "Comet").map(|w| w.files.len()), Some(3));
    std::fs::remove_dir_all(&broken).unwrap();

    // the album itself arrives, with two of the three songs: those two loose files go to the Recycle Bin, the
    // third stays and is still waited for. Nothing goes to the undo backups, and nothing is deleted
    let mut comet = scan(&l, &download(&l, "Test Artist - Comet", "Comet", &[(1, "Alpha"), (2, "Beta"), (3, "Gamma")], 320));
    assert_eq!(comet.action, Action::New, "{}", comet.note);
    assert_ne!(import_unit(&l.ctx, &mut comet, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {}), ImportResult::Failed, "{}", said.borrow());
    assert!(said.borrow().contains("2 loose tracks of 'Comet' are in the album now and went to the Recycle Bin"), "{}", said.borrow());
    let album = artist.join(&comet.folder);
    assert!(album.join("01 Alpha.mp3").is_file() && album.join("02 Beta.mp3").is_file());
    assert!(!alpha.exists() && !beta.exists() && stray.is_file() && other.is_file());
    assert_eq!(in_bin(), vec!["Alpha.mp3".to_string(), "Beta.mp3".to_string()]);
    assert_eq!(std::fs::read(l.base.join("bin").join("Alpha.mp3")).unwrap(), before, "the file in the bin is the loose file, whole");
    assert!(in_backups().is_empty() && crate::undo::batches(&l.ctx.repair_root).is_empty());
    assert_eq!(crate::loose::wanted(&l.ctx.loose_wants, &artist, "Comet").map(|w| w.files), Some(paths(&[&stray])));

    // the same title at another length is another recording: it stays (a nine-second "Alpha" against the album's two)
    std::fs::rename(l.base.join("bin").join("Beta.mp3"), &beta).unwrap();
    let long = artist.join("Alpha.mp3");
    let mut c = tools::command(&l.ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:duration=9", "-c:a", "libmp3lame", "-b:a", "128k", "-metadata", "artist=Test Artist", "-metadata", "title=Alpha", "-metadata", "album=Comet"]);
    c.arg(&long).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60));
    let done = crate::loose::recycle(&l.ctx.ffprobe, &l.ctx.library, &album, &paths(&[&long, &beta]), &l.ctx.recycle).unwrap();
    assert_eq!(done.moved, paths(&[&beta]));
    assert!(long.is_file() && done.stayed.len() == 1 && done.stayed[0].contains("not the same length"), "{:?}", done.stayed);
    // a bin that refuses, or says yes and takes nothing, leaves the file where it is and says so
    std::fs::rename(l.base.join("bin").join("Beta.mp3"), &beta).unwrap();
    let refused = crate::loose::recycle(&l.ctx.ffprobe, &l.ctx.library, &album, &paths(&[&beta]), &|_| Err("no room".into())).unwrap();
    assert!(refused.moved.is_empty() && refused.stayed[0].contains("no room") && beta.is_file());
    let idle = crate::loose::recycle(&l.ctx.ffprobe, &l.ctx.library, &album, &paths(&[&beta]), &|_| Ok(())).unwrap();
    assert!(idle.moved.is_empty() && idle.stayed[0].contains("still there") && beta.is_file());
    // "Move this loose file to the backups" holds to the same rule: the nine-second "Alpha" stays, and no
    // repair is recorded
    let kept = crate::loose::settle(&l.ctx.ffprobe, &l.ctx.library, &l.ctx.repair_root, &album, &paths(&[&long])).unwrap();
    assert!(kept.moved.is_empty() && kept.batch.is_none() && long.is_file() && kept.stayed.len() == 1 && kept.stayed[0].contains("not the same length"), "{:?}", kept.stayed);
    assert!(in_backups().is_empty());
    std::fs::remove_file(&long).unwrap();
    std::fs::copy(l.base.join("bin").join("Alpha.mp3"), &alpha).unwrap();

    // the control for a loose track whose album already holds its song: straight to the backups, nothing else
    let done = crate::loose::settle(&l.ctx.ffprobe, &l.ctx.library, &l.ctx.repair_root, &album, &paths(&[&alpha, &stray, &other])).unwrap();
    assert_eq!(done.moved, paths(&[&alpha]));
    assert!(done.batch.is_some() && done.stayed.len() == 2 && done.stayed.iter().all(|s| s.contains("has no song called like")), "{:?}", done.stayed);
    assert!(!alpha.exists() && beta.is_file() && stray.is_file() && other.is_file());
    // a file inside the album is never taken for a loose one, and a folder outside the library is refused
    let inside = crate::loose::settle(&l.ctx.ffprobe, &l.ctx.library, &l.ctx.repair_root, &album, &paths(&[&album.join("02 Beta.mp3")])).unwrap();
    assert!(inside.moved.is_empty() && inside.batch.is_none() && album.join("02 Beta.mp3").is_file());
    assert!(crate::loose::settle(&l.ctx.ffprobe, &l.ctx.library, &l.ctx.repair_root, &l.base, &paths(&[&beta])).is_err());
}

#[test]
fn a_song_with_no_album_tag_is_given_the_record_it_first_came_out_on() {
    use serde_json::json;
    let l = lab("songalbum");
    // Apple's answer for "Test Artist Polly": a compilation, a live album, a single, the album, a later reissue,
    // another artist's song of that name
    let apple = [
        json!({ "artistName": "Test Artist", "trackName": "Polly", "collectionName": "Hits of the 90s", "collectionArtistName": "Various Artists", "releaseDate": "1990-01-01T08:00:00Z", "trackCount": 40 }),
        json!({ "artistName": "Test Artist", "trackName": "Polly (Live)", "collectionName": "Live at the Hall", "releaseDate": "1990-06-01T08:00:00Z", "trackCount": 14 }),
        json!({ "artistName": "Test Artist", "trackName": "Polly", "collectionName": "Polly - Single", "releaseDate": "1991-08-01T08:00:00Z", "trackCount": 2 }),
        json!({ "artistName": "Test Artist", "trackName": "Polly", "collectionName": "Rocket (Remastered)", "releaseDate": "2011-09-20T07:00:00Z", "trackCount": 13 }),
        json!({ "artistName": "Test Artist", "trackName": "Polly", "collectionName": "Rocket", "releaseDate": "1991-09-24T07:00:00Z", "trackCount": 12 }),
        json!({ "artistName": "Somebody Else", "trackName": "Polly", "collectionName": "Their Record", "releaseDate": "1980-01-01T08:00:00Z", "trackCount": 10 }),
    ];
    assert_eq!(super::lookup::pick_song_album(&l.ctx, &apple, "Test Artist", "Polly").as_deref(), Some("Rocket"));
    assert_eq!(super::lookup::pick_song_album(&l.ctx, &apple, "Test Artist", "polly (feat. Someone)").as_deref(), Some("Rocket"));
    // only a single has it: the single is named; nothing by the artist: no album
    assert_eq!(super::lookup::pick_song_album(&l.ctx, &apple[2..3], "Test Artist", "Polly").as_deref(), Some("Polly - Single"));
    assert_eq!(super::lookup::pick_song_album(&l.ctx, &apple, "Test Artist", "Molly"), None);
    assert_eq!(super::lookup::pick_song_album(&l.ctx, &apple[5..], "Test Artist", "Polly"), None);

    // MusicBrainz: the earliest official album without a second kind
    let mb = [
        json!({ "title": "Polly", "artist-credit": [{ "name": "Test Artist" }], "releases": [
            { "title": "Best Of", "status": "Official", "date": "1989", "release-group": { "primary-type": "Album", "secondary-types": ["Compilation"] } },
            { "title": "Rocket", "status": "Official", "date": "1991-09-24", "release-group": { "primary-type": "Album", "secondary-types": [] } },
            { "title": "Rocket Demos", "status": "Bootleg", "date": "1990", "release-group": { "primary-type": "Album" } },
            { "title": "Polly", "status": "Official", "date": "1991-08-01", "release-group": { "primary-type": "Single" } } ] }),
        json!({ "title": "Polly", "artist-credit": [{ "name": "Somebody Else" }], "releases": [{ "title": "Their Record", "status": "Official", "date": "1980", "release-group": { "primary-type": "Album" } }] }),
    ];
    assert_eq!(super::tracklist::pick_recording_album(&l.ctx, &mb, "Test Artist", "Polly").as_deref(), Some("Rocket"));
    assert_eq!(super::tracklist::pick_recording_album(&l.ctx, &mb[1..], "Test Artist", "Polly"), None);
    // offline, nothing is asked at all
    assert_eq!(super::lookup::album_of_song(&l.ctx, "Test Artist", "Polly"), None);
}

#[test]
fn reading_a_download_in_three_steps_gives_what_reading_it_in_one_gave() {
    use super::scan::{add_pictures, quick_look, read_unit};
    let l = lab("steps");
    let have = l.ctx.library.join("Test Artist").join("Rocket (2020)");
    tone(&l.ctx, &have.join("01 One.mp3"), "Rocket", 1, "One", 96);
    // a new album, a better copy of one that is there, and a download with a file that is not audio
    let fresh = download(&l, "Test Artist - Comet", "Comet", &[(1, "One"), (2, "Two")], 320);
    let better = download(&l, "Test Artist - Rocket", "Rocket", &[(1, "One"), (2, "Two")], 320);
    let odd = download(&l, "Test Artist - Planet", "Planet", &[(1, "One"), (2, "Two")], 320);
    std::fs::write(odd.join("03 Three.flac"), b"this is text, not audio").unwrap();
    settle(&odd);
    for dir in [&fresh, &better, &odd] {
        let one = build_unit(&l.ctx, dir).unwrap();
        // the first step: only what listing the files says, and nothing is opened
        let q = quick_look(&l.ctx, dir).unwrap();
        assert_eq!((q.name.as_str(), q.bytes), (one.name.as_str(), one.bytes));
        assert_eq!(q.artist, "Test Artist");
        // the second step tells what it is working out, in order, and gives the facts without pictures
        let stages = std::cell::RefCell::new(vec![]);
        let (mut u, pictures) = read_unit(&l.ctx, dir, &|s| stages.borrow_mut().push(s)).unwrap();
        assert_eq!(*stages.borrow(), vec!["Reading the files", "Checking the audio", "Looking up the album", "Comparing the track list"]);
        assert!(u.cover.is_none() && u.spectro.is_none() && u.local_thumb.is_none() && !u.use_cover);
        // the tick is decided by the facts alone: the pictures do not change it
        let ticked = u.include;
        add_pictures(&l.ctx, &mut u, pictures);
        assert_eq!(u.include, ticked);
        // and the three steps together are the unit the one step gives
        let facts = |u: &Unit| format!("{:?} {:?}", (&u.name, &u.artist, &u.folder, u.include, &u.warnings, u.action, &u.note), (u.tracks.len(), u.convert, u.kbps, u.bytes, &u.fixes, &u.verdict.short, &u.existing));
        assert_eq!(facts(&u), facts(&one), "{}", dir.display());
        assert_eq!((u.use_cover, u.cover.is_some(), u.spectro.is_some(), u.local_thumb.is_some()), (one.use_cover, one.cover.is_some(), one.spectro.is_some(), one.local_thumb.is_some()));
    }
    // what the facts say about the tick: a sound new album is ticked, one with a file that is not what its name says is not
    assert!(build_unit(&l.ctx, &fresh).unwrap().include);
    let odd_unit = build_unit(&l.ctx, &odd).unwrap();
    assert!(!odd_unit.include && odd_unit.warnings.iter().any(|w| w.contains("unticked")), "{:?}", odd_unit.warnings);

    // a download with no audio in it, and one that is still arriving, get no row: the listing says so at once
    let empty = l.ctx.source.join("Just a picture");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join("cover.jpg"), b"x").unwrap();
    settle(&empty);
    assert_eq!(quick_look(&l.ctx, &empty).err().as_deref(), Some("no audio files"));
    let arriving = l.ctx.source.join("Test Artist - Arriving");
    tone(&l.ctx, &arriving.join("01 One.mp3"), "Arriving", 1, "One", 320);
    assert_eq!(quick_look(&l.ctx, &arriving).err().as_deref(), Some("still downloading"));
    // a name with no " - " in it is all album
    let plain = download(&l, "Meteor (2021)", "Meteor", &[(1, "One")], 320);
    let q = quick_look(&l.ctx, &plain).unwrap();
    assert_eq!((q.artist.as_str(), q.album.as_str(), q.audio_files), ("", "Meteor (2021)", 1));
}

#[test]
fn a_row_is_there_at_once_has_no_tick_until_its_facts_are_in_and_never_moves() {
    use super::scan::{quick_look, read_unit};
    use super::{facts_arrived, import_words, Row};
    let l = lab("rows");
    let dirs = [
        download(&l, "Test Artist - Alpha", "Alpha", &[(1, "One"), (2, "Two")], 320),
        download(&l, "Test Artist - Beta", "Beta", &[(1, "One"), (2, "Two")], 320),
        download(&l, "Test Artist - Gamma", "Gamma", &[(1, "One"), (2, "Two")], 320),
    ];
    std::fs::write(dirs[1].join("03 Three.flac"), b"this is text, not audio").unwrap();
    settle(&dirs[1]);
    // the listing: three rows at once, in folder order, none of them ticked or tickable
    let mut rows: Vec<Row> = dirs.iter().enumerate().map(|(i, d)| Row::placeholder(i, d, &quick_look(&l.ctx, d).unwrap())).collect();
    let names = |rows: &[Row]| rows.iter().map(|r| r.unit.name.clone()).collect::<Vec<_>>();
    let order = names(&rows);
    assert_eq!(order, vec!["Test Artist - Alpha", "Test Artist - Beta", "Test Artist - Gamma"]);
    assert!(rows.iter().all(|r| !r.ready() && !r.include && r.pending == Some("Reading the files") && r.quick.0 >= 2));
    let ticked = |rows: &[Row]| rows.iter().filter(|r| r.include && !r.done && r.ready()).count();
    assert_eq!(ticked(&rows), 0);

    // the facts arrive in another order than the rows stand in: the last one first
    let facts = |i: usize| read_unit(&l.ctx, &dirs[i], &|_| {}).map(|x| x.0);
    facts_arrived(&mut rows, 2, facts(2), false);
    assert_eq!(names(&rows), order, "no row moved");
    assert!(rows[2].ready() && rows[2].include && rows[2].pictures_pending && !rows[0].ready() && !rows[1].ready());
    assert_eq!(ticked(&rows), 1);
    // the one with a file that is not audio: its tick comes out off, decided by its own check
    facts_arrived(&mut rows, 1, facts(1), true);
    assert!(rows[1].ready() && !rows[1].include && rows[1].held, "it arrived while the pointer was on the list");
    assert!(rows[1].unit.warnings.iter().any(|w| w.contains("unticked")), "{:?}", rows[1].unit.warnings);
    // one that cannot be read after all stays where it is, as a line that says why
    facts_arrived(&mut rows, 0, Err("lookup stopped unexpectedly".into()), false);
    assert_eq!(names(&rows), order, "no row was taken away");
    assert!(!rows[0].ready() && rows[0].pending.is_none() && rows[0].left_out.as_deref() == Some("lookup stopped unexpectedly"));
    assert_eq!(ticked(&rows), 1);
    // facts for a download that has no row change nothing
    facts_arrived(&mut rows, 9, facts(2), false);
    assert_eq!(names(&rows), order);

    // what the import control says: what it waits for while anything is being worked out, then the count
    assert_eq!(import_words(true, 3, 0, 0, 3), "Import waits for 3 downloads still being checked");
    assert_eq!(import_words(true, 1, 2, 2, 3), "Import waits for 1 download still being checked");
    assert_eq!(import_words(true, 0, 2, 2, 3), "Import waits for 2 covers");
    assert_eq!(import_words(true, 0, 1, 2, 3), "Import waits for 1 cover");
    assert_eq!(import_words(false, 0, 0, 2, 3), "Import 2 albums");
    assert_eq!(import_words(false, 0, 0, 1, 3), "Import 1 album");
    assert_eq!(import_words(false, 0, 0, 0, 3), "Tick an album to import it");
    assert_eq!(import_words(false, 0, 0, 0, 0), "");
}

/// A track of `secs` seconds tagged only with a title (no track number).
fn untitled(ctx: &Ctx, out: &Path, secs: u32, freq: u32) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut c = tools::command(&ctx.ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("sine=frequency={freq}:duration={secs}")]);
    c.args(["-c:a", "libmp3lame", "-b:a", "192k", "-metadata", "artist=Test Artist", "-metadata", "album=Vague", "-metadata", "title=Untitled"]);
    c.arg(out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
}

#[test]
fn two_recordings_with_the_same_vague_tags_are_both_kept() {
    // review finding: "Untitled" twice with no track numbers was one song, so one recording was dropped
    // and then recycled with the download
    let l = lab("vague");
    let dir = l.ctx.source.join("Test Artist - Vague");
    untitled(&l.ctx, &dir.join("take a.mp3"), 2, 300);
    untitled(&l.ctx, &dir.join("take b.mp3"), 7, 500);
    settle(&dir);
    let u = scan(&l, &dir);
    assert_eq!(u.tracks.len(), 2, "{:?}", u.warnings);
    assert!(!u.warnings.iter().any(|w| w.contains("duplicate")), "{:?}", u.warnings);
    // the same recording twice (the same length) is still one song
    let same = l.ctx.source.join("Test Artist - Same");
    untitled(&l.ctx, &same.join("take a.mp3"), 3, 300);
    untitled(&l.ctx, &same.join("take a copy.mp3"), 3, 300);
    settle(&same);
    let u = scan(&l, &same);
    assert_eq!(u.tracks.len(), 1, "{:?}", u.warnings);
}

#[test]
fn a_leftover_is_only_removed_while_it_holds_what_was_imported() {
    // review finding: the clean-up compared the folder with a rescan, which reads a track that arrived
    // after the import as part of the leftover, so that track went to the Recycle Bin never imported
    use super::import::{leftover_is_what_was_imported, print_on_disk, scanned_print};
    let l = lab("leftover");
    let dir = download(&l, "Test Artist - Leftover", "Leftover", &[(1, "One"), (2, "Two")], 320);
    let imported = scan(&l, &dir);
    assert_eq!(scanned_print(&imported), print_on_disk(&imported));
    let item = imported.item.to_string_lossy().to_string();
    // the import went through, but Soulseek had the download open: it is remembered with what was imported
    l.ctx.set_pending(&item, Some(&scanned_print(&imported)));
    assert!(leftover_is_what_was_imported(&l.ctx, &imported));
    // a track arrives, and the next scan reads it as part of the leftover
    tone(&l.ctx, &dir.join("03 Three.mp3"), "Leftover", 3, "Three", 320);
    settle(&dir);
    let rescanned = scan(&l, &dir);
    assert!(!changed_since_scan(&rescanned), "the rescan alone sees nothing new: the old check would recycle");
    assert!(!leftover_is_what_was_imported(&l.ctx, &rescanned), "the new track was never imported");
    // with it gone again, the leftover is what was imported once more
    std::fs::remove_file(dir.join("03 Three.mp3")).unwrap();
    assert!(leftover_is_what_was_imported(&l.ctx, &scan(&l, &dir)));
    // forgetting it, and a line from before prints were kept, are not "imported"
    l.ctx.set_pending(&item, None);
    assert!(l.ctx.pending_print(&item).is_none());
    std::fs::write(&l.ctx.pending_file, format!("{item}\r\n")).unwrap();
    assert!(l.ctx.pending_print(&item).is_none() && !leftover_is_what_was_imported(&l.ctx, &imported));
    // and a new line for it replaces the old one
    l.ctx.set_pending(&item, Some("2-x"));
    assert_eq!(l.ctx.pending_print(&item).as_deref(), Some("2-x"));
    assert_eq!(std::fs::read_to_string(&l.ctx.pending_file).unwrap().lines().count(), 1);
}

#[test]
fn two_long_album_names_that_end_up_one_folder_never_import_at_once() {
    // review finding: the import order kept the names apart by their full text, but both were shortened to
    // one folder name, so the two imports shared and deleted one ".importing" folder
    use super::import::{fitted_folder, running_key};
    let l = lab("longnames");
    let stem = "A Very Long Album Name That Goes On ".repeat(5);
    let (one, two) = (format!("{stem}Part One"), format!("{stem}Part Two"));
    assert_eq!(fitted_folder(&l.ctx, "Test Artist", &one), fitted_folder(&l.ctx, "Test Artist", &two), "both are cut to one name");
    assert_ne!(super::scan::place_key("Test Artist", &one), super::scan::place_key("Test Artist", &two), "the full names differ");
    assert_eq!(running_key(&l.ctx, "Test Artist", &one), running_key(&l.ctx, "Test Artist", &two));
    // short names that differ stay apart
    assert_ne!(running_key(&l.ctx, "Test Artist", "Comet"), running_key(&l.ctx, "Test Artist", "Rocket"));
}

#[test]
fn a_stopped_import_adds_nothing_and_leaves_no_build_folder() {
    let mut l = lab("stop");
    l.ctx.keep_originals = Some(true);
    let dir = download(&l, "Test Artist - Comet", "Comet", &[(1, "One"), (2, "Two")], 320);
    let mut u = scan(&l, &dir);
    // asked to stop before it begins: it stops at the first check, between tracks
    l.ctx.stop = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
    let said = std::cell::RefCell::new(String::new());
    let r = import_unit(&l.ctx, &mut u, &|text, _| *said.borrow_mut() = text.to_string(), None, &|| {});
    assert_eq!(r, ImportResult::Failed);
    assert!(said.borrow().starts_with("Stopped"), "{}", said.borrow());
    assert!(std::fs::read_dir(&l.ctx.library).unwrap().next().is_none(), "no album, build folder or artist folder is left");
    assert!(dir.join("01 One.mp3").exists() && dir.join("02 Two.mp3").exists(), "the download stays");
    // not stopped: the same import goes through
    l.ctx.stop = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    assert_eq!(import_unit(&l.ctx, &mut u, &|_, _| {}, None, &|| {}), ImportResult::Ok);
}
