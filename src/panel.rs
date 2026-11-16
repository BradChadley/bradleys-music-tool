//! The artist panel (opened by clicking an artist) plus keyboard shortcuts and small shared widgets.
use super::*;

impl App {
    /// The library folder of an album you own, found once and remembered (None if the library has not been
    /// read yet or the album is not in it under that name).
    pub(crate) fn owned_folder(&mut self, artist: &str, title: &str) -> Option<std::path::PathBuf> {
        let lib = self.library_page.as_ref()?;
        lib.counts()?; // not scanned yet: ask again later rather than remember a miss
        let key = format!("{}|{}", artist.to_lowercase(), title.to_lowercase());
        if let Some(found) = self.owned_folders.get(&key) {
            return found.clone();
        }
        let found = lib.find(&format!("{artist} {title}"), 1).into_iter().next().map(|hit| hit.2);
        self.owned_folders.insert(key, found.clone());
        found
    }

    pub(crate) fn shortcuts(&mut self, ctx: &egui::Context) {
        let (esc, find, refresh) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.modifiers.command && i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::F5) || (i.modifiers.command && i.key_pressed(egui::Key::R)),
            )
        });
        if esc {
            if self.detail.is_some() {
                self.detail = None;
            } else if self.show_settings {
                self.show_settings = false;
            }
        }
        if find {
            ctx.memory_mut(|m| m.request_focus(filter_id()));
        }
        if refresh && !self.running {
            self.start(ctx);
        }
    }

    pub(crate) fn detail_panel(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(v) = &self.detail else { return };
        let data = v.data.clone();
        let because = v.because.clone();
        // the name we were opened with matches the release/library keys (Last.fm may restyle it, e.g. "Charli xcx")
        let v_name = v.name.clone();
        let name = data.as_ref().map(|d| d.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| v.name.clone());
        let mut close = false;
        let mut pin_search: Option<String> = None;

        ui.horizontal(|ui| {
            avatar(ui, &mut self.images, &name, 56.0, true);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.set_width((ui.available_width() - 40.0).max(100.0));
                ui.add(Label::new(RichText::new(&name).size(22.0).strong().color(TEXT())).truncate());
                let mut meta = vec![];
                if let Some(d) = &data {
                    if d.listeners > 0 {
                        meta.push(format!("{} listeners on Last.fm", short_num(d.listeners)));
                    }
                }
                if let Some(a) = self.results.artists.iter().find(|a| a.key == util::artist_key(&name)) {
                    if a.in_library {
                        meta.push(format!("you have {} albums", a.albums));
                    }
                    if a.plays_all > 0 {
                        meta.push(format!("{} of your plays", a.plays_all));
                    }
                }
                if !meta.is_empty() {
                    ui.label(RichText::new(meta.join("  ·  ")).color(DIM()));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if ui.add(Button::new(RichText::new("✕").size(16.0)).frame(false)).on_hover_text("Close (Esc)").clicked() {
                    close = true;
                }
            });
        });
        if let Some(d) = &data {
            if !d.tags.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for t in &d.tags {
                        chip(ui, t, ACCENT2());
                    }
                });
            }
        }
        if !because.is_empty() {
            ui.add_space(4.0);
            let names: Vec<&str> = because.iter().take(3).map(|s| s.as_str()).collect();
            ui.label(RichText::new(format!("Because you like {}", join_names(&names))).color(DIM()));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if let Some(url) = data.as_ref().map(|d| d.url.clone()).filter(|u| !u.is_empty()) {
                if link_btn(ui, "Last.fm").clicked() {
                    actions.push(Action::Open(url));
                }
            }
            if link_btn(ui, "Bandcamp").clicked() {
                actions.push(Action::Open(format!("https://bandcamp.com/search?q={}&item_type=b", util::urlenc(&name))));
            }
            if link_btn(ui, "YouTube").clicked() {
                actions.push(Action::Open(format!("https://www.youtube.com/results?search_query={}", util::urlenc(&name))));
            }
            if link_btn(ui, "Copy name").clicked() {
                actions.push(Action::Copy(name.clone()));
            }
            let key = util::artist_key(&v_name);
            if self.pin_ui.is_none() && !self.cfg.pins.contains_key(&key) && link_btn(ui, "Wrong artist?").on_hover_text("Pick which MusicBrainz artist this is").clicked() {
                pin_search = Some(v_name.clone());
            }
            if self.cfg.muted.contains_key(&key) {
                if link_btn(ui, "Unmute").on_hover_text("Show their new releases again").clicked() {
                    actions.push(Action::Unmute(key));
                }
            } else if link_btn(ui, "Mute").on_hover_text("Hide their new releases").clicked() {
                actions.push(Action::Mute(key, v_name.clone()));
            }
        });
        // pinned: say so, with a way back
        if let Some(p) = self.cfg.pins.get(&util::artist_key(&v_name)).cloned() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let what = if p.mbid.is_empty() { "not using MusicBrainz for this artist".to_string() } else { format!("Pinned to {}", p.label) };
                chip(ui, &what, ACCENT2());
                if ui.small_button("Change").clicked() {
                    pin_search = Some(v_name.clone());
                }
                if ui.small_button("Unpin").clicked() {
                    actions.push(Action::Unpin(util::artist_key(&v_name)));
                }
            });
        }
        ui.add_space(6.0);
        ui.separator();
        if let Some(q) = pin_search.take() {
            let ctx = ui.ctx().clone();
            self.start_pin_search(&v_name, q, &ctx);
        }
        if self.pin_ui.is_some() {
            let current = data.as_ref().map(|d| d.mbid.clone()).unwrap_or_default();
            ScrollArea::vertical().id_salt("pinpicker").max_height(ui.available_height() * 0.8).show(ui, |ui| {
                self.pin_picker(ui, &v_name, &current, actions);
            });
            ui.separator();
        }

        let Some(d) = data else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.label(RichText::new("Finding their best albums…").color(DIM()));
            });
            if close {
                self.detail = None;
            }
            return;
        };

        let auto = self.auto;
        ScrollArea::vertical().auto_shrink([false, false]).id_salt(("detail", &name)).show(ui, |ui| {
            crate::auto_scroll(ui, auto);
            if !d.bio.is_empty() {
                egui::CollapsingHeader::new(RichText::new("About").color(DIM())).id_salt(("bio", &name)).default_open(false).show(ui, |ui| {
                    ui.label(RichText::new(&d.bio).color(DIM()));
                });
                ui.add_space(4.0);
            }
            if !d.note.is_empty() {
                ui.label(RichText::new(&d.note).color(DIM()));
            }
            // newest album/EP, pinned so it's never buried by older favourites
            let latest = d.albums.iter().chain(d.other.iter()).filter(|a| !a.date.is_empty()).max_by(|a, b| a.date.cmp(&b.date)).cloned();
            if let Some(l) = &latest {
                let today = Local::now().date_naive();
                let is_recent = util::parse_date_latest(&l.date)
                    .map(|dt| dt >= today.checked_sub_months(chrono::Months::new(self.cfg.months_back)).unwrap_or(today))
                    .unwrap_or(false);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Latest release").size(15.0).strong().color(TEXT()));
                    if is_recent {
                        chip(ui, "New", ACCENT2());
                    }
                    ui.label(RichText::new(pretty_date(&l.date)).small().color(DIM()));
                });
                ui.add_space(4.0);
                self.album_row(ui, None, l, None, false, &name, actions);
                ui.add_space(6.0);
            }
            if !d.albums.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Albums").size(17.0).strong().color(TEXT()));
                    ui.add_space(8.0);
                    crate::theme::choice(ui, &mut self.album_sort, 0, "Best")
                        .on_hover_text("Plays adjusted for how long each album has been out, so new albums aren't buried");
                    crate::theme::choice(ui, &mut self.album_sort, 1, "Most played").on_hover_text("All-time Last.fm plays");
                    crate::theme::choice(ui, &mut self.album_sort, 2, "Newest");
                });
                let owned = d.albums.iter().filter(|a| a.owned).count();
                let mut sub = match self.album_sort {
                    0 => "Ranked by Last.fm plays, adjusted for how long each album has been out.",
                    1 => "Ranked by all-time Last.fm plays (favours older albums).",
                    _ => "Newest first.",
                }
                .to_string();
                if owned > 0 {
                    sub.push_str(&format!(" You have {owned} of these."));
                }
                ui.label(RichText::new(sub).small().color(DIM()));
                ui.add_space(6.0);
                // "Start here" always follows the Best ranking (d.albums is already in that order)
                let start_title = d.albums.iter().find(|a| !a.owned && !self.cfg.dismissed.contains(&engine::rel_key(&util::artist_key(&name), &a.title))).map(|a| a.title.clone());
                let mut list = d.albums.clone();
                match self.album_sort {
                    1 => list.sort_by(|a, b| b.plays.cmp(&a.plays)),
                    2 => list.sort_by(|a, b| b.date.cmp(&a.date)),
                    _ => {}
                }
                let max_score = list.iter().map(|a| a.score).fold(0.0, f64::max).max(1e-9);
                let max_plays = list.iter().map(|a| a.plays).max().unwrap_or(1).max(1) as f64;
                for (i, a) in list.iter().enumerate() {
                    let bar = match self.album_sort {
                        0 => (a.score / max_score) as f32,
                        _ => (a.plays as f64 / max_plays) as f32,
                    };
                    let rank = if self.album_sort == 2 { None } else { Some(i + 1) };
                    self.album_row(ui, rank, a, Some(bar), start_title.as_deref() == Some(a.title.as_str()), &name, actions);
                }
            }
            if !d.other.is_empty() {
                ui.add_space(8.0);
                let title = if d.albums.is_empty() { "Albums" } else { "Rest of the discography" };
                egui::CollapsingHeader::new(RichText::new(format!("{title} ({})", d.other.len())).strong())
                    .id_salt(("other", &name))
                    .default_open(d.albums.is_empty())
                    .show(ui, |ui| {
                        for a in &d.other {
                            self.album_row(ui, None, a, None, false, &name, actions);
                        }
                    });
            }
            ui.add_space(20.0);
        });
        if close {
            self.detail = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn album_row(&mut self, ui: &mut Ui, rank: Option<usize>, a: &engine::DetailAlbum, bar: Option<f32>, start_here: bool, artist: &str, actions: &mut Vec<Action>) {
        let art = 68.0;
        // the same key the New releases list uses, so "Not interested" here hides it there too
        let rkey = engine::rel_key(&util::artist_key(artist), &a.title);
        if crate::carry_hidden(&mut self.cfg.dismissed, &util::artist_key(artist), &a.title) {
            self.cfg.save();
        }
        let not_interested = self.cfg.dismissed.contains(&rkey);
        Frame::new().fill(CARD()).corner_radius(10).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if not_interested {
                ui.set_opacity(0.5);
            }
            ui.horizontal(|ui| {
                if let Some(r) = rank {
                    let c = if r <= 3 { ACCENT() } else { DIM() };
                    ui.add_sized([24.0, art], Label::new(RichText::new(r.to_string()).size(17.0).strong().color(c)));
                }
                let (rect, _) = ui.allocate_exact_size(vec2(art, art), Sense::hover());
                if ui.is_rect_visible(rect) {
                    // an album you own shows the cover from its own folder: no lookup, nothing fetched
                    let local = if a.owned { self.owned_folder(artist, &a.title) } else { None };
                    if let Some(folder) = local {
                        let px = (art * ui.ctx().pixels_per_point()).round() as u32;
                        crate::covers::paint(ui, crate::covers::for_folder(ui.ctx(), &folder, px), rect, 6);
                    } else {
                    match self.images.cover(&a.art, artist, &a.title, true) {
                        crate::images::Pic::Ready(t) => {
                            egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), vec2(art, art))).corner_radius(6).paint_at(ui, rect);
                        }
                        p => {
                            ui.painter().rect_filled(rect, 6, hue_color(&a.title, 0.3));
                            let glyph = if matches!(p, crate::images::Pic::Loading) { "…" } else { "♪" };
                            ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(24.0), Color32::from_white_alpha(130));
                        }
                    }
                    }
                }
                ui.add_space(2.0);
                let w = ui.available_width();
                ui.vertical(|ui| {
                    ui.set_width(w);
                    if artist_link(ui, &a.title, 15.5, TEXT()).on_hover_text("Open the album's Wikipedia page (or Bandcamp if it has none)").clicked() {
                        actions.push(Action::Album(artist.to_string(), a.title.clone(), a.url.clone(), mb_link(a)));
                    }
                    ui.horizontal(|ui| {
                        let mut meta = vec![];
                        if !a.year.is_empty() {
                            meta.push(a.year.clone());
                        }
                        if !a.kind.is_empty() && a.kind != "Album" {
                            meta.push(a.kind.clone());
                        }
                        if a.plays > 0 {
                            meta.push(format!("{} plays", short_num(a.plays)));
                        }
                        if !meta.is_empty() {
                            ui.label(RichText::new(meta.join(" · ")).size(12.5).color(DIM()));
                        }
                        if let Some(frac) = bar.filter(|_| a.plays > 0) {
                            ui.add(ProgressBar::new(frac).desired_width(60.0).desired_height(5.0).fill(ACCENT().gamma_multiply(0.8)));
                        }
                        if a.owned {
                            chip(ui, "In your library", GREEN());
                        } else if start_here {
                            chip(ui, "Start here", ACCENT());
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let q = format!("{artist} {}", a.title);
                        if link_btn(ui, "YouTube").on_hover_text("Search for the full album").clicked() {
                            actions.push(Action::Open(format!("https://www.youtube.com/results?search_query={}", util::urlenc(&format!("{q} full album")))));
                        }
                        if link_btn(ui, "Bandcamp").clicked() {
                            actions.push(Action::Bandcamp(artist.to_string(), a.title.clone(), mb_link(a)));
                        }
                        if link_btn(ui, "Apple Music").clicked() {
                            actions.push(Action::Open(format!("https://music.apple.com/{}/search?term={}", util::store_country().to_ascii_lowercase(), util::urlenc(&q))));
                        }
                        if link_btn(ui, "Copy").on_hover_text(format!("Copy \"{q}\" (for a Soulseek search)")).clicked() {
                            actions.push(Action::Copy(q));
                        }
                        if not_interested {
                            if link_btn(ui, "Undo not interested").clicked() {
                                actions.push(Action::Unhide(rkey.clone()));
                            }
                        } else if !a.owned && link_btn(ui, "Not interested").on_hover_text("Hide this album (it's hidden from New releases too, and won't be picked as Start here)").clicked() {
                            actions.push(Action::Hide(rkey.clone()));
                        }
                    });
                });
            });
        });
        ui.add_space(6.0);
    }
}

