//! Organize into Artist \ Album (Year): every album folder moved to `<library>\<Artist>\<Album (Year)>`, as
//! its tags say, by the person's choice and only after a preview (ENGINE-PROTOCOL.md, "Organize into
//! Artist \ Album (Year)").
//!
//! * `preview` reads the library and the tags and changes nothing. It says where each album would go, and
//!   which albums stay where they are and why.
//! * `run` moves the albums of a preview, one folder rename each, as one undo batch of kind "organize". For
//!   each album a "moved" line is written to the batch's log and flushed to the disk before the rename, and a
//!   "done" line after it, so `recover` can always tell where an album is after a crash. Repair history's
//!   Undo (`repair::undo` -> `undo::undo`) reads the same log backwards and moves each folder back.
//! * Renames only: `MoveFileExW` without `MOVEFILE_COPY_ALLOWED`, so a move is on one drive, never a copy and
//!   a delete, and no file is opened for writing. Nothing is ever moved onto something that is there.
//!
//! The service layer (the guard, the hold, `library.moved`, the waiting for a playing album) is around this.
use crate::importer::util::{clean_name, most_common};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// What Repair history calls an organize batch.
pub const TITLE: &str = "Organize into Artist \\ Album (Year)";
/// The batch kind in batch.json.
pub const KIND: &str = "organize";
/// Paths longer than this break many programs (the importer's own limit).
const MAX_PATH: usize = 240;
/// The audio files a folder can hold (Library Health's list). Everything else in an album's folder is a
/// sidecar and travels with it.
const AUDIO_EXT: [&str; 15] = ["mp3", "m4a", "flac", "wav", "aif", "aiff", "ogg", "opus", "wma", "aac", "ape", "wv", "tta", "dsf", "dff"];

static DISC_ONLY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^\s*(cd|disc|disk)[\s._-]*(\d{1,2}|one|two|three|four|[ivx]{1,4})\b").unwrap());
static DISC_NOTE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\s*[\(\[]\s*(cd|disc|disk)[\s._-]*\d{1,2}(\s*(of|/)\s*\d{1,2})?\s*[\)\]]\s*$|\s+-?\s*(cd|disc|disk)[\s._-]*\d{1,2}$").unwrap());
static VARIOUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^\s*(various(\s+artists?)?|va|v\.\s?a\.?|various\s+performers)\s*$").unwrap());
static YEAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(\d{4})").unwrap());

// ------------------------------------------------------------------ what a preview says

/// The tags of one track that decide where its album goes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackTags {
    pub album_artist: String,
    pub artist: String,
    pub album: String,
    /// the date or year tag as written ("2007", "2007-10-10")
    pub date: String,
    /// the compilation flag is set
    pub compilation: bool,
}

impl TrackTags {
    /// From tags as ffprobe names them (lowercase keys), the way Library Health reads them.
    pub fn from_map(tags: &std::collections::BTreeMap<String, String>) -> TrackTags {
        let get = |keys: &[&str]| keys.iter().filter_map(|k| tags.get(*k)).map(|v| v.trim()).find(|v| !v.is_empty()).unwrap_or("").to_string();
        let flag = get(&["compilation", "cpil", "tcmp"]);
        TrackTags {
            album_artist: get(&["album_artist", "albumartist", "album artist", "tpe2"]),
            artist: get(&["artist", "tpe1"]),
            album: get(&["album", "talb"]),
            date: get(&["date", "year", "originaldate", "tdrc", "tyer"]),
            compilation: matches!(flag.to_lowercase().as_str(), "1" | "true" | "yes"),
        }
    }
}

/// One album that would move.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
    /// its audio files
    pub files: usize,
    /// every other file it carries, as paths from its folder ("cover.jpg", "CD1\\rip.log")
    pub sidecars: Vec<String>,
    /// the folder's listing as it was at the preview (names, sizes, modified times), as one fingerprint
    pub signature: String,
    /// "Radiohead \\ In Rainbows (2007)", for progress
    pub words: String,
    /// what kind of move it is, so Cadence can group them: any of "artist" (the artist folder changes),
    /// "year" (the folder had a year and the tags' year differs), "renamed" (the folder's name and the album
    /// tag share no words), or "tidy" alone (only the way the name is written changes)
    #[serde(default)]
    pub changes: Vec<String>,
}

/// An album (or a folder of loose tracks) that stays where it is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Left {
    pub folder: PathBuf,
    /// `missing_tags`, `mixed_artists`, `loose_tracks`, `two_albums_one_folder`, `removed`,
    /// `read_only`, `open`, `target_exists`, `collision`, `too_long`, `changed`; and in a run also `playing`
    /// and `failed` (Windows would not do the rename)
    pub why: String,
    pub words: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Preview {
    /// "p-20261118-091200"
    pub preview: String,
    pub library: PathBuf,
    pub to_move: usize,
    pub in_place: usize,
    pub left: usize,
    /// renames only: a backup needs no room for them
    pub backup_extra_bytes: u64,
    pub moves: Vec<Move>,
    pub left_alone: Vec<Left>,
    /// how many moves carry each mark of `Move::changes`
    #[serde(default)]
    pub counts: Counts,
}

/// How many moves of a preview carry each mark (`Move::changes`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub artist: usize,
    pub renamed: usize,
    pub tidy: usize,
}

/// What a preview is told besides the library.
#[derive(Clone, Debug, Default)]
pub struct PreviewOptions {
    /// tracks removed from Cadence: an album holding any of them stays where it is
    pub hidden: Vec<PathBuf>,
    /// the Settings list of folders that are not artists (comma separated); a genre folder among them holds
    /// artist folders
    pub skip_folders: String,
    /// the file the player has loaded: not counted as "open in another program" (the run waits for it)
    pub playing: Option<PathBuf>,
    /// Settings' folder for compilations (`compilations_folder`; empty: "Various Artists")
    pub compilations: String,
    /// only this album folder is looked at (Keep the library organized, one new album); the rest of the
    /// library is only listed, to know its artist and album folders
    pub only: Option<PathBuf>,
}

fn words_for(why: &str) -> &'static str {
    match why {
        "missing_tags" => "Some tracks have no artist or album in their tags, so it is not clear where the album belongs.",
        "mixed_artists" => "Its tracks name different artists and no one album artist.",
        "loose_tracks" => "Tracks lying loose in this folder are not in an album folder, so they stay where they are.",
        "two_albums_one_folder" => "This folder holds more than one album.",
        "removed" => "Tracks of this album were removed from Cadence.",
        "read_only" => "Read-only: Cadence leaves read-only files alone.",
        "open" => "Open in another program.",
        "target_exists" => "Something is already where it would go.",
        "collision" => "Another album would go to the same place, so neither moves.",
        "too_long" => "Its new place would make a path too long for some programs.",
        "changed" => "It changed after the preview.",
        "playing" => "It was still playing.",
        _ => "Windows would not move it.",
    }
}

fn left(folder: &Path, why: &str) -> Left {
    Left { folder: folder.to_path_buf(), why: why.into(), words: words_for(why).into() }
}

fn left_with(folder: &Path, why: &str, words: String) -> Left {
    Left { folder: folder.to_path_buf(), why: why.into(), words }
}

