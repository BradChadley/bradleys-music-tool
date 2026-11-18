//! Backup through the engine (ENGINE-PROTOCOL.md, "Protocol 5"). The work is the Music Tool's backup worker
//! (`crate::backup::api`, which starts `backup\native.rs`); this is the service around it:
//!   * the test-run refusal: a test run never copies, binds, ejects or schedules (a preview, a whole check
//!     and "find out which file is sound" only read, and run);
//!   * moves follow: before a backup copies anything, its copies are renamed to match the moves written
//!     down in `<Tools>\backup-moves.jsonl`, and the batches followed are kept per backup (by its marker)
//!     in `<Tools>\backup\moves-followed.json`;
//!   * bringing a sound copy back into the library goes through the library guard, `library.hold` /
//!     `library.release` and one undo batch of kind "restore";
//!   * the nightly task is made only by `backup.schedule` with `on: true` (a click in Cadence), through the
//!     injectable `Scheduler`; it runs `cadence-engine.exe --backup-nightly`, with no window, and does not
//!     start while the engine is changing the library (the lock file `<Tools>\library-changing.lock`).
use super::jobs::{fail, load, Failure, OFFLINE};
use super::library::{bring_up_to_date, folder_of, hold, release, waits};
use super::{Engine, Env, Job, Out};
use crate::backup::api::{self, Followed, Moved, RunKind, Setup, Snapshots, Which};
use crate::guard::{self, Guard};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the engine keeps about Backup while it runs: the runs under way (one per backup), and the library's
/// size, measured at most every two minutes (a library of 24,000 songs takes a second or two to walk).
#[derive(Default)]
pub(crate) struct State {
    running: Mutex<Vec<(Which, &'static str)>>,
    sizes: Mutex<HashMap<String, (Instant, (u64, u64))>>,
}

/// The engine's backup setup: its own settings and logs in `<Tools>\backup`, Cadence's own data saved
/// beside the music, drives as the parts say.
pub(crate) fn setup(env: &Env) -> Setup {
    Setup {
        dir: env.dir.join("backup"),
        disks: env.parts.disks.clone(),
        snapshots: env.parts.cadence_data.clone().map(Snapshots::Cadence).unwrap_or(Snapshots::Nothing),
        recycle: env.parts.trash,
        nightly: false,
    }
}

fn which_of(params: &Value) -> Result<Which, Failure> {
    let w = params.get("which").and_then(Value::as_str).unwrap_or("main");
    api::which_of(w).ok_or_else(|| fail("bad_request", "Say which backup: \"main\" or \"second\"."))
}

fn failed(why: String) -> Failure {
    (api::code_of(&why).to_string(), why)
}

fn library_of(params: &Value) -> Result<PathBuf, Failure> {
    let folder = folder_of(params, "Backup")?;
    if !folder.is_dir() {
        return Err(fail("not_found", format!("The music folder {} could not be found.", folder.display())));
    }
    Ok(folder)
}

impl State {
    fn size(&self, library: &Path) -> Option<(u64, u64)> {
        let k = library.to_string_lossy().to_lowercase();
        if let Some((when, s)) = self.sizes.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
            if when.elapsed() < Duration::from_secs(120) {
                return Some(*s);
            }
        }
        let s = api::library_size(library).ok()?;
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).insert(k, (Instant::now(), s));
        Some(s)
    }

    /// Marks a run of this backup as under way; None when one already is.
    fn start(&self, which: Which, kind: &'static str) -> Option<Running<'_>> {
        let mut r = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if r.iter().any(|(w, _)| *w == which) {
            return None;
        }
        r.push((which, kind));
        Some(Running { state: self, which })
    }

    fn now(&self) -> Vec<(Which, &'static str)> {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A run under way; it is off the list when this is dropped.
struct Running<'a> {
    state: &'a State,
    which: Which,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.state.running.lock().unwrap_or_else(|e| e.into_inner()).retain(|(w, _)| *w != self.which);
    }
}

// ------------------------------------------------------------------ the library lock, seen from another process

/// While the engine holds the library (between `library.hold` and `library.release`) it keeps
/// `<Tools>\library-changing.lock` open with no sharing and "delete on close", so the nightly backup (another
/// process) can see it and wait. Windows removes the file when the engine ends, however it ends.
pub(crate) mod changing {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static HELD: Mutex<Option<HashMap<PathBuf, (usize, Option<std::fs::File>)>>> = Mutex::new(None);

