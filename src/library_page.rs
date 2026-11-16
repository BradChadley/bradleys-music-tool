//! The Library page: browse the music folder as cover-art tiles, with one album's details in a sidebar.
//!
//! This page only ever reads. Nothing here creates, changes, moves or deletes a file.
//!   * the folder scan runs on a background thread and reports progress
//!   * covers come from the app's shared cover cache (`crate::covers`), which decodes them on background
//!     threads at exactly the size they are drawn (tile size times the screen's pixels per point), only
//!     for tiles on or near the screen, and drops old ones
use crate::config::Config;
use crate::covers::{self, Found, Pic, Src};
use crate::engine::Results;
use crate::library::Library;
use crate::theme::{self, ACCENT, ACCENT2, BG2, BORDER, CARD, CARD_HOVER, DIM, FAINT, TEXT};
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::text::{LayoutJob, TextWrapping};
use egui::{pos2, vec2, Align, Align2, Color32, CornerRadius, FontId, Label, Layout, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

/// Side of a cover tile, in points, at its smallest. In the grids the covers grow from this to fill the
/// row (see `columns`); the tiles in the sidebar stay this size.
const TILE: f32 = 164.0;
/// Room under a tile's cover for its two lines of text.
const TILE_TEXT: f32 = 42.0;
const GAP_MIN: f32 = 18.0;
const GAP_MAX: f32 = 34.0;
const ROW_GAP: f32 = 8.0;
/// Space between the two columns of tiles in the sidebar.
const SIDE_GAP: f32 = 16.0;

static RE_TRAILING_YEAR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\s*\(\d{4}\)\s*$").unwrap());
static RE_LEADING_YEAR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^\[(\d{4})\]\s*(\S.*)$").unwrap());
static RE_TRACK_NO: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^(\d{1,3}(?:-\d{1,3})?)[\s._-]+(\S.*)$").unwrap());
static NEXT_UID: AtomicU64 = AtomicU64::new(1);

// ------------------------------------------------------------------ data

struct Track {
    /// "CD1", "Disc 2": the subfolder the file is in, empty for the album folder itself
    disc: String,
    /// "07", "1-03", or empty when the file name has no number
    no: String,
    title: String,
}

struct Album {
    /// a number of its own for each time an album is read from disk
    uid: u64,
    /// the name its covers are kept under in the shared cover cache
    cover_key: u64,
    artist: String,
    /// the artist's key in the Last.fm numbers
    stat_key: String,
    title: String,
    year: Option<i32>,
    /// the album folder (for loose tracks: the artist folder)
    path: PathBuf,
    loose: bool,
    tracks: Vec<Track>,
    art: Vec<Src>,
    /// lowercase artist and title, for the filter box
    search: String,
    sort_artist: String,
    /// when the album folder was made (else last changed), in seconds since 1970; 0 if not known.
    /// Read once, by the scan.
    added: u64,
}

enum Fail {
    Missing,
    Other(String),
}

enum Msg {
    /// artists read so far, of how many
    Progress(usize, usize),
    Done(u64, Result<Vec<Album>, Fail>),
    /// one album re-read; None if its folder is gone
    One(PathBuf, Option<Album>),
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Artist,
    Newest,
    Played,
}

/// What the grid shows a tile for.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Albums,
    Artists,
}

/// One artist in the Artists view: the albums of theirs that pass the filter.
struct Group {
    /// the artist's `sort_artist`
    key: String,
    name: String,
    stat_key: String,
    /// indexes into `albums`, oldest first
    albums: Vec<usize>,
    /// up to four of them that have a cover, for the tile
    mosaic: Vec<usize>,
    /// how many albums the artist has in all, filter or not
    total: usize,
    newest: i32,
    /// the worst open health problem among them (0 = none)
    worst: u8,
}

/// Everything the filtered, sorted lists depend on.
#[derive(Clone, PartialEq)]
struct ViewSig {
    filter: String,
    sort: Sort,
    problems: bool,
    focus: Option<String>,
    albums_rev: u64,
    marks_rev: u64,
    plays: (usize, i64),
}

enum Act {
    Close,
    Select(PathBuf),
    Open(PathBuf),
    RescanOne(usize),
    /// show one artist's albums (by `sort_artist`)
    Focus(String),
    /// open the large quick look for this album
    Quick(PathBuf),
}

// ------------------------------------------------------------------ reading folders (background threads)

fn clean_tracks(mut audio: Vec<(String, String, PathBuf)>) -> (Vec<Track>, Option<PathBuf>) {
    audio.sort_by(|a, b| (a.0.to_lowercase(), covers::natural_key(&a.1)).cmp(&(b.0.to_lowercase(), covers::natural_key(&b.1))));
    let first = audio.first().map(|a| a.2.clone());
    let mut stems: Vec<String> = audio.iter().map(|(_, name, _)| Path::new(name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| name.clone())).collect();
    strip_shared_prefix(&mut stems); // "Artist - Album - 01 Title"
    // Only treat leading digits as track numbers when most files have them ("99 Problems" on its own is a title).
    let numbered = stems.iter().filter(|s| RE_TRACK_NO.is_match(s)).count() * 10 >= stems.len() * 6;
    let (numbers, mut titles): (Vec<String>, Vec<String>) = stems
        .iter()
        .map(|stem| match RE_TRACK_NO.captures(stem) {
            Some(c) if numbered => (c[1].to_string(), c[2].to_string()),
            _ => (String::new(), stem.clone()),
        })
        .unzip();
    strip_shared_prefix(&mut titles); // "01 - Artist - Title"
    let tracks = audio
        .iter()
        .zip(numbers.into_iter().zip(titles))
        .map(|((disc, _, _), (no, title))| {
            let title = if title.contains(' ') { title } else { title.replace('_', " ") };
            Track { disc: disc.clone(), no, title: title.trim().to_string() }
        })
        .collect();
    (tracks, first)
}

/// Removes what every name starts with, up to its last " - " ("Artist - Album - "), when all of them share it.
fn strip_shared_prefix(names: &mut [String]) {
    if names.len() < 2 {
        return;
    }
    let first = names[0].clone();
    let mut shared = first.len();
    for n in names.iter() {
        shared = shared.min(first.bytes().zip(n.bytes()).take_while(|(a, b)| a == b).count());
    }
    while !first.is_char_boundary(shared) {
        shared -= 1;
    }
    let Some(at) = first[..shared].rfind(" - ") else { return };
    let cut = at + 3;
    if names.iter().all(|n| n.len() > cut && !n[cut..].trim().is_empty()) {
        for n in names.iter_mut() {
            *n = n[cut..].to_string();
        }
    }
}

