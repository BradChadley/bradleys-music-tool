//! Settings: a page in place of the pages, in the look of the first-start screen (`setup`): one centered
//! stretch of the window, cards, a large light heading. The page names stay in the top row; a page name,
//! the gear or Escape leaves. Five sections, one at a time (`SECTIONS`; the fifth, About, holds the
//! version, new versions of the app, and the ways to its author):
//!   Basics  the four cards of the first-start screen, drawn by the same code (`cards`)
//!   Files   where downloads arrive, the backup tool, artist names, lyrics
//!   Recs    what the Recs page looks for, and the artists muted and pinned
//!   Look    themes, single colors, size
//! Under Look, "Try it on the pages" folds Settings down to the one Look card at the right of the pages
//! (`beside`), because colors are judged on the pages. Everything is saved as it is changed.
use crate::cards::{self, FolderCard, GAP, PAD};
use crate::helpers::{self, Helper};
use crate::theme::{self, ACCENT, DIM, FAINT, GREEN, TEXT, WARN};
use crate::{backup, downloads, rows, script, slskd, App};
use egui::{pos2, vec2, Align, Color32, FontId, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui};
use std::path::Path;

/// The sections, in the order their names stand under the heading.
pub(crate) const SECTIONS: [&str; 5] = ["Basics", "Files", "Recs", "Look", "About"];
pub(crate) const BASICS: usize = 0;
pub(crate) const FILES: usize = 1;
const RECS: usize = 2;
const LOOK: usize = 3;
pub(crate) const ABOUT: usize = 4;
/// The page's one stretch of the window (the first-start screen's).
const PAGE: f32 = 1000.0;
/// The one column of rows the other sections are, so a row's words and its control stay near each other.
const ROWS: f32 = 680.0;
/// The one column of rows the other sections are, so a row's words and its control stay near each other. ROWS: f32 = 680.0;
/// How wide the Look card's strip is beside the pages.
pub(crate) const BESIDE: f32 = 392.0;
/// Basics' three cards stand side by side from this width of the page's stretch.
const THREE: f32 = 820.0;

/// What Settings keeps between frames beyond the settings themselves.
pub(crate) struct State {
    /// the card that says where the music is and what is in it
    pub(crate) folder: FolderCard,
    /// whether ffmpeg is on this PC, and when that was last looked at
    ffmpeg: (f64, bool),
    /// only the Look card is on show, at the right of the pages
    pub(crate) beside: bool,
    /// Settings was on show the frame before (opening it reads the music folder again)
    shown: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { folder: FolderCard::default(), ffmpeg: (-10.0, false), beside: false, shown: false }
    }
}

impl State {
    /// Settings is not on show: the next time it opens, it opens whole and reads the folder again.
    pub(crate) fn closed(&mut self) {
        self.shown = false;
        self.beside = false;
    }
}

/// Settings, in place of the pages.
pub(crate) fn page(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let now = ui.input(|i| i.time);
    let mut st = std::mem::take(&mut app.settings);
    if !st.shown {
        st.shown = true;
        st.folder.read_again();
    }
    let mut changed = picked(app);
    changed |= st.folder.tick(app, &ctx).picked;
    egui::CentralPanel::default().frame(theme::page_frame()).show(ui, |ui| {
        ScrollArea::vertical().id_salt(("settings-scroll", app.settings_tab)).auto_shrink([false, false]).show(ui, |ui| {
            // (Basics keeps the first-start screen's cards side by side; the other sections are one column of rows)
            let (left, wide) = theme::column(ui, if app.settings_tab == BASICS { PAGE } else { ROWS });
            top(app, ui, now);
            changed |= match app.settings_tab {
                BASICS => basics(app, &mut st, ui, left, wide, now),
                FILES => files(app, ui, left, wide, now),
                RECS => recs(app, ui, left, wide, now),
                LOOK => look(app, &mut st, ui, left, wide, now),
                _ => about(app, ui, left, wide),
            };
            ui.add_space(24.0);
        });
    });
    saved(app, &ctx, changed, now);
    app.settings = st;
}

/// Only the Look card, in a strip at the right of the pages, which stay free to walk through: colors
/// are judged on them. Its X brings the whole of Settings back.
pub(crate) fn beside(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let now = ui.input(|i| i.time);
    let mut st = std::mem::take(&mut app.settings);
    st.shown = true;
    let mut changed = false;
    ScrollArea::vertical().id_salt("look-beside-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let (left, wide) = (ui.max_rect().left(), ui.available_width());
        theme::panel(ui, left, wide, PAD, "look-beside", |ui| {
            changed |= themes(app, &mut st, ui, now);
            ui.add_space(20.0);
            sizes(ui);
        });
    });
    saved(app, &ctx, changed, now);
    app.settings = st;
}

/// A change is kept at once, and the page says so for a moment.
fn saved(app: &mut App, ctx: &egui::Context, changed: bool, now: f64) {
    if changed {
        app.cfg.save();
        app.saved_at = Some(now);
        // (what needs a look may have changed with it)
        app.attention_cache.0 = -10.0;
    }
    if app.saved_at.is_some_and(|t| now - t < 3.0) || app.cache_cleared_at.is_some_and(|t| now - t < 3.5) {
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }
}

/// A folder or file chosen in the Windows picker has come back (see `App::pick`).
fn picked(app: &mut App) -> bool {
    let Some((which, rx)) = &app.pick_rx else { return false };
    let Ok(found) = rx.try_recv() else { return false };
    let which = *which;
    app.pick_rx = None;
    let Some(p) = found else { return false };
    let text = p.to_string_lossy().to_string();
    match which {
        "downloads" => app.cfg.downloads_dir = text,
        "backup-tool" => app.cfg.backup_tool_dir = text,
        _ => app.cfg.alias_file = text,
    }
    true
}

