//! Backup (protocol 5) driven the way Cadence drives it. Each test has a library, a backup folder and a data
//! folder of its own in a temp folder, drives that are a stand-in (`backup::fake::FakeDisks`: the test names
//! the disk and can swap it) and a Task Scheduler that only writes down what it is asked
//! (`backup::fake::FakeScheduler`). Nothing here reads a real drive's backup, makes a task, or plays sound.
use super::*;
use crate::backup::api::TestDb;
use backup::fake::{FakeDisks, FakeScheduler};

struct Bk {
    env: Env,
    disks: Arc<FakeDisks>,
    sched: Arc<FakeScheduler>,
    base: PathBuf,
    lib: PathBuf,
    dest: PathBuf,
}

/// Six small files in three artists' folders.
const FILES: [(&str, &[u8]); 6] = [
    (r"Artist A\Album One (2001)\01 One.mp3", b"the first song, as it was"),
    (r"Artist A\Album One (2001)\02 Two.mp3", b"the second song"),
    (r"Artist B\album two\01 Three.mp3", b"the third song, a little longer than the others"),
    (r"Artist B\album two\cover.jpg", b"a picture"),
    (r"Artist C\Album Three\01 Four.mp3", b"the fourth song"),
    (r"Artist C\Album Three\02 Five.mp3", b"the fifth song"),
];

