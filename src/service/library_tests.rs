//! Protocol 3 driven the way Cadence drives it: Health's report, repairs and Fix all, Repair history and
//! Undo, Edit details, the library guard and the asks around every change. Each test has a library of its
//! own in a temp folder, made of short real MP3s (ffmpeg), and a report written the way the check writes it,
//! so the findings are known before a repair is asked for.
use super::*;

/// Protocol 4: Recs.
#[path = "recs_tests.rs"]
mod recs_tests;

/// Protocol 5: Backup.
#[path = "backup_tests.rs"]
mod backup_tests;

/// A test library: `<scratch>\Music\Test Artist\Quiet Rooms (2020)` with two songs, a third song lying loose
/// in the artist's folder, and two empty folders. The backups go beside `Music` (a test run's place).
struct Lab {
    env: Env,
    lib: PathBuf,
    artist: PathBuf,
    album: PathBuf,
    loose: PathBuf,
    empties: [PathBuf; 2],
}

fn lab(name: &str, tweak: impl FnOnce(&mut Env)) -> Lab {
    let (mut env, _s) = soul_env(name);
    tweak(&mut env);
    let lib = scratch(&format!("{name}-lib")).join("Music");
    let artist = lib.join("Test Artist");
    let album = artist.join("Quiet Rooms (2020)");
    std::fs::create_dir_all(&album).unwrap();
    let made = tracks("mp3", 3);
    for (n, (file, from)) in made.iter().enumerate() {
        let to = if n < 2 { album.join(file) } else { artist.join(file) };
        std::fs::copy(from, to).unwrap();
    }
    let empties = [artist.join("Empty"), artist.join("Empty Too")];
    for e in &empties {
        std::fs::create_dir_all(e).unwrap();
    }
    let loose = artist.join("03 Song 3.mp3");
    let l = Lab { env, lib, artist, album, loose, empties };
    l.write_report();
    l
}

impl Lab {
    /// The report the check would write about this library: the loose song (worth fixing) and the two empty
    /// folders (minor).
    fn write_report(&self) {
        let issue = |s: u8, c: &str, l: &str, p: &Path, k: &str, f: &[&Path]| {
            json!({ "S": s, "C": c, "A": "Test Artist", "L": l, "P": p, "D": format!("{c} here"), "K": k, "F": f, "X": {} })
        };
        let report = json!({
            "When": chrono::Local::now().to_rfc3339(), "By": "music-recs", "Deep": false, "Files": 3, "Albums": 1, "Artists": 1,
            "Issues": [
                issue(2, "Loose tracks", "(loose tracks)", &self.artist, "file-loose", &[&self.loose]),
                issue(3, "Empty folder", "Empty", &self.empties[0], "remove", &[&self.empties[0]]),
                issue(3, "Empty folder", "Empty Too", &self.empties[1], "remove", &[&self.empties[1]]),
            ],
        });
        std::fs::write(library::report_path(&self.env, &self.lib), report.to_string()).unwrap();
    }
    fn key(&self, cat: &str, p: &Path) -> String {
        format!("{cat}|{}", p.to_string_lossy().to_lowercase())
    }
    fn loose_key(&self) -> String {
        self.key("Loose tracks", &self.artist)
    }
    fn filed(&self) -> PathBuf {
        self.album.join("03 Song 3.mp3")
    }
    fn backups(&self) -> PathBuf {
        self.lib.parent().unwrap().join(".music-repair-backups")
    }
}

/// Cadence's side of the asks: what it answers to each (`{"result": ...}` or `{"error": ...}`).
fn usual(a: &Value) -> Value {
    match a["method"].as_str().unwrap() {
        "library.hold" => json!({ "result": { "hold": format!("h{}", a["ask"]) } }),
        "library.playing" => json!({ "result": { "path": null } }),
        "library.moved" => json!({ "result": { "remapped": a["params"]["moves"].as_array().map(Vec::len).unwrap_or(0) } }),
        _ => json!({ "result": {} }),
    }
}

impl Talk {
    /// Runs request `id` to its reply, answering Cadence's asks with `answer` and showing each progress
    /// event to `seen` (which may act: send a cancel, take the folder away). Replies to other requests on
    /// the way are passed over. Returns the reply, the progress events and the asks.
    fn drive(&mut self, id: u64, method: &str, params: Value, mut answer: impl FnMut(&Value) -> Value, mut seen: impl FnMut(&mut Talk, &Value)) -> (Value, Vec<Value>, Vec<Value>) {
        self.send(id, method, params);
        let (mut events, mut asks) = (vec![], vec![]);
        loop {
            let m = self.next();
            if let Some(n) = m.get("ask") {
                let mut a = answer(&m);
                a["answer"] = n.clone();
                self.raw(&a.to_string());
                asks.push(m);
            } else if m["id"] == json!(id) && m.get("event").is_some() {
                seen(self, &m["data"]);
                events.push(m["data"].clone());
            } else if m["id"] == json!(id) {
                return (m, events, asks);
            }
        }
    }
    fn plain(&mut self, id: u64, method: &str, params: Value) -> (Value, Vec<Value>, Vec<Value>) {
        self.drive(id, method, params, usual, |_, _| {})
    }
}

fn methods(asks: &[Value]) -> Vec<&str> {
    asks.iter().map(|a| a["method"].as_str().unwrap()).collect()
}

fn ok(r: &Value) -> &Value {
    assert!(r.get("result").is_some(), "expected a result, got {r}");
    &r["result"]
}

fn title_of(f: &Path) -> String {
    let ffprobe = crate::importer::tools::find_tool("ffprobe").unwrap();
    crate::healthscan::probe(&ffprobe, f).tags.get("title").cloned().unwrap_or_default()
}

#[test]
fn the_report_comes_grouped_by_album_with_its_fixes_and_ignored_issues_marked() {
    let l = lab("report", |_| {});
    let mut t = start(l.env.clone());
    assert_eq!(t.shake()["protocol"], PROTOCOL);
    let r = ok(&t.ask(2, "health.report", json!({ "folder": l.lib }))).clone();
    assert_eq!((r["library_changes"].as_str(), r["files"].as_u64(), r["deep"].as_bool()), (Some("organize"), Some(3), Some(false)), "{r}");
    assert!(r["when"].is_string() && r["updated"].is_null());
    let albums = r["albums_with_issues"].as_array().unwrap();
    assert_eq!(albums.len(), 1, "{r}");
    let i = &albums[0]["issues"][0];
    assert_eq!(i["key"], l.loose_key());
    assert_eq!((i["severity"].as_u64(), i["fix"].as_str(), i["ignored"].as_bool(), i["fix_all"].as_bool()), (Some(2), Some("file-loose"), Some(false), Some(false)));
    assert_eq!(i["fixes"], json!([{ "words": "File it in its album folder", "choice": "" }]));
    // the minor ones, by category, with the words for Fix all
    let small = r["small_things"].as_array().unwrap();
    assert_eq!(small.len(), 1, "{r}");
    assert_eq!((small[0]["category"].as_str(), small[0]["albums"].as_u64(), small[0]["keys"].as_array().unwrap().len()), (Some("Empty folder"), Some(2), 2));
    assert!(small[0]["fix_all_words"].is_string(), "{r}");

    // ignored, then brought back
    assert!(ok(&t.ask(3, "health.ignore", json!({ "keys": [l.loose_key()], "ignored": true }))).is_object());
    let r = ok(&t.ask(4, "health.report", json!({ "folder": l.lib }))).clone();
    assert_eq!(r["albums_with_issues"][0]["issues"][0]["ignored"], true, "{r}");
    assert!(std::fs::read_to_string(l.env.dir.join("ignored-issues.txt")).unwrap().contains(&l.loose_key()));
    ok(&t.ask(5, "health.ignore", json!({ "keys": [l.loose_key()], "ignored": false })));
    assert_eq!(ok(&t.ask(6, "health.report", json!({ "folder": l.lib })))["albums_with_issues"][0]["issues"][0]["ignored"], false);
    assert_eq!(code(&t.ask(7, "health.ignore", json!({ "keys": ["no bar"], "ignored": true }))), "bad_request");

    // no report yet: an empty answer, not an error
    let none = ok(&t.ask(8, "health.report", json!({ "folder": l.lib.join("Elsewhere") }))).clone();
    assert!(none["when"].is_null() && none["albums_with_issues"].as_array().unwrap().is_empty(), "{none}");
    // replace.want writes the album down in the engine's own list
    ok(&t.ask(9, "replace.want", json!({ "folder": l.album, "artist": "Test Artist", "album": "Quiet Rooms", "why": "Cut off", "files": [] })));
    assert_eq!(crate::replace::all(&l.env.dir.join("replacements.json")).len(), 1);
    // the plan says what would happen, and changes nothing
    let p = ok(&t.ask(10, "repair.plan", json!({ "folder": l.lib, "keys": [l.loose_key()] }))).clone();
    assert_eq!(p["steps"], json!(["Move 03 Song 3.mp3 to ".to_string() + &l.album.to_string_lossy()]), "{p}");
    assert!(l.loose.is_file());
    t.close();
}

