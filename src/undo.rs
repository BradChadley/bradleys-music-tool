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

/// An album being replaced by an import, written down before anything moves (Codex's review, H4: the old
/// album used to move first, and a record that then could not be written left no way back).
///
///   1. `begin`: the batch and its first line, synced to the disk: where the old album is, where it will be
///      set aside in the library for a moment, and where it will be kept. If this cannot be written, nothing
///      has moved and the import stops.
///   2. The importer moves the old album to `aside` and the new one into place (`swap_into_place`).
///   3. `finish`: the second line (the new album, which Undo takes out first), then the old album from
///      `aside` into the backups.
///
/// Undo reads whatever got written: the old album is found in the backups, or still at `aside`, or never
/// left. With only the first line (the second could not be written), Undo will not move the new album and
/// says so, and the old album stays safe in the backups.
pub struct Replacement {
    dir: PathBuf,
    was_at: PathBuf,
    new_at: PathBuf,
    aside: PathBuf,
    kept: PathBuf,
}

impl Replacement {
    /// Writes down how to undo putting `new_at` in place of the album at `was_at`. Nothing moves here.
    pub fn begin(root: &Path, library: &Path, title: &str, was_at: &Path, new_at: &Path) -> Result<Replacement, String> {
        let leaf = was_at.file_name().ok_or("the old copy has no folder name")?;
        let now = chrono::Local::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), rand_hex(6));
        let dir = root.join(&id);
        let slot = dir.join("items").join(format!("{}-{}", now.format("%H%M%S%3f"), rand_hex(8)));
        let aside = PathBuf::from(format!("{}.replaced-{}", was_at.display(), rand_hex(12)));
        let fresh_root = !root.exists();
        std::fs::create_dir_all(&slot).map_err(|e| crate::util::plain_err(format!("couldn't make the undo backup folder: {e}")))?;
        if fresh_root {
            hide(root);
        }
        let r = Replacement { dir, was_at: was_at.to_path_buf(), new_at: new_at.to_path_buf(), aside, kept: slot.join(leaf) };
        let batch = serde_json::json!({ "Id": id, "Title": title, "When": now.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string(), "Library": library.to_string_lossy() });
        let removed = serde_json::json!({ "Kind": "removed", "Original": r.was_at.to_string_lossy(), "Backup": r.kept.to_string_lossy(), "Aside": r.aside.to_string_lossy(), "Reason": "replaced by a new import" });
        let written = write_record(&r.dir, &batch).and_then(|_| r.append(&format!("{removed}\n")));
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&r.dir); // nothing has moved: leave no half record behind
            return Err(crate::util::plain_err(format!("the undo record couldn't be written, so the old album was not touched: {e}")));
        }
        Ok(r)
    }

    fn append(&self, lines: &str) -> std::io::Result<()> {
        #[cfg(test)]
        test_fault::before_log(&self.dir);
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("log.jsonl"))?;
        f.write_all(lines.as_bytes())?;
        f.sync_all() // on the disk before anything moves
    }

    /// Where the old album was, and where the importer sets it aside while the new one moves in.
    pub fn was_at(&self) -> &Path {
        &self.was_at
    }
    pub fn aside(&self) -> &Path {
        &self.aside
    }

    /// The new album is in place: the rest of the record, then the old album from `aside` into the backups.
    /// Returns where the old album is kept. An error is one sentence that says where the old album is now,
    /// and it is true: both steps are tried, and the record already names both places.
    pub fn finish(&self) -> Result<PathBuf, String> {
        let _held = crate::library_lock::hold();
        let created = serde_json::json!({ "Kind": "created", "Path": self.new_at.to_string_lossy() });
        let noted = self.append(&format!("{created}\n"));
        if let Err(e) = std::fs::rename(&self.aside, &self.kept) {
            return Err(crate::util::plain_err(format!("the old album couldn't be moved into the undo backups ({e}); it is safe at {}{}", self.aside.display(), if noted.is_ok() { ", and Undo in Repair history can still put it back" } else { "" })));
        }
        if let Err(e) = noted {
            return Err(crate::util::plain_err(format!("the old album is safe in the undo backups at {}, but the undo record couldn't be finished ({e}); to put it back, move the new album away first, then use Undo in Repair history", self.kept.display())));
        }
        Ok(self.kept.clone())
    }

    /// The swap did not happen. With the old album back where it was and nothing of it away, the record is
    /// taken out again (it would undo nothing); otherwise it stays, for Undo.
    pub fn cancel(self) {
        if self.was_at.exists() && !self.aside.exists() && !self.kept.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
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
        let (was, new) = (lib.join("A").join("Album"), lib.join("A").join("Album (2020)"));
        std::fs::create_dir_all(was.join("CD1")).unwrap();
        std::fs::write(was.join("CD1").join("01 old.mp3"), b"old").unwrap();

        // the record is written before anything moves: its first line names all three places
        let r = Replacement::begin(&root, &lib, "Import: replaced A - Album", &was, &new).unwrap();
        let aside = r.aside().to_path_buf();
        assert!(was.exists() && aside.starts_with(was.parent().unwrap()) && !aside.exists());
        let batch_dir = std::fs::read_dir(&root).unwrap().flatten().next().unwrap().path();
        let first: serde_json::Value = serde_json::from_str(std::fs::read_to_string(batch_dir.join("log.jsonl")).unwrap().trim()).unwrap();
        assert_eq!((first["Kind"].as_str(), first["Original"].as_str(), first["Aside"].as_str()), (Some("removed"), was.to_str(), aside.to_str()));
        // the importer's swap, then the rest
        std::fs::rename(&was, &aside).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let kept = r.finish().unwrap();
        assert!(!aside.exists() && kept.starts_with(&root));
        assert_eq!(kept.file_name(), was.file_name());
        assert_eq!(std::fs::read(kept.join("CD1").join("01 old.mp3")).unwrap(), b"old");

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

        // a swap that did not happen (the old album never left): the record is taken out again
        let other = lib.join("B").join("Album");
        std::fs::create_dir_all(&other).unwrap();
        Replacement::begin(&root, &lib, "x", &other, &other).unwrap().cancel();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        // and one where the old album is away is kept, for Undo
        let r = Replacement::begin(&root, &lib, "y", &other, &other).unwrap();
        std::fs::rename(&other, r.aside()).unwrap();
        r.cancel();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
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
    /// what made it: "repair", "edit" (Edit details), "organize", "import" (an import that replaced an
    /// album or tracks) or "restore" (Backup's bring-back). Written by `Batch::begin_kind`; a record from
    /// before that is told by its title (`kind_of`).
    pub kind: String,
    /// the library it was made in (empty in a record that does not say)
    pub library: String,
    /// every place in the library it changed, each once (`touched`), read in the same pass as the count
    pub places: Vec<PathBuf>,
}

