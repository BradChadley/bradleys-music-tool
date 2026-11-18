//! Health, repairs, Repair history and Edit details, through the engine (protocol 3; ENGINE-PROTOCOL.md,
//! "Protocol 3"). The work is the Music Tool's own: the check is `healthscan`, the report is read the way the
//! Health page reads it (`health::engine_report`), every change goes through `repair` and its undo journal
//! (`undo`), tags through the Rust tag editor (`tagwrite`, by way of `repair`'s `Step::Tags`).
//!
//! What every change to the music folder goes through, in this order:
//!   1. refused at once in a test run (`offline_test`), when `library_changes` is "look" (repairs and their
//!      Undo only; Edit details is asked for by hand and still works), and by the guard (`crate::guard`:
//!      a network drive, a folder that is not there);
//!   2. `library.hold`, and nothing changes without it;
//!   3. for each finding: the folder still there (`gone` otherwise), the playing-file wait, read-only and
//!      open files left out and listed, then the change through the journal;
//!   4. `library.moved` for whatever moved (kept for Cadence's next start if it cannot be told), and the
//!      moves written down for Backup;
//!   5. the report brought up to date for the top folders that changed (a check with `only`);
//!   6. `library.release`, whatever happened.
use super::jobs::{fail, load, Failure, OFFLINE};
use super::moves;
use super::{Env, Job, Out};
use crate::guard::{self, Guard, Waits};
use crate::healthscan;
use crate::repair::{self, Ask, Plan, Step, TagStep};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn text(params: &Value, key: &str) -> String {
    params.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string()
}

/// The music folder a request names. `bad_request` without one.
pub(crate) fn folder_of(params: &Value, what: &str) -> Result<PathBuf, Failure> {
    let f = text(params, "folder");
    if f.is_empty() {
        return Err(fail("bad_request", format!("{what} needs the music folder (\"folder\").")));
    }
    // (without a separator at the end, except for the top of a drive: "X:\")
    let t = f.trim_end_matches(['\\', '/']);
    Ok(PathBuf::from(if t.ends_with(':') { format!("{t}\\") } else { t.to_string() }))
}

/// Where the engine keeps a library's report: in its own folder, named as the Music Tool names it.
pub(crate) fn report_path(env: &Env, folder: &Path) -> PathBuf {
    env.dir.join(healthscan::report_file(&folder.to_string_lossy()).file_name().unwrap_or_default())
}

fn ignore_path(env: &Env) -> PathBuf {
    env.dir.join("ignored-issues.txt")
}

