//! The Home page: where your music stands, at a glance.
//!
//! Top to bottom, in one centered stretch of the window (the same one the Backup page uses):
//!   - the greeting, large, with the date under it and, beside the date, the one thing worth a look
//!     today when there is one (a click on it goes there);
//!   - the newest releases from your artists, then what is coming soon, each under a large heading. A row
//!     that is full reaches both edges of the stretch; a short one sits in its middle;
//!   - four cards, one for each of the other pages. A card is the way into its page, and all four are
//!     built alike (a small caption, one statement, one quiet line, one picture), so they read across.
//!
//! Everything here only shows what other pages know and leads to them; nothing is started from here.
use crate::engine::ReleaseRec;
use crate::theme::{self, ACCENT2, BORDER, CARD, CARD_HOVER, DIM, FAINT, GREEN, RED, TEXT, WARN};
use crate::{recs_grid, script, Action, App, Page};
use chrono::{Local, NaiveDate, Timelike};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{pos2, vec2, Align, Color32, FontId, Galley, Layout, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, Ui, UiBuilder};
use std::sync::Arc;

/// The page at its widest (as the Backup page), the room between two cards and a card's padding.
const PAGE: f32 = 1180.0;
const GAP: f32 = 20.0;
const PAD: f32 = 24.0;
/// A card, top to bottom: its caption, its statement, its quiet line and its picture, with the room
/// between them. Every card has all four, so the cards are one height and their parts line up.
const CAPTION_H: f32 = 14.0;
const SAY_H: f32 = 30.0;
const QUIET_H: f32 = 17.0;
const ART_H: f32 = 52.0;
const CARD_H: f32 = PAD + CAPTION_H + 10.0 + SAY_H + 4.0 + QUIET_H + 14.0 + ART_H + PAD;

/// How loud a few words are.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tone {
    Plain,
    Quiet,
    Good,
    News,
    Warn,
    Bad,
}

impl Tone {
    fn color(self) -> Color32 {
        match self {
            Tone::Plain => TEXT(),
            Tone::Quiet => DIM(),
            Tone::Good => GREEN(),
            Tone::News => ACCENT2(),
            Tone::Warn => WARN(),
            Tone::Bad => RED(),
        }
    }
}

/// What the line under the greeting chooses from.
#[derive(Default)]
struct Facts {
    backups_need_look: bool,
    ready: usize,
    failed: usize,
    arriving: usize,
    /// (title, artist) of the albums out today
    out_today: Vec<(String, String)>,
    /// releases found by the last refresh that were not there before
    new: usize,
}

/// The one thing worth a look today, most pressing first, and the page it is on. None on a quiet day.
fn lead(f: &Facts) -> Option<(String, Tone, Page)> {
    let n_are = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{} {many}", commas(n)) };
    if f.backups_need_look {
        return Some(("Your backups need a look".to_string(), Tone::Warn, Page::Backup));
    }
    if f.ready > 0 {
        return Some((n_are(f.ready, "album is ready to import", "albums are ready to import"), Tone::Good, Page::Import));
    }
    if f.failed > 0 {
        return Some((n_are(f.failed, "download did not arrive", "downloads did not arrive"), Tone::Warn, Page::Import));
    }
    if f.arriving > 0 {
        return Some((n_are(f.arriving, "album is on its way", "albums are on their way"), Tone::News, Page::Import));
    }
    match f.out_today.as_slice() {
        [] => {}
        [(title, artist)] => return Some((format!("Out today: {title} by {artist}"), Tone::News, Page::Recs)),
        many => return Some((format!("{} albums are out today", commas(many.len())), Tone::News, Page::Recs)),
    }
    (f.new > 0).then(|| (n_are(f.new, "new release", "new releases"), Tone::News, Page::Recs))
}

/// A piece of a card's statement: large and in the light font, or a small word beside a large number.
struct Seg {
    text: String,
    big: bool,
    color: Color32,
}

fn big(text: impl Into<String>, tone: Tone) -> Seg {
    Seg { text: text.into(), big: true, color: tone.color() }
}

fn small(text: impl Into<String>) -> Seg {
    Seg { text: text.into(), big: false, color: DIM() }
}

