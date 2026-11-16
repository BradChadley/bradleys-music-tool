//! Artists as picture tiles, for the Recs page's "Artists to try" and "Your artists".
//!
//! A tile is the artist: their photo (or, for an artist you own, their album covers), a thin coral line
//! whose length says how strong the match is or how much you play them, the name, and one quiet line.
//! Clicking it opens the artist; the rest is on the right-click menu, and a few facts show on the picture
//! under the pointer.
use crate::engine::{ArtistStat, SimilarRec};
use crate::library_page::LibraryPage;
use crate::panel::short_num;
use crate::theme::{ACCENT, ACCENT2, BG, CARD_HOVER, DIM, TEXT};
use crate::{covers, images, slskd, util, Action};
use egui::text::{LayoutJob, TextWrapping};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Ui};

/// Room under a picture for the bar and the two lines.
const LINE: f32 = 46.0;
const GAP_MIN: f32 = 18.0;
const GAP_MAX: f32 = 34.0;
const ROW_GAP: f32 = 10.0;

/// "12345" -> "12,345"
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn text_line(ui: &Ui, pos: Pos2, width: f32, text: &str, size: f32, color: Color32) {
    if text.is_empty() || width <= 4.0 {
        return;
    }
    let mut job = LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
    job.wrap = TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos, galley, color);
}

/// What a tile says besides its picture.
struct Words<'a> {
    name: &'a str,
    second: &'a str,
    /// shown on the picture while the pointer is on the tile
    facts: &'a str,
    /// 0 to 1: the length of the line under the picture
    bar: f32,
    /// new since the last refresh: a blue dot
    new: bool,
    /// hidden by you: drawn faint
    faint: bool,
}

/// Lays `count` tiles out in rows that reach the right edge and calls `draw(index, top left corner, side,
/// pixels)` for the ones on screen.
fn rows(ui: &mut Ui, count: usize, settle: &mut covers::Settle, mut draw: impl FnMut(&mut Ui, usize, Pos2, f32, u32)) {
    let ppp = ui.ctx().pixels_per_point();
    let full = ui.available_width();
    let avail = full - 12.0; // room for the scroll bar
    let t = covers::fill_row(avail, ppp, crate::recs_grid::tile_base(avail), GAP_MIN, GAP_MAX);
    let px = settle.px(ui.ctx(), avail, (t.side * ppp).round() as u32);
    let row_h = t.side + LINE;
    let mut i = 0;
    while i < count {
        let (row_rect, _) = ui.allocate_exact_size(vec2(full, row_h), Sense::hover());
        ui.add_space((ROW_GAP - ui.spacing().item_spacing.y).max(0.0));
        if ui.is_rect_visible(row_rect.expand(8.0)) {
            // a full row reaches both edges; the last, shorter one sits in the middle
            let n = t.cols.min(count - i);
            let left = crate::recs_grid::row_left(ui, row_rect, n, t.side, t.gap);
            for j in 0..n {
                let min = pos2(left + j as f32 * (t.side + t.gap), row_rect.top());
                let min = pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
                draw(ui, i + j, min, t.side, px);
            }
        }
        i += t.cols;
    }
}

/// The part every artist tile shares. `picture` paints into the square it is given and says whether it
/// painted anything (if not, the artist's initial on a color of their own stands in).
fn tile(ui: &mut Ui, id: egui::Id, min: Pos2, side: f32, w: &Words, picture: impl FnOnce(&Ui, Rect, Color32) -> bool) -> egui::Response {
    let cover = Rect::from_min_size(min, vec2(side, side));
    let rect = Rect::from_min_size(min, vec2(side, side + LINE));
    let resp = ui.interact(rect, id, Sense::click());
    let lift = ui.ctx().animate_bool_with_time(id.with("lift"), resp.hovered(), 0.12);
    if lift > 0.0 {
        ui.painter().rect_filled(rect.expand2(vec2(7.0, 7.0)).translate(vec2(0.0, -1.0)), 10, CARD_HOVER().gamma_multiply(lift));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fade = if w.faint { 0.45 } else { 1.0 };
    if !picture(ui, cover, Color32::WHITE.gamma_multiply(fade)) {
        ui.painter().rect_filled(cover, 6, crate::hue_color(w.name, 0.35).gamma_multiply(fade));
        let initial: String = w.name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "♪".into());
        ui.painter().text(cover.center(), Align2::CENTER_CENTER, initial, FontId::proportional(side * 0.34), Color32::from_white_alpha(150));
    }
    if lift > 0.0 {
        ui.painter().rect_stroke(cover, 6, Stroke::new(1.0, DIM().gamma_multiply(lift)), egui::StrokeKind::Outside);
        if !w.facts.is_empty() {
            let strip = Rect::from_min_max(pos2(cover.left(), cover.bottom() - 26.0), cover.max);
            ui.painter().rect_filled(strip, CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 }, Color32::from_black_alpha((200.0 * lift) as u8));
            text_line(ui, pos2(strip.left() + 8.0, strip.top() + 6.0), strip.width() - 16.0, w.facts, 11.5, Color32::WHITE.gamma_multiply(lift));
        }
    }
    if w.new {
        let at = pos2(cover.left() + 12.0, cover.top() + 12.0);
        ui.painter().circle_filled(at, 7.0, BG());
        ui.painter().circle_filled(at, 4.0, ACCENT2());
    }
    if w.bar > 0.0 {
        let len = (side * w.bar.clamp(0.0, 1.0)).max(3.0);
        let line = Rect::from_min_size(pos2(cover.left(), cover.bottom() + 4.0), vec2(len, 2.0));
        ui.painter().rect_filled(line, 1, ACCENT().gamma_multiply(fade));
    }
    text_line(ui, pos2(min.x + 1.0, cover.bottom() + 10.0), side - 2.0, w.name, 13.5, TEXT().gamma_multiply(fade));
    text_line(ui, pos2(min.x + 1.0, cover.bottom() + 27.0), side - 2.0, w.second, 12.0, DIM().gamma_multiply(fade));
    resp
}

