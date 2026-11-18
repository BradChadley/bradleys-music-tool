//! Backup for Cadence's engine (ENGINE-PROTOCOL.md, "Protocol 5"): what the Backup page of the Music Tool
//! does, as calls that answer with plain data instead of drawing. Every safety rule stays where it is, in
//! `native.rs`: folders bound by volume serial and file ID plus a marker file, only an explicit confirm
//! enrolls a pair, a different disk at the same letter refused, copies read back and checked, nothing
//! deleted from the backup (replaced and removed files go to backup history for 30 days). This file only
//! starts that worker and reads what it wrote.
//!
//! The engine keeps its own backup settings and logs in `<Tools>\backup` (never the Music Tool's backup
//! folder), saves only Cadence's own data beside the music (`Snapshots::Cadence`), and reaches drives
//! and Task Scheduler through parts a test can replace (`Disks`, `Scheduler`).
use super::*;
pub(crate) use super::native::{Followed, Moved};
#[allow(unused_imports)] // (Nightly: the tests' stand-in scheduler answers with it)
pub(crate) use super::schedule::{Nightly, Scheduler, TaskScheduler};
use super::schedule::nightly_args;
pub(crate) use super::NIGHTLY_LINE;
#[cfg(test)]
pub(crate) use super::sqlite::TestDb;
#[cfg(test)]
pub(crate) use super::schedule::CADENCE_TASK as CADENCE_TASK_NAME;
pub(crate) use super::{Snapshots, Which};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};

/// Where the engine keeps Backup's settings and logs, and what it reaches drives through.
#[derive(Clone)]
pub(crate) struct Setup {
    /// `<Tools>\backup`: `config.json` (the confirmed folders, as the Music Tool keeps them) and `logs`
    pub(crate) dir: PathBuf,
    pub(crate) disks: Arc<dyn Disks>,
    pub(crate) snapshots: Snapshots,
    pub(crate) recycle: fn(&Path) -> Result<(), String>,
    /// started by Cadence's nightly task (its runs are marked so in the log), not by hand
    pub(crate) nightly: bool,
}

impl Setup {
    fn places(&self) -> Places {
        Places { config: self.dir.join("config.json"), logs: self.dir.join("logs"), test: false, disks: self.disks.clone(), snapshots: self.snapshots.clone(), recycle: self.recycle, nightly: self.nightly }
    }
}

/// "main" or "second".
pub(crate) fn which_of(s: &str) -> Option<Which> {
    match s {
        "main" | "" => Some(Which::Main),
        "second" => Some(Which::Second),
        _ => None,
    }
}

pub(crate) fn which_name(w: Which) -> &'static str {
    match w {
        Which::Main => "main",
        Which::Second => "second",
    }
}

/// The code Cadence acts on, for the words the worker stopped with (its own sentences, `native.rs`).
pub(crate) fn code_of(why: &str) -> &'static str {
    let has = |s: &str| why.contains(s);
    if has("Confirm the library and backup folders") || has("saved folder identities are not readable") || has("library folder or its drive has changed") || has("identity file is missing") {
        "not_confirmed"
    } else if has("not the confirmed backup folder or drive") || has("not the saved disk") {
        "wrong_disk"
    } else if has("No backup folder has been chosen yet") {
        "not_set_up"
    } else if has("drive is not connected") || has("drive is no longer connected") {
        "drive_away"
    } else if has("already running") {
        "busy"
    } else if why.starts_with("Stopped") {
        "cancelled"
    } else if has("Many files are missing from the library") {
        "too_many_removed"
    } else if has("Not enough room") {
        "no_room"
    } else {
        "failed"
    }
}

fn tone_name(t: Tone) -> &'static str {
    match t {
        Tone::Good => "good",
        Tone::Bad => "bad",
        Tone::Unsure => "unsure",
        Tone::Plain => "plain",
    }
}