fn bk(name: &str) -> Bk {
    let (mut env, _s) = soul_env(name);
    let disks = Arc::new(FakeDisks::new());
    let sched = Arc::new(FakeScheduler::default());
    env.parts.disks = disks.clone();
    env.parts.scheduler = sched.clone();
    let base = scratch(&format!("{name}-bk"));
    let lib = base.join("Music");
    for (rel, bytes) in FILES {
        let p = lib.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    let dest = base.join("Backup drive").join("Music Backup");
    Bk { env, disks, sched, base, lib, dest }
}

impl Bk {
    fn talk(&self) -> Talk {
        let mut t = start(self.env.clone());
        t.shake();
        t
    }
    fn confirm(&self, t: &mut Talk, id: u64) -> Value {
        ok(&t.ask(id, "backup.confirm_folders", json!({ "folder": self.lib, "which": "main", "to": self.dest, "confirm": true }))).clone()
    }
    fn run(&self, t: &mut Talk, id: u64) -> (Value, usize) {
        t.send(id, "backup.run", json!({ "folder": self.lib, "which": "main" }));
        t.reply(id)
    }
    fn status(&self, t: &mut Talk, id: u64) -> Value {
        ok(&t.ask(id, "backup.status", json!({ "folder": self.lib }))).clone()
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.lib.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn lib_bytes(&self) -> u64 {
        FILES.iter().map(|f| f.1.len() as u64).sum()
    }
}

/// Every file under a folder, with its bytes, by its path inside the folder.
fn contents(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = if dir.is_dir() { walk(dir).into_iter().map(|p| (p.strip_prefix(dir).unwrap().to_string_lossy().to_string(), std::fs::read(&p).unwrap())).collect() } else { vec![] };
    v.sort();
    v
}

/// Damages a file the way a failing disk does: the same size and the same date, other bytes from `from` on.
fn damage(p: &Path, from: f64) {
    let when = std::fs::metadata(p).unwrap().modified().unwrap();
    let mut b = std::fs::read(p).unwrap();
    let at = (b.len() as f64 * from) as usize;
    for x in b.iter_mut().skip(at) {
        *x = 0x55;
    }
    std::fs::write(p, &b).unwrap();
    std::fs::File::options().write(true).open(p).unwrap().set_modified(when).unwrap();
}

#[test]
fn only_an_explicit_confirm_enrolls_a_pair_and_another_disk_at_the_same_letter_is_refused() {
    let b = bk("bk-enroll");
    let mut t = b.talk();
    let s = b.status(&mut t, 2);
    assert_eq!((s["state"].as_str(), s["main"]["set_up"].as_bool()), (Some("not_set_up"), Some(false)), "{s}");
    assert_eq!(s["sentence"]["words"], "No backup yet");

    // drive tiles: never the library's own drive
    *b.disks.roots.lock().unwrap() = vec!["C:\\".into(), format!("{}\\", b.base.join("Backup drive").display())];
    let d = ok(&t.ask(3, "backup.drives", json!({ "folder": b.lib }))).clone();
    let tiles = d["drives"].as_array().unwrap();
    assert_eq!(tiles.len(), 1, "{d}");
    assert_eq!(tiles[0]["folder"].as_str().map(PathBuf::from), Some(b.dest.clone()));
    assert_eq!((d["library"]["files"].as_u64(), d["library"]["bytes"].as_u64()), (Some(6), Some(b.lib_bytes())));
    assert!(tiles[0]["fits"].as_bool().unwrap() && tiles[0]["has_backup"] == false, "{d}");
    *b.disks.roots.lock().unwrap() = vec!["C:\\".into()];

    // the preview of the pair: both folders, the rules, what a first backup copies; nothing made
    let p = ok(&t.ask(4, "backup.preview_folders", json!({ "folder": b.lib, "which": "main", "to": b.dest }))).clone();
    assert_eq!((p["can_confirm"].as_bool(), p["to_copy"]["files"].as_u64(), p["to_copy"]["bytes"].as_u64()), (Some(true), Some(6), Some(b.lib_bytes())), "{p}");
    assert_eq!(p["rules"].as_array().unwrap().len(), 3);
    assert!(!b.dest.exists(), "a preview made the backup folder");
    // a folder inside the library is refused in plain words
    let bad = ok(&t.ask(5, "backup.preview_folders", json!({ "folder": b.lib, "to": b.lib.join("Backup") }))).clone();
    assert!(bad["problem"].as_str().unwrap().contains("separate") && bad["can_confirm"] == false, "{bad}");

    // nothing runs before a confirm, and a confirm needs its word
    assert_eq!(code(&b.run(&mut t, 6).0), "not_set_up");
    assert_eq!(code(&t.ask(7, "backup.confirm_folders", json!({ "folder": b.lib, "to": b.dest }))), "bad_request");
    assert!(!b.dest.exists() && !b.env.dir.join("backup").join("config.json").exists());
    let c = b.confirm(&mut t, 8);
    assert_eq!(c["confirmed"], true);
    // bound: the marker, and no music copied yet
    assert_eq!(contents(&b.dest).iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["_backup-id.txt"]);
    let s = b.status(&mut t, 9);
    assert_eq!((s["main"]["set_up"].as_bool(), s["main"]["drive"]["state"].as_str()), (Some(true), Some("here")), "{s}");

    // another disk at the same letter, by its name: refused, nothing copied
    b.disks.labels.lock().unwrap().insert("c:".into(), "Pocket".into());
    let s = b.status(&mut t, 10);
    assert_eq!(s["main"]["drive"]["state"], "wrong_disk", "{s}");
    assert!(s["main"]["drive"]["words"].as_str().unwrap().contains("is \"Pocket\", not \"Test drive\""), "{s}");
    assert_eq!((s["state"].as_str(), s["sentence"]["words"].as_str()), (Some("needs_look"), Some("Your backups need a look")));
    assert_eq!(code(&b.run(&mut t, 11).0), "wrong_disk");
    // the same name, but another disk (its volume serial): refused by the folder's identity
    b.disks.labels.lock().unwrap().clear();
    b.disks.swapped.lock().unwrap().push(b.dest.clone());
    assert_eq!(b.status(&mut t, 12)["main"]["drive"]["state"], "wrong_disk");
    let r = b.run(&mut t, 13).0;
    assert_eq!(code(&r), "wrong_disk", "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("not the confirmed backup folder or drive"));
    assert_eq!(contents(&b.dest).len(), 1, "something was copied to the wrong disk");
    // the right disk back: it runs
    b.disks.swapped.lock().unwrap().clear();
    assert!(ok(&b.run(&mut t, 14).0)["copied"] == 6);
    t.close();
}

#[test]
fn a_run_copies_reads_back_and_keeps_replaced_and_removed_files_in_history() {
    let b = bk("bk-run");
    let mut t = b.talk();
    b.confirm(&mut t, 2);
    let (r, events) = b.run(&mut t, 3);
    let r = ok(&r).clone();
    assert_eq!((r["copied"].as_u64(), r["verified"].as_u64(), r["bytes"].as_u64()), (Some(6), Some(6), Some(b.lib_bytes())), "{r}");
    assert!(r["words"].as_str().unwrap().starts_with("Backup finished. Copied 6 files"), "{r}");
    assert!(events >= 2, "no progress was sent");
    for (rel, bytes) in FILES {
        assert_eq!(std::fs::read(b.dest.join(rel)).unwrap(), bytes);
    }
    // a changed file: the new one copied, the old copy kept in backup history
    b.write(FILES[0].0, b"the first song, changed and longer now");
    // a removed file: its copy moves to backup history, never deleted
    std::fs::remove_file(b.lib.join(FILES[5].0)).unwrap();
    let r = ok(&b.run(&mut t, 4).0).clone();
    assert_eq!((r["copied"].as_u64(), r["kept_old"].as_u64(), r["removed"].as_u64()), (Some(1), Some(1), Some(1)), "{r}");
    assert_eq!(std::fs::read(b.dest.join(FILES[0].0)).unwrap(), b"the first song, changed and longer now");
    let kept: Vec<Vec<u8>> = contents(&b.dest.join("_changed")).into_iter().map(|f| f.1).collect();
    assert!(kept.contains(&FILES[0].1.to_vec()), "the old copy was not kept");
    let gone: Vec<Vec<u8>> = contents(&b.dest.join("_deleted")).into_iter().map(|f| f.1).collect();
    assert_eq!(gone, vec![FILES[5].1.to_vec()]);
    assert!(!b.dest.join(FILES[5].0).exists());
    // the library was only read
    assert_eq!(std::fs::read(b.lib.join(FILES[1].0)).unwrap(), FILES[1].1);
    // the card: done tonight, and the sentence
    let s = b.status(&mut t, 5);
    assert_eq!(s["main"]["nights"].as_array().unwrap().last().unwrap(), "done", "{s}");
    assert_eq!((s["state"].as_str(), s["main"]["last"]["tone"].as_str()), (Some("good"), Some("good")), "{s}");
    assert!(s["sentence"]["words"].as_str().unwrap().starts_with("Backed up "), "{s}");
    // the nightly backup is not set up: a gentle word only
    assert!(s["alerts"].as_array().unwrap().iter().any(|a| a["words"] == "No nightly backup is set up." && a["serious"] == false), "{s}");
    t.close();
}

#[test]
fn moves_in_the_library_are_followed_so_nothing_is_copied_twice() {
    let b = bk("bk-moves");
    let mut t = b.talk();
    b.confirm(&mut t, 2);
    let first = ok(&b.run(&mut t, 3).0).clone();
    assert_eq!(first["bytes"].as_u64(), Some(b.lib_bytes()));
    // an album moved to another artist's folder, an album renamed in case only, and a move whose copy was
    // never made (its file is new at its new place)
    let (from, to) = (b.lib.join(r"Artist A\Album One (2001)"), b.lib.join(r"Artist A2\Album One (2001)"));
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(&from, &to).unwrap();
    let (lower, upper) = (b.lib.join(r"Artist B\album two"), b.lib.join(r"Artist B\Album Two"));
    std::fs::rename(&lower, &upper).unwrap();
    b.write(r"Artist D\Newer\01 Six.mp3", b"a new song");
    let mv = |f: &Path, t: &Path, folder: bool| moves::Move { from: f.to_path_buf(), to: t.to_path_buf(), folder };
    moves::for_backup(&b.env.dir, "20261118-091200-aaaaaa", &[mv(&from, &to, true), mv(&lower, &upper, true), mv(&b.lib.join(r"Artist D\New\01 Six.mp3"), &b.lib.join(r"Artist D\Newer\01 Six.mp3"), false)]);
    let r = ok(&b.run(&mut t, 4).0).clone();
    // only the new song was copied: counted in bytes
    assert_eq!((r["renamed"].as_u64(), r["copied"].as_u64(), r["bytes"].as_u64()), (Some(2), Some(1), Some(10)), "{r}");
    assert!(b.dest.join(r"Artist A2\Album One (2001)\01 One.mp3").is_file() && !b.dest.join(r"Artist A\Album One (2001)").exists());
    let names: Vec<String> = std::fs::read_dir(b.dest.join("Artist B")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert_eq!(names, ["Album Two"], "the case-only rename was not followed");
    // a moved file never counts as removed
    assert!(!b.dest.join("_deleted").exists() && !b.dest.join("_changed").exists());
    // followed once: marked for this backup, and not followed again
    let marker = std::fs::read_to_string(b.dest.join("_backup-id.txt")).unwrap();
    let kept: Value = serde_json::from_slice(&std::fs::read(b.env.dir.join(r"backup\moves-followed.json")).unwrap()).unwrap();
    assert_eq!(kept[marker.trim()], json!(["20261118-091200-aaaaaa"]), "{kept}");
    let again = ok(&b.run(&mut t, 5).0).clone();
    assert_eq!((again["renamed"].as_u64(), again["copied"].as_u64()), (Some(0), Some(0)), "{again}");
    t.close();
}

#[test]
fn a_whole_check_changes_nothing_and_names_the_damaged_copies() {
    let b = bk("bk-check");
    let mut t = b.talk();
    b.confirm(&mut t, 2);
    ok(&b.run(&mut t, 3).0);
    let rel = FILES[2].0;
    damage(&b.dest.join(rel), 0.5);
    let (before_backup, before_lib) = (contents(&b.dest), contents(&b.lib));
    t.send(4, "backup.check", json!({ "folder": b.lib, "which": "main" }));
    let (r, events) = t.reply(4);
    let r = ok(&r).clone();
    assert_eq!((r["files"].as_u64(), r["good"].as_u64(), r["complete"].as_bool()), (Some(6), Some(5), Some(false)), "{r}");
    assert_eq!(r["damaged"], json!([rel]));
    assert_eq!(r["albums"][0]["album"], r"Artist B\album two");
    assert!(events >= 1);
    assert_eq!(contents(&b.dest), before_backup, "the check changed the backup");
    assert_eq!(contents(&b.lib), before_lib, "the check changed the library");
    let s = b.status(&mut t, 5);
    assert_eq!(s["main"]["check"]["damaged"], json!([rel]), "{s}");
    assert_eq!(s["state"], "needs_look");
    t.close();
}

#[test]
fn each_choice_for_a_damaged_copy_and_bringing_back_lands_in_history_with_undo() {
    let b = bk("bk-damaged");
    let album = b.lib.join(r"Test Artist\Quiet Rooms (2020)");
    std::fs::create_dir_all(&album).unwrap();
    for (name, from) in tracks("mp3", 2) {
        std::fs::copy(from, album.join(name)).unwrap();
    }
    let (one, two) = (r"Test Artist\Quiet Rooms (2020)\01 Song 1.mp3", r"Test Artist\Quiet Rooms (2020)\02 Song 2.mp3");
    let mut t = b.talk();
    b.confirm(&mut t, 2);
    ok(&b.run(&mut t, 3).0);
    // song 1: the copy is damaged; song 2: the library's file is
    damage(&b.dest.join(one), 0.3);
    damage(&b.lib.join(two), 0.3);
    let damaged_two = std::fs::read(b.lib.join(two)).unwrap();
    let r = ok(&t.ask(4, "backup.check", json!({ "folder": b.lib }))).clone();
    assert_eq!(r["damaged"], json!([one, two]), "{r}");

    // find out which file is sound: changes nothing
    let before = (contents(&b.dest), contents(&b.lib));
    let f = ok(&t.ask(5, "backup.repair_damaged", json!({ "folder": b.lib, "choice": "find_out", "files": [one, two, r"Artist B\album two\cover.jpg"] }))).clone();
    let sounds: Vec<&str> = f["files"].as_array().unwrap().iter().map(|x| x["sound"].as_str().unwrap()).collect();
    assert_eq!(sounds, ["library", "copy", "not_audio"], "{f}");
    assert_eq!((contents(&b.dest), contents(&b.lib)), before, "finding out changed something");

    // copy again: a fresh copy from the library, the damaged one kept in backup history
    let bad_copy = std::fs::read(b.dest.join(one)).unwrap();
    let c = ok(&t.ask(6, "backup.repair_damaged", json!({ "folder": b.lib, "choice": "copy_again", "files": [one] }))).clone();
    assert_eq!(c["repaired"], 1, "{c}");
    assert_eq!(std::fs::read(b.dest.join(one)).unwrap(), std::fs::read(b.lib.join(one)).unwrap());
    assert!(contents(&b.dest.join("_changed")).iter().any(|f| f.1 == bad_copy));

    // bring back: through the hold, as one "restore" batch in Repair history
    let sound_copy = std::fs::read(b.dest.join(two)).unwrap();
    let (r, _, asks) = t.plain(7, "backup.repair_damaged", json!({ "folder": b.lib, "choice": "bring_back", "files": [two] }));
    let r = ok(&r).clone();
    assert_eq!((r["changed"].as_u64(), r["problems"].as_array().map(Vec::len)), (Some(1), Some(0)), "{r}");
    let m = methods(&asks);
    assert_eq!((m.first(), m.last()), (Some(&"library.hold"), Some(&"library.release")), "{m:?}");
    assert_eq!(std::fs::read(b.lib.join(two)).unwrap(), sound_copy);
    assert_eq!(std::fs::read(b.dest.join(two)).unwrap(), sound_copy, "the backup was changed");
    let h = ok(&t.ask(8, "history.list", json!({ "folder": b.lib }))).clone();
    let entry = &h["repairs"][0];
    assert_eq!((entry["kind"].as_str(), entry["id"].as_str()), (Some("restore"), r["batch"].as_str()), "{h}");
    let (u, _, _) = t.plain(9, "history.undo", json!({ "folder": b.lib, "id": r["batch"] }));
    assert!(ok(&u)["left"].as_array().unwrap().is_empty(), "{u}");
    assert_eq!(std::fs::read(b.lib.join(two)).unwrap(), damaged_two, "Undo did not put the file back");

    // names that are not inside the library are refused
    assert_eq!(code(&t.ask(10, "backup.repair_damaged", json!({ "folder": b.lib, "choice": "copy_again", "files": [r"..\outside.mp3"] }))), "bad_request");
    assert_eq!(code(&t.ask(11, "backup.repair_damaged", json!({ "folder": b.lib, "choice": "melt", "files": [one] }))), "bad_request");
    t.close();
}

#[test]
fn the_nightly_task_is_made_and_removed_only_through_the_stand_in_scheduler() {
    assert_ne!(crate::backup::api::CADENCE_TASK_NAME, "Music Backup (nightly)");
    let b = bk("bk-schedule");
    let mut t = b.talk();
    // not before the main backup is confirmed
    assert_eq!(code(&t.ask(2, "backup.schedule", json!({ "on": true, "folder": b.lib }))), "not_confirmed");
    assert!(b.sched.calls.lock().unwrap().is_empty());
    assert_eq!(code(&t.ask(3, "backup.schedule", json!({ "folder": b.lib }))), "bad_request");
    b.confirm(&mut t, 4);
    assert_eq!(code(&t.ask(5, "backup.schedule", json!({ "on": true, "folder": b.lib, "at": "3am" }))), "failed");
    let r = ok(&t.ask(6, "backup.schedule", json!({ "on": true, "folder": b.lib }))).clone();
    assert_eq!(r["on"], true, "{r}");
    let calls = b.sched.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let data = b.env.dir.parent().unwrap().to_string_lossy().to_string();
    assert!(calls[0].contains(&format!("--backup-nightly --data \"{data}\"")) && calls[0].ends_with("at 03:00"), "{calls:?}");
    let s = b.status(&mut t, 7);
    assert_eq!((s["schedule"]["on"].as_bool(), s["schedule"]["words"].as_str()), (Some(true), Some("Every night at 03:00, even when Cadence is closed")), "{s}");
    ok(&t.ask(8, "backup.schedule", json!({ "on": false })));
    assert_eq!(b.sched.calls.lock().unwrap().last().unwrap(), "remove");
    assert_eq!(b.status(&mut t, 9)["schedule"]["on"], false);
    t.close();
}

#[test]
fn the_nightly_backup_runs_without_cadence_and_never_while_the_library_is_changing() {
    let b = bk("bk-nightly");
    let mut t = b.talk();
    ok(&t.ask(2, "settings.set", json!({ "changes": { "music_dir": b.lib } })));
    b.confirm(&mut t, 3);
    t.close();
    // the engine is changing the library: the nightly backup waits, then does not start
    backup::changing::enter(&b.env.dir);
    assert!(b.env.dir.join("library-changing.lock").is_file() && backup::changing::busy(&b.env.dir));
    let r = backup::nightly(&b.env, Duration::ZERO);
    assert_eq!(r.as_ref().map_err(|e| e.0.as_str()).err(), Some("busy"), "{r:?}");
    assert_eq!(contents(&b.dest).len(), 1, "it copied while the library was changing");
    backup::changing::leave(&b.env.dir);
    assert!(!backup::changing::busy(&b.env.dir) && !b.env.dir.join("library-changing.lock").exists());
    // that night says so
    let mut t = b.talk();
    let s = b.status(&mut t, 2);
    assert_eq!(s["main"]["nights"].as_array().unwrap().last().unwrap(), "problem", "{s}");
    assert!(s["main"]["last"]["result"].as_str().unwrap().contains("Cadence was changing your library"), "{s}");
    assert_eq!(s["main"]["last"]["by"], "nightly", "{s}");
    t.close();
    // with Cadence closed and nothing changing, it backs up
    let r = backup::nightly(&b.env, Duration::ZERO).unwrap();
    assert_eq!(r["copied"], 6, "{r}");
    // never in a test run
    assert_eq!(backup::nightly(&Env { offline_test: true, ..b.env.clone() }, Duration::ZERO).unwrap_err().0, "offline_test");
}

#[test]
fn eject_says_the_drive_can_go_only_once_windows_has_let_go() {
    let b = bk("bk-eject");
    let mut t = b.talk();
    let second = b.base.join("Travel").join("Music Backup");
    ok(&t.ask(2, "backup.confirm_folders", json!({ "folder": b.lib, "which": "second", "to": second, "confirm": true })));
    assert_eq!(b.status(&mut t, 3)["second"]["drive"]["state"], "here");
    let r = t.ask(4, "backup.eject", json!({ "which": "second" }));
    assert_eq!(code(&r), "drive_busy");
    assert!(r["error"]["message"].as_str().unwrap().contains("Do not pull it out yet"), "{r}");
    *b.disks.eject.lock().unwrap() = Ok(());
    let r = ok(&t.ask(5, "backup.eject", json!({ "which": "second" }))).clone();
    assert_eq!((r["ejected"].as_bool(), r["words"].as_str()), (Some(true), Some("Test drive can be pulled out now.")), "{r}");
    assert_eq!(b.disks.ejected.lock().unwrap().last().map(PathBuf::from), Some(second.clone()));
    *b.disks.removable.lock().unwrap() = false;
    assert!(t.ask(6, "backup.eject", json!({ "which": "second" }))["error"]["message"].as_str().unwrap().contains("not one that can be ejected"));
    t.close();
}

#[test]
fn a_test_run_never_copies_binds_ejects_or_schedules() {
    let b = bk("bk-offline");
    let mut t = start(Env { offline_test: true, ..b.env.clone() });
    t.shake();
    let refused = [
        ("backup.confirm_folders", json!({ "folder": b.lib, "to": b.dest, "confirm": true })),
        ("backup.run", json!({ "folder": b.lib })),
        ("backup.run", json!({ "folder": b.lib, "kind": "mirror" })),
        ("backup.repair_damaged", json!({ "folder": b.lib, "choice": "copy_again", "files": ["a.mp3"] })),
        ("backup.repair_damaged", json!({ "folder": b.lib, "choice": "bring_back", "files": ["a.mp3"] })),
        ("backup.eject", json!({ "which": "second" })),
        ("backup.schedule", json!({ "on": true, "folder": b.lib })),
        ("backup.schedule", json!({ "on": false })),
    ];
    for (n, (method, params)) in refused.into_iter().enumerate() {
        assert_eq!(code(&t.ask(10 + n as u64, method, params)), "offline_test", "{method}");
    }
    // what only reads still answers
    assert!(t.ask(30, "backup.status", json!({ "folder": b.lib })).get("result").is_some());
    assert!(t.ask(31, "backup.drives", json!({ "folder": b.lib })).get("result").is_some());
    assert!(t.ask(32, "backup.preview_folders", json!({ "folder": b.lib, "to": b.dest })).get("result").is_some());
    assert_eq!(code(&t.ask(33, "backup.run", json!({ "folder": b.lib, "kind": "preview" }))), "not_set_up");
    assert!(!b.dest.exists() && !b.env.dir.join("backup").join("config.json").exists());
    assert!(b.sched.calls.lock().unwrap().is_empty() && b.disks.ejected.lock().unwrap().is_empty());
    t.close();
}

#[test]
fn cadences_databases_are_saved_through_the_online_snapshot_never_as_live_files() {
    let mut b = bk("bk-sqlite");
    let data = b.base.join("Data");
    std::fs::create_dir_all(data.join(r"Tools\slskd\bin")).unwrap();
    std::fs::create_dir_all(data.join("Cache")).unwrap();
    std::fs::write(data.join("settings.json"), b"{\"theme\":\"dark\"}").unwrap();
    std::fs::write(data.join(r"Tools\config.json"), b"{}").unwrap();
    std::fs::write(data.join(r"Tools\slskd\bin\slskd.exe"), b"a helper, fetched again on demand").unwrap();
    std::fs::write(data.join(r"Cache\cover.jpg"), b"a cache").unwrap();
    // a live database with a committed row still in its WAL file, held open the whole time
    let live = TestDb::open(&data.join("library.db"));
    assert_eq!(live.exec("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE tracks(id INTEGER PRIMARY KEY, title TEXT); INSERT INTO tracks VALUES(1,'A song');"), 0);
    assert!(std::fs::metadata(data.join("library.db-wal")).unwrap().len() > 0);
    b.env.parts.cadence_data = Some(data.clone());
    let mut t = b.talk();
    b.confirm(&mut t, 2);
    let r = ok(&b.run(&mut t, 3).0).clone();
    assert_eq!(r["cadence_saved"], true, "{r}");
    t.close();
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let saved = b.dest.join("_Cadence").join(day).join("Data");
    assert!(saved.join("settings.json").is_file() && saved.join(r"Tools\config.json").is_file());
    assert!(!saved.join("library.db-wal").exists() && !saved.join("Cache").exists() && !saved.join(r"Tools\slskd").exists(), "{:?}", contents(&saved).iter().map(|f| &f.0).collect::<Vec<_>>());
    // the snapshot holds the row that was only in the WAL
    let copy = TestDb::open(&saved.join("library.db"));
    assert_eq!(copy.exec("INSERT INTO tracks VALUES(1,'a duplicate');"), 19, "the committed row is not in the snapshot");
    drop(copy);
    // and the live database was left as it was, still in use
    assert_eq!(live.exec("INSERT INTO tracks VALUES(2,'still writable');"), 0);
}

#[test]
fn each_run_says_whether_the_nightly_task_or_a_person_started_it() {
    let b = bk("bk-by");
    let mut t = b.talk();
    ok(&t.ask(2, "settings.set", json!({ "changes": { "music_dir": b.lib } })));
    b.confirm(&mut t, 3);
    // by hand
    ok(&b.run(&mut t, 4).0);
    let s = b.status(&mut t, 5);
    assert_eq!((s["main"]["last"]["by"].as_str(), s["main"]["nights_by"].as_array().unwrap().last().and_then(Value::as_str)), (Some("hand"), Some("hand")), "{s}");
    // the nights before any run say no one
    assert!(s["main"]["nights_by"].as_array().unwrap()[..13].iter().all(Value::is_null), "{s}");
    assert_eq!(s["main"]["nights_by"].as_array().unwrap().len(), 14);
    t.close();
    // then the nightly task, the same day: the latest good run decides the night
    b.write(FILES[1].0, b"the second song, changed since");
    backup::nightly(&b.env, Duration::ZERO).unwrap();
    let mut t = b.talk();
    let s = b.status(&mut t, 2);
    assert_eq!((s["main"]["last"]["by"].as_str(), s["main"]["nights_by"].as_array().unwrap().last().and_then(Value::as_str)), (Some("nightly"), Some("nightly")), "{s}");
    // and only the nightly run carries the mark in the log
    let log = std::fs::read_to_string(b.env.dir.join(r"backup\logs\backup-log.txt")).unwrap();
    assert_eq!(log.matches(crate::backup::api::NIGHTLY_LINE).count(), 1, "{log}");
    // another run by hand afterwards is by hand again
    ok(&b.run(&mut t, 3).0);
    assert_eq!(b.status(&mut t, 4)["main"]["last"]["by"], "hand");
    t.close();
}