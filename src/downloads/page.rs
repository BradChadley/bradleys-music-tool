//! How the Downloads page is laid out.
//!
//! Top to bottom, in one centered stretch of the window (as on the Backup and Home pages):
//!   - the search box, the page's centerpiece, with one faint line under it for the Soulseek account (and
//!     for anything that is wrong with it, with the word that fixes it);
//!   - the searches that are kept, as quiet words under the box, after the word that shows the import list;
//!   - a card for what is arriving, only while something is;
//!   - the main card: the copies a search found (here), or the import list (drawn by the importer).
//!
//! The list of copies says what differs and keeps the rest quiet: "Download" is loud on the best match
//! only and shows on another row when the pointer or the arrow keys are on it, a wait is written only
//! when there is one, and a format is colored only when it is a weak one.
//!
//! Nothing here is worked out more often than it changes: the order of the copies, the words in their
//! cells and the album over them are put together when the search, the filters or the library's answer
//! change (`Listing`, `Head`), and only the rows on screen are drawn.
use super::*;
use egui::text::{LayoutJob, TextFormat};
use egui::{pos2, Align2, FontId, Id, Rect, Stroke, UiBuilder};
use std::borrow::Cow;
use std::fmt::Write as _;

/// The page at its widest. Wider than Backup's: the import list and its side card stand side by side.
pub const PAGE: f32 = 1400.0;
/// The room inside a card's edges, and between two cards.
const PAD: egui::Vec2 = vec2(24.0, 20.0);
const GAP: f32 = 16.0;
/// A copy's row, a track's line under an opened row, and the album over the list.
const ROW: f32 = 30.0;
const FILE_ROW: f32 = 21.0;
const HEAD: f32 = 72.0;
/// A row of what is arriving.
const ARRIVING_ROW: f32 = 30.0;

/// Sentences the page shows every frame, put together when what they say changes.
#[derive(Default)]
pub(super) struct Words {
    name: Option<String>,
    signin: String,
    /// how many albums are ready, and the word that shows the import list
    ready: Option<usize>,
    import: String,
    /// how many arrived while a search was on show, and the words for it
    new: usize,
    new_words: String,
    /// a number being written (how many copies a kept search found)
    count: String,
}

/// One copy as it is listed: where it is among the search's copies, and the words in its cells.
pub(super) struct Line {
    at: usize,
    key: (String, String),
    /// edition words in the folder's name, shown when the name is cut short
    marks: Vec<String>,
    tracks: String,
    short: bool,
    size: String,
    speed: String,
    wait: String,
    /// a weak format (192 and under), and a clear step up from the library's copy
    weak: bool,
    better: bool,
    /// the track that holds a word typed in the filter box, when nothing else in the row shows it
    why: String,
}

/// The open search's copies as they are listed. Put together again when the copies, the order asked
/// for, a filter, the album or a download's state changes, and when the pointer leaves the list.
#[derive(Default)]
pub(super) struct Listing {
    for_text: String,
    stamp: Option<(u64, Sort, Kind, bool, u64, u64)>,
    lines: Vec<Line>,
    /// what is typed in the filter box (set before the list is put together), and how many copies the
    /// format choices alone would list
    pub(super) typed: String,
    before_typed: usize,
    best: Option<(String, String)>,
    /// the order the copies are shown in (see `settle`), and what it is the order of
    order: Vec<(String, String)>,
    order_for: Option<(String, Sort)>,
    /// how many copies arrived while the pointer was on the list
    late: usize,
    /// put together while the pointer was on the list: once it leaves, the order is best first again
    held: bool,
    /// the words for an opened row's tracks: (name, bitrate, length, size)
    files: HashMap<(String, String), Vec<[String; 4]>>,
    /// where the list was drawn last
    rect: Option<Rect>,
}

/// What stands over the copies: the album the search is for, the library's copy of it, and the line that
/// says how the search went. Looked at again at most every two seconds, or when an answer arrives.
pub(super) struct Head {
    text: String,
    gens: (u64, u64, u64),
    at: Instant,
    album: Option<Album>,
    owned: Option<Owned>,
    cover: Option<egui::TextureHandle>,
    title: String,
    about: String,
    owned_words: String,
    line: String,
}

/// The columns of the list of copies after the name: format, tracks, size, their speed, wait, from, and
/// the one for "Download". A narrow card lets go of "from" first, then the speed, then the size.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Cols {
    name: f32,
    w: [f32; 7],
}

const COL_NAMES: [(&str, Option<Sort>); 7] = [("FORMAT", Some(Sort::Format)), ("TRACKS", Some(Sort::Tracks)), ("SIZE", Some(Sort::Size)), ("THEIR SPEED", Some(Sort::Speed)), ("WAIT", Some(Sort::Wait)), ("FROM", None), ("", None)];

/// Lays the columns out in `inner` points. The name gets what the others leave, up to a width past which
/// the eye would have to travel: what is left over is shared among the columns of facts.
fn cols(inner: f32) -> Cols {
    const BASE: [f32; 7] = [96.0, 86.0, 82.0, 100.0, 92.0, 140.0, 96.0];
    const NAME_MOST: f32 = 560.0;
    const NAME_LEAST: f32 = 300.0;
    let mut on = [true; 7];
    let need = |on: &[bool; 7]| BASE.iter().zip(on).filter(|(_, o)| **o).map(|(w, _)| *w).sum::<f32>();
    for drop in [5usize, 3, 2] {
        if inner - need(&on) < NAME_LEAST {
            on[drop] = false;
        }
    }
    let rest = (inner - need(&on)).max(140.0);
    let name = rest.min(NAME_MOST);
    let facts = on[..6].iter().filter(|o| **o).count().max(1) as f32;
    let more = ((rest - name) / facts).floor();
    let mut w = [0.0; 7];
    for i in 0..7 {
        if on[i] {
            w[i] = BASE[i] + if i < 6 { more } else { 0.0 };
        }
    }
    Cols { name, w }
}

/// True for a copy that is the whole album (or, when the album is not known, at least four tracks).
fn is_whole(album: Option<&Album>, tracks: usize) -> bool {
    match album.and_then(|a| a.fit(tracks)) {
        Some((_, fit)) => fit == Fit::Whole,
        None => tracks >= 4,
    }
}

/// A lossy format at 192 kilobits or under ("MP3 192", "MP3 128").
fn is_weak(format: &str) -> bool {
    !is_lossless(format) && format.rsplit(' ').next().and_then(|r| r.parse::<u32>().ok()).is_some_and(|rate| rate > 0 && rate <= 200)
}

/// A file's name without the folders it is in, keeping a disc folder ("CD2\01 Intro.flac").
fn file_words(path: &str) -> &str {
    let is_sep = |c: char| c == '\\' || c == '/';
    let path = path.trim_end_matches(is_sep);
    let Some(leaf) = path.rfind(is_sep) else { return path };
    let above = &path[..leaf];
    let disc = above.rfind(is_sep).map_or(0, |i| i + 1);
    if crate::library::is_disc_like(&above[disc..]) { &path[disc..] } else { &path[leaf + 1..] }
}

/// True for a music file, by its name.
fn is_audio(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else { return false };
    let mut low = [0u8; 8];
    if ext.len() > low.len() || !ext.is_ascii() {
        return false;
    }
    for (to, from) in low.iter_mut().zip(ext.bytes()) {
        *to = from.to_ascii_lowercase();
    }
    std::str::from_utf8(&low[..ext.len()]).is_ok_and(slskd::is_audio_ext)
}

/// Minutes and seconds: "3:07".
fn length_words(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Puts the list of copies together: the order (steady under the pointer, see `settle`), the filters, the
/// copies that were tried and did not arrive at the bottom, and the words of every cell.
#[allow(clippy::too_many_arguments)]
fn relist(l: &mut Listing, kept: &Kept, stamp: (u64, Sort, Kind, bool, u64, u64), pointing: bool, album: Option<&Album>, mine: Option<Quality>, groups: &[Group]) {
    let (_, sort, kind, whole_only, _, _) = stamp;
    let key_of = |c: &Candidate| (c.user.clone(), c.name.clone());
    let ranked: Vec<(String, String)> = sorted(&kept.candidates, sort).into_iter().map(key_of).collect();
    // another search, or another order asked for (a click on a column's name, with the pointer on the
    // list): the list starts over in that order, and nothing in it counts as having arrived late
    let fresh = l.order_for.as_ref().is_none_or(|(text, by)| *text != kept.text || *by != sort);
    if fresh {
        (l.order, l.late, l.order_for) = (vec![], 0, Some((kept.text.clone(), sort)));
    }
    let pointing = pointing && !fresh;
    if l.for_text != kept.text || l.stamp.is_none_or(|s| s.0 != stamp.0) {
        l.files.clear();
    }
    let added = settle(&mut l.order, &ranked, pointing);
    l.late = if pointing { l.late + added } else { 0 };
    l.held = pointing;
    let at: HashMap<(&str, &str), usize> = kept.candidates.iter().enumerate().map(|(i, c)| ((c.user.as_str(), c.name.as_str()), i)).collect();
    let tried_and_failed = |c: &Candidate| groups.iter().any(|g| g.user == c.user && g.folder == c.name && g.finished() && g.failed > 0);
    let typed = filter::Filter::read(&l.typed);
    let (mut lines, mut sunk, mut before_typed) = (vec![], vec![], 0);
    for key in &l.order {
        let Some(&i) = at.get(&(key.0.as_str(), key.1.as_str())) else { continue };
        let c = &kept.candidates[i];
        let whole = is_whole(album, c.tracks);
        let shown = match kind {
            Kind::All => true,
            Kind::Lossless => is_lossless(&c.format),
            Kind::Mp3 => c.format.starts_with("MP3"),
        };
        if !shown || (whole_only && !whole) {
            continue;
        }
        before_typed += 1;
        let Some(why) = typed.keeps(c) else { continue };
        let (tracks, short) = match album.and_then(|a| a.fit(c.tracks)) {
            Some((words, fit)) => (words, fit == Fit::Short),
            None => (c.tracks.to_string(), false),
        };
        let line = Line {
            at: i,
            key: key.clone(),
            marks: dl_facts::marks(&c.name),
            tracks,
            short,
            size: size_words(c.bytes),
            speed: format!("{}/s", size_words(c.speed)),
            wait: if c.free_slot { String::new() } else { format!("{} ahead", c.queue) },
            weak: is_weak(&c.format),
            better: whole && mine.is_some_and(|m| Quality::of_format(&c.format).better_than(m)),
            why,
        };
        if tried_and_failed(c) { sunk.push(line) } else { lines.push(line) }
    }
    lines.extend(sunk);
    l.lines = lines;
    l.before_typed = before_typed;
    l.best = kept.candidates.first().map(key_of);
    (l.for_text, l.stamp) = (kept.text.clone(), Some(stamp));
}

// ---------------------------------------------------------------------- small pieces

/// One line of words, cut off at `most`, its middle at `mid`. Returns where it was drawn.
fn put(ui: &Ui, x: f32, mid: f32, most: f32, text: &str, size: f32, color: Color32) -> Rect {
    let words = theme::cut(ui, text, FontId::proportional(size), color, most);
    let at = pos2(x, (mid - words.size().y / 2.0).round());
    let rect = Rect::from_min_size(at, words.size());
    ui.painter().galley(at, words, color);
    rect
}

/// How wide these words are at a size.
fn width(ui: &Ui, text: &str, size: f32) -> f32 {
    ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(size), TEXT()).size().x
}