fn iso(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// "3 hours ago", "12 minutes ago", "yesterday", "6 days ago".
fn ago_words(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let mins = (now - t).num_minutes().max(0);
    match (now.date() - t.date()).num_days() {
        _ if mins < 1 => "just now".into(),
        0 if mins < 60 => if mins == 1 { "1 minute ago".into() } else { format!("{mins} minutes ago") },
        0 => if mins / 60 == 1 { "1 hour ago".into() } else { format!("{} hours ago", mins / 60) },
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

/// The runs in a log, oldest first: (backups, checks of the whole backup, repairs of copies). Previews left out.
fn runs_of(log: &Path) -> (Vec<Run>, Vec<Run>, Vec<Run>) {
    let Some(text) = read_tail(log, 600_000) else { return Default::default() };
    let start = if text.starts_with("====") { 0 } else { text.find("\n====================").map(|i| i + 1).unwrap_or(0) };
    let (mut backups, mut checks, mut recopies) = (vec![], vec![], vec![]);
    for r in parse_runs(&text[start..]).into_iter().filter(|r| !r.preview) {
        if r.check.is_some() {
            checks.push(r);
        } else if r.recopy {
            recopies.push(r);
        } else {
            backups.push(r);
        }
    }
    (backups, checks, recopies)
}

/// The last 14 nights, oldest first: "done" (a backup finished that day), "problem" (it ran and had one),
/// "missed" (none ran, after the first backup ever), "none" (before the first backup, or today before one ran).
/// With each, who started the run that decided it: "nightly" (the nightly task) or "hand" (anything else),
/// None when no run did.
/// Who started a run: "nightly" (the nightly task) or "hand".
fn by(r: &Run) -> &'static str {
    if r.nightly { "nightly" } else { "hand" }
}

fn nights(runs: &[Run], today: chrono::NaiveDate) -> Vec<(&'static str, Option<&'static str>)> {
    let first = runs.iter().filter_map(|r| r.when).map(|t| t.date()).min();
    (0..14)
        .rev()
        .map(|back| {
            let day = today - chrono::Duration::days(back);
            let that: Vec<&Run> = runs.iter().filter(|r| r.when.is_some_and(|t| t.date() == day)).collect();
            if let Some(r) = that.iter().rev().find(|r| r.error.is_none() && r.headline().1 != Tone::Bad) {
                ("done", Some(by(r)))
            } else if let Some(r) = that.last() {
                ("problem", Some(by(r)))
            } else if first.is_some_and(|f| day > f) && day < today {
                ("missed", None)
            } else {
                ("none", None)
            }
        })
        .collect()
}

/// A file's album, as the backup's folders name it: its first two folders ("Artist\Album").
fn album_of(rel: &str) -> String {
    let parts: Vec<&str> = rel.split(['\\', '/']).collect();
    if parts.len() >= 3 { format!("{}\\{}", parts[0], parts[1]) } else if parts.len() == 2 { parts[0].to_string() } else { String::new() }
}

/// The damaged files, album by album.
fn by_album(rels: &[String]) -> Vec<Value> {
    let mut albums: Vec<(String, Vec<String>)> = vec![];
    for r in rels {
        let a = album_of(r);
        match albums.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&a)) {
            Some((_, files)) => files.push(r.clone()),
            None => albums.push((a, vec![r.clone()])),
        }
    }
    albums.into_iter().map(|(album, files)| json!({ "album": album, "files": files })).collect()
}

fn root_of(folder: &str) -> Option<String> {
    folder.get(..2).filter(|r| r.ends_with(':')).map(|r| format!("{}\\", r.to_uppercase()))
}

/// Where a backup's folder is now, if its drive is here: the main one at its saved place, the second one on
/// whichever drive holds its marker today.
fn folder_now(at: &Places, saved: &Saved, which: Which) -> Option<String> {
    match which {
        Which::Main => (!saved.dest.is_empty() && Path::new(&saved.dest).is_dir()).then(|| saved.dest.clone()),
        Which::Second => find_second(saved, &at.disks.roots()),
    }
}

/// The folder a backup that is here and confirmed lives in, checked the way a run checks it (the disk's
/// name, the folder's identity and its marker). Err in the worker's own words.
pub(crate) fn confirmed_folder(setup: &Setup, which: Which, library: &Path) -> Result<PathBuf, String> {
    let at = setup.places();
    let saved = read_config(&at.config);
    let folder = match which {
        Which::Main => {
            if saved.dest.is_empty() {
                return Err("No backup folder has been chosen yet.".into());
            }
            let root = root_of(&saved.dest).ok_or("The saved backup path has no drive letter.")?;
            let label = at.disks.label(&root).ok_or("The backup drive is not connected.")?;
            if !saved.label.is_empty() && !label.eq_ignore_ascii_case(&saved.label) {
                return Err("The disk at the backup drive letter is not the saved disk. Nothing was copied.".into());
            }
            saved.dest.clone()
        }
        Which::Second => find_second(&saved, &at.disks.roots()).ok_or("The second backup drive is not connected.")?,
    };
    native::validate_binding(&at, which, library, Path::new(&folder))?;
    Ok(PathBuf::from(folder))
}

/// The confirmed backup's marker (what `_backup-id.txt` holds), the name moves already followed are kept
/// under. None until it is confirmed.
pub(crate) fn marker(setup: &Setup, which: Which) -> Option<String> {
    native::bound_marker(&setup.places(), which)
}

// ------------------------------------------------------------------ status

