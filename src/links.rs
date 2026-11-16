//! How pages point at each other without knowing each other.
//!
//! A page asks to go somewhere (`go`), and the app's shell carries it out on the next frame. The Health page
//! also publishes which album folders have open problems (`set_health_marks`), so other pages can show a mark.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// A request to show something on another page.
#[derive(Clone, Debug, PartialEq)]
pub enum Go {
    /// open this album in the Library page (its sidebar)
    LibraryAlbum { artist: String, album: String },
    /// show this album's problems in the Health page
    HealthAlbum { artist: String, album: String },
    /// show this artist in the Recs page (their panel)
    RecsArtist { artist: String },
    /// open Settings at Basics (the music folder, what the app needs, the Last.fm and Soulseek logins)
    SettingsBasics,
    /// the same, with the Windows picker for the music folder opened right away
    ChooseMusicFolder,
    /// open Settings at Files (the downloads folder, the backup tool, artist names, lyrics)
    SettingsFiles,
}

static NEXT: Mutex<Option<Go>> = Mutex::new(None);

pub fn go(to: Go) {
    *NEXT.lock().unwrap_or_else(|e| e.into_inner()) = Some(to);
}
pub fn take() -> Option<Go> {
    NEXT.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Folders in the library that something other than the Health page has just changed (an import put an
/// album there, or took loose tracks away). The Health page takes them up and checks them again.
static CHANGED: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

pub fn folder_changed(folder: &Path) {
    let mut list = CHANGED.lock().unwrap_or_else(|e| e.into_inner());
    if !list.iter().any(|f| f == folder) && list.len() < 500 {
        list.push(folder.to_path_buf());
    }
}

/// The next folder that changed, if any (taken off the list).
pub fn take_changed() -> Option<std::path::PathBuf> {
    let mut list = CHANGED.lock().unwrap_or_else(|e| e.into_inner());
    if list.is_empty() { None } else { Some(list.remove(0)) }
}

/// A long job has ended (a backup, a check, an import, a Recs refresh). When the app's window is not the
/// one in front, its taskbar button lights up and stays lit until the window is looked at, the way Windows
/// does it for a finished copy. No sound, no pop-up. Called from the job's own thread, so it works while
/// the window is minimized and draws nothing. A test run notes what it did in `job-ended.log` in its
/// data folder.
pub fn job_ended(what: &str) {
    let lit = light_taskbar_if_behind(crate::APP_NAME);
    if std::env::var_os("MUSIC_RECS_DATA").is_some_and(|v| !v.is_empty()) {
        use std::io::Write;
        let how = match lit {
            Some(true) => "the taskbar button was lit",
            Some(false) => "the window was in front",
            None => "no window was found",
        };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::config::app_dir().join("job-ended.log")) {
            let _ = writeln!(f, "{what}: {how}");
        }
    }
}

/// Lights the taskbar button of this program's window with that title, unless the window is in front.
/// Some(true) when it was lit, Some(false) when the window is in front, None when there is no such window.
fn light_taskbar_if_behind(title: &str) -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, FlashWindowEx, GetForegroundWindow, GetWindowThreadProcessId, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY};
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let window = FindWindowW(std::ptr::null(), wide.as_ptr());
        if window.is_null() {
            return None;
        }
        // (another program's window of the same name is never touched)
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, &mut pid);
        if pid != std::process::id() {
            return None;
        }
        if GetForegroundWindow() == window {
            return Some(false);
        }
        let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: window, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: 0, dwTimeout: 0 };
        FlashWindowEx(&info);
        Some(true)
    }
}

static MARKS: RwLock<Option<Arc<HashMap<String, u8>>>> = RwLock::new(None);

fn mark_key(folder: &Path) -> String {
    folder.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase()
}

/// The Health page calls this whenever it has read a report: album folder -> worst open problem there
/// (1 serious, 2 worth fixing, 3 minor). Ignored problems are left out.
pub fn set_health_marks(marks: impl IntoIterator<Item = (std::path::PathBuf, u8)>) {
    let mut map: HashMap<String, u8> = HashMap::new();
    for (folder, sev) in marks {
        let e = map.entry(mark_key(&folder)).or_insert(sev);
        *e = (*e).min(sev);
    }
    *MARKS.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(map));
}

/// The worst open problem in this album folder, if the Health page has a report and lists one.
pub fn health_mark(folder: &Path) -> Option<u8> {
    let marks = MARKS.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    marks.get(&mark_key(folder)).copied()
}

/// True once a health report has been read (so "no mark" means "no problem" rather than "not known").
pub fn health_known() -> bool {
    MARKS.read().unwrap_or_else(|e| e.into_inner()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_keep_the_worst_problem_per_folder() {
        set_health_marks([(r"M:\Music\A\One (2020)".into(), 3), (r"m:\music\a\one (2020)\".into(), 1), (r"M:\Music\B\Two".into(), 2)]);
        assert!(health_known());
        assert_eq!(health_mark(Path::new(r"M:\MUSIC\A\One (2020)")), Some(1));
        assert_eq!(health_mark(Path::new(r"M:\Music\B\Two")), Some(2));
        assert_eq!(health_mark(Path::new(r"M:\Music\C\Three")), None);
    }

    #[test]
    fn a_request_is_taken_once() {
        go(Go::RecsArtist { artist: "X".into() });
        assert_eq!(take(), Some(Go::RecsArtist { artist: "X".into() }));
        assert_eq!(take(), None);
    }
}
