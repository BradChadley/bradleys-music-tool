//! Organize into Artist \ Album (Year), through the engine (ENGINE-PROTOCOL.md, "Organize"). The work is
//! `crate::organize`'s; this is the service around it: the test-run refusal, the library setting (only
//! "organize" lets it run), the guard, the hold, the playing album's wait, `library.moved` and Backup's list
//! for what moved, the report brought up to date, and the release.
use super::jobs::{fail, load, Failure, OFFLINE};
use super::library::{bring_up_to_date, hold, release, renew, waits};
use super::moves::{self, Move};
use super::{Env, Job, Out};
use crate::guard::{self, Guard};
use crate::organize::{self, Preview, PreviewOptions, RunError};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn preview_file(env: &Env) -> PathBuf {
    env.dir.join("organize-preview.json")
}

/// The words when the library setting does not let Organize run.
const OFF_WORDS: &str = "Organizing is off. Turn on \"Repairs and organizing\" in Tools settings, Library.";

/// Refused in a test run, unless the library setting is "organize", and by the guard.
fn allowed(env: &Env, params: &Value) -> Result<Guard, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is organized while Cadence runs as a test."));
    }
    if load(env)?.library_changes() != "organize" {
        return Err(fail("guarded", OFF_WORDS));
    }
    let folder = super::library::folder_of(params, "Organize")?;
    Guard::for_library(&folder)
}

/// The file the player has loaded, as Cadence says (None when it says nothing, or cannot).
fn playing_now(out: &Out, env: &Env, job: &Job) -> Option<PathBuf> {
    out.ask("library.playing", json!({}), env.parts.ask_wait, &|| job.stopped()).ok().and_then(|v| v.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()).map(PathBuf::from))
}

pub(crate) struct PreviewAsk {
    guard: Guard,
    hidden: Vec<PathBuf>,
}

pub(crate) fn preview_check(env: &Env, params: &Value) -> Result<PreviewAsk, Failure> {
    let guard = allowed(env, params)?;
    let hidden = params.get("hidden").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default();
    Ok(PreviewAsk { guard, hidden })
}

/// `organize.preview`: reads tags only and writes `<Tools>\organize-preview.json`.
pub(crate) fn preview(out: &Out, job: &Job, id: u64, env: &Env, ask: PreviewAsk) -> Result<Value, Failure> {
    let library = ask.guard.library.clone();
    let cfg = load(env)?;
    let opts = PreviewOptions { hidden: ask.hidden, skip_folders: cfg.skip_folders, playing: playing_now(out, env, job), compilations: cfg.compilations_folder, only: None };
    let read = organize::app_tag_reader(&library).map_err(|m| fail("missing_helper", m))?;
    let p = organize::preview(&library, &opts, &read, &|done, total| out.progress(id, json!({ "done": done, "total": total, "words": "Reading tags" }))).map_err(|m| fail("failed", m))?;
    if job.stopped() {
        return Err(fail("cancelled", "The preview was stopped."));
    }
    let text = serde_json::to_vec(&p).map_err(|e| fail("failed", format!("The preview could not be kept ({e}).")))?;
    crate::util::write_atomic(&preview_file(env), &text).map_err(|e| fail("failed", crate::helpers::said("the preview could not be kept.", &e)))?;
    Ok(shown(&p))
}

fn shown(p: &Preview) -> Value {
    json!({
        "preview": p.preview, "to_move": p.to_move, "in_place": p.in_place, "left": p.left, "backup_extra_bytes": p.backup_extra_bytes,
        "counts": p.counts,
        "moves": p.moves.iter().map(|m| json!({ "from": m.from, "to": m.to, "files": m.files, "sidecars": m.sidecars, "changes": m.changes })).collect::<Vec<_>>(),
        "left_alone": p.left_alone.iter().map(|l| json!({ "folder": l.folder, "why": l.why, "words": l.words })).collect::<Vec<_>>(),
    })
}

