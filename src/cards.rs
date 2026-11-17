//! The cards the first-start screen (`setup`) and Settings (`settings`) share: the music folder, what the
//! app needs, Last.fm and Soulseek. Each is drawn by one function here, so the two places show the same
//! thing in the same way and cannot drift apart.
use crate::helpers::{self, Helper};
use crate::library_page::commas;
use crate::theme::{self, ACCENT2, DIM, FAINT, GREEN, TEXT, WARN};
use crate::{rows, script, util, App};
use crossbeam_channel::Receiver;
use egui::{pos2, vec2, Align, Color32, FontId, Label, Layout, Margin, Rect, RichText, Sense, TextEdit, Ui};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The room between two cards, and inside a card's edges.
pub(crate) const GAP: f32 = 16.0;
pub(crate) const PAD: egui::Vec2 = egui::Vec2::new(24.0, 20.0);
/// A cover beside what a folder holds.
const COVER: f32 = 64.0;

/// What a card says at its foot: the words and their color (see `theme::card_row`).
pub(crate) type Foot = Option<(String, Color32)>;

/// What reading a folder found.
#[derive(Default, Clone, PartialEq, Debug)]
pub(crate) struct Found {
    pub artists: usize,
    pub albums: usize,
    pub songs: usize,
    /// songs that are not in an album folder inside an artist folder
    pub misplaced: usize,
    /// a few album folders, for their covers
    pub covers: Vec<PathBuf>,
}

impl Found {
    /// More than a fifth of the songs are not where the app files music: the library is laid out another
    /// way, and repairs (which move and rename) should wait for its owner's word.
    pub(crate) fn odd(&self) -> bool {
        self.songs > 0 && self.misplaced * 5 > self.songs
    }

    /// What the folder holds, in a few words.
    pub(crate) fn words(&self) -> String {
        let n = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{} {many}", commas(n as u64)) };
        if self.songs == 0 {
            "Empty so far. Albums you import are filed here.".to_string()
        } else if self.odd() {
            format!("{} of {} are not in Artist \\ Album folders", commas(self.misplaced as u64), n(self.songs, "song", "songs"))
        } else {
            format!("{} by {}", n(self.albums, "album", "albums"), n(self.artists, "artist", "artists"))
        }
    }
}

enum Read {
    /// how many of the folders at the top have been read, of how many
    Far(usize, usize),
    Done(Found),
}

/// Reads a music folder: how many artist and album folders it holds, how many songs, and how many of the
/// songs are not in an album folder inside an artist folder. `skip` are the names at the top that are not
/// artists (their songs still count). Nothing is opened or changed; only names are read.
pub(crate) fn read_folder(dir: &Path, skip: &[String], stop: &AtomicBool, far: &dyn Fn(usize, usize)) -> Found {
    let is_song = |p: &Path| p.extension().and_then(|e| e.to_str()).is_some_and(|e| crate::importer::scan::is_audio_ext(&format!(".{}", e.to_lowercase())));
    let is_picture = |p: &Path| p.extension().and_then(|e| e.to_str()).is_some_and(|e| ["jpg", "jpeg", "png"].contains(&e.to_lowercase().as_str()));
    let list = |d: &Path| -> Vec<(PathBuf, bool)> { std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| (e.path(), e.file_type().map(|t| t.is_dir()).unwrap_or(false))).collect() };
    // every song under a folder, and whether a picture lies right in it
    fn count(d: &Path, list: &dyn Fn(&Path) -> Vec<(PathBuf, bool)>, is_song: &dyn Fn(&Path) -> bool, stop: &AtomicBool) -> usize {
        if stop.load(Ordering::Relaxed) {
            return 0;
        }
        list(d).into_iter().map(|(p, dir)| if dir { count(&p, list, is_song, stop) } else { is_song(&p) as usize }).sum()
    }
    let mut found = Found::default();
    let mut with_picture = vec![];
    let mut without = vec![];
    let top = list(dir);
    let folders: Vec<&PathBuf> = top.iter().filter(|(p, is_dir)| *is_dir && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(['.', '$']) || n == "System Volume Information")).map(|(p, _)| p).collect();
    // songs lying in the music folder itself
    let loose = top.iter().filter(|(p, is_dir)| !is_dir && is_song(p)).count();
    found.songs += loose;
    found.misplaced += loose;
    for (i, artist) in folders.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        far(i, folders.len());
        let named = artist.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let counts_as_artist = !skip.iter().any(|s| s.to_lowercase() == named);
        let mut albums = 0;
        // one album per artist is enough for the covers: the first with a picture in it, else the first
        let (mut pictured, mut first) = (false, None);
        let mut loose = false;
        for (p, is_dir) in list(artist) {
            if !is_dir {
                // a song right in the artist's folder, in no album
                if is_song(&p) {
                    found.songs += 1;
                    found.misplaced += 1;
                    loose = true;
                }
                continue;
            }
            let songs = count(&p, &list, &is_song, stop);
            if songs == 0 {
                continue;
            }
            found.songs += songs;
            albums += 1;
            if !pictured && with_picture.len() < 12 && list(&p).iter().any(|(f, d)| !d && is_picture(f)) {
                with_picture.push(p);
                pictured = true;
            } else if first.is_none() {
                first = Some(p);
            }
        }
        if let Some(p) = first.filter(|_| !pictured && without.len() < 12) {
            without.push(p);
        }
        // (an artist's loose songs are one more tile in the Library, "Loose tracks", and are counted as
        // it counts them, so the number here is the number Home and the Library show a moment later)
        albums += loose as usize;
        if albums > 0 && counts_as_artist {
            found.artists += 1;
        }
        found.albums += albums;
    }
    found.covers = with_picture.into_iter().chain(without).take(8).collect();
    found
}

