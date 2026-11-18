//! Cadence's hidden engine: the Music Tool's logic, answering Cadence instead of drawing a window.
//!
//! Cadence starts `cadence-engine.exe` with its standard input and output as pipes and writes one JSON
//! message per line; the engine answers the same way. The whole conversation is described in
//! `ENGINE-PROTOCOL.md` at the top of the project; keep the two in step.
//!
//! What it does now: the handshake, the settings it keeps, the two helper programs (status and fetch) and a
//! quick health check of a folder, with progress and cancel (protocol 1); and Get music (protocol 2): its own
//! Soulseek client and login, searching, downloading and saving an album with the Music Tool's importer, with
//! asks to Cadence to hold its library watcher around the writes; and Health, repairs, Repair history, Edit
//! details and the library guard (protocol 3, `library.rs`, `moves.rs`, `crate::guard`): every change to the
//! music folder goes through the one guard, asks `library.playing` before touching a song and `library.moved`
//! after moving one, and lands in the undo journal as one batch with Undo. Recs (protocol 4, `recs.rs`), and
//! Backup (protocol 5, `backup.rs`): the Music Tool's backup worker with Cadence's own settings, moves that
//! follow, bring-back through the guard, and a nightly task made only from a click.
//!
//! How it stays safe:
//!   * Everything it keeps is in Cadence's data folder (`<CADENCE_DATA_DIR>\Tools`), never in the Music
//!     Tool's, and never beside the exe (an update replaces that folder).
//!   * While `CADENCE_OFFLINE_TEST` is set it fetches nothing and saves no settings; a quick check reads the
//!     folder it is given and writes only its own report and caches, in the engine's folder. It also refuses
//!     every Soulseek sign-in, sign-out, search and download at once, and starts no Soulseek client.
//!   * Its Soulseek client is its own (`<Tools>\slskd`, login `Cadence: Soulseek`) and ends with it; its
//!     import work folder is `<Tools>\import-work`, never the Music Tool's.
//!   * It writes into the music folder only between Cadence's `library.hold` and `library.release`.
//!   * It never plays sound (nothing in it can), whatever `CADENCE_NO_SOUND` says; the handshake reports the
//!     switch back so a check can see it arrived.
//!   * When Cadence goes away its end of the pipe closes; the engine then stops every job and ends, so it
//!     never outlives Cadence.
//!   * Nothing but protocol lines reaches the pipe: the process's own standard output is pointed at standard
//!     error before the first message, so a stray print in the library cannot break a message.
mod backup;
pub use backup::nightly_if_requested;
mod jobs;
mod library;
mod organizing;
mod recs;
pub(crate) mod moves;
mod music;
mod soulseek;
#[cfg(test)]
mod standin;
#[cfg(test)]
mod tests;

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The protocol this engine speaks. Raise it whenever a message changes in a way an older Cadence would
/// read wrongly; Cadence refuses an engine whose number differs from its own.
pub const PROTOCOL: u32 = 5;

/// The longest line Cadence may send (a settings change is a few kilobytes at most).
const MAX_LINE: usize = 1 << 20;

/// What the engine was started with.
#[derive(Clone)]
pub struct Env {
    /// the engine's own folder: settings, caches, reports, helpers
    pub dir: PathBuf,
    /// `CADENCE_OFFLINE_TEST`: fetch nothing, save no settings
    pub offline_test: bool,
    /// `CADENCE_NO_SOUND`, reported back in the handshake
    pub no_sound: bool,
    /// how it reaches the world: the real Soulseek client and online lookups, or the tests' stand-ins
    pub(crate) parts: Parts,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env").field("dir", &self.dir).field("offline_test", &self.offline_test).field("no_sound", &self.no_sound).finish_non_exhaustive()
    }
}

