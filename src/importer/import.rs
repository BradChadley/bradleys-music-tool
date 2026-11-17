//! Importing one album: convert / retag / add the cover in a side folder, then swap it into the library.
use crate::importer::ctx::Ctx;
use crate::importer::lookup::Cover;
use crate::importer::lyrics;
use crate::importer::model::*;
use crate::importer::scan::{fate, find_existing_album, rand_id, Fate, IMAGE_EXT};
use crate::importer::tools;
use crate::importer::util::*;
use regex::Regex;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::sync::LazyLock;

/// Windows paths over ~260 characters break many programs, so long names get shortened.
const MAX_PATH: usize = 240;
static RE_COVER_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(cover|folder|front)$").unwrap());

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ImportResult {
    Ok,
    /// imported, but Soulseek still had the original open
    OkKept,
    Cleaned,
    Exists,
    Failed,
}

pub fn shorten_name(name: &str, max: usize, folder: bool) -> String {
    let len = name.chars().count();
    if len <= max {
        return name.to_string();
    }
    if folder {
        let keep = max.saturating_sub(1).max(8);
        let cut: String = name.chars().take(keep).collect();
        return format!("{}~", cut.trim_end_matches([' ', '.', '-']));
    }
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let keep = max.saturating_sub(ext.chars().count() + 1).max(8);
    let cut: String = base.chars().take(keep.min(base.chars().count())).collect();
    format!("{}~{}", cut.trim_end_matches([' ', '.', '-']), ext)
}

pub fn fit_path(dir: &Path, rel: &str) -> PathBuf {
    let full = dir.join(rel);
    if full.to_string_lossy().chars().count() <= MAX_PATH {
        return full;
    }
    let leaf = rel_leaf(rel);
    let sub = rel_parent(rel);
    let base = if sub.is_empty() { dir.to_path_buf() } else { dir.join(&sub) };
    let room = MAX_PATH.saturating_sub(base.to_string_lossy().chars().count() + 1).max(20);
    base.join(shorten_name(&leaf, room, false))
}

/// Rough size of the album once converted (lossless shrinks to roughly a third as 320kbps MP3; where
/// lossless downloads are kept, it is counted at the size it has).
pub fn output_bytes(u: &Unit, keep_lossless: bool, quality: crate::import_quality::Mp3Quality) -> u64 {
    let mut sum = 0u64;
    for t in &u.tracks {
        sum += if t.lossless && !keep_lossless { if t.duration.is_finite() && t.duration > 0.0 { (t.duration * quality.reserve_kbps() as f64 * 125.0 * 1.05) as u64 + 1024 * 1024 } else { (t.size as f64 * 0.4) as u64 } } else { t.size };
    }
    for f in u.files.iter().filter(|f| IMAGE_EXT.contains(&ext_lower(&f.path).as_str())) {
        sum += f.size;
    }
    sum + 20 * 1024 * 1024 // room for the downloaded cover
}