/// When a folder was made (else last changed), in seconds since 1970; 0 if the disk does not say.
fn added_secs(dir: &Path) -> u64 {
    let Ok(m) = std::fs::metadata(dir) else { return 0 };
    m.created().or_else(|_| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

fn make_album(artist: &str, stat_key: &str, title: String, year: Option<i32>, path: PathBuf, loose: bool, found: Found) -> Option<Album> {
    if found.audio.is_empty() {
        return None;
    }
    let (tracks, first) = clean_tracks(found.audio);
    let added = added_secs(&path);
    Some(Album {
        uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
        cover_key: covers::folder_key(&path),
        artist: artist.to_string(),
        stat_key: stat_key.to_string(),
        search: format!("{} {}", artist.to_lowercase(), title.to_lowercase()),
        sort_artist: artist.to_lowercase(),
        title,
        year,
        path,
        loose,
        tracks,
        art: covers::art_sources(found.pictures, first.as_ref()),
        added,
    })
}

/// One album folder: its tracks (including "CD1" style subfolders) and where its cover is.
fn read_album(artist: &str, stat_key: &str, dir: &Path, other_albums: &HashSet<String>) -> Option<Album> {
    let folder = dir.file_name()?.to_string_lossy().to_string();
    let mut found = Found::default();
    covers::walk(dir, "", 0, true, &mut found);
    // tracks that belong to an album kept inside this folder (`other_albums`: every album folder of the
    // artist, lowercase) are that album's, not this one's
    if !other_albums.is_empty() {
        let inner = |p: &PathBuf| p.ancestors().skip(1).take_while(|a| *a != dir).any(|a| other_albums.contains(&a.to_string_lossy().to_lowercase()));
        found.audio.retain(|a| !inner(&a.2));
        found.pictures.retain(|p| !inner(&p.1));
    }
    // the year is "Title (2013)" in this library; older folders are "[2013] Title"
    let mut year = crate::util::folder_year(&folder);
    let mut title = RE_TRAILING_YEAR.replace(&folder, "").trim().to_string();
    if let Some(c) = RE_LEADING_YEAR.captures(&title) {
        let (y, rest) = (c[1].parse().ok(), c[2].trim().to_string());
        year = year.or(y);
        title = rest;
    }
    let title = if title.is_empty() { folder.clone() } else { title };
    make_album(artist, stat_key, title, year, dir.to_path_buf(), false, found)
}

/// Audio files sitting directly in an artist folder, as one "Loose tracks" album.
fn read_loose(artist: &str, stat_key: &str, artist_dir: &Path) -> Option<Album> {
    let mut found = Found::default();
    covers::walk(artist_dir, "", 0, false, &mut found);
    make_album(artist, stat_key, "Loose tracks".to_string(), None, artist_dir.to_path_buf(), true, found)
}

fn scan_all(music_dir: &str, skip: &str, alias_file: &str, tx: &Sender<Msg>, ctx: &egui::Context) -> Result<Vec<Album>, Fail> {
    if !Path::new(music_dir).is_dir() {
        return Err(Fail::Missing);
    }
    let lib = Library::scan(music_dir, skip, alias_file).map_err(Fail::Other)?;
    let mut artists: Vec<_> = lib.artists.values().collect();
    artists.sort_by_key(|a| a.folder.to_lowercase());
    let mut out = vec![];
    for (i, a) in artists.iter().enumerate() {
        // (`folder` is a path for an artist inside a genre folder: "Dubstep\Skream")
        let dir = Path::new(music_dir).join(&a.folder);
        let key = lib.key_for(a.display());
        // an album kept inside another album's folder is read on its own, not as part of the outer one
        let all: HashSet<String> = a.albums.iter().map(|(rel, _)| dir.join(rel).to_string_lossy().to_lowercase()).collect();
        for (name, _) in &a.albums {
            out.extend(read_album(a.display(), &key, &dir.join(name), &all));
        }
        out.extend(read_loose(a.display(), &key, &dir));
        if i % 8 == 0 {
            let _ = tx.send(Msg::Progress(i + 1, artists.len()));
            ctx.request_repaint();
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ the page

pub struct LibraryPage {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    albums: Vec<Album>,
    /// bumped whenever `albums` changes
    albums_rev: u64,
    /// for other pages' artist tiles: the artist's key in the Last.fm numbers -> up to four of their albums
    /// that have a cover, oldest first; and the `albums_rev` it was worked out for
    by_artist: HashMap<String, Vec<usize>>,
    by_artist_rev: Option<u64>,
    n_artists: usize,
    /// the folder the last scan was started for
    scanned_dir: Option<String>,
    scan_gen: u64,
    /// a whole-library scan is running: artists read so far, of how many (None until the count is known)
    scanning: Option<Option<(usize, usize)>>,
    /// a whole-library scan has finished and its albums are the ones on screen
    scanned_once: bool,
    fail: Option<Fail>,
    rescanning_one: Option<PathBuf>,
    filter: String,
    sort: Sort,
    mode: Mode,
    /// the one artist whose albums are showing (their `sort_artist`), reached from the Artists view
    focus: Option<String>,
    /// show only albums with an open health problem
    only_problems: bool,
    /// indexes into `albums`, filtered and sorted
    view: Vec<usize>,
    /// the same albums by artist, for the Artists view
    groups: Vec<Group>,
    view_sig: Option<ViewSig>,
    /// artist key -> all-time plays
    plays: HashMap<String, u64>,
    plays_sig: Option<(usize, i64)>,
    /// per album, the worst open health problem (0 = none), as last published by the Health page
    marks: Vec<u8>,
    /// a health report has been read, so "no mark" means "no problem"
    marks_known: bool,
    marks_rev: u64,
    marks_at: f64,
    /// album indexes, most recently added first (loose tracks left out)
    recent_order: Vec<usize>,
    /// the album open in the sidebar, by folder
    selected: Option<PathBuf>,
    /// the album showing in the large quick look, by folder
    quick: Option<PathBuf>,
    /// the album tile the pointer was on in the last frame drawn
    hover: Option<PathBuf>,
    /// scroll back to the top: the album grid, the artist grid, one artist's albums
    to_top: [bool; 3],
    /// bring this album's tile into view
    scroll_to: Option<PathBuf>,
    /// an album another page asked for before the scan had finished: (artist, album)
    pending_show: Option<(String, String)>,
    /// Ctrl+F was pressed: put the keyboard in the filter box
    focus_filter: bool,
    filter_id: Option<egui::Id>,
    /// what had the keyboard when this frame began
    focus_at_tick: Option<egui::Id>,
    /// debug: `--lib-dir <folder>` looks at another folder, `--lib-select <text>` opens the first matching album
    dir_override: Option<String>,
    select_on_load: Option<String>,
    /// debug: `--lib-quicklook <text>` opens the quick look for the first matching album
    quick_on_load: Option<String>,
    /// debug: `--lib-side-scroll <points>` opens the sidebar scrolled down that far
    side_scroll: Option<f32>,
    /// debug: `--lib-demo-marks` shows made-up health marks, `--lib-size 900x600` resizes the window
    demo_marks: bool,
    debug_size: Option<egui::Vec2>,
    /// the size the grids' covers are decoded at, held steady while the window is being resized
    grid_px: covers::Settle,
}

/// debug: `--lib-hover <text>` draws matching tiles as if the pointer were on them
static DEBUG_HOVER: LazyLock<Option<String>> = LazyLock::new(|| arg_after("--lib-hover").map(|s| s.to_lowercase()));
/// debug: `--lib-covers-by-folder` lets the cover cache find each album's cover itself, as other pages do
/// debug: `--lib-menu <text>` shows the right-click menu of matching tiles, opened under them
static DEBUG_MENU: LazyLock<Option<String>> = LazyLock::new(|| arg_after("--lib-menu").map(|s| s.to_lowercase()));
static DEBUG_BY_FOLDER: LazyLock<bool> = LazyLock::new(|| std::env::args().any(|a| a == "--lib-covers-by-folder"));

fn arg_after(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

pub(crate) fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", commas(n as u64), if n == 1 { one } else { many })
}

/// The album folder's own name ("" for loose tracks, which sit in the artist folder).
fn folder_name(a: &Album) -> String {
    if a.loose {
        return String::new();
    }
    a.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// An album name for comparing: lowercase, without a trailing "(2013)" or a leading "[2013]".
fn album_key(s: &str) -> String {
    let s = RE_TRAILING_YEAR.replace(s.trim(), "").to_string();
    let s = match RE_LEADING_YEAR.captures(&s) {
        Some(c) => c[2].to_string(),
        None => s,
    };
    s.trim().to_lowercase()
}

/// Letters and digits only, lowercase: for comparing names that differ in punctuation or spacing.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

impl LibraryPage {
    pub fn new(egui_ctx: &egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let focus = arg_after("--lib-artist").map(|s| s.to_lowercase());
        Self {
            ctx: egui_ctx.clone(),
            tx,
            rx,
            albums: vec![],
            albums_rev: 0,
            by_artist: HashMap::new(),
            by_artist_rev: None,
            n_artists: 0,
            scanned_dir: None,
            scan_gen: 0,
            scanning: None,
            scanned_once: false,
            fail: None,
            rescanning_one: None,
            filter: arg_after("--lib-filter").unwrap_or_default(),
            sort: match arg_after("--lib-sort").as_deref() {
                Some("newest") => Sort::Newest,
                Some("played") => Sort::Played,
                _ => Sort::Artist,
            },
            mode: if focus.is_some() || arg_after("--lib-view").as_deref() == Some("artists") { Mode::Artists } else { Mode::Albums },
            focus,
            only_problems: std::env::args().any(|a| a == "--lib-problems"),
            view: vec![],
            groups: vec![],
            view_sig: None,
            plays: HashMap::new(),
            plays_sig: None,
            marks: vec![],
            marks_known: false,
            marks_rev: 0,
            marks_at: 0.0,
            recent_order: vec![],
            selected: None,
            quick: None,
            hover: None,
            to_top: [false; 3],
            scroll_to: None,
            pending_show: arg_after("--lib-show").and_then(|s| s.split_once('|').map(|(a, b)| (a.to_string(), b.to_string()))),
            focus_filter: false,
            filter_id: None,
            focus_at_tick: None,
            dir_override: arg_after("--lib-dir"),
            select_on_load: arg_after("--lib-select").map(|s| s.to_lowercase()),
            quick_on_load: arg_after("--lib-quicklook").map(|s| s.to_lowercase()),
            side_scroll: arg_after("--lib-side-scroll").and_then(|s| s.parse().ok()),
            demo_marks: std::env::args().any(|a| a == "--lib-demo-marks"),
            debug_size: arg_after("--lib-size").and_then(|s| s.split_once('x').and_then(|(w, h)| Some(vec2(w.parse().ok()?, h.parse().ok()?)))),
            grid_px: covers::Settle::default(),
        }
    }

    /// (albums, artists) in the library, once a scan has finished.
    /// For another page's artist tile: paints this artist's album covers into the square `cover` the way
    /// the Artists view does (one whole, two halves, a half and two quarters, or four quarters), asked for
    /// at `px` pixels. `stat_key` is the artist's key in the Last.fm numbers. Returns false, and paints
    /// nothing, when the library has no cover for them.
    pub fn paint_artist(&mut self, ui: &Ui, stat_key: &str, cover: Rect, px: u32) -> bool {
        if self.by_artist_rev != Some(self.albums_rev) {
            self.by_artist_rev = Some(self.albums_rev);
            self.by_artist.clear();
            let mut order: Vec<usize> = (0..self.albums.len()).filter(|&i| !self.albums[i].art.is_empty()).collect();
            order.sort_by_key(|&i| (self.albums[i].loose, self.albums[i].year.unwrap_or(i32::MAX)));
            for i in order {
                let picks = self.by_artist.entry(self.albums[i].stat_key.clone()).or_default();
                if picks.len() < 4 {
                    picks.push(i);
                }
            }
        }
        match self.by_artist.get(stat_key) {
            Some(picks) => {
                mosaic(ui, &self.albums, picks, cover, Size { side: cover.width(), px, was: px }, true);
                true
            }
            None => false,
        }
    }

    pub fn counts(&self) -> Option<(usize, usize)> {
        self.scanned_once.then_some((self.albums.len(), self.n_artists))
    }

    /// Starts reading the library if that has not begun yet, so other pages can use `find` and `recent`
    /// without this page ever being opened. Does nothing once a scan of the folder has started.
    pub fn ensure_scanned(&mut self, _ctx: &egui::Context, cfg: &Config) {
        let dir = self.dir_override.as_deref().unwrap_or(cfg.music_dir.as_str());
        if self.scanning.is_none() && self.scanned_dir.as_deref() != Some(dir) {
            let dir = dir.to_string();
            self.start_scan(cfg, &dir);
        }
    }

    /// Albums whose artist or title contain every word of `query`, whatever the case, as (artist, album
    /// title, album folder). A title or artist that starts with the query comes first, then one with a
    /// word that starts with it, then the rest. Empty until a scan has finished.
    pub fn find(&self, query: &str, limit: usize) -> Vec<(String, String, PathBuf)> {
        let query = query.to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        if !self.scanned_once || words.is_empty() || limit == 0 {
            return vec![];
        }
        let whole = words.join(" ");
        let mut hits: Vec<(u8, usize)> = vec![];
        for (i, a) in self.albums.iter().enumerate() {
            if !words.iter().all(|w| a.search.contains(w)) {
                continue;
            }
            // `search` is the lowercase artist, a space, the lowercase title
            let title = a.search.get(a.sort_artist.len() + 1..).unwrap_or("");
            let rank = if title.starts_with(&whole) || a.sort_artist.starts_with(&whole) {
                0
            } else if a.search.split_whitespace().any(|w| w.starts_with(words[0])) {
                1
            } else {
                2
            };
            hits.push((rank, i));
        }
        hits.sort_by_key(|h| h.0); // stable: within a rank they stay in library order
        hits.truncate(limit);
        hits.into_iter().map(|(_, i)| self.album_ref(i)).collect()
    }

    /// The albums added most recently, newest first, as (artist, album title, album folder). Goes by when
    /// each album folder was made, which the scan read. Empty until a scan has finished.
    pub fn recent(&self, limit: usize) -> Vec<(String, String, PathBuf)> {
        if !self.scanned_once {
            return vec![];
        }
        self.recent_order.iter().take(limit).map(|&i| self.album_ref(i)).collect()
    }

    fn album_ref(&self, i: usize) -> (String, String, PathBuf) {
        let a = &self.albums[i];
        (a.artist.clone(), a.title.clone(), a.path.clone())
    }

    /// Opens this album's sidebar and brings its tile into view. Before the first scan has finished the
    /// request is kept and carried out when it does. An album that is not in the library leaves the
    /// artist's name in the filter box instead.
    pub fn show_album(&mut self, artist: &str, album: &str) {
        if self.scanning.is_some() || self.scanned_dir.is_none() {
            self.pending_show = Some((artist.to_string(), album.to_string()));
        } else {
            self.apply_show(artist, album);
        }
    }

    fn find_album(&self, artist: &str, album: &str) -> Option<usize> {
        let (artist_l, album_l) = (artist.trim().to_lowercase(), album_key(album));
        let loose_wanted = album_l.is_empty() || album_l == "loose tracks";
        let exact = self.albums.iter().position(|a| {
            a.sort_artist == artist_l && if a.loose { loose_wanted } else { album_key(&a.title) == album_l || album_key(&folder_name(a)) == album_l }
        });
        exact.or_else(|| {
            // the same names written a little differently ("AC-DC" for "AC/DC", "&" spaced otherwise)
            let (artist_s, album_s) = (squash(artist), squash(&album_l));
            self.albums.iter().position(|a| {
                squash(&a.artist) == artist_s && if a.loose { loose_wanted } else { squash(&album_key(&a.title)) == album_s || squash(&album_key(&folder_name(a))) == album_s }
            })
        })
    }

    fn apply_show(&mut self, artist: &str, album: &str) {
        self.focus = None;
        self.only_problems = false;
        match self.find_album(artist, album) {
            Some(i) => {
                let a = &self.albums[i];
                if !self.filter.to_lowercase().split_whitespace().all(|w| a.search.contains(w)) {
                    self.filter.clear(); // the filter would hide the tile
                }
                self.mode = Mode::Albums;
                self.selected = Some(a.path.clone());
                self.scroll_to = Some(a.path.clone());
            }
            None => {
                self.selected = None;
                self.filter = artist.trim().to_string();
            }
        }
    }

    /// Runs every frame, whichever page is showing.
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.focus_at_tick = ctx.memory(|m| m.focused());
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Progress(done, total) => {
                    if self.scanning.is_some() {
                        self.scanning = Some(Some((done, total)));
                    }
                }
                Msg::Done(scan, result) => {
                    if scan != self.scan_gen {
                        continue; // an older scan, overtaken by a newer one
                    }
                    self.scanning = None;
                    match result {
                        Ok(mut new) => {
                            // an album whose cover sources did not change keeps its decoded covers
                            let old: HashMap<&PathBuf, &Album> = self.albums.iter().map(|a| (&a.path, a)).collect();
                            for a in &mut new {
                                if let Some(o) = old.get(&a.path) {
                                    if o.art == a.art {
                                        a.uid = o.uid;
                                    } else {
                                        covers::forget_folder(&a.path);
                                    }
                                }
                            }
                            self.albums = new;
                            self.fail = None;
                            self.scanned_once = true;
                        }
                        Err(f) => {
                            self.albums.clear();
                            self.fail = Some(f);
                            self.scanned_once = false;
                        }
                    }
                    self.after_change();
                    if let Some(want) = self.select_on_load.take() {
                        self.selected = self.albums.iter().find(|a| a.search.contains(&want)).map(|a| a.path.clone());
                    }
                    if let Some(want) = self.quick_on_load.take() {
                        self.quick = self.albums.iter().find(|a| a.search.contains(&want)).map(|a| a.path.clone());
                    }
                    if let Some((artist, album)) = self.pending_show.take() {
                        self.apply_show(&artist, &album);
                    }
                }
                Msg::One(path, album) => {
                    if self.rescanning_one.as_ref() == Some(&path) {
                        self.rescanning_one = None;
                    }
                    covers::forget_folder(&path); // its files may have changed: its covers are read again
                    let at = self.albums.iter().position(|a| a.path == path);
                    match (at, album) {
                        (Some(i), Some(a)) => self.albums[i] = a,
                        (Some(i), None) => {
                            self.albums.remove(i);
                        }
                        (None, Some(a)) => self.albums.push(a),
                        (None, None) => {}
                    }
                    self.after_change();
                }
            }
        }
    }

    fn after_change(&mut self) {
        self.albums_rev += 1;
        self.n_artists = self.albums.iter().map(|a| a.sort_artist.as_str()).collect::<HashSet<_>>().len();
        self.marks.clear(); // looked up again on the next frame
        let mut recent: Vec<usize> = (0..self.albums.len()).filter(|&i| !self.albums[i].loose && self.albums[i].added > 0).collect();
        recent.sort_by(|&x, &y| self.albums[y].added.cmp(&self.albums[x].added));
        self.recent_order = recent;
        if let Some(q) = &self.quick {
            if !self.albums.iter().any(|a| &a.path == q) {
                self.quick = None;
            }
        }
        if let Some(sel) = &self.selected {
            if !self.albums.iter().any(|a| &a.path == sel) {
                self.selected = None;
            }
        }
        if let Some(key) = &self.focus {
            if !self.albums.iter().any(|a| &a.sort_artist == key) {
                self.focus = None;
            }
        }
    }

    fn start_scan(&mut self, cfg: &Config, dir: &str) {
        self.scan_gen += 1;
        self.scanned_dir = Some(dir.to_string());
        self.scanning = Some(None);
        let (dir, skip, alias, tx, ctx, scan) = (dir.to_string(), cfg.skip_folders.clone(), cfg.alias_file.clone(), self.tx.clone(), self.ctx.clone(), self.scan_gen);
        std::thread::spawn(move || {
            let result = scan_all(&dir, &skip, &alias, &tx, &ctx);
            let _ = tx.send(Msg::Done(scan, result));
            ctx.request_repaint();
        });
    }

    fn start_rescan_one(&mut self, i: usize) {
        let Some(a) = self.albums.get(i) else { return };
        let (artist, key, path, loose, tx, ctx) = (a.artist.clone(), a.stat_key.clone(), a.path.clone(), a.loose, self.tx.clone(), self.ctx.clone());
        self.rescanning_one = Some(path.clone());
        std::thread::spawn(move || {
            let album = if loose { read_loose(&artist, &key, &path) } else { read_album(&artist, &key, &path, &HashSet::new()) };
            let _ = tx.send(Msg::One(path, album));
            ctx.request_repaint();
        });
    }

    /// Looks up each album's health mark again, at most once a second (the Health page can publish a new
    /// report at any time, and there is no signal when it does).
    fn refresh_marks(&mut self, now: f64) {
        if self.marks.len() == self.albums.len() && (now - self.marks_at).abs() < 1.0 {
            return;
        }
        self.marks_at = now;
        let known = self.demo_marks || crate::links::health_known();
        let marks: Vec<u8> = if self.demo_marks {
            self.albums.iter().map(|a| [1, 0, 0, 0, 2, 0, 0, 3, 0, 0, 0][(a.uid % 11) as usize]).collect()
        } else if known {
            self.albums.iter().map(|a| crate::links::health_mark(&a.path).unwrap_or(0)).collect()
        } else {
            vec![0; self.albums.len()]
        };
        if known != self.marks_known || marks != self.marks {
            self.marks_rev += 1;
        }
        self.marks_known = known;
        self.marks = marks;
    }

    /// Ctrl+F goes to the filter box. Esc closes the quick look, or else the sidebar, or else clears the
    /// filter, or else leaves the one artist being shown.
    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            self.focus_filter = true;
            self.quick = None;
        }
        // Esc belongs to whatever else has the keyboard (a box in Settings, say)
        let elsewhere = self.focus_at_tick.is_some() && self.focus_at_tick != self.filter_id;
        if !elsewhere && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.quick.is_some() {
                self.quick = None;
            } else if self.selected.is_some() {
                self.selected = None;
            } else if !self.filter.is_empty() {
                self.filter.clear();
            } else {
                self.focus = None;
            }
        }
    }

    /// Space opens the quick look for the album under the pointer (else the one in the sidebar), and closes
    /// it again. While it is open, Left and Right step through the albums in the grid's order. None of this
    /// happens while something has the keyboard, so a space typed in the filter box is only a space.
    fn quick_keys(&mut self, ctx: &egui::Context) {
        let hover = self.hover.take();
        if self.focus_at_tick.is_some() || ctx.memory(|m| m.focused()).is_some() {
            return;
        }
        let (space, left, right) = ctx.input(|i| (i.key_pressed(egui::Key::Space), i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight)));
        if space {
            self.quick = if self.quick.is_some() { None } else { hover.or_else(|| self.selected.clone()) };
        }
        if left != right {
            if let Some((at, _)) = self.quick_place() {
                let to = if right { at + 1 } else { at.wrapping_sub(1) };
                if let Some(a) = self.view.get(to).and_then(|&k| self.albums.get(k)) {
                    self.quick = Some(a.path.clone());
                    if self.mode == Mode::Albums || self.focus.is_some() {
                        self.scroll_to = Some(a.path.clone()); // the grid behind keeps up
                    }
                }
            }
        }
    }

    /// Where the quick look's album is in the grid's order: (its place, of how many).
    fn quick_place(&self) -> Option<(usize, usize)> {
        let q = self.quick.as_ref()?;
        let at = self.view.iter().position(|&k| self.albums.get(k).is_some_and(|a| &a.path == q))?;
        Some((at, self.view.len()))
    }

    fn refresh_view(&mut self, results: &Results) {
        let psig = (results.artists.len(), results.generated_ts);
        if self.plays_sig != Some(psig) {
            self.plays_sig = Some(psig);
            self.plays = results.artists.iter().filter(|a| a.plays_all > 0).map(|a| (a.key.clone(), a.plays_all)).collect();
        }
        let sig = ViewSig {
            filter: self.filter.clone(),
            sort: self.sort,
            problems: self.only_problems && self.marks_known,
            focus: self.focus.clone(),
            albums_rev: self.albums_rev,
            marks_rev: self.marks_rev,
            plays: psig,
        };
        if self.view_sig.as_ref() == Some(&sig) {
            return;
        }
        if let Some(old) = &self.view_sig {
            if old.filter != sig.filter || old.sort != sig.sort || old.problems != sig.problems {
                self.to_top = [true; 3];
            } else if old.focus != sig.focus && sig.focus.is_some() {
                self.to_top[2] = true;
            }
        }
        let words: Vec<String> = self.filter.to_lowercase().split_whitespace().map(|w| w.to_string()).collect();
        let (albums, plays, marks, sort) = (&self.albums, &self.plays, &self.marks, self.sort);
        let mark = |i: usize| marks.get(i).copied().unwrap_or(0);
        let by_artist = |a: &Album, b: &Album| {
            a.sort_artist.cmp(&b.sort_artist).then(a.loose.cmp(&b.loose)).then(a.year.unwrap_or(i32::MAX).cmp(&b.year.unwrap_or(i32::MAX))).then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        };
        let played = |key: &str| plays.get(key).copied().unwrap_or(0);
        // everything that passes the filter, by artist and then oldest first
        let mut hits: Vec<usize> = (0..albums.len()).filter(|&i| words.iter().all(|w| albums[i].search.contains(w)) && (!sig.problems || mark(i) != 0)).collect();
        hits.sort_by(|&x, &y| by_artist(&albums[x], &albums[y]));

        let mut totals: HashMap<&str, usize> = HashMap::new();
        for a in albums {
            *totals.entry(a.sort_artist.as_str()).or_default() += 1;
        }
        let mut groups: Vec<Group> = vec![];
        for &i in &hits {
            let a = &albums[i];
            if groups.last().map(|g| g.key != a.sort_artist).unwrap_or(true) {
                let total = totals.get(a.sort_artist.as_str()).copied().unwrap_or(0);
                groups.push(Group { key: a.sort_artist.clone(), name: a.artist.clone(), stat_key: a.stat_key.clone(), albums: vec![], mosaic: vec![], total, newest: i32::MIN, worst: 0 });
            }
            let Some(g) = groups.last_mut() else { continue };
            g.albums.push(i);
            g.newest = g.newest.max(a.year.unwrap_or(i32::MIN));
            if matches!(mark(i), 1 | 2) && (g.worst == 0 || mark(i) < g.worst) {
                g.worst = mark(i);
            }
            if g.mosaic.len() < 4 && !a.art.is_empty() {
                g.mosaic.push(i);
            }
        }
        // the sorts below are stable, so ties stay in artist order
        let mut view: Vec<usize> = hits.into_iter().filter(|&i| sig.focus.as_ref().map(|f| &albums[i].sort_artist == f).unwrap_or(true)).collect();
        match sort {
            Sort::Artist => {}
            Sort::Newest => {
                groups.sort_by(|a, b| b.newest.cmp(&a.newest));
                view.sort_by(|&x, &y| albums[y].year.unwrap_or(i32::MIN).cmp(&albums[x].year.unwrap_or(i32::MIN)));
            }
            Sort::Played => {
                groups.sort_by(|a, b| played(&b.stat_key).cmp(&played(&a.stat_key)));
                view.sort_by(|&x, &y| played(&albums[y].stat_key).cmp(&played(&albums[x].stat_key)));
            }
        }
        self.view = view;
        self.groups = groups;
        self.view_sig = Some(sig);
    }

    /// Draws the page in the space under the app's row of page names.
    pub fn ui(&mut self, ui: &mut Ui, cfg: &Config, results: &Results) {
        let dir = self.dir_override.clone().unwrap_or_else(|| cfg.music_dir.clone());
        if let Some(size) = self.debug_size.take() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        if self.scanning.is_none() && self.scanned_dir.as_deref() != Some(dir.as_str()) {
            self.start_scan(cfg, &dir);
        }
        self.refresh_marks(ui.input(|i| i.time));
        self.keys(ui.ctx());
        self.refresh_view(results);
        self.quick_keys(ui.ctx());

        let mut rescan = false;
        egui::Panel::top("lib-top").show_separator_line(false).frame(egui::Frame::new().fill(theme::BG()).inner_margin(egui::Margin { left: theme::PAGE_MARGIN, right: theme::PAGE_MARGIN, top: 22, bottom: 0 })).show(ui, |ui| rescan = self.top_strip(ui, &dir));

        let mut acts = vec![];
        let selected = self.selected.as_ref().and_then(|p| self.albums.iter().position(|a| &a.path == p));
        if let Some(i) = selected {
            // the album's card: as wide as two of its small covers and the room around them
            egui::Panel::right("lib-album")
                .resizable(false)
                .show_separator_line(false)
                .exact_size(TILE * 2.0 + SIDE_GAP + 3.0 * theme::PAGE_MARGIN as f32)
                .frame(egui::Frame::new().fill(theme::BG()).inner_margin(egui::Margin { left: 0, right: theme::PAGE_MARGIN, top: 12, bottom: theme::PAGE_MARGIN }))
                .show(ui, |ui| self.sidebar(ui, i, &mut acts));
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG()).inner_margin(egui::Margin { left: theme::PAGE_MARGIN, right: 6, top: 12, bottom: 0 })).show(ui, |ui| {
            if self.fail.is_some() || self.albums.is_empty() {
                // (the sentence at the top has said it: not found, being read, or nothing there)
            } else {
                let one_artist = self.focus_name();
                let artists = self.mode == Mode::Artists && one_artist.is_none();
                let nothing = if artists { self.groups.is_empty() } else { self.view.is_empty() };
                if nothing {
                    let problems = self.only_problems && self.marks_known;
                    theme::empty(
                        ui,
                        &match (one_artist, self.filter.trim().is_empty(), problems) {
                            (Some(name), true, _) => format!("The last check found no problems in albums by {name}."),
                            (Some(name), false, _) => format!("No album by {name} matches the filter."),
                            (None, true, _) => "The last check found no problems in any album.".to_string(),
                            (None, false, true) => "No album with a problem matches the filter.".to_string(),
                            (None, false, false) => "No artist or album matches the filter.".to_string(),
                        },
                    );
                } else if artists {
                    self.artist_grid(ui, &mut acts);
                } else {
                    self.grid(ui, selected, &mut acts);
                }
            }
        });

        if rescan && self.scanning.is_none() {
            self.start_scan(cfg, &dir);
        }
        for act in acts {
            match act {
                Act::Close => self.selected = None,
                Act::Select(p) => self.selected = Some(p),
                Act::Open(p) => {
                    let _ = open::that(p);
                }
                Act::RescanOne(i) => self.start_rescan_one(i),
                Act::Focus(key) => self.focus = Some(key),
                Act::Quick(p) => self.quick = Some(p),
            }
        }
        self.quick_look(ui.ctx());
    }

    /// What the page is showing, as the large sentence at its top (in parts, each with its color) and the
    /// quiet line under it.
    fn sentence(&self, dir: &str) -> (Vec<(String, Color32)>, String) {
        let plain = |s: &str, c: Color32| vec![(s.to_string(), c)];
        // (number, " of ", the whole): the quiet word in the middle steps back
        let of = |n: usize, joint: &str, whole: String| vec![(commas(n as u64), TEXT()), (format!(" {joint} "), DIM()), (whole, TEXT())];
        let reading = match &self.scanning {
            Some(Some((done, total))) => Some(format!("Reading {dir}, {} of {} artists", commas(*done as u64), commas(*total as u64))),
            Some(None) => Some(format!("Reading {dir}")),
            None => None,
        };
        if let Some(fail) = &self.fail {
            return match fail {
                // (none chosen yet is not a fault: the page says what to do)
                Fail::Missing if dir.trim().is_empty() => (plain("No music folder is chosen yet", TEXT()), "The Library, Health and imports all work from one folder.".to_string()),
                Fail::Missing => (plain("The music folder was not found", theme::WARN()), format!("The drive may not be connected.  ·  {dir}")),
                Fail::Other(why) => (plain(why, theme::WARN()), dir.to_string()),
            };
        }
        if self.albums.is_empty() {
            return match reading {
                Some(r) => (plain("Reading your library", DIM()), r),
                None => (plain("No albums found", TEXT()), format!("in {dir}")),
            };
        }
        let under = reading.unwrap_or_else(|| format!("in {dir}"));
        let artists = self.mode == Mode::Artists && self.focus.is_none();
        let words = if let Some(name) = self.focus_name() {
            plain(&name, TEXT())
        } else if artists && self.groups.len() != self.n_artists {
            of(self.groups.len(), "of", plural(self.n_artists, "artist", "artists"))
        } else if !artists && self.view.len() != self.albums.len() {
            of(self.view.len(), "of", plural(self.albums.len(), "album", "albums"))
        } else {
            vec![(plural(self.albums.len(), "album", "albums"), TEXT()), (" by ".to_string(), DIM()), (plural(self.n_artists, "artist", "artists"), TEXT())]
        };
        (words, under)
    }

    /// The top of the page, in the middle: what is showing as one large sentence with the folder under it,
    /// then one row of controls (the filter, Albums or Artists, the sort, "With problems") with "Rescan"
    /// at its right end, and the thin line that fills while the folder is read. Returns true when Rescan
    /// (or "Try again") was clicked.
    fn top_strip(&mut self, ui: &mut Ui, dir: &str) -> bool {
        let mut rescan = false;
        let (words, under) = self.sentence(dir);
        ui.vertical_centered(|ui| {
            let mut job = LayoutJob::default();
            for (text, color) in &words {
                job.append(text, 0.0, egui::text::TextFormat { font_id: theme::display(34.0), color: *color, ..Default::default() });
            }
            ui.add(Label::new(job).wrap());
            ui.add_space(1.0);
        });
        // one artist's albums: what they have, and the way back, where the folder otherwise stands
        let one_artist = self.focus.as_ref().and_then(|key| self.albums.iter().find(|a| &a.sort_artist == key)).filter(|_| self.fail.is_none()).map(|first| {
            let n = self.albums.iter().filter(|a| a.sort_artist == first.sort_artist).count();
            let mut facts = vec![plural(n, "album", "albums")];
            if let Some(p) = self.plays.get(&first.stat_key) {
                facts.push(format!("{} on Last.fm", plural(*p as usize, "play", "plays")));
            }
            facts.join("  ·  ")
        });
        // (a row of one height either way, so the controls under it stay where they are)
        let row = vec2(ui.available_width(), 18.0);
        match one_artist {
            Some(facts) => {
                ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Center), |ui| {
                    theme::centered_row(ui, "lib-one-artist", 0.0, |ui| {
                        ui.spacing_mut().item_spacing.x = 16.0;
                        ui.label(RichText::new(facts).size(12.5).color(DIM()));
                        if theme::quiet(ui, "All artists", 12.5, true).clicked() {
                            self.focus = None;
                        }
                    });
                });
            }
            None => {
                ui.allocate_ui_with_layout(row, Layout::top_down(Align::Center), |ui| ui.add(Label::new(RichText::new(under).size(12.5).color(FAINT())).truncate()));
            }
        }
        ui.add_space(14.0);
        if self.fail.is_some() {
            ui.horizontal(|ui| {
                ui.set_min_height(26.0);
                theme::centered_row(ui, "lib-try-again", 0.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 18.0;
                    // no folder chosen: choosing one is the one thing to do; one that is not there may
                    // only be a drive that is not connected, so trying again comes first
                    let none = dir.trim().is_empty();
                    if !none {
                        rescan = theme::quiet(ui, "Try again", 14.0, self.scanning.is_none()).clicked();
                    }
                    let choose = if none { theme::main_action(ui, "Choose a folder", true) } else { theme::quiet(ui, "Choose a folder", 14.0, true) };
                    if choose.clicked() {
                        crate::links::go(crate::links::Go::ChooseMusicFolder);
                    }
                });
            });
        } else {
            // a window too narrow for one row has two: what to look at, then how
            let narrow = ui.available_width() < if self.marks_known { 900.0 } else { 790.0 };
            ui.horizontal(|ui| {
                ui.set_min_height(26.0);
                theme::centered_row(ui, "lib-controls", if narrow { 0.0 } else { 76.0 }, |ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    self.look_words(ui);
                    if !narrow {
                        divider(ui);
                        self.sort_words(ui);
                    }
                });
                if !narrow {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| rescan = self.rescan_word(ui));
                }
            });
            if narrow {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.set_min_height(24.0);
                    theme::centered_row(ui, "lib-controls-sort", 0.0, |ui| {
                        ui.spacing_mut().item_spacing.x = 16.0;
                        self.sort_words(ui);
                        divider(ui);
                        rescan = self.rescan_word(ui);
                    });
                });
            }
        }
        ui.add_space(10.0);
        // the thin line is always given its room, so the page does not move when a scan starts
        let w = ui.available_width();
        match self.scanning {
            Some(Some((done, total))) => theme::progress(ui, done as f32 / total.max(1) as f32, w, ACCENT2()),
            Some(None) => {
                // the number of artists is not known yet: a short piece that travels along the line
                let (rect, _) = ui.allocate_exact_size(vec2(w, 4.0), Sense::hover());
                ui.painter().rect_filled(rect, 2, BORDER());
                let t = (ui.input(|i| i.time) * 0.7).fract() as f32;
                let piece = w * 0.2;
                let x = rect.left() - piece + (w + piece) * t;
                let part = Rect::from_min_max(pos2(x.max(rect.left()), rect.top()), pos2((x + piece).min(rect.right()), rect.bottom()));
                ui.painter().rect_filled(part, 2, ACCENT2());
                ui.ctx().request_repaint();
            }
            None => {
                ui.allocate_exact_size(vec2(w, 4.0), Sense::hover());
            }
        }
        rescan
    }

    /// The filter box, and Albums or Artists.
    fn look_words(&mut self, ui: &mut Ui) {
        let r = ui.add(TextEdit::singleline(&mut self.filter).hint_text("Filter by artist or album").desired_width(230.0));
        self.filter_id = Some(r.id);
        if std::mem::take(&mut self.focus_filter) {
            r.request_focus();
        }
        ui.add_space(6.0);
        for (mode, name) in [(Mode::Albums, "Albums"), (Mode::Artists, "Artists")] {
            // "Artists" while one artist is showing goes back to all of them
            if choice(ui, name, self.mode == mode) {
                self.mode = mode;
                self.focus = None;
            }
        }
    }

    /// The sort, and "With problems".
    fn sort_words(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Sort by").size(14.0).color(FAINT()));
        for (sort, name) in [(Sort::Artist, "Artist"), (Sort::Newest, "Newest"), (Sort::Played, "Most played")] {
            if choice(ui, name, self.sort == sort) {
                self.sort = sort;
            }
        }
        if self.marks_known {
            divider(ui);
            if choice(ui, "With problems", self.only_problems) {
                self.only_problems = !self.only_problems;
            }
        }
    }

    /// "Rescan", a quiet word; "Scanning" while the folder is being read.
    fn rescan_word(&self, ui: &mut Ui) -> bool {
        let busy = self.scanning.is_some();
        theme::quiet(ui, if busy { "Scanning" } else { "Rescan" }, 13.5, !busy).clicked()
    }

    /// The artist whose albums are showing, when it is one artist's.
    fn focus_name(&self) -> Option<String> {
        let key = self.focus.as_ref()?;
        self.albums.iter().find(|a| &a.sort_artist == key).map(|a| a.artist.clone())
    }

    /// The scrolling grid of cover tiles. Only rows on or near the screen are built.
    fn grid(&mut self, ui: &mut Ui, selected: Option<usize>, acts: &mut Vec<Act>) {
        let one_artist = self.focus.is_some();
        let Self { albums, view, plays, marks, to_top, scroll_to, rescanning_one, hover, grid_px, .. } = self;
        let mut area = ScrollArea::vertical().id_salt(if one_artist { "lib-one-artist" } else { "lib-grid" }).auto_shrink([false, false]);
        let bring = scroll_to.take();
        if std::mem::take(&mut to_top[if one_artist { 2 } else { 0 }]) && bring.is_none() {
            area = area.vertical_scroll_offset(0.0);
        }
        area.show(ui, |ui| {
            let avail = ui.available_width() - 12.0; // room for the scroll bar
            let (cols, gap, size) = grid_size(ui, grid_px, avail);
            let row_h = size.side + TILE_TEXT;
            ui.spacing_mut().item_spacing.y = 0.0;
            let near = ui.clip_rect().expand2(vec2(0.0, row_h * 1.5));
            let lead = short_row_lead(ui, avail, view.len(), cols, size.side, gap);
            for row in view.chunks(cols) {
                let (row_rect, _) = ui.allocate_exact_size(vec2(avail, row_h), Sense::hover());
                ui.add_space(ROW_GAP);
                if bring.as_ref().is_some_and(|p| row.iter().any(|&i| &albums[i].path == p)) {
                    ui.scroll_to_rect(row_rect, Some(Align::Center));
                }
                if !row_rect.intersects(near) {
                    continue;
                }
                let visible = ui.is_rect_visible(row_rect.expand(8.0));
                for (j, &i) in row.iter().enumerate() {
                    let a = &albums[i];
                    if !visible {
                        cover_of(ui, a, size.px); // about to scroll into view: have its cover ready
                        continue;
                    }
                    let year = a.year.map(|y| y.to_string()).unwrap_or_default();
                    // under one artist's heading the name would only repeat, so the year moves up
                    let (second, third) = if one_artist {
                        let tracks = plural(a.tracks.len(), "track", "tracks");
                        if year.is_empty() { (tracks, String::new()) } else { (year, tracks) }
                    } else {
                        let third = match plays.get(&a.stat_key) {
                            Some(n) if year.is_empty() => format!("{} artist plays", commas(*n)),
                            Some(n) => format!("{year}  ·  {} artist plays", commas(*n)),
                            None => year,
                        };
                        (a.artist.clone(), third)
                    };
                    let min = row_rect.min + vec2(lead + j as f32 * (size.side + gap), 0.0);
                    let mark = marks.get(i).copied().unwrap_or(0);
                    let r = tile(ui, a, min, size, selected == Some(i), &second, &third, mark);
                    if r.hovered() {
                        *hover = Some(a.path.clone());
                    }
                    if r.clicked() {
                        acts.push(if selected == Some(i) { Act::Close } else { Act::Select(a.path.clone()) });
                    }
                    album_menu(&r, a, i, mark, rescanning_one.as_ref() == Some(&a.path), acts);
                }
            }
            ui.add_space(14.0);
        });
    }

    /// The Artists view: one tile per artist, built the same way as the album grid.
    fn artist_grid(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let Self { albums, groups, plays, to_top, grid_px, .. } = self;
        let mut area = ScrollArea::vertical().id_salt("lib-artists").auto_shrink([false, false]);
        if std::mem::take(&mut to_top[1]) {
            area = area.vertical_scroll_offset(0.0);
        }
        area.show(ui, |ui| {
            let avail = ui.available_width() - 12.0; // room for the scroll bar
            let (cols, gap, size) = grid_size(ui, grid_px, avail);
            let row_h = size.side + TILE_TEXT;
            ui.spacing_mut().item_spacing.y = 0.0;
            let near = ui.clip_rect().expand2(vec2(0.0, row_h * 1.5));
            let lead = short_row_lead(ui, avail, groups.len(), cols, size.side, gap);
            for row in groups.chunks(cols) {
                let (row_rect, _) = ui.allocate_exact_size(vec2(avail, row_h), Sense::hover());
                ui.add_space(ROW_GAP);
                if !row_rect.intersects(near) {
                    continue;
                }
                let visible = ui.is_rect_visible(row_rect.expand(8.0));
                for (j, g) in row.iter().enumerate() {
                    let min = row_rect.min + vec2(lead + j as f32 * (size.side + gap), 0.0);
                    if !visible {
                        let cover = Rect::from_min_size(min, vec2(size.side, size.side));
                        mosaic(ui, albums, &g.mosaic, cover, size, false); // about to scroll into view
                        continue;
                    }
                    let count = if g.albums.len() == g.total { plural(g.total, "album", "albums") } else { format!("{} of {}", commas(g.albums.len() as u64), plural(g.total, "album", "albums")) };
                    let played = plays.get(&g.stat_key).map(|n| plural(*n as usize, "play", "plays")).unwrap_or_default();
                    let r = artist_tile(ui, albums, g, min, size, &count, &played);
                    if r.clicked() {
                        acts.push(Act::Focus(g.key.clone()));
                    }
                    artist_menu(&r, albums, g, acts);
                }
            }
            ui.add_space(14.0);
        });
    }

    /// One album's details, on a card at the right of the grid. Has its own scroll area, separate from
    /// the grid's.
    fn sidebar(&mut self, ui: &mut Ui, i: usize, acts: &mut Vec<Act>) {
        let Self { albums, plays, marks, rescanning_one, side_scroll, hover, .. } = self;
        let a = &albums[i];
        let card = ui.max_rect();
        ui.painter().rect_filled(card, theme::CARD_ROUND, theme::CARD());
        // the cover's own color: a soft wash down from the top of the card, the title, the thin rules
        let tint = Tint::of(ui.ctx(), a);
        if tint.on > 0.0 {
            let area = Rect::from_min_max(card.min, pos2(card.right(), (card.top() + card.width() + 240.0).min(card.bottom())));
            wash(ui.painter(), area, tint.color, 0.32 * tint.on);
        }
        // inside the card's edges (the scroll bar has its own room at the right)
        let room = Rect::from_min_max(card.min + vec2(theme::PAGE_MARGIN as f32, 12.0), card.max - vec2(theme::PAGE_MARGIN as f32 - 12.0, 12.0));
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(room).layout(Layout::top_down(Align::Min)));
        inner.set_clip_rect(card.intersect(ui.clip_rect()));
        let ui = &mut inner;
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.add_space(8.0);
            if theme::close_mark(ui, "Close", "Close").clicked() {
                acts.push(Act::Close);
            }
        });
        ui.add_space(4.0);
        let mut area = ScrollArea::vertical().id_salt(("lib-album-scroll", &a.path)).auto_shrink([false, false]);
        if let Some(y) = side_scroll.take() {
            area = area.vertical_scroll_offset(y);
        }
        area.show(ui, |ui| {
            let w = ui.available_width() - 12.0; // room for the scroll bar, beside the content instead of over it
            ui.set_width(w);
            let ppp = ui.ctx().pixels_per_point();
            let (rect, _) = ui.allocate_exact_size(vec2(w, w), Sense::hover());
            let rect = Rect::from_min_size(snap(rect.min, ppp), rect.size());
            covers::paint(ui, cover_of(ui, a, (w * ppp).round() as u32), rect, 8);
            ui.add_space(12.0);
            ui.label(RichText::new(&a.title).size(20.0).strong().color(tint.title()));
            ui.label(RichText::new(&a.artist).size(15.0).color(DIM()));
            ui.add_space(4.0);
            ui.label(RichText::new(facts_line(a)).size(13.5).color(DIM()));
            if let Some(n) = plays.get(&a.stat_key) {
                ui.label(RichText::new(format!("{} of {} on Last.fm", plural(*n as usize, "play", "plays"), a.artist)).size(13.5).color(DIM()));
            }
            // what the Health page's last check says about this folder (it only tells the worst problem, not how many)
            let found = match marks.get(i).copied().unwrap_or(0) {
                1 => Some(("A serious problem was found in the last check", theme::RED())),
                2 => Some(("A problem worth fixing was found in the last check", theme::WARN())),
                3 => Some(("A minor problem was found in the last check", DIM())),
                _ => None,
            };
            if let Some((line, color)) = found {
                ui.add_space(8.0);
                ui.label(RichText::new(line).size(13.5).color(color));
                if theme::quiet(ui, "See it in Health", 13.0, true).clicked() {
                    crate::links::go(crate::links::Go::HealthAlbum { artist: a.artist.clone(), album: folder_name(a) });
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 18.0;
                if theme::quiet(ui, "Open folder", 13.0, true).clicked() {
                    acts.push(Act::Open(a.path.clone()));
                }
                let busy = rescanning_one.as_ref() == Some(&a.path);
                if theme::quiet(ui, "Rescan this album", 13.0, !busy).clicked() {
                    acts.push(Act::RescanOne(i));
                }
            });
            ui.add_space(4.0);
            ui.add(Label::new(RichText::new(a.path.to_string_lossy()).size(12.0).color(FAINT())).wrap());

            ui.add_space(10.0);
            rule(ui, w, &tint);
            ui.label(RichText::new("TRACKS").size(11.0).extra_letter_spacing(1.4).color(tint.heading()));
            ui.add_space(4.0);
            track_list(ui, a, w);

            // the artist: their other albums, and the way to their panel in Recs
            let mut others: Vec<(usize, &Album)> = albums.iter().enumerate().filter(|(_, o)| o.sort_artist == a.sort_artist && o.path != a.path).collect();
            others.sort_by_key(|(_, o)| (o.loose, o.year.unwrap_or(i32::MAX), o.title.to_lowercase()));
            let heading = if others.is_empty() { a.artist.clone() } else { format!("More by {}", a.artist) };
            ui.add_space(10.0);
            rule(ui, w, &tint);
            let (row, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
            let recs = "See artist in Recs";
            let recs_w = ui.painter().layout_no_wrap(recs.to_string(), FontId::proportional(12.5), ACCENT2()).size().x;
            caption_line(ui, pos2(row.left(), row.top() + 2.0), w - recs_w - 14.0, &heading, tint.heading());
            let at = Rect::from_min_max(pos2(row.right() - recs_w, row.top()), row.max);
            if ui.put(at, |ui: &mut Ui| theme::quiet(ui, recs, 12.5, true)).clicked() {
                crate::links::go(crate::links::Go::RecsArtist { artist: a.artist.clone() });
            }
            if !others.is_empty() {
                ui.add_space(8.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                for pair in others.chunks(2) {
                    let (row, _) = ui.allocate_exact_size(vec2(w, TILE + TILE_TEXT), Sense::hover());
                    ui.add_space(ROW_GAP);
                    if !ui.is_rect_visible(row.expand(8.0)) {
                        continue;
                    }
                    for (j, (k, o)) in pair.iter().enumerate() {
                        let year = o.year.map(|y| y.to_string()).unwrap_or_default();
                        let mark = marks.get(*k).copied().unwrap_or(0);
                        let r = tile(ui, o, row.min + vec2(j as f32 * (w - TILE), 0.0), Size::fixed(ui), false, &year, "", mark);
                        if r.hovered() {
                            *hover = Some(o.path.clone());
                        }
                        if r.clicked() {
                            acts.push(Act::Select(o.path.clone()));
                        }
                        album_menu(&r, o, *k, mark, rescanning_one.as_ref() == Some(&o.path), acts);
                    }
                }
            }
            ui.add_space(18.0);
        });
    }

    /// The quick look: one album large, over the dimmed page. Its cover as big as fits, and beside it the
    /// title, artist, year and tracks, on a panel in the cover's color. A click outside it closes it
    /// (the keys are in `keys` and `quick_keys`).
    fn quick_look(&mut self, ctx: &egui::Context) {
        let open = ctx.animate_bool_with_time(egui::Id::unique("lib-quick-open"), self.quick.is_some(), 0.14);
        let Some(i) = self.quick.as_ref().and_then(|q| self.albums.iter().position(|a| &a.path == q)) else {
            self.quick = None;
            return;
        };
        let place = self.quick_place();
        let a = &self.albums[i];
        let ppp = ctx.pixels_per_point();
        // the size the grid behind has this album's cover at
        let grid_px = match self.grid_px.current() {
            0 => (TILE * ppp).round() as u32,
            px => px,
        };
        let screen = ctx.content_rect();
        const PAD: f32 = 26.0;
        // the cover in steps of 20 points, so dragging the window's edge does not decode it at every size
        let side = (screen.height() - 120.0).min(screen.width() - 48.0 - 3.0 * PAD - 250.0).min(520.0);
        let side = ((side / 20.0).floor() * 20.0).max(160.0);
        let text_w = (screen.width() - 48.0 - 3.0 * PAD - side).clamp(200.0, 340.0);
        let size = vec2(3.0 * PAD + side + text_w, 2.0 * PAD + side);
        let panel = Rect::from_min_size(snap(screen.center() - size / 2.0, ppp), size);
        let tint = Tint::of(ctx, a);
        let mut close = false;
        egui::Area::new(egui::Id::unique("lib-quick-look")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            // the whole window is this layer's, so nothing behind it is under the pointer
            let back = ui.allocate_rect(screen, Sense::click());
            ui.interact(panel, ui.scope_id().with("panel"), Sense::click()); // a click on the panel is not a click outside
            close = back.clicked();
            ui.painter().rect_filled(screen, 0, Color32::from_black_alpha((190.0 * open) as u8));
            ui.set_opacity(open);
            ui.painter().rect_filled(panel, 14, mix(BG2(), tint.color, 0.13 * tint.on));
            ui.painter().rect_stroke(panel, 14, Stroke::new(1.0, mix(BORDER(), tint.color, 0.45 * tint.on)), egui::StrokeKind::Inside);
            let cover = Rect::from_min_size(panel.min + vec2(PAD, PAD), vec2(side, side));
            big_cover(ui, a, cover, (side * ppp).round() as u32, grid_px);

            let hint = match place {
                Some((at, n)) if n > 1 && text_w >= 300.0 => format!("{} of {}  ·  Left and Right step  ·  Space closes", commas(at as u64 + 1), commas(n as u64)),
                Some((at, n)) if n > 1 => format!("{} of {}  ·  Space closes", commas(at as u64 + 1), commas(n as u64)),
                _ => "Space closes".to_string(),
            };
            text_line(ui, pos2(cover.right() + PAD, cover.bottom() - 15.0), text_w, &hint, 12.0, FAINT());
            let column = Rect::from_min_max(pos2(cover.right() + PAD, cover.top()), pos2(panel.right() - PAD + 12.0, cover.bottom() - 26.0));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(column).layout(Layout::top_down(Align::Min)));
            let w = column.width() - 12.0; // the scroll bar's room
            ui.add_space(2.0);
            ui.add(Label::new(RichText::new(&a.title).size(24.0).strong().color(tint.title())).wrap());
            ui.add_space(2.0);
            ui.add(Label::new(RichText::new(&a.artist).size(16.0).color(TEXT())).wrap());
            ui.add_space(4.0);
            ui.label(RichText::new(facts_line(a)).size(13.5).color(DIM()));
            ui.add_space(10.0);
            rule(&mut ui, w, &tint);
            ui.add_space(2.0);
            ScrollArea::vertical().id_salt(("lib-quick-tracks", a.uid)).auto_shrink([false, false]).show(&mut ui, |ui| {
                ui.set_width(w);
                track_list(ui, a, w);
            });

        });
        if close {
            self.quick = None;
        }
    }
}