// ------------------------------------------------------------------ small path helpers

fn lower(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase()
}

fn same_ci(a: &Path, b: &Path) -> bool {
    lower(a) == lower(b)
}

/// `p` is `dir` or inside it, ignoring case.
fn under_ci(p: &Path, dir: &Path) -> bool {
    let (p, d) = (lower(p), lower(dir));
    p == d || (p.starts_with(&d) && p[d.len()..].starts_with('\\'))
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn is_audio(p: &Path) -> bool {
    p.extension().is_some_and(|e| AUDIO_EXT.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

fn utf16_len(p: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().count()
}

/// Something by exactly this name (case and all) is in `p`'s folder. On Windows `exists` ignores case, which
/// cannot tell an album's old name from its new one after a case-only rename.
fn exists_exact(p: &Path) -> bool {
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return p.exists() };
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| e.file_name() == name)
}

fn hidden_or_system(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x6 != 0
}

fn is_reparse(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0
}

fn is_read_only(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x1 != 0
}

/// Another program has the file open (any handle at all): opening it with no sharing fails. Read access
/// only: no file is ever opened for writing.
fn open_elsewhere(file: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).share_mode(0).open(file).is_err()
}

fn rand_hex(digits: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let n = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{n:016x}")[..digits.min(16)].to_string()
}

// ------------------------------------------------------------------ an album's folder, read

/// One file or folder inside an album's folder.
struct Entry {
    rel: String,
    path: PathBuf,
    dir: bool,
    len: u64,
    modified: u128,
    read_only: bool,
}

/// Everything inside a folder, every level down (a junction is listed, never walked into).
fn listing(dir: &Path) -> Option<Vec<Entry>> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<Entry>) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            let name = e.file_name().to_string_lossy().to_string();
            let r = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
            let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            let isdir = m.is_dir();
            out.push(Entry { rel: r.clone(), path: e.path(), dir: isdir, len: if isdir { 0 } else { m.len() }, modified: if isdir { 0 } else { modified }, read_only: !isdir && is_read_only(&m) });
            if isdir && !is_reparse(&m) && !walk(&e.path(), &r, out) {
                return false;
            }
        }
        true
    }
    let mut out = vec![];
    walk(dir, "", &mut out).then(|| {
        out.sort_by(|a, b| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()));
        out
    })
}

/// The listing as one fingerprint: names, sizes and modified times. A tag written, a file added or taken
/// away, or a file renamed changes it.
fn signature(entries: &[Entry]) -> String {
    let mut text = String::new();
    for e in entries {
        text.push_str(&format!("{}\t{}\t{}\t{}\n", e.rel, e.dir as u8, e.len, e.modified));
    }
    format!("{:x}", md5::compute(text.as_bytes()))
}

/// The folder's fingerprint now; None when it cannot be read whole.
pub fn signature_of(dir: &Path) -> Option<String> {
    listing(dir).map(|l| signature(&l))
}

// ------------------------------------------------------------------ which folders are albums

/// The album folders in the library, and the folders holding loose tracks. Uses the app's own reading of an
/// artist folder (`library::album_paths`: shelves, disc folders, tracks one folder deeper) so Organize and
/// the library agree on what an album is.
fn find_albums(library: &Path, skip_folders: &str) -> (Vec<PathBuf>, Vec<PathBuf>, HashMap<String, PathBuf>) {
    let skip: HashSet<String> = skip_folders.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    let (mut albums, mut loose) = (vec![], vec![]);
    // the artist folders there are, by lowercase name: a new album goes into the one that is there
    let mut artists: HashMap<String, PathBuf> = HashMap::new();
    let tops = top_folders(library);
    if std::fs::read_dir(library).into_iter().flatten().flatten().any(|e| e.file_type().is_ok_and(|t| t.is_file()) && is_audio(&e.path())) {
        loose.push(library.to_path_buf());
    }
    let mut genre_children = vec![];
    for top in tops {
        let name = leaf(&top).to_lowercase();
        if skip.contains(&name) && crate::library::is_genre_folder(&leaf(&top)) {
            let (own, _) = crate::library::look(&top);
            if own > 0 {
                loose.push(top.clone());
            }
            for child in top_folders(&top) {
                genre_children.extend(artist_or_album(&child, &mut albums, &mut loose));
            }
            continue;
        }
        if let Some(a) = artist_or_album(&top, &mut albums, &mut loose) {
            artists.insert(leaf(&a).to_lowercase(), a);
        }
    }
    // (an artist inside a genre folder is used only when there is no folder of that name at the top)
    for child in genre_children {
        artists.entry(leaf(&child).to_lowercase()).or_insert(child);
    }
    (albums, loose, artists)
}

/// The folders in a folder that can hold music: not hidden or system ones, not "." ones (the undo backups),
/// not an import in progress, and not a junction.
fn top_folders(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { return false };
            m.is_dir() && !is_reparse(&m) && !hidden_or_system(&m) && !name.starts_with('.') && !name.ends_with(".importing") && !name.contains(".replaced-") && !name.contains(".organizing-") && !name.eq_ignore_ascii_case("Automatically Add to Apple Music")
        })
        .map(|e| e.path())
        .collect();
    out.sort_by_key(|p| lower(p));
    out
}

/// A folder at the top of the library (or of a genre folder) is an album of its own ("radiohead - in
/// rainbows", perhaps with CD1 and CD2 in it), or an artist's folder. Returns the artist's folder (an empty
/// folder too: an album of that artist goes into it, under the name it has).
fn artist_or_album(dir: &Path, albums: &mut Vec<PathBuf>, loose: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let (own, subs) = crate::library::look(dir);
    if own == 0 && subs.is_empty() {
        return Some(dir.to_path_buf());
    }
    let album = (own > 0 && subs.iter().all(|s| crate::library::is_disc_like(s))) || (own == 0 && subs.iter().all(|s| DISC_ONLY.is_match(s)));
    if album {
        albums.push(dir.to_path_buf());
        return None;
    }
    if own > 0 {
        loose.push(dir.to_path_buf());
    }
    albums.extend(crate::library::album_paths(dir).into_iter().map(|rel| dir.join(rel)).filter(|p| !leaf(p).contains(".organizing-")));
    Some(dir.to_path_buf())
}

// ------------------------------------------------------------------ the preview

struct Album {
    dir: PathBuf,
    entries: Vec<Entry>,
}

/// The folder compilations go into: Settings' name for it, cleaned by the Windows rules; "Various Artists"
/// when it is empty or cannot be a folder name. The importer uses the same.
pub fn compilations_name(setting: &str) -> String {
    let name = clean_name(setting);
    if crate::importer::import::is_folder_name(&name) { name } else { DEFAULT_COMPILATIONS.to_string() }
}

/// The compilations folder Settings starts with.
pub const DEFAULT_COMPILATIONS: &str = "Various Artists";