/// The parts of the engine that reach outside it, so the tests can put stand-ins in their place.
#[derive(Clone)]
pub(crate) struct Parts {
    /// covers, track lists and lyrics are looked up online
    pub(crate) online: bool,
    /// how the Soulseek client is started
    pub(crate) slskd: Arc<dyn soulseek::Launch>,
    /// the Credential Manager entry the Soulseek login is kept under (with `secrets::set_owner("Cadence")`,
    /// "Cadence: Soulseek")
    pub(crate) login: String,
    /// how the engine reaches MusicBrainz, Apple Music and Last.fm (with its cache); the tests' asks a stand-in
    pub(crate) net: Arc<crate::net::Net>,
    /// the album the search words mean, with its track counts (None when not known)
    pub(crate) album: Arc<dyn Fn(&str) -> Option<crate::dl_facts::Album> + Send + Sync>,
    /// how an imported download goes to the Recycle Bin
    pub(crate) trash: fn(&Path) -> Result<(), String>,
    /// how long an ask waits for Cadence's answer
    pub(crate) ask_wait: Duration,
    /// how often the guard looks again at a file that waits to finish playing (2 seconds; the tests' less)
    pub(crate) playing_every: Duration,
    /// how long `organize.album` waits for its album to stop playing before it leaves it (10 minutes; the
    /// tests' less)
    pub(crate) album_wait: Duration,
    /// how often a job that holds the library for long says so again (`library.renew`), well inside
    /// Cadence's 30 minutes (10 minutes; the tests' less)
    pub(crate) renew_every: Duration,
    /// Organize tells Cadence its moves (`library.moved`) once this many albums have moved (10; the tests'
    /// less) ...
    pub(crate) moves_every: usize,
    /// ... or once the first of them is this old (30 seconds), whichever comes first
    pub(crate) moves_after: Duration,
    /// tests only: the first saving step waits here until the test opens it
    pub(crate) saving_gate: Option<Arc<Gate>>,
    /// how Backup reads drives (names, folder identities, ejecting); the tests' stand-in swaps disks
    pub(crate) disks: Arc<dyn crate::backup::Disks>,
    /// where the nightly backup task is kept: Task Scheduler, or the tests' stand-in that only writes down
    pub(crate) scheduler: Arc<dyn crate::backup::api::Scheduler>,
    /// Cadence's data folder, saved beside the music by every backup (None: nothing is saved beside it)
    pub(crate) cadence_data: Option<PathBuf>,
}

impl Parts {
    /// The real ones: slskd from the engine's folder, Apple Music for track lists, the Recycle Bin.
    fn real(dir: &Path) -> Parts {
        let net = Arc::new(crate::net::Net::new(dir.join("cache")));
        let lookups = net.clone();
        Parts {
            net,
            online: true,
            slskd: Arc::new(soulseek::Spawned::default()),
            login: crate::secrets::SOULSEEK.to_string(),
            album: Arc::new(move |words: &str| crate::dl_facts::look_up(&lookups, words).ok().flatten()),
            trash: crate::undo::to_recycle_bin,
            ask_wait: Duration::from_secs(120),
            playing_every: Duration::from_secs(2),
            album_wait: Duration::from_secs(600),
            renew_every: Duration::from_secs(600),
            moves_every: 10,
            moves_after: Duration::from_secs(30),
            saving_gate: None,
            disks: Arc::new(crate::backup::RealDisks),
            scheduler: Arc::new(crate::backup::api::TaskScheduler),
            // (the engine's folder is <data>\Tools)
            cadence_data: dir.parent().map(Path::to_path_buf),
        }
    }
}

/// A door a test holds shut (see `Parts::saving_gate`).
#[derive(Default)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    turn: Condvar,
}

impl Gate {
    pub(crate) fn wait(&self) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        while !*open {
            open = self.turn.wait(open).unwrap_or_else(|e| e.into_inner());
        }
    }
    #[cfg(test)]
    pub(crate) fn open(&self) {
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.turn.notify_all();
    }
}

/// What every request can reach: the settings and parts, the Soulseek client, the copies found.
pub(crate) struct Engine {
    pub(crate) env: Env,
    pub(crate) soulseek: soulseek::Soulseek,
    pub(crate) music: music::Store,
    pub(crate) backup: backup::State,
}