pub(crate) fn waits(env: &Env) -> Waits {
    Waits { ask: env.parts.ask_wait, every: env.parts.playing_every, limit: env.parts.album_wait }
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// ffmpeg and ffprobe, which every check and repair needs.
pub(crate) fn helpers() -> Result<(PathBuf, PathBuf), Failure> {
    let ffmpeg = crate::importer::tools::find_tool("ffmpeg").map_err(|m| fail("missing_helper", m))?;
    let ffprobe = crate::importer::tools::find_tool("ffprobe").map_err(|m| fail("missing_helper", m))?;
    Ok((ffmpeg, ffprobe))
}

/// What a repair needs besides the finding, with the engine's own caches.
fn tools(env: &Env, cfg: &crate::config::Config) -> Result<repair::Tools, Failure> {
    let (ffmpeg, ffprobe) = helpers()?;
    let mut lyrics = crate::importer::lyrics::Lookup::for_app(env.parts.online, !cfg.lyrics_plain);
    lyrics.dir = env.dir.join("cache").join("lyrics");
    Ok(repair::Tools { ffmpeg, ffprobe, quality_cache: Some(env.dir.join("cache").join("quality-cache.txt")), online: env.parts.online, lyrics: Some(lyrics) })
}

// ------------------------------------------------------------------ the check

/// One check of a library (all of it, or `only` some top folders), on the calling thread, with progress
/// through `shown` and a stop through `stopped`. Returns the report it wrote; Err `cancelled` when it was
/// stopped before the end.
fn scan(env: &Env, folder: &Path, skip_folders: &str, deep: bool, only: Vec<PathBuf>, stopped: &dyn Fn() -> bool, shown: &mut dyn FnMut(usize, usize, usize)) -> Result<Value, Failure> {
    let (ffmpeg, ffprobe) = helpers()?;
    let name = folder.to_string_lossy().to_string();
    let named = |p: PathBuf| env.dir.join(p.file_name().unwrap_or_default());
    let cache = env.dir.join("cache");
    let o = healthscan::Options {
        library: folder.to_path_buf(),
        deep,
        skip_folders: skip_folders.to_string(),
        alias_file: String::new(),
        report: report_path(env, folder),
        quality_cache: cache.join("quality-cache.txt"),
        seed_tags: None,
        tracklists: named(crate::importer::recheck::overlay_file(&name)),
        lookups: cache.join("tracklists"),
        lyrics_dir: cache.join("lyrics"),
        online: false,
        only,
        cache_dir: cache,
    };
    let p = Arc::new(healthscan::Progress::default());
    let worker = {
        let p = p.clone();
        std::thread::spawn(move || healthscan::run(&o, &p, &ffmpeg, &ffprobe))
    };
    let mut last = None;
    while !worker.is_finished() {
        if stopped() {
            p.cancel.store(true, Ordering::Relaxed);
        }
        let now = (p.phase.load(Ordering::Relaxed) as usize, p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
        if last != Some(now) {
            last = Some(now);
            shown(now.0, now.1, now.2);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let ended = worker.join().map_err(|_| fail("failed", "The check hit a fault and stopped."))?;
    let found = ended.map_err(|m| fail("failed", m))?;
    found.filter(|r| !r.get("Cancelled").and_then(Value::as_bool).unwrap_or(false)).ok_or_else(|| fail("cancelled", "The check was stopped."))
}

pub(crate) struct CheckAsk {
    folder: PathBuf,
    skip_folders: String,
    deep: bool,
    only: Vec<PathBuf>,
}

/// `health.check` (and `health.quick`, its older name): the folder, which top folders, and how deep.
pub(crate) fn check_check(env: &Env, params: &Value) -> Result<CheckAsk, Failure> {
    let folder = text(params, "folder");
    if folder.is_empty() {
        return Err(fail("bad_request", "A check needs the folder to check."));
    }
    let folder = PathBuf::from(folder);
    if !folder.is_dir() {
        return Err(fail("not_found", format!("The folder {} could not be found.", folder.display())));
    }
    let mut only = vec![];
    for o in params.get("only").and_then(Value::as_array).into_iter().flatten() {
        let p = PathBuf::from(o.as_str().map(str::trim).unwrap_or_default());
        if p.parent().is_none_or(|parent| !parent.as_os_str().eq_ignore_ascii_case(folder.as_os_str().to_string_lossy().trim_end_matches(['\\', '/']))) {
            return Err(fail("bad_request", format!("{} is not a folder at the top of {}.", p.display(), folder.display())));
        }
        only.push(p);
    }
    // (without "skip_folders", the engine's own setting: the same list the checks after a repair use)
    let skip_folders = match params.get("skip_folders").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => load(env).map(|c| c.skip_folders).unwrap_or_default(),
    };
    Ok(CheckAsk { folder, skip_folders, deep: params.get("deep").and_then(Value::as_bool).unwrap_or(false), only })
}

/// The check (tags and the rules; `deep` also decodes every track not decoded before) of a folder laid out
/// as artist folders holding album folders, or of some of its top folders (`only`, folded into the report).
/// It only reads the folder; its report and caches are in the engine's own folder.
pub(crate) fn check(out: &Out, job: &Job, id: u64, env: &Env, ask: CheckAsk) -> Result<Value, Failure> {
    let started = Instant::now();
    let only = ask.only.len();
    let r = scan(env, &ask.folder, &ask.skip_folders, ask.deep, ask.only, &|| job.stopped(), &mut |phase, done, total| {
        let step = healthscan::PHASES.get(phase).copied().unwrap_or_default();
        out.progress(id, json!({ "phase": phase, "step": step, "done": done, "total": total }));
    })?;
    let n = |k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0);
    Ok(json!({
        "folder": ask.folder,
        "files": n("Files"),
        "albums": n("Albums"),
        "artists": n("Artists"),
        "unchecked": n("Unchecked"),
        "issues": r.get("Issues").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        "report": report_path(env, &ask.folder),
        "seconds": started.elapsed().as_secs_f64(),
        "deep": ask.deep,
        "only": only,
    }))
}

/// The top folders of the library that hold these places, each once.
fn tops_of(library: &Path, places: &[PathBuf]) -> Vec<PathBuf> {
    let mut tops: Vec<PathBuf> = vec![];
    for p in places {
        let Some(top) = p.strip_prefix(library).ok().and_then(|rel| rel.components().next()).map(|c| library.join(c.as_os_str())) else { continue };
        if top.is_dir() && !tops.iter().any(|t| t.as_os_str().eq_ignore_ascii_case(top.as_os_str())) {
            tops.push(top);
        }
    }
    tops
}

/// Brings the report up to date for the top folders a change touched (a check with `only`). Nothing to do
/// without a report yet; a check that does not work is said in the log and the change still stands.
pub(crate) fn bring_up_to_date(env: &Env, library: &Path, places: &[PathBuf]) {
    let tops = tops_of(library, places);
    if tops.is_empty() || !report_path(env, library).is_file() {
        return;
    }
    let skip = load(env).map(|c| c.skip_folders).unwrap_or_default();
    if let Err((_, why)) = scan(env, library, &skip, false, tops, &|| false, &mut |_, _, _| {}) {
        eprintln!("cadence-engine: the report could not be brought up to date after a change: {why}");
    }
}

// ------------------------------------------------------------------ the report, ignoring, replacing

/// `health.report`: the report in Cadence's shape, with the library setting.
pub(crate) fn report(env: &Env, params: &Value) -> Result<Value, Failure> {
    let folder = folder_of(params, "The report")?;
    let mut v = crate::health::engine_report(&report_path(env, &folder), &ignore_path(env)).map_err(|m| fail("failed", m))?;
    v["library_changes"] = json!(load(env)?.library_changes());
    Ok(v)
}

/// `health.ignore`: issues onto the ignore list, or off it.
pub(crate) fn ignore(env: &Env, params: &Value) -> Result<Value, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is changed while Cadence runs as a test."));
    }
    let keys: Vec<&str> = params.get("keys").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(on) = params.get("ignored").and_then(Value::as_bool) else {
        return Err(fail("bad_request", "Say whether to ignore the issues or bring them back (\"ignored\": true or false)."));
    };
    if keys.is_empty() || keys.iter().any(|k| !k.contains('|') || k.contains(['\r', '\n'])) {
        return Err(fail("bad_request", "Name the issues by their keys, as the report gives them."));
    }
    for k in &keys {
        crate::health::write_ignore(&ignore_path(env), k, on).map_err(|e| fail("failed", crate::helpers::said("the list of ignored issues could not be saved.", &e)))?;
    }
    Ok(json!({ "changed": keys.len() }))
}