#[test]
fn a_repair_that_moves_tells_cadence_then_history_lists_it_and_undo_moves_it_back() {
    let l = lab("move", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let (r, events, asks) = t.plain(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()], "title": "File loose songs of Test Artist" }));
    let r = ok(&r).clone();
    assert_eq!((r["changed"].as_u64(), r["problems"].as_array().unwrap().len()), (Some(1), 0), "{r}");
    assert!(!l.loose.exists() && l.filed().is_file());
    // the hold, the playing file, the moves, then the release, in that order
    assert_eq!(methods(&asks), ["library.hold", "library.playing", "library.moved", "library.release"]);
    let moved = &asks[2]["params"];
    assert_eq!(moved["batch"], r["batch"]);
    assert_eq!(moved["moves"], json!([{ "from": l.loose, "to": l.filed(), "folder": false }]));
    assert!(events.iter().any(|e| e["total"] == 1), "{events:?}");
    // Backup hears of it too; Cadence took it, so nothing waits for its next start
    assert!(std::fs::read_to_string(l.env.dir.join("backup-moves.jsonl")).unwrap().contains(r["batch"].as_str().unwrap()));
    assert!(!l.env.dir.join("moves-unacknowledged.jsonl").exists());
    // the report was brought up to date for the artist's folder: the loose song is no longer a finding
    let rep = ok(&t.ask(3, "health.report", json!({ "folder": l.lib }))).clone();
    assert!(rep["updated"].is_string(), "{rep}");
    assert!(!rep.to_string().contains("Loose tracks"), "{rep}");

    let h = ok(&t.ask(4, "history.list", json!({ "folder": l.lib }))).clone();
    let first = &h["repairs"][0];
    assert_eq!((first["id"].as_str(), first["kind"].as_str(), first["undone"].as_bool(), first["title"].as_str()), (r["batch"].as_str(), Some("repair"), Some(false), Some("File loose songs of Test Artist")), "{h}");
    assert!(first["folders"].as_array().unwrap().iter().any(|f| f.as_str() == Some(&*l.album.to_string_lossy())), "{h}");
    assert_eq!(h["old"], json!({ "count": 0, "bytes": 0 }));

    let (u, _, asks) = t.plain(5, "history.undo", json!({ "folder": l.lib, "id": r["batch"] }));
    assert_eq!(ok(&u)["left"], json!([]), "{u}");
    assert!(l.loose.is_file() && !l.filed().exists());
    assert_eq!(methods(&asks), ["library.hold", "library.playing", "library.moved", "library.release"]);
    assert_eq!(asks[2]["params"]["moves"], json!([{ "from": l.filed(), "to": l.loose, "folder": false }]));
    assert_eq!(ok(&t.ask(6, "history.list", json!({ "folder": l.lib })))["repairs"][0]["undone"], true);
    assert_eq!(code(&t.plain(7, "history.undo", json!({ "folder": l.lib, "id": r["batch"] })).0), "bad_request");
    t.close();
}

#[test]
fn fix_all_is_one_batch_with_one_undo() {
    let l = lab("fixall", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let keys: Vec<String> = l.empties.iter().map(|e| l.key("Empty folder", e)).collect();
    let (r, events, asks) = t.plain(2, "repair.apply", json!({ "folder": l.lib, "keys": keys }));
    let r = ok(&r).clone();
    assert_eq!(r["changed"], 2, "{r}");
    assert!(l.empties.iter().all(|e| !e.exists()));
    // nothing moved inside the library (the folders went into the backups), so Cadence is not asked to remap
    assert_eq!(methods(&asks), ["library.hold", "library.playing", "library.playing", "library.release"]);
    assert!(events.iter().any(|e| e["done"] == 1 && e["total"] == 2), "{events:?}");
    let h = ok(&t.ask(3, "history.list", json!({ "folder": l.lib }))).clone();
    assert_eq!(h["repairs"].as_array().unwrap().len(), 1, "{h}");
    assert!(h["repairs"][0]["title"].as_str().unwrap().starts_with("Fix all: Empty folder"));
    ok(&t.plain(4, "history.undo", json!({ "folder": l.lib, "id": r["batch"] })).0);
    assert!(l.empties.iter().all(|e| e.is_dir()));
    // a Fix all of different kinds, or of a kind that is looked at one by one, is refused
    assert_eq!(code(&t.plain(5, "repair.apply", json!({ "folder": l.lib, "keys": [keys[0], l.loose_key()] })).0), "bad_request");
    assert_eq!(code(&t.plain(6, "repair.apply", json!({ "folder": l.lib, "keys": ["Empty folder|x:\\not\\there"] })).0), "bad_request");
    t.close();
}

#[test]
fn moves_cadence_cannot_take_wait_for_its_next_start() {
    let l = lab("unack", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let refuse = |a: &Value| if a["method"] == "library.moved" { json!({ "error": { "code": "failed", "message": "the database is locked" } }) } else { usual(a) };
    let (r, _, asks) = t.drive(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }), refuse, |_, _| {});
    let batch = ok(&r)["batch"].clone();
    assert!(l.filed().is_file(), "the move stays done");
    assert_eq!(methods(&asks).last(), Some(&"library.release"));
    let un = ok(&t.ask(3, "library.unacknowledged", json!({}))).clone();
    assert_eq!(un["batches"].as_array().unwrap().len(), 1, "{un}");
    assert_eq!((&un["batches"][0]["batch"], &un["batches"][0]["moves"][0]["to"]), (&batch, &json!(l.filed())));
    assert_eq!(ok(&t.ask(4, "library.acknowledge", json!({ "batch": batch })))["acknowledged"], true);
    assert!(ok(&t.ask(5, "library.unacknowledged", json!({})))["batches"].as_array().unwrap().is_empty());
    assert_eq!(ok(&t.ask(6, "library.acknowledge", json!({ "batch": batch })))["acknowledged"], false);
    t.close();
}