/// An artist's photo from online, painted into `cover`. False if there is none (yet).
fn photo(ui: &Ui, images: &mut images::Images, name: &str, cover: Rect, tint: Color32) -> bool {
    match images.artist_photo(name, false) {
        images::Pic::Ready(t) => {
            egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), cover.size())).corner_radius(6).tint(tint).paint_at(ui, cover);
            true
        }
        _ => false,
    }
}

fn web_search(site: &str, name: &str) -> String {
    match site {
        "bandcamp" => format!("https://bandcamp.com/search?q={}&item_type=b", util::urlenc(name)),
        _ => format!("https://www.youtube.com/results?search_query={}", util::urlenc(name)),
    }
}

/// "Artists to try": a photo per artist, the line is how strong the match is, and the quiet line names the
/// artists of yours they are like.
pub(crate) fn similar_tiles(ui: &mut Ui, list: &[SimilarRec], top: f64, images: &mut images::Images, settle: &mut covers::Settle, hidden: &dyn Fn(&SimilarRec) -> bool, new: &dyn Fn(&SimilarRec) -> bool, actions: &mut Vec<Action>) {
    rows(ui, list.len(), settle, |ui, i, min, side, _px| {
        let s = &list[i];
        let because: Vec<String> = s.because.iter().take(5).map(|b| b.0.clone()).collect();
        let like = match s.because.len() {
            0 => String::new(),
            _ => format!("like {}", s.because.iter().take(3).map(|b| b.0.as_str()).collect::<Vec<_>>().join(", ")),
        };
        let mut facts: Vec<String> = s.tags.iter().take(2).cloned().collect();
        if s.listeners > 0 {
            facts.push(format!("{} listeners", short_num(s.listeners)));
        }
        let is_hidden = hidden(s);
        let words = Words { name: &s.name, second: &like, facts: &facts.join(" · "), bar: (s.score / top) as f32, new: new(s), faint: is_hidden };
        let resp = tile(ui, ui.scope_id().with(("try-tile", &s.key)), min, side, &words, |ui, cover, tint| photo(ui, images, &s.name, cover, tint));
        if resp.clicked() {
            actions.push(Action::Artist(s.name.clone(), because.clone()));
        }
        resp.context_menu(|ui| {
            if ui.button("Find on Soulseek").clicked() {
                slskd::ask_for(&s.name);
                ui.close();
            }
            ui.separator();
            if !s.url.is_empty() && ui.button("Open on Last.fm").clicked() {
                actions.push(Action::Open(s.url.clone()));
                ui.close();
            }
            if ui.button("Open on Bandcamp").clicked() {
                actions.push(Action::Open(web_search("bandcamp", &s.name)));
                ui.close();
            }
            if ui.button("Open on YouTube").clicked() {
                actions.push(Action::Open(web_search("youtube", &s.name)));
                ui.close();
            }
            if ui.button("Copy the name").clicked() {
                actions.push(Action::Copy(s.name.clone()));
                ui.close();
            }
            ui.separator();
            let key = format!("artist|{}", s.key);
            if is_hidden {
                if ui.button("Show this artist again").clicked() {
                    actions.push(Action::Unhide(key));
                    ui.close();
                }
            } else if ui.button("Not interested").clicked() {
                actions.push(Action::Hide(key));
                ui.close();
            }
        });
    });
}

/// "Your artists": the covers you own of theirs (their photo if you own none), `bar` for the line under
/// the picture and `second` for the quiet line, both by whatever the list is sorted on.
pub(crate) fn artist_tiles(ui: &mut Ui, list: &[&ArtistStat], images: &mut images::Images, mut library: Option<&mut LibraryPage>, settle: &mut covers::Settle, bar: &dyn Fn(&ArtistStat) -> f32, second: &dyn Fn(&ArtistStat) -> String, actions: &mut Vec<Action>) {
    rows(ui, list.len(), settle, |ui, i, min, side, px| {
        let a = list[i];
        let facts = match (a.plays_all, a.plays_12m) {
            (0, _) => String::new(),
            (all, 0) => format!("{} plays", thousands(all)),
            (all, year) => format!("{} plays · {} this year", thousands(all), thousands(year)),
        };
        let second = second(a);
        let words = Words { name: &a.name, second: &second, facts: &facts, bar: bar(a), new: false, faint: false };
        let resp = tile(ui, ui.scope_id().with(("your-tile", &a.key)), min, side, &words, |ui, cover, tint| {
            let own = a.in_library && library.as_mut().is_some_and(|l| l.paint_artist(ui, &a.key, cover, px));
            own || photo(ui, images, &a.name, cover, tint)
        });
        if resp.clicked() {
            actions.push(Action::Artist(a.name.clone(), vec![]));
        }
        resp.context_menu(|ui| {
            if ui.button("Find on Soulseek").clicked() {
                slskd::ask_for(&a.name);
                ui.close();
            }
            ui.separator();
            if !a.url.is_empty() && ui.button("Open on Last.fm").clicked() {
                actions.push(Action::Open(a.url.clone()));
                ui.close();
            }
            if ui.button("Open on Bandcamp").clicked() {
                actions.push(Action::Open(web_search("bandcamp", &a.name)));
                ui.close();
            }
            if ui.button("Copy the name").clicked() {
                actions.push(Action::Copy(a.name.clone()));
                ui.close();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_get_commas() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
