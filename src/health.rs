//! The Health page: what the last library check found.
//!
//! The check is the app's own (`healthscan`), and so is every repair (`repair`, through the undo journal
//! in `undo`). The check saves what it found to `health-report-<id>.json` in the app's data folder, one
//! file per library; this page reads that file and the list of ignored issues, starts checks and repairs,
//! and takes a repaired finding out of the report.
//!
//! The report (written by `healthscan::run_with`):
//! `{ When, By, Deep, Cancelled, Files, Albums, Artists, Unchecked, DeepLeft, ListsLeft, Issues: [ { S, C, A, L, P, D, K, F, X } ] }`
//! where S = severity (1 serious, 2 worth fixing, 3 minor), C = category, A = artist, L = album,
//! P = folder (or file) to open, D = the sentence describing the problem, K = fix kind ("" = none,
//! "redownload" = needs a better copy), F = files involved, X = extra facts as strings.
//! An issue's "ignore" key is `C + "|" + lowercase(P)`.
use crate::config::Config;
use crate::covers;
use crate::healthscan;
use crate::theme::{self, ACCENT, BG, BORDER, CARD, CARD_HOVER, DIM, FAINT, RED, TEXT, WARN};
use chrono::{DateTime, Local};
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::text::{LayoutJob, TextWrapping};
use egui::{pos2, vec2, Align, Color32, CornerRadius, FontId, Frame, Id, Key, Label, Layout, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

mod page;

/// How often `tick` looks at the report file.
const CHECK_EVERY: Duration = Duration::from_secs(3);
const HEAD_H: f32 = 40.0;
const LINE_H: f32 = 25.0;
/// the small heading where a severity starts in the list
const SECTION_H: f32 = 46.0;
/// room over such a heading in the list, when it is not the first
const SECTION_GAP: f32 = 14.0;
const CAT_COL: f32 = 206.0;
// the cover wall: the same tile as the Library page's. `TILE` is a cover's side at its smallest: on the
// wall the covers grow from it to fill the row (see `columns`)
const TILE: f32 = 164.0;
/// room under a cover for its two lines of text
const TILE_TEXT: f32 = 42.0;
const GAP_MIN: f32 = 18.0;
const GAP_MAX: f32 = 34.0;
const ROW_GAP: f32 = 8.0;
/// space at the wall's left edge, so a tile's backdrop is not cut off (the headings start there too)
const WALL_LEFT: f32 = 8.0;

// ------------------------------------------------------------------ the report

struct Issue {
    /// 1 serious, 2 worth fixing, 3 minor
    sev: u8,
    cat: String,
    detail: String,
    /// the first clause of `detail`, for the list
    short: String,
    /// the tool's fix kind: "" = nothing offered, "redownload" = needs a better copy
    fix: String,
    files: Vec<String>,
    data: HashMap<String, String>,
    ignored: bool,
    group: usize,
}

/// The issues of one album (or one folder, for problems that are not about a single album).
struct Group {
    key: String,
    artist: String,
    album: String,
    path: String,
    /// the album's folder (the folder `path` is in, when that is a file): what other pages mark
    folder: PathBuf,
    /// a real album with a real artist, so the Library page can show it
    in_library: bool,
    /// lowercase "artist album", for the filter box
    hay: String,
    issues: Vec<usize>,
}

#[derive(Default)]
struct Report {
    when: Option<DateTime<Local>>,
    /// when a check of some folders last brought the report up to date (after an import, a repair or an
    /// Undo); the whole check's own date is `when`
    updated: Option<DateTime<Local>>,
    deep: Option<bool>,
    cancelled: bool,
    /// written by the app's own check: only such a report is repaired from
    own: bool,
    files: Option<u64>,
    albums: Option<u64>,
    artists: Option<u64>,
    unchecked: u64,
    /// tracks the app's own check has not listened to yet (a full check that was stopped, or never run)
    deep_left: u64,
    /// albums whose track list has not been compared with the official one yet
    lists_left: u64,
    issues: Vec<Issue>,
    groups: Vec<Group>,
}

/// Modified time and size of the report file and of the ignore list (None = the file is not there).
type FileSig = Option<(SystemTime, u64)>;
type Sig = (FileSig, FileSig);

enum Loaded {
    Missing,
    Unreadable,
    /// the file changed while it was being read: try again once it settles
    Changing,
    Ready(Report),
}

enum Msg {
    Loaded { library: String, sig: Sig, what: Loaded },
    /// a repair has ended
    Fixed(Result<crate::repair::Done, String>),
    /// the list of past repairs has been read
    History(Vec<crate::undo::BatchInfo>),
    /// an Undo has ended: what could not be put back
    Undone(Result<Vec<String>, String>),
    /// old backups were sent to the Recycle Bin: (repairs cleared, what could not be)
    Cleared(usize, Vec<String>),
}

/// What was clicked in one problem's block.
#[derive(PartialEq)]
enum Click {
    None,
    Ignore,
    /// the repair, and which way to do it ("" when there is one way)
    Fix(String),
    /// "Replace this album", or only the finding's own tracks: look for a better copy (the words to search for, only the tracks)
    Replace(String, bool),
    /// "Get the full album" for the loose tracks of one album (which of the finding's albums)
    GetAlbum(usize),
    /// the loose tracks an album in the library already holds go to the backups (which of the finding's albums)
    ToBackups(usize),
}

#[derive(PartialEq)]
enum State {
    Loading,
    Missing,
    Unreadable,
    Ready,
}

#[derive(Clone, PartialEq)]
enum Filter {
    All,
    Sev(u8),
    Cat(u8, String),
}

enum Row {
    /// where a severity starts: (severity, albums under it)
    Section(u8, usize),
    Album(usize),
    Issue(usize),
}

/// One cover on the wall: an album with problems that pass the filters.
struct Tile {
    group: usize,
    /// the worst of its problems on screen: its edge, and the heading it is under
    sev: u8,
    /// that problem, and how many more there are
    first: usize,
    more: usize,
}

/// What the current filters leave on screen.
#[derive(Default)]
struct View {
    rows: Vec<Row>,
    /// the same albums in the same order, for the cover wall
    tiles: Vec<Tile>,
    /// top of each row, and the height of the whole list
    ys: Vec<f32>,
    height: f32,
    /// counts that ignore the category and text filters (they are what the filters are chosen from)
    total: usize,
    by_sev: [usize; 3],
    cats: Vec<(u8, String, usize)>,
    albums: usize,
    /// albums with a serious problem among them: what the sentence at the top counts
    serious_albums: usize,
    better_copy: usize,
    ignored: usize,
    shown: usize,
}

/// (group key, the chosen issue as (category, detail); None = the whole album)
type Selection = (String, Option<(String, String)>);

pub struct HealthPage {
    library: String,
    report_file: PathBuf,
    ignore_file: PathBuf,
    state: State,
    report: Report,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    loading: bool,
    /// the files as they were when the report on screen was read
    loaded_sig: Option<Sig>,
    /// the files as they were at the last check, when that differs from `loaded_sig`
    pending_sig: Option<Sig>,
    last_check: Instant,
    filter: Filter,
    text: String,
    show_ignored: bool,
    view: View,
    dirty: bool,
    to_top: bool,
    sel: Option<Selection>,
    note: Option<String>,
    /// [open issues, serious, worth fixing, minor] of the report on screen, ignored ones left out
    open: [usize; 4],
    /// an album another page asked for while the report was still being read
    wanted: Option<(String, String)>,
    /// bring this album (group key) into view on the next draw; true = put it at the top
    scroll_to: Option<(String, bool)>,
    focus_filter: bool,
    /// the severity the pointer was on in the header last frame (a number or its part of the bar)
    hover_sev: Option<u8>,
    /// the first row on screen at the last draw, so the arrow keys can start from what is in view
    first_row: usize,
    /// the wall of covers (true) or the list of sentences
    covers_view: bool,
    /// how many covers the wall had across at the last draw, and the first one on screen (for the arrow keys)
    wall_cols: usize,
    first_tile: usize,
    /// the size the wall's covers are decoded at, held steady while the window is being resized
    wall_px: covers::Settle,
    /// where the app's own check writes what it finds for this library
    own_file: PathBuf,
    /// the app's own check, while it runs
    check: Option<std::sync::Arc<healthscan::Progress>>,
    /// a repair or an Undo is running (one at a time)
    fixing: bool,
    /// "Fix all" while it runs: (findings done, findings in all, stop asked)
    bulk: Option<std::sync::Arc<(std::sync::atomic::AtomicUsize, usize, std::sync::atomic::AtomicBool)>>,
    /// read the report again on the next tick (a repair changed it)
    reload_now: bool,
    /// when the note on screen is what a repair or an Undo said: it stays up while the report is read again
    said_at: Option<Instant>,
    /// the undo record of the repair the note on screen is about ("Undo" beside "Done.")
    last_batch: Option<String>,
    /// the settings as the page last saw them (a check that starts by itself needs them)
    last_cfg: Option<Config>,
    /// Repair history is showing in place of the list
    history_open: bool,
    /// the past repairs, newest first; None while they are being read
    history: Option<Vec<crate::undo::BatchInfo>>,
}

impl HealthPage {
    pub fn new(_egui_ctx: &egui::Context) -> Self {
        let (tx, rx) = unbounded();
        Self {
            library: String::new(),
            report_file: PathBuf::new(),
            ignore_file: ignore_file(),
            state: State::Loading,
            report: Report::default(),
            tx,
            rx,
            loading: false,
            loaded_sig: None,
            pending_sig: None,
            last_check: Instant::now(),
            filter: Filter::All,
            text: String::new(),
            show_ignored: false,
            view: View::default(),
            dirty: true,
            to_top: false,
            sel: None,
            note: None,
            open: [0; 4],
            wanted: None,
            scroll_to: None,
            focus_filter: false,
            hover_sev: None,
            first_row: 0,
            // the page opens on the list of sentences; debug: `MUSIC_RECS_HEALTH_VIEW=covers` starts on the wall
            covers_view: std::env::var("MUSIC_RECS_HEALTH_VIEW").is_ok_and(|v| v.eq_ignore_ascii_case("covers")),
            wall_cols: 1,
            first_tile: 0,
            wall_px: covers::Settle::default(),
            own_file: PathBuf::new(),
            check: None,
            fixing: false,
            bulk: None,
            reload_now: false,
            said_at: None,
            last_batch: None,
            last_cfg: None,
            // debug, like `--screenshot`: `MUSIC_RECS_HEALTH_HISTORY=1` opens the page on Repair history
            history_open: std::env::var_os("MUSIC_RECS_HEALTH_HISTORY").is_some(),
            history: None,
        }
    }

    /// What the page is doing, in a few words for the top row (None while it is doing nothing).
    pub fn job(&self) -> Option<String> {
        use std::sync::atomic::Ordering;
        if let Some(c) = &self.check {
            let (phase, done, total) = (c.phase.load(Ordering::Relaxed) as usize, c.done.load(Ordering::Relaxed), c.total.load(Ordering::Relaxed));
            let name = healthscan::PHASES[phase.min(healthscan::PHASES.len() - 1)];
            return Some(if total > 0 { format!("{name} {} of {}", num(done.min(total) as u64), num(total as u64)) } else { name.to_string() });
        }
        if let Some(b) = &self.bulk {
            return Some(format!("Fixing {} of {}", num(b.0.load(Ordering::Relaxed).min(b.1) as u64), num(b.1 as u64)));
        }
        self.fixing.then(|| "Repairing".to_string())
    }

    /// [open issues, serious, worth fixing, minor] in the last check (ignored ones left out).
    /// None when no report is loaded.
    pub fn headline(&self) -> Option<[usize; 4]> {
        (self.state == State::Ready).then_some(self.open)
    }

    /// Starts reading the report for this library if that has not happened yet, so other pages can show
    /// health marks without this page being opened. Changes nothing on disk.
    pub fn ensure_loaded(&mut self, ctx: &egui::Context, cfg: &Config) {
        self.last_cfg = Some(cfg.clone());
        self.set_library(&cfg.music_dir, ctx);
    }

    /// Shows one album's problems: every category, the album in view and its details open. An album with
    /// no problems puts its artist in the filter box instead. Asked while the report is still being read,
    /// it happens as soon as that is done.
    pub fn show_album(&mut self, artist: &str, album: &str) {
        if self.state == State::Loading {
            self.wanted = Some((artist.to_string(), album.to_string()));
            return;
        }
        self.wanted = None;
        if self.state != State::Ready {
            return;
        }
        self.filter = Filter::All;
        self.dirty = true;
        let r = &self.report;
        let open = |g: &&Group| g.issues.iter().any(|&i| self.show_ignored || !r.issues[i].ignored);
        let by_artist = || r.groups.iter().filter(|g| g.artist.to_lowercase() == artist.to_lowercase()).filter(open);
        // the name as the report has it, else the same name without years and punctuation ("[1993] Vs" = "Vs")
        let found = by_artist().find(|g| g.album.to_lowercase() == album.to_lowercase()).or_else(|| {
            let want = loose_name(album);
            by_artist().find(|g| !want.is_empty() && loose_name(&g.album) == want)
        });
        match found.map(|g| g.key.clone()) {
            Some(key) => {
                self.text.clear();
                self.sel = Some((key.clone(), None));
                self.scroll_to = Some((key, true));
            }
            None => {
                self.text = artist.to_string();
                self.sel = None;
                self.to_top = true;
            }
        }
    }

    /// Tells the other pages which album folders have open problems (none when there is no report).
    fn publish_marks(&self) {
        crate::links::set_health_marks(if self.state == State::Ready { marks(&self.report) } else { vec![] });
    }

    /// Runs every frame, whichever page is showing.
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.poll();
        self.check_finished(ctx);
        self.recheck_changed(ctx);
        if std::mem::take(&mut self.reload_now) {
            self.start_load(ctx);
        }
        if self.history_open && self.history.is_none() && !self.fixing {
            self.read_history(ctx);
        }
        if self.library.is_empty() {
            return;
        }
        if self.last_check.elapsed() >= CHECK_EVERY {
            self.last_check = Instant::now();
            if !self.loading {
                let now = (file_sig(&self.report_file), file_sig(&self.ignore_file));
                if self.loaded_sig.as_ref() == Some(&now) {
                    self.pending_sig = None;
                } else if self.pending_sig.as_ref() == Some(&now) {
                    // unchanged since the last look, so whoever was writing it has finished
                    self.pending_sig = None;
                    self.start_load(ctx);
                } else {
                    self.pending_sig = Some(now);
                }
            }
        }
        ctx.request_repaint_after(CHECK_EVERY);
    }

    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Fixed(done) => {
                    self.fixing = false;
                    self.bulk = None;
                    self.reload_now = true;
                    self.said_at = Some(Instant::now());
                    // what was just done can be undone from the sentence that says so
                    self.last_batch = done.as_ref().ok().filter(|d| d.changed > 0).and_then(|d| d.batch.clone());
                    // the folders it changed are looked at again (`recheck_changed`), so the page says what is
                    // true now: a new album folder, what is left of a finding done in part
                    if let Some(id) = &self.last_batch {
                        for p in crate::undo::touched(&crate::repair::backups_root(Path::new(&self.library)), id) {
                            crate::links::folder_changed(&p);
                        }
                    }
                    self.note = Some(match done {
                        Ok(d) if d.problems.is_empty() && d.changed > 0 => "Done.".to_string(),
                        Ok(d) if d.problems.is_empty() => "There was nothing left to do.".to_string(),
                        Ok(d) if d.changed > 0 => format!("Done in part. {}", said(&d.problems)),
                        Ok(d) => said(&d.problems),
                        Err(why) => why,
                    });
                }
                Msg::History(list) => self.history = Some(list),
                Msg::Cleared(n, problems) => {
                    self.fixing = false;
                    self.said_at = Some(Instant::now());
                    self.history = None; // read again: the cleared repairs are gone from the list
                    let went = format!("{} old {} sent to the Recycle Bin.", num(n as u64), if n == 1 { "repair's backups were" } else { "repairs' backups were" });
                    self.note = Some(match (n, problems.is_empty()) {
                        (_, true) => went,
                        (0, false) => said(&problems),
                        (_, false) => format!("{went} {}", said(&problems)),
                    });
                }
                Msg::Undone(done) => {
                    self.fixing = false;
                    self.said_at = Some(Instant::now());
                    self.history = None; // read again: the repair is marked undone now
                    self.note = Some(match done {
                        Ok(problems) if problems.is_empty() => "Undone.".to_string(),
                        Ok(problems) => said(&problems),
                        Err(why) => why,
                    });
                }
                Msg::Loaded { library, sig, what } => {
                    if library != self.library {
                        continue; // an answer for a library that is no longer the chosen one
                    }
                    self.loading = false;
                    match what {
                        Loaded::Changing => {}
                        Loaded::Missing => {
                            self.loaded_sig = Some(sig);
                            self.report = Report::default();
                            self.state = State::Missing;
                            self.sel = None;
                            self.open = [0; 4];
                            self.publish_marks();
                        }
                        Loaded::Unreadable => {
                            // keep showing the last good report, if there is one: a check is probably mid-save
                            self.loaded_sig = Some(sig);
                            if self.state != State::Ready {
                                self.state = State::Unreadable;
                                self.publish_marks();
                            }
                        }
                        Loaded::Ready(r) => {
                            self.loaded_sig = Some(sig);
                            self.report = r;
                            if self.state != State::Ready {
                                self.to_top = true; // a first look starts at the worst, wherever the list was left
                            }
                            self.state = State::Ready;
                            if self.said_at.take_if(|t| t.elapsed() > Duration::from_secs(30)).is_some() || self.said_at.is_none() {
                                self.note = None;
                            }
                            self.open = [0; 4];
                            for i in self.report.issues.iter().filter(|i| !i.ignored && counted(i)) {
                                self.open[0] += 1;
                                self.open[i.sev as usize] += 1;
                            }
                            self.publish_marks();
                        }
                    }
                    self.dirty = true;
                    if self.state != State::Loading {
                        if let Some((artist, album)) = self.wanted.take() {
                            self.show_album(&artist, &album);
                        }
                    }
                }
            }
        }
    }

    fn start_load(&mut self, ctx: &egui::Context) {
        if self.loading || self.library.is_empty() {
            return;
        }
        self.loading = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let (library, report_file, ignore_file) = (self.library.clone(), self.report_file.clone(), self.ignore_file.clone());
        std::thread::spawn(move || {
            let before = (file_sig(&report_file), file_sig(&ignore_file));
            let mut what = load(&report_file, &ignore_file);
            let after = (file_sig(&report_file), file_sig(&ignore_file));
            if before != after {
                what = Loaded::Changing;
            }
            let _ = tx.send(Msg::Loaded { library, sig: after, what });
            ctx.request_repaint();
        });
    }

    /// Points the page at a library folder (each library has its own saved report).
    fn set_library(&mut self, music_dir: &str, ctx: &egui::Context) {
        if self.library == music_dir {
            return;
        }
        self.library = music_dir.to_string();
        self.own_file = healthscan::report_file(music_dir);
        self.report_file = match std::env::var_os("MUSIC_RECS_HEALTH_REPORT") {
            Some(p) => PathBuf::from(p), // for trying the page on a made-up report
            None => self.own_file.clone(),
        };
        if let Some(c) = self.check.take() {
            c.cancel.store(true, std::sync::atomic::Ordering::Relaxed); // it was for the other library
        }
        self.report = Report::default();
        self.state = State::Loading;
        self.loaded_sig = None;
        self.pending_sig = None;
        self.loading = false;
        self.sel = None;
        self.open = [0; 4];
        self.filter = Filter::All;
        self.dirty = true;
        self.last_check = Instant::now();
        self.start_load(ctx);
    }

    // ------------------------------------------------------------------ the view

    fn rebuild(&mut self) {
        let r = &self.report;
        let mut v = View::default();
        let visible = |i: &Issue| self.show_ignored || !i.ignored;
        let mut cats: HashMap<(u8, &str), usize> = HashMap::new();
        for i in &r.issues {
            if i.ignored {
                v.ignored += 1;
            }
            if !visible(i) {
                continue;
            }
            *cats.entry((i.sev, i.cat.as_str())).or_default() += 1;
            if !counted(i) {
                continue; // it has its line among the categories, and is not one of the open issues
            }
            v.total += 1;
            v.by_sev[(i.sev - 1) as usize] += 1;
            if i.fix == "redownload" {
                v.better_copy += 1;
            }
        }
        v.albums = r.groups.iter().filter(|g| g.issues.iter().any(|&i| visible(&r.issues[i]) && counted(&r.issues[i]))).count();
        v.serious_albums = r.groups.iter().filter(|g| g.issues.iter().any(|&i| r.issues[i].sev == 1 && visible(&r.issues[i]) && counted(&r.issues[i]))).count();
        v.cats = cats.into_iter().map(|((s, c), n)| (s, c.to_string(), n)).collect();
        v.cats.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)).then(a.1.cmp(&b.1)));
        // a category that is gone after a new check leaves nothing to show: go back to everything
        let exists = match &self.filter {
            Filter::All => true,
            Filter::Sev(s) => v.by_sev[(*s - 1) as usize] > 0,
            Filter::Cat(s, c) => v.cats.iter().any(|(s2, c2, _)| s2 == s && c2 == c),
        };
        if !exists {
            self.filter = Filter::All;
        }
        let text = self.text.trim().to_lowercase();
        // each album's lines that pass the filters; the worst of them decides which heading it goes under
        let mut shown: Vec<(u8, usize, Vec<usize>)> = vec![];
        for (gi, g) in r.groups.iter().enumerate() {
            if !text.is_empty() && !g.hay.contains(&text) {
                continue;
            }
            // (the quiet category is listed when it is the one picked, not among everything else)
            let wanted = |i: &Issue| match &self.filter {
                Filter::All => counted(i),
                Filter::Sev(s) => i.sev == *s && counted(i),
                Filter::Cat(s, c) => i.sev == *s && i.cat == *c,
            };
            let lines: Vec<usize> = g.issues.iter().copied().filter(|&ii| wanted(&r.issues[ii]) && visible(&r.issues[ii])).collect();
            if let Some(worst) = lines.iter().map(|&ii| r.issues[ii].sev).min() {
                shown.push((worst, gi, lines));
            }
        }
        shown.sort_by_key(|s| s.0); // the albums are already in order, and this keeps that order within a severity
        let mut albums_in = [0usize; 3];
        for s in &shown {
            albums_in[(s.0 - 1) as usize] += 1;
        }
        let (mut y, mut section) = (0.0, 0u8);
        for (worst, gi, lines) in shown {
            if worst != section {
                section = worst;
                v.rows.push(Row::Section(worst, albums_in[(worst - 1) as usize]));
                v.ys.push(y);
                // (a heading after the first has room over it: it is drawn that much lower in its row)
                y += SECTION_H + if v.rows.len() > 1 { SECTION_GAP } else { 0.0 };
            }
            v.tiles.push(Tile { group: gi, sev: worst, first: lines[0], more: lines.len() - 1 });
            v.rows.push(Row::Album(gi));
            v.ys.push(y);
            y += HEAD_H;
            for ii in lines {
                v.rows.push(Row::Issue(ii));
                v.ys.push(y);
                y += LINE_H;
                v.shown += 1;
            }
        }
        v.height = y + 16.0;
        // the album on the right is gone after a new check: close it
        if let Some((key, _)) = &self.sel {
            if !r.groups.iter().any(|g| &g.key == key) {
                self.sel = None;
            }
        }
        self.view = v;
        self.dirty = false;
    }

    // ------------------------------------------------------------------ drawing

    /// Draws the page in the space under the app's row of page names.
    pub fn ui(&mut self, ui: &mut Ui, cfg: &Config) {
        LOOK_ONLY.store(cfg.look_only, std::sync::atomic::Ordering::Relaxed);
        KEEP_LOSSLESS.store(cfg.keep_lossless, std::sync::atomic::Ordering::Relaxed);
        if self.last_cfg.as_ref().is_none_or(|c| c.music_dir != cfg.music_dir || c.skip_folders != cfg.skip_folders || c.alias_file != cfg.alias_file) {
            self.last_cfg = Some(cfg.clone());
        }
        // debug: `MUSIC_RECS_HEALTH_WIDTH=760` draws the page that wide, to see a narrow window without resizing one
        if let Some(w) = std::env::var("MUSIC_RECS_HEALTH_WIDTH").ok().and_then(|w| w.parse::<f32>().ok()).filter(|w| *w < ui.available_width()) {
            let mut rect = ui.available_rect_before_wrap();
            rect.set_width(w);
            let mut part = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            part.set_clip_rect(rect);
            return self.page(&mut part, cfg);
        }
        self.page(ui, cfg);
    }

    /// Ctrl+F goes to the filter box. Esc closes the details, or else clears the filter. Up and Down walk
    /// through the albums in the list (unless something is being typed).
    fn keys(&mut self, ctx: &egui::Context) {
        if self.state != State::Ready {
            return;
        }
        let typing = ctx.text_edit_focused();
        let sideways = !typing && self.covers_view;
        let (esc, find, up, down, left, right) = ctx.input_mut(|i| {
            (
                i.key_pressed(Key::Escape),
                i.modifiers.command && i.key_pressed(Key::F),
                !typing && i.consume_key(Modifiers::NONE, Key::ArrowUp),
                !typing && i.consume_key(Modifiers::NONE, Key::ArrowDown),
                sideways && i.consume_key(Modifiers::NONE, Key::ArrowLeft),
                sideways && i.consume_key(Modifiers::NONE, Key::ArrowRight),
            )
        });
        if find {
            self.focus_filter = true;
        }
        if esc {
            if self.sel.is_some() {
                self.sel = None;
            } else if !self.text.is_empty() {
                self.text.clear();
                self.dirty = true;
                self.to_top = true;
            } else if self.filter != Filter::All {
                self.filter = Filter::All;
                self.dirty = true;
                self.to_top = true;
            }
        }
        if self.covers_view {
            let (dx, dy) = (right as i32 - left as i32, down as i32 - up as i32);
            if dx != 0 || dy != 0 {
                self.wall_step(dx, dy);
            }
        } else if up != down {
            self.step(down);
        }
    }

    /// The arrow keys on the cover wall: the cover beside, above or under the chosen one (the first one on
    /// screen when none is chosen), kept in view.
    fn wall_step(&mut self, dx: i32, dy: i32) {
        let tiles = &self.view.tiles;
        if tiles.is_empty() {
            return;
        }
        let groups = &self.report.groups;
        let at = self.sel.as_ref().and_then(|(key, _)| tiles.iter().position(|t| groups[t.group].key == *key));
        let next = match at {
            Some(n) => wall_move(&runs(tiles), self.wall_cols.max(1), n, dx, dy),
            None => self.first_tile.min(tiles.len() - 1),
        };
        let key = groups[tiles[next].group].key.clone();
        self.sel = Some((key.clone(), None));
        self.scroll_to = Some((key, false));
    }

    /// Opens the album after (or before) the one whose details are open, and keeps it in view.
    fn step(&mut self, down: bool) {
        let groups = &self.report.groups;
        let albums: Vec<(usize, usize)> = self.view.rows.iter().enumerate().filter_map(|(row, r)| if let Row::Album(g) = r { Some((row, *g)) } else { None }).collect();
        let at = self.sel.as_ref().and_then(|(key, _)| albums.iter().position(|(_, g)| groups[*g].key == *key));
        let next = match at {
            Some(n) if down => (n + 1).min(albums.len().saturating_sub(1)),
            Some(n) => n.saturating_sub(1),
            // nothing chosen yet: start with the first album on screen
            None => albums.iter().position(|(row, _)| *row >= self.first_row).unwrap_or(0),
        };
        if let Some((_, g)) = albums.get(next) {
            let key = groups[*g].key.clone();
            self.sel = Some((key.clone(), None));
            self.scroll_to = Some((key, false));
        }
    }

    /// With one category chosen on the left: "Fix all N" runs that category's repair over every finding of
    /// it that is not ignored, as one entry in Repair history. Only the kinds with one way to go that move
    /// nothing of an album's own (`repair::can_fix_all`). The word stands beside the category's name over
    /// the list. While it runs the last check's card counts, and "Stop" there stops it after the finding
    /// in hand.
    fn fix_all_word(&mut self, ui: &mut Ui, ctx: &egui::Context, cfg: &Config) {
        use std::sync::atomic::Ordering;
        // (while one runs, the last check's card says how far it is, with "Stop")
        if self.bulk.is_some() || cfg.look_only {
            return;
        }
        let Filter::Cat(sev, cat) = &self.filter else { return };
        if self.state != State::Ready || !self.report.own || self.fixing {
            return;
        }
        let wanted = |i: &&Issue| i.sev == *sev && i.cat == *cat && !i.ignored && crate::repair::can_fix_all(&i.fix);
        let n = self.report.issues.iter().filter(wanted).count();
        // (quiet when these repairs rewrite tags and PowerShell 7 is missing: each finding says why)
        let can = crate::importer::tools::pwsh().is_some() || !self.report.issues.iter().filter(wanted).any(|i| crate::repair::needs_tag_editor(&i.fix, ""));
        if n < 2 || !theme::main_action_sized(ui, &format!("Fix all {}", num(n as u64)), 14.0, can).clicked() {
            return;
        }
        let asks: Vec<crate::repair::Ask> = self
            .report
            .issues
            .iter()
            .filter(wanted)
            .map(|i| {
                let g = &self.report.groups[i.group];
                crate::repair::Ask { choice: String::new(), fix: i.fix.clone(), cat: i.cat.clone(), path: g.path.clone(), files: i.files.clone(), artist: g.artist.clone(), album: g.album.clone(), data: i.data.clone() }
            })
            .collect();
        let title = format!("Fix all: {cat} ({} issues)", asks.len());
        let progress = std::sync::Arc::new((std::sync::atomic::AtomicUsize::new(0), asks.len(), std::sync::atomic::AtomicBool::new(false)));
        self.bulk = Some(progress.clone());
        self.fixing = true;
        self.note = None;
        let (tx, ctx, library, report) = (self.tx.clone(), ctx.clone(), PathBuf::from(&self.library), self.own_file.clone());
        let quality_cache = healthscan::Options::for_app(cfg, false).quality_cache;
        let lyrics = crate::importer::lyrics::Lookup::for_app(!std::env::args().any(|a| a == "--no-online"), !cfg.lyrics_plain);
        std::thread::spawn(move || {
            let done = crate::importer::tools::find_tool("ffprobe").and_then(|ffprobe| {
                let tools = crate::repair::Tools { ffmpeg: crate::importer::tools::find_tool("ffmpeg")?, ffprobe, quality_cache: Some(quality_cache), online: !std::env::args().any(|a| a == "--no-online"), lyrics: Some(lyrics.clone()) };
                crate::repair::apply_all(&asks, &title, &library, &crate::repair::backups_root(&library), &tools, |n, ok| {
                    if ok {
                        drop_finding(&report, &asks[n].cat, &asks[n].path);
                    }
                    progress.0.store(n + 1, Ordering::Relaxed);
                    ctx.request_repaint();
                    !progress.2.load(Ordering::Relaxed)
                })
            });
            let _ = tx.send(Msg::Fixed(done));
            ctx.request_repaint();
        });
    }

    /// Carries out the repair for one finding in the background (see `crate::repair`): planned, applied
    /// through an undo record, and, when all of it was done, taken out of the report.
    fn start_fix(&mut self, ctx: &egui::Context, cfg: &Config, ask: crate::repair::Ask, title: String) {
        // (one change to the library at a time, none while a check is reading it, and none at all for a
        // library whose owner chose "Look only, change nothing")
        if self.fixing || self.check.is_some() || cfg.look_only {
            return;
        }
        self.fixing = true;
        self.note = None;
        let (tx, ctx, library, report) = (self.tx.clone(), ctx.clone(), PathBuf::from(&self.library), self.own_file.clone());
        let quality_cache = healthscan::Options::for_app(cfg, false).quality_cache;
        let lyrics = crate::importer::lyrics::Lookup::for_app(!std::env::args().any(|a| a == "--no-online"), !cfg.lyrics_plain);
        std::thread::spawn(move || {
            let done = crate::importer::tools::find_tool("ffprobe").and_then(|ffprobe| {
                // the same saved deep-check results the check uses, so a file whose tags are rewritten keeps its result
                let tools = crate::repair::Tools { ffmpeg: crate::importer::tools::find_tool("ffmpeg")?, ffprobe, quality_cache: Some(quality_cache), online: !std::env::args().any(|a| a == "--no-online"), lyrics: Some(lyrics.clone()) };
                let plan = crate::repair::plan(&ask, &library, &tools);
                crate::repair::apply(&plan, &title, &library, &crate::repair::backups_root(&library), &tools)
            });
            if done.as_ref().is_ok_and(|d| d.problems.is_empty()) {
                drop_finding(&report, &ask.cat, &ask.path);
            }
            let _ = tx.send(Msg::Fixed(done));
            ctx.request_repaint();
        });
    }

    fn read_history(&mut self, ctx: &egui::Context) {
        self.history = Some(vec![]);
        let (tx, ctx, library) = (self.tx.clone(), ctx.clone(), PathBuf::from(&self.library));
        std::thread::spawn(move || {
            let _ = tx.send(Msg::History(crate::undo::batches(&crate::repair::backups_root(&library))));
            ctx.request_repaint();
        });
    }

    fn start_undo(&mut self, ctx: &egui::Context, id: String) {
        // (one change to the library at a time, and none while a check is reading it)
        if self.fixing || self.check.is_some() {
            return;
        }
        self.fixing = true;
        self.note = None;
        let (tx, ctx, library) = (self.tx.clone(), ctx.clone(), PathBuf::from(&self.library));
        std::thread::spawn(move || {
            let root = crate::repair::backups_root(&library);
            let done = crate::repair::undo(&root, &id);
            // what was put back is looked at again (`recheck_changed`), so its findings return by themselves
            if done.is_ok() {
                for p in crate::undo::touched(&root, &id) {
                    crate::links::folder_changed(&p);
                }
            }
            let _ = tx.send(Msg::Undone(done));
            ctx.request_repaint();
        });
    }

    /// Sends the backups of every repair older than 30 days to the Recycle Bin (see `undo::clear_old`).
    /// Only ever started by a click on its words in Repair history.
    fn start_clear(&mut self, ctx: &egui::Context) {
        // (one change to the library at a time, and none while a check is reading it)
        if self.fixing || self.check.is_some() {
            return;
        }
        self.fixing = true;
        self.note = None;
        let (tx, ctx, library) = (self.tx.clone(), ctx.clone(), PathBuf::from(&self.library));
        std::thread::spawn(move || {
            let (n, problems) = crate::undo::clear_old(&crate::repair::backups_root(&library), crate::undo::CLEAR_AFTER_DAYS, Local::now(), &crate::undo::to_recycle_bin);
            let _ = tx.send(Msg::Cleared(n, problems));
            ctx.request_repaint();
        });
    }

    /// Every repair there is a record of, newest first: when, what, how many changes, and the word that
    /// puts it all back. Above them, when some are older than 30 days, the words that clear those backups.
    fn history_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        theme::caption(ui, "REPAIR HISTORY");
        ui.add_space(8.0);
        let Some(list) = self.history.clone() else { return theme::empty(ui, "Reading the repairs.") };
        if list.is_empty() {
            return theme::empty(ui, "No repairs yet.");
        }
        let now = Local::now();
        let old = list.iter().filter(|b| b.when.is_some_and(|w| now.signed_duration_since(w) > chrono::Duration::days(crate::undo::CLEAR_AFTER_DAYS))).count();
        if old > 0 {
            let words = if old == 1 { "Send the backups of the 1 repair older than 30 days to the Recycle Bin".to_string() } else { format!("Send the backups of the {} repairs older than 30 days to the Recycle Bin", num(old as u64)) };
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                if theme::quiet(ui, &words, 13.5, !self.fixing).clicked() {
                    self.start_clear(ctx);
                }
                ui.label(RichText::new("They can then be undone only by restoring them from the Recycle Bin.").size(12.0).color(FAINT()));
            });
            ui.add_space(10.0);
        }
        let mut undo: Option<String> = None;
        ScrollArea::vertical().id_salt("health-history-scroll").auto_shrink([false, false]).show(ui, |ui| {
            for b in &list {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    let when = b.when.map(|w| when_words(w, now)).unwrap_or_default();
                    ui.allocate_ui_with_layout(vec2(150.0, 22.0), Layout::left_to_right(Align::Center), |ui| {
                        ui.set_min_width(150.0);
                        ui.add(Label::new(RichText::new(when).size(13.0).color(DIM())).truncate());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        if b.undone {
                            ui.add_sized(vec2(60.0, 22.0), Label::new(RichText::new("Undone").size(13.0).color(FAINT())));
                        } else if ui.add_sized(vec2(60.0, 22.0), |ui: &mut Ui| ui.centered_and_justified(|ui| theme::quiet(ui, "Undo", 13.5, !self.fixing)).inner).clicked() {
                            undo = Some(b.id.clone());
                        }
                        let n = format!("{} {}", num(b.changes as u64), if b.changes == 1 { "change" } else { "changes" });
                        ui.add_sized(vec2(110.0, 22.0), Label::new(RichText::new(n).size(13.0).color(DIM())));
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(Label::new(RichText::new(&b.title).size(14.0).color(if b.undone { DIM() } else { TEXT() })).truncate());
                        });
                    });
                });
                ui.add_space(2.0);
            }
        });
        // debug: `MUSIC_RECS_HEALTH_UNDO=1` undoes the newest repair once the list is up
        static UNDO_ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if undo.is_none() && std::env::var_os("MUSIC_RECS_HEALTH_UNDO").is_some() && !self.fixing && !UNDO_ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            undo = list.iter().find(|b| !b.undone).map(|b| b.id.clone());
        }
        if let Some(id) = undo {
            self.start_undo(ctx, id);
        }
    }

    /// Folders that an import, one of the page's own repairs or an Undo has just changed are checked again
    /// by themselves, so the page does not go on saying what was true before (see `links::folder_changed`,
    /// `healthscan::Options::only`). Only the artists' folders are read, in one pass, only when a report of
    /// the app's own is on screen, and never while another check runs (the folders then wait their turn).
    fn recheck_changed(&mut self, ctx: &egui::Context) {
        if self.check.is_some() || self.fixing || self.state != State::Ready || !self.report.own || self.report_file != self.own_file {
            return;
        }
        let Some(cfg) = self.last_cfg.clone() else { return };
        let library = PathBuf::from(&self.library);
        // the folders at the top of the library that hold what changed, each once
        let mut tops: Vec<PathBuf> = vec![];
        while let Some(changed) = crate::links::take_changed() {
            let Some(top) = changed.strip_prefix(&library).ok().and_then(|rel| rel.components().next()).map(|c| library.join(c.as_os_str())) else { continue };
            if !tops.iter().any(|t| t.as_os_str().eq_ignore_ascii_case(top.as_os_str())) {
                tops.push(top);
            }
        }
        if tops.is_empty() {
            return;
        }
        let mut o = healthscan::Options::for_app(&cfg, false);
        o.only = tops;
        let ctx = ctx.clone();
        if let Ok(p) = healthscan::start(o, move || ctx.request_repaint()) {
            self.check = Some(p);
        }
    }

    /// Starts the app's own check of the library (see `healthscan`). It only reads the library.
    fn start_check(&mut self, ctx: &egui::Context, cfg: &Config, deep: bool) {
        // (not while a repair is changing the library: the check would read it half done)
        if self.fixing {
            return;
        }
        let ctx = ctx.clone();
        match healthscan::start(healthscan::Options::for_app(cfg, deep), move || ctx.request_repaint()) {
            Ok(p) => {
                self.note = None;
                self.check = Some(p);
            }
            Err(why) => {
                FFMPEG_NOTED.store(crate::helpers::arrived(crate::helpers::Helper::Ffmpeg), std::sync::atomic::Ordering::Relaxed);
                self.note = Some(why);
            }
        }
    }

    /// A check that has ended: its report is read, or what went wrong is said.
    fn check_finished(&mut self, ctx: &egui::Context) {
        use std::sync::atomic::Ordering;
        let Some(c) = self.check.as_ref().filter(|c| c.finished.load(Ordering::Relaxed)) else { return };
        let (why, wrote, found) = (c.error.lock().unwrap_or_else(|e| e.into_inner()).clone(), c.wrote.load(Ordering::Relaxed), c.issues.load(Ordering::Relaxed));
        self.check = None;
        if !why.is_empty() {
            self.note = Some(why);
        } else if wrote && self.report_file == self.own_file {
            self.start_load(ctx);
        } else if wrote {
            // the page was pointed at a made-up report for a look: the check's own is not the one on screen
            self.note = Some(format!("The check found {} issues.", num(found as u64)));
        }
    }

    /// The two quiet lines under the sentence at the top: what the last check covered and when it ran
    /// ("22,840 tracks, 1,739 albums, 522 artists  ·  full check today 3:02 PM"), then what it has not
    /// got to yet. The folder itself is in Settings.
    fn facts(&self) -> (String, String) {
        let mut parts: Vec<String> = vec![];
        let r = &self.report;
        let mut sizes = vec![];
        for (n, one, many) in [(r.files, "track", "tracks"), (r.albums, "album", "albums"), (r.artists, "artist", "artists")] {
            if let Some(n) = n {
                sizes.push(format!("{} {}", num(n), if n == 1 { one } else { many }));
            }
        }
        if !sizes.is_empty() {
            parts.push(sizes.join(", "));
        }
        let when = r.when.or_else(|| self.loaded_sig.as_ref().and_then(|s| s.0).map(|(t, _)| DateTime::<Local>::from(t)));
        let kind = match r.deep {
            Some(true) => "full check",
            Some(false) => "quick check",
            None => "checked",
        };
        match when {
            Some(w) => parts.push(format!("{kind} {}", when_words(w, Local::now()))),
            None if r.deep.is_some() => parts.push(kind.to_string()),
            None => {}
        }
        // folders changed since (an import, a repair, an Undo) were looked at again, and the counts follow
        if let Some(u) = r.updated.filter(|u| when.is_none_or(|w| *u > w)) {
            parts.push(format!("brought up to date {}", when_words(u, Local::now())));
        }
        let mut left: Vec<String> = vec![];
        if r.cancelled {
            left.push("stopped early, so these are partial results".into());
        } else if r.unchecked > 0 {
            left.push(format!("{} {} could not be checked", num(r.unchecked), if r.unchecked == 1 { "track" } else { "tracks" }));
        }
        // a full check that was stopped part way: how much of the library has not been listened to
        if r.deep_left > 0 && r.files.is_some_and(|f| r.deep_left < f) {
            left.push(format!("{} {} not listened to yet", num(r.deep_left), if r.deep_left == 1 { "track" } else { "tracks" }));
        }
        // albums the check has not yet held against their official track list (stopped, offline, or still running)
        if r.lists_left > 0 {
            left.push(format!("{} {} still waiting for a track-list comparison", num(r.lists_left), if r.lists_left == 1 { "album" } else { "albums" }));
        }
        // (the second line is a sentence of its own)
        let mut more = left.join("  ·  ");
        if let Some(first) = more.get(..1).map(|c| c.to_uppercase()) {
            more.replace_range(..1, &first);
        }
        (parts.join("  ·  "), more)
    }

    fn filters(&mut self, ui: &mut Ui) {
        let before = (self.filter.clone(), self.text.clone(), self.show_ignored);
        let filter_box = Id::unique("health-filter-box");
        ui.add(TextEdit::singleline(&mut self.text).id(filter_box).hint_text("Filter by artist or album").desired_width(f32::INFINITY));
        if std::mem::take(&mut self.focus_filter) {
            ui.ctx().memory_mut(|m| m.request_focus(filter_box));
        }
        ui.add_space(10.0);
        let mut flip_ignored = false;
        ScrollArea::vertical().id_salt("health-cats").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut pick = None;
            if cat_row(ui, "All issues", self.view.total, TEXT(), 0.0, self.filter == Filter::All) {
                pick = Some(Filter::All);
            }
            for s in 1..=3u8 {
                let n = self.view.by_sev[(s - 1) as usize];
                if n == 0 {
                    continue;
                }
                ui.add_space(12.0);
                if cat_row(ui, sev_name(s), n, sev_color(s), 0.0, self.filter == Filter::Sev(s)) {
                    pick = Some(Filter::Sev(s));
                }
                for (cs, c, n) in &self.view.cats {
                    if *cs != s {
                        continue;
                    }
                    let f = Filter::Cat(s, c.clone());
                    let on = self.filter == f;
                    if cat_row(ui, c, *n, if on { TEXT() } else { DIM() }, 12.0, on) {
                        pick = Some(f);
                    }
                }
            }
            if let Some(f) = pick {
                self.filter = f;
            }
            // what was set aside, after the categories it would join
            if self.view.ignored > 0 || self.show_ignored {
                ui.add_space(14.0);
                let n = num(self.view.ignored as u64);
                let words = if self.show_ignored { format!("Hide the {n} ignored") } else { format!("Include the {n} ignored") };
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    flip_ignored = theme::quiet(ui, &words, 12.5, true).clicked();
                });
            }
            ui.add_space(8.0);
        });
        if flip_ignored {
            self.show_ignored = !self.show_ignored;
        }
        if before != (self.filter.clone(), self.text.clone(), self.show_ignored) {
            self.dirty = true;
            self.to_top = true;
            ui.ctx().request_repaint();
        }
    }

    /// What stands in the findings' card when there is nothing to list.
    fn nothing_words(&self) -> &'static str {
        if self.report.issues.is_empty() {
            "The last check found no problems."
        } else if self.view.total > 0 || !self.text.trim().is_empty() {
            "Nothing matches this filter."
        } else if self.view.ignored > 0 && !self.show_ignored {
            "Every issue found is on the ignore list."
        } else {
            "No open issues."
        }
    }

    fn list(&mut self, ui: &mut Ui) {
        if self.view.rows.is_empty() {
            theme::empty(ui, self.nothing_words());
            return;
        }
        let mut area = ScrollArea::vertical().id_salt("health-list").auto_shrink([false, false]);
        if std::mem::take(&mut self.to_top) {
            area = area.vertical_scroll_offset(0.0);
        }
        let scroll_to = self.scroll_to.take();
        let (view, report, sel) = (&self.view, &self.report, &self.sel);
        let mut clicked: Option<Selection> = None;
        let mut first_row = 0;
        // where one album's rows start and end in the list (its name row first)
        let span = |row: usize| {
            let end = (row + 1..view.rows.len()).find(|n| !matches!(view.rows[*n], Row::Issue(_))).map(|n| view.ys[n]).unwrap_or(view.height - 16.0);
            (view.ys[row] + HEAD_H - LINE_H - 2.0, end)
        };
        let row_of = |key: &str| view.rows.iter().position(|r| matches!(r, Row::Album(g) if report.groups[*g].key == key));
        area.show_viewport(ui, |ui, visible| {
            // whole pixels, so text does not shimmer while the list moves
            let px = ui.ctx().pixels_per_point();
            let snap = |v: f32| (v * px).round() / px;
            let (left, top, width) = (ui.max_rect().left(), ui.max_rect().top(), ui.available_width());
            ui.allocate_rect(Rect::from_min_size(pos2(left, top), vec2(width, view.height)), Sense::hover());
            if let Some((key, to_top)) = &scroll_to {
                if let Some(row) = row_of(key) {
                    let (from, to) = span(row);
                    // room above for the heading that stays at the top, and never more than fits on screen
                    let from = from - SECTION_H - 6.0;
                    let to = (to + 8.0).min(from + (visible.height() - 8.0).max(HEAD_H));
                    ui.scroll_to_rect(Rect::from_min_max(pos2(left, top + from), pos2(left + width, top + to)), (*to_top).then_some(Align::Min));
                }
            }
            // the album whose details are open sits on a card of its own
            if let Some(row) = sel.as_ref().and_then(|(key, _)| row_of(key)) {
                let (from, to) = span(row);
                if to > visible.top() && from < visible.bottom() {
                    let card = Rect::from_min_max(pos2(left, snap(top + from) - 3.0), pos2(left + width, snap(top + to) + 3.0));
                    ui.painter().rect_filled(card, 8, lifted());
                }
            }
            let first = view.ys.partition_point(|y| *y + HEAD_H < visible.top());
            first_row = first;
            for idx in first..view.rows.len() {
                let y = view.ys[idx];
                if y > visible.bottom() {
                    break;
                }
                match view.rows[idx] {
                    Row::Section(sev, albums) => {
                        let gap = if idx > 0 { SECTION_GAP } else { 0.0 };
                        section_head(ui, Rect::from_min_size(pos2(left, snap(top + y + gap)), vec2(width, SECTION_H)), sev, albums);
                    }
                    Row::Album(gi) => {
                        let g = &report.groups[gi];
                        // the name sits at the bottom of its row, so each album has air above it
                        let rect = Rect::from_min_size(pos2(left, snap(top + y + HEAD_H - LINE_H - 2.0)), vec2(width, LINE_H + 2.0));
                        let r = ui.interact(rect, ui.scope_id().with(("album", gi)), Sense::click());
                        if crate::script::on() {
                            crate::script::seen(&format!("album:{}", g.album), rect);
                        }
                        let chosen = matches!(sel, Some((k, None)) if *k == g.key);
                        // pointing at an album's name lights the whole album: that is what a click opens
                        let whole = ui.ctx().animate_bool_with_time(r.id.with("whole"), r.hovered(), 0.12);
                        if whole > 0.0 && !matches!(sel, Some((k, _)) if *k == g.key) {
                            let (from, to) = span(idx);
                            let card = Rect::from_min_max(pos2(left, snap(top + from) - 3.0), pos2(left + width, snap(top + to) + 3.0));
                            ui.painter().rect_filled(card, 8, lifted().gamma_multiply(whole));
                        }
                        row_back(ui, rect, r.id, r.hovered(), chosen);
                        let cy = rect.center().y;
                        let w = paint(ui, pos2(rect.left() + 12.0, cy), &g.artist, 15.0, TEXT(), rect.width() - 24.0);
                        if !g.album.is_empty() {
                            let x = rect.left() + 12.0 + w + 10.0;
                            paint(ui, pos2(x, cy + 0.5), &g.album, 14.0, DIM(), rect.right() - 12.0 - x);
                        }
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            clicked = Some((g.key.clone(), None));
                        }
                    }
                    Row::Issue(ii) => {
                        let i = &report.issues[ii];
                        let g = &report.groups[i.group];
                        let rect = Rect::from_min_size(pos2(left, snap(top + y)), vec2(width, LINE_H));
                        let r = ui.interact(rect, ui.scope_id().with(("issue", ii)), Sense::click());
                        if crate::script::on() {
                            crate::script::seen(&format!("issue:{}|{}", i.cat, g.album), rect);
                        }
                        let chosen = matches!(sel, Some((k, Some((c, d)))) if *k == g.key && *c == i.cat && *d == i.detail);
                        row_back(ui, rect, r.id, r.hovered(), chosen);
                        let cy = rect.center().y;
                        let (c1, c2) = if i.ignored { (sev_color(i.sev).gamma_multiply(0.45), FAINT().gamma_multiply(0.8)) } else { (sev_color(i.sev), if chosen || r.hovered() { TEXT() } else { DIM() }) };
                        let x = rect.left() + 26.0;
                        paint(ui, pos2(x, cy), &i.cat, 13.5, c1, CAT_COL - 10.0);
                        paint_words(ui, pos2(x + CAT_COL, cy), &i.short, 13.5, c2, rect.right() - 12.0 - x - CAT_COL);
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            clicked = Some((g.key.clone(), Some((i.cat.clone(), i.detail.clone()))));
                        }
                    }
                }
            }
            // the heading of the severity being read stays at the top, until the next one pushes it out
            let at = view.ys.partition_point(|y| *y <= visible.top());
            let here = (0..at).rev().find(|n| matches!(view.rows[*n], Row::Section(..)));
            if let Some((n, Row::Section(sev, albums))) = here.map(|n| (n, &view.rows[n])) {
                if view.ys[n] < visible.top() {
                    let next = (n + 1..view.rows.len()).find(|m| matches!(view.rows[*m], Row::Section(..))).map(|m| view.ys[m]);
                    let y = next.map_or(visible.top(), |next| visible.top().min(next + SECTION_GAP - SECTION_H));
                    let rect = Rect::from_min_size(pos2(left, snap(top + y)), vec2(width, SECTION_H));
                    // it covers the rows passing under it, and takes their clicks: a click goes to where it starts
                    let r = ui.interact(rect, ui.scope_id().with(("pinned", *sev)), Sense::click());
                    ui.painter().rect_filled(rect.expand2(vec2(0.0, 1.0)), 0, CARD());
                    section_head(ui, rect, *sev, *albums);
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        ui.scroll_to_rect(Rect::from_min_size(pos2(left, top + view.ys[n]), vec2(width, visible.height())), Some(Align::Min));
                    }
                }
            }
        });
        self.first_row = first_row;
        if let Some(c) = clicked {
            // clicking what is already open closes it
            self.sel = if self.sel.as_ref() == Some(&c) { None } else { Some(c) };
        }
    }

    /// The cover wall: one tile per album with problems, worst first under the list's headings. Only the
    /// rows on or near the screen are built.
    fn wall(&mut self, ui: &mut Ui) {
        if self.view.tiles.is_empty() {
            theme::empty(ui, self.nothing_words());
            return;
        }
        let mut area = ScrollArea::vertical().id_salt("health-wall").auto_shrink([false, false]);
        if std::mem::take(&mut self.to_top) {
            area = area.vertical_scroll_offset(0.0);
        }
        let scroll_to = self.scroll_to.take();
        let (view, report, sel, wall_px) = (&self.view, &self.report, &self.sel, &mut self.wall_px);
        let mut clicked: Option<String> = None;
        let (mut cols, mut first_tile) = (1, None);
        area.show_viewport(ui, |ui, visible| {
            let ppp = ui.ctx().pixels_per_point();
            let (edge, top, width) = (ui.max_rect().left(), ui.max_rect().top(), ui.available_width());
            let left = edge + WALL_LEFT;
            let avail = width - WALL_LEFT - 12.0; // 12: room for the scroll bar
            let wall = Wall::new(&view.tiles, avail, ppp);
            let (side, pitch) = (wall.side, wall.pitch);
            cols = wall.cols;
            ui.allocate_rect(Rect::from_min_size(pos2(edge, top), vec2(width, wall.height)), Sense::hover());
            if let Some((key, center)) = &scroll_to {
                if let Some(n) = view.tiles.iter().position(|t| report.groups[t.group].key == *key) {
                    // with room above for the heading that stays at the top
                    let at = wall.tile_min(n);
                    let rect = Rect::from_min_max(pos2(left + at.x, top + at.y - SECTION_H - 10.0), pos2(left + at.x + side, top + at.y + side + TILE_TEXT + 8.0));
                    ui.scroll_to_rect(rect, center.then_some(Align::Center));
                }
            }
            // the covers are decoded at the size they are drawn, once the window has stopped being resized
            let px = wall_px.px(ui.ctx(), avail, (side * ppp).round() as u32);
            let was = wall_px.was();
            let head = |y: f32| Rect::from_min_size(pos2(edge, (((top + y) * ppp).round()) / ppp), vec2(width - 12.0, SECTION_H));
            for sec in &wall.secs {
                if sec.top > visible.bottom() + pitch {
                    break;
                }
                if sec.top + SECTION_H > visible.top() && sec.top < visible.bottom() {
                    section_head(ui, head(sec.top), sec.sev, sec.count);
                }
                // the rows on screen, and one more each way so their covers are ready
                let rows = sec.count.div_ceil(wall.cols);
                let from = (((visible.top() - sec.tiles_top()) / pitch).floor() - 1.0).max(0.0) as usize;
                let to = ((((visible.bottom() - sec.tiles_top()) / pitch).ceil() + 1.0).max(0.0) as usize).min(rows);
                for row in from..to {
                    for col in 0..wall.cols {
                        let n = sec.start + row * wall.cols + col;
                        if n >= sec.start + sec.count {
                            break;
                        }
                        let t = &view.tiles[n];
                        let g = &report.groups[t.group];
                        let at = wall.tile_min(n);
                        if at.y + side + TILE_TEXT < visible.top() || at.y > visible.bottom() {
                            covers::for_folder(ui.ctx(), &g.folder, px); // about to scroll into view
                            continue;
                        }
                        first_tile.get_or_insert(n);
                        let chosen = matches!(sel, Some((k, _)) if *k == g.key);
                        let r = wall_tile(ui, report, g, t, pos2(left + at.x, top + at.y), (side, px, was), chosen, debug_hover(n, g));
                        if r.clicked() {
                            clicked = Some(g.key.clone());
                        }
                    }
                }
            }
            // the heading of the severity being looked at stays at the top, until the next one pushes it out
            if let Some(n) = wall.secs.iter().rposition(|s| s.top < visible.top()) {
                let sec = &wall.secs[n];
                let y = wall.secs.get(n + 1).map_or(visible.top(), |next| visible.top().min(next.top - SECTION_H));
                let rect = head(y);
                // it covers the covers passing under it, and takes their clicks: a click goes to where it starts
                let r = ui.interact(rect, ui.scope_id().with(("pinned", sec.sev)), Sense::click());
                ui.painter().rect_filled(Rect::from_min_max(pos2(edge, rect.top() - 1.0), pos2(edge + width, rect.bottom() + 1.0)), 0, CARD());
                section_head(ui, rect, sec.sev, sec.count);
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    ui.scroll_to_rect(Rect::from_min_size(pos2(edge, top + sec.top), vec2(width, visible.height())), Some(Align::Min));
                }
            }
        });
        self.wall_cols = cols;
        self.first_tile = first_tile.unwrap_or(0);
        if let Some(key) = clicked {
            // clicking the cover that is open closes it
            self.sel = if self.sel.as_ref().is_some_and(|(k, _)| *k == key) { None } else { Some((key, None)) };
        }
    }

    /// One album's problems: its cover and name, in the cover's own color, then each problem.
    fn details(&mut self, ui: &mut Ui, cfg: &Config, card: Rect) {
        let Some((key, chosen)) = self.sel.clone() else { return };
        let Some(g) = self.report.groups.iter().find(|g| g.key == key) else { return };
        let report = &self.report;
        // a finding that is gone (its repair has just been done) leaves the album's other findings on show,
        // not an empty card
        let chosen = chosen.filter(|(cat, _)| g.issues.iter().any(|&i| report.issues[i].cat == *cat));
        let mut close = false;
        let mut pick: Option<Selection> = None;
        // a problem whose Ignore word was clicked: (category, ignore it or stop)
        let mut set_aside: Option<(String, bool)> = None;
        // "Replace this album" was clicked: (folder, artist, album, what is wrong, the words to search for)
        let mut replace: Option<(String, String, String, String, String, Vec<String>)> = None;
        // a problem whose repair was clicked
        let mut fix: Option<(crate::repair::Ask, String)> = None;
        // loose tracks: the full album was asked for, or the ones an album already holds are to go to the backups
        let mut get_album: Option<LooseAlbum> = None;
        let mut to_backups: Option<(LooseAlbum, String)> = None;
        let (loose_dir, loose_artist) = (g.path.clone(), g.artist.clone());
        let fixable = self.report.own && !self.fixing && self.check.is_none();
        let asked = |i: &Issue, g: &Group, choice: String| (crate::repair::Ask { choice, fix: i.fix.clone(), cat: i.cat.clone(), path: g.path.clone(), files: i.files.clone(), artist: g.artist.clone(), album: g.album.clone(), data: i.data.clone() }, format!("{}: {} {}", i.cat, g.artist, g.album).trim().to_string());
        // the sidebar takes its color from the cover, once that is known (it comes in softly)
        let accent = covers::accent(&g.folder);
        let tint = ui.ctx().animate_bool_with_time(Id::unique("health-tint"), accent.is_some(), 0.25);
        if let Some(a) = accent {
            wash(ui, Rect::from_min_size(card.min, vec2(card.width(), 180.0_f32.min(card.height()))), a, tint);
        }
        let rule = accent.map_or(BORDER(), |a| mix(BORDER(), mix(a, CARD(), 0.45), tint));
        let title_color = accent.map_or(TEXT(), |a| mix(TEXT(), mix(a, Color32::WHITE, 0.4), tint));

        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            close = theme::close_mark(ui, "Close", "Close").clicked();
        });
        ui.add_space(4.0);
        let shown: Vec<&Issue> = g.issues.iter().map(|&i| &report.issues[i]).filter(|i| self.show_ignored || !i.ignored).collect();
        let room = ui.available_height();
        ScrollArea::vertical().id_salt(("health-details-scroll", &key)).auto_shrink([false, false]).show(ui, |ui| {
            let w = ui.available_width() - 12.0; // room for the scroll bar, beside the content instead of over it
            ui.set_width(w);
            let ppp = ui.ctx().pixels_per_point();
            // an album's cover is large, but leaves the first problem in sight (in steps, so that resizing the
            // window does not ask for a new size of picture at every pixel). A folder that is not one album
            // has the very picture its tile has: asked for larger, a folder of many albums can give another.
            let fit = ((room * 0.5 / 20.0).floor() * 20.0).clamp(120.0, SIDE_COVER).min(w);
            let (side, px) = if g.in_library { (fit, (fit * ppp).round() as u32) } else { (fit.min(TILE), (TILE * ppp).round() as u32) };
            let (rect, _) = ui.allocate_exact_size(vec2(w, side), Sense::hover());
            let min = rect.min;
            let cover = Rect::from_min_size(pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp), vec2(side, side));
            covers::paint(ui, covers::for_folder(ui.ctx(), &g.folder, px), cover, 8);
            ui.add_space(12.0);
            let (title, under) = names(g);
            ui.add(Label::new(RichText::new(title).size(20.0).strong().color(title_color)).wrap());
            if !under.is_empty() {
                ui.add(Label::new(RichText::new(under).size(15.0).color(DIM())).wrap());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 18.0;
                if theme::quiet(ui, "Open folder", 13.0, true).clicked() {
                    open_folder(g.path.clone());
                }
                if g.in_library && theme::quiet(ui, "Show in Library", 13.0, true).clicked() {
                    crate::links::go(crate::links::Go::LibraryAlbum { artist: g.artist.clone(), album: g.album.clone() });
                }
            });
            ui.add_space(4.0);
            ui.add(Label::new(RichText::new(&g.path).size(12.0).color(FAINT())).wrap());
            ui.add_space(12.0);
            let line = |ui: &mut Ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, rule));
            };
            line(ui);
            ui.add_space(4.0);
            match &chosen {
                Some((cat, detail)) => {
                    // the chosen problem in full, then the album's other problems as lines to click
                    // (a finding whose sentence has changed since it was chosen, after a repair or a new look at
                    // the folder, is still the one of its kind)
                    let all = || g.issues.iter().map(|&i| &report.issues[i]);
                    let Some(i) = all().find(|i| i.cat == *cat && i.detail == *detail).or_else(|| all().find(|i| i.cat == *cat)) else { return };
                    ui.add_space(10.0);
                    match issue_block(ui, i, &g.path, true, fixable, g.in_library && g.folder.as_os_str() == std::ffi::OsStr::new(&g.path)) {
                        Click::Ignore => set_aside = Some((i.cat.clone(), !i.ignored)),
                        Click::Fix(choice) => fix = Some(asked(i, g, choice)),
                        Click::Replace(search, only) => replace = Some((g.path.clone(), g.artist.clone(), g.album.clone(), i.cat.clone(), search, if only { i.files.clone() } else { vec![] })),
                        Click::GetAlbum(n) => get_album = loose_albums(i).into_iter().nth(n),
                        Click::ToBackups(n) => to_backups = loose_albums(i).into_iter().nth(n).map(|a| (a, i.cat.clone())),
                        Click::None => {}
                    }
                    let others: Vec<&&Issue> = shown.iter().filter(|o| !(o.cat == i.cat && o.detail == i.detail)).collect();
                    if !others.is_empty() {
                        section(ui, "Also here");
                        for o in others {
                            let r = ui
                                .horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    sev_mark(ui, o, 15.0);
                                    ui.add(Label::new(RichText::new(&o.cat).size(13.5).color(if o.ignored { FAINT() } else { TEXT() })).selectable(false).sense(Sense::click()))
                                })
                                .inner;
                            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                pick = Some((key.clone(), Some((o.cat.clone(), o.detail.clone()))));
                            }
                        }
                    }
                }
                None => {
                    // one search finds a better copy of the whole album, so it is offered once
                    let mut offered: Vec<&str> = vec![];
                    for (n, i) in shown.iter().enumerate() {
                        if n > 0 {
                            ui.add_space(6.0);
                            line(ui);
                        }
                        ui.add_space(10.0);
                        let search = i.data.get("search").map(|s| s.as_str()).unwrap_or("");
                        match issue_block(ui, i, &g.path, !offered.contains(&search), fixable, g.in_library && g.folder.as_os_str() == std::ffi::OsStr::new(&g.path)) {
                            Click::Ignore => set_aside = Some((i.cat.clone(), !i.ignored)),
                            Click::Fix(choice) => fix = Some(asked(i, g, choice)),
                            Click::Replace(search, only) => replace = Some((g.path.clone(), g.artist.clone(), g.album.clone(), i.cat.clone(), search, if only { i.files.clone() } else { vec![] })),
                        Click::GetAlbum(n) => get_album = loose_albums(i).into_iter().nth(n),
                        Click::ToBackups(n) => to_backups = loose_albums(i).into_iter().nth(n).map(|a| (a, i.cat.clone())),
                            Click::None => {}
                        }
                        if i.fix == "redownload" && !search.is_empty() {
                            offered.push(search);
                        }
                        ui.add_space(8.0);
                    }
                }
            }
            ui.add_space(12.0);
        });
        if close {
            self.sel = None;
        } else if pick.is_some() {
            self.sel = pick;
        }
        // debug: `MUSIC_RECS_HEALTH_FIX=<category>` starts that repair once, for the album whose details are open
        static FIX_ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(cat) = std::env::var("MUSIC_RECS_HEALTH_FIX").ok().filter(|_| fixable && fix.is_none()) {
            // (an album can be on the page more than once: its folder, and a file or the artist folder a finding points at)
            let found = report.groups.iter().filter(|o| o.artist == g.artist && o.album == g.album).find_map(|o| o.issues.iter().map(|&i| &report.issues[i]).find(|i| i.cat == cat && crate::repair::can_fix(&i.fix)).map(|i| (i, o)));
            if let Some((i, o)) = found {
                if !FIX_ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    fix = Some(asked(i, o, std::env::var("MUSIC_RECS_HEALTH_CHOICE").unwrap_or_default()));
                }
            }
        }
        if let Some((cat, on)) = set_aside {
            self.set_ignored(&key, &cat, on);
        }
        if let Some((folder, artist, album, why, search, files)) = replace {
            // written down first, so the download is known as this album's replacement when it arrives
            match crate::replace::add(&crate::replace::file(), Path::new(&folder), &artist, &album, &why, &files) {
                Ok(()) => crate::slskd::ask_for(&search),
                Err(e) => self.note = Some(e),
            }
        }
        if let Some((ask, title)) = fix {
            let ctx = ui.ctx().clone();
            self.start_fix(&ctx, cfg, ask, title);
        }
        if let Some(a) = get_album {
            // written down first, so the loose files are known as this album's when it arrives
            // (files in the library folder itself are waited for under the artist their tags name)
            let (dir, artist) = if a.artist_dir.is_empty() { (loose_dir.clone(), loose_artist.clone()) } else { (a.artist_dir.clone(), a.artist.clone()) };
            match crate::loose::add(&crate::loose::file(), Path::new(&dir), &artist, &a.album, &a.files) {
                Ok(()) => crate::slskd::ask_for(&a.search),
                Err(e) => self.note = Some(e),
            }
        }
        if let Some((a, cat)) = to_backups {
            let ctx = ui.ctx().clone();
            self.start_settle(&ctx, a, cat, loose_dir);
        }
    }

    /// Moves the loose tracks an album in the library already holds into the undo backups (see
    /// `crate::loose::settle`), in the background, and takes them out of the finding.
    fn start_settle(&mut self, ctx: &egui::Context, album: LooseAlbum, cat: String, path: String) {
        // (one change to the library at a time, none while a check is reading it, and none where the
        // library is only looked at)
        if self.fixing || self.check.is_some() || look_only() {
            return;
        }
        self.fixing = true;
        self.note = None;
        let (tx, ctx, library, report) = (self.tx.clone(), ctx.clone(), PathBuf::from(&self.library), self.own_file.clone());
        // (where lossless downloads are kept, a loose lossless track leaves only for a lossless copy of its song)
        let keep_lossless = KEEP_LOSSLESS.load(std::sync::atomic::Ordering::Relaxed);
        std::thread::spawn(move || {
            let done = crate::importer::tools::find_tool("ffprobe").and_then(|ffprobe| crate::loose::settle_keeping(&ffprobe, &library, &crate::repair::backups_root(&library), Path::new(&album.folder), &album.held, keep_lossless));
            let done = done.map(|d| {
                drop_loose_files(&report, &cat, &path, &d.moved);
                crate::repair::Done { batch: d.batch, changed: d.moved.len(), problems: d.stayed }
            });
            let _ = tx.send(Msg::Fixed(done));
            ctx.request_repaint();
        });
    }

    /// Sets a kind of problem aside for one folder, or brings it back. The list of what is ignored is a text
    /// file in the app's data folder (one "category|folder" per line). With nothing left to show for
    /// the album, its details close.
    fn set_ignored(&mut self, group: &str, cat: &str, on: bool) {
        let Some(gi) = self.report.groups.iter().position(|g| g.key == group) else { return };
        let path = self.report.groups[gi].path.to_lowercase();
        if let Err(e) = write_ignore(&self.ignore_file, &format!("{cat}|{path}"), on) {
            self.note = Some(crate::util::plain_err(format!("That could not be saved: {e}")));
            return;
        }
        let same: Vec<usize> = self.report.groups.iter().filter(|g| g.path.to_lowercase() == path).flat_map(|g| g.issues.iter().copied()).collect();
        for i in same {
            if self.report.issues[i].cat == cat {
                self.report.issues[i].ignored = on;
            }
        }
        self.open = [0; 4];
        for i in self.report.issues.iter().filter(|i| !i.ignored && counted(i)) {
            self.open[0] += 1;
            self.open[i.sev as usize] += 1;
        }
        if !self.show_ignored && self.report.groups[gi].issues.iter().all(|&i| self.report.issues[i].ignored) {
            self.sel = None;
        }
        self.publish_marks();
        self.dirty = true;
    }
}

