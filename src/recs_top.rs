//! The top of the Recs page, in the middle of the page itself (it used to be a bar of its own under the
//! page names): the three lists as large words, the one showing bright and each with its number beside it;
//! one faint line under them that says when the lists were last brought up to date, with "Refresh" right
//! beside it, or what a refresh is doing and how far it is, with "Stop"; and the thin line under a list's
//! controls, which fills while a refresh runs.
use crate::library_page::commas;
use crate::theme::{self, ACCENT2, BORDER, DIM, FAINT, TEXT};
use crate::{importer, script, App, Tab};
use chrono::{DateTime, Local, TimeZone};
use egui::{pos2, vec2, Align, Color32, FontId, Layout, Rect, Sense, Ui, UiBuilder};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

/// How far each source of a refresh is: (done, of how many, finished).
type Progress = BTreeMap<&'static str, (usize, usize, bool)>;

/// The three words in a roomy window and in ever narrower ones: (their size, the size of the number
/// beside each, the room between two of them). The first that fits is used.
const CUTS: [(f32, f32, f32); 4] = [(32.0, 15.0, 46.0), (26.0, 13.5, 34.0), (21.0, 12.5, 24.0), (17.0, 11.5, 16.0)];

/// The faint line under the three words.
pub(crate) struct Line {
    /// a refresh is running: a spinner leads the line
    pub busy: bool,
    /// what it says, part by part; a part is a step brighter when it is news rather than a fact
    pub words: Vec<(String, bool)>,
    /// the word at its end ("Refresh", or "Stop" while one runs)
    pub act: &'static str,
    /// that word is the page's loud one: only when there are no lists yet, so that it is the one thing to do
    pub loud: bool,
}

/// What the line under the three words says. `status` and `progress` are a refresh's own words and counts,
/// `generated` and `ts` say when the lists on screen were made.
pub(crate) fn line_words(running: bool, status: &str, progress: &Progress, generated: &str, ts: i64, now: DateTime<Local>) -> Line {
    if running {
        let doing = status.trim_end_matches(&['…', '.'][..]).trim();
        let mut words = vec![(if doing.is_empty() { "Refreshing".to_string() } else { doing.to_string() }, true)];
        if let Some(far) = how_far(progress) {
            words.push((far, false));
        }
        return Line { busy: true, words, act: "Stop", loud: false };
    }
    if generated.is_empty() && ts <= 0 {
        return Line { busy: false, words: vec![("Nothing has been looked up yet".to_string(), false)], act: "Refresh", loud: true };
    }
    let mut words = vec![];
    // (a refresh that was stopped leaves the lists as they were, so the time after it is still theirs)
    if status == "Stopped." {
        words.push(("Stopped".to_string(), true));
    }
    let when = Some(ts).filter(|t| *t > 0).and_then(|t| Local.timestamp_opt(t, 0).single()).map(|t| when_words(t, now)).unwrap_or_else(|| generated.to_string());
    words.push((format!("Updated {when}"), false));
    Line { busy: false, words, act: "Refresh", loud: false }
}

/// "today at 9:31 PM", "yesterday at 9:31 PM", "Nov 8 at 2:01 PM", "Nov 8, 2025 at 2:01 PM".
pub(crate) fn when_words(then: DateTime<Local>, now: DateTime<Local>) -> String {
    use chrono::Datelike;
    let time = then.format("%-I:%M %p");
    match (then.date_naive() - now.date_naive()).num_days() {
        0 => format!("today at {time}"),
        -1 => format!("yesterday at {time}"),
        _ if then.year() == now.year() => format!("{} at {time}", then.format("%b %-d")),
        _ => format!("{} at {time}", then.format("%b %-d, %Y")),
    }
}

/// How far a refresh is, in words: the count of the source that is furthest behind, since the refresh is
/// over when that one is. None when no source is counting yet, or all are done.
pub(crate) fn how_far(progress: &Progress) -> Option<String> {
    progress
        .values()
        .filter(|(_, total, finished)| !finished && *total > 0)
        .min_by(|a, b| (a.0 as f64 / a.1 as f64).total_cmp(&(b.0 as f64 / b.1 as f64)))
        .map(|(done, total, _)| format!("{} of {} artists", commas(*done as u64), commas(*total as u64)))
}

/// How far a refresh is over all its sources together, for the thin line. None before any has counted.
pub(crate) fn overall(progress: &Progress) -> Option<f32> {
    let (done, total) = progress.values().fold((0usize, 0usize), |(d, t), (done, total, _)| (d + (*done).min(*total), t + total));
    (total > 0).then(|| done as f32 / total as f32)
}

