//! The engine conversation, driven the way Cadence drives it: lines into one pipe, lines out of another.
use super::*;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::io::{BufReader, Write};

/// Protocol 3: Health, repairs, Repair history, Edit details and the library guard.
#[path = "library_tests.rs"]
mod library_tests;

struct Talk {
    to: Option<std::io::PipeWriter>,
    from: Receiver<Value>,
    ended: std::thread::JoinHandle<i32>,
}

fn scratch(name: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("engine-test-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn env(name: &str) -> Env {
    soul_env(name).0
}

/// A test's engine settings, with a stand-in Soulseek of its own (see `standin`), no online lookups, a login
/// entry of its own and a Recycle Bin that is a folder beside its downloads.
fn soul_env(name: &str) -> (Env, Arc<standin::StandIn>) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let s = standin::StandIn::start();
    let dir = scratch(name);
    let parts = Parts {
        // (nothing listens there: a test that does not set up a stand-in of its own reaches nothing)
        net: Arc::new(crate::net::Net::stand_in(dir.join("cache"), "http://127.0.0.1:9")),
        online: false,
        slskd: s.clone(),
        login: format!("Soulseek test {}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)),
        album: Arc::new(|words: &str| {
            words.to_lowercase().contains("quiet rooms").then(|| crate::dl_facts::Album { artist: "Test Artist".into(), title: "Quiet Rooms".into(), year: "2020".into(), editions: vec![3, 4], artwork: String::new() })
        }),
        trash: |p| {
            let bin = p.parent().and_then(Path::parent).ok_or("no test bin")?.join("bin");
            std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
            std::fs::rename(p, bin.join(p.file_name().unwrap())).map_err(|e| e.to_string())
        },
        ask_wait: Duration::from_secs(20),
        playing_every: Duration::from_millis(200),
        album_wait: Duration::from_millis(1500),
        renew_every: Duration::from_secs(600),
        moves_every: 10,
        moves_after: Duration::from_secs(30),
        saving_gate: None,
        // (drives and Task Scheduler are stand-ins: a test never reads a real disk's name or makes a task)
        disks: Arc::new(backup::fake::FakeDisks::new()),
        scheduler: Arc::new(backup::fake::FakeScheduler::default()),
        cadence_data: None,
    };
    (Env { dir, offline_test: false, no_sound: false, parts }, s)
}

fn start(env: Env) -> Talk {
    let (in_r, in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let ended = std::thread::spawn(move || serve(BufReader::new(in_r), Box::new(out_w), env));
    let (tx, from) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        for line in BufReader::new(out_r).lines() {
            let Ok(line) = line else { break };
            let v: Value = serde_json::from_str(&line).expect("every line the engine writes is JSON");
            if tx.send(v).is_err() {
                break;
            }
        }
    });
    Talk { to: Some(in_w), from, ended }
}

impl Talk {
    fn raw(&mut self, line: &str) {
        let w = self.to.as_mut().unwrap();
        w.write_all(line.as_bytes()).unwrap();
        w.write_all(b"\n").unwrap();
    }
    fn send(&mut self, id: u64, method: &str, params: Value) {
        self.raw(&json!({ "id": id, "method": method, "params": params }).to_string());
    }
    fn next(&self) -> Value {
        self.from.recv_timeout(Duration::from_secs(60)).expect("the engine answers within a minute")
    }
    /// The reply to `id`, passing over its progress events (counted).
    fn reply(&self, id: u64) -> (Value, usize) {
        let mut events = 0;
        loop {
            let m = self.next();
            if m["id"] == json!(id) && m.get("event").is_some() {
                events += 1;
                continue;
            }
            assert_eq!(m["id"], json!(id), "an answer to another request came first: {m}");
            return (m, events);
        }
    }
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(id, method, params);
        self.reply(id).0
    }
    fn shake(&mut self) -> Value {
        let r = self.ask(1, "handshake", json!({ "protocol": PROTOCOL, "host": "test" }));
        assert!(r.get("result").is_some(), "{r}");
        r["result"].clone()
    }
    /// Closes Cadence's end and waits for the engine to end.
    fn close(mut self) -> i32 {
        drop(self.to.take());
        self.ended.join().unwrap()
    }
}

fn code(m: &Value) -> &str {
    m["error"]["code"].as_str().unwrap_or_else(|| panic!("expected an error, got {m}"))
}

