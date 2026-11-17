//! Repairs for what the Health check finds.
//!
//! A repair is planned from one finding (what would be done, and what stands in the way), then applied
//! through one undo journal (`undo::Batch`): every change is written down before it is made, nothing is
//! deleted (what leaves the library goes into the undo backups), and Repair history can put it all back.
//! Each thing is looked at again at the moment it is changed: the finding may be a day old.
//!
//! The kinds that only move things:
//!   * "remove": an empty folder, a leftover import folder, a temp file
//!   * "file-loose": tracks lying in an artist's folder go into the folder of the album their tags name
//!   * "dupe-tracks": of a song that is there twice the better copy stays, the other goes to the backups
//! The kinds that rewrite a file's tags (`Step::Tags`): "strip-junk", "garbled", "album-artist",
//! "album-name", "missing-tags", "tracklist" and "tracklist-titles", "year-genre", "vbr-header", and
//! "cover" when the picture is already in the folder or in some of the tracks. A file is never edited in
//! place: a copy is made in the batch's folder, the Rust tag editor (`crate::tagwrite`)
//! changes the copy and checks that the audio is byte for byte the same and that the change is there, and
//! only then the copy takes the file's place and the old file goes into the undo backups.
//! Not here yet: covers found online, "unwrap", and the repairs that need a choice ("pick-album",
//! "merge-artist", "artist-mismatch").
use crate::healthscan;
use crate::importer::quality;
use crate::importer::util::{album_key, clean_name, file_stem, most_common, strip_year};
use crate::importer::{rules, tools};
use crate::undo::Batch;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The finding a repair is asked for, as the report has it.
pub struct Ask {
    /// the report's fix kind ("remove", "file-loose", ...)
    pub fix: String,
    pub cat: String,
    /// the folder (or file) the finding is about
    pub path: String,
    pub files: Vec<String>,
    /// the artist and the album folder as the report names them
    pub artist: String,
    pub album: String,
    /// the finding's extra facts (the report's X)
    pub data: HashMap<String, String>,
    /// which way to fix it, for the kinds that can go more than one way (see `choices`); "" otherwise
    pub choice: String,
}

/// A change to one file's tags.
#[derive(Debug, PartialEq, Default, Clone)]
pub struct TagStep {
    pub file: PathBuf,
    /// tags to set: title, artist, album, album_artist, genre, year, track, disc
    pub set: BTreeMap<String, String>,
    /// (field, value) pairs to take out, exactly these values
    pub junk: Vec<(String, String)>,
    /// a picture to embed as the cover
    pub cover: Option<PathBuf>,
    /// the picture replaces a cover the track already carries (else only tracks without one get it)
    pub cover_all: bool,
    /// add the length header an MP3 is missing
    pub vbr: bool,
    /// words for the lyrics field
    pub lyrics: Option<String>,
    /// "missing": the words go in only when the file has none; "set": in place of what is there; "clear":
    /// the field is emptied
    pub lyrics_mode: &'static str,
    /// lyrics kept in a tag no player reads go into the lyrics field
    pub unhide: bool,
    /// Only the lyrics field changes, in an MP3: no copy of the old file is kept. The tag editor proves that
    /// the audio and every other entry of the tag are byte for byte what they were, and the old words are
    /// written into the undo record, from which Undo puts them back. (A library-wide fill of lyrics that kept
    /// every old file would need as much free room as the library takes.)
    pub light: bool,
    /// undoing such a change: the lyrics the file must still hold for them to be put back
    pub only_if: Option<String>,
    pub reason: String,
}

#[derive(Debug, PartialEq)]
pub enum Step {
    Remove { path: PathBuf, reason: String },
    Move { from: PathBuf, to: PathBuf, reason: String },
    Tags(TagStep),
    /// a new file in the library (the full-size cover beside the tracks); `replace`: the one there goes to the backups first
    Create { path: PathBuf, source: PathBuf, replace: bool, reason: String },
    /// an MP3 taken out of the WAV wrapper it was saved in
    Unwrap { file: PathBuf, reason: String },
}

/// What a repair needs besides the finding.
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// the saved deep-check results, so a file whose tags were rewritten keeps its result
    pub quality_cache: Option<PathBuf>,
    /// false: nothing is looked up online (a year or genre that needs it is said to be missing)
    pub online: bool,
    /// how LRCLIB is asked for lyrics (None: the app's own way, timed lyrics when there are any)
    pub lyrics: Option<crate::importer::lyrics::Lookup>,
}

impl Tools {
    fn lookup(&self) -> crate::importer::lyrics::Lookup {
        self.lyrics.clone().unwrap_or_else(|| crate::importer::lyrics::Lookup::for_app(self.online, true))
    }
}

#[derive(Default, Debug)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// what will not be done, and why, in plain words
    pub problems: Vec<String>,
}

#[derive(Default, Debug)]
pub struct Done {
    /// the undo record, when anything was changed
    pub batch: Option<String>,
    pub changed: usize,
    pub problems: Vec<String>,
}

/// Whether the app can do this kind of repair itself.
pub fn can_fix(kind: &str) -> bool {
    matches!(kind, "remove" | "file-loose" | "dupe-tracks" | "strip-junk" | "garbled" | "album-artist" | "album-name" | "missing-tags" | "tracklist" | "tracklist-titles" | "year-genre" | "vbr-header" | "cover" | "pick-album" | "merge-artist" | "artist-mismatch" | "unwrap" | "lyrics-fill" | "lyrics-junk" | "lyrics-unhide")
}


/// The words on the control that starts the repair.
pub fn action_words(kind: &str, cat: &str, files: usize) -> &'static str {
    match (kind, cat) {
        ("remove", "Empty folder") => "Remove this empty folder",
        ("remove", "Leftover import folder") => "Remove this leftover folder",
        ("remove", _) if files == 1 => "Remove this file",
        ("remove", _) => "Remove these files",
        ("file-loose", _) if files == 1 => "File it in its album folder",
        ("file-loose", _) => "File them in their album folders",
        ("dupe-tracks", _) => "Keep the better copy of each",
        ("strip-junk", _) => "Remove the junk from the tags",
        ("garbled", _) => "Repair the scrambled text",
        ("album-artist", _) => "Set one album artist on every track",
        ("album-name", _) => "Set one album name on every track",
        ("missing-tags", _) => "Fill in the missing tags",
        ("tracklist", _) | ("tracklist-titles", _) => "Correct them to the official list",
        ("year-genre", _) => "Fill in the year and genre",
        ("vbr-header", _) => "Add the missing length header",
        ("cover", "No cover art") => "Find the cover and add it",
        ("cover", "Small cover art") => "Find a larger cover",
        ("cover", _) => "Add the cover to every track",
        ("unwrap", _) => "Convert to plain MP3",
        ("lyrics-fill", _) => "Find the lyrics on LRCLIB",
        ("lyrics-junk", _) => "Replace them from LRCLIB",
        ("lyrics-unhide", _) => "Put the lyrics where players look",
        _ => "Fix this",
    }
}

/// The ways a finding can be fixed, as (the words on the control, the choice to ask the repair for). Most
/// kinds have one way. Which copy of an album to keep, which artist folder to merge into, and whether a
/// wrongly filed album gets new tags or a new place are for the person to say.
pub fn choices(kind: &str, cat: &str, data: &HashMap<String, String>, files: usize) -> Vec<(String, String)> {
    let fact = |k: &str| data.get(k).map(|s| s.as_str()).unwrap_or("");
    let folders = || fact("folders").split('|').filter(|f| !f.is_empty()).map(|f| (leaf(Path::new(f)), f.to_string())).collect::<Vec<_>>();
    match kind {
        "pick-album" => folders().into_iter().map(|(name, f)| (format!("Keep '{name}'"), f)).collect(),
        "merge-artist" => folders().into_iter().map(|(name, f)| (format!("Merge into '{name}'"), f)).collect(),
        "lyrics-junk" => vec![("Replace them from LRCLIB".to_string(), String::new()), ("Only cut the junk lines".to_string(), "cut".to_string())],
        "artist-mismatch" => vec![(format!("Set the album artist to '{}'", fact("folder")), "tag".to_string()), (format!("Move the album to '{}'", fact("tagged")), "move".to_string())],
        _ => vec![(action_words(kind, cat, files).to_string(), String::new())],
    }
}

/// Where a library's undo backups are: the top of its drive, or beside the library for a test run
/// (`MUSIC_RECS_DATA` set), which must never leave anything at the top of a drive.
pub fn backups_root(library: &Path) -> PathBuf {
    if std::env::var_os("MUSIC_RECS_DATA").is_some_and(|v| !v.is_empty()) {
        return library.parent().unwrap_or(library).join(".music-repair-backups");
    }
    crate::undo::root_for(library)
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn inside(p: &Path, library: &Path) -> bool {
    p != library && p.starts_with(library) && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false)
}

/// A file some other program has open cannot be moved without cutting that program off.
fn locked(file: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(file).is_err()
}