/// The drive band of a card: is the drive here, is it the right disk, are the folders still the confirmed
/// ones.
fn drive_of(at: &Places, saved: &Saved, which: Which, library: &Path) -> (Value, Option<String>) {
    let (set_up, label) = match which {
        Which::Main => (!saved.dest.is_empty(), saved.label.clone()),
        Which::Second => (!saved.second_id.is_empty() && !saved.second_folder.is_empty(), saved.second_label.clone()),
    };
    if !set_up {
        return (json!({ "state": "none", "name": null, "letter": null, "free": null, "total": null, "words": "" }), None);
    }
    let name = |l: &str, letter: &str| if l.is_empty() { format!("Drive {letter}") } else { l.to_string() };
    match which {
        Which::Main => {
            let letter = saved.dest.get(..2).unwrap_or("").to_uppercase();
            let shown = name(&label, &letter);
            let root = root_of(&saved.dest).unwrap_or_default();
            match at.disks.label(&root) {
                None => (
                    json!({ "state": "away", "name": shown, "letter": letter, "free": null, "total": null,
                            "words": format!("{shown} is not connected. Nothing can be copied until it is back at {letter}.") }),
                    None,
                ),
                Some(found) if !label.is_empty() && !found.eq_ignore_ascii_case(&label) => (
                    json!({ "state": "wrong_disk", "name": shown, "letter": letter, "found": found, "free": null, "total": null,
                            "words": format!("The disk at {letter} is \"{found}\", not \"{label}\". Nothing will be copied to it.") }),
                    None,
                ),
                Some(_) => {
                    let space = at.disks.space(&root);
                    let (state, words) = match native::validate_binding(at, which, library, Path::new(&saved.dest)) {
                        Ok(()) => ("here", String::new()),
                        // (another disk with the same name: its folder's identity tells)
                        Err(why) if code_of(&why) == "wrong_disk" => ("wrong_disk", why),
                        Err(why) => ("needs_confirm", why),
                    };
                    (
                        json!({ "state": state, "name": shown, "letter": letter, "free": space.map(|s| s.0), "total": space.map(|s| s.1), "words": words }),
                        (state == "here").then(|| saved.dest.clone()),
                    )
                }
            }
        }
        Which::Second => match find_second(saved, &at.disks.roots()) {
            None => (
                json!({ "state": "away", "name": if label.is_empty() { "The second backup drive".to_string() } else { label.clone() }, "letter": null, "free": null, "total": null,
                        "words": "Not plugged in" }),
                None,
            ),
            Some(folder) => {
                let letter = folder.get(..2).unwrap_or("").to_uppercase();
                let space = at.disks.space(&folder);
                let (state, words) = match native::validate_binding(at, which, library, Path::new(&folder)) {
                    Ok(()) => ("here", String::new()),
                    Err(why) if code_of(&why) == "wrong_disk" => ("wrong_disk", why),
                    Err(why) => ("needs_confirm", why),
                };
                (
                    json!({ "state": state, "name": name(&label, &letter), "letter": letter, "free": space.map(|s| s.0), "total": space.map(|s| s.1), "words": words }),
                    (state == "here").then_some(folder),
                )
            }
        },
    }
}

/// One card of the Backup page: the drive, the last backup with its 14 nights, the last whole check, and
/// whether "Back up now" can run.
fn card(at: &Places, saved: &Saved, which: Which, library: &Path, now: NaiveDateTime) -> (Value, Option<Run>, bool) {
    let (drive, here) = drive_of(at, saved, which, library);
    let set_up = drive["state"] != "none";
    let (backups, checks, _) = runs_of(&at.logs.join(which.log_name()));
    let last_ok = match which {
        Which::Main => saved.last,
        Which::Second => saved.second_last,
    };
    let last = backups.last().cloned();
    let nights = nights(&backups, now.date());
    let last_v = last.as_ref().map(|r| {
        let (text, tone) = r.headline();
        json!({ "when": r.when.map(iso), "words": r.when.map(when_text), "result": text, "tone": tone_name(tone), "brief": page::run_brief(r), "by": by(r) })
    });
    let check = checks.last().map(|r| {
        let c = r.check.clone().unwrap_or_default();
        let mut damaged: Vec<String> = c.damaged.iter().map(|l| damaged_rel(l)).collect();
        // (only the pairs that still differ, when the backup is here to look at)
        if let Some(folder) = &here {
            damaged = still_different(library, Path::new(folder), &damaged);
        }
        let (text, tone) = r.headline();
        json!({ "when": r.when.map(iso), "words": text, "tone": tone_name(tone), "complete": c.complete() && r.error.is_none(), "finished": c.finished,
                "files": c.files, "good": c.good, "missing": c.missing, "waiting": c.waiting, "damaged_count": c.damaged_count,
                "damaged": damaged, "albums": by_album(&damaged) })
    });
    let state = drive["state"].as_str().unwrap_or("none").to_string();
    let why_not = match state.as_str() {
        "here" => None,
        "none" => Some("Choose a drive first.".to_string()),
        "away" if which == Which::Second => Some(format!("Plug in {} to back up to it.", drive["name"].as_str().unwrap_or("the drive"))),
        _ => Some(drive["words"].as_str().unwrap_or_default().to_string()),
    };
    let busy = here.as_deref().is_some_and(native::busy);
    let v = json!({
        "set_up": set_up,
        "folder": match which { Which::Main => (!saved.dest.is_empty()).then(|| saved.dest.clone()), Which::Second => here.clone() },
        "drive": drive,
        "last": last_v,
        "last_finished": last_ok.map(iso),
        "last_finished_words": last_ok.map(|t| ago_words(t, now)),
        "nights": nights.iter().map(|n| n.0).collect::<Vec<_>>(),
        "nights_by": nights.iter().map(|n| n.1).collect::<Vec<_>>(),
        "check": check,
        "can_back_up": why_not.is_none() && !busy,
        "why_not": why_not,
        "busy": busy,
    });
    (v, last, set_up)
}

