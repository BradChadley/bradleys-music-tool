//! Reading the music folder: which artists there are and which albums each has.
//!
//! The plain shape is `<Artist>\<Album>\tracks`. Real libraries bend it, and every bend used to be read
//! as "one album" (see "the shape of an artist folder" below): shelves, genre folders, albums whose tracks
//! sit one folder deeper, and extra releases kept inside an album's folder.
use crate::util::{album_key, artist_key, folder_year, norm, track_key_from_file};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct LibArtist {
    /// the artist's folder, as a path from the music folder: "Ween", or "Dubstep\Skream" inside a genre folder
    pub folder: String,
    /// the artist's name when it is not the whole of `folder` ("Skream"); empty otherwise
    #[serde(default)]
    pub name: String,
    /// each album's folder as a path from the artist's folder ("Sweden (1995)", "EPs and Singles\[1994]Philyra"),
    /// and its year
    pub albums: Vec<(String, Option<i32>)>,
    pub album_keys: HashSet<String>,
    pub track_keys: HashSet<String>,
    pub tracks: usize,
}

impl LibArtist {
    /// The artist's name as shown.
    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.folder } else { &self.name }
    }
}

pub struct Library {
    /// keyed by artist_key(artist name)
    pub artists: HashMap<String, LibArtist>,
    /// variant spelling key -> canonical artist key
    pub aliases: HashMap<String, String>,
}

const AUDIO: [&str; 7] = ["mp3", "m4a", "flac", "ogg", "opus", "wav", "aac"];

impl Library {
    pub fn scan(music_dir: &str, skip: &str, alias_file: &str) -> Result<Self, String> {
        let skip: HashSet<String> = skip.split(',').map(norm).filter(|s| !s.is_empty()).collect();
        let rd = std::fs::read_dir(music_dir).map_err(|e| crate::util::plain_err(format!("Can't read {music_dir}: {e}")))?;
        let mut artists: HashMap<String, LibArtist> = HashMap::new();
        let mut genres: Vec<String> = vec![];
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let folder = e.file_name().to_string_lossy().to_string();
            if folder.starts_with('.') {
                continue;
            }
            if skip.contains(&norm(&folder)) {
                // a folder listed as "not an artist": a genre folder holds artists, one level down
                if is_genre_folder(&folder) {
                    genres.push(folder);
                }
                continue;
            }
            if let Some(a) = read_artist(Path::new(music_dir), &folder, "") {
                artists.insert(artist_key(&folder), a);
            }
        }
        // the artists inside genre folders, after the ones with a folder of their own
        for genre in genres {
            for e in std::fs::read_dir(Path::new(music_dir).join(&genre)).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) || name.starts_with('.') {
                    continue;
                }
                let Some(a) = read_artist(Path::new(music_dir), &format!("{genre}\\{name}"), &name) else { continue };
                match artists.get_mut(&artist_key(&name)) {
                    // also has a folder of their own: what is here counts as owned, under that artist
                    Some(have) => {
                        have.album_keys.extend(a.album_keys);
                        have.track_keys.extend(a.track_keys);
                        have.tracks += a.tracks;
                    }
                    None => {
                        artists.insert(artist_key(&name), a);
                    }
                }
            }
        }
        Ok(Self { artists, aliases: Self::scan_aliases(alias_file) })
    }

    fn scan_aliases(alias_file: &str) -> HashMap<String, String> {
        let mut aliases = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(alias_file) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') {
                    continue;
                }
                if let Some((from, to)) = line.split_once('=') {
                    let (f, t) = (artist_key(from), artist_key(to));
                    if !f.is_empty() && !t.is_empty() {
                        aliases.insert(f, t);
                    }
                }
            }
        }
        aliases
    }

    /// Canonical key for any artist spelling (applies the alias file).
    pub fn key_for(&self, name: &str) -> String {
        let k = artist_key(name);
        self.aliases.get(&k).cloned().unwrap_or(k)
    }

    /// Is this release already in the library, for this artist or a collab folder that includes them?
    pub fn owns(&self, artist_k: &str, title: &str, is_single: bool) -> bool {
        let key = album_key(title);
        if key.is_empty() {
            return false;
        }
        for (k, a) in &self.artists {
            if k != artist_k && !(k.contains(artist_k) && artist_k.len() >= 6) {
                continue;
            }
            if a.album_keys.contains(&key) {
                return true;
            }
            // one title is the other with edition words after it ("Album Special Limited Edition"), not with
            // more of a name after it ("Fantasy" is not "Fantasy - Chapter 1")
            if key.len() >= 6 && a.album_keys.iter().any(|o| o.len() >= 6 && (edition_tail(o, &key) || edition_tail(&key, o))) {
                return true;
            }
            // a single whose song is already on an album you have
            if is_single && a.track_keys.contains(&key) {
                return true;
            }
        }
        false
    }
}