/// The Downloads card: the most pressing of what is on its way, waiting to be imported or did not
/// arrive, then the rest in one quiet line.
fn downloads_words(arriving: usize, ready: usize, failed: usize) -> (Seg, String) {
    let mut parts = vec![];
    if arriving > 0 {
        parts.push((format!("{} on {} way", commas(arriving), if arriving == 1 { "its" } else { "their" }), Tone::News));
    }
    if ready > 0 {
        parts.push((format!("{} ready to import", commas(ready)), Tone::Good));
    }
    if failed > 0 {
        parts.push((format!("{} did not arrive", commas(failed)), Tone::Warn));
    }
    if parts.is_empty() {
        return (big("Nothing on its way", Tone::Quiet), String::new());
    }
    let (first, tone) = parts.remove(0);
    (big(first, tone), parts.into_iter().map(|p| p.0).collect::<Vec<_>>().join("  ·  "))
}

/// The Health card, from the last check's [open, serious, worth fixing, minor]: the two numbers that ask
/// for something, then the minor ones in the quiet line.
fn health_words(open: Option<[usize; 4]>) -> (Vec<Seg>, String) {
    match open {
        None => (vec![big("No check yet", Tone::Quiet)], String::new()),
        Some([0, ..]) => (vec![big("Nothing wrong", Tone::Good)], "in the last check".to_string()),
        Some([_, 0, 0, minor]) => (vec![big(commas(minor), Tone::Plain), small("minor")], String::new()),
        Some([_, serious, worth, minor]) => {
            let mut segs = vec![];
            if serious > 0 {
                segs.extend([big(commas(serious), Tone::Bad), small("serious")]);
            }
            if worth > 0 {
                segs.extend([big(commas(worth), Tone::Warn), small("worth fixing")]);
            }
            (segs, if minor > 0 { format!("{} minor", commas(minor)) } else { String::new() })
        }
    }
}

/// The Library card: how much is in it.
fn library_words(counts: Option<(usize, usize)>) -> Vec<Seg> {
    match counts {
        Some((albums, artists)) => vec![
            big(if albums == 1 { "1 album".to_string() } else { format!("{} albums", commas(albums)) }, Tone::Plain),
            small(if artists == 1 { "by 1 artist".to_string() } else { format!("by {} artists", commas(artists)) }),
        ],
        None => vec![big("Reading the library", Tone::Quiet)],
    }
}

impl App {
    /// The releases to show: not hidden, not by a muted artist.
    fn open_releases(&self) -> impl Iterator<Item = &ReleaseRec> {
        self.results.releases.iter().filter(|r| !self.cfg.dismissed.contains(&r.key) && !self.cfg.muted.contains_key(&r.artist_key))
    }