/// Where an album's tags say it belongs: (artist folder, album folder name), or why it stays. An album
/// marked as a compilation (the compilation tag, or "Various Artists" as its album artist) goes into
/// `compilations`, the folder Settings names for them.
fn target_names(tags: &[Option<TrackTags>], compilations: &str) -> Result<(String, String), &'static str> {
    if tags.is_empty() || tags.iter().any(|t| t.as_ref().is_none_or(|t| t.album.trim().is_empty())) {
        return Err("missing_tags");
    }
    let tags: Vec<&TrackTags> = tags.iter().flatten().collect();
    // marked as a compilation: the tag, or "Various Artists" as the album artist (or as the artist, on a
    // track with no album artist). An album of several artists that is not marked stays (`mixed_artists`).
    let marked = tags.iter().any(|t| t.compilation || VARIOUS.is_match(&t.album_artist) || (t.album_artist.is_empty() && VARIOUS.is_match(&t.artist)));
    // one album: the same title on every track, a disc note ("(Disc 2)") aside
    let titles: Vec<String> = tags.iter().map(|t| DISC_NOTE.replace(t.album.trim(), "").trim().to_string()).collect();
    let keys: HashSet<String> = titles.iter().map(|t| t.to_lowercase()).collect();
    if keys.len() > 1 {
        return Err("two_albums_one_folder");
    }
    let distinct = |vals: Vec<&str>| -> HashSet<String> { vals.into_iter().map(|v| v.trim().to_lowercase()).filter(|v| !v.is_empty()).collect() };
    // the album artist, else the one artist every track shares
    let album_artists = distinct(tags.iter().map(|t| t.album_artist.as_str()).collect());
    let artist = match album_artists.len() {
        _ if marked => compilations_name(compilations),
        1 => most_common(tags.iter().map(|t| t.album_artist.trim().to_string())).unwrap_or_default(),
        0 => {
            if tags.iter().any(|t| t.artist.trim().is_empty()) {
                return Err("missing_tags");
            }
            if distinct(tags.iter().map(|t| t.artist.as_str()).collect()).len() != 1 {
                return Err("mixed_artists");
            }
            most_common(tags.iter().map(|t| t.artist.trim().to_string())).unwrap_or_default()
        }
        _ => return Err("mixed_artists"),
    };
    let album = most_common(titles).unwrap_or_default();
    let year = most_common(tags.iter().filter_map(|t| YEAR.captures(&t.date).map(|c| c[1].to_string()))).unwrap_or_default();
    // the importer's rules: cleaned names, and "Album (Year)", or just "Album" with no year
    let (artist, album) = (clean_name(&artist), clean_name(&album));
    let folder = if !year.is_empty() && !album.ends_with(&format!("({year})")) { format!("{album} ({year})") } else { album };
    if !crate::importer::import::is_folder_name(&artist) || !crate::importer::import::is_folder_name(&folder) {
        return Err("missing_tags");
    }
    Ok((artist, folder))
}

/// The year the tags give an album ("" when none does), as `target_names` reads it.
fn tag_year(tags: &[Option<TrackTags>]) -> String {
    most_common(tags.iter().flatten().filter_map(|t| YEAR.captures(&t.date).map(|c| c[1].to_string()))).unwrap_or_default()
}

/// The album's title as the tags give it (a disc note aside), as `target_names` reads it.
fn tag_album(tags: &[Option<TrackTags>]) -> String {
    most_common(tags.iter().flatten().map(|t| DISC_NOTE.replace(t.album.trim(), "").trim().to_string())).unwrap_or_default()
}

static GROUP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\(([^()\[\]{}]*)\)|\[([^()\[\]{}]*)\]|\{([^()\[\]{}]*)\}").unwrap());
static NUMBER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\d+(k|kbps|bit|khz)?$").unwrap());
static FOLDER_YEAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\b(19\d\d|20\d\d)\b").unwrap());
/// A year on its own in brackets or parentheses in a folder's name: "(1971)", "[1999] X", "X [2004]".
static BRACKET_YEAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[\(\[]\s*((?:19|20)\d\d)\s*[\)\]]").unwrap());

/// The year an album folder's name already gives, if any. It wins over the tags' year: the tags of a
/// reissue often carry the reissue's year ("Journey in Satchidananda (1971)" tagged 2009).
fn folder_year(name: &str) -> Option<String> {
    BRACKET_YEAR.captures(name).map(|c| c[1].to_string())
}

/// The album folder's name with `year` in place of the tags' year (`tag_year`) that `target_names` used.
fn with_year(folder: &str, tag_year: &str, year: &str) -> String {
    let base = if tag_year.is_empty() { folder } else { folder.strip_suffix(&format!(" ({tag_year})")).unwrap_or(folder) };
    if base.ends_with(&format!("({year})")) { base.to_string() } else { format!("{base} ({year})") }
}

/// Words for comparing names: lowercase, letters and digits only, one space between words.
fn words_of(text: &str) -> String {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// A bracketed note that only says how the files were made ("[FLAC]", "(320 kbps)", "[24bit-96kHz]",
/// "[WEB]"), or a bare number or year ("[2003]", which becomes the "(Year)"): not the album's own words.
fn format_note(inside: &str) -> bool {
    const WORDS: [&str; 26] = ["flac", "mp3", "aac", "alac", "ogg", "opus", "wav", "m4a", "wma", "v0", "v1", "v2", "cbr", "vbr", "kbps", "khz", "bit", "web", "webrip", "cd", "cdrip", "lossless", "hires", "hi", "res", "vinyl"];
    let words: Vec<String> = words_of(inside).split(' ').filter(|w| !w.is_empty()).map(str::to_string).collect();
    words.is_empty() || words.iter().all(|w| WORDS.contains(&w.as_str()) || NUMBER.is_match(w)) || DISC_ONLY.is_match(inside)
}

/// The bracketed or parenthesized words of an album folder's name that its album tag does not have, as
/// written ("(UK Version)", "[Deluxe Edition]", "(Remastered 2004)"), each once: carried over into its new
/// name. A bracketed year, a disc note and a note of the file format are left out.
fn own_words(folder: &str, album: &str) -> Vec<String> {
    let have = words_of(album);
    let mut out: Vec<String> = vec![];
    for c in GROUP.captures_iter(folder) {
        let inside = c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)).map(|m| m.as_str()).unwrap_or("");
        let w = words_of(inside);
        if format_note(inside) || (!have.is_empty() && format!(" {have} ").contains(&format!(" {w} "))) || out.iter().any(|o| words_of(o) == w) {
            continue;
        }
        let text = c.get(0).map(|m| m.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        // (curly braces are brackets too, written as parentheses: "{Target Deluxe Edition}")
        let text = if c.get(3).is_some() { format!("({})", inside.split_whitespace().collect::<Vec<_>>().join(" ")) } else { text };
        out.push(clean_name(&text));
    }
    // edition words outside brackets ("Trouble (2004) DELUXE EDITION"), put in parentheses
    for run in edition_runs(&GROUP.replace_all(folder, " ")) {
        let w = words_of(&run);
        if (!have.is_empty() && format!(" {have} ").contains(&format!(" {w} "))) || out.iter().any(|o| words_of(o) == w) {
            continue;
        }
        out.push(clean_name(&format!("({run})")));
    }
    out
}

static ORDINAL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\d+(st|nd|rd|th)$").unwrap());

/// A word that says which edition an album is ("deluxe", "remastered", "anniversary", ...).
fn edition_word(w: &str) -> bool {
    const WORDS: [&str; 16] = ["deluxe", "edition", "expanded", "anniversary", "special", "limited", "bonus", "collector's", "collectors", "reissue", "version", "explicit", "clean", "super", "ultimate", "definitive"];
    WORDS.contains(&w) || w.starts_with("remaster")
}

