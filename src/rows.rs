//! The parts every setting is made of (Settings, and the first-start screen), so that each kind of choice
//! looks and works the same wherever it is:
//!   `row`     what the setting is at the left, in plain words (a faint hint under it only where one is
//!             needed), and its control at the right, so the eye runs down one edge
//!   `mark`    on or off: one round mark, a coral check in a ring when on, an empty ring when off
//!   `pick_one`, `pick_many`: named choices. The row shows the chosen one with a small arrow; a click
//!             opens a menu under it, each choice with one short line of what it does, the chosen one
//!             with a coral check. A menu of one choice closes on a pick; one where several can be on
//!             stays open while they are ticked. A click outside or Escape closes either.
//!   `there`   a helper that is on this PC: a green check, nothing to read
//! Whatever is chosen is marked in coral the same way everywhere.
use crate::script;
use crate::theme::{self, ACCENT, DIM, FAINT, GREEN, TEXT};
use egui::{pos2, vec2, Align, Color32, FontId, Layout, Rect, Response, Sense, Stroke, Ui, UiBuilder};

/// The height a row's control is centered in, and the room over and under a row.
const CONTROL_H: f32 = 30.0;
const ROW_PAD: f32 = 7.0;
/// The room between a row's words and its control.
const BETWEEN: f32 = 18.0;

/// One setting as a row: `label` (and `hint` under it, faint) at the left, what `control` draws at the
/// right, in a layout laid out from the right (the first thing added stands at the right end). The
/// control's width is remembered from the frame before under `id`, so the words wrap before it.
pub(crate) fn row<R>(ui: &mut Ui, id: &str, label: &str, hint: Option<&str>, control: impl FnOnce(&mut Ui) -> R) -> R {
    let full = ui.available_width();
    let key = egui::Id::unique(("settings-row", id));
    let known: f32 = ui.ctx().data(|d| d.get_temp(key)).unwrap_or(150.0);
    let text_w = (full - known - BETWEEN).max(120.0);
    let p = ui.painter();
    let words = p.layout(label.to_string(), FontId::proportional(14.0), TEXT(), text_w);
    let under = hint.map(|h| p.layout(h.to_string(), FontId::proportional(12.5), FAINT(), text_w));
    let text_h = words.size().y + under.as_ref().map_or(0.0, |u| 1.0 + u.size().y);
    let h = (text_h.max(CONTROL_H) + 2.0 * ROW_PAD).round();
    let (rect, _) = ui.allocate_exact_size(vec2(full, h), Sense::hover());
    let top = (rect.center().y - text_h / 2.0).round();
    let words_h = words.size().y;
    ui.painter().galley(pos2(rect.left(), top), words, TEXT());
    if let Some(u) = under {
        ui.painter().galley(pos2(rect.left(), top + words_h + 1.0), u, FAINT());
    }
    let band = Rect::from_center_size(pos2(rect.center().x, rect.center().y), vec2(full, CONTROL_H));
    let mut c = ui.new_child(UiBuilder::new().id_salt(("row-control", id)).max_rect(band).layout(Layout::right_to_left(Align::Center)));
    c.spacing_mut().item_spacing.x = 8.0;
    let out = control(&mut c);
    let used = (band.right() - c.min_rect().left()).max(0.0);
    if (used - known).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(key, used));
        ui.ctx().request_repaint();
    }
    out
}

/// Words at a row's right end that are not a control ("days", "times or more").
pub(crate) fn unit(ui: &mut Ui, words: &str) {
    ui.add(egui::Label::new(egui::RichText::new(words).size(13.5).color(DIM())).selectable(false));
}

/// On or off, one round mark: a coral check in a ring when on, an empty ring when off. A click flips it;
/// true when it did. `name` is what a test script clicks.
pub(crate) fn mark(ui: &mut Ui, name: &str, on: &mut bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
    script::seen(name, rect);
    let c = rect.center();
    let p = ui.painter();
    if *on {
        p.circle_filled(c, 9.5, ACCENT().gamma_multiply(if r.hovered() { 0.24 } else { 0.16 }));
        p.circle_stroke(c, 9.5, Stroke::new(1.5, ACCENT()));
        check(p, c, 1.0, ACCENT());
    } else {
        if r.hovered() {
            p.circle_filled(c, 9.5, theme::CARD_HOVER());
        }
        p.circle_stroke(c, 9.5, Stroke::new(1.5, if r.hovered() { DIM() } else { FAINT() }));
    }
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
    if r.clicked() {
        *on = !*on;
        return true;
    }
    false
}

