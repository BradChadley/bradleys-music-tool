//! Bradley's Music Tool as a library: every page and all the logic. Two thin programs use it: the Music
//! Tool's window (`src/main.rs`, `run_app`) and Cadence's hidden engine (`src/bin/cadence-engine.rs`,
//! `service::run`; the conversation is described in `ENGINE-PROTOCOL.md`).

mod albumkind;
mod albumlink;
mod backup;
mod cards;
mod config;
mod import_quality;
mod lastfm_auth;
mod covers;
mod dl_facts;
mod downloads;
mod engine;
mod guard;
mod health;
mod healthscan;
mod helpers;
mod home;
mod icon;
mod images;
mod importer;
mod inbox;
mod library;
mod library_lock;
mod library_page;
mod links;
mod loose;
mod net;
pub mod organize;
mod panel;
mod recs_artists;
mod repair;
mod replace;
mod retire;
mod rows;
mod recs_grid;
mod recs_top;
mod script;
mod search;
mod secrets;
pub mod service;
mod settings;
mod setup;
mod slskd;
mod sources;
mod tagwrite;
#[cfg(test)]
mod test_tags;
mod theme;
mod undo;
mod update;
mod util;

use chrono::{Local, NaiveDate};
use config::Config;
use crossbeam_channel::Receiver;
use egui::{
    vec2, Align, Align2, Button, CentralPanel, Color32, FontId, Frame, Label, Layout, Margin, Panel, ProgressBar, RichText,
    Rect, ScrollArea, Sense, Stroke, TextEdit, Ui,
};
use engine::{merge_release, Msg, ReleaseRec, Results, SimilarRec};
use panel::filter_id;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use theme::{ACCENT, ACCENT2, BG, BG2, CARD, CARD_HOVER, DIM, GREEN, TEXT};

/// The app's top-level pages. Each one is a tool that used to be its own program.
#[derive(PartialEq, Clone, Copy)]
enum Page {
    Home,
    Recs,
    Import,
    Library,
    Health,
    Backup,
}

/// The page a first launch opens on.
/// How tall the row of page names is: the window's own title bar.
const BAR_H: f32 = 41.0;
/// How wide each of the three window controls is.
const CONTROL_W: f32 = 46.0;

/// Minimize, maximize (or restore) and close, drawn by the app at the right end of the page row. Laid out
/// right to left, so close comes first. Each is a plain mark that lights up under the pointer; close turns red.
fn window_controls(ui: &mut Ui) {
    let full = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    ui.spacing_mut().item_spacing.x = 0.0;
    for (name, what) in [("window:close", 2), ("window:maximize", 1), ("window:minimize", 0)] {
        // as tall as the bar, from the very top of the window, so the top right corner itself closes
        let (rect, _) = ui.allocate_exact_size(egui::vec2(CONTROL_W, 26.0), Sense::hover());
        let rect = Rect::from_min_max(egui::pos2(rect.left(), ui.ctx().content_rect().top()), egui::pos2(rect.right(), ui.ctx().content_rect().top() + BAR_H - 1.0));
        let r = ui.interact(rect, egui::Id::unique(name), Sense::click());
        script::seen(name, rect);
        let hot = r.hovered();
        if hot {
            ui.painter().rect_filled(rect, 0.0, if what == 2 { Color32::from_rgb(0xc4, 0x2b, 0x1c) } else { CARD_HOVER() });
        }
        let line = Stroke::new(1.0, if hot && what == 2 { Color32::WHITE } else if hot { TEXT() } else { DIM() });
        let c = rect.center();
        let c = egui::pos2(c.x.round() + 0.5, c.y.round() + 0.5);
        let p = ui.painter();
        match what {
            0 => {
                p.hline(c.x - 5.0..=c.x + 5.0, c.y, line);
            }
            1 if full => {
                // two squares, one behind the other: "put the window back"
                p.rect_stroke(Rect::from_min_max(egui::pos2(c.x - 5.0, c.y - 3.0), egui::pos2(c.x + 3.0, c.y + 5.0)), 0.0, line, egui::StrokeKind::Middle);
                p.line_segment([egui::pos2(c.x - 3.0, c.y - 3.0), egui::pos2(c.x - 3.0, c.y - 5.0)], line);
                p.line_segment([egui::pos2(c.x - 3.0, c.y - 5.0), egui::pos2(c.x + 5.0, c.y - 5.0)], line);
                p.line_segment([egui::pos2(c.x + 5.0, c.y - 5.0), egui::pos2(c.x + 5.0, c.y + 3.0)], line);
                p.line_segment([egui::pos2(c.x + 5.0, c.y + 3.0), egui::pos2(c.x + 3.0, c.y + 3.0)], line);
            }
            1 => {
                p.rect_stroke(Rect::from_center_size(c, egui::vec2(10.0, 10.0)), 0.0, line, egui::StrokeKind::Middle);
            }
            _ => {
                p.line_segment([egui::pos2(c.x - 5.0, c.y - 5.0), egui::pos2(c.x + 5.0, c.y + 5.0)], line);
                p.line_segment([egui::pos2(c.x + 5.0, c.y - 5.0), egui::pos2(c.x - 5.0, c.y + 5.0)], line);
            }
        }
        if r.clicked() {
            ui.ctx().send_viewport_cmd(match what {
                0 => egui::ViewportCommand::Minimized(true),
                1 => egui::ViewportCommand::Maximized(!full),
                _ => egui::ViewportCommand::Close,
            });
        }
    }
    ui.add_space(14.0);
    ui.spacing_mut().item_spacing.x = 22.0;
}