/// `backup.status`: both cards, the nightly backup, what needs a look, and the sentence over them. Only
/// reads: the settings, the logs, the drives and Task Scheduler.
pub(crate) fn status(setup: &Setup, library: &Path, scheduler: &dyn Scheduler) -> Value {
    let at = setup.places();
    let saved = read_config(&at.config);
    let now = Local::now().naive_local();
    let (main, main_run, main_set) = card(&at, &saved, Which::Main, library, now);
    let (second, _, second_set) = card(&at, &saved, Which::Second, library, now);
    let task = scheduler.query();
    let schedule = match &task {
        Err(why) => json!({ "on": false, "known": false, "words": why, "next": null, "problem": null }),
        Ok(None) => json!({ "on": false, "known": true, "words": "No nightly backup is set up.", "next": null, "problem": null }),
        Ok(Some(n)) => {
            let at_time = n.schedule.split(" at ").nth(1).unwrap_or("").to_string();
            let words = if !n.enabled {
                "The nightly backup is turned off in Task Scheduler.".to_string()
            } else if at_time.is_empty() {
                "Every night, even when Cadence is closed".to_string()
            } else {
                format!("Every night at {at_time}, even when Cadence is closed")
            };
            let problem = n.result.filter(|c| ![0, 267009, 267011].contains(c)).map(|c| format!("The last nightly backup ended with a problem (code {c})."));
            json!({ "on": n.enabled, "known": true, "words": words, "at": at_time, "next": (!n.next.is_empty()).then(|| n.next.clone()), "running": n.running, "problem": problem })
        }
    };
    // what needs a look, the Music Tool's own rules (`alerts_for`), with the drives' words
    let as_task = match &task {
        Err(_) => Task::Unknown,
        Ok(None) => Task::Missing,
        Ok(Some(n)) => Task::Found { enabled: n.enabled, native: true, running: n.running, schedule: n.schedule.clone(), next: n.next.clone(), result: n.result },
    };
    let mut alerts: Vec<(String, bool)> = alerts_for(&Facts { chosen: main_set, last_run: main_run.as_ref(), last_ok: saved.last, task: &as_task, second_last: saved.second_last }, now);
    // (the nightly backup is the person's own choice in Cadence: its absence is a gentle word, not an alarm)
    for a in alerts.iter_mut().filter(|a| a.0.starts_with("No nightly backup") || a.0.starts_with("The nightly backup is turned off")) {
        a.1 = false;
    }
    for card in [&main, &second] {
        let d = &card["drive"];
        match d["state"].as_str() {
            Some("wrong_disk" | "needs_confirm") => alerts.insert(0, (d["words"].as_str().unwrap_or_default().to_string(), true)),
            Some("away") if card == &main => alerts.push((d["words"].as_str().unwrap_or_default().to_string(), true)),
            _ => {}
        }
        if card["check"]["damaged_count"].as_u64().is_some_and(|n| n > 0) && card["check"]["damaged"].as_array().is_some_and(|a| !a.is_empty()) {
            alerts.push((card["check"]["words"].as_str().unwrap_or_default().to_string(), true));
        }
    }
    let serious: Vec<&str> = alerts.iter().filter(|a| a.1).map(|a| a.0.as_str()).collect();
    let (state, words, loud, line) = if !main_set && !second_set {
        ("not_set_up", "No backup yet".to_string(), "No backup".to_string(), "Pick a drive and Cadence copies your music and its own library there every night.".to_string())
    } else if !serious.is_empty() {
        ("needs_look", "Your backups need a look".to_string(), "need a look".to_string(), serious[0].to_string())
    } else {
        let newest = [saved.last, saved.second_last].into_iter().flatten().max();
        match newest {
            Some(t) => {
                let ago = ago_words(t, now);
                let brief = main["last"]["brief"].as_str().unwrap_or_default().to_string();
                ("good", format!("Backed up {ago}"), ago, brief)
            }
            None => ("needs_look", "No backup has finished yet".to_string(), "finished yet".to_string(), String::new()),
        }
    };
    json!({
        "state": state,
        "sentence": { "words": words, "loud": loud },
        "line": line,
        "alerts": alerts.iter().map(|(w, s)| json!({ "words": w, "serious": s })).collect::<Vec<_>>(),
        "main": main,
        "second": second,
        "schedule": schedule,
    })
}

