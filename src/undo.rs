//! The undo backups: where an old copy goes when something in the library is replaced.
//!
//! Nothing in the library is deleted. A replaced album is moved (never copied, never removed) into a folder
//! on the library's own drive, in the same layout Library Health's repairs use, so it shows in the same
//! Repair history and the same Undo puts it back:
//!
//!   <drive>\.music-repair-backups\<yyyyMMdd-HHmmss-xxxxxx>\batch.json   what this was and when
//!                                                        \log.jsonl    one line per change, oldest first
//!                                                        \items\<slot>\<the old folder, under its own name>
//!
//! Undo reads the log backwards: a "created" line moves what was added out of the library, a "removed" line
//! moves the old copy back to where it was.
use std::io::Write;
use std::path::{Path, PathBuf};

fn rand_hex(digits: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let n = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{n:016x}")[..digits.min(16)].to_string()
}

/// Where the undo backups for this library live: the hidden folder at the top of the library's drive (a move
/// into it is instant, and it is where Library Health keeps its own). On the Windows drive the top is
/// Windows' own and not the place for an app's folder, so there it is beside the library's folder instead
/// (the same drive, so a move is still instant).
pub fn root_for(library: &Path) -> PathBuf {
    root_for_on(library, &std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()))
}

fn root_for_on(library: &Path, system_drive: &str) -> PathBuf {
    let top = library.ancestors().last().filter(|p| !p.as_os_str().is_empty()).unwrap_or(library);
    let drive = top.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
    if !system_drive.is_empty() && drive.eq_ignore_ascii_case(system_drive.trim_end_matches(['\\', '/'])) {
        if let Some(beside) = library.parent() {
            return beside.join(".music-repair-backups");
        }
    }
    top.join(".music-repair-backups")
}

fn hide(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN);
    }
}