/// What the repair would do. Reads the library (and the tags of loose tracks); changes nothing.
pub fn plan(ask: &Ask, library: &Path, tools: &Tools) -> Plan {
    let ffprobe = tools.ffprobe.as_path();
    let mut p = Plan::default();
    // the finding's files that are still there, in the library
    let files: Vec<PathBuf> = ask.files.iter().map(PathBuf::from).filter(|f| inside(f, library) && f.is_file()).collect();
    let fact = |k: &str| ask.data.get(k).map(|s| s.as_str()).unwrap_or("");
    let tags_of = |f: &Path| healthscan::probe(ffprobe, f);
    match ask.fix.as_str() {
        "remove" => {
            for f in &ask.files {
                let path = PathBuf::from(f);
                if !inside(&path, library) {
                    p.problems.push(format!("{f} is not in the library"));
                } else if let Some(why) = why_not_remove(&path, &ask.cat) {
                    p.problems.push(format!("{}: {why}", leaf(&path)));
                } else if path.exists() {
                    p.steps.push(Step::Remove { path, reason: ask.cat.clone() });
                } // already gone: nothing to do
            }
        }
        "file-loose" => {
            let from_dir = PathBuf::from(&ask.path);
            // files lying in the library folder itself belong to no artist's folder: each one goes to the
            // folder of the artist its own tags name (the one that is there, else a new one)
            let same = |a: &Path, b: &Path| a.to_string_lossy().trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']));
            let at_root = same(&from_dir, library);
            if !at_root && (!inside(&from_dir, library) || !from_dir.is_dir()) {
                p.problems.push(format!("{} is not an artist folder in the library", ask.path));
                return p;
            }
            let albums_in = |dir: &Path| -> Vec<String> { if dir.is_dir() { crate::library::album_paths(dir) } else { vec![] } };
            let own_albums = if at_root { vec![] } else { albums_in(&from_dir) };
            let artist_folders: Vec<PathBuf> = if at_root { std::fs::read_dir(library).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default() } else { vec![] };
            for f in &ask.files {
                let from = PathBuf::from(f);
                let name = leaf(&from);
                if !from.parent().is_some_and(|d| same(d, &from_dir)) || !from.is_file() {
                    continue; // filed or gone since the check
                }
                let info = tags_of(&from);
                let tag = |k: &str| info.tags.get(k).map(|s| s.trim()).unwrap_or("");
                let (artist_dir, albums) = if at_root {
                    let artist = clean_name(["album_artist", "albumartist", "artist"].iter().map(|k| tag(k)).find(|a| !a.is_empty()).unwrap_or(""));
                    if artist.is_empty() {
                        p.problems.push(format!("{name}: no artist tag, so it is not clear whose it is"));
                        continue;
                    }
                    let key = crate::util::artist_key(&artist);
                    let dir = artist_folders.iter().find(|d| leaf(d).eq_ignore_ascii_case(&artist)).or(artist_folders.iter().find(|d| crate::util::artist_key(&leaf(d)) == key)).cloned().unwrap_or_else(|| library.join(&artist));
                    let albums = albums_in(&dir);
                    (dir, albums)
                } else {
                    (from_dir.clone(), own_albums.clone())
                };
                let mut folder = clean_name(tag("album"));
                if folder.is_empty() {
                    p.problems.push(format!("{name}: no album tag, so it is not clear where it belongs"));
                    continue;
                }
                let year: String = [tag("date"), tag("year")].iter().find_map(|d| d.get(..4).filter(|y| y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()) && (y.starts_with("19") || y.starts_with("20")))).unwrap_or("").to_string();
                if !year.is_empty() && !folder.ends_with(&format!("({year})")) {
                    folder = format!("{folder} ({year})");
                }
                // the album folder the artist already has for it, whatever year or edition its name carries
                let key = album_key(&folder);
                let have = albums.iter().find(|a| a.rsplit('\\').next().is_some_and(|l| l.eq_ignore_ascii_case(&folder))).or(albums.iter().find(|a| album_key(a.rsplit('\\').next().unwrap_or(a)) == key));
                let dest = match have {
                    Some(rel) => artist_dir.join(rel),
                    None => artist_dir.join(&folder),
                };
                let to = dest.join(&name);
                if to.exists() || p.steps.iter().any(|s| matches!(s, Step::Move { to: t, .. } if *t == to)) {
                    p.problems.push(format!("{name}: a file with that name is already in '{}'", leaf(&dest)));
                } else if to.to_string_lossy().encode_utf16().count() > 240 {
                    p.problems.push(format!("{name}: its place in '{}' would be a path too long for some programs", leaf(&dest)));
                } else {
                    p.steps.push(Step::Move { from, to, reason: "file loose track".into() });
                }
            }
        }
        "pick-album" => {
            let folders: Vec<PathBuf> = fact("folders").split('|').filter(|f| !f.is_empty()).map(PathBuf::from).collect();
            let keep = PathBuf::from(&ask.choice);
            if !folders.contains(&keep) || !keep.is_dir() {
                p.problems.push("Say which copy to keep.".into());
                return p;
            }
            for f in folders.into_iter().filter(|f| *f != keep) {
                if !inside(&f, library) {
                    p.problems.push(format!("{} is not in the library", f.display()));
                } else if f.is_dir() {
                    p.steps.push(Step::Remove { path: f, reason: "other copy of the same album".into() });
                }
            }
        }
        "merge-artist" => {
            let folders: Vec<PathBuf> = fact("folders").split('|').filter(|f| !f.is_empty()).map(PathBuf::from).collect();
            let into = PathBuf::from(&ask.choice);
            if !folders.contains(&into) || !into.is_dir() || !inside(&into, library) {
                p.problems.push("Say which folder to merge into.".into());
                return p;
            }
            for src in folders.into_iter().filter(|f| *f != into) {
                if !inside(&src, library) || !src.is_dir() {
                    continue;
                }
                // everything in the other folder moves over; a name that is in both stays where it is. The
                // emptied folder is tidied away afterwards, and only if it really is empty
                for e in crate::importer::util::read_dir_sorted(&src) {
                    let to = into.join(e.file_name());
                    if to.exists() {
                        p.problems.push(format!("'{}' is in both folders, so it stays in '{}'", e.file_name().to_string_lossy(), leaf(&src)));
                    } else {
                        p.steps.push(Step::Move { from: e.path(), to, reason: "merge artist folders".into() });
                    }
                }
            }
        }
        "artist-mismatch" if ask.choice == "move" => {
            let (album, artist) = (PathBuf::from(&ask.path), clean_name(fact("tagged")));
            if artist.is_empty() || artist.chars().all(|c| c == '.') {
                p.problems.push(format!("'{}' cannot be a folder's name", fact("tagged")));
            } else if !inside(&album, library) || !album.is_dir() {
                p.problems.push(format!("{} is not an album folder in the library", ask.path));
            } else {
                let to = library.join(&artist).join(leaf(&album));
                if to.exists() {
                    p.problems.push(format!("'{artist}' already has a folder named '{}'", leaf(&album)));
                } else {
                    p.steps.push(Step::Move { from: album, to, reason: "move album to its artist".into() });
                }
            }
        }
        "artist-mismatch" => {
            let want = fact("folder").trim();
            if ask.choice != "tag" || want.is_empty() {
                p.problems.push("Say whether to change the tags or move the album.".into());
                return p;
            }
            for f in files {
                let info = tags_of(&f);
                if info.ok && info.tags.get("album_artist").map(|s| s.as_str()).unwrap_or("") != want {
                    p.steps.push(Step::Tags(TagStep { file: f, set: BTreeMap::from([("album_artist".to_string(), want.to_string())]), reason: "album artist matches the folder".into(), ..Default::default() }));
                }
            }
        }
        "dupe-tracks" => {
            for group in fact("groups").split("||") {
                let mut copies: Vec<(PathBuf, u32, f64, u64)> = group.split('|').map(PathBuf::from).filter(|f| inside(f, library) && f.is_file()).map(|f| {
                    let info = tags_of(&f);
                    let size = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
                    (f, info.kbps, info.duration, size)
                }).collect();
                if copies.len() < 2 {
                    continue;
                }
                // the better copy stays: the higher bitrate, then the longer, then the larger
                copies.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)).then(b.3.cmp(&a.3)));
                p.steps.extend(copies.into_iter().skip(1).map(|c| Step::Remove { path: c.0, reason: "duplicate track".into() }));
            }
        }
        // lyrics kept in a tag no player reads: into the lyrics field (the whole old file is kept, as for
        // any other repair: the tag that hid them is taken out, so the old words are nowhere else)
        "lyrics-unhide" => {
            for f in &files {
                if healthscan::lyric_of(ffprobe, f, &tags_of(f)).place == crate::importer::lyrics::Place::Hidden {
                    p.steps.push(Step::Tags(TagStep { file: f.clone(), unhide: true, reason: "lyrics put where players read them".into(), ..Default::default() }));
                }
            }
        }
        // lyrics that are missing, or carry lines that are not lyrics: filled, replaced or cleaned. Each file
        // is looked at again now, and lyrics that are fine are never touched
        "lyrics-fill" | "lyrics-junk" => {
            use crate::importer::lyrics::{self, Act, Choice, Place};
            let lookup = tools.lookup();
            let fill = ask.fix == "lyrics-fill";
            let choice = if ask.choice == "cut" { Choice::Cut } else { Choice::Replace };
            let mut unasked: Vec<String> = vec![];
            for f in &files {
                let info = tags_of(f);
                let l = healthscan::lyric_of(ffprobe, f, &info);
                if (fill && !l.missing()) || (!fill && !l.has_junk()) {
                    continue; // put right since the check
                }
                let asks = fill || choice == Choice::Replace;
                let found = if asks { lookup.find(&healthscan::lyric_artist_of(&info, &ask.artist), info.tags.get("title").map(|s| s.as_str()).unwrap_or(""), info.duration) } else { None };
                if asks && found.is_none() {
                    unasked.push(leaf(f)); // nothing is known about the song: its lyrics stay as they are
                    continue;
                }
                let hidden = l.place == Place::Hidden;
                // the light undo record holds the words of one lyrics frame: a file with more (another
                // language, a timed copy) keeps its whole old copy instead, so Undo brings all of them back
                // (Codex's review, H1). So does a file whose frames cannot be read here.
                let one_frame = crate::importer::id3::frames(f).is_some_and(|fr| fr.iter().filter(|x| x.id == "USLT").count() <= 1 && !fr.iter().any(|x| x.id == "SYLT"));
                let step = |lyrics: Option<String>, mode: &'static str, reason: &str| {
                    let light = f.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3")) && !hidden && one_frame;
                    Step::Tags(TagStep { file: f.clone(), lyrics, lyrics_mode: mode, unhide: hidden, light, reason: reason.into(), ..Default::default() })
                };
                match lyrics::act(&l, found.as_ref(), choice, lookup.timed) {
                    Act::Nothing => {}
                    Act::Set { text, over: false } => p.steps.push(step(Some(text), "missing", "lyrics from LRCLIB")),
                    Act::Set { text, over: true } => p.steps.push(step(Some(text), "set", if fill || found.as_ref().is_some_and(|a| a.text(lookup.timed).is_some()) && choice == Choice::Replace { "lyrics replaced from LRCLIB" } else { "junk lines cut from the lyrics" })),
                    Act::Clear => p.steps.push(step(None, "clear", "lyrics that were only junk taken out")),
                }
            }
            if !unasked.is_empty() {
                let which = healthscan::list_of(&unasked, 3);
                p.problems.push(if lookup.online {
                    format!("LRCLIB could not be asked about {which} (no title or length to ask by, or the site did not answer). Try again later.")
                } else {
                    format!("Looking for lyrics online is switched off for this run, so {which} stayed as {}.", if unasked.len() == 1 { "it is" } else { "they are" })
                });
            }
        }
        "strip-junk" => {
            let rows: Vec<serde_json::Value> = serde_json::from_str(fact("junk")).unwrap_or_default();
            let mut by_file: Vec<TagStep> = vec![];
            for r in &rows {
                let (Some(file), Some(field), Some(value)) = (r[0].as_str(), r[1].as_str(), r[2].as_str()) else { continue };
                let file = PathBuf::from(file);
                if !inside(&file, library) || !file.is_file() {
                    continue;
                }
                match by_file.iter_mut().find(|s| s.file == file) {
                    Some(s) => s.junk.push((field.to_string(), value.to_string())),
                    None => by_file.push(TagStep { file, junk: vec![(field.to_string(), value.to_string())], reason: "remove junk tags".into(), ..Default::default() }),
                }
            }
            p.steps.extend(by_file.into_iter().map(Step::Tags));
        }
        "album-artist" | "album-name" => {
            let (tag, reason) = if ask.fix == "album-artist" { ("album_artist", "consistent album artist") } else { ("album", "consistent album name") };
            let want = fact("default").trim();
            if want.is_empty() {
                p.problems.push("The check did not say which name to settle on.".into());
                return p;
            }
            for f in files {
                let info = tags_of(&f);
                if !info.ok {
                    p.problems.push(format!("{}: it cannot be read, left alone", leaf(&f)));
                } else if info.tags.get(tag).map(|s| s.as_str()).unwrap_or("") != want {
                    p.steps.push(Step::Tags(TagStep { file: f, set: BTreeMap::from([(tag.to_string(), want.to_string())]), reason: reason.into(), ..Default::default() }));
                }
            }
        }
        "missing-tags" => {
            let infos: Vec<(PathBuf, healthscan::Info)> = files.into_iter().map(|f| { let i = tags_of(&f); (f, i) }).collect();
            let loose = ask.album.is_empty() || ask.album.starts_with('(');
            let album = most_common(infos.iter().map(|(_, i)| i.tags.get("album").cloned().unwrap_or_default())).unwrap_or_else(|| strip_folder_year(&ask.album));
            for (f, info) in infos {
                if !info.ok {
                    p.problems.push(format!("{}: it cannot be read, left alone", leaf(&f)));
                    continue;
                }
                let tag = |k: &str| info.tags.get(k).map(|s| s.trim()).unwrap_or("");
                let parsed = crate::importer::scan::parse_file_name(&file_stem(&leaf(&f)), &album);
                let mut set = BTreeMap::new();
                if tag("title").is_empty() {
                    if let Some(t) = &parsed.title {
                        set.insert("title".to_string(), t.clone());
                    }
                }
                let no_track = crate::importer::scan::lead_number(tag("track")).unwrap_or(0) == 0;
                if no_track {
                    if let Some(n) = parsed.track.filter(|n| *n > 0 && *n <= 200) {
                        set.insert("track".to_string(), n.to_string());
                    }
                }
                if tag("album").is_empty() && !album.is_empty() && !loose {
                    set.insert("album".to_string(), album.clone());
                }
                if tag("artist").is_empty() {
                    let aa = if tag("album_artist").is_empty() { tag("albumartist") } else { tag("album_artist") };
                    set.insert("artist".to_string(), if aa.is_empty() { ask.artist.clone() } else { aa.to_string() });
                }
                if set.is_empty() {
                    if no_track || tag("title").is_empty() {
                        p.problems.push(format!("{}: the missing tags cannot be worked out from the file's name", leaf(&f)));
                    }
                    continue;
                }
                p.steps.push(Step::Tags(TagStep { file: f, set, reason: "fill in missing tags".into(), ..Default::default() }));
            }
        }
        "garbled" => {
            for f in files {
                let info = tags_of(&f);
                let mut set = BTreeMap::new();
                for k in ["title", "artist", "album", "album_artist"] {
                    let v = info.tags.get(k).map(|s| s.as_str()).unwrap_or("");
                    match rules::repair_text(v) {
                        Some(fixed) if fixed != v => {
                            set.insert(k.to_string(), fixed);
                        }
                        _ if rules::test_garbled(v) => p.problems.push(format!("{}: the right spelling of '{}' cannot be worked out (the letters are gone)", leaf(&f), rules::short(v, 40))),
                        _ => {}
                    }
                }
                if !set.is_empty() {
                    p.steps.push(Step::Tags(TagStep { file: f, set, reason: "fix garbled text".into(), ..Default::default() }));
                }
            }
        }
        "tracklist" | "tracklist-titles" => {
            let rows: Vec<serde_json::Value> = serde_json::from_str(fact("changes")).unwrap_or_default();
            for r in &rows {
                let Some(file) = r[0].as_str().map(PathBuf::from).filter(|f| inside(f, library) && f.is_file()) else { continue };
                let mut set = BTreeMap::new();
                for (n, k) in [(1, "title"), (2, "track"), (3, "disc")] {
                    if let Some(v) = r[n].as_str().map(|v| v.trim()).filter(|v| !v.is_empty()) {
                        // a number that is not a number is not written
                        if k == "title" || v.parse::<u32>().is_ok_and(|n| n > 0 && n <= 999) {
                            set.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                if !set.is_empty() {
                    p.steps.push(Step::Tags(TagStep { file, set, reason: "match the official track list".into(), ..Default::default() }));
                }
            }
        }
        "vbr-header" => {
            p.steps.extend(files.into_iter().filter(|f| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"))).map(|f| Step::Tags(TagStep { file: f, vbr: true, reason: "add VBR header".into(), ..Default::default() })));
        }
        "year-genre" => {
            let (want_year, want_genre) = (fact("year") == "1", fact("genre") == "1");
            let infos: Vec<(PathBuf, healthscan::Info)> = files.into_iter().map(|f| { let i = tags_of(&f); (f, i) }).collect();
            let Some(first) = infos.first().map(|x| &x.1) else { return p };
            let tag = |k: &str| first.tags.get(k).map(|s| s.trim()).unwrap_or("").to_string();
            let artist = [tag("album_artist"), tag("albumartist"), tag("artist"), ask.artist.clone()].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
            let album = if tag("album").is_empty() { strip_folder_year(&ask.album) } else { tag("album") };
            // the year the folder's name carries, else what Apple Music says; the genre only from there
            let mut year = crate::util::folder_year(&ask.album).or(LEAD_YEAR.captures(&ask.album).and_then(|c| c[1].parse().ok())).map(|y| y.to_string());
            let mut genre = None;
            if tools.online && ((want_year && year.is_none()) || want_genre) {
                let mut c = crate::importer::ctx::Ctx::new(library.to_path_buf(), library.to_path_buf(), false, tools.ffmpeg.clone(), tools.ffprobe.clone());
                c.lookup_cache = Some(crate::config::app_dir().join("cache").join("tracklists"));
                if let Some(a) = crate::importer::lookup::find_apple_album(&c, &artist, &album, infos.len()) {
                    if year.is_none() && !crate::importer::lookup::is_reissue(&a.collection_name, &album) {
                        year = a.release_date.get(..4).filter(|y| y.chars().all(|c| c.is_ascii_digit())).map(|y| y.to_string());
                    }
                    genre = Some(a.genre).filter(|g| !g.is_empty());
                }
            }
            if want_year && year.is_none() {
                p.problems.push(format!("The year of '{album}' was not found (not in the folder's name or on Apple Music)"));
            }
            if want_genre && genre.is_none() {
                p.problems.push(format!("A genre for '{album}' was not found on Apple Music"));
            }
            for (f, info) in infos {
                let has = |k: &str| info.tags.get(k).is_some_and(|v| !v.trim().is_empty());
                let mut set = BTreeMap::new();
                if let Some(y) = year.as_ref().filter(|_| want_year && !has("date") && !has("year")) {
                    set.insert("year".to_string(), y.clone());
                }
                if let Some(g) = genre.as_ref().filter(|_| want_genre && !has("genre")) {
                    set.insert("genre".to_string(), g.clone());
                }
                if info.ok && !set.is_empty() {
                    p.steps.push(Step::Tags(TagStep { file: f, set, reason: "fill in year/genre".into(), ..Default::default() }));
                }
            }
        }
        "cover" => {
            let mode = fact("mode");
            let folder = PathBuf::from(&ask.path);
            let infos: Vec<(PathBuf, healthscan::Info)> = files.into_iter().map(|f| { let i = tags_of(&f); (f, i) }).collect();
            let work = std::env::temp_dir().join("music-recs-repair").join(format!("{:x}", crate::importer::scan::rand_id()));
            let _ = std::fs::create_dir_all(&work);
            if mode != "folder" && mode != "partial" {
                // no cover at all, or a small one: the original is looked for online (Apple Music, then the others)
                if !tools.online {
                    p.problems.push("Looking for a cover online is switched off for this run.".into());
                    return p;
                }
                let Some(first) = infos.first().map(|x| &x.1) else { return p };
                let tag = |k: &str| first.tags.get(k).map(|s| s.trim()).unwrap_or("").to_string();
                let artist = [tag("album_artist"), tag("albumartist"), ask.artist.clone()].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
                let album = if tag("album").is_empty() { strip_folder_year(&ask.album) } else { tag("album") };
                let mut c = crate::importer::ctx::Ctx::new(library.to_path_buf(), library.to_path_buf(), false, tools.ffmpeg.clone(), tools.ffprobe.clone());
                c.lookup_cache = Some(crate::config::app_dir().join("cache").join("tracklists"));
                let apple = crate::importer::lookup::find_apple_album(&c, &artist, &album, infos.len());
                let Some(cover) = crate::importer::lookup::find_cover(&c, &artist, &album, apple.as_ref()).filter(|cv| cv.default) else {
                    p.problems.push(format!("The original cover for '{album}' was not found online"));
                    return p;
                };
                let Some((full, embed, size, name)) = crate::importer::import::save_cover_in(&c, &cover, &work) else {
                    p.problems.push(format!("The cover was found on {} but could not be downloaded", cover.source));
                    return p;
                };
                let width = |s: &str| s.split('x').next().and_then(|w| w.trim().parse::<u32>().ok()).unwrap_or(0);
                if width(&size) == 0 || width(&size) > 12_000 {
                    p.problems.push(format!("The picture from {} is not a usable cover ({size})", cover.source));
                    return p;
                }
                if mode == "small" && width(&size) <= width(fact("size")) {
                    p.problems.push(format!("The best cover online ({size}) is not larger than the one there"));
                    return p;
                }
                // the full-size picture beside the tracks: in place of a small cover file, or where there is none
                // (only a picture counts: "front.txt" is not the cover)
                let picture = |f: &Path| f.extension().is_some_and(|e| ["jpg", "jpeg", "png", "webp", "gif", "bmp"].contains(&e.to_string_lossy().to_lowercase().as_str()));
                let there = std::fs::read_dir(&folder).into_iter().flatten().flatten().map(|e| e.path()).find(|f| f.is_file() && picture(f) && f.file_stem().is_some_and(|s| ["cover", "folder", "front"].contains(&s.to_string_lossy().to_lowercase().as_str())));
                let ext = full.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "jpg".into());
                match there {
                    Some(old) if mode == "small" => {
                        // a better cover of another kind ("cover.png" for "cover.jpg"): the old one goes to the
                        // backups too, so the folder is not left with two covers
                        let new = old.with_extension(&ext);
                        if new != old {
                            p.steps.push(Step::Remove { path: old.clone(), reason: "replaced by a better cover".into() });
                        }
                        p.steps.push(Step::Create { path: new, source: full, replace: true, reason: "save the full-size cover".into() });
                    }
                    None => p.steps.push(Step::Create { path: folder.join(name), source: full, replace: false, reason: "save the full-size cover".into() }),
                    Some(_) => {}
                }
                p.steps.extend(infos.into_iter().filter(|(_, i)| i.ok).map(|(f, _)| Step::Tags(TagStep { file: f, cover: Some(embed.clone()), cover_all: true, reason: "embed cover".into(), ..Default::default() })));
                return p;
            }
            let embed = work.join("cover.jpg");
            // the picture to embed: the folder's own cover, else one a track already carries
            let source = folder_cover(&folder).or_else(|| infos.iter().find(|(_, i)| i.has_art).map(|(f, _)| f.clone()));
            let made = source.as_ref().is_some_and(|src| {
                let mut c = tools::command(&tools.ffmpeg, true);
                c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]).args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(src).args(["-map", "0:v:0", "-frames:v", "1", "-vf", "scale='min(1400,iw)':-1", "-q:v", "2"]).arg(&embed);
                c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                tools::run_limited(&mut c, 60) && embed.is_file()
            });
            if !made {
                p.problems.push("No usable cover was found in the folder or in the tracks".into());
                return p;
            }
            p.steps.extend(infos.into_iter().filter(|(_, i)| i.ok && !i.has_art).map(|(f, _)| Step::Tags(TagStep { file: f, cover: Some(embed.clone()), reason: "embed cover".into(), ..Default::default() })));
        }
        "unwrap" => p.steps.extend(files.into_iter().map(|f| Step::Unwrap { file: f, reason: "unwrap MP3 from WAV".into() })),
        other => p.problems.push(format!("The app cannot do the repair \"{}\" yet.", other.replace('-', " "))),
    }
    p
}

