//! A stand-in Soulseek for the engine's tests. It never goes near the Soulseek network or the internet:
//!
//!   * a small HTTP server on 127.0.0.1 (a port Windows picks) that answers the requests `slskd::Client`
//!     makes of slskd: the server state, searches (start, state, answers, forget), asking for files, the list
//!     of downloads and cancelling one. Its answers have slskd's shape, so the engine's own client code runs.
//!   * a `Launch` that "starts" it: it takes a name and password the way Soulseek does (a free name makes a
//!     new account, a taken name needs its own password) and can take a while to sign in.
//!
//! What people share is made up by each test (`Share`): real small audio files on this PC. A file asked for
//! waits in line for `queue`, then takes `per_file` to arrive, and is then copied into the downloads folder
//! where slskd would put it (`<downloads>\<remote folder's last name>\<file>`).
use crate::secrets::Entry;
use crate::slskd::{Client, Settings};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One folder one person shares.
#[derive(Clone)]
pub(crate) struct Share {
    pub(crate) user: String,
    /// the folder on their computer ("Music\Quiet Rooms (2020) [FLAC]")
    pub(crate) dir: String,
    /// (file name, the real file on this PC it is made of)
    pub(crate) files: Vec<(String, PathBuf)>,
    pub(crate) free: bool,
    pub(crate) queue: u64,
    pub(crate) speed: u64,
    pub(crate) bitrate: u64,
    /// they turn every request down
    pub(crate) refuse: bool,
}

struct Transfer {
    id: String,
    user: String,
    /// the full remote path
    filename: String,
    size: u64,
    from: PathBuf,
    asked: Instant,
    starts: Duration,
    ends: Duration,
    refuse: bool,
    written: bool,
}

#[derive(Default)]
pub(crate) struct World {
    /// names that are taken, with their passwords
    pub(crate) accounts: HashMap<String, String>,
    pub(crate) installed: bool,
    pub(crate) running: bool,
    started_at: Option<Instant>,
    login: Option<(String, String)>,
    refused: bool,
    /// how long a start takes to be signed in
    pub(crate) sign_in_takes: Duration,
    pub(crate) starts: u32,
    downloads: Option<PathBuf>,
    pub(crate) shares: Vec<Share>,
    /// how long a search takes to end
    pub(crate) search_takes: Duration,
    searches: Vec<(String, Instant)>,
    pub(crate) queue: Duration,
    pub(crate) per_file: Duration,
    /// how many files have been asked for, in all
    pub(crate) asked: usize,
    transfers: Vec<Transfer>,
    next: u64,
    /// every request, "METHOD path", in order
    pub(crate) seen: Vec<String>,
}

pub(crate) struct StandIn {
    pub(crate) port: u16,
    pub(crate) world: Mutex<World>,
}