/// The runs of edition words in a folder's name outside brackets, each with the words joined to it
/// ("DELUXE EDITION", "20th Anniversary Edition"), as written, ALL CAPS made Title Case: "Deluxe Edition".
fn edition_runs(rest: &str) -> Vec<String> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let trim = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_string();
    let key = |w: &str| trim(w).to_lowercase().replace('\u{2019}', "'");
    let joins = |w: &str| edition_word(&key(w)) || ORDINAL.is_match(&key(w));
    let title = |w: String| {
        if w.chars().any(|c| c.is_alphabetic()) && !w.chars().any(|c| c.is_lowercase()) {
            let lower = w.to_lowercase();
            let mut c = lower.chars();
            match c.next() {
                Some(f) if f.is_alphabetic() => f.to_uppercase().chain(c).collect(),
                _ => lower,
            }
        } else {
            w
        }
    };
    let (mut out, mut i) = (vec![], 0);
    while i < words.len() {
        if !edition_word(&key(words[i])) {
            i += 1;
            continue;
        }
        let (mut start, mut end) = (i, i);
        while start > 0 && joins(words[start - 1]) {
            start -= 1;
        }
        while end + 1 < words.len() && joins(words[end + 1]) {
            end += 1;
        }
        let run: Vec<String> = words[start..=end].iter().map(|w| title(trim(w))).filter(|w| !w.is_empty()).collect();
        if !run.is_empty() {
            out.push(run.join(" "));
        }
        i = end + 1;
    }
    out
}

/// The album folder's new name with its own words carried over, before the "(Year)" when there is one:
/// "Burials (2013)" with "(Best Buy Exclusive)" is "Burials (Best Buy Exclusive) (2013)".
fn with_own_words(folder: &str, year: &str, own: &[String]) -> String {
    if own.is_empty() {
        return folder.to_string();
    }
    let extra = own.join(" ");
    let tail = format!(" ({year})");
    let name = match folder.strip_suffix(&tail) {
        Some(head) if !year.is_empty() => format!("{head} {extra}{tail}"),
        _ => format!("{folder} {extra}"),
    };
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A path's parts below the library, each as its words (case, punctuation and spacing aside).
fn word_parts(library: &Path, p: &Path) -> Vec<String> {
    p.strip_prefix(library).unwrap_or(p).components().map(|c| words_of(&c.as_os_str().to_string_lossy())).collect()
}

/// Where an album folder named `folder` goes in `artist_dir`: a long name is shortened the importer's way,
/// leaving room for the track names.
pub(crate) fn place_for(artist_dir: &Path, folder: &str) -> PathBuf {
    let room = MAX_PATH.saturating_sub(70 + artist_dir.to_string_lossy().chars().count()).clamp(20, 120);
    artist_dir.join(crate::importer::import::shorten_name(folder, room, true))
}

/// The album is already in place when its folder differs from where it would go only in letter case,
/// punctuation or spacing ("1000 gecs" and "1000 Gecs"; "B.O.A.T.S. II Me Time" and "B.O.A.T.S. II - Me
/// Time"). Compared below the music folder, part by part.
fn already_in_place(library: &Path, from: &Path, to: &Path) -> bool {
    word_parts(library, from) == word_parts(library, to)
}

/// The marks of one move (`Move::changes`).
fn changes_of(library: &Path, from: &Path, to: &Path, album: &str, year: &str) -> Vec<String> {
    let mut out = vec![];
    // the artist folder: the folder it goes into, among the folders it is in now (or, for an album lying at
    // the top, in its own folder's name: "radiohead - in rainbows")
    let artist = to.parent().map(|a| words_of(&leaf(a))).unwrap_or_default();
    let parts = word_parts(library, from);
    let (own, above) = parts.split_last().map(|(l, a)| (l.clone(), a.to_vec())).unwrap_or_default();
    let in_own = !artist.is_empty() && format!(" {own} ").contains(&format!(" {artist} "));
    if !above.contains(&artist) && !(above.is_empty() && in_own) {
        out.push("artist".to_string());
    }
    let name = leaf(from);
    // (no "year" mark: a year the folder's name gives is kept, so a move never changes it)
    let _ = year;
    const SMALL: [&str; 7] = ["the", "a", "an", "of", "and", "de", "la"];
    let tag: HashSet<String> = words_of(album).split(' ').filter(|w| !w.is_empty() && !SMALL.contains(w)).map(str::to_string).collect();
    let had: HashSet<String> = words_of(&name).split(' ').filter(|w| !w.is_empty() && !SMALL.contains(w) && !FOLDER_YEAR.is_match(w)).map(str::to_string).collect();
    if !tag.is_empty() && tag.is_disjoint(&had) {
        out.push("renamed".to_string());
    }
    if out.is_empty() {
        out.push("tidy".to_string());
    }
    out
}

/// Reads tags for `files` on several threads. `read` is the app's tag reading (`app_tag_reader`), or a
/// stand-in in tests.
fn read_all(files: &[PathBuf], read: &(dyn Fn(&Path) -> Option<TrackTags> + Sync), progress: &(dyn Fn(usize, usize) + Sync)) -> Vec<Option<TrackTags>> {
    let out: Vec<std::sync::OnceLock<Option<TrackTags>>> = files.iter().map(|_| std::sync::OnceLock::new()).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16).min(files.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= files.len() {
                    break;
                }
                let _ = out[i].set(read(&files[i]));
                progress(i + 1, files.len());
            });
        }
    });
    out.into_iter().map(|c| c.into_inner().flatten()).collect()
}

