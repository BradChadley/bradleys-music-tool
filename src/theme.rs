//! One look for every page: the colors, and the few small pieces every page is built from.
//! House rules: plain styled words instead of big buttons, no pill badges, and screens that explain
//! themselves without help text.
//!
//! The colors can be changed while the app runs (Settings, "Colors"): every page asks for a color by its
//! role (`BG()`, `TEXT()`, ...) each time it draws, so a change shows at once. What is chosen is kept in the
//! settings file as "#rrggbb" by role; with nothing kept there, the first theme in `THEMES` is used.
use egui::{vec2, Color32, CornerRadius, Frame, Label, Margin, Response, RichText, Sense, Stroke, Ui};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// How many colors a theme has.
pub const ROLES: usize = 13;
/// Each color's name in the settings file and its words in Settings, in the order themes list them.
pub const ROLE_NAMES: [(&str, &str); ROLES] = [
    ("background", "Background"),
    ("panel", "Top row and panels"),
    ("card", "Cards"),
    ("hover", "Under the pointer"),
    ("line", "Lines"),
    ("text", "Text"),
    ("quiet", "Quiet text"),
    ("faint", "Faint text"),
    ("accent", "Accent"),
    ("link", "Links and progress"),
    ("good", "Good"),
    ("warning", "Warning"),
    ("problem", "Problem"),
];

/// The themes to choose from, each a color per role as 0xRRGGBB. The first is the one the app starts with.
pub const THEMES: [(&str, [u32; ROLES]); 5] = [
    // Bradley's own: neutral black, 2026-11-11
    ("Black", [0x0f0f0f, 0x232323, 0x232323, 0x2e2e2e, 0x3a3a3a, 0xf0f0f0, 0xafafaf, 0x7d7d7d, 0xff6b6b, 0x82aaff, 0x6ed296, 0xf0aa50, 0xf06464]),
    // the look the app had before: dark with a little blue in it
    ("Midnight", [0x111115, 0x18181e, 0x1e1e26, 0x262630, 0x30303c, 0xe8e8ee, 0x9696a5, 0x6e6e7d, 0xff6b6b, 0x82aaff, 0x6ed296, 0xf0aa50, 0xf06464]),
    ("Graphite", [0x1c1c1e, 0x2a2a2d, 0x2a2a2d, 0x36363a, 0x48484d, 0xf2f2f4, 0xb4b4ba, 0x84848c, 0xff7a6b, 0x8ab4ff, 0x6ed296, 0xf0aa50, 0xf06464]),
    ("Forest", [0x0d1210, 0x1a2420, 0x1a2420, 0x24322c, 0x33453d, 0xeaf2ee, 0xa3b5ac, 0x71837a, 0xe8b860, 0x8fd0c0, 0x6ed296, 0xf0aa50, 0xf06464]),
    ("Paper", [0xf6f5f2, 0xe9e7e2, 0xffffff, 0xeeece6, 0xd6d3cc, 0x1c1c1c, 0x555555, 0x7c7c7c, 0xc0392b, 0x2360c8, 0x1f7a49, 0x945b00, 0xc22f2f]),
];

const fn slot(n: usize) -> AtomicU32 {
    AtomicU32::new(THEMES[0].1[n])
}
/// The colors in use now.
static NOW: [AtomicU32; ROLES] = [slot(0), slot(1), slot(2), slot(3), slot(4), slot(5), slot(6), slot(7), slot(8), slot(9), slot(10), slot(11), slot(12)];
/// Counts every change, so the window knows when to take the colors up again.
static CHANGES: AtomicU64 = AtomicU64::new(0);