/// `replace.want`: the next download of this album replaces it once it passes its own check.
pub(crate) fn replace_want(env: &Env, params: &Value) -> Result<Value, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is changed while Cadence runs as a test."));
    }
    let folder = folder_of(params, "Replacing an album")?;
    let files: Vec<String> = params.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    crate::replace::add(&env.dir.join("replacements.json"), &folder, &text(params, "artist"), &text(params, "album"), &text(params, "why"), &files).map_err(|m| fail("failed", m))?;
    Ok(json!({}))
}

// ------------------------------------------------------------------ repairs

/// The findings the keys name, read from the report as it is now, as repairs are asked for them.
fn asks_for(env: &Env, folder: &Path, keys: &[String], choice: &str) -> Result<Vec<Ask>, Failure> {
    let bytes = std::fs::read(report_path(env, folder)).map_err(|_| fail("bad_request", "There is no report for this music folder yet. Run a check first."))?;
    let root: Value = serde_json::from_slice(&bytes).map_err(|_| fail("failed", "The report of the last check could not be read. Run a check again."))?;
    let s = |v: &Value, k: &str| match &v[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    };
    let list: Vec<&Value> = match root.get("Issues") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v @ Value::Object(_)) => vec![v],
        _ => vec![],
    };
    let mut asks = vec![];
    for key in keys {
        let found = list.iter().find(|i| {
            let cat = if s(i, "C").is_empty() { "Other".to_string() } else { s(i, "C") };
            format!("{cat}|{}", s(i, "P").to_lowercase()) == *key
        });
        let Some(i) = found else {
            return Err(fail("bad_request", "That issue is no longer in the report. Run a check again."));
        };
        let files = match &i["F"] {
            Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            Value::String(f) if !f.is_empty() => vec![f.clone()],
            _ => vec![],
        };
        let data: HashMap<String, String> = i["X"].as_object().map(|x| x.keys().map(|k| (k.clone(), s(&i["X"], k))).collect()).unwrap_or_default();
        asks.push(Ask { fix: s(i, "K"), cat: s(i, "C"), path: s(i, "P"), files, artist: s(i, "A"), album: s(i, "L"), data, choice: choice.to_string() });
    }
    Ok(asks)
}

fn keys_of(params: &Value) -> Result<Vec<String>, Failure> {
    let keys: Vec<String> = params.get("keys").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    if keys.is_empty() {
        return Err(fail("bad_request", "Name the issues to repair by their keys, as the report gives them."));
    }
    Ok(keys)
}