/// What Organize would do. Reads the library and the tags; changes nothing (no file is opened for writing,
/// nothing is created). `progress(done, total)` follows the tag reading.
pub fn preview(library: &Path, opts: &PreviewOptions, read_tags: &(dyn Fn(&Path) -> Option<TrackTags> + Sync), progress: &(dyn Fn(usize, usize) + Sync)) -> Result<Preview, String> {
    if !library.is_dir() {
        return Err(format!("The music folder {} is not there.", library.display()));
    }
    let (dirs, loose, artists) = find_albums(library, &opts.skip_folders);
    let mut out = Preview { preview: format!("p-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")), library: library.to_path_buf(), ..Default::default() };
    for l in loose.iter().filter(|_| opts.only.is_none()) {
        out.left_alone.push(left(l, "loose_tracks"));
    }
    // an album folder holding another album: neither is moved (one would carry the other with it)
    let all: HashSet<String> = dirs.iter().map(|d| lower(d)).collect();
    let mut nested: HashSet<String> = HashSet::new();
    for d in &dirs {
        for up in d.ancestors().skip(1).take_while(|u| under_ci(u, library) && !same_ci(u, library)) {
            if all.contains(&lower(up)) {
                nested.insert(lower(up));
                nested.insert(lower(d));
            }
        }
    }
    let mut albums: Vec<Album> = vec![];
    for dir in dirs {
        if opts.only.as_ref().is_some_and(|o| !same_ci(&dir, o)) {
            continue;
        }
        if nested.contains(&lower(&dir)) {
            out.left_alone.push(left(&dir, "two_albums_one_folder"));
            continue;
        }
        match listing(&dir) {
            Some(entries) => albums.push(Album { dir, entries }),
            None => out.left_alone.push(left(&dir, "changed")),
        }
    }
    // the tags of every track, read at once
    let files: Vec<PathBuf> = albums.iter().flat_map(|a| a.entries.iter().filter(|e| !e.dir && is_audio(&e.path)).map(|e| e.path.clone())).collect();
    let mut tags = read_all(&files, read_tags, progress).into_iter();
    let hidden: HashSet<String> = opts.hidden.iter().map(|h| lower(h)).collect();
    let playing = opts.playing.as_ref().map(|p| lower(p));

    // where each album would go
    let mut candidates: Vec<(Move, bool)> = vec![];
    for a in &albums {
        let audio: Vec<&Entry> = a.entries.iter().filter(|e| !e.dir && is_audio(&e.path)).collect();
        let its_tags: Vec<Option<TrackTags>> = tags.by_ref().take(audio.len()).collect();
        if audio.iter().any(|e| hidden.contains(&lower(&e.path))) {
            out.left_alone.push(left(&a.dir, "removed"));
            continue;
        }
        // a track whose tags could not be read because another program holds it is "open", not untagged
        if audio.iter().zip(&its_tags).any(|(e, t)| t.is_none() && playing.as_ref() != Some(&lower(&e.path)) && open_elsewhere(&e.path)) {
            out.left_alone.push(left(&a.dir, "open"));
            continue;
        }
        let (artist, folder) = match target_names(&its_tags, &opts.compilations) {
            Ok(names) => names,
            Err(why) => {
                out.left_alone.push(left(&a.dir, why));
                continue;
            }
        };
        let artist_dir = artists.get(&artist.to_lowercase()).cloned().unwrap_or_else(|| library.join(&artist));
        // the folder's own words the tags do not have ("(UK Version)", "[Deluxe Edition]") come along
        let (album, tags_year) = (clean_name(&tag_album(&its_tags)), tag_year(&its_tags));
        // the folder's own year is kept; the tags' year is used only when the folder's name has none
        let (folder, year) = match folder_year(&leaf(&a.dir)) {
            Some(y) if y != tags_year => (with_year(&folder, &tags_year, &y), y),
            _ => (folder, tags_year),
        };
        let folder_full = with_own_words(&folder, &year, &own_words(&leaf(&a.dir), &album));
        let folder = &folder_full;
        let to = place_for(&artist_dir, &folder);
        let folder = leaf(&to);
        // (a folder that differs only in letter case, punctuation or spacing stays as it is, whether or not
        // its name was shortened)
        if to.as_os_str() == a.dir.as_os_str() || already_in_place(library, &a.dir, &to) || already_in_place(library, &a.dir, &artist_dir.join(&folder_full)) {
            out.in_place += 1;
            continue;
        }
        let words = format!("{} \\ {}", leaf(&artist_dir), folder);
        let sidecars: Vec<String> = a.entries.iter().filter(|e| !e.dir && !is_audio(&e.path)).map(|e| e.rel.clone()).collect();
        let changes = changes_of(library, &a.dir, &to, &album, &year);
        let m = Move { from: a.dir.clone(), to: to.clone(), files: audio.len(), sidecars, signature: signature(&a.entries), words, changes };
        if a.entries.iter().any(|e| !e.dir && utf16_len(&to.join(&e.rel)) > MAX_PATH && utf16_len(&to.join(&e.rel)) > utf16_len(&e.path)) {
            out.left_alone.push(left(&a.dir, "too_long"));
            continue;
        }
        if a.entries.iter().any(|e| e.read_only) {
            out.left_alone.push(left(&a.dir, "read_only"));
            continue;
        }
        let case_only = same_ci(&to, &a.dir);
        if !case_only {
            if let Some(why) = in_the_way(library, &a.dir, &to, &all) {
                out.left_alone.push(left_with(&a.dir, "target_exists", why));
                continue;
            }
        }
        if a.entries.iter().any(|e| !e.dir && playing.as_ref() != Some(&lower(&e.path)) && open_elsewhere(&e.path)) {
            out.left_alone.push(left(&a.dir, "open"));
            continue;
        }
        candidates.push((m, case_only));
    }
    // two albums aiming at one place: neither moves
    let mut aims: HashMap<String, usize> = HashMap::new();
    for (m, _) in &candidates {
        *aims.entry(lower(&m.to)).or_default() += 1;
    }
    for (m, _) in candidates {
        if aims[&lower(&m.to)] > 1 {
            let words = format!("Another album would also go to {}, so neither moves.", m.to.display());
            out.left_alone.push(left_with(&m.from, "collision", words));
        } else {
            out.moves.push(m);
        }
    }
    out.to_move = out.moves.len();
    out.left = out.left_alone.len();
    for m in &out.moves {
        for c in &m.changes {
            match c.as_str() {
                "artist" => out.counts.artist += 1,
                "renamed" => out.counts.renamed += 1,
                _ => out.counts.tidy += 1,
            }
        }
    }
    Ok(out)
}

/// Why `to` cannot be where `from` goes: something is there, or a folder on the way is a file or another
/// album's folder (never moved into). `from` itself on the way is fine: it goes through a temporary name.
fn in_the_way(library: &Path, from: &Path, to: &Path, albums: &HashSet<String>) -> Option<String> {
    if to.exists() {
        return Some(format!("Something is already at {}.", to.display()));
    }
    for up in to.ancestors().skip(1).take_while(|u| under_ci(u, library) && !same_ci(u, library)) {
        if same_ci(up, from) {
            continue;
        }
        if up.is_file() {
            return Some(format!("{} is a file, not a folder.", up.display()));
        }
        if albums.contains(&lower(up)) {
            return Some(format!("{} is another album's folder.", up.display()));
        }
    }
    None
}

// ------------------------------------------------------------------ the run

/// What a run is given by the service layer.
pub struct Hooks<'a> {
    /// Stop was asked for: the album in hand is finished, then the run ends
    pub cancel: &'a dyn Fn() -> bool,
    /// the music folder (its drive) is still there; checked before each album
    pub still_there: &'a dyn Fn() -> bool,
    /// this album folder holds the file the player has loaded
    pub is_playing: &'a dyn Fn(&Path) -> bool,
    /// (albums done, albums in all, "Radiohead \\ In Rainbows (2007)")
    pub progress: &'a dyn Fn(usize, usize, &str),
    /// an album is wholly at its new place: (its key, "<batch>#<step>", from, to). Called right after the
    /// rename and before its "done" line, so whatever it writes down survives a crash at any point (a crash
    /// before the "done" line has `recover` say the move again, under the same key). Also called for the
    /// moves a run's `recover` finds were done.
    pub settled: &'a dyn Fn(&str, &Path, &Path),
}

#[derive(Clone, Debug, Default)]
pub struct RunResult {
    /// the undo batch; None when nothing moved
    pub batch: Option<String>,
    /// every album moved, in the order they moved
    pub moved: Vec<(PathBuf, PathBuf)>,
    pub left_alone: Vec<Left>,
    /// Stop was asked for before every album was done
    pub stopped: bool,
}