fn rgb(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn packed(c: Color32) -> u32 {
    ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32
}

/// The color a role has now.
pub fn color(role: usize) -> Color32 {
    rgb(NOW[role.min(ROLES - 1)].load(Ordering::Relaxed))
}

/// Gives one role another color.
pub fn set(role: usize, c: Color32) {
    if role < ROLES && NOW[role].swap(packed(c), Ordering::Relaxed) != packed(c) {
        CHANGES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Takes up a whole theme.
pub fn set_all(colors: &[u32; ROLES]) {
    for (n, c) in colors.iter().enumerate() {
        set(n, rgb(*c));
    }
}

/// The colors in use now, by role.
pub fn snapshot() -> [u32; ROLES] {
    std::array::from_fn(|n| NOW[n].load(Ordering::Relaxed))
}

/// A theme kept in the settings file as "#rrggbb" by role: the first theme with each role named there in
/// its kept color. None when nothing is kept.
pub fn from_saved(saved: &BTreeMap<String, String>) -> Option<[u32; ROLES]> {
    if saved.is_empty() {
        return None;
    }
    let mut out = THEMES[0].1;
    for (n, (key, _)) in ROLE_NAMES.iter().enumerate() {
        if let Some(c) = saved.get(*key).and_then(|v| from_hex(v)) {
            out[n] = packed(c);
        }
    }
    Some(out)
}

/// A whole set of colors as the settings file keeps one.
pub fn as_saved(colors: &[u32; ROLES]) -> BTreeMap<String, String> {
    ROLE_NAMES.iter().enumerate().map(|(n, (key, _))| (key.to_string(), to_hex(rgb(colors[n])))).collect()
}

/// How many times a color has changed since the app started.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::Relaxed)
}

/// The theme whose colors are exactly the ones in use, if any.
pub fn current_theme() -> Option<&'static str> {
    THEMES.iter().find(|t| t.1.iter().enumerate().all(|(n, c)| NOW[n].load(Ordering::Relaxed) == *c)).map(|t| t.0)
}

/// "#rrggbb" (or "rrggbb") as a color; None for anything else.
pub fn from_hex(s: &str) -> Option<Color32> {
    let h = s.trim().trim_start_matches('#');
    (h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| u32::from_str_radix(h, 16).ok().map(rgb)).flatten()
}

pub fn to_hex(c: Color32) -> String {
    format!("#{:06x}", packed(c))
}

/// Takes up the colors kept in the settings file: the first theme, with each role named there in its
/// kept color. A name or a color that cannot be read is passed over.
pub fn load(saved: &BTreeMap<String, String>) {
    set_all(&from_saved(saved).unwrap_or(THEMES[0].1));
}

/// The colors in use, as the settings file keeps them: nothing at all while they are the first theme's.
pub fn saved() -> BTreeMap<String, String> {
    if current_theme() == Some(THEMES[0].0) {
        return BTreeMap::new();
    }
    as_saved(&snapshot())
}

/// `a` at 0, `b` at 1, in between in between.
pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// The sizes everything can be drawn at (Settings, "Look"; Ctrl with plus, minus and 0 step through them too).
pub const SIZES: [f32; 6] = [0.9, 1.0, 1.1, 1.25, 1.5, 1.75];

/// True when the background is a light one (a theme like "Paper").
pub fn is_light() -> bool {
    let c = BG();
    (c.r() as u32 * 299 + c.g() as u32 * 587 + c.b() as u32 * 114) / 1000 > 140
}

/// The fill of a box you type in: a step further from the text than the background is.
pub fn field_bg() -> Color32 {
    let c = BG();
    if is_light() { Color32::WHITE } else { Color32::from_rgb((c.r() as f32 * 0.65) as u8, (c.g() as f32 * 0.65) as u8, (c.b() as f32 * 0.65) as u8) }
}

macro_rules! roles {
    ($($(#[$doc:meta])* $name:ident = $n:expr;)*) => {
        $($(#[$doc])* #[allow(non_snake_case)] pub fn $name() -> Color32 { color($n) })*
    };
}
roles! {
    BG = 0;
    BG2 = 1;
    CARD = 2;
    CARD_HOVER = 3;
    BORDER = 4;
    TEXT = 5;
    DIM = 6;
    FAINT = 7;
    /// the app's own color (the current page, the main action)
    ACCENT = 8;
    /// things you can click, and progress
    ACCENT2 = 9;
    GREEN = 10;
    WARN = 11;
    RED = 12;
}

/// Space between a page's edge and its content.
pub const PAGE_MARGIN: i8 = 18;

/// A word you can click: blue, underlined while the pointer is on it. Returns the response (`.clicked()`).
pub fn link(ui: &mut Ui, text: &str) -> Response {
    link_sized(ui, text, 14.0, true)
}

/// Like [`link`], but grayed out and not clickable when `enabled` is false.
pub fn link_enabled(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    link_sized(ui, text, 14.0, enabled)
}

pub fn link_sized(ui: &mut Ui, text: &str, size: f32, enabled: bool) -> Response {
    let color = if enabled { ACCENT2() } else { FAINT() };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let r = ui.add(Label::new(RichText::new(text).size(size).color(color)).selectable(false).sense(sense));
    crate::script::seen(text, r.rect);
    if enabled && r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(1.0, ACCENT2()));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// A word that is either the chosen one (bright, underlined in coral) or not (gray). Click it to choose it.
pub fn word(ui: &mut Ui, text: &str, size: f32, chosen: bool) -> Response {
    let rich = RichText::new(text).size(size).color(if chosen { TEXT() } else { DIM() });
    // (never broken over two lines: in a row that wraps, the word goes to the next line whole)
    let r = ui.add(Label::new(rich).wrap_mode(egui::TextWrapMode::Extend).selectable(false).sense(Sense::click()));
    crate::script::seen(text, r.rect);
    if chosen {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() + 1.0, Stroke::new(1.5, ACCENT()));
    } else if r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() + 1.0, Stroke::new(1.0, FAINT()));
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// A word that switches something on and off. `.clicked()` on the result means it just changed.
pub fn toggle(ui: &mut Ui, on: &mut bool, text: impl Into<String>) -> Response {
    let r = word(ui, &text.into(), 14.0, *on);
    if r.clicked() {
        *on = !*on;
    }
    ui.add_space(8.0);
    r
}

/// One of several words where exactly one is chosen.
pub fn choice<T: PartialEq>(ui: &mut Ui, current: &mut T, value: T, text: impl Into<String>) -> Response {
    let r = word(ui, &text.into(), 14.0, *current == value);
    if r.clicked() {
        *current = value;
    }
    ui.add_space(8.0);
    r
}

/// Puts what `add` draws in the middle of the row it is in (call it first thing inside `ui.horizontal`).
/// The row's width is only known once it has been drawn, so it is remembered under `salt` from the frame
/// before; the very first frame draws it unseen. `keep_right` is room to leave at the right for whatever
/// follows it there, so a narrow window pushes the row left rather than under those words.
pub fn centered_row<R>(ui: &mut Ui, salt: &str, keep_right: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let id = egui::Id::unique(("centered-row", salt));
    let known: Option<f32> = ui.ctx().data(|d| d.get_temp(id));
    let full = ui.available_width();
    let lead = known.map_or(0.0, |w| ((full - w) / 2.0).min(full - w - keep_right).max(0.0).floor());
    ui.add_space(lead);
    let row = ui.scope(|ui| {
        if known.is_none() {
            ui.set_invisible();
        }
        add(ui)
    });
    let width = row.response.rect.width();
    if known.is_none_or(|w| (w - width).abs() > 0.5) {
        ui.ctx().data_mut(|d| d.insert_temp(id, width));
        ui.ctx().request_repaint();
    }
    row.inner
}

/// The one main action of a screen ("Back up now", "Import 3 albums"): a coral word, bold.
pub fn main_action(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    main_action_sized(ui, text, 15.0, enabled)
}

/// [`main_action`] at another size, for the one action a whole page is for ("Import 3 albums").
pub fn main_action_sized(ui: &mut Ui, text: &str, size: f32, enabled: bool) -> Response {
    let color = if enabled { ACCENT() } else { FAINT() };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let r = ui.add(Label::new(RichText::new(text).size(size).strong().color(color)).selectable(false).sense(sense));
    crate::script::seen(text, r.rect);
    if enabled && r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(if size > 16.0 { 2.0 } else { 1.5 }, ACCENT()));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// Called first thing every frame: notes whether a text box had the keyboard as the frame began. A box gives
/// the keyboard up on the very frame Enter is pressed in it, so a page drawn after it would otherwise take
/// that Enter for its own (see [`typing`]).
pub fn note_typing(ctx: &egui::Context) {
    let t = ctx.text_edit_focused();
    ctx.data_mut(|d| {
        // (what the frame before began with is kept too: see `typing_until_now`)
        let was = d.get_temp::<bool>(egui::Id::unique("typing-at-frame-start")).unwrap_or(false);
        d.insert_temp(egui::Id::unique("typing-a-frame-ago"), was);
        d.insert_temp(egui::Id::unique("typing-at-frame-start"), t);
    });
}

/// True while a text box has the keyboard, had it when this frame began, or had it when the frame before
/// began. Escape takes the keyboard from a box before the frame that carries the key even begins, so by
/// then `typing` says no: a page that closes on Escape asks this instead, and an Escape that only left a
/// box does not close the page as well.
pub fn typing_until_now(ctx: &egui::Context) -> bool {
    typing(ctx) || ctx.data(|d| d.get_temp::<bool>(egui::Id::unique("typing-a-frame-ago"))).unwrap_or(false)
}

/// True while a text box has the keyboard, or had it when this frame began: page keys (arrows, Space,
/// Enter) leave the frame alone then.
pub fn typing(ctx: &egui::Context) -> bool {
    ctx.text_edit_focused() || ctx.data(|d| d.get_temp::<bool>(egui::Id::unique("typing-at-frame-start"))).unwrap_or(false)
}

/// The frame for a page's main area.
pub fn page_frame() -> Frame {
    Frame::new().fill(BG()).inner_margin(Margin::same(PAGE_MARGIN))
}

/// A number with a word under it ("12 / damaged files"), for summaries. Plain text, no box.
pub fn stat(ui: &mut Ui, value: &str, label: &str, color: Color32) {
    ui.vertical(|ui| {
        ui.label(RichText::new(value).size(22.0).strong().color(color));
        ui.label(RichText::new(label).size(11.0).color(DIM()));
    });
}

/// A thin progress line.
pub fn progress(ui: &mut Ui, frac: f32, width: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 4.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(2), BORDER());
    let mut done = rect;
    done.set_width(rect.width() * frac.clamp(0.0, 1.0));
    ui.painter().rect_filled(done, CornerRadius::same(2), color);
}

/// Centered gray text for a page or list with nothing in it. Say what would make something appear.
pub fn empty(ui: &mut Ui, text: &str) {
    ui.add_space(80.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).size(14.0).color(DIM()));
    });
}