/// One repair, or a Fix all of one kind: the kinds the engine can repair, and only those.
fn repairable(asks: &[Ask]) -> Result<(), Failure> {
    let kind = asks[0].fix.as_str();
    if asks.len() > 1 {
        if asks.iter().any(|a| a.fix != kind) || !repair::can_fix_all(kind) {
            return Err(fail("bad_request", "Fix all works on issues of one kind, and only on the kinds it can fix all at once."));
        }
    } else if !repair::can_fix(kind) {
        return Err(fail("bad_request", "Cadence has no repair for this issue."));
    }
    Ok(())
}

/// Takes out of a plan the steps the guard leaves alone (read-only, open in another program), each said.
fn vet(guard: &Guard, plan: &mut Plan) {
    let mut kept = vec![];
    for step in std::mem::take(&mut plan.steps) {
        let target = step.target().to_path_buf();
        let checked = match &step {
            Step::Create { .. } => Ok(()),
            Step::Move { .. } | Step::Remove { .. } => guard.folder_ok(&target).map_err(|(f, r)| if f == target { r.words().to_string() } else { format!("{} inside it: {}", leaf(&f), r.words()) }),
            _ => guard.file_ok(&target).map_err(|r| r.words().to_string()),
        };
        match checked {
            Ok(()) => kept.push(step),
            Err(why) => plan.problem_with(&target, why),
        }
    }
    plan.steps = kept;
}

/// Whether the playing file is one of these steps' (a file a step changes, or one inside a folder it moves).
fn touches(steps: &[Step], playing: &Path) -> bool {
    steps.iter().any(|s| guard::covers(s.target(), playing))
}

/// `repair.plan`: what a repair would do, in plain words, and what stands in the way. Changes nothing.
pub(crate) fn plan(env: &Env, params: &Value) -> Result<Value, Failure> {
    let folder = folder_of(params, "A repair")?;
    let asks = asks_for(env, &folder, &keys_of(params)?, &text(params, "choice"))?;
    repairable(&asks)?;
    let tools = tools(env, &load(env)?)?;
    let guard = Guard::for_library(&folder);
    let (mut steps, mut problems) = (vec![], vec![]);
    if let Err((_, why)) = &guard {
        problems.push(why.clone());
    }
    for a in &asks {
        let mut p = repair::plan(a, &folder, &tools);
        if let Ok(g) = &guard {
            vet(g, &mut p);
        }
        steps.extend(p.steps.iter().map(Step::words));
        problems.extend(p.problems);
    }
    Ok(json!({ "steps": steps, "problems": problems }))
}

/// Refused when the library setting is "look".
fn not_look_only(env: &Env) -> Result<(), Failure> {
    if load(env)?.library_changes() == "look" {
        return Err(fail("guarded", "Look only is on, so repairs are off. Change it in Tools settings, Library."));
    }
    Ok(())
}

pub(crate) struct RepairAsk {
    guard: Guard,
    asks: Vec<Ask>,
    title: String,
}

/// `repair.apply`, before it starts: a test run, the library setting, the guard, and the findings.
pub(crate) fn apply_check(env: &Env, params: &Value) -> Result<RepairAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is repaired while Cadence runs as a test."));
    }
    not_look_only(env)?;
    let folder = folder_of(params, "A repair")?;
    let guard = Guard::for_library(&folder)?;
    let asks = asks_for(env, &folder, &keys_of(params)?, &text(params, "choice"))?;
    repairable(&asks)?;
    let title = match text(params, "title") {
        t if !t.is_empty() => t,
        _ if asks.len() > 1 => format!("Fix all: {} ({} issues)", asks[0].cat, asks.len()),
        _ => format!("{}: {} {}", asks[0].cat, asks[0].artist, asks[0].album).trim().to_string(),
    };
    Ok(RepairAsk { guard, asks, title })
}

/// The words when another change to the library is still running in the engine.
pub(crate) const BUSY_WORDS: &str = "Another change to your library is still finishing. Try again in a moment.";

/// The engines whose library has a writer right now, by the engine's folder (one engine has one folder; the
/// tests run many engines in one process, each with its own).
static WRITERS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(vec![]);

/// One writer to the library at a time in the engine: held from before `library.hold` is asked until after
/// `library.release`, and let go when dropped (a fault in the job too). Cadence's own claim on the library
/// can end sooner (it stops waiting for a stopped job after a few seconds), so this, not Cadence's claim, is
/// what keeps a second change (an Undo of the batch still being written, say) from starting.
pub(crate) struct Writer(PathBuf);

impl Writer {
    /// `busy` at once when another change is running.
    pub(crate) fn take(env: &Env) -> Result<Writer, Failure> {
        let mut w = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        if w.contains(&env.dir) {
            return Err(fail("busy", BUSY_WORDS));
        }
        w.push(env.dir.clone());
        Ok(Writer(env.dir.clone()))
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        WRITERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|d| *d != self.0);
    }
}

