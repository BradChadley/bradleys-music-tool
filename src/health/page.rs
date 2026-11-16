//! How the Health page is laid out.
//!
//! Top to bottom, in one centered stretch of the window:
//!   - one large sentence that says how the library is, and under it what the last check covered;
//!   - one card for the last check: the numbers, the bar they add up to, and the words that start a
//!     check. The thin line over those words fills in blue while a check or a "Fix all" runs, so nothing
//!     on the page moves when one starts or ends;
//!   - the categories and the findings as two cards side by side, and the album that is open as a third
//!     at the right. In a narrow window the open album takes the categories' place, and in the narrowest
//!     the list's too.
//!
//! Everything here only places the parts and starts what a click asks for; the list, the wall of covers
//! and an album's own card are drawn in `health.rs`.
use super::*;
use egui::text::TextFormat;
use egui::UiBuilder;

/// The page at its widest (the Downloads page's width: a list with a card beside it).
const PAGE: f32 = 1400.0;
const GAP: f32 = 16.0;
/// The least the cards under the summary are tall: a window shorter than that scrolls the page.
const BODY_MIN: f32 = 440.0;
const CATS_W: f32 = 256.0;
const DETAILS_W: f32 = 396.0;
/// Under this width the open album takes the categories' place; under `TINY`, the list's as well.
const NARROW: f32 = 1040.0;
const TINY: f32 = 700.0;
/// The room inside a card's edges.
const PAD: egui::Vec2 = vec2(20.0, 18.0);

impl HealthPage {
    pub(super) fn page(&mut self, ui: &mut Ui, cfg: &Config) {
        let ctx = ui.ctx().clone();
        self.set_library(&cfg.music_dir, &ctx);
        if self.dirty {
            self.rebuild();
            debug_select(self);
        }
        self.keys(&ctx);
        if self.dirty {
            self.rebuild();
        }
        egui::CentralPanel::default().frame(Frame::new().fill(BG())).show(ui, |ui| {
            // the cards reach down to the end of the window; in a window too short for them they keep a
            // least height and the page scrolls
            let tall = ui.available_height();
            ScrollArea::vertical().id_salt("health-page").auto_shrink([false, false]).show(ui, |ui| {
                let floor = ui.max_rect().top() + tall;
                let (left, wide) = theme::column(ui, PAGE);
                ui.add_space(26.0);
                self.sentence(ui, &ctx, cfg, left, wide);
                if self.state == State::Ready {
                    ui.add_space(22.0);
                    self.summary(ui, &ctx, cfg, left, wide);
                } else if !self.history_open {
                    return;
                }
                ui.add_space(GAP - ui.spacing().item_spacing.y);
                self.body(ui, &ctx, cfg, left, wide, floor);
            });
        });
    }

    // ------------------------------------------------------------------ the sentence at the top

    /// How the library is, in one large sentence, and its color.
    pub(super) fn state_words(&self) -> (String, Color32) {
        match self.state {
            State::Loading => ("Reading the last check".to_string(), DIM()),
            State::Unreadable => ("The last check could not be read".to_string(), WARN()),
            State::Missing => ("Your library has not been checked yet".to_string(), TEXT()),
            State::Ready => {
                let v = &self.view;
                let words = match (v.serious_albums, v.total) {
                    (1, _) => "1 album needs attention".to_string(),
                    (n, _) if n > 0 => format!("{} albums need attention", num(n as u64)),
                    (_, 0) => "Your library is in good shape".to_string(),
                    _ => "Nothing serious in your library".to_string(),
                };
                (words, TEXT())
            }
        }
    }

