//! One writer to the library at a time.
//!
//! The importer, Library Health's repairs, Undo and the Backup page's "bring back" all change files in the
//! library from threads of their own, and nothing else made them take turns: a lyrics repair could put its
//! working copy over a track an import had just put in place (Codex's review, H2). Every change to the library
//! now happens while holding this lock, and every check that something is still as it was is made under it,
//! right before the change, so nothing can slip in between.
//!
//! The lock is held only around the changes themselves, never around slow work (a conversion, the tag editor,
//! a download), so one writer waits seconds for another at most. A thread that holds it may take it again
//! (an import that undoes its own half-done swap, a repair whose steps each take it): it is let go when the
//! outermost hold ends. One app window runs at a time (`Local\MusicApp.OneWindow`), so a lock inside the
//! process is a lock on the library.
use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;

static OWNER: Mutex<Option<(ThreadId, usize)>> = Mutex::new(None);
static FREED: Condvar = Condvar::new();

/// The library is this thread's to change until this is dropped.
#[must_use = "the lock is let go as soon as this is dropped"]
pub struct Held(());

/// Waits until no other thread is changing the library, then holds it.
pub fn hold() -> Held {
    let me = std::thread::current().id();
    let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match &mut *owner {
            None => {
                *owner = Some((me, 1));
                return Held(());
            }
            Some((who, depth)) if *who == me => {
                *depth += 1;
                return Held(());
            }
            Some(_) => owner = FREED.wait(owner).unwrap_or_else(|e| e.into_inner()),
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, depth)) = &mut *owner {
            *depth -= 1;
            if *depth == 0 {
                *owner = None;
                FREED.notify_all();
            }
        }
    }
}

/// What a file was when a working copy of it was made: its size, its time and a fingerprint of its bytes.
/// Before the copy takes the file's place, the file must still be exactly this (see `still`).
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    len: u64,
    modified: Option<std::time::SystemTime>,
    md5: [u8; 16],
}

fn fingerprint(path: &std::path::Path) -> std::io::Result<[u8; 16]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    Ok(ctx.compute().0)
}

/// Makes `copy` a working copy of `original` and says what the original was. The size and time are read
/// before the copy and the fingerprint is of the copy itself, so a file that changed while it was being copied
/// does not match its snapshot afterwards.
pub fn copy_with_snapshot(original: &std::path::Path, copy: &std::path::Path) -> std::io::Result<Snapshot> {
    let meta = std::fs::metadata(original)?;
    std::fs::copy(original, copy)?;
    Ok(Snapshot { len: meta.len(), modified: meta.modified().ok(), md5: fingerprint(copy)? })
}

/// Whether `original` is still exactly what its working copy was made from. Called under `hold`, right
/// before the copy takes its place: Err says what changed, in plain words.
pub fn still(original: &std::path::Path, was: &Snapshot) -> Result<(), String> {
    const CHANGED: &str = "it was changed by something else (an import, or another program) while it was being repaired, so the repair was not saved and the file was left as it is now";
    let meta = std::fs::metadata(original).map_err(|_| "it is no longer there".to_string())?;
    if meta.len() != was.len || meta.modified().ok() != was.modified {
        return Err(CHANGED.into());
    }
    match fingerprint(original) {
        Ok(md5) if md5 == was.md5 => Ok(()),
        Ok(_) => Err(CHANGED.into()),
        Err(e) => Err(crate::util::plain_err(format!("it could not be read again before the repair was saved: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_second_writer_waits_and_the_same_thread_may_hold_it_again() {
        let outer = hold();
        let inner = hold(); // the same thread: no wait
        let done = Arc::new(AtomicBool::new(false));
        let other = {
            let done = done.clone();
            std::thread::spawn(move || {
                let _h = hold();
                done.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(250));
        assert!(!done.load(Ordering::SeqCst), "another thread got in while the library was held");
        drop(inner);
        std::thread::sleep(Duration::from_millis(250));
        assert!(!done.load(Ordering::SeqCst), "let go before the outermost hold ended");
        drop(outer);
        other.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn a_file_that_changed_after_its_copy_was_made_is_caught() {
        let dir = std::env::temp_dir().join(format!("music-recs-lock-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (file, copy) = (dir.join("song.mp3"), dir.join("copy.mp3"));
        std::fs::write(&file, b"the song as it was").unwrap();
        let snap = copy_with_snapshot(&file, &copy).unwrap();
        assert_eq!(still(&file, &snap), Ok(()));
        // the same size and the same time, other bytes: the fingerprint catches it
        let time = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"the song as it is!").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(time).unwrap();
        assert!(still(&file, &snap).unwrap_err().contains("changed by something else"));
        // another file put in its place
        std::fs::write(&file, b"a longer, newer import of the song").unwrap();
        assert!(still(&file, &snap).is_err());
        std::fs::remove_file(&file).unwrap();
        assert!(still(&file, &snap).unwrap_err().contains("no longer there"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
