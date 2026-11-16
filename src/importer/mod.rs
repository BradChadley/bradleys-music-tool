//! Soulseek Import: files finished Soulseek downloads into the music library.
//!   - converts FLAC/WAV to 320kbps MP3 (or, when Settings says to keep lossless downloads, leaves a FLAC
//!     as it is and makes any other lossless kind a FLAC)
//!   - fixes tags: one album artist, track/disc numbers, missing title/album/year/genre
//!   - grabs the original cover art (Apple Music, then Bandcamp, then MusicBrainz), saves it full size
//!     in the folder and embeds a 1400px copy in every track
//!   - names the folder "Album (Year)" under <library>\<Artist>\
//!   - sends the original download to the Recycle Bin
pub(crate) mod ctx;
mod disc_tags;
#[cfg(test)]
mod flow_tests;
pub(crate) mod id3;
mod images;
pub(crate) mod import;
pub(crate) mod lookup;
#[cfg(test)]
mod lossless_tests;
pub(crate) mod lyrics;
#[cfg(test)]
mod lyrics_tests;
mod model;
pub(crate) mod probe;
pub(crate) mod quality;
pub mod recheck;
pub(crate) mod rules;
pub(crate) mod scan;
pub(crate) mod tools;
pub(crate) mod tracklist;
mod unwrap;
pub(crate) mod util;

pub use import::is_folder_name;
pub use util::clean_name;
use crossbeam_channel::{unbounded, Receiver, Sender};
use ctx::Ctx;
use egui::{vec2, Align, Color32, CornerRadius, Frame, Label, Layout, Margin, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, Ui};
use import::ImportResult;
use model::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::theme::{self, BG, BORDER, CARD, CARD_HOVER, DIM, FAINT, GREEN, RED, TEXT, WARN};
/// progress and things you can click on this page are blue; red stays for problems
#[allow(non_snake_case)]
fn ACCENT() -> Color32 {
    theme::ACCENT2()
}
/// the height of the column headings, as in the tables in the top half of the page
const HEAD_H: f32 = 20.0;

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Scanning,
    Importing,
}

enum Msg {
    /// the downloads folder has been listed: every download that can be read gets its row at once
    Found { rows: Vec<(usize, PathBuf, scan::Quick)>, skipped: Vec<String> },
    /// what is being worked out for a download right now
    Stage { index: usize, text: &'static str },
    /// a download's facts are in (or it turned out not to be importable, and why)
    Scanned { index: usize, name: String, unit: Result<Box<Unit>, String>, done: usize, total: usize },
    /// every download's facts are in: which of two downloads of one album stays ticked can be settled
    FactsDone,
    /// a download's pictures are in
    Pictures { index: usize, cover: Option<lookup::Cover>, use_cover: bool, spectro: Option<PathBuf>, local_thumb: Option<PathBuf> },
    ScanDone,
    /// what LRCLIB has for one song of a download (`scan`: which scan asked)
    Lyric { scan: usize, index: usize, track: usize, answer: Option<lyrics::Answer> },
    /// every song of a download has been asked about
    LyricsDone { scan: usize, index: usize },
    RowStarted { row: usize, label: String },
    RowStatus { row: usize, text: String, frac: f32 },
    RowNames { row: usize, artist: String, folder: String },
    RowDone { row: usize, result: ImportResult, replaced: bool },
    ImportDone,
}

struct Row {
    index: usize,
    unit: Unit,
    include: bool,
    done: bool,
    artist: String,
    folder: String,
    use_online: bool,
    /// the scan's note about this album; cleared when the names are edited (Import re-checks anyway)
    note: Option<String>,
    status: String,
    progress: f32,
    /// how the last import of this album ended
    result: Option<ImportResult>,
    /// what is on disk where this album would land: (its folder is there, its artist folder is there).
    /// Looked up when the names change or a scan or import ends, never while drawing a row.
    place: Option<(bool, bool)>,
    /// the names `place` was looked up for
    place_for: (String, String),
    /// still being worked out: what is being done right now (None once its facts are in). Such a row has
    /// no tick and cannot be imported.
    pending: Option<&'static str>,
    /// its facts are in and its pictures are still being fetched
    pictures_pending: bool,
    /// it turned out not to be importable, and why: it stays in its place as a quiet line
    left_out: Option<String>,
    /// what listing its files said: how many audio files, and the size
    quick: (usize, u64),
    /// its facts arrived while the pointer was on the list: until the pointer leaves, it takes no more room
    /// than it had (see `row_ui`)
    held: bool,
}

/// A row is at least this tall, whether it is still being worked out or finished, so that a row filling in
/// does not change size: the two names, the line that says where it is from, and one more line.
const ROW_MIN_H: f32 = 74.0;

/// What the import control says. While downloads are still being checked, or their covers fetched, it says
/// what it waits for and cannot be clicked.
fn import_words(scanning: bool, checking: usize, covers: usize, ticked: usize, waiting: usize) -> String {
    let n = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    match (scanning, checking, covers) {
        (true, c, _) if c > 0 => format!("Import waits for {} still being checked", n(c, "download", "downloads")),
        (true, _, c) if c > 0 => format!("Import waits for {}", n(c, "cover", "covers")),
        _ => match ticked {
            0 if waiting == 0 => String::new(),
            0 => "Tick an album to import it".to_string(),
            1 => "Import 1 album".to_string(),
            n => format!("Import {n} albums"),
        },
    }
}

/// A download's facts have arrived, in whatever order the downloads were finished: its row is filled in
/// where it stands. No row is added, taken away or moved by this. `pointing`: the pointer is on the list.
fn facts_arrived(rows: &mut [Row], index: usize, unit: Result<Unit, String>, pointing: bool) {
    let Some(r) = rows.iter_mut().find(|r| r.index == index) else { return };
    match unit {
        // the same row, in the same place, now with its facts and its tick
        Ok(u) => r.fill(u, pointing),
        // not importable after all: it stays where it is and says why
        Err(why) => {
            r.pending = None;
            r.left_out = Some(why);
        }
    }
}

/// A pause between the stages of reading a download, for test runs only (`MUSIC_RECS_SLOW_SCAN` in
/// milliseconds, with `MUSIC_RECS_DATA` set), so a scripted run can take pictures of a list that is still
/// filling in. The real app never pauses.
fn test_pause() {
    let set = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if let (Some(_), Some(ms)) = (set("MUSIC_RECS_DATA"), set("MUSIC_RECS_SLOW_SCAN").and_then(|v| v.parse::<u64>().ok())) {
        std::thread::sleep(std::time::Duration::from_millis(ms.min(10_000)));
    }
}

impl Row {
    /// The row of a download whose files have only been listed: its name, how much is in it, and nothing
    /// that needs a file opened. It is not ticked and cannot be until its facts are in.
    fn placeholder(index: usize, item: &Path, q: &scan::Quick) -> Self {
        let unit = Unit {
            name: q.name.clone(),
            root: item.to_path_buf(),
            item: item.to_path_buf(),
            is_dir: item.is_dir(),
            files: vec![],
            tracks: vec![],
            tag_artist: None,
            artist: q.artist.clone(),
            folder: q.album.clone(),
            cover: None,
            use_cover: false,
            folder_img: None,
            fixes: vec![],
            convert: 0,
            kbps: 0,
            bytes: q.bytes,
            warnings: vec![],
            verdict: quality::verdict(&[]),
            spectro: None,
            local_thumb: None,
            action: Action::New,
            existing: None,
            home: None,
            note: String::new(),
            include: false,
            lyric_choice: Default::default(),
            lyrics_pending: false,
        };
        let mut row = Row::new(index, unit);
        row.pending = Some(scan::STAGE_FILES);
        row.quick = (q.audio_files, q.bytes);
        row
    }

    /// May this row be ticked, counted and imported? Not while it is being worked out, and not when it was left out.
    fn ready(&self) -> bool {
        self.pending.is_none() && self.left_out.is_none()
    }

    /// The row's facts have arrived: it is the same row, in the same place, now with everything but its pictures.
    fn fill(&mut self, unit: Unit, held: bool) {
        let (index, quick) = (self.index, self.quick);
        *self = Row::new(index, unit);
        self.quick = quick;
        self.pictures_pending = true;
        self.held = held;
    }

