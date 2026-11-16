//! Search everything (Ctrl+K): one box over any page. Type an artist or album and see who you have, what
//! is new from them, and the way to look on Soulseek. Up and Down choose, Enter goes, Esc closes.
use crate::theme::{self, ACCENT2, BG2, BORDER, CARD_HOVER, DIM, FAINT, GREEN, TEXT};
use crate::{images, links, slskd, util, App, Page};
use egui::{vec2, Align2, Color32, FontId, Frame, Key, Label, Margin, RichText, Sense, Stroke, TextEdit};

const WIDTH: f32 = 620.0;
const ROW: f32 = 44.0;
const PIC: f32 = 32.0;

/// One thing the search found.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Hit {
    /// an artist; `owned` = there are albums of theirs in the library
    Artist { name: String, owned: bool, albums: usize, plays: u64 },
    /// an album in the library
    Album { artist: String, title: String, folder: std::path::PathBuf },
    /// a new release from an artist you like
    Release { artist: String, title: String, art_url: String, when: String },
    /// always last: look for exactly what was typed on Soulseek
    Soulseek(String),
}

/// Every word typed must appear somewhere in the text.
fn matches(text: &str, words: &[String]) -> bool {
    let t = text.to_lowercase();
    words.iter().all(|w| t.contains(w.as_str()))
}

/// Lower is better: names that start with what was typed come first.
fn rank(name: &str, query: &str) -> u8 {
    let n = name.to_lowercase();
    if n == query {
        0
    } else if n.starts_with(query) {
        1
    } else if n.split_whitespace().any(|w| w.starts_with(query)) {
        2
    } else {
        3
    }
}