pub fn free_bytes(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut p = path;
    while !p.exists() {
        match p.parent() {
            Some(x) => p = x,
            None => return u64::MAX,
        }
    }
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    let (mut total, mut total_free) = (0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
    if ok == 0 { u64::MAX } else { free }
}

fn any_locked(files: &[PathBuf]) -> bool {
    files.iter().any(|f| std::fs::OpenOptions::new().read(true).share_mode(0).open(f).is_err())
}

/// test runs (`--keep-originals`): never touch the source downloads
pub static KEEP_ORIGINALS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends the original download to the Recycle Bin. Returns false (and remembers it) if Soulseek still has it open.
pub fn remove_original(ctx: &Ctx, u: &Unit) -> bool {
    if KEEP_ORIGINALS.load(Ordering::Relaxed) {
        return true;
    }
    let files: Vec<PathBuf> = if u.is_dir { walk_files(&u.item) } else { vec![u.item.clone()] };
    let key = u.item.to_string_lossy().to_string();
    if !any_locked(&files) && trash::delete(&u.item).is_ok() {
        ctx.set_pending(&key, false);
        return true;
    }
    ctx.set_pending(&key, true);
    false
}

/// A temp folder that removes itself, on every way out of a function.
struct TempDir(PathBuf);
impl TempDir {
    fn new(ctx: &Ctx, tag: &str) -> TempDir {
        let d = ctx.work_dir.join(format!("{tag}{:x}", rand_id()));
        let _ = std::fs::create_dir_all(&d);
        TempDir(d)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Art {
    full: PathBuf,
    embed: PathBuf,
    size: String,
    name: String,
    /// keeps the downloaded files alive until the album is done
    _dir: TempDir,
}

/// Downloads a cover into its own temp folder. None if no URL gave a usable picture.
fn fetch_cover(ctx: &Ctx, cover: &Cover) -> Option<Art> {
    let dir = TempDir::new(ctx, "cover-");
    save_cover(ctx, cover, dir)
}

fn save_cover(ctx: &Ctx, cover: &Cover, tmp: TempDir) -> Option<Art> {
    let dir = tmp.0.clone();
    let dir = dir.as_path();
    let _keep = &tmp;
    save_cover_in(ctx, cover, dir).map(|(full, embed, size, name)| Art { full, embed, size, name, _dir: tmp })
}

/// Downloads a cover into `dir`: (the full picture, a copy sized for embedding, "WxH", the name to save it under).
pub(crate) fn save_cover_in(ctx: &Ctx, cover: &Cover, dir: &Path) -> Option<(PathBuf, PathBuf, String, String)> {
    let src = dir.join("cover.download");
    let mut got = false;
    for url in &cover.urls {
        if let Ok(mut r) = ctx.dl.get(url).call() {
            if r.status().as_u16() == 200 {
                if let Ok(bytes) = r.body_mut().with_config().limit(300_000_000).read_to_vec() {
                    if std::fs::write(&src, bytes).is_ok() {
                        got = true;
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&src);
    }
    if !got || !src.exists() {
        return None;
    }
    const L: [&str; 4] = tools::LOCAL_ONLY;
    let out = tools::run_stdout(
        &ctx.ffprobe,
        &["-v".as_ref(), "error".as_ref(), "-select_streams".as_ref(), "v:0".as_ref(), "-show_entries".as_ref(), "stream=codec_name,width,height".as_ref(), "-of".as_ref(), "csv=p=0".as_ref(), L[0].as_ref(), L[1].as_ref(), L[2].as_ref(), L[3].as_ref(), "--".as_ref(), src.as_os_str()],
    );
    let text = String::from_utf8_lossy(&out).trim().to_string();
    let info: Vec<&str> = text.split(',').collect();
    if info.first().map(|s| s.is_empty()).unwrap_or(true) {
        return None;
    }
    let ext = if info[0] == "png" { ".png" } else { ".jpg" };
    let full = dir.join(format!("cover{ext}"));
    std::fs::rename(&src, &full).ok()?;
    // smaller copy for embedding in each track
    let embed = dir.join("embed.jpg");
    let mut c = tools::command(&ctx.ffmpeg, false);
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]).args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(&full).args(["-vf", "scale='min(1400,iw)':-1", "-q:v", "2", "--"]).arg(&embed);
    c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // a resize that failed or ran out of time may have left half a picture: nothing is embedded then
    if !tools::run_limited(&mut c, 120) || !embed.exists() {
        let _ = std::fs::remove_file(&embed);
        return None;
    }
    Some((full, embed, format!("{}x{}", info.get(1).unwrap_or(&"?"), info.get(2).unwrap_or(&"?")), format!("cover{ext}")))
}

// ---------- one conversion per core, across all albums ----------
static SLOTS: LazyLock<(Mutex<usize>, Condvar)> = LazyLock::new(|| (Mutex::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)), Condvar::new()));

struct Slot;
impl Slot {
    fn take() -> Slot {
        let mut n = SLOTS.0.lock().unwrap_or_else(|e| e.into_inner());
        while *n == 0 {
            n = SLOTS.1.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n -= 1;
        Slot
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        *SLOTS.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        SLOTS.1.notify_one();
    }
}

/// Artist folders are created and (when empty) removed by different albums; one at a time.
static ARTIST_DIRS: Mutex<()> = Mutex::new(());

/// A cover being downloaded ahead of time by the prefetch thread.
pub struct CoverSlot {
    state: Mutex<Option<Option<Art>>>, // None = still downloading
    cv: Condvar,
}
impl CoverSlot {
    fn new() -> Self {
        CoverSlot { state: Mutex::new(None), cv: Condvar::new() }
    }
    fn put(&self, art: Option<Art>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.is_none() {
            *s = Some(art);
        }
        self.cv.notify_all();
    }
    /// Drops an unused cover (and its temp folder); a late download is thrown away.
    fn discard(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *s = Some(None);
        self.cv.notify_all();
    }
    fn is_ready(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    /// Waits for the download, then hands the art over.
    fn take(&self) -> Option<Art> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while s.is_none() {
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.take().flatten()
    }
}

enum Mode {
    Copy,
    Ffmpeg(Vec<std::ffi::OsString>),
    /// copied byte for byte, then only the tags changed (TagLib helper); the ffmpeg args are the fallback
    Tags(Vec<std::ffi::OsString>, serde_json::Value),
}
struct Job {
    mode: Mode,
    input: PathBuf,
    out: PathBuf,
    /// the track this is (None for a picture)
    track: Option<usize>,
}

/// The tag changes for TagLib, in its terms ("track" "3/12" -> track 3, trackcount 12; "date" -> year).
fn taglib_set(t: &Track) -> serde_json::Map<String, serde_json::Value> {
    static NUM_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)(?:/(\d+))?").unwrap());
    static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{4})").unwrap());
    let mut m = serde_json::Map::new();
    for (k, v) in &t.new {
        match k.as_str() {
            "disc" if v.is_empty() => {
                m.insert("disc".into(), "0".into());
                m.insert("disccount".into(), "0".into());
            }
            "track" | "disc" => {
                if let Some(c) = NUM_OF.captures(v) {
                    m.insert(k.clone(), c[1].into());
                    if let Some(total) = c.get(2) {
                        m.insert(format!("{k}count"), total.as_str().into());
                    }
                }
            }
            "date" => {
                if let Some(c) = YEAR.captures(v) {
                    m.insert("year".into(), c[1].into());
                }
            }
            "title" | "artist" | "album" | "album_artist" | "genre" | "compilation" => {
                m.insert(k.clone(), v.clone().into());
            }
            _ => {}
        }
    }
    m
}

#[cfg(test)]
mod tag_tests {
    use super::*;
    #[test]
    fn clearing_disc_clears_taglib_number_and_total() {
        let mut track = Track::default();
        track.set_new("disc", "");
        let tags = taglib_set(&track);
        assert_eq!(tags["disc"], "0");
        assert_eq!(tags["disccount"], "0");
        track.set_new("disc", "2/3");
        let tags = taglib_set(&track);
        assert_eq!(tags["disc"], "2");
        assert_eq!(tags["disccount"], "3");
    }
}

/// Runs the tag editor (`tagwrite`) over the copies. Returns, per file: Ok(note) or Err(why it couldn't be edited safely).
fn run_tag_helper(ctx: &Ctx, plan: &[serde_json::Value]) -> std::collections::HashMap<String, Result<String, String>> {
    let mut out = std::collections::HashMap::new();
    for r in crate::tagwrite::run(plan, &ctx.ffmpeg) {
        let file = r["File"].as_str().unwrap_or("").to_lowercase();
        let v = if r["Ok"].as_bool() == Some(true) { Ok(r["Note"].as_str().unwrap_or("").to_string()) } else { Err(r["Why"].as_str().unwrap_or("").to_string()) };
        out.insert(file, v);
    }
    for p in plan {
        out.entry(p["File"].as_str().unwrap_or("").to_lowercase()).or_insert_with(|| Err("the tag editor didn't report back".into()));
    }
    out
}

/// The tag editor's instructions for a file ffmpeg wrote: carry the tags of the file it was made from over
/// into the real fields, leaving alone what the import set or took out on purpose. What another person's
/// player left in the download (their rating, play count, DJ data) is not carried and is taken out.
pub(crate) fn carry_plan(out: &Path, from: &Path, t: &Track) -> serde_json::Value {
    let mut skip: Vec<String> = t.new.iter().map(|(k, _)| k.to_lowercase()).chain(t.junk.iter().map(|(f, _, _)| f.to_lowercase())).collect();
    skip.sort();
    skip.dedup();
    serde_json::json!({ "File": out.to_string_lossy(), "CarryFrom": from.to_string_lossy(), "Skip": skip, "Set": {}, "Junk": [], "Cover": null, "CoverMode": "missing", "Vbr": false, "DropPersonal": true })
}

/// Runs the tag editor over files ffmpeg wrote (see `carry_plan`). Returns the ones it could not do, with
/// why: those keep what ffmpeg gave them, which is what every converted file had before this step existed.
pub(crate) fn carry_tags(ctx: &Ctx, plans: &[serde_json::Value]) -> Vec<(String, String)> {
    let res = run_tag_helper(ctx, plans);
    let mut failed = vec![];
    for p in plans {
        let file = p["File"].as_str().unwrap_or("");
        if let Some(Err(why)) = res.get(&file.to_lowercase()) {
            failed.push((Path::new(file).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), why.clone()));
        }
    }
    failed
}

/// Calls the "my jobs are all handed out" callback once, however the function ends.
struct Drained<'a>(&'a (dyn Fn() + Sync), AtomicBool);
impl Drained<'_> {
    fn fire(&self) {
        if !self.1.swap(true, Ordering::SeqCst) {
            (self.0)();
        }
    }
}
impl Drop for Drained<'_> {
    fn drop(&mut self) {
        self.fire();
    }
}