/// Words that only name an edition, as they stand in a folded key (no spaces, no signs).
const EDITION_WORDS: [&str; 22] = [
    "special", "limited", "deluxe", "edition", "expanded", "remastered", "remaster", "anniversary", "version", "bonus", "tracks", "track",
    "digital", "explicit", "clean", "super", "collectors", "complete", "reissue", "the", "mono", "stereo",
];

/// True when `long` is `short` with nothing after it but edition words ("deluxe", "special edition", "10th
/// anniversary"), both as folded keys.
fn edition_tail(long: &str, short: &str) -> bool {
    let Some(tail) = long.strip_prefix(short) else { return false };
    // the tail must be made wholly of edition words, and of numbers like "10th" or "2019" between them
    fn made_of(t: &str) -> bool {
        if t.is_empty() {
            return true;
        }
        // ("10th" or a year count; a bare "2" is a sequel, not an edition)
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            let rest = &t[digits..];
            return match ["st", "nd", "rd", "th"].iter().find_map(|s| rest.strip_prefix(s)) {
                Some(after) => made_of(after),
                None => digits == 4 && made_of(rest),
            };
        }
        EDITION_WORDS.iter().any(|w| t.strip_prefix(w).is_some_and(made_of))
    }
    !tail.is_empty() && made_of(tail)
}

// ------------------------------------------------------------------ the shape of an artist folder
//
// What a folder inside an artist folder can be:
//   * an album: `Sweden (1995)\tracks`, perhaps with disc folders (`CD1`, `Disc 2 - Live`, `Vol. 1`)
//   * an album whose tracks sit one folder deeper: `DAMN. (2017)\Kendrick Lamar - DAMN. (2017)\tracks`.
//     The outer folder is the album; the inner one is only where its tracks are.
//   * a shelf: `EPs and Singles\[1994]Philyra\tracks`. Not an album: the folders on it are, and a shelf may
//     stand on a shelf (`Live\Bootlegs\1999 Paris`). Tracks lying loose on a shelf are an album named after it.
//   * an album with another release kept inside it: `Feels (2005)\Live Bonus Disc\tracks` beside the album's
//     own tracks. The inner folder is an album of its own.
// And a folder in the music folder that Settings lists as "not an artist" may be a genre folder, whose
// folders are artists (`Dubstep\Skream\...`).
//
// `tests\layout-cases.tsv` holds the cases this rule must agree with.

static SHELF_NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?i)^(the\s+)?(studio\s+|official\s+|other\s+)?(albums?|eps?|e\.p\.s?|singles?|extended plays?|live(\s+albums?)?|compilations?|bootlegs?|demos?",
        r"|mixtapes?|soundtracks?|remix(es)?|b-?sides?|rarities|other|misc(ellaneous)?|discography|collabs?|collaborations?|side projects?|splits?)",
        r"(\s*(and|&|\+|,)\s*(albums?|eps?|singles?|live|compilations?|demos?|mixtapes?|b-?sides?|rarities|remix(es)?))*(\s*[\[\(][^\]\)]*[\]\)])?$",
    ))
    .unwrap()
});
/// a folder that can only be a disc or the artwork of an album
static HARD_DISC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)(^|[\s\[\(_-])(cd|disc|disk|dvd|side)[\s._-]*(\d{1,2}|one|two|three|four|five|six|[ivx]{1,4}|[a-d])\b|^(cd|dvd|covers?|scans?|artwork|booklet|art)$").unwrap()
});
/// a folder that is usually a part of an album ("Vol. 1", "EP 02", "Bonus Tracks"), but on a shelf may be a release
static SOFT_DISC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)(^|[\s\[\(_-])(vol|volume|part|pt|ep|lp)[\s._-]*(\d{1,2}|one|two|three|four|five|six|[ivx]{1,4})\b|^(lp|bonus.*|extras?)$").unwrap()
});
static HAS_YEAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(^|[\[\(\s-])(19|20)\d{2}([\]\)\s-]|$)").unwrap());
/// folders listed as "not an artist" that are not genre folders either
static NOT_A_GENRE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^(various(\s+artists)?|va|v\.a\.?|compilations?|unknown(\s+artist)?|misc(ellaneous)?|soundtracks?|automatically add.*|podcasts?|audiobooks?)$").unwrap());