#[test]
fn edit_details_is_one_edit_batch_leaves_read_only_files_alone_and_undoes() {
    let l = lab("edit", |_| {});
    let (one, two) = (l.album.join("01 Song 1.mp3"), l.album.join("02 Song 2.mp3"));
    let mut perm = std::fs::metadata(&two).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&two, perm.clone()).unwrap();
    let mut t = start(l.env.clone());
    t.shake();
    let files = json!([{ "path": one, "set": { "title": "A New Name", "genre": "" } }, { "path": two, "set": { "title": "Another" } }]);
    let (r, _, asks) = t.plain(2, "tags.edit", json!({ "folder": l.lib, "title": "Edit details: Quiet Rooms", "files": files }));
    let r = ok(&r).clone();
    assert_eq!(r["changed"], 1, "{r}");
    assert_eq!(r["problems"], json!([{ "path": two, "words": "Read-only: Cadence leaves read-only files alone." }]));
    assert_eq!((title_of(&one).as_str(), title_of(&two).as_str()), ("A New Name", "Song 2"));
    assert!(std::fs::metadata(&two).unwrap().permissions().readonly(), "the flag is never cleared");
    assert_eq!(methods(&asks), ["library.hold", "library.playing", "library.release"]);
    let h = ok(&t.ask(3, "history.list", json!({ "folder": l.lib }))).clone();
    assert_eq!((h["repairs"][0]["kind"].as_str(), h["repairs"][0]["id"].as_str()), (Some("edit"), r["batch"].as_str()), "{h}");
    ok(&t.plain(4, "history.undo", json!({ "folder": l.lib, "id": r["batch"] })).0);
    assert_eq!(title_of(&one), "Song 1");
    // only the eight fields, and only files in the music folder
    assert_eq!(code(&t.plain(5, "tags.edit", json!({ "folder": l.lib, "files": [{ "path": one, "set": { "lyrics": "x" } }] })).0), "bad_request");
    assert_eq!(code(&t.plain(6, "tags.edit", json!({ "folder": l.album, "files": [{ "path": l.loose, "set": { "title": "x" } }] })).0), "bad_request");
    perm.set_readonly(false);
    std::fs::set_permissions(&two, perm).unwrap();
    t.close();
}

#[test]
fn look_only_turns_repairs_off_but_not_edit_details() {
    let l = lab("look", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    assert_eq!(code(&t.ask(2, "settings.set", json!({ "changes": { "library_changes": "everything" } }))), "bad_request");
    let s = ok(&t.ask(3, "settings.set", json!({ "changes": { "library_changes": "look" } }))).clone();
    assert_eq!((s["settings"]["library_changes"].as_str(), s["settings"]["auto_check_new"].as_bool()), (Some("look"), Some(true)));
    let r = t.plain(4, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] })).0;
    assert_eq!(code(&r), "guarded");
    assert!(r["error"]["message"].as_str().unwrap().starts_with("Look only is on"));
    assert!(l.loose.is_file());
    assert_eq!(ok(&t.ask(5, "health.report", json!({ "folder": l.lib })))["library_changes"], "look");
    let one = l.album.join("01 Song 1.mp3");
    let (r, _, _) = t.plain(6, "tags.edit", json!({ "folder": l.lib, "files": [{ "path": one, "set": { "title": "By Hand" } }] }));
    let batch = ok(&r)["batch"].clone();
    // and its Undo still works: a person made it
    ok(&t.plain(7, "history.undo", json!({ "folder": l.lib, "id": batch })).0);
    assert_eq!(title_of(&one), "Song 1");
    t.close();
}

#[test]
fn the_guard_refuses_a_network_drive_and_a_missing_folder() {
    let l = lab("guard", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let unc = r"\\nowhere.invalid\share\Music";
    let g = ok(&t.ask(2, "library.guard", json!({ "folder": unc }))).clone();
    assert_eq!(g["state"], "network");
    assert!(g["words"].as_str().unwrap().contains("network drive"));
    assert_eq!(ok(&t.ask(3, "library.guard", json!({ "folder": l.lib.join("gone") })))["state"], "missing");
    assert_eq!(ok(&t.ask(4, "library.guard", json!({ "folder": l.lib })))["state"], "ok");
    for (n, method) in [(5, "repair.apply"), (6, "tags.edit"), (7, "history.undo")] {
        let r = t.plain(n, method, json!({ "folder": unc, "keys": ["a|b"], "id": "x", "files": [{ "path": format!(r"{unc}\a.mp3"), "set": { "title": "x" } }] })).0;
        assert_eq!(code(&r), "guarded", "{method}: {r}");
    }
    assert_eq!(code(&t.plain(8, "repair.apply", json!({ "folder": l.lib.join("gone"), "keys": [l.loose_key()] })).0), "not_found");
    t.close();
}

#[test]
fn a_change_to_the_playing_song_waits_until_it_stops_playing() {
    let l = lab("playing", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let loose = l.loose.clone();
    let mut asked = 0;
    let playing = move |a: &Value| {
        if a["method"] == "library.playing" {
            asked += 1;
            return json!({ "result": { "path": if asked <= 3 { json!(loose) } else { Value::Null } } });
        }
        usual(a)
    };
    let at = Instant::now();
    let (r, events, asks) = t.drive(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }), playing, |_, _| {});
    assert_eq!(ok(&r)["changed"], 1, "{r}");
    assert!(events.iter().any(|e| e["words"] == "Waiting for 03 Song 3 to finish playing"), "{events:?}");
    assert_eq!(methods(&asks).iter().filter(|m| **m == "library.playing").count(), 4);
    assert!(at.elapsed() >= Duration::from_millis(600), "it waited {:?}", at.elapsed());
    t.close();
}

#[test]
fn a_wait_for_the_playing_song_can_be_cancelled_and_changes_nothing() {
    let l = lab("playing-cancel", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let loose = l.loose.clone();
    let always = move |a: &Value| if a["method"] == "library.playing" { json!({ "result": { "path": loose } }) } else { usual(a) };
    let mut sent = false;
    let (r, _, asks) = t.drive(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }), always, |t, e| {
        if e["waiting"] == true && !sent {
            sent = true;
            t.send(3, "cancel", json!({ "id": 2 }));
        }
    });
    assert_eq!(code(&r), "cancelled", "{r}");
    assert_eq!(methods(&asks).last(), Some(&"library.release"));
    assert!(l.loose.is_file());
    assert!(ok(&t.ask(4, "history.list", json!({ "folder": l.lib })))["repairs"].as_array().unwrap().is_empty());
    t.close();
}

#[test]
fn the_music_folder_going_away_mid_run_ends_the_job_with_gone() {
    let l = lab("gone", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let loose = l.loose.clone();
    let always = move |a: &Value| if a["method"] == "library.playing" { json!({ "result": { "path": loose } }) } else { usual(a) };
    let (lib, away) = (l.lib.clone(), l.lib.with_file_name("Unplugged"));
    let mut pulled = false;
    let (r, _, asks) = t.drive(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }), always, |_, e| {
        if e["waiting"] == true && !pulled {
            pulled = true;
            std::fs::rename(&lib, &away).unwrap();
        }
    });
    assert_eq!(code(&r), "gone", "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("was the drive unplugged?"));
    assert_eq!(methods(&asks).last(), Some(&"library.release"));
    assert!(away.join("Test Artist").join("03 Song 3.mp3").is_file());
    t.close();
}