    fn file(dir: &Path) -> PathBuf {
        dir.join("library-changing.lock")
    }

    /// The engine starts changing the library (a hold was given).
    pub(crate) fn enter(dir: &Path) {
        use std::os::windows::fs::OpenOptionsExt;
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let e = held.get_or_insert_with(HashMap::new).entry(dir.to_path_buf()).or_insert((0, None));
        e.0 += 1;
        if e.1.is_none() {
            // FILE_FLAG_DELETE_ON_CLOSE; no sharing at all
            let opened = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).share_mode(0).custom_flags(0x0400_0000).open(file(dir));
            match opened {
                Ok(f) => e.1 = Some(f),
                Err(why) => eprintln!("cadence-engine: the library lock file could not be made ({why}); the nightly backup cannot see this change"),
            }
        }
    }

    /// The engine is done changing the library (the hold was released).
    pub(crate) fn leave(dir: &Path) {
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let Some(map) = held.as_mut() else { return };
        if let Some(e) = map.get_mut(dir) {
            e.0 = e.0.saturating_sub(1);
            if e.0 == 0 {
                map.remove(dir);
            }
        }
    }

    /// Whether an engine (this one, or another process) is changing the library now.
    pub(crate) fn busy(dir: &Path) -> bool {
        use std::os::windows::fs::OpenOptionsExt;
        if HELD.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|m| m.get(dir).is_some_and(|e| e.0 > 0)) {
            return true;
        }
        let f = file(dir);
        // (held open by an engine: it cannot be opened; left behind by nothing: Windows removed it)
        f.exists() && std::fs::OpenOptions::new().read(true).share_mode(0).open(&f).is_err()
    }
}

// ------------------------------------------------------------------ moves follow

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct FollowedList {
    /// by the backup's marker: the batches of moves it has followed
    #[serde(flatten)]
    by_marker: HashMap<String, Vec<String>>,
}

fn followed_file(env: &Env) -> PathBuf {
    env.dir.join("backup").join("moves-followed.json")
}