    fn new(index: usize, unit: Unit) -> Self {
        Row {
            index,
            include: unit.include,
            done: false,
            artist: unit.artist.clone(),
            folder: unit.folder.clone(),
            use_online: unit.use_cover,
            note: Some(unit.note.clone()).filter(|n| !n.is_empty()),
            status: "Ready".into(),
            progress: 0.0,
            result: None,
            place: None,
            place_for: (String::new(), String::new()),
            pending: None,
            pictures_pending: false,
            left_out: None,
            quick: (0, 0),
            held: false,
            unit,
        }
    }
    fn thumb_key(&self) -> String {
        if let (true, Some(c)) = (self.use_online, &self.unit.cover) {
            if !c.thumb.is_empty() {
                return c.thumb.clone();
            }
        }
        self.unit.local_thumb.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
    }
    fn dest(&self, lib: &Path) -> PathBuf {
        // (a shelf or a genre folder the scan chose, for as long as the artist is the one it chose it for)
        let home = self.unit.home.clone().filter(|_| self.artist == self.unit.artist).unwrap_or_else(|| lib.join(&self.artist));
        home.join(&self.folder)
    }
    /// Looks on disk for this album's folder and its artist folder, and remembers the answer.
    fn look(&mut self, lib: &Path) {
        self.place = Some((self.dest(lib).exists(), lib.join(&self.artist).exists()));
        self.place_for = (self.artist.clone(), self.folder.clone());
    }
    fn dest_note(&self) -> String {
        if self.done {
            return String::new();
        }
        if let Some(n) = &self.note {
            return n.clone();
        }
        // (not looked up yet: say nothing rather than guess)
        let (album_there, artist_there) = self.place.unwrap_or((false, true));
        if album_there {
            return "Already in your library - will be skipped".into();
        }
        if !artist_there {
            return "New artist folder".into();
        }
        String::new()
    }
    fn cover_note(&self) -> String {
        match (&self.unit.cover, self.use_online) {
            (Some(c), true) => c.label.clone(),
            (Some(_), false) => "Keeping the art that came with the download".into(),
            (None, _) => "No match on Apple Music, Bandcamp or MusicBrainz - keeping the art that came with the download".into(),
        }
    }
    fn quality_bad(&self) -> bool {
        self.unit.verdict.fake != quality::Fake::None || !self.unit.verdict.damaged.is_empty()
    }
}

#[derive(Default, Clone, Copy)]
struct Tally {
    imported: usize,
    replaced: usize,
    failed: usize,
    exists: usize,
    kept: usize,
    cleaned: usize,
}

/// What the finished screen shows.
struct Summary {
    tally: Tally,
    failures: Vec<(String, String)>,
    secs: f64,
}

struct Confirm {
    text: String,
}

pub struct ImportTab {
    ctx: Arc<Ctx>,
    images: images::Images,
    rows: Vec<Row>,
    selected: Option<usize>,
    phase: Phase,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    status: String,
    overall: f32,
    skipped: Vec<String>,
    confirm: Option<Confirm>,
    tally: Tally,
    /// failed albums of this run: (name, reason)
    failures: Vec<(String, String)>,
    /// rows being imported in this run (progress is their average)
    run_rows: Vec<usize>,
    run_start: Option<std::time::Instant>,
    summary: Option<Summary>,
    notice: Option<(String, f64)>,
    select_arg: Option<String>,
    started: bool,
    /// (phase, albums finished) when the rows last looked on disk for their folders; a change means look again
    place_stamp: (Phase, usize),
    /// where the list was drawn last, and whether the pointer was on it then (rows do not change size under it)
    list_rect: Option<egui::Rect>,
    pointing: bool,
    /// the arrow keys moved the marked album: bring it into view
    scroll_to_sel: bool,
    /// which scan the rows are from: lyrics that arrive for an earlier scan's rows are not these rows'
    scan_no: Arc<AtomicUsize>,
}

/// True when both `--source` and `--library` were given: a test run on copies, never the real folders by default.
/// Such a run does not take the one-importer lock and does not talk to Soulseek.
pub fn test_folders() -> bool {
    static TEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TEST.get_or_init(|| arg_after("--source").is_some() && arg_after("--library").is_some())
}

fn arg_after(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}
fn has_flag(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

impl ImportTab {
    /// Sets the importer up the first time its tab is opened. An error is a sentence for the tab to show
    /// (another importer is open, or ffmpeg is missing).
    pub fn new(egui_ctx: &egui::Context, cfg: &crate::config::Config) -> Result<Self, String> {
        // (a run on copies, with both folders given on the command line, may sit beside the real one)
        if !test_folders() && !single_instance() {
            return Err("The Soulseek importer is already open in another window. Close it, then open this page again.".into());
        }
        // the folders come from Settings (shared with every page); the flags are for testing on copies
        let (source, library) = Self::folders(cfg);
        let ffmpeg = tools::find_tool("ffmpeg")?;
        let ffprobe = tools::find_tool("ffprobe")?;
        let mut ctx = Ctx::new(source, library, has_flag("--no-online"), ffmpeg, ffprobe);
        ctx.genre_folders = cfg.skip_folders.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && crate::library::is_genre_folder(s)).collect();
        ctx.lyrics.timed = !cfg.lyrics_plain;
        ctx.keep_lossless = cfg.keep_lossless;
        let ctx = Arc::new(ctx);
        quality::load_cache(&ctx.quality_cache);
        unwrap::clean_old(&ctx.work_dir);
        let (tx, rx) = unbounded();
        Ok(Self {
            images: images::Images::new(egui_ctx.clone()),
            ctx,
            rows: vec![],
            selected: None,
            phase: Phase::Idle,
            tx,
            rx,
            status: String::new(),
            overall: 0.0,
            skipped: vec![],
            list_rect: None,
            pointing: false,
            scroll_to_sel: false,
            confirm: None,
            tally: Tally::default(),
            failures: vec![],
            run_rows: vec![],
            run_start: None,
            summary: None,
            notice: None,
            select_arg: arg_after("--select"),
            started: false,
            place_stamp: (Phase::Idle, 0),
            scan_no: Default::default(),
        })
    }

    pub fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// What the import list is doing, in a few words for the top row (None while it is doing nothing).
    pub fn job(&self) -> Option<String> {
        match self.phase {
            Phase::Idle => None,
            Phase::Scanning => Some("Reading downloads".to_string()),
            Phase::Importing => {
                let (done, all) = (self.rows.iter().filter(|r| r.done).count(), self.rows.iter().filter(|r| r.done || r.unit.include).count());
                Some(if all > 1 { format!("Importing {} of {all}", (done + 1).min(all)) } else { "Importing".to_string() })
            }
        }
    }

    /// Look at the downloads folder again (a download just finished). Does nothing in the middle of a scan or import.
    /// Where a download folder stands in the import list: None = not listed, Some(false) = waiting, Some(true) = imported.
    pub fn stage_of(&self, folder: &str) -> Option<bool> {
        self.rows.iter().filter(|r| r.left_out.is_none()).find(|r| r.unit.name.eq_ignore_ascii_case(folder)).map(|r| r.done)
    }

    /// How many albums are listed and not yet imported (for the page bar and the Home page).
    pub fn waiting(&self) -> usize {
        self.rows.iter().filter(|r| !r.done && r.left_out.is_none()).count()
    }

    /// Returns true if a scan was started (or will be, when this page is first shown).
    pub fn rescan(&mut self, egui_ctx: &egui::Context) -> bool {
        if self.busy() {
            return false;
        }
        if self.started {
            self.start_scan(egui_ctx);
        }
        true
    }

    /// This folder in the downloads folder is known to be complete (the app fetched it and every file arrived),
    /// so the "still downloading" wait does not apply to it.
    pub fn mark_settled(&mut self, folder: &str) {
        self.ctx.settled.write().unwrap_or_else(|e| e.into_inner()).insert(folder.to_lowercase());
    }

    /// (downloads folder, library folder) the importer should work on.
    fn folders(cfg: &crate::config::Config) -> (PathBuf, PathBuf) {
        let source = arg_after("--source").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(cfg.downloads_dir.trim()));
        let library = arg_after("--library").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(cfg.music_dir.trim()));
        (source, library)
    }

    /// True when Settings now point at different folders than this importer was set up with.
    pub fn folders_changed(&self, cfg: &crate::config::Config) -> bool {
        let (source, library) = Self::folders(cfg);
        // (the kind of lyrics to save, and whether lossless downloads stay lossless, are fixed when the list is
        // set up, so a change to either starts the list over too)
        source != self.ctx.source || library != self.ctx.library || cfg.lyrics_plain == self.ctx.lyrics.timed || cfg.keep_lossless != self.ctx.keep_lossless
    }

    fn selected_count(&self) -> usize {
        self.rows.iter().filter(|r| r.include && !r.done && r.ready()).count()
    }

    // ---------------------------------------------------------------- scan
    fn start_scan(&mut self, egui_ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        self.phase = Phase::Scanning;
        self.summary = None;
        self.rows.clear();
        self.selected = None;
        self.skipped.clear();
        self.ctx.refresh_existing_artists();
        let items = scan::get_source_items(&self.ctx);
        let total = items.len();
        if total > 0 {
            self.status = format!("Reading {total} downloads...");
            self.overall = 0.02;
        }
        let (ctx, tx, egui_ctx) = (self.ctx.clone(), self.tx.clone(), egui_ctx.clone());
        let scan_no = self.scan_no.fetch_add(1, Ordering::SeqCst) + 1;
        let scans = self.scan_no.clone();
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                egui_ctx.request_repaint();
            };
            let name_of = |item: &Path| item.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            // 1. the listing: no file is opened, so every download has its row at once, in folder order
            let (mut found, mut skipped) = (vec![], vec![]);
            for (i, item) in items.iter().enumerate() {
                match scan::quick_look(&ctx, item) {
                    Ok(q) => found.push((i, item.clone(), q)),
                    Err(why) => skipped.push(format!("{} ({why})", name_of(item))),
                }
            }
            let work: Vec<usize> = found.iter().map(|f| f.0).collect();
            let total = work.len();
            send(Msg::Found { rows: found, skipped });
            test_pause();
            // 2. the facts of each download (six at once): tags, the audio check, the lookups, the tick
            let next = AtomicUsize::new(0);
            let finished = AtomicUsize::new(0);
            let read: std::sync::Mutex<Vec<(usize, Unit, scan::Pictures)>> = Default::default();
            let (items, work, send) = (&items, &work, &send);
            std::thread::scope(|s| {
                for _ in 0..6.min(total.max(1)) {
                    s.spawn(|| loop {
                        let n = next.fetch_add(1, Ordering::SeqCst);
                        let Some(&i) = work.get(n) else { break };
                        let stage = |text: &'static str| {
                            send(Msg::Stage { index: i, text });
                            test_pause();
                        };
                        let unit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan::read_unit(&ctx, &items[i], &stage))).unwrap_or_else(|_| Err("lookup stopped unexpectedly".into()));
                        let done = finished.fetch_add(1, Ordering::SeqCst) + 1;
                        let name = name_of(&items[i]);
                        match unit {
                            Ok((u, pictures)) => {
                                send(Msg::Scanned { index: i, name, unit: Ok(Box::new(u.clone())), done, total });
                                read.lock().unwrap_or_else(|e| e.into_inner()).push((i, u, pictures));
                            }
                            Err(why) => send(Msg::Scanned { index: i, name, unit: Err(why), done, total }),
                        }
                    });
                }
            });
            send(Msg::FactsDone);
            // 3. the pictures (six at once): they come last and hold no fact back
            let read = read.into_inner().unwrap_or_else(|e| e.into_inner());
            let mut asks: Vec<(usize, Vec<(usize, String, String, f64)>)> = read.iter().map(|(i, u, _)| (*i, scan::lyric_asks(u))).collect();
            asks.sort_by_key(|a| a.0);
            let read = std::sync::Mutex::new(read);
            std::thread::scope(|s| {
                for _ in 0..6.min(total.max(1)) {
                    s.spawn(|| loop {
                        let Some((i, mut u, pictures)) = read.lock().unwrap_or_else(|e| e.into_inner()).pop() else { break };
                        test_pause();
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan::add_pictures(&ctx, &mut u, pictures)));
                        send(Msg::Pictures { index: i, cover: u.cover, use_cover: u.use_cover, spectro: u.spectro, local_thumb: u.local_thumb });
                    });
                }
            });
            quality::save_cache(&ctx.quality_cache); // rescans skip files already checked
            send(Msg::ScanDone);
            // 4. the lyrics: LRCLIB is asked about the songs that have none (or flagged ones), one at a time,
            // in the list's order. Nothing waits for this: an import that starts first asks for itself, and
            // every answer is kept, so no song is asked about twice. A newer scan ends this one's asking.
            for (i, songs) in asks {
                for (track, artist, title, secs) in songs {
                    if scans.load(Ordering::SeqCst) != scan_no {
                        return;
                    }
                    let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.lyrics.find(&artist, &title, secs))).unwrap_or(None);
                    send(Msg::Lyric { scan: scan_no, index: i, track, answer });
                }
                send(Msg::LyricsDone { scan: scan_no, index: i });
            }
        });
    }

    // ---------------------------------------------------------------- import
    fn start_import(&mut self, egui_ctx: &egui::Context, force: bool) {
        let todo: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].include && !self.rows[i].done && self.rows[i].ready()).collect();
        if todo.is_empty() {
            self.status = "Tick at least one album to import.".into();
            return;
        }
        let need: u64 = todo.iter().map(|&i| import::output_bytes(&self.rows[i].unit, self.ctx.keep_lossless)).sum();
        let free = import::free_bytes(&self.ctx.library);
        if !force && free < need + 500 * 1024 * 1024 {
            self.confirm = Some(Confirm {
                text: format!(
                    "There isn't enough space on {} for all of these.\n\nFree: {:.1} GB\nNeeded: about {:.1} GB\n\nImport as many as fit anyway?",
                    self.ctx.library.ancestors().last().map(|p| p.display().to_string()).unwrap_or_default(),
                    free as f64 / 1073741824.0,
                    need as f64 / 1073741824.0
                ),
            });
            return;
        }
        self.phase = Phase::Importing;
        self.summary = None;
        self.tally = Tally::default();
        self.failures.clear();
        self.run_rows = todo.clone();
        self.run_start = Some(std::time::Instant::now());
        for &i in &todo {
            let r = &mut self.rows[i];
            r.progress = 0.0;
            r.result = None;
            r.status = "Waiting".into();
        }
        let jobs: Vec<import::ImportJob> = todo.iter().map(|&i| import::ImportJob { id: i, unit: self.rows[i].unit.clone(), artist: self.rows[i].artist.clone(), folder: self.rows[i].folder.clone(), use_cover: self.rows[i].use_online }).collect();
        let (ctx, tx, egui_ctx) = (self.ctx.clone(), self.tx.clone(), egui_ctx.clone());
        std::thread::spawn(move || {
            let on = |e: import::Event| {
                let m = match e {
                    import::Event::Names { id, artist, folder } => Msg::RowNames { row: id, artist, folder },
                    import::Event::Started { id, label } => Msg::RowStarted { row: id, label },
                    import::Event::Status { id, text, frac } => Msg::RowStatus { row: id, text, frac: frac as f32 },
                    import::Event::Done { id, result, replaced } => Msg::RowDone { row: id, result, replaced },
                };
                let _ = tx.send(m);
                egui_ctx.request_repaint();
            };
            import::import_many(&ctx, jobs, &on);
            crate::links::job_ended("import");
            let _ = tx.send(Msg::ImportDone);
            egui_ctx.request_repaint();
        });
    }

    // ---------------------------------------------------------------- messages from the workers
    fn poll(&mut self, egui_ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Found { rows, skipped } => {
                    // every download is listed at once, in folder order: that is each row's place for good
                    self.skipped = skipped;
                    self.rows = rows.iter().map(|(index, item, q)| Row::placeholder(*index, item, q)).collect();
                    self.selected = (!self.rows.is_empty()).then_some(0);
                    let n = self.rows.len();
                    self.status = format!("Found {n} download{}. Reading {}...", if n != 1 { "s" } else { "" }, if n == 1 { "it" } else { "them" });
                }
                Msg::Stage { index, text } => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.index == index && r.pending.is_some()) {
                        r.pending = Some(text);
                    }
                }
                Msg::Scanned { index, name, unit, done, total } => {
                    self.status = format!("Read {done} of {total}  (latest: {name})");
                    self.overall = done as f32 / total.max(1) as f32;
                    facts_arrived(&mut self.rows, index, unit.map(|u| *u), self.pointing);
                    // (its songs without lyrics are asked about once the covers are in)
                    if let Some(r) = self.rows.iter_mut().find(|r| r.index == index && r.ready()) {
                        r.unit.lyrics_pending = self.ctx.lyrics.online && !scan::lyric_asks(&r.unit).is_empty();
                    }
                }
                Msg::Lyric { scan, index, track, answer } => {
                    if scan == self.scan_no.load(Ordering::SeqCst) {
                        if let Some(t) = self.rows.iter_mut().find(|r| r.index == index).and_then(|r| r.unit.tracks.get_mut(track)) {
                            t.lyric_found = answer;
                        }
                    }
                }
                Msg::LyricsDone { scan, index } => {
                    if scan == self.scan_no.load(Ordering::SeqCst) {
                        if let Some(r) = self.rows.iter_mut().find(|r| r.index == index) {
                            r.unit.lyrics_pending = false;
                        }
                    }
                }
                Msg::FactsDone => {
                    // two downloads of the same album: keep only the better one ticked (this needs every row's facts)
                    let at: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].ready()).collect();
                    let mut units: Vec<Unit> = at.iter().map(|&i| self.rows[i].unit.clone()).collect();
                    scan::resolve_batch_duplicates(&mut units, self.ctx.keep_lossless);
                    for (&i, u) in at.iter().zip(units) {
                        self.rows[i].include = u.include;
                        self.rows[i].unit = u;
                    }
                    self.status = "Finding the covers...".into();
                }
                Msg::Pictures { index, cover, use_cover, spectro, local_thumb } => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.index == index) {
                        r.unit.use_cover = use_cover;
                        r.use_online = use_cover;
                        r.unit.cover = cover;
                        r.unit.spectro = spectro;
                        r.unit.local_thumb = local_thumb;
                        r.pictures_pending = false;
                    }
                }
                Msg::ScanDone => {
                    // (nothing is left being worked out: a row that still is, was lost on the way)
                    for r in self.rows.iter_mut() {
                        if r.pending.take().is_some() {
                            r.left_out = Some("it could not be read".into());
                        }
                        r.pictures_pending = false;
                    }
                    let n = self.rows.iter().filter(|r| r.ready()).count();
                    let mut msg = format!("{n} album{} ready. Check the names and covers, then import.", if n != 1 { "s" } else { "" });
                    if !self.skipped.is_empty() {
                        msg.push_str(&format!("  Skipped: {}", self.skipped.join(", ")));
                    }
                    self.status = msg;
                    self.overall = 0.0;
                    self.phase = Phase::Idle;
                    if let Some(sel) = self.select_arg.take() {
                        let sel = sel.to_lowercase();
                        if let Some(i) = self.rows.iter().position(|r| r.unit.name.to_lowercase().contains(&sel)) {
                            self.selected = Some(i);
                        }
                    }
                }
                Msg::RowStarted { row, label } => {
                    // the label and selection follow the album that just started (not every message, or they'd flicker)
                    self.status = label;
                    self.selected = Some(row);
                    if let Some(r) = self.rows.get_mut(row) {
                        r.status = "Starting...".into();
                    }
                    self.update_overall();
                }
                Msg::RowStatus { row, text, frac } => {
                    if let Some(r) = self.rows.get_mut(row) {
                        r.status = text;
                        r.progress = frac.clamp(0.0, 1.0);
                    }
                    self.update_overall();
                }
                Msg::RowNames { row, artist, folder } => {
                    if let Some(r) = self.rows.get_mut(row) {
                        r.artist = artist;
                        r.folder = folder;
                    }
                }
                Msg::RowDone { row, result, replaced } => {
                    if let Some(r) = self.rows.get_mut(row) {
                        match result {
                            ImportResult::Ok | ImportResult::OkKept => {
                                self.tally.imported += 1;
                                if replaced {
                                    self.tally.replaced += 1;
                                }
                                if result == ImportResult::OkKept {
                                    self.tally.kept += 1;
                                }
                                r.done = true;
                            }
                            ImportResult::Cleaned => {
                                self.tally.cleaned += 1;
                                r.done = true;
                            }
                            ImportResult::Exists => {
                                self.tally.exists += 1;
                                r.done = true;
                            }
                            ImportResult::Failed => {
                                self.tally.failed += 1;
                                self.failures.push((r.unit.name.clone(), r.status.clone()));
                            }
                        }
                        r.result = Some(result);
                        r.include = false;
                        r.progress = 1.0;
                    }
                    self.update_overall();
                }
                Msg::ImportDone => {
                    let t = self.tally;
                    let mut msg = format!("Imported {} album{}.", t.imported, if t.imported != 1 { "s" } else { "" });
                    if t.kept > 0 {
                        msg.push_str(&format!(" {} original{} still open in Soulseek and left in place - Rescan later to remove.", t.kept, if t.kept != 1 { "s were" } else { " was" }));
                    }
                    if t.cleaned > 0 {
                        msg.push_str(&format!(" Removed {} leftover download{}.", t.cleaned, if t.cleaned != 1 { "s" } else { "" }));
                    }
                    if t.exists > 0 {
                        msg.push_str(&format!(" {} already in library.", t.exists));
                    }
                    if t.failed > 0 {
                        msg.push_str(&format!(" {} failed - those downloads were left in place.", t.failed));
                    }
                    self.status = msg;
                    self.overall = 1.0;
                    self.phase = Phase::Idle;
                    self.ctx.refresh_existing_artists();
                    let secs = self.run_start.take().map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                    self.summary = Some(Summary { tally: t, failures: std::mem::take(&mut self.failures), secs });
                    if !egui_ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
                        egui_ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
                    }
                }
            }
        }
    }

    /// Overall progress = the average of the albums in this run.
    fn update_overall(&mut self) {
        if self.run_rows.is_empty() {
            return;
        }
        let sum: f32 = self.run_rows.iter().filter_map(|&i| self.rows.get(i)).map(|r| r.progress).sum();
        self.overall = (sum / self.run_rows.len() as f32).clamp(0.0, 1.0);
    }

    // ---------------------------------------------------------------- drawing
    /// Each row remembers whether its album folder and its artist folder are already in the library, so drawing
    /// never touches the disk. A row looks again when its names change, and all of them do whenever a scan or an
    /// import moves on.
    fn refresh_places(&mut self) {
        let stamp = (self.phase, self.rows.iter().filter(|r| r.result.is_some()).count());
        let all = stamp != self.place_stamp;
        self.place_stamp = stamp;
        for r in &mut self.rows {
            if all || r.place.is_none() || r.place_for.0 != r.artist || r.place_for.1 != r.folder {
                r.look(&self.ctx.library);
            }
        }
    }

    /// The top of the list's card: "Ready to import", large, with how many beside it (or what is
    /// happening) and the two folders under it, each a word that opens it. At the right the page's one
    /// action ("Import 3 albums"), which turns into how far the import is while it runs, and "Rescan".
    /// The thin line under it all fills with how far a scan or an import is, so nothing moves when one
    /// starts or ends.
    fn header(&mut self, ui: &mut Ui, egui_ctx: &egui::Context) {
        let (waiting, ticked) = (self.waiting(), self.selected_count());
        let (checking, covers) = (self.rows.iter().filter(|r| r.pending.is_some()).count(), self.rows.iter().filter(|r| r.pictures_pending).count());
        let label = import_words(self.phase == Phase::Scanning, checking, covers, ticked, waiting);
        let inner = ui.available_width();
        // (in a narrow card the two folders give way)
        let folders = inner >= 560.0;
        let rect = egui::Rect::from_min_size(ui.cursor().min, vec2(inner, if folders { 56.0 } else { 36.0 }));
        ui.allocate_rect(rect, Sense::hover());
        let first = egui::Rect::from_min_size(rect.min, vec2(inner, 36.0));
        let (mut go, mut scan) = (false, false);
        // the right end first, so the words on the left take only the room that is left
        let mut right = ui.new_child(egui::UiBuilder::new().id_salt("imp-head-right").max_rect(first).layout(Layout::right_to_left(Align::Center)));
        match self.phase {
            Phase::Importing => {
                // how far it is stands where the action was
                let done = self.run_rows.iter().filter(|&&i| self.rows.get(i).is_some_and(|r| r.result.is_some())).count();
                right.spacing_mut().item_spacing.x = 10.0;
                if let Some(t) = self.run_start {
                    right.label(RichText::new(time_words(t.elapsed().as_secs())).color(FAINT()));
                }
                right.label(RichText::new(format!("{done} of {} done", self.run_rows.len())).size(15.0).strong().color(TEXT()));
            }
            _ if !label.is_empty() => {
                let r = theme::main_action_sized(&mut right, &label, 18.0, !self.busy() && ticked > 0);
                go = r.on_hover_text("Ctrl+Enter").clicked();
            }
            _ => {}
        }
        right.add_space(18.0);
        scan |= theme::quiet(&mut right, "Rescan", 14.0, !self.busy()).clicked();
        let room = right.min_rect().left() - 16.0;
        let title = ui.painter().layout_no_wrap("Ready to import".to_string(), theme::display(26.0), TEXT());
        let beside = rect.left() + title.size().x + 14.0;
        ui.painter().galley(egui::pos2(rect.left(), (first.center().y - title.size().y / 2.0).round()), title, TEXT());
        // how many, or what is happening, in the room left beside the title
        let at = egui::Rect::from_min_max(egui::pos2(beside, first.top() + 6.0), egui::pos2(room.max(beside), first.bottom()));
        let mut words = ui.new_child(egui::UiBuilder::new().id_salt("imp-head-words").max_rect(at).layout(Layout::left_to_right(Align::Center)));
        words.spacing_mut().item_spacing.x = 8.0;
        let line = match self.phase {
            Phase::Scanning | Phase::Importing => self.status.clone(),
            Phase::Idle if waiting == 0 => String::new(),
            Phase::Idle => format!("{waiting} ready  \u{b7}  {ticked} ticked"),
        };
        if self.phase == Phase::Scanning {
            words.add(egui::Spinner::new().size(13.0).color(DIM()));
        }
        // downloads the scan could not take (still arriving, no audio): how many, and which under the pointer
        if !self.skipped.is_empty() && self.phase == Phase::Idle {
            let skipped = format!("{} skipped", self.skipped.len());
            words.label(RichText::new(skipped).color(WARN())).on_hover_text(self.skipped.join("\n"));
        }
        words.add(Label::new(RichText::new(line).color(DIM())).truncate());
        if folders {
            let second = egui::Rect::from_min_max(egui::pos2(rect.left(), first.bottom()), rect.max);
            let mut from = ui.new_child(egui::UiBuilder::new().id_salt("imp-head-folders").max_rect(second).layout(Layout::left_to_right(Align::Center)));
            from.spacing_mut().item_spacing.x = 6.0;
            from.label(RichText::new("From").size(12.5).color(FAINT()));
            if folder_word(&mut from, &self.ctx.source) {
                let _ = open::that(&self.ctx.source);
            }
            from.label(RichText::new("into").size(12.5).color(FAINT()));
            if folder_word(&mut from, &self.ctx.library) {
                let _ = open::that(&self.ctx.library);
            }
        }
        ui.add_space(10.0);
        let y = ui.cursor().top();
        ui.painter().hline(rect.x_range(), y, Stroke::new(1.0, BORDER()));
        if self.busy() && self.overall > 0.0 {
            ui.painter().hline(rect.left()..=rect.left() + inner * self.overall.clamp(0.0, 1.0), y, Stroke::new(2.0, ACCENT()));
        }
        ui.add_space(10.0);
        if scan {
            self.start_scan(egui_ctx);
        }
        if go {
            self.start_import(egui_ctx, false);
        }
    }

    /// What the last import did, under the header: plain numbers behind a thin green (or amber) line.
    fn summary_line(&mut self, ui: &mut Ui) {
        let Some(sum) = &self.summary else { return };
        let t = sum.tally;
        let bad = t.failed > 0;
        let accent = if bad { WARN() } else { GREEN() };
        let took = time_words(sum.secs.round() as u64);
        let mut action: Option<&str> = None;
        {
            let body = Frame::new().inner_margin(Margin { left: 16, right: 0, top: 2, bottom: 2 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    // one line: how many came in and how long it took, then anything else that happened
                    let n = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
                    let mut line = egui::text::LayoutJob::default();
                    let mut say = |text: &str, color: Color32, size: f32| line.append(text, 0.0, egui::TextFormat { font_id: egui::FontId::proportional(size), color, ..Default::default() });
                    say(&format!("{} imported", n(t.imported, "album", "albums")), if t.imported > 0 { TEXT() } else { DIM() }, 16.0);
                    say(&format!("  in {took}"), DIM(), 14.0);
                    let others = [
                        (t.replaced, "replaced an older copy", GREEN()),
                        (t.exists, "already in the library", DIM()),
                        (t.failed, "failed", RED()),
                        (t.kept, "still open in Soulseek", WARN()),
                        (t.cleaned, "leftovers removed", DIM()),
                    ];
                    for (count, words, color) in others {
                        if count > 0 {
                            say("    \u{b7}    ", FAINT(), 14.0);
                            say(&format!("{count} {words}"), color, 14.0);
                        }
                    }
                    ui.add(Label::new(line).truncate());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 20.0;
                        if theme::quiet(ui, "Dismiss", 14.0, true).clicked() {
                            action = Some("dismiss");
                        }
                        if theme::quiet(ui, "Open library folder", 14.0, true).clicked() {
                            action = Some("open");
                        }
                    });
                });
                if !sum.failures.is_empty() {
                    ui.add_space(12.0);
                    theme::caption(ui, "LEFT IN YOUR DOWNLOADS FOLDER");
                    ui.add_space(4.0);
                    let name_w = (ui.available_width() * 0.34).clamp(160.0, 420.0);
                    ScrollArea::vertical().max_height(96.0).id_salt("failures").auto_shrink([false, true]).show(ui, |ui| {
                        for (name, why) in &sum.failures {
                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                ui.allocate_ui_with_layout(vec2(name_w, 18.0), Layout::left_to_right(Align::Min), |ui| {
                                    ui.set_width(name_w);
                                    ui.add(Label::new(RichText::new(name).size(12.5).color(TEXT())).truncate());
                                });
                                ui.add_space(14.0);
                                ui.add(Label::new(RichText::new(why).size(12.5).color(RED())).wrap());
                            });
                        }
                    });
                }
            });
            let r = body.response.rect;
            ui.painter().vline(r.left() + 1.0, r.y_range(), Stroke::new(2.0, accent));
            ui.add_space(12.0);
        }
        match action {
            Some("dismiss") => self.summary = None,
            Some("open") => {
                let _ = open::that(&self.ctx.library);
            }
            _ => {}
        }
    }

    /// The keys on the list: Up and Down move the marked album (the one the panel on the right is about),
    /// Space ticks or unticks it, Ctrl+Enter imports the ticked albums. Not while a name is being typed.
    fn keys(&mut self, ui: &mut Ui, egui_ctx: &egui::Context) {
        if self.rows.is_empty() || theme::typing(ui.ctx()) || ui.ctx().any_popup_open() || self.confirm.is_some() {
            return;
        }
        let (down, up, space, go) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Space),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter),
            )
        });
        let last = self.rows.len() - 1;
        if down || up {
            self.selected = Some(match (self.selected, down) {
                (None, _) => 0,
                (Some(i), true) => (i + 1).min(last),
                (Some(i), false) => i.saturating_sub(1),
            });
            self.scroll_to_sel = true;
        }
        if space && !self.busy() {
            if let Some(r) = self.selected.and_then(|i| self.rows.get_mut(i)).filter(|r| r.ready() && !r.done) {
                r.include = !r.include;
            }
        }
        if go && !self.busy() && self.selected_count() > 0 {
            self.start_import(egui_ctx, false);
        }
    }

    /// The page with no albums on it: what is being read, or, large and quiet, that nothing is waiting,
    /// with the folder that is watched as a word that opens it.
    fn nothing_yet(&mut self, ui: &mut Ui, egui_ctx: &egui::Context, room: f32) {
        ui.add_space((room * 0.2).clamp(30.0, 130.0));
        let mut scan = false;
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            if self.phase == Phase::Scanning {
                ui.add(egui::Spinner::new().size(18.0).color(DIM()));
                ui.add_space(2.0);
                // (a scan that found no folders leaves the old sentence in place; do not show that one)
                let reading = self.status.starts_with("Reading") || self.status.starts_with("Looked up");
                ui.label(RichText::new(if reading { self.status.as_str() } else { "Reading the downloads folder" }).size(14.0).color(DIM()));
                ui.add_space(4.0);
                theme::progress(ui, self.overall, 220.0, ACCENT());
                return;
            }
            ui.label(RichText::new("Nothing to import yet").font(theme::display(28.0)).color(DIM()));
            ui.horizontal(|ui| {
                theme::centered_row(ui, "imp-watching", 0.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new("Watching").size(13.5).color(FAINT()));
                    let r = theme::quiet(ui, &self.ctx.source.display().to_string(), 13.5, true);
                    if r.on_hover_text("Open the folder").clicked() {
                        let _ = open::that(&self.ctx.source);
                    }
                    ui.add_space(12.0);
                    scan = theme::quiet(ui, "Rescan", 13.5, true).clicked();
                });
            });
            if !self.skipped.is_empty() {
                ui.add_space(14.0);
                theme::caption(ui, "SKIPPED");
                for s in &self.skipped {
                    ui.add(Label::new(RichText::new(s).size(12.5).color(WARN())).wrap());
                }
            }
        });
        if scan {
            self.start_scan(egui_ctx);
        }
    }

    fn list(&mut self, ui: &mut Ui) {
        let busy = self.busy();
        let total = ui.available_width() - 12.0; // (the scroll bar's room)
        let widths = col_widths(total - ROW_INSET);
        // column headings, like the results table in the top half of the page
        let (rect, _) = ui.allocate_exact_size(vec2(total, HEAD_H), Sense::hover());
        let mut head = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
        head.spacing_mut().item_spacing.x = 0.0;
        for (i, (w, t)) in widths.iter().zip(["", "ALBUM", "COVER", "TRACKS AND SOUND", ""]).enumerate() {
            // (rows are drawn a little in from the edge, so every heading but the first moves with them)
            let w = if i == 0 { *w + ROW_INSET } else { *w };
            head.allocate_ui_with_layout(vec2(w, HEAD_H), Layout::left_to_right(Align::Center), |ui| {
                ui.set_width(w);
                if i == 0 {
                    // over the ticks, a tick for all of them: full, part (a dash) or none
                    ui.add_space(ROW_INSET);
                    // "all" is every album the scan itself ticked: one it left unticked on purpose (the worse
                    // of two copies of an album, one already in the library) stays unticked by "Tick all"
                    let open = |r: &&Row| r.ready() && !r.done;
                    let wanted: Vec<bool> = self.rows.iter().filter(open).filter(|r| r.unit.include).map(|r| r.include).collect();
                    let any = self.rows.iter().filter(open).any(|r| r.include);
                    let all_on = if wanted.is_empty() { self.rows.iter().filter(open).count() > 0 && self.rows.iter().filter(open).all(|r| r.include) } else { wanted.iter().all(|t| *t) };
                    let state = if all_on { Some(true) } else if any { None } else { Some(false) };
                    let some = any && !all_on;
                    let enabled = !busy && self.rows.iter().any(|r| open(&r));
                    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), if enabled { Sense::click() } else { Sense::hover() });
                    crate::script::seen("tick:all", r);
                    let inner = r.shrink(1.0);
                    if state == Some(true) || some {
                        ui.painter().rect_filled(inner, 4, if enabled { theme::ACCENT() } else { FAINT() });
                        let c = inner.center();
                        let pen = Stroke::new(2.0, BG());
                        if some {
                            ui.painter().line_segment([c + vec2(-4.0, 0.0), c + vec2(4.0, 0.0)], pen);
                        } else {
                            ui.painter().line_segment([c + vec2(-4.5, 0.5), c + vec2(-1.5, 3.5)], pen);
                            ui.painter().line_segment([c + vec2(-1.5, 3.5), c + vec2(4.5, -3.5)], pen);
                        }
                    } else {
                        ui.painter().rect_stroke(inner, 4, Stroke::new(1.5, if resp.hovered() { DIM() } else { FAINT() }), StrokeKind::Inside);
                    }
                    let tip = if all_on { "Untick all" } else { "Tick all (not the ones the scan left unticked)" };
                    if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        let only_wanted = !wanted.is_empty();
                        for row in self.rows.iter_mut().filter(|r| r.ready() && !r.done) {
                            if all_on {
                                row.include = false;
                            } else if !only_wanted || row.unit.include {
                                row.include = true;
                            }
                        }
                    }
                    return;
                }
                theme::caption(ui, t);
            });
        }
        ui.add_space(4.0);
        // rows do not change size under the pointer: a row whose facts arrive while he points at the list
        // takes no more room than it had until the pointer leaves (see `row_ui`)
        self.pointing = self.list_rect.is_some_and(|r| ui.rect_contains_pointer(r));
        if !self.pointing {
            self.rows.iter_mut().for_each(|r| r.held = false);
        }
        let head_rect = rect;
        let (timed, keep) = (self.ctx.lyrics.timed, self.ctx.keep_lossless);
        let ImportTab { rows, images, selected, list_rect, scroll_to_sel, .. } = self;
        let list = ScrollArea::vertical().id_salt("imp-rows").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in 0..rows.len() {
                // the row's background is only known once the row has been laid out, so its place is kept first
                let bg = ui.painter().add(egui::Shape::Noop);
                let fr = Frame::new().inner_margin(Margin { left: ROW_INSET as i8, right: 0, top: 10, bottom: 10 }).show(ui, |ui| {
                    ui.set_width(total - ROW_INSET);
                    row_ui(ui, &mut rows[i], images, &widths, busy, timed, keep);
                });
                let rect = fr.response.rect;
                let is_sel = *selected == Some(i);
                if is_sel && *scroll_to_sel {
                    ui.scroll_to_rect(rect, None);
                    *scroll_to_sel = false;
                }
                let hovered = ui.rect_contains_pointer(rect);
                if is_sel || hovered {
                    // (the list stands on a card: the marked row is the lighter one, the row under the pointer between the two)
                    ui.painter().set(bg, egui::Shape::rect_filled(rect, 6, if is_sel { CARD_HOVER() } else { theme::blend(CARD(), CARD_HOVER(), 0.5) }));
                }
                if is_sel {
                    // the album the panel on the right is about
                    ui.painter().vline(rect.left() + 1.0, egui::Rangef::new(rect.top() + 8.0, rect.bottom() - 8.0), Stroke::new(2.0, theme::ACCENT()));
                }
                if i + 1 < rows.len() {
                    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, theme::blend(CARD(), BORDER(), 0.55)));
                }
                if hovered && ui.input(|inp| inp.pointer.primary_clicked()) {
                    *selected = Some(i);
                }
            }
        });
        *list_rect = Some(list.inner_rect.union(head_rect));
    }

    /// The side card: the marked album's cover, how good it sounds, where it lands and what its tags
    /// become. `across`: the card lies under the list (a narrow window), so the picture stands at its
    /// left and the words run beside it in two columns.
    fn details(&mut self, ui: &mut Ui, across: bool) {
        let lib = self.ctx.library.clone();
        // (what happens to flagged lyrics can be chosen until the album is imported)
        let (timed, idle, keep) = (self.ctx.lyrics.timed, self.phase == Phase::Idle, self.ctx.keep_lossless);
        let ImportTab { rows, images, selected, .. } = self;
        let quiet_words = |ui: &mut Ui, text: &str| {
            ui.add_space(if across { 40.0 } else { 80.0 });
            ui.vertical_centered(|ui| ui.add(Label::new(RichText::new(text).size(14.0).color(DIM())).wrap()));
        };
        let Some(row) = selected.and_then(|i| rows.get_mut(i)) else {
            quiet_words(ui, "Click an album to see its cover, quality and tag changes.");
            return;
        };
        // a download that is still being worked out, or was left out, has nothing to show here yet
        if let Some(why) = &row.left_out {
            quiet_words(ui, &format!("{} was left out: {why}.", row.unit.name));
            return;
        }
        if row.pending.is_some() {
            quiet_words(ui, "This download is still being read. Its cover, quality and tag changes show here when that is done.");
            return;
        }
        if across {
            let tall = ui.available_height();
            let side = tall.min(180.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 20.0;
                details_cover(ui, row, images, side, side);
                let rest = ui.available_width();
                let first = (rest * 0.45).clamp(220.0, 380.0);
                for (n, w) in [first, rest - first - 20.0].into_iter().enumerate() {
                    ui.allocate_ui_with_layout(vec2(w, tall), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(w);
                        ScrollArea::vertical().id_salt(("imp-details", n)).auto_shrink([false, false]).show(ui, |ui| {
                            if n == 0 {
                                details_sound(ui, row, images, keep);
                            } else {
                                details_changes(ui, row, &lib, true, timed, idle);
                            }
                        });
                    });
                }
            });
            return;
        }
        ScrollArea::vertical().id_salt("imp-details").auto_shrink([false, false]).show(ui, |ui| {
            let w = ui.available_width();
            // (a square for the picture; an album with no picture at all gets a short strip instead)
            details_cover(ui, row, images, w, if row.thumb_key().is_empty() { 84.0 } else { w });
            ui.add_space(12.0);
            details_sound(ui, row, images, keep);
            details_changes(ui, row, &lib, false, timed, idle);
            ui.add_space(16.0);
        });
    }
}