/// A switch of a test run (only with `MUSIC_RECS_DATA` set, so the real app never reads it).
pub(crate) fn test_switch(name: &str) -> Option<String> {
    std::env::var_os("MUSIC_RECS_DATA").filter(|v| !v.is_empty())?;
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The "Music Library" folder the app makes when asked to: inside the user's own Music folder. (A test
/// run makes it inside its own data folder, never in the real Music folder.)
fn own_folder() -> Option<PathBuf> {
    if std::env::var_os("MUSIC_RECS_DATA").is_some_and(|v| !v.is_empty()) {
        return Some(crate::config::app_dir().join("Music Library"));
    }
    dirs::audio_dir().or_else(|| dirs::home_dir().map(|h| h.join("Music"))).map(|m| m.join("Music Library"))
}

/// How many covers there is room for beside the words of a card `room` wide: the words keep 240.
fn covers_that_fit(have: usize, room: f32) -> usize {
    have.min(7).min(((room - 240.0) / (COVER + 8.0)).floor().max(0.0) as usize)
}

// ------------------------------------------------------------------ the music folder

/// How the choice between repairing and only looking is asked.
pub(crate) enum Repairs<'a> {
    /// the first start: only of a library laid out another way, with "look only" chosen until the
    /// person says otherwise (it goes into the settings at "Start")
    First(&'a mut bool),
    /// Settings: always, and it is the setting itself
    Settings,
}

/// What a `tick` of the folder card brought.
#[derive(Default)]
pub(crate) struct Ticked {
    /// the Windows picker came back with another folder: it is in the settings now
    pub picked: bool,
    /// the card is about another folder than a moment ago
    pub another: bool,
}

/// The card that says where the music is and what is in it.
pub(crate) struct FolderCard {
    /// the folder `found` is about (as it stands in the settings), and what reading it found
    read: String,
    found: Option<Found>,
    /// a folder being read: how far, and the way to stop it
    reading: Option<(Receiver<Read>, Arc<AtomicBool>, (usize, usize))>,
    /// the Windows folder picker is open
    picking: Option<Receiver<Option<PathBuf>>>,
    /// why a folder could not be made or used
    trouble: Option<String>,
    /// read the folder once more though it is the same one (what was found stays on show meanwhile)
    again: bool,
    /// whether the folder is there, and when that was last looked at: asked at most every two seconds,
    /// so a sleeping drive never stalls the window on every redraw
    there: (f64, bool),
}

impl Default for FolderCard {
    fn default() -> Self {
        Self { read: String::new(), found: None, reading: None, picking: None, trouble: None, again: false, there: (-10.0, false) }
    }
}

impl FolderCard {
    /// Takes up what the picker and a folder being read have sent, and starts reading a folder that was
    /// just chosen.
    pub(crate) fn tick(&mut self, app: &mut App, ctx: &egui::Context) -> Ticked {
        let mut out = Ticked::default();
        let now = ctx.input(|i| i.time);
        if let Some(rx) = &self.picking {
            if let Ok(chosen) = rx.try_recv() {
                self.picking = None;
                if let Some(dir) = chosen {
                    let dir = dir.to_string_lossy().to_string();
                    out.picked = dir != app.cfg.music_dir;
                    app.cfg.music_dir = dir;
                    self.trouble = None;
                }
            }
        }
        let dir = app.cfg.music_dir.trim().to_string();
        out.another = dir != self.read;
        if out.another || now - self.there.0 > 2.0 {
            self.there = (now, !dir.is_empty() && Path::new(&dir).is_dir());
        }
        if out.another || self.again {
            // another folder: what was found, and anything still being read, is about the old one
            if let Some((_, stop, _)) = self.reading.take() {
                stop.store(true, Ordering::Relaxed);
            }
            if out.another {
                self.read = dir.clone();
                self.found = None;
            }
            self.again = false;
            if self.there.1 {
                let (tx, rx) = crossbeam_channel::unbounded();
                let stop = Arc::new(AtomicBool::new(false));
                self.reading = Some((rx, stop.clone(), (0, 0)));
                let skip: Vec<String> = app.cfg.skip_folders.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let far = |done: usize, total: usize| {
                        let _ = tx.send(Read::Far(done, total));
                        ctx.request_repaint();
                    };
                    let found = read_folder(Path::new(&dir), &skip, &stop, &far);
                    if !stop.load(Ordering::Relaxed) {
                        let _ = tx.send(Read::Done(found));
                        ctx.request_repaint();
                    }
                });
            } else {
                self.found = None;
            }
        }
        let mut done = None;
        if let Some((rx, _, far)) = &mut self.reading {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Read::Far(d, t) => *far = (d, t),
                    Read::Done(f) => done = Some(f),
                }
            }
        }
        if let Some(f) = done {
            self.found = Some(f);
            self.reading = None;
        }
        out
    }

    /// A folder is chosen and is there.
    pub(crate) fn ready(&self) -> bool {
        !self.read.is_empty() && self.there.1
    }

    /// What reading the folder found, once it has been read.
    pub(crate) fn found(&self) -> Option<&Found> {
        self.found.as_ref()
    }

    /// Reads the folder once more at the next `tick` (Settings was opened again: the numbers may have
    /// changed since). What was found before stays on show until the new numbers are in.
    pub(crate) fn read_again(&mut self) {
        self.again = true;
    }

    /// Stops a folder that is still being read.
    pub(crate) fn stop(&mut self) {
        if let Some((_, stop, _)) = self.reading.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// Opens the Windows picker for the music folder; what was chosen is taken up by `tick`.
    pub(crate) fn choose(&mut self, ctx: &egui::Context) {
        if self.picking.is_some() {
            return; // one picker at a time
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.picking = Some(rx);
        // (a test run names the folder instead of opening the Windows picker, which a script cannot work:
        // `MUSIC_RECS_SETUP_FOLDER`)
        if let Some(dir) = test_switch("MUSIC_RECS_SETUP_FOLDER") {
            let _ = tx.send(Some(PathBuf::from(dir)));
            return;
        }
        let (start, ctx) = (PathBuf::from(&self.read), ctx.clone());
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Choose your music folder");
            if start.is_dir() {
                dialog = dialog.set_directory(start);
            }
            let _ = tx.send(dialog.pick_folder());
            ctx.request_repaint();
        });
    }

    /// The card. True when something in the settings was changed here (the folder typed in or made, or
    /// the choice about repairs in Settings): Settings saves then.
    pub(crate) fn show(&mut self, app: &mut App, ui: &mut Ui, left: f32, wide: f32, mut repairs: Repairs) -> bool {
        let ctx = ui.ctx().clone();
        let in_settings = matches!(repairs, Repairs::Settings);
        let odd = self.found.as_ref().is_some_and(|f| f.odd());
        let ready = self.ready();
        let show_tree = self.read.is_empty() || odd || !ready;
        let mut changed = false;
        theme::panel(ui, left, wide, PAD, "music-folder", |ui| {
            let room = ui.max_rect();
            ui.set_min_height(86.0);
            // (the words leave room for the picture at the right; a narrow card has fewer covers, and no
            // drawing of the layout at all when it would stand on the words)
            let tree = show_tree && room.width() >= 420.0;
            let covers = if show_tree { 0 } else { self.found.as_ref().map_or(0, |f| covers_that_fit(f.covers.len(), room.width())) };
            let art_w = if tree { 170.0 } else { covers as f32 * (COVER + 8.0) };
            let words_w = (room.width() - art_w - 20.0).max(160.0);
            theme::caption(ui, "MUSIC FOLDER");
            ui.add_space(8.0);
            if self.read.is_empty() {
                let words = ui.painter().layout_no_wrap("No folder chosen yet".to_string(), theme::display(22.0), DIM());
                let (rect, _) = ui.allocate_exact_size(words.size(), Sense::hover());
                ui.painter().galley(rect.min, words, DIM());
            } else {
                // the folder as large words; a click on them turns them into a box to type a path in
                let mut dir = app.cfg.music_dir.clone();
                if typed_path(ui, "music", &self.read, &mut dir, theme::display(22.0), TEXT(), words_w) {
                    app.cfg.music_dir = dir;
                    self.trouble = None;
                    changed = true;
                }
            }
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(words_w, 0.0), Layout::left_to_right(Align::Center).with_main_wrap(true), |ui| {
                ui.spacing_mut().item_spacing = vec2(18.0, 4.0);
                if self.read.is_empty() {
                    if theme::main_action(ui, "Choose a folder", true).clicked() {
                        self.choose(&ctx);
                    }
                    if theme::quiet(ui, "Make one for me", 14.0, true).clicked() {
                        match own_folder().map(|d| std::fs::create_dir_all(&d).map(|_| d)) {
                            Some(Ok(d)) => {
                                app.cfg.music_dir = d.to_string_lossy().to_string();
                                self.trouble = None;
                                changed = true;
                            }
                            Some(Err(e)) => self.trouble = Some(format!("The folder could not be made ({}).", e.kind())),
                            None => self.trouble = Some("Windows did not say where your Music folder is. Choose a folder instead.".to_string()),
                        }
                    }
                    match &self.trouble {
                        Some(why) => drop(ui.label(RichText::new(why).size(12.5).color(WARN()))),
                        None => drop(ui.label(RichText::new("a \"Music Library\" folder inside your Music folder").size(12.5).color(FAINT()))),
                    }
                } else if !ready {
                    ui.label(RichText::new("This folder was not found. Is the drive connected?").size(14.0).color(WARN()));
                    if theme::main_action(ui, "Choose a folder", true).clicked() {
                        self.choose(&ctx);
                    }
                } else {
                    match (&self.found, &self.reading) {
                        (Some(f), _) => drop(ui.label(RichText::new(f.words()).size(14.0).color(if f.odd() { WARN() } else { DIM() }))),
                        (None, Some((_, _, (done, total)))) => {
                            ui.add(egui::Spinner::new().size(12.0).color(DIM()));
                            let far = if *total > 0 { format!("Reading it, {} of {} folders", commas(*done as u64), commas(*total as u64)) } else { "Reading it".to_string() };
                            ui.label(RichText::new(far).size(14.0).color(DIM()));
                        }
                        _ => {}
                    }
                    if theme::quiet(ui, "Change", 14.0, true).clicked() {
                        self.choose(&ctx);
                    }
                    if in_settings && theme::quiet(ui, "Open", 14.0, true).clicked() {
                        let _ = open::that(&self.read);
                    }
                }
            });
            // repairs move and rename files: for a library laid out another way they wait for a yes
            match &mut repairs {
                // a row with a menu, as every named choice in Settings is (`rows::pick_one`)
                Repairs::First(look_only) if odd => {
                    ui.add_space(6.0);
                    ui.allocate_ui_with_layout(vec2(words_w.max(room.width() - art_w), 0.0), Layout::top_down(Align::Min), |ui| {
                        repairs_row(ui, look_only);
                    });
                }
                Repairs::First(_) => {}
                Repairs::Settings => {
                    ui.add_space(6.0);
                    ui.allocate_ui_with_layout(vec2(words_w.max(room.width() - art_w), 0.0), Layout::top_down(Align::Min), |ui| {
                        changed |= repairs_row(ui, &mut app.cfg.look_only);
                    });
                }
            }
            // the picture at the right: the layout the app files music in, or what the folder holds
            if tree {
                let font = FontId::proportional(12.5);
                let lines = [("Music", 0.0), ("Artist", 14.0), ("Album (Year)", 28.0), ("01 Song.mp3", 42.0)];
                for (i, (name, indent)) in lines.iter().enumerate() {
                    let at = pos2(room.right() - 150.0 + indent, room.top() + 2.0 + i as f32 * 19.0);
                    let color = if i == 3 { FAINT() } else { DIM() };
                    ui.painter().text(at, egui::Align2::LEFT_TOP, *name, font.clone(), color);
                    if i > 0 {
                        // the small corner that hangs each name under the one before it
                        let (x, y) = (at.x - 9.0, at.y + 9.0);
                        ui.painter().line_segment([pos2(x, y - 8.0), pos2(x, y)], egui::Stroke::new(1.0, FAINT()));
                        ui.painter().line_segment([pos2(x, y), pos2(x + 6.0, y)], egui::Stroke::new(1.0, FAINT()));
                    }
                }
            } else if let Some(f) = &self.found {
                let px = (COVER * ui.ctx().pixels_per_point()).round() as u32;
                for (i, folder) in f.covers.iter().take(covers).enumerate() {
                    let at = Rect::from_min_size(pos2(room.right() - (covers - i) as f32 * (COVER + 8.0) + 8.0, room.top() + 6.0), vec2(COVER, COVER));
                    crate::covers::paint(ui, crate::covers::for_folder(ui.ctx(), folder, px), at, 5);
                }
            }
        });
        changed
    }
}