/// Puts a finished album where it belongs, all or nothing.
///
/// With no old copy this is one rename. With an old copy: move the old folder aside (same folder, so it is
/// instant and fails cleanly if a track is playing), move the new one in, and only then hand the old one to
/// `recycle` (which keeps it in the undo backups). If the new one can't be moved in, the old one is put back.
/// At no point is the library without a complete copy, and a crash at any step leaves the old copy on disk
/// under its own or its "aside" name.
///
/// `existing` is the old copy and the place beside it where it is set aside (`undo::Replacement` chooses it
/// and writes it down before this is called, so a crash at any step leaves a record that finds it).
///
/// Ok(Some(why)) means the swap worked but the old copy could not be put away: `why` is `recycle`'s own
/// words, which say where the old copy is now.
pub fn swap_into_place(build: &Path, final_dir: &Path, existing: Option<(&Path, &Path)>, recycle: &dyn Fn(&Path) -> Result<(), String>) -> Result<Option<String>, String> {
    let _held = crate::library_lock::hold();
    let Some((old, aside)) = existing else {
        return match std::fs::rename(build, final_dir) {
            Ok(()) => Ok(None),
            Err(_) => {
                let _ = std::fs::remove_dir_all(build);
                Err("Couldn't move the finished album into place - nothing added, download left in place".into())
            }
        };
    };
    if std::fs::rename(old, aside).is_err() {
        let _ = std::fs::remove_dir_all(build);
        return Err("Couldn't move the old copy out of the way (is it playing?) - old copy kept, download left in place".into());
    }
    if std::fs::rename(build, final_dir).is_err() {
        let back = std::fs::rename(aside, old).is_ok();
        let _ = std::fs::remove_dir_all(build);
        return Err(if back {
            "Couldn't move the finished album into place - old copy put back, download left in place".into()
        } else {
            format!("Couldn't move the finished album into place. Your old copy is safe at {}", aside.display())
        });
    }
    match recycle(aside) {
        Ok(()) => Ok(None),
        Err(why) => Ok(Some(why)),
    }
}

/// What `place_album` did with the old copy, when there was one.
#[derive(Debug, Default)]
pub struct Placed {
    /// where the old copy is kept in the undo backups
    pub old_copy: Option<PathBuf>,
    /// the new album is in place, but this went wrong with the old copy; the words say where it is now
    pub problem: Option<String>,
}

/// Puts a finished album (`build`) at `final_dir`, in place of the old copy at `existing` if there is one,
/// the way an import does: how to undo it is written down before the old copy moves at all (see
/// `undo::Replacement`), then the swap (`swap_into_place`), then the old copy into the undo backups.
/// Err: nothing in the library was changed, or the words say where the old copy is.
pub fn place_album(repair_root: &Path, library: &Path, artist: &str, build: &Path, final_dir: &Path, existing: Option<&Path>) -> Result<Placed, String> {
    let _held = crate::library_lock::hold();
    let record = match existing {
        None => None,
        Some(was) => {
            let title = format!("Import: replaced {artist} - {}", was.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            match crate::undo::Replacement::begin(repair_root, library, &title, was, final_dir) {
                Ok(r) => Some(r),
                Err(why) => {
                    let _ = std::fs::remove_dir_all(build);
                    return Err(format!("Couldn't replace the old copy: {why}. Nothing in the library was changed, download left in place"));
                }
            }
        }
    };
    let kept_at = std::cell::RefCell::new(None);
    let keep_old = |_: &Path| -> Result<(), String> {
        let r = record.as_ref().ok_or("there is no undo record for the old copy")?;
        *kept_at.borrow_mut() = Some(r.finish()?);
        Ok(())
    };
    match swap_into_place(build, final_dir, record.as_ref().map(|r| (r.was_at(), r.aside())), &keep_old) {
        Err(why) => {
            if let Some(r) = record {
                r.cancel(); // (kept if any of the old copy is away from its place, so Undo can find it)
            }
            Err(why)
        }
        Ok(problem) => Ok(Placed { old_copy: kept_at.into_inner(), problem }),
    }
}

/// True if the download folder no longer holds exactly what was scanned (files arrived, left or changed size).
pub fn changed_since_scan(u: &Unit) -> bool {
    if !u.is_dir {
        return false;
    }
    let now = walk_files(&u.item);
    if now.len() != u.files.len() {
        return true;
    }
    now.iter().any(|p| {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX);
        !u.files.iter().any(|f| f.path == *p && f.size == size)
    })
}