/// The side card's picture: the cover the album gets, in a box `w` by `h`.
fn details_cover(ui: &mut Ui, row: &Row, images: &mut images::Images, w: f32, h: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    if let Some(t) = images.get(&row.thumb_key()) {
        let sz = t.size_vec2();
        let scale = (rect.width() / sz.x).min(rect.height() / sz.y);
        let r = egui::Rect::from_center_size(rect.center(), sz * scale);
        egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), sz * scale)).corner_radius(6).paint_at(ui, r);
    } else {
        ui.painter().rect_filled(rect, 6, CARD_HOVER());
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "No cover art", egui::FontId::proportional(13.0), FAINT());
    }
}

/// The side card's first words: whose album it is, which cover it gets, and how it sounds (with the
/// picture of one track's sound).
fn details_sound(ui: &mut Ui, row: &Row, images: &mut images::Images, keep: bool) {
    let w = ui.available_width();
    ui.add(Label::new(RichText::new(&row.artist).size(17.0).strong().color(TEXT())).wrap());
    ui.add(Label::new(RichText::new(&row.folder).size(14.0).color(DIM())).wrap());
    ui.add_space(4.0);
    ui.add(Label::new(RichText::new(row.cover_note()).size(11.5).color(FAINT())).wrap());

    side_caption(ui, "QUALITY", false);
    ui.add(Label::new(RichText::new(&row.unit.verdict.summary).size(12.5).color(TEXT())).wrap());
    // what the import makes of a lossless album: in blue when it becomes something else, quietly when it stays
    for (words, change) in lossless_words(&row.unit, keep, true) {
        ui.add_space(2.0);
        ui.add(Label::new(RichText::new(words).size(12.5).color(if change { ACCENT() } else { FAINT() })).wrap());
    }
    if let Some(sp) = &row.unit.spectro {
        if let Some(t) = images.get(&sp.to_string_lossy()) {
            ui.add_space(8.0);
            let h = (w * t.size_vec2().y / t.size_vec2().x).min(112.0 * 2.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
            let sz = t.size_vec2();
            let scale = (rect.width() / sz.x).min(rect.height() / sz.y);
            egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), sz * scale)).corner_radius(4).paint_at(ui, egui::Rect::from_center_size(rect.center(), sz * scale));
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Spectrogram of one track (click to enlarge). Real lossless usually reaches the top; an MP3 shows a flat ceiling around 16-20kHz.");
            if resp.hovered() {
                ui.painter().rect_stroke(rect, 4, Stroke::new(1.0, ACCENT()), StrokeKind::Inside);
            }
            if resp.clicked() {
                let _ = open::that(sp);
            }
        }
    }
}