static LEAD_YEAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\s*((?:19|20)\d{2})\s+-\s+").unwrap());

/// "Album (2017)", "2017 - Album", "[2017] Album" -> "Album"
fn strip_folder_year(s: &str) -> String {
    static LEAD_BRACKET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\s*\[\d{4}\]\s*").unwrap());
    LEAD_BRACKET.replace(&strip_year(s), "").trim().to_string()
}

/// The album folder's cover picture: one named cover, folder or front in the folder itself, else the
/// largest picture there, else the largest in a folder inside it. A booklet scan in "Scans" never beats a
/// cover beside the tracks, and a file that only claims an impossible size is not a picture.
fn folder_cover(folder: &Path) -> Option<PathBuf> {
    let is_picture = |p: &PathBuf| p.extension().is_some_and(|e| ["jpg", "jpeg", "png"].contains(&e.to_string_lossy().to_lowercase().as_str()));
    let area = |p: &PathBuf| healthscan::image_size(p).map(|(w, h)| w as u64 * h as u64).unwrap_or(0);
    let own: Vec<PathBuf> = std::fs::read_dir(folder).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_picture(p)).collect();
    let named = own.iter().filter(|p| p.file_stem().is_some_and(|s| ["cover", "folder", "front"].contains(&s.to_string_lossy().to_lowercase().as_str()))).max_by_key(|p| area(p));
    if let Some(p) = named.or(own.iter().filter(|p| area(p) > 0).max_by_key(|p| area(p))) {
        return Some(p.clone());
    }
    crate::importer::util::walk_files(folder).into_iter().filter(|p| is_picture(p) && area(p) > 0).max_by_key(area)
}