/// The top of the page: "Settings", one line under it, and the sections. The line says that changes are
/// saved as they are made, "Saved" for a moment after one, or, in the warning color, what needs a look
/// (each a way to its section).
fn top(app: &mut App, ui: &mut Ui, now: f64) {
    ui.add_space(8.0);
    theme::centered(ui, "Settings", theme::display(34.0), TEXT());
    ui.add_space(2.0);
    let attention = app.attention(now);
    if !attention.is_empty() {
        for (words, tab) in attention {
            if theme::centered_word(ui, &format!("attention:{words}"), &words, 12.5, WARN()).clicked() {
                app.settings_tab = tab;
            }
        }
    } else if app.saved_at.is_some_and(|t| now - t < 2.5) {
        theme::centered(ui, "Saved", FontId::proportional(12.5), GREEN());
    } else {
        theme::centered(ui, "Changes are saved as you make them", FontId::proportional(12.5), FAINT());
    }
    ui.add_space(16.0);

    // the sections: measured before they are drawn, so they stand in the middle from the first frame
    const BETWEEN: f32 = 30.0;
    let words: Vec<_> = SECTIONS.iter().map(|name| ui.painter().layout_no_wrap(name.to_string(), FontId::proportional(15.0), Color32::PLACEHOLDER)).collect();
    let total = words.iter().map(|w| w.size().x).sum::<f32>() + BETWEEN * (words.len() - 1) as f32;
    let high = words[0].size().y;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), high + 8.0), Sense::hover());
    let mut x = (row.center().x - total / 2.0).round();
    for (n, (name, word)) in SECTIONS.iter().zip(words).enumerate() {
        let at = Rect::from_min_size(pos2(x, row.top()), word.size());
        let r = ui.interact(at.expand2(vec2(BETWEEN / 2.0, 6.0)), egui::Id::unique(("settings-section", n)), Sense::click());
        script::seen(&format!("settings:{name}"), at);
        let on = app.settings_tab == n;
        let color = if on {
            TEXT()
        } else if r.hovered() {
            DIM().lerp_to_gamma(TEXT(), 0.6)
        } else {
            DIM()
        };
        x += word.size().x + BETWEEN;
        ui.painter().galley(at.min, word, color);
        if on {
            ui.painter().hline(at.x_range(), at.bottom() + 4.0, Stroke::new(2.0, ACCENT()));
        }
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            app.settings_tab = n;
        }
    }
    ui.add_space(18.0);
}

// ------------------------------------------------------------------ Basics

/// The four cards of the first-start screen: the music folder, what the app needs, Last.fm, Soulseek.
fn basics(app: &mut App, st: &mut State, ui: &mut Ui, left: f32, wide: f32, now: f64) -> bool {
    let ctx = ui.ctx().clone();
    let mut changed = st.folder.show(app, ui, left, wide, cards::Repairs::Settings);
    ui.add_space(GAP);

    // how the Soulseek login stands, for the one line under its two boxes
    if !app.slsk_loaded {
        app.slsk_loaded = true;
        app.slsk_user = slskd::login_name().unwrap_or_default();
    }
    // (read from the file at most every two seconds, not on every redraw)
    let has_saved = match app.path_ok.get("slsk-login") {
        Some((_, ok, at)) if now - at < 2.0 => *ok,
        _ => {
            let ok = slskd::read_settings().map(|s| s.has_login).unwrap_or(false);
            app.path_ok.insert("slsk-login", (String::new(), ok, now));
            ok
        }
    };
    let client = slskd::exe().exists();
    let fetch = helpers::state(Helper::Soulseek);
    let stands = app.downloads.as_ref().map(|d| d.status_words());
    let typed = !app.slsk_pass.is_empty();
    let ready = typed && !app.slsk_user.trim().is_empty();
    let trouble = match &app.slsk_msg {
        Some(Err(why)) => Some(why.clone()),
        _ => None,
    };
    let mut save = false;

    theme::card_row(ui, "settings-needs", left, wide, 1, 1, GAP, vec2(PAD.x, 16.0), 0.0, false, |_, ui| cards::needs(ui, &mut st.ffmpeg));
    ui.add_space(GAP);
    let cols = if wide >= THREE { 2 } else { 1 };
    theme::card_row(ui, "settings-basics", left, wide, cols, 2, GAP, PAD, if cols == 2 { 220.0 } else { 0.0 }, false, |i, ui| match i {
        0 => {
            changed |= cards::lastfm(app, ui);
            Some(("Your listening history makes Recs better. It works without it.".to_string(), FAINT()))
        }
        _ => {
            let mut row = |ui: &mut Ui| {
                let small = |ui: &mut Ui, words: &str, color: Color32| drop(ui.add(Label::new(RichText::new(words).size(13.0).color(color)).wrap()));
                // a blue word that fetches: never broken over two lines, in a narrow card it goes to the next line whole
                let fetch_word = |ui: &mut Ui, words: &str| {
                    let r = ui.add(Label::new(RichText::new(words).size(13.0).color(theme::ACCENT2())).wrap_mode(egui::TextWrapMode::Extend).selectable(false).sense(Sense::click()));
                    script::seen(words, r.rect);
                    if r.hovered() {
                        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(1.0, theme::ACCENT2()));
                    }
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        helpers::start(Helper::Soulseek, ui.ctx());
                    }
                };
                if typed || !has_saved {
                    // a login to save: the card's one loud word, gray until a name and a password are there
                    save = theme::main_action(ui, "Save login and sign in", ready).clicked();
                    if let Some(why) = &trouble {
                        small(ui, why, WARN());
                    }
                    return;
                }
                match &fetch {
                    helpers::State::Getting(done, total) => {
                        small(ui, "Getting the client", DIM());
                        small(ui, &helpers::amount(*done, *total), FAINT());
                        if theme::quiet(ui, "Stop", 13.0, true).clicked() {
                            helpers::stop(Helper::Soulseek);
                        }
                        // how far it is, as a thin line right under the words
                        let room = ui.max_rect();
                        let line = Rect::from_min_size(pos2(room.left(), room.top() + 22.0), vec2(room.width(), 3.0));
                        ui.painter().rect_filled(line, 2.0, theme::BORDER());
                        let mut part = line;
                        part.set_width(line.width() * (*done as f32 / (*total).max(1) as f32).clamp(0.0, 1.0));
                        ui.painter().rect_filled(part, 2.0, theme::ACCENT2());
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    }
                    helpers::State::Unpacking => {
                        small(ui, "Unpacking the client", DIM());
                        ui.add(egui::Spinner::new().size(13.0).color(DIM()));
                    }
                    helpers::State::Failed(why) => {
                        small(ui, why, WARN());
                        fetch_word(ui, "Try again");
                    }
                    helpers::State::Idle if !client => {
                        small(ui, "Not installed", WARN());
                        fetch_word(ui, Helper::Soulseek.word());
                    }
                    helpers::State::Idle => match &stands {
                        Some((words, color)) if *color == GREEN() => {
                            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                            ui.painter().circle_filled(dot.center() + vec2(0.0, 1.0), 3.5, GREEN());
                            small(ui, words.trim_end_matches('.'), DIM());
                        }
                        Some((words, color)) => small(ui, words, *color),
                        None => small(ui, "Signs in when you open Downloads", DIM()),
                    },
                }
            };
            let login = cards::Slsk { user: &mut app.slsk_user, pass: &mut app.slsk_pass, show: &mut app.slsk_show, saved: has_saved, optional: !has_saved };
            cards::soulseek(ui, login, Some(&mut row));
            Some(if typed && !client {
                (format!("The Soulseek client ({} MB) is fetched when you save.", (Helper::Soulseek.pin().size as f64 / 1_000_000.0).round()), FAINT())
            } else if has_saved {
                ("Nothing is shared with other Soulseek users.".to_string(), FAINT())
            } else {
                ("Use an account of its own, not the one you chat with.".to_string(), FAINT())
            })
        }
    });
    if save {
        app.slsk_msg = match slskd::save_login(&app.slsk_user, &app.slsk_pass) {
            Ok(()) => {
                // the password is not kept on the page once it is in Credential Manager
                app.slsk_pass.clear();
                app.slsk_show = false;
                app.path_ok.remove("slsk-login");
                app.attention_cache.0 = -10.0;
                // it signs in from here on; a client that is not on this PC yet is fetched first, as
                // "Start" does on the first-start screen
                if !slskd::exe().exists() {
                    helpers::start(Helper::Soulseek, &ctx);
                }
                app.downloads.get_or_insert_with(|| downloads::DownloadsPanel::new(&ctx));
                None
            }
            Err(e) => Some(Err(e)),
        };
    }
    ui.add_space(14.0);
    theme::centered_wrapped(ui, "Both logins are kept in Windows Credential Manager on this PC, not in a file and not in the backups.", 12.5, FAINT(), wide);
    // what an import does with a lossless album: a card of its own, the full width
    ui.add_space(GAP);
    theme::card_row(ui, "settings-lossless", left, wide, 1, 1, GAP, PAD, 0.0, false, |_, ui| {
        changed |= cards::lossless(ui, &mut app.cfg.keep_lossless);
        None
    });
    changed
}