/// A path as words, cut short with "…" at `width`; a click on them turns them into a box to type in,
/// with the path chosen so that typing replaces it. Enter, or a click elsewhere, takes the typed path up;
/// Escape leaves it as it was. `shown` is what the words say (the path, or only its last part), `value`
/// the path itself. True when another path was typed. `name` tells one path from another on a page and
/// is what a test script calls the words (`path:<name>`).
pub(crate) fn typed_path(ui: &mut Ui, name: &str, shown: &str, value: &mut String, font: FontId, color: Color32, width: f32) -> bool {
    let id = egui::Id::unique(("typed-path", name));
    let box_id = id.with("box");
    // (the text being typed, and how many frames the box has been open: it asks for the keyboard on its
    // first, and its whole text is chosen on its second, once the keyboard is its own)
    let typing: Option<(String, u8)> = ui.data(|d| d.get_temp(id));
    let line = ui.painter().layout_no_wrap("Ag".to_string(), font.clone(), color).size().y;
    match typing {
        Some((mut text, frames)) => {
            let (rect, _) = ui.allocate_exact_size(vec2(width, line), Sense::hover());
            ui.painter().rect_filled(rect.expand2(vec2(6.0, 2.0)), 6.0, theme::field_bg());
            if frames == 1 {
                // the whole path is chosen as the box opens
                let mut state = TextEdit::load_state(ui.ctx(), box_id).unwrap_or_default();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::select_all_str(&text)));
                state.store(ui.ctx(), box_id);
            }
            let r = ui.put(rect, TextEdit::singleline(&mut text).id(box_id).font(font).text_color(color).frame(egui::Frame::NONE).margin(Margin::ZERO).desired_width(width).clip_text(true));
            script::seen(&format!("typing:{name}"), rect);
            if frames == 0 {
                r.request_focus();
            }
            if frames > 0 && r.lost_focus() {
                let left = ui.input(|i| i.key_pressed(egui::Key::Escape));
                ui.data_mut(|d| d.remove::<(String, u8)>(id));
                let typed = text.trim().trim_matches('"').to_string();
                if !left && typed != value.trim() {
                    *value = typed;
                    return true;
                }
            } else {
                ui.data_mut(|d| d.insert_temp(id, (text, frames.saturating_add(1).min(2))));
                if frames < 2 {
                    ui.ctx().request_repaint();
                }
            }
        }
        None => {
            let words = theme::cut(ui, shown, font, color, width);
            let (rect, r) = ui.allocate_exact_size(vec2(words.size().x, line), Sense::click());
            script::seen(&format!("path:{name}"), rect);
            ui.painter().galley(rect.min, words, color);
            if r.hovered() {
                ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, egui::Stroke::new(1.0, FAINT()));
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
            if r.clicked() {
                ui.data_mut(|d| d.insert_temp(id, (value.clone(), 0u8)));
            }
            if !value.trim().is_empty() {
                let _ = r.on_hover_text(value.as_str());
            }
        }
    }
    false
}