// ------------------------------------------------------------------ small pieces

/// The color an album's sidebar and quick look take from its cover, and how far it has faded in (0 to 1:
/// the color is only known once the cover has been decoded).
struct Tint {
    color: Color32,
    on: f32,
}

impl Tint {
    fn of(ctx: &egui::Context, a: &Album) -> Self {
        let color = covers::accent_for(a.cover_key);
        let on = ctx.animate_bool_with_time(egui::Id::unique(("lib-tint", a.uid)), color.is_some(), 0.45);
        Tint { color: color.unwrap_or(DIM()), on }
    }
    /// for the album's title: the cover's color, lightened
    fn title(&self) -> Color32 {
        mix(TEXT(), mix(self.color, TEXT(), 0.35), self.on)
    }
    fn heading(&self) -> Color32 {
        mix(DIM(), self.color, self.on)
    }
    fn rule(&self) -> Color32 {
        self.color.gamma_multiply(0.4 * self.on)
    }
}

/// `a` with `t` (0 to 1) of the way to `b`.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// `color` at `strength` along the top of `rect`, fading to nothing at its bottom.
fn wash(painter: &egui::Painter, rect: Rect, color: Color32, strength: f32) {
    const ROUND: f32 = theme::CARD_ROUND as f32; // the card's corners, which the wash keeps to
    if rect.height() <= ROUND {
        return;
    }
    let top = color.gamma_multiply(strength.clamp(0.0, 1.0));
    let cap = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + ROUND));
    painter.rect_filled(cap, egui::CornerRadius { nw: ROUND as u8, ne: ROUND as u8, sw: 0, se: 0 }, top);
    let rect = Rect::from_min_max(cap.left_bottom(), rect.max);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), Color32::TRANSPARENT);
    mesh.colored_vertex(rect.right_bottom(), Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// A thin line across, in the cover's color; with no color it only takes its room.
fn rule(ui: &mut Ui, w: f32, tint: &Tint) {
    let (line, _) = ui.allocate_exact_size(vec2(w, 9.0), Sense::hover());
    if tint.on > 0.0 {
        ui.painter().hline(line.x_range(), line.top() + 0.5, Stroke::new(1.0, tint.rule()));
    }
}

/// "2013  ·  12 tracks"
fn facts_line(a: &Album) -> String {
    let mut facts = vec![];
    if let Some(y) = a.year {
        facts.push(y.to_string());
    }
    facts.push(plural(a.tracks.len(), "track", "tracks"));
    facts.join("  ·  ")
}

/// An album's tracks, one to a line `w` wide, with a quiet heading where each disc starts.
fn track_list(ui: &mut Ui, a: &Album, w: f32) {
    // one width for the number column, so titles line up; no column at all when no file is numbered
    let no_w = match a.tracks.iter().map(|t| t.no.len()).max().unwrap_or(0) {
        0 => 0.0,
        1 | 2 => 22.0,
        _ => 40.0,
    };
    let mut disc = "";
    for t in &a.tracks {
        if t.disc != disc {
            disc = &t.disc;
            if !disc.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new(disc).size(12.0).color(FAINT()));
            }
        }
        let (row, _) = ui.allocate_exact_size(vec2(w, 21.0), Sense::hover());
        if ui.is_rect_visible(row) {
            ui.painter().text(pos2(row.left() + no_w, row.center().y), Align2::RIGHT_CENTER, &t.no, FontId::proportional(12.5), FAINT());
            let x = row.left() + if no_w > 0.0 { no_w + 10.0 } else { 0.0 };
            text_line(ui, pos2(x, row.top() + 1.0), row.right() - x, &t.title, 14.0, TEXT());
        }
    }
}