/// (album folder, worst open problem there) for every folder with a problem that is not ignored.
/// A library without lyrics has "No lyrics" on nearly every album: thousands of lines about something that
/// is not wrong with the music. So that finding is not one of the open issues: it is not in the numbers at
/// the top of the page or on Home, puts no mark on an album in the Library, and is not listed among
/// everything else. It has its own line among the categories (with "Fix all"), and an album's own panel
/// still names it.
const QUIET_CAT: &str = "No lyrics";

fn counted(i: &Issue) -> bool {
    i.cat != QUIET_CAT
}

fn marks(r: &Report) -> Vec<(PathBuf, u8)> {
    let worst = |g: &Group| g.issues.iter().map(|&i| &r.issues[i]).filter(|i| !i.ignored && counted(i)).map(|i| i.sev).min();
    r.groups.iter().filter_map(|g| Some((g.folder.clone(), worst(g)?))).collect()
}

// ------------------------------------------------------------------ the cover wall

/// The widest the cover at the top of the details gets.
const SIDE_COVER: f32 = 400.0;

/// One severity's part of the wall: its heading at `top`, then `count` tiles starting with tile `start`.
struct Sec {
    sev: u8,
    top: f32,
    start: usize,
    count: usize,
}

impl Sec {
    fn tiles_top(&self) -> f32 {
        self.top + SECTION_H + 8.0
    }
}