// ------------------------------------------------------------------ what the app needs

/// One thing the app needs: its name, a quiet line under it, and at the right how it stands or the
/// word that gets it.
fn need(ui: &mut Ui, name: &str, under: &str, under_color: Color32, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(Label::new(RichText::new(name).size(15.0).color(TEXT())).selectable(false));
            ui.add(Label::new(RichText::new(under).size(12.5).color(under_color)).wrap());
        });
        ui.with_layout(Layout::right_to_left(Align::Min), right);
    });
}

/// A small green dot, on a line of words.
fn green_dot(ui: &mut Ui) {
    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
    ui.painter().circle_filled(dot.center() + vec2(0.0, 1.0), 3.5, GREEN());
}


/// The card of what the app needs: ffmpeg and PowerShell 7, each with how it stands on this PC or the
/// word that gets it. `ffmpeg` is whether ffmpeg is there and when that was last looked at.
pub(crate) fn needs(ui: &mut Ui, ffmpeg: &mut (f64, bool)) -> Foot {
    let ctx = ui.ctx().clone();
    let now = ui.input(|i| i.time);
    // (looked for at most every two seconds, and again as soon as a fetch has put it in place)
    if now - ffmpeg.0 > 2.0 {
        *ffmpeg = (now, crate::importer::tools::find_tool("ffmpeg").is_ok() && crate::importer::tools::find_tool("ffprobe").is_ok());
    }
    theme::caption(ui, "WHAT IT NEEDS");
    ui.add_space(12.0);
    let state = helpers::state(Helper::Ffmpeg);
    match &state {
        helpers::State::Getting(done, total) => {
            need(ui, "ffmpeg", &helpers::amount(*done, *total), FAINT(), |ui| {
                if theme::quiet(ui, "Stop", 14.0, true).clicked() {
                    helpers::stop(Helper::Ffmpeg);
                }
            });
            ui.add_space(2.0);
            theme::progress(ui, *done as f32 / (*total).max(1) as f32, ui.available_width(), ACCENT2());
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        helpers::State::Unpacking => {
            need(ui, "ffmpeg", "Unpacking", FAINT(), |ui| drop(ui.add(egui::Spinner::new().size(13.0).color(DIM()))));
            ffmpeg.0 = -10.0;
        }
        helpers::State::Failed(why) => need(ui, "ffmpeg", why, WARN(), |ui| {
            if theme::main_action_sized(ui, "Try again", 14.0, true).clicked() {
                helpers::start(Helper::Ffmpeg, &ctx);
            }
        }),
        // there: only a green check. Not there: in its place the word that gets it, and its size
        helpers::State::Idle if ffmpeg.1 => need(ui, "ffmpeg", "for imports and checks", FAINT(), |ui| rows::there(ui, "there:ffmpeg")),
        helpers::State::Idle => {
            let size = format!("{} MB", (Helper::Ffmpeg.pin().size as f64 / 1_000_000.0).round());
            need(ui, "ffmpeg", "for imports and checks", FAINT(), |ui| {
                ui.label(RichText::new(size).size(12.5).color(FAINT()));
                if theme::main_action_sized(ui, "Get it", 14.0, true).clicked() {
                    helpers::start(Helper::Ffmpeg, &ctx);
                }
            });
        }
    }
    ui.add_space(14.0);
    // (one loud word a card: while ffmpeg's is there, this one is quiet)
    let ffmpeg_missing = !ffmpeg.1;
    if crate::importer::tools::pwsh().is_some() {
        need(ui, "PowerShell 7", "for imports and tag repairs", FAINT(), |ui| rows::there(ui, "there:pwsh"));
    } else if helpers::pwsh_installing() {
        need(ui, "PowerShell 7", "Installing in its own window", FAINT(), |ui| drop(ui.add(egui::Spinner::new().size(13.0).color(DIM()))));
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    } else {
        need(ui, "PowerShell 7", "for imports and tag repairs", FAINT(), |ui| {
            let clicked = if ffmpeg_missing { theme::quiet(ui, "Install", 14.0, true).clicked() } else { theme::main_action_sized(ui, "Install", 14.0, true).clicked() };
            if clicked {
                helpers::install_pwsh();
            }
        });
    }
    None
}

// ------------------------------------------------------------------ the two logins

/// A card's small heading, with "optional" at its right end while the card may be left alone.
fn head(ui: &mut Ui, name: &str, optional: bool) {
    ui.horizontal(|ui| {
        theme::caption(ui, name);
        if optional {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new("optional").size(12.0).color(FAINT())));
        }
    });
}