// ---------------------------------------------------------------------- pages built from cards
// (the Downloads page; the Backup page has its own copies of the first few in `backup/page.rs`)

/// The corner of a card.
pub const CARD_ROUND: u8 = 12;

/// A page's one centered stretch of the window: where it starts and how wide it is, at most `most`, with
/// a little room left at both edges of a narrow window.
pub fn column(ui: &Ui, most: f32) -> (f32, f32) {
    let full = ui.available_width();
    let wide = (full - 24.0).clamp(240.0, most).floor();
    ((ui.available_rect_before_wrap().left() + (full - wide) / 2.0).round(), wide)
}

/// The light cut of the font at a size, for a page's large words.
pub fn display(size: f32) -> egui::FontId {
    egui::FontId::new(size, crate::display_font())
}

/// One line of words, cut off with "…" at `width`.
pub fn cut(ui: &Ui, text: &str, font: egui::FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: width.max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

/// How far under a line of text's top its letters stand: small words beside large ones are set lower by
/// the difference, so both stand on one line of writing.
pub fn baseline(words: &egui::Galley) -> f32 {
    words.rows.first().and_then(|row| row.row.glyphs.first().map(|glyph| row.pos.y + glyph.pos.y)).unwrap_or(words.size().y * 0.8)
}

/// A small quiet heading over a part of a card, its letters set a little apart.
pub fn caption(ui: &mut Ui, text: &str) -> Response {
    ui.add(Label::new(RichText::new(text).size(11.0).extra_letter_spacing(1.4).color(DIM())).selectable(false))
}

/// A word you can click that does not ask for attention: gray, and bright and underlined under the
/// pointer. For what is done now and then, so that the one colored word in a card is the one it is for.
pub fn quiet(ui: &mut Ui, text: &str, size: f32, enabled: bool) -> Response {
    let font = egui::FontId::proportional(size);
    let words = ui.painter().layout_no_wrap(text.to_string(), font.clone(), DIM());
    let (rect, r) = ui.allocate_exact_size(words.size(), if enabled { Sense::click() } else { Sense::hover() });
    crate::script::seen(text, rect);
    let over = enabled && r.hovered();
    let color = if !enabled { FAINT() } else if over { TEXT() } else { DIM() };
    ui.painter().text(rect.min, egui::Align2::LEFT_TOP, text, font, color);
    if over {
        ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, DIM()));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// A card: a rounded box from `left`, `wide` across, as tall as what `add` draws in it, with `pad` of
/// room inside its edges (x at the sides, y over and under).
pub fn panel<R>(ui: &mut Ui, left: f32, wide: f32, pad: egui::Vec2, salt: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = ui.painter().add(egui::Shape::Noop);
    let top = ui.cursor().top();
    let room = egui::Rect::from_min_max(egui::pos2(left + pad.x, top + pad.y), egui::pos2(left + wide - pad.x, top + 100_000.0));
    let mut inner = ui.new_child(egui::UiBuilder::new().id_salt(salt).max_rect(room).layout(egui::Layout::top_down(egui::Align::Min)));
    let out = add(&mut inner);
    let rect = egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(left + wide, inner.min_rect().bottom().max(room.top()) + pad.y));
    ui.painter().set(fill, egui::epaint::RectShape::filled(rect, CornerRadius::same(CARD_ROUND), CARD()));
    ui.advance_cursor_after_rect(rect);
    out
}

