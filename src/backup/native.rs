//! Native backup work. The library is read only. A new copy is checked before an old copy moves,
//! every replaced file is kept, and a failed directory read stops the plan before anything is moved.
use super::{ID_FILE, Kind, Msg, Places, Saved, Which, drive_roots, find_second, fmt_size, read_config, volume_label};
use chrono::Local;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

type Result<T> = std::result::Result<T, String>;
const OWN: &[&str] = &["_changed", "_deleted", "_musicbee", "_musictool", "_code", "_backup-id.txt", "_backup-staging"];

fn err(e: impl std::fmt::Display) -> String {
    crate::util::plain_err(e.to_string())
}
fn key(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}
fn reserved(p: &Path) -> bool {
    p.components().next().is_some_and(|c| OWN.contains(&c.as_os_str().to_string_lossy().to_lowercase().as_str()))
}

/// Reject junctions as well as symbolic links, including in a destination's existing parents.
fn plain_path(path: &Path) -> Result<PathBuf> {
    let local = matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::Disk(_)));
    if !local || !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("Use a full folder path without '..': {}", path.display()));
    }
    let full = std::path::absolute(path).map_err(err)?;
    for p in full.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_attributes() & 0x400 != 0 => return Err(format!("A link or junction is in this path, so it was left alone: {}", p.display())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
    }
    Ok(full)
}
fn overlaps(a: &Path, b: &Path) -> bool {
    let a = key(a).trim_end_matches('\\').to_string();
    let b = key(b).trim_end_matches('\\').to_string();
    a == b || a.starts_with(&(b.clone() + "\\")) || b.starts_with(&(a + "\\"))
}
fn safe_roots(source: &Path, dest: &Path, main: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    let (source, dest) = (plain_path(source)?, plain_path(dest)?);
    if !source.is_dir() {
        return Err("The library folder was not found.".into());
    }
    if overlaps(&source, &dest) {
        return Err("The backup and library folders must be separate, with neither inside the other.".into());
    }
    if dest.parent().is_none() {
        return Err("Choose a folder on the backup drive, not the drive itself.".into());
    }
    if let Some(main) = main.filter(|p| !p.as_os_str().is_empty()) {
        if overlaps(&plain_path(main)?, &dest) {
            return Err("The second backup must be separate from the main backup.".into());
        }
    }
    Ok((source, dest))
}
fn below(root: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) || reserved(rel) {
        return Err(format!("This is not a library file: {}", rel.display()));
    }
    plain_path(&root.join(rel))
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    rel: PathBuf,
    size: u64,
    modified: SystemTime,
}
#[derive(Default, Debug, PartialEq)]
struct Tree {
    files: BTreeMap<String, Entry>,
    dirs: BTreeMap<String, PathBuf>,
}
fn tree(root: &Path, backup: bool) -> Result<Tree> {
    fn visit(root: &Path, dir: &Path, backup: bool, out: &mut Tree) -> Result<()> {
        for e in fs::read_dir(dir).map_err(|e| format!("Could not read {}: {}", dir.display(), err(e)))? {
            let e = e.map_err(err)?;
            let path = e.path();
            let rel = path.strip_prefix(root).map_err(err)?.to_path_buf();
            if reserved(&rel) {
                if backup {
                    continue;
                }
                return Err(format!("The library uses a name reserved for backup history: {}", rel.display()));
            }
            let m = fs::symlink_metadata(&path).map_err(err)?;
            if m.file_attributes() & 0x400 != 0 {
                return Err(format!("A link or junction was found, so nothing was moved: {}", path.display()));
            }
            if m.is_dir() {
                out.dirs.insert(key(&rel), rel);
                visit(root, &path, backup, out)?;
            } else if m.is_file() {
                out.files.insert(key(&rel), Entry { rel, size: m.len(), modified: m.modified().map_err(err)? });
            } else {
                return Err(format!("This file cannot be backed up: {}", path.display()));
            }
        }
        Ok(())
    }
    let mut out = Tree::default();
    if backup && !root.exists() {
        return Ok(out);
    }
    visit(root, root, backup, &mut out)?;
    Ok(out)
}
fn same_meta(a: &Entry, b: &Entry, tolerance: Duration) -> bool {
    a.size == b.size && a.modified.duration_since(b.modified).unwrap_or_else(|e| e.duration()) <= tolerance
}
fn fat_times(dest: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = super::wide(&format!("{}\\", &dest.to_string_lossy()[..2]));
    let mut name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), name.as_mut_ptr(), 64)
    };
    ok != 0 && String::from_utf16_lossy(&name).to_uppercase().contains("FAT")
}

/// The same named mutex as older scheduled backups, so both versions cannot write at once.
pub(super) struct RunLock(windows_sys::Win32::Foundation::HANDLE);
impl RunLock {
    pub(super) fn take(dest: &Path) -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0},
            Security::Cryptography::{BCRYPT_SHA1_ALG_HANDLE, BCryptHash},
            System::Threading::{CreateMutexW, WaitForSingleObject},
        };
        let normalized = key(&std::path::absolute(dest).map_err(err)?).trim_end_matches('\\').to_string();
        let mut hash = [0u8; 20];
        if unsafe { BCryptHash(BCRYPT_SHA1_ALG_HANDLE, std::ptr::null(), 0, normalized.as_ptr(), normalized.len() as u32, hash.as_mut_ptr(), 20) } != 0 {
            return Err("The backup lock could not be made.".into());
        }
        let name = super::wide(&format!("Global\\MusicBackup_{}", hash[..8].iter().map(|b| format!("{b:02X}")).collect::<String>()));
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(err(std::io::Error::last_os_error()));
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                CloseHandle(handle);
            }
            return Err("Another backup to this folder is already running. Wait for it to finish.".into());
        }
        Ok(Self(handle))
    }
}
impl Drop for RunLock {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
pub(super) fn busy(dest: &str) -> bool {
    !dest.is_empty() && RunLock::take(Path::new(dest)).is_err()
}

fn locked_read(path: &Path) -> Result<File> {
    // Other readers may use it, but a writer cannot change or rename it during this copy.
    OpenOptions::new().read(true).share_mode(1).open(path).map_err(err)
}
fn equal(a: &mut File, b: &mut File, stop: &AtomicBool) -> Result<bool> {
    a.seek(SeekFrom::Start(0)).map_err(err)?;
    b.seek(SeekFrom::Start(0)).map_err(err)?;
    if a.metadata().map_err(err)?.len() != b.metadata().map_err(err)?.len() {
        return Ok(false);
    }
    let mut left = vec![0; 1024 * 1024];
    let mut right = vec![0; left.len()];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("Stopped. Copies already finished are kept.".into());
        }
        let n = a.read(&mut left).map_err(err)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut right[..n]).map_err(err)?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
    }
}