/// Each source's own count, one to a line: what pointing at the line's words shows.
pub(crate) fn sources(progress: &Progress) -> String {
    let line = |(task, (done, total, finished)): (&&str, &(usize, usize, bool))| {
        if *finished {
            format!("{task}: done")
        } else {
            format!("{task}: {} of {}", commas(*done as u64), commas(*total as u64))
        }
    };
    progress.iter().map(line).collect::<Vec<_>>().join("\n")
}

/// A test run with `--recs-demo` draws the page as it looks in the middle of a refresh (nothing is looked
/// up), so that look can be seen without going online.
fn demo() -> Option<&'static Progress> {
    static ON: std::sync::OnceLock<Option<Progress>> = std::sync::OnceLock::new();
    ON.get_or_init(|| {
        (importer::test_folders() && std::env::args().any(|a| a == "--recs-demo"))
            .then(|| BTreeMap::from([("New releases (Apple Music)", (655, 1226, false)), ("New releases (MusicBrainz)", (412, 1226, false)), ("Similar artists (Last.fm)", (60, 60, true))]))
    })
    .as_ref()
}

/// The three lists and the line under them. Drawn first on the page, over whichever list is showing.
pub(crate) fn top(app: &mut App, ui: &mut Ui) {
    ui.add_space(10.0);
    lists(app, ui);
    ui.add_space(4.0);
    under(app, ui);
    ui.add_space(16.0);
}