/// A card that fills a rectangle that is already known (a list that takes the rest of the window):
/// `add` draws inside it, `pad` in from its edges, and nothing it draws shows outside the card.
pub fn panel_in<R>(ui: &mut Ui, rect: egui::Rect, pad: egui::Vec2, salt: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.painter().rect_filled(rect, CornerRadius::same(CARD_ROUND), CARD());
    let mut inner = ui.new_child(egui::UiBuilder::new().id_salt(salt).max_rect(rect.shrink2(pad)).layout(egui::Layout::top_down(egui::Align::Min)));
    inner.set_clip_rect(rect.intersect(ui.clip_rect()));
    let out = add(&mut inner);
    ui.advance_cursor_after_rect(rect);
    out
}

/// A small X that takes something off the page: faint, bright under the pointer. `name` is what a test
/// script calls it, `tip` what it says under the pointer.
pub fn close_mark(ui: &mut Ui, name: &str, tip: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
    crate::script::seen(name, rect);
    let hot = r.hovered();
    if hot {
        ui.painter().circle_filled(rect.center(), 7.0, CARD_HOVER());
    }
    let (c, s) = (rect.center(), 3.5);
    let line = Stroke::new(1.3, if hot { TEXT() } else { FAINT() });
    ui.painter().line_segment([c + vec2(-s, -s), c + vec2(s, s)], line);
    ui.painter().line_segment([c + vec2(s, -s), c + vec2(-s, s)], line);
    r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

/// Cards side by side, each row of them as tall as its tallest needs to be: `n` cards in `cols` columns
/// from `left`, `wide` across, `gap` between them and `pad` inside their edges, none lower than `min_h`.
/// `add` draws card `i` from its top and gives back what stands at its foot (words and their color),
/// which is set at the bottom of the card whatever its height. With `faint` the cards are faint and dead
/// to clicks. Nothing is remembered from the frame before: the cards are measured as they are drawn and
/// their boxes are filled in afterwards.
#[allow(clippy::too_many_arguments)]
pub fn card_row(ui: &mut Ui, salt: &str, left: f32, wide: f32, cols: usize, n: usize, gap: f32, pad: egui::Vec2, min_h: f32, faint: bool, mut add: impl FnMut(usize, &mut Ui) -> Option<(String, Color32)>) {
    let cols = cols.clamp(1, n.max(1));
    let card_w = if cols > 1 { ((wide - gap * (cols - 1) as f32) / cols as f32).floor() } else { wide };
    let top = ui.cursor().top();
    // (a foot has room for two lines even when it is one, so a card whose foot changes its words does not
    // change its height with them)
    let two_lines = 2.0 * ui.fonts_mut(|f| f.row_height(&egui::FontId::proportional(12.5)));
    let mut y = top;
    let mut i = 0;
    while i < n {
        let mut drawn = vec![];
        let mut tallest = min_h - 2.0 * pad.y;
        for k in 0..cols.min(n - i) {
            let x = if cols == 1 { left } else { (left + k as f32 * (wide - card_w) / (cols - 1) as f32).round() };
            let fill = ui.painter().add(egui::Shape::Noop);
            let room = egui::Rect::from_min_max(egui::pos2(x + pad.x, y + pad.y), egui::pos2(x + card_w - pad.x, y + 100_000.0));
            let mut inner = ui.new_child(egui::UiBuilder::new().id_salt((salt, i + k)).max_rect(room).layout(egui::Layout::top_down(egui::Align::Min)));
            if faint {
                inner.disable();
                inner.multiply_opacity(0.7);
            }
            let foot = add(i + k, &mut inner).map(|(words, color)| (inner.painter().layout(words, egui::FontId::proportional(12.5), color, room.width()), color));
            let used = inner.min_rect().bottom().max(room.top()) - room.top();
            tallest = tallest.max(used + foot.as_ref().map_or(0.0, |(f, _)| 14.0 + f.size().y.max(two_lines) - 2.0));
            drawn.push((x, fill, inner, foot));
        }
        let h = (tallest + 2.0 * pad.y).ceil();
        for (x, fill, inner, foot) in drawn {
            let card = egui::Rect::from_min_size(egui::pos2(x, y), vec2(card_w, h));
            ui.painter().set(fill, egui::epaint::RectShape::filled(card, CornerRadius::same(CARD_ROUND), CARD()));
            if let Some((words, color)) = foot {
                // (two under the card's own room at the bottom, where the first-start screen has it)
                inner.painter().galley(egui::pos2(card.left() + pad.x, card.bottom() - (pad.y - 2.0) - words.size().y), words, color);
            }
        }
        y += h + gap;
        i += cols;
    }
    ui.advance_cursor_after_rect(egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(left + wide, (y - gap).max(top))));
}