// ------------------------------------------------------------------ Files

/// What was done in a folder's card.
#[derive(Default)]
struct Did {
    /// another path was typed in place of the words
    typed: bool,
    choose: bool,
    remove: bool,
}

/// A card of rows: its small heading, then what `add` draws, one column wide.
fn rows_card<R>(ui: &mut Ui, left: f32, wide: f32, salt: &str, caption: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    theme::panel(ui, left, wide, vec2(PAD.x, 14.0), salt, |ui| {
        if let Some(c) = caption {
            ui.add_space(2.0);
            theme::caption(ui, c);
            ui.add_space(2.0);
        }
        add(ui)
    })
}

/// A folder as a row: its name with the path under it (a click on the path types one), and at the right
/// the words that act on it. `note` stands in the path's place when there is something wrong with it.
#[allow(clippy::too_many_arguments)]
fn folder_row(ui: &mut Ui, name: &str, label: &str, shown: (&str, Color32), value: &mut String, note: Option<(&str, Color32)>, choose: &str, open: Option<&str>, remove: bool) -> Did {
    let mut did = Did::default();
    let full = ui.available_width();
    const RIGHT: f32 = 190.0;
    ui.allocate_ui_with_layout(vec2(full, 52.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_height(52.0);
        ui.allocate_ui_with_layout(vec2(full - RIGHT - 18.0, 52.0), Layout::top_down(Align::Min), |ui| {
            ui.add_space(7.0);
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add(Label::new(RichText::new(label).size(14.0).color(TEXT())).selectable(false));
            did.typed = cards::typed_path(ui, name, shown.0, value, FontId::proportional(12.5), shown.1, full - RIGHT - 18.0);
            if let Some((words, color)) = note {
                ui.add(Label::new(RichText::new(words).size(12.5).color(color)).wrap());
            }
            ui.add_space(6.0);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if remove {
                did.remove = theme::quiet(ui, "Remove", 13.5, true).clicked();
            }
            if let Some(dir) = open {
                if theme::quiet(ui, "Open", 13.5, true).clicked() {
                    let _ = open::that(dir);
                }
            }
            did.choose = theme::quiet(ui, choose, 13.5, true).clicked();
        });
    });
    did
}