pub(crate) struct RunAsk {
    guard: Guard,
    preview: Preview,
}

/// `organize.run`, before it starts: the preview named must be the one kept, of this music folder.
pub(crate) fn run_check(env: &Env, params: &Value) -> Result<RunAsk, Failure> {
    let guard = allowed(env, params)?;
    let named = params.get("preview").and_then(Value::as_str).unwrap_or_default();
    let kept: Option<Preview> = std::fs::read(preview_file(env)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let preview = kept
        .filter(|p| !named.is_empty() && p.preview == named && guard::covers(&guard.library, &p.library) && guard::covers(&p.library, &guard.library))
        .ok_or_else(|| fail("bad_request", "That preview is not the latest one for this music folder. Preview again."))?;
    Ok(RunAsk { guard, preview })
}

/// `organize.run`: the albums of the preview, moved as one batch of kind "organize". The playing album goes
/// last and waits until it stops playing (checking every couple of seconds) up to `album_wait`, then stays
/// (`playing`); a stop or the folder going away while it waits leaves it where it is. Each album's move is
/// written down for Cadence and Backup as soon as it is done, and Cadence is told in small batches as the
/// run goes (`Told`); the hold is renewed between albums.
pub(crate) fn run(out: &Out, job: &Job, id: u64, env: &Env, ask: RunAsk) -> Result<Value, Failure> {
    let library = ask.guard.library.clone();
    let hold = hold(out, env, job, organize::TITLE, "Organize")?;
    let renewed = Cell::new(Instant::now());
    let told = Told::new(out, env);
    // (a run an earlier engine was cut off in is settled first; `organize::run` does it again, harmlessly)
    told.recovered(&organize::recover_all(&[crate::repair::backups_root(&library)]));
    told.flush();
    let playing = playing_now(out, env, job);
    // the first calls only put the playing album last; once the moves have started, a call is about the
    // album left for last, and waits for it
    let started = Cell::new(false);
    let waited: RefCell<Option<Failure>> = RefCell::new(None);
    let is_playing = |album: &Path| {
        if !started.get() {
            return playing.as_deref().is_some_and(|p| guard::covers(album, p));
        }
        // (what moved so far is told before a wait that may be long)
        told.flush();
        let went = ask.guard.wait_not_playing(&|p| guard::covers(album, p), job, out, id, waits(env));
        renew(out, env, &hold.token, &renewed);
        match went {
            Ok(None) => false,
            // still playing at the limit: it stays (`playing`)
            Ok(Some(_)) => true,
            Err(f) => {
                *waited.borrow_mut() = Some(f);
                true
            }
        }
    };
    let progress = |done: usize, total: usize, words: &str| {
        started.set(true);
        if told.due() {
            told.flush();
        }
        renew(out, env, &hold.token, &renewed);
        out.progress(id, json!({ "done": done, "total": total, "words": words }));
    };
    let cancel = || job.stopped();
    let there = || ask.guard.still_there().is_ok();
    let settled = |key: &str, from: &Path, to: &Path| told.settled(key, from, to);
    let hooks = organize::Hooks { cancel: &cancel, still_there: &there, is_playing: &is_playing, progress: &progress, settled: &settled };
    let ran = organize::run(&library, &ask.preview, &hooks);
    let (done, ended) = match ran {
        Ok(r) => (r, None),
        Err(RunError::Gone { words, done }) => (done, Some(fail("gone", words))),
        Err(RunError::Failed { words, done }) => (done, Some(fail("failed", words))),
    };
    // what moved is told whatever ended the run (it was written down for Cadence and Backup as it moved)
    told.flush();
    let places: Vec<PathBuf> = done.moved.iter().flat_map(|(f, t)| [f.clone(), t.clone()]).collect();
    if library.is_dir() {
        bring_up_to_date(env, &library, &places);
    }
    release(out, env, hold);
    let _ = std::fs::remove_file(preview_file(env));
    if let Some(f) = ended.or_else(|| waited.borrow_mut().take()) {
        return Err(f);
    }
    if done.stopped || job.stopped() {
        return Err(fail("cancelled", format!("Organizing was stopped after {} albums; what moved is one entry in Repair history.", done.moved.len())));
    }
    let left: Vec<Value> = done.left_alone.iter().map(|l| json!({ "folder": l.folder, "why": l.why, "words": l.words })).collect();
    Ok(json!({ "batch": done.batch, "moved": done.moved.len(), "left_alone": left }))
}

/// The moves of an Organize run on their way to Cadence and Backup. Each album's move is written into the
/// unacknowledged list and Backup's list as soon as its rename is done (under its own key,
/// "<batch>#<step>"), so a crash at any point leaves every finished move in both. Cadence is told
/// (`library.moved`, with the batch's id) once `moves_every` albums are waiting or the first of them is
/// `moves_after` old, before a wait for the playing album, and at the end; what it takes comes off the
/// unacknowledged list again.
struct Told<'a> {
    out: &'a Out,
    env: &'a Env,
    waiting: RefCell<Vec<(String, Move)>>,
    since: Cell<Option<Instant>>,
}