/// What the import makes of an album's lossless tracks, in a few words: (the words, whether they say a
/// change). Nothing for an album with no lossless track. `keep`: lossless downloads are kept (the choice in
/// Settings). `side`: the words for the side card; the list keeps the words it has always had for MP3.
fn lossless_words(u: &Unit, keep: bool, side: bool) -> Vec<(String, bool)> {
    if u.convert == 0 {
        return vec![];
    }
    let n = u.tracks.len();
    let count = |f: scan::Fate| u.tracks.iter().filter(|t| scan::fate(t, keep) == f).count();
    let (mp3, stays, flac) = (count(scan::Fate::Mp3), count(scan::Fate::StaysFlac), count(scan::Fate::Flac));
    let mut out = vec![];
    if mp3 > 0 {
        let words = match (side, mp3 == n) {
            (false, true) => "All converted to MP3".to_string(),
            (false, false) => format!("{mp3} converted to MP3"),
            (true, true) => "Becomes MP3 320".to_string(),
            (true, false) => format!("{mp3} of {n} {} MP3 320", if mp3 == 1 { "becomes" } else { "become" }),
        };
        out.push((words, true));
    }
    if stays > 0 {
        out.push((if stays == n { "Stays FLAC".to_string() } else { format!("{stays} {} FLAC", if stays == 1 { "stays" } else { "stay" }) }, false));
    }
    if flac > 0 {
        out.push((if flac == n { "Becomes FLAC".to_string() } else { format!("{flac} {} FLAC", if flac == 1 { "becomes" } else { "become" }) }, true));
    }
    out
}