/// Where everything on the wall goes, for the width there is.
struct Wall {
    cols: usize,
    /// a cover's side: `TILE`, grown so that the row reaches the right edge
    side: f32,
    gap: f32,
    /// from the top of one row of tiles to the top of the next
    pitch: f32,
    secs: Vec<Sec>,
    height: f32,
}

impl Wall {
    /// `ppp`: the screen's pixels per point (a cover's side is a whole number of pixels).
    fn new(tiles: &[Tile], avail: f32, ppp: f32) -> Self {
        let covers::Tiles { cols, side, gap } = columns(avail, ppp);
        let pitch = side + TILE_TEXT + ROW_GAP;
        let (mut secs, mut y) = (vec![], 0.0);
        for (start, count) in runs(tiles) {
            secs.push(Sec { sev: tiles[start].sev, top: y, start, count });
            y += SECTION_H + 8.0 + count.div_ceil(cols) as f32 * pitch + 4.0;
        }
        Wall { cols, side, gap, pitch, secs, height: y + 14.0 }
    }

    /// The top left corner of tile `n`, from the wall's own top left corner.
    fn tile_min(&self, n: usize) -> egui::Vec2 {
        let Some(sec) = self.secs.iter().rev().find(|s| s.start <= n) else { return vec2(0.0, 0.0) };
        let k = n - sec.start;
        vec2((k % self.cols) as f32 * (self.side + self.gap), sec.tiles_top() + (k / self.cols) as f32 * self.pitch)
    }
}