/// A check stroke around `c`, `scale` times its usual size.
fn check(p: &egui::Painter, c: egui::Pos2, scale: f32, color: Color32) {
    let pts = [c + vec2(-3.9, 0.2) * scale, c + vec2(-1.2, 2.9) * scale, c + vec2(4.0, -2.7) * scale];
    p.line_segment([pts[0], pts[1]], Stroke::new(1.8 * scale, color));
    p.line_segment([pts[1], pts[2]], Stroke::new(1.8 * scale, color));
}

/// A helper that is on this PC: a green check in a soft green ring.
pub(crate) fn there(ui: &mut Ui, name: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    script::seen(name, rect);
    ui.painter().circle_filled(rect.center(), 10.0, GREEN().gamma_multiply(0.16));
    check(ui.painter(), rect.center(), 0.9, GREEN());
}

/// One choice in a menu: its name, and one short line of what it does (empty: none).
pub(crate) struct Item<'a> {
    pub name: &'a str,
    pub about: &'a str,
}

/// The height of a box at a row's end (a menu's opener, a number), so a card's controls stand level.
pub(crate) const BOX_H: f32 = 26.0;
/// How round a box's corners are.
pub(crate) const BOX_ROUND: u8 = 6;

/// A box's fill: a soft field, a little darker than the card it stands on (worked out from the colors in
/// use, so every theme gets its own).
pub(crate) fn box_fill() -> Color32 {
    theme::BG().lerp_to_gamma(theme::CARD(), 0.45)
}

/// A box's hairline edge: faint at rest, a step brighter under the pointer, brighter again while its menu
/// is open or it is being typed in.
pub(crate) fn box_edge(hot: bool, open: bool) -> Color32 {
    if open {
        theme::BORDER().lerp_to_gamma(FAINT(), 0.5)
    } else if hot {
        theme::BORDER().lerp_to_gamma(FAINT(), 0.3)
    } else {
        theme::CARD().lerp_to_gamma(theme::BORDER(), 0.5)
    }
}

/// The chosen value in a box with a small arrow: what a menu opens from. A box at rest too, so it reads
/// as one; the same fill, edge and height as the number boxes, so a card's right edge is one column.
fn opener(ui: &mut Ui, name: &str, shown: &str) -> Response {
    let font = FontId::proportional(13.5);
    let words = ui.painter().layout_no_wrap(shown.to_string(), font, TEXT());
    let size = vec2(11.0 + words.size().x + 10.0 + 8.0 + 9.0, BOX_H);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    script::seen(name, rect);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&r));
    let hot = r.hovered();
    let p = ui.painter();
    p.rect_filled(rect, BOX_ROUND, if hot || open { box_fill().lerp_to_gamma(theme::CARD(), 0.25) } else { box_fill() });
    p.rect_stroke(rect, BOX_ROUND, Stroke::new(1.0, box_edge(hot, open)), egui::StrokeKind::Inside);
    let color = if hot || open { TEXT() } else { TEXT().lerp_to_gamma(DIM(), 0.15) };
    p.galley_with_override_text_color(pos2(rect.left() + 11.0, (rect.center().y - words.size().y / 2.0).round()), words, color);
    // the arrow: small and faint, down when shut, up while open
    let a = pos2(rect.right() - 13.0, rect.center().y);
    let (w, h) = (2.8, if open { -1.5 } else { 1.5 });
    let stroke = Stroke::new(1.2, if hot || open { DIM() } else { FAINT() });
    p.line_segment([a + vec2(-w, -h), a + vec2(0.0, h)], stroke);
    p.line_segment([a + vec2(0.0, h), a + vec2(w, -h)], stroke);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The menu under `opener`: each item a row with the coral check where `is_on` says, its name, and its
