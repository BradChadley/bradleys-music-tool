//! Reading one download (a folder or a loose track) and working out where it goes and what to fix.
use crate::importer::ctx::Ctx;
use crate::importer::lookup::{self, AppleTrack};
use crate::importer::model::*;
use crate::importer::probe;
use crate::importer::quality::{self, Fake, VTrack};
use crate::importer::rules;
use crate::importer::tracklist;
use crate::importer::util::*;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

pub const CONVERT_EXT: [&str; 9] = [".flac", ".wav", ".aif", ".aiff", ".ape", ".wv", ".tta", ".dsf", ".dff"]; // always lossless
pub const REMUX_EXT: [&str; 2] = [".mp3", ".m4a"]; // copied without re-encoding, but tags/art fixed
pub const PLAIN_COPY: [&str; 4] = [".ogg", ".opus", ".aac", ".wma"]; // copied untouched
pub const IMAGE_EXT: [&str; 5] = [".jpg", ".jpeg", ".png", ".webp", ".gif"];
const SKIP_FOLDERS: [&str; 2] = ["incomplete", "downloading"];

pub fn is_audio_ext(ext: &str) -> bool {
    CONVERT_EXT.contains(&ext) || REMUX_EXT.contains(&ext) || PLAIN_COPY.contains(&ext)
}

/// What the import makes of one track.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fate {
    /// a lossless track becomes a 320 kbps MP3
    Mp3,
    /// a FLAC file stays as it is, and only its tags and cover are put right (where lossless downloads are kept)
    StaysFlac,
    /// a lossless track of another kind becomes FLAC, the same sound sample for sample (where lossless
    /// downloads are kept)
    Flac,
    /// MP3 audio in a WAV wrapper is taken out into a plain MP3
    Unwrapped,
    /// an MP3 or M4A stays as it is, and only its tags and cover are put right
    Retagged,
    /// copied untouched
    Copied,
}

/// What the import makes of a track. `keep_lossless` is the choice in Settings (`Ctx::keep_lossless`).
pub fn fate(t: &Track, keep_lossless: bool) -> Fate {
    if t.lossless {
        return match keep_lossless {
            false => Fate::Mp3,
            true if t.codec == "flac" && t.ext == ".flac" => Fate::StaysFlac,
            true => Fate::Flac,
        };
    }
    if t.wrapped {
        Fate::Unwrapped
    } else if REMUX_EXT.contains(&t.ext.as_str()) {
        Fate::Retagged
    } else {
        Fate::Copied
    }
}

/// True when the import leaves this download lossless: lossless downloads are kept, and every track is one.
pub fn stays_lossless(ctx: &Ctx, u: &Unit) -> bool {
    ctx.keep_lossless && !u.tracks.is_empty() && u.tracks.iter().all(|t| t.lossless)
}

static LOSSLESS_CODEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(flac|alac|ape|wavpack|tta|mlp|truehd|wmalossless|pcm_.*|dsd_.*)$").unwrap());
static RE_DISC_TRACK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d)-(\d{1,3})[\s._-]+(.*)$").unwrap());
static RE_TRACK_ONLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,3})(?:[\s._-]+(.*))?$").unwrap());
static RE_TRACK_MID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|\s-\s)(\d{1,3})[\s.]+([^-].*)$").unwrap());
static RE_ARTIST_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)\s-\s(.+)$").unwrap());
static RE_DISC_DIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:^|\\)(?:cd|disc|disk)\s*(\d+)\\").unwrap());
static RE_VARIOUS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(various( artists)?|va|v\.a\.?)$").unwrap());
static RE_TRACK_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+/\d+$").unwrap());
static RE_LEAD_DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)").unwrap());
static RE_YEAR_IN_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d{4})\)").unwrap());
static RE_YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{4})").unwrap());
static RE_TRAIL_CURLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*[\[{][^\]}]*[\]}]\s*$").unwrap());
static RE_LOW_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(cover|folder|front)$").unwrap());
static RE_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// the title a track will end up with
fn tracks_title(t: &Track) -> String {
    t.new.iter().find(|e| e.0 == "title").map(|e| e.1.clone()).unwrap_or_else(|| t.old("title").to_string())
}

pub(crate) struct Parsed {
    pub(crate) disc: Option<u32>,
    pub(crate) track: Option<u32>,
    pub(crate) title: Option<String>,
}

// ---------- track number / title from filename ----------
pub(crate) fn parse_file_name(base: &str, album: &str) -> Parsed {
    let (mut disc, mut track, mut title) = (None, None, None);
    if let Some(c) = RE_DISC_TRACK.captures(base) {
        disc = c[1].parse().ok();
        track = c[2].parse().ok();
        title = Some(c[3].to_string());
    } else if let Some(c) = RE_TRACK_ONLY.captures(base) {
        track = c[1].parse().ok();
        title = c.get(2).map(|m| m.as_str().to_string()).filter(|s| !s.is_empty());
    } else if let Some(c) = RE_TRACK_MID.captures(base) {
        track = c[1].parse().ok();
        title = Some(c[2].to_string());
    }
    if let Some(t) = title.clone() {
        let mut t = t.trim_start_matches(|c: char| c.is_whitespace() || c == '.' || c == '-').to_string();
        // drop a leading "Artist - " left over from names like "01 - Artist - Title"
        if let Some(c) = RE_ARTIST_TITLE.captures(&t) {
            if norm(&c[1]) != norm(album) {
                t = c[2].to_string();
            }
        }
        title = Some(t).filter(|s| !s.is_empty());
    }
    Parsed { disc, track, title }
}

pub(crate) fn disc_from_path(rel: &str) -> Option<u32> {
    RE_DISC_DIR.captures(rel).and_then(|c| c[1].parse().ok())
}

pub(crate) fn lead_number(s: &str) -> Option<u32> {
    // capped: a hostile tag like "track 4294967295" would otherwise make the missing-track check count to four billion
    RE_LEAD_DIGITS.captures(s).and_then(|c| c[1].parse::<u32>().ok()).filter(|n| *n <= 9999)
}

// ---------- what's in the download folder ----------
pub fn get_source_items(ctx: &Ctx) -> Vec<PathBuf> {
    if !ctx.source.exists() {
        let _ = std::fs::create_dir_all(&ctx.source);
    }
    read_dir_sorted(&ctx.source)
        .into_iter()
        .filter(|e| match e.file_type() {
            Ok(t) if t.is_dir() => !SKIP_FOLDERS.contains(&e.file_name().to_string_lossy().to_lowercase().as_str()),
            Ok(_) => is_audio_ext(&ext_lower(&e.path())),
            Err(_) => false,
        })
        .map(|e| e.path())
        .collect()
}

fn file_entries(paths: Vec<PathBuf>) -> Vec<FileEntry> {
    paths
        .into_iter()
        .filter_map(|p| {
            let md = std::fs::metadata(&p).ok()?;
            Some(FileEntry { size: md.len(), modified: md.modified().unwrap_or(SystemTime::UNIX_EPOCH), path: p })
        })
        .collect()
}

// ---------- the library side ----------
/// Finds this album in the library even if the folder is named a bit differently ("Rocket" vs "Rocket (2017)"),
/// wherever the artist keeps it: in their folder, on a shelf in it ("EPs and Singles\Rocket"), or in the
/// artist's folder inside a genre folder ("Dubstep\Artist\Rocket").
pub fn find_existing_album(ctx: &Ctx, artist: &str, folder: &str) -> Option<PathBuf> {
    let homes = artist_dirs(ctx, artist);
    if let Some(exact) = homes.iter().map(|d| d.join(folder)).find(|p| p.exists() && is_album_folder(ctx, p)) {
        return Some(exact);
    }
    let key = album_key(folder);
    homes.iter().flat_map(|d| album_dirs(d)).find(|d| album_key(&leaf_name(d)) == key && is_album_folder(ctx, d))
}