/// A window without the Windows frame has no edges to pull, so the app gives it its own: within a few
/// points of an edge or a corner the pointer shows the resize arrows, and pressing there starts the resize.
/// Not while the window fills the screen.
fn window_edges(ctx: &egui::Context) {
    use egui::{CursorIcon, ResizeDirection as D};
    const EDGE: f32 = 5.0;
    let (full, at, pressed) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false), i.pointer.hover_pos(), i.pointer.primary_pressed()));
    let Some(at) = at.filter(|_| !full) else { return };
    let win = ctx.content_rect();
    let (left, right, top, bottom) = (at.x <= win.left() + EDGE, at.x >= win.right() - EDGE, at.y <= win.top() + EDGE, at.y >= win.bottom() - EDGE);
    let found = match (left, right, top, bottom) {
        (true, _, true, _) => Some((D::NorthWest, CursorIcon::ResizeNwSe)),
        (_, true, true, _) => Some((D::NorthEast, CursorIcon::ResizeNeSw)),
        (true, _, _, true) => Some((D::SouthWest, CursorIcon::ResizeNeSw)),
        (_, true, _, true) => Some((D::SouthEast, CursorIcon::ResizeNwSe)),
        (true, ..) => Some((D::West, CursorIcon::ResizeHorizontal)),
        (_, true, ..) => Some((D::East, CursorIcon::ResizeHorizontal)),
        (_, _, true, _) => Some((D::North, CursorIcon::ResizeVertical)),
        (_, _, _, true) => Some((D::South, CursorIcon::ResizeVertical)),
        _ => None,
    };
    if let Some((direction, cursor)) = found {
        ctx.set_cursor_icon(cursor);
        if pressed {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

/// A release hidden with "Not interested" before "&" was read as "and" in album names was saved under its
/// old key ("artist|redseaslostghosts"). When that release turns up again, it is hidden under its new key
/// too. Returns true when the list changed (so it is worth saving).
fn carry_hidden(dismissed: &mut std::collections::BTreeSet<String>, artist_key: &str, title: &str) -> bool {
    if !title.contains('&') {
        return false;
    }
    let (new, old) = (engine::rel_key(artist_key, title), engine::rel_key(artist_key, &title.replace('&', " ")));
    !dismissed.contains(&new) && dismissed.contains(&old) && dismissed.insert(new)
}

/// The app's name, as the window's title bar and the taskbar show it (and what its window state is kept under).
const APP_NAME: &str = "Bradley's Music Tool";
/// The program file's name, lower case: how a second launch knows the window already open is this app's.
const EXE_NAME: &str = "bradleys-music-tool.exe";
const START_PAGE: Page = Page::Home;

/// The pages in the order they appear across the top (and under Ctrl+1, Ctrl+2, ...): page, its name, its saved key.
const PAGES: [(Page, &str, &str); 6] = [(Page::Home, "Home", "home"), (Page::Recs, "Recs", "recs"), (Page::Import, "Downloads", "downloads"), (Page::Library, "Library", "library"), (Page::Health, "Health", "health"), (Page::Backup, "Backup", "backup")];

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Releases,
    Similar,
    Artists,
}

#[derive(PartialEq, Clone, Copy)]
enum RelSort {
    Newest,
    Favorite,
}

enum Action {
    Open(String),
    /// open an album's Wikipedia page, else Bandcamp, else the fallback link: (artist, title, fallback, MusicBrainz link)
    Album(String, String, String, String),
    /// open an album's Bandcamp page (found via MusicBrainz or Bandcamp's search), else a Bandcamp search:
    /// (artist, title, MusicBrainz link)
    Bandcamp(String, String, String),
    Copy(String),
    Hide(String),
    Unhide(String),
    /// open the artist panel: (name, "because you like" names)
    Artist(String, Vec<String>),
    /// hide all new releases from an artist: (artist key, display name)
    Mute(String, String),
    Unmute(String),
    /// open the artist panel straight into the "Wrong artist?" picker
    FixMatch(String),
    /// use this MusicBrainz artist for (artist key, pin)
    Pin(String, config::Pin),
    Unpin(String),
}

/// State of the "Wrong artist?" picker in the artist panel.
struct PinUi {
    query: String,
    loading: bool,
    results: Option<Result<Vec<engine::Candidate>, String>>,
}

struct DetailView {
    name: String,
    because: Vec<String>,
    data: Option<engine::ArtistDetail>,
}

struct App {
    page: Page,
    /// The importer, set up the first time its page is opened. Err holds the reason it couldn't start.
    importer: Option<Result<importer::ImportTab, String>>,
    /// where the albums you own live, for showing their own covers in the Recs artist panel: "artist|title" -> folder
    owned_folders: std::collections::HashMap<String, Option<std::path::PathBuf>>,
    /// Search everything (Ctrl+K): open or not, just opened (to take the keyboard), the text, the chosen row.
    search_open: bool,
    search_fresh: bool,
    search_text: String,
    search_sel: usize,
    /// Settings, Soulseek login boxes. The password is cleared from here as soon as it is saved to slskd's file.
    slsk_user: String,
    slsk_pass: String,
    slsk_show: bool,
    slsk_loaded: bool,
    slsk_msg: Option<Result<String, String>>,
    /// when Settings were last saved / the lookups were last forgotten (for the short "Saved" notes)
    saved_at: Option<f64>,
    cache_cleared_at: Option<f64>,
    /// "is this folder there" answers for Settings, looked at beside the window (`settings::PathChecks`)
    path_ok: settings::PathChecks,
    /// whether a Soulseek login is saved, and when that was read (Settings reads it at most every two seconds)
    slsk_login_seen: Option<(bool, f64)>,
    /// Search and fetch from Soulseek (the top of the Downloads page), set up the first time that page is opened.
    downloads: Option<downloads::DownloadsPanel>,
    library_page: Option<library_page::LibraryPage>,
    health: Option<health::HealthPage>,
    backup: Option<backup::BackupPage>,
    /// the first-start screen, while it has the window in place of the pages (see `setup`)
    setup: Option<setup::Setup>,
    /// the Music Tool's last version: its tools are in Cadence now (`retire`), said on Home and on the page
    /// the app opened on
    notice: retire::Notice,
    landing: Option<Page>,
    /// the backup tool is set up on this PC (its folder is in Settings and holds the script). Only then is
    /// there a Backup page: its name in the top row, its card on Home, its mark and Ctrl+6
    backup_on: bool,
    detail: Option<DetailView>,
    detail_cache: std::collections::HashMap<String, engine::ArtistDetail>,
    detail_tx: crossbeam_channel::Sender<(String, engine::ArtistDetail)>,
    detail_rx: Receiver<(String, engine::ArtistDetail)>,
    only_new: bool,
    auto_checked: bool,
    /// artist panel album order: 0 best, 1 most played, 2 newest
    album_sort: u8,
    /// a source failed during the current run
    incomplete: bool,
    /// Your artists filter: 0 all, 1 played but not owned, 2 owned but never played
    art_filter: u8,
    /// Browser authorization, including an in-flight request that can be cancelled.
    lastfm_auth: lastfm_auth::Flow,
    pin_ui: Option<PinUi>,
    pin_rx: Option<Receiver<Result<Vec<engine::Candidate>, String>>>,
    cfg: Config,
    net: Arc<net::Net>,
    images: images::Images,
    results: Results,
    rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    running: bool,
    status: String,
    progress: BTreeMap<&'static str, (usize, usize, bool)>,
    warnings: Vec<String>,
    errors: Vec<String>,
    tab: Tab,
    show_settings: bool,
    filter: String,
    show_hidden: bool,
    rel_sort: RelSort,
    art_sort: (usize, bool),
    /// holds the artist tiles' picture size steady while the window is being resized
    tile_settle: covers::Settle,
    copied: Option<(String, f64)>,
    /// debug: `--screenshot path.png` saves the window after a few seconds and exits
    shot: Option<String>,
    shot_done: Arc<AtomicBool>,
    /// debug: `--script "..."` clicks and scrolls through the app by itself (see `script.rs`)
    script: Option<script::Script>,
    /// the running job is a "load more" (infinite scroll), not a full refresh
    more: bool,
    /// the scanned music library from the last run, so "load more" doesn't rescan
    lib: Option<Arc<library::Library>>,
    /// set by the list views when the user has scrolled near the end
    near_bottom: bool,
    /// middle-click autoscroll: where it was started, and how far to scroll this frame
    autoscroll: Option<egui::Pos2>,
    auto: Option<(egui::Pos2, f32)>,
    /// how many color changes the window has taken up (see `theme::changes`)
    colors_seen: u64,
    /// which section of Settings is showing (see `settings::SECTIONS`)
    settings_tab: usize,
    /// what Settings keeps between frames: the music folder's card, and whether only its Look card is on
    /// show beside the pages
    settings: settings::State,
    /// "Adjust colors" is unfolded
    colors_open: bool,
    /// the colors before each change made in this run, newest last ("Undo the last change")
    color_undo: Vec<[u32; theme::ROLES]>,
    color_changed_at: f64,
    /// a Windows picker that is open: which box it is for, and where its answer arrives
    pick_rx: Option<(&'static str, Receiver<Option<std::path::PathBuf>>)>,
    /// what in Settings needs a look, and when that was last worked out
    attention_cache: (f64, Vec<(String, usize)>),
    /// what Health asked for and is waiting for, and when that was last read
    waiting_cache: (f64, Vec<Waiting>),
    /// the list of missing albums Cadence sent (see `inbox.rs`)
    inbox: inbox::Inbox,
    /// the import list was busy last frame (an import ending reads the library again for that list)
    import_was_busy: bool,
    /// when the answer for Cadence was last worked out (see `keep_cadence_answer`)
    answer_at: f64,
}

/// One album that is waited for (see `App::waiting_for`).
#[derive(Clone)]
struct Waiting {
    album: String,
    artist: String,
    /// "a better copy of the album (damaged or incomplete)"
    what: String,
    /// the words to search Soulseek for
    search: String,
    /// the album's folder (a better copy) or the artist's folder (loose tracks): what the entry is kept by
    folder: String,
    loose: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let cfg = Config::load();
        // the colors chosen in Settings
        theme::load(&cfg.colors);
        apply_colors(&cc.egui_ctx);
        // and the size everything is drawn at
        if (0.5..=3.0).contains(&cfg.size) {
            cc.egui_ctx.set_zoom_factor(cfg.size);
        }
        let net = Arc::new(net::Net::new(config::app_dir().join("cache")));
        {
            // tidy the lookup cache in the background (old answers out, size capped)
            let net = net.clone();
            std::thread::spawn(move || net.trim_cache());
        }
        let images = images::Images::new(net.clone(), cc.egui_ctx.clone());
        let mut results: Results = std::fs::read(engine::results_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        results.merge_twins(); // (results saved before this rule existed may list one release twice)
        let show_settings = std::env::args().any(|a| a == "--settings");
        let tab = match arg_after("--tab").as_deref() {
            Some("similar") => Tab::Similar,
            Some("artists") => Tab::Artists,
            _ => Tab::Releases,
        };
        let (detail_tx, detail_rx) = crossbeam_channel::unbounded();
        Self {
            page: match arg_after("--tab").as_deref() {
                // debug: `--dl-search "artist album"` opens Downloads and lists the copies (nothing is fetched)
                _ if arg_after("--dl-search").map(|q| slskd::ask_for(&q)).is_some() => Page::Import,
                Some("import") | Some("downloads") => Page::Import,
                Some("library") => Page::Library,
                Some("health") => Page::Health,
                Some("backup") => Page::Backup,
                Some("home") => Page::Home,
                Some(_) => Page::Recs,
                // no flag: the page that was open last time
                None => PAGES.iter().find(|p| p.2 == cfg.last_page).map(|p| p.0).unwrap_or(START_PAGE),
            },
            importer: None,
            // debug: `--search "text"` starts with the search box open on that text (for screenshots)
            search_open: arg_after("--search").is_some(),
            search_fresh: true,
            owned_folders: Default::default(),
            search_text: arg_after("--search").unwrap_or_default(),
            search_sel: 0,
            slsk_user: String::new(),
            slsk_pass: String::new(),
            slsk_show: false,
            slsk_loaded: false,
            slsk_msg: None,
            saved_at: None,
            cache_cleared_at: None,
            path_ok: Default::default(),
            slsk_login_seen: None,
            downloads: None,
            library_page: None,
            health: None,
            backup: None,
            setup: (!cfg.setup_done).then(setup::Setup::new),
            notice: retire::Notice::default(),
            landing: None,
            backup_on: backup::tool_dir_for(&cfg.backup_tool_dir).is_some(),
            detail: None,
            detail_cache: Default::default(),
            detail_tx,
            detail_rx,
            only_new: false,
            auto_checked: false,
            album_sort: 0,
            incomplete: false,
            art_filter: 0,
            lastfm_auth: lastfm_auth::Flow::default(),
            pin_ui: None,
            pin_rx: None,
            errors: cfg.load_error.clone().into_iter().collect(),
            cfg,
            net,
            images,
            results,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            running: false,
            status: String::new(),
            progress: BTreeMap::new(),
            warnings: vec![],
            tab,
            show_settings,
            filter: String::new(),
            show_hidden: false,
            rel_sort: RelSort::Newest,
            art_sort: (8, true),
            tile_settle: covers::Settle::default(),
            copied: None,
            shot: arg_after("--screenshot"),
            shot_done: Arc::new(AtomicBool::new(false)),
            script: script::Script::from_args(),
            more: false,
            lib: None,
            near_bottom: false,
            autoscroll: None,
            auto: None,
            colors_seen: theme::changes(),
            settings_tab: 0,
            settings: settings::State::default(),
            colors_open: false,
            color_undo: vec![],
            color_changed_at: -10.0,
            pick_rx: None,
            attention_cache: (-10.0, vec![]),
            waiting_cache: (-10.0, vec![]),
            inbox: inbox::Inbox::new(),
            import_was_busy: false,
            answer_at: -10.0,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        if self.running {
            return;
        }
        self.cfg.save();
        let (tx, rx) = crossbeam_channel::unbounded();
        self.rx = Some(rx);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.running = true;
        self.more = false;
        self.progress.clear();
        self.warnings.clear();
        self.errors.clear();
        self.incomplete = false;
        self.results.releases.clear();
        self.results.rejected.clear();
        let (cfg, net, cancel, ctx) = (self.cfg.clone(), self.net.clone(), self.cancel.clone(), ctx.clone());
        std::thread::spawn(move || engine::run(cfg, net, tx, cancel, move || ctx.request_repaint()));
    }

    /// Middle-click autoscroll, like a browser: click the wheel, then move the mouse up or down from that
    /// spot to scroll (further = faster). Click again, or press Esc, to stop.
    fn update_autoscroll(&mut self, ctx: &egui::Context) {
        use egui::PointerButton;
        let (mid, left, esc, pos, dt) = ctx.input(|i| {
            (i.pointer.button_pressed(PointerButton::Middle), i.pointer.button_pressed(PointerButton::Primary), i.key_pressed(egui::Key::Escape), i.pointer.hover_pos(), i.stable_dt)
        });
        if self.autoscroll.is_some() && esc {
            // Stopping autoscroll must not also close Settings or the open menu.
            ctx.input_mut(|i| { i.consume_key(egui::Modifiers::NONE, egui::Key::Escape); });
        }
        match self.autoscroll {
            Some(_) if mid || left || esc => self.autoscroll = None,
            None if mid => self.autoscroll = pos,
            _ => {}
        }
        self.auto = None;
        ctx.data_mut(|d| d.remove::<(egui::Pos2, f32)>(egui::Id::unique("menu-autoscroll")));
        let Some(anchor) = self.autoscroll else { return };
        ctx.request_repaint();
        ctx.set_cursor_icon(egui::CursorIcon::AllScroll);
        let dy = pos.map(|p| p.y - anchor.y).unwrap_or(0.0);
        // speed grows faster than the distance: a small nudge is a gentle scroll, a short push is a blur
        let d = (dy.abs() - 8.0).max(0.0);
        let speed = (d * 18.0 + d * d * 1.5) * dy.signum(); // points per second
        self.auto = Some((anchor, speed * dt.min(0.05)));
        ctx.data_mut(|d| d.insert_temp(egui::Id::unique("menu-autoscroll"), self.auto.unwrap()));
        // a small marker where it started
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("autoscroll")));
        painter.circle_filled(anchor, 13.0, Color32::from_black_alpha(170));
        painter.circle_stroke(anchor, 13.0, Stroke::new(1.5, TEXT()));
        painter.text(anchor, Align2::CENTER_CENTER, "↕", FontId::proportional(15.0), TEXT());
    }

    /// How far the ranking has been used: (releases checked up to, similar-artist seeds used).
    /// An older results file has no cursors, so assume the configured amounts.
    fn cursors(&self) -> (usize, usize) {
        let r = if self.results.rel_cursor == 0 { self.cfg.check_artists } else { self.results.rel_cursor };
        let s = if self.results.seed_cursor == 0 { self.cfg.similar_seeds } else { self.results.seed_cursor };
        (r, s)
    }

    /// How far back releases have been loaded (None = just the normal window).
    fn back_to(&self) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(&self.results.back_to, "%Y-%m-%d").ok()
    }

    /// Is there anything more to load?
    fn can_load_more(&self, what: engine::More) -> bool {
        let (_, s) = self.cursors();
        match what {
            engine::More::Releases => self.back_to().map(|d| d > engine::earliest()).unwrap_or(true),
            engine::More::Similar => self.cfg.has_lastfm() && s < self.results.artists.len(),
        }
    }

    /// Infinite scroll: look at the next few artists and add what turns up to the list.
    fn start_more(&mut self, what: engine::More, ctx: &egui::Context) {
        if self.running || !self.can_load_more(what) {
            return;
        }
        let (r, s) = self.cursors();
        let (tx, rx) = crossbeam_channel::unbounded();
        self.rx = Some(rx);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.running = true;
        self.more = true;
        self.progress.clear();
        let existing = self.results.similar.iter().map(|x| x.key.clone()).collect();
        let (cfg, net, cancel, lib, artists, ctx) =
            (self.cfg.clone(), self.net.clone(), self.cancel.clone(), self.lib.clone(), self.results.artists.clone(), ctx.clone());
        let from = if what == engine::More::Releases { r } else { s };
        let back_to = self.back_to();
        std::thread::spawn(move || engine::load_more(cfg, net, tx, cancel, move || ctx.request_repaint(), what, artists, from, back_to, existing, lib));
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut finished = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Status(s) => self.status = s,
                Msg::Progress { task, done, total } => {
                    self.progress.insert(task, (done, total, false));
                }
                Msg::TaskDone(task) => {
                    let e = self.progress.entry(task).or_insert((1, 1, true));
                    e.0 = e.1;
                    e.2 = true;
                }
                Msg::Artists(a) => self.results.artists = a,
                Msg::Similar(s) => self.results.similar = s,
                Msg::SimilarMore(s) => {
                    for x in s {
                        if !self.results.similar.iter().any(|y| y.key == x.key) {
                            self.results.similar.push(x);
                        }
                    }
                }
                Msg::RelCursor(n) => self.results.rel_cursor = n,
                Msg::BackTo(d) => self.results.back_to = d,
                Msg::SeedCursor(n) => self.results.seed_cursor = n,
                Msg::Lib(l) => self.lib = Some(l),
                Msg::Release(r) => {
                    if carry_hidden(&mut self.cfg.dismissed, &r.artist_key, &r.title) {
                        self.cfg.save();
                    }
                    self.results.add_release(r)
                }
                Msg::Reject(k) => self.results.reject(k),
                Msg::Warn(w) => self.warnings.push(w),
                Msg::Error(e) => self.errors.push(e),
                Msg::Incomplete => self.incomplete = true,
                Msg::Finished => finished = true,
            }
        }
        if finished {
            self.running = false;
            self.rx = None;
            if self.more {
                // Infinite scroll added to the list. Don't badge it "new" (you're scrolling for it) and
                // don't let the next full refresh call it new either.
                self.more = false;
                let today = Local::now().format("%Y-%m-%d").to_string();
                for r in &self.results.releases {
                    self.results.seen.entry(r.key.clone()).or_insert_with(|| today.clone());
                }
                for s in &self.results.similar {
                    self.results.seen.entry(format!("artist|{}", s.key)).or_insert_with(|| today.clone());
                }
                if let Ok(b) = serde_json::to_vec(&self.results) {
                    let _ = util::write_atomic(&engine::results_path(), &b);
                }
                return;
            }
            let old = std::fs::read(engine::results_path()).ok().and_then(|b| serde_json::from_slice::<Results>(&b).ok());
            if !self.cancel.load(Ordering::Relaxed) {
                if let (true, Some(old)) = (self.incomplete, old) {
                    // A source went down part-way: keep last time's finds so nothing silently disappears.
                    for r in old.releases {
                        merge_release(&mut self.results.releases, r);
                    }
                    if self.results.similar.is_empty() {
                        self.results.similar = old.similar;
                    }
                }
                self.results.mark_new();
                if let Ok(b) = serde_json::to_vec(&self.results) {
                    let _ = util::write_atomic(&engine::results_path(), &b);
                }
            } else if let Some(old) = old {
                // stopped part-way: go back to the last complete results
                self.results = old;
            }
        }
    }

    /// Results from Last.fm sign-in and the "Wrong artist?" picker. Kept separate from
    /// `poll`, which only runs while a refresh is in progress.
    fn poll_side_tasks(&mut self, ctx: &egui::Context) {
        if self.lastfm_auth.tick(&mut self.cfg, ctx) { self.lastfm_changed(); }
        if let Some(rx) = &self.pin_rx {
            if let Ok(r) = rx.try_recv() {
                if let Some(p) = &mut self.pin_ui {
                    p.results = Some(r);
                    p.loading = false;
                }
                self.pin_rx = None;
            }
        }
    }

    /// Look up MusicBrainz artists that could be `query`, in the background.
    fn start_pin_search(&mut self, artist_name: &str, query: String, ctx: &egui::Context) {
        let key = util::artist_key(artist_name);
        let owned: Vec<String> = self.results.artists.iter().find(|a| a.key == key).map(|a| a.album_keys.clone()).unwrap_or_default();
        let (tx, rx) = crossbeam_channel::unbounded();
        self.pin_rx = Some(rx);
        self.pin_ui = Some(PinUi { query: query.clone(), loading: true, results: None });
        let (net, ctx) = (self.net.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(engine::find_candidates(&net, &query, &owned));
            ctx.request_repaint();
        });
    }

    /// After a pin changes: forget cached details, drop releases that came only from the old
    /// MusicBrainz artist (the next refresh fetches the right ones), and reload the panel.
    fn after_pin_change(&mut self, key: &str, ctx: &egui::Context) {
        self.cfg.save();
        self.detail_cache.remove(key);
        let mbid = self.cfg.pinned_mbid(key).map(|s| s.to_string());
        if let Some(a) = self.results.artists.iter_mut().find(|a| a.key == key) {
            if let Some(m) = &mbid {
                a.mbid = m.clone();
            }
        }
        let before = self.results.releases.len();
        self.results.releases.retain(|r| !(r.artist_key == key && r.apple_url.is_empty()));
        if self.results.releases.len() != before {
            if let Ok(b) = serde_json::to_vec(&self.results) {
                let _ = util::write_atomic(&engine::results_path(), &b);
            }
        }
        self.pin_ui = None;
        if let Some(v) = &self.detail {
            let (name, because) = (v.name.clone(), v.because.clone());
            self.open_artist(name, because, ctx);
        }
    }

    /// Personal recommendations must be refreshed after changing the Last.fm account.
    fn lastfm_changed(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.rx = None;
        self.running = false;
        self.more = false;
        self.progress.clear();
        self.status.clear();
        self.warnings.clear();
        self.errors.clear();
        (self.detail_tx, self.detail_rx) = crossbeam_channel::unbounded();
        self.results = Default::default();
        self.detail_cache.clear();
        self.detail = None;
        let _ = std::fs::remove_file(engine::results_path());
        self.attention_cache.0 = -10.0;
    }

    fn open_artist(&mut self, name: String, because: Vec<String>, ctx: &egui::Context) {
        self.show_settings = false;
        self.pin_ui = None;
        let key = util::artist_key(&name);
        let data = self.detail_cache.get(&key).cloned();
        if data.is_none() {
            let stat = self.results.artists.iter().find(|a| a.key == key);
            let owned: Vec<String> = stat.map(|a| a.album_keys.clone()).unwrap_or_default();
            let mbid = stat
                .map(|a| a.mbid.clone())
                .filter(|m| !m.is_empty())
                .or_else(|| self.results.similar.iter().find(|s| s.key == key).map(|s| s.mbid.clone()))
                .unwrap_or_default();
            let (cfg, net, tx, ctx, n) = (self.cfg.clone(), self.net.clone(), self.detail_tx.clone(), ctx.clone(), name.clone());
            std::thread::spawn(move || {
                let d = engine::load_detail(&cfg, &net, &n, &mbid, &owned);
                let _ = tx.send((util::artist_key(&n), d));
                ctx.request_repaint();
            });
        }
        self.detail = Some(DetailView { name, because, data });
    }

    fn poll_detail(&mut self) {
        while let Ok((key, d)) = self.detail_rx.try_recv() {
            if let Some(v) = &mut self.detail {
                if util::artist_key(&v.name) == key {
                    v.data = Some(d.clone());
                }
            }
            if !d.failed {
                self.detail_cache.insert(key, d); // failed loads are retried next time it's opened
            }
        }
    }

    fn apply(&mut self, actions: Vec<Action>, ctx: &egui::Context) {
        for a in actions {
            match a {
                Action::Open(u) => {
                    util::open_url(&u);
                }
                Action::Album(artist, title, fallback, mb) => {
                    let net = self.net.clone();
                    std::thread::spawn(move || {
                        util::open_url(&albumlink::find(&net, &artist, &title, &fallback, &mb));
                    });
                }
                Action::Bandcamp(artist, title, mb) => {
                    let net = self.net.clone();
                    std::thread::spawn(move || {
                        let url = albumlink::bandcamp(&net, &artist, &title, &mb)
                            .unwrap_or_else(|| format!("https://bandcamp.com/search?q={}&item_type=a", util::urlenc(&format!("{artist} {}", util::strip_edition(&title)))));
                        util::open_url(&url);
                    });
                }
                Action::Copy(t) => {
                    ctx.copy_text(t.clone());
                    self.copied = Some((t, ctx.input(|i| i.time)));
                }
                Action::Hide(k) => {
                    self.cfg.dismissed.insert(k);
                    self.cfg.save();
                }
                Action::Unhide(k) => {
                    self.cfg.dismissed.remove(&k);
                    self.cfg.save();
                }
                Action::Artist(name, because) => self.open_artist(name, because, ctx),
                Action::Mute(key, name) => {
                    self.cfg.muted.insert(key, name);
                    self.cfg.save();
                }
                Action::Unmute(key) => {
                    self.cfg.muted.remove(&key);
                    self.cfg.save();
                }
                Action::FixMatch(name) => {
                    self.open_artist(name.clone(), vec![], ctx);
                    self.start_pin_search(&name, name.clone(), ctx);
                }
                Action::Pin(key, pin) => {
                    self.cfg.pins.insert(key.clone(), pin);
                    self.after_pin_change(&key, ctx);
                }
                Action::Unpin(key) => {
                    self.cfg.pins.remove(&key);
                    self.after_pin_change(&key, ctx);
                }
            }
        }
    }

    // ---------------------------------------------------------------- settings

    /// Whether a folder or file from Settings is there: the last answer, looked at beside the window, so a
    /// sleeping drive or a share that is gone never stalls it.
    fn path_found(&mut self, which: &'static str, path: &str, now: f64) -> bool {
        self.path_ok.found(which, path, now, true, |p| p.exists(), None)
    }

    /// What in Settings needs a look, each with the section it is in (see `settings::SECTIONS`). Worked out at most
    /// every two seconds, so the top row can ask on every draw.
    fn attention(&mut self, now: f64) -> Vec<(String, usize)> {
        if now - self.attention_cache.0 < 2.0 {
            return self.attention_cache.1.clone();
        }
        // only what was set up and has stopped working: a login or a tool that was left out is no fault
        // (see `settings::needs_a_look`)
        let dir = self.cfg.music_dir.clone();
        let music = (!dir.trim().is_empty()).then(|| self.path_found("music", &dir, now));
        let login = slskd::read_settings().map(|s| s.has_login).unwrap_or(false);
        let client = slskd::exe().exists() || helpers::busy(helpers::Helper::Soulseek);
        let folder = backup::tool_dir_for(&self.cfg.backup_tool_dir).map(|d| d.to_string_lossy().to_string());
        let backup = folder.map(|s| self.path_found("backup-tool", &s, now));
        let out = settings::needs_a_look(music, (login, client), backup);
        self.attention_cache = (now, out.clone());
        out
    }

    /// Opens the Windows picker for one of the folders in Settings (or, for the artist name list, a file).
    /// It runs beside the app; what was chosen comes back through `pick_rx` and is taken up by the Settings
    /// page. (The music folder has its own, in its card: `cards::FolderCard::choose`.)
    fn pick(&mut self, which: &'static str, start: &str, folder: bool, ctx: &egui::Context) {
        if self.pick_rx.is_some() {
            return; // one picker at a time
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.pick_rx = Some((which, rx));
        // (a test run names what is chosen instead of opening the Windows picker, which a script cannot
        // work: `MUSIC_RECS_PICK`)
        if let Some(chosen) = cards::test_switch("MUSIC_RECS_PICK") {
            let _ = tx.send(Some(std::path::PathBuf::from(chosen)));
            return;
        }
        let (start, ctx) = (std::path::PathBuf::from(start.trim()), ctx.clone());
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            // start where the box points now, when that is still there
            let here = if folder { Some(start.clone()) } else { start.parent().map(|p| p.to_path_buf()) };
            if let Some(dir) = here.filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            let found = if folder { dialog.pick_folder() } else { dialog.add_filter("Text", &["txt"]).pick_file() };
            let _ = tx.send(found);
            ctx.request_repaint();
        });
    }

    /// What the last refresh had to say (a source that failed, and its smaller notes), over the list and
    /// under the page's top, so the top stays where it is when a refresh brings one.
    fn run_notes(&self, ui: &mut Ui) {
        for e in &self.errors {
            Frame::new().fill(theme::blend(BG(), theme::RED(), 0.22)).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(e).color(TEXT()));
            });
            ui.add_space(6.0);
        }
        if !self.warnings.is_empty() {
            let n = self.warnings.len();
            let head = if n == 1 { "1 note from the last run".to_string() } else { format!("{n} notes from the last run") };
            egui::CollapsingHeader::new(RichText::new(head).color(DIM()).small()).id_salt("warnings").show(ui, |ui| {
                ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                    for w in &self.warnings {
                        ui.label(RichText::new(w).small().color(DIM()));
                    }
                });
            });
            ui.add_space(4.0);
        }
    }

    // ---------------------------------------------------------------- releases

    fn releases_view(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut actions = vec![];
        let mut copy_list = false;
        ui.horizontal(|ui| {
            theme::centered_row(ui, "recs-releases-row", 80.0, |ui| {
                ui.add(TextEdit::singleline(&mut self.filter).id(filter_id()).hint_text("Filter").desired_width(200.0));
                ui.add_space(8.0);
                for (i, name) in ["Albums", "EPs", "Singles", "Other"].iter().enumerate() {
                    if crate::theme::toggle(ui, &mut self.cfg.view_kinds[i], *name).clicked() {
                        self.cfg.save(); // remember which types you like to see
                    }
                }
                ui.add_space(14.0);
                crate::theme::choice(ui, &mut self.rel_sort, RelSort::Newest, "Newest");
                crate::theme::choice(ui, &mut self.rel_sort, RelSort::Favorite, "Favorites");
                ui.add_space(14.0);
                // (counted the way the list is filtered, so the number is what switching it on shows)
                let new = self.new_releases(self.show_hidden);
                if new > 0 || self.only_new {
                    new_toggle(ui, &mut self.only_new, new);
                }
                crate::theme::toggle(ui, &mut self.show_hidden, "Hidden");
            });
            copy_list = ui.with_layout(Layout::right_to_left(Align::Center), |ui| theme::quiet(ui, "Copy list", 13.5, true).clicked()).inner;
        });
        recs_top::line(self, ui);
        self.run_notes(ui);

        let today = Local::now().date_naive();
        let f = self.filter.to_lowercase();
        let mut list: Vec<ReleaseRec> = self
            .results
            .releases
            .iter()
            .filter(|r| self.show_hidden || (!self.cfg.dismissed.contains(&r.key) && !self.cfg.muted.contains_key(&r.artist_key)))
            .filter(|r| !self.only_new || self.results.new_keys.contains(&r.key))
            .filter(|r| self.kind_shown(&r.kind))
            .filter(|r| f.is_empty() || r.title.to_lowercase().contains(&f) || r.credit.to_lowercase().contains(&f) || r.artist.to_lowercase().contains(&f))
            .cloned()
            .collect();
        match self.rel_sort {
            RelSort::Newest => list.sort_by(|a, b| b.date.cmp(&a.date).then(b.artist_weight.total_cmp(&a.artist_weight))),
            RelSort::Favorite => list.sort_by(|a, b| b.artist_weight.total_cmp(&a.artist_weight).then(b.date.cmp(&a.date))),
        }

        if list.is_empty() {
            empty_state(
                ui,
                if self.running {
                    "Looking for new releases…"
                } else if self.results.releases.is_empty() {
                    "Press Refresh to look for new music from the artists you like."
                } else {
                    "Nothing matches these filters."
                },
            );
            return actions;
        }

        // the whole list as "Artist - Title" lines (e.g. to paste into a Soulseek wishlist)
        if copy_list {
            let text: Vec<String> = list.iter().map(|r| format!("{} - {}", r.artist, util::strip_edition(&r.title))).collect();
            actions.push(Action::Copy(text.join("\n")));
        }

        // group into "Upcoming" and months (newest sort) or one group (favorites sort)
        let mut groups: Vec<(String, Vec<ReleaseRec>)> = vec![];
        for r in list {
            let g = if self.rel_sort == RelSort::Favorite {
                String::new()
            } else {
                // "2026" alone means sometime this year, so only call it upcoming if even Jan 1 is in the future
                match util::parse_date_earliest(&r.date) {
                    Some(d) if d > today => "Upcoming".into(),
                    _ => month_label(&r.date),
                }
            };
            match groups.last_mut() {
                Some((name, items)) if *name == g => items.push(r),
                _ => groups.push((g, vec![r])),
            }
        }

        // infinite scroll only makes sense when nothing is narrowing the list down
        let scrolling = self.filter.is_empty() && !self.only_new;
        let (can_more, loading) = (self.can_load_more(engine::More::Releases), self.running && self.more);
        let mut near = false;
        let auto = self.auto;
        ScrollArea::vertical().id_salt("recs-releases-scroll").auto_shrink([false, false]).show(ui, |ui| {
            auto_scroll(ui, auto);
            for (n, (name, items)) in groups.iter().enumerate() {
                if !name.is_empty() {
                    recs_grid::group_head(ui, name, if name == "Upcoming" { ACCENT() } else { TEXT() }, n == 0);
                }
                self.release_tiles(ui, items, today, &mut actions);
            }
            if scrolling {
                near = more_footer(ui, can_more, loading, "That's every artist you listen to.");
            }
            ui.add_space(20.0);
        });
        self.near_bottom = near;
        actions
    }


    // ---------------------------------------------------------------- similar artists

    fn similar_view(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut actions = vec![];
        ui.horizontal(|ui| {
            theme::centered_row(ui, "recs-similar-row", 0.0, |ui| {
                ui.add(TextEdit::singleline(&mut self.filter).id(filter_id()).hint_text("Filter").desired_width(200.0));
                ui.add_space(8.0);
                let new = self.new_artists(self.show_hidden);
                if new > 0 || self.only_new {
                    new_toggle(ui, &mut self.only_new, new);
                }
                crate::theme::toggle(ui, &mut self.show_hidden, "Hidden");
            });
        });
        recs_top::line(self, ui);
        self.run_notes(ui);
        let f = self.filter.to_lowercase();
        let list: Vec<SimilarRec> = self
            .results
            .similar
            .iter()
            .filter(|s| self.show_hidden || !self.cfg.dismissed.contains(&format!("artist|{}", s.key)))
            .filter(|s| !self.only_new || self.results.new_artist_keys.contains(&format!("artist|{}", s.key)))
            .filter(|s| {
                f.is_empty()
                    || s.name.to_lowercase().contains(&f)
                    || s.tags.iter().any(|t| t.contains(&f))
                    || s.because.iter().any(|b| b.0.to_lowercase().contains(&f))
            })
            .cloned()
            .collect();
        if list.is_empty() {
            empty_state(
                ui,
                if self.running {
                    "Asking Last.fm for similar artists…"
                } else if !self.cfg.has_lastfm() {
                    "Similar artists come from Last.fm. Add your username and API key in Settings."
                } else if self.results.similar.is_empty() {
                    "Press Refresh to find artists you might like."
                } else {
                    "Nothing matches this filter."
                },
            );
            return actions;
        }
        let top = self.results.similar.first().map(|s| s.score).unwrap_or(1.0).max(1e-9);
        let scrolling = self.filter.is_empty() && !self.only_new;
        let (can_more, loading) = (self.can_load_more(engine::More::Similar), self.running && self.more);
        let mut near = false;
        let auto = self.auto;
        ScrollArea::vertical().id_salt("recs-similar-scroll").auto_shrink([false, false]).show(ui, |ui| {
            auto_scroll(ui, auto);
            let dismissed = &self.cfg.dismissed;
            let new = &self.results.new_artist_keys;
            let hidden = |s: &SimilarRec| dismissed.contains(&format!("artist|{}", s.key));
            let is_new = |s: &SimilarRec| new.contains(&format!("artist|{}", s.key));
            recs_artists::similar_tiles(ui, &list, top, &mut self.images, &mut self.tile_settle, &hidden, &is_new, &mut actions);
            if scrolling {
                near = more_footer(ui, can_more, loading, "That's all the artists Last.fm could suggest.");
            }
            ui.add_space(20.0);
        });
        self.near_bottom = near;
        actions
    }

    // ---------------------------------------------------------------- your artists

    fn artists_view(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut actions = vec![];
        let auto = self.auto;
        ui.horizontal(|ui| {
            theme::centered_row(ui, "recs-artists-row", 0.0, |ui| {
                ui.add(TextEdit::singleline(&mut self.filter).id(filter_id()).hint_text("Filter").desired_width(200.0));
                ui.add_space(8.0);
                // what the tiles are ordered by, which is also what the line under each picture measures
                for (col, name) in [(8, "Score"), (4, "Plays"), (5, "This year"), (6, "Lately"), (2, "Albums"), (0, "Name")] {
                    if theme::word(ui, name, 14.0, self.art_sort.0 == col).clicked() {
                        self.art_sort = (col, col != 0);
                    }
                    ui.add_space(8.0);
                }
                ui.add_space(14.0);
                crate::theme::choice(ui, &mut self.art_filter, 0, "All");
                crate::theme::choice(ui, &mut self.art_filter, 1, "Played, not owned");
                crate::theme::choice(ui, &mut self.art_filter, 2, "Owned, never played");
            });
        });
        recs_top::line(self, ui);
        self.run_notes(ui);
        if self.results.artists.is_empty() {
            empty_state(ui, "Press Refresh to scan your library.");
            return actions;
        }
        let f = self.filter.to_lowercase();
        let mut list: Vec<(usize, &engine::ArtistStat)> = self
            .results
            .artists
            .iter()
            .enumerate()
            .filter(|(_, a)| f.is_empty() || a.name.to_lowercase().contains(&f))
            .filter(|(_, a)| match self.art_filter {
                1 => !a.in_library && a.plays_all > 0,
                2 => a.in_library && a.plays_all == 0 && a.plays_12m == 0,
                _ => true,
            })
            .collect();
        let (col, desc) = self.art_sort;
        list.sort_by(|(_, a), (_, b)| {
            let o = match col {
                0 => b.name.to_lowercase().cmp(&a.name.to_lowercase()),
                1 => a.in_library.cmp(&b.in_library),
                2 => a.albums.cmp(&b.albums),
                3 => a.tracks.cmp(&b.tracks),
                4 => a.plays_all.cmp(&b.plays_all),
                5 => a.plays_12m.cmp(&b.plays_12m),
                6 => a.plays_3m.cmp(&b.plays_3m),
                7 => a.plays_recent.cmp(&b.plays_recent),
                _ => a.weight.total_cmp(&b.weight),
            };
            if desc { o.reverse() } else { o }
        });
        if list.is_empty() {
            empty_state(ui, "Nothing matches this filter.");
            return actions;
        }
        // the line under each picture: this artist's share of the most there is of whatever the list is sorted
        // by (plays fall off so steeply that their square root is what stays readable)
        let value: Box<dyn Fn(&engine::ArtistStat) -> f64> = match col {
            4 => Box::new(|a| (a.plays_all as f64).sqrt()),
            5 => Box::new(|a| (a.plays_12m as f64).sqrt()),
            6 => Box::new(|a| (a.plays_3m as f64).sqrt()),
            2 => Box::new(|a| a.albums as f64),
            _ => Box::new(|a| a.weight),
        };
        let top = self.results.artists.iter().map(|a| value(a)).fold(0.0f64, f64::max).max(1e-9);
        let bar = |a: &engine::ArtistStat| (value(a) / top) as f32;
        let plays = |n: u64| if n == 0 { "no plays".to_string() } else { format!("{} plays", recs_artists::thousands(n)) };
        let second = |a: &engine::ArtistStat| match col {
            4 => plays(a.plays_all),
            5 => plays(a.plays_12m),
            6 => plays(a.plays_3m),
            _ if a.in_library => format!("{} {}", a.albums, if a.albums == 1 { "album" } else { "albums" }),
            _ => plays(a.plays_all),
        };
        let shown: Vec<&engine::ArtistStat> = list.iter().map(|(_, a)| *a).collect();
        ScrollArea::vertical().id_salt("recs-artists-scroll").auto_shrink([false, false]).show(ui, |ui| {
            auto_scroll(ui, auto);
            recs_artists::artist_tiles(ui, &shown, &mut self.images, self.library_page.as_mut(), &mut self.tile_settle, &bar, &second, &mut actions);
            ui.add_space(20.0);
        });
        actions
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(s) = &mut self.script {
            s.feed(raw_input);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        theme::note_typing(&ctx); // (before anything is drawn: see `theme::typing`)
        // new versions: the worker starts with the first frame (its first look is five seconds later) and
        // hears here when the choice in Settings changes
        update::start(&ctx, update::Policy::from_saved(&self.cfg.updates));
        // once, after the app went back from a new version that could not start: said on whatever page is
        // open, for the first 20 seconds
        if let Some(words) = update::notice() {
            if ctx.input(|i| i.time) < 20.0 {
                egui::Area::new(egui::Id::unique("update-went-back")).order(egui::Order::Foreground).anchor(Align2::CENTER_BOTTOM, vec2(0.0, -24.0)).show(&ctx, |ui| {
                    Frame::new().fill(CARD_HOVER()).corner_radius(8).inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
                        ui.set_max_width(560.0);
                        ui.label(RichText::new(words).color(TEXT()));
                    });
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
        }
        // the Soulseek client saves where Settings says (before the Downloads page can start it)
        slskd::want_downloads(&self.cfg.downloads_dir);
        if let Some(s) = &mut self.script {
            s.tick(&ctx);
        }
        if self.shot.is_some() && !self.running && self.rx.is_none() && std::env::args().any(|a| a == "--refresh") && ctx.input(|i| i.time) < 1.0 {
            self.start(&ctx);
        }
        if !self.auto_checked {
            if let Some(a) = arg_after("--artist") {
                self.open_artist(a, vec![], &ctx);
            }
            if let Some(a) = arg_after("--fix-match") {
                self.apply(vec![Action::FixMatch(a)], &ctx);
            }
        }
        self.update_autoscroll(&ctx);
        self.poll();
        self.poll_detail();
        self.poll_side_tasks(&ctx);
        self.images.poll(&ctx);
        // the first start: one screen in place of the pages, until "Start" is clicked there
        if self.setup.is_some() {
            window_edges(&ctx);
            Panel::top("pages").frame(Frame::new().fill(BG2()).inner_margin(Margin::ZERO)).show(ui, |ui| self.bare_bar(ui));
            // (someone starting the Music Tool for the first time hears first that its tools are in Cadence)
            Panel::top("now-in-cadence")
                .frame(Frame::new().fill(retire::strip_fill()).inner_margin(Margin { left: theme::PAGE_MARGIN, right: theme::PAGE_MARGIN, top: theme::PAGE_MARGIN, bottom: 0 }))
                .show_separator_line(false)
                .show(ui, |ui| self.notice.show(ui));
            setup::screen(self, ui);
            covers::end_frame(&ctx);
            rows::note_popups(&ctx); // (whether a menu is open as this frame ends: `rows::popup_until_now`)
            self.debug_screenshot(&ctx);
            return;
        }
        // A chosen settings folder enables the Backup page.
        self.backup_on = backup::tool_dir_for(&self.cfg.backup_tool_dir).is_some();
        if self.page == Page::Backup && !self.backup_on {
            self.page = START_PAGE;
        }
        if self.page == Page::Recs && !self.search_open && !self.settings_page() {
            self.shortcuts(&ctx);
        }
        // the importer keeps working while another page is showing
        if let Some(Ok(imp)) = &mut self.importer {
            imp.tick(&ctx);
        }
        if slskd::take_open_settings() {
            self.open_settings(settings::BASICS); // the Soulseek login is under Basics
        }
        // one page asked to show something on another: go there
        if let Some(go) = links::take() {
            match go {
                links::Go::LibraryAlbum { artist, album } => {
                    self.go_to(Page::Library);
                    self.library_page.get_or_insert_with(|| library_page::LibraryPage::new(&ctx)).show_album(&artist, &album);
                }
                links::Go::HealthAlbum { artist, album } => {
                    self.go_to(Page::Health);
                    let h = self.health.get_or_insert_with(|| health::HealthPage::new(&ctx));
                    h.ensure_loaded(&ctx, &self.cfg);
                    h.show_album(&artist, &album);
                }
                links::Go::RecsArtist { artist } => {
                    self.go_to(Page::Recs);
                    self.open_artist(artist, vec![], &ctx);
                }
                links::Go::SettingsBasics => self.open_settings(settings::BASICS),
                links::Go::ChooseMusicFolder => {
                    self.open_settings(settings::BASICS);
                    self.settings.folder.choose(&ctx);
                }
                links::Go::SettingsFiles => self.open_settings(settings::FILES),
            }
        }
        // the last health check is read at start, so the page bar and the Library can show what it found
        if !self.auto_checked {
            self.health.get_or_insert_with(|| health::HealthPage::new(&ctx)).ensure_loaded(&ctx, &self.cfg);
            // the library is read in the background at start too, for Home and the search box
            self.library_page.get_or_insert_with(|| library_page::LibraryPage::new(&ctx)).ensure_scanned(&ctx, &self.cfg);
        }
        // Ctrl+1, Ctrl+2, ... switch pages
        const DIGITS: [egui::Key; 6] = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6];
        if let Some(i) = ctx.input(|i| if i.modifiers.command { DIGITS.iter().position(|k| i.key_pressed(*k)) } else { None }) {
            if let Some(p) = self.pages().get(i) {
                self.go_to(p.0);
            }
        }
        // another page asked for a Soulseek search: go to Downloads and list the copies (nothing is fetched yet)
        if let Some(q) = slskd::take_asked() {
            self.page = Page::Import;
            self.downloads.get_or_insert_with(|| downloads::DownloadsPanel::new(&ctx)).search(&q);
        }
        // Cadence sent a newer list of missing albums: the Downloads page opens on it, once (not over the
        // first-start screen)
        if self.inbox.tick(ctx.input(|i| i.time)) && self.cfg.setup_done {
            self.go_to(Page::Import);
            if let Some(d) = &mut self.downloads {
                d.show_import_list();
            }
        }
        // Cadence's list: the library read for it when due, and the answer for Cadence kept up to date
        self.keep_cadence_answer(&ctx);
        if let Some(d) = &mut self.downloads {
            d.tick(&ctx);
            // folders that just arrived complete: tell the importer they are done (so it does not wait for them
            // to "settle") and have it look again. If it is busy, this is tried again next frame.
            if !d.arrived().is_empty() {
                if let Some(Ok(imp)) = &mut self.importer {
                    for name in d.arrived() {
                        imp.mark_settled(name);
                    }
                    if imp.rescan(&ctx) {
                        d.arrived_handed_over();
                    }
                }
            }
        }
        if let Some(p) = &mut self.library_page {
            p.tick(&ctx);
        }
        if let Some(p) = &mut self.health {
            p.tick(&ctx);
        }
        // (the backup's state is read once at the start, so the top row can say that something needs a look)
        let backup = self.backup.get_or_insert_with(|| backup::BackupPage::new(&ctx));
        backup.load_once(&ctx, &self.cfg);
        backup.tick(&ctx);
        if !self.auto_checked {
            self.auto_checked = true;
            let age = Local::now().timestamp() - self.results.generated_ts;
            if self.cfg.auto_refresh && self.cfg.has_lastfm() && self.shot.is_none() && age > 12 * 3600 {
                self.start(&ctx);
            }
        }

        window_edges(&ctx);
        // the size was changed with Ctrl and plus, minus or 0: it is kept like one chosen in Settings
        if (ctx.zoom_factor() - self.cfg.size).abs() > 0.001 {
            self.cfg.size = ctx.zoom_factor();
            self.cfg.save();
        }
        // Ctrl+1 to Ctrl+6: the pages, in the order of the top row
        const NUMBERS: [egui::Key; 6] = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6];
        for (n, key) in NUMBERS.iter().enumerate() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, *key)) {
                if let Some(p) = self.pages().get(n) {
                    self.go_to(p.0);
                }
            }
        }
        // a color was changed in Settings: egui's own parts take it up too
        if theme::changes() != self.colors_seen {
            self.colors_seen = theme::changes();
            apply_colors(&ctx);
        }
        Panel::top("pages").frame(Frame::new().fill(BG2()).inner_margin(Margin::ZERO)).show(ui, |ui| self.page_bar(ui));
        // the tools are in Cadence now: said on Home and on the page the app opened on (not in Settings)
        let landing = *self.landing.get_or_insert(self.page);
        if !self.settings_page() && (self.page == Page::Home || self.page == landing) {
            Panel::top("now-in-cadence")
                .frame(Frame::new().fill(retire::strip_fill()).inner_margin(Margin { left: theme::PAGE_MARGIN, right: theme::PAGE_MARGIN, top: theme::PAGE_MARGIN, bottom: 0 }))
                .show_separator_line(false)
                .show(ui, |ui| self.notice.show(ui));
        }
        // Settings takes the window in place of the pages (`settings::page`). Escape leaves: from the Look
        // card beside the pages back to the whole of Settings, from Settings back to the page. Not while a
        // picker is open, and not the Escape that only leaves a box that was being typed in
        if self.show_settings && !rows::popup_until_now(&ctx) && !theme::typing_until_now(&ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.settings.beside {
                self.settings.beside = false;
            } else {
                self.show_settings = false;
            }
        }
        if !self.show_settings {
            self.settings.closed();
        }
        if self.settings_page() {
            settings::page(self, ui);
            self.search_overlay(&ctx);
            covers::end_frame(&ctx);
            rows::note_popups(&ctx); // (whether a menu is open as this frame ends: `rows::popup_until_now`)
            self.debug_screenshot(&ctx);
            return;
        }
        // under "Look", "Try it on the pages" leaves only the Look card, in a strip at the right: the
        // pages are what the colors are judged on, so they stay free to walk through beside it
        let mut look_open = self.show_settings;
        Panel::right("look-beside")
            .resizable(false)
            .drag_to_open(false)
            .exact_size(settings::BESIDE)
            .show_separator_line(false)
            .frame(Frame::new().fill(BG()).inner_margin(Margin { left: 0, right: theme::PAGE_MARGIN, top: theme::PAGE_MARGIN, bottom: theme::PAGE_MARGIN }))
            .show_collapsible(ui, &mut look_open, |ui| settings::beside(self, ui));
        if self.page != Page::Recs {
            match self.page {
                Page::Home => self.home_page(ui),
                Page::Import => self.import_page(ui),
                Page::Library => self.library_page.get_or_insert_with(|| library_page::LibraryPage::new(&ctx)).ui(ui, &self.cfg, &self.results),
                Page::Health => self.health.get_or_insert_with(|| health::HealthPage::new(&ctx)).ui(ui, &self.cfg),
                Page::Backup => self.backup.get_or_insert_with(|| backup::BackupPage::new(&ctx)).ui(ui, &self.cfg),
                Page::Recs => {}
            }
            self.search_overlay(&ctx);
            covers::end_frame(&ctx);
            rows::note_popups(&ctx); // (whether a menu is open as this frame ends: `rows::popup_until_now`)
            self.debug_screenshot(&ctx);
            return;
        }
        let mut actions = vec![];
        if self.detail.is_some() && !self.show_settings {
            Panel::right("artist")
                .resizable(true)
                .default_size(500.0)
                .min_size(380.0)
                .max_size(760.0)
                .frame(Frame::new().fill(BG2()).inner_margin(Margin::same(16)))
                .show(ui, |ui| self.detail_panel(ui, &mut actions));
        }
        CentralPanel::default().frame(Frame::new().fill(BG()).inner_margin(Margin::same(16))).show(ui, |ui| {
            recs_top::top(self, ui);
            self.near_bottom = false;
            match self.tab {
                Tab::Releases => actions.extend(self.releases_view(ui)),
                Tab::Similar => actions.extend(self.similar_view(ui)),
                Tab::Artists => actions.extend(self.artists_view(ui)),
            }
        });
        if self.near_bottom && !self.running && self.shot.is_none() {
            match self.tab {
                Tab::Releases => self.start_more(engine::More::Releases, &ctx),
                Tab::Similar => self.start_more(engine::More::Similar, &ctx),
                Tab::Artists => {}
            }
        }
        if let Some((t, at)) = &self.copied {
            if ctx.input(|i| i.time) - at < 2.0 {
                egui::Area::new(egui::Id::unique("toast")).anchor(Align2::CENTER_BOTTOM, vec2(0.0, -24.0)).show(&ctx, |ui| {
                    Frame::new().fill(CARD_HOVER()).corner_radius(8).inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
                        ui.label(RichText::new(format!("Copied \"{t}\"")).color(TEXT()));
                    });
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            } else {
                self.copied = None;
            }
        }
        self.apply(actions, &ctx);
        self.search_overlay(&ctx);
        covers::end_frame(&ctx);
        rows::note_popups(&ctx); // (whether a menu is open as this frame ends: `rows::popup_until_now`)
        self.debug_screenshot(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.inbox.can_answer() {
            let steps = self.cadence_steps();
            self.inbox.answer_with(&steps, true);
        }
        if let Some(Ok(imp)) = &mut self.importer {
            imp.on_exit();
        }
        if let Some(d) = &mut self.downloads {
            d.on_exit();
        }
    }
}