impl App {
    /// "Which artist is this?": MusicBrainz artists with this name, the one matching your folders first.
    fn pin_picker(&mut self, ui: &mut Ui, artist: &str, current: &str, actions: &mut Vec<Action>) {
        let key = util::artist_key(artist);
        let mut search = None;
        let mut cancel = false;
        Frame::new().fill(CARD()).corner_radius(10).inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Which artist is this?").size(15.0).strong().color(TEXT()));
            ui.label(
                RichText::new(format!("Pick the MusicBrainz artist that is your \"{artist}\". New releases and albums will come from that one."))
                    .small()
                    .color(DIM()),
            );
            ui.add_space(4.0);
            let Some(p) = &mut self.pin_ui else { return };
            ui.horizontal(|ui| {
                let r = ui.add(TextEdit::singleline(&mut p.query).desired_width(220.0).hint_text("Search name"));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Search").clicked() || enter {
                    search = Some(p.query.clone());
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            if p.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Looking up matches (about a second each)…").color(DIM()));
                });
            }
            match &p.results {
                Some(Err(e)) => {
                    ui.label(RichText::new(crate::util::plain_err(format!("Couldn't search MusicBrainz: {e}"))).color(ACCENT()));
                }
                Some(Ok(list)) if list.is_empty() => {
                    ui.label(RichText::new("No artists found. Try a different spelling.").color(DIM()));
                }
                Some(Ok(list)) => {
                    for c in list.clone() {
                        ui.add_space(4.0);
                        Frame::new().fill(BG2()).corner_radius(8).inner_margin(Margin::same(8)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(&c.c.name).strong().color(TEXT()));
                                if !c.c.disambiguation.is_empty() {
                                    ui.label(RichText::new(format!("({})", c.c.disambiguation)).color(DIM()));
                                }
                            });
                            ui.horizontal_wrapped(|ui| {
                                let mut meta = vec![];
                                for m in [&c.c.kind, &c.c.area] {
                                    if !m.is_empty() {
                                        meta.push(m.clone());
                                    }
                                }
                                if !c.c.begin.is_empty() {
                                    meta.push(format!("since {}", c.c.begin));
                                }
                                meta.push(format!("{}{} releases", c.releases, if c.releases >= 100 { "+" } else { "" }));
                                ui.label(RichText::new(meta.join(" · ")).small().color(DIM()));
                                if c.owned_hits > 0 {
                                    chip(ui, &format!("matches {} of your albums", c.owned_hits), GREEN());
                                }
                                if c.c.id == current {
                                    chip(ui, "currently used", ACCENT2());
                                }
                            });
                            if !c.titles.is_empty() {
                                ui.add(Label::new(RichText::new(c.titles.join(" · ")).small().color(DIM())).wrap());
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                let use_this = crate::theme::main_action(ui, "Use this", true);
                                ui.add_space(10.0);
                                if use_this.clicked() {
                                    let label = if c.c.disambiguation.is_empty() { c.c.name.clone() } else { format!("{} ({})", c.c.name, c.c.disambiguation) };
                                    actions.push(Action::Pin(key.clone(), config::Pin { artist: artist.to_string(), mbid: c.c.id.clone(), label }));
                                }
                                if link_btn(ui, "MusicBrainz").clicked() {
                                    actions.push(Action::Open(format!("https://musicbrainz.org/artist/{}", c.c.id)));
                                }
                            });
                        });
                    }
                    ui.add_space(6.0);
                    if ui
                        .button("None of these: don't use MusicBrainz for this artist")
                        .on_hover_text("New releases will come from Apple Music only, and albums from Last.fm")
                        .clicked()
                    {
                        actions.push(Action::Pin(key.clone(), config::Pin { artist: artist.to_string(), mbid: String::new(), label: String::new() }));
                    }
                }
                None => {}
            }
        });
        if cancel {
            self.pin_ui = None;
        }
        if let Some(q) = search {
            let ctx = ui.ctx().clone();
            self.start_pin_search(artist, q, &ctx);
        }
    }
}