/// The quick look's cover at `px` pixels. Until that size is decoded, the tile's small cover (`tile_px`
/// pixels) stands in, drawn large, and the sharp one fades in over it.
fn big_cover(ui: &Ui, a: &Album, rect: Rect, px: u32, tile_px: u32) {
    let image = |t: &egui::TextureHandle, tint: Color32| egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size())).corner_radius(8).tint(tint).paint_at(ui, rect);
    let stand_in = || match cover_of(ui, a, tile_px) {
        Pic::Ready(t, _) => image(&t, Color32::WHITE),
        _ => covers::paint(ui, Pic::Loading, rect, 8),
    };
    match cover_of(ui, a, px) {
        Pic::Ready(t, shown) if shown < 1.0 => {
            stand_in();
            image(&t, Color32::WHITE.gamma_multiply(shown * shown * (3.0 - 2.0 * shown)));
        }
        Pic::Loading => stand_in(),
        pic => covers::paint(ui, pic, rect, 8),
    }
}

/// A right-click menu on `r`.
fn menu(r: &egui::Response, forced: bool, items: impl FnOnce(&mut Ui)) {
    if forced {
        egui::Popup::menu(r).open(true).show(items); // debug: `--lib-menu`
    } else {
        r.context_menu(items);
    }
}