impl App {
    /// The row of page names across the top of the window: plain words, the current one underlined.
    /// Switch pages, and remember the choice for the next time the app opens.
    /// The pages there are on this PC, in the order of the top row: all of them, less Backup where the
    /// backup tool is not set up.
    pub(crate) fn pages(&self) -> Vec<(Page, &'static str, &'static str)> {
        PAGES.iter().copied().filter(|p| p.0 != Page::Backup || self.backup_on).collect()
    }

    /// The top row while the first-start screen has the window: the mark, the app's name, and the window
    /// controls. It drags the window as the row of page names does.
    fn bare_bar(&mut self, ui: &mut Ui) {
        let bar = Rect::from_min_size(ui.max_rect().min, vec2(ui.max_rect().width(), BAR_H));
        let grip = ui.interact(bar, egui::Id::unique("window-title-bar"), Sense::click_and_drag());
        if grip.double_clicked() {
            let full = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!full));
        } else if grip.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let painter = ui.painter().clone();
        let mid = bar.center().y;
        painter.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, theme::BORDER().gamma_multiply(0.55)));
        icon::paint_mark(ui, Rect::from_center_size(egui::pos2(bar.left() + 28.0, mid), vec2(22.0, 22.0)), 1.0);
        painter.text(egui::pos2(bar.center().x, mid), Align2::CENTER_CENTER, "Bradley's Music Tool", FontId::proportional(13.0), theme::FAINT());
        let mut right = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
        window_controls(&mut right);
        ui.advance_cursor_after_rect(bar);
    }

    /// Something is running that a restart would cut off: a health check, an import, a backup, a Recs
    /// refresh, or a download on its way.
    pub(crate) fn busy(&self) -> bool {
        self.running
            || self.health.as_ref().is_some_and(|h| h.job().is_some())
            || matches!(&self.importer, Some(Ok(imp)) if imp.job().is_some())
            || self.backup.as_ref().is_some_and(|b| b.job().is_some())
            || self.downloads.as_ref().is_some_and(|d| d.arriving_count() > 0)
    }

    /// Settings has the window in place of the pages (not when only its Look card stands beside them).
    pub(crate) fn settings_page(&self) -> bool {
        self.show_settings && !self.settings.beside
    }

    /// Opens Settings, whole, at one of its sections (see `settings::SECTIONS`).
    pub(crate) fn open_settings(&mut self, section: usize) {
        self.show_settings = true;
        self.settings.beside = false;
        self.settings_tab = section;
    }

    pub(crate) fn go_to(&mut self, page: Page) {
        // a page was asked for: Settings gives the window back (its Look card beside the pages stays)
        if self.settings_page() {
            self.show_settings = false;
        }
        if self.page != page {
            self.page = page;
            if let Some((_, _, key)) = PAGES.iter().find(|p| p.0 == page) {
                self.cfg.last_page = key.to_string();
                self.cfg.save();
            }
        }
    }

    /// Whether releases of this kind are switched on in the Recs filter row (Albums, EPs, Singles, Other).
    fn kind_shown(&self, kind: &str) -> bool {
        self.cfg.view_kinds[match kind {
            "Album" | "Mixtape" => 0,
            "EP" => 1,
            "Single" => 2,
            _ => 3,
        }]
    }

    /// How many releases are new since the last refresh and would be listed: of a kind that is switched on,
    /// and not hidden or muted (unless `hidden` ones are being shown).
    fn new_releases(&self, hidden: bool) -> usize {
        let new = &self.results.new_keys;
        self.results
            .releases
            .iter()
            .filter(|r| new.contains(&r.key) && self.kind_shown(&r.kind))
            .filter(|r| hidden || (!self.cfg.dismissed.contains(&r.key) && !self.cfg.muted.contains_key(&r.artist_key)))
            .count()
    }

    /// The same for "Artists to try".
    fn new_artists(&self, hidden: bool) -> usize {
        let new = &self.results.new_artist_keys;
        self.results.similar.iter().map(|s| format!("artist|{}", s.key)).filter(|k| new.contains(k) && (hidden || !self.cfg.dismissed.contains(k))).count()
    }

    /// The small number beside a page's name when that page has something for you, and its color.
    fn page_note(&self, page: Page) -> Option<(String, Color32)> {
        match page {
            Page::Recs => {
                // only what the Recs page would show: a new single counts for nothing while Singles are switched off
                let n = self.new_releases(false);
                (n > 0).then(|| (n.to_string(), ACCENT2()))
            }
            Page::Import => {
                let arriving = self.downloads.as_ref().map(|d| d.arriving_count()).unwrap_or(0);
                let waiting = match &self.importer {
                    Some(Ok(imp)) => imp.waiting(),
                    _ => 0,
                };
                if arriving > 0 {
                    Some((format!("{arriving} arriving"), ACCENT2()))
                } else if waiting > 0 {
                    Some((format!("{waiting} ready"), GREEN()))
                } else {
                    None
                }
            }
            // running, or a quiet mark when something about the backups needs a look
            Page::Backup => match &self.backup {
                Some(b) if b.busy() => Some(("running".to_string(), ACCENT2())),
                Some(b) if b.needs_look() => Some(("\u{25cf}".to_string(), theme::WARN())),
                _ => None,
            },
            _ => self.page_note_more(page),
        }
    }

    fn page_bar(&mut self, ui: &mut Ui) {
        let bar = Rect::from_min_size(ui.max_rect().min, vec2(ui.max_rect().width(), BAR_H));
        // the row is also the window's title bar: its empty parts drag the window, and a double click on
        // them makes it fill the screen or brings it back. Asked for first, so the words on it stay on top.
        let grip = ui.interact(bar, egui::Id::unique("window-title-bar"), Sense::click_and_drag());
        if grip.double_clicked() {
            let full = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!full));
        } else if grip.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let painter = ui.painter().clone();
        let mid = bar.center().y;
        // a hairline under the whole row
        painter.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, theme::BORDER().gamma_multiply(0.55)));

        // the mark at the left
        // the mark at the left: the two wheels of the app's icon
        let logo = Rect::from_center_size(egui::pos2(bar.left() + 28.0, mid), vec2(22.0, 22.0));
        icon::paint_mark(ui, logo, 1.0);
        // beside it, only while a newer version lies fetched and proven: two small words that open
        // Settings at About. Nothing pops up and nothing restarts by itself.
        let mut names_from = logo.right() + 28.0;
        if let Some(version) = update::ready() {
            let mut job = egui::text::LayoutJob::default();
            job.append("UPDATE READY", 0.0, egui::TextFormat { font_id: FontId::proportional(11.0), extra_letter_spacing: 1.4, color: ACCENT(), ..Default::default() });
            let words = painter.layout_job(job);
            let at = Rect::from_min_size(egui::pos2(logo.right() + 12.0, (mid - words.size().y / 2.0).round()), words.size());
            let r = ui.interact(at.expand2(vec2(6.0, 8.0)), egui::Id::unique("update-ready"), Sense::click());
            script::seen("UPDATE READY", at);
            painter.galley(at.min, words, ACCENT());
            if r.hovered() {
                painter.hline(at.x_range(), at.bottom() + 1.0, Stroke::new(1.0, ACCENT()));
            }
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Version {version} is ready")).clicked() {
                self.open_settings(settings::ABOUT);
            }
            names_from = at.right() + 24.0;
        }

        // the right end: Settings as a gear, then the window controls
        let mut right = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
        window_controls(&mut right);
        right.add_space(4.0);
        let word = if self.show_settings { "Close settings" } else { "Settings" };
        let (gear, gear_r) = right.allocate_exact_size(vec2(32.0, 30.0), Sense::click());
        script::seen(word, gear);
        let hot = gear_r.hovered();
        if hot || self.show_settings {
            painter.rect_filled(gear, 7.0, CARD_HOVER());
        }
        painter.text(gear.center() + vec2(0.0, -0.5), Align2::CENTER_CENTER, "\u{2699}", FontId::proportional(17.0), if hot || self.show_settings { TEXT() } else { DIM() });
        // a quiet mark when something in Settings needs a look (Settings never opens by itself)
        let now = ui.input(|i| i.time);
        if !self.show_settings && !self.attention(now).is_empty() {
            painter.circle_filled(gear.right_top() + vec2(-7.0, 7.0), 3.0, theme::WARN());
        }
        if gear_r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(word).clicked() {
            self.show_settings = !self.show_settings;
            self.settings.beside = false;
        }
        let right_edge = gear.left();

        // what is running on a page that is not the one showing: a small spinner beside that page's name
        let jobs: Vec<(Page, String)> = [
            (Page::Health, self.health.as_ref().and_then(|h| h.job())),
            (Page::Import, match &self.importer {
                Some(Ok(imp)) => imp.job(),
                _ => None,
            }),
            (Page::Backup, self.backup.as_ref().and_then(|b| b.job())),
            (Page::Recs, self.running.then(|| "Refreshing Recs".to_string())),
        ]
        .into_iter()
        .filter(|(page, _)| *page != self.page)
        .filter_map(|(page, job)| job.map(|j| (page, j)))
        .collect();

        // the page names, centered on the window (pushed aside only when the window is too narrow)
        const GAP: f32 = 30.0;
        const SPIN: f32 = 9.0;
        let name_font = FontId::proportional(14.5);
        let note_font = FontId::proportional(10.5);
        struct Item {
            page: Page,
            name: &'static str,
            text: Arc<egui::Galley>,
            note: Option<(Arc<egui::Galley>, Color32)>,
            job: Option<String>,
            w: f32,
        }
        let items: Vec<Item> = self
            .pages()
            .into_iter()
            .map(|(page, name, _)| {
                let text = painter.layout_no_wrap(name.to_string(), name_font.clone(), Color32::PLACEHOLDER);
                let job = jobs.iter().find(|(p, _)| *p == page).map(|(_, j)| j.clone());
                let note = if job.is_some() { None } else { self.page_note(page).map(|(n, c)| (painter.layout_no_wrap(n, note_font.clone(), c), c)) };
                let extra = if job.is_some() { 5.0 + SPIN } else { note.as_ref().map_or(0.0, |(g, _)| 4.0 + g.size().x) };
                let w = text.size().x + extra;
                Item { page, name, text, note, job, w }
            })
            .collect();
        let total: f32 = items.iter().map(|i| i.w).sum::<f32>() + GAP * (items.len() - 1) as f32;
        let mut x = (bar.center().x - total / 2.0).min(right_edge - 20.0 - total).max(names_from).round();
        let mut chosen: Option<(f32, f32)> = None;
        for item in items {
            let text_h = item.text.size().y;
            let top = (mid - text_h / 2.0 - 1.0).round();
            let hit = Rect::from_min_max(egui::pos2(x - GAP / 2.0, bar.top() + 4.0), egui::pos2(x + item.w + GAP / 2.0, bar.bottom() - 2.0));
            let r = ui.interact(hit, egui::Id::unique(("page-tab", item.name)), Sense::click());
            script::seen(item.name, Rect::from_min_size(egui::pos2(x, top), item.text.size()));
            let sel = self.page == item.page && !self.settings_page();
            let color = if sel {
                TEXT()
            } else if r.hovered() {
                DIM().lerp_to_gamma(TEXT(), 0.6)
            } else {
                DIM()
            };
            painter.galley(egui::pos2(x, top), item.text.clone(), color);
            let name_w = item.text.size().x;
            // a quiet number when the page has something for you, raised a little like a footnote
            if let Some((note, c)) = &item.note {
                painter.galley(egui::pos2(x + name_w + 4.0, top + 1.0), note.clone(), *c);
            }
            if let Some(words) = &item.job {
                let spin = Rect::from_center_size(egui::pos2(x + name_w + 5.0 + SPIN / 2.0, top + 7.0), vec2(SPIN, SPIN));
                egui::Spinner::new().size(SPIN).color(ACCENT2()).paint_at(ui, spin);
                script::seen(&format!("job:{words}"), hit);
            }
            if sel {
                chosen = Some((x, name_w));
            }
            let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
            let r = match &item.job {
                Some(words) => r.on_hover_text(words),
                None => r,
            };
            if r.clicked() {
                self.go_to(item.page);
            }
            x += item.w + GAP;
        }
        // the chosen page's line glides from one name to the next
        if let Some((at, w)) = chosen {
            let ctx = ui.ctx().clone();
            let left = ctx.animate_value_with_time(egui::Id::unique("page-tab-line-x"), at, 0.16);
            let width = ctx.animate_value_with_time(egui::Id::unique("page-tab-line-w"), w, 0.16);
            let line = Rect::from_min_size(egui::pos2(left, bar.bottom() - 7.0), vec2(width, 2.0));
            painter.rect_filled(line, 1.0, ACCENT());
        }
        ui.advance_cursor_after_rect(bar);
    }

    /// Page-bar numbers that come from the pages built by others (filled in as those pages grow the getters).
    fn page_note_more(&self, page: Page) -> Option<(String, Color32)> {
        match page {
            // how many serious problems the last check found
            Page::Health => self.health.as_ref().and_then(|h| h.headline()).filter(|h| h[1] > 0).map(|h| (h[1].to_string(), theme::RED())),
            _ => None,
        }
    }

    /// What Health was asked to get and is still waiting for: a better copy of an album or of some of its
    /// tracks (`replace`), and the full album of loose tracks (`loose`). Read from the two lists at most every
    /// two seconds.
    fn waiting_for(&mut self, now: f64) -> Vec<Waiting> {
        if now - self.waiting_cache.0 < 2.0 {
            return self.waiting_cache.1.clone();
        }
        let leaf = |p: &str| p.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or("").to_string();
        // what can no longer be met drops off by itself: an album folder or loose files that are gone
        let library = std::path::PathBuf::from(self.cfg.music_dir.trim());
        replace::prune(&replace::file(), &library);
        loose::prune(&loose::file(), &library);
        let mut out = vec![];
        for w in replace::all(&replace::file()) {
            let what = match w.files.len() {
                0 => format!("a better copy of the album ({})", w.why.to_lowercase()),
                1 => format!("a better copy of 1 track ({})", w.why.to_lowercase()),
                n => format!("a better copy of {n} tracks ({})", w.why.to_lowercase()),
            };
            out.push(Waiting { album: leaf(&w.folder), artist: w.artist.clone(), what, search: format!("{} {}", w.artist, importer::util::strip_year(&leaf(&w.folder))).trim().to_string(), folder: w.folder.clone(), loose: false });
        }
        for w in loose::all(&loose::file()) {
            let what = if w.files.len() == 1 { "the full album, for 1 loose track".to_string() } else { format!("the full album, for {} loose tracks", w.files.len()) };
            out.push(Waiting { album: w.album.clone(), artist: w.artist.clone(), what, search: format!("{} {}", w.artist, importer::util::strip_year(&w.album)).trim().to_string(), folder: w.artist_dir.clone(), loose: true });
        }
        self.waiting_cache = (now, out.clone());
        out
    }

    /// On the import list's view, while anything is waited for: a card with one row per album Health
    /// asked for. A row is the control: a click searches Soulseek for that album (the word that says so
    /// shows on the row under the pointer), and the small X stops waiting for it. Forgetting changes
    /// nothing in the library.
    fn waiting_card(&mut self, ui: &mut Ui, left: f32, wide: f32) {
        const ROW: f32 = 28.0;
        let now = ui.input(|i| i.time);
        if now - self.waiting_cache.0 >= 2.0 {
            let _ = self.waiting_for(now);
        }
        if self.waiting_cache.1.is_empty() {
            return;
        }
        // (the list is looked at where it is kept: nothing is copied to draw it)
        let list = std::mem::take(&mut self.waiting_cache.1);
        let can_search = !importer::test_folders();
        let mut forget: Option<usize> = None;
        ui.add_space(16.0);
        theme::panel(ui, left, wide, vec2(24.0, 16.0), "waiting-for", |ui| {
            theme::caption(ui, &format!("WAITING FOR  \u{b7}  {}", list.len()));
            ui.add_space(4.0);
            ScrollArea::vertical().id_salt("waiting-for-scroll").max_height(ROW * 4.3).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let total = ui.available_width() - if list.len() > 4 { 12.0 } else { 0.0 };
                for (n, w) in list.iter().enumerate() {
                    let (rect, row) = ui.allocate_exact_size(vec2(total, ROW), if can_search { Sense::click() } else { Sense::hover() });
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let over = ui.rect_contains_pointer(rect);
                    if over {
                        ui.painter().rect_filled(rect.expand2(vec2(8.0, 0.0)), 6, CARD_HOVER());
                    }
                    let mid = rect.center().y;
                    let mark = Rect::from_center_size(egui::pos2(rect.right() - 7.0, mid), vec2(14.0, 14.0));
                    let mut end = mark.left() - 14.0;
                    // under the pointer, what a click on the row does
                    if over && can_search {
                        let word = ui.painter().layout_no_wrap("Search again".to_string(), FontId::proportional(13.0), TEXT());
                        end -= word.size().x;
                        ui.painter().galley(egui::pos2(end, (mid - word.size().y / 2.0).round()), word, TEXT());
                        end -= 16.0;
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    // the album, whose it is, and what is waited for
                    let mut x = rect.left();
                    for (text, size, color) in [(&w.album, 14.0, TEXT()), (&w.artist, 13.0, DIM()), (&w.what, 13.0, theme::FAINT())] {
                        if end - x < 24.0 {
                            break;
                        }
                        let words = theme::cut(ui, text, FontId::proportional(size), color, end - x);
                        let at = egui::pos2(x, (mid - words.size().y / 2.0).round());
                        x += words.size().x + 12.0;
                        ui.painter().galley(at, words, color);
                    }
                    let name = if script::on() { format!("forget:{}", w.album) } else { String::new() };
                    if theme::close_mark(&mut ui.new_child(egui::UiBuilder::new().max_rect(mark)), &name, "Stop waiting for this album").clicked() {
                        forget = Some(n);
                    } else if row.clicked() {
                        slskd::ask_for(&w.search);
                    }
                }
            });
        });
        if let Some(w) = forget.and_then(|n| list.get(n)) {
            if w.loose {
                loose::forget(&loose::file(), std::path::Path::new(&w.folder), &w.album);
            } else {
                replace::done(&replace::file(), std::path::Path::new(&w.folder));
            }
            self.waiting_cache.0 = -10.0;
        }
        self.waiting_cache.1 = list;
    }

    /// The music folder Cadence's list is checked against, with what is skipped and the artist name list
    /// (a test run's own library copy in a test run).
    fn inbox_folders(&self) -> inbox::Folders {
        let music = if importer::test_folders() { arg_after("--library").unwrap_or_default() } else { self.cfg.music_dir.clone() };
        inbox::Folders { music, skip: self.cfg.skip_folders.clone(), aliases: self.cfg.alias_file.clone() }
    }

    /// Every frame, on any page: the library is read for Cadence's list when due (a new list, after an
    /// import ends, every ten minutes), and at most every two seconds the answer for Cadence is worked out
    /// and written when what it says changed (`inbox::ANSWER_FILE`).
    fn keep_cadence_answer(&mut self, ctx: &egui::Context) {
        let busy = matches!(&self.importer, Some(Ok(imp)) if imp.busy());
        let ended = self.import_was_busy && !busy;
        self.import_was_busy = busy;
        let now = ctx.input(|i| i.time);
        let folders = self.inbox_folders();
        self.inbox.keep_library(&folders, now, ended, ctx);
        if now - self.answer_at < 2.0 || !self.inbox.can_answer() {
            return;
        }
        self.answer_at = now;
        let steps = self.cadence_steps();
        self.inbox.answer_with(&steps, false);
    }

    /// The albums on Cadence's list that have a step, with it: the rows' own, then those set aside here.
    fn cadence_steps(&self) -> Vec<(inbox::Wanted, inbox::Stage)> {
        let mut steps: Vec<(inbox::Wanted, inbox::Stage)> = self.inbox.rows().into_iter().filter_map(|w| self.stage_of(&w).map(|s| (w, s))).collect();
        steps.extend(self.inbox.set_aside().into_iter().map(|w| (w, inbox::Stage::SetAside)));
        steps
    }

    /// How far one album on Cadence's list has come: its search, a copy of it arriving, then where that
    /// copy stands on the import list. None while it has not been looked for.
    fn stage_of(&self, w: &inbox::Wanted) -> Option<inbox::Stage> {
        use inbox::Stage;
        let trail = self.downloads.as_ref().and_then(|d| d.trail(&w.search()));
        Some(match trail {
            Some(downloads::Trail::Searching) => Stage::Searching,
            Some(downloads::Trail::Found(n)) => Stage::Found(n),
            Some(downloads::Trail::Arriving(done, of)) => Stage::Arriving(done, of),
            Some(downloads::Trail::Arrived(folder)) => match &self.importer {
                Some(Ok(imp)) => match imp.stage_of(&folder) {
                    Some(false) => Stage::Ready,
                    Some(true) => Stage::Imported,
                    None => Stage::Arrived,
                },
                _ => Stage::Arrived,
            },
            None if self.inbox.state.searched.contains(&w.key()) => Stage::Searched,
            None => return None,
        })
    }

    /// On the import list's view, over "Waiting for": the albums Cadence says are missed and the library
    /// does not have, most missed first (`inbox.rs`), as a card of its own: "FROM CADENCE", the count in a
    /// large light line (the count the one heavy word), when it was sent, then five rows, all of them after
    /// "Show all". A row is the control: a click searches Soulseek for the album (nothing is fetched until a
    /// copy is picked). Its end says how far it has come, or "Find it" while it has not been looked for.
    /// Pointing at a row lists the songs that were wanted, as a tip that covers nothing and moves nothing.
    /// The X sets one aside for good; "Forget the list" puts the list away until Cadence sends a newer one.
    fn cadence_card(&mut self, ui: &mut Ui, left: f32, wide: f32) {
        const ROW: f32 = 54.0;
        const FIRST: usize = 5;
        const CARD_WIDE: f32 = 1100.0;
        let test = importer::test_folders();
        // a list that cannot be read says so in one line, and nothing more
        if let Some(Err(why)) = &self.inbox.list {
            let why = why.clone();
            ui.add_space(16.0);
            theme::panel(ui, left, wide, vec2(24.0, 16.0), "cadence-list", |ui| {
                theme::caption(ui, "FROM CADENCE");
                ui.add_space(6.0);
                ui.add(Label::new(RichText::new(why).size(13.0).color(DIM())).wrap());
            });
            return;
        }
        let rows = self.inbox.rows();
        if rows.is_empty() {
            return;
        }
        let when = match self.inbox.list.as_ref().and_then(|l| l.as_ref().ok()).and_then(inbox::written_at) {
            Some(t) => format!("Sent {}, most missed first. Each one leaves once your library has it.", recs_top::when_words(t, Local::now())),
            None => "Most missed first. Each one leaves once your library has it.".to_string(),
        };
        let stages: Vec<Option<inbox::Stage>> = rows.iter().map(|w| self.stage_of(w)).collect();
        let can_search = !test;
        let show_all = self.inbox.show_all && rows.len() > FIRST;
        let (mut forget, mut search, mut put_away, mut toggle) = (None, None, false, false);
        let rule = theme::blend(CARD(), TEXT(), 0.07);
        // a card of reading width in the middle of the page, as the sketch has it
        let (left, wide) = (left + ((wide - wide.min(CARD_WIDE)) / 2.0).round(), wide.min(CARD_WIDE));
        ui.add_space(16.0);
        theme::panel(ui, left, wide, vec2(34.0, 26.0), "cadence-list", |ui| {
            ui.vertical_centered(|ui| theme::caption(ui, "FROM CADENCE"));
            ui.add_space(4.0);
            // the large light line, its count the one heavy word
            let n = rows.len();
            let mut job = egui::text::LayoutJob::default();
            job.append(&library_page::commas(n as u64), 0.0, egui::TextFormat::simple(FontId::proportional(32.0), TEXT()));
            let rest = if n == 1 { " album is missing from your library" } else { " albums are missing from your library" };
            job.append(rest, 0.0, egui::TextFormat::simple(theme::display(32.0), TEXT()));
            job.wrap.max_width = ui.available_width();
            let head = ui.painter().layout_job(job);
            let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), head.size().y), Sense::hover());
            ui.painter().galley(egui::pos2((line.center().x - head.size().x / 2.0).round(), line.top()), head, TEXT());
            ui.add_space(4.0);
            let room = ui.available_width();
            theme::centered_wrapped(ui, &when, 13.0, theme::FAINT(), room);
            ui.add_space(16.0);
            let shown = if show_all { rows.len() } else { rows.len().min(FIRST) };
            let mut draw = |ui: &mut Ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                // (room for the scroll bar only when there is one, so the X marks do not move otherwise)
                let total = ui.available_width() - if show_all && rows.len() > 7 { 12.0 } else { 0.0 };
                for (w, stage) in rows.iter().zip(&stages).take(shown) {
                    let (rect, row) = ui.allocate_exact_size(vec2(total, ROW), if can_search { Sense::click() } else { Sense::hover() });
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    script::seen(&format!("cadence:{}", w.title()), rect);
                    let over = ui.rect_contains_pointer(rect);
                    if over {
                        ui.painter().rect_filled(rect.expand2(vec2(8.0, 0.0)), 6, CARD_HOVER());
                    } else {
                        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, rule));
                    }
                    let mid = rect.center().y;
                    let mark = Rect::from_center_size(egui::pos2(rect.right() - 7.0, mid), vec2(14.0, 14.0));
                    let mut end = mark.left() - 18.0;
                    let font = FontId::proportional(13.0);
                    // how far it has come, or "Find it" while it has not been looked for
                    let (words, color) = match stage {
                        Some(s) => {
                            use inbox::Stage::*;
                            let color = match s {
                                Searching => DIM(),
                                Found(0) | Searched | SetAside => theme::FAINT(),
                                Found(_) | Arriving(..) => ACCENT2(),
                                Ready | Imported | Arrived => GREEN(),
                            };
                            (s.words(), color)
                        }
                        None => ("Find it".to_string(), if over && can_search { TEXT() } else { DIM() }),
                    };
                    let g = ui.painter().layout_no_wrap(words, font.clone(), color);
                    let size = g.size();
                    end -= size.x;
                    let at = egui::pos2(end, (mid - size.y / 2.0).round());
                    ui.painter().galley(at, g, color);
                    if over && can_search {
                        if stage.is_none() {
                            ui.painter().hline(at.x..=at.x + size.x, at.y + size.y - 1.0, Stroke::new(1.0, TEXT()));
                        } else {
                            // a row that was looked for: what a click does now, beside how far it came
                            let g = ui.painter().layout_no_wrap("Search again".to_string(), font.clone(), TEXT());
                            end -= 16.0 + g.size().x;
                            ui.painter().galley(egui::pos2(end, (mid - g.size().y / 2.0).round()), g, TEXT());
                        }
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    end -= 18.0;
                    // the album (or its first song) and whose it is, on one line of writing; why it is missed under them
                    let width = (end - rect.left()).max(0.0);
                    let title = theme::cut(ui, w.title(), FontId::proportional(15.0), TEXT(), width);
                    let top = (mid - 20.0).round();
                    let title_w = title.size().x;
                    let title_base = theme::baseline(&title);
                    ui.painter().galley(egui::pos2(rect.left(), top), title, TEXT());
                    let room = width - title_w - 10.0;
                    if room > 24.0 && !w.artist.is_empty() {
                        let artist = theme::cut(ui, &w.artist, FontId::proportional(13.5), DIM(), room);
                        let y = (top + title_base - theme::baseline(&artist)).round();
                        ui.painter().galley(egui::pos2(rect.left() + title_w + 10.0, y), artist, DIM());
                    }
                    let why = theme::cut(ui, &w.why(), FontId::proportional(12.5), theme::FAINT(), width);
                    ui.painter().galley(egui::pos2(rect.left(), top + 22.0), why, theme::FAINT());
                    let name = if script::on() { format!("forget-cadence:{}", w.title()) } else { String::new() };
                    if theme::close_mark(&mut ui.new_child(egui::UiBuilder::new().max_rect(mark)), &name, "Take this album off the list").clicked() {
                        forget = Some(w.clone());
                    } else {
                        let row = match w.songs_words() {
                            Some(words) => row.on_hover_text(words),
                            None => row,
                        };
                        if row.clicked() {
                            search = Some(w.clone());
                        }
                    }
                }
            };
            if show_all {
                ScrollArea::vertical().id_salt("cadence-list-scroll").max_height(ROW * 7.4).auto_shrink([false, true]).show(ui, |ui| draw(ui));
            } else {
                draw(ui);
            }
            // under the rows, in the middle: the two quiet words
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                theme::centered_row(ui, "cadence-list-foot", 0.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 28.0;
                    if rows.len() > FIRST {
                        let word = if show_all { "Show fewer".to_string() } else { format!("Show all {}", library_page::commas(rows.len() as u64)) };
                        if theme::quiet(ui, &word, 13.0, true).clicked() {
                            toggle = true;
                        }
                    }
                    if theme::quiet(ui, "Forget the list", 13.0, true).clicked() {
                        put_away = true;
                    }
                });
            });
        });
        if let Some(w) = forget {
            self.inbox.forget(&w);
        }
        if let Some(w) = search {
            self.inbox.searched(&w);
            slskd::ask_for(&w.search());
        }
        if toggle {
            self.inbox.show_all = !self.inbox.show_all;
        }
        if put_away {
            self.inbox.put_away();
        }
    }

    /// While the copies of a search a row of Cadence's list asked for are on show: the list as one line over
    /// them, with the way back to it. True when "Show the list" was clicked.
    fn cadence_line(ui: &mut Ui, left: f32, wide: f32, count: usize, title: &str) -> bool {
        let mut back = false;
        // (as wide as the copies' card under it)
        theme::panel(ui, left, wide, vec2(24.0, 14.0), "cadence-line", |ui| {
            // (one line's height, so the words at the right sit on the line and not halfway down the card's room)
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 20.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                back = theme::quiet(ui, "Show the list", 13.0, true).clicked();
                ui.add_space(18.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    theme::caption(ui, "FROM CADENCE");
                    let n = if count == 1 { "1 album missing from your library".to_string() } else { format!("{} albums missing from your library", library_page::commas(count as u64)) };
                    ui.add(Label::new(RichText::new(n).size(14.0).color(TEXT())).selectable(false).truncate());
                    ui.add(Label::new(RichText::new(format!("{title} is open below")).size(13.0).color(theme::FAINT())).selectable(false).truncate());
                });
            });
        });
        back
    }

    /// The Downloads page, in one centered stretch of the window: the search box and the kept searches,
    /// what is arriving, then the copies of the search on show or the import list (started the first time
    /// this page is opened). How the parts look is in `downloads/page.rs` and `importer/mod.rs`.
    fn import_page(&mut self, ui: &mut Ui) {
        // Settings changed the folders: start over with the new ones (never in the middle of a scan or import)
        if matches!(&self.importer, Some(Ok(imp)) if !imp.busy() && imp.folders_changed(&self.cfg)) {
            if let Some(Ok(imp)) = &mut self.importer {
                imp.on_exit();
            }
            self.importer = None;
        }
        if self.importer.is_none() {
            self.importer = Some(importer::ImportTab::new(ui.ctx(), &self.cfg));
        }
        if let Some(Ok(imp)) = &mut self.importer {
            imp.wake(ui.ctx());
        }
        // a test run on copies never touches Soulseek. It shows the search side only with made-up data:
        // `--search-demo <folder>` lists the copies of a made-up search (its answers come from files in that
        // folder; nothing is contacted), `--dl-demo` shows made-up arriving rows, and `--library` stands
        // for the music folder (a test run looks in its own library copy, never the real one)
        static DEMO: std::sync::OnceLock<(Option<String>, bool, String)> = std::sync::OnceLock::new();
        let (search_demo, dl_demo, test_library) = DEMO.get_or_init(|| (arg_after("--search-demo"), std::env::args().any(|a| a == "--dl-demo"), arg_after("--library").unwrap_or_default()));
        let test = importer::test_folders();
        if !test {
            self.downloads.get_or_insert_with(|| downloads::DownloadsPanel::new(ui.ctx())).attach(&self.net);
        } else if let Some(folder) = search_demo {
            self.downloads.get_or_insert_with(|| downloads::DownloadsPanel::made_up_search(ui.ctx(), std::path::PathBuf::from(folder)));
        } else if *dl_demo {
            self.downloads.get_or_insert_with(downloads::DownloadsPanel::demo);
        }
        let mut retry = false;
        let list_rows = self.inbox.rows();
        CentralPanel::default().frame(Frame::new().fill(BG())).show(ui, |ui| {
          // the cards reach down to the end of the window; in a window too short for them they keep a least
          // height and the page scrolls
          let tall = ui.available_height();
          ScrollArea::vertical().id_salt("dl-page").auto_shrink([false, false]).show(ui, |ui| {
            let floor = ui.max_rect().top() + tall;
            let (left, wide) = theme::column(ui, downloads::PAGE);
            ui.add_space(26.0);
            // top to bottom, the trip an album takes: find it, watch it arrive, import it
            if let Some(dl) = &mut self.downloads {
                let importer = &self.importer;
                let stage = |folder: &str| match importer {
                    Some(Ok(imp)) => imp.stage_of(folder),
                    _ => None,
                };
                let ready = match importer {
                    Some(Ok(imp)) => imp.waiting(),
                    _ => 0,
                };
                dl.hero(ui, left, wide, ready);
                dl.arriving(ui, left, wide, &stage);
                // the kept search on show takes the main card; the import list is one click away
                if dl.showing_results() {
                    if let Some(w) = dl.shown_text().and_then(|t| list_rows.iter().find(|w| w.search() == t)) {
                        if App::cadence_line(ui, left, wide, list_rows.len(), w.title()) {
                            dl.show_import_list();
                        }
                    }
                    let music_dir = std::path::Path::new(if test { test_library.as_str() } else { self.cfg.music_dir.trim() });
                    dl.results(ui, left, wide, floor, downloads::Around { images: &mut self.images, music_dir, stage: &stage });
                    return;
                }
            }
            self.cadence_card(ui, left, wide);
            self.waiting_card(ui, left, wide);
            match self.importer.as_mut() {
                Some(Ok(imp)) => imp.ui(ui, left, wide, floor),
                Some(Err(why)) => {
                    let mut column = ui.available_rect_before_wrap();
                    (column.min.x, column.max.x) = (left, left + wide);
                    let no_ffmpeg = why == importer::tools::NO_FFMPEG;
                    ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
                        ui.add_space(70.0);
                        ui.vertical_centered(|ui| {
                            ui.add(Label::new(RichText::new(why.as_str()).font(theme::display(24.0)).color(TEXT())).wrap());
                            ui.add_space(10.0);
                            if !no_ffmpeg {
                                retry = theme::quiet(ui, "Try again", 14.0, true).clicked();
                                return;
                            }
                            // the one thing this card is for: "Get ffmpeg", in coral; while it is fetched, how
                            // far it is and "Stop" in the same place, on a row that keeps its height
                            ui.horizontal(|ui| {
                                ui.set_min_height(24.0);
                                theme::centered_row(ui, "dl-get-ffmpeg", 0.0, |ui| {
                                    ui.spacing_mut().item_spacing.x = 14.0;
                                    if helpers::state(helpers::Helper::Ffmpeg) == helpers::State::Idle {
                                        if theme::main_action_sized(ui, helpers::Helper::Ffmpeg.word(), 16.0, true).clicked() {
                                            helpers::start(helpers::Helper::Ffmpeg, ui.ctx());
                                        }
                                    } else {
                                        helpers::fetch_line(ui, helpers::Helper::Ffmpeg, "", DIM(), 13.5, false, false);
                                    }
                                });
                            });
                        });
                    });
                    // fetched: the import list starts over and finds it
                    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let arrived = helpers::arrived(helpers::Helper::Ffmpeg);
                    if no_ffmpeg && SEEN.swap(arrived, std::sync::atomic::Ordering::Relaxed) != arrived {
                        retry = true;
                    }
                }
                None => {}
            }
          });
        });
        if retry {
            self.importer = None;
        }
    }
}