/// A box to type in, as wide as its card.
fn box_(ui: &mut Ui, edit: TextEdit) -> egui::Response {
    let w = ui.available_width();
    ui.add_sized([w, 28.0], edit.clip_text(true).desired_width(w))
}

/// A box for a password or a key, as wide as its card: its letters are dots until "show" at its right
/// end is clicked. `name` tells the boxes of a page apart for a test script (`show:<name>`).
fn secret(ui: &mut Ui, name: &str, text: &mut String, hint: &str, show: &mut bool) -> egui::Response {
    let word = if *show { "hide" } else { "show" };
    let id = egui::Id::unique(("secret-show", name));
    // the word is the box's own last part, so the box keeps its look and the typed letters stop short
    // of it; bright under the pointer (as it stood a frame ago: the box is drawn before the word is asked)
    let hot = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let r = box_(ui, TextEdit::singleline(text).hint_text(hint).password(!*show).suffix(RichText::new(format!(" {word}")).size(12.0).color(if hot { TEXT() } else { FAINT() })));
    let wide = ui.painter().layout_no_wrap(word.to_string(), FontId::proportional(12.0), FAINT()).size().x;
    let at = Rect::from_min_max(pos2(r.rect.right() - wide - 18.0, r.rect.top()), r.rect.right_bottom());
    let w = ui.interact(at, id, Sense::click());
    script::seen(&format!("show:{name}"), at);
    if w.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        *show = !*show;
    }
    r
}