/// What made a batch: the kind written in its record, else told by its title.
pub fn kind_of(written: &str, title: &str) -> String {
    if !written.trim().is_empty() {
        return written.trim().to_lowercase();
    }
    let t = title.trim_start().to_lowercase();
    if t.starts_with("import:") {
        "import"
    } else if t.starts_with("backup:") {
        "restore"
    } else if t.starts_with("organize") {
        "organize"
    } else if t.starts_with("edit details") {
        "edit"
    } else {
        "repair"
    }
    .to_string()
}

/// The places a batch's lines name, each once, in the order they were written.
fn places_in(list: &[serde_json::Value]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    for e in list {
        for k in ["Original", "From", "To", "Path", "File"] {
            let p = text(e, k);
            if !p.is_empty() && !out.iter().any(|o| o.as_os_str().eq_ignore_ascii_case(&p)) {
                out.push(PathBuf::from(p));
            }
        }
    }
    out
}

/// The moves inside the library a batch made, oldest first: (from, to).
pub fn moves(root: &Path, id: &str) -> Vec<(PathBuf, PathBuf)> {
    if id.is_empty() || id.contains(['\\', '/', ':']) || id.starts_with('.') {
        return vec![];
    }
    entries(&root.join(id)).iter().filter(|e| e["Kind"] == "moved").map(|e| (PathBuf::from(text(e, "From")), PathBuf::from(text(e, "To")))).filter(|(f, t)| !f.as_os_str().is_empty() && !t.as_os_str().is_empty()).collect()
}