#[test]
fn a_test_run_changes_nothing_but_still_reads() {
    let l = lab("offline", |_| {});
    let mut t = start(Env { offline_test: true, ..l.env.clone() });
    t.shake();
    let f = json!({ "folder": l.lib, "keys": [l.loose_key()], "ignored": true, "id": "x", "files": [{ "path": l.loose, "set": { "title": "x" } }] });
    for (n, m) in [(2, "health.ignore"), (3, "replace.want"), (4, "repair.apply"), (5, "history.undo"), (6, "history.clear_old"), (7, "tags.edit")] {
        assert_eq!(code(&t.plain(n, m, f.clone()).0), "offline_test", "{m}");
    }
    ok(&t.ask(8, "health.report", json!({ "folder": l.lib })));
    ok(&t.ask(9, "repair.plan", json!({ "folder": l.lib, "keys": [l.loose_key()] })));
    ok(&t.ask(10, "history.list", json!({ "folder": l.lib })));
    ok(&t.ask(11, "library.guard", json!({ "folder": l.lib })));
    assert!(l.loose.is_file() && !l.backups().exists());
    t.close();
}

#[test]
fn every_new_method_is_in_the_handshake() {
    let mut t = start(env("methods"));
    let r = t.shake();
    for m in ["health.check", "health.quick", "health.report", "health.ignore", "replace.want", "repair.plan", "repair.apply", "history.list", "history.undo", "history.clear_old", "tags.edit", "library.guard", "library.unacknowledged", "library.acknowledge"] {
        assert!(r["methods"].as_array().unwrap().iter().any(|x| x == m), "{m} is missing");
    }
    t.close();
}

#[test]
fn a_check_of_some_top_folders_is_folded_into_the_report() {
    let l = lab("only", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    // a top folder that is not the library's is refused
    assert_eq!(code(&t.ask(2, "health.check", json!({ "folder": l.lib, "only": [l.album] }))), "bad_request");
    let (r, events, _) = t.plain(3, "health.check", json!({ "folder": l.lib, "only": [l.artist] }));
    let r = ok(&r).clone();
    assert_eq!((r["deep"].as_bool(), r["only"].as_u64()), (Some(false), Some(1)), "{r}");
    assert!(!events.is_empty());
    // the artist's folder was looked at again: its findings are what is there now, and the report says when
    let rep = ok(&t.ask(4, "health.report", json!({ "folder": l.lib }))).clone();
    assert!(rep["updated"].is_string(), "{rep}");
    assert!(!rep.to_string().contains("Empty folder here"), "the old findings were replaced: {rep}");
    t.close();
}

#[test]
fn old_backups_go_to_the_recycle_bin_and_only_they() {
    let l = lab("clear", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let (r, _, _) = t.plain(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.key("Empty folder", &l.empties[0])] }));
    let fresh = ok(&r)["batch"].as_str().unwrap().to_string();
    // a repair from two months ago
    let old = l.backups().join("20200101-000000-abcdef");
    std::fs::create_dir_all(old.join("items")).unwrap();
    let when = (chrono::Local::now() - chrono::Duration::days(60)).to_rfc3339();
    std::fs::write(old.join("batch.json"), json!({ "Id": "20200101-000000-abcdef", "Title": "An old repair", "When": when, "Library": l.lib }).to_string()).unwrap();
    std::fs::write(old.join("log.jsonl"), "{\"Kind\":\"madedir\",\"Path\":\"x\"}\n").unwrap();
    let h = ok(&t.ask(3, "history.list", json!({ "folder": l.lib }))).clone();
    assert_eq!(h["old"]["count"], 1, "{h}");
    assert!(h["old"]["bytes"].as_u64().unwrap() > 0);
    let (c, _, _) = t.plain(4, "history.clear_old", json!({ "folder": l.lib }));
    assert_eq!(ok(&c)["cleared"], 1, "{c}");
    assert!(!old.exists() && l.backups().join(&fresh).is_dir());
    t.close();
}
// ------------------------------------------------------------------ Organize, through the engine

/// A library with one album in a folder that does not follow Artist \ Album (Year).
fn messy(name: &str) -> (Env, PathBuf, PathBuf, PathBuf) {
    let (env, _s) = soul_env(name);
    let lib = scratch(&format!("{name}-lib")).join("Music");
    let from = lib.join("Test Artist").join("quiet rooms");
    std::fs::create_dir_all(&from).unwrap();
    for (file, made) in tracks("mp3", 2) {
        std::fs::copy(made, from.join(file)).unwrap();
    }
    let to = lib.join("Test Artist").join("Quiet Rooms (2020)");
    (env, lib, from, to)
}

#[test]
fn organize_is_refused_in_a_test_run_and_unless_the_setting_allows_it() {
    let (env, lib, from, _) = messy("org-refused");
    let mut t = start(env.clone());
    t.shake();
    for (n, setting) in [(2, "look"), (4, "repair")] {
        ok(&t.ask(n, "settings.set", json!({ "changes": { "library_changes": setting } })));
        let r = t.plain(n + 1, "organize.preview", json!({ "folder": lib }));
        assert_eq!(code(&r.0), "guarded", "{setting}: {:?}", r.0);
        assert!(r.0["error"]["message"].as_str().unwrap().contains("Repairs and organizing"));
    }
    ok(&t.ask(6, "settings.set", json!({ "changes": { "library_changes": "organize" } })));
    assert_eq!(code(&t.plain(7, "organize.run", json!({ "folder": lib, "preview": "p-none" })).0), "bad_request");
    assert_eq!(code(&t.plain(8, "organize.preview", json!({ "folder": r"\\nowhere.invalid\share\Music" })).0), "guarded");
    t.close();
    let mut t = start(Env { offline_test: true, ..env });
    t.shake();
    assert_eq!(code(&t.plain(2, "organize.preview", json!({ "folder": lib })).0), "offline_test");
    assert_eq!(code(&t.plain(3, "organize.run", json!({ "folder": lib, "preview": "p-x" })).0), "offline_test");
    assert!(from.is_dir());
    t.close();
}

#[test]
fn an_organize_run_asks_hold_moved_and_release_in_order_and_undo_moves_it_back() {
    let (env, lib, from, to) = messy("org-run");
    let mut t = start(env.clone());
    t.shake();
    ok(&t.ask(2, "settings.set", json!({ "changes": { "library_changes": "organize" } })));
    let (p, _, _) = t.plain(3, "organize.preview", json!({ "folder": lib, "hidden": [] }));
    let p = ok(&p).clone();
    assert_eq!(p["to_move"], 1, "{p}");
    assert_eq!((p["moves"][0]["from"].as_str(), p["moves"][0]["to"].as_str()), (from.to_str(), to.to_str()), "{p}");
    assert!(from.is_dir() && !to.exists(), "a preview changes nothing");
    let (r, events, asks) = t.plain(4, "organize.run", json!({ "folder": lib, "preview": p["preview"] }));
    let r = ok(&r).clone();
    assert_eq!(r["moved"], 1, "{r}");
    assert!(to.is_dir() && !from.exists());
    assert!(events.iter().any(|e| e["total"] == 1), "{events:?}");
    let order: Vec<&str> = methods(&asks).into_iter().filter(|m| *m != "library.playing").collect();
    assert_eq!(order, ["library.hold", "library.moved", "library.release"]);
    let moved = asks.iter().find(|a| a["method"] == "library.moved").unwrap();
    assert_eq!(moved["params"]["moves"], json!([{ "from": from, "to": to, "folder": true }]));
    assert!(std::fs::read_to_string(env.dir.join("backup-moves.jsonl")).unwrap().contains(r["batch"].as_str().unwrap()));
    // the preview is used up
    assert_eq!(code(&t.plain(5, "organize.run", json!({ "folder": lib, "preview": p["preview"] })).0), "bad_request");
    let h = ok(&t.ask(6, "history.list", json!({ "folder": lib }))).clone();
    assert_eq!((h["repairs"][0]["kind"].as_str(), h["repairs"][0]["id"].as_str()), (Some("organize"), r["batch"].as_str()), "{h}");
    assert_eq!(ok(&t.ask(7, "organize.status", json!({ "folder": lib })))["interrupted"], Value::Null);

    let (u, _, asks) = t.plain(8, "history.undo", json!({ "folder": lib, "id": r["batch"] }));
    assert_eq!(ok(&u)["left"], json!([]), "{u}");
    assert!(from.is_dir() && !to.exists());
    let moved = asks.iter().find(|a| a["method"] == "library.moved").expect("the Undo tells Cadence");
    assert_eq!(moved["params"]["moves"], json!([{ "from": to, "to": from, "folder": true }]));
    t.close();
}
// ------------------------------------------------------------------ Keep the library organized