/// Cadence's hold, and the engine's one writer slot with it.
pub(crate) struct Held {
    pub(crate) token: Value,
    _writer: Writer,
}

/// Asks Cadence to hold its library watcher, after taking the engine's one writer slot (`busy` at once when
/// another change is still running). `failed` without a hold: nothing is changed then.
pub(crate) fn hold(out: &Out, env: &Env, job: &Job, reason: &str, what: &str) -> Result<Held, Failure> {
    let writer = Writer::take(env)?;
    match out.ask("library.hold", json!({ "reason": reason }), env.parts.ask_wait, &|| job.stopped()) {
        Ok(v) => {
            // (the nightly backup, another process, sees the library is being changed and waits)
            super::backup::changing::enter(&env.dir);
            Ok(Held { token: v.get("hold").cloned().unwrap_or(Value::Null), _writer: writer })
        }
        Err(_) if job.stopped() => Err(fail("cancelled", "It was stopped before anything was changed.")),
        Err((_, why)) => {
            eprintln!("cadence-engine: no hold for {what}: {why}");
            Err(fail("failed", format!("Cadence did not let the {what} start; nothing was changed.")))
        }
    }
}

/// Says again that the engine still holds the library (`library.renew {hold}`), when `renew_every` has
/// passed since the hold was taken or last renewed (`last`). Cadence drops a hold that is older than 30
/// minutes without a renewal; a job that runs long calls this between its steps. A Cadence that cannot
/// renew (an error) is written in the engine's log, and the job goes on.
pub(crate) fn renew(out: &Out, env: &Env, hold: &Value, last: &std::cell::Cell<Instant>) {
    if last.get().elapsed() < env.parts.renew_every {
        return;
    }
    last.set(Instant::now());
    if let Err((_, why)) = out.ask("library.renew", json!({ "hold": hold }), env.parts.ask_wait, &|| false) {
        eprintln!("cadence-engine: the hold on the library could not be renewed ({why})");
    }
}

pub(crate) fn release(out: &Out, env: &Env, hold: Held) {
    super::backup::changing::leave(&env.dir);
    let _ = out.ask("library.release", json!({ "hold": hold.token }), env.parts.ask_wait, &|| false);
    drop(hold); // (the writer slot last)
}

/// The moves of a record with a hop through a temporary name (a case-only rename: A to A.tmp, A.tmp to a)
/// joined into one move each, so Cadence hears where a thing went, not where it passed through.
fn chained(pairs: Vec<(PathBuf, PathBuf)>) -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = vec![];
    for (from, to) in pairs {
        match out.iter_mut().find(|(_, t)| t.as_os_str().eq_ignore_ascii_case(from.as_os_str())) {
            Some(m) => m.1 = to,
            None => out.push((from, to)),
        }
    }
    out
}

/// After the changes, with the library still held: Cadence and Backup hear of the moves, and the report is
/// brought up to date for what was touched.
fn after(out: &Out, env: &Env, library: &Path, batch: Option<&str>, extra: &[PathBuf]) {
    let root = repair::backups_root(library);
    let mut places: Vec<PathBuf> = extra.to_vec();
    if let Some(id) = batch {
        let moved = moves::from_pairs(&chained(crate::undo::moves(&root, id)));
        moves::for_backup(&env.dir, id, &moved);
        moves::tell(out, &env.dir, id, &moved, env.parts.ask_wait);
        places.extend(crate::undo::touched(&root, id));
    }
    if library.is_dir() {
        bring_up_to_date(env, library, &places);
    }
}