/// What is rarely touched: where downloads arrive, the backup tool, artist names, lyrics.
fn files(app: &mut App, ui: &mut Ui, left: f32, wide: f32, now: f64) -> bool {
    let ctx = ui.ctx().clone();
    let mut changed = false;
    rows_card(ui, left, wide, "settings-folders", Some("FOLDERS"), |ui| {
        let dir = app.cfg.downloads_dir.clone();
        let there = app.path_found("downloads", &dir, now);
        let note = (!there).then_some(("Not there yet. It is made the first time something is downloaded.", FAINT()));
        let did = folder_row(ui, "downloads", "Downloads", (&dir, DIM()), &mut app.cfg.downloads_dir, note, "Change", there.then_some(dir.as_str()), false);
        changed |= did.typed;
        if did.choose {
            app.pick("downloads", &dir, true, &ctx);
        }
        // where the backup script lives (none: no backup tool, and no Backup page)
        let dir = backup::tool_dir_for(&app.cfg.backup_tool_dir).map(|d| d.to_string_lossy().to_string());
        let found = dir.as_ref().is_some_and(|d| app.path_found("backup-tool", d, now));
        let there = dir.as_ref().is_some_and(|d| app.path_found("backup-dir", d, now));
        let shown = match &dir {
            Some(d) => (d.as_str(), DIM()),
            None => ("None chosen", FAINT()),
        };
        let note = (dir.is_some() && !found).then_some(("The backup settings folder was not found.", WARN()));
        let open = dir.as_deref().filter(|_| there);
        let did = folder_row(ui, "backup-tool", "Backup settings", shown, &mut app.cfg.backup_tool_dir, note, if dir.is_some() { "Change" } else { "Choose" }, open, dir.is_some());
        changed |= did.typed;
        if did.remove {
            app.cfg.backup_tool_dir.clear();
            changed = true;
        }
        if did.choose {
            app.pick("backup-tool", dir.as_deref().unwrap_or(""), true, &ctx);
        }
    });
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-names", Some("ARTIST NAMES"), |ui| {
        ui.add_space(10.0);
        ui.add(Label::new(RichText::new("Folders that are not artists").size(14.0).color(TEXT())).selectable(false));
        ui.add_space(6.0);
        let w = ui.available_width();
        changed |= ui.add(TextEdit::multiline(&mut app.cfg.skip_folders).desired_rows(1).desired_width(w).hint_text("Names, with commas between them")).changed();
        ui.add_space(4.0);
        // the list is a text file. The app starts its own the first time an import learns a spelling,
        // so one that is not there yet is no fault
        let file = app.cfg.alias_file.clone();
        let there = app.path_found("alias", &file, now);
        let own = Path::new(file.trim()) == crate::config::app_dir().join("artist-aliases.txt");
        let none = !there && (own || file.trim().is_empty());
        let name = Path::new(file.trim()).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| file.clone());
        let hint = own.then_some("The app adds a spelling when an import teaches it one");
        rows::row(ui, "alias", "Other spellings of the same artist", hint, |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if there && theme::quiet(ui, "Open", 13.5, true).clicked() {
                let _ = open::that(file.trim());
            }
            if theme::quiet(ui, if none { "Choose" } else { "Change" }, 13.5, true).clicked() {
                app.pick("alias", &file, false, &ctx);
            }
            if !there && !none {
                ui.label(RichText::new("Not found").size(13.0).color(WARN()));
            }
            let shown = if none { ("None yet", DIM()) } else { (name.as_str(), TEXT()) };
            changed |= cards::typed_path(ui, "alias", shown.0, &mut app.cfg.alias_file, FontId::proportional(14.0), shown.1, 180.0);
        });
    });
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-lyrics", Some("LYRICS"), |ui| {
        const WAYS: [rows::Item; 2] = [
            rows::Item { name: "Timed when there are any", about: "Lines that follow the song, in players that show them." },
            rows::Item { name: "Plain words only", about: "The words alone, for players that show lyrics as written." },
        ];
        rows::row(ui, "lyrics", "Lyrics to save", Some("From LRCLIB, a free library; only a song's artist, title and length are sent"), |ui| {
            if let Some(n) = rows::pick_one(ui, "pick:lyrics", &WAYS, usize::from(app.cfg.lyrics_plain)) {
                app.cfg.lyrics_plain = n == 1;
                changed = true;
            }
        });
    });
    changed
}

// ------------------------------------------------------------------ Recs

/// A number in a small box: drag it, or click it and type. The same box as a menu's opener (`rows`): a
/// soft field, a hairline edge that brightens under the pointer and while typing, the same height.
fn number<N: egui::emath::Numeric>(ui: &mut Ui, value: &mut N, range: std::ops::RangeInclusive<N>) -> bool {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
            w.weak_bg_fill = rows::box_fill();
            w.bg_fill = rows::box_fill();
            w.bg_stroke = Stroke::new(1.0, rows::box_edge(false, false));
            w.corner_radius = rows::BOX_ROUND.into();
            w.fg_stroke.color = TEXT();
        }
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, rows::box_edge(true, false));
        v.widgets.active.bg_stroke = Stroke::new(1.0, rows::box_edge(false, true));
        v.selection.stroke = Stroke::new(1.0, rows::box_edge(false, true));
        ui.spacing_mut().button_padding = vec2(8.0, 3.0);
        ui.spacing_mut().interact_size = vec2(44.0, rows::BOX_H);
        ui.style_mut().override_font_id = Some(FontId::proportional(13.5));
        ui.add(egui::DragValue::new(value).range(range)).changed()
    })
    .inner
}

/// A list card's small heading, with what the list is for at its right end where there is room.
fn list_head(ui: &mut Ui, name: &str, about: &str) {
    ui.horizontal(|ui| {
        let room = ui.available_width();
        let r = theme::caption(ui, name);
        let words = ui.painter().layout_no_wrap(about.to_string(), FontId::proportional(12.0), FAINT());
        if room - r.rect.width() > words.size().x + 24.0 {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(Label::new(RichText::new(about).size(12.0).color(FAINT())).selectable(false)));
        }
    });
    ui.add_space(8.0);
}