/// The tiles under each heading, as (first tile, how many): the tiles are in order of severity.
fn runs(tiles: &[Tile]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = vec![];
    for (n, t) in tiles.iter().enumerate() {
        match out.last_mut() {
            Some((start, count)) if tiles[*start].sev == t.sev => *count += 1,
            _ => out.push((n, 1)),
        }
    }
    out
}

/// The tile an arrow key leads to from tile `at`. Left and Right follow the reading order; Up and Down
/// keep the column, also from one heading's tiles into the next heading's.
fn wall_move(runs: &[(usize, usize)], cols: usize, at: usize, dx: i32, dy: i32) -> usize {
    let total: usize = runs.iter().map(|r| r.1).sum();
    let Some(s) = runs.iter().position(|(start, count)| at >= *start && at < start + count) else { return at };
    let (start, count) = runs[s];
    let (row, col) = ((at - start) / cols, (at - start) % cols);
    if dy > 0 {
        if (row + 1) * cols < count {
            return (at + cols).min(start + count - 1);
        }
        return runs.get(s + 1).map_or(at, |(next, n)| next + col.min(n - 1));
    }
    if dy < 0 {
        if row > 0 {
            return at - cols;
        }
        return match s.checked_sub(1).map(|p| runs[p]) {
            Some((prev, n)) => prev + ((n - 1) / cols * cols + col).min(n - 1),
            None => at,
        };
    }
    if dx > 0 {
        return (at + 1).min(total.saturating_sub(1));
    }
    if dx < 0 {
        return at.saturating_sub(1);
    }
    at
}

/// How many tiles fit across `avail` and the space between them, with the covers grown from `TILE` so
/// that the row reaches the right edge (the Library page's rule, in `covers::fill_row`): as many as fit at
/// `TILE` with `GAP_MIN` between them, the gap never more than `GAP_MAX`. In the smallest window with the
/// details open, two tiles a little closer beat one tile and a blank half.
fn columns(avail: f32, ppp: f32) -> covers::Tiles {
    covers::fill_row(avail, ppp, TILE, GAP_MIN, GAP_MAX)
}

/// What to call a group: (the bright name, the quiet one). An album goes by its title, without the year
/// its folder carries; anything else (loose tracks, a whole artist) goes by the artist.
fn names(g: &Group) -> (&str, &str) {
    if g.in_library {
        (album_title(&g.album), &g.artist)
    } else {
        (&g.artist, g.album.trim_matches(['(', ')']))
    }
}

/// "[1993] Vs" and "Vs (1993)" are both "Vs".
fn album_title(name: &str) -> &str {
    let mut s = name.trim();
    let b = s.as_bytes();
    if b.len() > 6 && b[0] == b'[' && b[5] == b']' && b[1..5].iter().all(u8::is_ascii_digit) && !s[6..].trim().is_empty() {
        s = s[6..].trim_start();
    }
    let b = s.as_bytes();
    let n = b.len();
    if n > 6 && b[n - 1] == b')' && b[n - 6] == b'(' && b[n - 5..n - 1].iter().all(u8::is_ascii_digit) && !s[..n - 6].trim().is_empty() {
        s = s[..n - 6].trim_end();
    }
    s
}

/// Debug: `MUSIC_RECS_HEALTH_HOVER=3` (the wall's nth tile) or `=some words` (part of "artist album")
/// draws that tile as if the pointer were on it.
fn debug_hover(n: usize, g: &Group) -> bool {
    static HOVER: std::sync::LazyLock<Option<String>> = std::sync::LazyLock::new(|| std::env::var("MUSIC_RECS_HEALTH_HOVER").ok().map(|h| h.to_lowercase()));
    match HOVER.as_deref() {
        Some(h) => h.parse::<usize>().map_or_else(|_| g.hay.contains(h), |want| want == n),
        None => false,
    }
}