#[cfg(test)]
mod swap_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("music-recs-swap-{name}-{}-{:x}", std::process::id(), rand_id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn album(dir: &Path, track: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(track), b"x").unwrap();
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn replace_is_all_or_nothing() {
        let root = scratch("a");
        let (old, build, fin) = (root.join("Album"), root.join("Album (2020).importing"), root.join("Album (2020)"));

        // normal replace: new in, old handed to the recycler
        album(&old, "old.mp3");
        album(&build, "new.mp3");
        let recycled = std::cell::RefCell::new(vec![]);
        let r = swap_into_place(&build, &fin, Some((&old, &root.join("Album.replaced-1"))), &|p| {
            recycled.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p).map_err(|e| e.to_string())
        });
        assert_eq!(r, Ok(None));
        assert_eq!(names(&root), vec!["Album (2020)"]);
        assert!(fin.join("new.mp3").exists() && recycled.borrow().len() == 1);

        // the recycler fails: new copy is in, old copy is still on disk under its aside name
        album(&build, "newer.mp3");
        let kept = root.join("Album (2020).replaced-2");
        let said = swap_into_place(&build, &fin, Some((&fin, &kept)), &|_| Err("bin full, the old copy is at its aside name".into())).unwrap().unwrap();
        assert!(said.contains("bin full"));
        assert!(fin.join("newer.mp3").exists() && kept.join("new.mp3").exists());
        std::fs::remove_dir_all(&kept).unwrap();

        // the new copy can't be moved in (the build is missing): the old copy is put back untouched
        let r = swap_into_place(&root.join("nope.importing"), &root.join("Other"), Some((&fin, &root.join("Album (2020).replaced-3"))), &|_| panic!("must not recycle"));
        assert!(r.is_err());
        assert_eq!(names(&root), vec!["Album (2020)"]);
        assert!(fin.join("newer.mp3").exists());

        // no old copy: a plain move
        album(&build, "n.mp3");
        assert_eq!(swap_into_place(&build, &root.join("Fresh"), None, &|_| panic!("nothing to recycle")), Ok(None));
        assert!(root.join("Fresh").join("n.mp3").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// True if `s` can be exactly one folder level: not empty, not "." or "..", no separators or drive colon.
pub fn is_folder_name(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.trim_matches(['.', ' ']) != "" && !t.contains(['\\', '/', ':']) && {
        let mut parts = std::path::Path::new(t).components();
        matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none()
    }
}

#[cfg(test)]
mod folder_name_tests {
    use super::is_folder_name;

    #[test]
    fn only_single_real_folder_names_pass() {
        for ok in ["Rocket (2017)", "AC_DC", "...And Justice for All", "한국어"] {
            assert!(is_folder_name(ok), "{ok}");
        }
        for bad in ["", " ", ".", "..", "...", r"a\b", "a/b", "C:", r"..\x", r"\\host\share"] {
            assert!(!is_folder_name(bad), "{bad}");
        }
    }
}

/// `report` is called as report("status text", fraction 0..1). `cover` is the prefetched download, if there is one.
/// `drained` is called once every conversion job has been handed out (the next album may start then).
pub fn import_unit(ctx: &Ctx, u: &mut Unit, report: &dyn Fn(&str, f64), cover: Option<&CoverSlot>, drained: &(dyn Fn() + Sync)) -> ImportResult {
    let drained = Drained(drained, AtomicBool::new(false));
    // Only ever work on <library>\<artist>\<album>. An empty or odd name (a tag of "." cleans to nothing) would
    // otherwise make "the album already in the library" mean a whole artist folder, or the library itself.
    if !is_folder_name(&u.artist) || !is_folder_name(&u.folder) {
        report("The artist or album name is empty or can't be a folder name - type a name and import again", 1.0);
        return ImportResult::Failed;
    }
    let artist_dir = ctx.library.join(&u.artist);
    // leave at least ~70 characters for track names (and the temporary ".importing" suffix)
    let room = MAX_PATH.saturating_sub(70 + artist_dir.to_string_lossy().chars().count()).clamp(20, 120);
    u.folder = shorten_name(&u.folder, room, true);
    let existing = find_existing_album(ctx, &u.artist, &u.folder);
    // The album goes beside the copy it replaces (which may be on a shelf, or in a genre folder), else where
    // this artist keeps such releases, else in the artist's folder.
    let home = match &existing {
        Some(e) => e.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| artist_dir.clone()),
        None => crate::importer::scan::home_for_new(ctx, &u.artist, &u.folder, u.tracks.len()),
    };
    let final_dir = home.join(&u.folder);
    // the copy this import may replace must be one album folder of this artist, nothing wider, and the place
    // it goes must be inside the library
    let inside = home.starts_with(&ctx.library) && home != ctx.library;
    if !is_folder_name(&u.folder) || !inside || existing.as_ref().is_some_and(|e| !crate::importer::scan::is_album_of(ctx, &u.artist, e)) {
        report("The album's place in the library couldn't be worked out safely - nothing changed", 1.0);
        ctx.log(&format!("REFUSED unsafe destination: {} / {}", u.artist, u.folder));
        return ImportResult::Failed;
    }

    if u.action == Action::Cleanup && existing.is_some() {
        if !KEEP_ORIGINALS.load(Ordering::Relaxed) && changed_since_scan(u) {
            report("The leftover download changed after it was scanned, so it was left alone. Rescan to look at it again.", 1.0);
            return ImportResult::OkKept;
        }
        if remove_original(ctx, u) {
            report("Removed the leftover download", 1.0);
            ctx.log(&format!("CLEAN {}", u.item.display()));
            return ImportResult::Cleaned;
        }
        report("Soulseek still has the original open - try again later", 1.0);
        return ImportResult::OkKept;
    }
    if existing.is_some() && u.action != Action::Replace {
        report("Already in library, skipped", 1.0);
        ctx.log(&format!("SKIP exists: {}", existing.unwrap().display()));
        return ImportResult::Exists;
    }
    if existing.is_none() && u.action == Action::Replace {
        u.action = Action::New;
    }
    // a replacement Health asked for is checked once more, now that the download's audio has been heard in
    // full: a copy with its own damage never takes the place of the one in the library
    if let Some(e) = existing.as_ref().filter(|e| crate::replace::wanted(&ctx.replacements, e).is_some()) {
        if let Some(why) = crate::importer::scan::unfit_to_replace(ctx, u, e) {
            report(&format!("Health asked for a better copy, but this download {why}. Your copy stays as it is."), 1.0);
            ctx.log(&format!("REFUSED replacement for {}: {why}", e.display()));
            return ImportResult::Failed;
        }
    }

    let need = output_bytes(u, ctx.keep_lossless, ctx.mp3_quality);
    let free = free_bytes(&ctx.library);
    if free < need + 500 * 1024 * 1024 {
        report(&format!("Not enough space on the library drive ({:.1} GB free, needs about {:.1} GB)", free as f64 / 1073741824.0, need as f64 / 1073741824.0), 1.0);
        return ImportResult::Failed;
    }

    // build the album in a side folder first, so a failure never leaves half an album in the library
    let build = PathBuf::from(format!("{}.importing", final_dir.display()));
    if build.exists() {
        let _ = std::fs::remove_dir_all(&build);
    }
    let made = {
        let _g = ARTIST_DIRS.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&build).is_ok()
    };
    if !made {
        report("Couldn't create the album folder", 1.0);
        return ImportResult::Failed;
    }
    let mut art: Option<Art> = None;
    if let (Some(c), true) = (&u.cover, u.use_cover) {
        match cover {
            Some(slot) => {
                if !slot.is_ready() {
                    report(&format!("Waiting for the {} cover...", c.source), 0.0);
                }
                art = slot.take();
            }
            None => {
                report(&format!("Downloading cover from {}...", c.source), 0.0);
                art = fetch_cover(ctx, c);
            }
        }
        if art.is_none() {
            report(&format!("{} cover download failed, keeping existing art", c.source), 0.0);
        }
    }
    let folder_img = u.folder_img.clone();

    // Lyrics: LRCLIB is asked now about the songs the list had not got to yet (its answers are kept, so a
    // song is asked about once). Lyrics a file has are never written over, unless they were flagged and the
    // choice was to replace or clean them.
    let asks: Vec<(usize, String, String, f64)> = crate::importer::scan::lyric_asks(u).into_iter().filter(|a| u.tracks[a.0].lyric_found.is_none()).collect();
    for (n, (i, artist, title, secs)) in asks.iter().enumerate() {
        if ctx.lyrics.online && ctx.lyrics.known(artist, title, *secs).is_none() {
            report(&format!("Finding lyrics {}/{}...", n + 1, asks.len()), 0.0);
        }
        u.tracks[*i].lyric_found = ctx.lyrics.find(artist, title, *secs);
    }
    let lyric_acts: Vec<lyrics::Act> = u
        .tracks
        .iter()
        .map(|t| if crate::importer::scan::takes_lyrics(t) { lyrics::act(&t.lyric, t.lyric_found.as_ref(), u.lyric_choice, ctx.lyrics.timed) } else { lyrics::Act::Nothing })
        .collect();

    let mut jobs: Vec<Job> = vec![];
    for (ti, t) in u.tracks.iter().enumerate() {
        let mut meta: Vec<std::ffi::OsString> = vec![];
        for (k, v) in &t.new {
            meta.push("-metadata".into());
            meta.push(format!("{k}={v}").into());
        }
        for m in ["albumartist=", "ALBUMARTIST="] {
            meta.push("-metadata".into());
            meta.push(m.into());
        }
        for (f, _, _) in &t.junk {
            if !t.has_new(f) {
                meta.push("-metadata".into());
                meta.push(format!("{f}=").into()); // an empty value removes the junk field
            }
        }
        // what becomes of this track: an MP3, a FLAC, or the file it is (the Settings choice decides for lossless)
        let fate = fate(t, ctx.keep_lossless);
        let art_in: Option<PathBuf> = match (&art, &folder_img) {
            (Some(a), _) => Some(a.embed.clone()),
            (None, Some(f)) if !t.has_art => Some(f.clone()),
            _ => None,
        };
        // (ffmpeg cannot put a GIF into a FLAC file and would fail the whole track: such a picture stays
        // beside the tracks, and is not put into them)
        let art_in = art_in.filter(|a| !(matches!(fate, Fate::StaysFlac | Fate::Flac) && ext_lower(a) == ".gif"));
        let mut argv: Vec<std::ffi::OsString> = tools::LOCAL_ONLY.iter().map(|s| s.into()).collect();
        argv.extend(["-i".into(), t.path.clone().into()]);
        if let Some(a) = &art_in {
            argv.extend(tools::LOCAL_ONLY.iter().map(|s| s.into()));
            argv.push("-i".into());
            argv.push(a.clone().into());
        }
        let maps: [&str; 4] = if art_in.is_some() { ["-map", "0:a:0", "-map", "1:v:0"] } else { ["-map", "0:a:0", "-map", "0:v?"] };
        argv.extend(maps.iter().map(|s| s.into()));
        let out;
        if fate == Fate::Mp3 {
            // lossless of any kind (FLAC, ALAC .m4a, APE, WavPack, DSD...) -> 320kbps MP3
            let mp3_rel = match t.rel.rfind('.') {
                Some(i) => format!("{}.mp3", &t.rel[..i]),
                None => format!("{}.mp3", t.rel),
            };
            out = fit_path(&build, &mp3_rel);
            argv.extend(ctx.mp3_quality.encoder_args().iter().map(|s| s.into()));
            if t.sample_rate > 48000 || t.codec.starts_with("dsd_") {
                argv.extend(["-ar", "44100"].iter().map(|s| s.into())); // MP3 tops out at 48kHz
            }
            argv.extend(["-c:v", "copy", "-disposition:v", "attached_pic", "-map_metadata", "0"].iter().map(|s| s.into()));
            argv.extend(meta);
            argv.extend(["-id3v2_version", "3"].iter().map(|s| s.into()));
        } else if fate == Fate::Flac {
            // lossless downloads are kept: lossless of any other kind (WAV, AIFF, ALAC .m4a, APE, WavPack...)
            // -> FLAC, which holds the same sound sample for sample (but no more than 24 bits of each: a
            // 32-bit file, which is rare, comes out as 24-bit)
            let flac_rel = match t.rel.rfind('.') {
                Some(i) => format!("{}.flac", &t.rel[..i]),
                None => format!("{}.flac", t.rel),
            };
            out = fit_path(&build, &flac_rel);
            argv.extend(["-c:a", "flac"].iter().map(|s| s.into()));
            if t.codec.starts_with("dsd_") {
                argv.extend(["-ar", "88200"].iter().map(|s| s.into())); // DSD is not PCM: read at 352.8kHz, which few players take
            }
            argv.extend(["-c:v", "copy", "-disposition:v", "attached_pic", "-map_metadata", "0"].iter().map(|s| s.into()));
            argv.extend(meta);
            // (facts about an .m4a file it was made from, which ffmpeg would write as tags)
            for k in ["major_brand=", "minor_version=", "compatible_brands="] {
                argv.extend(["-metadata", k].iter().map(|s| s.into()));
            }
            // (a WavPack or Monkey's Audio file names its album artist "Album Artist": with the album artist
            // set above, the old one must not stay beside it under that name)
            if t.has_new("album_artist") {
                argv.extend(["-metadata", "album artist="].iter().map(|s| s.into()));
            }
        } else if fate == Fate::Unwrapped {
            // MP3 audio inside a WAV wrapper: the same MP3 frames, taken out into a plain .mp3 (with a fresh length header)
            let mp3_rel = match t.rel.rfind('.') {
                Some(i) => format!("{}.mp3", &t.rel[..i]),
                None => format!("{}.mp3", t.rel),
            };
            out = fit_path(&build, &mp3_rel);
            argv.extend(["-c:a", "copy", "-c:v", "copy", "-disposition:v", "attached_pic", "-map_metadata", "0"].iter().map(|s| s.into()));
            argv.extend(meta);
            argv.extend(["-id3v2_version", "3", "-f", "mp3"].iter().map(|s| s.into()));
        } else if matches!(fate, Fate::Retagged | Fate::StaysFlac) {
            // (an MP3 or M4A; and a FLAC, where lossless downloads are kept)
            // kept byte for byte; only the tags change, through the same safe editor the Health repairs use. It keeps
            // the lyrics and everything else about the music; the rating, play count and DJ data in a download are
            // another person's, so an import has them taken out ("DropPersonal"; a Health repair never asks for
            // that). ffmpeg rewriting the file is the fallback if the tag can't be edited safely.
            out = fit_path(&build, &t.rel);
            argv.extend(["-c:a", "copy", "-c:v", "copy", "-disposition:v", "attached_pic", "-map_metadata", "0"].iter().map(|s| s.into()));
            argv.extend(meta);
            if t.ext == ".mp3" {
                argv.extend(["-id3v2_version", "3"].iter().map(|s| s.into()));
            }
            let mut args: Vec<std::ffi::OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"].iter().map(|s| s.into()).collect();
            args.extend(argv);
            args.push("--".into());
            args.push(out.clone().into());
            // a missing VBR length header (players show the wrong length, seeking jumps): decoded length far from the header's
            // (only when the file has no length header of its own: rebuilding one that's there changes nothing)
            let need_vbr = t.ext == ".mp3"
                && t.header_secs <= 0.0
                && t.quality.as_ref().map(|q| q.decoded_seconds > 0.0 && q.error_count < 10 && (t.duration - q.decoded_seconds).abs() > (3.0f64).max(q.decoded_seconds * 0.03)).unwrap_or(false);
            let mut plan = serde_json::json!({
                "File": out.to_string_lossy(),
                "Set": taglib_set(t),
                "Junk": t.junk.iter().map(|(f, v, _)| serde_json::json!([f, v])).collect::<Vec<_>>(),
                "Cover": art_in.as_ref().map(|p| p.to_string_lossy().to_string()),
                "CoverMode": if art.is_some() { "all" } else { "missing" },
                "Vbr": need_vbr,
                "DropPersonal": true,
            });
            lyrics::into_plan(&mut plan, &t.lyric, &lyric_acts[ti]);
            jobs.push(Job { mode: Mode::Tags(args, plan), input: t.path.clone(), out, track: Some(ti) });
            continue;
        } else {
            jobs.push(Job { mode: Mode::Copy, input: t.path.clone(), out: fit_path(&build, &t.rel), track: Some(ti) });
            continue;
        }
        let mut args: Vec<std::ffi::OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"].iter().map(|s| s.into()).collect();
        args.extend(argv);
        args.push("--".into());
        args.push(out.clone().into());
        jobs.push(Job { mode: Mode::Ffmpeg(args), input: t.path.clone(), out, track: Some(ti) });
    }
    let mut kept_rels: Vec<String> = u.tracks.iter().map(|t| rel_parent(&t.rel)).collect();
    kept_rels.sort();
    kept_rels.dedup();
    for f in u.files.iter().filter(|f| IMAGE_EXT.contains(&ext_lower(&f.path).as_str())) {
        let stem = file_stem(&f.path.file_name().unwrap().to_string_lossy());
        if art.is_some() && RE_COVER_NAME.is_match(&stem) {
            continue; // the downloaded original replaces it
        }
        // a file that only has a picture's name stays out of the library
        if !crate::importer::images::is_picture(&f.path) {
            ctx.log(&format!("LEFT OUT {} : '{}' is not a picture", u.name, f.path.file_name().unwrap().to_string_lossy()));
            continue;
        }
        let mut rel = f.path.strip_prefix(&u.root).unwrap_or(&f.path).to_string_lossy().replace('/', "\\");
        // images from a flattened "FLAC\" subfolder (or the album root) go next to the tracks
        if kept_rels.len() == 1 && kept_rels[0].is_empty() {
            rel = rel_leaf(&rel);
        }
        jobs.push(Job { mode: Mode::Copy, input: f.path.clone(), out: fit_path(&build, &rel), track: None });
    }
    // two inputs that would write the same file (e.g. "cover.jpg" from two subfolders): keep the first
    // Two TRACKS that would write the same file is a different matter: dropping one silently would lose a song
    // and then recycle the download it came from. That stops the import.
    let mut seen = std::collections::HashSet::new();
    let mut clash: Option<String> = None;
    jobs.retain(|j| {
        if seen.insert(j.out.to_string_lossy().to_lowercase()) {
            return true;
        }
        if !IMAGE_EXT.contains(&ext_lower(&j.out).as_str()) && clash.is_none() {
            clash = j.out.file_name().map(|n| n.to_string_lossy().to_string());
        }
        false
    });
    if let Some(name) = clash {
        {
            let _g = ARTIST_DIRS.lock().unwrap_or_else(|e| e.into_inner());
            let _ = std::fs::remove_dir_all(&build);
            if std::fs::read_dir(&artist_dir).map(|mut d| d.next().is_none()).unwrap_or(false) {
                let _ = std::fs::remove_dir(&artist_dir);
            }
        }
        report(&format!("Two tracks would both be saved as '{name}' - nothing added, download left in place. Rename one of them and rescan."), 1.0);
        ctx.log(&format!("FAIL {} : two tracks map to {name}", u.name));
        return ImportResult::Failed;
    }
    for j in &jobs {
        if let Some(d) = j.out.parent() {
            let _ = std::fs::create_dir_all(d);
        }
    }
    if let Some(a) = &art {
        let _ = std::fs::copy(&a.full, build.join(&a.name));
    }

    // convert several files at once
    let total = jobs.len();
    let next = AtomicUsize::new(0);
    let (tx, rx) = crossbeam_channel::unbounded::<(bool, PathBuf, String)>();
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(total.max(1));
    let mut results: Vec<(bool, PathBuf, String)> = vec![];
    std::thread::scope(|s| {
        let jobs = &jobs;
        let drained = &drained;
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i + 1 >= jobs.len() {
                    drained.fire(); // the last job is taken: the next album can start
                }
                if i >= jobs.len() {
                    break;
                }
                let _slot = Slot::take(); // one conversion per core across all albums
                let j = &jobs[i];
                let ok_run = match &j.mode {
                    Mode::Copy | Mode::Tags(..) => std::fs::copy(&j.input, &j.out).is_ok(),
                    Mode::Ffmpeg(args) => {
                        let mut c = tools::command(&ctx.ffmpeg, false);
                        c.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                        tools::run_limited(&mut c, 1800)
                    }
                };
                let ok = ok_run && std::fs::metadata(&j.out).map(|m| m.len() > 0).unwrap_or(false);
                let _ = tx.send((ok, j.input.clone(), j.out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
            });
        }
        drop(tx);
        for r in rx.iter() {
            results.push(r);
            let n = results.len();
            report(&format!("{} {}/{}: {}", if r_ok(&results) { "Converting" } else { "FAILED" }, n, total, results.last().unwrap().2), n as f64 / total.max(1) as f64);
        }
    });

    // tags on the byte-for-byte copies: one call to the TagLib helper for the whole album
    let mut rewritten: Vec<&Job> = vec![];
    let tag_jobs: Vec<&Job> = jobs.iter().filter(|j| matches!(j.mode, Mode::Tags(..)) && results.iter().any(|r| r.0 && r.1 == j.input)).collect();
    if !tag_jobs.is_empty() {
        report(&format!("Fixing tags on {} track(s)...", tag_jobs.len()), 0.99);
        let plans: Vec<serde_json::Value> = tag_jobs.iter().map(|j| if let Mode::Tags(_, p) = &j.mode { p.clone() } else { serde_json::Value::Null }).collect();
        let res = run_tag_helper(ctx, &plans);
        for j in tag_jobs {
            let Mode::Tags(args, _) = &j.mode else { continue };
            let name = j.out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            match res.get(&j.out.to_string_lossy().to_lowercase()) {
                Some(Ok(note)) => {
                    if !note.is_empty() {
                        ctx.log(&format!("NOTE {name}: {note}"));
                    }
                }
                other => {
                    // couldn't edit the tag safely: rewrite it with ffmpeg, as before
                    let why = match other {
                        Some(Err(w)) => w.clone(),
                        _ => "no answer".into(),
                    };
                    let _ = std::fs::remove_file(&j.out);
                    let mut c = tools::command(&ctx.ffmpeg, false);
                    c.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                    let ok = tools::run_limited(&mut c, 1800) && std::fs::metadata(&j.out).map(|m| m.len() > 0).unwrap_or(false);
                    ctx.log(&format!("NOTE {name}: tags rewritten with ffmpeg ({why})"));
                    if !ok {
                        if let Some(r) = results.iter_mut().find(|r| r.1 == j.input) {
                            r.0 = false;
                        }
                    } else {
                        rewritten.push(j);
                    }
                }
            }
        }
    }
    // What ffmpeg wrote (a converted track, an MP3 taken out of its wrapper, an MP3 it had to rewrite) has lost
    // part of its tags, or keeps them where no player looks: they are carried over from the download's own
    // file into the real fields. This follows every conversion, whatever format it converts to.
    let carry: Vec<&Job> = jobs
        .iter()
        .filter(|j| j.track.is_some() && (matches!(j.mode, Mode::Ffmpeg(..)) || rewritten.iter().any(|r| std::ptr::eq(*r, *j))) && results.iter().any(|r| r.0 && r.1 == j.input))
        .collect();
    if !carry.is_empty() {
        report(&format!("Carrying tags over on {} track(s)...", carry.len()), 0.99);
        let plans: Vec<serde_json::Value> = carry
            .iter()
            .map(|j| {
                let ti = j.track.unwrap_or(0);
                let mut plan = carry_plan(&j.out, &j.input, &u.tracks[ti]);
                lyrics::into_plan(&mut plan, &u.tracks[ti].lyric, &lyric_acts[ti]);
                plan
            })
            .collect();
        for (file, why) in carry_tags(ctx, &plans) {
            ctx.log(&format!("NOTE {file}: the rest of its tags could not be carried over ({why})"));
        }
    }

    let bad: Vec<&(bool, PathBuf, String)> = results.iter().filter(|r| !r.0).collect();
    if !bad.is_empty() || results.len() != total {
        {
            let _g = ARTIST_DIRS.lock().unwrap_or_else(|e| e.into_inner());
            let _ = std::fs::remove_dir_all(&build); // our own half-finished output
            if std::fs::read_dir(&artist_dir).map(|mut d| d.next().is_none()).unwrap_or(false) {
                let _ = std::fs::remove_dir(&artist_dir);
            }
        }
        report(&format!("{} file(s) failed - nothing added, original left in place", bad.len().max(total - results.len())), 1.0);
        ctx.log(&format!("FAIL {} -> {} : {}", u.name, final_dir.display(), bad.iter().map(|b| b.1.display().to_string()).collect::<Vec<_>>().join("; ")));
        return ImportResult::Failed;
    }

    // from here on the library itself changes: one writer at a time (see `library_lock`), so a Health repair
    // or an Undo never works on what this import is putting in place, nor this import on what they are
    let _held = crate::library_lock::hold();

    // "Replace only these tracks": the album in the library stays, and only the tracks Health flagged are
    // swapped for the ones just built, each matched by its number and its length
    if let Some((want, old)) = existing.as_ref().and_then(|e| crate::replace::wanted(&ctx.replacements, e).map(|w| (w, e))).filter(|(w, _)| !w.files.is_empty()) {
        let swapped = replace_tracks(ctx, u, &want, old, &build);
        let _ = std::fs::remove_dir_all(&build); // what was built and not used is the import's own scratch
        return match swapped {
            Ok(n) => {
                crate::replace::done(&ctx.replacements, old);
                crate::links::folder_changed(old); // Health looks at this artist again
                ctx.log(&format!("REPLACED {n} track(s) in {} (old files kept in the undo backups)", old.display()));
                if !KEEP_ORIGINALS.load(Ordering::Relaxed) && !changed_since_scan(u) && remove_original(ctx, u) {
                    report(&format!("Replaced {n} {} - the rest of the album was left as it is", if n == 1 { "track" } else { "tracks" }), 1.0);
                    ImportResult::Ok
                } else {
                    report(&format!("Replaced {n} {} - the rest of the album was left as it is. The download was left in place.", if n == 1 { "track" } else { "tracks" }), 1.0);
                    ImportResult::OkKept
                }
            }
            Err(why) => {
                report(&format!("{why}. Nothing in the library was changed."), 1.0);
                ctx.log(&format!("FAIL replace tracks in {} : {why}", old.display()));
                ImportResult::Failed
            }
        };
    }

    // swap the finished album into place
    // the old copy is never deleted: it goes to the undo backups, where Repair history can put it back
    let mut old_copy_note: Option<String> = None;
    match place_album(&ctx.repair_root, &ctx.library, &u.artist, &build, &final_dir, existing.as_deref()) {
        Err(why) => {
            report(&why, 1.0);
            ctx.log(&format!("FAIL swap {} : {why}", final_dir.display()));
            return ImportResult::Failed;
        }
        Ok(Placed { old_copy, problem }) => {
            if let Some(e) = &existing {
                crate::replace::done(&ctx.replacements, e); // if Health was waiting for this album, it has its better copy now
                match (problem, old_copy) {
                    (Some(why), _) => {
                        ctx.log(&format!("REPLACED {} ({why})", e.display()));
                        old_copy_note = Some(why);
                    }
                    (None, Some(kept)) => ctx.log(&format!("REPLACED {} (old copy kept in the undo backups: {})", e.display(), kept.display())),
                    (None, None) => {}
                }
            }
        }
    }
    for t in u.tracks.iter().filter(|t| t.unwrapped) {
        if let Some(d) = t.path.parent() {
            let _ = std::fs::remove_dir_all(d); // the temporary unwrapped copies
        }
    }

    let mut cover_note = match (&art, &u.cover) {
        (Some(a), Some(c)) => format!("{} cover {}", c.source, a.size),
        _ => "kept original art".to_string(),
    };
    // loose tracks that were waiting for this album: the import is done and the album is in place, so the
    // ones it now holds (the same title and length) go to the Recycle Bin
    if let Some(said) = crate::loose::album_arrived(&ctx.loose_wants, &ctx.ffprobe, &ctx.library, &final_dir, &ctx.recycle, ctx.keep_lossless) {
        ctx.log(&format!("LOOSE {said}"));
        cover_note = format!("{cover_note}. Also: {said}");
    }
    if let Some(why) = &old_copy_note {
        cover_note = format!("{cover_note}. But {why}");
    }
    // Health looks at this artist again, now that everything about the import is in place: what it said about
    // the folder is no longer true
    crate::links::folder_changed(&final_dir);
    let verb = if existing.is_some() { "Replaced" } else { "Done" };
    ctx.log(&format!(
        "OK   {} -> {} | {}cover={} | fixes: {}",
        u.name,
        final_dir.display(),
        if existing.is_some() { "replaced old copy | " } else { "" },
        match (&art, &u.cover) {
            (Some(_), Some(c)) => c.source.clone(),
            _ => "kept".into(),
        },
        u.format_fixes()
    ));
    // files that arrived after the scan were not imported: never recycle a folder that holds more than was read
    if !KEEP_ORIGINALS.load(Ordering::Relaxed) && changed_since_scan(u) {
        report(&format!("{verb} - {cover_note}. The download folder changed after it was scanned (files were added or changed), so it was left in place. Rescan to bring in the rest."), 1.0);
        ctx.log(&format!("KEPT original (changed since scan): {}", u.item.display()));
        return ImportResult::OkKept;
    }
    if remove_original(ctx, u) {
        report(&format!("{verb} - {cover_note}"), 1.0);
        return ImportResult::Ok;
    }
    report(&format!("{verb} - {cover_note}. Soulseek still has the original open, so it was left in place; Rescan later to remove it."), 1.0);
    ctx.log(&format!("KEPT original (in use): {}", u.item.display()));
    ImportResult::OkKept
}