impl<'a> Told<'a> {
    fn new(out: &'a Out, env: &'a Env) -> Told<'a> {
        Told { out, env, waiting: RefCell::new(vec![]), since: Cell::new(None) }
    }

    /// One album is at its new place.
    fn settled(&self, key: &str, from: &Path, to: &Path) {
        let m = Move { from: from.to_path_buf(), to: to.to_path_buf(), folder: true };
        moves::keep(&self.env.dir, key, std::slice::from_ref(&m));
        moves::for_backup(&self.env.dir, key, std::slice::from_ref(&m));
        self.waiting.borrow_mut().push((key.to_string(), m));
        if self.since.get().is_none() {
            self.since.set(Some(Instant::now()));
        }
    }

    /// The moves a settling of cut-off runs found done.
    fn recovered(&self, settled: &[organize::Interrupted]) {
        for i in settled {
            for (key, from, to) in &i.newly_done {
                self.settled(key, from, to);
            }
        }
    }

    fn due(&self) -> bool {
        let n = self.waiting.borrow().len();
        n > 0 && (n >= self.env.parts.moves_every.max(1) || self.since.get().is_some_and(|t| t.elapsed() >= self.env.parts.moves_after))
    }

    /// Tells Cadence what is waiting, one `library.moved` per batch id.
    fn flush(&self) {
        let waiting = std::mem::take(&mut *self.waiting.borrow_mut());
        self.since.set(None);
        let mut batches: Vec<String> = vec![];
        for (key, _) in &waiting {
            let b = key.split('#').next().unwrap_or(key).to_string();
            if !batches.contains(&b) {
                batches.push(b);
            }
        }
        for b in batches {
            let mine: Vec<&(String, Move)> = waiting.iter().filter(|(k, _)| k.split('#').next() == Some(b.as_str())).collect();
            let list: Vec<Move> = mine.iter().map(|(_, m)| m.clone()).collect();
            if moves::ask_moved(self.out, &b, &list, self.env.parts.ask_wait) {
                let keys: Vec<String> = mine.iter().map(|(k, _)| k.clone()).collect();
                moves::forget(&self.env.dir, &keys);
            }
        }
    }
}

/// `organize.status`: an organize run that was cut off and settled, until Cadence says it has seen it
/// (`"seen": "<batch>"`). Saying it has seen one writes (in the library's undo backups), so that is refused
/// in a test run; only asking is not.
pub(crate) fn status(env: &Env, params: &Value) -> Result<Value, Failure> {
    let folder = super::library::folder_of(params, "Organize")?;
    let root = crate::repair::backups_root(&folder);
    let seen = params.get("seen").and_then(Value::as_str).unwrap_or_default();
    if !seen.is_empty() {
        if env.offline_test {
            return Err(fail(OFFLINE, "Nothing is written while Cadence runs as a test."));
        }
        organize::acknowledge(&root, seen);
    }
    Ok(json!({ "interrupted": organize::status(&root) }))
}