#[test]
fn the_handshake_says_who_answers_and_nothing_else_is_answered_before_it() {
    let e = env("shake");
    let mut t = start(Env { offline_test: true, no_sound: true, ..e.clone() });
    assert_eq!(code(&t.ask(5, "ping", json!({}))), "not_ready");
    let r = t.shake();
    assert_eq!(r["engine"], "cadence-engine");
    assert_eq!(r["protocol"], json!(PROTOCOL));
    assert_eq!(r["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(r["folder"], json!(e.dir));
    assert_eq!((r["offline_test"].as_bool(), r["no_sound"].as_bool()), (Some(true), Some(true)));
    assert!(t.ask(6, "ping", json!({}))["result"]["up_ms"].is_u64());
    assert_eq!(t.close(), 0);
}

#[test]
fn another_protocol_is_refused_in_plain_words() {
    let mut t = start(env("mismatch"));
    let r = t.ask(1, "handshake", json!({ "protocol": PROTOCOL + 1 }));
    assert_eq!(code(&r), "version_mismatch");
    let words = r["error"]["message"].as_str().unwrap();
    assert!(words.contains(&format!("protocol {PROTOCOL}")) && words.contains("reinstalling Cadence"), "{words}");
    // still refused: no handshake has been accepted
    assert_eq!(code(&t.ask(2, "ping", json!({}))), "not_ready");
    assert_eq!(code(&t.ask(3, "handshake", json!({}))), "version_mismatch");
    t.close();
}

#[test]
fn messages_it_cannot_read_are_answered_not_fatal() {
    let mut t = start(env("bad"));
    t.raw("this is not json");
    assert_eq!(code(&t.next()), "bad_request");
    t.raw("[1, 2]");
    assert_eq!(code(&t.next()), "bad_request");
    t.raw(r#"{"method":"ping"}"#);
    let m = t.next();
    assert_eq!((code(&m), &m["id"]), ("bad_request", &Value::Null));
    t.raw(&"x".repeat(MAX_LINE + 10));
    assert_eq!(code(&t.next()), "bad_request");
    t.shake();
    assert_eq!(code(&t.ask(9, "no.such.thing", json!({}))), "unknown_method");
    assert_eq!(code(&t.ask(10, "", json!({}))), "bad_request");
    // and after all that, it still answers
    assert!(t.ask(11, "ping", json!({})).get("result").is_some());
    t.close();
}

#[test]
fn settings_round_trip_and_bad_changes_change_nothing() {
    let e = env("settings");
    let mut t = start(e.clone());
    t.shake();
    let first = t.ask(2, "settings.get", json!({}))["result"]["settings"].clone();
    assert!(first.get("music_dir").is_some() && first.get("lastfm_api_key").is_none(), "{first}");
    let r = t.ask(3, "settings.set", json!({ "changes": { "music_dir": r"X:\Made Up", "months_back": 7 } }));
    assert_eq!(r["result"]["settings"]["music_dir"], r"X:\Made Up", "{r}");
    let again = t.ask(4, "settings.get", json!({}))["result"]["settings"].clone();
    assert_eq!((again["music_dir"].as_str(), again["months_back"].as_u64()), (Some(r"X:\Made Up"), Some(7)));
    // kept in the engine's own folder, written whole
    let kept: Value = serde_json::from_slice(&std::fs::read(e.dir.join("config.json")).unwrap()).unwrap();
    assert_eq!(kept["months_back"], 7);
    assert!(!e.dir.join("config.json.part").exists());
    // a name it does not have, and a value of the wrong kind: refused, and the file stands
    assert_eq!(code(&t.ask(5, "settings.set", json!({ "changes": { "no_such": 1 } }))), "bad_request");
    assert_eq!(code(&t.ask(6, "settings.set", json!({ "changes": { "months_back": "seven" } }))), "bad_request");
    assert_eq!(code(&t.ask(7, "settings.set", json!({}))), "bad_request");
    assert_eq!(t.ask(8, "settings.get", json!({}))["result"]["settings"]["months_back"], 7);
    t.close();

    // a test run saves nothing
    let mut t = start(Env { offline_test: true, ..e.clone() });
    t.shake();
    assert_eq!(code(&t.ask(2, "settings.set", json!({ "changes": { "months_back": 9 } }))), "offline_test");
    assert_eq!(t.ask(3, "settings.get", json!({}))["result"]["settings"]["months_back"], 7);
    t.close();
}

#[test]
fn a_damaged_settings_file_is_reported_and_left_alone() {
    let e = env("damaged");
    std::fs::write(e.dir.join("config.json"), b"{ not json").unwrap();
    let mut t = start(e.clone());
    t.shake();
    let r = t.ask(2, "settings.get", json!({}));
    assert_eq!(code(&r), "failed");
    assert!(r["error"]["message"].as_str().unwrap().contains("damaged"));
    assert_eq!(code(&t.ask(3, "settings.set", json!({ "changes": { "months_back": 2 } }))), "failed");
    assert_eq!(std::fs::read(e.dir.join("config.json")).unwrap(), b"{ not json");
    t.close();
}

#[test]
fn helpers_are_listed_and_a_test_run_fetches_nothing() {
    let mut t = start(Env { offline_test: true, ..env("helpers") });
    t.shake();
    let list = t.ask(2, "helpers.status", json!({}))["result"]["helpers"].clone();
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["ffmpeg", "slskd"]);
    for h in list.as_array().unwrap() {
        assert!(h["found"].is_boolean() && h["version"].is_string() && h["state"].is_string(), "{h}");
    }
    assert_eq!(code(&t.ask(3, "helpers.fetch", json!({ "helper": "slskd" }))), "offline_test");
    assert_eq!(code(&t.ask(4, "helpers.fetch", json!({ "helper": "winamp" }))), "bad_request");
    t.close();
}

/// An artist folder with album folders of made-up tracks (not real audio: each is read, and found unreadable).
fn library(albums: usize, tracks: usize) -> PathBuf {
    let lib = scratch("library");
    for a in 0..albums {
        let album = lib.join("Made Up Artist").join(format!("Album {a:03} (2001)"));
        std::fs::create_dir_all(&album).unwrap();
        for n in 0..tracks {
            std::fs::write(album.join(format!("{:02} Track {n}.mp3", n + 1)), vec![0u8; 2048]).unwrap();
        }
    }
    lib
}

#[test]
fn a_quick_check_reports_progress_and_its_findings() {
    let lib = library(2, 3);
    let e = env("quick");
    let mut t = start(e.clone());
    t.shake();
    assert_eq!(code(&t.ask(2, "health.quick", json!({ "folder": lib.join("nowhere") }))), "not_found");
    assert_eq!(code(&t.ask(3, "health.quick", json!({}))), "bad_request");
    t.send(4, "health.quick", json!({ "folder": lib }));
    let (r, events) = t.reply(4);
    let r = &r["result"];
    assert_eq!(r["files"], 6, "{r}");
    assert_eq!(r["artists"], 1, "{r}");
    assert!(events >= 1, "no progress was sent");
    // the report is in the engine's own folder; the library was only read
    assert!(r["report"].as_str().unwrap().starts_with(&*e.dir.to_string_lossy()));
    assert!(std::path::Path::new(r["report"].as_str().unwrap()).is_file());
    let after: Vec<_> = walk(&lib);
    assert_eq!(after.len(), 6, "the check wrote into the library: {after:?}");
    t.close();
}

fn walk(d: &std::path::Path) -> Vec<PathBuf> {
    let mut v = vec![];
    for e in std::fs::read_dir(d).unwrap().flatten() {
        if e.path().is_dir() { v.extend(walk(&e.path())) } else { v.push(e.path()) }
    }
    v
}

#[test]
fn a_quick_check_stops_when_cancelled() {
    let lib = library(40, 12);
    let mut t = start(env("cancel"));
    t.shake();
    t.send(2, "health.quick", json!({ "folder": lib }));
    // wait until it is reading tags, then stop it
    loop {
        let m = t.next();
        assert_eq!(m["event"], "progress", "it ended before it could be stopped: {m}");
        if m["data"]["phase"] == 1 {
            break;
        }
    }
    let stopped = Instant::now();
    t.send(3, "cancel", json!({ "id": 2 }));
    let mut got = (None, None);
    while got.0.is_none() || got.1.is_none() {
        let m = t.next();
        match m["id"].as_u64() {
            Some(3) => got.0 = Some(m),
            Some(2) if m.get("event").is_none() => got.1 = Some(m),
            _ => {}
        }
    }
    assert_eq!(got.0.unwrap()["result"]["cancelled"], true);
    assert_eq!(code(got.1.as_ref().unwrap()), "cancelled");
    assert!(stopped.elapsed() < Duration::from_secs(10), "it took {:?} to stop", stopped.elapsed());
    // nothing left to cancel
    assert_eq!(t.ask(4, "cancel", json!({ "id": 2 }))["result"]["cancelled"], false);
    assert_eq!(t.ask(5, "ping", json!({}))["result"]["jobs"], 0);
    t.close();
}

#[test]
fn closing_the_pipe_stops_running_jobs_and_ends_the_engine() {
    let lib = library(40, 12);
    let mut t = start(env("eof"));
    t.shake();
    t.send(2, "health.quick", json!({ "folder": lib }));
    assert_eq!(t.next()["event"], "progress");
    let at = Instant::now();
    assert_eq!(t.close(), 0);
    assert!(at.elapsed() < Duration::from_secs(6), "ending took {:?}", at.elapsed());
}

#[test]
fn shutdown_answers_then_ends() {
    let mut t = start(env("shutdown"));
    t.shake();
    assert!(t.ask(2, "shutdown", json!({})).get("result").is_some());
    let Talk { to, from, ended } = t;
    assert_eq!(ended.join().unwrap(), 0);
    drop(to);
    assert!(matches!(from.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected)));
}

#[test]
fn a_request_id_in_use_is_refused() {
    let lib = library(40, 12);
    let mut t = start(env("dup"));
    t.shake();
    t.send(2, "health.quick", json!({ "folder": lib }));
    assert_eq!(t.next()["event"], "progress");
    t.send(2, "ping", json!({}));
    loop {
        let m = t.next();
        if m.get("error").is_some() {
            assert_eq!(code(&m), "bad_request");
            break;
        }
    }
    t.close();
}

// ------------------------------------------------------------------ Get music (protocol 2), against the stand-in

use standin::Share;
use std::path::Path;

impl Talk {
    /// Runs request `id` to its reply. Cadence's asks on the way are answered (a hold gets "h<n>") when
    /// `answer` says so, else left unanswered. Returns the reply, its progress events and the asks seen.
    fn run(&mut self, id: u64, method: &str, params: Value, answer: bool) -> (Value, Vec<Value>, Vec<Value>) {
        self.send(id, method, params);
        self.until(id, answer)
    }
    /// `run` for a request already sent.
    fn until(&mut self, id: u64, answer: bool) -> (Value, Vec<Value>, Vec<Value>) {
        let (mut events, mut asks) = (vec![], vec![]);
        loop {
            let m = self.next();
            if m.get("ask").is_some() {
                if answer {
                    self.answer_ask(&m);
                }
                asks.push(m);
                continue;
            }
            if m["id"] == json!(id) && m.get("event").is_some() {
                events.push(m["data"].clone());
                continue;
            }
            assert_eq!(m["id"], json!(id), "an answer to another request came first: {m}");
            return (m, events, asks);
        }
    }
    fn answer_ask(&mut self, m: &Value) {
        let n = m["ask"].as_u64().unwrap();
        let result = if m["method"] == "library.hold" { json!({ "hold": format!("h{n}") }) } else { json!({}) };
        self.raw(&json!({ "answer": n, "result": result }).to_string());
    }
    /// Sends `cancel` for `target` and reads on until both its answer and the target's own reply are in.
    /// Asks on the way are answered. Returns the target's reply and the asks seen.
    fn cancel(&mut self, cancel_id: u64, target: u64) -> (Value, Vec<Value>) {
        self.send(cancel_id, "cancel", json!({ "id": target }));
        let (mut said, mut reply, mut asks) = (None, None, vec![]);
        while said.is_none() || reply.is_none() {
            let m = self.next();
            if m.get("ask").is_some() {
                self.answer_ask(&m);
                asks.push(m);
            } else if m["id"] == json!(cancel_id) {
                said = Some(m);
            } else if m["id"] == json!(target) && m.get("event").is_none() {
                reply = Some(m);
            }
        }
        assert_eq!(said.unwrap()["result"]["cancelled"], true);
        (reply.unwrap(), asks)
    }
}

/// Short tagged tracks of "Quiet Rooms" by "Test Artist", made with ffmpeg.
fn tracks(kind: &str, how_many: u32) -> Vec<(String, PathBuf)> {
    let dir = scratch("remote");
    let ffmpeg = crate::importer::tools::find_tool("ffmpeg").expect("these tests need ffmpeg");
    (1..=how_many)
        .map(|n| {
            let name = format!("{n:02} Song {n}.{kind}");
            let out = dir.join(&name);
            let mut c = crate::importer::tools::command(&ffmpeg, false);
            c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("sine=frequency={}:duration=2", 300 + n * 100)]);
            if kind == "flac" {
                c.args(["-c:a", "flac"]);
            } else {
                c.args(["-c:a", "libmp3lame", "-b:a", "320k"]);
            }
            c.args(["-metadata", "artist=Test Artist", "-metadata", "album=Quiet Rooms", "-metadata", &format!("title=Song {n}"), "-metadata", &format!("track={n}"), "-metadata", "date=2020"]);
            c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            assert!(crate::importer::tools::run_limited(&mut c, 60), "ffmpeg could not make {name}");
            (name, out)
        })
        .collect()
}