impl StandIn {
    pub(crate) fn start() -> Arc<StandIn> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a port on this PC for the stand-in");
        let port = listener.local_addr().unwrap().port();
        let me = Arc::new(StandIn {
            port,
            world: Mutex::new(World { installed: true, search_takes: Duration::from_millis(250), queue: Duration::from_millis(300), per_file: Duration::from_millis(300), ..World::default() }),
        });
        let weak = Arc::downgrade(&me);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Some(me) = weak.upgrade() else { break };
                let Ok(conn) = conn else { continue };
                std::thread::spawn(move || me.serve(conn));
            }
        });
        me
    }

    pub(crate) fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn serve(&self, mut conn: std::net::TcpStream) {
        let mut r = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        if r.read_line(&mut line).is_err() {
            return;
        }
        let mut length = 0usize;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        let _ = r.read_exact(&mut body);
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
        let (path, query) = target.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target.clone(), String::new()));
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let (status, answer) = self.answer(&method, &path, &query, body);
        let text = answer.to_string();
        let _ = write!(conn, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        let _ = conn.flush();
    }

    fn answer(&self, method: &str, path: &str, query: &str, body: Value) -> (u16, Value) {
        let mut w = self.world();
        w.seen.push(format!("{method} {path}"));
        if !w.running {
            return (503, json!("not running"));
        }
        let segs: Vec<String> = path.trim_start_matches("/api/v0/").split('/').map(decode).collect();
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        match (method, segs.as_slice()) {
            ("GET", ["server"]) => {
                let on = w.login.is_some() && !w.refused && w.started_at.is_some_and(|t| t.elapsed() >= w.sign_in_takes);
                (200, json!({ "isConnected": on, "isLoggedIn": on, "state": if on { "Connected, LoggedIn" } else if w.refused { "Disconnected" } else { "Connecting" } }))
            }
            ("PUT", ["server"]) => (200, json!({})),
            ("POST", ["searches"]) => {
                let id = body["id"].as_str().unwrap_or_default().to_string();
                w.searches.push((id, Instant::now()));
                (200, json!({}))
            }
            ("GET", ["searches"]) => (200, Value::Array(w.searches.iter().map(|(id, _)| json!({ "id": id })).collect())),
            ("GET", ["searches", id]) => {
                let Some(&(_, at)) = w.searches.iter().find(|(s, _)| s == id) else { return (404, json!("no such search")) };
                let done = at.elapsed() >= w.search_takes;
                let mut v = json!({ "id": id, "isComplete": done, "state": if done { "Completed, TimedOut" } else { "InProgress" }, "responseCount": if done { w.shares.len() } else { 0 } });
                if query.contains("includeResponses=true") {
                    v["responses"] = Value::Array(if done { w.shares.iter().map(response).collect() } else { vec![] });
                }
                (200, v)
            }
            ("PUT", ["searches", _]) => (200, json!({})),
            ("DELETE", ["searches", id]) => {
                w.searches.retain(|(s, _)| s != id);
                (204, json!({}))
            }
            ("POST", ["transfers", "downloads", user]) => {
                let Some(share) = w.shares.iter().find(|s| s.user == *user).cloned() else { return (404, json!("nobody of that name")) };
                let mut enqueued = vec![];
                let (queue, per_file) = (w.queue, w.per_file);
                for (n, f) in body.as_array().cloned().unwrap_or_default().iter().enumerate() {
                    let filename = f["filename"].as_str().unwrap_or_default().to_string();
                    let leaf = filename.rsplit('\\').next().unwrap_or_default().to_string();
                    let Some((_, from)) = share.files.iter().find(|(name, _)| *name == leaf) else { continue };
                    w.next += 1;
                    let id = format!("t{}", w.next);
                    let size = std::fs::metadata(from).map(|m| m.len()).unwrap_or(0);
                    let t = Transfer { id, user: user.to_string(), filename: filename.clone(), size, from: from.clone(), asked: Instant::now(), starts: queue + per_file * n as u32, ends: queue + per_file * (n as u32 + 1), refuse: share.refuse, written: false };
                    w.transfers.push(t);
                    enqueued.push(json!({ "filename": filename }));
                }
                w.asked += enqueued.len();
                (201, json!({ "enqueued": enqueued, "failed": [] }))
            }
            ("GET", ["transfers", "downloads"]) => {
                let downloads = w.downloads.clone();
                let mut by_user: Vec<(String, Vec<(String, Vec<Value>)>)> = vec![];
                for t in w.transfers.iter_mut() {
                    let at = t.asked.elapsed();
                    let (state, done, place) = if at < t.starts {
                        ("Queued, Remote", 0, 3)
                    } else if t.refuse {
                        ("Completed, Rejected", 0, 0)
                    } else if at < t.ends {
                        let part = (at - t.starts).as_secs_f64() / (t.ends - t.starts).as_secs_f64();
                        ("InProgress", (t.size as f64 * part) as u64, 0)
                    } else {
                        if !t.written {
                            if let Some(dl) = &downloads {
                                let (dir, leaf) = t.filename.rsplit_once('\\').unwrap_or(("", &t.filename));
                                let last = dir.rsplit('\\').next().unwrap_or(dir);
                                let to = dl.join(last).join(leaf);
                                let _ = std::fs::create_dir_all(to.parent().unwrap());
                                let _ = std::fs::copy(&t.from, &to);
                            }
                            t.written = true;
                        }
                        ("Completed, Succeeded", t.size, 0)
                    };
                    let dir = t.filename.rsplit_once('\\').map(|(d, _)| d.to_string()).unwrap_or_default();
                    let file = json!({ "id": t.id, "filename": t.filename, "size": t.size, "bytesTransferred": done, "averageSpeed": 1_000_000.0, "state": state, "placeInQueue": place });
                    let u = match by_user.iter().position(|(u, _)| *u == t.user) {
                        Some(i) => i,
                        None => {
                            by_user.push((t.user.clone(), vec![]));
                            by_user.len() - 1
                        }
                    };
                    match by_user[u].1.iter().position(|(d, _)| *d == dir) {
                        Some(i) => by_user[u].1[i].1.push(file),
                        None => by_user[u].1.push((dir, vec![file])),
                    }
                }
                let v: Vec<Value> = by_user.into_iter().map(|(u, dirs)| json!({ "username": u, "directories": dirs.into_iter().map(|(d, f)| json!({ "directory": d, "files": f })).collect::<Vec<_>>() })).collect();
                (200, Value::Array(v))
            }
            ("DELETE", ["transfers", "downloads", "all", "completed"]) => {
                w.transfers.retain(|t| t.asked.elapsed() < t.ends);
                (204, json!({}))
            }
            ("DELETE", ["transfers", "downloads", user, id]) => {
                w.transfers.retain(|t| !(t.user == *user && t.id == *id));
                (204, json!({}))
            }
            _ => (404, json!("the stand-in does not answer that")),
        }
    }

    /// The files of every transfer still on the list.
    pub(crate) fn transfers(&self) -> usize {
        self.world().transfers.len()
    }
}

