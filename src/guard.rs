//! The library guard: one rule set for everything Cadence's engine changes in the music folder.
//!
//! Every writer (`music.get`, `music.put_in`, `repair.apply`, `history.undo`, `tags.edit`, Organize, and
//! later Backup's bring-back) goes through it, before Cadence's hold is asked for and again right before each
//! change. The rules (ENGINE-PROTOCOL.md, "The library guard"):
//!   * a music folder on a network drive (a `\\server\share` path, or a drive letter Windows says is remote)
//!     is only read: the whole job is refused with `guarded`. A UNC path is told by its spelling alone, so
//!     the guard itself never reaches the network;
//!   * on a removable drive changes are allowed, but the folder must still be there before each one;
//!   * the folder (or an album's folder) gone mid-run stops the job at once with `gone`;
//!   * a read-only file is left alone and listed (its flag is never cleared);
//!   * a file open in another program is left alone and listed;
//!   * the file Cadence is playing (asked with `library.playing`) waits until it is no longer playing,
//!     checking every couple of seconds, for up to 10 minutes; still playing then, it is left alone and
//!     listed ("It was still playing."). Cancel works while it waits.
//!
//! How a writer uses it:
//! ```ignore
//! let guard = Guard::for_library(&music)?;          // guarded / not_found before anything is asked
//! // ... library.hold ...
//! guard.still_there()?;                             // before each change: `gone` if the drive went away
//! if let Err(why) = guard.file_ok(&file) { problems.push(format!("{}: {}", leaf, why.words())) }
//! if let Some(p) = guard.wait_not_playing(&|p| guard::covers(&album, p), job, out, id, waits)? { /* leave p's change out */ }
//! ```
use crate::service::{Job, Out};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// (code, sentence), as every engine error.
pub(crate) type Failure = (String, String);

fn fail(code: &str, message: impl Into<String>) -> Failure {
    (code.to_string(), message.into())
}

pub(crate) const NETWORK_WORDS: &str = "Your music is on a network drive. Cadence only reads it there; repairs and moves are off.";
pub(crate) const GONE_WORDS: &str = "The music folder went away (was the drive unplugged?). Nothing more was changed; what was done is in Repair history.";

/// Where a music folder is, for the guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Local,
    Removable,
    Network,
    Missing,
}

impl Place {
    /// The name Cadence gets (`library.guard`).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Place::Local => "ok",
            Place::Removable => "removable",
            Place::Network => "network",
            Place::Missing => "missing",
        }
    }
}

/// A `\\server\share` path (also written `\\?\UNC\server\share`). A `\\?\C:\` path is a local one.
fn is_unc(p: &str) -> bool {
    let p = p.replace('/', "\\");
    if let Some(rest) = p.strip_prefix(r"\\?\").or_else(|| p.strip_prefix(r"\\.\")) {
        return rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(r"UNC\");
    }
    p.starts_with(r"\\")
}

/// The drive letter's root ("X:\") of a local path, None for anything else.
fn drive_root(p: &str) -> Option<String> {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    let b = p.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| format!("{}:\\", b[0] as char))
}

/// Where a folder is. The spelling decides a UNC path (nothing is asked over the network); a drive letter is
/// asked of Windows (`GetDriveTypeW`, which answers from what it knows without reaching the share).
pub(crate) fn place(folder: &Path) -> Place {
    let text = folder.to_string_lossy();
    if is_unc(&text) {
        return Place::Network;
    }
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_REMOTE: u32 = 4;
    let kind = drive_root(&text).map(|root| {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = std::ffi::OsStr::new(&root).encode_wide().chain(std::iter::once(0)).collect();
        unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) }
    });
    match kind {
        Some(DRIVE_REMOTE) => Place::Network,
        Some(DRIVE_NO_ROOT_DIR) => Place::Missing,
        _ if !folder.is_dir() => Place::Missing,
        Some(DRIVE_REMOVABLE) => Place::Removable,
        _ => Place::Local,
    }
}

/// The words for `library.guard`.
pub(crate) fn place_words(p: Place, folder: &Path) -> String {
    match p {
        Place::Local => "Cadence can make the changes you ask for here.".to_string(),
        Place::Removable => "Your music is on a removable drive. Keep it plugged in while Cadence changes anything.".to_string(),
        Place::Network => NETWORK_WORDS.to_string(),
        Place::Missing => format!("The music folder {} could not be found.", folder.display()),
    }
}

/// Why one file is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    ReadOnly,
    Open,
}

impl Reason {
    pub(crate) fn words(self) -> &'static str {
        match self {
            Reason::ReadOnly => "Read-only: Cadence leaves read-only files alone.",
            Reason::Open => "Open in another program.",
        }
    }
}

/// How long the guard waits for Cadence's answers, and how often it looks again at the playing file.
#[derive(Clone, Copy)]
pub(crate) struct Waits {
    /// for one answer to `library.playing`
    pub(crate) ask: Duration,
    /// between two looks while a file waits to finish playing
    pub(crate) every: Duration,
    /// the longest a change waits for its file to finish playing (10 minutes, as `organize.album`)
    pub(crate) limit: Duration,
}

/// The words for a change left out because its file was still playing at the end of the wait.
pub(crate) const STILL_PLAYING: &str = "It was still playing, so it was left as it is.";