/// The moves this backup has not followed yet, oldest first, and the batches they come from.
fn pending(env: &Env, marker: Option<&str>) -> (Vec<Moved>, Vec<String>) {
    let Some(marker) = marker else { return (vec![], vec![]) };
    let done: FollowedList = std::fs::read(followed_file(env)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let seen = done.by_marker.get(marker).cloned().unwrap_or_default();
    let (mut moves, mut batches) = (vec![], vec![]);
    for (batch, list) in super::moves::for_backup_list(&env.dir) {
        if seen.contains(&batch) || batches.contains(&batch) {
            continue;
        }
        moves.extend(list.into_iter().map(|m| Moved { from: m.from, to: m.to }));
        batches.push(batch);
    }
    (moves, batches)
}

/// Marks these batches as followed by this backup.
fn mark_followed(env: &Env, marker: &str, batches: &[String]) {
    if batches.is_empty() {
        return;
    }
    let file = followed_file(env);
    let mut done: FollowedList = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let list = done.by_marker.entry(marker.to_string()).or_default();
    for b in batches {
        if !list.contains(b) {
            list.push(b.clone());
        }
    }
    let written = serde_json::to_vec_pretty(&done).map_err(std::io::Error::other).and_then(|t| {
        std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
        crate::util::write_atomic(&file, &t)
    });
    if let Err(why) = written {
        eprintln!("cadence-engine: the moves Backup followed could not be written down ({why}); the next backup looks at them again, harmlessly");
    }
}

/// A backup (or a mirror), with the moves it has not followed yet; they are marked once followed, whatever
/// the copying after them did.
/// `nightly`: started by the nightly task, so the run is marked so in the log.
fn backup_with_moves(env: &Env, which: Which, kind: RunKind, library: &Path, nightly: bool, stop: &AtomicBool, progress: &mut dyn FnMut(Value)) -> Result<Value, Failure> {
    let s = Setup { nightly, ..setup(env) };
    let marker = api::marker(&s, which);
    let (moves, batches) = pending(env, marker.as_deref());
    let (ended, followed): (Result<Value, String>, Option<Followed>) = api::run(&s, which, kind, library, &moves, stop, progress);
    if let (Some(_), Some(m)) = (followed, &marker) {
        mark_followed(env, m, &batches);
    }
    ended.map_err(failed)
}

// ------------------------------------------------------------------ the methods

/// `backup.status`: both cards, the nightly backup, and what needs a look. Only reads.
pub(crate) fn status(eng: &Engine, params: &Value) -> Result<Value, Failure> {
    let library = folder_of(params, "Backup")?;
    let mut v = api::status(&setup(&eng.env), &library, eng.env.parts.scheduler.as_ref());
    let running: Vec<Value> = eng.backup.now().into_iter().map(|(w, k)| json!({ "which": api::which_name(w), "kind": k })).collect();
    v["running"] = json!(running);
    Ok(v)
}

/// `backup.drives`: the drives a backup could go to. Only reads.
pub(crate) fn drives(eng: &Engine, params: &Value) -> Result<Value, Failure> {
    let library = library_of(params)?;
    Ok(api::drives(&setup(&eng.env), &library, eng.backup.size(&library)))
}

fn to_of(params: &Value) -> Result<PathBuf, Failure> {
    let to = params.get("to").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if to.is_empty() {
        return Err(fail("bad_request", "Say which folder the backup goes to (\"to\")."));
    }
    Ok(PathBuf::from(to.trim_end_matches(['\\', '/'])))
}

/// `backup.preview_folders`: both folders of a pair before it is confirmed. Only reads.
pub(crate) fn preview_folders(eng: &Engine, params: &Value) -> Result<Value, Failure> {
    let library = library_of(params)?;
    let which = which_of(params)?;
    let to = to_of(params)?;
    Ok(api::preview_folders(&setup(&eng.env), which, &library, &to, eng.backup.size(&library)))
}

/// `backup.confirm_folders`: enrolls a library and backup folder pair. Only with `"confirm": true` (Cadence
/// sends it only from the Confirm click), never in a test run, never while that backup runs.
pub(crate) fn confirm_folders(eng: &Engine, params: &Value) -> Result<Value, Failure> {
    if eng.env.offline_test {
        return Err(fail(OFFLINE, "No backup folder is confirmed while Cadence runs as a test."));
    }
    if params.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err(fail("bad_request", "Folders are confirmed only with \"confirm\": true, from the Confirm button."));
    }
    let library = library_of(params)?;
    let which = which_of(params)?;
    let to = to_of(params)?;
    let _running = eng.backup.start(which, "confirm").ok_or_else(|| fail("busy", "That backup is running. Confirm the folders when it is done."))?;
    let words = api::confirm(&setup(&eng.env), which, &library, &to).map_err(failed)?;
    Ok(json!({ "confirmed": true, "words": words }))
}

pub(crate) struct RunAsk {
    library: PathBuf,
    which: Which,
    kind: RunKind,
    name: &'static str,
}

/// `backup.run`, before it starts: the kind, a test run (only a preview runs), the library mid-change.
pub(crate) fn run_check(env: &Env, params: &Value) -> Result<RunAsk, Failure> {
    let (kind, name) = match params.get("kind").and_then(Value::as_str).unwrap_or("backup") {
        "backup" => (RunKind::Backup, "backup"),
        "mirror" => (RunKind::Mirror, "mirror"),
        "preview" => (RunKind::Preview, "preview"),
        other => return Err(fail("bad_request", format!("A backup run is \"backup\", \"mirror\" or \"preview\", not \"{other}\"."))),
    };
    if env.offline_test && kind != RunKind::Preview {
        return Err(fail(OFFLINE, "Nothing is copied while Cadence runs as a test."));
    }
    let library = library_of(params)?;
    let which = which_of(params)?;
    if kind != RunKind::Preview && changing::busy(&env.dir) {
        return Err(fail("busy", "Cadence is changing your library right now. Back up again when that is done."));
    }
    Ok(RunAsk { library, which, kind, name })
}

/// `backup.run` (job): follows the moves, copies what is new or changed, reads each copy back and checks
/// it, keeps replaced and removed files in backup history, then saves Cadence's own data.
pub(crate) fn run(out: &Out, job: &Job, id: u64, eng: &Engine, ask: RunAsk) -> Result<Value, Failure> {
    let _running = eng.backup.start(ask.which, ask.name).ok_or_else(|| fail("busy", "That backup is already running."))?;
    let stop = stop_flag(job);
    let mut progress = |v: Value| out.progress(id, v);
    let r = if ask.kind == RunKind::Preview {
        api::run(&setup(&eng.env), ask.which, RunKind::Preview, &ask.library, &[], &stop, &mut progress).0.map_err(failed)
    } else {
        backup_with_moves(&eng.env, ask.which, ask.kind, &ask.library, false, &stop, &mut progress)
    };
    r
}