/// Whether a folder in the library can be an album at all, whatever a download's tags call it. Tags come
/// from strangers: an "artist" named like a genre folder and an "album" named like an artist in it, or an
/// "album" named like a shelf, must never make an artist's folder or a whole shelf the thing an import
/// replaces. So: not the library, not a folder straight in it, not an artist's folder inside a genre
/// folder, not a shelf, and it has to hold tracks (its own, or in folders such as discs).
pub fn is_album_folder(ctx: &Ctx, dir: &Path) -> bool {
    let same = |a: &Path, b: &Path| a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase();
    let Some(parent) = dir.parent() else { return false };
    if same(dir, &ctx.library) || same(parent, &ctx.library) || ctx.genre_folders.iter().any(|g| same(parent, &ctx.library.join(g))) {
        return false;
    }
    if crate::library::shelf_albums(dir).is_some() {
        return false;
    }
    let (own, subs) = crate::library::look(dir);
    own > 0 || !subs.is_empty()
}

fn leaf_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// The folders an artist's music may be in: their folder in the library, and a folder of the same name
/// inside a genre folder (`ctx.genre_folders`). Only those that exist.
pub fn artist_dirs(ctx: &Ctx, artist: &str) -> Vec<PathBuf> {
    let mut out = vec![ctx.library.join(artist)];
    out.extend(ctx.genre_folders.iter().map(|g| ctx.library.join(g).join(artist)));
    out.retain(|d| d.is_dir());
    out
}

/// The album folders in an artist's folder, as the library reads them (see `crate::library`): shelves are
/// looked into, and an album kept inside another album's folder is one of its own.
pub fn album_dirs(artist_dir: &Path) -> Vec<PathBuf> {
    crate::library::album_paths(artist_dir).into_iter().map(|rel| artist_dir.join(rel)).collect()
}

/// Whether `dir` is an album folder of this artist that an import may replace: one the library's own
/// reading finds, or (as before) any folder directly inside the artist's folder. Never a folder that
/// cannot be an album (`is_album_folder`).
pub fn is_album_of(ctx: &Ctx, artist: &str, dir: &Path) -> bool {
    let homes = artist_dirs(ctx, artist);
    is_album_folder(ctx, dir) && !homes.iter().any(|h| h == dir) && dir != ctx.library && (homes.iter().any(|h| dir.parent() == Some(h.as_path())) || homes.iter().any(|h| album_dirs(h).iter().any(|a| a == dir)))
}

static RE_SMALL_RELEASE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(\b|\s-\s)(e\.?p\.?|single)\s*([\(\[][^\)\]]*[\)\]])?\s*$").unwrap());
static RE_SHELF_FOR_SMALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(eps?|e\.p\.s?|singles?|extended plays?)\b").unwrap());
static RE_SHELF_FOR_ALBUMS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\balbums?\b").unwrap());

/// The folder a new album of this artist goes into. That is the artist's folder, unless the artist keeps
/// releases on shelves: then an EP or single goes on the shelf for those ("EPs and Singles") and an album
/// on the shelf for albums ("Studio Albums"), when there is one.
pub fn home_for_new(ctx: &Ctx, artist: &str, album: &str, tracks: usize) -> PathBuf {
    let artist_dir = artist_dirs(ctx, artist).into_iter().next().unwrap_or_else(|| ctx.library.join(artist));
    let shelves: Vec<PathBuf> =
        read_dir_sorted(&artist_dir).into_iter().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).map(|e| e.path()).filter(|p| crate::library::shelf_albums(p).is_some()).collect();
    if shelves.is_empty() {
        return artist_dir;
    }
    let small = RE_SMALL_RELEASE.is_match(album) || tracks <= 3;
    let wanted: &Regex = if small { &RE_SHELF_FOR_SMALL } else { &RE_SHELF_FOR_ALBUMS };
    shelves.into_iter().find(|s| wanted.is_match(&leaf_name(s))).unwrap_or(artist_dir)
}

/// An album of this artist whose name differs only by a trailing bracket that may or may not mean another
/// recording ("Rocket" in the library, "Rocket (Live)" arriving). Never treated as the same album: only
/// pointed out, so the choice is made by a person.
pub fn find_similar_album(ctx: &Ctx, artist: &str, folder: &str) -> Option<PathBuf> {
    let loose = |name: &str| norm(&strip_edition(&strip_year(name)));
    let key = loose(folder);
    artist_dirs(ctx, artist).iter().flat_map(|d| album_dirs(d)).find(|d| loose(&leaf_name(d)) == key)
}

pub struct AlbumQuality {
    pub tracks: usize,
    pub kbps: u32,
    pub lossless: bool,
}

pub fn get_album_quality(ctx: &Ctx, dir: &Path) -> AlbumQuality {
    let audio: Vec<PathBuf> = walk_files(dir).into_iter().filter(|p| is_audio_ext(&ext_lower(p))).collect();
    let sample: Vec<probe::Probe> = audio.iter().take(4).map(|p| probe::probe(&ctx.ffprobe, p)).collect();
    let kbps = if sample.is_empty() { 0 } else { (sample.iter().map(|p| p.kbps as f64).sum::<f64>() / sample.len() as f64).round() as u32 };
    AlbumQuality { tracks: audio.len(), kbps, lossless: sample.iter().any(|p| LOSSLESS_CODEC.is_match(&p.codec)) }
}