/// Runs plans through one batch under the guard: before each, the folder must still be there, the playing
/// file is waited for, and read-only and open files are left out. Returns what was done, and why it ended
/// early (a stop, or the folder gone).
#[allow(clippy::too_many_arguments)]
fn run_plans(out: &Out, job: &Job, id: u64, env: &Env, guard: &Guard, hold: &Held, tools: &repair::Tools, title: &str, kind: &str, count: usize, words: &dyn Fn(usize) -> String, mut make: impl FnMut(usize) -> Plan) -> Result<(repair::Done, Option<Failure>), Failure> {
    let library = guard.library.clone();
    let renewed = std::cell::Cell::new(Instant::now());
    let mut ended: Option<Failure> = None;
    let stop = || job.stopped();
    let done = repair::apply_each_until(
        count,
        title,
        kind,
        &library,
        &repair::backups_root(&library),
        tools,
        |n| {
            let went = (|| {
                if job.stopped() {
                    return Err(fail("cancelled", "It was stopped; what was done is one entry in Repair history."));
                }
                guard.still_there()?;
                renew(out, env, &hold.token, &renewed);
                out.progress(id, json!({ "done": n, "total": count, "words": words(n) }));
                let mut plan = make(n);
                let waited = std::cell::Cell::new(false);
                let still = guard.wait_not_playing(
                    &|p| {
                        let hit = touches(&plan.steps, p);
                        waited.set(waited.get() | hit);
                        hit
                    },
                    job,
                    out,
                    id,
                    waits(env),
                )?;
                renew(out, env, &hold.token, &renewed);
                if waited.get() {
                    // (planned again: the song that finished may have been changed while it played)
                    plan = make(n);
                }
                if let Some(p) = still {
                    // still playing at the end of the wait: what touches it is left out, and said
                    let (out_steps, kept): (Vec<Step>, Vec<Step>) = std::mem::take(&mut plan.steps).into_iter().partition(|s| guard::covers(s.target(), &p));
                    plan.steps = kept;
                    for s in out_steps {
                        plan.problem_with(s.target(), guard::STILL_PLAYING.to_string());
                    }
                }
                vet(guard, &mut plan);
                Ok(plan)
            })();
            match went {
                Ok(plan) => Some(plan),
                Err(f) => {
                    ended = Some(f);
                    None
                }
            }
        },
        |_, _| true,
        &stop,
    )
    .map_err(|m| fail("failed", m))?;
    if ended.is_none() {
        out.progress(id, json!({ "done": count, "total": count, "words": "" }));
    }
    Ok((done, ended))
}

/// `repair.apply`: one repair, or a Fix all as one batch (one Undo).
pub(crate) fn apply(out: &Out, job: &Job, id: u64, env: &Env, ask: RepairAsk) -> Result<Value, Failure> {
    let tools = tools(env, &load(env)?)?;
    let hold = hold(out, env, job, &ask.title, "repair")?;
    let asks = &ask.asks;
    let ran = run_plans(out, job, id, env, &ask.guard, &hold, &tools, &ask.title, "repair", asks.len(), &|n| format!("{} {}", asks[n].artist, asks[n].album).trim().to_string(), |n| repair::plan(&asks[n], &ask.guard.library, &tools));
    if let Ok((done, _)) = &ran {
        let paths: Vec<PathBuf> = asks.iter().map(|a| PathBuf::from(&a.path)).collect();
        after(out, env, &ask.guard.library, done.batch.as_deref(), &paths);
    }
    release(out, env, hold);
    let (done, ended) = ran?;
    if let Some(f) = ended {
        return Err(f);
    }
    if job.stopped() {
        return Err(fail("cancelled", "It was stopped; what was done is one entry in Repair history, and Undo puts it back."));
    }
    Ok(json!({ "batch": done.batch, "changed": done.changed, "problems": done.problems }))
}

// ------------------------------------------------------------------ Repair history

fn same_folder(a: &str, b: &Path) -> bool {
    a.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']))
}

/// The album folders a batch touched (a file's folder, or the folder itself), each once, at most 20.
fn folders_of(places: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    for p in places {
        let f = if crate::covers::ext_in(p, &crate::covers::AUDIO) || p.is_file() { p.parent().map(Path::to_path_buf).unwrap_or_default() } else { p.clone() };
        if !f.as_os_str().is_empty() && !out.iter().any(|o| o.as_os_str().eq_ignore_ascii_case(f.as_os_str())) {
            out.push(f);
        }
        if out.len() >= 20 {
            break;
        }
    }
    out
}

fn size_of(dir: &Path) -> u64 {
    crate::importer::util::walk_files(dir).iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum()
}

/// The batches of this library (a record that does not name its library is listed too).
fn batches_of(folder: &Path) -> (PathBuf, Vec<crate::undo::BatchInfo>) {
    let root = repair::backups_root(folder);
    let list = crate::undo::batches(&root).into_iter().filter(|b| b.library.is_empty() || same_folder(&b.library, folder)).collect();
    (root, list)
}

/// `history.list`: every repair, edit, import and organize of this library, newest first, and what
/// `history.clear_old` would clear.
pub(crate) fn history_list(params: &Value) -> Result<Value, Failure> {
    let folder = folder_of(params, "Repair history")?;
    let (root, list) = batches_of(&folder);
    let now = chrono::Local::now();
    let old: Vec<&crate::undo::BatchInfo> = list.iter().filter(|b| b.when.is_some_and(|w| now.signed_duration_since(w) > chrono::Duration::days(crate::undo::CLEAR_AFTER_DAYS))).collect();
    let bytes: u64 = old.iter().map(|b| size_of(&root.join(&b.id))).sum();
    let repairs: Vec<Value> = list
        .iter()
        .map(|b| json!({ "id": b.id, "title": b.title, "when": b.when.map(|w| w.to_rfc3339()), "changes": b.changes, "undone": b.undone, "kind": b.kind, "folders": folders_of(&b.places) }))
        .collect();
    Ok(json!({ "repairs": repairs, "old": { "count": old.len(), "bytes": bytes } }))
}