/// One line of a list: its words (cut short when long) and, at the right end, the quiet word that takes
/// it off the list. True when that word was clicked.
fn list_line(ui: &mut Ui, name: &str, more: &str, word: &str) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = theme::quiet(ui, word, 13.0, true).clicked();
            ui.add_space(10.0);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let mut job = egui::text::LayoutJob::default();
                job.append(name, 0.0, egui::TextFormat { font_id: FontId::proportional(14.0), color: TEXT(), ..Default::default() });
                if !more.is_empty() {
                    job.append(more, 8.0, egui::TextFormat { font_id: FontId::proportional(12.5), color: FAINT(), valign: Align::Center, ..Default::default() });
                }
                ui.add(Label::new(job).truncate().selectable(false));
            });
        });
    });
    clicked
}

/// What the Recs page looks for, how it refreshes, and the artists muted and pinned.
fn recs(app: &mut App, ui: &mut Ui, left: f32, wide: f32, now: f64) -> bool {
    let mut changed = false;
    rows_card(ui, left, wide, "settings-new-releases", Some("NEW RELEASES"), |ui| {
        changed |= number_row(ui, "months-back", "How far back", None, None, &mut app.cfg.months_back, 1..=24, "months");
        changed |= number_row(ui, "check-artists", "Artists to check", None, Some("top"), &mut app.cfg.check_artists, 10..=800, "artists");
        // the kinds of release: a menu where any of them can be on
        const KINDS: [rows::Item; 5] = [
            rows::Item { name: "Albums and mixtapes", about: "" },
            rows::Item { name: "EPs", about: "" },
            rows::Item { name: "Singles", about: "" },
            rows::Item { name: "Live albums, compilations, remixes", about: "" },
            rows::Item { name: "Songs they are only featured on", about: "" },
        ];
        let c = &mut app.cfg;
        let mut on = [c.include_albums, c.include_eps, c.include_singles, c.include_live_comp, c.include_features];
        let shown = rows::summary(&["Albums", "EPs", "Singles", "Live and compilations", "Features"], &on, "All kinds");
        rows::row(ui, "kinds", "Kinds", None, |ui| {
            if rows::pick_many(ui, "pick:kinds", &shown, &KINDS, &mut on) {
                [c.include_albums, c.include_eps, c.include_singles, c.include_live_comp, c.include_features] = on;
                changed = true;
            }
        });
        let minutes = ((app.cfg.check_artists as f64 * if app.cfg.use_itunes { 3.1 } else { 1.5 }) / 60.0).ceil();
        let hint = format!("Finds more; a first run takes about {minutes} minutes");
        rows::row(ui, "itunes", "Also check Apple Music", Some(&hint), |ui| changed |= rows::mark(ui, "mark:itunes", &mut app.cfg.use_itunes));
    });
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-artists-to-try", Some("ARTISTS TO TRY"), |ui| {
        changed |= number_row(ui, "seeds", "Start from", None, Some("top"), &mut app.cfg.similar_seeds, 5..=300, "artists");
        changed |= number_row(ui, "played", "Leave out artists you have played", Some("0 keeps them all in; your library's artists are always left out"), None, &mut app.cfg.hide_listened_plays, 0..=10000, "times or more");
    });
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-refreshing", Some("REFRESHING"), |ui| {
        rows::row(ui, "auto-refresh", "Refresh when the app opens", Some("When the last one is over 12 hours old"), |ui| changed |= rows::mark(ui, "mark:auto-refresh", &mut app.cfg.auto_refresh));
        // (for a moment after "Forget them now", the hint says it was done)
        let cleared = app.cache_cleared_at.is_some_and(|t| now - t < 3.0);
        let hint = cleared.then_some("Done. The next refresh looks everything up again");
        rows::row(ui, "cache-days", "Keep lookups for", hint, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if theme::quiet(ui, "Forget them now", 13.5, true).clicked() {
                app.net.clear_cache();
                app.cache_cleared_at = Some(now);
            }
            ui.add_space(8.0);
            rows::unit(ui, "days");
            changed |= number(ui, &mut app.cfg.cache_days, 1..=60);
        });
        let n = app.cfg.dismissed.len();
        rows::row(ui, "hidden", "Hidden releases and artists", None, |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if n > 0 && theme::quiet(ui, "Show them again", 13.5, true).clicked() {
                app.cfg.dismissed.clear();
                changed = true;
            }
            rows::unit(ui, &commas(n));
        });
    });
    ui.add_space(GAP);
    let none = |ui: &mut Ui, words: &str| {
        ui.add_space(8.0);
        ui.add(Label::new(RichText::new(words).size(13.0).color(FAINT())).wrap());
        ui.add_space(6.0);
    };
    rows_card(ui, left, wide, "settings-muted", None, |ui| {
        ui.add_space(2.0);
        list_head(ui, "MUTED ARTISTS", "their new releases are not shown");
        if app.cfg.muted.is_empty() {
            none(ui, "None. Mute an artist from Hide on one of their releases.");
        }
        let mut unmute = None;
        for (k, name) in &app.cfg.muted {
            if list_line(ui, name, "", "Unmute") {
                unmute = Some(k.clone());
            }
        }
        if let Some(k) = unmute {
            app.cfg.muted.remove(&k);
            changed = true;
        }
        ui.add_space(4.0);
    });
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-pinned", None, |ui| {
        ui.add_space(2.0);
        list_head(ui, "PINNED ARTISTS", "matched by hand, where a name is shared");
        if app.cfg.pins.is_empty() {
            none(ui, "None. Open an artist and choose \"Wrong artist?\" to pin the right one.");
        }
        let mut unpin = None;
        for (k, p) in &app.cfg.pins {
            let what = if p.mbid.is_empty() { "not using MusicBrainz".to_string() } else { p.label.clone() };
            if list_line(ui, &p.artist, &format!("\u{2192}  {what}"), "Unpin") {
                unpin = Some(k.clone());
            }
        }
        if let Some(k) = unpin {
            app.cfg.pins.remove(&k);
            app.detail_cache.remove(&k);
            changed = true;
        }
        ui.add_space(4.0);
    });
    changed
}