/// The job's stop, as the worker's own flag (which it looks at between files and between buffers): cancel
/// sets it at once, so a long copy or check stops within one buffer.
struct StopFlag<'a> {
    flag: Arc<AtomicBool>,
    job: &'a Job,
}

impl std::ops::Deref for StopFlag<'_> {
    type Target = AtomicBool;
    fn deref(&self) -> &AtomicBool {
        &self.flag
    }
}

fn stop_flag(job: &Job) -> StopFlag<'_> {
    let flag = Arc::new(AtomicBool::new(job.stopped()));
    let f = flag.clone();
    *job.on_stop.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || f.store(true, Ordering::Relaxed)));
    StopFlag { flag, job }
}

impl Drop for StopFlag<'_> {
    fn drop(&mut self) {
        *self.job.on_stop.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub(crate) struct CheckAsk {
    library: PathBuf,
    which: Which,
}

pub(crate) fn check_check(params: &Value) -> Result<CheckAsk, Failure> {
    Ok(CheckAsk { library: library_of(params)?, which: which_of(params)? })
}

/// `backup.check` (job): every file in the library and its copy, compared. Nothing is copied or changed
/// (it runs in a test run too: it writes only its line in the engine's own log).
pub(crate) fn check(out: &Out, job: &Job, id: u64, eng: &Engine, ask: CheckAsk) -> Result<Value, Failure> {
    let _running = eng.backup.start(ask.which, "check").ok_or_else(|| fail("busy", "That backup is already running."))?;
    let stop = stop_flag(job);
    let r = api::run(&setup(&eng.env), ask.which, RunKind::Check, &ask.library, &[], &stop, &mut |v| out.progress(id, v)).0.map_err(failed);
    r
}

pub(crate) struct RepairAsk {
    library: PathBuf,
    which: Which,
    choice: &'static str,
    files: Vec<String>,
}

/// `backup.repair_damaged`, before it starts: the choice, the files (paths inside the library, as the check
/// named them), and a test run (only "find_out", which changes nothing, runs).
pub(crate) fn repair_check(env: &Env, params: &Value) -> Result<RepairAsk, Failure> {
    let choice = match params.get("choice").and_then(Value::as_str).unwrap_or_default() {
        "copy_again" => "copy_again",
        "bring_back" => "bring_back",
        "find_out" => "find_out",
        _ => return Err(fail("bad_request", "Say what to do with the damaged copies: \"copy_again\", \"bring_back\" or \"find_out\".")),
    };
    if env.offline_test && choice != "find_out" {
        return Err(fail(OFFLINE, "Nothing is repaired while Cadence runs as a test."));
    }
    let library = library_of(params)?;
    let which = which_of(params)?;
    let mut files = vec![];
    for f in params.get("files").and_then(Value::as_array).into_iter().flatten() {
        let rel = f.as_str().map(str::trim).unwrap_or_default();
        let plain = !rel.is_empty() && Path::new(rel).components().all(|c| matches!(c, std::path::Component::Normal(_)));
        if !plain {
            return Err(fail("bad_request", format!("\"{rel}\" is not a file inside the library, as the check names them.")));
        }
        files.push(rel.to_string());
    }
    if files.is_empty() {
        return Err(fail("bad_request", "Name the damaged files, as the check named them (\"files\")."));
    }
    Ok(RepairAsk { library, which, choice, files })
}

/// `backup.repair_damaged` (job). "copy_again": the library's files are sound; fresh copies are made and
/// checked, and the damaged copies kept in backup history. "bring_back": the copies are sound; each takes
/// the damaged file's place in the library, under the guard and a hold, as one batch of kind "restore" in
/// Repair history (Undo puts the damaged files back). "find_out": listens to both files of each pair and
/// says which is sound; nothing is changed.
pub(crate) fn repair(out: &Out, job: &Job, id: u64, eng: &Engine, ask: RepairAsk) -> Result<Value, Failure> {
    let env = &eng.env;
    let s = setup(env);
    match ask.choice {
        "copy_again" => {
            let _running = eng.backup.start(ask.which, "copy_again").ok_or_else(|| fail("busy", "That backup is running. Try again when it is done."))?;
            let stop = stop_flag(job);
            let r = api::run(&s, ask.which, RunKind::Recopy(ask.files), &ask.library, &[], &stop, &mut |v| out.progress(id, v)).0.map_err(failed);
                    r
        }
        "find_out" => {
            let folder = api::confirmed_folder(&s, ask.which, &ask.library).map_err(failed)?;
            let stop = stop_flag(job);
            let total = ask.files.len();
            let mut done = 0;
            let found = api::find_out(&ask.library, &folder, &ask.files, &stop, &mut |file, sound, words| {
                done += 1;
                out.progress(id, json!({ "done": done, "total": total, "file": file, "sound": sound, "words": words }));
            });
                    let found = found.map_err(|why| if why.starts_with("Stopped") { fail("cancelled", why) } else { fail("missing_helper", why) })?;
            Ok(json!({ "files": found }))
        }
        _ => bring_back(out, job, id, eng, ask),
    }
}

/// Bringing sound copies back into the library: the guard, the hold, one "restore" batch, the report brought
/// up to date, the release.
fn bring_back(out: &Out, job: &Job, id: u64, eng: &Engine, ask: RepairAsk) -> Result<Value, Failure> {
    let env = &eng.env;
    let g = Guard::for_library(&ask.library)?;
    let _running = eng.backup.start(ask.which, "bring_back").ok_or_else(|| fail("busy", "That backup is running. Try again when it is done."))?;
    // (the backup's folder is checked before anything is asked of Cadence)
    api::confirmed_folder(&setup(env), ask.which, &ask.library).map_err(failed)?;
    let title = format!("Backup: bring back {}", if ask.files.len() == 1 { "1 file".to_string() } else { format!("{} files", ask.files.len()) });
    let held = hold(out, env, job, &title, "bring back")?;
    let renewed = std::cell::Cell::new(std::time::Instant::now());
    let ended: std::cell::RefCell<Option<Failure>> = std::cell::RefCell::new(None);
    let tools = super::library::helpers().ok();
    let mut vet = |file: &Path| -> Result<(), String> {
        let stop = |f: Failure| {
            let words = f.1.clone();
            *ended.borrow_mut() = Some(f);
            words
        };
        g.still_there().map_err(stop)?;
        if g.wait_not_playing(&|p| guard::covers(file, p), job, out, id, waits(env)).map_err(stop)?.is_some() {
            return Err(guard::STILL_PLAYING.to_string());
        }
        super::library::renew(out, env, &held.token, &renewed);
        g.file_ok(file).map_err(|r| r.words().to_string())
    };
    let stopped = || job.stopped() || ended.borrow().is_some();
    out.progress(id, json!({ "done": 0, "total": ask.files.len(), "words": title }));
    let done = api::bring_back_files(&setup(env), ask.which, &ask.library, &ask.files, tools.as_ref().map(|(a, b)| (a.as_path(), b.as_path())), &mut vet, &stopped);
    if tools.is_some() {
        crate::importer::quality::save_cache(&env.dir.join("cache").join("quality-cache.txt"));
    }
    if let Ok(d) = &done {
        if d.batch.is_some() && ask.library.is_dir() {
            let places: Vec<PathBuf> = ask.files.iter().map(|r| ask.library.join(r)).collect();
            bring_up_to_date(env, &ask.library, &places);
        }
    }
    release(out, env, held);
    if let Some(f) = ended.into_inner() {
        return Err(f);
    }
    let done = done.map_err(failed)?;
    if job.stopped() {
        return Err(fail("cancelled", "It was stopped; what was brought back is one entry in Repair history."));
    }
    Ok(json!({ "batch": done.batch, "changed": done.changed, "problems": done.problems }))
}

/// `backup.eject` (job): asks Windows to let go of the backup's drive, and says it can be pulled out only
/// once Windows has really let go.
pub(crate) fn eject_check(env: &Env, params: &Value) -> Result<Which, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "No drive is ejected while Cadence runs as a test."));
    }
    which_of(params)
}