/// A share as one search answer, in slskd's shape.
fn response(s: &Share) -> Value {
    let files: Vec<Value> = s
        .files
        .iter()
        .map(|(name, from)| json!({ "filename": format!("{}\\{name}", s.dir), "size": std::fs::metadata(from).map(|m| m.len()).unwrap_or(1000), "bitRate": s.bitrate, "length": 2 }))
        .collect();
    json!({ "username": s.user, "hasFreeUploadSlot": s.free, "uploadSpeed": s.speed, "queueLength": s.queue, "files": files })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(n) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl super::soulseek::Launch for StandIn {
    fn installed(&self) -> bool {
        self.world().installed
    }
    fn start(&self, login: &Entry, downloads: &Path) -> Result<Client, String> {
        let mut w = self.world();
        w.running = true;
        w.starts += 1;
        w.started_at = Some(Instant::now());
        w.downloads = Some(downloads.to_path_buf());
        let _ = std::fs::create_dir_all(downloads);
        w.refused = w.accounts.get(&login.user).is_some_and(|p| *p != login.secret);
        if !w.refused {
            // (a free name: Soulseek makes it a new account with this password)
            w.accounts.insert(login.user.clone(), login.secret.clone());
        }
        w.login = Some((login.user.clone(), login.secret.clone()));
        let set = Settings { port: self.port, listen_port: 0, key: "stand-in".into(), has_login: true, login_mark: 0 };
        Ok(Client::at(&set, self.port))
    }
    fn stop(&self) {
        let mut w = self.world();
        w.running = false;
        w.login = None;
        w.refused = false;
    }
    fn running(&self) -> bool {
        self.world().running
    }
    fn refused(&self) -> bool {
        self.world().refused
    }
    fn why_stopped(&self) -> Option<String> {
        None
    }
}