fn text(v: &serde_json::Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Writes a batch's `batch.json` and waits until it is on the disk, with its folder's entry for it, before
/// the first line of the log is written: after a power cut a batch is never left with its log and a torn or
/// missing record (which hid it from Repair history, and its Undo with it).
pub(crate) fn write_record(dir: &Path, record: &serde_json::Value) -> std::io::Result<()> {
    let mut f = std::fs::File::create(dir.join("batch.json"))?;
    f.write_all(record.to_string().as_bytes())?;
    f.sync_all()?;
    sync_dir(dir);
    Ok(())
}

/// Asks Windows to put a folder's entries on the disk (best effort: NTFS journals them anyway, and a folder
/// that cannot be opened for this is not an error).
pub(crate) fn sync_dir(dir: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    if let Ok(d) = std::fs::OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir) {
        let _ = d.sync_all();
    }
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false)
}

impl Batch {
    /// Starts a batch: its folder and the note of what it is. Nothing in the library has changed yet.
    pub fn begin(root: &Path, library: &Path, title: &str) -> Result<Batch, String> {
        Batch::begin_kind(root, library, title, "")
    }

    /// `begin`, with what made the batch written into its record ("edit", "organize", ...; see
    /// `BatchInfo::kind`). "" writes none, and the kind is then told by the title.
    pub fn begin_kind(root: &Path, library: &Path, title: &str, kind: &str) -> Result<Batch, String> {
        let now = chrono::Local::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), rand_hex(6));
        let dir = root.join(&id);
        let fresh_root = !root.exists();
        std::fs::create_dir_all(dir.join("items")).map_err(|e| crate::util::plain_err(format!("couldn't make the undo backup folder: {e}")))?;
        if fresh_root {
            hide(root);
        }
        let mut batch = serde_json::json!({ "Id": id, "Title": title, "When": now.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string(), "Library": library.to_string_lossy() });
        if !kind.is_empty() {
            batch["Kind"] = serde_json::Value::String(kind.to_string());
        }
        write_record(&dir, &batch).map_err(|e| crate::util::plain_err(format!("couldn't write the undo record: {e}")))?;
        Ok(Batch { id, dir, changes: Default::default() })
    }

    /// How many changes the batch has made.
    pub fn changes(&self) -> usize {
        self.changes.get()
    }

    /// Writes one line to the batch's log and waits until it is on the disk, for a change the caller makes
    /// right after (Organize's write-ahead journal). If this fails, the caller must change nothing.
    pub fn journal(&self, entry: serde_json::Value) -> Result<(), String> {
        let line = format!("{entry}\n");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("log.jsonl"))
            .and_then(|mut f| f.write_all(line.as_bytes()).and_then(|_| f.sync_all()))
            .map_err(|e| crate::util::plain_err(format!("the undo record couldn't be written, so nothing was changed: {e}")))
    }

    /// A batch that is already written, to add lines to its log (Organize settling a run that was cut
    /// off). Never `close` it: it counts no changes of its own.
    pub fn reopen(dir: &Path) -> Batch {
        Batch { id: leaf(dir), dir: dir.to_path_buf(), changes: Default::default() }
    }

    /// Counts one change the caller made after writing it down with `journal`.
    pub fn count_change(&self) {
        self.changes.set(self.changes.get() + 1);
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
        let _held = crate::library_lock::hold();
        let kept = self.slot(path)?;
        self.log(serde_json::json!({ "Kind": "removed", "Original": path.to_string_lossy(), "Backup": kept.to_string_lossy(), "Reason": reason }))?;
        std::fs::rename(path, &kept).map_err(|e| crate::util::plain_err(format!("couldn't move it into the undo backups: {e}")))?;
        self.changes.set(self.changes.get() + 1);
        Ok(kept)
    }

    /// Moves a file or folder to another place in the library, making the folder it goes into if need be.
    /// Never onto something that is already there.
    pub fn moved(&self, from: &Path, to: &Path, reason: &str) -> Result<(), String> {
        let _held = crate::library_lock::hold();
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
        let _held = crate::library_lock::hold();
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
        let _held = crate::library_lock::hold();
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
        let _held = crate::library_lock::hold();
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
            let title = text(&meta, "Title");
            let kind = kind_of(&text(&meta, "Kind"), &title);
            let list = entries(&dir);
            // (an organize batch counts the albums it moved; "done" and "notdone" lines are notes, not changes)
            let changes = if kind == "organize" { list.iter().filter(|e| e["Kind"] == "done").count() } else { list.iter().filter(|e| e["Kind"] != "done" && e["Kind"] != "notdone").count() };
            Some(BatchInfo {
                library: text(&meta, "Library"),
                places: places_in(&list),
                title,
                when: chrono::DateTime::parse_from_rfc3339(&text(&meta, "When")).ok().map(|d| d.with_timezone(&chrono::Local)),
                changes,
                undone: dir.join("undone.txt").exists(),
                kind,
                id,
            })
        })
        .collect();
    // IDs have only whole seconds followed by random characters. Sorting them can reverse
    // two quick repairs, making a sequence of Undos finish at an intermediate file version.
    out.sort_by(|a, b| b.when.cmp(&a.when).then_with(|| b.id.cmp(&a.id)));
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
pub(crate) fn entries(dir: &Path) -> Vec<serde_json::Value> {
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
    undo_except(root, id, &|_| false, &|| false)
}