pub(crate) struct UndoAsk {
    guard: Guard,
    id: String,
    info: crate::undo::BatchInfo,
}

/// `history.undo`, before it starts. Look only refuses the Undo of a repair or an Organize as it refuses
/// them; an Edit details, an import or a bring-back from Backup, which a person made by hand, can still be
/// undone.
pub(crate) fn undo_check(env: &Env, params: &Value) -> Result<UndoAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is undone while Cadence runs as a test."));
    }
    let folder = folder_of(params, "Undo")?;
    // (the guard first: Repair history is not even read from a network drive here)
    let guard = Guard::for_library(&folder)?;
    let id = text(params, "id");
    let (_, list) = batches_of(&folder);
    let info = list.into_iter().find(|b| b.id == id).ok_or_else(|| fail("bad_request", "There is no such entry in Repair history."))?;
    if info.undone {
        return Err(fail("bad_request", "This was already undone."));
    }
    if !matches!(info.kind.as_str(), "edit" | "import" | "restore") {
        not_look_only(env)?;
    }
    // (an organize batch is undone the same way: its "moved" lines are read backwards; its "done" and
    // "notdone" lines move nothing)
    Ok(UndoAsk { guard, id, info })
}

/// `history.undo`: puts a batch back (`repair::undo`) under the guard and a hold, and tells Cadence and
/// Backup about the folders and files that moved back.
pub(crate) fn undo(out: &Out, job: &Job, id: u64, env: &Env, ask: UndoAsk) -> Result<Value, Failure> {
    let library = ask.guard.library.clone();
    let root = repair::backups_root(&library);
    let hold = hold(out, env, job, &format!("Undoing {}", ask.info.title), "Undo")?;
    let held_at = std::cell::Cell::new(Instant::now());
    let went = (|| -> Result<Vec<String>, Failure> {
        ask.guard.still_there()?;
        let still = ask.guard.wait_not_playing(&|p| ask.info.places.iter().any(|t| guard::covers(t, p)), job, out, id, waits(env))?;
        renew(out, env, &hold.token, &held_at);
        let pairs = chained(crate::undo::moves(&root, &ask.id));
        // a file still playing at the end of the wait (or the folder holding it) is left as it is and said;
        // Undo can be run again for it later
        let playing = |place: &Path| still.as_ref().is_some_and(|p| guard::covers(place, p));
        let left = repair::undo_except(&root, &ask.id, &playing, &|| job.stopped()).map_err(|m| fail("failed", m))?;
        // what moved back: each move whose old place is filled again and whose new one is empty
        let back: Vec<(PathBuf, PathBuf)> = pairs.into_iter().rev().filter(|(from, to)| from.exists() && (!to.exists() || from.as_os_str().eq_ignore_ascii_case(to.as_os_str()))).map(|(from, to)| (to, from)).collect();
        let back = moves::from_pairs(&back);
        let undo_id = format!("{}-undo", ask.id);
        moves::for_backup(&env.dir, &undo_id, &back);
        moves::tell(out, &env.dir, &undo_id, &back, env.parts.ask_wait);
        Ok(left)
    })();
    if went.is_ok() && library.is_dir() {
        bring_up_to_date(env, &library, &ask.info.places);
    }
    release(out, env, hold);
    let left = went?;
    if job.stopped() && left.iter().any(|l| l == crate::undo::STOPPED) {
        return Err(fail("cancelled", crate::undo::STOPPED));
    }
    Ok(json!({ "left": left }))
}

pub(crate) fn clear_old_check(env: &Env, params: &Value) -> Result<PathBuf, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is cleared while Cadence runs as a test."));
    }
    folder_of(params, "Clearing old backups")
}

/// `history.clear_old`: the backups of repairs older than 30 days go to the Recycle Bin (never deleted).
pub(crate) fn clear_old(env: &Env, folder: &Path) -> Result<Value, Failure> {
    let (n, left) = crate::undo::clear_old(&repair::backups_root(folder), crate::undo::CLEAR_AFTER_DAYS, chrono::Local::now(), &env.parts.trash);
    Ok(json!({ "cleared": n, "left": left }))
}

// ------------------------------------------------------------------ Edit details