// ---------------------------------------------------------------- small widgets & helpers

/// A small word you can click (opens a site, copies, hides).
/// The "only what is new since the last refresh" switch: the same blue dot the new tiles carry, and how many.
fn new_toggle(ui: &mut Ui, on: &mut bool, count: usize) {
    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(dot.center(), 4.0, ACCENT2());
    crate::theme::toggle(ui, on, format!("New  {count}"));
}

fn link_btn(ui: &mut Ui, text: &str) -> egui::Response {
    let r = theme::link_sized(ui, text, 12.5, true);
    ui.add_space(10.0);
    r
}

/// A short fact about the thing it sits on ("Album", "Upcoming", "In your library"): plain colored text.
fn chip(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(RichText::new(text).size(12.0).color(color));
    ui.add_space(8.0);
}

fn empty_state(ui: &mut Ui, text: &str) {
    ui.add_space(80.0);
    ui.vertical_centered(|ui| {
        // (the app's mark, standing back)
        let (mark, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
        icon::paint_mark(ui, mark, 0.5);
        ui.add_space(8.0);
        ui.label(RichText::new(text).size(16.0).color(DIM()));
    });
}

fn join_names(n: &[&str]) -> String {
    match n.len() {
        0 => String::new(),
        1 => n[0].to_string(),
        2 => format!("{} and {}", n[0], n[1]),
        _ => format!("{}, {} and {}", n[0], n[1], n[2]),
    }
}

fn hue_color(seed: &str, lightness: f32) -> Color32 {
    let h = (util::fnv64(seed) % 360) as f32 / 360.0;
    egui::ecolor::Hsva::new(h, 0.45, lightness + 0.15, 1.0).into()
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTHS_LONG: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn pretty_date(d: &str) -> String {
    let p: Vec<&str> = d.split('-').collect();
    let m = p.get(1).and_then(|m| m.parse::<usize>().ok()).filter(|m| (1..=12).contains(m));
    match (p.len(), m) {
        (3, Some(m)) => format!("{} {}, {}", MONTHS[m - 1], p[2].trim_start_matches('0'), p[0]),
        (2, Some(m)) => format!("{} {}", MONTHS[m - 1], p[0]),
        _ => d.to_string(),
    }
}

/// "today", "yesterday", "5 days ago", "in 6 days" for exact dates close to today; empty otherwise.
fn relative_day(d: &str, today: NaiveDate) -> String {
    if d.len() < 10 {
        return String::new(); // "2026" or "2026-05": too vague to count days
    }
    let Some(date) = util::parse_date_earliest(d) else { return String::new() };
    match (date - today).num_days() {
        0 => "today".into(),
        1 => "tomorrow".into(),
        -1 => "yesterday".into(),
        n @ 2..=90 => format!("in {n} days"),
        n @ -13..=-2 => format!("{} days ago", -n),
        _ => String::new(),
    }
}

fn month_label(d: &str) -> String {
    let p: Vec<&str> = d.split('-').collect();
    match p.get(1).and_then(|m| m.parse::<usize>().ok()).filter(|m| (1..=12).contains(m)) {
        Some(m) => format!("{} {}", MONTHS_LONG[m - 1], p[0]),
        None => p[0].to_string(),
    }
}

/// The font large headings are set in: the light cut of the text font.
pub(crate) fn display_font() -> egui::FontFamily {
    egui::FontFamily::Name("display".into())
}

fn setup_style(ctx: &egui::Context) {
    // Segoe UI for text, with Korean / Japanese / Chinese and symbol fallbacks so titles like "신세기" render.
    let mut fonts = egui::FontDefinitions::default();
    let mut added = vec![];
    for (name, file) in [
        ("segoe", "segoeui.ttf"),
        ("malgun", "malgun.ttf"),
        ("yugoth", "YuGothM.ttc"),
        ("msyh", "msyh.ttc"),
        ("segsym", "seguisym.ttf"),
    ] {
        // Mapped from disk, not read into memory: these five files are 48 MB, and almost all of that is glyphs for
        // languages that may never appear on screen. Mapped, only the parts actually used are ever loaded.
        let path = format!(r"C:\Windows\Fonts\{file}");
        let data = match util::map_file(&path) {
            Some(bytes) => Some(egui::FontData::from_static(bytes)),
            None => std::fs::read(&path).ok().map(egui::FontData::from_owned),
        };
        if let Some(data) = data {
            fonts.font_data.insert(name.into(), Arc::new(data));
            added.push(name.to_string());
        }
    }
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        let mut v = added.clone();
        v.extend(prop.drain(..));
        *prop = v;
    }
    if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        mono.extend(added.iter().skip(1).cloned());
    }
    // Segoe UI Light for the large headings, with everything above behind it for what it lacks
    let mut display = vec![];
    let light = r"C:\Windows\Fonts\segoeuil.ttf";
    let data = match util::map_file(light) {
        Some(bytes) => Some(egui::FontData::from_static(bytes)),
        None => std::fs::read(light).ok().map(egui::FontData::from_owned),
    };
    if let Some(data) = data {
        fonts.font_data.insert("segoe-light".into(), Arc::new(data));
        display.push("segoe-light".to_string());
    }
    display.extend(fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(display_font(), display);
    ctx.set_fonts(fonts);
    apply_colors(ctx);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(14.5));
        s.text_styles.insert(Button, FontId::proportional(14.0));
        s.text_styles.insert(Heading, FontId::proportional(22.0));
        s.text_styles.insert(Small, FontId::proportional(12.0));
    });
}