/// At the engine's start, before any library write: organize runs a crash cut off are settled. Not in a
/// test run (it writes), and not on a network drive (the guard's rule).
pub(crate) fn recover_at_start(env: &Env) {
    if env.offline_test {
        return;
    }
    let Ok(cfg) = load(env) else { return };
    let music = PathBuf::from(cfg.music_dir.trim());
    if cfg.music_dir.trim().is_empty() || guard::place(&music) != guard::Place::Local && guard::place(&music) != guard::Place::Removable {
        return;
    }
    for i in organize::recover_all(&[crate::repair::backups_root(&music)]) {
        eprintln!("cadence-engine: an organize run that was cut off was settled: {} (moved {}, left {}, still at a temporary name {})", i.batch, i.moved, i.left, i.stuck.len());
        // (Cadence is not asked here: the moves wait in the unacknowledged list, which it reads at its start)
        for (key, from, to) in &i.newly_done {
            let m = [Move { from: from.clone(), to: to.clone(), folder: true }];
            moves::keep(&env.dir, key, &m);
            moves::for_backup(&env.dir, key, &m);
        }
    }
}

// ------------------------------------------------------------------ Keep the library organized: one album

/// The words when Keep the library organized is off.
const KEEP_OFF_WORDS: &str = "Keep the library organized is off. Turn it on in Tools settings, Library.";

pub(crate) struct AlbumAsk {
    guard: Guard,
    album: PathBuf,
}

/// `organize.album`, before it starts: as Organize (a test run, the library setting, the guard), and only
/// with `keep_organized` on. `folder` is the album's folder, in the music folder (`library`, else the one in
/// Settings).
pub(crate) fn album_check(env: &Env, params: &Value) -> Result<AlbumAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is organized while Cadence runs as a test."));
    }
    let cfg = load(env)?;
    if cfg.library_changes() != "organize" {
        return Err(fail("guarded", OFF_WORDS));
    }
    if !cfg.keep_organized {
        return Err(fail("guarded", KEEP_OFF_WORDS));
    }
    let album = super::library::folder_of(params, "Filing a new album")?;
    let library = match params.get("library").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        Some(l) => super::library::folder_of(&json!({ "folder": l }), "Filing a new album")?,
        None => PathBuf::from(cfg.music_dir.trim()),
    };
    if library.as_os_str().is_empty() || !guard::covers(&library, &album) || guard::covers(&album, &library) {
        return Err(fail("bad_request", "That folder is not an album in the music folder."));
    }
    Ok(AlbumAsk { guard: Guard::for_library(&library)?, album })
}