/// Works out what Import should do, given what's already in the library under the current artist/folder names.
pub fn update_unit_action(ctx: &Ctx, u: &mut Unit) {
    u.existing = find_existing_album(ctx, &u.artist, &u.folder);
    u.action = Action::New;
    u.note.clear();
    // where it will go, when that is not simply the artist's folder: beside the copy it replaces (on its
    // shelf, or in a genre folder), or on the shelf this artist keeps such releases on
    let plain = ctx.library.join(&u.artist);
    let home = match &u.existing {
        Some(e) => e.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| plain.clone()),
        None => home_for_new(ctx, &u.artist, &u.folder, u.tracks.len()),
    };
    u.home = (home != plain).then_some(home);
    let Some(existing) = u.existing.clone() else {
        // a near match is never replaced and never skipped: it is left unticked for a person to decide
        if let Some(near) = find_similar_album(ctx, &u.artist, &u.folder) {
            u.include = false;
            u.note = format!("You have '{}'. This may be another recording of it: tick to add it beside that one.", near.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        }
        return;
    };
    let item = u.item.to_string_lossy().to_string();
    if ctx.get_pending().contains(&item) {
        u.action = Action::Cleanup;
        u.include = true;
        u.note = "Already imported - Soulseek had the original open last time. Import will just remove the leftover download.".into();
        return;
    }
    let have = get_album_quality(ctx, &existing);
    let name = existing.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // Health asked for a better copy of this album ("Replace this album"): this download is it, whatever
    // its bitrate or track count, as long as its own audio check found nothing wrong
    if let Some(want) = crate::replace::wanted(&ctx.replacements, &existing) {
        match unfit_to_replace(ctx, u, &existing) {
            None => {
                u.action = Action::Replace;
                u.note = if want.files.is_empty() {
                    format!("Replaces '{name}', which Health flagged ({}). The old copy is kept in the undo backups.", want.why.to_lowercase())
                } else {
                    let n = want.files.len();
                    format!("Replaces {n} {} of '{name}' that Health flagged ({}); the rest of the album stays. The old {} kept in the undo backups.", if n == 1 { "track" } else { "tracks" }, want.why.to_lowercase(), if n == 1 { "file is" } else { "files are" })
                };
            }
            Some(why) => {
                u.action = Action::Exists;
                u.include = false;
                u.note = format!("Health asked for a better copy of '{name}', but this download {why}. Your copy stays as it is.");
            }
        }
        return;
    }
    // (a download that stays lossless and passed its own check as real lossless: only where lossless downloads are kept)
    let lossless = stays_lossless(ctx, u) && u.verdict.fake == Fake::None;
    let mut better: Vec<String> = vec![];
    if !have.lossless && have.kbps > 0 && have.kbps < 200 && u.kbps >= have.kbps + 64 {
        better.push(if lossless { format!("your copy is {}kbps, this one is lossless", have.kbps) } else { format!("your copy is {}kbps, this one will be {}kbps", have.kbps, u.kbps) });
    }
    // more tracks only wins if the sound is not worse: never trade a lossless or clearly higher-bitrate copy for a longer one
    // (a lossless copy is only matched by a download that stays lossless)
    let not_worse = if have.lossless { lossless } else { have.kbps == 0 || u.kbps + 32 >= have.kbps };
    if have.tracks > 0 && u.tracks.len() > have.tracks && u.include && not_worse {
        better.push(format!("your copy has {} tracks, this has {}", have.tracks, u.tracks.len()));
    }
    if !better.is_empty() {
        u.action = Action::Replace;
        u.note = format!("Replaces '{}' in your library: {}. The old copy is kept in the undo backups.", name, better.join("; "));
    } else if lossless && !have.lossless && have.tracks > 0 && u.tracks.len() >= have.tracks {
        // a lossless copy of an album held as MP3 (or another lossy kind) at a bitrate that is fine: it may
        // take that copy's place, but only when a person ticks it
        u.action = Action::Replace;
        u.include = false;
        u.note = format!("You have '{}' ({} tracks, {}kbps). This copy is lossless: tick to replace it. The old copy is kept in the undo backups.", name, have.tracks, have.kbps);
    } else {
        u.action = Action::Exists;
        u.include = false;
        u.note = format!("Already in your library as '{}' ({} tracks, {}kbps) - skipped.", name, have.tracks, have.kbps);
    }
}

/// Why a download may not take the place of an album Health flagged (`old`): None when its own audio check
/// found nothing wrong with it. Where lossless downloads are kept, a whole album that is lossless is also
/// never given up for a download that is not (single tracks keep their format anyway, see `replace_tracks`).
pub fn unfit_to_replace(ctx: &Ctx, u: &Unit, old: &Path) -> Option<String> {
    if !u.verdict.damaged.is_empty() {
        let n = u.verdict.damaged.len();
        return Some(format!("has {n} damaged {} itself", if n == 1 { "file" } else { "files" }));
    }
    if u.verdict.fake != Fake::None {
        return Some("is not the quality its label says either".into());
    }
    let whole = crate::replace::wanted(&ctx.replacements, old).is_some_and(|w| w.files.is_empty());
    if ctx.keep_lossless && whole && !stays_lossless(ctx, u) && get_album_quality(ctx, old).lossless {
        return Some("is not lossless, and your copy is".into());
    }
    None
}

/// Same artist + album folder (ignoring year and edition) means the same destination.
pub fn batch_key(u: &Unit) -> String {
    format!("{}|{}", norm(&u.artist), album_key(&u.folder))
}

/// Two downloads in one batch that would land in the same folder: keep the best one ticked.
/// `keep_lossless` (the choice in Settings): of two copies that are otherwise as good, the one that stays
/// lossless is the better one, whatever its size (a lossless copy counts as 320kbps, like the best MP3).
pub fn resolve_batch_duplicates(units: &mut [Unit], keep_lossless: bool) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        groups.entry(batch_key(u)).or_default().push(i);
    }
    let stays = |u: &Unit| keep_lossless && !u.tracks.is_empty() && u.tracks.iter().all(|t| t.lossless) && u.verdict.fake == Fake::None;
    for (_, mut idx) in groups {
        if idx.len() < 2 {
            continue;
        }
        idx.sort_by(|&a, &b| {
            let (ua, ub) = (&units[a], &units[b]);
            (ub.include as u8, ub.tracks.len(), ub.kbps, stays(ub), ub.bytes).cmp(&(ua.include as u8, ua.tracks.len(), ua.kbps, stays(ua), ua.bytes))
        });
        let best = units[idx[0]].name.clone();
        for &o in &idx[1..] {
            units[o].include = false;
            units[o].warnings.push(format!("Same album as '{best}', which looks like the better copy (unticked)"));
        }
    }
}

pub fn set_unit_artist(ctx: &Ctx, u: &mut Unit, new_artist: &str) {
    let new_artist = clean_name(new_artist);
    if new_artist.is_empty() || new_artist == u.artist {
        return;
    }
    u.artist = new_artist.clone();
    for t in &mut u.tracks {
        t.set_new("album_artist", &new_artist);
        if t.has_new("artist") {
            t.set_new("artist", &new_artist);
        }
    }
    if let Some(tag) = &u.tag_artist {
        ctx.save_alias(tag, &new_artist);
    }
}

// ---------- the main read ----------
/// Reads one download. `Err` says why it was skipped.
/// What a download is, as far as listing its files says (no file is opened): enough for its row in the
/// import list to be there at once, before anything slow is known about it.
pub struct Quick {
    pub name: String,
    /// how many audio files it holds, and the size of everything in it
    pub audio_files: usize,
    pub bytes: u64,
    /// the artist and album as its own name gives them ("Artist - Album"; all of it is the album otherwise)
    pub artist: String,
    pub album: String,
}

/// The files of a download, and whether it can be read at all yet: Err for one with no audio in it and for
/// one that is still arriving.
fn listing(ctx: &Ctx, item: &Path) -> Result<(Vec<FileEntry>, PathBuf, bool, String), String> {
    let is_dir = item.is_dir();
    let (files, root) = if is_dir {
        (file_entries(walk_files(item)), item.to_path_buf())
    } else {
        (file_entries(vec![item.to_path_buf()]), item.parent().unwrap_or(item).to_path_buf())
    };
    let item_name = item.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !files.iter().any(|f| is_audio_ext(&ext_lower(&f.path))) {
        return Err("no audio files".into());
    }
    let recent = SystemTime::now().checked_sub(Duration::from_secs(120)).unwrap_or(SystemTime::UNIX_EPOCH);
    // (a folder the app fetched itself, with every file arrived, does not need the wait)
    let settled = ctx.settled.read().unwrap_or_else(|e| e.into_inner()).contains(&item_name.to_lowercase());
    if !settled && files.iter().any(|f| f.modified > recent) {
        return Err("still downloading".into());
    }
    Ok((files, root, is_dir, item_name))
}

/// The first of the three steps of reading a download: only its files are listed.
pub fn quick_look(ctx: &Ctx, item: &Path) -> Result<Quick, String> {
    let (files, _, is_dir, name) = listing(ctx, item)?;
    let base = if is_dir { name.clone() } else { file_stem(&name) };
    let (artist, album) = match base.split_once(" - ") {
        Some((a, b)) if !a.trim().is_empty() && !b.trim().is_empty() => (a.trim().to_string(), b.trim().to_string()),
        _ => (String::new(), base.trim().to_string()),
    };
    Ok(Quick { audio_files: files.iter().filter(|f| is_audio_ext(&ext_lower(&f.path))).count(), bytes: files.iter().map(|f| f.size).sum(), name, artist, album })
}