/// Runs the tag editor over copies of the files. For each copy (by its path, lower case): Ok(true) = it
/// was changed and checked, Ok(false) = nothing needed changing, Err(why) = it could not be changed safely.
fn run_tag_editor(items: &[(PathBuf, &TagStep)], ffmpeg: &Path) -> HashMap<String, Result<(bool, String), String>> {
    let mut out = HashMap::new();
    let plan: Vec<serde_json::Value> = items
        .iter()
        .map(|(copy, s)| {
            serde_json::json!({
                "File": copy.to_string_lossy(), "Set": s.set, "Junk": s.junk.iter().map(|(f, v)| serde_json::json!([f, v])).collect::<Vec<_>>(),
                "Cover": s.cover.as_ref().map(|c| c.to_string_lossy().to_string()), "CoverMode": if s.cover_all { "all" } else { "missing" }, "Vbr": s.vbr,
                "Lyrics": s.lyrics, "LyricsMode": s.lyrics_mode, "Unhide": s.unhide, "Exact": s.light, "OnlyIf": s.only_if,
            })
        })
        .collect();
    let rows = crate::tagwrite::run(&plan, ffmpeg);
    for r in rows {
        let (why, note) = (r["Why"].as_str().unwrap_or(""), r["Note"].as_str().unwrap_or(""));
        let said = if r["Ok"].as_bool() != Some(true) {
            Err(if why.is_empty() { "the tag editor could not change it".to_string() } else { why.to_string() })
        } else if note.starts_with("length header not fixed") {
            Err(note.to_string())
        } else {
            // (with what the lyrics field held before, when this changed it)
            Ok((why != "nothing needed changing" || note.starts_with("added"), r["OldLyrics"].as_str().unwrap_or("").to_string()))
        };
        out.insert(r["File"].as_str().unwrap_or("").to_lowercase(), said);
    }
    for (copy, _) in items {
        out.entry(copy.to_string_lossy().to_lowercase()).or_insert_with(|| Err("the tag editor did not report back".into()));
    }
    out
}

/// Why a thing the check called removable may not be removed now (None = it may). The check may be a day
/// old: a folder that was empty then can hold an album today.
fn why_not_remove(path: &Path, cat: &str) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match cat {
        "Empty folder" => (!path.is_dir() || !is_empty_dir(path)).then(|| "it is no longer empty".to_string()),
        "Leftover import folder" => {
            if !path.is_dir() || !leaf(path).to_lowercase().ends_with(".importing") {
                Some("it is not a leftover import folder".into())
            } else if healthscan::quiet_seconds(path) < healthscan::ABANDONED_SECS {
                Some("an import is writing to it right now".into())
            } else {
                None
            }
        }
        "other copy of the same album" => (!path.is_dir()).then(|| "it is not a folder".to_string()),
        _ => (!path.is_file()).then(|| "it is not a file".to_string()),
    }
}

/// Carries a plan out, each step through the journal. A step that cannot be done is left undone and said;
/// the others still happen. Each thing is looked at once more just before it is changed.
pub fn apply(plan: &Plan, title: &str, library: &Path, root: &Path, tools: &Tools) -> Result<Done, String> {
    let mut done = Done { problems: plan.problems.clone(), ..Default::default() };
    if plan.steps.is_empty() {
        return Ok(done);
    }
    let batch = Batch::begin(root, library, title)?;
    apply_into(&batch, plan, library, tools, &mut done);
    done.changed = batch.changes();
    done.batch = (done.changed > 0).then(|| batch.id.clone());
    batch.close();
    Ok(done)
}

/// "Fix all": the same repair for many findings, one after the other, in one batch (one line in Repair
/// history, one Undo). `each` is called after every finding with (its place in the list, whether all of it
/// was done) and says whether to go on. Returns what happened in all.
pub fn apply_all(asks: &[Ask], title: &str, library: &Path, root: &Path, tools: &Tools, mut each: impl FnMut(usize, bool) -> bool) -> Result<Done, String> {
    let batch = Batch::begin(root, library, title)?;
    let mut done = Done::default();
    for (n, ask) in asks.iter().enumerate() {
        let plan = plan(ask, library, tools);
        let before = done.problems.len();
        done.problems.extend(plan.problems.iter().cloned());
        apply_into(&batch, &plan, library, tools, &mut done);
        if !each(n, done.problems.len() == before) {
            break;
        }
    }
    done.changed = batch.changes();
    done.batch = (done.changed > 0).then(|| batch.id.clone());
    batch.close();
    Ok(done)
}