fn copy_stream(input: &mut File, output: &mut File, stop: &AtomicBool) -> Result<()> {
    let mut bytes = vec![0; 1024 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("Stopped. Copies already finished are kept.".into());
        }
        let n = input.read(&mut bytes).map_err(err)?;
        if n == 0 {
            return Ok(());
        }
        output.write_all(&bytes[..n]).map_err(err)?;
    }
}

fn copy_metadata(output: &File, path: &Path, meta: &fs::Metadata) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::*;
    output
        .set_times(
            fs::FileTimes::new()
                .set_created(meta.created().map_err(err)?)
                .set_accessed(meta.accessed().map_err(err)?)
                .set_modified(meta.modified().map_err(err)?),
        )
        .map_err(err)?;
    let attributes = meta.file_attributes()
        & (FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED);
    let attributes = if attributes == 0 { FILE_ATTRIBUTE_NORMAL } else { attributes };
    if unsafe { SetFileAttributesW(super::wide(&path.to_string_lossy()).as_ptr(), attributes) } == 0 {
        return Err(err(std::io::Error::last_os_error()));
    }
    output.sync_all().map_err(err)
}

fn clear_readonly(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

/// Keep a file that has become a folder in the library, then create that folder.
fn ensure_dirs(dest: &Path, rel: &Path, stamp: &str) -> Result<()> {
    let mut part = PathBuf::new();
    for component in rel.components() {
        part.push(component);
        let to = below(dest, &part)?;
        if to.is_file() {
            let aside = plain_path(&dest.join("_changed").join(stamp).join(&part))?;
            fs::create_dir_all(aside.parent().unwrap()).map_err(err)?;
            if aside.exists() {
                return Err("The old-copy folder already has this file. Nothing was replaced.".into());
            }
            fs::rename(&to, aside).map_err(err)?;
        }
        fs::create_dir_all(to).map_err(err)?;
    }
    Ok(())
}

fn install_file(stage: &Path, to: &Path, aside: &Path) -> Result<bool> {
    let old = to.exists();
    if old {
        plain_path(aside)?;
        fs::create_dir_all(aside.parent().unwrap()).map_err(err)?;
        if aside.exists() {
            return Err("The old-copy folder already has this file. Nothing was replaced.".into());
        }
        fs::rename(to, aside).map_err(|e| format!("Could not keep the old copy of {}. Nothing was replaced: {}", to.display(), err(e)))?;
    }
    if let Err(e) = fs::rename(stage, to) {
        if old {
            fs::rename(aside, to).map_err(|restore| {
                format!("The new copy could not be put in place ({e}). The old copy is kept at {} and could not be put back: {restore}", aside.display())
            })?;
        }
        return Err(err(e));
    }
    Ok(old)
}

/// A checked temporary file replaces the current copy only after the current copy is safely kept.
fn copy_checked(source: &Path, dest: &Path, rel: &Path, stamp: &str, stop: &AtomicBool) -> Result<(u64, bool)> {
    let from = below(source, rel)?;
    let to = below(dest, rel)?;
    let mut input = locked_read(&from)?;
    let meta = input.metadata().map_err(err)?;
    let stage_dir = plain_path(&dest.join("_backup-staging"))?;
    fs::create_dir_all(&stage_dir).map_err(err)?;
    let stage = stage_dir.join(format!("{}-{:x}.tmp", std::process::id(), crate::importer::scan::rand_id()));
    let mut output = OpenOptions::new().create_new(true).read(true).write(true).open(&stage).map_err(err)?;
    let result = (|| {
        copy_stream(&mut input, &mut output, stop)?;
        output.sync_all().map_err(err)?;
        if !equal(&mut input, &mut output, stop)? {
            return Err(format!("The new copy of {} did not match. The old copy was left alone.", rel.display()));
        }
        copy_metadata(&output, &stage, &meta)?;
        drop(output);
        plain_path(&to)?;
        ensure_dirs(dest, rel.parent().unwrap(), stamp)?;
        let aside = plain_path(&dest.join("_changed").join(stamp).join(rel))?;
        let old = install_file(&stage, &to, &aside)?;
        Ok((meta.len(), old))
    })();
    clear_readonly(&stage);
    let _ = fs::remove_file(&stage);
    result
}

fn select_dest(at: &Places, saved: &Saved, which: Which, kind: &Kind) -> Result<PathBuf> {
    let dest = match kind {
        Kind::Setup(d) => PathBuf::from(d),
        _ if which == Which::Main => {
            if saved.dest.is_empty() {
                return Err("No backup folder has been chosen yet.".into());
            }
            let root = saved.dest.get(..2).filter(|r| r.ends_with(':')).ok_or("The saved backup path has no drive letter.")?;
            let label = volume_label(&format!("{root}\\")).ok_or("The backup drive is not connected.")?;
            if !saved.label.is_empty() && !label.eq_ignore_ascii_case(&saved.label) {
                return Err("The disk at the backup drive letter is not the saved disk. Nothing was copied.".into());
            }
            PathBuf::from(&saved.dest)
        }
        _ => {
            let roots = if at.test {
                std::env::var("MUSIC_RECS_BACKUP_DRIVES").unwrap_or_default().split(';').filter(|s| !s.is_empty()).map(str::to_string).collect()
            } else {
                drive_roots()
            };
            let found = if at.test {
                roots
                    .iter()
                    .map(|r| Path::new(r).join(super::BACKUP_FOLDER))
                    .find(|d| fs::read_to_string(d.join(ID_FILE)).is_ok_and(|v| v.trim() == saved.second_id))
            } else {
                find_second(saved, &roots).map(PathBuf::from)
            };
            found.ok_or("The second backup drive is not connected.")?
        }
    };
    if at.test {
        let lab = at.config.parent().and_then(Path::parent).and_then(Path::parent).ok_or("The lab folder is missing.")?;
        if !key(&dest).starts_with(&(key(lab) + "\\")) {
            return Err("A lab can only use a backup folder inside its own lab.".into());
        }
    }
    Ok(dest)
}

fn remember(at: &Places, which: Which, dest: &Path) -> Result<()> {
    // Re-read under a separate lock so two backups finishing together keep each other's settings.
    let _lock = RunLock::take(&at.config)?;
    let mut value: serde_json::Value = match fs::read_to_string(&at.config) {
        Ok(s) => serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(err)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(err(e)),
    };
    if !value.is_object() {
        return Err("The backup settings are not readable. They were left alone.".into());
    }
    let folder = dest.to_string_lossy();
    let label = volume_label(&format!("{}\\", &folder[..2])).ok_or("The backup drive is no longer connected.")?;
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    if which == Which::Main {
        value["Destination"] = folder.as_ref().into();
        value["DestinationLabel"] = label.into();
        value["LastRun"] = now.into();
    } else {
        let id_file = plain_path(&dest.join(ID_FILE))?;
        if !id_file.exists() {
            let id = format!("{:x}-{:x}-{}", crate::importer::scan::rand_id(), crate::importer::scan::rand_id(), std::process::id());
            let mut file = OpenOptions::new().create_new(true).write(true).open(&id_file).map_err(err)?;
            file.write_all(id.as_bytes()).map_err(err)?;
            file.sync_all().map_err(err)?;
        }
        value["SecondaryId"] = fs::read_to_string(id_file).map_err(err)?.trim_start_matches('\u{feff}').trim().into();
        value["SecondaryFolder"] = folder[2..].into();
        value["SecondaryLabel"] = label.into();
        value["SecondaryLastRun"] = now.into();
    }
    fs::create_dir_all(at.config.parent().unwrap()).map_err(err)?;
    crate::util::write_atomic(&at.config, &serde_json::to_vec_pretty(&value).map_err(err)?).map_err(err)
}

pub(super) fn execute(at: &Places, which: Which, kind: &Kind, library: &Path, stop: &AtomicBool, send: &mut dyn FnMut(Msg)) -> Result<()> {
    if at.test {
        let source = plain_path(library)?;
        let lab = at.config.parent().and_then(Path::parent).and_then(Path::parent).ok_or("The lab folder is missing.")?;
        if !key(&source).starts_with(&(key(lab) + "\\")) {
            return Err("A lab can only read its own made-up library.".into());
        }
    }
    let preview = *kind == Kind::Preview;
    let _log_lock = RunLock::take(&at.logs.join(which.log_name()))?;
    let mut log = if preview {
        None
    } else {
        plain_path(&at.logs)?;
        fs::create_dir_all(&at.logs).map_err(err)?;
        Some(OpenOptions::new().create(true).append(true).open(at.logs.join(which.log_name())).map_err(err)?)
    };
    let send = std::cell::RefCell::new(send);
    let mut line = |text: String| -> Result<()> {
        if let Some(f) = &mut log {
            writeln!(f, "{text}").map_err(err)?;
            f.flush().map_err(err)?;
        }
        send.borrow_mut()(Msg::Line(text));
        Ok(())
    };
    line(format!("==================== {} ====================", Local::now().format("%Y-%m-%d %H:%M")))?;
    let result = (|| {
        let saved = read_config(&at.config);
        let destination = select_dest(at, &saved, which, kind)?;
        let (source, dest) = safe_roots(library, &destination, (which == Which::Second).then(|| Path::new(&saved.dest)))?;
        let _lock = RunLock::take(&dest)?;
        line(format!("Backing up {}  ->  {}{}", source.display(), dest.display(), if which == Which::Second { "  (second backup)" } else { "" }))?;
        run(at, which, kind, &source, &dest, stop, &mut line, &mut |files, bytes| send.borrow_mut()(Msg::Copied(files, bytes)))
    })();
    if let Err(why) = &result {
        line(format!("ERROR: {why}"))?;
    }
    if !preview {
        line(format!("Log: {}", at.logs.join(which.log_name()).display()))?;
    }
    drop(line);
    if let Some(f) = log {
        f.sync_all().map_err(err)?;
    }
    result
}

fn run(
    at: &Places,
    which: Which,
    kind: &Kind,
    source: &Path,
    dest: &Path,
    stop: &AtomicBool,
    line: &mut dyn FnMut(String) -> Result<()>,
    copied: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    line("Comparing library with backup...".into())?;
    let start = Instant::now();
    let library = tree(source, false)?;
    let backup = tree(dest, true)?;
    if library.files.is_empty() {
        return Err("The library has no files. Nothing was moved or copied.".into());
    }
    let total: u64 = library.files.values().map(|e| e.size).sum();
    if *kind == Kind::CheckAll {
        line(format!("Checking every file in {} against the library. Nothing is copied or changed.", dest.display()))?;
        let (mut good, mut waiting, mut damaged, mut done) = (0, 0, vec![], 0);
        for (i, (k, e)) in library.files.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                return Err("Stopped. Nothing was copied or changed.".into());
            }
            line(format!("Checking {} of {} files, {} of {}", i + 1, library.files.len(), fmt_size(done), fmt_size(total)))?;
            done += e.size;
            if backup.files.get(k).is_none_or(|b| !same_meta(e, b, Duration::from_secs(2))) {
                waiting += 1;
                continue;
            }
            let comparison = (|| equal(&mut locked_read(&below(source, &e.rel)?)?, &mut locked_read(&below(dest, &e.rel)?)?, stop))();
            if stop.load(Ordering::Relaxed) {
                return Err("Stopped. Nothing was copied or changed.".into());
            }
            match comparison {
                Ok(true) => good += 1,
                Ok(false) => {
                    damaged.push(format!("{} (same size and date, but the contents differ: the copy or the library's file is damaged)", e.rel.display()))
                }
                Err(why) => damaged.push(format!("{} ({why})", e.rel.display())),
            }
        }
        line(format!(
            "Checked {} files in {:.1}s: {good} good copies, {}.",
            library.files.len(),
            start.elapsed().as_secs_f64(),
            if damaged.is_empty() { "none damaged".into() } else { format!("{} DAMAGED", damaged.len()) }
        ))?;
        for f in &damaged {
            line(format!("  damaged: {f}"))?;
        }
        if waiting > 0 {
            line(format!("{waiting} files are new or changed since the last backup; the next backup will copy them."))?;
        }
        return if damaged.is_empty() { Ok(()) } else { Err(format!("{} copies need a look.", damaged.len())) };
    }
    let tolerance = if fat_times(dest) { Duration::from_secs(2) } else { Duration::ZERO };
    let mut copies: Vec<&Entry> =
        library.files.iter().filter(|(k, e)| backup.files.get(*k).is_none_or(|b| !same_meta(e, b, tolerance))).map(|(_, e)| e).collect();
    if let Kind::Recopy(list) = kind {
        copies.clear();
        for rel in fs::read_to_string(list).map_err(err)?.trim_start_matches('\u{feff}').lines().map(str::trim).filter(|s| !s.is_empty()) {
            below(source, Path::new(rel))?;
            let e = library.files.get(&key(Path::new(rel))).ok_or_else(|| format!("This file is no longer in the library: {rel}"))?;
            if !copies.iter().any(|other| other.rel == e.rel) {
                copies.push(e);
            }
        }
    }
    let size: u64 = copies.iter().map(|e| e.size).sum();
    let new = copies.iter().filter(|e| !backup.files.contains_key(&key(&e.rel))).count();
    line(format!("To copy: {new} new files, {} changed files, {} in total.", copies.len() - new, fmt_size(size)))?;
    let mut albums: BTreeMap<PathBuf, (u64, u64)> = BTreeMap::new();
    for e in &copies {
        let n = e.rel.components().count();
        let album = if n >= 3 { e.rel.components().take(2).collect() } else { e.rel.parent().unwrap_or(Path::new("")).to_path_buf() };
        let sums = albums.entry(album).or_default();
        sums.0 += 1;
        sums.1 += e.size;
    }
    for is_new in [true, false] {
        let group: Vec<_> = albums.iter().filter(|(a, _)| (!a.as_os_str().is_empty() && !backup.dirs.contains_key(&key(a))) == is_new).collect();
        if group.is_empty() {
            continue;
        }
        line(if is_new { format!("New albums / folders ({}):", group.len()) } else { format!("Albums with new or changed files ({}):", group.len()) })?;
        for (a, (n, bytes)) in group {
            let name = if a.as_os_str().is_empty() { "(library root)".into() } else { a.to_string_lossy().to_string() };
            line(if is_new { format!("  + {name}  ({n} files, {})", fmt_size(*bytes)) } else { format!("  ~ {name}  ({n} files)") })?;
        }
    }
    let mut extras: Vec<PathBuf> = vec![];
    let mut extra_dirs = 0;
    let replaced_dirs: Vec<_> = backup.dirs.iter().filter(|(k, _)| library.files.contains_key(*k)).map(|(_, rel)| rel).collect();
    let replaced = |rel: &Path| replaced_dirs.iter().any(|r| key(rel) == key(r) || key(rel).starts_with(&(key(r) + "\\")));
    for (k, rel) in &backup.dirs {
        if !library.dirs.contains_key(k) && !replaced(rel) && !extras.iter().any(|r| rel.starts_with(r)) {
            extras.push(rel.clone());
            extra_dirs += 1;
        }
    }
    let extra_files = backup.files.iter().filter(|(k, e)| !library.files.contains_key(*k) && !library.dirs.contains_key(*k) && !replaced(&e.rel)).count();
    for (k, e) in &backup.files {
        if !library.files.contains_key(k) && !library.dirs.contains_key(k) && !replaced(&e.rel) && !extras.iter().any(|r| e.rel.starts_with(r)) {
            extras.push(e.rel.clone());
        }
    }
    if !extras.is_empty() {
        line(format!("In the backup but no longer in the library: {extra_dirs} folders, {} files.", extras.len() - extra_dirs))?;
        for p in &extras {
            line(format!("  - {}{}", p.display(), if backup.dirs.contains_key(&key(p)) { "\\" } else { "" }))?;
        }
    }
    if *kind == Kind::Preview {
        line("Preview only, nothing copied.".into())?;
        return Ok(());
    }
    let stamp = format!("{}-{:x}", Local::now().format("%Y-%m-%d_%H%M%S"), crate::importer::scan::rand_id());
    let (mut done_bytes, mut kept) = (0, 0);
    if let Some((free, _)) = super::disk_space(&dest.to_string_lossy()[..3]) {
        if size > free {
            return Err(format!("Not enough room: need {}, have {}. Nothing was copied.", fmt_size(size), fmt_size(free)));
        }
    }
    for (i, e) in copies.iter().enumerate() {
        let (bytes, old) = copy_checked(source, dest, &e.rel, &stamp, stop)?;
        done_bytes += bytes;
        kept += usize::from(old);
        copied((i + 1) as u64, done_bytes);
    }
    if kept > 0 {
        line(format!("Kept the old copy of {kept} changed files in {}", dest.join("_changed").join(&stamp).display()))?;
    }
    if !copies.is_empty() {
        line(format!("Copied {} files ({}) in {:.1}s.", copies.len(), fmt_size(done_bytes), start.elapsed().as_secs_f64()))?;
        line(format!("Checked {} copied files against the library in {:.1}s: all good.", copies.len(), start.elapsed().as_secs_f64()))?;
    }
    if matches!(kind, Kind::Recopy(_)) {
        line(format!(
            "Repaired {} copies from the library. The copies they replaced are kept in {}",
            copies.len(),
            dest.join("_changed").join(&stamp).display()
        ))?;
        return Ok(());
    }
    for rel in library.dirs.values() {
        if stop.load(Ordering::Relaxed) {
            return Err("Stopped. Copies already finished are kept.".into());
        }
        ensure_dirs(dest, rel, &stamp)?;
    }
    if !extras.is_empty() {
        if *kind != Kind::Mirror && extra_files > 200 && extra_files * 5 > backup.files.len() {
            line(format!(
                "{extra_files} of the backup's {} files are no longer in the library. That is a lot at once, so they were left where they are.",
                backup.files.len()
            ))?;
        } else {
            if tree(source, false)? != library {
                return Err("The library changed during the backup. Removed items were left where they are; run the backup again.".into());
            }
            for rel in &extras {
                let from = below(dest, rel)?;
                if below(source, rel)?.exists() {
                    return Err("A removed file is back in the library. Nothing more was moved.".into());
                }
                let to = plain_path(&dest.join("_deleted").join(&stamp).join(rel))?;
                fs::create_dir_all(to.parent().unwrap()).map_err(err)?;
                fs::rename(from, to).map_err(err)?;
            }
            line(format!("Moved {} removed items to {}", extras.len(), dest.join("_deleted").join(&stamp).display()))?;
        }
    }
    if copies.is_empty() && extras.is_empty() {
        line("Backup is already up to date.".into())?;
    }
    if stop.load(Ordering::Relaxed) {
        return Err("Stopped. Copies already finished are kept.".into());
    }
    if !at.test {
        snapshots(at, dest, stop, line)?;
    }
    clean_history(dest, at.test, line)?;
    remember(at, which, dest)?;
    if which == Which::Second {
        if let Some((free, _)) = super::disk_space(&dest.to_string_lossy()[..3]) {
            line(format!("Room left on the second backup drive: {}.", fmt_size(free)))?;
        }
    }
    Ok(())
}