/// Hands the theme's colors to the parts egui draws itself (boxes to type in, scroll bars, menus). Called at
/// the start and again whenever a color is changed in Settings.
fn apply_colors(ctx: &egui::Context) {
    let mut v = if theme::is_light() { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.panel_fill = BG();
    v.window_fill = BG2();
    v.extreme_bg_color = theme::field_bg();
    v.faint_bg_color = BG2();
    v.selection.bg_fill = ACCENT().gamma_multiply(0.5);
    v.hyperlink_color = ACCENT2();
    v.override_text_color = None;
    v.widgets.inactive.weak_bg_fill = CARD();
    v.widgets.inactive.bg_fill = CARD();
    v.widgets.hovered.weak_bg_fill = CARD_HOVER();
    v.widgets.hovered.bg_fill = CARD_HOVER();
    v.widgets.active.weak_bg_fill = CARD_HOVER();
    v.widgets.noninteractive.fg_stroke.color = TEXT();
    v.widgets.inactive.fg_stroke.color = TEXT();
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.corner_radius = 6.into();
    }
    ctx.set_visuals(v);
}

/// `bradleys-music-tool.exe --headless`: run once without a window and write a text summary (handy for checking results).
fn headless() {
    use std::fmt::Write;
    let cfg = Config::load();
    let mut out = String::new();
    if let Some(e) = &cfg.load_error {
        writeln!(out, "ERROR: {e}").unwrap();
    }
    let net = Arc::new(net::Net::new(config::app_dir().join("cache")));
    let (tx, rx) = crossbeam_channel::unbounded();
    let t = std::thread::spawn(move || engine::run(cfg, net, tx, Arc::new(AtomicBool::new(false)), || {}));
    let mut res = Results::default();
    for m in rx {
        match m {
            Msg::Artists(a) => res.artists = a,
            Msg::Similar(s) => res.similar = s,
            Msg::Release(r) => res.add_release(r),
            Msg::Reject(k) => res.reject(k),
            Msg::Warn(w) => writeln!(out, "note: {w}").unwrap(),
            Msg::Error(e) => writeln!(out, "ERROR: {e}").unwrap(),
            Msg::Status(s) => eprintln!("{s}"),
            Msg::Progress { task, done, total } if done % 25 == 0 => eprintln!("{task}: {done}/{total}"),
            _ => {}
        }
    }
    let _ = t.join();
    res.releases.sort_by(|a, b| b.date.cmp(&a.date));
    writeln!(out, "\n== Top artists ==").unwrap();
    for a in res.artists.iter().take(40) {
        writeln!(out, "{:6.2}  {} (lib:{} tracks:{} plays:{}/{}/{}/{})", a.weight, a.name, a.in_library, a.tracks, a.plays_all, a.plays_12m, a.plays_3m, a.plays_recent).unwrap();
    }
    writeln!(out, "\n== New releases ({}) ==", res.releases.len()).unwrap();
    for r in &res.releases {
        let src = format!("{}{}", if r.mb_url.is_empty() { "" } else { "MB " }, if r.apple_url.is_empty() { "" } else { "Apple" });
        writeln!(out, "{}  [{}] {} — {}   ({})", r.date, r.kind, r.credit, r.title, src).unwrap();
    }
    writeln!(out, "\n== Similar artists ({}) ==", res.similar.len()).unwrap();
    for s in res.similar.iter().take(60) {
        let b: Vec<&str> = s.because.iter().take(4).map(|b| b.0.as_str()).collect();
        writeln!(out, "{:7.2}  {}  <- {} ({} sources)", s.score, s.name, b.join(", "), s.because.len()).unwrap();
    }
    if let Some(old) = std::fs::read(engine::results_path()).ok().and_then(|b| serde_json::from_slice::<Results>(&b).ok()) {
        res.seen = old.seen;
    }
    res.mark_new();
    writeln!(out, "\n{} new releases and {} new artists since last run", res.new_keys.len(), res.new_artist_keys.len()).unwrap();
    if let Ok(b) = serde_json::to_vec(&res) {
        let _ = std::fs::write(engine::results_path(), b);
    }
    let _ = std::fs::write(config::app_dir().join("headless-report.txt"), &out);
}