/// A small heading in the side card. `first`: nothing stands over it, so it needs no room above.
fn side_caption(ui: &mut Ui, text: &str, first: bool) {
    ui.add_space(if first { 0.0 } else { 14.0 });
    theme::caption(ui, text);
    ui.add_space(4.0);
}

/// The side card's last words: where the album is saved, and what its tags become. `top`: they start a
/// column of their own.
fn details_changes(ui: &mut Ui, row: &mut Row, lib: &Path, top: bool, timed: bool, idle: bool) {
    side_caption(ui, "SAVES TO", top);
    // the library folder by its own name, then the part this album adds, standing out (the whole
    // path under the pointer)
    let (full, root) = (row.dest(lib).display().to_string(), lib.display().to_string());
    let (head, tail) = match full.strip_prefix(&root) {
        Some(tail) => (lib.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| root.clone()), tail.to_string()),
        None => (String::new(), full.clone()),
    };
    let mut path = egui::text::LayoutJob::default();
    for (text, color) in [(head.as_str(), FAINT()), (tail.as_str(), TEXT())] {
        path.append(text, 0.0, egui::TextFormat { font_id: egui::FontId::proportional(12.5), color, ..Default::default() });
    }
    ui.add(Label::new(path).wrap()).on_hover_text(&full);
    let dn = row.dest_note();
    if !dn.is_empty() {
        ui.add_space(2.0);
        ui.add(Label::new(RichText::new(dn).size(11.5).color(WARN())).wrap());
    }
    for warn in &row.unit.warnings {
        ui.add_space(2.0);
        ui.add(Label::new(RichText::new(warn).size(11.5).color(RED())).wrap());
    }

    details_lyrics(ui, row, timed, idle);

    side_caption(ui, "TAG CHANGES", false);
    // one block per file: its name, then each field on its own line as "field   old → new"
    let font = egui::FontId::proportional(12.0);
    let mut any = false;
    for t in row.unit.tracks.iter().filter(|t| !t.new.is_empty()) {
        if any {
            ui.add_space(7.0);
        }
        any = true;
        ui.add(Label::new(RichText::new(&t.base).size(12.5).color(TEXT())).truncate());
        for (k, v) in &t.new {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.allocate_ui_with_layout(vec2(FIELD_W, 16.0), Layout::left_to_right(Align::Min), |ui| {
                    ui.set_width(FIELD_W);
                    ui.add(Label::new(RichText::new(k.replace('_', " ")).size(12.0).color(DIM())).truncate());
                });
                let mut job = egui::text::LayoutJob::default();
                let mut say = |text: &str, color: Color32| job.append(text, 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
                say(if t.old(k).is_empty() { "(empty)" } else { t.old(k) }, FAINT());
                say("  →  ", FAINT());
                say(v, TEXT());
                ui.add(Label::new(job).wrap());
            });
        }
    }
    if !any {
        ui.label(RichText::new("No tag changes needed").size(12.5).color(DIM()));
    }
}

