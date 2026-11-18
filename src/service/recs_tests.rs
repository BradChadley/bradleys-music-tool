//! Recs (protocol 4) driven the way Cadence drives it, against a stand-in of MusicBrainz, Apple Music and
//! Last.fm on this PC (`Web`): the real services are never reached.
use super::*;
use std::io::{BufRead, Read as _};
use std::sync::Mutex;

/// A tiny web server on 127.0.0.1 that answers the few lookups a refresh makes, and writes down every
/// address it was asked for.
struct Web {
    base: String,
    asked: Arc<Mutex<Vec<String>>>,
    /// how long a release lookup takes (to stop a refresh part-way)
    slow: Arc<Mutex<Duration>>,
}

const ARTISTS: [(&str, &str, &str); 3] = [
    ("test artist", "aaaaaaaa-0000-4000-8000-000000000001", "New Light"),
    ("second artist", "aaaaaaaa-0000-4000-8000-000000000002", "Second Wind"),
    ("third artist", "aaaaaaaa-0000-4000-8000-000000000003", "Third Time"),
];

fn decoded(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_lowercase()
}

fn answer(path: &str) -> (u16, String) {
    let q = decoded(path);
    let day = (chrono::Local::now() - chrono::Duration::days(20)).format("%Y-%m-%d").to_string();
    let display = |n: &str| n.split(' ').map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default() }).collect::<Vec<_>>().join(" ");
    if q.starts_with("musicbrainz.org/ws/2/artist?") {
        let found: Vec<Value> = ARTISTS.iter().filter(|(n, _, _)| q.contains(n)).map(|(n, id, _)| json!({ "id": id, "name": display(n), "score": 100 })).collect();
        return (200, json!({ "artists": found }).to_string());
    }
    if q.starts_with("musicbrainz.org/ws/2/release-group?") {
        let groups: Vec<Value> = ARTISTS
            .iter()
            .filter(|(_, id, _)| q.contains(id))
            .map(|(n, id, title)| json!({ "id": format!("rg-{id}"), "title": title, "first-release-date": day, "primary-type": "Album", "secondary-types": [], "artist-credit": [{ "name": display(n), "joinphrase": "", "artist": { "id": id } }] }))
            .collect();
        return (200, json!({ "release-groups": groups }).to_string());
    }
    if q.starts_with("itunes.apple.com/") {
        return (200, json!({ "results": [] }).to_string());
    }
    if q.starts_with("ws.audioscrobbler.com/") {
        if q.contains("method=artist.getsimilar") {
            return (200, json!({ "similarartists": { "artist": [{ "name": "Heavy Heavy", "match": "0.9", "url": "https://www.last.fm/music/Heavy+Heavy", "mbid": "" }] } }).to_string());
        }
        if q.contains("method=artist.getinfo") {
            return (200, json!({ "artist": { "name": "Heavy Heavy", "url": "https://www.last.fm/music/Heavy+Heavy", "stats": { "listeners": "120000" }, "tags": { "tag": [{ "name": "rock" }] } } }).to_string());
        }
        return (200, json!({ "topartists": { "artist": [] }, "recenttracks": { "track": [] } }).to_string());
    }
    (404, "{}".into())
}

impl Web {
    fn start() -> Web {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (asked, slow) = (Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(Duration::ZERO)));
        let (a, s) = (asked.clone(), slow.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (a, s) = (a.clone(), s.clone());
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut first = String::new();
                    if r.read_line(&mut first).is_err() {
                        return;
                    }
                    let mut len = 0usize;
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; len];
                    let _ = r.read_exact(&mut body);
                    let path = first.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                    a.lock().unwrap().push(path.clone());
                    if path.contains("release-group") {
                        let wait = *s.lock().unwrap();
                        std::thread::sleep(wait);
                    }
                    let (status, text) = answer(&path);
                    let mut w = conn;
                    let _ = write!(w, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                });
            }
        });
        Web { base, asked, slow }
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

const KEY: &str = "SECRET-api-key-0123456789";

/// An engine whose lookups go to its own stand-in, with a music folder of three artists.
fn recs_lab(name: &str) -> (Talk, Env, Web, PathBuf) {
    let (mut env, _s) = soul_env(name);
    let web = Web::start();
    env.parts.net = Arc::new(crate::net::Net::stand_in(env.dir.join("cache"), &web.base));
    let lib = scratch(&format!("{name}-lib")).join("Music");
    for (artist, album) in [("Test Artist", "Quiet Rooms (2020)"), ("Second Artist", "First Light (2019)"), ("Third Artist", "Early (2018)")] {
        let d = lib.join(artist).join(album);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("01 Song.mp3"), b"not audio; only the names are read").unwrap();
    }
    let mut t = start(env.clone());
    t.shake();
    (t, env, web, lib)
}