pub(crate) fn eject(eng: &Engine, which: Which) -> Result<Value, Failure> {
    let _running = eng.backup.start(which, "eject").ok_or_else(|| fail("busy", "A backup to this drive is running. Do not pull it out yet."))?;
    match api::eject_drive(&setup(&eng.env), which) {
        Ok(words) => Ok(json!({ "ejected": true, "words": words })),
        Err(why) => Err(fail("drive_busy", why)),
    }
}

/// `backup.schedule`: the nightly backup on or off. Only from the person's own click (Cadence never sends
/// it by itself); refused in a test run. On needs the main backup confirmed. Only Cadence's own task
/// ("Cadence Backup (nightly)") is ever made or removed.
pub(crate) fn schedule(eng: &Engine, params: &Value) -> Result<Value, Failure> {
    let env = &eng.env;
    if env.offline_test {
        return Err(fail(OFFLINE, "The nightly backup is not changed while Cadence runs as a test."));
    }
    let Some(on) = params.get("on").and_then(Value::as_bool) else {
        return Err(fail("bad_request", "Say whether the nightly backup is on or off (\"on\": true or false)."));
    };
    let scheduler = env.parts.scheduler.as_ref();
    if on {
        let library = library_of(params)?;
        let at = params.get("at").and_then(Value::as_str).unwrap_or("03:00");
        let data = env.dir.parent().ok_or_else(|| fail("failed", "Cadence's data folder could not be found."))?;
        let exe = std::env::current_exe().map_err(|e| fail("failed", crate::helpers::said("the engine could not find its own program file.", &e)))?;
        api::schedule_on(&setup(env), scheduler, &library, &exe, data, at).map_err(|why| (if why.starts_with("Confirm the main backup first") { "not_confirmed" } else { "failed" }.to_string(), why))?;
    } else {
        scheduler.remove().map_err(|why| fail("failed", why))?;
    }
    let now = scheduler.query().map_err(|why| fail("failed", why))?;
    Ok(json!({ "on": now.as_ref().is_some_and(|n| n.enabled), "words": if on { "Cadence backs up every night, even when it is closed." } else { "The nightly backup is off." } }))
}