// ------------------------------------------------------------------ choosing a drive and folders

/// How many files the library holds and how many bytes they take. Only reads.
pub(crate) fn library_size(library: &Path) -> Result<(u64, u64), String> {
    native::sizes(library, false)
}

/// `backup.drives`: the drives a backup could go to, as tiles. Never the library's own drive, never a
/// network drive or a CD. Only reads.
pub(crate) fn drives(setup: &Setup, library: &Path, size: Option<(u64, u64)>) -> Value {
    let at = setup.places();
    let saved = read_config(&at.config);
    let lib = library.to_string_lossy().to_lowercase();
    let second_now = find_second(&saved, &at.disks.roots());
    let mut tiles = vec![];
    for root in at.disks.roots() {
        let r = root.trim_end_matches('\\').to_lowercase();
        // (the library's own drive is no place for its backup)
        if lib == r || lib.starts_with(&(r.clone() + "\\")) || !at.disks.usable(&root) {
            continue;
        }
        let Some(label) = at.disks.label(&root) else { continue };
        let Some((free, total)) = at.disks.space(&root) else { continue };
        let letter = root.get(..2).unwrap_or("").to_uppercase();
        let folder = Path::new(&root).join(BACKUP_FOLDER).to_string_lossy().to_string();
        let used_by = if !saved.dest.is_empty() && saved.dest.to_lowercase().starts_with(&r) {
            Some("main")
        } else if second_now.as_deref().is_some_and(|s| s.to_lowercase().starts_with(&r)) {
            Some("second")
        } else {
            None
        };
        let needs = size.map(|s| s.1);
        let fits = needs.is_none_or(|n| n <= free);
        let words = match needs {
            Some(n) if fits => format!("{} free · your music needs {}", fmt_size(free), fmt_size(n)),
            Some(n) => format!("{} free · not enough for your music ({})", fmt_size(free), fmt_size(n)),
            None => format!("{} free", fmt_size(free)),
        };
        tiles.push(json!({
            "root": root, "letter": letter, "name": if label.is_empty() { format!("Drive {letter}") } else { label.clone() }, "label": label,
            "removable": at.disks.removable(&root), "free": free, "total": total, "fits": fits,
            "folder": folder, "has_backup": Path::new(&folder).join(ID_FILE).is_file(), "used_by": used_by, "words": words,
        }));
    }
    json!({ "library": size.map(|(files, bytes)| json!({ "files": files, "bytes": bytes })), "drives": tiles })
}

/// The three rules the confirm step shows (picture 10).
pub(crate) const RULES: [&str; 3] = [
    "Nothing is copied until you confirm.",
    "Each folder is known by its identity. If either folder is a different one, even at the same drive letter, nothing is copied or moved until you confirm again.",
    "Files are never deleted from the backup. Files that leave your library move to backup history and are kept 30 days.",
];