/// Anything that stops unexpectedly, on any thread, is written to `crash.log` in the app's data folder with
/// the time and place, so "it just closed" can be looked into afterwards.
fn install_crash_log() {
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let path = config::app_dir().join("crash.log");
        if std::fs::metadata(&path).map(|m| m.len() > 300_000).unwrap_or(false) {
            let _ = std::fs::rename(&path, path.with_extension("log.old"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let thread = std::thread::current();
            let _ = writeln!(f, "{} [{}] {info}", Local::now().format("%Y-%m-%d %H:%M:%S"), thread.name().unwrap_or("worker"));
        }
        before(info);
    }));
}

/// True if the app is already open. Then its window is brought to the front, so a second click on the shortcut
/// shows the app you have instead of starting a rival that would fight over settings and Soulseek.
fn already_open() -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::{CreateMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};
    let wide = |s: &str| -> Vec<u16> { std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect() };
    unsafe {
        let lock = CreateMutexW(std::ptr::null(), 0, wide("Local\\MusicApp.OneWindow").as_ptr());
        if lock.is_null() || GetLastError() != ERROR_ALREADY_EXISTS {
            return false; // ours now (kept for the life of the app), or the check could not be made
        }
        CloseHandle(lock);
        // find the other window by its title, and make sure it really is this app before touching it
        let window = FindWindowW(std::ptr::null(), wide(APP_NAME).as_ptr());
        if !window.is_null() {
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, &mut pid);
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !process.is_null() {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                let ours = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) != 0 && String::from_utf16_lossy(&buf[..len as usize]).to_lowercase().ends_with(EXE_NAME);
                CloseHandle(process);
                if ours && pid != std::process::id() {
                    if IsIconic(window) != 0 {
                        ShowWindow(window, SW_RESTORE);
                    }
                    SetForegroundWindow(window);
                }
            }
        }
        true
    }
}