/// What the third step needs from the second: what to look the cover up by.
pub struct Pictures {
    artist: String,
    album: String,
    apple: Option<lookup::AppleAlbum>,
}

/// The stages of reading a download, as its row says them while it is being worked out.
pub const STAGE_FILES: &str = "Reading the files";
pub const STAGE_AUDIO: &str = "Checking the audio";
pub const STAGE_ALBUM: &str = "Looking up the album";
pub const STAGE_TRACKS: &str = "Comparing the track list";
pub const STAGE_COVER: &str = "Finding the cover";

/// Everything about a download, read in one go (the three steps one after another). The import list runs
/// the steps apart, so a row is there at once and fills in (see `quick_look`, `read_unit`, `add_pictures`).
pub fn build_unit(ctx: &Ctx, item: &Path) -> Result<Unit, String> {
    let (mut u, pictures) = read_unit(ctx, item, &|_| {})?;
    add_pictures(ctx, &mut u, pictures);
    Ok(u)
}

/// The import writes lyrics into the tracks it runs the tag editor over: the lossless ones (converted, or a
/// FLAC that stays as it is), and MP3 and M4A files. Other kinds are copied as they are, lyrics and all.
pub fn takes_lyrics(t: &Track) -> bool {
    t.lossless || t.wrapped || REMUX_EXT.contains(&t.ext.as_str())
}

/// The songs of a download LRCLIB is asked about (the ones with no lyrics, and the ones whose lyrics were
/// flagged), by the names they will have once imported: (which track, artist, title, seconds).
pub fn lyric_asks(u: &Unit) -> Vec<(usize, String, String, f64)> {
    let after = |t: &Track, k: &str| t.new.iter().find(|e| e.0 == k).map(|e| e.1.clone()).unwrap_or_else(|| t.old(k).to_string());
    u.tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| takes_lyrics(t) && (t.lyric.missing() || t.lyric.has_junk()))
        .map(|(i, t)| {
            let artist = Some(after(t, "artist")).filter(|a| !a.trim().is_empty()).unwrap_or_else(|| u.artist.clone());
            (i, artist, tracks_title(t), t.duration)
        })
        .collect()
}

/// The third step: the cover found online, a spectrogram of one track and a small picture of the download's
/// own art. None of it changes whether the download is ticked.
pub fn add_pictures(ctx: &Ctx, u: &mut Unit, p: Pictures) {
    let cover = lookup::find_cover(ctx, &p.artist, &p.album, p.apple.as_ref());
    u.use_cover = cover.as_ref().map(|c| c.default).unwrap_or(false);
    u.cover = cover;
    u.spectro = pick_spectrogram(ctx, &u.tracks, &u.verdict);
    u.local_thumb = make_local_thumb(ctx, &u.tracks, u.folder_img.as_deref());
}