/// One tile of the wall: the cover with its severity as the edge, two lines under it, the short version of
/// what is wrong along the cover's bottom edge under the pointer, and a short menu on right-click.
/// `size`: the cover's side in points, the size its picture is asked for in pixels (the size it is drawn at,
/// except while the window is being resized), and the size asked for before that.
fn wall_tile(ui: &mut Ui, report: &Report, g: &Group, t: &Tile, min: Pos2, size: (f32, u32, u32), selected: bool, forced: bool) -> egui::Response {
    let ppp = ui.ctx().pixels_per_point();
    let (side, px, was) = size;
    let min = pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
    let cover = Rect::from_min_size(min, vec2(side, side));
    let rect = Rect::from_min_size(min, vec2(side, side + TILE_TEXT));
    let r = ui.interact(rect, ui.scope_id().with(("tile", t.group)), Sense::click());
    if crate::script::on() {
        crate::script::seen(&format!("album:{}", g.album), rect); // what a test script calls it, as in the list
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let lift = ui.ctx().animate_bool_with_time(r.id.with("lift"), r.hovered() || forced, 0.12);
    let back = rect.expand2(vec2(7.0, 7.0)).translate(vec2(0.0, -1.0));
    if selected {
        ui.painter().rect_filled(back, 10, lifted());
    }
    if lift > 0.0 {
        ui.painter().rect_filled(back, 10, CARD_HOVER().gamma_multiply(lift));
    }
    let pic = covers::for_folder(ui.ctx(), &g.folder, px);
    if was == px {
        covers::paint(ui, pic, cover, 6);
    } else {
        // its size has just changed: the old one, stretched, until the new one is decoded
        covers::paint_over(ui, pic, covers::folder_key(&g.folder), &[was], cover, 6);
    }
    if lift > 0.0 {
        let first = &report.issues[t.first];
        let text = if t.more > 0 { format!("{}  ·  +{} more", first.cat, t.more) } else { first.cat.clone() };
        let strip = Rect::from_min_max(pos2(cover.left(), cover.bottom() - 26.0), cover.max);
        ui.painter().rect_filled(strip, CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 }, Color32::from_black_alpha((200.0 * lift) as u8));
        paint(ui, pos2(strip.left() + 8.0, strip.center().y), &text, 11.5, Color32::WHITE.gamma_multiply(lift), strip.width() - 16.0);
    }
    // how serious it is: the cover's edge
    if let Some(color) = [RED(), WARN()].get(t.sev as usize - 1) {
        ui.painter().rect_stroke(cover, 6, Stroke::new(2.5, *color), egui::StrokeKind::Inside);
    }
    if selected {
        ui.painter().rect_stroke(cover.expand(2.0), 8, Stroke::new(2.0, ACCENT()), egui::StrokeKind::Outside);
    } else if lift > 0.0 {
        ui.painter().rect_stroke(cover, 6, Stroke::new(1.0, DIM().gamma_multiply(lift)), egui::StrokeKind::Outside);
    }
    // two lines: the name bright, the quieter one under it
    let (name, more) = names(g);
    name_line(ui, pos2(min.x + 1.0, cover.bottom() + 7.0), side - 2.0, name, more);

    r.context_menu(|ui| {
        if ui.button("Open folder").clicked() {
            open_folder(g.path.clone());
            ui.close();
        }
        if g.in_library && ui.button("Show in Library").clicked() {
            crate::links::go(crate::links::Go::LibraryAlbum { artist: g.artist.clone(), album: g.album.clone() });
            ui.close();
        }
        // any of its open problems that a better copy would fix
        let search = g.issues.iter().map(|&i| &report.issues[i]).filter(|i| !i.ignored && i.fix == "redownload").find_map(|i| i.data.get("search").filter(|s| !s.is_empty()));
        if let Some(s) = search {
            if ui.button("Find a better copy on Soulseek").clicked() {
                crate::slskd::ask_for(s);
                ui.close();
            }
        }
    });
    r
}

/// The two lines under a cover (as on the Library page): the name bright, and under it a quieter word or
/// two. Each has the whole width and is cut off with "…" on its own if it is too long.
fn name_line(ui: &Ui, pos: Pos2, width: f32, name: &str, more: &str) {
    let line = |text: &str, size: f32, color: Color32, y: f32| {
        if text.is_empty() || width <= 4.0 {
            return;
        }
        let mut job = LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
        job.wrap = TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(pos2(pos.x, y), galley, color);
    };
    line(name, 13.5, TEXT(), pos.y);
    line(more, 12.0, DIM(), pos.y + 17.0);
}

/// The cover's color behind the top of the details: strongest at the top edge, gone at the bottom of `rect`.
fn wash(ui: &Ui, rect: Rect, color: Color32, strength: f32) {
    const ROUND: f32 = theme::CARD_ROUND as f32; // the card's corners
    if strength <= 0.0 || rect.height() <= ROUND {
        return;
    }
    let top = color.gamma_multiply(0.24 * strength);
    let cap = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + ROUND));
    ui.painter().rect_filled(cap, CornerRadius { nw: ROUND as u8, ne: ROUND as u8, sw: 0, se: 0 }, top);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(cap.left_bottom(), top);
    mesh.colored_vertex(cap.right_bottom(), top);
    mesh.colored_vertex(rect.right_bottom(), Color32::TRANSPARENT);
    mesh.colored_vertex(rect.left_bottom(), Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
}

/// `a` at 0, `b` at 1, in between in between.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// A problem's severity as a small dot in its color, at the start of a line `height` tall.
fn sev_mark(ui: &mut Ui, i: &Issue, height: f32) {
    let (rect, r) = ui.allocate_exact_size(vec2(8.0, height), Sense::hover());
    let color = match i.sev {
        1 => RED(),
        2 => WARN(),
        _ => FAINT(),
    };
    ui.painter().circle_filled(rect.center(), 4.0, if i.ignored { color.gamma_multiply(0.45) } else { color });
    r.on_hover_text(if i.ignored { format!("{}, ignored", sev_name(i.sev)) } else { sev_name(i.sev).to_string() });
}

/// One line in the list of categories: a name, and how many. True when clicked.
fn cat_row(ui: &mut Ui, name: &str, count: usize, color: Color32, indent: f32, on: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
    if crate::script::on() {
        crate::script::seen(&format!("cat:{name}"), rect);
    }
    if !ui.is_rect_visible(rect) {
        return false;
    }
    if r.hovered() || on {
        ui.painter().rect_filled(rect, 6, if on { CARD_HOVER() } else { lifted() });
    }
    if on {
        ui.painter().vline(rect.left() + 1.0, rect.shrink2(vec2(0.0, 5.0)).y_range(), Stroke::new(2.0, ACCENT()));
    }
    let cy = rect.center().y;
    let n = num(count as u64);
    let nw = paint_right(ui, pos2(rect.right() - 8.0, cy), &n, 12.5, if on { DIM() } else { FAINT() });
    paint(ui, pos2(rect.left() + 10.0 + indent, cy), name, 13.5, color, rect.width() - 28.0 - indent - nw);
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// What is behind a line of the list: lit while the pointer is on it (fading in and out), and lit with a
/// coral edge while its details are open.
fn row_back(ui: &Ui, rect: Rect, id: Id, hovered: bool, chosen: bool) {
    let lit = ui.ctx().animate_bool_with_time(id.with("lit"), hovered || chosen, 0.12);
    if lit > 0.0 {
        ui.painter().rect_filled(rect, 6, CARD_HOVER().gamma_multiply(lit));
    }
    if chosen {
        ui.painter().vline(rect.left() + 1.0, rect.shrink2(vec2(0.0, 5.0)).y_range(), Stroke::new(2.0, ACCENT()));
    }
}

/// The heading where a severity starts in the list or on the wall: its name large, in its color and in
/// the light cut of the font, with how many albums quiet beside it on the same line of writing.
fn section_head(ui: &Ui, rect: Rect, sev: u8, albums: usize) {
    let name = ui.painter().layout_no_wrap(sev_name(sev).to_string(), theme::display(22.0), sev_color(sev));
    let count = ui.painter().layout_no_wrap(format!("{} {}", num(albums as u64), if albums == 1 { "album" } else { "albums" }), FontId::proportional(12.5), FAINT());
    let at = pos2(rect.left() + 10.0, (rect.bottom() - 7.0 - name.size().y).round());
    let beside = pos2(at.x + name.size().x + 10.0, at.y + theme::baseline(&name) - theme::baseline(&count));
    ui.painter().galley(at, name, sev_color(sev));
    if beside.x + count.size().x <= rect.right() {
        ui.painter().galley(beside, count, FAINT());
    }
}

/// What is lit on a card (the album that is open, the line under the pointer): a step lighter than the card.
fn lifted() -> Color32 {
    theme::blend(CARD(), CARD_HOVER(), 0.5)
}

/// A small quiet heading over a part of an album's card, its letters set a little apart.
fn section(ui: &mut Ui, text: &str) {
    ui.add_space(14.0);
    ui.add(Label::new(RichText::new(text.to_uppercase()).size(11.0).extra_letter_spacing(1.4).color(DIM())).wrap());
    ui.add_space(4.0);
}

/// One thin bar, split by severity in proportion to the counts. Returns the severity the pointer is on,
/// and the one that was clicked.
fn sev_bar(ui: &mut Ui, counts: [usize; 3], active: Option<u8>, lit: Option<u8>) -> (Option<u8>, Option<u8>) {
    const GAP: f32 = 3.0;
    const LEAST: f32 = 10.0; // a severity with only a few issues can still be seen and clicked
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 12.0), Sense::hover());
    let total: usize = counts.iter().sum();
    let parts = counts.iter().filter(|n| **n > 0).count();
    if parts == 0 {
        return (None, None);
    }
    let spare = (rect.width() - GAP * (parts as f32 - 1.0) - LEAST * parts as f32).max(0.0);
    let (mut hovered, mut clicked) = (None, None);
    let mut x = rect.left();
    for s in 1..=3u8 {
        let n = counts[(s - 1) as usize];
        if n == 0 {
            continue;
        }
        let w = LEAST + spare * n as f32 / total as f32;
        let part = Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
        x += w + GAP;
        let r = ui.interact(part, ui.scope_id().with(("bar", s)), Sense::click());
        let hot = r.hovered() || lit == Some(s);
        if r.hovered() {
            hovered = Some(s);
        }
        let color = [RED(), WARN(), FAINT()][(s - 1) as usize];
        // the others step back while one severity is being looked at
        let color = if hot || active.is_none_or(|a| a == s) { color } else { color.gamma_multiply(0.3) };
        let h = 5.0 + 3.0 * ui.ctx().animate_bool_with_time(r.id.with("tall"), hot, 0.1);
        ui.painter().rect_filled(Rect::from_center_size(part.center(), vec2(w, h)), 3, color);
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            clicked = Some(s);
        }
    }
    (hovered, clicked)
}

/// One line of text that ends at a whole word, with "…", when it does not fit. `at` is its left edge and
/// vertical middle.
fn paint_words(ui: &Ui, at: egui::Pos2, text: &str, size: f32, color: Color32, max_width: f32) {
    if max_width <= 4.0 {
        return;
    }
    let lay = |s: String| ui.painter().layout_no_wrap(s, FontId::proportional(size), color);
    let mut g = lay(text.to_string());
    if g.size().x > max_width {
        // the longest run of whole words that fits with the "…" after it
        let ends: Vec<usize> = text.char_indices().filter(|(_, c)| *c == ' ').map(|(i, _)| i).collect();
        let (mut lo, mut hi, mut best) = (0, ends.len(), None);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cut = lay(format!("{}…", text[..ends[mid]].trim_end_matches([' ', ',', ';', ':', '-', '(', '.'])));
            if cut.size().x <= max_width {
                best = Some(cut);
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        match best {
            Some(cut) => g = cut,
            None => {
                paint(ui, at, text, size, color, max_width);
                return;
            }
        }
    }
    let h = g.size().y;
    ui.painter().galley(pos2(at.x, at.y - h / 2.0), g, color);
}

/// The first clause of a problem's sentence, for the list (the details have all of it). A sentence that is
/// short already is kept whole.
fn first_clause(sentence: &str) -> String {
    const LEAST: usize = 24; // a clause shorter than this says too little ("16 Cashin'.mp3")
    let s = one_line(sentence);
    let s = s.trim();
    if s.chars().count() <= 64 {
        return s.to_string();
    }
    // closing remarks in brackets go first: "... 17 of 17 (the tags say 17 tracks) (couldn't confirm ...)"
    let mut body = s;
    while body.ends_with(')') {
        let mut depth = 0;
        let mut open = None;
        for (i, c) in body.char_indices().rev() {
            match c {
                ')' => depth += 1,
                '(' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match open {
            Some(i) if i >= LEAST && body[..i].ends_with(' ') => body = body[..i].trim_end(),
            _ => break,
        }
    }
    // a sentence that starts with a file name is not cut inside that name
    let lower = body.to_ascii_lowercase();
    let name_end = [".mp3", ".flac", ".m4a", ".mp4", ".ogg", ".opus", ".wav", ".wma", ".aac"].iter().filter_map(|e| lower.find(e).map(|p| p + e.len())).min();
    let least = match (name_end, lower.find(": ")) {
        (Some(end), colon) if colon.is_none_or(|c| c >= end) => end.max(LEAST),
        _ => LEAST,
    };
    let (mut depth, mut before) = (0i32, ' ');
    let mut cut = body.len();
    for (i, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            _ => {}
        }
        if depth == 0 && i >= least {
            let rest = &body[i..];
            let numbered = before.is_ascii_digit(); // "01. Song", "10 - Song"
            if rest.starts_with(": ") || rest.starts_with("; ") || (rest.starts_with(". ") && !numbered) || (rest.starts_with(" - ") && !numbered) {
                cut = i;
                break;
            }
        }
        before = c;
    }
    body[..cut].trim_end_matches([' ', ',', ';', ':', '.']).to_string()
}

/// An album's name without years, brackets and punctuation, so "[1993] Vs" and "Vs. (1993)" are the same.
fn loose_name(name: &str) -> String {
    let mut out = String::new();
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '(' || c == '[' {
            // a year in brackets is dropped whole
            let inside: String = chars.clone().take_while(|c| *c != ')' && *c != ']').collect();
            if inside.len() == 4 && inside.chars().all(|c| c.is_ascii_digit()) {
                for _ in 0..5 {
                    chars.next();
                }
                continue;
            }
        }
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// The folder an issue is about: the folder itself, or the one the file is in.
fn folder_of(path: &str, on_disk: bool) -> PathBuf {
    let p = Path::new(path);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let named_like_a_file = ["mp3", "flac", "m4a", "mp4", "ogg", "opus", "wav", "wma", "aac", "aiff", "ape", "wv"].contains(&ext.as_str());
    let is_file = if on_disk && p.is_dir() {
        false
    } else {
        (on_disk && p.is_file()) || named_like_a_file
    };
    match p.parent() {
        Some(parent) if is_file => parent.to_path_buf(),
        _ => p.to_path_buf(),
    }
}

/// One line of text, cut short with "…" when it does not fit. `at` is its left edge and vertical middle.
/// Returns how wide it came out.
fn paint(ui: &Ui, at: egui::Pos2, text: &str, size: f32, color: Color32, max_width: f32) -> f32 {
    if max_width <= 4.0 {
        return 0.0;
    }
    let mut job = LayoutJob::simple_singleline(one_line(text), FontId::proportional(size), color);
    job.wrap = TextWrapping::truncate_at_width(max_width);
    let g = ui.painter().layout_job(job);
    let sz = g.size();
    ui.painter().galley(pos2(at.x, at.y - sz.y / 2.0), g, color);
    sz.x
}

/// Like [`paint`], but `at` is the right edge.
fn paint_right(ui: &Ui, at: egui::Pos2, text: &str, size: f32, color: Color32) -> f32 {
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(size), color);
    let sz = g.size();
    ui.painter().galley(pos2(at.x - sz.x, at.y - sz.y / 2.0), g, color);
    sz.x
}

fn one_line(s: &str) -> String {
    if s.contains(['\n', '\r', '\t']) {
        s.replace(['\n', '\r', '\t'], " ")
    } else {
        s.to_string()
    }
}

/// Everything the report says about one problem. `offer` = show the search for a better copy here
/// (false when the same search is already offered by a problem above it).
/// One problem in full. Says which of its words was clicked: "Ignore" (or "Stop ignoring"), or the repair
/// (`fixable`: the page can start one now).
fn issue_block(ui: &mut Ui, i: &Issue, folder: &str, offer: bool, fixable: bool, replaceable: bool) -> Click {
    // "Look only, change nothing": the findings are listed as ever, and nothing that would change the
    // library can be started from them
    let looking = look_only();
    let replaceable = replaceable && !looking;
    let dim = i.ignored;
    let mut click = Click::None;
    // how serious (the dot), what (the heading), then the sentence; at the right, the word that sets it aside
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        sev_mark(ui, i, 20.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::quiet(ui, if dim { "Stop ignoring" } else { "Ignore" }, 12.5, true).clicked() {
                click = Click::Ignore;
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(Label::new(RichText::new(&i.cat).size(15.0).color(if dim { DIM() } else { TEXT() })).truncate());
            });
        });
    });
    if dim {
        ui.label(RichText::new("Ignored").size(11.0).color(FAINT()));
    }
    ui.add_space(4.0);
    ui.add(Label::new(RichText::new(&i.detail).size(13.5).color(if dim { DIM() } else { TEXT() })).wrap());

    if i.fix == "redownload" {
        match i.data.get("search").filter(|s| !s.is_empty()) {
            Some(_) if !offer => {} // the search offered above finds this one too
            Some(s) => {
                ui.add_space(10.0);
                // an album gets "Replace this album": the download that arrives takes this folder's place once
                // it has passed its own check. A single file or a loose track is only searched for.
                if replaceable {
                    if theme::main_action(ui, "Replace this album", true).clicked() {
                        click = Click::Replace(s.clone(), false);
                    }
                    // the finding names its tracks: those alone can be swapped, the rest of the album stays
                    if !i.files.is_empty() {
                        let words = if i.files.len() == 1 { "Replace only this track".to_string() } else { format!("Replace only these {} tracks", i.files.len()) };
                        if theme::quiet(ui, &words, 14.0, true).clicked() {
                            click = Click::Replace(s.clone(), true);
                        }
                    }
                    ui.add(Label::new(RichText::new(format!("Searches Soulseek for \"{s}\". Your copy stays until a new one has passed its own check, and then goes to the undo backups.")).size(12.0).color(FAINT())).wrap());
                } else {
                    if theme::main_action(ui, "Find a better copy on Soulseek", true).clicked() {
                        crate::slskd::ask_for(s);
                    }
                    ui.add(Label::new(RichText::new(format!("Searches for \"{s}\". Nothing is replaced until you choose a copy.")).size(12.0).color(FAINT())).wrap());
                }
            }
            None => {
                ui.add_space(8.0);
                ui.add(Label::new(RichText::new("Needs a better copy: no repair can fix the files that are here.").size(13.0).color(DIM())).wrap());
            }
        }
    } else if !i.fix.is_empty() {
        // loose tracks: the album each one comes from, one block (and one control) per album
        for (n, a) in loose_albums(i).iter().enumerate() {
            let (many, names) = (a.files.len() > 1, a.files.iter().map(|f| leaf(f)).collect::<Vec<_>>().join(", "));
            let heading = if a.album.is_empty() { "No album known".to_string() } else if a.artist.is_empty() { a.album.clone() } else { format!("{}: {}", a.artist, a.album) };
            section(ui, &heading);
            ui.add(Label::new(RichText::new(names).size(13.0).color(TEXT())).wrap());
            let from = match a.source.as_str() {
                "" => "No album tag. The album is looked up by artist and title at the next check that is online.".to_string(),
                "not found" => "No album tag, and no album was found by artist and title.".to_string(),
                "tag" => String::new(),
                site => format!("No album tag: found on {site} by artist and title. "),
            };
            let held = a.held.len();
            let here = if a.album.is_empty() {
                String::new()
            } else if a.folder.is_empty() {
                "Not in your library.".to_string()
            } else if held == a.files.len() {
                format!("'{}' is in your library and already has {}.", leaf(&a.folder), if many { "these songs" } else { "this song" })
            } else if held == 0 {
                format!("'{}' is in your library, without {}.", leaf(&a.folder), if many { "these songs" } else { "this song" })
            } else {
                format!("'{}' is in your library and has {held} of these {} songs.", leaf(&a.folder), a.files.len())
            };
            ui.add(Label::new(RichText::new(format!("{from}{here}")).size(12.5).color(DIM())).wrap());
            if !a.album.is_empty() && held < a.files.len() {
                ui.add_space(4.0);
                if theme::quiet(ui, "Get the full album", 14.0, true).clicked() {
                    click = Click::GetAlbum(n);
                }
                let left = a.files.len() - held;
                ui.add(Label::new(RichText::new(format!("Searches Soulseek for \"{}\". Once the album has been imported and holds {}, the loose {} to the Recycle Bin.", a.search, if left > 1 { "these songs" } else { "this song" }, if left > 1 { "files go" } else { "file goes" })).size(12.0).color(FAINT())).wrap());
            }
            if held > 0 && fixable && !looking {
                ui.add_space(4.0);
                let words = if held == 1 { "Move this loose file to the backups".to_string() } else { format!("Move these {held} loose files to the backups") };
                if theme::quiet(ui, &words, 14.0, true).clicked() {
                    click = Click::ToBackups(n);
                }
                ui.add(Label::new(RichText::new(format!("The album already has {}. {} kept in the undo backups, not deleted, and Repair history can put {} back.", if held == 1 { "it" } else { "them" }, if held == 1 { "The file is" } else { "The files are" }, if held == 1 { "it" } else { "them" })).size(12.0).color(FAINT())).wrap());
            }
        }
        section(ui, "Suggested fix");
        if fixable && crate::repair::can_fix(&i.fix) {
            // the fix is the control: nothing is deleted by it, and Repair history puts it back
            // one word for each way it can be fixed; a single way stands out, a choice is two equal words
            let ways = crate::repair::choices(&i.fix, &i.cat, &i.data, i.files.len());
            let one = ways.len() == 1;
            // a repair that rewrites tags needs PowerShell 7: without it the word is quiet and says why,
            // rather than failing after the click
            let editor = crate::importer::tools::pwsh().is_some();
            let mut held_back = false;
            for (words, choice) in ways {
                let can = !looking && (editor || !crate::repair::needs_tag_editor(&i.fix, &choice));
                held_back |= !can && !looking;
                let r = if one { theme::main_action(ui, &words, can) } else { theme::quiet(ui, &words, 14.0, can) };
                if r.clicked() {
                    click = Click::Fix(choice);
                }
            }
            if looking {
                look_only_line(ui);
            } else if held_back {
                no_pwsh_line(ui);
            }
        } else {
            ui.add(Label::new(RichText::new(fix_words(&i.fix)).size(13.0).color(TEXT())).wrap());
        }
    }

    // extra facts, in plain words
    let d = |k: &str| i.data.get(k).map(|s| s.as_str()).filter(|s| !s.is_empty());
    // the lines of each track's lyrics that are not lyrics, as they stand in the file
    if let Some(Ok(Value::Array(list))) = d("lines").map(serde_json::from_str::<Value>) {
        section(ui, "Lines that are not lyrics");
        for row in &list {
            let lines: Vec<&str> = row["lines"].as_array().map(|a| a.iter().filter_map(|l| l.as_str()).collect()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.add(Label::new(RichText::new(row["file"].as_str().unwrap_or("")).size(13.0).color(TEXT())).truncate());
                if row["only"].as_bool() == Some(true) {
                    ui.label(RichText::new("nothing else there").size(12.0).color(FAINT()));
                }
            });
            let block = ui
                .horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for line in &lines {
                            ui.add(Label::new(RichText::new(*line).size(12.5).color(DIM())).wrap());
                        }
                    });
                })
                .response
                .rect;
            ui.painter().vline(block.left() + 1.0, block.y_range(), Stroke::new(2.0, WARN()));
            ui.add_space(6.0);
        }
    }
    if let Some(m) = d("missing") {
        let disc = match (d("disc"), d("discs")) {
            (Some(n), Some(of)) if of != "1" => format!(" on disc {n}"),
            _ => String::new(),
        };
        let many = m.contains(',') || m.contains(';');
        section(ui, if many { "Missing tracks" } else { "Missing track" });
        // the app's own check lists every disc in one fact ("1:3,5;2:7"); older reports had one disc per issue
        let words = if m.contains(':') {
            let several = m.contains(';') || d("discs").is_some_and(|n| n != "1");
            m.split(';').filter_map(|part| part.split_once(':')).map(|(disc, nums)| if several { format!("disc {disc}: {}", nums.replace(',', ", ")) } else { nums.replace(',', ", ") }).collect::<Vec<_>>().join("; ")
        } else {
            format!("{}{disc}", m.replace(',', ", "))
        };
        ui.add(Label::new(RichText::new(words).size(13.0).color(TEXT())).wrap());
    }
    if let (Some(tagged), Some(filed)) = (d("tagged"), d("folder")) {
        section(ui, "Artist");
        fact(ui, "In the tags", tagged);
        fact(ui, "Folder", filed);
    }
    if let Some(v) = d("values") {
        section(ui, "Names in the tags");
        for name in v.split('|').filter(|s| !s.is_empty()) {
            ui.add(Label::new(RichText::new(name).size(13.0).color(TEXT())).wrap());
        }
    }
    if let Some(size) = d("size") {
        section(ui, "Cover size");
        ui.label(RichText::new(size).size(13.0).color(TEXT()));
    }
    if let Some(f) = d("folders") {
        section(ui, "Folders");
        for p in f.split('|').filter(|s| !s.is_empty()) {
            ui.add(Label::new(RichText::new(short_path(p, folder)).size(13.0).color(TEXT())).wrap());
        }
    }
    if let Some(groups) = d("groups") {
        section(ui, "Copies");
        for p in groups.split(['|', ';', '\n']).filter(|s| !s.trim().is_empty()) {
            ui.add(Label::new(RichText::new(short_path(p.trim(), folder)).size(13.0).color(TEXT())).wrap());
        }
    }
    let changes = d("changes").map(changes).unwrap_or_default();
    if !changes.is_empty() {
        section(ui, "Corrections from the official track list");
        for (file, what) in &changes {
            ui.add(Label::new(RichText::new(short_path(file, folder)).size(13.0).color(TEXT())).wrap());
            ui.add(Label::new(RichText::new(what).size(12.5).color(DIM())).wrap());
            ui.add_space(4.0);
        }
    } else if !i.files.is_empty() && !(i.files.len() == 1 && i.files[0].eq_ignore_ascii_case(folder)) {
        section(ui, &if i.files.len() == 1 { "1 file".to_string() } else { format!("{} files", num(i.files.len() as u64)) });
        for f in &i.files {
            ui.add(Label::new(RichText::new(short_path(f, folder)).size(13.0).color(DIM())).wrap());
        }
    }
    click
}