// ------------------------------------------------------------------ lyrics, in a row and in the side card

/// How one track's lyrics stand.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dot {
    /// the file has them
    Has,
    /// the import adds them from LRCLIB
    Adds,
    /// they carry lines that are not lyrics
    Flagged,
    /// none, and none to be had
    Hollow,
    /// LRCLIB is still being asked
    Asking,
}

/// A track's dot, and the words that go with it (in the side card, and under the pointer).
fn lyric_state(t: &Track, pending: bool, choice: lyrics::Choice, timed: bool) -> (Dot, String) {
    use lyrics::{Act, Answer, Place};
    let l = &t.lyric;
    let writes = scan::takes_lyrics(t);
    if l.has_junk() {
        let n = l.junk.lines.len();
        let what = match l.junk.lines[0].1 {
            _ if n > 1 => format!("{n} lines are not lyrics"),
            "web address" if l.junk.only => "only a web address".to_string(),
            "placeholder" => "a placeholder, no lyrics".to_string(),
            "scrambled characters" => "scrambled characters".to_string(),
            "markup" => "web page markup in it".to_string(),
            _ => "1 line is not lyrics".to_string(),
        };
        if !writes {
            return (Dot::Flagged, format!("{what} (copied as it is)"));
        }
        // while LRCLIB is still being asked, what will happen is not known yet
        let then = match lyrics::act(l, t.lyric_found.as_ref(), choice, timed) {
            _ if pending && t.lyric_found.is_none() && choice == lyrics::Choice::Replace => "",
            Act::Nothing => "",
            Act::Clear => ": taken out",
            Act::Set { .. } if choice == lyrics::Choice::Replace && t.lyric_found.as_ref().is_some_and(|a| a.text(timed).is_some()) => ": replaced from LRCLIB",
            Act::Set { .. } => ": those lines cut",
        };
        return (Dot::Flagged, format!("{what}{then}"));
    }
    if l.junk.instrumental {
        return (Dot::Has, "instrumental".into());
    }
    if l.there() {
        let words = match (l.place, l.timed) {
            (Place::Hidden, _) if writes => "in the file, where players cannot see them: moved into the lyrics field",
            (_, true) => "in the file, timed",
            _ => "in the file",
        };
        return (Dot::Has, words.into());
    }
    if !writes {
        return (Dot::Hollow, "none (this kind of file is copied as it is)".into());
    }
    match &t.lyric_found {
        Some(a @ Answer::Found { .. }) => (Dot::Adds, if a.is_timed(timed) { "from LRCLIB, timed" } else { "from LRCLIB" }.into()),
        Some(Answer::Instrumental) => (Dot::Hollow, "instrumental".into()),
        Some(Answer::Nothing) => (Dot::Hollow, "none, and LRCLIB has none".into()),
        None if pending => (Dot::Asking, "asking LRCLIB".into()),
        None => (Dot::Hollow, "none".into()),
    }
}

fn dot_color(d: Dot) -> Color32 {
    match d {
        Dot::Has => DIM(),
        Dot::Adds | Dot::Asking => theme::ACCENT2(),
        Dot::Flagged => WARN(),
        Dot::Hollow => FAINT(),
    }
}

/// The few words beside an album's dots: what needs a look, else what is still being done, else nothing.
fn lyric_words(states: &[(Dot, String)]) -> Option<(String, Color32)> {
    let flagged: Vec<&String> = states.iter().filter(|s| s.0 == Dot::Flagged).map(|s| &s.1).collect();
    let verb = |one: &str, many: &str| if flagged.len() == 1 { format!("1 {one}") } else { format!("{} {many}", flagged.len()) };
    if !flagged.is_empty() {
        let all = |start: &str| flagged.iter().all(|w| w.starts_with(start));
        return Some((
            if all("only a web address") {
                verb("is only a web address", "are only a web address")
            } else if all("a placeholder") {
                verb("is a placeholder", "are placeholders")
            } else {
                verb("has junk lines", "have junk lines")
            },
            WARN(),
        ));
    }
    if states.iter().any(|s| s.0 == Dot::Asking) {
        return Some(("asking LRCLIB".into(), FAINT()));
    }
    let quiet = states.iter().filter(|s| s.1 == "instrumental").count();
    (quiet > 0).then(|| (format!("{quiet} instrumental"), FAINT()))
}

/// An album's lyrics in its row: a dot for every track (filled: the file has them; blue: the import adds
/// them; amber: they need a look; a ring: none), and words only for what needs saying.
fn lyrics_line(ui: &mut Ui, u: &Unit, timed: bool) {
    if u.tracks.is_empty() {
        return;
    }
    let states: Vec<(Dot, String)> = u.tracks.iter().map(|t| lyric_state(t, u.lyrics_pending, u.lyric_choice, timed)).collect();
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
        // (words go to the next line whole, never broken in two)
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.label(RichText::new("Lyrics").size(11.5).color(DIM()));
        ui.add_space(6.0);
        for (t, (dot, words)) in u.tracks.iter().zip(&states) {
            let (rect, resp) = ui.allocate_exact_size(vec2(11.0, 15.0), Sense::hover());
            let (c, color) = (rect.center() + vec2(0.0, 1.0), dot_color(*dot));
            match dot {
                Dot::Hollow | Dot::Asking => drop(ui.painter().circle_stroke(c, 3.0, Stroke::new(1.2, if *dot == Dot::Asking { color.gamma_multiply(0.7) } else { color }))),
                _ => drop(ui.painter().circle_filled(c, 3.5, color)),
            }
            resp.on_hover_text(format!("{}: {words}", t.base));
        }
        if let Some((text, color)) = lyric_words(&states) {
            ui.add_space(6.0);
            ui.label(RichText::new(&text).size(11.5).color(color));
            if text == "asking LRCLIB" {
                ui.add_space(5.0);
                ui.add(egui::Spinner::new().size(9.0).color(theme::ACCENT2()));
            }
        }
    });
}

/// The side card's lyrics: every track and how its lyrics stand; under a flagged one the lines that are not
/// lyrics, as they are in the file; and, when any is flagged, what the import does with them.
fn details_lyrics(ui: &mut Ui, row: &mut Row, timed: bool, idle: bool) {
    if row.unit.tracks.is_empty() {
        return;
    }
    side_caption(ui, "LYRICS", false);
    let u = &row.unit;
    let states: Vec<(Dot, String)> = u.tracks.iter().map(|t| lyric_state(t, u.lyrics_pending, u.lyric_choice, timed)).collect();
    let w = ui.available_width();
    let can = idle && !row.done;
    let (mut pick, mut chosen) = (u.lyric_choice, false);
    for (t, (dot, words)) in u.tracks.iter().zip(&states) {
        // the track at the left, how its lyrics stand at the right; a long name gives way to the words
        let color = match dot {
            Dot::Has | Dot::Hollow => DIM(),
            d => dot_color(*d),
        };
        let said = ui.painter().layout_no_wrap(words.clone(), egui::FontId::proportional(12.0), color);
        let narrow = said.size().x > w * 0.62;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let room = if narrow { w } else { (w - said.size().x - 8.0).max(60.0) };
            ui.allocate_ui_with_layout(vec2(room, 18.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_width(room);
                ui.add(Label::new(RichText::new(&t.base).size(12.5).color(TEXT())).truncate());
            });
            if !narrow {
                ui.label(RichText::new(words).size(12.0).color(color));
            }
        });
        if narrow {
            ui.add(Label::new(RichText::new(words).size(12.0).color(color)).wrap());
        }
        if *dot == Dot::Flagged {
            // the lines themselves, set off by an amber rule at their left
            let lines = &t.lyric.junk.lines;
            let block = ui
                .horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for (line, _) in lines.iter().take(4) {
                            ui.add(Label::new(RichText::new(line).size(12.0).color(DIM())).wrap());
                        }
                        if lines.len() > 4 {
                            ui.label(RichText::new(format!("and {} more", lines.len() - 4)).size(11.5).color(FAINT()));
                        }
                    });
                })
                .response
                .rect;
            ui.painter().vline(block.left() + 1.0, block.y_range(), Stroke::new(2.0, WARN()));
            // one choice for all of the album's flagged lyrics, as words under the first of them: the
            // chosen one is underlined
            if !chosen && scan::takes_lyrics(t) {
                chosen = true;
                ui.add_space(5.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(10.0, 4.0);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    for (choice, words, hint) in [
                        (lyrics::Choice::Replace, "Replace from LRCLIB", "LRCLIB's lyrics take their place. Where it has none, only these lines are cut."),
                        (lyrics::Choice::Cut, "Cut these lines", "The rest of the lyrics stay as they are. Lyrics that are nothing but these lines are taken out."),
                        (lyrics::Choice::Keep, "Leave them", "The lyrics are imported exactly as they came."),
                    ] {
                        let r = if can { theme::word(ui, words, 12.0, pick == choice) } else { ui.label(RichText::new(words).size(12.0).color(if pick == choice { TEXT() } else { FAINT() })) };
                        if r.on_hover_text(hint).clicked() && can {
                            pick = choice;
                        }
                    }
                });
                ui.add_space(3.0);
            }
            ui.add_space(4.0);
        }
    }
    row.unit.lyric_choice = pick;
}

/// how far a row's content sits in from the row's edge (as in the results table above)
const ROW_INSET: f32 = 6.0;
/// the little cover in a row
const THUMB: f32 = 52.0;
/// the field names in the details panel's tag changes
const FIELD_W: f32 = 92.0;
/// space kept clear at the right of every column
const COL_GAP: f32 = 14.0;