#[derive(Debug)]
pub enum RunError {
    /// the music folder went away mid-run: what was done is in `done` (and in Repair history)
    Gone { words: String, done: RunResult },
    /// the run could not go on (its undo record could not be written); what was done is in `done`
    Failed { words: String, done: RunResult },
}

pub const GONE_WORDS: &str = "The music folder went away (was the drive unplugged?). Nothing more was changed; what was done is in Repair history.";

/// Engine tests: a crash at a named point of a run of one library, as if the program died there (a
/// panic: nothing after it in the job runs). Keyed by the library, so tests running side by side are not
/// touched. `after` albums pass the point first.
#[cfg(test)]
pub(crate) mod crash_at {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    static AT: Mutex<Vec<(PathBuf, &'static str, usize)>> = Mutex::new(vec![]);
    pub(crate) fn set(library: &Path, point: &'static str, after: usize) {
        AT.lock().unwrap().push((library.to_path_buf(), point, after));
    }
    pub(super) fn here(album: &Path, point: &str) {
        let mut at = AT.lock().unwrap();
        let Some(i) = at.iter().position(|(lib, p, _)| *p == point && super::under_ci(album, lib)) else { return };
        if at[i].2 > 0 {
            at[i].2 -= 1;
            return;
        }
        at.remove(i);
        drop(at);
        panic!("test crash at {point}");
    }
}

/// What happened to one album.
enum Outcome {
    Moved,
    Left(Left),
    Gone,
    /// the undo record could not be written
    Stop(String),
    #[cfg(test)]
    Crashed,
}

/// The moves as `library.moved` and `backup-moves.jsonl` want them: `[{"from", "to", "folder": true}]`.
pub fn moves_json(moved: &[(PathBuf, PathBuf)]) -> Vec<serde_json::Value> {
    moved.iter().map(|(from, to)| serde_json::json!({ "from": from.to_string_lossy(), "to": to.to_string_lossy(), "folder": true })).collect()
}

/// Moves the albums of a preview. Each album is checked again right before it moves (its listing and tags
/// fingerprint, read-only and open files, the place it goes), and is either wholly moved or left wholly where
/// it was. The playing album goes last; if it is still playing then, it stays (`playing`): the service layer
/// does the waiting. Before anything, a batch an earlier run left half written is settled (`recover`).
pub fn run(library: &Path, preview: &Preview, hooks: &Hooks) -> Result<RunResult, RunError> {
    let mut res = RunResult::default();
    if !same_ci(&preview.library, library) {
        return Err(RunError::Failed { words: "That preview is of another music folder.".into(), done: res });
    }
    let root = crate::repair::backups_root(library);
    for i in recover(&root) {
        for (key, from, to) in &i.newly_done {
            (hooks.settled)(key, from, to);
        }
    }
    let there = || (hooks.still_there)() && library.is_dir();
    if !there() {
        return Err(RunError::Gone { words: GONE_WORDS.into(), done: res });
    }
    let batch = crate::undo::Batch::begin_kind(&root, library, TITLE, KIND).map_err(|e| RunError::Failed { words: e, done: RunResult::default() })?;
    // the playing album last
    let (mut order, mut later): (Vec<&Move>, Vec<&Move>) = (vec![], vec![]);
    for m in &preview.moves {
        if (hooks.is_playing)(&m.from) { later.push(m) } else { order.push(m) }
    }
    let deferred = order.len();
    order.extend(later);
    let total = order.len();
    let mut step: u64 = next_step(&batch.dir);
    let mut ended: Option<RunError> = None;
    for (n, m) in order.iter().enumerate() {
        if (hooks.cancel)() {
            res.stopped = true;
            break;
        }
        (hooks.progress)(n, total, &m.words);
        if !there() {
            ended = Some(RunError::Gone { words: GONE_WORDS.into(), done: RunResult::default() });
            break;
        }
        if n >= deferred && (hooks.is_playing)(&m.from) {
            res.left_alone.push(left(&m.from, "playing"));
            continue;
        }
        // checked and moved with the library held, so nothing can change the album in between
        let held = crate::library_lock::hold();
        let outcome = match check(library, m) {
            Err(l) => Outcome::Left(l),
            Ok(()) => {
                step += 1;
                move_album(&batch, step, m, &there, hooks.settled)
            }
        };
        drop(held);
        match outcome {
            Outcome::Moved => res.moved.push((m.from.clone(), m.to.clone())),
            Outcome::Left(l) => res.left_alone.push(l),
            Outcome::Gone => {
                ended = Some(RunError::Gone { words: GONE_WORDS.into(), done: RunResult::default() });
                break;
            }
            Outcome::Stop(words) => {
                ended = Some(RunError::Failed { words, done: RunResult::default() });
                break;
            }
            #[cfg(test)]
            Outcome::Crashed => {
                // as if the program died here: nothing after this point happens
                return Err(RunError::Failed { words: "crashed".into(), done: res });
            }
        }
    }
    (hooks.progress)(total, total, "");
    if !matches!(ended, Some(RunError::Gone { .. })) {
        remove_emptied(library, &res.moved);
    }
    if batch.changes() > 0 {
        res.batch = Some(batch.id.clone());
    } else {
        // nothing moved: no record (a step that was written down and then not done leaves nothing to undo)
        let _ = std::fs::remove_file(batch.dir.join("log.jsonl"));
    }
    batch.close();
    match ended {
        None => Ok(res),
        Some(RunError::Gone { words, .. }) => Err(RunError::Gone { words, done: res }),
        Some(RunError::Failed { words, .. }) => Err(RunError::Failed { words, done: res }),
    }
}

/// The step number after the highest one in a batch's log (a fresh batch starts at 1).
fn next_step(dir: &Path) -> u64 {
    crate::undo::entries(dir).iter().filter_map(|e| e["Step"].as_u64()).max().unwrap_or(0)
}

/// Is the album still as the preview saw it, and can it still go where the preview said?
fn check(library: &Path, m: &Move) -> Result<(), Left> {
    if !exists_exact(&m.from) || !m.from.is_dir() {
        return Err(left_with(&m.from, "changed", "It is no longer where the preview found it.".into()));
    }
    let Some(entries) = listing(&m.from) else { return Err(left(&m.from, "changed")) };
    if signature(&entries) != m.signature {
        return Err(left(&m.from, "changed"));
    }
    if entries.iter().any(|e| e.read_only) {
        return Err(left(&m.from, "read_only"));
    }
    if !same_ci(&m.from, &m.to) {
        // (only the place itself and the folders on the way: the other albums were checked by the preview)
        if let Some(why) = in_the_way(library, &m.from, &m.to, &HashSet::new()) {
            return Err(left_with(&m.from, "target_exists", why));
        }
    }
    if entries.iter().any(|e| !e.dir && open_elsewhere(&e.path)) {
        return Err(left(&m.from, "open"));
    }
    Ok(())
}

/// A rename on one drive, never a copy: `MoveFileExW` with no flags (so it also never replaces anything).
pub(crate) fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |p: &Path| -> Vec<u16> {
        // (a long path needs the \\?\ form to get past the old 260-character limit)
        let s = p.as_os_str();
        let long = utf16_len(p) >= 248 && p.to_string_lossy().as_bytes().get(1) == Some(&b':');
        let mut w: Vec<u16> = if long { r"\\?\".encode_utf16().chain(s.encode_wide()).collect() } else { s.encode_wide().collect() };
        w.push(0);
        w
    };
    let (f, t) = (wide(from), wide(to));
    #[cfg(test)]
    if tests::fault::fail_rename() {
        return Err(std::io::Error::from_raw_os_error(32));
    }
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// What a failed rename means for the album.
fn rename_failed(from: &Path, e: &std::io::Error, there: &dyn Fn() -> bool) -> Outcome {
    if !there() {
        return Outcome::Gone;
    }
    match e.raw_os_error() {
        Some(5 | 32 | 33) => Outcome::Left(left(from, "open")),
        Some(80 | 183) => Outcome::Left(left(from, "target_exists")),
        Some(2 | 3) => Outcome::Left(left_with(from, "changed", "It is no longer where the preview found it.".into())),
        Some(17) => Outcome::Left(left_with(from, "failed", "Its new place is on another drive; Organize only renames.".into())),
        _ => Outcome::Left(left_with(from, "failed", crate::util::plain_err(format!("Windows would not move it: {e}")))),
    }
}

/// One album: written down (and on the disk) before each rename, and "done" after the last one.
///
///   {Kind: "madedir", Path, Step}            a folder made for it (Undo takes it away again once empty)
///   {Kind: "moved", From, To, Reason, Step}  the rename
///   {Kind: "done", Step}                     the album is wholly at its new place
///   {Kind: "notdone", Step, Why}             it is wholly at its old place (Undo passes the step by)
///
/// A case-only rename, or a move into a folder named like the album's own ("Radiohead" holding the album
/// Radiohead \ OK Computer), goes through a temporary name beside it: two "moved" lines.
fn move_album(batch: &crate::undo::Batch, step: u64, m: &Move, there: &dyn Fn() -> bool, settled: &dyn Fn(&str, &Path, &Path)) -> Outcome {
    let _held = crate::library_lock::hold();
    let journal = |e: serde_json::Value| batch.journal(e);
    let moved = |from: &Path, to: &Path| serde_json::json!({ "Kind": "moved", "From": from.to_string_lossy(), "To": to.to_string_lossy(), "Reason": "organize", "Step": step });
    let not_done = |why: &str| journal(serde_json::json!({ "Kind": "notdone", "Step": step, "Why": why }));
    let via_temp = same_ci(&m.from, &m.to) || under_ci(&m.to, &m.from);
    let mut start = m.from.clone();
    if via_temp {
        let tmp = m.from.with_file_name(format!("{}.organizing-{}", leaf(&m.from), rand_hex(6)));
        if let Err(e) = journal(moved(&m.from, &tmp)) {
            return Outcome::Stop(e);
        }
        if let Err(e) = rename(&m.from, &tmp) {
            let out = rename_failed(&m.from, &e, there);
            let _ = not_done(&e.to_string());
            return out;
        }
        start = tmp;
        #[cfg(test)]
        if tests::fault::crash_mid_hop() {
            return Outcome::Crashed;
        }
        #[cfg(test)]
        crash_at::here(&m.from, "mid_hop");
    }
    // back to where it was, on any failure after the first rename
    let put_back = |start: &Path| {
        if *start != m.from {
            let _ = rename(start, &m.from);
        }
    };
    // the folders it goes into, outermost first
    let mut missing: Vec<PathBuf> = m.to.parent().map(|p| p.ancestors().take_while(|a| !a.exists()).map(Path::to_path_buf).collect()).unwrap_or_default();
    missing.reverse();
    let mut made: Vec<PathBuf> = vec![];
    for dir in &missing {
        if let Err(e) = journal(serde_json::json!({ "Kind": "madedir", "Path": dir.to_string_lossy(), "Step": step })) {
            put_back(&start);
            return Outcome::Stop(e);
        }
        if let Err(e) = std::fs::create_dir(dir) {
            for d in made.iter().rev() {
                let _ = std::fs::remove_dir(d);
            }
            put_back(&start);
            let _ = not_done(&e.to_string());
            return if there() { Outcome::Left(left_with(&m.from, "failed", crate::util::plain_err(format!("The folder {} could not be made: {e}", dir.display())))) } else { Outcome::Gone };
        }
        made.push(dir.clone());
    }
    if let Err(e) = journal(moved(&start, &m.to)) {
        put_back(&start);
        return Outcome::Stop(e);
    }
    #[cfg(test)]
    if tests::fault::crash_before_rename() {
        return Outcome::Crashed;
    }
    if let Err(e) = rename(&start, &m.to) {
        put_back(&start);
        for d in made.iter().rev() {
            let _ = std::fs::remove_dir(d);
        }
        let out = rename_failed(&m.from, &e, there);
        let _ = not_done(&e.to_string());
        return out;
    }
    // written down for Cadence and Backup before the "done" line (see `Hooks::settled`)
    settled(&format!("{}#{step}", batch.id), &m.from, &m.to);
    #[cfg(test)]
    if tests::fault::crash_before_done() {
        return Outcome::Crashed;
    }
    batch.count_change();
    match journal(serde_json::json!({ "Kind": "done", "Step": step })) {
        // (the album is moved and the log says so; `recover` writes the missing "done" line at the next start)
        Err(e) => Outcome::Stop(e),
        Ok(()) => {
            #[cfg(test)]
            crash_at::here(&m.from, "after_album");
            Outcome::Moved
        }
    }
}

/// The folders the moved albums left empty go, up to (not including) the music folder. Only truly empty
/// ones: a folder holding anything, a hidden file too, stays. Undo makes them again when it needs them.
fn remove_emptied(library: &Path, moved: &[(PathBuf, PathBuf)]) {
    let _held = crate::library_lock::hold();
    for (from, _) in moved {
        for up in from.ancestors().skip(1) {
            if !under_ci(up, library) || same_ci(up, library) || std::fs::read_dir(up).map(|mut d| d.next().is_some()).unwrap_or(true) || std::fs::remove_dir(up).is_err() {
                break;
            }
        }
    }
}

// ------------------------------------------------------------------ after a crash

/// An organize run that was cut off, as it was settled.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Interrupted {
    pub batch: String,
    /// albums the batch moved (all of it, the settled ones too)
    pub moved: usize,
    /// albums that were about to move and were found where they were
    pub left: usize,
    /// albums found at a temporary name (`<name>.organizing-<hex>`, beside their old place) that could not be
    /// renamed back: the step stays open, and the next start tries again
    #[serde(default)]
    pub stuck: Vec<PathBuf>,
    /// the moves this settling found done ("<batch>#<step>", from, to), for Cadence and Backup to hear of
    #[serde(skip)]
    pub newly_done: Vec<(String, PathBuf, PathBuf)>,
}

/// Settles every organize batch in these undo backups folders whose log has a move without its "done" line
/// (the program stopped between the two): the album at its new place gets "done", at its old place
/// "notdone". An album caught at its temporary name goes back to its old place. A batch whose music folder
/// is not there now is left for later. Run when a run starts and when the engine starts, before any other
/// library write. Returns what was settled.
pub fn recover_all(roots: &[PathBuf]) -> Vec<Interrupted> {
    roots.iter().flat_map(|r| recover(r)).collect()
}

fn recover(root: &Path) -> Vec<Interrupted> {
    let _held = crate::library_lock::hold();
    let mut out = vec![];
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let dir = e.path();
        let read = std::fs::read_to_string(dir.join("batch.json")).ok().and_then(|m| serde_json::from_str::<serde_json::Value>(m.trim_start_matches('\u{feff}')).ok());
        let Some(meta) = read.or_else(|| rewrite_record(&dir)) else { continue };
        if meta["Kind"] != KIND || dir.join("undone.txt").exists() {
            continue;
        }
        if let Some(i) = settle(&dir, Path::new(meta["Library"].as_str().unwrap_or(""))) {
            out.push(i);
        }
    }
    out
}