    /// Releases already out, newest first, that are not singles: the first `n`, and how many there are.
    fn fresh_releases(&self, today: NaiveDate, n: usize) -> (Vec<ReleaseRec>, usize) {
        let mut all: Vec<(NaiveDate, &ReleaseRec)> = self
            .open_releases()
            .filter(|r| r.kind != "Single")
            .filter_map(|r| NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").ok().map(|d| (d, r)))
            .filter(|(d, _)| *d <= today)
            .collect();
        all.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.artist_weight.total_cmp(&a.1.artist_weight)));
        let total = all.len();
        (all.into_iter().take(n).map(|(_, r)| r.clone()).collect(), total)
    }

    /// The next few releases that are not out yet, soonest first.
    fn coming_soon(&self, today: NaiveDate, n: usize) -> Vec<ReleaseRec> {
        let mut all: Vec<(NaiveDate, &ReleaseRec)> = self.open_releases().filter_map(|r| crate::util::parse_date_earliest(&r.date).map(|d| (d, r))).filter(|(d, _)| *d > today).collect();
        all.sort_by_key(|(d, _)| *d);
        all.into_iter().take(n).map(|(_, r)| r.clone()).collect()
    }

    /// (on their way, waiting to be imported, did not arrive). All 0 until the Downloads page was opened.
    fn download_counts(&self) -> (usize, usize, usize) {
        let ready = match &self.importer {
            Some(Ok(imp)) => imp.waiting(),
            _ => 0,
        };
        let d = self.downloads.as_ref();
        (d.map(|d| d.arriving_count()).unwrap_or(0), ready, d.map(|d| d.failed_count()).unwrap_or(0))
    }

    fn facts(&self, today: NaiveDate) -> Facts {
        let (arriving, ready, failed) = self.download_counts();
        let day = today.format("%Y-%m-%d").to_string();
        Facts {
            backups_need_look: self.backup_on && self.backup.as_ref().is_some_and(|b| b.needs_look()),
            ready,
            failed,
            arriving,
            out_today: self.open_releases().filter(|r| r.kind != "Single" && r.date == day).map(|r| (r.title.clone(), if r.credit.is_empty() { r.artist.clone() } else { r.credit.clone() })).collect(),
            new: self.new_releases(false),
        }
    }

    pub(crate) fn home_page(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // the facts shown here belong to other pages: make sure they are being read
        self.backup.get_or_insert_with(|| crate::backup::BackupPage::new(&ctx)).ensure_loaded(&ctx, &self.cfg);
        self.health.get_or_insert_with(|| crate::health::HealthPage::new(&ctx)).ensure_loaded(&ctx, &self.cfg);

        let now = Local::now();
        let today = now.date_naive();
        let mut goto: Option<Page> = None;
        let mut album: Option<(String, String)> = None;
        let mut actions: Vec<Action> = vec![];

        egui::CentralPanel::default().frame(theme::page_frame()).show(ui, |ui| {
            ScrollArea::vertical().id_salt("home-scroll").auto_shrink([false, false]).show(ui, |ui| {
                let full = ui.available_width();
                let wide = (full - 24.0).clamp(240.0, PAGE).floor();
                let mut column = ui.available_rect_before_wrap();
                column.min.x = (column.left() + (full - wide) / 2.0).round();
                column.max.x = column.min.x + wide;
                ui.scope_builder(UiBuilder::new().max_rect(column).layout(Layout::top_down(Align::Min)), |ui| {
                    ui.add_space(26.0);
                    let hello = match now.hour() {
                        5..=11 => "Good morning",
                        12..=17 => "Good afternoon",
                        _ => "Good evening",
                    };
                    if let Some(page) = top(ui, hello, &now.format("%A, %B %-d").to_string(), lead(&self.facts(today))) {
                        goto = Some(page);
                    }
                    // imports, tag repairs and backups need PowerShell 7: said here, at the start, when it is
                    // missing, with the word that installs it
                    if crate::importer::tools::pwsh().is_none() {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            theme::centered_row(ui, "home-pwsh", 0.0, |ui| {
                                ui.spacing_mut().item_spacing.x = 14.0;
                                crate::helpers::pwsh_line(ui, 14.0, WARN(), false);
                            });
                        });
                    }

                    // the same covers and sizes as the Recs grid; the room between them is what is left,
                    // so a full row reaches both edges of the page
                    let (fit, _, side) = recs_grid::columns(wide);
                    let gap = if fit > 1 { ((wide - fit as f32 * side) / (fit as f32 - 1.0)).max(12.0) } else { GAP };
                    let (fresh, total) = self.fresh_releases(today, fit);
                    ui.add_space(26.0);
                    let all = (total > 0).then(|| format!("See all {}", commas(total)));
                    if head(ui, "New from artists you like", all.as_deref()) {
                        goto = Some(Page::Recs);
                    }
                    if fresh.is_empty() {
                        if self.cfg.has_lastfm() && self.running {
                            ui.horizontal(|ui| {
                                theme::centered_row(ui, "home-looking", 0.0, |ui| {
                                    ui.add(egui::Spinner::new().size(13.0).color(ACCENT2()));
                                    ui.label(RichText::new("Looking for new releases").color(DIM()));
                                });
                            });
                        } else {
                            let why = if self.cfg.has_lastfm() { "Nothing new yet. Refresh on the Recs page to look again." } else { "Connect Last.fm in Settings and the newest releases from your artists show up here." };
                            ui.vertical_centered(|ui| ui.label(RichText::new(why).color(DIM())));
                        }
                    } else {
                        self.shelf(ui, &fresh, side, gap, today, &mut actions);
                    }
                    let soon = self.coming_soon(today, fit);
                    if !soon.is_empty() {
                        ui.add_space(24.0);
                        head(ui, "Coming soon", None);
                        self.shelf(ui, &soon, side, gap, today, &mut actions);
                    }

                    ui.add_space(24.0);
                    head(ui, "Where things stand", None);
                    self.cards(ui, &mut goto, &mut album);
                    ui.add_space(28.0);
                });
            });
        });

        // a click on a cover opens its artist, which the Recs page shows
        let artist = actions.iter().any(|a| matches!(a, Action::Artist(..)));
        self.apply(actions, &ctx);
        if let Some((artist, title)) = album {
            self.library_page.get_or_insert_with(|| crate::library_page::LibraryPage::new(&ctx)).show_album(&artist, &title);
            self.go_to(Page::Library);
        } else if artist {
            self.go_to(Page::Recs);
        } else if let Some(page) = goto {
            self.go_to(page);
        }
    }

    /// One row of covers (the Recs page's own tiles), in the middle of the page when it is not full.
    fn shelf(&mut self, ui: &mut Ui, items: &[ReleaseRec], side: f32, gap: f32, today: NaiveDate, actions: &mut Vec<Action>) {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), side + recs_grid::LINE), Sense::hover());
        let left = recs_grid::row_left(ui, row, items.len(), side, gap);
        for (i, r) in items.iter().enumerate() {
            self.release_tile(ui, r, pos2(left + i as f32 * (side + gap), row.top()), side, today, actions);
        }
    }

    /// The four cards: in a row, two by two in a narrower window, one under the other in a narrow one.
    fn cards(&mut self, ui: &mut Ui, goto: &mut Option<Page>, album: &mut Option<(String, String)>) {
        const ALL: [(&str, Page); 4] = [("DOWNLOADS", Page::Import), ("LIBRARY", Page::Library), ("HEALTH", Page::Health), ("BACKUP", Page::Backup)];
        // (no Backup card where the backup tool is not set up: the three that are left share the row)
        let cards: Vec<(&str, Page)> = ALL.into_iter().filter(|c| c.1 != Page::Backup || self.backup_on).collect();
        #[allow(non_snake_case)]
        let CARDS = cards;
        let wide = ui.available_width();
        let cols = if wide >= 900.0 { CARDS.len() } else if wide >= 520.0 { 2 } else { 1 };
        let card = ((wide - GAP * (cols as f32 - 1.0)) / cols as f32).floor();
        let (left, top) = (ui.cursor().left(), ui.cursor().top());
        for (i, (caption, page)) in CARDS.iter().copied().enumerate() {
            let (col, row) = (i % cols, i / cols);
            // (the last card of a row ends exactly where the page does)
            let x = if cols == 1 { left } else { (left + col as f32 * (wide - card) / (cols as f32 - 1.0)).round() };
            let rect = Rect::from_min_size(pos2(x, top + row as f32 * (CARD_H + GAP)), vec2(card, CARD_H));
            if self.card(ui, rect, caption, page, album) {
                *goto = Some(page);
            }
        }
        let rows = CARDS.len().div_ceil(cols) as f32;
        ui.advance_cursor_after_rect(Rect::from_min_size(pos2(left, top), vec2(wide, rows * CARD_H + (rows - 1.0) * GAP)));
    }

    /// One card. The whole of it is the way into its page: true when it was clicked.
    fn card(&mut self, ui: &mut Ui, rect: Rect, caption: &str, page: Page, album: &mut Option<(String, String)>) -> bool {
        let id = ui.scope_id().with(("home-card", caption));
        let hit = ui.interact(rect, id, Sense::click());
        script::seen(&format!("card:{caption}"), rect);
        let over = ui.rect_contains_pointer(rect);
        let lift = ui.ctx().animate_bool_with_time(id.with("lift"), over, 0.12);
        ui.painter().rect_filled(rect, 12, theme::blend(CARD(), CARD_HOVER(), lift));
        if over {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let inner = rect.shrink(PAD);
        let say_at = pos2(inner.left(), inner.top() + CAPTION_H + 10.0);
        let quiet_at = pos2(inner.left(), say_at.y + SAY_H + 4.0);
        let art = Rect::from_min_size(pos2(inner.left(), quiet_at.y + QUIET_H + 14.0), vec2(inner.width(), ART_H));

        let mut job = LayoutJob::default();
        job.append(caption, 0.0, TextFormat { font_id: FontId::proportional(11.0), extra_letter_spacing: 1.4, color: DIM(), ..Default::default() });
        ui.painter().galley(inner.min, ui.painter().layout_job(job), DIM());

        // what the card says, then its picture
        let (segs, quiet) = match page {
            Page::Import => {
                let (arriving, ready, failed) = self.download_counts();
                let (first, rest) = downloads_words(arriving, ready, failed);
                // one ring per album on its way (how much of it has arrived), a full green one for each
                // waiting to be imported; an empty one while there is nothing
                let mut rings = ui.new_child(UiBuilder::new().id_salt("home-rings").max_rect(art).layout(Layout::left_to_right(Align::Center)));
                rings.spacing_mut().item_spacing.x = 4.0;
                let room = ((art.width() + 4.0) / 26.0).floor() as usize;
                if self.downloads.as_ref().map(|d| d.paint_rings(&mut rings, room)).unwrap_or(0) == 0 {
                    ui.painter().circle_stroke(pos2(art.left() + 11.0, art.center().y), 8.0, Stroke::new(2.5, BORDER()));
                }
                (vec![first], (rest, DIM()))
            }
            Page::Library => {
                let lib = self.library_page.as_ref();
                let counts = lib.and_then(|l| l.counts());
                // the newest additions as covers: a click on one opens that album in the Library
                const COVER: f32 = 52.0;
                let room = (((art.width() + 8.0) / (COVER + 8.0)).floor() as usize).clamp(1, 8);
                let recent = lib.map(|l| l.recent(room)).unwrap_or_default();
                let px = (COVER * ui.ctx().pixels_per_point()).round() as u32;
                for (i, (artist, title, folder)) in recent.iter().enumerate() {
                    let at = Rect::from_min_size(pos2(art.left() + i as f32 * (COVER + 8.0), art.top()), vec2(COVER, COVER));
                    let cover = ui.interact(at, id.with(("added", i)), Sense::click());
                    script::seen(&format!("added:{title}"), at);
                    crate::covers::paint(ui, crate::covers::for_folder(ui.ctx(), folder, px), at, 4);
                    if cover.hovered() {
                        ui.painter().rect_stroke(at, 4, Stroke::new(1.0, DIM()), egui::StrokeKind::Outside);
                    }
                    if cover.on_hover_text(format!("{title}\n{artist}")).clicked() {
                        *album = Some((artist.clone(), title.clone()));
                    }
                }
                let quiet = if counts.is_none() { self.cfg.music_dir.clone() } else if recent.is_empty() { String::new() } else { "Latest additions".to_string() };
                (library_words(counts), (quiet, DIM()))
            }
            Page::Health => {
                let health = self.health.as_ref();
                let open = health.and_then(|h| h.headline());
                let (segs, mut quiet) = health_words(open);
                let mut color = DIM();
                // a check or a repair that is running says how far it is
                if let Some(doing) = health.and_then(|h| h.job()) {
                    (quiet, color) = (doing, ACCENT2());
                }
                // the split by seriousness, as one bar
                let line = Rect::from_center_size(art.center(), vec2(art.width(), 6.0));
                match open {
                    Some([0, ..]) => bar(ui, line, &[(1.0, GREEN())]),
                    Some([_, serious, worth, minor]) => bar(ui, line, &[(serious as f32, RED()), (worth as f32, WARN()), (minor as f32, FAINT())]),
                    None => bar(ui, line, &[]),
                }
                (segs, (quiet, color))
            }
            Page::Backup => {
                let [(say, color), quiet] = self.backup.as_ref().map(|b| b.in_short()).unwrap_or_else(|| [("Checking".to_string(), DIM()), (String::new(), DIM())]);
                // the recent backups as the Backup page draws them, the row of dots on the cards' middle line
                let dots = Rect::from_min_max(pos2(art.left(), (art.center().y - 12.5).round()), pos2(art.right(), rect.bottom()));
                let mut part = ui.new_child(UiBuilder::new().id_salt("home-dots").max_rect(dots).layout(Layout::top_down(Align::Min)));
                if let Some(b) = &self.backup {
                    b.recent_dots(&mut part);
                }
                (vec![Seg { text: say, big: true, color }], quiet)
            }
            _ => (vec![], (String::new(), DIM())),
        };
        say(ui, say_at, inner.width(), &segs);
        let words = cut(ui, &quiet.0, FontId::proportional(12.5), quiet.1, inner.width());
        let cut_short = words.elided;
        ui.painter().galley(quiet_at, words, quiet.1);
        // (a line too long for the card is whole under the pointer)
        let hit = if cut_short { hit.on_hover_text(&quiet.0) } else { hit };
        hit.clicked()
    }
}