/// line of what it does. Gives back the item clicked.
fn menu(opener: &Response, name: &str, items: &[Item], is_on: &dyn Fn(usize) -> bool, many: bool) -> Option<usize> {
    let ctx = opener.ctx.clone();
    // (as wide as its longest line wants, within reason)
    let width = ctx.fonts_mut(|f| {
        items
            .iter()
            .map(|it| {
                let n = f.layout_no_wrap(it.name.to_string(), FontId::proportional(14.0), TEXT()).size().x;
                let a = f.layout_no_wrap(it.about.to_string(), FontId::proportional(12.5), FAINT()).size().x;
                n.max(a) + 52.0
            })
            .fold(260.0, f32::max)
            .min(380.0)
    });
    let frame = egui::Frame::new()
        .fill(theme::CARD_HOVER())
        .stroke(Stroke::new(1.0, theme::BORDER()))
        .corner_radius(10)
        .inner_margin(6)
        .shadow(egui::epaint::Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(150) });
    let popup = egui::Popup::from_toggle_button_response(opener)
        .align(egui::RectAlign::BOTTOM_END)
        .align_alternatives(&[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START])
        .gap(4.0)
        .frame(frame)
        .width(width)
        .close_behavior(if many { egui::PopupCloseBehavior::CloseOnClickOutside } else { egui::PopupCloseBehavior::CloseOnClick });
    popup
        .show(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut picked = None;
            for (n, it) in items.iter().enumerate() {
                let w = ui.available_width();
                let text_w = w - 40.0;
                let p = ui.painter();
                let title = p.layout_no_wrap(it.name.to_string(), FontId::proportional(14.0), TEXT());
                let about = (!it.about.is_empty()).then(|| p.layout(it.about.to_string(), FontId::proportional(12.5), FAINT(), text_w));
                let h = 8.0 + title.size().y + about.as_ref().map_or(0.0, |a| 1.0 + a.size().y) + 8.0;
                let (rect, r) = ui.allocate_exact_size(vec2(w, h), Sense::click());
                script::seen(&format!("{name}:{}", it.name), rect);
                let p = ui.painter();
                if r.hovered() {
                    p.rect_filled(rect, 7, theme::CARD_HOVER().lerp_to_gamma(TEXT(), 0.07));
                }
                let top = rect.top() + 8.0;
                if is_on(n) {
                    check(p, pos2(rect.left() + 16.0, top + title.size().y / 2.0), 0.95, ACCENT());
                }
                let title_h = title.size().y;
                p.galley(pos2(rect.left() + 32.0, top), title, TEXT());
                if let Some(a) = about {
                    p.galley(pos2(rect.left() + 32.0, top + title_h + 1.0), a, FAINT());
                }
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    picked = Some(n);
                }
            }
            picked
        })
        .and_then(|r| r.inner)
}

/// Named choices, one of which is chosen: the chosen one's name with the arrow, and the menu. Gives back
/// the choice clicked when it is another than `chosen`. `name` is what a test script clicks
/// (`name:<choice>` for a choice in the open menu).
pub(crate) fn pick_one(ui: &mut Ui, name: &str, items: &[Item], chosen: usize) -> Option<usize> {
    let shown = items.get(chosen).map_or("", |it| it.name);
    let r = opener(ui, name, shown);
    menu(&r, name, items, &|n| n == chosen, false).filter(|n| *n != chosen)
}

/// Choices of which any can be on: `shown` (a few words for what is on) with the arrow, and the menu,
/// which stays open while they are ticked. True when one was flipped.
pub(crate) fn pick_many(ui: &mut Ui, name: &str, shown: &str, items: &[Item], on: &mut [bool]) -> bool {
    let r = opener(ui, name, shown);
    let now: Vec<bool> = on.to_vec();
    match menu(&r, name, items, &|n| now.get(n).copied().unwrap_or(false), true) {
        Some(n) if n < on.len() => {
            on[n] = !on[n];
            true
        }
        _ => false,
    }
}

/// A few words for which of `names` are on: all of them, none, up to three by name, or how many.
pub(crate) fn summary(names: &[&str], on: &[bool], all: &str) -> String {
    let picked: Vec<&str> = names.iter().zip(on).filter(|(_, o)| **o).map(|(n, _)| *n).collect();
    match picked.len() {
        0 => "None".to_string(),
        n if n == names.len() => all.to_string(),
        1..=3 => picked.join(", "),
        n => format!("{n} of {}", names.len()),
    }
}

/// Notes, as a frame ends, whether a menu (or any popup) is open.
pub(crate) fn note_popups(ctx: &egui::Context) {
    let open = ctx.any_popup_open();
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique("popup-at-frame-end"), open));
}

/// True while a menu is open, or was open as the frame before ended. Escape closes a menu before the
/// frame that carries the key is drawn, so by then no menu is open: a page that closes on Escape asks
/// this, and the Escape that only closed a menu does not close the page as well.
pub(crate) fn popup_until_now(ctx: &egui::Context) -> bool {
    ctx.any_popup_open() || ctx.data(|d| d.get_temp::<bool>(egui::Id::unique("popup-at-frame-end"))).unwrap_or(false)
}
#[cfg(test)]
mod tests {
    #[test]
    fn what_is_on_is_said_in_a_few_words() {
        let names = ["Albums", "EPs", "Singles", "Live and compilations", "Features"];
        assert_eq!(super::summary(&names, &[true, true, true, false, false], "All kinds"), "Albums, EPs, Singles");
        assert_eq!(super::summary(&names, &[true; 5], "All kinds"), "All kinds");
        assert_eq!(super::summary(&names, &[false; 5], "All kinds"), "None");
        assert_eq!(super::summary(&names, &[true, true, true, true, false], "All kinds"), "4 of 5");
        assert_eq!(super::summary(&names, &[false, true, false, false, false], "All kinds"), "EPs");
    }
}