/// Moves the old copy of an album into the undo backups and writes down how to put it back.
///   * `now_at`: where the old copy is at this moment (it has already been moved aside)
///   * `was_at`: where it was in the library, which is where Undo returns it to
///   * `new_at`: the album that took its place, which Undo moves out first
/// Returns where the old copy is kept. On an error nothing has been lost: the old copy is still at `now_at`
/// or already in the backups.
pub fn keep_replaced(root: &Path, library: &Path, title: &str, now_at: &Path, was_at: &Path, new_at: &Path) -> Result<PathBuf, String> {
    let leaf = was_at.file_name().ok_or("the old copy has no folder name")?;
    let now = chrono::Local::now();
    let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), rand_hex(6));
    let dir = root.join(&id);
    let slot = dir.join("items").join(format!("{}-{}", now.format("%H%M%S%3f"), rand_hex(8)));
    let fresh_root = !root.exists();
    std::fs::create_dir_all(&slot).map_err(|e| crate::util::plain_err(format!("couldn't make the undo backup folder: {e}")))?;
    if fresh_root {
        hide(root);
    }
    let batch = serde_json::json!({ "Id": id, "Title": title, "When": now.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string(), "Library": library.to_string_lossy() });
    std::fs::write(dir.join("batch.json"), batch.to_string()).map_err(|e| crate::util::plain_err(format!("couldn't write the undo record: {e}")))?;
    let kept = slot.join(leaf);
    if let Err(e) = std::fs::rename(now_at, &kept) {
        let _ = std::fs::remove_dir_all(&dir); // nothing was moved: leave no empty record behind
        return Err(crate::util::plain_err(format!("couldn't move the old copy into the undo backups: {e}")));
    }
    let removed = serde_json::json!({ "Kind": "removed", "Original": was_at.to_string_lossy(), "Backup": kept.to_string_lossy(), "Reason": "replaced by a new import" });
    let created = serde_json::json!({ "Kind": "created", "Path": new_at.to_string_lossy() });
    let lines = format!("{removed}\n{created}\n");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log.jsonl"))
        .and_then(|mut f| f.write_all(lines.as_bytes()))
        .map_err(|e| crate::util::plain_err(format!("the old copy is safe at {} but the undo record couldn't be written: {e}", kept.display())))?;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backups_sit_at_the_top_of_the_librarys_drive() {
        assert_eq!(root_for(Path::new(r"M:\Music")), PathBuf::from(r"M:\.music-repair-backups"));
        assert_eq!(root_for_on(Path::new(r"D:\Users\x\Music\Library"), "C:"), PathBuf::from(r"D:\.music-repair-backups"));
        // on the Windows drive: beside the library, never at the top of the drive
        assert_eq!(root_for_on(Path::new(r"C:\Users\x\Music\Library"), "C:"), PathBuf::from(r"C:\Users\x\Music\.music-repair-backups"));
        assert_eq!(root_for_on(Path::new(r"c:\Music"), "C:"), PathBuf::from(r"c:\.music-repair-backups"));
        assert_eq!(root_for_on(Path::new(r"M:\Music"), "C:"), PathBuf::from(r"M:\.music-repair-backups"));
    }

    #[test]
    fn a_replaced_album_is_moved_whole_and_written_down() {
        let base = std::env::temp_dir().join(format!("music-recs-undo-{}-{}", std::process::id(), rand_hex(6)));
        let (lib, root) = (base.join("lib"), base.join("backups"));
        let (was, aside, new) = (lib.join("A").join("Album"), lib.join("A").join("Album.replaced-1"), lib.join("A").join("Album (2020)"));
        std::fs::create_dir_all(aside.join("CD1")).unwrap();
        std::fs::write(aside.join("CD1").join("01 old.mp3"), b"old").unwrap();
        std::fs::create_dir_all(&new).unwrap();

        let kept = keep_replaced(&root, &lib, "Import: replaced A - Album", &aside, &was, &new).unwrap();
        assert!(!aside.exists() && kept.starts_with(&root));
        assert_eq!(kept.file_name(), was.file_name());
        assert_eq!(std::fs::read(kept.join("CD1").join("01 old.mp3")).unwrap(), b"old");

        let batch_dir = std::fs::read_dir(&root).unwrap().flatten().next().unwrap().path();
        let batch: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(batch_dir.join("batch.json")).unwrap()).unwrap();
        assert_eq!(batch["Id"].as_str(), batch_dir.file_name().and_then(|n| n.to_str()));
        assert_eq!(batch["Title"], "Import: replaced A - Album");
        let log = std::fs::read_to_string(batch_dir.join("log.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        // Undo reads backwards: the new album leaves first, then the old one returns to where it was
        assert_eq!((lines[0]["Kind"].as_str(), lines[0]["Original"].as_str()), (Some("removed"), was.to_str()));
        assert_eq!(lines[0]["Backup"].as_str(), kept.to_str());
        assert_eq!((lines[1]["Kind"].as_str(), lines[1]["Path"].as_str()), (Some("created"), new.to_str()));

        // the old copy can't be moved (it isn't there): an error, and no empty record is left
        assert!(keep_replaced(&root, &lib, "x", &lib.join("nope"), &was, &new).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }
}

// ------------------------------------------------------------------ a repair's journal, and Undo
//
// A repair is one batch: every change it makes is written to the batch's log before it is made, so a
// repair that is cut off can still be undone, and Undo can be run again after being cut off itself.
// The lines are the ones Library Health writes (`LibraryRepair.ps1`), so either program can undo the
// other's batches:
//   { Kind: "removed",  Original, Backup, Reason }   something moved out of the library into the batch
//   { Kind: "moved",    From, To, Reason }           something moved inside the library
//   { Kind: "replaced", Original, Backup, Reason }   a file swapped for a fixed copy; Backup is the old one
//   { Kind: "created",  Path, Reason }               a file the repair added
//   { Kind: "madedir",  Path }                       a folder the repair made
// Nothing is ever deleted, by a repair or by Undo: what leaves the library goes into the batch's folder.

/// A repair in progress.
pub struct Batch {
    pub id: String,
    pub dir: PathBuf,
    changes: std::cell::Cell<usize>,
}

/// One past repair, for Repair history.
#[derive(Clone, Debug)]
pub struct BatchInfo {
    pub id: String,
    pub title: String,
    pub when: Option<chrono::DateTime<chrono::Local>>,
    pub changes: usize,
    pub undone: bool,
}

fn text(v: &serde_json::Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false)
}