fn plays() -> Value {
    json!([
        { "name": "Test Artist", "plays": 400, "plays_12m": 100, "plays_3m": 30, "plays_recent": 5 },
        { "name": "Second Artist", "plays": 50, "plays_12m": 10, "plays_3m": 2, "plays_recent": 0 },
    ])
}

fn with_account(a: &Value) -> Value {
    if a["method"] == "lastfm.account" { json!({ "result": { "user": "someone", "api_key": KEY } }) } else { json!({ "result": {} }) }
}

fn no_account(a: &Value) -> Value {
    if a["method"] == "lastfm.account" { json!({ "result": { "user": null } }) } else { json!({ "result": {} }) }
}

fn titles(v: &Value) -> Vec<String> {
    v["releases"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap().to_string()).collect()
}

fn walk_all(d: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        if e.path().is_dir() { v.extend(walk_all(&e.path())) } else { v.push(e.path()) }
    }
    v
}

#[test]
fn a_refresh_with_cadences_plays_and_no_last_fm_finds_new_releases_only() {
    let (mut t, env, web, lib) = recs_lab("recs-plain");
    let (r, events, asks) = t.drive(2, "recs.refresh", json!({ "folder": lib, "artists": plays() }), no_account, |_, _| {});
    let r = ok(&r).clone();
    assert_eq!((r["lastfm"].as_bool(), r["similar"].as_u64()), (Some(false), Some(0)), "{r}");
    assert_eq!(r["releases"], 3, "every library artist is checked, with plays or not: {r}");
    assert_eq!(methods(&asks), ["lastfm.account"]);
    // rows streamed as they were found
    let streamed: Vec<&str> = events.iter().flat_map(|e| e["new"].as_array().unwrap().iter().filter_map(|x| x["title"].as_str())).collect();
    assert!(streamed.contains(&"New Light"), "{events:?}");
    assert!(events.iter().all(|e| e["step"] == "releases"), "{events:?}");
    // Last.fm was not asked anything
    assert!(!web.asked().iter().any(|a| a.contains("audioscrobbler")), "{:?}", web.asked());
    let g = ok(&t.ask(3, "recs.get", json!({}))).clone();
    assert_eq!(g["lastfm"], false);
    let first = g["releases"].as_array().unwrap().iter().find(|x| x["title"] == "New Light").unwrap().clone();
    assert_eq!((first["artist"].as_str(), first["kind"].as_str(), first["plays"].as_u64(), first["in_library"].as_bool()), (Some("Test Artist"), Some("album"), Some(400), Some(false)), "{first}");
    assert!(g["generated"].is_string() && g["newest_date"].is_string(), "{g}");
    assert!(env.dir.join("recs.json").is_file());
    t.close();
}

#[test]
fn a_refresh_with_an_account_suggests_artists_and_never_keeps_the_account() {
    let (mut t, env, web, lib) = recs_lab("recs-lastfm");
    let mut lines = vec![];
    let (r, events, _) = t.drive(2, "recs.refresh", json!({ "folder": lib, "artists": plays() }), with_account, |_, e| lines.push(e.to_string()));
    let r = ok(&r).clone();
    assert_eq!(r["lastfm"], true, "{r}");
    assert!(r["similar"].as_u64().unwrap() >= 1, "{r}");
    assert!(events.iter().flat_map(|e| e["new"].as_array().unwrap().iter()).any(|x| x["name"] == "Heavy Heavy"), "artists to try stream in too: {events:?}");
    // Cadence's plays stood in for Last.fm's history: none of its four history requests was made
    let asked = web.asked();
    assert!(asked.iter().any(|a| a.contains("artist.getsimilar")));
    assert!(!asked.iter().any(|a| a.contains("user.gettopartists") || a.contains("user.getrecenttracks")), "{asked:?}");
    let g = ok(&t.ask(3, "recs.get", json!({}))).clone();
    let s = &g["similar"][0];
    assert_eq!((s["name"].as_str(), s["score"].as_f64(), s["listeners"].as_u64()), (Some("Heavy Heavy"), Some(1.0), Some(120000)), "{g}");
    assert!(s["because"].as_array().unwrap().iter().any(|b| b == "Test Artist"), "{s}");
    // the key is in no message and in no file the engine keeps
    lines.push(r.to_string());
    lines.push(g.to_string());
    assert!(lines.iter().all(|l| !l.contains(KEY)));
    for f in walk_all(&env.dir) {
        let b = std::fs::read(&f).unwrap();
        assert!(!String::from_utf8_lossy(&b).contains(KEY), "the key is in {}", f.display());
    }
    // the artist page is answered from what is kept, with nothing asked of the network
    let before = web.asked().len();
    let a = ok(&t.ask(4, "recs.artist", json!({ "name": "Test Artist", "owned": ["Quiet Rooms"] }))).clone();
    assert_eq!((a["new"]["title"].as_str(), a["covered"].as_bool()), (Some("New Light"), Some(true)), "{a}");
    assert_eq!(a["try"][0]["name"], "Heavy Heavy");
    let none = ok(&t.ask(5, "recs.artist", json!({ "name": "Nobody Known" }))).clone();
    assert_eq!((none["new"].is_null(), none["covered"].as_bool()), (true, Some(false)), "{none}");
    assert_eq!(web.asked().len(), before, "recs.artist asked the network");
    t.close();
}