/// What a right-click on an album's cover offers.
fn album_menu(r: &egui::Response, a: &Album, i: usize, mark: u8, rescanning: bool, acts: &mut Vec<Act>) {
    let forced = DEBUG_MENU.as_ref().is_some_and(|m| a.search.contains(m));
    menu(r, forced, |ui| {
        if ui.add(egui::Button::new("Quick look").shortcut_text("Space")).clicked() {
            acts.push(Act::Quick(a.path.clone()));
            ui.close();
        }
        if ui.button("Open folder").clicked() {
            acts.push(Act::Open(a.path.clone()));
            ui.close();
        }
        if mark != 0 && ui.button("See in Health").clicked() {
            crate::links::go(crate::links::Go::HealthAlbum { artist: a.artist.clone(), album: folder_name(a) });
            ui.close();
        }
        // this only lists the copies people share; nothing is fetched
        if !a.loose && ui.button("Find a better copy on Soulseek").clicked() {
            crate::slskd::ask_for(&format!("{} {}", a.artist, a.title));
            ui.close();
        }
        if ui.button("See artist in Recs").clicked() {
            crate::links::go(crate::links::Go::RecsArtist { artist: a.artist.clone() });
            ui.close();
        }
        if ui.add_enabled(!rescanning, egui::Button::new("Rescan this album")).clicked() {
            acts.push(Act::RescanOne(i));
            ui.close();
        }
    });
}