/// Words in the middle of the row they are in, on one line.
pub fn centered(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32) {
    let words = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), words.size().y), Sense::hover());
    ui.painter().galley(egui::pos2((row.center().x - words.size().x / 2.0).round(), row.top()), words, color);
}

/// Words in the middle of the row they are in, on as many lines as `width` makes of them, each line in
/// the middle.
pub fn centered_wrapped(ui: &mut Ui, text: &str, size: f32, color: Color32, width: f32) {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), egui::FontId::proportional(size), color, width);
    job.halign = egui::Align::Center;
    let words = ui.painter().layout_job(job);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), words.size().y), Sense::hover());
    // (words set around their middle are placed by that middle)
    ui.painter().galley(egui::pos2(row.center().x.round(), row.top()), words, color);
}

/// Centered words that can be clicked, in a color of their own (what needs a look, at the top of
/// Settings). `name` is what a test script calls them.
pub fn centered_word(ui: &mut Ui, name: &str, text: &str, size: f32, color: Color32) -> Response {
    let words = ui.painter().layout_no_wrap(text.to_string(), egui::FontId::proportional(size), color);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), words.size().y), Sense::hover());
    let rect = egui::Rect::from_min_size(egui::pos2((row.center().x - words.size().x / 2.0).round(), row.top()), words.size());
    let r = ui.interact(rect, egui::Id::unique(("centered-word", name)), Sense::click());
    crate::script::seen(name, rect);
    ui.painter().galley(rect.min, words, color);
    if r.hovered() {
        ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, color));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far apart two colors are in brightness, as the usual contrast ratio (1 = the same, 21 = black on white).
    fn contrast(a: u32, b: u32) -> f64 {
        let lum = |v: u32| {
            let ch = |c: u32| {
                let c = c as f64 / 255.0;
                if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch((v >> 16) & 255) + 0.7152 * ch((v >> 8) & 255) + 0.0722 * ch(v & 255)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn every_theme_can_be_read() {
        for (name, c) in THEMES.iter() {
            // text, quiet text and every signal color on the background, the panels and the cards
            for ground in [c[0], c[1], c[2]] {
                assert!(contrast(c[5], ground) >= 7.0, "{name}: text on {ground:06x}");
                assert!(contrast(c[6], ground) >= 4.5, "{name}: quiet text on {ground:06x}");
                assert!(contrast(c[7], ground) >= 2.6, "{name}: faint text on {ground:06x}");
                for signal in &c[8..] {
                    assert!(contrast(*signal, ground) >= 3.0, "{name}: {signal:06x} on {ground:06x}");
                }
            }
            // what is under the pointer can be told from a card, and a line from the background
            assert!(c[3] != c[2] && c[4] != c[0], "{name}");
        }
        let mut names: Vec<&str> = THEMES.iter().map(|t| t.0).collect();
        names.dedup();
        assert_eq!(names.len(), THEMES.len());
    }

    #[test]
    fn colors_are_written_and_read_as_hex() {
        assert_eq!(from_hex("#0f0f0f"), Some(Color32::from_rgb(15, 15, 15)));
        assert_eq!(from_hex(" AFAFAF "), Some(Color32::from_rgb(175, 175, 175)));
        for bad in ["", "#fff", "#12345g", "0f0f0f0f", "red"] {
            assert_eq!(from_hex(bad), None, "{bad}");
        }
        assert_eq!(to_hex(Color32::from_rgb(35, 35, 35)), "#232323");
        assert_eq!(ROLE_NAMES.len(), ROLES);
        // a kept theme comes back as it was; a role that is missing or unreadable falls back to the first theme's
        let forest = THEMES[3].1;
        assert_eq!(from_saved(&as_saved(&forest)), Some(forest));
        assert_eq!(from_saved(&BTreeMap::new()), None);
        let mut part = BTreeMap::new();
        part.insert("text".to_string(), "#ffffff".to_string());
        part.insert("accent".to_string(), "not a color".to_string());
        part.insert("no such role".to_string(), "#000000".to_string());
        let got = from_saved(&part).unwrap();
        assert_eq!((got[5], got[8], got[0]), (0xffffff, THEMES[0].1[8], THEMES[0].1[0]));
    }
}