#[test]
fn a_fresh_settings_file_organizes_but_files_no_new_album_until_cadence_says_so() {
    let (env, _s) = soul_env("org-defaults");
    let mut t = start(env);
    t.shake();
    let s = ok(&t.ask(2, "settings.get", json!({}))).clone();
    // (Keep the library organized is off until Cadence sends its own answer: an engine that starts before
    // Cadence has said anything, an upgrade say, files nothing on its own)
    assert_eq!((s["settings"]["library_changes"].as_str(), s["settings"]["keep_organized"].as_bool(), s["settings"]["compilations_folder"].as_str()), (Some("organize"), Some(false), Some("Various Artists")), "{s}");
    t.close();
    // the Music Tool's window reads none of these: its own switch (look_only) is as it was
    let c = crate::config::Config::default();
    assert!(!c.look_only && c.library_changes() == "organize" && !c.keep_organized);
    assert_eq!(crate::config::Config { look_only: true, ..Default::default() }.library_changes(), "look");
}

/// Keep the library organized, on (as Cadence sets it once the person has chosen Organize).
fn keep_organized(t: &mut Talk) {
    ok(&t.ask(90, "settings.set", json!({ "changes": { "keep_organized": true } })));
}

#[test]
fn a_new_album_is_filed_as_one_entry_and_undo_puts_it_back() {
    let (env, lib, from, to) = messy("org-album");
    let mut t = start(env.clone());
    t.shake();
    keep_organized(&mut t);
    let (r, _, asks) = t.plain(2, "organize.album", json!({ "folder": from, "library": lib }));
    let r = ok(&r).clone();
    assert_eq!((r["moved"].as_bool(), r["from"].as_str(), r["to"].as_str()), (Some(true), from.to_str(), to.to_str()), "{r}");
    assert!(to.is_dir() && !from.exists());
    let order: Vec<&str> = methods(&asks).into_iter().filter(|m| *m != "library.playing").collect();
    assert_eq!(order, ["library.hold", "library.moved", "library.release"]);
    let moved = asks.iter().find(|a| a["method"] == "library.moved").unwrap();
    assert_eq!(moved["params"]["moves"], json!([{ "from": from, "to": to, "folder": true }]));
    assert!(std::fs::read_to_string(env.dir.join("backup-moves.jsonl")).unwrap().contains(r["batch"].as_str().unwrap()));
    let h = ok(&t.ask(3, "history.list", json!({ "folder": lib }))).clone();
    assert_eq!((h["repairs"].as_array().unwrap().len(), h["repairs"][0]["kind"].as_str(), h["repairs"][0]["id"].as_str()), (1, Some("organize"), r["batch"].as_str()), "{h}");
    // already in place now: nothing to do
    let (again, _, _) = t.plain(4, "organize.album", json!({ "folder": to, "library": lib }));
    assert_eq!(ok(&again), &json!({ "moved": false }));
    // Undo, with the reverse move told
    let (u, _, asks) = t.plain(5, "history.undo", json!({ "folder": lib, "id": r["batch"] }));
    assert_eq!(ok(&u)["left"], json!([]), "{u}");
    assert!(from.is_dir() && !to.exists());
    let moved = asks.iter().find(|a| a["method"] == "library.moved").expect("the Undo tells Cadence");
    assert_eq!(moved["params"]["moves"], json!([{ "from": to, "to": from, "folder": true }]));
    t.close();
}

#[test]
fn filing_a_new_album_is_refused_unless_organizing_and_keep_organized_are_on() {
    let (env, lib, from, _) = messy("org-album-refused");
    let mut t = start(env.clone());
    t.shake();
    for (n, change) in [(2, json!({ "library_changes": "look" })), (4, json!({ "library_changes": "repair" })), (6, json!({ "library_changes": "organize", "keep_organized": false }))] {
        ok(&t.ask(n, "settings.set", json!({ "changes": change })));
        let r = t.plain(n + 1, "organize.album", json!({ "folder": from, "library": lib }));
        assert_eq!(code(&r.0), "guarded", "{change}: {:?}", r.0);
    }
    ok(&t.ask(8, "settings.set", json!({ "changes": { "keep_organized": true } })));
    // not an album in that music folder
    assert_eq!(code(&t.plain(9, "organize.album", json!({ "folder": lib, "library": lib })).0), "bad_request");
    assert_eq!(code(&t.plain(10, "organize.album", json!({ "folder": r"C:\elsewhere\x", "library": lib })).0), "bad_request");
    t.close();
    let mut t = start(Env { offline_test: true, ..env });
    t.shake();
    assert_eq!(code(&t.plain(2, "organize.album", json!({ "folder": from, "library": lib })).0), "offline_test");
    assert!(from.is_dir());
    t.close();
}

#[test]
fn a_new_album_waits_while_it_plays_and_stays_if_it_still_plays() {
    let (env, lib, from, to) = messy("org-album-playing");
    let song = from.join("01 Song 1.mp3");
    let mut t = start(env.clone());
    t.shake();
    keep_organized(&mut t);
    // stops after a few looks: filed
    let (s, mut asked) = (song.clone(), 0);
    let playing = move |a: &Value| {
        if a["method"] == "library.playing" {
            asked += 1;
            return json!({ "result": { "path": if asked <= 3 { json!(s) } else { Value::Null } } });
        }
        usual(a)
    };
    let (r, events, _) = t.drive(2, "organize.album", json!({ "folder": from, "library": lib }), playing, |_, _| {});
    assert_eq!(ok(&r)["moved"], true, "{r}");
    assert!(events.iter().any(|e| e["words"] == "Waiting for 01 Song 1 to finish playing"), "{events:?}");
    assert!(to.is_dir());
    // back, and now it plays on past the limit: left where it is
    std::fs::rename(&to, &from).unwrap();
    let s = song.clone();
    let always = move |a: &Value| if a["method"] == "library.playing" { json!({ "result": { "path": s } }) } else { usual(a) };
    let at = Instant::now();
    let (r, _, asks) = t.drive(3, "organize.album", json!({ "folder": from, "library": lib }), always, |_, _| {});
    assert_eq!((ok(&r)["moved"].as_bool(), ok(&r)["why"].as_str()), (Some(false), Some("playing")), "{r}");
    assert!(at.elapsed() >= Duration::from_millis(1500));
    assert!(from.is_dir() && !to.exists());
    assert!(!methods(&asks).contains(&"library.moved") && methods(&asks).last() == Some(&"library.release"));
    t.close();
}