fn fact(ui: &mut Ui, name: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(name).size(13.0).color(DIM()));
        ui.add(Label::new(RichText::new(value).size(13.0).color(TEXT())).wrap());
    });
}

/// The "changes" fact of a track list problem: rows of [file, new title, new track number, new disc].
fn changes(json: &str) -> Vec<(String, String)> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out = vec![];
    for row in rows {
        let Value::Array(cells) = row else { continue };
        let cell = |n: usize| cells.get(n).map(text_of).unwrap_or_default();
        let file = cell(0);
        if file.is_empty() {
            continue;
        }
        let mut bits = vec![];
        if !cell(1).is_empty() {
            bits.push(format!("title: {}", cell(1)));
        }
        if !cell(2).is_empty() {
            bits.push(format!("track {}", cell(2)));
        }
        if !cell(3).is_empty() {
            bits.push(format!("disc {}", cell(3)));
        }
        out.push((file, bits.join(", ")));
    }
    out
}

/// A file's path as seen from its album folder (the folder's own path is already on screen).
fn short_path(path: &str, folder: &str) -> String {
    let folder = folder.trim_end_matches(['\\', '/']);
    match path.get(..folder.len()) {
        Some(head) if !folder.is_empty() && head.eq_ignore_ascii_case(folder) && path[folder.len()..].starts_with(['\\', '/']) => path[folder.len() + 1..].to_string(),
        _ => path.to_string(),
    }
}

fn sev_name(s: u8) -> &'static str {
    match s {
        1 => "Serious",
        2 => "Worth fixing",
        _ => "Minor",
    }
}

fn sev_color(s: u8) -> Color32 {
    match s {
        1 => RED(),
        2 => WARN(),
        _ => DIM(),
    }
}

/// The tool's fix kinds, in plain words (what its repair would do).
fn fix_words(kind: &str) -> String {
    match kind {
        "" => "None offered. This one is for you to look at.",
        "album-artist" => "Set the same album artist on every track.",
        "album-name" => "Give every track the same album name.",
        "artist-mismatch" => "Make the artist in the tags and the artist folder agree.",
        "merge-artist" => "Merge the two artist folders into one.",
        "cover" => "Find cover art and add it to every track.",
        "dupe-tracks" => "Keep one copy of each song and remove the others.",
        "file-loose" => "Move the loose tracks into an album folder.",
        "garbled" => "Repair the scrambled characters in the tags.",
        "missing-tags" => "Fill in the missing tags.",
        "pick-album" => "Choose which copy of the album to keep.",
        "remove" => "Remove the file.",
        "tracklist" => "Renumber the tracks to match the official track list.",
        "tracklist-titles" => "Correct track numbers and titles to match the official track list.",
        "unwrap" => "Convert to plain MP3. The audio itself is not changed.",
        "lyrics-fill" => "Find the lyrics on LRCLIB and save them in the tracks.",
        "lyrics-junk" => "Replace the lyrics from LRCLIB, or cut the lines that are not lyrics.",
        "lyrics-unhide" => "Move the lyrics into the field players read.",
        "vbr-header" => "Add the missing length header. The audio itself is not changed.",
        other => return format!("This repair is called \"{}\".", other.replace('-', " ")),
    }
    .to_string()
}

/// Opens the album's folder (or the folder a file is in) without making the window wait for the disk.
fn open_folder(path: String) {
    std::thread::spawn(move || {
        let p = Path::new(&path);
        let target = if p.is_file() { p.parent().unwrap_or(p) } else { p };
        let _ = open::that(target);
    });
}

/// Debug: `MUSIC_RECS_HEALTH_SELECT=album:3` or `issue:3` opens the details of the list's nth album or
/// issue, so a `--screenshot` can show the sidebar.
fn debug_select(page: &mut HealthPage) {
    if page.sel.is_some() || page.state != State::Ready {
        return;
    }
    // `MUSIC_RECS_HEALTH_SHOW=Artist|Album` does what another page's "show this album's problems" does,
    // `MUSIC_RECS_HEALTH_FILTER=2` chooses a severity
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut first = false;
    ONCE.call_once(|| first = true);
    if first {
        if let Some((artist, album)) = std::env::var("MUSIC_RECS_HEALTH_SHOW").ok().as_deref().and_then(|v| v.split_once('|')) {
            page.show_album(artist, album);
            page.rebuild();
            return;
        }
        if let Some(s) = std::env::var("MUSIC_RECS_HEALTH_FILTER").ok().and_then(|v| v.parse::<u8>().ok()).filter(|s| (1..=3).contains(s)) {
            page.filter = Filter::Sev(s);
            page.rebuild();
        }
    }
    let Ok(v) = std::env::var("MUSIC_RECS_HEALTH_SELECT") else { return };
    for part in v.split(',') {
        if part == "ignored" {
            // also include the ignored issues: "ignored,issue:3"
            if !page.show_ignored {
                page.show_ignored = true;
                page.rebuild();
            }
            continue;
        }
        let Some((kind, n)) = part.split_once(':') else { continue };
        let n: usize = n.parse().unwrap_or(0);
        if kind == "down" {
            // "down:5" = the Down key five times
            for _ in 0..n {
                // (the wall has not been drawn yet, so there it counts as one cover across: the next cover)
                if page.covers_view { page.wall_step(0, 1) } else { page.step(true) }
            }
            continue;
        }
        let r = &page.report;
        page.sel = match kind {
            // "at:40" = like "album:40", and brought into view as well
            "album" | "at" => page.view.rows.iter().filter_map(|row| if let Row::Album(g) = row { Some(*g) } else { None }).nth(n).map(|g| (r.groups[g].key.clone(), None)),
            _ => page.view.rows.iter().filter_map(|row| if let Row::Issue(i) = row { Some(*i) } else { None }).nth(n).map(|i| {
                let i = &r.issues[i];
                (r.groups[i.group].key.clone(), Some((i.cat.clone(), i.detail.clone())))
            }),
        };
        if kind == "at" {
            page.scroll_to = page.sel.as_ref().map(|(key, _)| (key.clone(), true));
        }
    }
}

// ------------------------------------------------------------------ words and numbers

/// 22840 -> "22,840"
fn num(n: u64) -> String {
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

/// "today 3:02 PM", "yesterday 3:02 PM", "3 days ago", "Mar 4, 2026"
fn when_words(when: DateTime<Local>, now: DateTime<Local>) -> String {
    let days = (now.date_naive() - when.date_naive()).num_days();
    let time = when.format("%-I:%M %p");
    match days {
        0 => format!("today {time}"),
        1 => format!("yesterday {time}"),
        2..=30 => format!("{days} days ago"),
        _ if days < 0 => format!("{} {time}", when.format("%b %-d, %Y")),
        _ => when.format("%b %-d, %Y").to_string(),
    }
}

// ------------------------------------------------------------------ files

/// One album the loose tracks of a finding come from (the check's "albums" fact, see `healthscan`).
#[derive(Clone)]
struct LooseAlbum {
    /// "" when no album is known for these files
    album: String,
    /// "tag", the site it was found on, "not found", or "" when it has not been looked up
    source: String,
    /// the words to search Soulseek for
    search: String,
    /// the album's folder in the library ("" when the artist does not have it)
    folder: String,
    files: Vec<String>,
    /// the files whose song that album already holds
    held: Vec<String>,
    /// for files lying in the library folder itself: the artist their tags name, and that artist's folder
    /// (both empty for loose tracks in an artist's folder, which is then the finding's own)
    artist: String,
    artist_dir: String,
}

fn loose_albums(i: &Issue) -> Vec<LooseAlbum> {
    let Some(Ok(Value::Array(list))) = i.data.get("albums").map(|t| serde_json::from_str::<Value>(t)) else { return vec![] };
    let words = |v: &Value| v.as_str().unwrap_or("").to_string();
    let paths = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|f| f.as_str()).filter(|f| !f.is_empty() && f.len() <= MAX_PATH_TEXT).map(|f| f.to_string()).collect::<Vec<_>>()).unwrap_or_default();
    list.iter()
        .take(500)
        .map(|g| LooseAlbum { album: cut(words(&g["Album"]), 300), source: cut(words(&g["Source"]), 60), search: cut(words(&g["Search"]), 400), folder: words(&g["Folder"]), files: paths(&g["Files"]), held: paths(&g["Held"]), artist: cut(words(&g["Artist"]), 300), artist_dir: words(&g["ArtistDir"]) })
        .filter(|a| !a.files.is_empty() && a.folder.len() <= MAX_PATH_TEXT)
        .collect()
}

/// Takes loose files that have gone to the backups out of their finding in the app's own report: out of its
/// files, its rows and its albums, with the sentence said again for what is left. With none left the finding
/// goes. A report that cannot be read, or that the app's check did not write, is left as it is.
fn drop_loose_files(report: &Path, cat: &str, path: &str, gone: &[String]) {
    if gone.is_empty() {
        return;
    }
    let Some(mut root) = std::fs::read(report).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    if root.get("By").and_then(|b| b.as_str()) != Some("music-recs") {
        return;
    }
    let Some(list) = root.get_mut("Issues").and_then(|i| i.as_array_mut()) else { return };
    let is_gone = |f: &Value| f.as_str().is_some_and(|f| gone.iter().any(|g| g.eq_ignore_ascii_case(f)));
    let Some(at) = list.iter().position(|i| i["C"].as_str() == Some(cat) && i["P"].as_str().is_some_and(|p| p.eq_ignore_ascii_case(path))) else { return };
    let i = &mut list[at];
    if let Some(files) = i["F"].as_array_mut() {
        files.retain(|f| !is_gone(f));
    }
    let left: Vec<String> = i["F"].as_array().map(|a| a.iter().filter_map(|f| f.as_str()).map(leaf).collect()).unwrap_or_default();
    if left.is_empty() {
        list.remove(at);
    } else {
        i["D"] = Value::String(if left.len() == 1 { format!("{} isn't in an album folder", left[0]) } else { format!("{} tracks aren't in an album folder: {}", left.len(), healthscan::list_of(&left, 3)) });
        // the two facts are JSON kept as text
        let edit = |i: &mut Value, fact: &str, change: &dyn Fn(&mut Vec<Value>)| {
            if let Some(Value::Array(mut rows)) = i["X"][fact].as_str().and_then(|t| serde_json::from_str::<Value>(t).ok()) {
                change(&mut rows);
                i["X"][fact] = Value::String(Value::Array(rows).to_string());
            }
        };
        edit(i, "loose", &|rows| rows.retain(|r| !is_gone(&r[0])));
        edit(i, "albums", &|groups| {
            for g in groups.iter_mut() {
                for k in ["Files", "Held"] {
                    if let Some(a) = g[k].as_array_mut() {
                        a.retain(|f| !is_gone(f));
                    }
                }
            }
            groups.retain(|g| g["Files"].as_array().is_some_and(|a| !a.is_empty()));
        });
    }
    let _ = crate::util::write_atomic(report, root.to_string().as_bytes());
}

/// How many times ffmpeg had been fetched when a check last could not start for want of it.
static FFMPEG_NOTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The library's owner chose "Look only, change nothing" (the first-start screen offers it for a library
/// that is not laid out as artist folders holding album folders; Settings, Folders has the choice). Set
/// by the page each time it is drawn, for the parts of it that do not get the settings handed to them.
static LOOK_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The same for "Lossless downloads: keep as they are" (Settings, Folders).
static KEEP_LOSSLESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn look_only() -> bool {
    LOOK_ONLY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Why the repair words are quiet, and the word that leads to the choice.
fn look_only_line(ui: &mut Ui) {
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.label(RichText::new("Repairs are off for this library: it is set to look only.").size(12.5).color(FAINT()));
        if theme::quiet(ui, "Open Settings", 12.5, true).clicked() {
            crate::links::go(crate::links::Go::SettingsBasics);
        }
    });
}

/// "PowerShell 7 is not installed ..." with the word that installs it.
fn no_pwsh_line(ui: &mut Ui) {
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        crate::helpers::pwsh_line(ui, 12.5, WARN(), false);
    });
}

/// The first thing that went wrong, and how many more there were.
fn said(problems: &[String]) -> String {
    match problems {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (and {} more)", rest.len()),
    }
}