/// The tags Edit details may set.
const FIELDS: [&str; 8] = ["title", "artist", "album", "album_artist", "genre", "year", "track", "disc"];

pub(crate) struct EditAsk {
    guard: Guard,
    title: String,
    files: Vec<(PathBuf, std::collections::BTreeMap<String, String>)>,
}

/// `tags.edit`, before it starts: a test run, the guard, and the files and fields asked for. Not refused by
/// Look only: a person asks for it by hand.
pub(crate) fn edit_check(env: &Env, params: &Value) -> Result<EditAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "No tags are changed while Cadence runs as a test."));
    }
    let folder = folder_of(params, "Edit details")?;
    let guard = Guard::for_library(&folder)?;
    let mut files = vec![];
    for f in params.get("files").and_then(Value::as_array).into_iter().flatten() {
        let path = PathBuf::from(f.get("path").and_then(Value::as_str).unwrap_or_default());
        if !guard::covers(&folder, &path) || same_folder(&path.to_string_lossy(), &folder) || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err(fail("bad_request", format!("{} is not in the music folder.", path.display())));
        }
        let mut set = std::collections::BTreeMap::new();
        for (k, v) in f.get("set").and_then(Value::as_object).into_iter().flatten() {
            if !FIELDS.contains(&k.as_str()) {
                return Err(fail("bad_request", format!("Edit details cannot change \"{k}\"; it changes the title, artist, album, album artist, genre, year, track and disc.")));
            }
            let Some(v) = v.as_str() else { return Err(fail("bad_request", format!("The new {} must be words.", k.replace('_', " ")))) };
            if v.chars().count() > 1000 || v.contains(['\0', '\r', '\n']) {
                return Err(fail("bad_request", format!("The new {} is too long or holds a line break.", k.replace('_', " "))));
            }
            set.insert(k.clone(), v.trim().to_string());
        }
        if !set.is_empty() {
            files.push((path, set));
        }
    }
    if files.is_empty() {
        return Err(fail("bad_request", "Edit details needs the files and the details to set on them."));
    }
    let title = match text(params, "title") {
        t if !t.is_empty() => t,
        _ => format!("Edit details: {}", files[0].0.parent().map(leaf).unwrap_or_default()),
    };
    Ok(EditAsk { guard, title, files })
}

/// `tags.edit`: each file's tags through the Rust tag editor (which proves the audio is byte for byte the
/// same), the old file into the undo backups, all as one batch of kind "edit".
pub(crate) fn edit(out: &Out, job: &Job, id: u64, env: &Env, ask: EditAsk) -> Result<Value, Failure> {
    let tools = tools(env, &load(env)?)?;
    let hold = hold(out, env, job, &ask.title, "edit")?;
    let steps: Vec<Step> = ask.files.iter().map(|(file, set)| Step::Tags(TagStep { file: file.clone(), set: set.clone(), reason: "Edit details".into(), ..Default::default() })).collect();
    let ran = run_plans(out, job, id, env, &ask.guard, &hold, &tools, &ask.title, "edit", 1, &|_| ask.title.clone(), |_| Plan { steps: steps.clone(), ..Default::default() });
    if let Ok((done, _)) = &ran {
        let paths: Vec<PathBuf> = ask.files.iter().map(|(f, _)| f.clone()).collect();
        after(out, env, &ask.guard.library, done.batch.as_deref(), &paths);
    }
    release(out, env, hold);
    let (done, ended) = ran?;
    if let Some(f) = ended {
        return Err(f);
    }
    if job.stopped() {
        return Err(fail("cancelled", "It was stopped; what was done is one entry in Repair history, and Undo puts it back."));
    }
    // each problem with the full path of the file it is about (two files of one name in different folders
    // are told apart); a problem about no one file comes with "path": null
    let mut problems: Vec<Value> = done.about.iter().map(|(f, why)| json!({ "path": f, "words": why })).collect();
    let mut told: Vec<String> = done.about.iter().map(|(f, why)| format!("{}: {why}", leaf(f))).collect();
    for p in &done.problems {
        match told.iter().position(|t| t == p) {
            Some(i) => {
                told.remove(i);
            }
            None => problems.push(json!({ "path": null, "words": p })),
        }
    }
    Ok(json!({ "batch": done.batch, "changed": done.changed, "problems": problems }))
}

// ------------------------------------------------------------------ the guard, asked directly

/// `library.guard`: whether Cadence may change files in this folder, before anything is asked.
pub(crate) fn guard_state(params: &Value) -> Result<Value, Failure> {
    let folder = folder_of(params, "The library guard")?;
    let p = guard::place(&folder);
    Ok(json!({ "state": p.name(), "words": guard::place_words(p, &folder) }))
}