/// An organize batch whose `batch.json` is torn or missing (a power cut while it was written, by an engine
/// that did not yet wait for it to reach the disk) gets it written again from its log, so it is settled and
/// listed in Repair history with its Undo. Only a log of Organize's own lines counts; anything else stays as
/// it is. The music folder is not known then: the record names none (Repair history lists such a record for
/// any music folder).
fn rewrite_record(dir: &Path) -> Option<serde_json::Value> {
    let lines = crate::undo::entries(dir);
    if !lines.iter().any(|l| l["Kind"] == "moved" && l["Reason"] == "organize") || lines.iter().any(|l| l["Kind"] == "moved" && l["Reason"] != "organize") {
        return None;
    }
    let id = leaf(dir);
    let when = chrono::NaiveDateTime::parse_from_str(id.get(..15)?, "%Y%m%d-%H%M%S").ok()?.and_local_timezone(chrono::Local).earliest()?;
    let record = serde_json::json!({ "Id": id, "Title": TITLE, "When": when.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string(), "Library": "", "Kind": KIND });
    crate::undo::write_record(dir, &record).ok()?;
    eprintln!("cadence-engine: the record of organize batch {id} was torn; it was written again from its log");
    Some(record)
}

fn settle(dir: &Path, library: &Path) -> Option<Interrupted> {
    let lines = crate::undo::entries(dir);
    let mut steps: Vec<u64> = vec![];
    let mut moves: HashMap<u64, Vec<(PathBuf, PathBuf)>> = HashMap::new();
    let mut made: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut settled: HashSet<u64> = HashSet::new();
    for l in &lines {
        let Some(s) = l["Step"].as_u64() else { continue };
        let text = |k: &str| PathBuf::from(l[k].as_str().unwrap_or(""));
        match l["Kind"].as_str().unwrap_or("") {
            "moved" => {
                if !steps.contains(&s) {
                    steps.push(s);
                }
                moves.entry(s).or_default().push((text("From"), text("To")));
            }
            "madedir" => made.entry(s).or_default().push(text("Path")),
            "done" | "notdone" => {
                settled.insert(s);
            }
            _ => {}
        }
    }
    let open: Vec<u64> = steps.into_iter().filter(|s| !settled.contains(s)).collect();
    if open.is_empty() {
        return None;
    }
    if !library.as_os_str().is_empty() && !library.is_dir() {
        return None; // the drive is not there: settle it when it is
    }
    let batch = crate::undo::Batch::reopen(dir);
    let (mut left, mut stuck, mut newly_done) = (0, vec![], vec![]);
    for s in open {
        let ms = &moves[&s];
        let (from, to) = (ms[0].0.clone(), ms[ms.len() - 1].1.clone());
        // the temporary name of a hop (case-only, or into a folder named like its own), when there was one.
        // With only the hop's first line written, the last place named is the temporary one: the album may
        // be there, and that is not its new place
        let tmp = ms.iter().map(|m| &m.1).find(|t| leaf(t).contains(".organizing-")).cloned();
        let at_tmp = |t: &Option<PathBuf>| t.as_ref().is_some_and(|t| exists_exact(t));
        let line = if !tmp.as_ref().is_some_and(|t| same_ci(t, &to)) && exists_exact(&to) {
            newly_done.push((format!("{}#{s}", leaf(dir)), from.clone(), to.clone()));
            serde_json::json!({ "Kind": "done", "Step": s })
        } else {
            // the folders made for it go first (only empty ones): moving into a folder named like its own
            // makes a folder at its old name, which must be gone before it can go back
            for d in made.get(&s).into_iter().flatten().rev() {
                if std::fs::read_dir(d).is_ok_and(|mut r| r.next().is_none()) {
                    let _ = std::fs::remove_dir(d);
                }
            }
            if at_tmp(&tmp) && !from.exists() {
                let _ = rename(tmp.as_deref().unwrap_or(&from), &from);
            }
            if at_tmp(&tmp) {
                // still at its temporary name (Windows would not rename it back): not "notdone", which would
                // leave it hidden there for good. The step stays open and the next start tries again
                stuck.extend(tmp.clone());
                continue;
            }
            left += 1;
            serde_json::json!({ "Kind": "notdone", "Step": s, "Why": "the program stopped before it moved" })
        };
        if batch.journal(line).is_err() {
            return None;
        }
    }
    let moved = crate::undo::entries(dir).iter().filter(|e| e["Kind"] == "done").count();
    let i = Interrupted { batch: leaf(dir), moved, left, stuck, newly_done };
    let _ = std::fs::write(dir.join("interrupted.json"), serde_json::to_string(&i).unwrap_or_default());
    Some(i)
}