impl Batch {
    /// Starts a batch: its folder and the note of what it is. Nothing in the library has changed yet.
    pub fn begin(root: &Path, library: &Path, title: &str) -> Result<Batch, String> {
        let now = chrono::Local::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), rand_hex(6));
        let dir = root.join(&id);
        let fresh_root = !root.exists();
        std::fs::create_dir_all(dir.join("items")).map_err(|e| crate::util::plain_err(format!("couldn't make the undo backup folder: {e}")))?;
        if fresh_root {
            hide(root);
        }
        let batch = serde_json::json!({ "Id": id, "Title": title, "When": now.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string(), "Library": library.to_string_lossy() });
        std::fs::write(dir.join("batch.json"), batch.to_string()).map_err(|e| crate::util::plain_err(format!("couldn't write the undo record: {e}")))?;
        Ok(Batch { id, dir, changes: Default::default() })
    }

    /// How many changes the batch has made.
    pub fn changes(&self) -> usize {
        self.changes.get()
    }

    /// A batch that changed nothing leaves no record behind.
    pub fn close(self) {
        if self.changes.get() == 0 {
            let _ = std::fs::remove_dir(self.dir.join("items"));
            let _ = std::fs::remove_file(self.dir.join("batch.json"));
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    fn log(&self, entry: serde_json::Value) -> Result<(), String> {
        let line = format!("{entry}\n");
        std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("log.jsonl")).and_then(|mut f| f.write_all(line.as_bytes())).map_err(|e| crate::util::plain_err(format!("the undo record couldn't be written, so nothing was changed: {e}")))
    }

    /// A place of its own inside the batch for something leaving the library, under its own name.
    fn slot(&self, original: &Path) -> Result<PathBuf, String> {
        let slot = self.dir.join("items").join(format!("{}-{}", chrono::Local::now().format("%H%M%S%3f"), rand_hex(8)));
        std::fs::create_dir_all(&slot).map_err(|e| crate::util::plain_err(format!("couldn't make room in the undo backups: {e}")))?;
        Ok(slot.join(original.file_name().ok_or("it has no name")?))
    }

    /// Moves a file or folder out of the library into the batch. Undo puts it back.
    pub fn remove(&self, path: &Path, reason: &str) -> Result<PathBuf, String> {
        let kept = self.slot(path)?;
        self.log(serde_json::json!({ "Kind": "removed", "Original": path.to_string_lossy(), "Backup": kept.to_string_lossy(), "Reason": reason }))?;
        std::fs::rename(path, &kept).map_err(|e| crate::util::plain_err(format!("couldn't move it into the undo backups: {e}")))?;
        self.changes.set(self.changes.get() + 1);
        Ok(kept)
    }

    /// Moves a file or folder to another place in the library, making the folder it goes into if need be.
    /// Never onto something that is already there.
    pub fn moved(&self, from: &Path, to: &Path, reason: &str) -> Result<(), String> {
        if to.exists() {
            return Err(format!("something is already at {}", to.display()));
        }
        if let Some(parent) = to.parent().filter(|p| !p.exists()) {
            // every folder that has to be made, outermost first, so Undo can take each away again
            let mut missing: Vec<&Path> = parent.ancestors().take_while(|p| !p.exists()).collect();
            missing.reverse();
            for dir in missing {
                self.log(serde_json::json!({ "Kind": "madedir", "Path": dir.to_string_lossy() }))?;
                std::fs::create_dir(dir).map_err(|e| crate::util::plain_err(format!("couldn't make the folder {}: {e}", dir.display())))?;
            }
        }
        self.log(serde_json::json!({ "Kind": "moved", "From": from.to_string_lossy(), "To": to.to_string_lossy(), "Reason": reason }))?;
        std::fs::rename(from, to).map_err(|e| crate::util::plain_err(format!("couldn't move it: {e}")))?;
        self.changes.set(self.changes.get() + 1);
        Ok(())
    }

    /// Swaps a file for a fixed copy of it: the old one goes into the batch, the fixed copy takes its
    /// place. `fixed` must be on the same drive (the batch's own folder is). If the copy cannot be put in
    /// place the old file goes straight back.
    pub fn replaced(&self, original: &Path, fixed: &Path, reason: &str) -> Result<(), String> {
        let kept = self.slot(original)?;
        self.log(serde_json::json!({ "Kind": "replaced", "Original": original.to_string_lossy(), "Backup": kept.to_string_lossy(), "Reason": reason }))?;
        std::fs::rename(original, &kept).map_err(|e| crate::util::plain_err(format!("couldn't move the old file into the undo backups: {e}")))?;
        if let Err(e) = std::fs::rename(fixed, original) {
            let back = std::fs::rename(&kept, original);
            return Err(match back {
                Ok(()) => crate::util::plain_err(format!("couldn't put the fixed copy in place, so the file is as it was: {e}")),
                Err(e2) => crate::util::plain_err(format!("couldn't put the fixed copy in place ({e}), and the old file is still in the undo backups at {} ({e2})", kept.display())),
            });
        }
        // a fresh time, so players and the next check see that the file has changed
        if let Ok(f) = std::fs::File::options().write(true).open(original) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        self.changes.set(self.changes.get() + 1);
        Ok(())
    }

    /// A file whose lyrics alone were changed: the fixed copy takes its place, and what is kept is the
    /// words the lyrics field held before (`old`, empty when it held none), not the old file. Only for a
    /// copy the tag editor has proved to differ from the file in nothing but its lyrics. Undo puts the
    /// old words back (see `repair::undo`). If the copy cannot be put in place the file is as it was.
    pub fn lyrics_changed(&self, original: &Path, fixed: &Path, old: &str, new: &str, reason: &str) -> Result<(), String> {
        self.log(serde_json::json!({ "Kind": "lyrics", "File": original.to_string_lossy(), "Old": old, "New": new, "Reason": reason }))?;
        // (one step: the name is taken from the old file and given to the fixed copy)
        std::fs::rename(fixed, original).map_err(|e| crate::util::plain_err(format!("couldn't put the fixed copy in place, so the file is as it was: {e}")))?;
        if let Ok(f) = std::fs::File::options().write(true).open(original) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        self.changes.set(self.changes.get() + 1);
        Ok(())
    }

    /// Puts a new file into the library (a cover picture): a copy of `source` at `path`. Never over
    /// something that is there. Undo moves it into the batch.
    pub fn created(&self, path: &Path, source: &Path, reason: &str) -> Result<(), String> {
        if path.exists() {
            return Err(format!("{} is already there", path.display()));
        }
        self.log(serde_json::json!({ "Kind": "created", "Path": path.to_string_lossy(), "Reason": reason }))?;
        std::fs::copy(source, path).map_err(|e| crate::util::plain_err(format!("couldn't save it: {e}")))?;
        self.changes.set(self.changes.get() + 1);
        Ok(())
    }

    /// After something left a folder: the folders that are now empty, up to but not including the library
    /// folder, go into the batch too.
    pub fn tidy_parents(&self, path: &Path, library: &Path) {
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == library || !d.starts_with(library) || !is_empty_dir(d) || self.remove(d, "left empty by the repair").is_err() {
                break;
            }
            dir = d.parent();
        }
    }
}