/// A folder that is one disc, one part or the artwork of an album, by its name.
pub fn is_disc_like(name: &str) -> bool {
    HARD_DISC.is_match(name.trim()) || SOFT_DISC.is_match(name.trim())
}

/// A folder listed in Settings as "not an artist" whose folders are artists.
pub fn is_genre_folder(name: &str) -> bool {
    !NOT_A_GENRE.is_match(name.trim())
}

/// Whether a folder inside an artist folder is a shelf, from what a look inside found: its name, how many
/// tracks sit directly in it, and the names of the folders in it that hold tracks.
///   * a name that says so ("EPs and Singles", "Studio Albums", "Live"): a shelf unless a folder in it can
///     only be a disc ("CD1"). One album is enough, tracks of its own are allowed, and "EP 1" is a release.
///   * any other name: no tracks of its own and nothing disc-like in it, and either three or more folders,
///     or two of which at least half carry a year the way album folders do.
pub fn is_shelf(name: &str, own_tracks: usize, with_tracks: &[String]) -> bool {
    if with_tracks.is_empty() || with_tracks.iter().any(|n| HARD_DISC.is_match(n.trim())) {
        return false;
    }
    if SHELF_NAME.is_match(name.trim()) {
        return true;
    }
    if own_tracks > 0 || with_tracks.iter().any(|n| SOFT_DISC.is_match(n.trim())) {
        return false;
    }
    let dated = with_tracks.iter().filter(|n| HAS_YEAR.is_match(n)).count();
    with_tracks.len() >= 3 || (with_tracks.len() >= 2 && dated * 2 >= with_tracks.len())
}

fn has_tracks(dir: &Path, depth: u32) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { depth < 3 && has_tracks(&p, depth + 1) } else { is_audio(&p) }
    })
}

/// How many tracks sit directly in a folder, and the names of the folders in it that hold tracks.
pub(crate) fn look(dir: &Path) -> (usize, Vec<String>) {
    let (mut own, mut subs) = (0, vec![]);
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".importing") && has_tracks(&p, 0) {
                subs.push(name);
            }
        } else if is_audio(&p) {
            own += 1;
        }
    }
    subs.sort_by_key(|s| s.to_lowercase());
    (own, subs)
}

/// The album folders in (and under) one folder of an artist, as paths from the artist's folder. `rel` is
/// this folder's own path from there.
fn albums_under(dir: &Path, rel: &str, depth: u32, out: &mut Vec<String>) {
    let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (own, subs) = look(dir);
    if own == 0 && subs.is_empty() {
        return;
    }
    if depth < 2 && is_shelf(&name, own, &subs) {
        if own > 0 {
            out.push(rel.to_string()); // the tracks lying loose on the shelf
        }
        for sub in &subs {
            albums_under(&dir.join(sub), &format!("{rel}\\{sub}"), depth + 1, out);
        }
        return;
    }
    out.push(rel.to_string());
    // another release kept inside this album's folder, beside the album's own tracks
    if own > 0 {
        out.extend(subs.iter().filter(|s| !is_disc_like(s)).map(|s| format!("{rel}\\{s}")));
    }
}

/// Every album folder of an artist, as paths from the artist's folder.
pub fn album_paths(artist_dir: &Path) -> Vec<String> {
    let mut out = vec![];
    let mut dirs: Vec<String> = std::fs::read_dir(artist_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.ends_with(".importing"))
        .collect();
    dirs.sort_by_key(|s| s.to_lowercase());
    for name in dirs {
        albums_under(&artist_dir.join(&name), &name, 0, &mut out);
    }
    out
}

/// If this folder (one inside an artist folder) is a shelf, the names of the folders on it that hold tracks.
pub fn shelf_albums(dir: &Path) -> Option<Vec<String>> {
    let name = dir.file_name()?.to_string_lossy().to_string();
    let (own, subs) = look(dir);
    is_shelf(&name, own, &subs).then_some(subs)
}