/// What an Undo that was stopped part way says: what is back stays back, and running it again finishes.
pub const STOPPED: &str = "Undo was stopped part way. What was put back stays back; Undo it again to put back the rest.";

/// `undo`, leaving alone every line about a place `skip` names (a file still playing, or a folder holding
/// it): each is said in what comes back, and the batch is not marked undone, so a later Undo finishes it.
pub fn undo_except(root: &Path, id: &str, skip: &dyn Fn(&Path) -> bool, stop: &dyn Fn() -> bool) -> Result<Vec<String>, String> {
    if id.is_empty() || id.contains(['\\', '/', ':']) || id.starts_with('.') {
        return Err("That is not a repair.".into());
    }
    let dir = root.join(id);
    if dir.join("undone.txt").exists() {
        return Err("This repair was already undone.".into());
    }
    // one writer to the library at a time (see `library_lock`)
    let _held = crate::library_lock::hold();
    let list = entries(&dir);
    if list.is_empty() {
        return Err("There is nothing to undo for this repair.".into());
    }
    // Each step that is put back is written down (`undo-progress.txt`, the step's place in the log), and a
    // second run skips it. Without that, a run cut off after the old album was back but before `undone.txt`
    // was written would take the album that is back for "what the import added" and move it away again
    // (Codex's review, H3). A step that cannot be written down stops the Undo there, so no step is ever
    // done without the record that it was.
    let progress_file = dir.join("undo-progress.txt");
    let already: std::collections::HashSet<usize> = std::fs::read_to_string(&progress_file).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect();
    let note_done = |n: usize| -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&progress_file).map_err(|e| crate::util::plain_err(format!("Undo stopped: it could not write down what it had put back, so it can be run again safely: {e}")))?;
        f.write_all(format!("{n}\n").as_bytes()).and_then(|_| f.sync_all()).map_err(|e| crate::util::plain_err(format!("Undo stopped: it could not write down what it had put back, so it can be run again safely: {e}")))
    };
    let aside = |p: &Path| dir.join(format!("undone-{}-{}", rand_hex(6), leaf(p)));
    let make_parent = |p: &Path| p.parent().map(std::fs::create_dir_all).transpose().map(|_| ());
    // Organize's moves found not to have happened (a "notdone" line names the step): nothing to put back
    let not_done: std::collections::HashSet<u64> = list.iter().filter(|e| e["Kind"] == "notdone").filter_map(|e| e["Step"].as_u64()).collect();
    let mut problems = vec![];
    for (n, e) in list.iter().enumerate().rev() {
        if already.contains(&n) || (e["Kind"] == "moved" && e["Step"].as_u64().is_some_and(|s| not_done.contains(&s))) {
            continue;
        }
        // a stop lands between lines: each line is wholly put back (and written down) or not touched, and
        // the batch is not marked undone, so a later Undo finishes it
        if stop() {
            problems.push(STOPPED.to_string());
            break;
        }
        let (original, backup) = (PathBuf::from(text(e, "Original")), PathBuf::from(text(e, "Backup")));
        let (from, to, path) = (PathBuf::from(text(e, "From")), PathBuf::from(text(e, "To")), PathBuf::from(text(e, "Path")));
        if let Some(p) = [&original, &to, &from, &path, &PathBuf::from(text(e, "File"))].into_iter().find(|p| !p.as_os_str().is_empty() && skip(p)) {
            problems.push(format!("{}: {}", p.display(), crate::guard::STILL_PLAYING));
            continue;
        }
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
                // (an album an import replaced may still be where it was set aside, beside its old place)
                let set_aside = PathBuf::from(text(e, "Aside"));
                if !set_aside.as_os_str().is_empty() && set_aside.exists() {
                    if original.exists() {
                        Err(format!("Something new is now at {}; the old one is still beside it, at {}", original.display(), set_aside.display()))
                    } else {
                        std::fs::rename(&set_aside, &original).map_err(|err| format!("{} could not be put back: {err}", original.display()))
                    }
                } else if !backup.exists() {
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
        match done {
            Ok(()) => note_done(n)?,
            Err(why) => problems.push(why),
        }
        #[cfg(test)]
        crate::repair::mid_write::here(&dir);
    }
    // marked undone only when everything is back: with something left over, Undo can be run again
    if problems.is_empty() {
        #[cfg(test)]
        test_fault::before_marker(&dir);
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
    fn same_second_repair_history_uses_recorded_time_not_random_id() {
        let (base, _, root) = lab("history-order");
        let older = "20261117-030000-ffffff";
        let newer = "20261117-030000-000000";
        for (id, when) in [(older, "2026-11-17T03:00:00.100000-05:00"), (newer, "2026-11-17T03:00:00.900000-05:00")] {
            put(&root.join(id).join("batch.json"), &serde_json::json!({"Id": id, "Title": id, "When": when}).to_string());
        }
        let found = batches(&root);
        assert_eq!(found.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), vec![newer, older]);
        std::fs::remove_dir_all(base).unwrap();
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
    fn an_undo_waits_while_another_writer_holds_the_library() {
        let (base, lib, root) = lab("waits");
        let song = lib.join("A").join("one.mp3");
        put(&song, "one");
        let b = Batch::begin(&root, &lib, "x").unwrap();
        b.remove(&song, "r").unwrap();
        let id = b.id.clone();
        b.close();
        let held = crate::library_lock::hold(); // an import putting an album in place, say
        let undoing = {
            let (root, id) = (root.clone(), id.clone());
            std::thread::spawn(move || undo(&root, &id))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!song.exists() && !undoing.is_finished(), "Undo changed the library while it was held");
        drop(held);
        assert_eq!(undoing.join().unwrap().unwrap(), Vec::<String>::new());
        assert_eq!(read(&song), "one");
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
        // as the older repair log wrote them, the first with a byte-order marker
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

/// Fault points for the tests only (never built into the app): a real write error at the moment an undo
/// record is written, or at the moment Undo writes down that it is done.
#[cfg(test)]
pub(crate) mod test_fault {
    use std::cell::RefCell;
    use std::path::Path;
    thread_local! {
        pub static BEFORE_LOG: RefCell<Option<Box<dyn FnOnce(&Path)>>> = RefCell::new(None);
        pub static BEFORE_MARKER: RefCell<Option<Box<dyn FnOnce(&Path)>>> = RefCell::new(None);
    }
    pub fn before_log(dir: &Path) {
        if let Some(f) = BEFORE_LOG.with(|h| h.borrow_mut().take()) {
            f(dir);
        }
    }
    pub fn before_marker(dir: &Path) {
        if let Some(f) = BEFORE_MARKER.with(|h| h.borrow_mut().take()) {
            f(dir);
        }
    }
}

/// Codex's review, H3 and H4: an album replaced by an import, with a write that fails at the worst moment.
#[cfg(test)]
mod replace_fault_tests {
    use super::*;

    fn lab(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("music-recs-replfault-{name}-{}-{}", std::process::id(), rand_hex(6)));
        let (lib, root) = (base.join("lib"), base.join("undo"));
        std::fs::create_dir_all(&lib).unwrap();
        (base, lib, root)
    }
    fn put(p: &Path, data: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    fn read(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap_or_else(|_| b"<gone>".to_vec())
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }
    /// The importer's own way of putting an album in place of the old one.
    fn import(root: &Path, lib: &Path, build: &Path, album: &Path) -> Result<crate::importer::import::Placed, String> {
        crate::importer::import::place_album(root, lib, "Artist", build, album, Some(album))
    }
    /// A real write error the next time an undo record is written to (`skip` writes later).
    fn fail_write(skip: usize) {
        test_fault::BEFORE_LOG.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |dir: &Path| {
                let log = dir.join("log.jsonl");
                if skip == 0 && log.exists() {
                    // the lines written so far stay; no more can be added
                    let mut p = std::fs::metadata(&log).unwrap().permissions();
                    p.set_readonly(true);
                    std::fs::set_permissions(&log, p).unwrap();
                } else if skip == 0 {
                    std::fs::create_dir(&log).unwrap(); // a folder where the log would go
                } else {
                    fail_write(skip - 1);
                }
            }))
        });
    }

    /// H4: the first line of the record cannot be written. Nothing moves, the import says so, and no
    /// record of a change that never happened is left in Repair history.
    #[test]
    fn an_import_whose_undo_record_cannot_be_written_changes_nothing() {
        let (base, lib, root) = lab("h4-first");
        let album = lib.join("Artist").join("Album");
        let build = base.join("built-album");
        put(&album.join("track.txt"), b"ORIGINAL");
        put(&build.join("track.txt"), b"REPLACEMENT");
        fail_write(0);
        let why = import(&root, &lib, &build, &album).unwrap_err();
        assert!(why.contains("Nothing in the library was changed"), "{why}");
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        assert_eq!(names(album.parent().unwrap()), vec!["Album"]);
        assert!(batches(&root).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// H4: the record's second line cannot be written after the swap. The new album is in, the old one is in
    /// the backups where the import says it is, and Undo brings it back once the new one is moved away.
    #[test]
    fn an_import_whose_record_cannot_be_finished_says_where_the_old_album_is() {
        let (base, lib, root) = lab("h4-second");
        let album = lib.join("Artist").join("Album");
        let build = base.join("built-album");
        put(&album.join("track.txt"), b"ORIGINAL");
        put(&build.join("track.txt"), b"REPLACEMENT");
        fail_write(1);
        let placed = import(&root, &lib, &build, &album).unwrap();
        assert_eq!(read(&album.join("track.txt")), b"REPLACEMENT");
        let (why, kept) = (placed.problem.unwrap(), placed.old_copy);
        assert!(kept.is_none(), "{kept:?}");
        let at = why.split(" at ").nth(1).and_then(|s| s.split(", but").next()).map(PathBuf::from).unwrap();
        assert_eq!(read(&at.join("track.txt")), b"ORIGINAL", "{why}");
        // Undo will not take the new album away on its own, and says so; the old one stays safe
        let id = batches(&root)[0].id.clone();
        let said = undo(&root, &id).unwrap();
        assert!(said.len() == 1 && said[0].contains("Something new is now at"), "{said:?}");
        assert_eq!(read(&at.join("track.txt")), b"ORIGINAL");
        // moved away by hand: Undo finishes
        std::fs::rename(&album, base.join("the new one")).unwrap();
        assert_eq!(undo(&root, &id), Ok(vec![]));
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// H4: the old album cannot be moved into the backups after the swap. It stays beside the new one, the
    /// import says where, and Undo from Repair history brings it back from there.
    #[test]
    fn an_old_album_that_cannot_go_into_the_backups_is_found_by_undo_where_it_was_set_aside() {
        let (base, lib, root) = lab("h4-move");
        let album = lib.join("Artist").join("Album");
        let build = base.join("built-album");
        put(&album.join("track.txt"), b"ORIGINAL");
        put(&build.join("track.txt"), b"REPLACEMENT");
        // just before the second line is written, a file takes the old album's place in the backups
        test_fault::BEFORE_LOG.with(|h| {
            *h.borrow_mut() = Some(Box::new(|_: &Path| {
                test_fault::BEFORE_LOG.with(|h| {
                    *h.borrow_mut() = Some(Box::new(|dir: &Path| {
                        let slot = std::fs::read_dir(dir.join("items")).unwrap().flatten().next().unwrap().path();
                        put(&slot.join("Album").join("in the way.txt"), b"in the way");
                    }))
                })
            }))
        });
        let placed = import(&root, &lib, &build, &album).unwrap();
        assert_eq!(read(&album.join("track.txt")), b"REPLACEMENT");
        let why = placed.problem.unwrap();
        let aside = names(album.parent().unwrap()).into_iter().find(|n| n.starts_with("Album.replaced-")).map(|n| album.parent().unwrap().join(n)).unwrap();
        assert!(why.contains(&aside.display().to_string()) && why.contains("Undo in Repair history can still put it back"), "{why}");
        assert_eq!(undo(&root, &batches(&root)[0].id), Ok(vec![]));
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        assert!(!aside.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// H4: cut off right after the old album was set aside (the app closed, the power went): the record is
    /// already on the disk, and Undo puts the old album back.
    #[test]
    fn an_import_cut_off_after_setting_the_old_album_aside_can_be_undone() {
        let (base, lib, root) = lab("h4-cut");
        let album = lib.join("Artist").join("Album");
        put(&album.join("track.txt"), b"ORIGINAL");
        let r = Replacement::begin(&root, &lib, "cut off", &album, &album).unwrap();
        std::fs::rename(&album, r.aside()).unwrap();
        drop(r);
        assert_eq!(undo(&root, &batches(&root)[0].id), Ok(vec![]));
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        assert_eq!(names(album.parent().unwrap()), vec!["Album"]);
        // and cut off before anything moved: Undo has nothing to do and changes nothing
        let r = Replacement::begin(&root, &lib, "cut off sooner", &album, &album).unwrap();
        drop(r);
        let id = batches(&root).into_iter().find(|b| b.title == "cut off sooner").unwrap().id;
        assert_eq!(undo(&root, &id), Ok(vec![]));
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// H3: Undo put the old album back but could not write down that it was done. Run again, it must
    /// leave the album that is back where it is (it used to move it away again, as "what the import added").
    #[test]
    fn an_undo_run_again_after_its_last_write_failed_leaves_the_restored_album_alone() {
        let (base, lib, root) = lab("h3");
        let album = lib.join("Artist").join("Album");
        let build = base.join("built-album");
        put(&album.join("track.txt"), b"ORIGINAL");
        put(&build.join("track.txt"), b"REPLACEMENT");
        let placed = import(&root, &lib, &build, &album).unwrap();
        assert!(placed.problem.is_none());
        let id = batches(&root)[0].id.clone();
        // a real write error at the last write
        test_fault::BEFORE_MARKER.with(|h| *h.borrow_mut() = Some(Box::new(|dir: &Path| std::fs::create_dir(dir.join("undone.txt")).unwrap())));
        assert!(undo(&root, &id).unwrap_err().contains("could not be written down"));
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        std::fs::remove_dir(root.join(&id).join("undone.txt")).unwrap(); // only the fault's own empty folder
        // run again: the album that is back stays, and the repair is now marked undone
        assert_eq!(undo(&root, &id), Ok(vec![]));
        assert_eq!(read(&album.join("track.txt")), b"ORIGINAL");
        assert!(batches(&root)[0].undone);
        // the new album is in the backups, not lost
        let kept: Vec<_> = crate::importer::util::walk_files(&root.join(&id)).into_iter().filter(|p| read(p) == b"REPLACEMENT").collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// H3: Undo cut off right after a step moved something and before it was written down (the progress
    /// record one step short; with none written, only the first step can have moved): run again, the old
    /// album is back and stays back, every time.
    #[test]
    fn an_undo_cut_off_between_any_two_steps_finishes_with_the_old_album_back() {
        for written in 0..=2 {
            let (base, lib, root) = lab(&format!("h3-cut-{written}"));
            let album = lib.join("Artist").join("Album");
            let build = base.join("built-album");
            put(&album.join("track.txt"), b"ORIGINAL");
            put(&build.join("track.txt"), b"REPLACEMENT");
            import(&root, &lib, &build, &album).unwrap();
            let id = batches(&root)[0].id.clone();
            test_fault::BEFORE_MARKER.with(|h| *h.borrow_mut() = Some(Box::new(|dir: &Path| std::fs::create_dir(dir.join("undone.txt")).unwrap())));
            assert!(undo(&root, &id).is_err());
            let dir = root.join(&id);
            std::fs::remove_dir(dir.join("undone.txt")).unwrap();
            // keep only the first `written` steps of the record, as if it was cut off after the rest moved
            let progress = std::fs::read_to_string(dir.join("undo-progress.txt")).unwrap();
            let kept: Vec<&str> = progress.lines().take(written).collect();
            std::fs::write(dir.join("undo-progress.txt"), kept.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
            if written == 0 {
                // only the first step (the new album out) had moved: the old one is still in the backups
                std::fs::rename(&album, text(&entries(&dir)[0], "Backup")).unwrap();
            }
            assert_eq!(undo(&root, &id), Ok(vec![]), "with {written} steps written down");
            assert_eq!(read(&album.join("track.txt")), b"ORIGINAL", "with {written} steps written down");
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
