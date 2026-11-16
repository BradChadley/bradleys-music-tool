//! "Replace this album": the albums Health was asked to find a better copy for.
//!
//! Clicking it on the Health page writes the album's folder down here and opens the Soulseek search. When a
//! download of that album later reaches the import list, the importer treats it as the replacement for
//! that folder whether or not it looks "better" by bitrate or track count (a damaged 320 kbps copy is
//! replaced by a sound 320 kbps one), but only if the download passes the importer's own check of its
//! audio. The swap is the importer's usual one: the new album is built beside the old, the old folder goes
//! to the undo backups, and Repair history can put it back. Once that has happened the entry is taken out.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Want {
    /// the album's folder in the library
    pub folder: String,
    pub artist: String,
    pub album: String,
    /// what Health found ("Damaged or incomplete")
    pub why: String,
    pub when: String,
    /// "Replace only these tracks": the files to swap, the rest of the album stays. Empty = the whole album.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Where the list is kept (the app's own folder; a test run's is its own).
pub fn file() -> PathBuf {
    crate::config::app_dir().join("replacements.json")
}

pub fn all(file: &Path) -> Vec<Want> {
    std::fs::read(file).ok().filter(|b| b.len() < 5_000_000).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn same(a: &str, b: &Path) -> bool {
    a.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']))
}

fn save(file: &Path, list: &[Want]) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
    crate::util::write_atomic(file, &text).map_err(|e| crate::util::plain_err(format!("The list of albums to replace could not be saved: {e}")))
}

/// Writes down that this album wants a better copy (once: asking again only renews the reason).
pub fn add(file: &Path, folder: &Path, artist: &str, album: &str, why: &str, files: &[String]) -> Result<(), String> {
    let mut list = all(file);
    list.retain(|w| !same(&w.folder, folder));
    list.push(Want { folder: folder.to_string_lossy().to_string(), artist: artist.to_string(), album: album.to_string(), why: why.to_string(), when: chrono::Local::now().to_rfc3339(), files: files.to_vec() });
    save(file, &list)
}

/// Whether a better copy was asked for this album folder.
pub fn wanted(file: &Path, folder: &Path) -> Option<Want> {
    all(file).into_iter().find(|w| same(&w.folder, folder))
}

/// The album was replaced: it is no longer waited for.
pub fn done(file: &Path, folder: &Path) {
    let list = all(file);
    let left: Vec<Want> = list.iter().filter(|w| !same(&w.folder, folder)).cloned().collect();
    if left.len() != list.len() {
        let _ = save(file, &left);
    }
}

/// Takes out the entries that can no longer be met, so the "Waiting for" list does not keep them for ever:
/// the album's folder is gone (it was replaced, renamed or filed by other means), or, where single tracks
/// were asked for, none of them is there any more. Only while the library folder itself is there (a drive
/// that is not connected makes everything look gone) and only for entries inside it. Nothing in the
/// library is touched. Returns how many entries went.
pub fn prune(file: &Path, library: &Path) -> usize {
    if !library.is_dir() {
        return 0;
    }
    let list = all(file);
    let gone = |w: &Want| {
        let folder = Path::new(w.folder.trim_end_matches(['\\', '/']));
        folder.starts_with(library) && (!folder.is_dir() || (!w.files.is_empty() && !w.files.iter().any(|f| Path::new(f).is_file())))
    };
    let left: Vec<Want> = list.iter().filter(|w| !gone(w)).cloned().collect();
    if left.len() != list.len() {
        let _ = save(file, &left);
    }
    list.len() - left.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_whose_album_is_gone_is_no_longer_waited_for() {
        let dir = std::env::temp_dir().join(format!("music-recs-replace-prune-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let lib = dir.join("lib");
        let (here, tracks, gone) = (lib.join(r"A\Here (2020)"), lib.join(r"A\Tracks (2020)"), lib.join(r"A\Gone (2020)"));
        for d in [&here, &tracks, &gone] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(tracks.join("01.mp3"), b"x").unwrap();
        let file = dir.join("replacements.json");
        let s = |p: PathBuf| p.to_string_lossy().to_string();
        add(&file, &here, "A", "Here (2020)", "Low bitrate", &[]).unwrap();
        add(&file, &tracks, "A", "Tracks (2020)", "Damaged or incomplete", &[s(tracks.join("01.mp3")), s(tracks.join("02.mp3"))]).unwrap();
        add(&file, &gone, "A", "Gone (2020)", "Low bitrate", &[]).unwrap();
        add(&file, Path::new(r"Q:\Elsewhere\B\Other"), "B", "Other", "Low bitrate", &[]).unwrap();
        // everything is there: nothing goes
        assert_eq!((prune(&file, &lib), all(&file).len()), (0, 4));
        // the library's drive is not there: nothing goes, whatever looks gone
        assert_eq!(prune(&file, &dir.join("no such drive")), 0);
        // an album folder is gone, and the one track that was still there of another: both entries go; the
        // album that is there stays, and so does an entry outside this library
        std::fs::remove_dir_all(&gone).unwrap();
        std::fs::remove_file(tracks.join("01.mp3")).unwrap();
        assert_eq!(prune(&file, &lib), 2);
        let left: Vec<String> = all(&file).into_iter().map(|w| w.album).collect();
        assert_eq!(left, vec!["Here (2020)".to_string(), "Other".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_album_is_waited_for_once_and_until_it_is_replaced() {
        let dir = std::env::temp_dir().join(format!("music-recs-replace-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("replacements.json");
        let album = Path::new(r"X:\Lib\A\Rocket (2017)");
        assert!(wanted(&file, album).is_none() && all(&file).is_empty());
        add(&file, album, "A", "Rocket (2017)", "Damaged or incomplete", &[]).unwrap();
        add(&file, Path::new(r"x:\lib\a\rocket (2017)\"), "A", "Rocket (2017)", "Fake lossless", &["x.mp3".to_string()]).unwrap();
        add(&file, Path::new(r"X:\Lib\B\Other"), "B", "Other", "Upscaled MP3", &[]).unwrap();
        assert_eq!(all(&file).len(), 2);
        assert_eq!(wanted(&file, album).map(|w| (w.why, w.files)), Some(("Fake lossless".to_string(), vec!["x.mp3".to_string()])));
        done(&file, album);
        assert!(wanted(&file, album).is_none() && wanted(&file, Path::new(r"X:\Lib\B\Other")).is_some());
        // a list that cannot be read is no list, and asking again starts a new one
        std::fs::write(&file, b"{ not a list").unwrap();
        assert!(all(&file).is_empty());
        add(&file, album, "A", "Rocket (2017)", "x", &[]).unwrap();
        assert_eq!(all(&file).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
