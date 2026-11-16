//! Loose tracks that belong to an album: "Get the full album".
//!
//! A track lying in an artist's folder usually comes from an album. The Health page names that album (from
//! the track's album tag, or looked up by artist and title) and offers "Get the full album". A click writes
//! the artist folder, the album and the loose files down here and opens the Soulseek search; nothing is
//! downloaded by the click. When that album later reaches the library through the importer, and only once
//! that import has finished without error, each of those loose files whose song the album now holds (the
//! same title and the same length, within three seconds) is sent to the Windows Recycle Bin. That is
//! Bradley's own exception to the rule that everything goes to the undo backups (2026-11-11: "Loose file can
//! be deleted so long as the new album is imported successfully"), and it covers this case only. It is never
//! a permanent delete: a file the Recycle Bin does not take stays where it is, and so does a loose file whose
//! song the album does not hold.
//!
//! For a loose file whose album is already in the library and already holds its song, the page offers a
//! move to the undo backups (`settle`), like every other repair.
use crate::importer::util::{album_key, ext_lower, file_stem, walk_files};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Want {
    /// the artist's folder in the library, where the loose files lie
    pub artist_dir: String,
    pub artist: String,
    /// the album the loose files come from, as their tags (or the lookup) name it
    pub album: String,
    /// the loose files
    pub files: Vec<String>,
    pub when: String,
}

/// What became of the loose files of one album.
#[derive(Default, Debug)]
pub struct Settled {
    /// the files that left the library (to the undo backups, or to the Recycle Bin)
    pub moved: Vec<String>,
    /// the files that stayed, each with why, in plain words
    pub stayed: Vec<String>,
    /// the undo record, when anything was moved
    pub batch: Option<String>,
}

/// Where the list is kept (the app's own folder; a test run's is its own).
pub fn file() -> PathBuf {
    crate::config::app_dir().join("loose-albums.json")
}

pub fn all(file: &Path) -> Vec<Want> {
    std::fs::read(file).ok().filter(|b| b.len() < 5_000_000).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn same_path(a: &str, b: &Path) -> bool {
    a.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']))
}

fn save(file: &Path, list: &[Want]) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
    crate::util::write_atomic(file, &text).map_err(|e| crate::util::plain_err(format!("The list of albums to get could not be saved: {e}")))
}

/// Writes down that the full album is wanted for these loose files (once per artist folder and album:
/// asking again adds any new files to the same entry).
pub fn add(file: &Path, artist_dir: &Path, artist: &str, album: &str, files: &[String]) -> Result<(), String> {
    let mut list = all(file);
    let key = album_key(album);
    let mut files: Vec<String> = files.to_vec();
    if let Some(at) = list.iter().position(|w| same_path(&w.artist_dir, artist_dir) && album_key(&w.album) == key) {
        for f in list.remove(at).files {
            if !files.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                files.push(f);
            }
        }
    }
    list.push(Want { artist_dir: artist_dir.to_string_lossy().to_string(), artist: artist.to_string(), album: album.to_string(), files, when: chrono::Local::now().to_rfc3339() });
    save(file, &list)
}

/// Takes an album out of the list: it is no longer waited for, and its loose files stay as they are.
pub fn forget(file: &Path, artist_dir: &Path, album: &str) {
    let list = all(file);
    let key = album_key(album);
    let left: Vec<Want> = list.iter().filter(|w| !(same_path(&w.artist_dir, artist_dir) && album_key(&w.album) == key)).cloned().collect();
    if left.len() != list.len() {
        let _ = save(file, &left);
    }
}