// ------------------------------------------------------------------ the nightly backup

/// `cadence-engine.exe --backup-nightly --data "<Cadence's data folder>"`: what the nightly task runs, with
/// no window, whether Cadence is open or not. None when the program was not started for it.
pub fn nightly_if_requested() -> Option<i32> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if !args.iter().any(|a| a == "--backup-nightly") {
        return None;
    }
    let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(PathBuf::from).or_else(|| std::env::var_os("CADENCE_DATA_DIR").filter(|v| !v.is_empty()).map(PathBuf::from));
    let Some(data) = data.filter(|d| d.is_absolute() && d.is_dir()) else {
        eprintln!("cadence-engine: the nightly backup needs Cadence's data folder (--data, a full path)");
        return Some(2);
    };
    let mut env = super::Env::from_process();
    env.dir = data.join("Tools");
    env.parts = super::Parts::real(&env.dir);
    env.parts.cadence_data = Some(data);
    crate::config::set_engine_dir(env.dir.clone());
    crate::secrets::set_owner("Cadence");
    Some(match nightly(&env, Duration::from_secs(15 * 60)) {
        Ok(_) => 0,
        Err((_, why)) => {
            eprintln!("cadence-engine: the nightly backup did not finish: {why}");
            1
        }
    })
}

/// The nightly backup of the main backup. Waits up to `wait` while the engine is changing the library, and
/// does not start if it still is (that night is written in the log as not started). Refused in a test run.
pub(crate) fn nightly(env: &Env, wait: Duration) -> Result<Value, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is copied while Cadence runs as a test."));
    }
    let cfg = load(env)?;
    let library = PathBuf::from(cfg.music_dir.trim());
    if cfg.music_dir.trim().is_empty() || !library.is_dir() {
        return Err(fail("no_music_folder", "The music folder could not be found, so tonight's backup did not start."));
    }
    let until = Instant::now() + wait;
    while changing::busy(&env.dir) {
        if Instant::now() >= until {
            let why = "Cadence was changing your library, so tonight's backup did not start. Nothing was copied.";
            note_in_log(env, why);
            return Err(fail("busy", why));
        }
        std::thread::sleep(Duration::from_secs(1).min(wait));
    }
    backup_with_moves(env, Which::Main, RunKind::Backup, &library, true, &AtomicBool::new(false), &mut |_| {})
}