struct SnapshotSource {
    name: &'static str,
    path: PathBuf,
    skip_dirs: Vec<PathBuf>,
    skip_files: Vec<&'static str>,
}
fn snapshot_files(root: &Path, skip_dirs: &[PathBuf], skip_files: &[&str]) -> Result<Vec<PathBuf>> {
    fn visit(root: &Path, dir: &Path, skip_dirs: &[PathBuf], skip_files: &[&str], out: &mut Vec<PathBuf>) -> Result<()> {
        for item in fs::read_dir(dir).map_err(err)? {
            let p = item.map_err(err)?.path();
            let rel = p.strip_prefix(root).map_err(err)?.to_path_buf();
            if skip_dirs.iter().any(|s| key(&rel) == key(s) || key(&rel).starts_with(&(key(s) + "\\"))) {
                continue;
            }
            let meta = fs::symlink_metadata(&p).map_err(err)?;
            if meta.file_attributes() & 0x400 != 0 {
                continue;
            } // snapshots, like the old /XJ copies, omit links
            if meta.is_dir() {
                visit(root, &p, skip_dirs, skip_files, out)?;
            } else if !skip_files.iter().any(|s| p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(s))) {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut out = vec![];
    visit(root, root, skip_dirs, skip_files, &mut out)?;
    Ok(out)
}
fn snapshots(at: &Places, dest: &Path, stop: &AtomicBool, line: &mut dyn FnMut(String) -> Result<()>) -> Result<()> {
    let source = |name, path| SnapshotSource { name, path, skip_dirs: vec![], skip_files: vec![] };
    let local = dirs::data_local_dir().ok_or("The app data folder was not found.")?;
    let roaming = dirs::config_dir().ok_or("The settings folder was not found.")?;
    let program = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from);
    let mut bee = vec![source("Settings", roaming.join("MusicBee"))];
    if let Some(music) = dirs::audio_dir() {
        bee.push(source("Library", music.join("MusicBee")));
    }
    if let Some(p) = program {
        bee.push(source("Plugins", p.join("MusicBee/Plugins")));
        bee.push(source("Skins", p.join("MusicBee/Skins")));
    }
    let mut app = source("Data", local.join("music-recs"));
    app.skip_dirs = [
        "slskd/bin",
        "slskd/data/incomplete",
        "slskd/data/downloads",
        "cache/img",
        "cache/bandcamp",
        "cache/deezer",
        "cache/itunes",
        "cache/lastfm",
        "cache/mb",
        "cache/wiki",
    ]
    .map(PathBuf::from)
    .to_vec();
    app.skip_files.push("library-tags-cache.txt");
    let tool = at.config.parent().ok_or("The backup settings folder was not found.")?;
    let mut code = vec![];
    if let Some(parent) = tool.parent() {
        let mut music = source("music-recs", parent.join("music-recs"));
        music.skip_dirs.push("target".into());
        code.push(music);
        let mut backup = source("music-backup", tool.to_path_buf());
        backup.skip_dirs.push("logs".into());
        code.push(backup);
    }
    for (folder, label, sources) in [
        ("_MusicBee", "MusicBee", bee),
        ("_MusicTool", "Music tool", vec![app, source("Window", roaming.join("Bradley's Music Tool"))]),
        ("_Code", "Code", code),
    ] {
        save_snapshot(dest, folder, label, &sources, stop, line)?;
    }
    Ok(())
}
struct ScratchDir(PathBuf);
impl Drop for ScratchDir {
    fn drop(&mut self) {
        if plain_path(&self.0).is_ok() {
            if let Ok(files) = tree(&self.0, true) {
                for entry in files.files.values() {
                    clear_readonly(&self.0.join(&entry.rel));
                }
            } else {
                return;
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn save_snapshot(
    dest: &Path,
    folder: &str,
    label: &str,
    sources: &[SnapshotSource],
    stop: &AtomicBool,
    line: &mut dyn FnMut(String) -> Result<()>,
) -> Result<()> {
    let dated = plain_path(&dest.join(folder).join(Local::now().format("%Y-%m-%d").to_string()))?;
    let stage = plain_path(&dest.join("_backup-staging").join(format!("snapshot-{:x}", crate::importer::scan::rand_id())))?;
    fs::create_dir_all(stage.parent().unwrap()).map_err(err)?;
    fs::create_dir(&stage).map_err(err)?;
    let _scratch = ScratchDir(stage.clone());
    let (mut count, mut bytes) = (0, 0);
    for s in sources {
        if !s.path.exists() {
            continue;
        }
        plain_path(&s.path)?;
        if overlaps(&s.path, dest) {
            return Err(format!("{label}: its source and backup folders overlap, so it was left alone."));
        }
        for rel in snapshot_files(&s.path, &s.skip_dirs, &s.skip_files)? {
            let from = plain_path(&s.path.join(&rel))?;
            let to = stage.join(s.name).join(&rel);
            fs::create_dir_all(to.parent().unwrap()).map_err(err)?;
            let mut input = locked_read(&from)?;
            let meta = input.metadata().map_err(err)?;
            let mut output = OpenOptions::new().create_new(true).read(true).write(true).open(&to).map_err(err)?;
            copy_stream(&mut input, &mut output, stop)?;
            output.sync_all().map_err(err)?;
            if !equal(&mut input, &mut output, stop)? {
                return Err(format!("{label}: {} did not copy correctly. The previous snapshot was kept.", rel.display()));
            }
            copy_metadata(&output, &to, &meta)?;
            count += 1;
            bytes += meta.len();
        }
    }
    if count == 0 {
        line(format!("{label}: nothing found to save, skipped."))?;
        return Ok(());
    }
    fs::create_dir_all(dated.parent().unwrap()).map_err(err)?;
    let old =
        plain_path(&dest.join("_changed").join(format!("{}-{:x}", Local::now().format("%Y-%m-%d_%H%M%S"), crate::importer::scan::rand_id())).join(folder))?;
    let existed = dated.exists();
    if existed {
        fs::create_dir_all(old.parent().unwrap()).map_err(err)?;
        fs::rename(&dated, &old).map_err(err)?;
    }
    if let Err(e) = fs::rename(&stage, &dated) {
        if existed {
            fs::rename(&old, &dated).map_err(|why| format!("{label}: {e}. The old snapshot is kept in {}: {why}", old.display()))?;
        }
        return Err(err(e));
    }
    line(format!("{label}: saved today's copy ({count} files, {}) to {}", fmt_size(bytes), dated.display()))
}
fn clean_history(dest: &Path, test: bool, line: &mut dyn FnMut(String) -> Result<()>) -> Result<()> {
    let cutoff = Local::now().date_naive() - chrono::Duration::days(30);
    for (folder, what, daily) in [
        ("_changed", "old copies", false),
        ("_deleted", "removed files", false),
        ("_MusicBee", "MusicBee copies", true),
        ("_MusicTool", "Music tool copies", true),
        ("_Code", "Code copies", true),
    ] {
        let root = plain_path(&dest.join(folder))?;
        if !root.exists() {
            continue;
        }
        let mut dated = vec![];
        for entry in fs::read_dir(&root).map_err(err)? {
            let p = entry.map_err(err)?.path();
            plain_path(&p)?;
            let name = p.file_name().unwrap().to_string_lossy();
            if !p.is_dir() {
                continue;
            }
            if daily && name.len() != 10 {
                continue;
            }
            if !daily && (name.len() < 17 || name.get(..17).is_none_or(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d_%H%M%S").is_err())) {
                continue;
            }
            if let Some(date) = name.get(..10).and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()) {
                dated.push((date, p));
            }
        }
        dated.sort_by(|a, b| b.0.cmp(&a.0));
        let old: Vec<_> = dated.into_iter().enumerate().filter(|(i, (d, _))| if daily { *i >= 30 } else { *d < cutoff }).map(|(_, (_, p))| p).collect();
        for path in &old {
            // A lab keeps these in its own stand-in for the Recycle Bin.
            if test {
                let bin = plain_path(&dest.join("_backup-staging").join("recycled").join(folder))?;
                fs::create_dir_all(&bin).map_err(err)?;
                fs::rename(path, bin.join(path.file_name().unwrap())).map_err(err)?;
            } else {
                trash::delete(path).map_err(err)?;
            }
        }
        if !old.is_empty() {
            line(format!("Sent {} sets of {what} older than 30 days to the Recycle Bin.", old.len()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Lab {
        base: PathBuf,
        source: PathBuf,
        dest: PathBuf,
        at: Places,
    }
    impl Drop for Lab {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    impl Lab {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!("native-backup-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
            let source = base.join("library");
            let dest = base.join("backup");
            fs::create_dir_all(&source).unwrap();
            let at = Places { config: base.join("data/backup/config.json"), logs: base.join("data/backup/logs"), test: true };
            Self { base, source, dest, at }
        }
        fn write(&self, rel: &str, bytes: &[u8]) {
            let p = self.source.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
        fn run(&self, kind: Kind) -> Result<Vec<String>> {
            let mut lines = vec![];
            execute(&self.at, Which::Main, &kind, &self.source, &AtomicBool::new(false), &mut |m| {
                if let Msg::Line(s) = m {
                    lines.push(s)
                }
            })?;
            Ok(lines)
        }
        fn setup(&self) -> Vec<String> {
            self.run(Kind::Setup(self.dest.to_string_lossy().into())).unwrap()
        }
        fn contents(&self, folder: &str) -> Vec<Vec<u8>> {
            crate::importer::util::walk_files(&self.dest.join(folder)).iter().map(|p| fs::read(p).unwrap()).collect()
        }
    }
    #[test]
    fn preview_reads_only_and_a_backup_keeps_changed_and_removed_files() {
        let l = Lab::new();
        l.write("A/Album/one.mp3", b"first version");
        l.write("A/Album/two.mp3", b"second song");
        let lines = l.setup();
        assert_eq!(fs::read(l.dest.join("A/Album/one.mp3")).unwrap(), b"first version");
        let run = super::super::parse_runs(&lines.join("\n"));
        assert_eq!(run[0].copied.as_ref().unwrap().0, 2);
        assert_eq!(run[0].verified.as_ref().unwrap().0, 2);
        l.write("A/Album/one.mp3", b"a new longer version");
        fs::remove_file(l.source.join("A/Album/two.mp3")).unwrap();
        let config = fs::read(&l.at.config).unwrap();
        let log = fs::read(l.at.logs.join(Which::Main.log_name())).unwrap();
        let preview = l.run(Kind::Preview).unwrap();
        assert!(preview.iter().any(|s| s == "Preview only, nothing copied."));
        assert_eq!(fs::read(&l.at.config).unwrap(), config);
        assert_eq!(fs::read(l.at.logs.join(Which::Main.log_name())).unwrap(), log);
        assert!(!l.dest.join("_changed").exists() && !l.dest.join("_deleted").exists());
        assert_eq!(fs::read(l.dest.join("A/Album/one.mp3")).unwrap(), b"first version");
        l.run(Kind::Backup).unwrap();
        assert!(l.contents("_changed").contains(&b"first version".to_vec()));
        assert!(l.contents("_deleted").contains(&b"second song".to_vec()));
        assert_eq!(fs::read(l.dest.join("A/Album/one.mp3")).unwrap(), b"a new longer version");
        assert!(l.run(Kind::Backup).unwrap().iter().any(|s| s == "Backup is already up to date."));
        assert_eq!(fs::read(l.source.join("A/Album/one.mp3")).unwrap(), b"a new longer version");
    }
    #[test]
    fn damaged_same_size_and_date_is_found_then_repaired_with_the_old_copy_kept() {
        let l = Lab::new();
        l.write("one.mp3", b"good bytes");
        l.setup();
        let p = l.dest.join("one.mp3");
        let time = p.metadata().unwrap().modified().unwrap();
        fs::write(&p, b"bad! bytes").unwrap();
        File::options().write(true).open(&p).unwrap().set_modified(time).unwrap();
        assert!(l.run(Kind::CheckAll).is_err());
        let (_, check) = super::super::read_runs(&l.at.logs.join(Which::Main.log_name()));
        assert_eq!(check.unwrap().check.unwrap().damaged_count, 1);
        let list = l.base.join("recopy.txt");
        fs::write(&list, "one.mp3\n").unwrap();
        l.run(Kind::Recopy(list)).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"good bytes");
        assert!(l.contents("_changed").contains(&b"bad! bytes".to_vec()));
        l.run(Kind::CheckAll).unwrap();
    }
    #[test]
    fn no_old_copy_can_be_overwritten_when_history_cannot_be_created() {
        let l = Lab::new();
        l.write("song.mp3", b"original");
        l.setup();
        fs::write(l.dest.join("_changed"), b"a file blocks the history folder").unwrap();
        l.write("song.mp3", b"replacement is longer");
        let last = read_config(&l.at.config).last;
        assert!(l.run(Kind::Backup).is_err());
        assert_eq!(fs::read(l.dest.join("song.mp3")).unwrap(), b"original");
        assert_eq!(read_config(&l.at.config).last, last);
    }
    #[test]
    fn more_than_two_hundred_removed_files_need_the_explicit_tidy_action() {
        let l = Lab::new();
        l.write("keep.mp3", b"keep");
        for i in 0..201 {
            l.write(&format!("Gone/{i}.mp3"), b"removed");
        }
        l.setup();
        fs::remove_dir_all(l.source.join("Gone")).unwrap();
        let run = l.run(Kind::Backup).unwrap();
        assert!(run.iter().any(|s| s.contains("That is a lot at once")));
        assert!(l.dest.join("Gone/0.mp3").exists());
        l.run(Kind::Mirror).unwrap();
        assert!(!l.dest.join("Gone").exists());
        assert_eq!(l.contents("_deleted").len(), 201);
    }
    #[test]
    fn unsafe_paths_empty_libraries_and_the_wrong_disk_are_refused() {
        let l = Lab::new();
        assert!(safe_roots(&l.source, &l.source.join("backup"), None).is_err());
        assert!(safe_roots(&l.source, &l.base, None).is_err());
        assert!(safe_roots(&l.source, &l.dest, Some(&l.dest)).is_err());
        for rel in ["../elsewhere", "_changed/a.mp3", "C:\\elsewhere", "a/../../song.mp3"] {
            assert!(below(&l.dest, Path::new(rel)).is_err(), "{rel}");
        }
        assert!(l.run(Kind::Setup(l.dest.to_string_lossy().into())).is_err());
        assert!(!l.dest.exists());
        l.write("one.mp3", b"one");
        l.setup();
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&l.at.config).unwrap()).unwrap();
        v["DestinationLabel"] = "definitely not the disk label".into();
        fs::write(&l.at.config, v.to_string()).unwrap();
        l.write("two.mp3", b"two");
        assert!(l.run(Kind::Backup).is_err());
        assert!(!l.dest.join("two.mp3").exists());
    }
    #[test]
    fn only_one_worker_can_own_a_destination_and_source_files_cannot_change_during_copy() {
        let l = Lab::new();
        l.write("song.mp3", b"song");
        let _hold = RunLock::take(&l.dest).unwrap();
        let d = l.dest.clone();
        assert!(std::thread::spawn(move || RunLock::take(&d).is_err()).join().unwrap());
        let file = locked_read(&l.source.join("song.mp3")).unwrap();
        assert!(OpenOptions::new().write(true).open(l.source.join("song.mp3")).is_err());
        drop(file);
        assert!(OpenOptions::new().write(true).open(l.source.join("song.mp3")).is_ok());
    }
    #[test]
    fn a_stopped_check_never_changes_a_backup_and_recopy_cannot_escape_it() {
        let l = Lab::new();
        l.write("one.mp3", b"one");
        l.setup();
        assert!(execute(&l.at, Which::Main, &Kind::CheckAll, &l.source, &AtomicBool::new(true), &mut |_| {}).is_err());
        let list = l.base.join("bad-list.txt");
        fs::write(&list, "../outside.mp3").unwrap();
        assert!(l.run(Kind::Recopy(list)).is_err());
        assert_eq!(fs::read(l.dest.join("one.mp3")).unwrap(), b"one");
    }
    #[test]
    fn copies_keep_windows_dates_and_attributes_and_readonly_copies_can_be_replaced() {
        let l = Lab::new();
        l.write("song.mp3", b"old content");
        let from = l.source.join("song.mp3");
        let original = File::options().write(true).open(&from).unwrap();
        let date = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        original.set_times(fs::FileTimes::new().set_created(date).set_modified(date)).unwrap();
        drop(original);
        unsafe {
            assert_ne!(windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(super::super::wide(&from.to_string_lossy()).as_ptr(), 3), 0);
        }
        l.setup();
        let to = l.dest.join("song.mp3");
        let meta = to.metadata().unwrap();
        assert_eq!(meta.created().unwrap(), date);
        assert_eq!(meta.modified().unwrap(), date);
        assert_eq!(meta.file_attributes() & 3, 3);
        clear_readonly(&from);
        l.write("song.mp3", b"new longer content");
        l.run(Kind::Backup).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"new longer content");
        let old = crate::importer::util::walk_files(&l.dest.join("_changed"));
        assert_eq!(fs::read(&old[0]).unwrap(), b"old content");
        assert!(old[0].metadata().unwrap().permissions().readonly());
        clear_readonly(&old[0]);
    }
    #[test]
    fn empty_folders_and_changes_between_files_and_folders_keep_the_previous_contents() {
        let l = Lab::new();
        l.write("file-first", b"old file");
        l.write("folder-first/old.mp3", b"old folder contents");
        fs::create_dir_all(l.source.join("empty/nested")).unwrap();
        l.setup();
        assert!(l.dest.join("empty/nested").is_dir());
        fs::remove_file(l.source.join("file-first")).unwrap();
        l.write("file-first/new.mp3", b"new nested file");
        fs::remove_file(l.source.join("folder-first/old.mp3")).unwrap();
        fs::remove_dir(l.source.join("folder-first")).unwrap();
        l.write("folder-first", b"new plain file");
        l.run(Kind::Backup).unwrap();
        assert_eq!(fs::read(l.dest.join("file-first/new.mp3")).unwrap(), b"new nested file");
        assert_eq!(fs::read(l.dest.join("folder-first")).unwrap(), b"new plain file");
        let kept = l.contents("_changed");
        assert!(kept.contains(&b"old file".to_vec()) && kept.contains(&b"old folder contents".to_vec()));
        assert!(!l.dest.join("_deleted").exists());
    }
    #[test]
    fn a_failed_install_puts_the_old_copy_back_and_a_stopped_copy_never_replaces_it() {
        let l = Lab::new();
        l.write("song.mp3", b"original");
        l.setup();
        let target = l.dest.join("song.mp3");
        assert!(install_file(&l.dest.join("missing-stage"), &target, &l.dest.join("_changed/rollback/song.mp3")).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        l.write("song.mp3", b"new version");
        assert!(copy_checked(&l.source, &l.dest, Path::new("song.mp3"), "stopped", &AtomicBool::new(true)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(fs::read_dir(l.dest.join("_backup-staging")).unwrap().next().is_none());
    }
    #[test]
    fn a_second_backup_keeps_its_id_and_the_main_settings_and_the_schedule_stays_in_the_lab() {
        let l = Lab::new();
        l.write("song.mp3", b"made up song");
        l.setup();
        let main = read_config(&l.at.config);
        let second = l.base.join("removable/Music Backup");
        let kind = Kind::Setup(second.to_string_lossy().into());
        execute(&l.at, Which::Second, &kind, &l.source, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let id = fs::read(second.join(ID_FILE)).unwrap();
        assert!(!id.is_empty());
        l.write("song.mp3", b"another made up song");
        execute(&l.at, Which::Second, &kind, &l.source, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(fs::read(second.join(ID_FILE)).unwrap(), id);
        let now = read_config(&l.at.config);
        assert_eq!(now.dest, main.dest);
        assert_eq!(now.last, main.last);
        assert!(now.second_last.is_some());
        assert_eq!(fs::read(second.join("song.mp3")).unwrap(), b"another made up song");
        assert_eq!(fs::read(l.dest.join("song.mp3")).unwrap(), b"made up song");
        let note = super::super::schedule::install(&l.at, &l.source).unwrap();
        assert!(note.contains("No Windows task was changed"));
        assert!(fs::read_to_string(l.at.logs.join("nightly-task-preview.xml")).unwrap().contains("--backup-run"));
    }
    #[test]
    fn daily_snapshots_check_the_files_apply_exclusions_and_keep_the_previous_snapshot() {
        let l = Lab::new();
        l.write("keep.txt", b"first");
        l.write("cache/img/skip.png", b"skip");
        l.write("library-tags-cache.txt", b"skip");
        let sources =
            vec![SnapshotSource { name: "Data", path: l.source.clone(), skip_dirs: vec!["cache/img".into()], skip_files: vec!["library-tags-cache.txt"] }];
        let stop = AtomicBool::new(false);
        save_snapshot(&l.dest, "_MusicTool", "Music tool", &sources, &stop, &mut |_| Ok(())).unwrap();
        let dated = l.dest.join("_MusicTool").join(Local::now().format("%Y-%m-%d").to_string()).join("Data");
        assert_eq!(fs::read(dated.join("keep.txt")).unwrap(), b"first");
        assert!(!dated.join("cache").exists() && !dated.join("library-tags-cache.txt").exists());
        l.write("keep.txt", b"second");
        save_snapshot(&l.dest, "_MusicTool", "Music tool", &sources, &stop, &mut |_| Ok(())).unwrap();
        assert_eq!(fs::read(dated.join("keep.txt")).unwrap(), b"second");
        assert!(l.contents("_changed").contains(&b"first".to_vec()));
        l.write("keep.txt", b"third");
        assert!(save_snapshot(&l.dest, "_MusicTool", "Music tool", &sources, &AtomicBool::new(true), &mut |_| Ok(())).is_err());
        assert_eq!(fs::read(dated.join("keep.txt")).unwrap(), b"second");
    }
    #[test]
    fn old_history_is_recycled_in_a_lab_and_current_history_stays() {
        let l = Lab::new();
        l.write("one.mp3", b"one");
        l.setup();
        let old = l.dest.join("_deleted/2000-01-01_030000/old.mp3");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, b"old").unwrap();
        let recent = l.dest.join(format!("_changed/{}/recent.mp3", Local::now().format("%Y-%m-%d_%H%M%S")));
        fs::create_dir_all(recent.parent().unwrap()).unwrap();
        fs::write(&recent, b"recent").unwrap();
        l.run(Kind::Backup).unwrap();
        assert!(!old.exists());
        assert!(recent.exists());
        assert_eq!(fs::read(l.dest.join("_backup-staging/recycled/_deleted/2000-01-01_030000/old.mp3")).unwrap(), b"old");
    }
}

pub(super) fn choose_main(at: &Places, library: &Path, destination: &Path) -> Result<String> {
    let (source, dest) = safe_roots(library, destination, None)?;
    if at.test {
        let lab = at.config.parent().and_then(Path::parent).and_then(Path::parent).ok_or("The lab folder is missing.")?;
        if ![&source, &dest].iter().all(|p| key(p).starts_with(&(key(lab) + "\\"))) {
            return Err("A lab can only use its own made-up folders.".into());
        }
    }
    let text = dest.to_string_lossy();
    let root = text.get(..2).filter(|r| r.ends_with(':')).ok_or("Choose a folder on a connected drive.")?;
    let label = volume_label(&format!("{root}\\")).ok_or("That drive is not connected.")?;
    let _lock = RunLock::take(&at.config)?;
    let mut v: serde_json::Value = if at.config.exists() {
        serde_json::from_str(fs::read_to_string(&at.config).map_err(err)?.trim_start_matches('\u{feff}')).map_err(err)?
    } else {
        serde_json::json!({})
    };
    let object = v.as_object_mut().ok_or("The backup settings could not be read.")?;
    if object.get("Destination").and_then(|v| v.as_str()) != Some(text.as_ref()) {
        object.remove("LastRun");
    }
    object.insert("Destination".into(), text.as_ref().into());
    object.insert("DestinationLabel".into(), label.into());
    fs::create_dir_all(at.config.parent().unwrap()).map_err(err)?;
    crate::util::write_atomic(&at.config, &serde_json::to_vec_pretty(&v).map_err(err)?).map_err(err)?;
    Ok("Backup folder saved. Preview or start the backup when you are ready.".into())
}