/// The guard over one music folder, for one job.
pub(crate) struct Guard {
    pub(crate) library: PathBuf,
    /// where it is (a removable drive is let through, and looked at again before each change)
    #[allow(dead_code)]
    pub(crate) place: Place,
}

/// `p` is `target` or lies inside it (case does not matter, as on Windows).
pub(crate) fn covers(target: &Path, p: &Path) -> bool {
    let (t, p) = (target.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase(), p.to_string_lossy().to_lowercase());
    !t.is_empty() && (p == t || p.strip_prefix(&t).is_some_and(|rest| rest.starts_with(['\\', '/'])))
}

impl Guard {
    /// The guard for a music folder: refused at once with `guarded` on a network drive, `not_found` when it
    /// is not there. Asks nothing of Cadence.
    pub(crate) fn for_library(library: &Path) -> Result<Guard, Failure> {
        match place(library) {
            Place::Network => Err(fail("guarded", NETWORK_WORDS)),
            Place::Missing => Err(fail("not_found", format!("The music folder {} could not be found.", library.display()))),
            place => Ok(Guard { library: library.to_path_buf(), place }),
        }
    }

    /// The music folder is still there (before each change). `gone` otherwise.
    pub(crate) fn still_there(&self) -> Result<(), Failure> {
        if self.library.is_dir() { Ok(()) } else { Err(fail("gone", GONE_WORDS)) }
    }

    /// Whether this file may be changed: not read-only, not open in another program. A file that is not
    /// there is the writer's own business (it says "no longer there" in its own words).
    pub(crate) fn file_ok(&self, file: &Path) -> Result<(), Reason> {
        let Ok(meta) = std::fs::metadata(file) else { return Ok(()) };
        if meta.permissions().readonly() {
            return Err(Reason::ReadOnly);
        }
        if meta.is_file() && open_elsewhere(file) {
            return Err(Reason::Open);
        }
        Ok(())
    }

    /// The first file in a folder (looked at to any depth) that may not be changed, with why. For a move of
    /// a whole folder: one read-only or open file keeps the folder where it is.
    pub(crate) fn folder_ok(&self, folder: &Path) -> Result<(), (PathBuf, Reason)> {
        if !folder.is_dir() {
            return self.file_ok(folder).map_err(|r| (folder.to_path_buf(), r));
        }
        for f in crate::importer::util::walk_files(folder) {
            self.file_ok(&f).map_err(|r| (f.clone(), r))?;
        }
        Ok(())
    }

    /// Waits while the file Cadence is playing is one this change touches (`touches` says which), asking
    /// `library.playing` again every `waits.every`, for at most `waits.limit`. Progress says what it waits
    /// for. Ok(None): nothing it touches is playing (any more), go on. Ok(Some(file)): `file` was still
    /// playing at the limit, and the caller leaves what touches it alone (`STILL_PLAYING`). Ends with
    /// `cancelled` when the job is stopped and `gone` when the music folder goes away meanwhile. When Cadence
    /// cannot say what is playing, the change goes on: a file the player holds open is still caught by
    /// `file_ok`.
    pub(crate) fn wait_not_playing(&self, touches: &dyn Fn(&Path) -> bool, job: &Job, out: &Out, id: u64, waits: Waits) -> Result<Option<PathBuf>, Failure> {
        let mut said = false;
        let limit = Instant::now() + waits.limit;
        loop {
            self.still_there()?;
            if job.stopped() {
                return Err(fail("cancelled", "It was stopped; what was done is in Repair history."));
            }
            let playing = match out.ask("library.playing", json!({}), waits.ask, &|| job.stopped()) {
                Ok(v) => v.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()).map(PathBuf::from),
                Err((code, _)) if code == "cancelled" => return Err(fail("cancelled", "It was stopped; what was done is in Repair history.")),
                Err((_, why)) => {
                    eprintln!("cadence-engine: Cadence did not say what is playing ({why}); going on");
                    None
                }
            };
            let Some(p) = playing.filter(|p| touches(p)) else { return Ok(None) };
            if Instant::now() >= limit {
                return Ok(Some(p));
            }
            if !said {
                let song = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                out.progress(id, json!({ "words": format!("Waiting for {song} to finish playing"), "waiting": true }));
                said = true;
            }
            let until = Instant::now() + waits.every;
            while Instant::now() < until {
                if job.stopped() {
                    return Err(fail("cancelled", "It was stopped while it waited for a song to finish playing; what was done is in Repair history."));
                }
                self.still_there()?;
                std::thread::sleep(Duration::from_millis(50).min(waits.every));
            }
        }
    }
}

/// Some other program has the file open (the test the repairs make: an open with no sharing fails).
pub(crate) fn open_elsewhere(file: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).share_mode(0).open(file).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_paths_are_told_by_their_spelling() {
        assert!(is_unc(r"\\server\share\Music") && is_unc(r"\\?\UNC\server\share") && is_unc("//server/share"));
        assert!(!is_unc(r"\\?\C:\Music") && !is_unc(r"C:\Music"));
        assert_eq!(place(Path::new(r"\\nowhere.invalid\share\Music")), Place::Network);
        assert_eq!(drive_root(r"\\?\q:\Songs").as_deref(), Some(r"q:\"));
        assert!(covers(Path::new(r"X:\Music\A"), Path::new(r"x:\music\a\01.mp3")) && !covers(Path::new(r"X:\Music\A"), Path::new(r"X:\Music\AB\01.mp3")));
    }
}