/// Swaps single tracks of an album in the library for the ones just built from a download. Every track asked
/// for must have its match in the download (the same track number, and the same disc when both say one,
/// with a length within three seconds of what the old file says it should be); otherwise nothing is changed.
/// A track keeps its format: where the download was built into another one (FLAC into MP3), the download's
/// own file is swapped in if it is the old track's format, and if it is not, nothing is changed.
/// The old files go to the undo backups in one batch. Returns how many were swapped.
fn replace_tracks(ctx: &Ctx, u: &Unit, want: &crate::replace::Want, album: &Path, build: &Path) -> Result<usize, String> {
    use crate::importer::scan::lead_number;
    let leaf = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // (disc, number, seconds) as the tags say, the number from the file's name when the tag has none
    let read = |f: &Path| {
        let p = crate::importer::probe::probe(&ctx.ffprobe, f);
        let tag = |k: &str| p.tags.get(k).cloned().unwrap_or_default();
        let named = crate::importer::scan::parse_file_name(&crate::importer::util::file_stem(&leaf(f)), "");
        (lead_number(&tag("disc")).or(named.disc).unwrap_or(0), lead_number(&tag("track")).or(named.track).unwrap_or(0), p.duration)
    };
    let built: Vec<(PathBuf, (u32, u32, f64))> = walk_files(build).into_iter().filter(|f| crate::importer::scan::is_audio_ext(&crate::importer::util::ext_lower(f))).map(|f| { let r = read(&f); (f, r) }).collect();
    let mut pairs: Vec<(PathBuf, PathBuf)> = vec![];
    for old in want.files.iter().map(PathBuf::from) {
        if !old.starts_with(album) || !old.is_file() {
            return Err(format!("'{}' is no longer in the album", leaf(&old)));
        }
        let (disc, no, secs) = read(&old);
        if no == 0 {
            return Err(format!("'{}' has no track number to match it by", leaf(&old)));
        }
        let Some((new, (_, _, new_secs))) = built.iter().find(|(_, (d, n, _))| *n == no && (*d == 0 || disc == 0 || *d == disc)) else {
            return Err(format!("This download has no track {no} to take the place of '{}'", leaf(&old)));
        };
        if secs > 0.0 && (new_secs - secs).abs() > 3.0 {
            return Err(format!("Track {no} of this download is {} long and '{}' should be {}: not the same recording", crate::importer::util::format_time(*new_secs), leaf(&old), crate::importer::util::format_time(secs)));
        }
        // a swap never changes a track's format (a FLAC album would end up with one MP3 in it, where
        // lossless downloads are built as MP3). When what was built is another format, the download's own file
        // is taken as it is, if that is the old track's format; if not, nothing is swapped
        let kind = |p: &Path| crate::importer::util::ext_lower(p);
        if kind(new) == kind(&old) {
            pairs.push((old, new.clone()));
            continue;
        }
        let own = u.tracks.iter().filter(|t| !t.wrapped && !t.unwrapped && kind(&t.path) == kind(&old)).find(|t| {
            let (d, n, s) = read(&t.path);
            n == no && (d == 0 || disc == 0 || d == disc) && !(secs > 0.0 && (s - secs).abs() > 3.0)
        });
        let Some(own) = own else {
            let name = |p: &Path| kind(p).trim_start_matches('.').to_uppercase();
            return Err(format!("'{}' is {} and track {no} of this download would come in as {}, which would leave the album in two formats. Find a {} copy, or use \"Replace this album\"", leaf(&old), name(&old), name(new), name(&old)));
        };
        // a copy of it, so the download itself stays whole
        let copy = build.join(format!("as-it-is-{}", pairs.len())).join(leaf(&old));
        copy.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::copy(&own.path, &copy)).map_err(|e| crate::util::plain_err(format!("'{}' could not be copied: {e}", leaf(&own.path))))?;
        pairs.push((old, copy));
    }
    if pairs.is_empty() {
        return Err("No track was asked for".into());
    }
    let title = format!("Import: replaced {} {} of {} - {}", pairs.len(), if pairs.len() == 1 { "track" } else { "tracks" }, u.artist, leaf(album));
    let batch = crate::undo::Batch::begin(&ctx.repair_root, &ctx.library, &title)?;
    let mut failed = None;
    for (old, new) in &pairs {
        // (every pair is one format: the new file takes the old one's place and name)
        if let Err(why) = batch.replaced(old, new, "replaced by a better copy") {
            failed = Some(format!("'{}': {why}", leaf(old)));
            break;
        }
    }
    let (changes, id) = (batch.changes(), batch.id.clone());
    batch.close();
    match failed {
        None => Ok(pairs.len()),
        // a swap that stopped half way is put back, so the album is as it was
        Some(why) => {
            if changes > 0 {
                let _ = crate::undo::undo(&ctx.repair_root, &id);
            }
            Err(why)
        }
    }
}