/// The second step: the facts. Every file's tags and format, the audio check, the lookups (edition, track
/// list), what the tags would be changed to, and whether the download is ticked. `stage` is told what is
/// being worked out. The unit comes back without its pictures.
pub fn read_unit(ctx: &Ctx, item: &Path, stage: &dyn Fn(&'static str)) -> Result<(Unit, Pictures), String> {
    let (files, root, is_dir, item_name) = listing(ctx, item)?;
    // a directory's "base name" is its whole name; a file's loses the extension
    let item_base = if is_dir { item_name.clone() } else { file_stem(&item_name) };
    let audio: Vec<&FileEntry> = files.iter().filter(|f| is_audio_ext(&ext_lower(&f.path))).collect();
    stage(STAGE_FILES);

    let mut tracks: Vec<Track> = vec![];
    let mut not_audio: Vec<String> = vec![];
    for f in &audio {
        let rel = if f.path == item { f.path.file_name().unwrap().to_string_lossy().to_string() } else { f.path.strip_prefix(&root).unwrap_or(&f.path).to_string_lossy().replace('/', "\\") };
        let ext = ext_lower(&f.path);
        let name = f.path.file_name().unwrap().to_string_lossy().to_string();
        // an .mp3 that's really a WAV wrapper around MP3 audio: unwrap a copy (verified) and work from that
        let mut path = f.path.clone();
        let mut unwrapped = false;
        if ext == ".mp3" && crate::importer::unwrap::is_riff_in_mp3(&f.path) {
            if let Some(c) = crate::importer::unwrap::unwrapped_copy(&ctx.ffmpeg, &ctx.work_dir, &f.path, f.size, f.modified) {
                path = c;
                unwrapped = true;
            }
        }
        let p = probe::probe(&ctx.ffprobe, &path);
        // a file with an audio file's name and no audio in it (anything can be given such a name) is not a
        // track: it is left out, like every other file in a download that is not music or a picture
        if p.codec.is_empty() {
            not_audio.push(name);
            continue;
        }
        // the codec decides, not the extension: MP3 audio in a .wav is not lossless (it's unwrapped, not re-encoded)
        let lossless = if p.codec.is_empty() { CONVERT_EXT.contains(&ext.as_str()) } else { LOSSLESS_CODEC.is_match(&p.codec) };
        let header_secs = if ext == ".mp3" || p.codec == "mp3" { crate::importer::unwrap::mp3_header_secs(&path) } else { 0.0 };
        let path_for_lyrics = path.clone();
        tracks.push(Track {
            size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(f.size),
            path,
            rel,
            base: file_stem(&name),
            lossless,
            wrapped: p.codec == "mp3" && ext != ".mp3",
            unwrapped,
            header_secs,
            junk: vec![],
            lyric: crate::importer::lyrics::of_file(&path_for_lyrics, p.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
            lyric_found: None,
            ext,
            old: p.tags,
            new: vec![],
            has_art: p.has_art,
            codec: p.codec,
            sample_rate: p.sample_rate,
            kbps: p.kbps,
            bits: p.bits,
            duration: p.duration,
            quality: None,
            track_no: None,
            disc_no: None,
        });
    }
    if tracks.is_empty() {
        return Err("no audio files".into());
    }
    let mut warnings: Vec<String> = vec![];
    let mut incomplete = false; // anything that should leave the album unticked sets this
    if !not_audio.is_empty() {
        incomplete = true;
        let shown: Vec<String> = not_audio.iter().take(3).map(|n| format!("'{n}'")).collect();
        let more = if not_audio.len() > 3 { format!(" and {} more", not_audio.len() - 3) } else { String::new() };
        warnings.push(format!("{}{more} {} not audio and would be left out (unticked)", shown.join(", "), if not_audio.len() == 1 { "is" } else { "are" }));
    }

    // --- the same song in two formats (e.g. FLAC and MP3 folders): keep the best copy ---
    let song_key = |t: &Track| -> String {
        let parsed = parse_file_name(&t.base, "");
        let title = if !t.old("title").is_empty() { Some(t.old("title").to_string()) } else { parsed.title.clone() };
        let num = lead_number(t.old("track")).or(parsed.track);
        // the disc from the tag, else from a "CD2\" folder: two discs that both open with "01 Intro" are two songs
        let disc = lead_number(t.old("disc")).or_else(|| disc_from_path(&t.rel)).unwrap_or(1);
        // (a title that folds to nothing, "???" or "...", says nothing about which song it is)
        let title = title.filter(|t| !norm(t).is_empty());
        match title {
            None => format!("file:{}\\{}", rel_parent(&t.rel), t.base).to_lowercase(),
            Some(title) => format!("{}|{}|{}", disc, num.map(|n| n.to_string()).unwrap_or_default(), norm(&title)),
        }
    };
    let keys: Vec<String> = tracks.iter().map(song_key).collect();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, k) in keys.iter().enumerate() {
        groups.entry(k.clone()).or_default().push(i);
    }
    let mut dupes: Vec<usize> = vec![];
    for (_, mut idx) in groups {
        if idx.len() > 1 {
            idx.sort_by(|&a, &b| (tracks[b].lossless as u8, tracks[b].kbps).cmp(&(tracks[a].lossless as u8, tracks[a].kbps)));
            dupes.extend(&idx[1..]);
        }
    }
    if !dupes.is_empty() {
        let mut kinds: Vec<String> = dupes.iter().map(|&i| tracks[i].ext.trim_start_matches('.').to_uppercase()).collect();
        kinds.sort();
        kinds.dedup();
        warnings.push(format!("Skipped {} duplicate {} copies of the same songs (kept the best version)", dupes.len(), kinds.join("/")));
        tracks = tracks.into_iter().enumerate().filter(|(i, _)| !dupes.contains(i)).map(|(_, t)| t).collect();
    }
    // everything sits in one subfolder like "FLAC\" or "MP3 320\": put the tracks straight in the album folder
    let mut dirs: Vec<String> = tracks.iter().map(|t| rel_parent(&t.rel)).collect();
    dirs.sort();
    dirs.dedup();
    if dirs.len() == 1 && !dirs[0].is_empty() && !dirs[0].contains('\\') && disc_from_path(&format!("{}\\", dirs[0])).is_none() {
        let prefix = format!("{}\\", dirs[0]);
        for t in &mut tracks {
            t.rel = t.rel[prefix.len()..].to_string();
        }
    }

    // --- scrambled text (saved in the wrong encoding): repaired before anything is worked out from the tags ---
    let mut garbled_of: Vec<Vec<(String, String)>> = vec![vec![]; tracks.len()];
    let mut unfixable: Vec<String> = vec![];
    for (i, t) in tracks.iter_mut().enumerate() {
        for k in ["title", "artist", "album", "album_artist", "albumartist"] {
            let v = t.old(k).to_string();
            if v.is_empty() || !rules::test_garbled(&v) {
                continue;
            }
            match rules::repair_text(&v) {
                Some(fixed) => {
                    t.old.insert(k.to_string(), fixed.clone());
                    garbled_of[i].push((if k == "albumartist" { "album_artist" } else { k }.to_string(), fixed));
                }
                None => {
                    let s = format!("{k} '{}'", rules::short(&v, 40));
                    if !unfixable.contains(&s) {
                        unfixable.push(s);
                    }
                }
            }
        }
    }
    let garbled_count = garbled_of.iter().filter(|g| !g.is_empty()).count();
    if garbled_count > 0 {
        warnings.push(format!("Fixed scrambled text (saved in the wrong encoding) on {garbled_count} track(s)"));
    }
    if !unfixable.is_empty() {
        warnings.push(format!("Scrambled characters that can't be undone automatically: {}. Worth correcting by hand", unfixable.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
    }
    // --- junk in tags (links, uploader signatures, ads): removed on import, same rules as Library Health ---
    let mut junk_seen: Vec<(String, String)> = vec![];
    for t in tracks.iter_mut() {
        t.junk = rules::find_junk(&t.old);
        for (f, v, why) in &t.junk {
            let s = format!("{} '{}'", f.replace('_', " "), rules::short(&RE_WS.replace_all(v, " "), 40));
            if !junk_seen.iter().any(|x| x.0 == s) {
                junk_seen.push((s, why.clone()));
            }
        }
    }
    let junk_count = tracks.iter().filter(|t| !t.junk.is_empty()).count();

    // --- audio quality: damaged files and fake quality (checks several tracks at once, cached) ---
    stage(STAGE_AUDIO);
    let paths: Vec<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();
    let durs: Vec<f64> = tracks.iter().map(|t| t.duration).collect();
    let analyze: Vec<bool> = tracks.iter().map(|t| t.lossless || t.kbps >= 190).collect(); // lossy too: upscaled MP3s
    let qr = quality::check_files(&ctx.ffmpeg, &paths, &durs, &analyze, ctx.quality_threads);
    for (t, q) in tracks.iter_mut().zip(qr) {
        t.quality = Some(q);
    }
    let vt_ext = |t: &Track| -> String { if t.wrapped || t.unwrapped { ".mp3".into() } else { t.ext.clone() } };
    let mut verdict = {
        let exts: Vec<String> = tracks.iter().map(vt_ext).collect();
        let vt: Vec<VTrack> = tracks
            .iter()
            .zip(&exts)
            .map(|(t, e)| VTrack { base: &t.base, ext: e, codec: &t.codec, kbps: t.kbps, duration: t.duration, lossless: t.lossless, quality: t.quality.as_ref() })
            .collect();
        quality::verdict(&vt)
    }; // (damaged files are reported after the track list check, which can clear some)
    let any_lossless = tracks.iter().any(|t| t.lossless);
    if verdict.fake == Fake::Strong {
        incomplete = true;
        warnings.push(if any_lossless && ctx.keep_lossless {
            format!("{} As FLAC it would still be no better than that MP3 (unticked)", verdict.fake_text)
        } else if any_lossless {
            format!("{} Converting can't bring the quality back, so it's no better than that MP3 (unticked)", verdict.fake_text)
        } else {
            format!("{} It sounds like a ~{}kbps file; a proper copy would sound better (unticked)", verdict.fake_text, verdict.est_kbps)
        });
    } else if verdict.fake == Fake::Mild {
        warnings.push(if any_lossless {
            format!("{} It's fine to import, but it isn't true lossless", verdict.fake_text)
        } else {
            format!("{} It's fine to import, but a proper copy would sound a little better", verdict.fake_text)
        });
    }
    if !verdict.flagged.is_empty() {
        for t in tracks.iter_mut().filter(|t| t.lossless || !any_lossless) {
            t.kbps = t.kbps.min(verdict.est_kbps); // judged by its real quality, so Replace doesn't treat it as an upgrade
        }
    }
    let wrapped_n = tracks.iter().filter(|t| t.wrapped || t.unwrapped).count();
    if wrapped_n > 0 {
        warnings.push(format!("{wrapped_n} track(s) are MP3s inside a WAV wrapper (some players can't open them): unwrapped to plain MP3, no quality lost"));
    }
    let low: Vec<&Track> = tracks.iter().filter(|t| t.sample_rate > 0 && t.sample_rate < 44100).collect();
    if let Some(first) = low.first() {
        warnings.push(format!("Low sample rate ({}kHz) on {} track(s): sounds dull, likely a poor source", (first.sample_rate / 100) as f64 / 10.0, low.len()));
    }
    if tracks.iter().any(|t| t.bits == 8) {
        warnings.push("8-bit audio: very low quality source".into());
    }

    // --- album-level values ---
    // album artist tags win over track artists; with none, a mix of many track artists means a compilation
    let album_artist_tags: Vec<String> = tracks.iter().flat_map(|t| [t.old("album_artist").to_string(), t.old("albumartist").to_string()]).filter(|s| !s.is_empty()).collect();
    let track_artists: Vec<String> = tracks.iter().map(|t| t.old("artist").to_string()).filter(|s| !s.is_empty()).collect();
    let tag_artists: Vec<String> = if !album_artist_tags.is_empty() { album_artist_tags.clone() } else { track_artists.clone() };
    let mut is_comp = false;
    if album_artist_tags.iter().any(|a| RE_VARIOUS.is_match(a)) {
        is_comp = true;
    } else if album_artist_tags.is_empty() && track_artists.len() >= 4 {
        let groups = group_by_key(track_artists.clone(), |a| norm(a));
        let top = groups.iter().map(|g| g.1.len()).max().unwrap_or(0);
        if groups.len() >= 3 && (top as f64) < track_artists.len() as f64 / 2.0 {
            is_comp = true;
        }
    }
    if tracks.iter().any(|t| t.old("compilation") == "1") {
        is_comp = true;
    }
    let mut artist = if is_comp { Some("Various Artists".to_string()) } else { ctx.resolve_artist(&tag_artists) }.unwrap_or_default();
    if artist.is_empty() {
        artist = "Unknown Artist".into();
    }

    let albums: Vec<String> = tracks.iter().map(|t| t.old("album").to_string()).filter(|s| !s.is_empty()).collect();
    let mut album = most_common(albums.clone());
    let mixed = group_by_key(albums, |a| norm(a)).len() > 1;

    // Which record this is and what kind of recording (studio, live, remixes...), from its name and its track
    // titles: only an official album of the same kind is compared with it, and of that album's editions the
    // one whose track list fits these titles best.
    let lookup_name = album.clone().unwrap_or_else(|| item_base.clone());
    let titles: Vec<String> = tracks.iter().map(|t| if !t.old("title").is_empty() { t.old("title").to_string() } else { parse_file_name(&t.base, "").title.unwrap_or_default() }).collect();
    let want = crate::albumkind::classify_with_titles(&lookup_name, &titles);
    // Apple's track list fills missing numbers/titles and gives each track's real length (to catch cut-off downloads)
    stage(STAGE_ALBUM);
    let (apple, apple_tracks): (Option<lookup::AppleAlbum>, Vec<AppleTrack>) = match lookup::find_apple_edition(ctx, &artist, &lookup_name, &want, &titles) {
        Some((a, t)) => (Some(a), t),
        None => (None, vec![]),
    };
    let mut cut_short: Vec<String> = vec![];

    let album = album.take().unwrap_or_else(|| match &apple {
        Some(a) => a.collection_name.clone(),
        None => strip_edition(&RE_YEAR_PAREN_END.replace(&item_base, "")),
    });
    let dates: Vec<String> = tracks.iter().flat_map(|t| [t.old("date").to_string(), t.old("year").to_string()]).filter(|s| !s.is_empty()).collect();
    let mut date = most_common(dates).unwrap_or_default();
    if date.is_empty() {
        if let Some(a) = &apple {
            if !lookup::is_reissue(&a.collection_name, &album) {
                date = a.release_date.chars().take(4).collect();
            }
        }
    }
    if date.is_empty() {
        if let Some(c) = RE_YEAR_IN_NAME.captures(&item_name) {
            date = c[1].to_string();
        }
    }
    let year = RE_YEAR.captures(&date).map(|c| c[1].to_string()).unwrap_or_default();
    let mut genre = most_common(tracks.iter().map(|t| t.old("genre").to_string())).unwrap_or_default();
    if genre.is_empty() {
        if let Some(a) = &apple {
            genre = a.genre.clone();
        }
    }

    // --- per-track values ---
    let mut fixes: Vec<(String, usize)> = vec![];
    let mut fix = |t: &mut Track, key: &str, value: &str| {
        if value.is_empty() || t.old(key) == value {
            return;
        }
        t.set_new(key, value);
        match fixes.iter_mut().find(|f| f.0 == key) {
            Some(f) => f.1 += 1,
            None => fixes.push((key.to_string(), 1)),
        }
    };
    for t in tracks.iter_mut() {
        let parsed = parse_file_name(&t.base, &album);
        let title = if !t.old("title").is_empty() { t.old("title").to_string() } else { parsed.title.clone().unwrap_or_default() };
        let at = if !apple_tracks.is_empty() && !title.is_empty() { apple_tracks.iter().find(|a| norm(&a.name) == norm(&title)) } else { None };

        t.track_no = lead_number(t.old("track")).or(at.map(|a| a.number)).or(parsed.track);
        t.disc_no = lead_number(t.old("disc"))
            .or(at.filter(|a| a.disc_count > 1).map(|a| a.disc_number))
            .or(parsed.disc)
            .or(disc_from_path(&t.rel));
        // much shorter than Apple's version of the same track (skip reissues: their versions can differ)
        if let (Some(at), Some(a)) = (at, &apple) {
            if at.millis > 0 && t.duration > 0.0 && !lookup::is_reissue(&a.collection_name, &album) {
                let apple_sec = at.millis as f64 / 1000.0;
                if t.duration < apple_sec * 0.85 && (apple_sec - t.duration) > 15.0 {
                    cut_short.push(format!("{} is {}, Apple's is {}", t.base, format_time(t.duration), format_time(apple_sec)));
                }
            }
        }

        fix(t, "title", &title);
        fix(t, "album_artist", &artist);
        // track artist: only clean up when it's a spelling variant of the album artist (keeps "X feat. Y")
        let ta = t.old("artist").to_string();
        if is_comp {
            fix(t, "compilation", "1");
        } else if ta.is_empty() || ctx.is_alias(&ta) || (norm(&ta) == norm(&artist) && ta != artist) {
            fix(t, "artist", &artist);
        }
        if t.old("album").is_empty() || (mixed && norm(&strip_edition(t.old("album"))) == norm(&strip_edition(&album))) {
            fix(t, "album", &album);
        }
        if t.old("date").is_empty() && t.old("year").is_empty() {
            fix(t, "date", &date);
        }
        if t.old("genre").is_empty() {
            fix(t, "genre", &genre);
        }
    }
    // repaired scrambled text goes in as a tag fix
    for (t, g) in tracks.iter_mut().zip(&garbled_of) {
        for (k, v) in g {
            if !t.has_new(k) {
                t.set_new(k, v);
            }
        }
    }
    if junk_count > 0 {
        warnings.push(format!(
            "Removing junk from the tags of {junk_count} track(s): {}",
            junk_seen.iter().take(3).map(|(s, why)| format!("{s} ({why})")).collect::<Vec<_>>().join("; ")
        ));
    }

    // --- compare with the official track list (Apple Music, else MusicBrainz): missing songs, swapped files, odd lengths ---
    stage(STAGE_TRACKS);
    let mut real: Option<tracklist::RealList> = None;
    let rt: Vec<tracklist::RealTrack> = apple_tracks
        .iter()
        .filter(|a| a.millis > 0)
        .map(|a| tracklist::RealTrack { d: a.disc_number, n: a.number, t: a.name.clone(), s: (a.millis as f64 / 100.0).round() / 10.0 })
        .collect();
    if !rt.is_empty() {
        real = Some(tracklist::RealList { source: "Apple Music".into(), tracks: rt });
    }
    if real.is_none() {
        real = tracklist::musicbrainz_tracklist(ctx, &artist, &album, &want, &titles);
    }
    let mut fit = tracklist::Fit::default();
    let mut tl_missing = false;
    let src = real.as_ref().map(|r| r.source.clone()).unwrap_or_default();
    if let Some(real) = &real {
        let ours: Vec<tracklist::OurTrack> = tracks
            .iter()
            .map(|t| tracklist::OurTrack {
                file: t.path.to_string_lossy().to_string(),
                title: tracks_title(t),
                disc: t.disc_no.unwrap_or(0),
                no: t.track_no.unwrap_or(0),
                secs: t.quality.as_ref().map(|q| q.decoded_seconds).filter(|s| *s > 0.0).unwrap_or(t.duration),
                errors: t.quality.as_ref().map(|q| q.error_count).unwrap_or(0),
            })
            .collect();
        for f in tracklist::resolve(&ours, real, &mut fit) {
            match f.category {
                "Wrong track numbers or titles" => {
                    let mut titles: Vec<String> = vec![];
                    for (file, title, no, disc) in &f.changes {
                        let Some(t) = tracks.iter_mut().find(|t| t.path.to_string_lossy() == file.as_str()) else { continue };
                        if let Ok(n) = no.parse() {
                            t.track_no = Some(n); // numbers were missing or used twice: take the official ones
                        }
                        if let Ok(d) = disc.parse() {
                            t.disc_no = Some(d);
                        }
                        if !title.is_empty() {
                            titles.push(format!("'{}' -> '{}'", rules::short(&tracks_title(t), 30), rules::short(title, 30)));
                        }
                    }
                    // titles are left as they are: official lists have typos too. The Health page offers them one album at a time.
                    if !titles.is_empty() {
                        warnings.push(format!(
                            "Title{} that may be misspelled (per {src}): {}{}. Left as they are; the Health page can fix them later",
                            if titles.len() > 1 { "s" } else { "" },
                            titles.iter().take(3).cloned().collect::<Vec<_>>().join("; "),
                            if titles.len() > 3 { format!("; and {} more", titles.len() - 3) } else { String::new() }
                        ));
                    }
                }
                "Wrong or swapped songs" => warnings.push(format!("Swapped files: {}", f.detail)),
                "Different version of a song" => warnings.push(format!("Different length from the official version: {}", f.detail)),
                "Incomplete album" => {
                    if f.severity <= 2 {
                        incomplete = true;
                        tl_missing = true;
                        warnings.push(format!("{} - did some downloads fail? (unticked; tick to import anyway)", f.detail));
                    } else {
                        warnings.push(f.detail.clone());
                    }
                }
                _ => {}
            }
        }
    }
    // every song on the official list is here: gaps in the numbering are just numbering, not missing songs
    let all_there = fit.fit && !tl_missing && fit.matched >= tracks.len();
    // exactly the official edition, song for song: its numbering wins over leftover numbers from another edition
    // (e.g. tags reading 1-6, 8-13 on a complete 12-track album)
    if let (true, Some(real)) = (all_there && fit.total == tracks.len(), &real) {
        let plan: Option<Vec<(usize, u32, u32)>> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let r = tracklist::find_real_track(&real.tracks, 0, 0, &tracks_title(t))?;
                let secs = t.quality.as_ref().map(|q| q.decoded_seconds).filter(|s| *s > 0.0).unwrap_or(t.duration);
                ((r.s - secs).abs() <= 5.0).then_some((i, r.d, r.n))
            })
            .collect();
        if let Some(plan) = plan {
            let mut spots: Vec<(u32, u32)> = plan.iter().map(|p| (p.1, p.2)).collect();
            spots.sort();
            spots.dedup();
            if spots.len() == tracks.len() {
                // every track found once, with no two landing on the same number
                let mut ds: Vec<u32> = real.tracks.iter().map(|r| r.d).collect();
                ds.sort();
                ds.dedup();
                let multi = ds.len() > 1;
                let mut renumbered = 0;
                // a download whose own numbers run 1, 2, 3 with nothing missing was numbered on purpose:
                // editions of one album differ in order, so the official list does not overrule it
                let clean = tracklist::numbered_cleanly(&tracks.iter().map(|t| (t.disc_no.unwrap_or(0), t.track_no)).collect::<Vec<_>>());
                if clean {
                    let other: Vec<(usize, u32)> = plan.iter().filter(|(i, _, n)| tracks[*i].track_no != Some(*n)).map(|(i, _, n)| (*i, *n)).collect();
                    if let Some((i, n)) = other.first() {
                        warnings.push(format!(
                            "{src} has {} track(s) in another order (for example '{}' as track {n}, here track {}). The download's own numbering is kept: it runs from 1 with nothing missing",
                            other.len(),
                            rules::short(&tracks_title(&tracks[*i]), 30),
                            tracks[*i].track_no.unwrap_or(0)
                        ));
                    }
                }
                for (i, d, n) in plan.into_iter().filter(|_| !clean) {
                    let t = &mut tracks[i];
                    if t.track_no != Some(n) || (multi && t.disc_no != Some(d)) {
                        t.track_no = Some(n); // (the totals below write it, since it now differs from the tag)
                        if multi {
                            t.disc_no = Some(d);
                        }
                        renumbered += 1;
                    }
                }
                if renumbered > 0 {
                    warnings.push(format!("Renumbered {renumbered} track(s) to match {src}'s track list (the download's numbers had gaps or came from another edition)"));
                }
            }
        }
    }

    // --- damaged files: an MP3 that only looks short because its length header is wrong isn't damaged ---
    let mut length_ok = std::collections::HashSet::new();
    // its own length header agrees with what plays: complete (a longer guess comes from extra tags at the end)
    for (i, t) in tracks.iter().enumerate() {
        if let Some(q) = &t.quality {
            if t.header_secs > 0.0 && q.decoded_seconds > 0.0 && (t.header_secs - q.decoded_seconds).abs() <= 2.0 {
                length_ok.insert(i);
            }
        }
    }
    if let (true, Some(real)) = (fit.fit, &real) {
        for (i, t) in tracks.iter().enumerate() {
            let Some(q) = t.quality.as_ref().filter(|q| q.decoded_seconds > 0.0 && !t.lossless) else { continue };
            if let Some(r) = tracklist::find_real_track(&real.tracks, t.disc_no.unwrap_or(0), t.track_no.unwrap_or(0), &tracks_title(t)) {
                if (r.s - q.decoded_seconds).abs() <= 3.0 {
                    length_ok.insert(i);
                }
            }
        }
    }
    {
        if !length_ok.is_empty() {
            let exts: Vec<String> = tracks.iter().map(vt_ext).collect();
            let vt: Vec<VTrack> = tracks
                .iter()
                .zip(&exts)
                .map(|(t, e)| VTrack { base: &t.base, ext: e, codec: &t.codec, kbps: t.kbps, duration: t.duration, lossless: t.lossless, quality: t.quality.as_ref() })
                .collect();
            quality::set_damage(&mut verdict, &vt, &length_ok);
        }
    }
    if !verdict.damaged.is_empty() {
        incomplete = true;
        let mut list = verdict.damaged.iter().take(4).cloned().collect::<Vec<_>>().join("; ");
        if verdict.damaged.len() > 4 {
            list.push_str(&format!("; and {} more", verdict.damaged.len() - 4));
        }
        warnings.push(format!("Damaged or incomplete file{}: {}. Re-download from another user (unticked)", if verdict.damaged.len() > 1 { "s" } else { "" }, list));
    }
    if !verdict.glitchy.is_empty() {
        warnings.push(format!(
            "{} track(s) have a few decode errors, usually inaudible junk at the end of the file: {}",
            verdict.glitchy.len(),
            verdict.glitchy.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
        ));
    }

    // totals: "3/12", disc "1/2"
    let mut discs: Vec<u32> = tracks.iter().filter_map(|t| t.disc_no).filter(|d| *d != 0).collect();
    discs.sort();
    discs.dedup();
    let snapshot: Vec<(Option<u32>, Option<u32>)> = tracks.iter().map(|t| (t.disc_no, t.track_no)).collect();
    for t in tracks.iter_mut() {
        if let Some(n) = t.track_no.filter(|n| *n != 0) {
            let same: Vec<&(Option<u32>, Option<u32>)> = snapshot.iter().filter(|s| s.0 == t.disc_no).collect();
            let total = match &apple {
                Some(a) if discs.len() <= 1 && a.track_count as usize >= same.len() => a.track_count,
                _ => same.iter().filter_map(|s| s.1).max().unwrap_or(n),
            };
            // rewritten when there's no total yet, or the number itself changed (renumbered / fixed from the official list)
            if !RE_TRACK_OF.is_match(t.old("track")) || lead_number(t.old("track")) != Some(n) {
                fix(t, "track", &format!("{n}/{total}"));
            }
        }
        if let Some(d) = t.disc_no.filter(|d| *d != 0) {
            if discs.len() > 1 && (!RE_TRACK_OF.is_match(t.old("disc")) || lead_number(t.old("disc")) != Some(d)) {
                fix(t, "disc", &format!("{d}/{}", discs.len()));
            }
        }
    }
    let _ = &fix;
    let cleared_discs = crate::importer::disc_tags::clear_single_disc(&mut tracks, apple_tracks.iter().any(|t| t.disc_count > 1 || t.disc_number > 1));
    if cleared_discs > 0 {
        fixes.push(("single-disc label removed".into(), cleared_discs));
    }
    if garbled_count > 0 {
        fixes.push(("scrambled text".into(), garbled_count));
    }
    if junk_count > 0 {
        fixes.push(("junk removed".into(), junk_count));
    }

    if !cut_short.is_empty() && !fit.fit {
        // (when the official list fitted, its length comparison above covers this)
        warnings.push(format!(
            "Possibly cut short: {}{}. Could be a different edit, or an incomplete download",
            cut_short.iter().take(3).cloned().collect::<Vec<_>>().join("; "),
            if cut_short.len() > 3 { format!("; and {} more", cut_short.len() - 3) } else { String::new() }
        ));
    }

    // --- missing tracks: gaps in the numbering, or fewer tracks than Apple lists ---
    let by_disc = group_by_key(tracks.iter().map(|t| (t.disc_no.filter(|d| *d != 0).unwrap_or(1), t.track_no)).collect(), |x| x.0.to_string());
    for (disc_name, group) in by_disc {
        let mut nums: Vec<u32> = group.iter().filter_map(|x| x.1).filter(|n| *n != 0).collect();
        nums.sort();
        nums.dedup();
        let Some(&max) = nums.last() else { continue };
        let missing: Vec<u32> = (1..=max).filter(|n| !nums.contains(n)).collect();
        if !missing.is_empty() && missing.len() as f64 <= max as f64 / 2.0 {
            let label = if discs.len() > 1 { format!("disc {disc_name} ") } else { String::new() };
            let list = missing.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
            if all_there {
                warnings.push(format!("Track numbers skip {label}{list}, but every song on {src}'s list is here - just odd numbering"));
                continue;
            }
            incomplete = true;
            if tl_missing {
                continue; // already explained, with song names, above
            }
            warnings.push(format!(
                "Missing {}track{} {} of {} - did some downloads fail? (unticked; tick to import anyway)",
                label,
                if missing.len() > 1 { "s" } else { "" },
                missing.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "),
                max
            ));
        }
    }
    if let Some(a) = &apple {
        if !fit.fit && !incomplete && discs.len() <= 1 && !lookup::is_reissue(&a.collection_name, &album) && a.track_count as usize > tracks.len() && tracks.len() > 1 {
            warnings.push(format!("Apple Music lists {} tracks, this download has {} (could be a different edition)", a.track_count, tracks.len()));
        }
    }

    // --- one big file plus a .cue sheet (a whole album as one track) ---
    if files.iter().any(|f| ext_lower(&f.path) == ".cue") && tracks.len() <= 2 && tracks.iter().any(|t| t.lossless) {
        incomplete = true;
        warnings.push("This is the whole album as one big file plus a .cue sheet, so it would import as a single long track (unticked)".into());
    }

    // (the cover is looked up in the third step, by the album's name as it is here)
    let pictures = Pictures { artist: artist.clone(), album: album.clone(), apple };
    let album = RE_TRAIL_CURLY.replace(&album, "").trim().to_string(); // drop trailing [Web FLAC] / {catalog #}
    let folder = if !year.is_empty() && !album.ends_with(&format!("({year})")) { format!("{album} ({year})") } else { album.clone() };
    // (only files that really are pictures: one that just has a picture's name is never put into a track)
    let mut imgs: Vec<&FileEntry> = files.iter().filter(|f| IMAGE_EXT.contains(&ext_lower(&f.path).as_str()) && crate::importer::images::is_picture(&f.path)).collect();
    imgs.sort_by(|a, b| {
        let key = |f: &FileEntry| (RE_LOW_NAME.is_match(&file_stem(&f.path.file_name().unwrap().to_string_lossy())), f.size);
        key(b).cmp(&key(a))
    });
    let folder_img = imgs.first().map(|f| f.path.clone());

    // quality of this download as it will end up (lossless becomes 320kbps MP3; a lossless track that is
    // kept counts as 320 too, since a lossless file's own bitrate says nothing about how it sounds)
    let new_kbps = (tracks.iter().map(|t| if t.lossless { 320.0 } else { t.kbps as f64 }).sum::<f64>() / tracks.len().max(1) as f64).round() as u32;

    let convert = tracks.iter().filter(|t| t.lossless).count();
    let mut u = Unit {
        name: item_name,
        root,
        item: item.to_path_buf(),
        is_dir,
        bytes: files.iter().map(|f| f.size).sum(),
        files,
        tag_artist: tag_artists.iter().find(|a| !a.is_empty()).cloned(),
        artist: clean_name(&artist),
        folder: clean_name(&folder),
        use_cover: false,
        cover: None,
        folder_img,
        fixes,
        convert,
        kbps: new_kbps,
        tracks,
        warnings,
        verdict,
        spectro: None,
        local_thumb: None,
        action: Action::New,
        existing: None,
        home: None,
        note: String::new(),
        include: !incomplete,
        lyric_choice: Default::default(),
        lyrics_pending: false,
    };
    update_unit_action(ctx, &mut u);
    Ok((u, pictures))
}

/// A spectrogram picture of one track (the worst-looking one, or one from the middle) for the window.
fn pick_spectrogram(ctx: &Ctx, tracks: &[Track], verdict: &quality::Verdict) -> Option<PathBuf> {
    let idx = if let Some(&i) = verdict.flagged.first() {
        i
    } else {
        let pool: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].lossless).collect();
        let pool = if pool.is_empty() { (0..tracks.len()).collect() } else { pool };
        *pool.get(pool.len() / 2)?
    };
    let t = tracks.get(idx)?;
    let out = ctx.work_dir.join(format!("spec-{:x}.png", rand_id()));
    quality::make_spectrogram(&ctx.ffmpeg, &t.path, t.duration, &out)
}

/// small preview for the art that came with the download
fn make_local_thumb(ctx: &Ctx, tracks: &[Track], folder_img: Option<&Path>) -> Option<PathBuf> {
    if let Some(t) = tracks.iter().find(|t| t.has_art) {
        let out = ctx.work_dir.join(format!("thumb-{:x}.jpg", rand_id()));
        let mut c = crate::importer::tools::command(&ctx.ffmpeg, true);
        c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]).args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(&t.path).args(["-an", "-frames:v", "1", "-vf", "scale=400:-1", "--"]).arg(&out);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let _ = crate::importer::tools::run_limited(&mut c, 60);
        if out.exists() {
            return Some(out);
        }
    }
    folder_img.map(|p| p.to_path_buf())
}

pub fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    t ^ (N.fetch_add(1, Ordering::SeqCst) << 48) ^ (std::process::id() as u64)
}