/// Every repair there is a record of, newest first. A record that cannot be read whole is still listed
/// with what could be read (one torn line must not hide the others).
pub fn batches(root: &Path) -> Vec<BatchInfo> {
    let mut out: Vec<BatchInfo> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let dir = e.path();
            let meta: serde_json::Value = serde_json::from_str(std::fs::read_to_string(dir.join("batch.json")).ok()?.trim_start_matches('\u{feff}')).ok()?;
            let id = leaf(&dir);
            Some(BatchInfo {
                title: text(&meta, "Title"),
                when: chrono::DateTime::parse_from_rfc3339(&text(&meta, "When")).ok().map(|d| d.with_timezone(&chrono::Local)),
                changes: entries(&dir).len(),
                undone: dir.join("undone.txt").exists(),
                id,
            })
        })
        .collect();
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// The lyrics a repair changed without keeping the old file, newest first: (the file, the words its lyrics
/// field held before, the words the repair wrote). An error for a repair that is not one or was undone.
pub fn lyric_lines(root: &Path, id: &str) -> Result<Vec<(PathBuf, String, String)>, String> {
    if id.is_empty() || id.contains(['\\', '/', ':']) || id.starts_with('.') {
        return Err("That is not a repair.".into());
    }
    let dir = root.join(id);
    if dir.join("undone.txt").exists() {
        return Err("This repair was already undone.".into());
    }
    let mut out: Vec<(PathBuf, String, String)> = entries(&dir).iter().filter(|e| e["Kind"] == "lyrics").map(|e| (PathBuf::from(text(e, "File")), text(e, "Old"), text(e, "New"))).collect();
    out.reverse();
    Ok(out)
}