    fn sentence(&mut self, ui: &mut Ui, ctx: &egui::Context, cfg: &Config, left: f32, wide: f32) {
        let mut column = ui.available_rect_before_wrap();
        (column.min.x, column.max.x) = (left, left + wide);
        ui.scope_builder(UiBuilder::new().max_rect(column).layout(Layout::top_down(Align::Center)), |ui| {
            let (words, color) = self.state_words();
            ui.add(Label::new(RichText::new(words).font(theme::display(34.0)).color(color)).wrap());
            ui.add_space(2.0);
            let (facts, more) = match self.state {
                State::Ready => self.facts(),
                State::Unreadable => ("Run a new check to replace it.".to_string(), String::new()),
                State::Missing => (self.library.clone(), String::new()),
                State::Loading => (String::new(), String::new()),
            };
            if !facts.is_empty() {
                ui.add(Label::new(RichText::new(facts).size(14.0).color(DIM())).wrap());
            }
            if !more.is_empty() {
                ui.add(Label::new(RichText::new(more).size(12.5).color(FAINT())).wrap());
            }
            if self.state == State::Ready {
                return;
            }
            // no card for a check yet: the words that start one stand under the sentence
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.set_min_height(24.0);
                theme::centered_row(ui, "health-first-check", 0.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 24.0;
                    self.check_words(ui, ctx, cfg);
                    // (left to right here, so the other way round than in the card)
                    self.history_word(ui, 14.0);
                    self.reload_word(ui, ctx, 14.0);
                });
            });
            if self.note.is_some() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.set_min_height(22.0);
                    theme::centered_row(ui, "health-first-note", 0.0, |ui| self.note_words(ui, ctx, false));
                });
            }
        });
    }

    // ------------------------------------------------------------------ the last check's card

    fn summary(&mut self, ui: &mut Ui, ctx: &egui::Context, cfg: &Config, left: f32, wide: f32) {
        theme::panel(ui, left, wide, vec2(24.0, 20.0), "health-summary", |ui| {
            ui.horizontal(|ui| {
                theme::caption(ui, "LAST CHECK");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 22.0;
                    self.reload_word(ui, ctx, 13.0);
                    self.history_word(ui, 13.0);
                });
            });
            ui.add_space(12.0);
            self.numbers(ui);
            // the thin line doubles as progress: how far a check or a "Fix all" is
            ui.add_space(16.0 - ui.spacing().item_spacing.y);
            let (seam, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
            ui.painter().hline(seam.x_range(), seam.center().y, Stroke::new(1.0, BORDER()));
            if let Some(done) = self.progress() {
                ui.painter().hline(seam.left()..=seam.left() + seam.width() * done, seam.center().y, Stroke::new(2.0, theme::ACCENT2()));
            }
            ui.add_space(12.0 - ui.spacing().item_spacing.y);
            ui.horizontal(|ui| {
                ui.set_min_height(24.0);
                ui.spacing_mut().item_spacing.x = 24.0;
                self.check_words(ui, ctx, cfg);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.note_words(ui, ctx, true));
            });
        });
    }

    /// How far what is running is, 0 to 1. None while nothing runs, or while there is no count yet.
    fn progress(&self) -> Option<f32> {
        use std::sync::atomic::Ordering;
        if let Some(c) = &self.check {
            let (done, total) = (c.done.load(Ordering::Relaxed), c.total.load(Ordering::Relaxed));
            return (total > 0).then(|| (done.min(total) as f32 / total as f32).clamp(0.0, 1.0));
        }
        self.bulk.as_ref().map(|b| (b.0.load(Ordering::Relaxed).min(b.1) as f32 / b.1.max(1) as f32).clamp(0.0, 1.0))
    }

    /// The six numbers across the card, and the bar the three severities add up to. The first four choose
    /// what the list shows: a click on a number or on its part of the bar.
    fn numbers(&mut self, ui: &mut Ui) {
        let v = &self.view;
        // the severity being looked at (chosen on the left, here, or through one of its categories)
        let active = match &self.filter {
            Filter::All => None,
            Filter::Sev(s) | Filter::Cat(s, _) => Some(*s),
        };
        let was_on = self.hover_sev;
        let (mut on, mut pick) = (None, None);
        let plural = |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
        // (value, the word under it, its color, what a click chooses: 0 everything, 1 to 3 a severity)
        let cells: [(usize, &str, Color32, Option<u8>); 6] = [
            (v.total, plural(v.total, "open issue", "open issues"), TEXT(), Some(0)),
            (v.by_sev[0], "serious", if v.by_sev[0] == 0 { DIM() } else { RED() }, Some(1)),
            (v.by_sev[1], "worth fixing", if v.by_sev[1] == 0 { DIM() } else { WARN() }, Some(2)),
            (v.by_sev[2], "minor", DIM(), Some(3)),
            (v.albums, plural(v.albums, "album affected", "albums affected"), TEXT(), None),
            (v.better_copy, plural(v.better_copy, "needs a better copy", "need a better copy"), TEXT(), None),
        ];
        let full = ui.available_width();
        // six across; three over three in a card too narrow for that
        let (across, size) = if full >= 640.0 { (6, 34.0) } else { (3, 28.0) };
        let (cell_w, cell_h) = (full / across as f32, size + 26.0);
        let rows = cells.len().div_ceil(across);
        let (area, _) = ui.allocate_exact_size(vec2(full, rows as f32 * cell_h + (rows as f32 - 1.0) * 10.0), Sense::hover());
        for (n, (value, label, color, kind)) in cells.into_iter().enumerate() {
            let min = pos2(area.left() + (n % across) as f32 * cell_w, area.top() + (n / across) as f32 * (cell_h + 10.0));
            let cx = (min.x + cell_w / 2.0).round();
            let sev = kind.filter(|k| *k > 0);
            let chosen = sev.is_some() && active == sev;
            let number = ui.painter().layout_no_wrap(num(value as u64), theme::display(size), color);
            let word = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(12.0), DIM());
            let reach = number.size().x.max(word.size().x) + 20.0;
            let hit = Rect::from_min_size(pos2(cx - reach / 2.0, min.y), vec2(reach, cell_h));
            let hot = match kind {
                Some(k) => {
                    let r = ui.interact(hit, ui.scope_id().with(("health-number", n)), Sense::click());
                    if r.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        on = on.or(sev);
                    }
                    if r.clicked() && (k == 0 || value > 0) {
                        // clicking the severity already chosen goes back to everything
                        pick = Some(if k == 0 || self.filter == Filter::Sev(k) { Filter::All } else { Filter::Sev(k) });
                    }
                    r.hovered() || (sev.is_some() && was_on == sev)
                }
                None => false,
            };
            // the other severities step back while one is being looked at
            let faded = sev.is_some() && active.is_some() && !chosen && !hot;
            let tone = |c: Color32| if faded { c.gamma_multiply(0.5) } else { c };
            let nw = number.size().x;
            ui.painter().galley_with_override_text_color(pos2((cx - nw / 2.0).round(), min.y), number, tone(color));
            let (ww, wh) = (word.size().x, word.size().y);
            let at = pos2((cx - ww / 2.0).round(), min.y + size + 8.0);
            ui.painter().galley_with_override_text_color(at, word, if chosen || hot { TEXT() } else { DIM() });
            if chosen {
                ui.painter().hline(at.x..=at.x + ww, at.y + wh + 1.0, Stroke::new(1.5, ACCENT()));
            } else if hot {
                ui.painter().hline(at.x..=at.x + ww, at.y + wh + 1.0, Stroke::new(1.0, FAINT()));
            }
        }
        if across == 6 {
            // a line between the numbers that choose what the list shows and the two that only say something
            let x = (area.left() + 4.0 * cell_w).round();
            ui.painter().vline(x, area.top() + 8.0..=area.bottom() - 8.0, Stroke::new(1.0, BORDER()));
        }
        ui.add_space(14.0 - ui.spacing().item_spacing.y);
        if v.total > 0 {
            let (hovered, clicked) = sev_bar(ui, v.by_sev, active, was_on);
            on = on.or(hovered);
            if let Some(s) = clicked {
                pick = Some(if self.filter == Filter::Sev(s) { Filter::All } else { Filter::Sev(s) });
            }
        } else {
            // nothing to split: a plain line of the same height, so the card is as tall either way
            let (rect, _) = ui.allocate_exact_size(vec2(full, 12.0), Sense::hover());
            ui.painter().rect_filled(Rect::from_center_size(rect.center(), vec2(full, 5.0)), 3, BORDER());
        }
        self.hover_sev = on;
        if let Some(f) = pick {
            if f != self.filter {
                self.filter = f;
                self.dirty = true;
                self.to_top = true;
                ui.ctx().request_repaint();
            }
        }
    }

    /// "Quick check" and "Full check" start the app's own check; "Quick check" is the card's one loud
    /// word. While a check or a "Fix all" runs, what it is doing and how far along it is stand in their
    /// place with "Stop" (a full check picks up where it stopped).
    fn check_words(&mut self, ui: &mut Ui, ctx: &egui::Context, cfg: &Config) {
        use std::sync::atomic::Ordering;
        let doing = |ui: &mut Ui, words: &str| {
            ui.add(Label::new(RichText::new(words).size(14.0).color(theme::ACCENT2())).selectable(false).wrap_mode(egui::TextWrapMode::Extend));
            theme::quiet(ui, "Stop", 14.0, true).clicked()
        };
        if let Some(c) = &self.check {
            let (phase, done, total) = (c.phase.load(Ordering::Relaxed) as usize, c.done.load(Ordering::Relaxed), c.total.load(Ordering::Relaxed));
            let name = healthscan::PHASES[phase.min(healthscan::PHASES.len() - 1)];
            let words = if total > 0 { format!("{name}  {} of {}", num(done.min(total) as u64), num(total as u64)) } else { name.to_string() };
            if doing(ui, &words) {
                c.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        if let Some(b) = &self.bulk {
            if doing(ui, &format!("Fixing  {} of {}", num(b.0.load(Ordering::Relaxed).min(b.1) as u64), num(b.1 as u64))) {
                b.2.store(true, Ordering::Relaxed);
            }
            return;
        }
        // debug, like `--screenshot`: `--health-check quick` or `full` starts one check once the page is up
        static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let forced = if ASKED.swap(true, Ordering::Relaxed) { None } else { std::env::args().skip_while(|a| a != "--health-check").nth(1) };
        // (not while a repair is changing the library)
        let can = !self.fixing;
        let first = self.state == State::Missing;
        let quick = theme::main_action(ui, if first { "Check the library" } else { "Quick check" }, can).clicked() || forced.as_deref() == Some("quick");
        let full = theme::quiet(ui, "Full check", 14.0, can).clicked() || forced.as_deref() == Some("full");
        if full || quick {
            self.start_check(ctx, cfg, full);
        }
    }

    fn history_word(&mut self, ui: &mut Ui, size: f32) {
        if theme::quiet(ui, if self.history_open { "Back to the list" } else { "Repair history" }, size, true).clicked() {
            self.history_open = !self.history_open;
            self.history = None;
        }
    }

    fn reload_word(&mut self, ui: &mut Ui, ctx: &egui::Context, size: f32) {
        if theme::quiet(ui, "Reload", size, !self.loading).clicked() {
            self.start_load(ctx);
        }
    }

    /// What the last repair, Undo or check had to say, with "Undo" after a repair's "Done.". `rtl`: the
    /// row is laid out from the right edge inward.
    fn note_words(&mut self, ui: &mut Ui, ctx: &egui::Context, rtl: bool) {
        ui.spacing_mut().item_spacing.x = 16.0;
        if self.note.is_some() && self.last_batch.is_some() && !self.fixing && theme::quiet(ui, "Undo", 13.5, true).clicked() {
            if let Some(id) = self.last_batch.take() {
                self.start_undo(ctx, id);
            }
        }
        // "ffmpeg is not on this PC" goes once the app has fetched it
        if self.note.as_deref() == Some(crate::importer::tools::NO_FFMPEG) && crate::helpers::arrived(crate::helpers::Helper::Ffmpeg) != FFMPEG_NOTED.load(std::sync::atomic::Ordering::Relaxed) {
            self.note = None;
        }
        match &self.note {
            // with the word that fetches it
            Some(n) if n == crate::importer::tools::NO_FFMPEG => crate::helpers::fetch_line(ui, crate::helpers::Helper::Ffmpeg, n, WARN(), 13.5, rtl, false),
            Some(n) => {
                let color = match n.as_str() {
                    "Done." | "Undone." => theme::GREEN(),
                    "There was nothing left to do." => DIM(),
                    _ => WARN(),
                };
                // one line; all of it under the pointer when it is cut short
                ui.add(Label::new(RichText::new(n).size(13.5).color(color)).truncate()).on_hover_text(n);
            }
            None => self.last_batch = None,
        }
    }

    // ------------------------------------------------------------------ the cards under it

    fn body(&mut self, ui: &mut Ui, ctx: &egui::Context, cfg: &Config, left: f32, wide: f32, floor: f32) {
        let top = ui.cursor().top();
        let bottom = (floor - 20.0).max(top + BODY_MIN);
        let whole = Rect::from_min_max(pos2(left, top), pos2(left + wide, bottom));
        if self.history_open {
            theme::panel_in(ui, whole, vec2(24.0, 20.0), "health-history", |ui| self.history_ui(ui, ctx));
        } else if self.report.issues.is_empty() {
            // nothing was found: the sentence and the numbers have said so, and there is nothing to list
            return;
        } else {
            let narrow = wide < NARROW;
            if narrow && self.focus_filter {
                self.sel = None; // Ctrl+F: the filter box has to be there to type in
            }
            let open = self.sel.is_some();
            let (cats, list) = (!(narrow && open), !(wide < TINY && open));
            let cats_w = if wide < 760.0 { 208.0 } else { CATS_W };
            // in a narrow window the album's card gives up a few pixels, so two covers still fit beside it
            let details_w = if !list { wide } else if narrow { (wide - 388.0).clamp(340.0, DETAILS_W) } else { DETAILS_W };
            let mut x = left;
            if cats {
                let rect = Rect::from_min_max(pos2(x, top), pos2(x + cats_w, bottom));
                theme::panel_in(ui, rect, vec2(14.0, 16.0), "health-cats", |ui| self.filters(ui));
                x += cats_w + GAP;
            }
            let right = left + wide - if open { details_w + GAP } else { 0.0 };
            if list {
                let rect = Rect::from_min_max(pos2(x, top), pos2(right, bottom));
                theme::panel_in(ui, rect, PAD, "health-list", |ui| self.findings(ui, ctx, cfg));
            }
            if open {
                let rect = Rect::from_min_max(pos2(left + wide - details_w, top), pos2(left + wide, bottom));
                theme::panel_in(ui, rect, PAD, "health-details", |ui| self.details(ui, cfg, rect));
            }
        }
        if bottom > floor - 19.0 {
            ui.add_space(20.0 - ui.spacing().item_spacing.y); // the page scrolls: room under the cards
        }
    }

    /// The findings' card: what is listed and how many, "Fix all" for a category that has one, the two
    /// ways to look at them, then the list or the wall of covers.
    fn findings(&mut self, ui: &mut Ui, ctx: &egui::Context, cfg: &Config) {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
        // the words at the right first, so the name at the left is cut short rather than pushing them out
        let mut covers_view = self.covers_view;
        let mut words = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)));
        words.spacing_mut().item_spacing.x = 8.0;
        // how to look at them: the wall of covers, or every sentence in a list
        theme::choice(&mut words, &mut covers_view, false, "List");
        theme::choice(&mut words, &mut covers_view, true, "Covers");
        words.add_space(10.0);
        self.fix_all_word(&mut words, ctx, cfg);
        let edge = words.min_rect().left();
        let mut job = LayoutJob::default();
        job.append(&self.listed(), 0.0, TextFormat { font_id: FontId::proportional(11.0), color: DIM(), extra_letter_spacing: 1.4, ..Default::default() });
        job.wrap = TextWrapping { max_width: (edge - row.left() - 16.0).max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let name = ui.painter().layout_job(job);
        ui.painter().galley(pos2(row.left(), (row.center().y - name.size().y / 2.0).round()), name, DIM());
        if covers_view != self.covers_view {
            self.covers_view = covers_view;
            // the album that is open stays in view; with none open, both views start at the worst
            match &self.sel {
                Some((key, _)) => self.scroll_to = Some((key.clone(), true)),
                None => self.to_top = true,
            }
            ui.ctx().request_repaint();
        }
        ui.add_space(6.0);
        if self.covers_view { self.wall(ui) } else { self.list(ui) }
    }

    /// What the list is showing, for the small heading over it: "ALL ISSUES · 796 IN 609 ALBUMS".
    pub(super) fn listed(&self) -> String {
        let name = match &self.filter {
            Filter::All => "All issues".to_string(),
            Filter::Sev(s) => sev_name(*s).to_string(),
            Filter::Cat(_, c) => c.clone(),
        };
        let (issues, albums) = (self.view.shown, self.view.tiles.len());
        if issues == 0 {
            return name.to_uppercase();
        }
        format!("{name}  ·  {} in {} {}", num(issues as u64), num(albums as u64), if albums == 1 { "album" } else { "albums" }).to_uppercase()
    }
}