fn r_ok(results: &[(bool, PathBuf, String)]) -> bool {
    results.last().map(|r| r.0).unwrap_or(true)
}


// ---------- a whole run: several albums, overlapped ----------
/// At most this many albums are being built at once.
const ALBUMS_AT_ONCE: usize = 2;
/// Covers are downloaded this many albums ahead of the one being converted.
const COVERS_AHEAD: usize = 3;

pub struct ImportJob {
    pub id: usize,
    pub unit: Unit,
    pub artist: String,
    pub folder: String,
    pub use_cover: bool,
}

pub enum Event {
    /// the final artist / folder names (sent for every album before the first one starts)
    Names { id: usize, artist: String, folder: String },
    Started { id: usize, label: String },
    Status { id: usize, text: String, frac: f64 },
    Done { id: usize, result: ImportResult, replaced: bool },
}

#[derive(Default)]
struct Sched {
    started: usize,
    in_flight: usize,
    /// the newest album has handed out all its conversion jobs
    drained: bool,
    keys: std::collections::HashSet<String>,
    stop: bool,
}

/// Imports the albums in order, two at a time at most: the next one starts once the current one has handed out all its
/// conversion jobs. Never two albums for the same artist + folder at once. Covers download in the background, a few albums ahead.
pub fn import_many(ctx: &Ctx, jobs: Vec<ImportJob>, on: &(dyn Fn(Event) + Sync)) -> Vec<(usize, ImportResult)> {
    let total = jobs.len();
    let mut prepared: Vec<(usize, Unit)> = Vec::with_capacity(total);
    for j in jobs {
        let (id, mut u) = (j.id, j.unit);
        let scanned = (u.artist.clone(), u.folder.clone());
        crate::importer::scan::set_unit_artist(ctx, &mut u, &j.artist);
        u.folder = clean_name(&j.folder);
        u.use_cover = j.use_cover;
        // the names were edited after the scan: what the scan decided (new, replace, already there) was about a
        // different place in the library, so work it out again for where the album is really going
        if (u.artist.clone(), u.folder.clone()) != scanned {
            crate::importer::scan::update_unit_action(ctx, &mut u);
            // Import was clicked with this row ticked and these names typed in: a near match is added beside
            u.include = true;
        }
        on(Event::Names { id, artist: u.artist.clone(), folder: u.folder.clone() });
        prepared.push((id, u));
    }
    // covers worth fetching ahead: not for leftovers or albums already in the library
    let slots: Vec<Option<std::sync::Arc<CoverSlot>>> = prepared
        .iter()
        .map(|(_, u)| (u.use_cover && u.cover.is_some() && !matches!(u.action, Action::Cleanup | Action::Exists)).then(|| std::sync::Arc::new(CoverSlot::new())))
        .collect();
    let covers: Vec<Option<Cover>> = prepared.iter().map(|(_, u)| u.cover.clone()).collect();
    let sched: (Mutex<Sched>, Condvar) = (Mutex::new(Sched::default()), Condvar::new());
    let lock = || sched.0.lock().unwrap_or_else(|e| e.into_inner());
    let results: Mutex<Vec<(usize, ImportResult)>> = Mutex::new(vec![]);

    std::thread::scope(|s| {
        // cover downloads, in import order
        s.spawn(|| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for (i, slot) in slots.iter().enumerate() {
                    let (Some(slot), Some(cover)) = (slot, &covers[i]) else { continue };
                    {
                        let mut g = lock();
                        while !g.stop && i >= g.started + COVERS_AHEAD {
                            g = sched.1.wait(g).unwrap_or_else(|e| e.into_inner());
                        }
                        if g.stop {
                            break;
                        }
                    }
                    if !slot.is_ready() {
                        slot.put(fetch_cover(ctx, cover));
                    }
                }
            }));
            for slot in slots.iter().flatten() {
                slot.put(None);
            }
        });

        for (n, (id, mut u)) in prepared.into_iter().enumerate() {
            let key = crate::importer::scan::batch_key(&u);
            {
                let mut g = lock();
                while g.in_flight >= ALBUMS_AT_ONCE || (g.in_flight > 0 && !g.drained) || g.keys.contains(&key) {
                    g = sched.1.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                g.in_flight += 1;
                g.started += 1;
                g.drained = false;
                g.keys.insert(key.clone());
                sched.1.notify_all();
            }
            on(Event::Started { id, label: format!("Importing {} of {}: {} - {}", n + 1, total, u.artist, u.folder) });
            let (sched, results, slot) = (&sched, &results, slots[n].clone());
            s.spawn(move || {
                let report = |text: &str, frac: f64| on(Event::Status { id, text: text.to_string(), frac });
                let mark_drained = || {
                    sched.0.lock().unwrap_or_else(|e| e.into_inner()).drained = true;
                    sched.1.notify_all();
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| import_unit(ctx, &mut u, &report, slot.as_deref(), &mark_drained))).unwrap_or_else(|e| {
                    let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    ctx.log(&format!("FAIL {}: {}", u.name, msg));
                    report("Stopped unexpectedly - original left in place", 1.0);
                    ImportResult::Failed
                });
                let replaced = u.action == Action::Replace && matches!(result, ImportResult::Ok | ImportResult::OkKept);
                results.lock().unwrap_or_else(|e| e.into_inner()).push((id, result));
                on(Event::Done { id, result, replaced });
                if let Some(sl) = &slot {
                    sl.discard(); // frees an unused cover's temp folder
                }
                let mut g = sched.0.lock().unwrap_or_else(|e| e.into_inner());
                g.in_flight -= 1;
                g.drained = true;
                g.keys.remove(&key);
                if g.in_flight == 0 && g.started == total {
                    g.stop = true;
                }
                sched.1.notify_all();
            });
        }
        let mut g = lock();
        if total == 0 {
            g.stop = true;
        }
        sched.1.notify_all();
    });
    results.into_inner().unwrap_or_else(|e| e.into_inner())
}