/// "New releases", "Artists to try" and "Your artists" in the light cut of the font, in the middle. The
/// one showing is bright; the others are gray, a step lighter under the pointer, and a click shows them.
fn lists(app: &mut App, ui: &mut Ui) {
    let (releases, similar) = {
        let hidden = &app.cfg.dismissed;
        (
            app.results.releases.iter().filter(|r| !hidden.contains(&r.key) && !app.cfg.muted.contains_key(&r.artist_key)).count(),
            app.results.similar.iter().filter(|r| !hidden.contains(&format!("artist|{}", r.key))).count(),
        )
    };
    let artists = app.results.artists.len();
    // before the first refresh there are no lists, and three zeros would say nothing
    let numbers = !app.results.generated.is_empty() || releases + similar + artists > 0;
    let all = [(Tab::Releases, "New releases", releases), (Tab::Similar, "Artists to try", similar), (Tab::Artists, "Your artists", artists)];
    let room = ui.available_rect_before_wrap();
    let lay = |size: f32, small: f32| {
        all.map(|(_, name, n)| {
            let word = ui.painter().layout_no_wrap(name.to_string(), theme::display(size), Color32::PLACEHOLDER);
            let count = numbers.then(|| ui.painter().layout_no_wrap(commas(n as u64), FontId::proportional(small), Color32::PLACEHOLDER));
            (word, count)
        })
    };
    let mut cut = 0;
    let (parts, beside, gap, total) = loop {
        let (size, small, gap) = CUTS[cut];
        let parts = lay(size, small);
        let beside = (size * 0.28).round();
        let total = parts.iter().map(|(word, count)| word.size().x + count.as_ref().map_or(0.0, |c| beside + c.size().x)).sum::<f32>() + gap * 2.0;
        if total <= room.width() || cut + 1 == CUTS.len() {
            break (parts, beside, gap, total);
        }
        cut += 1;
    };
    let high = parts[0].0.size().y;
    let top = ui.cursor().top();
    let mut x = (room.center().x - total / 2.0).max(room.left());
    for (i, ((tab, name, _), (word, count))) in all.iter().zip(parts).enumerate() {
        let wide = word.size().x + count.as_ref().map_or(0.0, |c| beside + c.size().x);
        let rect = Rect::from_min_size(pos2(x.round(), top), vec2(wide, high));
        let r = ui.interact(rect, ui.scope_id().with(("recs-list", i)), Sense::click());
        script::seen(name, rect);
        let chosen = app.tab == *tab;
        let (bright, quiet) = if chosen {
            (TEXT(), DIM())
        } else if r.hovered() {
            (DIM(), DIM())
        } else {
            (FAINT(), FAINT())
        };
        if let Some(count) = count {
            // the number stands on the word's line of writing
            let down = theme::baseline(&word) - theme::baseline(&count);
            ui.painter().galley(pos2(rect.left() + word.size().x + beside, top + down), count, quiet);
        }
        ui.painter().galley(rect.min, word, bright);
        if r.hovered() && !chosen {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if r.clicked() {
            app.tab = *tab;
        }
        x += wide + gap;
    }
    ui.advance_cursor_after_rect(Rect::from_min_size(pos2(room.left(), top), vec2(room.width(), high)));
}

/// The word at the end of the line for someone with no Last.fm login: it opens Settings at the logins.
const LASTFM_WORD: &str = "Connect Last.fm for more";

/// The faint line: a row of one height whatever it says, so the controls under it never move.
fn under(app: &mut App, ui: &mut Ui) {
    const GAP: f32 = 9.0;
    const SPIN: f32 = 11.0;
    const HIGH: f32 = 18.0;
    let made_up = demo();
    let mut line = match made_up {
        Some(progress) => line_words(true, "Looking for new releases and similar artists…", progress, "", 0, Local::now()),
        None => line_words(app.running, &app.status, &app.progress, &app.results.generated, app.results.generated_ts, Local::now()),
    };
    // "Stop" was clicked: a refresh ends at its next step, which can take a few seconds, so the word
    // says that it was heard
    let stopping = line.busy && made_up.is_none() && app.cancel.load(Ordering::Relaxed);
    if stopping {
        line.act = "Stopping";
    }
    let tip = match made_up {
        Some(progress) => sources(progress),
        None if app.running => sources(&app.progress),
        None => String::new(),
    };
    let font = FontId::proportional(12.5);
    let wide = |text: &str| ui.painter().layout_no_wrap(text.to_string(), font.clone(), Color32::PLACEHOLDER).size().x;
    let dot = wide("·");
    let lead = if line.busy { SPIN + GAP } else { 0.0 };
    let lastfm_word = !line.busy && made_up.is_none() && !app.cfg.has_lastfm();
    let fixed = lead + line.words.len() as f32 * (GAP + dot + GAP) + wide(line.act) + if lastfm_word { GAP + dot + GAP + wide(LASTFM_WORD) } else { 0.0 };
    let mut widths: Vec<f32> = line.words.iter().map(|(text, _)| wide(text)).collect();
    let room = ui.available_rect_before_wrap();
    // in a window too narrow for all of it, the first part is cut short
    let over = fixed + widths.iter().sum::<f32>() - room.width();
    if over > 0.0 {
        if let Some(first) = widths.first_mut() {
            *first = (*first - over).max(40.0);
        }
    }
    let total = fixed + widths.iter().sum::<f32>();
    let top = ui.cursor().top();
    let left = (room.center().x - total / 2.0).round().max(room.left());
    let place = Rect::from_min_size(pos2(left, top), vec2(total + 2.0, HIGH));
    let mut row = ui.new_child(UiBuilder::new().id_salt("recs-under").max_rect(place).layout(Layout::left_to_right(Align::Center)));
    row.spacing_mut().item_spacing.x = GAP;
    if line.busy {
        row.add(egui::Spinner::new().size(SPIN));
    }
    for ((text, bright), width) in line.words.iter().zip(&widths) {
        let r = say(&mut row, text, font.clone(), if *bright { DIM() } else { FAINT() }, *width);
        if !tip.is_empty() {
            r.on_hover_text(tip.as_str());
        }
        say(&mut row, "·", font.clone(), BORDER(), dot);
    }
    let clicked = if line.loud { theme::main_action_sized(&mut row, line.act, 12.5, true).clicked() } else { theme::quiet(&mut row, line.act, 12.5, !stopping).clicked() };
    // without a Last.fm login the lists come from the library alone: one quiet word says there is more
    if lastfm_word {
        say(&mut row, "·", font.clone(), BORDER(), dot);
        if theme::quiet(&mut row, LASTFM_WORD, 12.5, true).clicked() {
            crate::links::go(crate::links::Go::SettingsBasics);
        }
    }
    ui.advance_cursor_after_rect(Rect::from_min_size(pos2(room.left(), top), vec2(room.width(), HIGH)));
    if clicked && made_up.is_none() {
        if line.busy {
            app.cancel.store(true, Ordering::Relaxed);
        } else {
            let ctx = ui.ctx().clone();
            app.start(&ctx);
        }
    }
}

/// Words on one line, cut off with "…" at `width`.
fn say(ui: &mut Ui, text: &str, font: FontId, color: Color32, width: f32) -> egui::Response {
    let words = theme::cut(ui, text, font, color, width + 0.5);
    let (rect, r) = ui.allocate_exact_size(words.size(), Sense::hover());
    ui.painter().galley(rect.min, words, color);
    r
}

/// The thin line under a list's controls. It always has its room, so the covers stay where they are when a
/// refresh starts; while one runs it fills in blue with how far the sources are, taken together.
pub(crate) fn line(app: &App, ui: &mut Ui) {
    ui.add_space(10.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    let far = match demo() {
        Some(progress) => Some(overall(progress)),
        None => app.running.then(|| overall(&app.progress)),
    };
    if let Some(far) = far {
        ui.painter().rect_filled(rect, 2, BORDER());
        let part = match far {
            Some(f) => Rect::from_min_size(rect.min, vec2(rect.width() * f.clamp(0.0, 1.0), rect.height())),
            None => {
                // no source has counted yet: a short piece that travels along the line
                let t = (ui.input(|i| i.time) * 0.7).fract() as f32;
                let piece = rect.width() * 0.2;
                let x = rect.left() - piece + (rect.width() + piece) * t;
                ui.ctx().request_repaint();
                Rect::from_min_max(pos2(x.max(rect.left()), rect.top()), pos2((x + piece).min(rect.right()), rect.bottom()))
            }
        };
        ui.painter().rect_filled(part, 2, ACCENT2());
    }
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn the_time_of_the_last_refresh_reads_as_a_person_would_say_it() {
        let now = at(2026, 11, 15, 4, 40);
        assert_eq!(when_words(at(2026, 11, 15, 0, 5), now), "today at 12:05 AM");
        assert_eq!(when_words(at(2026, 11, 14, 21, 31), now), "yesterday at 9:31 PM");
        assert_eq!(when_words(at(2026, 11, 8, 14, 1), now), "Nov 8 at 2:01 PM");
        assert_eq!(when_words(at(2025, 12, 30, 9, 0), now), "Dec 30, 2025 at 9:00 AM");
    }

    #[test]
    fn the_line_under_the_lists_says_how_they_stand() {
        let now = at(2026, 11, 15, 4, 40);
        let none = Progress::new();
        let words = |l: &Line| l.words.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(" | ");

        // no lists yet: "Refresh" is the one thing to do
        let never = line_words(false, "", &none, "", 0, now);
        assert_eq!((words(&never).as_str(), never.act, never.loud, never.busy), ("Nothing has been looked up yet", "Refresh", true, false));

        // lists from yesterday: the time, and "Refresh" as a quiet word
        let ts = at(2026, 11, 14, 21, 31).timestamp();
        let fresh = line_words(false, "Done.", &none, "Nov 14, 2026 21:31", ts, now);
        assert_eq!((words(&fresh).as_str(), fresh.act, fresh.loud, fresh.busy), ("Updated yesterday at 9:31 PM", "Refresh", false, false));

        // lists saved before the time was kept as a number still say when, the way they wrote it down
        let old = line_words(false, "", &none, "Nov 2, 2026 08:00", 0, now);
        assert_eq!(words(&old), "Updated Nov 2, 2026 08:00");

        // a refresh that is running: what it is doing, how far the slowest source is, and "Stop"
        let going = Progress::from([("New releases (Apple Music)", (655, 1226, false)), ("New releases (MusicBrainz)", (412, 1226, false)), ("Similar artists (Last.fm)", (60, 60, true))]);
        let busy = line_words(true, "Looking for new releases and similar artists…", &going, "Nov 14, 2026 21:31", ts, now);
        assert_eq!((words(&busy).as_str(), busy.act, busy.loud, busy.busy), ("Looking for new releases and similar artists | 412 of 1,226 artists", "Stop", false, true));
        // before any source counts there is only what it is doing
        assert_eq!(words(&line_words(true, "Scanning D:\\Music…", &none, "", 0, now)), "Scanning D:\\Music");

        // one that was stopped says so, and the time is still that of the lists on screen
        assert_eq!(words(&line_words(false, "Stopped.", &going, "Nov 14, 2026 21:31", ts, now)), "Stopped | Updated yesterday at 9:31 PM");
    }

    #[test]
    fn the_thin_line_is_all_sources_together_and_each_has_its_own_count() {
        assert_eq!(overall(&Progress::new()), None);
        assert_eq!(how_far(&Progress::new()), None);
        let going = Progress::from([("New releases (Apple Music)", (300, 400, false)), ("New releases (MusicBrainz)", (100, 400, false)), ("Similar artists (Last.fm)", (200, 200, true))]);
        assert_eq!(overall(&going), Some(0.6));
        assert_eq!(how_far(&going).as_deref(), Some("100 of 400 artists"));
        assert_eq!(sources(&going), "New releases (Apple Music): 300 of 400\nNew releases (MusicBrainz): 100 of 400\nSimilar artists (Last.fm): done");
        // all done: nothing left to count, and the line is full
        let done = Progress::from([("New releases (MusicBrainz)", (400, 400, true))]);
        assert_eq!((how_far(&done), overall(&done)), (None, Some(1.0)));
    }
}