/// A row with a number in it: `label` at the left, and at the right "[150] artists" (with `before` in
/// front of the box when there is one: "top [150] artists"). True when the number was changed.
#[allow(clippy::too_many_arguments)]
fn number_row<N: egui::emath::Numeric>(ui: &mut Ui, id: &str, label: &str, hint: Option<&str>, before: Option<&str>, value: &mut N, range: std::ops::RangeInclusive<N>, after: &str) -> bool {
    rows::row(ui, id, label, hint, |ui| {
        rows::unit(ui, after);
        let changed = number(ui, value, range);
        if let Some(b) = before {
            rows::unit(ui, b);
        }
        changed
    })
}

fn commas(n: usize) -> String {
    crate::library_page::commas(n as u64)
}

// ------------------------------------------------------------------ Look

/// The themes and the size, a card each.
fn look(app: &mut App, st: &mut State, ui: &mut Ui, left: f32, wide: f32, now: f64) -> bool {
    let mut changed = false;
    theme::panel(ui, left, wide, PAD, "settings-theme", |ui| changed = themes(app, st, ui, now));
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-size", None, sizes);
    changed
}

/// One theme as a small picture of a page in its colors (the top row, a card, two lines of text, the
/// accent and a link), with its name under it. The whole tile is the control; the one in use has the
/// accent line under its name. True when it was clicked.
fn theme_tile(ui: &mut Ui, name: &str, colors: &[u32; theme::ROLES], on: bool, s: f32) -> bool {
    // (drawn at 142 by 88, or `s` times that so a row of them fits)
    let size = egui::Vec2::new(142.0, 88.0) * s;
    let (rect, r) = ui.allocate_exact_size(size + vec2(0.0, 28.0), Sense::click());
    script::seen(&format!("theme:{name}"), rect);
    let color = |role: usize| {
        let c = colors[role];
        Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    };
    let picture = Rect::from_min_size(rect.min, size);
    let p = ui.painter();
    p.rect_filled(picture, 8.0, color(0));
    p.rect_filled(Rect::from_min_size(picture.min, vec2(size.x, 12.0 * s)), egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, color(1));
    p.rect_filled(Rect::from_min_size(picture.min + vec2(10.0, 22.0) * s, vec2(54.0, 54.0) * s), 5.0, color(2));
    for (y, w, h, role) in [(26.0, 52.0, 5.0, 5), (38.0, 36.0, 4.0, 6), (56.0, 26.0, 5.0, 8), (68.0, 44.0, 3.0, 9)] {
        p.rect_filled(Rect::from_min_size(picture.min + vec2(74.0, y) * s, vec2(w, h) * s), 2.0, color(role));
    }
    // the one in use: a coral edge round it (the one mark of what is chosen, as everywhere in Settings)
    if on {
        p.rect_stroke(picture.expand(3.0), 10.0, Stroke::new(1.5, ACCENT()), egui::StrokeKind::Inside);
    }
    p.rect_stroke(picture, 8.0, Stroke::new(1.0, if r.hovered() && !on { TEXT() } else { theme::BORDER() }), egui::StrokeKind::Inside);
    let words = p.layout_no_wrap(name.to_string(), FontId::proportional(13.0), Color32::PLACEHOLDER);
    let at = pos2(rect.left(), picture.bottom() + 6.0);
    p.galley(at, words, if on { TEXT() } else { DIM() });
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// One color by itself: the swatch opens a picker, the box takes a color written as #rrggbb.
fn color_line(ui: &mut Ui, n: usize, changed: &mut bool) {
    let (key, words) = theme::ROLE_NAMES[n];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let shown = theme::color(n);
        let mut c = [shown.r(), shown.g(), shown.b()];
        let r = ui.color_edit_button_srgb(&mut c);
        script::seen(&format!("color:{key}"), r.rect);
        if r.changed() {
            theme::set(n, Color32::from_rgb(c[0], c[1], c[2]));
            *changed = true;
        }
        // the box shows the color in use until something is typed in it; a whole color is taken up at once
        let id = egui::Id::unique(("color-hex", n));
        let mut typed: String = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| theme::to_hex(theme::color(n)));
        let box_r = ui.add(TextEdit::singleline(&mut typed).desired_width(74.0).font(FontId::monospace(12.5)));
        script::seen(&format!("hex:{key}"), box_r.rect);
        if box_r.changed() {
            if let Some(c) = theme::from_hex(&typed) {
                theme::set(n, c);
                *changed = true;
            }
        }
        if box_r.has_focus() {
            ui.data_mut(|d| d.insert_temp(id, typed));
        } else {
            ui.data_mut(|d| d.remove::<String>(id));
        }
        ui.add(Label::new(RichText::new(words).size(13.5).color(TEXT())).truncate().selectable(false));
    });
}