/// What a right-click on an artist's tile offers.
fn artist_menu(r: &egui::Response, albums: &[Album], g: &Group, acts: &mut Vec<Act>) {
    let forced = DEBUG_MENU.as_ref().is_some_and(|m| g.key.contains(m));
    menu(r, forced, |ui| {
        if ui.button("See artist in Recs").clicked() {
            crate::links::go(crate::links::Go::RecsArtist { artist: g.name.clone() });
            ui.close();
        }
        // an album folder sits in its artist's folder; loose tracks are in the artist's folder itself
        let folder = g.albums.first().map(|&i| &albums[i]).and_then(|a| if a.loose { Some(a.path.clone()) } else { a.path.parent().map(Path::to_path_buf) });
        if let Some(folder) = folder {
            if ui.button("Open folder").clicked() {
                acts.push(Act::Open(folder));
                ui.close();
            }
        }
    });
}

/// The album's cover at `px` pixels square, from the app's shared cover cache.
fn cover_of(ui: &Ui, a: &Album, px: u32) -> Pic {
    if *DEBUG_BY_FOLDER && !a.loose {
        return covers::for_folder(ui.ctx(), &a.path, px);
    }
    covers::for_sources(ui.ctx(), a.cover_key, &a.art, px)
}

/// How large a tile's cover is.
#[derive(Clone, Copy)]
struct Size {
    /// its side on screen, in points
    side: f32,
    /// the size its picture is asked for, in pixels: the size it is drawn at, except while the window is
    /// being resized (see `covers::Settle`)
    px: u32,
    /// the size asked for before that one, which stands in until the new one is decoded
    was: u32,
}