/// A column's name, its letters set a little apart, its middle at `mid`.
fn caption_at(ui: &Ui, x: f32, mid: f32, text: &str, color: Color32) -> Rect {
    let mut job = LayoutJob::default();
    job.append(text, 0.0, TextFormat { font_id: FontId::proportional(11.0), extra_letter_spacing: 1.4, color, ..Default::default() });
    let words = ui.painter().layout_job(job);
    let at = pos2(x, (mid - words.size().y / 2.0).round());
    let rect = Rect::from_min_size(at, words.size());
    ui.painter().galley(at, words, color);
    rect
}

/// A word that is the chosen one (bright, underlined in coral) or not (gray, bright under the pointer),
/// drawn at a place that is already known.
fn word_at(ui: &Ui, id: Id, at: egui::Pos2, text: &str, size: f32, chosen: bool) -> egui::Response {
    let font = FontId::proportional(size);
    let words = ui.painter().layout_no_wrap(text.to_string(), font.clone(), TEXT());
    let rect = Rect::from_min_size(at, words.size());
    let r = ui.interact(rect.expand2(vec2(3.0, 3.0)), id, Sense::click());
    crate::script::seen(text, rect);
    let color = if chosen || r.hovered() { TEXT() } else { DIM() };
    ui.painter().text(at, Align2::LEFT_TOP, text, font, color);
    if chosen {
        ui.painter().hline(rect.x_range(), rect.bottom() + 1.0, Stroke::new(1.5, theme::ACCENT()));
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// The word that asks for a copy, at the left of its cell: coral on the best match, bright on any other
/// row. Returns true when it was clicked.
fn download_word(ui: &Ui, id: Id, x: f32, mid: f32, loud: bool) -> bool {
    let font = FontId::proportional(14.0);
    let color = if loud { theme::ACCENT() } else { TEXT() };
    let words = ui.painter().layout_no_wrap("Download".to_string(), font.clone(), color);
    let rect = Rect::from_min_size(pos2(x, (mid - words.size().y / 2.0).round()), words.size());
    let r = ui.interact(rect.expand2(vec2(6.0, 5.0)), id, Sense::click());
    crate::script::seen("Download", rect);
    ui.painter().galley(rect.min, words, color);
    if r.hovered() {
        ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(if loud { 1.5 } else { 1.0 }, color));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

/// A number that changes when the text does (for telling when the list has to be put together again).
fn text_stamp(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// How large the box is that narrows the copies.
const FILTER_BOX: egui::Vec2 = vec2(236.0, 30.0);

/// The box that narrows the copies as it is typed in (see `filter`): a funnel, the words, and an X that
/// empties it. Esc empties it too; `focus` hands it the keyboard.
fn filter_box(ui: &mut Ui, rect: Rect, text: &mut String, focus: &mut bool) {
    let id = Id::unique("dl-filter");
    let frame = ui.painter().add(egui::Shape::Noop);
    // Esc in a box with words in it empties the box and leaves the keyboard there. (Esc takes the keyboard
    // from a box before the frame that carries the key begins, so what counts is whether the box had it
    // the frame before.)
    let had_keyboard = ui.data(|d| d.get_temp::<bool>(id.with("had-keyboard"))).unwrap_or(false);
    if had_keyboard && !text.is_empty() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        text.clear();
        *focus = true;
    }
    let has = !text.is_empty();
    let at = pos2(rect.left() + 10.0, (rect.center().y - 6.5).round());
    let funnel: Vec<egui::Pos2> = [(1.0, 2.0), (12.0, 2.0), (7.8, 7.0), (7.8, 11.0), (5.2, 9.6), (5.2, 7.0)].iter().map(|(x, y)| pos2(at.x + x, at.y + y)).collect();
    ui.painter().add(egui::Shape::closed_line(funnel, Stroke::new(1.2, if has { theme::ACCENT() } else { FAINT() })));
    let field = Rect::from_min_max(pos2(rect.left() + 31.0, rect.top()), pos2(rect.right() - if has { 30.0 } else { 8.0 }, rect.bottom()));
    let mut inside = ui.new_child(UiBuilder::new().id_salt("dl-filter-in").max_rect(field).layout(Layout::left_to_right(Align::Center)));
    let edit = TextEdit::singleline(text).id(id).hint_text(RichText::new("Filter these copies").color(FAINT())).desired_width(field.width()).font(FontId::proportional(14.0)).frame(egui::Frame::new());
    let r = inside.add(edit);
    crate::script::seen("Filter these copies", rect);
    if std::mem::take(focus) {
        r.request_focus();
    }
    // (asked of the app's own record, which holds whether or not the window is the one Windows types
    // into; and asked before the note is written: asking inside it would wait on itself for good)
    let has_keyboard = ui.memory(|m| m.has_focus(id));
    ui.data_mut(|d| d.insert_temp(id.with("had-keyboard"), has_keyboard));
    ui.painter().set(frame, egui::epaint::RectShape::new(rect, 8, theme::field_bg(), Stroke::new(1.0, if has_keyboard { DIM() } else { theme::BORDER() }), egui::StrokeKind::Inside));
    // (a click on the funnel puts the keyboard in the box too)
    if ui.interact(Rect::from_min_max(rect.min, pos2(field.left(), rect.bottom())), Id::unique("dl-filter-funnel"), Sense::click()).clicked() {
        r.request_focus();
    }
    if has {
        let mid = pos2(rect.right() - 16.0, rect.center().y);
        let hit = ui.interact(Rect::from_center_size(mid, vec2(22.0, 22.0)), Id::unique("dl-filter-x"), Sense::click());
        crate::script::seen("Empty the filter box", hit.rect);
        let pen = Stroke::new(1.3, if hit.hovered() { TEXT() } else { DIM() });
        ui.painter().line_segment([mid + vec2(-4.0, -4.0), mid + vec2(4.0, 4.0)], pen);
        ui.painter().line_segment([mid + vec2(-4.0, 4.0), mid + vec2(4.0, -4.0)], pen);
        if hit.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            text.clear();
            r.request_focus();
        }
    }
}

/// A part of one centered line of small words.
enum Piece<'a> {
    Dot(Color32),
    Say(&'a str, Color32),
    /// a quiet word that can be clicked
    Word(&'a str),
    Spin,
    /// a thin line, filled in blue this far (0 to 1)
    Bar(f32),
}

/// How wide `Piece::Bar` is.
const BAR: f32 = 120.0;

/// Draws the pieces as one line with its middle at `center`. Returns which `Word` was clicked.
fn centered_line(ui: &mut Ui, center: f32, top: f32, tall: f32, size: f32, pieces: &[Piece]) -> Option<usize> {
    const GAP: f32 = 14.0;
    let widths: Vec<f32> = pieces
        .iter()
        .map(|p| match p {
            Piece::Dot(_) => 7.0,
            Piece::Spin => 12.0,
            Piece::Bar(_) => BAR,
            Piece::Say(t, _) | Piece::Word(t) => width(ui, t, size),
        })
        .collect();
    // (a dot hangs close to the words it is about)
    let gap = |p: &Piece| if matches!(p, Piece::Dot(_) | Piece::Spin) { 7.0 } else { GAP };
    let total: f32 = widths.iter().sum::<f32>() + pieces.iter().take(pieces.len().saturating_sub(1)).map(gap).sum::<f32>();
    let (mut x, mid) = ((center - total / 2.0).round(), top + tall / 2.0);
    let mut clicked = None;
    for (n, p) in pieces.iter().enumerate() {
        let w = widths[n];
        match p {
            Piece::Dot(c) => drop(ui.painter().circle_filled(pos2(x + 3.5, mid + 0.5), 3.5, *c)),
            Piece::Spin => egui::Spinner::new().color(DIM()).paint_at(ui, Rect::from_center_size(pos2(x + 6.0, mid), vec2(12.0, 12.0))),
            Piece::Bar(part) => {
                let line = Rect::from_min_size(pos2(x, (mid - 1.0).round()), vec2(w, 3.0));
                ui.painter().rect_filled(line, egui::CornerRadius::same(2), theme::BORDER());
                ui.painter().rect_filled(Rect::from_min_size(line.min, vec2(w * part.clamp(0.0, 1.0), 3.0)), egui::CornerRadius::same(2), theme::ACCENT2());
            }
            Piece::Say(t, c) => drop(put(ui, x, mid, f32::INFINITY, t, size, *c)),
            Piece::Word(t) => {
                let rect = Rect::from_min_size(pos2(x, (mid - size * 0.66).round()), vec2(w, size * 1.32));
                let r = ui.put(rect, |ui: &mut Ui| theme::quiet(ui, t, size, true));
                if r.clicked() {
                    clicked = Some(n);
                }
            }
        }
        x += w + gap(p);
    }
    clicked
}

/// Large quiet words in the middle of a card with nothing to list, and a smaller line under them.
fn nothing(ui: &mut Ui, big: &str, color: Color32, small: &str) {
    ui.vertical_centered(|ui| {
        ui.add(Label::new(RichText::new(big).font(theme::display(22.0)).color(color)).wrap());
        if !small.is_empty() {
            ui.add_space(4.0);
            ui.add(Label::new(RichText::new(small).size(13.5).color(DIM())).wrap());
        }
    });
}

/// A fixed-width cell of a row, its content centered top to bottom and cut off at the cell's edge.
fn cell(ui: &mut Ui, w: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(w, ui.available_height()), Layout::left_to_right(Align::Center), |ui| {
        ui.set_width(w);
        ui.set_max_width(w);
        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
        add(ui);
    });
}

/// Four dots for the four steps of an album's trip (found, arriving, ready to import, filed); the first `done` are lit.
fn steps(ui: &mut Ui, done: usize, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(4.0 * 14.0, 12.0), Sense::hover());
    for i in 0..4 {
        let c = pos2(rect.left() + 5.0 + i as f32 * 14.0, rect.center().y);
        if i + 1 < 4 {
            ui.painter().hline(egui::Rangef::new(c.x + 4.0, c.x + 10.0), c.y, Stroke::new(1.0, if i + 1 < done { color } else { theme::BORDER() }));
        }
        if i < done {
            ui.painter().circle_filled(c, 3.5, color);
        } else {
            ui.painter().circle_stroke(c, 3.0, Stroke::new(1.0, theme::BORDER()));
        }
    }
}

impl DownloadsPanel {
    // ------------------------------------------------------------------ the top of the page

    /// The search box, the line under it, and the kept searches. `ready` is how many albums wait in the
    /// import list.
    pub fn hero(&mut self, ui: &mut Ui, left: f32, wide: f32, ready: usize) {
        self.search_box(ui, left, wide);
        self.state_line(ui, left, wide);
        self.kept_row(ui, left, wide, ready);
    }

    /// The box: a magnifying glass, the words, and "Search" at its right end, which is the page's loud
    /// word only while the box holds words that are not a kept search's.
    fn search_box(&mut self, ui: &mut Ui, left: f32, wide: f32) {
        let ready = self.snap.setup == Setup::Ready;
        let w = wide.min(620.0);
        let rect = Rect::from_min_size(pos2((left + (wide - w) / 2.0).round(), ui.cursor().top()), vec2(w, 46.0));
        let frame = ui.painter().add(egui::Shape::Noop);
        let (glass, pen) = (pos2(rect.left() + 24.0, rect.center().y - 1.0), Stroke::new(1.6, FAINT()));
        ui.painter().circle_stroke(glass, 5.6, pen);
        ui.painter().line_segment([glass + vec2(4.3, 4.3), glass + vec2(8.6, 8.6)], pen);
        let font = FontId::proportional(15.0);
        let word = ui.painter().layout_no_wrap("Search".to_string(), font.clone(), TEXT()).size();
        let word_rect = Rect::from_min_size(pos2(rect.right() - 18.0 - word.x, (rect.center().y - word.y / 2.0).round()), word);
        let field = Rect::from_min_max(pos2(rect.left() + 46.0, rect.top()), pos2(word_rect.left() - 14.0, rect.bottom()));
        let hint = if field.width() > 470.0 { "Artist and album, or a Spotify, Apple Music or Bandcamp link" } else { "Artist and album, or a link" };
        let mut inside = ui.new_child(UiBuilder::new().id_salt("dl-box").max_rect(field).layout(Layout::left_to_right(Align::Center)));
        let edit = TextEdit::singleline(&mut self.query).hint_text(RichText::new(hint).color(FAINT())).desired_width(field.width()).font(FontId::proportional(16.5)).frame(egui::Frame::new());
        let r = inside.add_enabled(ready, edit);
        self.facts.box_busy = r.has_focus() || r.lost_focus();
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.painter().set(frame, egui::epaint::RectShape::new(rect, 12, theme::field_bg(), Stroke::new(1.0, if r.has_focus() { DIM() } else { theme::BORDER() }), egui::StrokeKind::Inside));
        // (a click on the glass puts the keyboard in the box too)
        if ready && ui.interact(Rect::from_min_max(rect.min, pos2(field.left(), rect.bottom())), Id::unique("dl-box-glass"), Sense::click()).clicked() {
            r.request_focus();
        }
        let has = !self.query.trim().is_empty();
        // (words that are a kept search's are not new: the word that would run them again stays quiet)
        let on_show = self.history.list.iter().any(|k| k.text.eq_ignore_ascii_case(self.query.trim()));
        let can = ready && has;
        let hit = ui.interact(word_rect.expand(6.0), Id::unique("dl-box-search"), if can { Sense::click() } else { Sense::hover() });
        crate::script::seen("Search", word_rect);
        let over = can && hit.hovered();
        let color = if !can { FAINT() } else if !on_show { theme::ACCENT() } else if over { TEXT() } else { DIM() };
        ui.painter().text(word_rect.min, Align2::LEFT_TOP, "Search", font, color);
        if over {
            ui.painter().hline(word_rect.x_range(), word_rect.bottom() - 1.0, Stroke::new(if on_show { 1.0 } else { 1.5 }, color));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if (hit.clicked() || enter) && ready {
            let q = self.query.clone();
            self.search(&q);
        }
        ui.advance_cursor_after_rect(rect);
    }

    /// Under the box, always there: the Soulseek account in one faint line, or what is wrong with it and
    /// the word that fixes it. Then, only when there is something to say, why a search could not be run.
    /// Whether a Soulseek login is saved on this PC. Asked at most every two seconds, not on every redraw.
    fn has_login(&self, now: f64) -> bool {
        static SEEN: std::sync::Mutex<(f64, bool)> = std::sync::Mutex::new((-10.0, false));
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if now - seen.0 > 2.0 || now < seen.0 {
            *seen = (now, slskd::login_name().is_some());
        }
        seen.1
    }

    fn state_line(&mut self, ui: &mut Ui, left: f32, wide: f32) {
        if self.words.name != self.name || self.words.signin.is_empty() {
            self.words.name = self.name.clone();
            self.words.signin = self.name.as_ref().map(|n| format!("Signed in to Soulseek as {n}")).unwrap_or_else(|| "Signed in to Soulseek".into());
        }
        let top = ui.cursor().top() + 6.0;
        let center = left + wide / 2.0;
        let setup = self.snap.setup.clone();
        let (mut settings, mut again, mut get, mut stop) = (false, false, false, false);
        // the client is not installed: the word that fetches it, then how far that is, in this same line
        use crate::helpers::{Helper, State};
        let not_installed = setup == Setup::NotInstalled;
        let fetch = if not_installed { crate::helpers::state(Helper::Soulseek) } else { State::Idle };
        let doing = crate::helpers::doing(Helper::Soulseek, &fetch);
        let amount = match fetch {
            State::Getting(done, all) => crate::helpers::amount(done, all),
            _ => String::new(),
        };
        // (put in place a moment before the client is looked for again)
        let in_place = not_installed && doing.is_none() && crate::helpers::arrived(Helper::Soulseek) > 0 && slskd::exe().exists();
        {
            let pieces: Vec<Piece> = if self.facts.link.is_some() {
                vec![Piece::Spin, Piece::Say("Reading the link", DIM())]
            } else if self.waiting.is_some() {
                vec![Piece::Spin, Piece::Say("Will search as soon as Soulseek is signed in", DIM())]
            } else {
                match &setup {
                    Setup::Starting => vec![Piece::Say("Starting the Soulseek client", DIM())],
                    Setup::NotInstalled => match (&doing, &fetch) {
                        (Some((words, Some(part))), _) => vec![Piece::Say(words, DIM()), Piece::Bar(*part), Piece::Say(&amount, FAINT()), Piece::Word("Stop")],
                        (Some((words, None)), _) => vec![Piece::Spin, Piece::Say(words, DIM()), Piece::Word("Stop")],
                        (None, _) if in_place => vec![Piece::Say("Starting the Soulseek client", DIM())],
                        (None, State::Failed(why)) => vec![Piece::Say(why, WARN()), Piece::Word("Try again")],
                        // nothing of Soulseek is set up yet: one quiet line that says what searching needs
                        _ if !self.has_login(ui.input(|i| i.time)) => vec![Piece::Say("Searching needs a Soulseek login", FAINT()), Piece::Word("Set up Soulseek")],
                        _ => vec![Piece::Say("The Soulseek client is not installed", RED()), Piece::Word(Helper::Soulseek.word())],
                    },
                    Setup::BadSettings(why) => vec![Piece::Say(why, RED()), Piece::Word("Open Settings")],
                    Setup::NoLogin => vec![Piece::Say("Searching needs a Soulseek login", FAINT()), Piece::Word("Set up Soulseek")],
                    Setup::Offline(why) => vec![Piece::Say(why, WARN()), Piece::Word("Try again"), Piece::Word("Open Settings")],
                    Setup::Ready => vec![Piece::Dot(GREEN()), Piece::Say(&self.words.signin, FAINT())],
                }
            };
            if let Some(n) = centered_line(ui, center, top, 18.0, 12.5, &pieces) {
                match pieces[n] {
                    Piece::Word("Stop") => stop = true,
                    // (while the client is not installed, "Try again" fetches it again)
                    Piece::Word("Try again") if not_installed => get = true,
                    Piece::Word("Try again") => again = true,
                    Piece::Word(w) if w == Helper::Soulseek.word() => get = true,
                    _ => settings = true,
                }
            }
        }
        if get {
            crate::helpers::start(Helper::Soulseek, ui.ctx());
        }
        if stop {
            crate::helpers::stop(Helper::Soulseek);
        }
        if settings {
            slskd::ask_open_settings();
        }
        if again {
            let _ = self.tx.send(Cmd::Reconnect);
        }
        ui.advance_cursor_after_rect(Rect::from_min_size(pos2(left, top), vec2(wide, 18.0)));
        if self.snap.note.is_none() && self.said.is_none() {
            return;
        }
        let mut column = ui.available_rect_before_wrap();
        (column.min.x, column.max.x) = (left, left + wide);
        ui.scope_builder(UiBuilder::new().max_rect(column).layout(Layout::top_down(Align::Center)), |ui| {
            if let Some(n) = &self.snap.note {
                ui.add(Label::new(RichText::new(n).size(13.0).color(RED())).wrap());
            }
            if let Some(why) = &self.said {
                ui.add(Label::new(RichText::new(why).size(13.0).color(WARN())).wrap());
            }
        });
    }

    /// The word that shows the import list, then every kept search: its words, how many copies it found
    /// (a spinner while it runs) and a small X. In the middle, on as many lines as they need. A click
    /// shows that one in the main card; nothing is run again and nothing is thrown away. Not drawn until
    /// there is a search to go back to.
    fn kept_row(&mut self, ui: &mut Ui, left: f32, wide: f32, ready: usize) {
        self.seen_ready = if self.view == View::Import { ready } else { self.seen_ready.min(ready) };
        if self.history.list.is_empty() {
            self.view = View::Import;
            return;
        }
        const LINE: f32 = 26.0;
        const BETWEEN: f32 = 24.0;
        let new = ready - self.seen_ready;
        if self.words.ready != Some(ready) {
            self.words.ready = Some(ready);
            self.words.import = if ready == 0 { "Import list".to_string() } else { format!("Ready to import  \u{b7}  {ready}") };
        }
        if self.words.new != new {
            self.words.new = new;
            self.words.new_words = format!("{new} new");
        }
        // how wide each one is: the import list's word (and "2 new"), a thin line, then the searches
        let import_w = width(ui, &self.words.import, 14.0) + if new > 0 { 8.0 + width(ui, &self.words.new_words, 12.5) } else { 0.0 };
        let mut widths: Vec<f32> = Vec::with_capacity(self.history.list.len() + 2);
        widths.push(import_w);
        widths.push(1.0);
        for k in &self.history.list {
            let count = if k.run != Run::Done {
                5.0 + 11.0
            } else if !k.candidates.is_empty() {
                self.words.count.clear();
                let _ = write!(self.words.count, "{}", k.candidates.len());
                5.0 + width(ui, &self.words.count, 11.5)
            } else {
                0.0
            };
            widths.push(width(ui, &short_words(&k.text), 14.0) + count + 5.0 + 14.0);
        }
        // broken into lines that fit, each one in the middle
        let mut lines: Vec<(usize, usize, f32)> = vec![];
        let (mut from, mut used) = (0, 0.0);
        for (n, w) in widths.iter().enumerate() {
            if n > from && used + BETWEEN + w > wide {
                lines.push((from, n, used));
                (from, used) = (n, 0.0);
            }
            used += if n > from { BETWEEN } else { 0.0 } + w;
        }
        lines.push((from, widths.len(), used));
        let top = ui.cursor().top() + 14.0;
        let (mut go, mut close, mut home) = (None, None, false);
        for (row, (from, to, used)) in lines.iter().enumerate() {
            let mid = top + row as f32 * LINE + LINE / 2.0;
            let mut x = (left + (wide - used) / 2.0).round();
            for n in *from..*to {
                match n {
                    0 => {
                        let r = word_at(ui, Id::unique("dl-show-import"), pos2(x, (mid - 9.5).round()), &self.words.import, 14.0, self.view == View::Import);
                        home |= r.clicked();
                        // albums that arrived while a search was on show
                        if new > 0 {
                            put(ui, r.rect.right() + 5.0, mid + 0.5, f32::INFINITY, &self.words.new_words, 12.5, GREEN());
                        }
                    }
                    1 => drop(ui.painter().vline(x, egui::Rangef::new(mid - 7.0, mid + 8.0), Stroke::new(1.0, theme::BORDER()))),
                    _ => {
                        let k = &self.history.list[n - 2];
                        let chosen = self.view == View::Results && self.history.open.as_deref() == Some(k.text.as_str());
                        let r = word_at(ui, Id::unique(("dl-kept", &k.text)), pos2(x, (mid - 9.5).round()), &short_words(&k.text), 14.0, chosen);
                        if r.clicked() {
                            go = Some(k.text.clone());
                        }
                        if r.middle_clicked() {
                            close = Some(k.text.clone());
                        }
                        let mut at = r.rect.right() + 2.0;
                        if r.hovered() {
                            r.on_hover_text(kept_line(k, now_secs()));
                        }
                        if k.run != Run::Done {
                            egui::Spinner::new().color(DIM()).paint_at(ui, Rect::from_center_size(pos2(at + 5.5, mid + 1.0), vec2(11.0, 11.0)));
                            at += 16.0;
                        } else if !k.candidates.is_empty() {
                            self.words.count.clear();
                            let _ = write!(self.words.count, "{}", k.candidates.len());
                            at = put(ui, at, mid + 1.0, f32::INFINITY, &self.words.count, 11.5, FAINT()).right() + 5.0;
                        } else {
                            at += 3.0;
                        }
                        let mark = Rect::from_min_size(pos2(at, mid - 6.0), vec2(14.0, 14.0));
                        let name = if crate::script::on() { Cow::Owned(format!("close:{}", k.text)) } else { Cow::Borrowed("") };
                        if ui.put(mark, |ui: &mut Ui| theme::close_mark(ui, &name, "Close this search")).clicked() {
                            close = Some(k.text.clone());
                        }
                    }
                }
                x += widths[n] + BETWEEN;
            }
        }
        ui.advance_cursor_after_rect(Rect::from_min_size(pos2(left, top), vec2(wide, lines.len() as f32 * LINE)));
        if home {
            self.view = View::Import;
        }
        if let Some(text) = go {
            self.query = text.clone(); // (the box shows the words of the search on show)
            (self.history.open, self.view) = (Some(text), View::Results);
        }
        if let Some(text) = close {
            self.close_search(&text, ui.ctx());
        }
        // Esc goes back to the import list (but not the Esc that only leaves a box that was being typed in,
        // or empties the filter box: the box has let go of the keyboard by the time that key is seen)
        if self.view == View::Results && !theme::typing_until_now(ui.ctx()) && !ui.ctx().any_popup_open() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.view = View::Import;
        }
    }

    // ------------------------------------------------------------------ what is arriving

    /// The trip each album is on, as a card that is only there while something is arriving: a ring for
    /// how much has arrived, four dots for how far along it is, and what is happening. `import_stage`
    /// says where the import list has a folder: None = not listed, Some(false) = waiting there,
    /// Some(true) = imported.
    pub fn arriving(&mut self, ui: &mut Ui, left: f32, wide: f32, import_stage: &dyn Fn(&str) -> Option<bool>) {
        // albums already handed to the import list stay here, one step further along, until they are filed
        let now = Instant::now();
        for j in &mut self.journeys {
            if j.filed_at.is_none() && import_stage(&j.folder) == Some(true) {
                j.filed_at = Some(now);
            }
        }
        self.journeys.retain(|j| j.filed_at.is_none_or(|t| t.elapsed() < Duration::from_secs(25)));
        if self.journeys.iter().any(|j| j.filed_at.is_some()) {
            ui.ctx().request_repaint_after(Duration::from_secs(5));
        }
        let (handed, tx, journeys) = (&self.handed, &self.tx, &self.journeys);
        let groups: Vec<&Group> = self.snap.groups.iter().filter(|g| !handed.contains(&g.key())).collect();
        if groups.is_empty() && journeys.is_empty() {
            return;
        }
        ui.add_space(GAP);
        theme::panel(ui, left, wide, vec2(PAD.x, 16.0), "dl-arriving", |ui| {
            let live = groups.iter().filter(|g| !g.finished()).count();
            let failed: Vec<&&Group> = groups.iter().filter(|g| g.finished() && g.failed > 0).collect();
            ui.horizontal(|ui| {
                let head = match (live, failed.len()) {
                    (0, 0) => "ON THEIR WAY IN".to_string(),
                    (0, f) => format!("{f} DID NOT ARRIVE"),
                    (n, 0) => format!("ARRIVING  \u{b7}  {n}"),
                    (n, f) => format!("ARRIVING  \u{b7}  {n}     {f} DID NOT ARRIVE"),
                };
                theme::caption(ui, &head);
                if !failed.is_empty() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::quiet(ui, "Clear the ones that did not arrive", 12.5, true).clicked() {
                            let ids = failed.iter().flat_map(|g| g.all_ids.iter().map(|id| (g.user.clone(), id.clone()))).collect();
                            let _ = tx.send(Cmd::Remove(ids));
                        }
                    });
                }
            });
            ui.add_space(4.0);
            let total = ui.available_width();
            // ring | name | from | the four steps | what is happening | action
            let from_w = if total >= 760.0 { 150.0 } else { 0.0 };
            let name_w = (total * 0.3).clamp(140.0, 420.0);
            let words_w = (total - 34.0 - name_w - from_w - 84.0 - 64.0).max(120.0);
            let rows = groups.len() + journeys.len();
            ScrollArea::vertical().id_salt("dl-arriving-rows").max_height(ARRIVING_ROW * 4.5).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let total = if rows > 4 { total - 12.0 } else { total };
                for g in &groups {
                    let (rect, _) = ui.allocate_exact_size(vec2(total, ARRIVING_ROW), Sense::hover());
                    let mut row = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
                    row.spacing_mut().item_spacing.x = 0.0;
                    let (text, color, bar) = arriving_words(g);
                    cell(&mut row, 34.0, |ui| ring(ui, if g.finished() { 1.0 } else { g.frac() }, bar));
                    cell(&mut row, name_w - 16.0, |ui| {
                        ui.add(Label::new(RichText::new(&g.folder).color(TEXT())).truncate());
                    });
                    row.add_space(16.0);
                    if from_w > 0.0 {
                        cell(&mut row, from_w, |ui| {
                            ui.add(Label::new(RichText::new(format!("from {}", g.user)).size(12.5).color(DIM())).truncate());
                        });
                    }
                    cell(&mut row, 84.0, |ui| steps(ui, 2, bar));
                    cell(&mut row, words_w - 12.0, |ui| {
                        ui.add(Label::new(RichText::new(text).size(12.5).color(color)).truncate());
                    });
                    row.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let word = if g.finished() { "Clear" } else { "Cancel" };
                        if !g.arrived() && theme::quiet(ui, word, 13.0, true).clicked() {
                            let ids = if g.finished() { &g.all_ids } else { &g.live_ids };
                            let _ = tx.send(Cmd::Remove(ids.iter().map(|id| (g.user.clone(), id.clone())).collect()));
                        }
                    });
                }
                for j in journeys {
                    let (rect, _) = ui.allocate_exact_size(vec2(total, ARRIVING_ROW), Sense::hover());
                    let mut row = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
                    row.spacing_mut().item_spacing.x = 0.0;
                    let filed = j.filed_at.is_some();
                    let (step, text) = if filed { (4, "Filed in your library".to_string()) } else { (3, format!("Arrived: {} files, {}. Ready to import.", j.files, size_words(j.bytes))) };
                    cell(&mut row, 34.0, |ui| ring(ui, 1.0, GREEN()));
                    cell(&mut row, name_w - 16.0, |ui| {
                        ui.add(Label::new(RichText::new(&j.folder).color(TEXT())).truncate());
                    });
                    row.add_space(16.0);
                    if from_w > 0.0 {
                        cell(&mut row, from_w, |ui| {
                            ui.add(Label::new(RichText::new(format!("from {}", j.user)).size(12.5).color(DIM())).truncate());
                        });
                    }
                    cell(&mut row, 84.0, |ui| steps(ui, step, GREEN()));
                    cell(&mut row, words_w + 64.0, |ui| {
                        ui.add(Label::new(RichText::new(text).size(12.5).color(GREEN())).truncate());
                    });
                }
            });
        });
    }

    // ------------------------------------------------------------------ the copies a search found

    /// Looks again, when it may have changed, at what stands over the copies: the album the search is
    /// for, the library's copy of it, and the line that says how the search went.
    fn refresh_head(&mut self, around: &Around, ctx: &egui::Context) {
        let Some(kept) = self.history.shown() else { return };
        let gens = (self.facts_gen, self.snap_gen, kept.version);
        if self.head.as_ref().is_some_and(|h| h.text == kept.text && h.gens == gens && h.at.elapsed() < Duration::from_secs(2)) {
            return;
        }
        let (text, line) = (kept.text.clone(), kept_line(kept, now_secs()));
        let album = self.album_for(&text, ctx);
        let owned = album.as_ref().and_then(|a| self.owned_for(a, around, ctx));
        let (mut title, mut about, mut owned_words) = (format!("Copies of \"{text}\""), String::new(), String::new());
        if let Some(a) = &album {
            title = a.title.clone();
            let mut parts = vec![a.artist.clone()];
            if !a.year.is_empty() {
                parts.push(a.year.clone());
            }
            if let Some(n) = a.editions.first() {
                parts.push(if *n == 1 { "1 track".to_string() } else { format!("{n} tracks") });
            }
            about = parts.join("  \u{b7}  ");
            if let Some(o) = &owned {
                let least = a.editions.first().copied().unwrap_or(0);
                let sound = o.quality.words();
                let sound = if sound.is_empty() { String::new() } else { format!(", {sound}") };
                owned_words = if o.tracks >= least { format!("In your library{sound}") } else { format!("In your library: {} of its {} tracks{sound}", o.tracks, least) };
            }
        }
        // (the cover that is already on show stays while the same album is)
        let cover = self.head.take().filter(|h| h.text == text && h.album == album).and_then(|h| h.cover);
        self.head = Some(Head { text, gens, at: Instant::now(), album, owned, cover, title, about, owned_words, line });
        // what is listed leans on the album and the library's copy: put it together again
        self.listing.stamp = None;
    }

    /// The copies a search found, as the page's main card: the album the search is for at its top (its
    /// cover, and whether the library has it) with the filters beside it, then the copies, best first or
    /// by a column. The card is as tall as its copies, up to `floor`, where the window ends (in a window
    /// too short for it the card keeps a least height, and the page scrolls).
    pub fn results(&mut self, ui: &mut Ui, left: f32, wide: f32, floor: f32, around: Around) {
        self.refresh_head(&around, ui.ctx());
        let snap_gen = self.snap_gen;
        ui.ctx().request_repaint_after(Duration::from_secs(60)); // (the line says how old the list is)
        // the list fills in while the search runs, but never moves under the pointer: while he points at
        // it the rows keep their place and new copies go below (see `settle`). Worked out before anything
        // is drawn, so that what is said about it stands in the top line and nothing pushes the list down.
        let pointing = self.listing.rect.is_some_and(|r| ui.rect_contains_pointer(r));
        let Self { history, listing, head, facts, kind, snap, asked, .. } = self;
        let (Some(kept), Some(head)) = (history.shown(), head.as_mut()) else { return };
        // what is typed in the filter box belongs to the search on show: another search has its own
        if facts.filter_for != kept.text {
            let was = std::mem::take(&mut facts.filter);
            if !was.trim().is_empty() {
                facts.filters.insert(facts.filter_for.clone(), was);
            } else {
                facts.filters.remove(&facts.filter_for);
            }
            facts.filter = facts.filters.get(&kept.text).cloned().unwrap_or_default();
            facts.filter_for.clone_from(&kept.text);
        }
        // Ctrl+F puts the keyboard in that box, as it does on the Library and Health pages
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            facts.focus_filter = true;
        }
        let stamp = (kept.version, facts.sort, *kind, facts.whole, text_stamp(&facts.filter), snap_gen);
        if listing.for_text != kept.text || listing.stamp != Some(stamp) || (!pointing && listing.held) {
            listing.typed.clone_from(&facts.filter);
            relist(listing, kept, stamp, pointing, head.album.as_ref(), head.owned.as_ref().map(|o| o.quality), &snap.groups);
        }
        let filtering = !filter::Filter::read(&facts.filter).is_empty();
        let mut clear_filter = false;
        let running = kept.run != Run::Done;
        let (mut stop, mut again) = (false, false);
        // the copy this frame asks for, if any (where it is among the search's copies)
        let mut get: Option<usize> = None;

        ui.add_space(GAP);
        let top = ui.cursor().top();
        let floor = (floor - GAP).max(top + 340.0);
        let fill = ui.painter().add(egui::Shape::Noop);
        let room = Rect::from_min_max(pos2(left + PAD.x, top + PAD.y), pos2(left + wide - PAD.x, floor - PAD.y));
        let mut card = ui.new_child(UiBuilder::new().id_salt("dl-copies").max_rect(room).layout(Layout::top_down(Align::Min)));
        card.set_clip_rect(Rect::from_min_max(pos2(left, top), pos2(left + wide, floor)).intersect(ui.clip_rect()));
        {
            let ui = &mut card;
            ui.spacing_mut().item_spacing.y = 0.0;
            let inner = ui.available_width();

            // ---- the album (or the words searched for), and beside it what to show
            let head_rect = Rect::from_min_size(ui.cursor().min, vec2(inner, HEAD));
            ui.allocate_rect(head_rect, Sense::hover());
            let two_lines = inner < 1040.0;
            let mut words_left = head_rect.right();
            {
                let mut act = |ui: &mut Ui| {
                    if running {
                        stop = theme::quiet(ui, "Stop searching", 14.0, true).clicked();
                    } else {
                        again = theme::quiet(ui, "Search again", 14.0, true).clicked();
                    }
                };
                if two_lines {
                    // a narrow card: the filters go under the album, the word that runs the search at their right
                    ui.add_space(8.0);
                    let line = Rect::from_min_size(ui.cursor().min, vec2(inner, 30.0));
                    ui.allocate_rect(line, Sense::hover());
                    let mut filters = ui.new_child(UiBuilder::new().id_salt("dl-filters").max_rect(line).layout(Layout::left_to_right(Align::Center)));
                    filters.spacing_mut().item_spacing.x = 10.0;
                    let (box_rect, _) = filters.allocate_exact_size(vec2((inner - 430.0).clamp(140.0, FILTER_BOX.x), FILTER_BOX.y), Sense::hover());
                    filter_box(&mut filters, box_rect, &mut facts.filter, &mut facts.focus_filter);
                    filters.add_space(8.0);
                    theme::choice(&mut filters, kind, Kind::All, "All");
                    theme::choice(&mut filters, kind, Kind::Lossless, "Lossless");
                    theme::choice(&mut filters, kind, Kind::Mp3, "MP3");
                    filters.add_space(8.0);
                    theme::toggle(&mut filters, &mut facts.whole, "Whole albums");
                    let mut right = ui.new_child(UiBuilder::new().id_salt("dl-act").max_rect(line).layout(Layout::right_to_left(Align::Center)));
                    act(&mut right);
                } else {
                    let mut right = ui.new_child(UiBuilder::new().id_salt("dl-head-words").max_rect(head_rect).layout(Layout::right_to_left(Align::Center)));
                    right.spacing_mut().item_spacing.x = 14.0;
                    // (right to left, so the last word is added first)
                    act(&mut right);
                    right.add_space(18.0);
                    theme::toggle(&mut right, &mut facts.whole, "Whole albums");
                    right.add_space(10.0);
                    theme::choice(&mut right, kind, Kind::Mp3, "MP3");
                    theme::choice(&mut right, kind, Kind::Lossless, "Lossless");
                    theme::choice(&mut right, kind, Kind::All, "All");
                    // the box that is typed in stands before the words that are clicked
                    right.add_space(8.0);
                    let (box_rect, _) = right.allocate_exact_size(FILTER_BOX, Sense::hover());
                    filter_box(&mut right, box_rect, &mut facts.filter, &mut facts.focus_filter);
                    words_left = right.min_rect().left() - 20.0;
                }
            }
            match &head.album {
                Some(a) => {
                    let pic = Rect::from_min_size(head_rect.min, vec2(HEAD, HEAD));
                    if head.cover.is_none() {
                        if let crate::images::Pic::Ready(t) = around.images.cover(&a.artwork, &a.artist, &a.title, true) {
                            head.cover = Some(t);
                        }
                    }
                    match &head.cover {
                        Some(t) => egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), pic.size())).corner_radius(6).paint_at(ui, pic),
                        None => drop(ui.painter().rect_filled(pic, 6, CARD_HOVER())),
                    }
                    let x = pic.right() + 16.0;
                    let most = (words_left - x).max(60.0);
                    // (two lines, or three with the library's copy, set in the middle of the cover's height)
                    let mut y = head_rect.top() + if head.owned_words.is_empty() { 10.0 } else { 1.0 };
                    let title = theme::cut(ui, &head.title, theme::display(26.0), TEXT(), most);
                    ui.painter().galley(pos2(x, y), title, TEXT());
                    y += 35.0;
                    put(ui, x, y + 8.0, most, &head.about, 13.0, DIM());
                    if let (Some(o), false) = (&head.owned, head.owned_words.is_empty()) {
                        let rect = put(ui, x, y + 27.0, most, &head.owned_words, 12.5, GREEN());
                        let r = ui.interact(rect, Id::unique("dl-owned"), Sense::click());
                        if r.on_hover_text("Open the folder").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            let _ = open::that(&o.folder);
                        }
                    }
                }
                None => {
                    let title = theme::cut(ui, &head.title, theme::display(26.0), TEXT(), (words_left - head_rect.left()).max(60.0));
                    ui.painter().galley(pos2(head_rect.left(), (head_rect.center().y - title.size().y / 2.0).round()), title, TEXT());
                }
            }

            // ---- one line, always there, for how the search went (so nothing said here pushes the list down)
            ui.add_space(10.0);
            let line_rect = Rect::from_min_size(ui.cursor().min, vec2(inner, 20.0));
            ui.allocate_rect(line_rect, Sense::hover());
            let failed = kept.error.is_some();
            if !kept.candidates.is_empty() {
                let mut x = line_rect.left();
                if running {
                    egui::Spinner::new().color(DIM()).paint_at(ui, Rect::from_center_size(pos2(x + 6.5, line_rect.center().y), vec2(13.0, 13.0)));
                    x += 23.0;
                }
                let line: Cow<str> = match (facts.sort, listing.late) {
                    (Sort::Best, 0) => Cow::Borrowed(&head.line),
                    // copies that arrived while the pointer was on the list wait below the rows that were there
                    (by, late) => {
                        let line = if by == Sort::Best { head.line.clone() } else { head.line.replace("best first", sort_words(by)) };
                        Cow::Owned(if late > 0 { format!("{line}  {}", late_words(late)) } else { line })
                    }
                };
                let color = if failed && !running { RED() } else if listing.late > 0 { ACCENT2() } else { DIM() };
                // with words in the filter box the line says how many of the copies are listed: "4 of 60
                // found, best first"
                let all = kept.candidates.len().to_string();
                match line.strip_prefix(all.as_str()).filter(|rest| filtering && rest.starts_with(" found")) {
                    Some(rest) => {
                        let lead = format!("{} of {all}", listing.lines.len());
                        x = put(ui, x, line_rect.center().y, line_rect.right() - x, &lead, 13.5, TEXT()).right();
                        let rest = if listing.lines.is_empty() { " found" } else { rest };
                        put(ui, x, line_rect.center().y, line_rect.right() - x, rest, 13.5, color);
                    }
                    None if filtering => drop(put(ui, x, line_rect.center().y, line_rect.right() - x, &format!("{line}  {} of {all} fit the filter.", listing.lines.len()), 13.5, color)),
                    None => drop(put(ui, x, line_rect.center().y, line_rect.right() - x, &line, 13.5, color)),
                }
            }
            // the thin line under the album: while the search runs it fills with how much of it is in
            ui.add_space(10.0);
            let seam = ui.cursor().top();
            ui.painter().hline(line_rect.x_range(), seam, Stroke::new(1.0, theme::BORDER()));
            if running {
                let done = ui.ctx().animate_value_with_time(Id::unique("dl-search-part"), kept.part.clamp(0.0, 1.0), 0.4);
                if done > 0.0 {
                    ui.painter().hline(line_rect.left()..=line_rect.left() + inner * done, seam, Stroke::new(2.0, ACCENT2()));
                }
            }
            ui.add_space(9.0);

            if kept.candidates.is_empty() {
                // nothing found: what is happening, or how it ended, in the middle of the card
                ui.add_space(34.0);
                if running {
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Spinner::new().size(24.0).color(DIM()));
                    });
                    ui.add_space(12.0);
                    let who: Cow<str> = match kept.answers {
                        0 => "The first copies usually show within ten seconds.".into(),
                        1 => "1 person has answered so far.".into(),
                        n => format!("{n} people have answered so far.").into(),
                    };
                    nothing(ui, "Asking people on Soulseek", TEXT(), &who);
                } else {
                    let (big, small) = head.line.split_once(". ").unwrap_or((head.line.trim_end_matches('.'), ""));
                    nothing(ui, big, if failed { RED() } else { DIM() }, small);
                }
                ui.add_space(30.0);
            } else if listing.lines.is_empty() && filtering && listing.before_typed > 0 {
                // the words in the filter box leave nothing: say so, with the word that empties the box
                ui.add_space(34.0);
                let typed: String = facts.filter.trim().chars().take(48).collect();
                let of = match listing.before_typed {
                    1 => "The one copy here does not fit".to_string(),
                    n => format!("None of the {n} copies fits"),
                };
                nothing(ui, &format!("{of} \"{typed}\""), DIM(), "");
                ui.add_space(8.0);
                let (row, _) = ui.allocate_exact_size(vec2(inner, 22.0), Sense::hover());
                clear_filter = centered_line(ui, row.center().x, row.top(), row.height(), 13.5, &[Piece::Word("Clear the filter")]).is_some();
                ui.add_space(22.0);
            } else if listing.lines.is_empty() {
                ui.add_space(34.0);
                nothing(ui, if facts.whole { "None of these copies is a whole album in this format" } else { "None of these copies is in this format" }, DIM(), "");
                ui.add_space(30.0);
            } else {
                // ---- the keys: up and down move along the rows, Space shows a row's files, Enter downloads it
                let mut moved = false;
                if !facts.box_busy && !theme::typing(ui.ctx()) && !ui.ctx().any_popup_open() {
                    let (down, up, space, enter) = ui.input_mut(|i| {
                        (
                            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                            i.consume_key(egui::Modifiers::NONE, egui::Key::Space),
                            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                        )
                    });
                    let at = facts.sel.as_ref().and_then(|s| listing.lines.iter().position(|l| &l.key == s));
                    if down || up {
                        let next = match (at, down) {
                            (None, _) => 0,
                            (Some(i), true) => (i + 1).min(listing.lines.len() - 1),
                            (Some(i), false) => i.saturating_sub(1),
                        };
                        facts.sel = Some(listing.lines[next].key.clone());
                        moved = true;
                    }
                    if let (Some(i), true) = (at, space) {
                        let key = &listing.lines[i].key;
                        if !facts.open_rows.remove(key) {
                            facts.open_rows.insert(key.clone());
                        }
                        moved = true;
                    }
                    if let (Some(i), true) = (at, enter) {
                        let l = &listing.lines[i];
                        if !asked.contains(&l.key) && !snap.groups.iter().any(|g| g.user == l.key.0 && g.folder == l.key.1) {
                            get = Some(l.at);
                        }
                    }
                }

                // ---- the columns' names: a click orders the list by that column, a second click goes back to best first
                let cols = cols(inner - 12.0 - 20.0);
                let names = Rect::from_min_size(ui.cursor().min, vec2(inner, 24.0));
                ui.allocate_rect(names, Sense::hover());
                let mid = names.center().y;
                let mut x = names.left() + 10.0;
                caption_at(ui, x, mid, "FOLDER", DIM());
                x += cols.name;
                for (n, (name, by)) in COL_NAMES.into_iter().enumerate() {
                    if cols.w[n] == 0.0 {
                        continue;
                    }
                    match by {
                        Some(by) => {
                            let on = facts.sort == by;
                            let shown: Cow<str> = if on { format!("{name} \u{2193}").into() } else { name.into() };
                            let rect = caption_at(ui, x, mid, &shown, if on { TEXT() } else { DIM() });
                            let r = ui.interact(rect.expand2(vec2(4.0, 5.0)), Id::unique(("dl-sort", n)), Sense::click());
                            if crate::script::on() {
                                crate::script::seen(&format!("sort:{name}"), rect);
                            }
                            if r.hovered() && !on {
                                ui.painter().hline(rect.x_range(), rect.bottom() + 1.0, Stroke::new(1.0, FAINT()));
                            }
                            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                facts.sort = if on { Sort::Best } else { by };
                            }
                        }
                        None => drop(caption_at(ui, x, mid, name, DIM())),
                    }
                    x += cols.w[n];
                }
                ui.add_space(2.0);

                // ---- the copies. Only the rows on screen are drawn; the rest only take their room.
                let Listing { lines, files, best, .. } = &mut *listing;
                let most = ui.available_height();
                let list = ScrollArea::vertical().id_salt("dl-results").max_height(most).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let total = inner - 12.0;
                    for (i, l) in lines.iter().enumerate() {
                        let Some(c) = kept.candidates.get(l.at) else { continue };
                        let open = facts.open_rows.contains(&l.key);
                        let chosen = facts.sel.as_ref() == Some(&l.key);
                        let (_, rect) = ui.allocate_space(vec2(total, ROW));
                        // an opened row lists its tracks under it, and how many pictures come with them
                        let pictures = c.files.len().saturating_sub(c.tracks);
                        let hidden = cols.w[3] == 0.0 || cols.w[5] == 0.0;
                        let under = if open { 2.0 + if hidden { FILE_ROW } else { 0.0 } + c.tracks as f32 * FILE_ROW + if pictures > 0 { FILE_ROW } else { 0.0 } + 6.0 } else { 0.0 };
                        let under_rect = if open { ui.allocate_space(vec2(total, under)).1 } else { Rect::NOTHING };
                        if chosen && moved {
                            ui.scroll_to_rect(rect, None);
                        }
                        if ui.is_rect_visible(rect) {
                            // the row itself is the control that shows its files (the word "Download" sits on top of it)
                            let id = Id::unique(("dl-row", &l.key.0, &l.key.1));
                            let row = ui.interact(rect, id, Sense::click());
                            if crate::script::on() {
                                crate::script::seen(&format!("row:{}", c.name), rect);
                            }
                            if row.clicked() {
                                if !facts.open_rows.remove(&l.key) {
                                    facts.open_rows.insert(l.key.clone());
                                }
                                facts.sel = Some(l.key.clone());
                            }
                            let over = ui.rect_contains_pointer(rect);
                            if over || chosen || open {
                                ui.painter().rect_filled(rect, 6, CARD_HOVER());
                            }
                            if chosen {
                                ui.painter().vline(rect.left() + 1.0, rect.y_range().shrink(7.0), Stroke::new(2.0, theme::ACCENT()));
                            }
                            let mid = rect.center().y;
                            let mut x = rect.left() + 10.0;
                            let name_end = x + cols.name - 18.0;
                            // (only when the top row really is the best of everything known so far)
                            let is_best = i == 0 && *kind == Kind::All && !facts.whole && facts.sort == Sort::Best && best.as_ref() == Some(&l.key);
                            // (in a narrow name column the coral "Download" alone says so)
                            if is_best && cols.name >= 280.0 {
                                x = put(ui, x, mid + 0.5, f32::INFINITY, "Best match", 11.5, theme::ACCENT()).right() + 10.0;
                            }
                            // small words after the name: a step up from the library's copy, and where it is; the
                            // edition words in the folder's name too, when the name is cut short and hides them
                            let place = match (around.stage)(&c.name) {
                                Some(false) => "In the import list",
                                Some(true) => "Imported",
                                None => "",
                            };
                            let tag_w = |t: &str| if t.is_empty() { 0.0 } else { width(ui, t, 11.5) + 10.0 };
                            // the name comes first: it keeps room for itself (up to a point), and the small words
                            // show only in what that leaves
                            let room = name_end - x;
                            let name_w = width(ui, &c.name, 14.0);
                            let spare = room - name_w.min(170.0);
                            let place = if tag_w(place) <= spare { place } else { "" };
                            let better = l.better && tag_w("Better than yours") <= spare - tag_w(place);
                            let fixed = if better { tag_w("Better than yours") } else { 0.0 } + tag_w(place);
                            // the track that holds a word typed in the filter box, so the row says why it is here
                            let why = if tag_w(&l.why) <= spare - fixed { l.why.as_str() } else { "" };
                            let fixed = fixed + tag_w(why);
                            let marks_w: f32 = l.marks.iter().map(|m| tag_w(m)).sum();
                            let cut_short = name_w + fixed > room;
                            let with_marks = cut_short && room - fixed - marks_w >= 170.0;
                            let mut right = name_end;
                            let mut tag = |text: &str, color: Color32| {
                                if !text.is_empty() {
                                    right -= tag_w(text) - 10.0;
                                    put(ui, right, mid + 0.5, f32::INFINITY, text, 11.5, color);
                                    right -= 10.0;
                                }
                            };
                            tag(place, DIM());
                            if better {
                                tag("Better than yours", GREEN());
                            }
                            tag(why, ACCENT2());
                            if with_marks {
                                for m in l.marks.iter().rev() {
                                    tag(m, FAINT());
                                }
                            }
                            put(ui, x, mid, (right - x).max(40.0), &c.name, 14.0, TEXT());
                            // the facts, each in its column
                            let mut x = rect.left() + 10.0 + cols.name;
                            let facts_of: [(&str, Color32); 6] = [
                                (c.format.as_str(), if l.weak { WARN() } else { TEXT() }),
                                (l.tracks.as_str(), if l.short { WARN() } else { DIM() }),
                                (l.size.as_str(), DIM()),
                                (l.speed.as_str(), DIM()),
                                (l.wait.as_str(), TEXT()),
                                (c.user.as_str(), DIM()),
                            ];
                            for (n, (text, color)) in facts_of.into_iter().enumerate() {
                                if cols.w[n] == 0.0 {
                                    continue;
                                }
                                let said = put(ui, x, mid, cols.w[n] - 12.0, text, 13.5, color);
                                if n == 1 && l.short && over {
                                    ui.interact(said, id.with("short"), Sense::hover()).on_hover_text("Fewer tracks than the album has");
                                }
                                x += cols.w[n];
                            }
                            // where it stands, or the one word that asks for it (whether the search has ended or not)
                            match snap.groups.iter().find(|g| g.user == l.key.0 && g.folder == l.key.1) {
                                Some(g) if g.arrived() => drop(put(ui, x, mid, cols.w[6], "Arrived", 13.5, GREEN())),
                                Some(g) if g.finished() => drop(put(ui, x, mid, cols.w[6], if g.refused == g.files { "Refused" } else { "Did not arrive" }, 13.5, RED())),
                                Some(g) => drop(put(ui, x, mid, cols.w[6], &format!("Arriving {:.0}%", g.frac() * 100.0), 13.5, ACCENT2())),
                                None if asked.contains(&l.key) => drop(put(ui, x, mid, cols.w[6], "Asked for", 13.5, DIM())),
                                None => {
                                    if (is_best || over || chosen) && download_word(ui, id.with("get"), x, mid, is_best) {
                                        get = Some(l.at);
                                    }
                                }
                            }
                        }
                        if open && ui.is_rect_visible(under_rect) {
                            if !files.contains_key(&l.key) {
                                let words = c.files.iter().filter(|f| is_audio(&f.filename)).map(|f| [file_words(&f.filename).to_string(), if f.bitrate > 0 { format!("{} kbps", f.bitrate) } else { String::new() }, if f.secs > 0 { length_words(f.secs) } else { String::new() }, size_words(f.size)]).collect();
                                files.insert(l.key.clone(), words);
                            }
                            let mut y = under_rect.top() + 2.0 + FILE_ROW / 2.0;
                            let x0 = under_rect.left() + 10.0;
                            if hidden {
                                // what the narrow card's columns left out
                                put(ui, x0 + 22.0, y, cols.name - 40.0, &format!("from {}  \u{b7}  {}", c.user, l.speed), 12.5, DIM());
                                y += FILE_ROW;
                            }
                            for f in files.get(&l.key).into_iter().flatten() {
                                if ui.is_rect_visible(Rect::from_center_size(pos2(under_rect.center().x, y), vec2(total, FILE_ROW))) {
                                    put(ui, x0 + 22.0, y, cols.name - 40.0, &f[0], 12.5, DIM());
                                    let mut x = x0 + cols.name;
                                    for n in 0..3 {
                                        if cols.w[n] > 0.0 {
                                            put(ui, x, y, cols.w[n] - 12.0, &f[n + 1], 12.0, FAINT());
                                            x += cols.w[n];
                                        }
                                    }
                                }
                                y += FILE_ROW;
                            }
                            if pictures > 0 {
                                let words: Cow<str> = if pictures == 1 { "and 1 picture".into() } else { format!("and {pictures} pictures").into() };
                                put(ui, x0 + 22.0, y, cols.name, &words, 12.0, FAINT());
                            }
                        }
                    }
                });
                // (the column names count as part of the list: a pointer resting just above the first row is on it)
                listing.rect = Some(list.inner_rect.union(names));
            }
        }
        let rect = Rect::from_min_max(pos2(left, top), pos2(left + wide, (card.min_rect().bottom() + PAD.y).min(floor)));
        ui.painter().set(fill, egui::epaint::RectShape::filled(rect, egui::CornerRadius::same(theme::CARD_ROUND), theme::CARD()));
        ui.advance_cursor_after_rect(rect);

        if clear_filter {
            facts.filter.clear();
        }
        let text = kept.text.clone();
        let wanted = get.and_then(|at| kept.candidates.get(at).cloned());
        if stop {
            let _ = self.tx.send(Cmd::CloseSearch(text.clone()));
        }
        if again {
            self.search(&text);
        }
        // the one place a copy is asked for: the row whose "Download" was clicked, or Enter on the marked row
        if let Some(at) = get {
            debug_assert!(wanted.is_some(), "copy {at} is listed");
            if let Some(c) = &wanted {
                self.fetch(c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_columns_fit_and_a_narrow_card_lets_go_of_the_least_needed() {
        // roomy: every column, the name no wider than the eye travels well, the rest shared out
        let wide = cols(1320.0);
        assert!(wide.w.iter().all(|w| *w > 0.0));
        assert_eq!(wide.name, 560.0);
        assert!(wide.name + wide.w.iter().sum::<f32>() <= 1320.0);
        assert!(wide.w[0] > 96.0 && wide.w[6] == 96.0, "the facts share what is left, the action keeps its width");
        // narrower: "from" goes first, then their speed, then the size; the name never drops under its least
        let mid = cols(860.0);
        assert!(mid.w[5] == 0.0 && mid.w[3] > 0.0 && mid.name >= 300.0, "{mid:?}");
        let narrow = cols(760.0);
        assert!(narrow.w[5] == 0.0 && narrow.w[3] == 0.0 && narrow.w[2] > 0.0 && narrow.name >= 300.0, "{narrow:?}");
        let tight = cols(560.0);
        assert!(tight.w[2] == 0.0 && tight.w[0] > 0.0 && tight.w[1] > 0.0 && tight.w[4] > 0.0 && tight.w[6] > 0.0, "{tight:?}");
        for inner in [560.0f32, 700.0, 860.0, 1000.0, 1320.0] {
            let c = cols(inner);
            assert!(c.name + c.w.iter().sum::<f32>() <= inner.max(140.0 + c.w.iter().sum::<f32>()), "{inner}: {c:?}");
        }
    }

    #[test]
    fn a_weak_format_is_192_and_under() {
        for weak in ["MP3 192", "MP3 128", "M4A 160"] {
            assert!(is_weak(weak), "{weak}");
        }
        for fine in ["FLAC", "MP3 320", "MP3 256", "MP3", "WAV", "MP3 245", "OGG"] {
            assert!(!is_weak(fine), "{fine}");
        }
    }

    #[test]
    fn a_track_is_named_without_its_folders_but_with_its_disc() {
        assert_eq!(file_words("@@abc\\Music\\Album (2020)\\01 Intro.flac"), "01 Intro.flac");
        assert_eq!(file_words("Music\\Album\\CD2\\01 Intro.flac"), "CD2\\01 Intro.flac");
        assert_eq!(file_words("01 Intro.flac"), "01 Intro.flac");
        assert_eq!(file_words(""), "");
        assert!(is_audio("a\\b\\01 Intro.FLAC") && is_audio("x.mp3"));
        assert!(!is_audio("cover.jpg") && !is_audio("no extension") && !is_audio("odd.extensionthatislong"));
    }

    fn copy(user: &str, name: &str, format: &str, tracks: usize, free: bool, queue: u64) -> Candidate {
        Candidate { user: user.into(), name: name.into(), files: vec![], tracks, bytes: 100_000_000, format: format.into(), free_slot: free, speed: 2_000_000, queue, score: 0 }
    }

    #[test]
    fn words_in_the_filter_box_narrow_the_list() {
        let mut candidates = vec![copy("ann", "Album (2020) [FLAC]", "FLAC", 14, true, 0), copy("bob", "Album", "MP3 192", 11, false, 12), copy("cy", "Album - Single", "MP3 320", 2, true, 0)];
        candidates[1].files = vec![crate::slskd::RemoteFile { filename: "@@x\\Music\\Album\\03 Night Drive.mp3".into(), size: 1, bitrate: 192, secs: 200 }];
        let kept = Kept { text: "artist album".into(), candidates, found_at: 1, complete: true, run: Run::Done, run_id: 1, answers: 3, error: None, fresh: true, asked_at: None, version: 7, part: 1.0 };
        let mut l = Listing::default();
        let mut listed = |l: &mut Listing, typed: &str, kind: Kind, n: u64| {
            l.typed = typed.into();
            relist(l, &kept, (7, Sort::Best, kind, false, n, 0), false, None, None, &[]);
            l.lines.iter().map(|x| x.key.0.clone()).collect::<Vec<_>>()
        };
        // a format's name goes by the format, and the count of what the format choices alone would list stays
        assert_eq!(listed(&mut l, "flac", Kind::All, 1), ["ann"]);
        assert_eq!(l.before_typed, 3);
        // a word that is only in a track's name keeps the copy, and the row names the track
        assert_eq!(listed(&mut l, "night", Kind::All, 2), ["bob"]);
        assert_eq!(l.lines[0].why, "03 Night Drive");
        // together with a format choice: both have to hold
        assert_eq!(listed(&mut l, "-single", Kind::Mp3, 3), ["bob"]);
        assert_eq!(l.before_typed, 2);
        assert_eq!(listed(&mut l, "nothing has this", Kind::All, 4), [] as [&str; 0]);
        assert_eq!(l.before_typed, 3);
        // an empty box, or one with only spaces, narrows nothing and names no track
        assert_eq!(listed(&mut l, "  ", Kind::All, 5), ["ann", "bob", "cy"]);
        assert!(l.lines.iter().all(|x| x.why.is_empty()));
        assert_ne!(text_stamp("flac"), text_stamp("flac "));
    }

    #[test]
    fn the_list_says_only_what_differs_and_is_put_together_once() {
        let kept = Kept {
            text: "artist album".into(),
            candidates: vec![copy("ann", "Album (2020) [FLAC]", "FLAC", 14, true, 0), copy("bob", "Album", "MP3 192", 11, false, 12), copy("cy", "Album - Single", "MP3 320", 2, true, 0)],
            found_at: 1,
            complete: true,
            run: Run::Done,
            run_id: 1,
            answers: 3,
            error: None,
            fresh: true,
            asked_at: None,
            version: 7,
            part: 1.0,
        };
        let album = Album { artist: "Artist".into(), title: "Album".into(), year: "2020".into(), editions: vec![14], artwork: String::new() };
        let mut l = Listing::default();
        let stamp = (kept.version, Sort::Best, Kind::All, false, 0, 0);
        relist(&mut l, &kept, stamp, false, Some(&album), Some(Quality::of_format("MP3 192")), &[]);
        assert_eq!(l.lines.len(), 3);
        // a copy that starts now says nothing about a wait; one with a line says how long it is
        assert_eq!((l.lines[0].wait.as_str(), l.lines[1].wait.as_str()), ("", "12 ahead"));
        // only the weak format and the short copy are marked
        assert_eq!(l.lines.iter().map(|x| (x.weak, x.short)).collect::<Vec<_>>(), vec![(false, false), (true, true), (false, true)]);
        assert_eq!((l.lines[0].tracks.as_str(), l.lines[1].tracks.as_str()), ("14 of 14", "11 of 14"));
        // "better than yours" only on a whole copy that is a clear step up
        assert_eq!(l.lines.iter().map(|x| x.better).collect::<Vec<_>>(), vec![true, false, false]);
        assert_eq!(l.best, Some(("ann".to_string(), "Album (2020) [FLAC]".to_string())));
        // whole albums only; and a copy that was tried and did not arrive sinks to the bottom
        relist(&mut l, &kept, (7, Sort::Best, Kind::All, true, 0, 0), false, Some(&album), None, &[]);
        assert_eq!(l.lines.iter().map(|x| x.key.0.as_str()).collect::<Vec<_>>(), vec!["ann"]);
        let failed = Group { user: "ann".into(), folder: "Album (2020) [FLAC]".into(), files: 14, done: 0, failed: 14, refused: 14, bytes: 1, done_bytes: 0, speed: 0.0, all_ids: vec![], live_ids: vec![], parts: vec![] };
        relist(&mut l, &kept, (7, Sort::Best, Kind::All, false, 0, 1), false, Some(&album), None, &[failed]);
        assert_eq!(l.lines.iter().map(|x| x.key.0.as_str()).collect::<Vec<_>>(), vec!["bob", "cy", "ann"]);
        // the filter by format
        relist(&mut l, &kept, (7, Sort::Best, Kind::Lossless, false, 0, 1), false, Some(&album), None, &[]);
        assert_eq!(l.lines.iter().map(|x| x.key.0.as_str()).collect::<Vec<_>>(), vec!["ann"]);
        // put together while the pointer is on the list: it is held, to be put in order once the pointer leaves
        relist(&mut l, &kept, (7, Sort::Best, Kind::All, false, 0, 2), true, Some(&album), None, &[]);
        assert!(l.held);
        relist(&mut l, &kept, (7, Sort::Best, Kind::All, false, 0, 2), false, Some(&album), None, &[]);
        assert!(!l.held && l.late == 0);
        // a click on a column's name is made with the pointer on the list: the new order is taken at once,
        // and none of the copies counts as having arrived late
        relist(&mut l, &kept, (7, Sort::Tracks, Kind::All, false, 0, 2), true, Some(&album), None, &[]);
        assert_eq!((l.late, l.held), (0, false));
        assert_eq!(l.lines.iter().map(|x| x.key.0.as_str()).collect::<Vec<_>>(), vec!["ann", "bob", "cy"]);
        // while a copy that arrives with the pointer still there goes below and is counted
        let mut more = Kept { candidates: kept.candidates.clone(), version: 8, ..kept.without_copies() };
        more.candidates.insert(0, copy("dee", "Album (Deluxe)", "FLAC", 18, true, 0));
        relist(&mut l, &more, (8, Sort::Tracks, Kind::All, false, 0, 2), true, Some(&album), None, &[]);
        assert_eq!((l.late, l.held), (1, true));
        assert_eq!(l.lines.last().map(|x| x.key.0.as_str()), Some("dee"));
    }
}