/// For `organize.status`: the newest organize run that was cut off and settled, not yet acknowledged.
pub fn status(root: &Path) -> Option<Interrupted> {
    let mut found: Vec<(String, Interrupted)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let i: Interrupted = serde_json::from_str(&std::fs::read_to_string(e.path().join("interrupted.json")).ok()?).ok()?;
            Some((leaf(&e.path()), i))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().next().map(|f| f.1)
}

/// Cadence has said what happened: `status` no longer names this batch.
pub fn acknowledge(root: &Path, batch: &str) {
    if batch.is_empty() || batch.contains(['\\', '/', ':']) || batch.starts_with('.') {
        return;
    }
    let dir = root.join(batch);
    let _ = std::fs::rename(dir.join("interrupted.json"), dir.join("interrupted-seen.json"));
}

// ------------------------------------------------------------------ the app's own tag reading

/// The app's tag reading for a preview: what Library Health's checks already read for this library
/// (`healthscan::saved_tags`, keyed as the check keys them), and ffprobe for a file not read yet or changed
/// since. Only the few tags Organize needs are kept from the cache.
pub fn app_tag_reader(library: &Path) -> Result<impl Fn(&Path) -> Option<TrackTags> + Sync, String> {
    let ffprobe = crate::importer::tools::find_tool("ffprobe")?;
    let cache: HashMap<String, TrackTags> = crate::healthscan::saved_tags(library).into_iter().map(|(k, i)| (k, TrackTags::from_map(&i.tags))).collect();
    Ok(move |file: &Path| {
        let m = std::fs::metadata(file).ok()?;
        if let Some(tags) = cache.get(&crate::healthscan::tag_key(file, &m)) {
            return Some(tags.clone());
        }
        let info = crate::healthscan::probe(&ffprobe, file);
        info.ok.then(|| TrackTags::from_map(&info.tags))
    })
}