#[test]
fn a_new_album_with_a_reason_to_stay_stays_and_says_it() {
    let (env, lib, from, to) = messy("org-album-left");
    std::fs::create_dir_all(&to).unwrap();
    std::fs::write(to.join("notes.txt"), "already here").unwrap();
    let mut t = start(env.clone());
    t.shake();
    keep_organized(&mut t);
    let (r, _, asks) = t.plain(2, "organize.album", json!({ "folder": from, "library": lib }));
    assert_eq!((ok(&r)["moved"].as_bool(), ok(&r)["why"].as_str()), (Some(false), Some("target_exists")), "{r}");
    assert!(!ok(&r)["words"].as_str().unwrap().is_empty());
    assert!(from.is_dir() && !methods(&asks).contains(&"library.moved"));
    let h = ok(&t.ask(3, "history.list", json!({ "folder": lib }))).clone();
    assert_eq!(h["repairs"], json!([]), "{h}");
    t.close();
}

// ------------------------------------------------------------------ review fixes (2026-11-18)

/// A library of `n` albums, each one song in `<lib>\Test Artist\album <k>` tagged as "Album <k>" (2020):
/// each would move to `Test Artist\Album <k> (2020)`. Returns (env, library, [(from, to)]).
fn shelf(name: &str, n: usize, tweak: impl FnOnce(&mut Env)) -> (Env, PathBuf, Vec<(PathBuf, PathBuf)>) {
    let (mut env, _s) = soul_env(name);
    tweak(&mut env);
    let lib = scratch(&format!("{name}-lib")).join("Music");
    let base = &tracks("mp3", 1)[0].1;
    let ffmpeg = crate::importer::tools::find_tool("ffmpeg").unwrap();
    let mut out = vec![];
    for k in 1..=n {
        let from = lib.join("Test Artist").join(format!("album {k}"));
        std::fs::create_dir_all(&from).unwrap();
        let mut c = crate::importer::tools::command(&ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(base).args(["-c", "copy", "-metadata", &format!("album=Album {k}")]).arg(from.join("01 Song 1.mp3"));
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        assert!(crate::importer::tools::run_limited(&mut c, 60));
        out.push((from, lib.join("Test Artist").join(format!("Album {k} (2020)"))));
    }
    (env, lib, out)
}

/// Organizing on, and a preview of the library: its id.
fn previewed(t: &mut Talk, lib: &Path) -> Value {
    ok(&t.ask(80, "settings.set", json!({ "changes": { "library_changes": "organize" } })));
    let (p, _, _) = t.plain(81, "organize.preview", json!({ "folder": lib, "hidden": [] }));
    ok(&p).clone()
}

/// Every move in `library.unacknowledged`, as (from, to).
fn unacknowledged_moves(t: &mut Talk) -> Vec<(PathBuf, PathBuf)> {
    let un = ok(&t.ask(82, "library.unacknowledged", json!({}))).clone();
    un["batches"].as_array().unwrap().iter().flat_map(|b| b["moves"].as_array().unwrap().clone()).map(|m| (PathBuf::from(m["from"].as_str().unwrap()), PathBuf::from(m["to"].as_str().unwrap()))).collect()
}

/// Every move in Backup's list, as (from, to).
fn backup_moves(env: &Env) -> Vec<(PathBuf, PathBuf)> {
    moves::for_backup_list(&env.dir).into_iter().flat_map(|(_, l)| l).map(|m| (m.from, m.to)).collect()
}

#[test]
fn an_organize_run_cut_off_by_a_crash_leaves_every_finished_move_for_cadence_and_backup() {
    let (env, lib, albums) = shelf("org-crash", 4, |_| {});
    let mut t = start(env.clone());
    t.shake();
    let p = previewed(&mut t, &lib);
    assert_eq!(p["to_move"], 4, "{p}");
    // the program dies right after the second album is wholly moved
    crate::organize::crash_at::set(&lib, "after_album", 1);
    let (r, _, asks) = t.plain(2, "organize.run", json!({ "folder": lib, "preview": p["preview"] }));
    assert_eq!(code(&r), "failed", "{r}");
    assert!(!methods(&asks).contains(&"library.moved"), "nothing was told before the crash: {asks:?}");
    let moved: Vec<(PathBuf, PathBuf)> = albums.iter().filter(|(_, to)| to.is_dir()).cloned().collect();
    assert_eq!(moved.len(), 2);
    // both lists have them already, so Cadence's next start and the next backup follow them
    let mut un = unacknowledged_moves(&mut t);
    un.sort();
    let mut bk = backup_moves(&env);
    bk.sort();
    assert_eq!((un, bk), (moved.clone(), moved.clone()));
    t.close();

    // a power cut also tore the batch's record: the next start writes it again from its log, so it is in
    // Repair history with its Undo
    let batch = std::fs::read_dir(lib.parent().unwrap().join(".music-repair-backups")).unwrap().flatten().next().unwrap().path();
    std::fs::write(batch.join("batch.json"), "{\"Id\": \"20").unwrap();
    let mut t = start(env.clone());
    t.shake();
    assert!(ok(&t.ask(3, "history.list", json!({ "folder": lib })))["repairs"].as_array().unwrap().is_empty(), "torn: not listed yet");
    ok(&t.ask(4, "settings.set", json!({ "changes": { "music_dir": lib } })));
    t.close();
    let mut t = start(env.clone());
    t.shake();
    let h = ok(&t.ask(5, "history.list", json!({ "folder": lib }))).clone();
    assert_eq!((h["repairs"].as_array().unwrap().len(), h["repairs"][0]["kind"].as_str(), h["repairs"][0]["changes"].as_u64()), (1, Some("organize"), Some(2)), "{h}");
    let (u, _, _) = t.plain(6, "history.undo", json!({ "folder": lib, "id": h["repairs"][0]["id"] }));
    assert_eq!(ok(&u)["left"], json!([]), "{u}");
    assert!(albums.iter().all(|(from, to)| from.is_dir() && !to.exists()));
    t.close();
}

#[test]
fn an_organize_run_cut_off_in_the_middle_of_its_temporary_hop_is_renamed_back_at_the_next_start() {
    // an album lying at the top of the library, in a folder named like its artist: it moves into a folder of
    // that very name, so it goes through a temporary name
    let (env, _s) = soul_env("org-hop");
    let lib = scratch("org-hop-lib").join("Music");
    let from = lib.join("Test Artist");
    std::fs::create_dir_all(&from).unwrap();
    for (file, made) in tracks("mp3", 2) {
        std::fs::copy(made, from.join(file)).unwrap();
    }
    let mut t = start(env.clone());
    t.shake();
    let p = previewed(&mut t, &lib);
    assert_eq!(p["moves"][0]["to"].as_str(), from.join("Quiet Rooms (2020)").to_str(), "{p}");
    crate::organize::crash_at::set(&lib, "mid_hop", 0);
    assert_eq!(code(&t.plain(2, "organize.run", json!({ "folder": lib, "preview": p["preview"] })).0), "failed");
    let hidden: Vec<PathBuf> = std::fs::read_dir(&lib).unwrap().flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().contains(".organizing-")).collect();
    assert_eq!(hidden.len(), 1, "caught at its temporary name");
    assert!(!from.exists());
    ok(&t.ask(3, "settings.set", json!({ "changes": { "music_dir": lib } })));
    t.close();
    // the next start puts it back where it was, and says so
    let mut t = start(env.clone());
    t.shake();
    assert!(from.join("01 Song 1.mp3").is_file() && !hidden[0].exists());
    let s = ok(&t.ask(4, "organize.status", json!({ "folder": lib }))).clone();
    assert_eq!((s["interrupted"]["moved"].as_u64(), s["interrupted"]["left"].as_u64()), (Some(0), Some(1)), "{s}");
    assert!(unacknowledged_moves(&mut t).is_empty() && backup_moves(&env).is_empty(), "nothing moved");
    t.close();
}

