//! New releases as a grid of covers: the cover says what it is, one line under it names it, and everything
//! else waits until you point at it (the date and "Download" appear on the cover) or right-click it.
use crate::engine::ReleaseRec;
use crate::theme::{ACCENT, ACCENT2, CARD_HOVER, DIM, TEXT};
use crate::{images, slskd, util, Action, App};
use chrono::NaiveDate;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{pos2, vec2, Align, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, Ui};

/// A cover's side at its smallest, by how much room the row has: large in a roomy window, and stepping
/// down as the window narrows so that a narrow one still shows several across.
pub(crate) fn tile_base(avail: f32) -> f32 {
    if avail >= 2200.0 {
        272.0
    } else if avail >= 1600.0 {
        240.0
    } else if avail >= 1000.0 {
        208.0
    } else if avail >= 760.0 {
        184.0
    } else {
        160.0
    }
}
/// Room under a cover for its two lines: the album, then the artist.
pub(crate) const LINE: f32 = 42.0;
const GAP_MIN: f32 = 18.0;
const GAP_MAX: f32 = 34.0;
const ROW_GAP: f32 = 10.0;

/// How many covers fit across, the space between them, and how big each one is drawn so that the row reaches
/// the right edge: the covers grow a little rather than the gaps.
pub(crate) fn columns(avail: f32) -> (usize, f32, f32) {
    let cover = tile_base(avail);
    let cols = (((avail + GAP_MIN) / (cover + GAP_MIN)).floor() as usize).max(1);
    if cols == 1 {
        return (1, GAP_MIN, cover.min(avail.max(80.0)));
    }
    let side = ((avail - GAP_MAX * (cols as f32 - 1.0)) / cols as f32).floor().max(cover);
    let gap = ((avail - cols as f32 * side) / (cols as f32 - 1.0)).clamp(GAP_MIN, GAP_MAX);
    (cols, gap, side)
}
/// Where a row of `n` covers starts so that it sits in the middle of `row`, on a whole pixel. A full row
/// reaches both edges of the grid; a short one (a month with three releases) is centered under its heading.
pub(crate) fn row_left(ui: &Ui, row: Rect, n: usize, side: f32, gap: f32) -> f32 {
    let ppp = ui.ctx().pixels_per_point();
    let used = n as f32 * side + n.saturating_sub(1) as f32 * gap;
    ((row.center().x - used / 2.0) * ppp).round() / ppp
}

/// A group's name in the middle of the page, large and in the light cut of the font: "Upcoming" in coral,
/// a month bright with its year quiet beside it ("November" then "2026"). `first` is the group at the top
/// of the list, which needs less room over it.
pub(crate) fn group_head(ui: &mut Ui, name: &str, color: Color32, first: bool) {
    ui.add_space(if first { 10.0 } else { 34.0 });
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
    ui.add_space(14.0);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let font = FontId::new(42.0, crate::display_font());
    // a year at the end is the quiet part
    let (main, year) = match name.rsplit_once(' ') {
        Some((m, y)) if y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()) => (m, y),
        _ => (name, ""),
    };
    let mut job = LayoutJob::default();
    job.append(main, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
    if !year.is_empty() {
        job.append(year, 12.0, TextFormat { font_id: font, color: DIM(), ..Default::default() });
    }
    let words = ui.painter().layout_job(job);
    let at = pos2((rect.center().x - words.size().x / 2.0).round(), (rect.center().y - words.size().y / 2.0).round());
    ui.painter().galley(at, words, color);
}