/// Undoes a repair (see `undo::undo`). Lyrics that a repair changed without keeping the old file are put
/// back first, through the tag editor, from the words the undo record holds: each file only if its lyrics
/// are still what the repair wrote (lyrics changed since then are somebody's later work, and stay). What
/// comes back is what could not be undone, and which lyrics were left alone because they had changed.
pub fn undo(root: &Path, id: &str) -> Result<Vec<String>, String> {
    let lines = crate::undo::lyric_lines(root, id)?;
    let mut notes = vec![];
    if !lines.is_empty() {
        let ffmpeg = tools::find_tool("ffmpeg")?;
        let work = root.join(id).join("undo-work");
        let steps: Vec<TagStep> = lines
            .iter()
            .filter(|(file, _, _)| file.is_file())
            .map(|(file, old, new)| TagStep { file: file.clone(), lyrics: Some(old.clone()).filter(|o| !o.trim().is_empty()), lyrics_mode: if old.trim().is_empty() { "clear" } else { "set" }, light: true, only_if: Some(new.clone()), ..Default::default() })
            .collect();
        let mut items: Vec<(PathBuf, &TagStep)> = vec![];
        let mut snaps: Vec<crate::library_lock::Snapshot> = vec![];
        let mut problems = vec![];
        for (n, t) in steps.iter().enumerate() {
            let copy = work.join(n.to_string()).join(leaf(&t.file));
            if locked(&t.file) {
                problems.push(format!("{}: it is open in another program", leaf(&t.file)));
                continue;
            }
            match std::fs::create_dir_all(copy.parent().unwrap_or(&work)).and_then(|_| crate::library_lock::copy_with_snapshot(&t.file, &copy)) {
                Err(e) => problems.push(crate::util::plain_err(format!("{}: a working copy could not be made: {e}", leaf(&t.file)))),
                Ok(snap) => {
                    items.push((copy, t));
                    snaps.push(snap);
                }
            }
        }
        let said = if items.is_empty() { HashMap::new() } else { run_tag_editor(&items, &ffmpeg) };
        // (the library is held from here to the end of the Undo, and each file must still be what its copy
        // was made from: a track an import put in place meanwhile is never written over)
        let _held = crate::library_lock::hold();
        for ((copy, t), snap) in items.iter().zip(&snaps) {
            match said.get(&copy.to_string_lossy().to_lowercase()) {
                Some(Ok((true, _))) => {
                    if let Err(why) = crate::library_lock::still(&t.file, snap) {
                        problems.push(format!("{}: {why}", leaf(&t.file)));
                    } else if let Err(e) = std::fs::rename(copy, &t.file) {
                        problems.push(crate::util::plain_err(format!("{}: the copy with the old lyrics could not be put in place: {e}", leaf(&t.file))));
                    }
                }
                Some(Ok((false, _))) => {} // already as it was
                Some(Err(why)) if why.starts_with("its lyrics were changed after the repair") => notes.push(format!("{}: {why}", leaf(&t.file))),
                Some(Err(why)) => problems.push(format!("{}: {why}", leaf(&t.file))),
                None => problems.push(format!("{}: the tag editor did not report back", leaf(&t.file))),
            }
        }
        let _ = std::fs::remove_dir_all(&work);
        if !problems.is_empty() {
            return Ok(problems); // not marked undone: Undo can be run again for what is left
        }
    }
    let mut left = crate::undo::undo(root, id)?;
    left.extend(notes);
    Ok(left)
}

/// The kinds "Fix all" may run over a whole category: the ones with one way to go that neither move nor
/// set aside anything of the album's own (which copy of a song or an album stays is looked at one by one).
pub fn can_fix_all(kind: &str) -> bool {
    matches!(kind, "remove" | "strip-junk" | "garbled" | "missing-tags" | "tracklist" | "tracklist-titles" | "year-genre" | "vbr-header" | "cover" | "unwrap" | "lyrics-fill" | "lyrics-junk" | "lyrics-unhide")
}

/// The audio of a file as a fingerprint that tags and wrappers do not change (None if it cannot be read).
fn audio_md5(ffmpeg: &Path, file: &Path, force: Option<&str>) -> Option<String> {
    let mut c = tools::command(ffmpeg, true);
    c.args(["-hide_banner", "-nostdin", "-v", "error"]);
    if let Some(f) = force {
        c.args(["-f", f]);
    }
    c.args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(file).args(["-map", "0:a", "-c", "copy", "-f", "md5", "-"]);
    let (out, _) = tools::capture_limited(&mut c, 180)?;
    Some(String::from_utf8_lossy(&out).trim().to_string()).filter(|s| s.starts_with("MD5="))
}

/// The plain MP3 inside a WAV wrapper: the tag at the front, the frames from the wrapper's "data" part, the
/// old tag at the end. Err = why this file is not the kind that can be unwrapped.
fn unwrapped(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let id3 = if bytes.len() >= 10 && &bytes[..3] == b"ID3" {
        let size = (bytes[6] as usize & 0x7f) << 21 | (bytes[7] as usize & 0x7f) << 14 | (bytes[8] as usize & 0x7f) << 7 | (bytes[9] as usize & 0x7f);
        (10 + size + if bytes[5] & 0x10 != 0 { 10 } else { 0 }).min(bytes.len())
    } else {
        0
    };
    if bytes.len() < id3 + 12 || &bytes[id3..id3 + 4] != b"RIFF" {
        return Err("it is not the WAV-wrapped kind this can unwrap".into());
    }
    let mut p = id3 + 12;
    let mut data: Option<(usize, usize)> = None;
    while p + 8 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7]]) as usize;
        if &bytes[p..p + 4] == b"data" {
            data = Some((p + 8, len.min(bytes.len() - (p + 8))));
            break;
        }
        p = p.saturating_add(8).saturating_add(len).saturating_add(len % 2);
    }
    let Some((at, len)) = data.filter(|d| d.1 > 0) else { return Err("the audio inside the wrapper was not found".into()) };
    let mut out = bytes[..id3].to_vec();
    out.extend_from_slice(&bytes[at..at + len]);
    if bytes.len() > 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG" && at + len <= bytes.len() - 128 {
        out.extend_from_slice(&bytes[bytes.len() - 128..]);
    }
    Ok(out)
}