/// "48s" or "2m 05s".
fn time_words(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// A folder as a quiet word that opens it: the whole path when it is short, otherwise its last parts.
fn folder_word(ui: &mut Ui, path: &Path) -> bool {
    theme::quiet(ui, &short_path(path), 12.5, true).on_hover_ui(|ui| drop(ui.label(format!("Open {}", path.display())))).clicked()
}

fn short_path(path: &Path) -> String {
    let full = path.display().to_string();
    if full.chars().count() <= 34 {
        return full;
    }
    let parts: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    match parts.as_slice() {
        [.., a, b] if a.chars().count() + b.chars().count() <= 32 => format!("...\\{a}\\{b}"),
        [.., b] => format!("...\\{b}"),
        [] => full,
    }
}

/// tick | album (cover and names) | cover | tracks and sound | what happened to it
fn col_widths(total: f32) -> [f32; 5] {
    let fixed = [50.0, 0.0, 190.0, 220.0, 150.0];
    let rest = (total - fixed.iter().sum::<f32>()).max(260.0);
    [fixed[0], rest, fixed[2], fixed[3], fixed[4]]
}

/// One column of a row: `w` wide, its content stacked from the top and kept clear of the next column.
fn col(ui: &mut Ui, w: f32, f: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(w - COL_GAP, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(w - COL_GAP);
        f(ui);
    });
    ui.add_space(COL_GAP);
}

/// The tick in front of a row: coral like the main action, because ticked albums are what that action imports.
/// An album that has been dealt with shows a plain check instead and can no longer be ticked.
fn tick(ui: &mut Ui, row: &mut Row) {
    // a download that is still being worked out has no tick yet (the checks that could untick it are not
    // done), and one that was left out never gets one
    if !row.ready() {
        ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        return;
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), if row.done { Sense::hover() } else { Sense::click() });
    let c = rect.center();
    let check = |color: Color32| {
        let pen = Stroke::new(2.0, color);
        ui.painter().line_segment([c + vec2(-4.5, 0.5), c + vec2(-1.5, 3.5)], pen);
        ui.painter().line_segment([c + vec2(-1.5, 3.5), c + vec2(4.5, -3.5)], pen);
    };
    if row.done {
        check(if matches!(row.result, Some(ImportResult::Exists)) { FAINT() } else { GREEN() });
        return;
    }
    if row.include {
        ui.painter().rect_filled(rect.shrink(1.0), 4, theme::ACCENT());
        check(BG());
    } else {
        ui.painter().rect_stroke(rect.shrink(1.0), 4, Stroke::new(1.5, if resp.hovered() { DIM() } else { FAINT() }), StrokeKind::Inside);
    }
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        row.include = !row.include;
    }
}

/// An artist or album name: plain text until the pointer is on it or it is being typed in, then a quiet box.
/// Returns true when the text was just changed.
fn name_edit(ui: &mut Ui, text: &mut String, size: f32, color: Color32, editable: bool) -> bool {
    let bg = ui.painter().add(egui::Shape::Noop);
    let edit = TextEdit::singleline(text)
        .font(egui::FontId::proportional(size))
        .text_color(color)
        .frame(Frame::new().inner_margin(Margin::symmetric(6, 2)))
        .desired_width(ui.available_width())
        .interactive(editable);
    let r = ui.add(edit);
    if editable && (r.hovered() || r.has_focus()) {
        let line = if r.has_focus() { ACCENT() } else { FAINT() };
        ui.painter().set(bg, egui::epaint::RectShape::new(r.rect, 4, BG(), Stroke::new(1.0, line), StrokeKind::Inside));
    }
    r.changed()
}

/// One album, read left to right: import it or not, what it is, which cover, what happens to the tracks,
/// how good it is, where it stands.
fn row_ui(ui: &mut Ui, row: &mut Row, images: &mut images::Images, widths: &[f32; 5], busy: bool, timed: bool, keep: bool) {
    let editable = !row.done && !busy;
    // albums that will be imported are bright; the rest step back
    let name_color = if row.left_out.is_some() { FAINT() } else if row.include { TEXT() } else { DIM() };
    // every row is at least as tall as one that is still being worked out, so filling in changes no size
    ui.set_min_height(ROW_MIN_H);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
        col(ui, widths[0], |ui| {
            ui.add_space((THUMB - 18.0) / 2.0);
            tick(ui, row);
        });
        // cover and names
        col(ui, widths[1], |ui| {
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(THUMB, THUMB), Sense::hover());
                if let Some(t) = images.get(&row.thumb_key()) {
                    let sz = t.size_vec2();
                    let scale = (rect.width() / sz.x).max(rect.height() / sz.y);
                    let uv_w = rect.width() / (sz.x * scale);
                    let uv_h = rect.height() / (sz.y * scale);
                    let uv = egui::Rect::from_min_max(egui::pos2(0.5 - uv_w / 2.0, 0.5 - uv_h / 2.0), egui::pos2(0.5 + uv_w / 2.0, 0.5 + uv_h / 2.0));
                    let tint = if row.include { Color32::WHITE } else { Color32::from_gray(150) };
                    egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size())).uv(uv).tint(tint).corner_radius(4).paint_at(ui, rect);
                } else {
                    ui.painter().rect_filled(rect, CornerRadius::same(4), CARD());
                    ui.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, BORDER()), StrokeKind::Inside);
                }
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if name_edit(ui, &mut row.artist, 15.0, name_color, editable) {
                        row.note = None;
                    }
                    ui.add_space(1.0);
                    if name_edit(ui, &mut row.folder, 13.5, name_color, editable) {
                        row.note = None;
                    }
                    ui.add_space(1.0);
                    // (the lines under the names start where the names' letters start)
                    let under = |ui: &mut Ui, text: &str, color: Color32, cut: bool| {
                        ui.horizontal_top(|ui| {
                            ui.add_space(6.0);
                            let label = Label::new(RichText::new(text).size(11.5).color(color));
                            ui.add(if cut { label.truncate() } else { label.wrap() });
                        });
                    };
                    under(ui, &format!("from {}", row.unit.name), FAINT(), true);
                    if let Some(why) = &row.left_out {
                        under(ui, &format!("Left out: {why}"), WARN(), true);
                    } else if row.pending.is_some() {
                        // what listing its files said, until the files themselves have been read
                        under(ui, &format!("{} audio file{}, {}", row.quick.0, if row.quick.0 == 1 { "" } else { "s" }, crate::downloads::size_words(row.quick.1)), FAINT(), true);
                    } else {
                        // what is to be said about it: where it lands, and what is wrong with it
                        let dn = row.dest_note();
                        let mut lines: Vec<(String, Color32)> = vec![];
                        if !dn.is_empty() {
                            lines.push((dn, WARN()));
                        }
                        lines.extend(row.unit.warnings.iter().map(|w| (w.clone(), RED())));
                        if row.held {
                            // its facts arrived while the pointer was on the list: one line, cut short, so the
                            // row is no taller than it was; all of it shows when the pointer leaves the list
                            // (what is wrong with it comes before where it lands: that is why it is not ticked)
                            if let Some((first, color)) = lines.iter().find(|l| l.1 == RED()).or(lines.first()) {
                                let more = if lines.len() > 1 { format!("  (and {} more)", lines.len() - 1) } else { String::new() };
                                under(ui, &format!("{first}{more}"), *color, true);
                            }
                        } else {
                            for (text, color) in &lines {
                                under(ui, text, *color, false);
                            }
                        }
                    }
                });
            });
        });
        // cover: the chosen word is the picture the album gets
        col(ui, widths[2], |ui| {
            ui.add_space(4.0);
            let can = !row.done && !busy;
            if !row.ready() {
                return; // nothing is known about its cover yet
            }
            if row.pictures_pending {
                // the facts are in; the picture comes last
                ui.add(Label::new(RichText::new(scan::STAGE_COVER).size(12.5).color(FAINT())).truncate());
                return;
            }
            // one line: the cover it gets (a choice of two when one was found online)
            match &row.unit.cover {
                Some(c) => {
                    let online = c.source.replacen("Apple", "Apple Music", 1);
                    if can {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            if theme::word(ui, &online, 13.5, row.use_online).clicked() {
                                row.use_online = true;
                            }
                            if theme::word(ui, "Download's", 13.5, !row.use_online).on_hover_text("The cover that came with the download").clicked() {
                                row.use_online = false;
                            }
                        });
                    } else {
                        ui.label(RichText::new(if row.use_online { online.as_str() } else { "Download's cover" }).size(13.5).color(name_color));
                    }
                }
                // nothing found online: the download's own picture, or a warning when it has none either
                None if row.thumb_key().is_empty() => {
                    ui.label(RichText::new("No cover").size(13.5).color(WARN()));
                }
                None => {
                    ui.label(RichText::new("Download's cover").size(13.5).color(name_color)).on_hover_text("No cover was found on Apple Music, Bandcamp or MusicBrainz");
                }
            }
        });
        // tracks and how they sound on one line; under it only what will change (converting, tag fixes)
        col(ui, widths[3], |ui| {
            ui.add_space(4.0);
            if !row.ready() {
                return; // the tracks have not been read yet (what the listing said is under the name)
            }
            let n = row.unit.tracks.len();
            let mut line = egui::text::LayoutJob::default();
            let font = egui::FontId::proportional(13.5);
            let mut say = |text: &str, color: Color32| line.append(text, 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
            say(&format!("{n} track{}", if n != 1 { "s" } else { "" }), name_color);
            say("  \u{b7}  ", FAINT());
            say(&row.unit.verdict.short, if row.quality_bad() { RED() } else { GREEN() });
            ui.add(Label::new(line).wrap());
            // (a FLAC that stays as it is is said quietly: nothing happens to it)
            for (words, change) in lossless_words(&row.unit, keep, false) {
                ui.label(RichText::new(words).size(11.5).color(if change { DIM() } else { FAINT() }));
            }
            if !row.unit.fixes.is_empty() {
                let fields: Vec<String> = row.unit.fixes.iter().map(|(k, _)| k.replace('_', " ")).collect();
                let changes: usize = row.unit.fixes.iter().map(|(_, n)| *n).sum();
                let words = format!("{changes} tag fix{}: {}", if changes == 1 { "" } else { "es" }, fields.join(", "));
                ui.add(Label::new(RichText::new(words).size(11.5).color(theme::ACCENT2())).truncate()).on_hover_text(row.unit.format_fixes().replace('_', " "));
            }
            lyrics_line(ui, &row.unit, timed);
        });
        // what happened to it: nothing is said until something does
        col(ui, widths[4], |ui| {
            ui.add_space(4.0);
            if row.left_out.is_some() {
                ui.label(RichText::new("Left out").size(13.5).color(FAINT()));
                return;
            }
            if let Some(stage) = row.pending {
                // one plain word for what is being worked out
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add(egui::Spinner::new().size(12.0).color(theme::ACCENT2()));
                    ui.label(RichText::new(stage).size(13.5).color(DIM()));
                });
                return;
            }
            if row.result.is_none() && row.status == "Ready" {
                return; // nothing has happened to it yet (an unticked album is told by its dimmed names)
            }
            let (text, color, bar) = match row.result {
                Some(ImportResult::Failed) => (format!("Failed: {}", row.status), RED(), RED()),
                Some(ImportResult::OkKept) => (row.status.clone(), WARN(), GREEN()),
                Some(ImportResult::Ok) | Some(ImportResult::Cleaned) => (row.status.clone(), GREEN(), GREEN()),
                Some(ImportResult::Exists) => (row.status.clone(), DIM(), FAINT()),
                None => (row.status.clone(), TEXT(), ACCENT()),
            };
            ui.add(Label::new(RichText::new(text).size(12.5).color(color)).wrap());
            ui.add_space(4.0);
            theme::progress(ui, row.progress, ui.available_width(), bar);
        });
    });
}