fn share(user: &str, dir: &str, files: Vec<(String, PathBuf)>, free: bool, queue: u64, speed: u64, bitrate: u64) -> Share {
    Share { user: user.into(), dir: dir.into(), files, free, queue, speed, bitrate, refuse: false }
}

/// An engine with a scratch music folder, signed in to its stand-in, which shares one FLAC copy of the album.
fn signed_in(name: &str, tweak: impl FnOnce(&mut Env)) -> (Talk, Arc<standin::StandIn>, Env, PathBuf) {
    let (mut e, s) = soul_env(name);
    tweak(&mut e);
    s.world().shares = vec![share("fast flac", r"Music\Quiet Rooms (2020) [FLAC]", tracks("flac", 3), true, 0, 3_000_000, 0)];
    let lib = scratch("music");
    let mut t = start(e.clone());
    t.shake();
    assert!(t.ask(2, "settings.set", json!({ "changes": { "music_dir": lib } })).get("result").is_some());
    let (r, _, _) = t.run(3, "soulseek.sign_in", json!({ "name": "maker", "password": "pw" }), true);
    assert_eq!(r["result"]["name"], "maker", "{r}");
    (t, s, e, lib)
}

/// The first copy a search finds.
fn first_copy(t: &mut Talk, id: u64) -> String {
    let (r, _, _) = t.run(id, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    r["result"]["copies"][0]["copy"].as_str().unwrap_or_else(|| panic!("no copy: {r}")).to_string()
}

fn files_in(d: &Path) -> Vec<PathBuf> {
    if d.is_dir() { walk(d) } else { vec![] }
}

/// Everything in a folder, folders too.
fn anything_in(d: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(d).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

#[test]
fn soulseek_sign_in_keeps_only_a_login_soulseek_took() {
    let (e, s) = soul_env("signin");
    s.world().accounts.insert("taken".into(), "theirs".into());
    let mut t = start(e.clone());
    t.shake();
    assert_eq!(t.ask(2, "soulseek.status", json!({}))["result"]["state"], "no_login");
    // a name that belongs to someone else, with the wrong password: refused, and nothing kept
    let (r, _, _) = t.run(3, "soulseek.sign_in", json!({ "name": "taken", "password": "mine" }), true);
    assert_eq!(code(&r), "sign_in_refused");
    assert!(r["error"]["message"].as_str().unwrap().contains("did not take that name and password"));
    assert!(crate::secrets::get(&e.parts.login).is_none());
    // a free name makes a new account and is kept
    let (r, _, _) = t.run(4, "soulseek.sign_in", json!({ "name": "new name", "password": "pw" }), true);
    assert_eq!(r["result"]["name"], "new name", "{r}");
    assert_eq!(crate::secrets::get(&e.parts.login).map(|k| (k.user, k.secret)), Some(("new name".to_string(), "pw".to_string())));
    let st = t.ask(5, "soulseek.status", json!({}))["result"].clone();
    assert_eq!((st["state"].as_str(), st["name"].as_str()), (Some("ready"), Some("new name")), "{st}");
    // refused again: the login kept before stays
    let (r, _, _) = t.run(6, "soulseek.sign_in", json!({ "name": "taken", "password": "wrong" }), true);
    assert_eq!(code(&r), "sign_in_refused");
    assert_eq!(crate::secrets::get(&e.parts.login).map(|k| k.user), Some("new name".to_string()));
    // a name and password that cannot be kept at all
    assert_eq!(code(&t.ask(7, "soulseek.sign_in", json!({ "name": " ", "password": "x" }))), "bad_request");
    // signing out forgets the login and stops the client
    assert!(t.ask(8, "soulseek.sign_out", json!({})).get("result").is_some());
    assert!(crate::secrets::get(&e.parts.login).is_none());
    assert!(!s.world().running);
    assert_eq!(t.ask(9, "soulseek.status", json!({}))["result"]["state"], "no_login");
    // signed in again, then the engine ends: the client ends with it
    let (r, _, _) = t.run(10, "soulseek.sign_in", json!({ "name": "new name", "password": "pw" }), true);
    assert!(r.get("result").is_some(), "{r}");
    assert!(s.world().running);
    t.close();
    assert!(!s.world().running, "the client ends with the engine");
}

#[test]
fn a_search_ranks_names_and_filters_copies_and_is_reused() {
    let (mut t, s, _e, _lib) = signed_in("search", |_| {});
    s.world().shares = vec![
        share("fast flac", r"Music\Quiet Rooms (2020) [FLAC]", tracks("flac", 3), true, 0, 3_000_000, 0),
        share("queued mp3", r"Shared\Test Artist - Quiet Rooms (2020) [320]", tracks("mp3", 3), false, 40, 9_000_000, 320),
        share("two songs", r"x\Quiet Rooms", tracks("mp3", 2), true, 0, 1_000_000, 320),
    ];
    let at = Instant::now();
    let (r, events, _) = t.run(10, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    let first_took = at.elapsed();
    let r = &r["result"];
    assert_eq!(r["cached"], false, "{r}");
    let copies = r["copies"].as_array().unwrap();
    let users: Vec<&str> = copies.iter().map(|c| c["user"].as_str().unwrap()).collect();
    assert_eq!(users, ["fast flac", "two songs", "queued mp3"], "{r}");
    // the best is marked, and only it
    assert_eq!(copies.iter().filter(|c| c["best"] == true).count(), 1);
    assert_eq!(copies[0]["best"], true);
    // each has an id of its own
    let ids: Vec<&str> = copies.iter().map(|c| c["copy"].as_str().unwrap()).collect();
    assert!(ids.iter().all(|i| i.starts_with('c')) && ids.len() == 3 && ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2], "{ids:?}");
    // the track list is known: whole, or how many are missing
    assert_eq!((copies[0]["whole"].as_bool(), copies[0]["missing"].as_u64()), (Some(true), Some(0)));
    assert_eq!((copies[1]["whole"].as_bool(), copies[1]["missing"].as_u64()), (Some(false), Some(1)));
    assert_eq!((copies[0]["format"].as_str(), copies[2]["format"].as_str()), (Some("FLAC"), Some("MP3 320")));
    // the copies streamed in as they arrived, each once, with the same ids
    let streamed: Vec<&str> = events.iter().flat_map(|e| e["new"].as_array().unwrap().iter().map(|c| c["copy"].as_str().unwrap())).collect();
    assert!(!streamed.is_empty() && streamed.iter().all(|c| ids.contains(c)), "{events:?}");
    let mut once = streamed.clone();
    once.sort();
    once.dedup();
    assert_eq!(streamed.len(), once.len());
    assert!(events.iter().all(|e| e["copies"].as_u64().is_some()));

    // asked again: from the reuse list, at once, with the same copies
    let at = Instant::now();
    let (again, events, _) = t.run(11, "music.search", json!({ "artist": "test artist", "album": "Quiet Rooms" }), true);
    let again_took = at.elapsed();
    assert_eq!(again["result"]["cached"], true);
    assert_eq!(again["result"]["copies"], r["copies"]);
    assert!(events.is_empty());
    eprintln!("MEASURE search: first {first_took:?}, reused {again_took:?}");
    assert!(again_took * 5 < first_took, "first {first_took:?}, reused {again_took:?}");
    // "fresh" searches again
    let (fresh, _, _) = t.run(12, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms", "fresh": true }), true);
    assert_eq!(fresh["result"]["cached"], false);

    // the filter narrows the copies
    let (flac, _, _) = t.run(13, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms", "filter": "flac" }), true);
    let only: Vec<&str> = flac["result"]["copies"].as_array().unwrap().iter().map(|c| c["user"].as_str().unwrap()).collect();
    assert_eq!(only, ["fast flac"]);
    assert_eq!(flac["result"]["copies"][0]["best"], true);
    // a filter that leaves the best out: nothing is marked
    let (mp3, _, _) = t.run(14, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms", "filter": "mp3" }), true);
    let left = mp3["result"]["copies"].as_array().unwrap();
    assert_eq!(left.len(), 2, "{mp3}");
    assert!(left.iter().all(|c| c["best"] == false && c["format"].as_str().unwrap().starts_with("MP3")), "{mp3}");

    // with prefer_starts_now off, a free slot no longer lifts a copy over a better one
    assert!(t.ask(15, "settings.set", json!({ "changes": { "prefer_starts_now": false } })).get("result").is_some());
    let (r, _, _) = t.run(16, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    assert_eq!(r["result"]["cached"], false, "a change of ranking is not answered from the reuse list");
    assert_eq!(r["result"]["copies"][0]["user"], "queued mp3", "{r}");
    // bad asks
    assert_eq!(code(&t.ask(17, "music.search", json!({}))), "bad_request");
    assert_eq!(code(&t.ask(18, "music.get", json!({ "copy": "c999999" }))), "bad_request");
    t.close();
}

#[test]
fn a_search_without_a_login_or_a_client_says_so() {
    let (e, s) = soul_env("nologin");
    let mut t = start(e.clone());
    t.shake();
    let (r, _, _) = t.run(2, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    assert_eq!(code(&r), "no_login");
    s.world().installed = false;
    let (r, _, _) = t.run(3, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    assert_eq!(code(&r), "no_helper");
    assert_eq!(t.ask(4, "soulseek.status", json!({}))["result"]["state"], "no_helper");
    t.close();
}

#[test]
fn status_warms_the_client_up_so_the_first_search_is_quicker() {
    // a client that takes 1.5 seconds to sign in, as slskd takes a few
    let (e, s) = soul_env("warm");
    s.world().sign_in_takes = Duration::from_millis(1500);
    s.world().shares = vec![share("fast flac", r"Music\Quiet Rooms (2020) [FLAC]", tracks("flac", 1), true, 0, 3_000_000, 0)];
    let mut t = start(e.clone());
    t.shake();
    assert!(t.run(2, "soulseek.sign_in", json!({ "name": "maker", "password": "pw" }), true).0.get("result").is_some());
    t.close();
    // cold: the first search starts the client itself
    let mut t = start(e.clone());
    t.shake();
    let at = Instant::now();
    assert!(t.run(2, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true).0.get("result").is_some());
    let cold = at.elapsed();
    t.close();
    // warm: the page asked for the status when it opened, a moment before the search
    let mut t = start(e.clone());
    t.shake();
    assert_eq!(t.ask(2, "soulseek.status", json!({}))["result"]["state"], "starting");
    std::thread::sleep(Duration::from_millis(1600));
    let at = Instant::now();
    assert!(t.run(3, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }), true).0.get("result").is_some());
    let warm = at.elapsed();
    t.close();
    eprintln!("MEASURE first search: cold {cold:?}, warm {warm:?}");
    assert!(warm + Duration::from_millis(1000) < cold, "cold {cold:?}, warm {warm:?}");
}

#[test]
fn a_copy_is_downloaded_and_saved_between_a_hold_and_its_release() {
    let (mut t, s, e, lib) = signed_in("get", |_| {});
    let copy = first_copy(&mut t, 10);
    let (r, events, asks) = t.run(11, "music.get", json!({ "copy": copy, "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    assert_eq!(r["result"]["result"], "imported", "{r}");
    assert_eq!(r["result"]["words"], "In your library now");
    let folder = PathBuf::from(r["result"]["folder"].as_str().unwrap());
    assert!(folder.starts_with(&lib) && folder.ends_with(r"Test Artist\Quiet Rooms (2020)"), "{}", folder.display());
    // the FLAC copy became 320 MP3s (keep_lossless is off by default)
    let saved: Vec<String> = files_in(&lib).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(saved.len(), 3, "{saved:?}");
    assert!(saved.iter().all(|n| n.ends_with(".mp3")), "{saved:?}");
    // progress through every stage
    let stages: Vec<&str> = events.iter().map(|e| e["stage"].as_str().unwrap()).collect();
    for want in ["waiting", "downloading", "saving"] {
        assert!(stages.contains(&want), "no {want} in {events:?}");
    }
    assert!(events.iter().any(|e| e["stage"] == "saving" && e["words"].as_str().unwrap().starts_with("Saving as 320 MP3")), "{events:?}");
    // the hold came before the first write and was let go after
    let methods: Vec<&str> = asks.iter().map(|a| a["method"].as_str().unwrap()).collect();
    assert_eq!(methods, ["library.hold", "library.release"]);
    assert_eq!(asks[1]["params"]["hold"], format!("h{}", asks[0]["ask"]));
    assert!(asks[0]["params"]["reason"].as_str().unwrap().contains("Quiet Rooms by Test Artist"));
    // the download went to the (test's) Recycle Bin; the client's list is clear; the work folder is the engine's
    assert!(files_in(&e.dir.join("downloads")).is_empty());
    assert_eq!(files_in(&e.dir.join("bin")).len(), 3);
    assert_eq!(s.transfers(), 0);
    assert!(e.dir.join("import-work").is_dir());
    assert!(anything_in(&lib.join("Test Artist")).iter().all(|d| !d.to_string_lossy().ends_with(".importing")));

    // the same album again: it is already there, and nothing is held
    let (r, _, asks) = t.run(12, "music.get", json!({ "copy": copy }), true);
    assert_eq!(r["result"]["result"], "already", "{r}");
    assert!(asks.is_empty());
    assert_eq!(files_in(&lib).len(), 3);
    t.close();
}

#[test]
fn a_download_cancelled_while_it_arrives_leaves_nothing() {
    let (mut t, s, e, lib) = signed_in("cancel-dl", |_| {});
    s.world().per_file = Duration::from_secs(4);
    let copy = first_copy(&mut t, 10);
    t.send(11, "music.get", json!({ "copy": copy }));
    // once the first file is in, stop it
    loop {
        let m = t.next();
        assert!(m.get("ask").is_none(), "nothing may be held while downloading: {m}");
        assert_eq!(m["event"], "progress", "it ended before it could be stopped: {m}");
        if m["data"]["stage"] == "downloading" && m["data"]["done_files"].as_u64() >= Some(1) {
            break;
        }
    }
    assert_eq!(files_in(&e.dir.join("downloads")).len(), 1, "the first file should have arrived");
    let (r, asks) = t.cancel(12, 11);
    assert_eq!(code(&r), "cancelled");
    assert!(asks.is_empty());
    assert_eq!(s.transfers(), 0, "the transfers were cancelled");
    assert!(anything_in(&e.dir.join("downloads")).is_empty(), "what arrived was removed");
    assert!(anything_in(&lib).is_empty());
    t.close();
}

#[test]
fn an_import_cancelled_while_saving_adds_nothing_keeps_the_download_and_a_later_get_carries_on() {
    let gate = Arc::new(Gate::default());
    let held = gate.clone();
    let (mut t, s, e, lib) = signed_in("cancel-save", move |e| e.parts.saving_gate = Some(held));
    let copy = first_copy(&mut t, 10);
    t.send(11, "music.get", json!({ "copy": copy }));
    // the hold is given; then, at the first step of saving (held there by the gate), it is stopped
    loop {
        let m = t.next();
        if m.get("ask").is_some() {
            assert_eq!(m["method"], "library.hold");
            t.answer_ask(&m);
            continue;
        }
        assert_eq!(m["event"], "progress", "it ended before it could be stopped: {m}");
        if m["data"]["stage"] == "saving" && m["data"]["words"].as_str().unwrap().starts_with("Saving as") {
            break;
        }
    }
    t.send(12, "cancel", json!({ "id": 11 }));
    // (the cancel is in once its answer is: then the import may go on, and must stop)
    loop {
        let m = t.next();
        if m["id"] == json!(12) {
            assert_eq!(m["result"]["cancelled"], true);
            break;
        }
    }
    gate.open();
    let (r, _, asks) = t.until(11, true);
    assert_eq!(code(&r), "cancelled", "{r}");
    let methods: Vec<&str> = asks.iter().map(|a| a["method"].as_str().unwrap()).collect();
    assert_eq!(methods, ["library.release"], "the hold is let go after a stop too");
    // no album, no build folder, no artist folder made for it; the download is kept to try again
    assert!(anything_in(&lib).is_empty(), "{:?}", walk(&lib));
    let kept_dir = e.dir.join("downloads").join("Quiet Rooms (2020) [FLAC]");
    assert_eq!(files_in(&kept_dir).len(), 3);
    let kept = t.ask(13, "music.kept", json!({}))["result"]["kept"].clone();
    assert_eq!((kept[0]["reason"].as_str(), kept[0]["user"].as_str(), kept[0]["on_disk"].as_bool()), (Some("stopped"), Some("fast flac"), Some(true)), "{kept}");

    // one file goes missing; getting the copy again asks for that one only, and saves the album
    std::fs::remove_file(kept_dir.join("02 Song 2.flac")).unwrap();
    let asked = s.world().asked;
    let (r, _, asks) = t.run(14, "music.get", json!({ "copy": copy }), true);
    assert_eq!(r["result"]["result"], "imported", "{r}");
    assert_eq!(s.world().asked, asked + 1, "only the missing file is downloaded again");
    assert_eq!(asks.len(), 2);
    assert_eq!(files_in(&lib).len(), 3);
    assert!(t.ask(15, "music.kept", json!({}))["result"]["kept"].as_array().unwrap().is_empty());
    t.close();
}

#[test]
fn a_hold_cadence_never_gives_fails_the_job_and_writes_nothing() {
    let (mut t, _s, _e, lib) = signed_in("no-hold", |e| e.parts.ask_wait = Duration::from_secs(1));
    let copy = first_copy(&mut t, 10);
    let (r, _, asks) = t.run(11, "music.get", json!({ "copy": copy }), false);
    assert_eq!(code(&r), "failed");
    assert_eq!(r["error"]["message"], "Cadence did not let the import start; nothing was changed.");
    let methods: Vec<&str> = asks.iter().map(|a| a["method"].as_str().unwrap()).collect();
    assert_eq!(methods, ["library.hold"], "nothing to release without a hold");
    assert!(anything_in(&lib).is_empty());
    // an answer that comes too late is passed over
    t.raw(&json!({ "answer": asks[0]["ask"], "result": { "hold": "late" } }).to_string());
    assert!(t.ask(12, "ping", json!({})).get("result").is_some());
    t.close();
}

#[test]
fn a_test_run_refuses_soulseek_at_once_and_writes_nothing() {
    let (e, s) = soul_env("offline");
    let e = Env { offline_test: true, ..e };
    let mut t = start(e.clone());
    t.shake();
    assert_eq!(code(&t.ask(2, "soulseek.sign_in", json!({ "name": "maker", "password": "pw" }))), "offline_test");
    assert_eq!(code(&t.ask(3, "soulseek.sign_out", json!({}))), "offline_test");
    assert_eq!(code(&t.ask(4, "music.search", json!({ "artist": "Test Artist", "album": "Quiet Rooms" }))), "offline_test");
    assert_eq!(code(&t.ask(5, "music.get", json!({ "copy": "c1" }))), "offline_test");
    assert_eq!(code(&t.ask(7, "music.put_in", json!({ "user": "u", "folder": "f" }))), "offline_test");
    assert_eq!(code(&t.ask(8, "music.forget", json!({ "user": "u", "folder": "f" }))), "offline_test");
    assert!(t.ask(9, "music.kept", json!({}))["result"]["kept"].as_array().unwrap().is_empty());
    assert!(t.ask(6, "soulseek.status", json!({})).get("result").is_some());
    t.close();
    let w = s.world();
    assert_eq!((w.starts, w.seen.len()), (0, 0), "a test run never started or asked the client anything");
    assert!(anything_in(&e.dir).is_empty(), "{:?}", anything_in(&e.dir));
}

/// A copy with a file that only has a song's name (text inside): the importer doubts it.
fn doubtful_share(s: &standin::StandIn) {
    let mut files = tracks("flac", 3);
    let junk = scratch("junk").join("04 Four.ogg");
    std::fs::write(&junk, "this is not music").unwrap();
    files.push(("04 Four.ogg".into(), junk));
    s.world().shares = vec![share("odd one", r"Music\Quiet Rooms (2020) [FLAC]", files, true, 0, 3_000_000, 0)];
}

#[test]
fn a_doubtful_copy_is_kept_for_a_person_and_can_be_put_in_after_a_restart() {
    let (mut t, s, e, lib) = signed_in("doubtful", |_| {});
    doubtful_share(&s);
    let copy = first_copy(&mut t, 10);
    let (r, _, asks) = t.run(11, "music.get", json!({ "copy": copy, "artist": "Test Artist", "album": "Quiet Rooms" }), true);
    assert_eq!(code(&r), "doubtful", "{r}");
    let words = r["error"]["message"].as_str().unwrap();
    assert!(words.contains("04 Four.ogg") && words.contains("not audio") && words.contains("kept"), "{words}");
    assert!(asks.is_empty(), "nothing was held: nothing was written");
    assert!(anything_in(&lib).is_empty());
    assert_eq!(files_in(&e.dir.join("downloads")).len(), 4, "the download is kept");
    t.close();

    // a new run of the engine: the copy id is gone, the kept download is not
    let mut t = start(e.clone());
    t.shake();
    assert_eq!(code(&t.ask(2, "music.put_in", json!({ "copy": copy }))), "bad_request");
    let kept = t.ask(3, "music.kept", json!({}))["result"]["kept"].clone();
    assert_eq!((kept[0]["reason"].as_str(), kept[0]["files"].as_u64(), kept[0]["album"].as_str()), (Some("doubtful"), Some(4), Some("Quiet Rooms")), "{kept}");
    let named = json!({ "user": kept[0]["user"], "folder": kept[0]["folder"] });
    // put in anyway: the importer's own path, between a hold and its release; the junk stays out
    let (r, _, asks) = t.run(4, "music.put_in", named.clone(), true);
    assert_eq!(r["result"]["result"], "imported", "{r}");
    let methods: Vec<&str> = asks.iter().map(|a| a["method"].as_str().unwrap()).collect();
    assert_eq!(methods, ["library.hold", "library.release"]);
    let saved: Vec<String> = files_in(&lib).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(saved.len(), 3, "{saved:?}");
    assert!(!saved.iter().any(|n| n.contains("Four")));
    assert!(t.ask(5, "music.kept", json!({}))["result"]["kept"].as_array().unwrap().is_empty());
    assert_eq!(code(&t.ask(6, "music.put_in", named)), "bad_request", "nothing is kept now");
    t.close();
}

#[test]
fn a_kept_download_can_be_forgotten_into_the_recycle_bin() {
    let (mut t, s, e, lib) = signed_in("forget", |_| {});
    doubtful_share(&s);
    let copy = first_copy(&mut t, 10);
    assert_eq!(code(&t.run(11, "music.get", json!({ "copy": copy }), true).0), "doubtful");
    let (r, _, _) = t.run(12, "music.forget", json!({ "copy": copy }), true);
    assert_eq!(r["result"]["forgotten"], true, "{r}");
    assert!(anything_in(&e.dir.join("downloads")).is_empty());
    assert_eq!(files_in(&e.dir.join("bin")).len(), 4, "into the (test's) Recycle Bin");
    assert!(t.ask(13, "music.kept", json!({}))["result"]["kept"].as_array().unwrap().is_empty());
    assert_eq!(t.run(14, "music.forget", json!({ "copy": copy }), true).0["result"]["forgotten"], false);
    assert!(anything_in(&lib).is_empty());
    t.close();
}

#[test]
fn an_idle_client_is_stopped_and_started_again_when_needed() {
    let (mut t, s, _e, _lib) = signed_in("idle", |_| {});
    let copy = first_copy(&mut t, 10);
    let set = |t: &mut Talk, id: u64, minutes: f64| assert!(t.ask(id, "settings.set", json!({ "changes": { "soulseek_idle_minutes": minutes } })).get("result").is_some());
    // 0: never stopped
    set(&mut t, 11, 0.0);
    std::thread::sleep(Duration::from_millis(1200));
    assert!(s.world().running);
    // a short wait (0.6 seconds): stopped once nothing has needed it for that long
    set(&mut t, 12, 0.01);
    let at = Instant::now();
    while s.world().running && at.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!s.world().running, "the idle client was not stopped");
    eprintln!("MEASURE idle stop after {:?} (0.6 s asked)", at.elapsed());
    // the next look starts it again
    let starts = s.world().starts;
    let st = t.ask(13, "soulseek.status", json!({}))["result"].clone();
    assert_eq!(st["state"], "starting", "{st}");
    assert!(s.world().running && s.world().starts == starts + 1);
    // a download under way keeps it, however long it takes
    s.world().per_file = Duration::from_millis(900);
    t.send(14, "music.get", json!({ "copy": copy }));
    loop {
        let m = t.next();
        if m.get("ask").is_some() {
            t.answer_ask(&m);
            continue;
        }
        assert!(s.world().running, "stopped while it was needed: {m}");
        if m.get("event").is_none() {
            assert_eq!(m["result"]["result"], "imported", "{m}");
            break;
        }
    }
    t.close();
}