/// `backup.preview_folders`: both folders side by side, with their sizes, what a first backup would copy,
/// and what stands in the way. Only reads; nothing is enrolled.
pub(crate) fn preview_folders(setup: &Setup, which: Which, library: &Path, to: &Path, size: Option<(u64, u64)>) -> Value {
    let at = setup.places();
    let saved = read_config(&at.config);
    let main = (which == Which::Second).then(|| Path::new(&saved.dest));
    let mut problem = native::safe_roots(library, to, main).err();
    let root = to.to_string_lossy().get(..3).map(str::to_string).unwrap_or_default();
    let label = at.disks.label(&root);
    if problem.is_none() && label.is_none() {
        problem = Some("That drive is not connected.".into());
    }
    let space = at.disks.space(&root);
    let has_backup = to.join(ID_FILE).is_file();
    let existing = if to.is_dir() { native::sizes(to, true).ok() } else { None };
    let copy = if problem.is_none() { native::to_copy(library, to).map_err(|e| problem = Some(e)).ok() } else { None };
    let fits = match (copy, space) {
        (Some((_, bytes)), Some((free, _))) => bytes <= free,
        _ => true,
    };
    if problem.is_none() && !fits {
        problem = Some(format!("Not enough room: the first backup needs {}, and the drive has {}.", fmt_size(copy.map(|c| c.1).unwrap_or(0)), fmt_size(space.map(|s| s.0).unwrap_or(0))));
    }
    // a backup already confirmed elsewhere is replaced by this one (its copies stay where they are)
    let current = match which {
        Which::Main => (!saved.dest.is_empty()).then(|| saved.dest.clone()),
        Which::Second => find_second(&saved, &at.disks.roots()),
    };
    let replaces = current.filter(|c| !c.eq_ignore_ascii_case(&to.to_string_lossy()));
    let letter = root.get(..2).unwrap_or("").to_uppercase();
    json!({
        "which": which_name(which),
        "library": { "path": library, "files": size.map(|s| s.0), "bytes": size.map(|s| s.1) },
        "backup": { "path": to, "exists": to.is_dir(), "has_backup": has_backup, "files": existing.map(|s| s.0), "bytes": existing.map(|s| s.1),
                    "drive": label.as_ref().map(|l| if l.is_empty() { format!("Drive {letter}") } else { l.clone() }), "letter": letter,
                    "free": space.map(|s| s.0), "total": space.map(|s| s.1) },
        "to_copy": copy.map(|(files, bytes)| json!({ "files": files, "bytes": bytes })),
        "fits": fits,
        "replaces": replaces,
        "rules": RULES,
        "words": if has_backup { "This folder already holds a backup. It is kept and brought up to date; nothing in it is deleted." } else { "" },
        "problem": problem,
        "can_confirm": problem.is_none(),
    })
}

/// `backup.confirm_folders`: the only way a library and backup folder pair is enrolled (the worker's own
/// `confirm_folders`: identities bound, the marker made or kept). Copies nothing.
pub(crate) fn confirm(setup: &Setup, which: Which, library: &Path, to: &Path) -> Result<String, String> {
    native::confirm_folders(&setup.places(), which, library, to)
}

// ------------------------------------------------------------------ runs

/// What a run is asked to do.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RunKind {
    Backup,
    /// a backup that also moves a great many removed files to backup history (asked for by a click, after
    /// a run stopped short of it)
    Mirror,
    /// what a backup would copy; nothing is written
    Preview,
    /// every file in the library and its copy, compared; nothing is written to the backup
    Check,
    /// the damaged copies named, made again from the library (the copies they replace kept in history)
    Recopy(Vec<String>),
}