/// One artist's folder. None if there is no music in it.
fn read_artist(music_dir: &Path, folder: &str, name: &str) -> Option<LibArtist> {
    let dir = music_dir.join(folder);
    let mut a = LibArtist { folder: folder.to_string(), name: name.to_string(), albums: vec![], album_keys: HashSet::new(), track_keys: HashSet::new(), tracks: 0 };
    let albums = album_paths(&dir);
    let all: HashSet<String> = albums.iter().map(|r| r.to_lowercase()).collect();
    for rel in albums {
        let leaf = rel.rsplit('\\').next().unwrap_or(&rel).to_string();
        a.album_keys.insert(album_key(&leaf));
        a.albums.push((rel.clone(), folder_year(&leaf)));
        count_tracks(&dir.join(&rel), &rel, &all, &mut a, 0);
    }
    // loose tracks directly in the artist folder
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if is_audio(&e.path()) {
            a.tracks += 1;
            if let Some(stem) = e.path().file_stem() {
                a.track_keys.insert(track_key_from_file(&stem.to_string_lossy()));
            }
        }
    }
    (a.tracks > 0 || !a.albums.is_empty()).then_some(a)
}

fn is_audio(p: &Path) -> bool {
    p.extension().map(|e| AUDIO.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

/// Counts an album's tracks: its own and its discs', but not those of an album kept inside it (`albums`
/// holds every album path of the artist, lowercase).
fn count_tracks(dir: &Path, rel: &str, albums: &HashSet<String>, a: &mut LibArtist, depth: u32) {
    if depth > 3 {
        return;
    }
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let sub = format!("{rel}\\{}", e.file_name().to_string_lossy());
            if !albums.contains(&sub.to_lowercase()) {
                count_tracks(&p, &sub, albums, a, depth + 1); // "CD1", "Disc 2", or the one folder the tracks are in
            }
        } else if is_audio(&p) {
            a.tracks += 1;
            if let Some(stem) = p.file_stem() {
                a.track_keys.insert(track_key_from_file(&stem.to_string_lossy()));
            }
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn shelves_and_albums_are_told_apart_as_the_shared_cases_say() {
        let mut n = 0;
        for line in include_str!("../tests/layout-cases.tsv").lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let parts: Vec<&str> = line.split('\t').collect();
            assert!(parts.len() >= 3, "a case needs four parts: {line}");
            let subs: Vec<String> = parts.get(3).unwrap_or(&"").split('|').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
            let got = if is_shelf(parts[1], parts[2].parse().unwrap(), &subs) { "shelf" } else { "album" };
            assert_eq!(got, parts[0], "{line}");
            n += 1;
        }
        assert!(n >= 20);
    }

    #[test]
    fn genre_folders_are_told_from_other_folders_that_are_not_artists() {
        for genre in ["Dubstep", "Jazz", "Drum & Bass"] {
            assert!(is_genre_folder(genre), "{genre}");
        }
        for other in ["Various Artists", "Compilations", "Unknown Artist", "Misc", "Automatically Add to Apple Music", "VA"] {
            assert!(!is_genre_folder(other), "{other}");
        }
    }

    #[test]
    fn an_artists_albums_are_found_whatever_shape_the_folder_has() {
        let dir = std::env::temp_dir().join(format!("music-recs-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in [
            // a shelf, with a shelf on it, an album with discs, and a track lying loose on the shelf
            r"A\EPs and Singles\[1994]Philyra\01 a.mp3",
            r"A\EPs and Singles\[1995]Songs About Fire\CD1\01 b.mp3",
            r"A\EPs and Singles\Live\Paris 1999\01 c.mp3",
            r"A\EPs and Singles\stray single.mp3",
            r"A\EPs and Singles\notes\readme.txt",
            // a plain album with discs
            r"A\Sweden (1995)\CD1\01.mp3",
            r"A\Sweden (1995)\CD2\01.mp3",
            // an album whose tracks are one folder deeper
            r"A\DAMN (2017)\A - DAMN (2017) [320]\01.mp3",
            // an album with another release inside it
            r"A\Feels (2005)\01.mp3",
            r"A\Feels (2005)\Live Bonus Disc\01 live.mp3",
            r"A\Feels (2005)\Bonus Tracks\13 extra.mp3",
            r"A\loose.mp3",
            // a genre folder: artists inside, one of whom also has a folder of their own
            r"Dubstep\Skream\Skream! (2006)\01.mp3",
            r"Dubstep\Skream\loose tune.mp3",
            r"Dubstep\A\Only Here (2011)\01.mp3",
            r"Various Artists\Some Comp\01.mp3",
        ] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let lib = Library::scan(&dir.to_string_lossy(), "Dubstep, Various Artists", "").unwrap();
        let a = &lib.artists[&artist_key("A")];
        let mut albums: Vec<&str> = a.albums.iter().map(|x| x.0.as_str()).collect();
        albums.sort();
        assert_eq!(
            albums,
            vec![
                "DAMN (2017)",
                "EPs and Singles",
                r"EPs and Singles\Live\Paris 1999",
                r"EPs and Singles\[1994]Philyra",
                r"EPs and Singles\[1995]Songs About Fire",
                "Feels (2005)",
                r"Feels (2005)\Live Bonus Disc",
                "Sweden (1995)",
            ]
        );
        // every track once: 4 on the shelf, 2 + 1 + 2 + 1 in the albums, 1 loose, and the one in the genre folder
        assert_eq!(a.tracks, 12);
        for owned in ["Philyra", "Songs About Fire", "Paris 1999", "Feels", "Live Bonus Disc", "DAMN", "Only Here"] {
            assert!(lib.owns(&artist_key("A"), owned, false), "{owned}");
        }
        assert_eq!(a.albums.iter().find(|x| x.0.ends_with("Philyra")).unwrap().1, Some(1994));
        // the artist who only lives in the genre folder
        let s = &lib.artists[&artist_key("Skream")];
        assert_eq!((s.folder.as_str(), s.display(), s.tracks), (r"Dubstep\Skream", "Skream", 2));
        assert_eq!(s.albums.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["Skream! (2006)"]);
        assert!(!lib.artists.contains_key(&artist_key("Dubstep")) && !lib.artists.contains_key(&artist_key("Various Artists")));
        assert_eq!(lib.artists.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_title_is_owned_with_edition_words_after_it_but_not_with_more_of_a_name() {
        let mut a = LibArtist { folder: "M83".into(), name: String::new(), albums: vec![], album_keys: HashSet::new(), track_keys: HashSet::new(), tracks: 0 };
        for folder in ["Fantasy (2023)", "Dead Cities, Red Seas and Lost Ghosts (2003)", "Hurry Up Special Limited Edition"] {
            a.album_keys.insert(album_key(&crate::importer::util::strip_year(folder)));
        }
        let lib = Library { artists: HashMap::from([(artist_key("M83"), a)]), aliases: HashMap::new() };
        let owns = |t: &str| lib.owns(&artist_key("M83"), t, false);
        assert!(owns("Fantasy"));
        assert!(owns("Dead Cities, Red Seas & Lost Ghosts"));
        // edition words after the title, either way round
        assert!(owns("Fantasy Deluxe Edition") && owns("Fantasy 10th Anniversary") && owns("Fantasy 2024 Remaster"));
        assert!(owns("Hurry Up"));
        // more of a name after it is another release
        assert!(!owns("Fantasy - Chapter 1"));
        assert!(!owns("Fantasy 2"));
        assert!(!owns("Fantasyland"));
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    saved: String,
    music_dir: String,
    artists: HashMap<String, LibArtist>,
}

fn snapshot_path() -> std::path::PathBuf {
    crate::config::app_dir().join("library-snapshot.json")
}

impl Library {
    /// Scan the music folder. If it's missing or empty (drive unplugged, NAS asleep), fall back to the
    /// last good scan so "already owned" checks still work. Returns a note when the fallback was used.
    pub fn scan_or_snapshot(music_dir: &str, skip: &str, alias_file: &str) -> Result<(Self, Option<String>), String> {
        let scanned = Self::scan(music_dir, skip, alias_file);
        match scanned {
            Ok(lib) if !lib.artists.is_empty() => {
                let snap = Snapshot {
                    saved: chrono::Local::now().format("%b %-d, %Y").to_string(),
                    music_dir: music_dir.to_string(),
                    artists: lib.artists,
                };
                if let Ok(b) = serde_json::to_vec(&snap) {
                    let _ = crate::util::write_atomic(&snapshot_path(), &b);
                }
                Ok((Self { artists: snap.artists, aliases: lib.aliases }, None))
            }
            other => {
                let why = match other {
                    Err(e) => e,
                    Ok(_) => format!("{music_dir} has no artist folders"),
                };
                let snap: Snapshot = std::fs::read(snapshot_path())
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .ok_or_else(|| format!("{why}. Is the drive connected? (No earlier scan to fall back on.)"))?;
                let aliases = Self::scan_aliases(alias_file);
                let note = format!("{why}. Using your library as it was on {} ({} artists). Is the drive connected?", snap.saved, snap.artists.len());
                Ok((Self { artists: snap.artists, aliases }, Some(note)))
            }
        }
    }
}