impl ImportTab {
    /// Runs every frame, whichever tab is showing, so a scan or import keeps moving in the background.
    pub fn tick(&mut self, ectx: &egui::Context) {
        self.poll(ectx);
        self.images.poll(ectx);
        if self.busy() && ectx.input(|i| i.viewport().close_requested()) {
            ectx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.notice = Some(("Please wait for the current step to finish.".into(), ectx.input(|i| i.time)));
        }
        if self.busy() {
            ectx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    /// The Downloads page is on show: reads the downloads folder the first time it is. Called whether the
    /// import list or a search has the main card, so that the word that leads to the list says how many
    /// albums are ready before the list has ever been looked at.
    pub fn wake(&mut self, egui_ctx: &egui::Context) {
        if !self.started {
            self.started = true;
            self.start_scan(egui_ctx);
        }
    }

    /// Draws the import list in the page's centered stretch (`left`, `wide`), in the room left under the
    /// search box and the cards above it: the list in a card that reaches the bottom of the window, and
    /// the marked album's details in a card beside it (under it in a narrow window). With no albums there
    /// are no cards, only what the page is waiting for. `floor` is where the window ends: a window too
    /// short for the cards gets them at a least height, and the page scrolls.
    pub fn ui(&mut self, ui: &mut Ui, left: f32, wide: f32, floor: f32) {
        /// the room between cards, the side card's width, and the least width the two fit side by side in
        const GAP: f32 = 16.0;
        const SIDE: f32 = 320.0;
        const SIDE_BY_SIDE: f32 = 1250.0;
        /// how tall the side card is when it lies under the list
        const ACROSS: f32 = 224.0;
        let ectx = ui.ctx().clone();
        self.wake(&ectx);
        self.refresh_places();
        self.keys(ui, &ectx);
        ui.add_space(GAP);
        let pad = vec2(20.0, 18.0);
        if self.rows.is_empty() {
            if self.summary.is_some() {
                theme::panel(ui, left, wide, pad, "imp-summary", |ui| self.summary_line(ui));
            }
            let mut column = ui.available_rect_before_wrap();
            (column.min.x, column.max.x) = (left, left + wide);
            let room = floor - ui.cursor().top();
            ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| self.nothing_yet(ui, &ectx, room));
        } else {
            let top = ui.cursor().top();
            let across = wide < SIDE_BY_SIDE;
            let floor = (floor - GAP).max(top + if across { 300.0 + GAP + ACROSS } else { 420.0 });
            let whole = egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(left + wide, floor));
            let (mut list, mut side) = (whole, whole);
            if across {
                list.max.y = whole.bottom() - ACROSS - GAP;
                side.min.y = whole.bottom() - ACROSS;
            } else {
                list.max.x = whole.right() - SIDE - GAP;
                side.min.x = whole.right() - SIDE;
            }
            theme::panel_in(ui, list, pad, "imp-list", |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                self.header(ui, &ectx);
                self.summary_line(ui);
                ui.spacing_mut().item_spacing.y = 3.0;
                self.list(ui);
            });
            theme::panel_in(ui, side, vec2(20.0, 20.0), "imp-side", |ui| self.details(ui, across));
        }

        // "not enough space" question
        if let Some(c) = &self.confirm {
            let mut answer: Option<bool> = None;
            egui::Window::new("Soulseek Import").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0)).show(&ectx, |ui| {
                ui.label(&c.text);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Yes").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("No").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if let Some(yes) = answer {
                self.confirm = None;
                if yes {
                    self.start_import(&ectx, true);
                }
            }
        }
        if let Some((t, at)) = &self.notice {
            if ectx.input(|i| i.time) - at < 3.0 {
                egui::Area::new(egui::Id::unique("notice")).anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -70.0)).show(&ectx, |ui| {
                    Frame::new().fill(CARD_HOVER()).stroke(egui::Stroke::new(1.0, BORDER())).corner_radius(8).inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
                        ui.label(RichText::new(t).color(TEXT()));
                    });
                });
                ectx.request_repaint_after(std::time::Duration::from_millis(250));
            } else {
                self.notice = None;
            }
        }
    }

    pub fn on_exit(&mut self) {
        // clean up the little preview pictures
        if let Ok(rd) = std::fs::read_dir(&self.ctx.work_dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("thumb-") || n.starts_with("spec-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Only one importer at a time, so two runs never grab the same downloads.
fn single_instance() -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    // once this app holds the lock it keeps it, so setting the importer up again (after a folder change) still works
    static HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if HELD.load(Ordering::SeqCst) {
        return true;
    }
    // a test run (its own data folder) has a lock of its own, so it can be tried with the real app open
    let lock = match std::env::var_os("MUSIC_RECS_DATA").filter(|v| !v.is_empty()) {
        Some(data) => format!("Local\\music-recs-import-test-{:x}", crate::util::fnv64(&data.to_string_lossy().to_lowercase())),
        None => "Local\\music-recs-import".to_string(),
    };
    let name: Vec<u16> = std::ffi::OsStr::new(&lock).encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // someone else has it: let go, so "Try again" works once they close
            CloseHandle(h);
            return false;
        }
        // (ours: the handle is kept open for the life of the process)
        HELD.store(true, Ordering::SeqCst);
        true
    }
}

fn message_box(text: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
    let w = |s: &str| -> Vec<u16> { std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect() };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), w(text).as_ptr(), w("Soulseek Import").as_ptr(), 0);
    }
}

/// `--dump report.txt [--import] [--keep-originals]`: no window - read every download, write what the importer would do,
/// and optionally import (for testing on copies).
fn headless(ctx: &Ctx, out: &str, do_import: bool) {
    let items = scan::get_source_items(ctx);
    let results: std::sync::Mutex<Vec<(usize, String, Result<Unit, String>)>> = std::sync::Mutex::new(vec![]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..6.min(items.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= items.len() {
                    break;
                }
                let name = items[i].file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let u = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan::build_unit(ctx, &items[i]))).unwrap_or_else(|e| {
                    Err(format!("crashed: {}", e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()))
                });
                results.lock().unwrap().push((i, name, u));
            });
        }
    });
    quality::save_cache(&ctx.quality_cache);
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.0);
    let mut units: Vec<Unit> = results.iter().filter_map(|r| r.2.as_ref().ok().cloned()).collect();
    scan::resolve_batch_duplicates(&mut units, ctx.keep_lossless);
    let mut text = String::new();
    for (_, name, r) in &results {
        if let Err(e) = r {
            text.push_str(&crate::util::plain_err(format!("SKIPPED {name}: {e}\n")));
        }
    }
    // each album's plan text; the import result is added after the run
    let mut blocks: Vec<String> = vec![];
    for u in units.iter() {
        let mut t = format!("\n### {}  ->  {}\\{}   [{}]\n", u.name, u.artist, u.folder, if u.include { "ticked" } else { "UNTICKED" });
        t.push_str(&format!("    quality: {} | {}\n    fixes:   {}\n", u.verdict.short, u.verdict.summary, u.format_fixes()));
        for w in &u.warnings {
            t.push_str(&format!("    ! {w}\n"));
        }
        for tr in &u.tracks {
            let num = tr.new.iter().find(|e| e.0 == "track").map(|e| e.1.clone()).unwrap_or_else(|| tr.old("track").to_string());
            t.push_str(&format!("      {} -> track {}\n", tr.base, num));
        }
        blocks.push(t);
    }
    let mut import_secs = 0.0;
    let mut trace: Vec<String> = vec![];
    if do_import {
        // test hooks (copies only): --break-covers makes every cover URL fail; --vanish <text> deletes the first track of a
        // matching album just before importing, as if the file disappeared half way
        if has_flag("--break-covers") {
            for u in units.iter_mut() {
                if let Some(c) = u.cover.as_mut() {
                    c.urls = vec!["http://127.0.0.1:9/none.jpg".to_string()];
                }
            }
        }
        if let (Some(pat), true) = (arg_after("--vanish"), import::KEEP_ORIGINALS.load(Ordering::Relaxed)) {
            for u in units.iter().filter(|u| u.name.to_lowercase().contains(&pat.to_lowercase())) {
                if let Some(t) = u.tracks.last() {
                    let _ = std::fs::remove_file(&t.path);
                }
            }
        }
        // the same path the window uses; every album, ticked or not, like the old test mode
        let jobs: Vec<import::ImportJob> = units.drain(..).enumerate().map(|(i, u)| import::ImportJob { id: i, artist: u.artist.clone(), folder: u.folder.clone(), use_cover: u.use_cover, unit: u }).collect();
        let t0 = std::time::Instant::now();
        let names: Vec<String> = jobs.iter().map(|j| j.unit.name.clone()).collect();
        let log = std::sync::Mutex::new(Vec::<String>::new());
        let res = import::import_many(ctx, jobs, &|e| match e {
            import::Event::Started { id, .. } => log.lock().unwrap().push(format!("{:7.2}s  start  {}", t0.elapsed().as_secs_f64(), names[id])),
            import::Event::Done { id, result, .. } => log.lock().unwrap().push(format!("{:7.2}s  done   {}  {result:?}", t0.elapsed().as_secs_f64(), names[id])),
            _ => {}
        });
        trace = log.into_inner().unwrap();
        import_secs = t0.elapsed().as_secs_f64();
        for (id, r) in res {
            blocks[id].push_str(&format!("    IMPORT: {r:?}\n"));
        }
    }
    for b in blocks {
        text.push_str(&b);
    }
    if do_import {
        text.push_str(&format!("\nIMPORT TIME: {import_secs:.1}s\n"));
        for l in trace {
            text.push_str(&format!("TRACE {l}\n"));
        }
    }
    let _ = std::fs::write(out, text);
}
/// `--dump plan.txt --source <dir> --library <dir> [--import] [--keep-originals]`: the importer's no-window test mode.
/// Returns true if the flag was there (the app then exits without opening a window).
pub fn run_dump() -> bool {
    let Some(out) = arg_after("--dump") else { return false };
    // test mode on a folder of copies: needs an explicit --source, so it can never grab the real downloads by accident
    let (Some(source), Some(library)) = (arg_after("--source"), arg_after("--library")) else {
        message_box("--dump needs --source and --library");
        return true;
    };
    let (Ok(ffmpeg), Ok(ffprobe)) = (tools::find_tool("ffmpeg"), tools::find_tool("ffprobe")) else { return true };
    import::KEEP_ORIGINALS.store(has_flag("--keep-originals"), Ordering::Relaxed);
    let ctx = Ctx::new(PathBuf::from(source), PathBuf::from(library), has_flag("--no-online"), ffmpeg, ffprobe);
    quality::load_cache(&ctx.quality_cache);
    headless(&ctx, &out, has_flag("--import"));
    true
}