/// A night that did not start, in the main backup's log the way the worker writes a run that stopped at once,
/// so the page shows that night as a problem rather than as missed.
fn note_in_log(env: &Env, why: &str) {
    use std::io::Write;
    let logs = env.dir.join("backup").join("logs");
    let written = std::fs::create_dir_all(&logs).and_then(|_| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(logs.join(Which::Main.log_name()))?;
        writeln!(f, "==================== {} ====================", chrono::Local::now().format("%Y-%m-%d %H:%M"))?;
        writeln!(f, "{}", crate::backup::api::NIGHTLY_LINE)?;
        writeln!(f, "ERROR: {why}")
    });
    if let Err(e) = written {
        eprintln!("cadence-engine: the backup log could not be written: {e}");
    }
}

// ------------------------------------------------------------------ stand-ins for the tests

#[cfg(test)]
pub(crate) mod fake {
    use crate::backup::api::{Nightly, Scheduler};
    use crate::backup::Disks;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Drives for a test: every folder is on "C:", whose name the test sets, and the test can swap the disk
    /// that holds a folder (its identity then has another volume serial, as a different disk would).
    pub(crate) struct FakeDisks {
        pub(crate) labels: Mutex<HashMap<String, String>>,
        pub(crate) roots: Mutex<Vec<String>>,
        pub(crate) swapped: Mutex<Vec<PathBuf>>,
        pub(crate) removable: Mutex<bool>,
        /// what Windows does when asked to eject: Ok (it let go), or the words it gave
        pub(crate) eject: Mutex<Result<(), String>>,
        pub(crate) ejected: Mutex<Vec<String>>,
    }

    impl FakeDisks {
        pub(crate) fn new() -> FakeDisks {
            FakeDisks {
                labels: Mutex::new(HashMap::new()),
                roots: Mutex::new(vec!["C:\\".into()]),
                swapped: Mutex::new(vec![]),
                removable: Mutex::new(true),
                eject: Mutex::new(Err("Windows did not let go of the drive: something is still using it. Close what has it open and try again, or eject it from the taskbar. Do not pull it out yet.".into())),
                ejected: Mutex::new(vec![]),
            }
        }
    }

    impl Disks for FakeDisks {
        fn label(&self, root: &str) -> Option<String> {
            let k = root.trim_end_matches('\\').to_lowercase();
            let labels = self.labels.lock().unwrap();
            match labels.get(&k) {
                Some(l) if l == "<away>" => None,
                Some(l) => Some(l.clone()),
                None => Some("Test drive".into()),
            }
        }
        fn dir_id(&self, path: &Path) -> Result<String, String> {
            let real = crate::backup::RealDisks.dir_id(path)?;
            let p = path.to_string_lossy().to_lowercase();
            if self.swapped.lock().unwrap().iter().any(|s| p.starts_with(&s.to_string_lossy().to_lowercase())) {
                return Ok(format!("ffffffff{}", &real[8..]));
            }
            Ok(real)
        }
        fn roots(&self) -> Vec<String> {
            self.roots.lock().unwrap().clone()
        }
        fn removable(&self, _root: &str) -> bool {
            *self.removable.lock().unwrap()
        }
        fn usable(&self, _root: &str) -> bool {
            true
        }
        fn space(&self, _path: &str) -> Option<(u64, u64)> {
            Some((500 << 30, 1000 << 30))
        }
        fn eject(&self, folder: &str) -> Result<(), String> {
            self.ejected.lock().unwrap().push(folder.to_string());
            self.eject.lock().unwrap().clone()
        }
    }

    /// A Task Scheduler that only writes down what it was asked; it never reaches Windows.
    #[derive(Default)]
    pub(crate) struct FakeScheduler {
        pub(crate) calls: Mutex<Vec<String>>,
        pub(crate) task: Mutex<Option<Nightly>>,
    }

    impl Scheduler for FakeScheduler {
        fn query(&self) -> Result<Option<Nightly>, String> {
            Ok(self.task.lock().unwrap().clone())
        }
        fn create(&self, exe: &Path, args: &str, at: &str, _work: &Path) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("create {} {args} at {at}", exe.display()));
            *self.task.lock().unwrap() = Some(Nightly { enabled: true, schedule: format!("Daily at {at}"), next: String::new(), running: false, result: None });
            Ok(())
        }
        fn remove(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("remove".into());
            *self.task.lock().unwrap() = None;
            Ok(())
        }
    }
}