/// `organize.album`: files one album that just arrived by Artist \ Album (Year), as one entry in Repair
/// history (its own batch, kind "organize"), the way a run does: the guard, the hold, the journal,
/// `library.moved` and Backup's list. An album the player has loaded waits until it stops (checking every
/// couple of seconds) up to a limit, then stays with the reason `playing`. An album with a reason to stay
/// stays and says it; one already in place answers `{"moved": false}`.
pub(crate) fn album(out: &Out, job: &Job, id: u64, env: &Env, ask: AlbumAsk) -> Result<Value, Failure> {
    let library = ask.guard.library.clone();
    let cfg = load(env)?;
    let hold = hold(out, env, job, organize::TITLE, "Organize")?;
    let held_at = Cell::new(Instant::now());
    let told = Told::new(out, env);
    let went = (|| -> Result<(Value, Vec<PathBuf>), Failure> {
        ask.guard.still_there()?;
        told.recovered(&organize::recover_all(&[crate::repair::backups_root(&library)]));
        told.flush();
        let opts = PreviewOptions { hidden: vec![], skip_folders: cfg.skip_folders.clone(), playing: playing_now(out, env, job), compilations: cfg.compilations_folder.clone(), only: Some(ask.album.clone()) };
        let read = organize::app_tag_reader(&library).map_err(|m| fail("missing_helper", m))?;
        let p = organize::preview(&library, &opts, &read, &|_, _| {}).map_err(|m| fail("failed", m))?;
        if let Some(l) = p.left_alone.first() {
            return Ok((json!({ "moved": false, "why": l.why, "words": l.words }), vec![]));
        }
        let Some(m) = p.moves.first() else {
            if p.in_place == 0 {
                return Err(fail("bad_request", "That folder is not an album in the music folder."));
            }
            return Ok((json!({ "moved": false }), vec![]));
        };
        // never while it plays: wait, then leave it
        let still = wait_until_not_playing(out, env, job, id, &ask.guard, &m.from);
        renew(out, env, &hold.token, &held_at);
        if !still? {
            let l = json!({ "moved": false, "why": "playing", "words": "It was still playing." });
            return Ok((l, vec![]));
        }
        out.progress(id, json!({ "done": 0, "total": 1, "words": m.words }));
        let cancel = || job.stopped();
        let there = || ask.guard.still_there().is_ok();
        let settled = |key: &str, from: &Path, to: &Path| told.settled(key, from, to);
        let hooks = organize::Hooks { cancel: &cancel, still_there: &there, is_playing: &|_| false, progress: &|_, _, _| {}, settled: &settled };
        let (done, ended) = match organize::run(&library, &p, &hooks) {
            Ok(r) => (r, None),
            Err(RunError::Gone { words, done }) => (done, Some(fail("gone", words))),
            Err(RunError::Failed { words, done }) => (done, Some(fail("failed", words))),
        };
        told.flush();
        let places: Vec<PathBuf> = done.moved.iter().flat_map(|(f, t)| [f.clone(), t.clone()]).collect();
        if let Some(f) = ended {
            return Err(f);
        }
        let answer = match (done.moved.first(), done.left_alone.first()) {
            (Some((from, to)), _) => json!({ "moved": true, "batch": done.batch, "from": from, "to": to }),
            (None, Some(l)) => json!({ "moved": false, "why": l.why, "words": l.words }),
            (None, None) => json!({ "moved": false }),
        };
        Ok((answer, places))
    })();
    if let Ok((_, places)) = &went {
        if !places.is_empty() && library.is_dir() {
            bring_up_to_date(env, &library, places);
        }
    }
    release(out, env, hold);
    went.map(|w| w.0)
}

/// Waits while the player has a file of `album` loaded, asking every `playing_every`, for at most
/// `album_wait`. True when it is not playing (any more); false when it still was at the limit.
fn wait_until_not_playing(out: &Out, env: &Env, job: &Job, id: u64, guard: &Guard, album: &Path) -> Result<bool, Failure> {
    let until = std::time::Instant::now() + env.parts.album_wait;
    let mut said = false;
    loop {
        guard.still_there()?;
        if job.stopped() {
            return Err(fail("cancelled", "It was stopped; nothing was moved."));
        }
        let Some(p) = playing_now(out, env, job).filter(|p| guard::covers(album, p)) else { return Ok(true) };
        if std::time::Instant::now() >= until {
            return Ok(false);
        }
        if !said {
            let song = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            out.progress(id, json!({ "words": format!("Waiting for {song} to finish playing"), "waiting": true }));
            said = true;
        }
        let next = std::time::Instant::now() + env.parts.playing_every;
        while std::time::Instant::now() < next {
            if job.stopped() {
                return Err(fail("cancelled", "It was stopped while it waited for a song to finish playing; nothing was moved."));
            }
            std::thread::sleep(std::time::Duration::from_millis(50).min(env.parts.playing_every));
        }
    }
}