/// The themes as pictures of their colors, your own saved theme among them, a way back from the last
/// change and, folded away until asked for, each color by itself. In the page its heading has "Try it on
/// the pages"; beside the pages it has the X that brings Settings back.
fn themes(app: &mut App, st: &mut State, ui: &mut Ui, now: f64) -> bool {
    let mut changed = false;
    let before = theme::snapshot();
    ui.horizontal(|ui| {
        theme::caption(ui, "THEME");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if st.beside {
                if theme::close_mark(ui, "Back to Settings", "Back to Settings").clicked() {
                    st.beside = false;
                }
            } else if theme::quiet(ui, "Try it on the pages", 13.0, true).clicked() {
                st.beside = true;
            }
        });
    });
    ui.add_space(12.0);
    let mine = theme::from_saved(&app.cfg.my_theme);
    ui.horizontal_wrapped(|ui| {
        let gap = if st.beside { 14.0 } else { 18.0 };
        ui.spacing_mut().item_spacing = vec2(gap, 8.0);
        // as many to a row as fit at 104 wide or more, then as wide as that row allows (142 at most)
        let n = theme::THEMES.len() + usize::from(mine.is_some());
        let room = ui.available_width();
        let per_row = (((room + gap) / (104.0 + gap)).floor() as usize).clamp(1, n);
        let s = (((room - gap * (per_row - 1) as f32) / per_row as f32 - 1.0) / 142.0).min(1.0);
        for (name, colors) in theme::THEMES.iter().map(|t| (t.0, t.1)).chain(mine.map(|m| ("Mine", m))) {
            let on = colors == before;
            if theme_tile(ui, name, &colors, on, s) && !on {
                theme::set_all(&colors);
                changed = true;
            }
        }
    });
    ui.add_space(2.0);
    let mut undone = false;
    // each color by itself, folded under one row until asked for
    rows::row(ui, "single-colors", "Single colors", None, |ui| {
        if theme::quiet(ui, if app.colors_open { "Hide them" } else { "Show them" }, 13.5, true).clicked() {
            app.colors_open = !app.colors_open;
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(18.0, 4.0);
        if !app.color_undo.is_empty() && theme::quiet(ui, "Undo the last change", 13.5, true).clicked() {
            if let Some(was) = app.color_undo.pop() {
                theme::set_all(&was);
                (changed, undone) = (true, true);
            }
        }
        // colors that are no theme's can be kept as one
        if theme::current_theme().is_none() && mine != Some(before) {
            ui.label(RichText::new("Your own colors").size(13.5).color(TEXT()));
            if theme::quiet(ui, "Save as my theme", 13.5, true).clicked() {
                app.cfg.my_theme = theme::as_saved(&before);
                app.cfg.save();
                app.saved_at = Some(now);
            }
        }
    });
    if app.colors_open {
        ui.add_space(10.0);
        // each color by itself, in as many columns as there is room for
        let room = ui.available_width();
        let cols = if room >= 780.0 {
            3
        } else if room >= 520.0 {
            2
        } else {
            1
        };
        let each = theme::ROLES.div_ceil(cols);
        ui.columns(cols, |col| {
            for n in 0..theme::ROLES {
                color_line(&mut col[n / each], n, &mut changed);
            }
        });
    }
    if changed {
        // what the colors were before this change can be had back; a drag in the picker is one change
        if !undone && theme::snapshot() != before && now - app.color_changed_at > 0.8 {
            app.color_undo.push(before);
            if app.color_undo.len() > 40 {
                app.color_undo.remove(0);
            }
        }
        if !undone {
            app.color_changed_at = now;
        }
        app.cfg.colors = theme::saved();
    }
    changed
}

/// How large everything is drawn: a row with a menu of the sizes.
fn sizes(ui: &mut Ui) {
    let at = ui.ctx().zoom_factor();
    let names: Vec<String> = theme::SIZES.iter().map(|s| format!("{}%", (s * 100.0).round() as u32)).collect();
    let items: Vec<rows::Item> = names.iter().map(|n| rows::Item { name: n, about: "" }).collect();
    let chosen = theme::SIZES.iter().position(|s| (at - s).abs() < 0.01).unwrap_or(1);
    let mut pick = None;
    rows::row(ui, "size", "Size", None, |ui| {
        if let Some(n) = rows::pick_one(ui, "pick:size", &items, chosen) {
            pick = theme::SIZES.get(n).copied();
        }
    });
    if let Some(size) = pick {
        // taken up after this frame is drawn; the app then keeps it in the settings file
        ui.ctx().set_zoom_factor(size);
    }
}

// ------------------------------------------------------------------ About

