//! Online snapshots through Windows' SQLite library (included in Windows 10/11).
//! Reads committed WAL data without checkpointing or modifying Cadence's live databases.
use std::ffi::{CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[link(name = "winsqlite3")]
unsafe extern "C" {
    fn sqlite3_open_v2(path: *const c_char, db: *mut *mut c_void, flags: c_int, vfs: *const c_char) -> c_int;
    fn sqlite3_close(db: *mut c_void) -> c_int;
    fn sqlite3_backup_init(dst: *mut c_void, dst_name: *const c_char, src: *mut c_void, src_name: *const c_char) -> *mut c_void;
    fn sqlite3_backup_step(backup: *mut c_void, pages: c_int) -> c_int;
    fn sqlite3_backup_finish(backup: *mut c_void) -> c_int;
    #[cfg(test)]
    fn sqlite3_exec(db: *mut c_void, sql: *const c_char, callback: *const c_void, arg: *mut c_void, error: *mut *mut c_char) -> c_int;
}
struct Db(*mut c_void);
impl Drop for Db { fn drop(&mut self) { unsafe { sqlite3_close(self.0); } } }
impl Db {
    fn open(path: &Path, flags: c_int) -> Result<Self, String> {
        let path = CString::new(path.to_str().ok_or("Cadence's database path is not valid Unicode.")?).map_err(|_| "Invalid database path.")?;
        let mut db = std::ptr::null_mut();
        let result = unsafe { sqlite3_open_v2(path.as_ptr(), &mut db, flags, std::ptr::null()) };
        if result != 0 {
            if !db.is_null() { unsafe { sqlite3_close(db); } }
            return Err(format!("Cadence's database could not be opened (SQLite {result}). The previous backup was kept."));
        }
        Ok(Self(db))
    }
}
struct Backup(*mut c_void);
impl Drop for Backup { fn drop(&mut self) { unsafe { sqlite3_backup_finish(self.0); } } }

pub(super) fn snapshot(source: &Path, destination: &Path, stop: &AtomicBool) -> Result<(), String> {
    if stop.load(Ordering::Relaxed) { return Err("Stopped. The previous Cadence backup was kept.".into()); }
    // Reserve a fresh staging file; never let SQLite overwrite any existing file.
    std::fs::OpenOptions::new().write(true).create_new(true).open(destination).map_err(|e| e.to_string())?;
    let input = Db::open(source, 1)?; // SQLITE_OPEN_READONLY
    let output = Db::open(destination, 2)?; // SQLITE_OPEN_READWRITE, the reserved file already exists
    let handle = unsafe { sqlite3_backup_init(output.0, c"main".as_ptr(), input.0, c"main".as_ptr()) };
    if handle.is_null() { return Err("Cadence's database snapshot could not start. The previous backup was kept.".into()); }
    let backup = Backup(handle);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if stop.load(Ordering::Relaxed) { return Err("Stopped. The previous Cadence backup was kept.".into()); }
        if Instant::now() >= deadline { return Err("Cadence's database stayed busy. Try again shortly; the previous backup was kept.".into()); }
        match unsafe { sqlite3_backup_step(backup.0, 256) } {
            101 => break, // SQLITE_DONE: a complete, consistent database, including committed WAL pages
            0 => {},
            5 | 6 => std::thread::sleep(Duration::from_millis(25)), // BUSY / LOCKED
            code => return Err(format!("Cadence's database could not be saved (SQLite {code}). The previous backup was kept.")),
        }
    }
    // Finish before closing either connection. SQLite commits and syncs the destination transaction.
    let result = unsafe { sqlite3_backup_finish(backup.0) };
    std::mem::forget(backup);
    if result != 0 { return Err(format!("Cadence's snapshot could not finish (SQLite {result}).")); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn exec(db: &Db, sql: &str) -> i32 {
        unsafe { sqlite3_exec(db.0, CString::new(sql).unwrap().as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut()) }
    }
    #[test]
    fn snapshot_includes_committed_wal_and_leaves_live_database_open() {
        let dir = std::env::temp_dir().join(format!("cadence-snapshot-{:x}", crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = Db::open(&dir.join("library.db"), 2 | 4).unwrap();
        assert_eq!(exec(&live, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE tracks(id INTEGER PRIMARY KEY, title TEXT); INSERT INTO tracks VALUES(1,'A song');"), 0);
        assert!(dir.join("library.db-wal").metadata().unwrap().len() > 0);
        snapshot(&dir.join("library.db"), &dir.join("saved.db"), &AtomicBool::new(false)).unwrap();
        let copy = Db::open(&dir.join("saved.db"), 2).unwrap();
        // The row in the WAL must be present in the standalone snapshot.
        assert_eq!(exec(&copy, "INSERT INTO tracks VALUES(1,'duplicate');"), 19);
        assert_eq!(exec(&live, "INSERT INTO tracks VALUES(2,'still writable');"), 0);
        assert!(snapshot(&dir.join("library.db"), &dir.join("saved.db"), &AtomicBool::new(false)).is_err());
        assert!(snapshot(&dir.join("library.db"), &dir.join("stopped.db"), &AtomicBool::new(true)).is_err());
        assert!(!dir.join("stopped.db").exists());
        drop(copy); drop(live);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