fn switch(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

impl Env {
    /// From the process's environment: Cadence's data folder from `CADENCE_DATA_DIR` (Cadence always sets
    /// it), else where an installed Cadence keeps its data.
    pub fn from_process() -> Env {
        let data = std::env::var_os("CADENCE_DATA_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("CadencePlayer").join("Data"));
        let dir = data.join("Tools");
        Env { parts: Parts::real(&dir), dir, offline_test: switch("CADENCE_OFFLINE_TEST"), no_sound: switch("CADENCE_NO_SOUND") }
    }
}

/// The engine program: protocol on standard input and output until Cadence closes the pipe or says
/// `shutdown`. Returns the exit code.
pub fn run() -> i32 {
    let env = Env::from_process();
    if let Err(why) = std::fs::create_dir_all(&env.dir) {
        eprintln!("cadence-engine: its folder {} could not be made: {why}", env.dir.display());
    }
    crate::config::set_engine_dir(env.dir.clone());
    // its own Credential Manager entries ("Cadence: Soulseek"), never the Music Tool's
    crate::secrets::set_owner("Cadence");
    let out = protocol_output();
    eprintln!("cadence-engine {} (protocol {PROTOCOL}) started; folder {}", env!("CARGO_PKG_VERSION"), env.dir.display());
    serve(std::io::stdin().lock(), out, env)
}

/// Takes the process's standard output for the protocol alone and points the process's standard output at
/// standard error, so a print anywhere in the library lands in the engine's log, not in a message.
fn protocol_output() -> Box<dyn Write + Send> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut own = std::ptr::null_mut();
        if out.is_null() || out == INVALID_HANDLE_VALUE || DuplicateHandle(GetCurrentProcess(), out, GetCurrentProcess(), &mut own, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
            return Box::new(std::io::stdout());
        }
        SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE));
        Box::new(std::fs::File::from_raw_handle(own as _))
    }
}

// ------------------------------------------------------------------ sending

/// The engine's end of the pipe. Each message is written whole, then flushed, under one lock, so lines from
/// different jobs never mix.
pub(crate) struct Out {
    w: Mutex<Box<dyn Write + Send>>,
    /// Cadence's end is gone; nothing more is written
    gone: AtomicBool,
    /// asks waiting for Cadence's answer, by their number
    asks: Mutex<HashMap<u64, crossbeam_channel::Sender<Result<Value, jobs::Failure>>>>,
    next_ask: AtomicU64,
}

impl Out {
    fn new(w: Box<dyn Write + Send>) -> Arc<Out> {
        Arc::new(Out { w: Mutex::new(w), gone: AtomicBool::new(false), asks: Mutex::default(), next_ask: AtomicU64::new(0) })
    }

    pub(crate) fn send(&self, msg: &Value) {
        if self.gone.load(Ordering::Relaxed) {
            return;
        }
        let mut line = serde_json::to_vec(msg).unwrap_or_default();
        line.push(b'\n');
        let mut w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        if w.write_all(&line).and_then(|_| w.flush()).is_err() {
            self.gone.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn result(&self, id: u64, result: Value) {
        self.send(&json!({ "id": id, "result": result }));
    }

    pub(crate) fn error(&self, id: Option<u64>, code: &str, message: impl Into<String>) {
        self.send(&json!({ "id": id, "error": { "code": code, "message": message.into() } }));
    }

    pub(crate) fn progress(&self, id: u64, data: Value) {
        self.send(&json!({ "id": id, "event": "progress", "data": data }));
    }

    /// Asks Cadence something and waits up to `wait` for its answer (see "Asks" in ENGINE-PROTOCOL.md). Stops
    /// waiting when `stopped` says so, or when Cadence goes away.
    pub(crate) fn ask(&self, method: &str, params: Value, wait: Duration, stopped: &dyn Fn() -> bool) -> Result<Value, jobs::Failure> {
        let n = self.next_ask.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.asks.lock().unwrap_or_else(|e| e.into_inner()).insert(n, tx);
        self.send(&json!({ "ask": n, "method": method, "params": params }));
        let until = Instant::now() + wait;
        let got = loop {
            if self.gone.load(Ordering::Relaxed) {
                break Err(jobs::fail("failed", "Cadence went away before it answered."));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(jobs::fail("no_answer", format!("Cadence did not answer \"{method}\" in time.")));
            }
            match rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(r) => break r,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) if stopped() => break Err(jobs::fail("cancelled", "It was stopped while it waited for Cadence.")),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break Err(jobs::fail("failed", "Cadence went away before it answered.")),
            }
        };
        self.asks.lock().unwrap_or_else(|e| e.into_inner()).remove(&n);
        got
    }