/// The Last.fm card: the name, the key, and the words that get a key and check the login. True when the
/// name or the key was changed (Settings saves then; the first-start screen saves at "Start").
pub(crate) fn lastfm(app: &mut App, ui: &mut Ui) -> bool {
    let ctx = ui.ctx().clone();
    head(ui, "LAST.FM", !app.cfg.has_lastfm());
    ui.add_space(8.0);
    let mut changed = box_(ui, TextEdit::singleline(&mut app.cfg.lastfm_user).hint_text("User name")).changed();
    ui.add_space(6.0);
    changed |= secret(ui, "lastfm", &mut app.cfg.lastfm_api_key, "API key", &mut app.show_key).changed();
    if changed {
        // what a check said was about the login as it stood
        app.test_result = None;
    }
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(16.0, 4.0);
        match (&app.test_result, app.test_rx.is_some()) {
            (_, true) => drop(ui.add(egui::Spinner::new().size(13.0).color(DIM()))),
            (Some(Ok(who)), _) => {
                green_dot(ui);
                ui.label(RichText::new(who).size(13.0).color(DIM()));
            }
            (Some(Err(why)), _) => drop(ui.add(Label::new(RichText::new(why).size(13.0).color(WARN())).wrap())),
            _ => {}
        }
        if app.test_result.as_ref().is_none_or(|r| r.is_err()) && app.test_rx.is_none() {
            if theme::quiet(ui, "Get a free API key", 13.0, true).clicked() {
                util::open_url("https://www.last.fm/api/account/create");
            }
            if theme::quiet(ui, "Check this login", 13.0, !app.cfg.lastfm_user.trim().is_empty() && !app.cfg.lastfm_api_key.trim().is_empty()).clicked() {
                app.test_lastfm(&ctx);
            }
        }
    });
    changed
}

/// "Repairs on the Health page", with its menu: allowed, or look only. True when it was changed.
fn repairs_row(ui: &mut Ui, look_only: &mut bool) -> bool {
    const WAYS: [rows::Item; 2] = [
        rows::Item { name: "Allowed", about: "Health can repair what it finds; every repair can be undone." },
        rows::Item { name: "Look only, change nothing", about: "Health reports, and the library is never touched." },
    ];
    rows::row(ui, "repairs", "Repairs on the Health page", None, |ui| match rows::pick_one(ui, "pick:repairs", &WAYS, usize::from(*look_only)) {
        Some(n) => {
            *look_only = n == 1;
            true
        }
        None => false,
    })
}
/// What the Soulseek card works on.
pub(crate) struct Slsk<'a> {
    pub user: &'a mut String,
    pub pass: &'a mut String,
    /// the password's letters are on show
    pub show: &'a mut bool,
    /// a password is saved already: its box says so
    pub saved: bool,
    /// "optional" stands at the card's top
    pub optional: bool,
}

/// The Soulseek card: the login the Downloads page searches with (what happens to a lossless download is
/// the card of its own under it, `lossless`). `row` draws the one line under the two boxes (Settings: how the
/// sign-in stands, or the word that saves the login); `None` leaves that line out (the first start,
/// where "Start" saves).
pub(crate) fn soulseek(ui: &mut Ui, s: Slsk, row: Option<&mut dyn FnMut(&mut Ui)>) {
    head(ui, "SOULSEEK", s.optional);
    ui.add_space(8.0);
    box_(ui, TextEdit::singleline(s.user).hint_text("Name"));
    ui.add_space(6.0);
    secret(ui, "soulseek", s.pass, if s.saved { "Saved. Type to change it" } else { "Password" }, s.show);
    if let Some(row) = row {
        ui.add_space(8.0);
        // (one height whatever it says, so nothing under it moves)
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 20.0), Layout::left_to_right(Align::Center).with_main_wrap(true), |ui| {
            ui.set_min_height(20.0);
            ui.set_row_height(20.0);
            ui.spacing_mut().item_spacing = vec2(12.0, 4.0);
            row(ui);
        });
    }
}