/// Carries one plan's steps out in a batch that is already open.
fn apply_into(batch: &Batch, plan: &Plan, library: &Path, tools: &Tools, done: &mut Done) {
    // each step looks at what it changes and changes it with the library held, so nothing (an import, an
    // Undo) can change it in between
    let held = crate::library_lock::hold();
    for step in &plan.steps {
        let said = match step {
            Step::Remove { path, reason } => {
                if !path.exists() {
                    Ok(())
                } else if let Some(why) = why_not_remove(path, reason) {
                    Err(why)
                } else if path.is_file() && locked(path) {
                    Err("it is open in another program".into())
                } else {
                    batch.remove(path, reason).map(|_| batch.tidy_parents(path, library))
                }
            }
            Step::Move { from, to, reason } => {
                if !from.exists() {
                    Err("it is no longer there".into())
                } else if from.is_file() && locked(from) {
                    Err("it is open in another program".into())
                } else {
                    batch.moved(from, to, reason).map(|_| batch.tidy_parents(from, library))
                }
            }
            Step::Tags(_) => Ok(()), // all together, below
            Step::Create { path, source, replace, reason } => {
                let cleared = if path.exists() && *replace { batch.remove(path, "replaced by a better cover").map(|_| ()) } else { Ok(()) };
                cleared.and_then(|_| batch.created(path, source, reason))
            }
            Step::Unwrap { file, reason } => (|| {
                if !file.is_file() {
                    return Err("it is no longer there".to_string());
                }
                if locked(file) {
                    return Err("it is open in another program".into());
                }
                if std::fs::metadata(file).map(|m| m.len()).unwrap_or(0) > 1_000_000_000 {
                    return Err("it is too large to unwrap".into());
                }
                let plain = unwrapped(&std::fs::read(file).map_err(|e| crate::util::plain_err(format!("it could not be read: {e}")))?)?;
                let copy = batch.dir.join("work").join(format!("unwrap-{:x}", crate::importer::scan::rand_id())).join(leaf(file));
                std::fs::create_dir_all(copy.parent().unwrap_or(&batch.dir)).and_then(|_| std::fs::write(&copy, plain)).map_err(|e| crate::util::plain_err(format!("a working copy could not be made: {e}")))?;
                // the frames must be the very same before the plain file takes the wrapped one's place
                let (was, is) = (audio_md5(&tools.ffmpeg, file, Some("mp3")), audio_md5(&tools.ffmpeg, &copy, None));
                if was.is_none() || was != is {
                    return Err("the unwrapped file did not match the original audio, so nothing was saved".into());
                }
                batch.replaced(file, &copy, reason)
            })(),
        };
        if let Err(why) = said {
            let what = match step {
                Step::Remove { path, .. } => path,
                Step::Move { from, .. } => from,
                Step::Tags(t) => &t.file,
                Step::Create { path, .. } => path,
                Step::Unwrap { file, .. } => file,
            };
            done.problems.push(format!("{}: {why}", leaf(what)));
        }
    }
    drop(held);
    // the files whose tags change: a copy of each in the batch's folder, the editor over the copies, and
    // each copy that was changed and checked takes its file's place. The library is not held while the
    // editor works; instead each file must still be what its copy was made from when the copy is saved
    let tag_steps: Vec<&TagStep> = plan.steps.iter().filter_map(|s| if let Step::Tags(t) = s { Some(t) } else { None }).collect();
    if !tag_steps.is_empty() {
        let work = batch.dir.join("work");
        let mut items: Vec<(PathBuf, &TagStep)> = vec![];
        let mut snaps: HashMap<String, crate::library_lock::Snapshot> = HashMap::new();
        for (n, t) in tag_steps.iter().enumerate() {
            let why = if !t.file.is_file() {
                Some("it is no longer there".to_string())
            } else if locked(&t.file) {
                Some("it is open in another program".to_string())
            } else {
                let copy = work.join(n.to_string()).join(leaf(&t.file));
                match std::fs::create_dir_all(copy.parent().unwrap_or(&work)).and_then(|_| crate::library_lock::copy_with_snapshot(&t.file, &copy)) {
                    Ok(snap) => {
                        snaps.insert(copy.to_string_lossy().to_lowercase(), snap);
                        items.push((copy, t));
                        None
                    }
                    Err(e) => Some(crate::util::plain_err(format!("a working copy could not be made: {e}"))),
                }
            };
            if let Some(why) = why {
                done.problems.push(format!("{}: {why}", leaf(&t.file)));
            }
        }
        if !items.is_empty() {
            if let Some(cache) = &tools.quality_cache {
                quality::load_cache(cache);
            }
            let said = run_tag_editor(&items, &tools.ffmpeg);
            #[cfg(test)]
            test_pause::after_tags();
            let _held = crate::library_lock::hold();
            for (copy, t) in &items {
                let key = copy.to_string_lossy().to_lowercase();
                match said.get(&key) {
                    Some(Ok((true, old_lyrics))) => {
                        let heard = quality::key_of(&t.file);
                        // a change to the lyrics alone keeps the old words in the undo record, not the old file
                        let swapped = snaps.get(&key).ok_or_else(|| "its working copy was not written down".to_string()).and_then(|snap| crate::library_lock::still(&t.file, snap)).and_then(|_| {
                            if t.light { batch.lyrics_changed(&t.file, copy, old_lyrics, t.lyrics.as_deref().unwrap_or(""), &t.reason) } else { batch.replaced(&t.file, copy, &t.reason) }
                        });
                        match swapped {
                            Ok(()) => quality::carry(heard, &t.file), // the audio is the same, so what was heard stands
                            Err(why) => done.problems.push(format!("{}: {why}", leaf(&t.file))),
                        }
                    }
                    Some(Ok((false, _))) => {}
                    Some(Err(why)) => done.problems.push(format!("{}: {why}", leaf(&t.file))),
                    None => done.problems.push(format!("{}: the tag editor did not report back", leaf(&t.file))),
                }
            }
            if let Some(cache) = &tools.quality_cache {
                quality::save_cache(cache);
            }
        }
        // the working copies that were not used are the batch's own scratch, not anything from the library
        let _ = std::fs::remove_dir_all(&work);
    }
    let _ = std::fs::remove_dir_all(batch.dir.join("work"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::importer::tools;

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
        let base = std::env::temp_dir().join(format!("music-recs-repair-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let tools = Tools { ffmpeg: tools::find_tool("ffmpeg").unwrap(), ffprobe: tools::find_tool("ffprobe").unwrap(), quality_cache: None, online: false, lyrics: None };
        Lab { root: base.join("backups"), base, lib, tools }
    }
    impl Lab {
        fn tone(&self, rel: &str, tags: &[&str]) -> PathBuf {
            let out = self.lib.join(rel);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            let mut c = tools::command(&self.tools.ffmpeg, false);
            c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-c:a", "libmp3lame", "-b:a", "128k"]);
            for t in tags {
                c.args(["-metadata", t]);
            }
            c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            assert!(tools::run_limited(&mut c, 60));
            out
        }
        fn ask(&self, fix: &str, cat: &str, path: &Path, files: &[&Path]) -> Ask {
            Ask { fix: fix.into(), cat: cat.into(), path: path.to_string_lossy().to_string(), files: files.iter().map(|f| f.to_string_lossy().to_string()).collect(), artist: "A".into(), album: leaf(path), data: HashMap::new(), choice: String::new() }
        }
        fn fix(&self, ask: &Ask) -> Done {
            apply(&plan(ask, &self.lib, &self.tools), "a test repair", &self.lib, &self.root, &self.tools).unwrap()
        }
    }

    #[test]
    fn an_empty_folder_is_removed_only_while_it_is_still_empty() {
        let l = lab("empty");
        let (empty, filled) = (l.lib.join("A").join("Empty"), l.lib.join("A").join("Was Empty (2024)"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&filled).unwrap();
        // the check saw two empty folders; since then an album arrived in one
        std::fs::write(filled.join("01 new.mp3"), b"an album that arrived after the check").unwrap();
        let done = l.fix(&l.ask("remove", "Empty folder", &empty, &[&empty]));
        assert_eq!((done.changed, done.problems.len()), (1, 0));
        assert!(!empty.exists() && l.lib.join("A").exists());
        let done = l.fix(&l.ask("remove", "Empty folder", &filled, &[&filled]));
        assert_eq!((done.changed, done.batch.clone(), done.problems), (0, None, vec!["Was Empty (2024): it is no longer empty".to_string()]));
        assert!(filled.join("01 new.mp3").exists());
        // nothing was changed by the second repair, so it left no record; the first one's Undo brings the folder back
        let list = crate::undo::batches(&l.root);
        assert_eq!(list.len(), 1);
        assert!(crate::undo::undo(&l.root, &list[0].id).unwrap().is_empty() && empty.is_dir());
        // a path outside the library is never touched, whatever a report says
        let outside = l.base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let done = l.fix(&l.ask("remove", "Empty folder", &outside, &[&outside, &l.lib, &l.lib.join("A").join("..").join("..").join("outside")]));
        assert_eq!((done.changed, done.problems.len()), (0, 3));
        assert!(outside.exists());
    }

    #[test]
    fn a_temp_file_and_an_abandoned_import_folder_go_to_the_backups_and_a_busy_one_stays() {
        let l = lab("junk");
        let part = l.lib.join("A").join("Album").join("03 x.mp3.part");
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"half a download").unwrap();
        let done = l.fix(&l.ask("remove", "Junk file", part.parent().unwrap(), &[&part]));
        // the album folder had nothing else in it, and the artist's folder nothing but that: they went too
        // (and come back with Undo)
        assert_eq!((done.changed, done.problems.len()), (3, 0));
        assert!(!part.exists() && !l.lib.join("A").exists() && l.lib.exists());

        let busy = l.lib.join("A").join("New.importing");
        std::fs::create_dir_all(&busy).unwrap();
        std::fs::write(busy.join("01.mp3"), b"arriving").unwrap();
        let done = l.fix(&l.ask("remove", "Leftover import folder", &busy, &[&busy]));
        assert_eq!((done.changed, done.problems), (0, vec!["New.importing: an import is writing to it right now".to_string()]));
        assert!(busy.join("01.mp3").exists());

        let list = crate::undo::batches(&l.root);
        assert!(crate::undo::undo(&l.root, &list[0].id).unwrap().is_empty());
        assert_eq!(std::fs::read(&part).unwrap(), b"half a download");
    }

    #[test]
    fn tracks_lying_in_the_music_folder_itself_go_to_their_artist_and_album() {
        let l = lab("stray");
        // "a" has a folder (spelled otherwise) with the album in it; "New Band" has no folder yet
        l.tone(r"The A\Rocket (2017)\01 One.mp3", &["artist=The A", "album=Rocket", "title=One"]);
        let two = l.tone("02 Two.mp3", &["artist=the a", "album=Rocket", "title=Two"]);
        let fresh = l.tone("Fresh.mp3", &["album_artist=New Band", "artist=New Band feat. X", "album=Debut", "title=Fresh", "date=2024"]);
        let nobody = l.tone("Nobody.mp3", &["album=Somewhere", "title=Nobody"]);
        let nowhere = l.tone("Nowhere.mp3", &["artist=The A", "title=Nowhere"]);
        let ask = l.ask("file-loose", "Loose tracks", &l.lib, &[&two, &fresh, &nobody, &nowhere]);
        let planned = plan(&ask, &l.lib, &l.tools);
        assert_eq!(
            planned.steps,
            vec![
                Step::Move { from: two.clone(), to: l.lib.join(r"The A\Rocket (2017)\02 Two.mp3"), reason: "file loose track".into() },
                Step::Move { from: fresh.clone(), to: l.lib.join(r"New Band\Debut (2024)\Fresh.mp3"), reason: "file loose track".into() },
            ]
        );
        assert_eq!(planned.problems, vec!["Nobody.mp3: no artist tag, so it is not clear whose it is".to_string(), "Nowhere.mp3: no album tag, so it is not clear where it belongs".to_string()]);
        let done = apply(&planned, "Loose tracks", &l.lib, &l.root, &l.tools).unwrap();
        assert_eq!(done.changed, 2);
        assert!(l.lib.join(r"The A\Rocket (2017)\02 Two.mp3").exists() && l.lib.join(r"New Band\Debut (2024)\Fresh.mp3").exists() && nobody.exists() && nowhere.exists());
        // Undo: both lie in the music folder again, and the folders made for the new band are gone
        assert!(crate::undo::undo(&l.root, done.batch.as_deref().unwrap()).unwrap().is_empty());
        assert!(two.exists() && fresh.exists() && !l.lib.join("New Band").exists());
    }

    #[test]
    fn loose_tracks_go_into_the_album_their_tags_name() {
        let l = lab("loose");
        let artist = l.lib.join("A");
        // one belongs to an album that is there (under a name with a year), one to an album that is not, one names none
        l.tone(r"A\Rocket (2017)\01 One.mp3", &["album=Rocket", "title=One"]);
        let two = l.tone(r"A\02 Two.mp3", &["album=Rocket", "title=Two", "date=2017"]);
        let single = l.tone(r"A\Single.mp3", &["album=Brand: New", "title=Single", "date=2021-03-04"]);
        let nameless = l.tone(r"A\Nameless.mp3", &["title=Nameless"]);
        // and one whose name is taken in the album it belongs to
        let clash = l.tone(r"A\01 One.mp3", &["album=Rocket", "title=One again"]);
        let ask = l.ask("file-loose", "Loose tracks", &artist, &[&two, &single, &nameless, &clash]);
        let planned = plan(&ask, &l.lib, &l.tools);
        assert_eq!(
            planned.steps,
            vec![
                Step::Move { from: two.clone(), to: artist.join("Rocket (2017)").join("02 Two.mp3"), reason: "file loose track".into() },
                Step::Move { from: single.clone(), to: artist.join("Brand - New (2021)").join("Single.mp3"), reason: "file loose track".into() },
            ]
        );
        assert_eq!(planned.problems, vec!["Nameless.mp3: no album tag, so it is not clear where it belongs".to_string(), "01 One.mp3: a file with that name is already in 'Rocket (2017)'".to_string()]);
        // planning changed nothing
        assert!(two.exists() && single.exists() && !artist.join("Brand - New (2021)").exists());

        let done = apply(&planned, "Loose tracks: A", &l.lib, &l.root, &l.tools).unwrap();
        assert_eq!((done.changed, done.problems.len()), (2, 2));
        assert!(artist.join("Rocket (2017)").join("02 Two.mp3").exists() && artist.join("Brand - New (2021)").join("Single.mp3").exists());
        assert!(nameless.exists() && clash.exists() && !two.exists());

        // Undo: both are loose again and the folder that was made for the single is gone
        assert!(crate::undo::undo(&l.root, done.batch.as_deref().unwrap()).unwrap().is_empty());
        assert!(two.exists() && single.exists() && !artist.join("Brand - New (2021)").exists() && artist.join("Rocket (2017)").join("01 One.mp3").exists());
    }

    #[test]
    fn words() {
        assert!(can_fix("remove") && can_fix("file-loose") && can_fix("cover") && can_fix("unwrap") && !can_fix("redownload") && !can_fix(""));
        assert_eq!(action_words("remove", "Empty folder", 1), "Remove this empty folder");
        assert_eq!(action_words("file-loose", "Loose tracks", 3), "File them in their album folders");
        let none = Tools { ffmpeg: "none".into(), ffprobe: "none".into(), quality_cache: None, online: false, lyrics: None };
        let ask = |fix: &str| Ask { fix: fix.into(), cat: "x".into(), path: String::new(), files: vec![], artist: String::new(), album: String::new(), data: HashMap::new(), choice: String::new() };
        assert_eq!(plan(&ask("resample"), Path::new(r"X:\lib"), &none).problems, vec!["The app cannot do the repair \"resample\" yet.".to_string()]);
        assert!(can_fix_all("strip-junk") && can_fix_all("remove") && !can_fix_all("dupe-tracks") && !can_fix_all("pick-album") && !can_fix_all("file-loose") && !can_fix_all("album-artist"));
        assert_eq!((strip_folder_year("[2017] Rocket"), strip_folder_year("Rocket (2017)"), strip_folder_year("2017 - Rocket")), ("Rocket".to_string(), "Rocket".to_string(), "Rocket".to_string()));
    }
}

#[cfg(test)]
mod tag_tests {
    //! The repairs that rewrite tags, on made-up tracks. They run the Rust tag editor over copies
    //! in a scratch folder.
    use super::*;

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
        let base = std::env::temp_dir().join(format!("music-recs-tagfix-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let tools = Tools { ffmpeg: tools::find_tool("ffmpeg").unwrap(), ffprobe: tools::find_tool("ffprobe").unwrap(), quality_cache: None, online: false, lyrics: None };
        Lab { root: base.join("backups"), base, lib, tools }
    }
    impl Lab {
        fn tone(&self, rel: &str, kbps: u32, tags: &[&str]) -> PathBuf {
            let out = self.lib.join(rel);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            let mut c = tools::command(&self.tools.ffmpeg, false);
            c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "libmp3lame", "-b:a", &format!("{kbps}k")]);
            for t in tags {
                c.args(["-metadata", t]);
            }
            c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            assert!(tools::run_limited(&mut c, 60));
            out
        }
        fn ask(&self, fix: &str, folder: &Path, files: &[&Path], data: &[(&str, String)]) -> Ask {
            Ask {
                fix: fix.into(),
                cat: "a finding".into(),
                path: folder.to_string_lossy().to_string(),
                files: files.iter().map(|f| f.to_string_lossy().to_string()).collect(),
                artist: "A".into(),
                album: leaf(folder),
                data: data.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
                choice: data.iter().find(|(k, _)| *k == "choice").map(|(_, v)| v.clone()).unwrap_or_default(),
            }
        }
        fn fix(&self, ask: &Ask) -> Done {
            apply(&plan(ask, &self.lib, &self.tools), "a test repair", &self.lib, &self.root, &self.tools).unwrap()
        }
        fn tag(&self, f: &Path, k: &str) -> String {
            let v = healthscan::probe(&self.tools.ffprobe, f).tags.get(k).cloned().unwrap_or_default();
            // the editor writes "02" or "2/12": the number is what matters
            if k == "track" { v.split('/').next().unwrap_or("").trim_start_matches('0').to_string() } else { v }
        }
    }

    #[test]
    fn tags_are_changed_on_a_copy_and_undo_brings_the_old_file_back() {
        let l = lab("tags");
        let folder = l.lib.join("A").join("Album (2020)");
        let one = l.tone(r"A\Album (2020)\01 One.mp3", 128, &["artist=A", "album=Album", "title=CafÃ©", "track=1", "album_artist=Somebody", "comment=Downloaded from www.example.com", "genre=Rock"]);
        let two = l.tone(r"A\Album (2020)\02 Two.mp3", 128, &["album_artist=A", "album=Album"]);
        let before = std::fs::read(&one).unwrap();
        let s = |p: &Path| p.to_string_lossy().to_string();

        // the junk comment goes, and nothing else does
        let junk = serde_json::json!([[s(&one), "comment", "Downloaded from www.example.com"]]).to_string();
        let done = l.fix(&l.ask("strip-junk", &folder, &[&one], &[("junk", junk)]));
        assert_eq!((done.changed, done.problems.clone()), (1, vec![]), "{done:?}");
        assert_eq!((l.tag(&one, "comment"), l.tag(&one, "album"), l.tag(&one, "genre")), (String::new(), "Album".to_string(), "Rock".to_string()));
        let strip = done.batch.clone().unwrap();
        // no working copy is left beside the track or in the batch
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 2);
        assert!(!l.root.join(&strip).join("work").exists());

        // one album artist on every track: only the track that differs is rewritten
        let done = l.fix(&l.ask("album-artist", &folder, &[&one, &two], &[("default", "A".into())]));
        assert_eq!((done.changed, done.problems.len()), (1, 0), "{done:?}");
        assert_eq!((l.tag(&one, "album_artist"), l.tag(&two, "album_artist")), ("A".to_string(), "A".to_string()));

        // scrambled text is repaired; missing tags come from the file's name and the folder
        let done = l.fix(&l.ask("garbled", &folder, &[&one, &two], &[]));
        assert_eq!((done.changed, l.tag(&one, "title")), (1, "Café".to_string()), "{done:?}");
        let done = l.fix(&l.ask("missing-tags", &folder, &[&one, &two], &[]));
        assert_eq!((done.changed, done.problems.len()), (1, 0), "{done:?}");
        assert_eq!((l.tag(&two, "title"), l.tag(&two, "track"), l.tag(&two, "artist")), ("Two".to_string(), "2".to_string(), "A".to_string()));

        // the official list's corrections; and the year the folder's name carries (the genre would need a lookup)
        let changes = serde_json::json!([[s(&two), "Dos", "3", ""], [s(&l.lib.join("nope.mp3")), "x", "", ""]]).to_string();
        let done = l.fix(&l.ask("tracklist", &folder, &[&two], &[("changes", changes)]));
        assert_eq!((done.changed, l.tag(&two, "title"), l.tag(&two, "track")), (1, "Dos".to_string(), "3".to_string()), "{done:?}");
        let done = l.fix(&l.ask("year-genre", &folder, &[&one, &two], &[("year", "1".into()), ("genre", "1".into())]));
        assert_eq!((done.changed, done.problems.clone()), (2, vec!["A genre for 'Album' was not found on Apple Music".to_string()]), "{done:?}");
        assert!(l.tag(&one, "date").starts_with("2020") && l.tag(&two, "date").starts_with("2020"));

        // a repair with nothing to do changes nothing and leaves no record
        let records = crate::undo::batches(&l.root).len();
        let done = l.fix(&l.ask("album-artist", &folder, &[&one, &two], &[("default", "A".into())]));
        assert_eq!((done.changed, done.batch, crate::undo::batches(&l.root).len()), (0, None, records));

        // every repair of the first track undone, newest first: it is byte for byte the file it was
        for b in crate::undo::batches(&l.root) {
            assert_eq!(crate::undo::undo(&l.root, &b.id).unwrap(), Vec::<String>::new(), "{}", b.title);
        }
        assert!(std::fs::read(&one).unwrap() == before, "the first track is not what it was");
        assert_eq!(l.tag(&one, "comment"), "Downloaded from www.example.com");
    }

    #[test]
    fn the_repairs_that_need_a_choice_do_what_was_chosen_and_nothing_without_one() {
        let l = lab("choice");
        let s = |p: &Path| p.to_string_lossy().to_string();
        // two copies of an album: the one chosen stays, the other goes to the backups whole
        let (short, long) = (l.lib.join("A").join("Twin"), l.lib.join("A").join("Twin (Deluxe)"));
        l.tone(r"A\Twin\01 One.mp3", 128, &["title=One"]);
        l.tone(r"A\Twin (Deluxe)\01 One.mp3", 128, &["title=One"]);
        l.tone(r"A\Twin (Deluxe)\02 Two.mp3", 128, &["title=Two"]);
        let folders = format!("{}|{}", s(&short), s(&long));
        assert_eq!(choices("pick-album", "Same album twice?", &HashMap::from([("folders".to_string(), folders.clone())]), 0), vec![("Keep 'Twin'".to_string(), s(&short)), ("Keep 'Twin (Deluxe)'".to_string(), s(&long))]);
        let done = l.fix(&l.ask("pick-album", &l.lib.join("A"), &[], &[("folders", folders.clone())]));
        assert_eq!((done.changed, done.problems), (0, vec!["Say which copy to keep.".to_string()]));
        let done = l.fix(&l.ask("pick-album", &l.lib.join("A"), &[], &[("folders", folders), ("choice", s(&long))]));
        assert_eq!((done.changed, done.problems.len()), (1, 0), "{done:?}");
        assert!(!short.exists() && long.join("02 Two.mp3").exists());
        assert!(crate::undo::undo(&l.root, done.batch.as_deref().unwrap()).unwrap().is_empty() && short.join("01 One.mp3").exists());

        // two folders for one artist: everything moves into the chosen one, a name in both stays, and the
        // other folder is only tidied away if it is empty
        let (the, plain) = (l.lib.join("The Band"), l.lib.join("Band"));
        l.tone(r"The Band\First\01.mp3", 128, &[]);
        l.tone(r"Band\Second\01.mp3", 128, &[]);
        l.tone(r"Band\First\01 other.mp3", 128, &[]);
        let folders = format!("{}|{}", s(&the), s(&plain));
        let done = l.fix(&l.ask("merge-artist", &the, &[], &[("folders", folders), ("choice", s(&the))]));
        assert_eq!((done.changed, done.problems), (1, vec!["'First' is in both folders, so it stays in 'Band'".to_string()]), "{:?}", std::fs::read_dir(&plain).map(|d| d.count()));
        assert!(the.join("Second").join("01.mp3").exists() && plain.join("First").join("01 other.mp3").exists());

        // filed under the wrong artist: the album moves to the artist its tags name, or its tags change
        let wrong = l.lib.join("Zed").join("Wrong (2020)");
        let track = l.tone(r"Zed\Wrong (2020)\01 One.mp3", 128, &["artist=Somebody Else", "album_artist=Somebody Else", "album=Wrong", "title=One"]);
        let facts = [("tagged", "Somebody Else".to_string()), ("folder", "Zed".to_string())];
        let done = l.fix(&l.ask("artist-mismatch", &wrong, &[&track], &facts));
        assert_eq!((done.changed, done.problems.len()), (0, 1));
        let mut tag = facts.to_vec();
        tag.push(("choice", "tag".to_string()));
        let done = l.fix(&l.ask("artist-mismatch", &wrong, &[&track], &tag));
        assert_eq!((done.changed, l.tag(&track, "album_artist"), l.tag(&track, "artist")), (1, "Zed".to_string(), "Somebody Else".to_string()), "{done:?}");
        let mut mv = facts.to_vec();
        mv.push(("choice", "move".to_string()));
        let done = l.fix(&l.ask("artist-mismatch", &wrong, &[&track], &mv));
        assert_eq!((done.changed, done.problems.len()), (2, 0), "{done:?}"); // the album, and the artist folder it left empty
        assert!(l.lib.join("Somebody Else").join("Wrong (2020)").join("01 One.mp3").exists() && !l.lib.join("Zed").exists());
        // a tag that cannot be a folder's name moves nothing
        let odd = l.tone(r"Yan\Odd\01.mp3", 128, &[]);
        let done = l.fix(&l.ask("artist-mismatch", &l.lib.join("Yan").join("Odd"), &[&odd], &[("tagged", "..".to_string()), ("folder", "Yan".to_string()), ("choice", "move".to_string())]));
        assert_eq!((done.changed, done.problems.len()), (0, 1));
        assert!(odd.exists());
    }

    #[test]
    fn the_better_copy_of_a_song_stays_and_the_folders_cover_goes_into_the_tracks() {
        let l = lab("dupes");
        let folder = l.lib.join("A").join("Album");
        let good = l.tone(r"A\Album\01 One.mp3", 320, &["title=One", "track=1"]);
        let poor = l.tone(r"A\Album\01 One (copy).mp3", 96, &["title=One", "track=1"]);
        let groups = format!("{}|{}", poor.to_string_lossy(), good.to_string_lossy());
        let done = l.fix(&l.ask("dupe-tracks", &folder, &[], &[("groups", groups)]));
        assert_eq!((done.changed, done.problems.len()), (1, 0), "{done:?}");
        assert!(good.exists() && !poor.exists());

        // a cover beside the tracks and a larger scan in a folder inside: the cover is the one embedded
        let mut c = tools::command(&l.tools.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=600x600", "-frames:v", "1"]).arg(folder.join("cover.png"));
        assert!(tools::run_limited(&mut c, 60));
        std::fs::create_dir_all(folder.join("Scans")).unwrap();
        let mut c = tools::command(&l.tools.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=blue:s=1200x1200", "-frames:v", "1"]).arg(folder.join("Scans").join("booklet.png"));
        assert!(tools::run_limited(&mut c, 60));
        assert_eq!(folder_cover(&folder), Some(folder.join("cover.png")));
        assert!(!healthscan::probe(&l.tools.ffprobe, &good).has_art);
        let done = l.fix(&l.ask("cover", &folder, &[&good], &[("mode", "folder".into())]));
        assert_eq!((done.changed, done.problems.len()), (1, 0), "{done:?}");
        let info = healthscan::probe(&l.tools.ffprobe, &good);
        assert!(info.has_art && info.art_w == 600, "{info:?}");
        // with no picture anywhere there is nothing to embed, and that is said
        std::fs::remove_file(folder.join("cover.png")).unwrap();
        std::fs::remove_dir_all(folder.join("Scans")).unwrap();
        let bare = l.tone(r"A\Bare\01 One.mp3", 128, &["title=One"]);
        let done = l.fix(&l.ask("cover", &l.lib.join("A").join("Bare"), &[&bare], &[("mode", "folder".into())]));
        assert_eq!((done.changed, done.problems), (0, vec!["No usable cover was found in the folder or in the tracks".to_string()]));
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    fn lab(name: &str) -> (PathBuf, PathBuf, PathBuf, Tools) {
        let base = std::env::temp_dir().join(format!("music-recs-fixall-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let tools = Tools { ffmpeg: tools::find_tool("ffmpeg").unwrap(), ffprobe: tools::find_tool("ffprobe").unwrap(), quality_cache: None, online: false, lyrics: None };
        (base.clone(), lib, base.join("backups"), tools)
    }
    fn ask(fix: &str, cat: &str, path: &Path, files: &[&Path]) -> Ask {
        Ask { fix: fix.into(), cat: cat.into(), path: path.to_string_lossy().to_string(), files: files.iter().map(|f| f.to_string_lossy().to_string()).collect(), artist: "A".into(), album: leaf(path), data: HashMap::new(), choice: String::new() }
    }

    #[test]
    fn fix_all_is_one_batch_and_says_which_findings_were_done() {
        let (base, lib, root, tools) = lab("all");
        let dirs: Vec<PathBuf> = ["One", "Two", "Three", "Four"].iter().map(|n| lib.join("A").join(n)).collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(lib.join("A").join("keep.txt"), b"so the artist folder is never empty").unwrap();
        // the second one filled up after the check
        std::fs::write(dirs[1].join("01.mp3"), b"new").unwrap();
        let asks: Vec<Ask> = dirs.iter().map(|d| ask("remove", "Empty folder", d, &[d])).collect();
        let mut seen = vec![];
        // stopped after the third finding: the fourth is not touched
        let done = apply_all(&asks, "Fix all: Empty folder (4 issues)", &lib, &root, &tools, |n, ok| {
            seen.push((n, ok));
            n < 2
        })
        .unwrap();
        assert_eq!(seen, vec![(0, true), (1, false), (2, true)]);
        assert_eq!((done.changed, done.problems), (2, vec!["Two: it is no longer empty".to_string()]));
        assert!(!dirs[0].exists() && dirs[1].exists() && !dirs[2].exists() && dirs[3].exists());
        let list = crate::undo::batches(&root);
        assert_eq!((list.len(), list[0].title.as_str(), list[0].changes), (1, "Fix all: Empty folder (4 issues)", 2));
        assert!(crate::undo::undo(&root, &list[0].id).unwrap().is_empty() && dirs[0].is_dir() && dirs[2].is_dir());
        // nothing to do at all: no record is left
        let none = apply_all(&[], "Fix all: nothing", &lib, &root, &tools, |_, _| true).unwrap();
        assert_eq!((none.changed, none.batch, crate::undo::batches(&root).len()), (0, None, 1));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_mp3_in_a_wav_wrapper_becomes_a_plain_mp3_with_the_same_audio() {
        let (base, lib, root, tools) = lab("unwrap");
        let file = lib.join("A").join("Album").join("01 One.mp3");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut c = tools::command(&tools.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "libmp3lame", "-b:a", "128k", "-f", "wav"]).arg(&file);
        assert!(tools::run_limited(&mut c, 60));
        let wrapped = std::fs::read(&file).unwrap();
        assert_eq!(&wrapped[..4], b"RIFF");
        let heard = audio_md5(&tools.ffmpeg, &file, Some("mp3")).unwrap();

        let done = apply(&plan(&ask("unwrap", "Unusual file format", file.parent().unwrap(), &[&file]), &lib, &tools), "unwrap", &lib, &root, &tools).unwrap();
        assert_eq!((done.changed, done.problems.len()), (1, 0), "{done:?}");
        let plain = std::fs::read(&file).unwrap();
        assert!(&plain[..4] != b"RIFF" && plain.len() < wrapped.len());
        assert_eq!(audio_md5(&tools.ffmpeg, &file, None), Some(heard));
        // a plain MP3 is not the kind this unwraps, and is left exactly as it is
        let done = apply(&plan(&ask("unwrap", "Unusual file format", file.parent().unwrap(), &[&file]), &lib, &tools), "unwrap", &lib, &root, &tools).unwrap();
        assert_eq!((done.changed, done.problems), (0, vec!["01 One.mp3: it is not the WAV-wrapped kind this can unwrap".to_string()]));
        assert_eq!(std::fs::read(&file).unwrap(), plain);
        // Undo: the wrapped file is back, byte for byte
        let id = crate::undo::batches(&root)[0].id.clone();
        assert!(crate::undo::undo(&root, &id).unwrap().is_empty());
        assert!(std::fs::read(&file).unwrap() == wrapped);
        // bytes that only claim to be a wrapper do not crash it
        assert!(unwrapped(b"RIFF\x04\x00\x00\x00WAVEdata\xff\xff\xff\xff").is_err());
        assert!(unwrapped(b"ID3\x03\x00\x00\x7f\x7f\x7f\x7f").is_err() && unwrapped(b"").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod lyric_tests;

/// A pause point for the tests only (never built into the app): between the tag editor's answer and the
/// moment its copies take their files' places, where an import can land in the meantime.
#[cfg(test)]
pub(crate) mod test_pause {
    use std::cell::RefCell;
    thread_local! {
        pub static AFTER_TAGS: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }
    pub fn after_tags() {
        if let Some(f) = AFTER_TAGS.with(|h| h.borrow_mut().take()) {
            f();
        }
    }
}