    /// Hands Cadence's answer to the ask waiting for it. False when nothing waits for that number.
    fn answer(&self, n: u64, answer: Result<Value, jobs::Failure>) -> bool {
        match self.asks.lock().unwrap_or_else(|e| e.into_inner()).remove(&n) {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// Cadence is gone: every ask still waiting ends at once.
    fn end_asks(&self) {
        self.asks.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

// ------------------------------------------------------------------ jobs that take a while

/// One running job: cancel sets `stop` (and runs `on_stop`, for work that has its own stop).
pub(crate) struct Job {
    pub(crate) stop: AtomicBool,
    pub(crate) on_stop: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Job {
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.on_stop.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            f();
        }
    }
}

type Jobs = Arc<Mutex<HashMap<u64, Arc<Job>>>>;

/// Runs `work` on its own thread as job `id`. Whatever it returns is the reply: a result, or an error (a
/// stopped job answers `cancelled`). A fault inside it is caught and answered as `failed`, and the engine
/// goes on.
fn spawn_job(out: &Arc<Out>, jobs: &Jobs, id: u64, work: impl FnOnce(&Out, &Job) -> Result<Value, (String, String)> + Send + 'static) {
    let job = Arc::new(Job { stop: AtomicBool::new(false), on_stop: Mutex::new(None) });
    jobs.lock().unwrap_or_else(|e| e.into_inner()).insert(id, job.clone());
    let (out, jobs) = (out.clone(), jobs.clone());
    std::thread::spawn(move || {
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&out, &job)));
        // (taken off the list before the reply, so a cancel that follows the reply finds nothing to stop)
        jobs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        match ended {
            Ok(Ok(v)) => out.result(id, v),
            Ok(Err((code, message))) => out.error(Some(id), &code, message),
            Err(fault) => {
                let what = fault.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| fault.downcast_ref::<String>().cloned()).unwrap_or_default();
                eprintln!("cadence-engine: job {id} hit a fault: {what}");
                out.error(Some(id), "failed", format!("The engine hit a fault in this job and stopped it. {what}").trim_end().to_string());
            }
        }
    });
}

/// Answers a quick request on a thread of its own, so a slow answer (Soulseek not answering) never holds up
/// the messages behind it. It is not a job: it cannot be cancelled.
fn later(out: &Arc<Out>, id: u64, work: impl FnOnce() -> Result<Value, jobs::Failure> + Send + 'static) {
    let out = out.clone();
    std::thread::spawn(move || match work() {
        Ok(v) => out.result(id, v),
        Err((code, m)) => out.error(Some(id), &code, m),
    });
}

/// Answers a quick request at once: its result, or its error.
fn answer(out: &Out, id: u64, r: Result<Value, jobs::Failure>) {
    match r {
        Ok(v) => out.result(id, v),
        Err((code, m)) => out.error(Some(id), &code, m),
    }
}

/// Everything this engine answers (protocol 5).
/// (a slice: serde writes fixed arrays only up to 32 long)
const METHODS: &[&str] = &[
    "handshake", "ping", "cancel", "shutdown", "settings.get", "settings.set", "helpers.status", "helpers.fetch", "health.quick",
    "soulseek.status", "soulseek.sign_in", "soulseek.sign_out", "music.search", "music.get", "music.put_in", "music.forget", "music.kept",
    "health.check", "health.report", "health.ignore", "replace.want", "repair.plan", "repair.apply", "history.list", "history.undo",
    "history.clear_old", "tags.edit", "library.guard", "library.unacknowledged", "library.acknowledge",
    "organize.preview", "organize.run", "organize.status", "organize.album",
    "recs.refresh", "recs.get", "recs.artist", "recs.not_interested", "recs.mute", "recs.hidden",
    "backup.status", "backup.drives", "backup.preview_folders", "backup.confirm_folders", "backup.run", "backup.check",
    "backup.repair_damaged", "backup.eject", "backup.schedule",
];