#[test]
fn saying_an_interrupted_run_was_seen_is_refused_in_a_test_run() {
    let (env, lib, _, _) = messy("org-seen");
    let mut t = start(Env { offline_test: true, ..env });
    t.shake();
    assert_eq!(code(&t.ask(2, "organize.status", json!({ "folder": lib, "seen": "20261118-090000-abcdef" }))), "offline_test");
    assert_eq!(ok(&t.ask(3, "organize.status", json!({ "folder": lib })))["interrupted"], Value::Null, "asking is still answered");
    t.close();
}

#[test]
fn edit_details_names_each_problem_by_its_full_path_when_two_files_share_a_name() {
    let l = lab("edit-paths", |_| {});
    let one = l.album.join("01 Song 1.mp3");
    let other = l.artist.join("Other").join("01 Song 1.mp3");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::copy(&one, &other).unwrap();
    let mut perm = std::fs::metadata(&other).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&other, perm.clone()).unwrap();
    let mut t = start(l.env.clone());
    t.shake();
    // the read-only one is listed second; matching by name alone pinned its problem on the first
    let files = json!([{ "path": one, "set": { "title": "A New Name" } }, { "path": other, "set": { "title": "Another" } }]);
    let r = ok(&t.plain(2, "tags.edit", json!({ "folder": l.lib, "files": files })).0).clone();
    assert_eq!(r["changed"], 1, "{r}");
    assert_eq!(r["problems"], json!([{ "path": other, "words": "Read-only: Cadence leaves read-only files alone." }]));
    assert_eq!((title_of(&one).as_str(), title_of(&other).as_str()), ("A New Name", "Song 1"));
    perm.set_readonly(false);
    std::fs::set_permissions(&other, perm).unwrap();
    t.close();
}

#[test]
fn a_song_still_playing_at_the_end_of_the_wait_is_left_by_a_repair_an_undo_and_organize() {
    let l = lab("playing-limit", |_| {});
    let mut t = start(l.env.clone());
    t.shake();
    let always = |file: PathBuf| move |a: &Value| if a["method"] == "library.playing" { json!({ "result": { "path": file } }) } else { usual(a) };
    // a repair: the loose song stays, said with its path
    let at = Instant::now();
    let (r, _, _) = t.drive(2, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }), always(l.loose.clone()), |_, _| {});
    assert!(at.elapsed() >= Duration::from_millis(1500));
    let r = ok(&r).clone();
    assert_eq!(r["changed"], 0, "{r}");
    assert!(r["problems"][0].as_str().unwrap().contains("still playing"), "{r}");
    assert!(l.loose.is_file());
    // done once it has stopped, then its Undo waits for the filed song, and leaves it
    let r = ok(&t.plain(3, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] })).0).clone();
    assert!(l.filed().is_file());
    let (u, _, _) = t.drive(4, "history.undo", json!({ "folder": l.lib, "id": r["batch"] }), always(l.filed()), |_, _| {});
    let left = ok(&u)["left"].clone();
    assert!(left[0].as_str().unwrap().contains("still playing"), "{left}");
    assert!(l.filed().is_file() && !l.loose.exists());
    // Undo again once it has stopped: it finishes
    assert_eq!(ok(&t.plain(5, "history.undo", json!({ "folder": l.lib, "id": r["batch"] })).0)["left"], json!([]));
    assert!(l.loose.is_file());
    t.close();

    // Organize: the album left for last stays, with the reason `playing`
    let (env, lib, albums) = shelf("org-playing-limit", 2, |_| {});
    let mut t = start(env);
    t.shake();
    let p = previewed(&mut t, &lib);
    let song = albums[0].0.join("01 Song 1.mp3");
    let at = Instant::now();
    let (r, _, asks) = t.drive(2, "organize.run", json!({ "folder": lib, "preview": p["preview"] }), always(song), |_, _| {});
    assert!(at.elapsed() >= Duration::from_millis(1500));
    let r = ok(&r).clone();
    assert_eq!((r["moved"].as_u64(), r["left_alone"][0]["why"].as_str()), (Some(1), Some("playing")), "{r}");
    assert!(albums[0].0.is_dir() && albums[1].1.is_dir());
    // (the album that moved was told before the wait began)
    let order: Vec<&str> = methods(&asks).into_iter().filter(|m| *m != "library.playing").collect();
    assert_eq!(order, ["library.hold", "library.moved", "library.release"]);
    t.close();
}

#[test]
fn an_organize_run_tells_its_moves_in_small_batches_and_takes_them_off_the_list() {
    let (env, lib, albums) = shelf("org-batches", 5, |e| e.parts.moves_every = 2);
    let mut t = start(env.clone());
    t.shake();
    let p = previewed(&mut t, &lib);
    let (r, _, asks) = t.plain(2, "organize.run", json!({ "folder": lib, "preview": p["preview"] }));
    let r = ok(&r).clone();
    assert_eq!(r["moved"], 5, "{r}");
    let sizes: Vec<usize> = asks.iter().filter(|a| a["method"] == "library.moved").map(|a| a["params"]["moves"].as_array().unwrap().len()).collect();
    assert_eq!(sizes, [2, 2, 1], "{asks:?}");
    assert!(asks.iter().filter(|a| a["method"] == "library.moved").all(|a| a["params"]["batch"] == r["batch"]));
    // Cadence took them all: none wait for its next start; Backup has every one
    assert!(unacknowledged_moves(&mut t).is_empty());
    let mut bk = backup_moves(&env);
    bk.sort();
    assert_eq!(bk, albums);
    t.close();
}

#[test]
fn a_long_job_renews_its_hold_on_the_library() {
    let (env, lib, _) = shelf("org-renew", 3, |e| e.parts.renew_every = Duration::ZERO);
    let mut t = start(env);
    t.shake();
    let p = previewed(&mut t, &lib);
    let (r, _, asks) = t.plain(2, "organize.run", json!({ "folder": lib, "preview": p["preview"] }));
    ok(&r);
    let hold = asks.iter().position(|a| a["method"] == "library.hold").unwrap();
    let renews: Vec<&Value> = asks.iter().filter(|a| a["method"] == "library.renew").collect();
    assert!(renews.len() >= 3, "{asks:?}");
    assert!(renews.iter().all(|a| a["params"]["hold"] == json!(format!("h{}", asks[hold]["ask"]))));
    t.close();
}

/// A copy of `base` at `dest` with these tags (the rest of its tags as they are).
fn tagged(base: &Path, dest: &Path, tags: &[(&str, &str)]) {
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let ffmpeg = crate::importer::tools::find_tool("ffmpeg").unwrap();
    let mut c = crate::importer::tools::command(&ffmpeg, false);
    c.args(["-hide_banner", "-v", "error", "-y", "-i"]).arg(base).args(["-c", "copy"]);
    for (k, v) in tags {
        c.args(["-metadata", &format!("{k}={v}")]);
    }
    c.arg(dest).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    assert!(crate::importer::tools::run_limited(&mut c, 60));
}