/// A repair's backups may be cleared once they are this old.
pub const CLEAR_AFTER_DAYS: i64 = 30;

/// The repairs made more than `days` days before `now`: the ones whose backups may be cleared. A repair
/// with no readable date is never one of them.
pub fn old_batches(root: &Path, days: i64, now: chrono::DateTime<chrono::Local>) -> Vec<BatchInfo> {
    batches(root).into_iter().filter(|b| b.when.is_some_and(|w| now.signed_duration_since(w) > chrono::Duration::days(days))).collect()
}

/// Clears the backups of every repair older than `days` days: each one's whole folder is handed to `bin`,
/// which sends it to the Recycle Bin (the app's is `to_recycle_bin`). Nothing is deleted here, and a folder
/// the bin did not take stays where it is and is named in what comes back: (repairs cleared, what could not be).
/// After this those repairs can no longer be undone from Repair history; their files are in the Recycle Bin.
pub fn clear_old(root: &Path, days: i64, now: chrono::DateTime<chrono::Local>, bin: &dyn Fn(&Path) -> Result<(), String>) -> (usize, Vec<String>) {
    let (mut cleared, mut problems) = (0, vec![]);
    for b in old_batches(root, days, now) {
        let dir = root.join(&b.id);
        // only a repair's own folder, straight inside the backups folder
        if b.id.is_empty() || b.id.contains(['\\', '/', ':']) || b.id.starts_with('.') || !dir.join("batch.json").is_file() {
            continue;
        }
        match bin(&dir) {
            Ok(()) if !dir.exists() => cleared += 1,
            Ok(()) => problems.push(format!("'{}' is still there: the Recycle Bin did not take it", b.title)),
            Err(why) => problems.push(format!("'{}' could not go to the Recycle Bin: {why}", b.title)),
        }
    }
    (cleared, problems)
}

/// Sends a folder to the Windows Recycle Bin. A folder the bin cannot hold is refused, never deleted.
pub fn to_recycle_bin(dir: &Path) -> Result<(), String> {
    trash::delete(dir).map_err(|e| e.to_string())
}

/// The lines of a batch's log that can be read, oldest first.
fn entries(dir: &Path) -> Vec<serde_json::Value> {
    let log = std::fs::read(dir.join("log.jsonl")).unwrap_or_default();
    String::from_utf8_lossy(&log).lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim().trim_start_matches('\u{feff}')).ok()).filter(|v| v.is_object()).collect()
}

/// Every place in the library a repair changed, each once: where a file or folder was taken from, where one
/// was moved to, and what was made. The Health page looks at those folders again after a repair or an Undo.
pub fn touched(root: &Path, id: &str) -> Vec<PathBuf> {
    if id.is_empty() || id.contains(['\\', '/', ':']) || id.starts_with('.') {
        return vec![];
    }
    let mut out: Vec<PathBuf> = vec![];
    for e in entries(&root.join(id)) {
        for k in ["Original", "From", "To", "Path"] {
            let p = text(&e, k);
            if !p.is_empty() && !out.iter().any(|o| o.as_os_str().eq_ignore_ascii_case(&p)) {
                out.push(PathBuf::from(p));
            }
        }
    }
    out
}