impl App {
    /// What matches the text in the box, best first, with the Soulseek line last.
    pub(crate) fn search_hits(&self, text: &str) -> Vec<Hit> {
        let query = text.trim().to_lowercase();
        if query.is_empty() {
            return vec![];
        }
        let words: Vec<String> = query.split_whitespace().map(|w| w.to_string()).collect();
        let mut artists: Vec<(u8, u64, Hit)> = self
            .results
            .artists
            .iter()
            .filter(|a| matches(&a.name, &words))
            .map(|a| (rank(&a.name, &query), a.plays_all, Hit::Artist { name: a.name.clone(), owned: a.in_library, albums: a.albums as usize, plays: a.plays_all }))
            .collect();
        for s in self.results.similar.iter().filter(|s| matches(&s.name, &words)) {
            if !artists.iter().any(|a| matches!(&a.2, Hit::Artist { name, .. } if util::artist_key(name) == s.key)) {
                artists.push((rank(&s.name, &query), 0, Hit::Artist { name: s.name.clone(), owned: false, albums: 0, plays: s.plays }));
            }
        }
        artists.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut releases: Vec<(u8, &crate::engine::ReleaseRec)> = self
            .results
            .releases
            .iter()
            .filter(|r| !self.cfg.dismissed.contains(&r.key) && !self.cfg.muted.contains_key(&r.artist_key))
            .filter(|r| matches(&format!("{} {}", r.artist, r.title), &words))
            .map(|r| (rank(&r.title, &query).min(rank(&r.artist, &query)), r))
            .collect();
        releases.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.date.cmp(&a.1.date)));
        let mut out: Vec<Hit> = artists.into_iter().take(5).map(|a| a.2).collect();
        if let Some(lib) = &self.library_page {
            out.extend(lib.find(text, 5).into_iter().map(|(artist, title, folder)| Hit::Album { artist, title, folder }));
        }
        out.extend(releases.into_iter().take(5).map(|(_, r)| Hit::Release { artist: r.artist.clone(), title: r.title.clone(), art_url: r.art_url.clone(), when: crate::pretty_date(&r.date) }));
        out.push(Hit::Soulseek(text.trim().to_string()));
        out
    }

    fn run_hit(&mut self, hit: &Hit, ctx: &egui::Context) {
        match hit {
            // an artist you own: their albums in the Library; otherwise their panel in Recs
            Hit::Artist { name, owned: true, .. } => links::go(links::Go::LibraryAlbum { artist: name.clone(), album: String::new() }),
            Hit::Artist { name, .. } => {
                self.go_to(Page::Recs);
                self.open_artist(name.clone(), vec![], ctx);
            }
            Hit::Album { artist, title, .. } => links::go(links::Go::LibraryAlbum { artist: artist.clone(), album: title.clone() }),
            Hit::Release { artist, .. } => {
                self.go_to(Page::Recs);
                self.open_artist(artist.clone(), vec![], ctx);
            }
            Hit::Soulseek(q) => slskd::ask_for(q), // lists the copies; nothing is fetched until one is picked
        }
    }

    /// Draws the search box over whatever page is showing, when it is open. Call once per frame.
    pub(crate) fn search_overlay(&mut self, ctx: &egui::Context) {
        let (open_key, esc, up, down, enter) = ctx.input(|i| (i.modifiers.command && i.key_pressed(Key::K), i.key_pressed(Key::Escape), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter)));
        if open_key {
            self.search_open = !self.search_open;
            self.search_fresh = self.search_open;
            if self.search_open {
                self.search_text.clear();
                self.search_sel = 0;
            }
        }
        if !self.search_open {
            return;
        }
        if esc {
            self.search_open = false;
            return;
        }
        let hits = self.search_hits(&self.search_text.clone());
        if !hits.is_empty() {
            if down {
                self.search_sel = (self.search_sel + 1).min(hits.len() - 1);
            }
            if up {
                self.search_sel = self.search_sel.saturating_sub(1);
            }
            self.search_sel = self.search_sel.min(hits.len() - 1);
        }
        let mut chosen: Option<usize> = if enter && !hits.is_empty() { Some(self.search_sel) } else { None };
        let mut clicked_away = false;

        egui::Area::new(egui::Id::unique("search-everything")).order(egui::Order::Foreground).anchor(Align2::CENTER_TOP, vec2(0.0, 84.0)).show(ctx, |ui| {
            // the page behind goes dim, so the box is plainly the thing being used
            let screen = ui.ctx().content_rect();
            ui.ctx().layer_painter(ui.layer_id()).rect_filled(screen, 0, Color32::from_black_alpha(150));
            let frame = Frame::new().fill(BG2()).stroke(Stroke::new(1.0, BORDER())).corner_radius(12).inner_margin(Margin::same(14)).show(ui, |ui| {
                ui.set_width(WIDTH);
                let edit = TextEdit::singleline(&mut self.search_text).hint_text("Artist or album").desired_width(WIDTH).font(FontId::proportional(18.0)).frame(Frame::new());
                let r = ui.add(edit);
                if self.search_fresh {
                    self.search_fresh = false;
                    r.request_focus();
                }
                if r.changed() {
                    self.search_sel = 0;
                }
                if hits.is_empty() {
                    return;
                }
                ui.add_space(8.0);
                ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, BORDER()));
                ui.add_space(8.0);
                for (i, hit) in hits.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(vec2(WIDTH, ROW), Sense::click());
                    if resp.hovered() && ui.input(|inp| inp.pointer.delta() != egui::Vec2::ZERO) {
                        self.search_sel = i;
                    }
                    if i == self.search_sel {
                        ui.painter().rect_filled(rect, 8, CARD_HOVER());
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        chosen = Some(i);
                    }
                    let pic = egui::Rect::from_min_size(rect.min + vec2(8.0, (ROW - PIC) / 2.0), vec2(PIC, PIC));
                    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(egui::pos2(pic.right() + 12.0, rect.top()), rect.max - vec2(10.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                    row.spacing_mut().item_spacing.x = 10.0;
                    match hit {
                        Hit::Artist { name, owned, albums, plays } => {
                            let mut at = ui.new_child(egui::UiBuilder::new().max_rect(pic));
                            crate::panel::avatar(&mut at, &mut self.images, name, PIC, true);
                            row.add(Label::new(RichText::new(name).size(15.0).color(TEXT())).selectable(false).truncate());
                            let fact = match (*owned, *albums, *plays) {
                                (true, n, _) if n > 0 => (format!("{n} in your library"), GREEN()),
                                (true, _, _) => ("in your library".to_string(), GREEN()),
                                (false, _, p) if p > 0 => (format!("{} plays, none owned", crate::panel::short_num(p)), DIM()),
                                _ => ("not in your library".to_string(), FAINT()),
                            };
                            row.add(Label::new(RichText::new(fact.0).size(12.5).color(fact.1)).selectable(false));
                        }
                        Hit::Album { artist, title, folder } => {
                            let px = (PIC * ui.ctx().pixels_per_point()).round() as u32;
                            crate::covers::paint(ui, crate::covers::for_folder(ui.ctx(), folder, px), pic, 4);
                            row.add(Label::new(RichText::new(title).size(15.0).color(TEXT())).selectable(false).truncate());
                            row.add(Label::new(RichText::new(format!("{artist}, in your library")).size(12.5).color(GREEN())).selectable(false).truncate());
                        }
                        Hit::Release { artist, title, art_url, when } => {
                            match self.images.cover(art_url, artist, title, true) {
                                images::Pic::Ready(t) => {
                                    egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), pic.size())).corner_radius(4).paint_at(ui, pic);
                                }
                                _ => {
                                    ui.painter().rect_filled(pic, 4, crate::hue_color(artist, 0.35));
                                }
                            }
                            row.add(Label::new(RichText::new(title).size(15.0).color(TEXT())).selectable(false).truncate());
                            row.add(Label::new(RichText::new(format!("new from {artist}, {when}")).size(12.5).color(ACCENT2())).selectable(false).truncate());
                        }
                        Hit::Soulseek(q) => {
                            ui.painter().circle_stroke(pic.center(), 9.0, Stroke::new(2.0, theme::ACCENT()));
                            ui.painter().line_segment([pic.center() + vec2(6.5, 6.5), pic.center() + vec2(12.0, 12.0)], Stroke::new(2.0, theme::ACCENT()));
                            row.add(Label::new(RichText::new(format!("Look for \"{q}\" on Soulseek")).size(15.0).color(TEXT())).selectable(false).truncate());
                        }
                    }
                }
            });
            // a click anywhere outside the box closes it
            if ui.input(|i| i.pointer.any_pressed()) && !ui.ctx().pointer_interact_pos().is_some_and(|p| frame.response.rect.contains(p)) {
                clicked_away = true;
            }
        });

        if let Some(i) = chosen {
            if let Some(hit) = hits.get(i).cloned() {
                self.search_open = false;
                self.run_hit(&hit, ctx);
            }
        } else if clicked_away {
            self.search_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_must_all_match_and_prefixes_win() {
        let w = |s: &str| s.split_whitespace().map(|x| x.to_lowercase()).collect::<Vec<_>>();
        assert!(matches("Arcade Fire", &w("fire arc")));
        assert!(!matches("Arcade Fire", &w("fire water")));
        assert_eq!(rank("Alex G", "alex g"), 0);
        assert_eq!(rank("Alex G", "ale"), 1);
        assert_eq!(rank("The Flaming Lips", "lip"), 2);
        assert_eq!(rank("Radiohead", "head"), 3);
    }
}