/// One line: the name bright, then a quieter word or two, cut off with "…" if too long.
fn name_line(ui: &Ui, pos: egui::Pos2, width: f32, name: &str, more: &str, fade: f32) {
    let cut = |text: &str, size: f32, color: Color32, max: f32| {
        let mut job = LayoutJob::default();
        job.append(text, 0.0, TextFormat { font_id: FontId::proportional(size), color, ..Default::default() });
        job.wrap = TextWrapping { max_width: max, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        ui.painter().layout_job(job)
    };
    // stacked: the album on its own line, the artist under it, each with the tile's full width
    ui.painter().galley(pos, cut(name, 13.5, TEXT().gamma_multiply(fade), width), TEXT());
    if !more.is_empty() {
        ui.painter().galley(pos2(pos.x, pos.y + 17.0), cut(more, 12.0, DIM().gamma_multiply(fade), width), DIM());
    }
    let _ = Align::BOTTOM;
}

impl App {
    /// Draws one group of releases as rows of covers. Only rows on screen are built.
    pub(crate) fn release_tiles(&mut self, ui: &mut Ui, items: &[ReleaseRec], today: NaiveDate, actions: &mut Vec<Action>) {
        let full = ui.available_width();
        let avail = full - 12.0; // room for the scroll bar
        let (cols, gap, side) = columns(avail);
        let row_h = side + LINE;
        for row in items.chunks(cols) {
            let (row_rect, _) = ui.allocate_exact_size(vec2(full, row_h), Sense::hover());
            ui.add_space((ROW_GAP - ui.spacing().item_spacing.y).max(0.0));
            if !ui.is_rect_visible(row_rect.expand(8.0)) {
                continue;
            }
            let left = row_left(ui, row_rect, row.len(), side, gap);
            for (j, r) in row.iter().enumerate() {
                let min = pos2(left + j as f32 * (side + gap), row_rect.top());
                self.release_tile(ui, r, min, side, today, actions);
            }
        }
    }

    pub(crate) fn release_tile(&mut self, ui: &mut Ui, r: &ReleaseRec, min: egui::Pos2, side: f32, today: NaiveDate, actions: &mut Vec<Action>) {
        let muted = self.cfg.muted.contains_key(&r.artist_key);
        let hidden = muted || self.cfg.dismissed.contains(&r.key);
        let fade = if hidden { 0.45 } else { 1.0 };
        let cover = Rect::from_min_size(min, vec2(side, side));
        let rect = Rect::from_min_size(min, vec2(side, side + LINE));
        let id = ui.scope_id().with(("rel-tile", &r.key));
        let resp = ui.interact(rect, id, Sense::click());
        // (asked of the rectangle, not the response: the pointer is still "on the tile" while it is on "Download")
        // debug: `--rel-hover <text>` shows that release as if the pointer were on it (for screenshots)
        static FORCE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let forced = FORCE.get_or_init(|| crate::arg_after("--rel-hover").map(|h| h.to_lowercase())).as_ref().is_some_and(|h| r.title.to_lowercase().contains(h));
        let over = ui.rect_contains_pointer(rect) || forced;
        let lift = ui.ctx().animate_bool_with_time(id.with("lift"), over, 0.12);
        if lift > 0.0 {
            ui.painter().rect_filled(rect.expand2(vec2(7.0, 7.0)).translate(vec2(0.0, -1.0)), 10, CARD_HOVER().gamma_multiply(lift));
        }
        if over {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        // ---- the cover
        match self.images.cover(&r.art_url, &r.artist, &r.title, false) {
            images::Pic::Ready(t) => {
                egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), cover.size())).corner_radius(6).tint(Color32::WHITE.gamma_multiply(fade)).paint_at(ui, cover);
            }
            p => {
                ui.painter().rect_filled(cover, 6, crate::hue_color(&r.artist, 0.35).gamma_multiply(fade));
                let glyph = if matches!(p, images::Pic::Loading) { "…" } else { "♪" };
                ui.painter().text(cover.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(40.0), Color32::from_white_alpha(140));
            }
        }
        if lift > 0.0 {
            ui.painter().rect_stroke(cover, 6, Stroke::new(1.0, DIM().gamma_multiply(lift)), egui::StrokeKind::Outside);
        }
        // a small blue dot: this one is new since the last refresh
        if self.results.new_keys.contains(&r.key) {
            let at = pos2(cover.left() + 12.0, cover.top() + 12.0);
            ui.painter().circle_filled(at, 7.0, crate::theme::BG());
            ui.painter().circle_filled(at, 4.0, ACCENT2());
        }

        // ---- under the pointer: what and when along the cover's bottom edge, and the one main action
        // a release that is not out yet always shows its date on the cover: when is the whole point of it
        let upcoming = util::parse_date_earliest(&r.date).is_some_and(|d| d > today);
        let show = if upcoming { 1.0 } else { lift };
        let mut download = false;
        if show > 0.0 {
            let strip = Rect::from_min_max(pos2(cover.left(), cover.bottom() - 30.0), cover.max);
            ui.painter().rect_filled(strip, CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 }, Color32::from_black_alpha((215.0 * show) as u8));
            let rel = crate::relative_day(&r.date, today);
            let when = if rel.is_empty() { crate::pretty_date(&r.date) } else { rel };
            let white = Color32::WHITE.gamma_multiply(show);
            let get = ui.painter().layout_no_wrap(if upcoming { String::new() } else { "Download".into() }, FontId::proportional(12.5), ACCENT().gamma_multiply(lift));
            let get_rect = Rect::from_min_size(pos2(strip.right() - 8.0 - get.size().x, strip.center().y - get.size().y / 2.0), get.size());
            let facts_w = (get_rect.left() - strip.left() - 16.0).max(0.0);
            let facts_text = if upcoming { when.clone() } else { format!("{} · {when}", r.kind) };
            let mut job = LayoutJob::simple_singleline(facts_text, FontId::proportional(11.5), white);
            job.wrap = TextWrapping { max_width: facts_w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let facts = ui.painter().layout_job(job);
            ui.painter().galley(pos2(strip.left() + 8.0, strip.center().y - facts.size().y / 2.0), facts, white);
            let hit = ui.interact(get_rect.expand(5.0), id.with("download"), Sense::click());
            if hit.hovered() {
                ui.painter().hline(get_rect.x_range(), get_rect.bottom(), Stroke::new(1.0, ACCENT()));
            }
            ui.painter().galley(get_rect.min, get, ACCENT());
            download = hit.clicked() && !upcoming;
        }

        // ---- the one line of words
        let credit = if r.credit.is_empty() { &r.artist } else { &r.credit };
        name_line(ui, pos2(min.x + 1.0, cover.bottom() + 7.0), side - 2.0, &r.title, credit, fade);

        // ---- what clicking does
        let search = format!("{} {}", r.artist, util::strip_edition(&r.title));
        if download {
            slskd::ask_for(&search); // lists the copies on Soulseek; nothing is fetched until one is picked
        } else if resp.clicked() {
            actions.push(Action::Artist(r.artist.clone(), vec![]));
        }
        resp.context_menu(|ui| {
            if ui.button("Find on Soulseek").clicked() {
                slskd::ask_for(&search);
                ui.close();
            }
            if ui.button(format!("See {}", r.artist)).clicked() {
                actions.push(Action::Artist(r.artist.clone(), vec![]));
                ui.close();
            }
            ui.separator();
            if !r.apple_url.is_empty() && ui.button("Open in Apple Music").clicked() {
                actions.push(Action::Open(r.apple_url.clone()));
                ui.close();
            }
            if !r.mb_url.is_empty() && ui.button("Open on MusicBrainz").clicked() {
                actions.push(Action::Open(r.mb_url.clone()));
                ui.close();
            }
            if ui.button("Open on Bandcamp").clicked() {
                actions.push(Action::Bandcamp(r.artist.clone(), r.title.clone(), r.mb_url.clone()));
                ui.close();
            }
            if ui.button("Copy the name").clicked() {
                actions.push(Action::Copy(search.clone()));
                ui.close();
            }
            ui.separator();
            if muted {
                if ui.button(format!("Unmute {}", r.artist)).clicked() {
                    actions.push(Action::Unmute(r.artist_key.clone()));
                    ui.close();
                }
            } else if hidden {
                if ui.button("Show this album again").clicked() {
                    actions.push(Action::Unhide(r.key.clone()));
                    ui.close();
                }
            } else {
                if ui.button("Not interested in this album").clicked() {
                    actions.push(Action::Hide(r.key.clone()));
                    ui.close();
                }
                if ui.button(format!("Mute {} (hide all their new releases)", r.artist)).clicked() {
                    actions.push(Action::Mute(r.artist_key.clone(), r.artist.clone()));
                    ui.close();
                }
                if ui.button("Wrong artist? Fix the match").clicked() {
                    actions.push(Action::FixMatch(r.artist.clone()));
                    ui.close();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_fill_the_row() {
        for width in [400.0f32, 700.0, 940.0, 1240.0, 1500.0, 2200.0] {
            let (cols, gap, side) = columns(width);
            let used = cols as f32 * side + (cols as f32 - 1.0) * gap;
            assert!(cols >= 2 && (width - used).abs() <= cols as f32, "{width}: {cols} x {side} + gaps {gap} = {used}");
            assert!((GAP_MIN..=GAP_MAX).contains(&gap) && side >= tile_base(width) && side < tile_base(width) + 60.0, "{width}: gap {gap}, side {side}");
        }
        // roomy: five large covers; narrow: smaller ones, so there are still several across
        assert_eq!((columns(1240.0).0, tile_base(1240.0)), (5, 208.0));
        assert_eq!((columns(700.0).0, tile_base(700.0)), (4, 160.0));
        assert_eq!(columns(100.0).0, 1);
    }
}