/// The greeting, large, in the middle; under it the date and, beside the date, the one thing worth a
/// look. Returns that thing's page when it was clicked.
fn top(ui: &mut Ui, hello: &str, date: &str, lead: Option<(String, Tone, Page)>) -> Option<Page> {
    let wide = ui.available_width();
    let greeting = ui.painter().layout_no_wrap(hello.to_string(), FontId::new(34.0, crate::display_font()), TEXT());
    let (rect, _) = ui.allocate_exact_size(vec2(wide, greeting.size().y), Sense::hover());
    ui.painter().galley(pos2((rect.center().x - greeting.size().x / 2.0).round(), rect.top()), greeting, TEXT());

    let font = FontId::proportional(14.0);
    let day = ui.painter().layout_no_wrap(date.to_string(), font.clone(), DIM());
    let Some((text, tone, page)) = lead else {
        let (row, _) = ui.allocate_exact_size(vec2(wide, day.size().y), Sense::hover());
        ui.painter().galley(pos2((row.center().x - day.size().x / 2.0).round(), row.top()), day, DIM());
        return None;
    };
    let dot = ui.painter().layout_no_wrap("  ·  ".to_string(), font.clone(), FAINT());
    let beside = wide - day.size().x - dot.size().x;
    // on one line with the date while there is room for it there, else on a line of its own
    let one_line = beside >= 240.0;
    let words = cut(ui, &text, font, tone.color(), if one_line { beside } else { wide });
    let at = if one_line {
        let (row, _) = ui.allocate_exact_size(vec2(wide, day.size().y), Sense::hover());
        let x = (row.center().x - (day.size().x + dot.size().x + words.size().x) / 2.0).round();
        let after = x + day.size().x + dot.size().x;
        ui.painter().galley(pos2(after - dot.size().x, row.top()), dot, FAINT());
        ui.painter().galley(pos2(x, row.top()), day, DIM());
        pos2(after, row.top())
    } else {
        let (row, _) = ui.allocate_exact_size(vec2(wide, day.size().y * 2.0 + 2.0), Sense::hover());
        ui.painter().galley(pos2((row.center().x - day.size().x / 2.0).round(), row.top()), day.clone(), DIM());
        pos2((row.center().x - words.size().x / 2.0).round(), row.top() + day.size().y + 2.0)
    };
    let spot = Rect::from_min_size(at, words.size());
    let hit = ui.interact(spot.expand2(vec2(4.0, 3.0)), ui.scope_id().with("home-lead"), Sense::click());
    script::seen(&text, spot);
    ui.painter().galley(at, words, tone.color());
    if hit.hovered() {
        ui.painter().hline(spot.x_range(), spot.bottom() - 1.0, Stroke::new(1.0, tone.color()));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    hit.clicked().then_some(page)
}

/// A part's name in the middle of the page, large and in the light font, and at the right end of its line
/// a few quiet words that lead on ("See all 76"). True when those were clicked.
fn head(ui: &mut Ui, name: &str, more: Option<&str>) -> bool {
    let wide = ui.available_width();
    let words = ui.painter().layout_no_wrap(name.to_string(), FontId::new(28.0, crate::display_font()), TEXT());
    let (rect, _) = ui.allocate_exact_size(vec2(wide, words.size().y), Sense::hover());
    let (x, line) = ((rect.center().x - words.size().x / 2.0).round(), baseline(&words));
    let end = x + words.size().x;
    ui.painter().galley(pos2(x, rect.top()), words, TEXT());
    let mut clicked = false;
    if let Some(more) = more {
        let font = FontId::proportional(13.0);
        let quiet = ui.painter().layout_no_wrap(more.to_string(), font.clone(), DIM());
        // on the name's line of writing, at the page's right edge; under the name when the two would meet
        let at = if rect.right() - quiet.size().x >= end + 24.0 {
            pos2(rect.right() - quiet.size().x, rect.top() + line - baseline(&quiet))
        } else {
            let (under, _) = ui.allocate_exact_size(vec2(wide, quiet.size().y), Sense::hover());
            pos2((under.center().x - quiet.size().x / 2.0).round(), under.top())
        };
        let spot = Rect::from_min_size(at, quiet.size());
        let hit = ui.interact(spot.expand2(vec2(4.0, 3.0)), ui.scope_id().with(("home-more", name)), Sense::click());
        script::seen(more, spot);
        ui.painter().text(at, egui::Align2::LEFT_TOP, more, font, if hit.hovered() { TEXT() } else { DIM() });
        if hit.hovered() {
            ui.painter().hline(spot.x_range(), spot.bottom() - 1.0, Stroke::new(1.0, DIM()));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        clicked = hit.clicked();
    }
    ui.add_space(8.0);
    clicked
}

/// A card's statement on one line: large words in the light font, small ones beside them on the same
/// line of writing. What does not fit is cut off with "…".
fn say(ui: &Ui, at: Pos2, width: f32, segs: &[Seg]) {
    let large = FontId::new(22.0, crate::display_font());
    let zero = ui.painter().layout_no_wrap("0".to_string(), large.clone(), TEXT());
    let top = at.y + ((SAY_H - zero.size().y) / 2.0).round();
    let line = baseline(&zero);
    let mut x = at.x;
    for (i, seg) in segs.iter().enumerate() {
        // a small word stands close to its number; the next number stands apart
        if i > 0 {
            x += if seg.big { 12.0 } else { 5.0 };
        }
        let room = at.x + width - x;
        if room < 12.0 {
            break;
        }
        let color = seg.color;
        let words = cut(ui, &seg.text, if seg.big { large.clone() } else { FontId::proportional(13.0) }, color, room);
        let y = if seg.big { top } else { top + line - baseline(&words) };
        let w = words.size().x;
        ui.painter().galley(pos2(x, y), words, color);
        x += w;
    }
}

/// How far under a line of text's top its letters stand: small words beside large ones are set lower by
/// the difference, so both stand on one line of writing.
fn baseline(words: &Galley) -> f32 {
    words.rows.first().and_then(|row| row.row.glyphs.first().map(|glyph| row.pos.y + glyph.pos.y)).unwrap_or(words.size().y * 0.8)
}

/// One line of words, cut off with "…" where the room ends.
fn cut(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

/// One thin bar split into colored parts in proportion to their sizes; a plain line with nothing to split.
fn bar(ui: &Ui, rect: Rect, parts: &[(f32, Color32)]) {
    let total: f32 = parts.iter().map(|p| p.0).sum();
    if total <= 0.0 {
        ui.painter().rect_filled(rect, 3, BORDER());
        return;
    }
    let gap = 2.0;
    let room = rect.width() - gap * (parts.iter().filter(|p| p.0 > 0.0).count().saturating_sub(1)) as f32;
    let mut x = rect.left();
    for (size, color) in parts.iter().filter(|p| p.0 > 0.0) {
        let w = (room * size / total).max(6.0).min(rect.right() - x);
        ui.painter().rect_filled(Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height())), 3, *color);
        x += w + gap;
    }
}

/// 1790 -> "1,790"
fn commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(f: Facts) -> Option<(String, Page)> {
        lead(&f).map(|(text, _, page)| (text, page))
    }

    #[test]
    fn the_line_under_the_greeting_names_the_most_pressing_thing() {
        let days = |title: &str| (title.to_string(), "The Mountain Goats".to_string());
        // a quiet day says nothing
        assert!(lead(&Facts::default()).is_none());
        // everything at once: the backups first, then each next thing as the one before it goes away
        let mut f = Facts { backups_need_look: true, ready: 3, failed: 1, arriving: 2, out_today: vec![days("Days")], new: 4 };
        let mut seen = vec![];
        for step in 0..6 {
            seen.push(lead(&f).unwrap());
            match step {
                0 => f.backups_need_look = false,
                1 => f.ready = 0,
                2 => f.failed = 0,
                3 => f.arriving = 0,
                _ => f.out_today.clear(),
            }
        }
        let texts: Vec<&str> = seen.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(texts, ["Your backups need a look", "3 albums are ready to import", "1 download did not arrive", "2 albums are on their way", "Out today: Days by The Mountain Goats", "4 new releases"]);
        assert!(seen.iter().map(|s| s.2).eq([Page::Backup, Page::Import, Page::Import, Page::Import, Page::Recs, Page::Recs]));
        assert_eq!((seen[0].1, seen[1].1, seen[2].1, seen[3].1), (Tone::Warn, Tone::Good, Tone::Warn, Tone::News));
        // one and many
        assert_eq!(said(Facts { ready: 1, ..Facts::default() }).unwrap().0, "1 album is ready to import");
        assert_eq!(said(Facts { failed: 2, ..Facts::default() }).unwrap().0, "2 downloads did not arrive");
        assert_eq!(said(Facts { arriving: 1, ..Facts::default() }).unwrap().0, "1 album is on its way");
        assert_eq!(said(Facts { out_today: vec![days("Days"), days("Nights")], ..Facts::default() }).unwrap().0, "2 albums are out today");
        assert_eq!(said(Facts { new: 1, ..Facts::default() }).unwrap().0, "1 new release");
        assert_eq!(said(Facts { new: 1200, ..Facts::default() }).unwrap().0, "1,200 new releases");
    }

    #[test]
    fn the_cards_say_what_matters_first() {
        let downloads = |a, r, f| {
            let (first, rest) = downloads_words(a, r, f);
            (first.text, first.color, rest)
        };
        assert_eq!(downloads(0, 0, 0), ("Nothing on its way".to_string(), Tone::Quiet.color(), String::new()));
        assert_eq!(downloads(1, 0, 0), ("1 on its way".to_string(), Tone::News.color(), String::new()));
        assert_eq!(downloads(2, 1, 1), ("2 on their way".to_string(), Tone::News.color(), "1 ready to import  ·  1 did not arrive".to_string()));
        assert_eq!(downloads(0, 3, 1), ("3 ready to import".to_string(), Tone::Good.color(), "1 did not arrive".to_string()));
        assert_eq!(downloads(0, 0, 2), ("2 did not arrive".to_string(), Tone::Warn.color(), String::new()));

        let health = |open| {
            let (segs, quiet) = health_words(open);
            (segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" "), segs.first().map(|s| s.color), quiet)
        };
        assert_eq!(health(None), ("No check yet".to_string(), Some(Tone::Quiet.color()), String::new()));
        assert_eq!(health(Some([0, 0, 0, 0])), ("Nothing wrong".to_string(), Some(Tone::Good.color()), "in the last check".to_string()));
        assert_eq!(health(Some([1486, 26, 256, 1204])), ("26 serious 256 worth fixing".to_string(), Some(Tone::Bad.color()), "1,204 minor".to_string()));
        assert_eq!(health(Some([260, 0, 256, 4])), ("256 worth fixing".to_string(), Some(Tone::Warn.color()), "4 minor".to_string()));
        assert_eq!(health(Some([3, 3, 0, 0])), ("3 serious".to_string(), Some(Tone::Bad.color()), String::new()));
        assert_eq!(health(Some([7, 0, 0, 7])), ("7 minor".to_string(), Some(Tone::Plain.color()), String::new()));

        let library = |counts| library_words(counts).iter().map(|s| s.text.clone()).collect::<Vec<_>>().join(" ");
        assert_eq!(library(Some((1940, 637))), "1,940 albums by 637 artists");
        assert_eq!(library(Some((1, 1))), "1 album by 1 artist");
        assert_eq!(library(None), "Reading the library");
        assert_eq!((commas(0), commas(999), commas(1000), commas(1234567)), ("0".to_string(), "999".to_string(), "1,000".to_string(), "1,234,567".to_string()));
    }
}