/// Takes out the entries none of whose loose files is lying there any more (they were filed, moved or
/// removed by other means), and the missing files of the others, so the "Waiting for" list does not keep
/// them for ever. Only while the library folder itself is there (a drive that is not connected makes
/// everything look gone) and only for files inside it. Nothing in the library is touched. Returns how
/// many entries went.
pub fn prune(file: &Path, library: &Path) -> usize {
    if !library.is_dir() {
        return 0;
    }
    let list = all(file);
    let mut left: Vec<Want> = vec![];
    let mut changed = false;
    for mut w in list.iter().cloned() {
        let before = w.files.len();
        w.files.retain(|f| !Path::new(f).starts_with(library) || Path::new(f).is_file());
        changed |= w.files.len() != before;
        if !w.files.is_empty() {
            left.push(w);
        }
    }
    if changed {
        let _ = save(file, &left);
    }
    list.len() - left.len()
}

/// Whether the full album was asked for these loose files' album.
#[cfg(test)]
pub fn wanted(file: &Path, artist_dir: &Path, album: &str) -> Option<Want> {
    let key = album_key(album);
    all(file).into_iter().find(|w| same_path(&w.artist_dir, artist_dir) && album_key(&w.album) == key)
}

fn is_audio(p: &Path) -> bool {
    crate::importer::scan::is_audio_ext(&ext_lower(p))
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// The song a file holds, for comparing: its title (the tag, else the title its name gives, without case,
/// punctuation or brackets) and how long it is in seconds (0 when that is not known).
/// What a file is, for telling whether an album holds the same recording: its title as compared (brackets
/// and all stripped), the words that make it another recording of that title ("live", "acoustic",
/// "instrumental", which the stripping throws away), its length, and how good it sounds.
struct Song {
    title: String,
    version: String,
    secs: f64,
    sound: u32,
}

/// Words in a title that make it another recording of the song, sorted and joined: "Song (Live)" and
/// "Song" are two recordings; "Song (2011 Remaster)" and "Song" are one.
fn version_words(title: &str) -> String {
    const OTHER: [&str; 14] = ["live", "acoustic", "instrumental", "remix", "mix", "demo", "unplugged", "acapella", "cappella", "karaoke", "session", "orchestral", "piano", "stripped"];
    let mut found: Vec<&str> = title.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter_map(|w| OTHER.iter().find(|o| **o == w).copied()).collect();
    found.sort();
    found.dedup();
    found.join(" ")
}

/// What `sound_of` says of a lossless file.
const LOSSLESS: u32 = 10_000;

/// How good a file sounds, to compare two copies: lossless above everything, else its bitrate.
fn sound_of(info: &crate::healthscan::Info) -> u32 {
    let lossless = ["flac", "alac", "wavpack", "ape", "tta"].contains(&info.codec.as_str()) || info.codec.starts_with("pcm_");
    if lossless { LOSSLESS } else { info.kbps }
}

/// Whether the album's copy sounds good enough for the loose one to leave. The library is kept as 320 MP3s
/// (the importer converts to that), so 320 is as good as it needs to be: a loose FLAC leaves for a 320 of
/// it, not for a 128. Within a fifth counts, since a variable bitrate reads a little under its mark.
fn good_enough(album: u32, loose: u32) -> bool {
    album as f64 >= loose.min(320) as f64 * 0.8
}

fn song_of(ffprobe: &Path, file: &Path) -> Song {
    let info = crate::healthscan::probe(ffprobe, file);
    let tagged = info.tags.get("title").map(|t| t.trim().to_string()).unwrap_or_default();
    let title = if tagged.is_empty() { crate::importer::scan::parse_file_name(&file_stem(&leaf(file)), "").title.unwrap_or_default() } else { tagged };
    Song { title: crate::importer::tracklist::norm_title(&title), version: version_words(&title), secs: info.duration, sound: sound_of(&info) }
}

/// Two lengths this close are the same recording.
const SAME_LENGTH_SECS: f64 = 3.0;

/// Whether two files of one title are the same recording: both lengths known, and within three seconds.
pub fn same_length(a: f64, b: f64) -> bool {
    a > 0.0 && b > 0.0 && (a - b).abs() <= SAME_LENGTH_SECS
}

fn in_library(p: &Path, library: &Path) -> bool {
    p != library && p.starts_with(library) && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Which of these loose files the album holds: (the ones that may leave, why each of the others stays).
/// The album must have a song of the same title and the same version words ("live", "acoustic") that is
/// as long as the loose file, within three seconds (and both lengths must be known): the same title at
/// another length, or of another version, is another recording. And the album's copy must sound at least
/// as good: a loose FLAC does not leave for an MP3 of it. A file that is gone is neither. Reads tags;
/// changes nothing. `keep_lossless`: lossless downloads are kept (the choice in Settings), so the library is
/// not all 320 MP3s, and a loose lossless file leaves only for a lossless copy of its song.
fn held_by(ffprobe: &Path, library: &Path, album_dir: &Path, files: &[String], keep_lossless: bool) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    if !in_library(album_dir, library) || !album_dir.is_dir() {
        return Err(format!("{} is not an album folder in the library", album_dir.display()));
    }
    let album = leaf(album_dir);
    let songs: Vec<Song> = walk_files(album_dir).into_iter().filter(|p| is_audio(p)).map(|p| song_of(ffprobe, &p)).filter(|s| !s.title.is_empty()).collect();
    let (mut go, mut stayed) = (vec![], vec![]);
    for f in files {
        let path = PathBuf::from(f);
        let name = leaf(&path);
        if !path.is_file() {
            continue; // filed, moved or gone since it was asked for
        }
        if !in_library(&path, library) {
            stayed.push(format!("{name} is not in the library"));
            continue;
        }
        if path.starts_with(album_dir) {
            stayed.push(format!("{name} is in the album itself"));
            continue;
        }
        let me = song_of(ffprobe, &path);
        let named: Vec<&Song> = songs.iter().filter(|s| s.title == me.title && s.version == me.version).collect();
        let same: Vec<&&Song> = named.iter().filter(|s| same_length(s.secs, me.secs)).collect();
        if me.title.is_empty() {
            stayed.push(format!("{name} has no title to know its song by"));
        } else if named.is_empty() {
            stayed.push(format!("'{album}' has no song called like {name}, so it stays where it is"));
        } else if same.is_empty() {
            stayed.push(format!("the song in '{album}' is not the same length as {name} ({} against {}), so it stays where it is", crate::importer::util::format_time(named[0].secs), crate::importer::util::format_time(me.secs)));
        } else if !same.iter().any(|s| good_enough(s.sound, me.sound) && !(keep_lossless && me.sound == LOSSLESS && s.sound < LOSSLESS)) {
            stayed.push(format!("the album's copy of {name} sounds worse than this one, so it stays where it is"));
        } else if std::fs::OpenOptions::new().read(true).write(true).open(&path).is_err() {
            stayed.push(format!("{name} is open in another program"));
        } else {
            go.push(path);
        }
    }
    Ok((go, stayed))
}

/// Moves each of these loose files whose song the album holds (by title and by length) into the undo backups, as one
/// batch. A file that is gone, lies inside the album itself, is open in another program, or whose song the
/// album does not hold, stays, and why is said. Nothing is deleted. (As the tests call it: lossless not kept.)
#[cfg(test)]
pub fn settle(ffprobe: &Path, library: &Path, backups: &Path, album_dir: &Path, files: &[String]) -> Result<Settled, String> {
    settle_keeping(ffprobe, library, backups, album_dir, files, false)
}

/// `settle`, told whether lossless downloads are kept (see `held_by`): what the Health page calls, with
/// the choice from Settings.
pub fn settle_keeping(ffprobe: &Path, library: &Path, backups: &Path, album_dir: &Path, files: &[String], keep_lossless: bool) -> Result<Settled, String> {
    let (go, stayed) = held_by(ffprobe, library, album_dir, files, keep_lossless)?;
    let mut out = Settled { stayed, ..Default::default() };
    if go.is_empty() {
        return Ok(out);
    }
    let title = format!("Loose {} now in the album: {} ({})", if go.len() == 1 { "track" } else { "tracks" }, leaf(album_dir), if go.len() == 1 { "1 file".to_string() } else { format!("{} files", go.len()) });
    let batch = crate::undo::Batch::begin(backups, library, &title)?;
    for path in &go {
        match batch.remove(path, "the album in the library holds this song") {
            Ok(_) => out.moved.push(path.to_string_lossy().to_string()),
            Err(why) => out.stayed.push(format!("{}: {why}", leaf(path))),
        }
    }
    if batch.changes() > 0 {
        out.batch = Some(batch.id.clone());
    }
    batch.close();
    Ok(out)
}

/// Sends each of these loose files whose song the album holds, by title and by length, to the Recycle Bin
/// (`bin`; the app's is `undo::to_recycle_bin`). Only for an album that has just been imported without
/// error (see `album_arrived`). A file the bin did not take stays where it is and is named; nothing is ever
/// deleted for good here.
#[cfg(test)]
pub fn recycle(ffprobe: &Path, library: &Path, album_dir: &Path, files: &[String], bin: &dyn Fn(&Path) -> Result<(), String>) -> Result<Settled, String> {
    recycle_keeping(ffprobe, library, album_dir, files, bin, false)
}

/// `recycle`, told whether lossless downloads are kept (see `held_by`): what the importer calls.
pub fn recycle_keeping(ffprobe: &Path, library: &Path, album_dir: &Path, files: &[String], bin: &dyn Fn(&Path) -> Result<(), String>, keep_lossless: bool) -> Result<Settled, String> {
    let (go, stayed) = held_by(ffprobe, library, album_dir, files, keep_lossless)?;
    let mut out = Settled { stayed, ..Default::default() };
    for path in &go {
        match bin(path) {
            Ok(()) if !path.exists() => out.moved.push(path.to_string_lossy().to_string()),
            Ok(()) => out.stayed.push(format!("{} is still there: the Recycle Bin did not take it", leaf(path))),
            Err(why) => out.stayed.push(format!("{} could not go to the Recycle Bin: {why}", leaf(path))),
        }
    }
    Ok(out)
}

/// Called by the importer when an album has been put in the library and the import of it has finished
/// without error. If the full album was asked for loose files of that artist and this is that album, the
/// loose files whose songs it holds (title and length) go to the Recycle Bin, and the entry leaves the list
/// once none of its files is left lying loose. Returns what was said about it, for the import log.
/// `keep_lossless`: see `held_by`.
pub fn album_arrived(wants: &Path, ffprobe: &Path, library: &Path, album_dir: &Path, bin: &dyn Fn(&Path) -> Result<(), String>, keep_lossless: bool) -> Option<String> {
    let list = all(wants);
    let key = album_key(&leaf(album_dir));
    let at = list.iter().position(|w| album_dir.starts_with(Path::new(w.artist_dir.trim_end_matches(['\\', '/']))) && album_key(&w.album) == key)?;
    let want = &list[at];
    let done = match recycle_keeping(ffprobe, library, album_dir, &want.files, bin, keep_lossless) {
        Ok(d) => d,
        Err(why) => return Some(format!("loose tracks of '{}' were left where they are: {why}", want.album)),
    };
    // what still lies loose is still waited for (a later, fuller copy of the album may hold it)
    let left: Vec<String> = want.files.iter().filter(|f| Path::new(f).is_file()).cloned().collect();
    let mut list = list.clone();
    if left.is_empty() {
        list.remove(at);
    } else {
        list[at].files = left;
    }
    let _ = save(wants, &list);
    let mut said = match done.moved.len() {
        0 => format!("no loose track of '{}' was removed", want.album),
        1 => format!("1 loose track of '{}' is in the album now and went to the Recycle Bin", want.album),
        n => format!("{n} loose tracks of '{}' are in the album now and went to the Recycle Bin", want.album),
    };
    if !done.stayed.is_empty() {
        said.push_str(&format!(" ({})", done.stayed.join("; ")));
    }
    Some(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_full_album_is_asked_for_once_per_artist_and_album() {
        let dir = std::env::temp_dir().join(format!("music-recs-loose-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("loose-albums.json");
        let artist = Path::new(r"X:\Lib\A");
        assert!(all(&file).is_empty() && wanted(&file, artist, "Comet").is_none());
        add(&file, artist, "A", "Comet", &[r"X:\Lib\A\Alpha.mp3".to_string()]).unwrap();
        // the same album under another spelling of its name, with one more file: still one entry, both files
        add(&file, Path::new(r"x:\lib\a\"), "A", "Comet (2021)", &[r"X:\Lib\A\Beta.mp3".to_string(), r"x:\lib\a\alpha.mp3".to_string()]).unwrap();
        add(&file, artist, "A", "Comet (Live)", &[r"X:\Lib\A\Gamma.mp3".to_string()]).unwrap();
        add(&file, Path::new(r"X:\Lib\B"), "B", "Comet", &[r"X:\Lib\B\Delta.mp3".to_string()]).unwrap();
        assert_eq!(all(&file).len(), 3);
        assert_eq!(wanted(&file, artist, "Comet").unwrap().files.len(), 2);
        assert_eq!(wanted(&file, artist, "Comet (Live)").unwrap().files, vec![r"X:\Lib\A\Gamma.mp3".to_string()]);
        // forgetting one leaves the others
        forget(&file, artist, "Comet (2021)");
        assert!(wanted(&file, artist, "Comet").is_none() && wanted(&file, artist, "Comet (Live)").is_some() && all(&file).len() == 2);
        // a list that cannot be read is no list
        std::fs::write(&file, b"{ not a list").unwrap();
        assert!(all(&file).is_empty());

        // an entry whose loose files are all gone is no longer waited for; one with a file left keeps that file
        let lib = dir.join("lib");
        std::fs::create_dir_all(lib.join("A")).unwrap();
        let (alpha, beta, gamma) = (lib.join(r"A\Alpha.mp3"), lib.join(r"A\Beta.mp3"), lib.join(r"A\Gamma.mp3"));
        for f in [&alpha, &beta, &gamma] {
            std::fs::write(f, b"x").unwrap();
        }
        let s = |p: &PathBuf| p.to_string_lossy().to_string();
        std::fs::remove_file(&file).unwrap();
        add(&file, &lib.join("A"), "A", "Comet", &[s(&alpha), s(&beta)]).unwrap();
        add(&file, &lib.join("A"), "A", "Meteor", &[s(&gamma)]).unwrap();
        assert_eq!((prune(&file, &lib), all(&file).len()), (0, 2));
        assert_eq!(prune(&file, &dir.join("no such drive")), 0, "a library that is not there prunes nothing");
        std::fs::remove_file(&alpha).unwrap();
        std::fs::remove_file(&gamma).unwrap();
        assert_eq!(prune(&file, &lib), 1);
        assert_eq!(all(&file).into_iter().map(|w| (w.album, w.files)).collect::<Vec<_>>(), vec![("Comet".to_string(), vec![s(&beta)])]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod match_tests {
    use super::*;

    #[test]
    fn another_version_of_a_song_or_a_worse_copy_of_it_does_not_hold_a_loose_track() {
        // the words that make another recording survive the stripping of brackets
        assert_eq!(version_words("Song (Live)"), "live");
        assert_eq!(version_words("Song - Acoustic Version"), "acoustic");
        assert_eq!(version_words("Song (2011 Remaster)"), "");
        assert_eq!(version_words("Olive Tree"), ""); // (a word, not a part of one)
        assert_ne!(version_words("Song (Live)"), version_words("Song"));
        // a 320 holds a loose FLAC; a 128 does not; a VBR a little under 320 does
        assert!(good_enough(320, 10_000) && good_enough(260, 10_000) && good_enough(320, 320));
        assert!(!good_enough(128, 10_000) && !good_enough(192, 320));
    }
}