/// Takes a finding that has been repaired out of the app's own report, so the page shows what is true
/// without a new check. A report that cannot be read is left as it is.
fn drop_finding(report: &Path, cat: &str, path: &str) {
    let Some(mut root) = std::fs::read(report).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    if root.get("By").and_then(|b| b.as_str()) != Some("music-recs") {
        return;
    }
    let Some(list) = root.get_mut("Issues").and_then(|i| i.as_array_mut()) else { return };
    let before = list.len();
    list.retain(|i| !(i["C"].as_str() == Some(cat) && i["P"].as_str().is_some_and(|p| p.to_lowercase() == path.to_lowercase())));
    if list.len() != before {
        let _ = crate::util::write_atomic(report, root.to_string().as_bytes());
    }
}

/// The keys of the issues the user chose to ignore, one per line, in the app's data folder (a test run,
/// with `MUSIC_RECS_DATA` set, has its own there, so trying "Ignore" never touches the real one).
fn ignore_file() -> PathBuf {
    crate::config::app_dir().join("ignored-issues.txt")
}

/// Adds a key to the list of ignored issues, or takes it out. Every other line stays as it is, and so do
/// the file's line ends and the marker some editors put at the start of a UTF-8 file.
fn write_ignore(file: &Path, key: &str, on: bool) -> std::io::Result<()> {
    let old = match std::fs::read(file) {
        Ok(b) => String::from_utf8_lossy(&b).to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let marked = old.starts_with('\u{feff}');
    let end = if old.contains("\r\n") || old.is_empty() { "\r\n" } else { "\n" };
    let mut lines: Vec<&str> = old.trim_start_matches('\u{feff}').lines().filter(|l| !l.trim().is_empty() && *l != key).collect();
    if on {
        lines.push(key);
    }
    let mut out = String::from(if marked { "\u{feff}" } else { "" });
    for l in lines {
        out.push_str(l);
        out.push_str(end);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::util::write_atomic(file, out.as_bytes())
}

fn file_sig(path: &Path) -> FileSig {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn load(report_file: &Path, ignore_file: &Path) -> Loaded {
    // a report is a few hundred KB; one far beyond that is not read into memory at all
    if std::fs::metadata(report_file).is_ok_and(|m| m.len() > 200_000_000) {
        return Loaded::Unreadable;
    }
    let bytes = match std::fs::read(report_file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(_) => return Loaded::Unreadable,
    };
    let ignored: HashSet<String> = std::fs::read(ignore_file)
        .map(|b| text(&b).lines().map(|l| l.trim_end_matches('\r').to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default();
    match parse(&text(&bytes), &ignored) {
        Some(mut r) => {
            // this runs off the window's thread, so it can ask the disk what is a file and what is a folder
            for g in &mut r.groups {
                g.folder = folder_of(&g.path, true);
            }
            // "1 of 1 tracks checked are ..." names its track instead (reports made before the check did so itself)
            for n in 0..r.issues.len() {
                if !r.issues[n].detail.starts_with("1 of 1 ") {
                    continue;
                }
                // a loose track's issue points at the file itself; an album's at its folder
                let g = &r.groups[r.issues[n].group];
                let only = if Path::new(&g.path).is_file() { Some(leaf(&g.path)) } else { only_track(&g.folder) };
                if let Some(better) = name_the_one_track(&r.issues[n].detail, only.as_deref()) {
                    r.issues[n].short = first_clause(&better);
                    r.issues[n].detail = better;
                }
            }
            Loaded::Ready(r)
        }
        None => Loaded::Unreadable,
    }
}

/// File bytes as text, without the marker some editors put at the start of UTF-8 files.
fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.strip_prefix('\u{feff}').unwrap_or(&s).to_string()
}

/// A JSON value as the text a person would read: strings as they are, numbers and true/false printed.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn count_of(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn flag_of(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

/// Reads the report leniently: anything missing or of the wrong type is left out instead of failing.
/// None only when the text is not a JSON object at all (for example a half-written file).
fn parse(json: &str, ignored: &HashSet<String>) -> Option<Report> {
    let root: Value = serde_json::from_str(json).ok()?;
    let root = root.as_object()?;
    let mut r = Report {
        when: root.get("When").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok()).map(|d| d.with_timezone(&Local)),
        deep: flag_of(root.get("Deep")),
        cancelled: flag_of(root.get("Cancelled")).unwrap_or(false),
        own: root.get("By").and_then(|b| b.as_str()) == Some("music-recs"),
        files: count_of(root.get("Files")),
        updated: root.get("Updated").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok()).map(|d| d.with_timezone(&Local)),
        albums: count_of(root.get("Albums")),
        artists: count_of(root.get("Artists")),
        unchecked: count_of(root.get("Unchecked")).unwrap_or(0),
        deep_left: count_of(root.get("DeepLeft")).unwrap_or(0),
        lists_left: count_of(root.get("ListsLeft")).unwrap_or(0),
        ..Default::default()
    };
    // read leniently: a single issue may arrive as an object instead of a list of one
    let list: Vec<&Value> = match root.get("Issues") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v @ Value::Object(_)) => vec![v],
        _ => vec![],
    };
    let mut index: HashMap<String, usize> = HashMap::new();
    for v in list {
        let Some(o) = v.as_object() else { continue };
        let s = |k: &str| o.get(k).map(text_of).unwrap_or_default();
        // The words in the report come from tags in downloaded files, so they can be any length: what is shown
        // is cut to a size a person could read, and an issue with an impossible path is left out.
        let (cat, artist, album, path, detail) = (cut(s("C"), 200), cut(s("A"), 300), cut(s("L"), 300), s("P"), cut(s("D"), 4000));
        if (cat.is_empty() && detail.is_empty()) || path.len() > MAX_PATH_TEXT {
            continue;
        }
        let cat = if cat.is_empty() { "Other".to_string() } else { cat };
        let sev = match count_of(o.get("S")) {
            Some(1) => 1,
            Some(2) => 2,
            _ => 3,
        };
        let files = match o.get("F") {
            Some(Value::Array(a)) => a.iter().take(5000).map(text_of).filter(|f| !f.is_empty() && f.len() <= MAX_PATH_TEXT).collect(),
            Some(Value::String(f)) if !f.is_empty() && f.len() <= MAX_PATH_TEXT => vec![f.clone()],
            _ => vec![],
        };
        let data = match o.get("X") {
            Some(Value::Object(x)) => x.iter().take(64).map(|(k, v)| (cut(k.clone(), 100), cut(text_of(v), 400_000))).collect(),
            _ => HashMap::new(),
        };
        let ignored = ignored.contains(&format!("{cat}|{}", path.to_lowercase()));
        let key = format!("{}|{}|{}", path.to_lowercase(), artist.to_lowercase(), album.to_lowercase());
        let group = *index.entry(key.clone()).or_insert_with(|| {
            let shown_artist = if artist.is_empty() { leaf(&path) } else { artist.clone() };
            let in_library = !artist.is_empty() && !album.is_empty() && !album.starts_with("(loose");
            let folder = folder_of(&path, false);
            r.groups.push(Group { key, hay: format!("{artist} {album}").to_lowercase(), artist: shown_artist, album: album.clone(), path: path.clone(), folder, in_library, issues: vec![] });
            r.groups.len() - 1
        });
        r.groups[group].issues.push(r.issues.len());
        let short = first_clause(&detail);
        r.issues.push(Issue { sev, cat, detail, short, fix: s("K"), files, data, ignored, group });
    }
    // albums with the most serious problems first, then by artist and album; inside an album the same
    let issues = &r.issues;
    for g in &mut r.groups {
        g.issues.sort_by(|a, b| issues[*a].sev.cmp(&issues[*b].sev).then_with(|| issues[*a].cat.cmp(&issues[*b].cat)));
    }
    let worst = |g: &Group| g.issues.iter().filter(|i| !issues[**i].ignored).map(|i| issues[*i].sev).min().unwrap_or(4);
    let mut order: Vec<usize> = (0..r.groups.len()).collect();
    order.sort_by(|a, b| {
        let (ga, gb) = (&r.groups[*a], &r.groups[*b]);
        worst(ga).cmp(&worst(gb)).then_with(|| ga.artist.to_lowercase().cmp(&gb.artist.to_lowercase())).then_with(|| ga.album.to_lowercase().cmp(&gb.album.to_lowercase()))
    });
    let mut place = vec![0; order.len()];
    for (new, old) in order.iter().enumerate() {
        place[*old] = new;
    }
    let mut groups: Vec<Option<Group>> = std::mem::take(&mut r.groups).into_iter().map(Some).collect();
    r.groups = order.iter().map(|old| groups[*old].take().expect("each group is moved once")).collect();
    for i in &mut r.issues {
        i.group = place[i.group];
    }
    Some(r)
}

/// The name of the one audio file in a folder (its own files, not subfolders'); None if there are none or several.
fn only_track(folder: &Path) -> Option<String> {
    let mut found: Option<String> = None;
    for e in std::fs::read_dir(folder).ok()?.flatten() {
        let p = e.path();
        if p.is_file() && covers::ext_in(&p, &covers::AUDIO) {
            if found.is_some() {
                return None;
            }
            found = Some(e.file_name().to_string_lossy().to_string());
        }
    }
    found
}

/// "1 of 1 tracks checked are labelled ..." says nothing a person wants to count: with `name` (the folder's
/// only track) it becomes "<name> is labelled ...", without it "The one track checked is labelled ...".
/// None for any other sentence.
fn name_the_one_track(detail: &str, name: Option<&str>) -> Option<String> {
    let who = name.map(|n| n.to_string());
    for (was, verb) in [("1 of 1 tracks checked are labelled", "is labelled"), ("1 of 1 lossless tracks checked stop sharply", "stops sharply")] {
        if let Some(rest) = detail.strip_prefix(was) {
            let who = who.unwrap_or_else(|| "The one track checked".to_string());
            return Some(format!("{who} {verb}{}", rest.replacen(" but stop sharply", " but stops sharply", 1)));
        }
    }
    None
}

/// Longer than any path Windows can open.
const MAX_PATH_TEXT: usize = 33_000;