/// The version and new versions of the app, one card, and under it the quiet ways to its author.
/// The card's second line is the only thing that changes with what the updater is doing (`update::State`);
/// a fetch fills the thin line that always has its room, so nothing moves. The one loud word is there
/// only when there is something to do.
fn about(app: &mut App, ui: &mut Ui, left: f32, wide: f32) -> bool {
    use crate::update::{self, Policy, State as Up, Why};
    let ctx = ui.ctx().clone();
    let state = update::state();
    let can = update::available();
    let chosen = Policy::from_saved(&app.cfg.updates);
    let busy = app.busy();
    let mut changed = false;
    theme::panel(ui, left, wide, vec2(PAD.x, 26.0), "settings-version", |ui| {
        // the name and version: the page's one large line, in the middle
        theme::centered(ui, &format!("Bradley's Music Tool {}", update::current()), theme::display(30.0), TEXT());
        ui.add_space(8.0);
        let say = |ui: &mut Ui, text: &str, color: Color32| {
            ui.add(Label::new(RichText::new(text).size(13.5).color(color)).selectable(false));
        };
        let faint = |ui: &mut Ui, text: &str| {
            ui.add(Label::new(RichText::new(text).size(12.5).color(FAINT())).selectable(false));
        };
        // under it, one line: how it stands, then the words that act on it (they act on what is said)
        ui.horizontal(|ui| {
            ui.set_min_height(21.0);
            theme::centered_row(ui, "about-state", 0.0, |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            match &state {
                _ if !can => say(ui, "This copy is a test run or a build from source, and does not update itself.", DIM()),
                Up::Idle if chosen == Policy::Off => say(ui, "Not checking for new versions.", DIM()),
                Up::Idle => faint(ui, "Not checked yet"),
                Up::Checking => say(ui, "Checking\u{2026}", DIM()),
                Up::UpToDate(at) => {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    let (dot, _) = ui.allocate_exact_size(vec2(7.0, 7.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 3.5, GREEN());
                    say(ui, "Up to date", DIM());
                    ui.add_space(9.0);
                    faint(ui, &format!("checked {}", crate::recs_top::when_words(*at, chrono::Local::now())));
                }
                Up::Fetching(v, done, total) => {
                    say(ui, &format!("Fetching version {v}"), DIM());
                    faint(ui, &helpers::amount(*done, *total));
                }
                Up::Ready(v) => {
                    if theme::main_action(ui, "Restart to update", !busy).clicked() {
                        update::ask_restart();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    let rest = if busy { "It goes in the next time you open the app, or here once what is running is done." } else { "It also goes in the next time you open the app." };
                    say(ui, &format!("Version {v} is ready. {rest}"), DIM());
                }
                Up::Out(v, why) => {
                    match why {
                        Why::TellOnly => say(ui, &format!("Version {v} is out."), DIM()),
                        Why::CannotWrite => say(ui, &format!("Version {v} is out, but the app cannot write to its own folder."), WARN()),
                        Why::Stopped => say(ui, &format!("Version {v} is out. Fetching it was stopped."), DIM()),
                    }
                    if *why != Why::Stopped && theme::main_action(ui, "Get it", true).clicked() {
                        crate::util::open_url(&format!("{}/releases/latest", update::REPO));
                    }
                }
                Up::Failed => {
                    say(ui, "The last check did not get through.", DIM());
                    faint(ui, "It tries again in 30 minutes.");
                }
            }
            // the word for the updater, then what is new
            match &state {
                Up::Fetching(..) => {
                    if theme::quiet(ui, "Stop", 13.5, true).clicked() {
                        update::stop_fetch();
                    }
                }
                Up::Ready(_) | Up::Checking => {}
                Up::Out(_, Why::Stopped) => {
                    // (the part that arrived is still there: the fetch carries on from it)
                    if theme::quiet(ui, "Carry on", 13.5, true).clicked() {
                        update::check_now();
                    }
                }
                _ if can => {
                    if theme::quiet(ui, "Check now", 13.5, true).clicked() {
                        update::check_now();
                    }
                }
                _ => {}
            }
            let (words, page) = match &state {
                Up::Fetching(v, ..) | Up::Ready(v) | Up::Out(v, _) => (format!("What's new in {v}"), format!("{}/releases/tag/v{v}", update::REPO)),
                _ => ("What's new".to_string(), format!("{}/releases", update::REPO)),
            };
            if theme::quiet(ui, &words, 13.5, true).clicked() {
                crate::util::open_url(&page);
            }
            });
        });
        // the thin line: always has its room, filled only while a version is being fetched
        ui.add_space(10.0);
        let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 3.0), Sense::hover());
        if let Up::Fetching(_, done, total) = &state {
            let line = Rect::from_center_size(line.center(), vec2(line.width().min(360.0), 3.0));
            ui.painter().rect_filled(line, 1.5, theme::BORDER());
            let mut filled = line;
            filled.set_width(line.width() * (*done as f32 / (*total).max(1) as f32).clamp(0.0, 1.0));
            ui.painter().rect_filled(filled, 1.5, theme::ACCENT2());
        }
    });
    ui.add_space(GAP);
    rows_card(ui, left, wide, "settings-new-versions", None, |ui| {
        const WAYS: [rows::Item; 3] = [
            rows::Item { name: "Install by themselves", about: "Fetched and checked in the background; put in when you restart." },
            rows::Item { name: "Only tell me", about: "Says a new version is out; you choose when to get it." },
            rows::Item { name: "Never check", about: "The app does not look for new versions." },
        ];
        let policies = [Policy::Auto, Policy::Tell, Policy::Off];
        let at = policies.iter().position(|p| *p == chosen).unwrap_or(0);
        rows::row(ui, "new-versions", "New versions", None, |ui| {
            if let Some(n) = rows::pick_one(ui, "pick:updates", &WAYS, at) {
                app.cfg.updates = policies[n].saved().to_string();
                changed = true;
            }
        });
    });
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        theme::centered_row(ui, "about-ways", 0.0, |ui| {
            ui.spacing_mut().item_spacing.x = 22.0;
            let ways = [
                ("Support the app on Ko-fi", update::KOFI.to_string()),
                ("Cadence, the music player", update::CADENCE.to_string()),
                ("Licenses", format!("{}/blob/main/THIRD-PARTY-NOTICES.txt", update::REPO)),
                ("Report a problem", format!("{}/issues", update::REPO)),
            ];
            for (words, page) in ways {
                if theme::quiet(ui, words, 12.5, true).clicked() {
                    crate::util::open_url(&page);
                }
            }
        });
    });
    changed
}

/// What in Settings needs a look, each with the section it is in: only something that was set up and
/// has stopped working, never something that was left out. `music` is whether the music folder is there
/// (None: none is chosen), `soulseek` whether a login is saved and whether its client is on this PC or
/// on its way, `backup` whether the backup tool's script is in the folder that is set (None: none is).
pub(crate) fn needs_a_look(music: Option<bool>, soulseek: (bool, bool), backup: Option<bool>) -> Vec<(String, usize)> {
    let mut out = vec![];
    match music {
        None => out.push(("No music folder is chosen".to_string(), BASICS)),
        Some(false) => out.push(("The music folder was not found".to_string(), BASICS)),
        Some(true) => {}
    }
    if soulseek == (true, false) {
        out.push(("A Soulseek login is saved, but its client is not installed".to_string(), BASICS));
    }
    if backup == Some(false) {
        out.push(("The backup settings folder was not found".to_string(), FILES));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_was_set_up_and_stopped_working_needs_a_look() {
        // someone who skipped everything optional: a music folder that is there, no login, no backup tool
        assert!(needs_a_look(Some(true), (false, false), None).is_empty());
        // (no Soulseek client without a login is not a fault, and neither is a client without a login)
        assert!(needs_a_look(Some(true), (false, true), None).is_empty());
        // all set up and working
        assert!(needs_a_look(Some(true), (true, true), Some(true)).is_empty());
        // the drive is unplugged
        assert_eq!(needs_a_look(Some(false), (false, false), None), vec![("The music folder was not found".to_string(), BASICS)]);
        // the app cannot work without a music folder at all
        assert_eq!(needs_a_look(None, (false, false), None)[0].1, BASICS);
        // a login that has no client to sign in with, and a backup tool whose script is gone
        let both = needs_a_look(Some(true), (true, false), Some(false));
        assert_eq!(both.len(), 2);
        assert_eq!((both[0].1, both[1].1), (BASICS, FILES));
        assert!(SECTIONS[both[1].1] == "Files" && SECTIONS[both[0].1] == "Basics");
    }
}