pub(crate) fn filter_id() -> egui::Id {
    egui::Id::unique("filter")
}

/// Clickable text that underlines on hover.
pub(crate) fn artist_link(ui: &mut Ui, text: &str, size: f32, color: Color32) -> egui::Response {
    let mut rt = RichText::new(text).size(size).color(color);
    if size >= 15.0 {
        rt = rt.strong();
    }
    let r = ui.add(Label::new(rt).truncate().sense(Sense::click()));
    if r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(1.0, color));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// The album's MusicBrainz link (its page, or the Cover Art Archive link holding the same id), or "".
fn mb_link(a: &engine::DetailAlbum) -> String {
    [&a.url, &a.art].into_iter().find(|l| crate::albumlink::mb_release_group(l).is_some()).cloned().unwrap_or_default()
}

/// The artist's photo in a circle; their initial on a coloured circle while it loads or when there's none.
pub(crate) fn avatar(ui: &mut Ui, images: &mut crate::images::Images, name: &str, d: f32, prio: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(d, d), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return; // (no photo lookups for cards scrolled out of view)
    }
    if let crate::images::Pic::Ready(t) = images.artist_photo(name, prio) {
        egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), vec2(d, d))).corner_radius(d / 2.0).paint_at(ui, rect);
        return;
    }
    ui.painter().circle_filled(rect.center(), d / 2.0, hue_color(name, 0.55));
    let initial: String = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_default();
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, initial, FontId::proportional(d * 0.46), Color32::WHITE);
}

pub(crate) fn short_num(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.0}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}