impl Size {
    /// A tile that is not in a grid: always `TILE`.
    fn fixed(ui: &Ui) -> Self {
        let px = (TILE * ui.ctx().pixels_per_point()).round() as u32;
        Size { side: TILE, px, was: px }
    }
}

/// How many tiles fit across `avail` and the space between them, with the covers grown from `TILE` so
/// that the row reaches the right edge: as many as fit at `TILE` with `GAP_MIN` between them, the gap
/// never more than `GAP_MAX`, the side a whole number of pixels (`covers::fill_row` has the rule). In the
/// smallest window with the sidebar open, two tiles a little closer beat one tile and a blank half.
fn columns(avail: f32, ppp: f32) -> covers::Tiles {
    covers::fill_row(avail, ppp, TILE, GAP_MIN, GAP_MAX)
}

/// A grid `avail` wide: how many tiles across, the space between them, and the size of their covers.
fn grid_size(ui: &Ui, settle: &mut covers::Settle, avail: f32) -> (usize, f32, Size) {
    let ppp = ui.ctx().pixels_per_point();
    let t = columns(avail, ppp);
    let px = settle.px(ui.ctx(), avail, (t.side * ppp).round() as u32);
    (t.cols, t.gap, Size { side: t.side, px, was: settle.was() })
}

/// Paints an album's cover in a tile. A cover whose size has just changed shows at its old size, stretched,
/// until the new one is decoded.
fn paint_cover(ui: &Ui, a: &Album, size: Size, rect: Rect) {
    let pic = cover_of(ui, a, size.px);
    if size.was == size.px {
        covers::paint(ui, pic, rect, 6);
    } else {
        covers::paint_over(ui, pic, a.cover_key, &[size.was], rect, 6);
    }
}

/// Moves a point onto the screen's pixel grid, so a cover drawn at its decoded size stays sharp.
fn snap(p: Pos2, ppp: f32) -> Pos2 {
    pos2((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp)
}

/// How far in a grid's tiles start: nothing for a grid of a row or more (its rows reach both edges), and
/// for fewer tiles than one row holds, what puts them in the middle (on a whole pixel).
fn short_row_lead(ui: &Ui, avail: f32, n: usize, cols: usize, side: f32, gap: f32) -> f32 {
    if n == 0 || n >= cols {
        return 0.0;
    }
    let ppp = ui.ctx().pixels_per_point();
    let used = n as f32 * side + (n - 1) as f32 * gap;
    (((avail - used) / 2.0).max(0.0) * ppp).round() / ppp
}

/// A thin upright line between two groups of words in the row of controls.
fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 15.0), Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), Stroke::new(1.0, BORDER()));
}

/// A choice in the top strip: a plain word, the chosen one bright and underlined.
fn choice(ui: &mut Ui, text: &str, selected: bool) -> bool {
    let t = RichText::new(text).size(14.0).color(if selected { TEXT() } else { DIM() });
    let r = ui.add(Label::new(if selected { t.strong() } else { t }).selectable(false).sense(Sense::click()));
    crate::script::seen(text, r.rect);
    if selected {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() + 1.0, Stroke::new(1.5, ACCENT()));
    } else if r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() + 1.0, Stroke::new(1.0, DIM()));
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

/// A small heading in a card, in capitals with its letters set a little apart, cut short with "…" if it
/// is wider than `width`.
fn caption_line(ui: &Ui, pos: Pos2, width: f32, text: &str, color: Color32) {
    if text.is_empty() || width <= 4.0 {
        return;
    }
    let mut job = LayoutJob::default();
    job.append(&text.to_uppercase(), 0.0, egui::text::TextFormat { font_id: FontId::proportional(11.0), color, extra_letter_spacing: 1.4, ..Default::default() });
    job.wrap = TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos, galley, color);
}

/// One line of text, cut short with "…" if it is wider than `width`.
fn text_line(ui: &Ui, pos: Pos2, width: f32, text: &str, size: f32, color: Color32) {
    if text.is_empty() || width <= 4.0 {
        return;
    }
    let mut job = LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
    job.wrap = TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos, galley, color);
}

/// The part every tile shares: the pointer, and the backdrop that comes up under the pointer or stays
/// under the selected tile. Returns how far the hover has come in (0 to 1).
fn tile_backdrop(ui: &Ui, r: &egui::Response, rect: Rect, selected: bool, forced: bool) -> f32 {
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let lift = ui.ctx().animate_bool_with_time(r.id.with("lift"), r.hovered() || forced, 0.12);
    let back = rect.expand2(vec2(7.0, 7.0)).translate(vec2(0.0, -1.0));
    if selected {
        ui.painter().rect_filled(back, 10, CARD());
    }
    if lift > 0.0 {
        ui.painter().rect_filled(back, 10, CARD_HOVER().gamma_multiply(lift));
    }
    lift
}

/// Over the cover: coral around the selected tile, a brighter edge under the pointer, and the health mark.
fn tile_marks(ui: &Ui, cover: Rect, selected: bool, lift: f32, mark: u8) {
    if selected {
        ui.painter().rect_stroke(cover, 6, Stroke::new(2.0, ACCENT()), egui::StrokeKind::Outside);
    } else if lift > 0.0 {
        ui.painter().rect_stroke(cover, 6, Stroke::new(1.0, DIM().gamma_multiply(lift)), egui::StrokeKind::Outside);
    }
    let color = match mark {
        1 => theme::RED(),
        2 => theme::WARN(),
        _ => return,
    };
    let at = pos2(cover.right() - 12.0, cover.top() + 12.0);
    ui.painter().circle_filled(at, 7.0, theme::BG());
    ui.painter().circle_filled(at, 4.0, color);
}

/// The two lines under a cover: the name bright, and under it a quieter word or two. Each has the whole
/// width and is cut off with "…" on its own if it is too long.
fn name_line(ui: &Ui, pos: Pos2, width: f32, name: &str, more: &str) {
    text_line(ui, pos, width, name, 13.5, TEXT());
    text_line(ui, pos2(pos.x, pos.y + 17.0), width, more, 12.0, DIM());
}

/// The smaller facts (year, plays) live on the cover itself and only show under the pointer: a dark strip
/// along the cover's bottom edge that fades in with the hover.
fn hover_facts(ui: &Ui, cover: Rect, lift: f32, text: &str) {
    if lift <= 0.0 || text.is_empty() {
        return;
    }
    let strip = Rect::from_min_max(pos2(cover.left(), cover.bottom() - 26.0), cover.max);
    ui.painter().rect_filled(strip, CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 }, Color32::from_black_alpha((200.0 * lift) as u8));
    text_line(ui, pos2(strip.left() + 8.0, strip.top() + 6.0), strip.width() - 16.0, text, 11.5, Color32::WHITE.gamma_multiply(lift));
}

/// A cover with two lines under it (the album, then `second`); `third` shows on the cover under the pointer.
/// Returns the tile's response (`.clicked()`).
fn tile(ui: &mut Ui, a: &Album, min: Pos2, size: Size, selected: bool, second: &str, third: &str, mark: u8) -> egui::Response {
    let ppp = ui.ctx().pixels_per_point();
    let min = snap(min, ppp);
    let cover = Rect::from_min_size(min, vec2(size.side, size.side));
    let rect = Rect::from_min_size(min, vec2(size.side, size.side + TILE_TEXT));
    let r = ui.interact(rect, ui.scope_id().with(("lib-tile", a.uid)), Sense::click());
    if crate::script::on() {
        crate::script::seen(&format!("album:{}", folder_name(a)), rect); // what a test script calls it, as on Health
    }
    let forced = DEBUG_HOVER.as_ref().is_some_and(|h| a.search.contains(h));
    let lift = tile_backdrop(ui, &r, rect, selected, forced);
    paint_cover(ui, a, size, cover);
    tile_marks(ui, cover, selected, lift, mark);
    let x = min.x + 1.0;
    let y = cover.bottom() + 7.0;
    hover_facts(ui, cover, lift, third);
    name_line(ui, pos2(x, y), size.side - 2.0, &a.title, second);
    r
}