/// Text cut to at most `max` characters (with "…" when something was cut).
fn cut(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// The last part of a path: the folder's own name.
fn leaf(path: &str) -> String {
    path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_words_in_a_report_are_cut_and_impossible_paths_left_out() {
        let long = "a-".repeat(600_000);
        let json = serde_json::json!({ "Issues": [
            { "S": 2, "C": "Mixed-up tags", "A": long, "L": "Album", "P": "X:/Lib/A/B", "D": long, "F": [long, "X:/Lib/A/B/01.mp3"], "X": { "note": long } },
            { "S": 1, "C": "Damaged", "A": "A", "L": "B", "P": long, "D": "x" } ] })
        .to_string();
        let r = parse(&json, &HashSet::new()).unwrap();
        assert_eq!(r.issues.len(), 1);
        let i = &r.issues[0];
        assert_eq!((i.detail.chars().count(), r.groups[0].artist.chars().count()), (4001, 301));
        assert_eq!(i.files.len(), 1);
        assert_eq!(i.data["note"].chars().count(), 400_001);
        assert_eq!(cut("short".into(), 10), "short");
    }

    #[test]
    fn a_check_of_one_track_names_the_track() {
        let was = "1 of 1 tracks checked are labelled ~320kbps but stop sharply at 15.8kHz: really ~128kbps quality";
        assert_eq!(name_the_one_track(was, Some("03 We Want Fun.mp3")).as_deref(), Some("03 We Want Fun.mp3 is labelled ~320kbps but stops sharply at 15.8kHz: really ~128kbps quality"));
        assert_eq!(name_the_one_track(was, None).as_deref(), Some("The one track checked is labelled ~320kbps but stops sharply at 15.8kHz: really ~128kbps quality"));
        assert_eq!(name_the_one_track("1 of 1 lossless tracks checked stop sharply at 16kHz: converted", Some("a.flac")).as_deref(), Some("a.flac stops sharply at 16kHz: converted"));
        assert_eq!(name_the_one_track("3 of 12 tracks checked are labelled ~320kbps but stop sharply", Some("a.mp3")), None);
        assert_eq!(name_the_one_track("1 of 15 tracks differ from the official list", Some("a.mp3")), None);
    }

    #[test]
    fn a_broken_report_never_panics() {
        let none = HashSet::new();
        assert!(parse("", &none).is_none());
        assert!(parse("{\"When\":\"2026-11-08T13:49:01.89", &none).is_none());
        assert!(parse("[1,2]", &none).is_none());
        assert!(parse("null", &none).is_none());
        let r = parse("{}", &none).unwrap();
        assert!(r.issues.is_empty() && r.when.is_none() && r.deep.is_none());
        let r = parse(r#"{"When":7,"Deep":"yes","Files":"12","Albums":-3,"Issues":[1,"x",null,{},{"S":"2","C":"Low bitrate","P":5,"F":"one.mp3","X":[1]},{"S":9,"D":"only a sentence","F":[1,null,"a"],"X":{"n":3}}]}"#, &none).unwrap();
        assert_eq!(r.issues.len(), 2);
        assert_eq!(r.files, Some(12));
        assert_eq!(r.albums, None);
        let low = r.issues.iter().find(|i| i.cat == "Low bitrate").unwrap();
        assert_eq!((low.sev, low.files.len()), (2, 1));
        let other = r.issues.iter().find(|i| i.cat == "Other").unwrap();
        assert_eq!((other.sev, other.files.len(), other.data["n"].as_str()), (3, 2, "3"));
        // one issue saved as an object instead of a list
        assert_eq!(parse(r#"{"Issues":{"S":1,"C":"Damaged","P":"M:\\Music\\A\\B"}}"#, &none).unwrap().issues.len(), 1);
    }

    #[test]
    fn ignored_issues_are_matched_by_key_and_groups_are_ordered() {
        let ignored: HashSet<String> = [r"Low bitrate|m:\music\a\b".to_string()].into();
        let r = parse(
            r#"{"When":"2026-11-08T13:49:01.8989668-05:00","Deep":true,"Issues":[
            {"S":3,"C":"Low bitrate","A":"A","L":"B","P":"M:\\Music\\A\\B","D":"d1"},
            {"S":2,"C":"No cover art","A":"A","L":"B","P":"M:\\Music\\A\\B","D":"d2"},
            {"S":1,"C":"Damaged or incomplete","A":"Z","L":"Y","P":"M:\\Music\\Z\\Y","D":"d3"}]}"#,
            &ignored,
        )
        .unwrap();
        assert!(r.when.is_some());
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].artist, "Z"); // the serious one first
        assert_eq!(r.issues.iter().filter(|i| i.ignored).count(), 1);
        for (gi, g) in r.groups.iter().enumerate() {
            assert!(g.issues.iter().all(|i| r.issues[*i].group == gi));
        }
        let b = &r.groups[1];
        assert_eq!(r.issues[b.issues[0]].cat, "No cover art"); // worth fixing before minor
    }

    #[test]
    fn the_list_shows_the_first_clause() {
        for (full, short) in [
            ("No cover in the tracks or the folder", "No cover in the tracks or the folder"),
            ("Best cover is only 200x200 (500+ looks sharp on most screens)", "Best cover is only 200x200 (500+ looks sharp on most screens)"),
            (
                "1 of 1 tracks checked are labelled ~320kbps but stop sharply at 15.8kHz: really ~128kbps quality that was re-encoded bigger. A proper 320k copy would sound better",
                "1 of 1 tracks checked are labelled ~320kbps but stop sharply at 15.8kHz",
            ),
            ("16 Cashin'.mp3: full length (4:17, real 4:21) but 57 decode errors - likely audible glitches. Re-download these", "16 Cashin'.mp3: full length (4:17, real 4:21) but 57 decode errors"),
            (
                "1 of 20 tracks differ from the official list (MusicBrainz): 07 Where's Waldo.mp3: title 'Where's Waldo' -> 'Where Is Waldo'",
                "1 of 20 tracks differ from the official list (MusicBrainz)",
            ),
            (
                "Missing disc 1 tracks 16, 17 of 17 (the tags say 17 tracks; you have up to 15) (couldn't confirm: the edition on MusicBrainz has 15 tracks, yours has 17)",
                "Missing disc 1 tracks 16, 17 of 17",
            ),
            (
                "01. Digital Dash.mp3: plays the full 3:51 (real track: 3:51, Apple Music) - only the length shown in players is wrong; 02. Big Rings.mp3: plays the full 3:38",
                "01. Digital Dash.mp3: plays the full 3:51 (real track: 3:51, Apple Music)",
            ),
            (
                "09 - Weathered Tome.mp3 and 10 - The Last Kiss.mp3 are swapped: the first plays 3:03 (the official 'The Last Kiss'), the second 2:13. Have a listen",
                "09 - Weathered Tome.mp3 and 10 - The Last Kiss.mp3 are swapped",
            ),
            ("Same song twice: track 20 (20 Cloud Cult - silence.mp3, 22 Cloud Cult - silence.mp3, 24 Cloud Cult - silence.mp3)", "Same song twice: track 20"),
            ("Cover is only a file in the folder, not embedded - phones and some players won't show it", "Cover is only a file in the folder, not embedded"),
            (
                "'[1998] No Substance' (16 tracks) / '[1998] No Substance (Bonus B-Side Disc)' (9 tracks). Could be different editions; worth a look",
                "'[1998] No Substance' (16 tracks) / '[1998] No Substance (Bonus B-Side Disc)' (9 tracks)",
            ),
            ("2 track(s) have a few decode errors, usually inaudible: 04. Origin.mp3; 05. Well And Spring.mp3", "2 track(s) have a few decode errors, usually inaudible"),
            ("Ünïcödé wörds äre nöt cut in the middle of a letter, whatever häppens: thïs is the rest of the sentence", "Ünïcödé wörds äre nöt cut in the middle of a letter, whatever häppens"),
        ] {
            assert_eq!(first_clause(full), short);
        }
    }

    #[test]
    fn albums_are_found_and_marked_by_folder() {
        assert_eq!(loose_name("[1993] Vs"), "vs");
        assert_eq!(loose_name("Vs. (1993)"), "vs");
        assert_eq!(loose_name("Manners (2009)"), loose_name("manners"));
        assert_ne!(loose_name("Baby I'm Back II"), loose_name("Baby I'm Back III"));
        assert_eq!(loose_name("( ) (2002)"), "");
        assert_eq!(folder_of(r"M:\Music\A\A - Song.mp3", false), PathBuf::from(r"M:\Music\A"));
        assert_eq!(folder_of(r"M:\Music\A\Vol. 2", false), PathBuf::from(r"M:\Music\A\Vol. 2"));
        let r = parse(
            r#"{"Issues":[
            {"S":1,"C":"Upscaled MP3","A":"A","L":"(loose tracks)","P":"M:\\Music\\A\\A - Song.mp3","D":"d"},
            {"S":3,"C":"Loose tracks","A":"A","L":"","P":"M:\\Music\\A","D":"d"},
            {"S":2,"C":"No cover art","A":"A","L":"B (2001)","P":"M:\\Music\\A\\B (2001)","D":"d"}]}"#,
            &HashSet::new(),
        )
        .unwrap();
        let by_album = |l: &str| r.groups.iter().find(|g| g.album == l).unwrap();
        assert_eq!(by_album("(loose tracks)").folder, PathBuf::from(r"M:\Music\A"));
        assert!(!by_album("(loose tracks)").in_library && !by_album("").in_library && by_album("B (2001)").in_library);
    }

    const THREE: &str = r#"{"Issues":[
        {"S":3,"C":"Low bitrate","A":"A","L":"B (2001)","P":"M:\\Music\\A\\B (2001)","D":"d1"},
        {"S":2,"C":"No cover art","A":"A","L":"B (2001)","P":"M:\\Music\\A\\B (2001)","D":"d2"},
        {"S":1,"C":"Upscaled MP3","A":"Z","L":"(loose tracks)","P":"M:\\Music\\Z\\Z - Song.mp3","D":"d3"},
        {"S":3,"C":"Small cover art","A":"Q","L":"[1993] Vs","P":"M:\\Music\\Q\\[1993] Vs","D":"d4"}]}"#;

    fn page(ignored: &[&str]) -> HealthPage {
        let mut p = HealthPage::new(&egui::Context::default());
        p.report = parse(THREE, &ignored.iter().map(|s| s.to_string()).collect()).unwrap();
        p.state = State::Ready;
        p.rebuild();
        p
    }

    #[test]
    fn the_line_under_the_title_says_when_the_report_was_brought_up_to_date() {
        let (checked, updated) = (Local::now() - chrono::Duration::minutes(90), Local::now() - chrono::Duration::minutes(1));
        let report = |updated: Option<DateTime<Local>>| {
            let mut root = serde_json::json!({ "When": checked.to_rfc3339(), "By": "music-recs", "Deep": false, "Files": 12, "Albums": 3, "Artists": 2, "DeepLeft": 4, "Issues": [] });
            if let Some(u) = updated {
                root["Updated"] = serde_json::json!(u.to_rfc3339());
            }
            parse(&root.to_string(), &HashSet::new()).unwrap()
        };
        let mut p = page(&[]);
        p.state = State::Ready;
        p.report = report(None);
        assert!(!p.facts().0.contains("brought up to date"), "{}", p.facts().0);
        p.report = report(Some(updated));
        let (line, more) = p.facts();
        let at = (line.find("12 tracks, 3 albums, 2 artists").unwrap(), line.find("quick check").unwrap(), line.find("brought up to date").unwrap());
        assert!(at.0 < at.1 && at.1 < at.2, "{line}");
        // what the check has not got to yet is a line of its own
        assert_eq!(more, "4 tracks not listened to yet");
        // a date from before the whole check says nothing
        p.report = report(Some(checked - chrono::Duration::days(1)));
        assert!(!p.facts().0.contains("brought up to date"));
    }

    #[test]
    fn the_sentence_at_the_top_says_how_the_library_is() {
        // one album with a serious problem among three with problems
        let mut p = page(&[]);
        assert_eq!(p.state_words().0, "1 album needs attention");
        assert_eq!(p.listed(), "ALL ISSUES  ·  4 IN 3 ALBUMS");
        // the serious one set aside: nothing serious is left
        let mut p2 = page(&["Upscaled MP3|m:\\music\\z\\z - song.mp3"]);
        assert_eq!(p2.state_words().0, "Nothing serious in your library");
        p2.filter = Filter::Cat(2, "No cover art".into());
        p2.rebuild();
        assert_eq!(p2.listed(), "NO COVER ART  ·  1 IN 1 ALBUM");
        // nothing found at all
        p.report = parse(r#"{"Issues":[]}"#, &HashSet::new()).unwrap();
        p.rebuild();
        assert_eq!(p.state_words().0, "Your library is in good shape");
        assert_eq!(p.nothing_words(), "The last check found no problems.");
        // before any check, and while the last one is read
        p.state = State::Missing;
        assert_eq!(p.state_words().0, "Your library has not been checked yet");
        p.state = State::Loading;
        assert_eq!(p.state_words().0, "Reading the last check");
    }

    #[test]
    fn ignoring_a_problem_writes_its_key_and_stopping_takes_it_out_again() {
        let dir = std::env::temp_dir().join(format!("music-recs-ignore-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ignored-issues.txt");
        // a list as Windows PowerShell leaves it: a marker at the start, CRLF line ends, someone else's line
        std::fs::write(&file, "\u{feff}Missing tags|m:\\music\\other\\album\r\n").unwrap();
        let mut p = page(&[]);
        p.ignore_file = file.clone();
        let album = p.report.groups.iter().find(|g| g.album == "B (2001)").unwrap().key.clone();
        p.sel = Some((album.clone(), None));
        assert_eq!(p.open, [0; 4]); // (the test page is made without counting; set_ignored counts)

        p.set_ignored(&album, "No cover art", true);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\u{feff}Missing tags|m:\\music\\other\\album\r\nNo cover art|m:\\music\\a\\b (2001)\r\n");
        assert_eq!(p.open, [3, 1, 0, 2]);
        assert!(p.report.issues.iter().any(|i| i.cat == "No cover art" && i.ignored) && p.sel.is_some() && p.dirty);
        // what the page shows is what a fresh read of the report and the list gives
        let ignored: HashSet<String> = text(&std::fs::read(&file).unwrap()).lines().map(|l| l.to_string()).collect();
        assert_eq!(parse(THREE, &ignored).unwrap().issues.iter().filter(|i| i.ignored).count(), 1);

        // the album's last open problem set aside: its details close
        p.set_ignored(&album, "Low bitrate", true);
        assert!(p.sel.is_none());
        assert_eq!(p.open, [2, 1, 0, 1]);

        // and back: only that line leaves the list
        p.set_ignored(&album, "No cover art", false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\u{feff}Missing tags|m:\\music\\other\\album\r\nLow bitrate|m:\\music\\a\\b (2001)\r\n");
        assert_eq!(p.open, [3, 1, 1, 1]);
        // a list that is not there yet is made
        let fresh = dir.join("new").join("ignored.txt");
        write_ignore(&fresh, "X|y", true).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "X|y\r\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repaired_finding_leaves_the_apps_own_report_and_no_other() {
        let dir = std::env::temp_dir().join(format!("music-recs-dropfinding-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (own, theirs) = (dir.join("own.json"), dir.join("theirs.json"));
        std::fs::write(&own, THREE.replacen('{', r#"{"By":"music-recs","Files":7,"#, 1)).unwrap();
        std::fs::write(&theirs, THREE).unwrap();
        drop_finding(&own, "No cover art", r"m:\music\A\B (2001)");
        let left: Value = serde_json::from_slice(&std::fs::read(&own).unwrap()).unwrap();
        let cats: Vec<&str> = left["Issues"].as_array().unwrap().iter().map(|i| i["C"].as_str().unwrap()).collect();
        assert_eq!(cats, vec!["Low bitrate", "Upscaled MP3", "Small cover art"]);
        assert_eq!(left["Files"], 7);
        // a report the app's check did not write is never changed; a file that is not a report is left alone
        drop_finding(&theirs, "No cover art", r"M:\Music\A\B (2001)");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), THREE);
        std::fs::write(&own, "{ not json").unwrap();
        drop_finding(&own, "Low bitrate", r"M:\Music\A\B (2001)");
        assert_eq!(std::fs::read_to_string(&own).unwrap(), "{ not json");
        assert_eq!(said(&["a".to_string(), "b".to_string(), "c".to_string()]), "a (and 2 more)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marks_are_the_worst_open_problem_of_each_folder() {
        let mut m = marks(&page(&[]).report);
        m.sort();
        assert_eq!(m, vec![(PathBuf::from(r"M:\Music\A\B (2001)"), 2), (PathBuf::from(r"M:\Music\Q\[1993] Vs"), 3), (PathBuf::from(r"M:\Music\Z"), 1)]);
        // ignored problems are left out, and an album with nothing else gets no mark
        let mut m = marks(&page(&[r"No cover art|m:\music\a\b (2001)", r"Small cover art|m:\music\q\[1993] vs"]).report);
        m.sort();
        assert_eq!(m, vec![(PathBuf::from(r"M:\Music\A\B (2001)"), 3), (PathBuf::from(r"M:\Music\Z"), 1)]);
    }

    #[test]
    fn the_list_starts_with_the_worst_under_a_heading_per_severity() {
        let mut p = page(&[]);
        let kinds: String = p.view.rows.iter().map(|r| match r { Row::Section(s, n) => format!("[{s}:{n}]"), Row::Album(_) => "A".into(), Row::Issue(_) => "i".into() }).collect();
        assert_eq!(kinds, "[1:1]Ai[2:1]Aii[3:1]Ai");
        // filtered to minor, the album whose worst problem was "worth fixing" moves under Minor
        p.filter = Filter::Sev(3);
        p.rebuild();
        let kinds: String = p.view.rows.iter().map(|r| match r { Row::Section(s, n) => format!("[{s}:{n}]"), Row::Album(_) => "A".into(), Row::Issue(_) => "i".into() }).collect();
        assert_eq!(kinds, "[3:2]AiAi");
        assert_eq!(p.view.ys.len(), p.view.rows.len());
    }

    #[test]
    fn another_page_can_ask_for_an_album() {
        let mut p = page(&[]);
        p.filter = Filter::Sev(1);
        p.text = "zzz".into();
        p.show_album("q", "Vs");
        assert!(p.filter == Filter::All && p.text.is_empty());
        let key = p.sel.clone().unwrap().0;
        assert_eq!(p.report.groups.iter().find(|g| g.key == key).unwrap().album, "[1993] Vs");
        assert_eq!(p.scroll_to, Some((key, true)));
        // an album with no problems: its artist goes in the filter box
        p.show_album("A", "Something Else");
        assert!(p.sel.is_none() && p.text == "A" && p.filter == Filter::All);
        // an album whose only problems are ignored counts as having none
        let mut p = page(&[r"Small cover art|m:\music\q\[1993] vs"]);
        p.show_album("Q", "[1993] Vs");
        assert!(p.sel.is_none() && p.text == "Q");
        // asked before the report is read: remembered
        let mut p = HealthPage::new(&egui::Context::default());
        assert_eq!(p.headline(), None);
        p.show_album("Q", "Vs");
        assert_eq!(p.wanted, Some(("Q".to_string(), "Vs".to_string())));
        assert!(p.sel.is_none());
    }

    #[test]
    fn the_arrow_keys_walk_through_the_albums() {
        let mut p = page(&[]);
        let album = |p: &HealthPage| p.sel.as_ref().map(|(k, _)| p.report.groups.iter().find(|g| g.key == *k).unwrap().artist.clone());
        p.step(true);
        assert_eq!(album(&p).as_deref(), Some("Z"));
        p.step(true);
        p.step(true);
        p.step(true); // stays on the last one
        assert_eq!(album(&p).as_deref(), Some("Q"));
        p.step(false);
        assert_eq!(album(&p).as_deref(), Some("A"));
    }

    #[test]
    fn the_wall_has_one_tile_per_album_in_the_lists_order() {
        let mut p = page(&[]);
        assert!(!p.covers_view, "the page opens on the list");
        p.covers_view = true;
        let tile = |p: &HealthPage, n: usize| {
            let t = &p.view.tiles[n];
            (p.report.groups[t.group].artist.clone(), t.sev, p.report.issues[t.first].cat.clone(), t.more)
        };
        assert_eq!(p.view.tiles.len(), 3);
        assert_eq!(tile(&p, 0), ("Z".to_string(), 1, "Upscaled MP3".to_string(), 0));
        assert_eq!(tile(&p, 1), ("A".to_string(), 2, "No cover art".to_string(), 1)); // the worst names it, "+1 more"
        assert_eq!(tile(&p, 2), ("Q".to_string(), 3, "Small cover art".to_string(), 0));
        assert_eq!(runs(&p.view.tiles), vec![(0, 1), (1, 1), (2, 1)]);
        // the filters decide which tiles show, as they decide the list's rows
        p.filter = Filter::Sev(3);
        p.rebuild();
        assert_eq!(p.view.tiles.len(), 2);
        assert_eq!(tile(&p, 0), ("A".to_string(), 3, "Low bitrate".to_string(), 0));
        assert_eq!(runs(&p.view.tiles), vec![(0, 2)]);
        p.filter = Filter::All;
        p.text = "vs".into();
        p.rebuild();
        assert_eq!(p.view.tiles.len(), 1);
        // an ignored problem's album has no tile, unless ignored ones are included
        let mut p = page(&[r"Small cover art|m:\music\q\[1993] vs"]);
        assert_eq!(p.view.tiles.len(), 2);
        p.show_ignored = true;
        p.rebuild();
        assert_eq!(p.view.tiles.len(), 3);
        // the names under a cover: an album by its title, anything else by its artist
        let by_artist = |a: &str| names(p.report.groups.iter().find(|g| g.artist == a).unwrap());
        assert_eq!(by_artist("Q"), ("Vs", "Q"));
        assert_eq!(by_artist("A"), ("B", "A"));
        assert_eq!(by_artist("Z"), ("Z", "loose tracks"));
        assert_eq!(album_title("(2002)"), "(2002)");
        assert_eq!(album_title("[1998] Ünï (1998)"), "Ünï");
        assert_eq!(album_title("1999"), "1999");
    }

    #[test]
    fn the_wall_is_laid_out_in_rows_under_headings() {
        let tiles: Vec<Tile> = [1, 1, 1, 1, 1, 2, 2, 3].iter().map(|s| Tile { group: 0, sev: *s, first: 0, more: 0 }).collect();
        // room for three tiles across, with the smallest gap
        let w = Wall::new(&tiles, 3.0 * TILE + 2.0 * GAP_MIN, 1.0);
        assert_eq!((w.cols, w.side, w.gap), (3, TILE, GAP_MIN));
        assert_eq!(w.pitch, TILE + TILE_TEXT + ROW_GAP);
        assert_eq!(w.secs.iter().map(|s| (s.sev, s.start, s.count)).collect::<Vec<_>>(), vec![(1, 0, 5), (2, 5, 2), (3, 7, 1)]);
        assert_eq!(w.tile_min(0), vec2(0.0, SECTION_H + 8.0));
        assert_eq!(w.tile_min(4), vec2(TILE + GAP_MIN, SECTION_H + 8.0 + w.pitch));
        // the next heading starts under the two rows of the first
        assert_eq!(w.secs[1].top, SECTION_H + 8.0 + 2.0 * w.pitch + 4.0);
        assert_eq!(w.tile_min(5).x, 0.0);
        assert!(w.height > w.tile_min(7).y + TILE + TILE_TEXT);
        // too narrow for two: one, and never none
        assert_eq!(columns(200.0, 1.0).cols, 1);
        assert_eq!(columns(10.0, 1.0).cols, 1);
        assert_eq!(columns(2.0 * TILE + 14.0, 1.0), covers::Tiles { cols: 2, side: TILE, gap: 14.0 });
    }

    #[test]
    fn the_walls_covers_grow_to_reach_the_right_edge() {
        let tiles: Vec<Tile> = [1, 1, 1, 1, 1, 1, 1, 2, 2, 3].iter().map(|s| Tile { group: 0, sev: *s, first: 0, more: 0 }).collect();
        // room to spare for five: the gap goes to its limit, and the covers take the rest in steps of 4 pixels
        let w = Wall::new(&tiles, 1000.0, 1.0);
        assert_eq!((w.cols, w.side, w.gap), (5, 176.0, 30.0));
        assert_eq!(w.pitch, 176.0 + TILE_TEXT + ROW_GAP);
        // everything that depends on the cover's side follows it: the last column ends at the edge, the
        // second row and the next heading sit one pitch further down
        assert_eq!(w.tile_min(4).x + w.side, 1000.0);
        assert_eq!(w.tile_min(5), vec2(0.0, SECTION_H + 8.0 + w.pitch));
        assert_eq!(w.secs[1].top, SECTION_H + 8.0 + 2.0 * w.pitch + 4.0);
        assert!(w.height > w.tile_min(9).y + w.side + TILE_TEXT);
        for ppp in [1.0f32, 1.25, 1.5, 2.0] {
            let mut avail = 2.0 * TILE + GAP_MIN;
            while avail < 2600.0 {
                let w = Wall::new(&tiles, avail, ppp);
                let row = w.cols as f32 * w.side + (w.cols - 1) as f32 * w.gap;
                assert!((avail - row).abs() < 4.0, "{avail} at {ppp}: {} x {} + {} is {row} wide", w.cols, w.side, w.gap);
                assert!(w.gap >= GAP_MIN - 1.0 / ppp && w.gap <= GAP_MAX + 0.01, "{avail} at {ppp}: gap {}", w.gap);
                assert!(w.side >= TILE - 0.5 / ppp && w.side <= (TILE + (TILE + GAP_MAX) / w.cols as f32 - (GAP_MAX - GAP_MIN)).max(TILE) + 4.0, "{avail} at {ppp}: side {}", w.side);
                // never fewer across than when the covers were a fixed size
                assert!(w.cols >= (((avail + GAP_MIN) / (TILE + GAP_MIN)).floor() as usize).max(1), "{avail} at {ppp}: {} across", w.cols);
                assert!((w.side * ppp - (w.side * ppp).round()).abs() < 0.01, "{avail} at {ppp}: side {}", w.side);
                avail += 0.73;
            }
        }
    }

    #[test]
    fn the_arrow_keys_walk_the_wall() {
        // three across:  0 1 2 / 3 4   then   5 6   then   7
        let runs = [(0, 5), (5, 2), (7, 1)];
        let go = |at, dx, dy| wall_move(&runs, 3, at, dx, dy);
        assert_eq!((go(0, 1, 0), go(2, 1, 0), go(4, 1, 0), go(7, 1, 0)), (1, 3, 5, 7));
        assert_eq!((go(0, -1, 0), go(3, -1, 0), go(5, -1, 0)), (0, 2, 4));
        // down keeps the column, into a shorter row and into the next heading's tiles
        assert_eq!((go(0, 0, 1), go(2, 0, 1), go(3, 0, 1), go(4, 0, 1), go(6, 0, 1), go(7, 0, 1)), (3, 4, 5, 6, 7, 7));
        // up the same way back
        assert_eq!((go(4, 0, -1), go(1, 0, -1), go(5, 0, -1), go(6, 0, -1), go(7, 0, -1)), (1, 1, 3, 4, 5));
        // from the page: the first press opens the first cover on screen, the next ones move
        let mut p = page(&[]);
        let artist = |p: &HealthPage| p.sel.as_ref().map(|(k, _)| p.report.groups.iter().find(|g| g.key == *k).unwrap().artist.clone());
        p.wall_step(1, 0);
        assert_eq!(artist(&p).as_deref(), Some("Z"));
        p.wall_step(0, 1);
        assert_eq!(artist(&p).as_deref(), Some("A"));
        assert_eq!(p.scroll_to.as_ref().map(|s| s.1), Some(false));
        p.wall_step(1, 0);
        p.wall_step(1, 0);
        assert_eq!(artist(&p).as_deref(), Some("Q"));
        p.wall_step(0, -1);
        assert_eq!(artist(&p).as_deref(), Some("A"));
    }

    #[test]
    fn words() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(22840), "22,840");
        assert_eq!(num(1234567), "1,234,567");
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local);
        let now = Local::now();
        assert!(when_words(now, now).starts_with("today "));
        assert_eq!(when_words(now - chrono::Duration::days(3), now), "3 days ago");
        assert!(when_words(at("2020-03-04T10:00:00Z"), now).contains("2020"));
        assert_eq!(short_path(r"M:\Music\A\B\01 x.mp3", r"M:\Music\A\B"), "01 x.mp3");
        assert_eq!(short_path(r"M:\Music\A\Bee\01 x.mp3", r"M:\Music\A\B"), r"M:\Music\A\Bee\01 x.mp3");
        assert_eq!(changes(r#"[["E:\\a.mp3","New","10",""],["E:\\b.mp3","","","2"],"junk"]"#), vec![("E:\\a.mp3".to_string(), "title: New, track 10".to_string()), ("E:\\b.mp3".to_string(), "disc 2".to_string())]);
    }
}
