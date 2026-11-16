//! The first start: one screen in place of the pages, on a PC that has never run the app (no settings
//! file; see `Config::load`). It asks one thing, where the music is. Under that, three cards that may all
//! be left alone: what the app needs (ffmpeg, PowerShell 7), Last.fm, and Soulseek with the choice of what
//! happens to lossless downloads. They are faint until a folder is chosen, so the page has one thing to
//! do; "Start" writes the settings and opens Home. Everything here can be changed later in Settings,
//! whose first section is these same four cards (`cards`).
use crate::cards::{self, FolderCard, GAP, PAD};
use crate::helpers::{self, Helper};
use crate::theme::{self, FAINT, TEXT, WARN};
use crate::{script, slskd, App, Page};
use egui::{vec2, FontId, Label, Rect, RichText, ScrollArea, Sense, Ui};

/// The page's one stretch of the window.
const PAGE: f32 = 1000.0;
/// The three cards under the folder, side by side, are never lower than this.
const CARD_H: f32 = 252.0;

pub(crate) struct Setup {
    /// the card that asks for the music folder and says what is in it
    folder: FolderCard,
    /// "Look only, change nothing", for a library laid out another way (it starts chosen)
    look_only: bool,
    /// lossless downloads stay lossless; for someone new they become MP3 320 (a third of the room, and they
    /// play on anything), the choice shown first on the card
    keep_lossless: bool,
    slsk_user: String,
    slsk_pass: String,
    slsk_show: bool,
    slsk_trouble: Option<String>,
    /// whether ffmpeg is on this PC, and when that was last looked at
    ffmpeg: (f64, bool),
}

impl Setup {
    pub(crate) fn new() -> Self {
        // (a test run can start with a made-up Soulseek login typed in, since a script cannot type:
        // `MUSIC_RECS_SETUP_LOGIN=name:password`)
        let (slsk_user, slsk_pass) = cards::test_switch("MUSIC_RECS_SETUP_LOGIN").and_then(|l| l.split_once(':').map(|(n, p)| (n.to_string(), p.to_string()))).unwrap_or_default();
        Self { folder: FolderCard::default(), look_only: true, keep_lossless: false, slsk_user, slsk_pass, slsk_show: false, slsk_trouble: None, ffmpeg: (-10.0, false) }
    }

    /// A Soulseek login was typed in: both parts are there.
    fn has_login(&self) -> bool {
        !self.slsk_user.trim().is_empty() && !self.slsk_pass.is_empty()
    }

    /// "Start": the choices go into the settings, a Soulseek login is saved (and its client fetched when
    /// it is not on this PC), and the pages take the window. False when the login could not be saved: the
    /// screen stays, and its card says why.
    fn finish(&mut self, app: &mut App, ctx: &egui::Context) -> bool {
        if self.has_login() {
            if let Err(why) = slskd::save_login(&self.slsk_user, &self.slsk_pass) {
                self.slsk_trouble = Some(why);
                return false;
            }
            self.slsk_pass.clear();
            if !slskd::exe().exists() {
                helpers::start(Helper::Soulseek, ctx);
            }
        }
        self.folder.stop();
        app.cfg.keep_lossless = self.keep_lossless;
        app.cfg.look_only = self.folder.found().is_some_and(|f| f.odd()) && self.look_only;
        app.cfg.setup_done = true;
        app.cfg.last_page = "home".to_string();
        app.cfg.save();
        app.page = Page::Home;
        true
    }
}

/// The first-start screen, in place of the pages. Leaves `app.setup` empty once "Start" has been clicked.
pub(crate) fn screen(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let Some(mut s) = app.setup.take() else { return };
    if s.folder.tick(app, &ctx).another {
        // another folder: the choice about repairs starts over
        s.look_only = true;
    }
    let mut start = false;
    egui::CentralPanel::default().frame(theme::page_frame()).show(ui, |ui| {
        ScrollArea::vertical().id_salt("setup-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let (left, wide) = theme::column(ui, PAGE);
            ui.add_space(14.0);
            // the app's mark over the question
            let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 72.0), Sense::hover());
            crate::icon::paint_mark(ui, Rect::from_center_size(row.center(), vec2(72.0, 72.0)), 1.0);
            ui.add_space(10.0);
            theme::centered(ui, "Where is your music?", theme::display(34.0), TEXT());
            ui.add_space(2.0);
            theme::centered_wrapped(ui, "One folder is all the app needs. The rest can wait, and all of it can be changed later in Settings.", 12.5, FAINT(), wide);
            ui.add_space(24.0);
            s.folder.show(app, ui, left, wide, cards::Repairs::First(&mut s.look_only));
            ui.add_space(GAP);

            // the three cards that may be left alone: in a row, or one under the other in a narrow window.
            // Faint, and nothing in them takes a click, until there is a folder
            let ready = s.folder.ready();
            let cols = if wide >= 820.0 { 3 } else { 1 };
            // (one under the other, each is only as tall as what it holds)
            theme::card_row(ui, "setup-card", left, wide, cols, 3, GAP, PAD, if cols == 3 { CARD_H } else { 0.0 }, !ready, |i, ui| match i {
                0 => cards::needs(ui, &mut s.ffmpeg),
                1 => {
                    cards::lastfm(app, ui);
                    Some(("Your listening history makes Recs better. It works without it.".to_string(), FAINT()))
                }
                _ => {
                    let login = cards::Slsk { user: &mut s.slsk_user, pass: &mut s.slsk_pass, show: &mut s.slsk_show, saved: false, optional: true };
                    cards::soulseek(ui, login, None);
                    let fetched = format!("The Soulseek client ({} MB) is fetched when you start.", (Helper::Soulseek.pin().size as f64 / 1_000_000.0).round());
                    match &s.slsk_trouble {
                        Some(why) => Some((why.clone(), WARN())),
                        None if s.has_login() && !slskd::exe().exists() => Some((fetched, FAINT())),
                        None => Some(("Use an account of its own, not the one you chat with.".to_string(), FAINT())),
                    }
                }
            });

            // what an import does with a lossless album: a card of its own, the full width, faint like the three
            ui.add_space(GAP);
            theme::card_row(ui, "setup-lossless", left, wide, 1, 1, GAP, PAD, 0.0, !ready, |_, ui| {
                cards::lossless(ui, &mut s.keep_lossless);
                None
            });

            // the one loud word, once there is a folder
            ui.add_space(26.0);
            ui.horizontal(|ui| {
                theme::centered_row(ui, "setup-start", 0.0, |ui| {
                    if ready {
                        start = theme::main_action_sized(ui, "Start", 24.0, true).clicked();
                    } else {
                        let r = ui.add(Label::new(RichText::new("Start").size(24.0).strong().color(theme::BORDER())).selectable(false));
                        script::seen("Start (not yet)", r.rect);
                    }
                });
            });
            ui.add_space(2.0);
            theme::centered(ui, if ready { "Opens Home. Everything here is in Settings afterwards." } else { "Choose a music folder first" }, FontId::proportional(12.5), FAINT());
            ui.add_space(24.0);
        });
    });
    if !(start && s.finish(app, &ctx)) {
        app.setup = Some(s);
    }
}
