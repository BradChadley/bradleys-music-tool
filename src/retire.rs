//! The Music Tool's last version: its tools now live in Cadence. A card on Home, and on the page the app
//! opens on, says so and has one action: "Get Cadence" (Cadence's newest release, in the browser), or
//! "Open Cadence" when Cadence is already installed for this Windows account.
//!
//! Nothing else changes: every page keeps working as it is, nothing is deleted or switched off, and the
//! nightly backup task is not touched. Whether Cadence is installed is told only by its program file being
//! there; it is never run, except when "Open Cadence" is clicked.
use crate::theme::{self, BG, DIM, TEXT};
use egui::{vec2, Ui};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Cadence's newest release, where its installer is (the page Cadence Installer installs it from).
pub const CADENCE_LATEST: &str = "https://github.com/BradChadley/Cadence-Releases/releases/latest";

/// The card's words: its caption, its statement, its quiet line.
pub const CAPTION: &str = "NOW PART OF CADENCE";
pub const STATEMENT: &str = "Bradley's Music Tool is now part of Cadence";
pub const LINE: &str = "Health, Get music, Recs and Backup, in one app. This app keeps working as it is, but it gets no more updates.";

/// The card's one action.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Cadence is not installed: its newest release page, in the browser
    Get,
    /// Cadence is installed: start it (its program file)
    Open(PathBuf),
}

impl Action {
    pub fn words(&self) -> &'static str {
        match self {
            Action::Get => "Get Cadence",
            Action::Open(_) => "Open Cadence",
        }
    }
}

/// The action for what is installed: Open when Cadence's program file is there, else Get.
pub fn choose(installed: Option<PathBuf>) -> Action {
    installed.map_or(Action::Get, Action::Open)
}

/// Cadence's program file, when Cadence is installed under `local_app_data` (Velopack's places:
/// `Cadence\Cadence.exe`, which starts the current version, else `Cadence\current\Cadence.exe`). Only
/// looks whether the file is there.
pub fn installed_in(local_app_data: &Path) -> Option<PathBuf> {
    let home = local_app_data.join("Cadence");
    [home.join("Cadence.exe"), home.join("current").join("Cadence.exe")].into_iter().find(|p| p.is_file())
}

/// `installed_in` for this Windows account.
pub fn installed() -> Option<PathBuf> {
    dirs::data_local_dir().and_then(|d| installed_in(&d))
}

/// Does the action. Get opens the release page; Open starts Cadence on its own (the Music Tool does not
/// wait for it), and falls back to the page if it cannot be started.
pub fn act(a: &Action) {
    match a {
        Action::Get => crate::util::open_url(CADENCE_LATEST),
        Action::Open(exe) => {
            let started = std::process::Command::new(exe).current_dir(exe.parent().unwrap_or(Path::new("."))).spawn();
            if let Err(e) = started {
                crate::config::log(&format!("retire: Cadence could not be started ({e}); opening its release page"));
                crate::util::open_url(CADENCE_LATEST);
            }
        }
    }
}

/// What the card knows of Cadence, looked at again now and then (it may get installed while the app is
/// open): a file check every few seconds at most.
pub struct Notice {
    action: Action,
    looked: Option<Instant>,
    look: fn() -> Option<PathBuf>,
}

impl Default for Notice {
    fn default() -> Self {
        Notice::with(installed)
    }
}

impl Notice {
    /// With its own "is Cadence installed" check (the tests' stand-in).
    pub fn with(look: fn() -> Option<PathBuf>) -> Notice {
        Notice { action: Action::Get, looked: None, look }
    }

    /// The action now.
    pub fn action(&mut self) -> &Action {
        if self.looked.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            self.action = choose((self.look)());
            self.looked = Some(Instant::now());
        }
        &self.action
    }

    /// The card, centered in a strip across the top of the page.
    pub fn show(&mut self, ui: &mut Ui) {
        let action = self.action().clone();
        let wide = ui.available_width().min(760.0);
        let left = ui.max_rect().center().x - wide / 2.0;
        theme::panel(ui, left, wide, vec2(28.0, 20.0), "now-in-cadence", |ui| {
            ui.vertical_centered(|ui| theme::caption(ui, CAPTION));
            ui.add_space(8.0);
            theme::centered(ui, STATEMENT, theme::display(24.0), TEXT());
            ui.add_space(6.0);
            theme::centered_wrapped(ui, LINE, 13.5, DIM(), wide - 56.0);
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                if theme::main_action(ui, action.words(), true).clicked() {
                    act(&action);
                }
            });
        });
    }
}

/// The strip's color: the page's own background, so the card sits on the page.
pub fn strip_fill() -> egui::Color32 {
    BG()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_says_where_the_tools_went_in_plain_words() {
        assert_eq!(STATEMENT, "Bradley's Music Tool is now part of Cadence");
        assert!(LINE.starts_with("Health, Get music, Recs and Backup, in one app."));
        for words in [CAPTION, STATEMENT, LINE, Action::Get.words(), Action::Open(PathBuf::new()).words()] {
            assert!(!words.contains('\u{2014}') && !words.contains('\u{2013}'), "{words}");
        }
        assert_eq!(CADENCE_LATEST, format!("{}/releases/latest", crate::update::CADENCE));
    }

    #[test]
    fn the_action_is_get_until_cadence_is_installed_then_open() {
        let base = std::env::temp_dir().join(format!("music-recs-retire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Cadence").join("current")).unwrap();
        // a folder alone is not an install
        assert_eq!(installed_in(&base), None);
        assert_eq!(choose(installed_in(&base)), Action::Get);
        assert_eq!(choose(None).words(), "Get Cadence");
        // Velopack's current version only
        let current = base.join("Cadence").join("current").join("Cadence.exe");
        std::fs::write(&current, b"not run").unwrap();
        assert_eq!(installed_in(&base), Some(current.clone()));
        // its starter, when there is one, comes first
        let starter = base.join("Cadence").join("Cadence.exe");
        std::fs::write(&starter, b"not run").unwrap();
        let a = choose(installed_in(&base));
        assert_eq!((a.words(), a), ("Open Cadence", Action::Open(starter)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_card_asks_its_check_now_and_then_not_every_frame() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static ASKED: AtomicUsize = AtomicUsize::new(0);
        fn installed_here() -> Option<PathBuf> {
            ASKED.fetch_add(1, Ordering::Relaxed);
            Some(PathBuf::from(r"X:\nowhere\Cadence.exe"))
        }
        let mut n = Notice::with(installed_here);
        for _ in 0..100 {
            assert_eq!(n.action(), &Action::Open(PathBuf::from(r"X:\nowhere\Cadence.exe")));
        }
        assert_eq!(ASKED.load(Ordering::Relaxed), 1);
        fn none() -> Option<PathBuf> {
            None
        }
        assert_eq!(Notice::with(none).action(), &Action::Get);
    }
}