/// Runs the worker and gives its progress as events (at most every 200 ms, and at each stage), then answers
/// what it did. The second part is what following the moves did, when it got that far (whatever happened
/// after), so the caller can mark the moves as followed.
pub(crate) fn run(setup: &Setup, which: Which, kind: RunKind, library: &Path, moves: &[Moved], stop: &AtomicBool, progress: &mut dyn FnMut(Value)) -> (Result<Value, String>, Option<Followed>) {
    let at = setup.places();
    let mut list_file = None;
    let native_kind = match &kind {
        RunKind::Backup => Kind::Backup,
        RunKind::Mirror => Kind::Mirror,
        RunKind::Preview => Kind::Preview,
        RunKind::Check => Kind::CheckAll,
        RunKind::Recopy(rels) => {
            let list = setup.dir.join(format!("recopy-{:x}.txt", crate::importer::scan::rand_id()));
            if let Err(e) = std::fs::create_dir_all(&setup.dir).and_then(|_| std::fs::write(&list, rels.join("\r\n"))) {
                return (Err(crate::util::plain_err(format!("The list of copies to make again could not be written: {e}"))), None);
            }
            list_file = Some(list.clone());
            Kind::Recopy(list)
        }
    };
    let lines: RefCell<Vec<String>> = RefCell::default();
    let totals = Cell::new((0u64, 0u64));
    let copied = Cell::new((0u64, 0u64));
    let followed: Cell<Option<Followed>> = Cell::new(None);
    let last = Cell::new(Instant::now() - Duration::from_secs(1));
    let saving = setup.snapshots != Snapshots::Nothing;
    let progress = RefCell::new(progress);
    let emit = |force: bool, v: Value| {
        if force || last.get().elapsed() >= Duration::from_millis(200) {
            last.set(Instant::now());
            (progress.borrow_mut())(v);
        }
    };
    let mut send = |m: Msg| match m {
        Msg::Line(l) => {
            if l.starts_with("Comparing library with backup") {
                emit(true, json!({ "stage": "comparing", "words": "Comparing your library with the backup" }));
            } else if let Some(c) = rx().to_copy.captures(&l) {
                let files = c[1].parse::<u64>().unwrap_or(0) + c[2].parse::<u64>().unwrap_or(0);
                totals.set((files, parse_size(&c[3])));
                emit(true, json!({ "stage": "copying", "done": 0, "total": files, "done_bytes": 0, "total_bytes": totals.get().1, "words": format!("Copying {}", plural_commas(files, "file")) }));
            } else if let Some(c) = rx().checking.captures(&l) {
                let (done, total) = (c[1].parse::<u64>().unwrap_or(0), c[2].parse::<u64>().unwrap_or(0));
                emit(done == total, json!({ "stage": "checking", "done": done, "total": total, "words": format!("Checked {} of {} files, {} of {}", commas(done.saturating_sub(1)), commas(total), &c[3], &c[4]) }));
            } else if saving && (l.starts_with("Backup is already up to date.") || rx().verified.is_match(&l)) {
                emit(true, json!({ "stage": "saving", "words": "Saving Cadence's library and settings" }));
            } else if l.starts_with("Renamed ") && l.contains("to follow moves") {
                emit(true, json!({ "stage": "following", "words": l.trim_end_matches('.') }));
            }
            lines.borrow_mut().push(l);
        }
        Msg::Copied(files, bytes) => {
            copied.set((files, bytes));
            let (tf, tb) = totals.get();
            emit(files == tf, json!({ "stage": "copying", "done": files, "total": tf, "done_bytes": bytes, "total_bytes": tb,
                                       "words": format!("Copied and checked {} of {} files, {} of {}", commas(files), commas(tf), fmt_size(bytes), fmt_size(tb)) }));
        }
        _ => {}
    };
    let ended = native::execute_following(&at, which, &native_kind, library, moves, &mut |f| followed.set(Some(f)), stop, &mut send);
    if let Some(list) = list_file {
        let _ = std::fs::remove_file(list);
    }
    let run = parse_runs(&lines.borrow().join("\n")).pop().unwrap_or_default();
    let f = followed.get();
    let (files, bytes) = copied.get();
    let renamed = f.map(|f| f.renamed).unwrap_or(0);
    let value = match &kind {
        RunKind::Check => {
            let Some(c) = run.check.clone().filter(|c| c.finished) else { return (Err(ended.err().unwrap_or_else(|| "The check stopped before it finished.".into())), f) };
            let damaged: Vec<String> = c.damaged.iter().map(|l| damaged_rel(l)).collect();
            let words = if c.complete() { format!("Checked {}: every copy matches", plural_commas(c.files, "file")) } else { run.headline().0 };
            json!({ "which": which_name(which), "files": c.files, "good": c.good, "missing": c.missing, "waiting": c.waiting, "damaged_count": c.damaged_count,
                    "damaged": damaged, "albums": by_album(&damaged), "complete": c.complete(), "words": words })
        }
        _ if ended.is_err() => return (Err(ended.unwrap_err()), f),
        RunKind::Preview => json!({
            "which": which_name(which), "to_copy": { "files": run.new_files + run.changed_files, "bytes": parse_size(&run.size) },
            "new_files": run.new_files, "changed_files": run.changed_files,
            "new_albums": run.new_albums.iter().map(|a| json!({ "name": a.name, "detail": a.detail })).collect::<Vec<_>>(),
            "changed_albums": run.changed_albums.iter().map(|a| json!({ "name": a.name, "detail": a.detail })).collect::<Vec<_>>(),
            "removed": run.extra_files, "words": if run.new_files + run.changed_files == 0 { "Nothing new to copy.".to_string() } else { format!("A backup now would copy {} ({}).", plural_commas(run.new_files + run.changed_files, "file"), run.size) },
        }),
        RunKind::Recopy(_) => json!({ "which": which_name(which), "repaired": run.repaired, "not_repaired": run.not_repaired, "words": run.headline().0, "bytes": bytes }),
        RunKind::Backup | RunKind::Mirror => {
            let words = if files > 0 { format!("Backup finished. Copied {} ({}).", plural_commas(files, "file"), fmt_size(bytes)) } else { "Backup finished. Nothing new to copy.".to_string() };
            json!({
                "which": which_name(which), "copied": files, "bytes": bytes, "new_files": run.new_files, "changed_files": run.changed_files,
                "verified": run.verified.as_ref().map(|v| v.0).unwrap_or(0), "kept_old": run.kept_old.unwrap_or(0), "removed": run.moved.unwrap_or(0),
                "renamed": renamed, "up_to_date": run.up_to_date, "cadence_saved": run.cadence.is_some(),
                "new_albums": run.new_albums.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                "changed_albums": run.changed_albums.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                "words": words, "brief": page::run_brief(&run),
            })
        }
    };
    (Ok(value), f)
}