/// The Music Tool's window, after what a plain start does first (an update going in, one window only).
pub fn run_app() -> eframe::Result {
    // (a freshly fetched update is asked which version it is before it may take this file's place)
    if update::answered_self_check() {
        return Ok(());
    }
    // (the old program file, unseen, guarding the first start of an update that was just put in)
    if update::guarded_update() {
        return Ok(());
    }
    if let Some(code) = backup::run_if_requested() { std::process::exit(code); }
    install_crash_log();
    // (only a plain launch is held to one window: test and debug runs with flags may sit beside the real one.
    // A start right after an update is a plain launch too, once the copy that made way has gone.)
    let plain = std::env::args().len() == 1 || update::started_after_update();
    if plain && already_open() {
        return Ok(());
    }
    // an update that was fetched and proven earlier goes in now, before the window: the new program
    // file is started in this one's place
    if plain && update::put_in_at_start() {
        return Ok(());
    }
    if std::env::args().any(|a| a == "--headless") {
        headless();
        return Ok(());
    }
    // `--write-icon <file>` saves the app's icon (the shortcut's icon file in `assets` is made this way) and ends
    if let Some(path) = arg_after("--write-icon") {
        if let Err(why) = icon::write(std::path::Path::new(&path)) {
            eprintln!("{why}");
        }
        return Ok(());
    }
    if importer::run_dump() {
        return Ok(());
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            // no Windows title bar: the row of page names is the top of the window, with the app's own
            // minimize, maximize and close at its right end (see `window_controls` and `window_edges`)
            .with_decorations(false)
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(icon::window_icon()),
        // a test run (`MUSIC_RECS_DATA` set) keeps the window's place and size in its own folder
        persistence_path: config::window_state_file(std::env::var_os("MUSIC_RECS_DATA")),
        ..Default::default()
    };
    let ended = eframe::run_native(APP_NAME, opts, Box::new(|cc| Ok(Box::new(App::new(cc)))));
    // "Restart to update" closed the window the usual way: now the update goes in and the app starts again
    update::after_close();
    ended
}