/// One of the two ways a lossless album can come in, as `lossless` draws it.
struct Way {
    keep: bool,
    from: &'static str,
    to: &'static str,
    /// the format it comes out as is lossless too (drawn in blue, as FLAC going in is)
    to_lossless: bool,
    name: &'static str,
    why: &'static str,
    /// a typical album of this kind, in MB, and how long its bar is
    size: &'static str,
    bar: f32,
}

const WAYS: [Way; 2] = [
    Way { keep: false, from: "FLAC", to: "MP3 320", to_lossless: false, name: "Convert to MP3 320", why: "About a third of the room, and it plays on anything. Tags, cover and lyrics come along.", size: "about 110 MB an album", bar: 34.0 },
    Way { keep: true, from: "FLAC", to: "FLAC", to_lossless: true, name: "Keep lossless", why: "The sound exactly as downloaded. WAV, ALAC and APE become FLAC.", size: "about 300 MB an album", bar: 100.0 },
];

/// The card of its own under the three: what an import does with a lossless album (`keep`: it stays
/// lossless). Each way is a small picture of what happens, the format going in and coming out, why, and
/// how much room an album takes; the chosen one is lit and a click on the other chooses it. Side by side,
/// one under the other in a narrow card, and one faint line under them. True when the choice was changed.
pub(crate) fn lossless(ui: &mut Ui, keep: &mut bool) -> bool {
    theme::caption(ui, "WHEN A LOSSLESS ALBUM IS IMPORTED");
    ui.add_space(10.0);
    let full = ui.available_width();
    let side = full >= 640.0;
    let (gap, inset) = (18.0, 10.0);
    let w = if side { ((full - gap) / 2.0).floor() } else { full };
    // (both are as tall as the taller, so they read across)
    let text_w = w - 2.0 * inset - 25.0;
    let whys: Vec<_> = WAYS.iter().map(|way| ui.painter().layout(way.why.to_string(), FontId::proportional(12.5), DIM(), text_w)).collect();
    let h = (inset + 20.0 + 4.0 + whys.iter().map(|g| g.size().y).fold(0.0, f32::max) + 6.0 + 14.0 + inset).ceil();
    let rows = if side { 1 } else { 2 };
    let (area, _) = ui.allocate_exact_size(vec2(full, h * rows as f32 + if side { 0.0 } else { 8.0 }), Sense::hover());
    let mut changed = false;
    for (n, (way, why)) in WAYS.iter().zip(whys).enumerate() {
        let at = if side { pos2(area.left() + n as f32 * (w + gap), area.top()) } else { pos2(area.left(), area.top() + n as f32 * (h + 8.0)) };
        let rect = Rect::from_min_size(at, vec2(w, h));
        let chosen = *keep == way.keep;
        let r = ui.interact(rect, egui::Id::unique(("lossless-way", way.keep)), Sense::click());
        script::seen(if way.keep { "lossless:keep" } else { "lossless:convert" }, rect);
        let p = ui.painter();
        if chosen {
            p.rect_filled(rect, 8, theme::CARD_HOVER());
        } else if r.hovered() {
            p.rect_filled(rect, 8, theme::CARD_HOVER().gamma_multiply(0.5));
        }
        if r.hovered() && !chosen {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        // the round mark: coral and filled when chosen
        let dot = pos2(rect.left() + inset + 6.0, rect.top() + inset + 10.0);
        if chosen {
            p.circle_stroke(dot, 6.0, egui::Stroke::new(1.4, theme::ACCENT()));
            p.circle_filled(dot, 3.0, theme::ACCENT());
        } else {
            p.circle_stroke(dot, 6.0, egui::Stroke::new(1.4, if r.hovered() { DIM() } else { FAINT() }));
        }
        // the picture: the format going in, an arrow, the format coming out, then its name
        let mut x = rect.left() + inset + 25.0;
        let mid = dot.y;
        let chip = |p: &egui::Painter, x: f32, words: &str, lossless: bool| -> f32 {
            let color = if lossless { ACCENT2() } else { TEXT() };
            let g = p.layout_no_wrap(words.to_string(), FontId::proportional(10.5), color);
            let r = Rect::from_min_size(pos2(x, (mid - g.size().y / 2.0 - 2.0).round()), g.size() + vec2(12.0, 4.0));
            p.rect_stroke(r, 4, egui::Stroke::new(1.0, if lossless { ACCENT2().gamma_multiply(0.45) } else { theme::BORDER().lerp_to_gamma(TEXT(), 0.25) }), egui::StrokeKind::Inside);
            p.galley(pos2(r.left() + 6.0, r.top() + 2.0), g, color);
            r.right()
        };
        x = chip(p, x, way.from, true) + 7.0;
        let arrow = p.layout_no_wrap("\u{2192}".to_string(), FontId::proportional(12.0), FAINT());
        p.galley(pos2(x, (mid - arrow.size().y / 2.0).round()), arrow.clone(), FAINT());
        x += arrow.size().x + 7.0;
        x = chip(p, x, way.to, way.to_lossless) + 10.0;
        let name_color = if chosen { TEXT() } else { DIM() };
        let name = p.layout_no_wrap(way.name.to_string(), FontId::proportional(14.0), name_color);
        p.galley(pos2(x, (mid - name.size().y / 2.0).round()), name, name_color);
        // why, and how much room an album takes, as a bar and in words
        let text_x = rect.left() + inset + 25.0;
        let why_color = if chosen { DIM() } else { FAINT() };
        let why_top = rect.top() + inset + 24.0;
        let why_h = why.size().y;
        p.galley_with_override_text_color(pos2(text_x, why_top), why, why_color);
        let bar_y = why_top + why_h + 6.0 + 7.0;
        let bar = Rect::from_min_size(pos2(text_x, bar_y - 2.5), vec2(way.bar, 5.0));
        p.rect_filled(bar, 3, if chosen { DIM() } else { theme::BORDER().lerp_to_gamma(FAINT(), 0.3) });
        let size = p.layout_no_wrap(way.size.to_string(), FontId::proportional(11.5), FAINT());
        p.galley(pos2(bar.right() + 6.0, (bar_y - size.size().y / 2.0).round()), size, FAINT());
        if r.clicked() && !chosen {
            *keep = way.keep;
            changed = true;
        }
    }
    ui.add_space(12.0);
    ui.add(Label::new(RichText::new(LOSSLESS_FOOT).size(12.5).color(FAINT())).wrap());
    changed
}

/// What the lossless card says at its foot.
pub(crate) const LOSSLESS_FOOT: &str = "MP3 and other lossy downloads come in as they are.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lossless_card_offers_convert_first_and_says_what_each_way_does() {
        // (the first-start screen starts on the first: MP3 320 for someone new)
        assert!(!WAYS[0].keep && WAYS[1].keep);
        assert_eq!((WAYS[0].from, WAYS[0].to, WAYS[1].to), ("FLAC", "MP3 320", "FLAC"));
        // the bars keep the sizes' proportion (about 110 MB to about 300 MB)
        assert!((WAYS[0].bar / WAYS[1].bar - 110.0 / 300.0).abs() < 0.05);
        for w in &WAYS {
            assert!(!w.why.contains('\u{2014}'), "no em dashes");
        }
    }

    fn lab(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("music-recs-setup-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    fn read(dir: &Path, skip: &[&str]) -> Found {
        read_folder(dir, &skip.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &AtomicBool::new(false), &|_, _| {})
    }

    #[test]
    fn a_folder_of_artists_and_albums_is_counted_and_reads_as_laid_out_right() {
        let d = lab("right");
        for (artist, album, songs) in [("Night Ferry", "Tide Glass (2024)", 3), ("Night Ferry", "Last Boat (2020)", 2), ("Low Sun", "Light Engines (2025)", 4)] {
            for n in 1..=songs {
                put(&d, &format!("{artist}\\{album}\\0{n} Song.mp3"));
            }
        }
        // a disc folder inside an album, a cover, a folder that is not an artist, and things that are not music
        put(&d, "Low Sun\\Light Engines (2025)\\CD2\\01 Extra.flac");
        put(&d, "Night Ferry\\Tide Glass (2024)\\cover.jpg");
        put(&d, "Compilations\\Summer Mix\\01 One.mp3");
        put(&d, "Low Sun\\notes.txt");
        put(&d, "$RECYCLE.BIN\\S-1\\old.mp3");
        let f = read(&d, &["Compilations"]);
        assert_eq!((f.artists, f.albums, f.songs, f.misplaced), (2, 4, 11, 0));
        assert!(!f.odd());
        assert_eq!(f.words(), "4 albums by 2 artists");
        // the album with a picture in it comes first among the covers
        assert_eq!(f.covers.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).as_deref(), Some("Tide Glass (2024)"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_library_laid_out_another_way_is_told_apart_and_an_empty_folder_says_so() {
        // albums at the top with no artist folder over them, and songs lying loose
        let d = lab("odd");
        for n in 1..=4 {
            put(&d, &format!("Night Ferry - Tide Glass\\0{n} Song.mp3"));
        }
        put(&d, "Stray.mp3");
        put(&d, "Low Sun\\Light Engines (2025)\\01 Song.mp3");
        let f = read(&d, &[]);
        assert_eq!((f.songs, f.misplaced, f.albums, f.artists), (6, 5, 2, 2));
        assert!(f.odd());
        assert_eq!(f.words(), "5 of 6 songs are not in Artist \\ Album folders");
        // a fifth or less out of place is an ordinary library with a few loose songs
        let few = Found { songs: 100, misplaced: 20, albums: 9, artists: 4, covers: vec![] };
        assert!(!few.odd());
        assert_eq!(few.words(), "9 albums by 4 artists");
        assert!(Found { songs: 100, misplaced: 21, ..Default::default() }.odd());

        let empty = lab("empty");
        let f = read(&empty, &[]);
        assert_eq!((f.songs, f.odd()), (0, false));
        assert_eq!(f.words(), "Empty so far. Albums you import are filed here.");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn a_narrow_card_has_fewer_covers_and_the_words_keep_their_room() {
        // the first-start screen's card at its full width: all seven
        assert_eq!(covers_that_fit(8, 952.0), 7);
        assert_eq!(covers_that_fit(3, 952.0), 3);
        // a window about 640 wide: the covers that are left never reach into the words' 240
        let n = covers_that_fit(8, 568.0);
        assert_eq!(n, 4);
        assert!(568.0 - n as f32 * (COVER + 8.0) >= 240.0);
        // too narrow for any
        assert_eq!(covers_that_fit(8, 280.0), 0);
        assert_eq!(covers_that_fit(8, 100.0), 0);
    }
}