/// Puts everything a repair changed back the way it was, the newest change first. Returns what could not
/// be put back, in plain words; with none, the batch is marked undone. Nothing is deleted: what the repair
/// put in the library moves into the batch's folder. Safe to run again after being cut off.
pub fn undo(root: &Path, id: &str) -> Result<Vec<String>, String> {
    if id.is_empty() || id.contains(['\\', '/', ':']) || id.starts_with('.') {
        return Err("That is not a repair.".into());
    }
    let dir = root.join(id);
    if dir.join("undone.txt").exists() {
        return Err("This repair was already undone.".into());
    }
    let mut list = entries(&dir);
    if list.is_empty() {
        return Err("There is nothing to undo for this repair.".into());
    }
    list.reverse();
    let aside = |p: &Path| dir.join(format!("undone-{}-{}", rand_hex(6), leaf(p)));
    let make_parent = |p: &Path| p.parent().map(std::fs::create_dir_all).transpose().map(|_| ());
    let mut problems = vec![];
    for e in &list {
        let (original, backup) = (PathBuf::from(text(e, "Original")), PathBuf::from(text(e, "Backup")));
        let (from, to, path) = (PathBuf::from(text(e, "From")), PathBuf::from(text(e, "To")), PathBuf::from(text(e, "Path")));
        let done: Result<(), String> = match e["Kind"].as_str().unwrap_or("") {
            // a file was swapped for a fixed copy: the fixed copy moves into the batch, the old one returns
            "replaced" => {
                if !backup.exists() {
                    // already put back (an undo that was cut off), or the swap never happened: the file in
                    // the library is the one to keep
                    if original.exists() { Ok(()) } else { Err(format!("{} is neither in the library nor in the backups", original.display())) }
                } else {
                    let parked = aside(&original);
                    let had = original.exists();
                    (if had { std::fs::rename(&original, &parked) } else { Ok(()) }).and_then(|_| std::fs::rename(&backup, &original)).map_err(|err| {
                        if had && !original.exists() {
                            let _ = std::fs::rename(&parked, &original); // the old one could not return: the fixed copy stays
                        }
                        format!("{} could not be put back: {err}", original.display())
                    })
                }
            }
            "removed" => {
                if !backup.exists() {
                    if original.exists() { Ok(()) } else { Err(format!("{} is neither in the library nor in the backups", original.display())) }
                } else if original.is_dir() && is_empty_dir(&backup) {
                    Ok(()) // a folder that was tidied away empty is there again: nothing of it is missing
                } else if original.exists() {
                    Err(format!("Something new is now at {}; the old one stays in the backups", original.display()))
                } else {
                    make_parent(&original).and_then(|_| std::fs::rename(&backup, &original)).map_err(|err| format!("{} could not be put back: {err}", original.display()))
                }
            }
            "moved" => {
                if !to.exists() {
                    if from.exists() { Ok(()) } else { Err(format!("{} is no longer where the repair put it", to.display())) }
                } else if from.exists() {
                    Err(format!("Something new is now at {}; left {} where it is", from.display(), to.display()))
                } else {
                    make_parent(&from).and_then(|_| std::fs::rename(&to, &from)).map_err(|err| format!("{} could not be moved back: {err}", to.display()))
                }
            }
            // a file the repair added (a cover picture): into the batch, not deleted
            "created" => {
                if path.exists() { std::fs::rename(&path, aside(&path)).map_err(|err| format!("{} could not be taken out: {err}", path.display())) } else { Ok(()) }
            }
            // a folder the repair made goes away once it is empty again (an empty folder holds nothing to lose)
            "madedir" => {
                if is_empty_dir(&path) {
                    let _ = std::fs::remove_dir(&path);
                }
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(why) = done {
            problems.push(why);
        }
    }
    // marked undone only when everything is back: with something left over, Undo can be run again
    if problems.is_empty() {
        std::fs::write(dir.join("undone.txt"), chrono::Local::now().to_rfc3339()).map_err(|e| crate::util::plain_err(format!("Everything was put back, but that could not be written down: {e}")))?;
    }
    Ok(problems)
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    fn lab(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("music-recs-journal-{name}-{}-{}", std::process::id(), rand_hex(6)));
        let (lib, root) = (base.join("lib"), base.join("backups"));
        std::fs::create_dir_all(&lib).unwrap();
        (base, lib, root)
    }
    fn put(p: &Path, what: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, what).unwrap();
    }
    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|_| "<gone>".into())
    }

    #[test]
    fn a_repair_names_every_place_in_the_library_it_changed() {
        let (base, lib, root) = lab("touched");
        let (loose, filed, junk, cover) = (lib.join(r"A\loose.mp3"), lib.join(r"A\Album\loose.mp3"), lib.join(r"B\junk.tmp"), lib.join(r"C\cover.jpg"));
        put(&loose, "a song");
        put(&junk, "junk");
        put(&lib.join(r"C\01.mp3"), "a song");
        put(&base.join("found.jpg"), "a picture");
        let b = Batch::begin(&root, &lib, "made up").unwrap();
        b.moved(&loose, &filed, "filed in its album").unwrap();
        b.remove(&junk, "junk").unwrap();
        b.created(&cover, &base.join("found.jpg"), "a cover").unwrap();
        let id = b.id.clone();
        b.close();
        let places = touched(&root, &id);
        // where the file was, where it went (the new album folder), what left, what was made
        for p in [&loose, &filed, &junk, &cover] {
            assert!(places.contains(p), "{} is not named in {places:?}", p.display());
        }
        // only places in the library: never the backups themselves
        assert!(places.iter().all(|p| p.starts_with(&lib)), "{places:?}");
        // the same after an Undo, and nothing for what is not a repair
        undo(&root, &id).unwrap();
        assert_eq!(touched(&root, &id), places);
        assert!(touched(&root, "").is_empty() && touched(&root, r"..\x").is_empty() && touched(&root, "20000101-000000-none").is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_repair_is_undone_newest_first_and_nothing_is_deleted() {
        let (base, lib, root) = lab("undo");
        let (junk, loose, empty) = (lib.join("A").join("Album").join("x.part"), lib.join("A").join("loose.mp3"), lib.join("A").join("Old").join("Empty"));
        put(&junk, "junk");
        put(&loose, "song");
        std::fs::create_dir_all(&empty).unwrap();
        let b = Batch::begin(&root, &lib, "Health: three things").unwrap();
        b.remove(&junk, "Junk file").unwrap();
        // a move makes the folders it needs, and never lands on something that is there
        let filed = lib.join("A").join("New Album (2020)").join("CD1").join("loose.mp3");
        b.moved(&loose, &filed, "file loose track").unwrap();
        put(&loose, "another song that arrived later");
        assert!(b.moved(&loose, &filed, "again").is_err());
        std::fs::remove_file(&loose).unwrap();
        // an emptied folder goes, and the folder above it that is now empty too; the library folder never
        b.remove(&empty, "Empty folder").unwrap();
        b.tidy_parents(&empty, &lib);
        assert!(!lib.join("A").join("Old").exists() && lib.join("A").exists());
        assert_eq!((b.changes(), read(&filed).as_str()), (4, "song"));
        let id = b.id.clone();
        b.close();

        let list = batches(&root);
        assert_eq!((list.len(), list[0].title.as_str(), list[0].changes, list[0].undone), (1, "Health: three things", 6, false));
        assert!(list[0].when.is_some());

        assert_eq!(undo(&root, &id).unwrap(), Vec::<String>::new());
        assert_eq!((read(&junk).as_str(), read(&loose).as_str()), ("junk", "song"));
        assert!(empty.is_dir() && !lib.join("A").join("New Album (2020)").exists());
        assert!(batches(&root)[0].undone);
        assert!(undo(&root, &id).is_err());
        assert!(undo(&root, "..").is_err() && undo(&root, "nope").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_undo_that_cannot_finish_says_so_and_can_be_run_again() {
        let (base, lib, root) = lab("again");
        let (one, two) = (lib.join("A").join("one.mp3"), lib.join("A").join("two.mp3"));
        put(&one, "one");
        put(&two, "two");
        let b = Batch::begin(&root, &lib, "x").unwrap();
        b.remove(&one, "r").unwrap();
        b.remove(&two, "r").unwrap();
        let id = b.id.clone();
        b.close();
        // something new took the place of one of them: it is left alone and the old one stays in the backups
        put(&two, "a newer two");
        let problems = undo(&root, &id).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!((read(&one).as_str(), read(&two).as_str()), ("one", "a newer two"));
        assert!(!batches(&root)[0].undone);
        // the way is cleared: the second run finishes, and what was already back is left as it is
        std::fs::remove_file(&two).unwrap();
        assert_eq!(undo(&root, &id).unwrap(), Vec::<String>::new());
        assert_eq!((read(&one).as_str(), read(&two).as_str()), ("one", "two"));
        assert!(batches(&root)[0].undone);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_backups_older_than_thirty_days_are_cleared_and_only_into_the_bin() {
        let (base, _lib, root) = lab("clear");
        let now = chrono::Local::now();
        let batch = |id: &str, days_ago: i64, title: &str| {
            let dir = root.join(id);
            put(&dir.join("batch.json"), &serde_json::json!({ "Id": id, "Title": title, "When": (now - chrono::Duration::days(days_ago)).to_rfc3339() }).to_string());
            put(&dir.join("items").join("1-aa").join("song.mp3"), "an old copy");
            put(&dir.join("log.jsonl"), &serde_json::json!({ "Kind": "removed", "Original": "x", "Backup": "y" }).to_string());
        };
        batch("20260901-120000-aaaaaa", 71, "Old one");
        batch("20261001-120000-bbbbbb", 41, "Old two");
        batch("20261020-120000-cccccc", 22, "Recent");
        batch("20261110-120000-dddddd", 1, "Yesterday");
        put(&root.join("20260101-000000-eeeeee").join("batch.json"), r#"{"Title":"No date"}"#);
        put(&root.join("not a repair").join("keep.txt"), "something else in the folder");
        assert_eq!(old_batches(&root, CLEAR_AFTER_DAYS, now).iter().map(|b| b.title.clone()).collect::<Vec<_>>(), vec!["Old two", "Old one"]);

        // the bin here is a folder beside the backups: what is cleared can be counted, and nothing is deleted
        let bin_dir = base.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = |dir: &Path| std::fs::rename(dir, bin_dir.join(dir.file_name().unwrap())).map_err(|e| e.to_string());
        assert_eq!(clear_old(&root, CLEAR_AFTER_DAYS, now, &bin), (2, vec![]));
        assert_eq!(read(&bin_dir.join("20260901-120000-aaaaaa").join("items").join("1-aa").join("song.mp3")), "an old copy");
        assert!(bin_dir.join("20261001-120000-bbbbbb").is_dir());
        let left: Vec<String> = batches(&root).into_iter().map(|b| b.title).collect();
        assert_eq!(left, vec!["Yesterday", "Recent", "No date"]);
        assert!(root.join("not a repair").join("keep.txt").is_file());
        // nothing old is left: a second click does nothing
        assert_eq!(clear_old(&root, CLEAR_AFTER_DAYS, now, &bin), (0, vec![]));

        // a bin that refuses (too large for it, say) leaves the backups where they are and says so
        batch("20260801-120000-ffffff", 100, "Too big");
        let (n, problems) = clear_old(&root, CLEAR_AFTER_DAYS, now, &|_| Err("no room".into()));
        assert_eq!(n, 0);
        assert!(problems.len() == 1 && problems[0].contains("Too big") && problems[0].contains("no room"), "{problems:?}");
        // and one that says yes but took nothing is not believed
        let (n, problems) = clear_old(&root, CLEAR_AFTER_DAYS, now, &|_| Ok(()));
        assert!(n == 0 && problems[0].contains("still there"), "{problems:?}");
        assert!(root.join("20260801-120000-ffffff").join("items").join("1-aa").join("song.mp3").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn batches_written_the_old_way_are_read_and_undone_and_a_torn_line_hides_nothing() {
        let (base, lib, root) = lab("theirs");
        let dir = root.join("20261107-120000-abcdef");
        let (song, kept, cover) = (lib.join("A").join("Album").join("01 song.mp3"), dir.join("items").join("120000123-aaaaaaaa").join("01 song.mp3"), lib.join("A").join("Album").join("cover.jpg"));
        put(&song, "the fixed copy");
        put(&kept, "the original");
        put(&cover, "a cover the repair added");
        // as `New-RepairBatch` and `Write-BatchLog` write them, the first with the marker PowerShell adds
        put(&dir.join("batch.json"), &format!("\u{feff}{}", serde_json::json!({ "Id": "20261107-120000-abcdef", "Title": "Junk in tags (1)", "When": "2026-11-07T12:00:00.0000000-05:00", "Library": lib.to_string_lossy() })));
        let lines = [
            serde_json::json!({ "Kind": "replaced", "Original": song.to_string_lossy(), "Backup": kept.to_string_lossy(), "Reason": "junk" }).to_string(),
            serde_json::json!({ "Kind": "created", "Path": cover.to_string_lossy(), "Reason": "cover" }).to_string(),
            r#"{"Kind":"replaced","Original":"M:\\half a li"#.to_string(),
        ];
        put(&dir.join("log.jsonl"), &lines.join("\n"));
        let list = batches(&root);
        assert_eq!((list.len(), list[0].title.as_str(), list[0].changes), (1, "Junk in tags (1)", 2));
        assert!(list[0].when.is_some());

        assert_eq!(undo(&root, "20261107-120000-abcdef").unwrap(), Vec::<String>::new());
        assert_eq!(read(&song), "the original");
        // the fixed copy and the added cover are in the batch's folder, not gone
        let parked: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("undone-")).collect();
        assert_eq!(parked.len(), 2, "{parked:?}");
        assert!(!cover.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