// ------------------------------------------------------------------ damaged copies

/// Which file of each damaged pair is sound, by listening to both (`judge_pair`). Changes nothing.
/// `each` hears each verdict as it comes; Err when a pair is audio and ffmpeg is not there.
pub(crate) fn find_out(library: &Path, backup: &Path, rels: &[String], stop: &AtomicBool, each: &mut dyn FnMut(&str, &'static str, &'static str)) -> Result<Vec<Value>, String> {
    let mut tools: Option<(PathBuf, PathBuf)> = None;
    let mut out = vec![];
    for rel in rels {
        if stop.load(Ordering::Relaxed) {
            return Err("Stopped. Nothing was changed.".into());
        }
        let sound = match (inside(library, rel), inside(backup, rel)) {
            (Some(a), Some(b)) => {
                let audio = a.extension().is_some_and(|e| crate::importer::scan::is_audio_ext(&format!(".{}", e.to_string_lossy().to_lowercase())));
                if audio && a.is_file() && b.is_file() && tools.is_none() {
                    let ffmpeg = crate::importer::tools::find_tool("ffmpeg")?;
                    let ffprobe = crate::importer::tools::find_tool("ffprobe")?;
                    tools = Some((ffmpeg, ffprobe));
                }
                match &tools {
                    Some((ffmpeg, ffprobe)) => judge_pair(ffmpeg, ffprobe, &a, &b),
                    // (no audio to listen to: judge_pair answers without ffmpeg)
                    None => judge_pair(Path::new(""), Path::new(""), &a, &b),
                }
            }
            _ => Sound::Gone,
        };
        let name = match sound {
            Sound::Library => "library",
            Sound::Copy => "copy",
            Sound::Both => "both",
            Sound::Neither => "neither",
            Sound::NotAudio => "not_audio",
            Sound::Gone => "gone",
        };
        each(rel, name, sound.words());
        out.push(json!({ "file": rel, "sound": name, "words": sound.words() }));
    }
    Ok(out)
}

/// Brings sound copies back from the backup over the damaged files in the library, as one batch of kind
/// "restore" in Repair history (Undo puts the damaged files back). The backup's folder must be the
/// confirmed one (checked under its lock, so no backup writes it meanwhile); nothing in it is changed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bring_back_files(
    setup: &Setup,
    which: Which,
    library: &Path,
    rels: &[String],
    tools: Option<(&Path, &Path)>,
    vet: &mut dyn FnMut(&Path) -> Result<(), String>,
    stopped: &dyn Fn() -> bool,
) -> Result<crate::repair::Done, String> {
    let folder = confirmed_folder(setup, which, library)?;
    let _lock = native::RunLock::take(&folder)?;
    native::validate_binding(&setup.places(), which, library, &folder)?;
    bring_back(library, &folder, &crate::repair::backups_root(library), rels, tools, "restore", vet, stopped)
}


// ------------------------------------------------------------------ eject

/// `backup.eject`: asks Windows to let go of the backup's drive, and says so only once it really has.
pub(crate) fn eject_drive(setup: &Setup, which: Which) -> Result<String, String> {
    let at = setup.places();
    let saved = read_config(&at.config);
    let folder = folder_now(&at, &saved, which).ok_or("That drive is not plugged in.")?;
    let root = root_of(&folder).ok_or("That drive cannot be ejected.")?;
    if !at.disks.removable(&root) {
        return Err("This drive is not one that can be ejected.".into());
    }
    if native::busy(&folder) {
        return Err("A backup to this drive is running. Do not pull it out yet.".into());
    }
    at.disks.eject(&folder)?;
    let name = match which {
        Which::Main => saved.label,
        Which::Second => saved.second_label,
    };
    Ok(format!("{} can be pulled out now.", if name.is_empty() { "The drive".to_string() } else { name }))
}

// ------------------------------------------------------------------ the nightly task

/// Makes Cadence's nightly task: every day at `at` ("03:00"), `exe --backup-nightly --data <data>`, with no
/// window, so it runs with Cadence closed. Only after the main backup is confirmed. Never touches the Music
/// Tool's task.
pub(crate) fn schedule_on(setup: &Setup, scheduler: &dyn Scheduler, library: &Path, exe: &Path, data: &Path, at: &str) -> Result<(), String> {
    let ok_time = at.len() == 5 && chrono::NaiveTime::parse_from_str(at, "%H:%M").is_ok();
    if !ok_time {
        return Err("The time must be like 03:00.".into());
    }
    confirmed_folder(setup, Which::Main, library).map_err(|why| format!("Confirm the main backup first. {why}"))?;
    let args = nightly_args(data)?;
    scheduler.create(exe, &args, at, &setup.dir.join("logs"))
}