/// An artist's covers in the square `cover`, none of them twice: one cover whole; two as tall halves side
/// by side; three as a tall half and two quarters; four as quarters. A tall half is the middle of its
/// cover, at full height. With `paint` false the covers are only asked for, not drawn.
fn mosaic(ui: &Ui, albums: &[Album], picks: &[usize], cover: Rect, size: Size, paint: bool) {
    let ppp = ui.ctx().pixels_per_point();
    match picks.len() {
        0 => {
            if paint {
                covers::paint(ui, Pic::None, cover, 6);
            }
        }
        1 => {
            if paint {
                paint_cover(ui, &albums[picks[0]], size, cover);
            } else {
                cover_of(ui, &albums[picks[0]], size.px);
            }
        }
        n => {
            // worked out in whole pixels, so each quarter is drawn at exactly its decoded size
            let line = ((2.0 * ppp).round() as u32).max(1);
            let half_of = |full: u32| full.saturating_sub(line) / 2;
            // where the quarters go follows the square being drawn; their pictures are asked for at the
            // size being decoded, which is the same except while the window is being resized
            let drawn = (cover.width() * ppp).round() as u32;
            let (side, far) = (half_of(drawn) as f32 / ppp, (drawn - half_of(drawn)) as f32 / ppp);
            let (full, half) = (size.px, half_of(size.px));
            let round = |on: bool| if on { 6 } else { 0 };
            // album `k` in quarter `q` (0 and 1 on top, 2 and 3 under them)
            let quarter = |k: usize, q: usize| {
                // a cover already decoded for the album grid is drawn smaller instead of being decoded again
                let a = &albums[picks[k]];
                let pic = covers::for_sources_or_larger(ui.ctx(), a.cover_key, &a.art, half, full);
                if !paint {
                    return;
                }
                // a quarter whose picture cannot be read stays a plain square: the note sign is for whole tiles
                let pic = if matches!(pic, Pic::None) { Pic::Loading } else { pic };
                let at = cover.min + vec2(if q % 2 == 1 { far } else { 0.0 }, if q >= 2 { far } else { 0.0 });
                let radius = CornerRadius { nw: round(q == 0), ne: round(q == 1), sw: round(q == 2), se: round(q == 3) };
                let rect = Rect::from_min_size(at, vec2(side, side));
                if size.was == size.px {
                    covers::paint(ui, pic, rect, radius);
                } else {
                    covers::paint_over(ui, pic, a.cover_key, &[half_of(size.was), size.was], rect, radius);
                }
            };
            // album `k` as the left or right half of the square: the middle of its cover, from top to bottom.
            // The cover is the one decoded for a whole tile, and the strip is drawn pixel for pixel.
            let tall = |k: usize, right: bool| {
                let a = &albums[picks[k]];
                let pic = cover_of(ui, a, full);
                if !paint {
                    return;
                }
                let (whole, strip) = (drawn.max(1), half_of(drawn));
                let skip = (whole - strip) / 2; // whole pixels, so the strip is not drawn between two of them
                let uv = Rect::from_min_max(pos2(skip as f32 / whole as f32, 0.0), pos2((skip + strip) as f32 / whole as f32, 1.0));
                let at = cover.min + vec2(if right { far } else { 0.0 }, 0.0);
                let radius = CornerRadius { nw: round(!right), sw: round(!right), ne: round(right), se: round(right) };
                let was: &[u32] = if size.was == size.px { &[] } else { &[size.was] };
                covers::paint_part(ui, pic, a.cover_key, was, uv, Rect::from_min_size(at, vec2(side, whole as f32 / ppp)), radius);
            };
            match n {
                2 => {
                    tall(0, false);
                    tall(1, true);
                }
                3 => {
                    tall(0, false);
                    quarter(1, 1);
                    quarter(2, 3);
                }
                _ => (0..4).for_each(|q| quarter(q, q)),
            }
        }
    }
}

/// An artist: their covers, then name, `second` and `third`. Returns the tile's response (`.clicked()`).
fn artist_tile(ui: &mut Ui, albums: &[Album], g: &Group, min: Pos2, size: Size, second: &str, third: &str) -> egui::Response {
    let ppp = ui.ctx().pixels_per_point();
    let min = snap(min, ppp);
    let cover = Rect::from_min_size(min, vec2(size.side, size.side));
    let rect = Rect::from_min_size(min, vec2(size.side, size.side + TILE_TEXT));
    let r = ui.interact(rect, ui.scope_id().with(("lib-artist", &g.key)), Sense::click());
    let forced = DEBUG_HOVER.as_ref().is_some_and(|h| g.key.contains(h));
    let lift = tile_backdrop(ui, &r, rect, false, forced);
    mosaic(ui, albums, &g.mosaic, cover, size, true);
    tile_marks(ui, cover, false, lift, g.worst);
    let x = min.x + 1.0;
    let y = cover.bottom() + 7.0;
    hover_facts(ui, cover, lift, third);
    name_line(ui, pos2(x, y), size.side - 2.0, &g.name, second);
    r
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn album(artist: &str, title: &str, added: u64, loose: bool) -> Album {
        Album {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            cover_key: 0,
            artist: artist.to_string(),
            stat_key: artist.to_lowercase(),
            title: title.to_string(),
            year: None,
            path: PathBuf::from(format!(r"X:\\Music\\{artist}\\{title}")),
            loose,
            tracks: vec![],
            art: vec![],
            search: format!("{} {}", artist.to_lowercase(), title.to_lowercase()),
            sort_artist: artist.to_lowercase(),
            added,
        }
    }

    fn page(albums: Vec<Album>) -> LibraryPage {
        let mut p = LibraryPage::new(&egui::Context::default());
        p.albums = albums;
        p.scanned_once = true;
        p.scanned_dir = Some("X:\\\\Music".to_string());
        p.after_change();
        p.view = (0..p.albums.len()).collect();
        p
    }

    fn titles(found: Vec<(String, String, PathBuf)>) -> Vec<String> {
        found.into_iter().map(|f| f.1).collect()
    }

    #[test]
    fn find_wants_every_word_and_puts_names_that_start_with_the_query_first() {
        let p = page(vec![
            album("Boards of Canada", "Geogaddi", 1, false),
            album("Can", "Tago Mago", 2, false),
            album("Neu!", "Canada Tapes", 3, false),
            album("Pelican", "Australasia", 4, false),
        ]);
        // "Can" the artist and "Canada Tapes" the title start with it; then a word that does; then the rest
        assert_eq!(titles(p.find("CAN", 10)), ["Tago Mago", "Canada Tapes", "Geogaddi", "Australasia"]);
        assert_eq!(titles(p.find("can", 2)), ["Tago Mago", "Canada Tapes"]);
        assert_eq!(titles(p.find("canada  boards", 10)), ["Geogaddi"]);
        assert_eq!(titles(p.find("tago mago", 10)), ["Tago Mago"]);
        assert!(p.find("canada zzz", 10).is_empty());
        assert!(p.find("   ", 10).is_empty());
        let hit = &p.find("geog", 10)[0];
        assert_eq!((hit.0.as_str(), hit.2.clone()), ("Boards of Canada", p.albums[0].path.clone()));
    }

    #[test]
    fn find_and_recent_are_empty_until_a_scan_has_finished() {
        let mut p = page(vec![album("Can", "Tago Mago", 2, false)]);
        p.scanned_once = false;
        assert!(p.find("can", 10).is_empty());
        assert!(p.recent(10).is_empty());
    }

    #[test]
    fn recent_is_newest_first_without_loose_tracks_or_unknown_dates() {
        let p = page(vec![
            album("A", "Old", 100, false),
            album("B", "New", 300, false),
            album("C", "Loose tracks", 400, true),
            album("D", "Undated", 0, false),
            album("E", "Middle", 200, false),
        ]);
        assert_eq!(titles(p.recent(10)), ["New", "Middle", "Old"]);
        assert_eq!(titles(p.recent(1)), ["New"]);
    }

    #[test]
    fn a_folder_that_exists_has_a_date_and_one_that_does_not_has_none() {
        assert!(added_secs(&std::env::temp_dir()) > 0);
        assert_eq!(added_secs(Path::new(r"X:\\no\\such\\folder\\anywhere")), 0);
    }

    fn press(p: &mut LibraryPage, key: egui::Key, typing: bool) {
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let ctx = p.ctx.clone();
        ctx.run_ui(egui::RawInput { events: vec![event], ..Default::default() }, |ui| {
            let ctx = &ui.ctx().clone();
            if typing {
                ctx.memory_mut(|m| m.request_focus(egui::Id::unique("a text box")));
            }
            p.keys(ctx);
            p.quick_keys(ctx);
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn space_opens_and_closes_the_quick_look_and_the_arrows_step_through_the_grid() {
        let mut p = page(vec![album("A", "One", 1, false), album("B", "Two", 2, false), album("C", "Three", 3, false)]);
        let path = |p: &LibraryPage, i: usize| Some(p.albums[i].path.clone());
        press(&mut p, egui::Key::Space, false);
        assert_eq!(p.quick, None, "nothing under the pointer and nothing selected");
        p.selected = path(&p, 0);
        p.hover = path(&p, 1);
        press(&mut p, egui::Key::Space, false);
        assert_eq!(p.quick, path(&p, 1), "the tile under the pointer wins over the selected album");
        press(&mut p, egui::Key::ArrowRight, false);
        assert_eq!(p.quick, path(&p, 2));
        press(&mut p, egui::Key::ArrowRight, false);
        assert_eq!(p.quick, path(&p, 2), "stops at the last album");
        press(&mut p, egui::Key::ArrowLeft, false);
        press(&mut p, egui::Key::ArrowLeft, false);
        press(&mut p, egui::Key::ArrowLeft, false);
        assert_eq!(p.quick, path(&p, 0), "stops at the first album");
        press(&mut p, egui::Key::Space, false);
        assert_eq!(p.quick, None);
        press(&mut p, egui::Key::Space, false);
        assert_eq!(p.quick, path(&p, 0), "with no tile under the pointer, the selected album");
        press(&mut p, egui::Key::Escape, false);
        assert_eq!((p.quick.clone(), p.selected.clone()), (None, path(&p, 0)), "Esc closes the quick look first, not the sidebar");
    }

    #[test]
    fn a_row_of_tiles_reaches_the_right_edge() {
        for ppp in [1.0f32, 1.25, 1.5, 2.0] {
            // from the narrowest grid with two proper tiles to a very wide window
            let mut w = 2.0 * TILE + GAP_MIN;
            while w < 2600.0 {
                let t = columns(w, ppp);
                let row = t.cols as f32 * t.side + (t.cols - 1) as f32 * t.gap;
                assert!((w - row).abs() < 4.0, "{w} at {ppp}: {t:?} is {row} wide");
                assert!(t.gap >= GAP_MIN - 1.0 / ppp && t.gap <= GAP_MAX + 0.01, "{w} at {ppp}: {t:?}");
                assert!(t.side >= TILE - 0.5 / ppp && t.side <= (TILE + (TILE + GAP_MAX) / t.cols as f32 - (GAP_MAX - GAP_MIN)).max(TILE) + 4.0, "{w} at {ppp}: {t:?}");
                // never fewer across than when the covers were a fixed size
                let before = (((w + GAP_MIN) / (TILE + GAP_MIN)).floor() as usize).max(1);
                assert!(t.cols >= before, "{w} at {ppp}: {t:?}");
                // drawn at a whole number of pixels: the size its picture is decoded at
                assert!((t.side * ppp - (t.side * ppp).round()).abs() < 0.01, "{w} at {ppp}: {t:?}");
                w += 0.61;
            }
        }
        // four or more across, the covers stay near their own size
        for w in [800.0f32, 1000.0, 1236.0, 1600.0, 1900.0, 2500.0] {
            let t = columns(w, 1.0);
            assert!(t.cols >= 4 && t.side <= 200.0, "{w}: {t:?}");
        }
        // the window as it opens, without and with the sidebar
        let t = columns(1236.0, 1.0);
        assert_eq!((t.cols, t.side), (6, 180.0));
        let t = columns(1236.0 - (TILE * 2.0 + SIDE_GAP + 2.0 * theme::PAGE_MARGIN as f32), 1.0);
        assert!(t.cols == 4 && t.side >= TILE && t.gap >= GAP_MIN && t.gap <= GAP_MAX, "{t:?}");
        // the smallest window with the sidebar open: two tiles a little closer, at their own size
        let t = columns(2.0 * TILE + 14.0, 1.0);
        assert_eq!((t.cols, t.side, t.gap), (2, TILE, 14.0));
        assert_eq!(columns(2.0 * TILE + 13.0, 1.0).cols, 1);
        assert_eq!(columns(10.0, 1.0), covers::Tiles { cols: 1, side: TILE, gap: 0.0 });
    }

    #[test]
    fn space_typed_into_a_text_box_does_not_open_the_quick_look() {
        let mut p = page(vec![album("A", "One", 1, false)]);
        p.selected = Some(p.albums[0].path.clone());
        p.hover = p.selected.clone();
        press(&mut p, egui::Key::Space, true);
        assert_eq!(p.quick, None);
    }
}