// ------------------------------------------------------------------ the conversation

/// Answers messages from `input` on `out` until the input ends or Cadence says `shutdown`. Every running
/// job is then stopped; the engine waits up to 5 seconds for them to answer. Returns the exit code.
pub fn serve(mut input: impl BufRead, out: Box<dyn Write + Send>, env: Env) -> i32 {
    let out = Out::new(out);
    let jobs: Jobs = Arc::default();
    let eng = Arc::new(Engine {
        soulseek: soulseek::Soulseek::new(env.parts.slskd.clone(), env.parts.login.clone(), env.dir.join("downloads")),
        music: music::Store::default(),
        backup: backup::State::default(),
        env: env.clone(),
    });
    // the Soulseek client is ended when it has had nothing to do for `soulseek_idle_minutes`
    let watching = Arc::new(AtomicBool::new(true));
    {
        let (eng, watching) = (eng.clone(), watching.clone());
        std::thread::spawn(move || {
            while watching.load(Ordering::Relaxed) {
                let minutes = jobs::load(&eng.env).map(|c| c.soulseek_idle_minutes).unwrap_or(20.0);
                let on = minutes.is_finite() && minutes > 0.0;
                // (looked at every few seconds, so a changed setting counts soon)
                let tick = if on { Duration::from_secs_f64((minutes * 60.0 / 4.0).clamp(0.1, 5.0)) } else { Duration::from_secs(5) };
                let until = Instant::now() + tick;
                while watching.load(Ordering::Relaxed) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if on && watching.load(Ordering::Relaxed) {
                    eng.soulseek.stop_if_idle(Duration::from_secs_f64(minutes * 60.0));
                }
            }
        });
    }
    // Before any library write: settle Organize batches a crash cut off (ENGINE-PROTOCOL.md, "Crash-safe").
    organizing::recover_at_start(&env);
    let started = Instant::now();
    let mut shaken = false;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // (read a line, but never more than MAX_LINE of it into memory)
        let n = match std::io::Read::take(&mut input, MAX_LINE as u64 + 1).read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(why) => {
                eprintln!("cadence-engine: reading from Cadence failed: {why}");
                break;
            }
        };
        if n == 0 {
            eprintln!("cadence-engine: Cadence closed the pipe; ending");
            break;
        }
        if buf.len() > MAX_LINE {
            out.error(None, "bad_request", "That message is too long for the engine to read.");
            // skip the rest of the long line
            let mut rest = Vec::new();
            let _ = input.read_until(b'\n', &mut rest);
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let msg: Map<String, Value> = match serde_json::from_str(text) {
            Ok(Value::Object(m)) => m,
            _ => {
                out.error(None, "bad_request", "The engine could not read that message: it is not a JSON object.");
                continue;
            }
        };
        // Cadence answering one of the engine's asks
        if let Some(n) = msg.get("answer") {
            let Some(n) = n.as_u64() else {
                out.error(None, "bad_request", "An answer needs the whole-number ask it answers.");
                continue;
            };
            let answer = match msg.get("error") {
                Some(e) => Err((e["code"].as_str().unwrap_or("failed").to_string(), e["message"].as_str().unwrap_or("Cadence said no.").to_string())),
                None => Ok(msg.get("result").cloned().unwrap_or_else(|| json!({}))),
            };
            if !out.answer(n, answer) {
                eprintln!("cadence-engine: Cadence answered ask {n}, which nothing waits for");
            }
            continue;
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            out.error(None, "bad_request", "A request needs a whole-number id.");
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if jobs.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
            out.error(Some(id), "bad_request", format!("Request {id} is still running; each request needs its own id."));
            continue;
        }
        if method != "handshake" && !shaken {
            out.error(Some(id), "not_ready", "The engine answers only a handshake until it has had one.");
            continue;
        }
        match method.as_str() {
            "handshake" => {
                let theirs = params.get("protocol").and_then(Value::as_u64);
                if theirs != Some(PROTOCOL as u64) {
                    let theirs = theirs.map(|n| n.to_string()).unwrap_or_else(|| "none".into());
                    out.error(
                        Some(id),
                        "version_mismatch",
                        format!("This engine speaks protocol {PROTOCOL} and Cadence asked for {theirs}. The engine does not match this Cadence; reinstalling Cadence puts matching parts back."),
                    );
                    continue;
                }
                shaken = true;
                out.result(
                    id,
                    json!({
                        "engine": "cadence-engine",
                        "version": env!("CARGO_PKG_VERSION"),
                        "protocol": PROTOCOL,
                        "pid": std::process::id(),
                        "folder": env.dir,
                        "offline_test": env.offline_test,
                        "no_sound": env.no_sound,
                        "methods": METHODS,
                    }),
                );
            }
            "ping" => out.result(id, json!({ "jobs": jobs.lock().unwrap_or_else(|e| e.into_inner()).len(), "up_ms": started.elapsed().as_millis() as u64 })),
            "cancel" => {
                let Some(target) = params.get("id").and_then(Value::as_u64) else {
                    out.error(Some(id), "bad_request", "Cancel needs the id of the request to stop.");
                    continue;
                };
                let job = jobs.lock().unwrap_or_else(|e| e.into_inner()).get(&target).cloned();
                if let Some(job) = &job {
                    job.cancel();
                }
                out.result(id, json!({ "cancelled": job.is_some() }));
            }
            "shutdown" => {
                out.result(id, json!({}));
                break;
            }
            "settings.get" => match jobs::settings_get(&env) {
                Ok(v) => out.result(id, v),
                Err((code, m)) => out.error(Some(id), &code, m),
            },
            "settings.set" => match jobs::settings_set(&env, &params) {
                Ok(v) => out.result(id, v),
                Err((code, m)) => out.error(Some(id), &code, m),
            },
            "helpers.status" => out.result(id, jobs::helpers_status()),
            "helpers.fetch" => match jobs::helpers_fetch_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(h) => spawn_job(&out, &jobs, id, move |out, job| jobs::helpers_fetch(out, job, id, h)),
            },
            // ("health.quick" is protocol 1's name for it)
            "health.check" | "health.quick" => match library::check_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| library::check(out, job, id, &env, ask))
                }
            },
            "health.report" => {
                let env = env.clone();
                later(&out, id, move || library::report(&env, &params));
            }
            "health.ignore" => answer(&out, id, library::ignore(&env, &params)),
            "replace.want" => answer(&out, id, library::replace_want(&env, &params)),
            "repair.plan" => {
                let env = env.clone();
                later(&out, id, move || library::plan(&env, &params));
            }
            "repair.apply" => match library::apply_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| library::apply(out, job, id, &env, ask))
                }
            },
            "history.list" => later(&out, id, move || library::history_list(&params)),
            "history.undo" => match library::undo_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| library::undo(out, job, id, &env, ask))
                }
            },
            "history.clear_old" => match library::clear_old_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(folder) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |_, _| library::clear_old(&env, &folder))
                }
            },
            "tags.edit" => match library::edit_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| library::edit(out, job, id, &env, ask))
                }
            },
            "organize.preview" => match organizing::preview_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| organizing::preview(out, job, id, &env, ask))
                }
            },
            "organize.run" => match organizing::run_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| organizing::run(out, job, id, &env, ask))
                }
            },
            "organize.status" => answer(&out, id, organizing::status(&env, &params)),
            "organize.album" => match organizing::album_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let env = env.clone();
                    spawn_job(&out, &jobs, id, move |out, job| organizing::album(out, job, id, &env, ask))
                }
            },
            "recs.refresh" => match recs::refresh_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| recs::refresh(out, job, id, &eng, ask))
                }
            },
            "recs.get" => {
                let env = env.clone();
                later(&out, id, move || recs::get(&env, &params));
            }
            "recs.artist" => {
                let env = env.clone();
                later(&out, id, move || recs::artist(&env, &params));
            }
            "recs.not_interested" => answer(&out, id, recs::not_interested(&env, &params)),
            "recs.mute" => answer(&out, id, recs::mute(&env, &params)),
            "recs.hidden" => answer(&out, id, recs::hidden(&env)),
            "backup.status" => {
                let eng = eng.clone();
                later(&out, id, move || backup::status(&eng, &params));
            }
            "backup.drives" => {
                let eng = eng.clone();
                later(&out, id, move || backup::drives(&eng, &params));
            }
            "backup.preview_folders" => {
                let eng = eng.clone();
                later(&out, id, move || backup::preview_folders(&eng, &params));
            }
            "backup.confirm_folders" => {
                let eng = eng.clone();
                later(&out, id, move || backup::confirm_folders(&eng, &params));
            }
            "backup.schedule" => {
                let eng = eng.clone();
                later(&out, id, move || backup::schedule(&eng, &params));
            }
            "backup.run" => match backup::run_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| backup::run(out, job, id, &eng, ask))
                }
            },
            "backup.check" => match backup::check_check(&params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| backup::check(out, job, id, &eng, ask))
                }
            },
            "backup.repair_damaged" => match backup::repair_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| backup::repair(out, job, id, &eng, ask))
                }
            },
            "backup.eject" => match backup::eject_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(which) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |_, _| backup::eject(&eng, which))
                }
            },
            "library.guard" => answer(&out, id, library::guard_state(&params)),
            "library.unacknowledged" => out.result(id, moves::unacknowledged(&env.dir)),
            "library.acknowledge" => answer(&out, id, moves::acknowledge(&env.dir, &params)),
            "soulseek.status" => {
                let eng = eng.clone();
                later(&out, id, move || Ok(jobs::soulseek_status(&eng)));
            }
            "soulseek.sign_in" => match jobs::sign_in_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok((name, password)) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |_, job| eng.soulseek.sign_in(job, &name, &password).map(|name| json!({ "name": name })))
                }
            },
            "soulseek.sign_out" => {
                if env.offline_test {
                    out.error(Some(id), jobs::OFFLINE, "Nothing is changed while Cadence runs as a test.");
                    continue;
                }
                let eng = eng.clone();
                later(&out, id, move || {
                    eng.soulseek.sign_out();
                    Ok(json!({}))
                });
            }
            "music.search" => match music::search_check(&env, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| music::search(out, job, id, &eng, ask))
                }
            },
            "music.get" => match music::get_check(&env, &eng, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| music::get(out, job, id, &eng, ask))
                }
            },
            "music.put_in" => match music::put_in_check(&env, &eng, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(ask) => {
                    let eng = eng.clone();
                    spawn_job(&out, &jobs, id, move |out, job| music::put_in(out, job, id, &eng, ask))
                }
            },
            "music.forget" => match music::forget_check(&env, &eng, &params) {
                Err((code, m)) => out.error(Some(id), &code, m),
                Ok(c) => {
                    let eng = eng.clone();
                    later(&out, id, move || music::forget(&eng, &c));
                }
            },
            "music.kept" => out.result(id, music::kept_list(&eng)),
            "" => out.error(Some(id), "bad_request", "A request needs a method."),
            other => out.error(Some(id), "unknown_method", format!("The engine does not know \"{other}\".")),
        }
    }
    watching.store(false, Ordering::Relaxed);
    // no ask can be answered now
    out.end_asks();
    // stop everything still going and give it a moment to answer
    let running: Vec<Arc<Job>> = jobs.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for job in &running {
        job.cancel();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !jobs.lock().unwrap_or_else(|e| e.into_inner()).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    eng.soulseek.stop();
    0
}