#[test]
fn an_organize_preview_keeps_the_folders_own_words_leaves_near_matches_and_marks_each_move() {
    let (env, _s) = soul_env("org-marks");
    let lib = scratch("org-marks-lib").join("Music");
    let base = tracks("mp3", 1)[0].1.clone();
    let put = |rel: &str, artist: &str, album: &str, year: &str| tagged(&base, &lib.join(rel).join("01 Song 1.mp3"), &[("artist", artist), ("album", album), ("date", year)]);
    put(r"AFI\[2013] Burials (Best Buy Exclusive)", "AFI", "Burials", "2013");
    put(r"Andrew Jackson Jihad\Knife Man (2011)", "AJJ", "Knife Man", "2011");
    put(r"Alice Coltrane\Journey in Satchidananda (1971)", "Alice Coltrane", "Journey in Satchidananda", "2009");
    put(r"50 Cent\Animal Ambition", "50 Cent", "Don't Worry 'Bout It - Single", "2014");
    put(r"100 gecs\1000 gecs (2019)", "100 gecs", "1000 Gecs", "2019");
    put(r"2 Chainz\B.O.A.T.S. II Me Time (2013)", "2 Chainz", "B.O.A.T.S. II: Me Time", "2013");
    let mut t = start(env);
    t.shake();
    let p = previewed(&mut t, &lib);
    assert_eq!((p["to_move"].as_u64(), p["in_place"].as_u64()), (Some(3), Some(3)), "{p}");
    assert_eq!(p["counts"], json!({ "artist": 1, "renamed": 1, "tidy": 1 }), "{p}");
    let find = |rel: &str| p["moves"].as_array().unwrap().iter().find(|m| m["from"].as_str() == lib.join(rel).to_str()).cloned().unwrap_or_else(|| panic!("{rel}: {p}"));
    let afi = find(r"AFI\[2013] Burials (Best Buy Exclusive)");
    assert_eq!((afi["to"].as_str(), &afi["changes"]), (crate::organize::place_for(&lib.join("AFI"), "Burials (Best Buy Exclusive) (2013)").to_str(), &json!(["tidy"])), "{afi}");
    assert_eq!(find(r"Andrew Jackson Jihad\Knife Man (2011)")["changes"], json!(["artist"]));
    assert_eq!(find(r"50 Cent\Animal Ambition")["changes"], json!(["renamed"]));
    t.close();
}

// ------------------------------------------------------------------ one writer at a time, and Stop

/// Reads the engine's lines, answering its asks as Cadence would, until the job says it reached its pause.
fn until_paused(t: &mut Talk, reached: &crossbeam_channel::Receiver<()>) {
    let start = Instant::now();
    loop {
        if reached.try_recv().is_ok() {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the job never reached its pause");
        if let Ok(m) = t.from.recv_timeout(Duration::from_millis(50)) {
            if let Some(n) = m.get("ask") {
                let mut a = usual(&m);
                a["answer"] = n.clone();
                t.raw(&a.to_string());
            }
        }
    }
}

/// The reply to `id`, answering asks on the way and passing over everything else.
fn answered(t: &mut Talk, id: u64) -> Value {
    loop {
        let m = t.next();
        if let Some(n) = m.get("ask") {
            let mut a = usual(&m);
            a["answer"] = n.clone();
            t.raw(&a.to_string());
        } else if m["id"] == json!(id) && m.get("event").is_none() {
            return m;
        }
    }
}

/// Eight copies of a song in the test album, for a change long enough to stop part way.
fn eight(l: &Lab) -> Vec<PathBuf> {
    let src = l.album.join("01 Song 1.mp3");
    (1..=8)
        .map(|n| {
            let f = l.album.join(format!("{n:02} Track.mp3"));
            std::fs::copy(&src, &f).unwrap();
            f
        })
        .collect()
}

#[test]
fn a_stopped_change_keeps_the_library_until_it_has_really_ended_and_then_undoes_whole() {
    let l = lab("stop-undo", |_| {});
    let files = eight(&l);
    let (reached_tx, reached) = crossbeam_channel::bounded(1);
    let (go, go_rx) = crossbeam_channel::bounded(1);
    crate::repair::mid_write::set(&l.album, reached_tx, go_rx);
    let mut t = start(l.env.clone());
    t.shake();
    let set: Vec<Value> = files.iter().enumerate().map(|(n, f)| json!({ "path": f, "set": { "title": format!("New {n}") } })).collect();
    t.send(2, "tags.edit", json!({ "folder": l.lib, "files": set }));
    // part way: the first group of files is saved, and the job waits before the next
    until_paused(&mut t, &reached);
    // Stop, as Cadence does (its own claim on the library may end now), then at once an Undo of that very
    // batch, and another change: both are refused while the job is still writing
    t.send(3, "cancel", json!({ "id": 2 }));
    answered(&mut t, 3);
    t.send(4, "history.list", json!({ "folder": l.lib }));
    let h = ok(&answered(&mut t, 4)).clone();
    let batch = h["repairs"][0]["id"].clone();
    assert!(batch.is_string(), "{h}");
    t.send(5, "history.undo", json!({ "folder": l.lib, "id": batch }));
    let u = answered(&mut t, 5);
    assert_eq!(code(&u), "busy", "{u}");
    assert_eq!(u["error"]["message"], "Another change to your library is still finishing. Try again in a moment.");
    t.send(6, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }));
    assert_eq!(code(&answered(&mut t, 6)), "busy");
    assert!(l.loose.is_file());
    // let it go on: the stop lands before the next group of files
    go.send(()).unwrap();
    let r = answered(&mut t, 2);
    assert_eq!(code(&r), "cancelled", "{r}");
    let titles: Vec<String> = files.iter().map(|f| title_of(f)).collect();
    assert_eq!(titles.iter().filter(|t| t.starts_with("New")).count(), 4, "{titles:?}");
    // now it has really ended: the Undo runs, and puts back everything the batch did
    t.send(7, "history.undo", json!({ "folder": l.lib, "id": batch }));
    assert_eq!(ok(&answered(&mut t, 7))["left"], json!([]));
    assert!(files.iter().all(|f| title_of(f) == "Song 1"));
    t.send(8, "history.list", json!({ "folder": l.lib }));
    assert_eq!(ok(&answered(&mut t, 8))["repairs"][0]["undone"], true);
    // and the next change is let in
    t.send(9, "repair.apply", json!({ "folder": l.lib, "keys": [l.loose_key()] }));
    ok(&answered(&mut t, 9));
    assert!(l.filed().is_file());
    t.close();
}

#[test]
fn a_stopped_undo_ends_between_lines_and_a_second_undo_finishes_it() {
    let l = lab("undo-stop", |_| {});
    let files = eight(&l);
    let mut t = start(l.env.clone());
    t.shake();
    let set: Vec<Value> = files.iter().map(|f| json!({ "path": f, "set": { "title": "New" } })).collect();
    let r = ok(&t.plain(2, "tags.edit", json!({ "folder": l.lib, "files": set })).0).clone();
    assert_eq!(r["changed"], 8, "{r}");
    let (reached_tx, reached) = crossbeam_channel::bounded(1);
    let (go, go_rx) = crossbeam_channel::bounded(1);
    crate::repair::mid_write::set(&l.backups(), reached_tx, go_rx);
    t.send(3, "history.undo", json!({ "folder": l.lib, "id": r["batch"] }));
    // one line is put back, then Stop
    until_paused(&mut t, &reached);
    t.send(4, "cancel", json!({ "id": 3 }));
    answered(&mut t, 4);
    go.send(()).unwrap();
    let u = answered(&mut t, 3);
    assert_eq!(code(&u), "cancelled", "{u}");
    assert_eq!(files.iter().filter(|f| title_of(f) == "Song 1").count(), 1);
    t.send(5, "history.list", json!({ "folder": l.lib }));
    assert_eq!(ok(&answered(&mut t, 5))["repairs"][0]["undone"], false, "not marked undone");
    // Undo again: the rest comes back, and the one already back is left as it is
    let (u, _, _) = t.plain(6, "history.undo", json!({ "folder": l.lib, "id": r["batch"] }));
    assert_eq!(ok(&u)["left"], json!([]));
    assert!(files.iter().all(|f| title_of(f) == "Song 1"));
    t.close();
}