fn arg_after(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

impl App {
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        if self.shot_done.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let Some(path) = self.shot.clone() else {
            // asked for, not delivered yet: the picture only arrives with a redraw, so keep drawing until it does
            if arg_after("--screenshot").is_some() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            return;
        };
        let wait: f64 = arg_after("--wait").and_then(|w| w.parse().ok()).unwrap_or(8.0);
        if ctx.input(|i| i.time) < wait {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }
        self.shot = None;
        let done = self.shot_done.clone();
        let ctx2 = ctx.clone();
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
            let [w, h] = img.size;
            let raw: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, raw) {
                let _ = buf.save(&path);
            }
            done.store(true, Ordering::Relaxed);
            ctx2.request_repaint();
        })));
    }
}


/// Applies the middle-click autoscroll to the scroll area this `ui` is inside, if that's where it was started.
pub(crate) fn auto_scroll(ui: &mut Ui, auto: Option<(egui::Pos2, f32)>) {
    if let Some((anchor, dy)) = auto {
        if dy != 0.0 && ui.clip_rect().contains(anchor) {
            ui.scroll_with_delta(vec2(0.0, -dy));
        }
    }
}

/// What sits under the last card of an infinite-scroll list.
fn more_footer(ui: &mut Ui, can_more: bool, loading: bool, end_text: &str) -> bool {
    // how far the visible part of the list is from its end
    let near = ui.min_rect().bottom() - ui.clip_rect().bottom() < 900.0;
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        if loading {
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() / 2.0 - 90.0).max(0.0));
                ui.add(egui::Spinner::new().size(14.0).color(DIM()));
                ui.label(RichText::new("Finding more…").color(DIM()));
            });
        } else if can_more {
            ui.label(RichText::new("Keep scrolling for more").small().color(DIM()));
        } else {
            ui.label(RichText::new(end_text).small().color(DIM()));
        }
    });
    near
}