#[test]
fn not_interested_mute_removed_and_owned_albums_are_left_out_or_marked() {
    let (mut t, _env, _web, lib) = recs_lab("recs-filters");
    let removed = json!([{ "artist": "Third Artist", "album": "Third Time" }]);
    ok(&t.drive(2, "recs.refresh", json!({ "folder": lib, "artists": plays(), "removed": removed }), no_account, |_, _| {}).0);
    let g = ok(&t.ask(3, "recs.get", json!({}))).clone();
    let mut got = titles(&g);
    got.sort();
    assert_eq!(got, ["New Light", "Second Wind"], "a removed album is never suggested");
    let key = g["releases"].as_array().unwrap().iter().find(|x| x["title"] == "New Light").unwrap()["key"].clone();
    ok(&t.ask(4, "recs.not_interested", json!({ "key": key, "on": true })));
    ok(&t.ask(5, "recs.mute", json!({ "artist": "Second Artist", "on": true })));
    assert!(titles(ok(&t.ask(6, "recs.get", json!({})))).is_empty());
    let h = ok(&t.ask(7, "recs.hidden", json!({}))).clone();
    assert_eq!(h["not_interested"], json!([{ "key": key, "artist": "Test Artist", "title": "New Light" }]), "{h}");
    assert_eq!(h["muted"], json!(["Second Artist"]));
    ok(&t.ask(8, "recs.not_interested", json!({ "key": key, "on": false })));
    ok(&t.ask(9, "recs.mute", json!({ "artist": "second artist", "on": false })));
    assert_eq!(titles(ok(&t.ask(10, "recs.get", json!({})))).len(), 2);
    let h = ok(&t.ask(11, "recs.hidden", json!({}))).clone();
    assert!(h["not_interested"].as_array().unwrap().is_empty() && h["muted"].as_array().unwrap().is_empty(), "{h}");
    // an album that arrived in the library since: marked, not left out
    let arrived = lib.join("Test Artist").join("New Light (2026)");
    std::fs::create_dir_all(&arrived).unwrap();
    std::fs::write(arrived.join("01 Song.mp3"), b"x").unwrap();
    let g = ok(&t.ask(12, "recs.get", json!({}))).clone();
    let row = g["releases"].as_array().unwrap().iter().find(|x| x["title"] == "New Light").unwrap().clone();
    assert_eq!(row["in_library"], true, "{row}");
    assert_eq!(code(&t.ask(13, "recs.not_interested", json!({ "key": "x" }))), "bad_request");
    t.close();
}

#[test]
fn a_stopped_refresh_keeps_the_rows_it_found() {
    let (mut t, _env, web, lib) = recs_lab("recs-cancel");
    *web.slow.lock().unwrap() = Duration::from_millis(1500);
    let mut sent = false;
    let (r, _, _) = t.drive(2, "recs.refresh", json!({ "folder": lib, "artists": plays() }), no_account, |t, e| {
        if !sent && e["new"].as_array().is_some_and(|n| !n.is_empty()) {
            sent = true;
            t.send(3, "cancel", json!({ "id": 2 }));
        }
    });
    assert_eq!(code(&r), "cancelled", "{r}");
    let got = titles(ok(&t.ask(4, "recs.get", json!({}))));
    assert!(!got.is_empty() && got.len() < 3, "what was found before the stop is kept: {got:?}");
    t.close();
}

#[test]
fn a_test_run_refuses_the_refresh_and_every_change() {
    let (_t, env, _web, lib) = recs_lab("recs-offline");
    let mut t = start(Env { offline_test: true, ..env });
    t.shake();
    assert_eq!(code(&t.ask(2, "recs.refresh", json!({ "folder": lib, "artists": [] }))), "offline_test");
    assert_eq!(code(&t.ask(3, "recs.not_interested", json!({ "key": "a|b", "on": true }))), "offline_test");
    assert_eq!(code(&t.ask(4, "recs.mute", json!({ "artist": "A", "on": true }))), "offline_test");
    ok(&t.ask(5, "recs.get", json!({})));
    ok(&t.ask(6, "recs.hidden", json!({})));
    ok(&t.ask(7, "recs.artist", json!({ "name": "A" })));
    t.close();
}
