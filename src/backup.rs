//! The Backup page and its native worker. It only reads the library, keeps changed and removed files,
//! verifies fresh copies, and refuses a different disk at the saved drive letter. Existing settings
//! and summary logs are read in place. A lab uses only its own settings, folders, and logs.
use crate::config::Config;
use crate::theme::{self, ACCENT2, DIM, FAINT, GREEN, RED, TEXT, WARN};
use chrono::{Local, NaiveDateTime, NaiveTime};
use crossbeam_channel::{Receiver, Sender};
use egui::{vec2, Align, Color32, FontId, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke, Ui};
use regex::Regex;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

mod page;
mod native;
mod sqlite;
mod schedule;

pub fn run_if_requested() -> Option<i32> { schedule::requested() }

const TASK_NAME: &str = "Music Backup (nightly)";
/// CREATE_NO_WINDOW: the child gets no console window.
const NO_WINDOW: u32 = 0x0800_0000;

/// The backup settings folder; empty until the settings were seen,
/// and when nothing was chosen there.
static TOOL_DIR: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// The backup settings folder: the one chosen in Settings. None when nothing is
/// chosen there. (It used to fall back on a folder fixed when the app was built, which only exists on
/// the PC it was built on and put that PC's paths into the exe.)
fn tool_dir() -> Option<PathBuf> {
    tool_dir_for(&TOOL_DIR.read().unwrap_or_else(|e| e.into_inner()))
}

pub fn tool_dir_for(setting: &str) -> Option<PathBuf> {
    let chosen = setting.trim().trim_matches('"').trim_end_matches(['\\', '/']);
    (!chosen.is_empty()).then(|| PathBuf::from(chosen))
}

/// Where backup settings and logs are, and whether this is a test run.
struct Places {
    config: PathBuf,
    logs: PathBuf,
    /// a test run: settings and logs are in the test's own folder, and the worker leaves
    /// Cadence, the music tool's data and the code alone
    test: bool,
}

/// Existing backups keep their settings and logs in their original folder. A test run (`MUSIC_RECS_DATA` set) has its
/// own in `<that folder>\backup`, so nothing a test does can reach the real backup.
fn places() -> Places {
    // nothing chosen: a folder of the app's own that is never there, so nobody can find settings
    // or a log by accident in whatever folder the app happened to be started from
    let tool = tool_dir().unwrap_or_else(|| crate::config::app_dir().join("no-backup-tool-chosen"));
    places_for(&tool, std::env::var_os("MUSIC_RECS_DATA"))
}

fn places_for(tool: &Path, data: Option<std::ffi::OsString>) -> Places {
    match data.filter(|d| !d.is_empty()) {
        Some(d) => {
            let base = PathBuf::from(d).join("backup");
            Places { config: base.join("config.json"), logs: base.join("logs"), test: true }
        }
        None => Places { config: tool.join("config.json"), logs: tool.join("logs"), test: false },
    }
}

/// Which of the two backups: the main one (the nightly task's) or the second, on a drive plugged in by hand.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
enum Which {
    #[default]
    Main,
    Second,
}

impl Which {
    fn log_name(self) -> &'static str {
        match self {
            Which::Main => "backup-log.txt",
            Which::Second => "backup-log-second.txt",
        }
    }
}

// ------------------------------------------------------------------ what the script writes

/// The three things a run saves besides the music, each with a line of its own in the log.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Part {
    LegacyPlayer, // Historical logs remain readable.
    Cadence,
    Tool,
    Code,
}

impl Part {
    fn failed(self) -> &'static str {
        match self {
            Part::Cadence => "The Cadence copy had a problem",
            Part::LegacyPlayer => "The previous player data copy had a problem",
            Part::Tool => "The music tool's data could not be saved",
            Part::Code => "The code could not be saved",
        }
    }
}

#[derive(Clone, Default)]
struct Album {
    name: String,
    /// "5 files, 32 MB" or "11 files"
    detail: String,
}

/// One run of the script, read from its log (or from what it printed).
#[derive(Clone, Default)]
struct Run {
    when: Option<NaiveDateTime>,
    has_plan: bool,
    new_files: u64,
    changed_files: u64,
    size: String,
    new_albums: Vec<Album>,
    new_count: usize,
    changed_albums: Vec<Album>,
    changed_count: usize,
    extra_dirs: u64,
    extra_files: u64,
    extras: Vec<String>,
    preview: bool,
    up_to_date: bool,
    cancelled: bool,
    /// files, size, time taken
    copied: Option<(u64, String, String)>,
    problems: u64,
    robocopy_fail: bool,
    moved: Option<u64>,
    /// files, size
    legacy_player: Option<(u64, String)>,
    cadence: Option<(u64, String)>,
    /// the music tool's own data, and the code: files, size
    tool: Option<(u64, String)>,
    code: Option<(u64, String)>,
    /// what went wrong while saving one of those, named for the part it belongs to
    part_problems: Vec<(Part, String)>,
    /// the header said "(second backup)"
    second: bool,
    /// old copies of changed files moved to `_changed` before the new ones were copied
    kept_old: Option<u64>,
    /// old copies that could not be kept and were replaced
    old_lost: u64,
    /// the safety catch: this many of the backup's files (of so many) are no longer in the library, and
    /// were left where they are because that is a lot at once
    too_many: Option<(u64, u64)>,
    /// the copied files read back and compared with the library: how many, and how long it took ("" when
    /// some were bad)
    verified: Option<(u64, String)>,
    /// how many were not good copies, and the ones the log names
    bad_count: u64,
    bad_copies: Vec<String>,
    /// a check of the whole backup (`-CheckAll`): nothing was copied
    check: Option<Check>,
    /// "Room left on the second backup drive"
    room_left: Option<String>,
    /// a run that repaired damaged copies from the library (`-Recopy`): it copied nothing else
    recopy: bool,
    repaired: u64,
    not_repaired_count: u64,
    not_repaired: Vec<String>,
    /// sets of removed files (`_deleted`) older than 30 days that went to the Recycle Bin
    cleared_removed: Option<u64>,
    /// tidying steps that failed: a removed file that could not be moved aside, an old set that could
    /// not be cleared ("  couldn't move ...", "  couldn't clear ...")
    step_fails: Vec<String>,
    error: Option<String>,
    closed: bool,
}

/// What a check of a whole backup found.
#[derive(Clone, Default, PartialEq, Debug)]
struct Check {
    /// false until its closing line was read (a check that was stopped has none)
    finished: bool,
    files: u64,
    time: String,
    good: u64,
    damaged_count: u64,
    /// the damaged copies the log names
    damaged: Vec<String>,
    /// new or changed since the last backup: the next backup copies them
    waiting: u64,
    missing: u64,
}

impl Check {
    fn complete(&self) -> bool {
        self.finished && self.files > 0 && self.good == self.files && self.damaged_count == 0 && self.waiting == 0 && self.missing == 0
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tone {
    Good,
    Bad,
    Unsure,
    Plain,
}

impl Run {
    /// How the run ended, in one line.
    fn headline(&self) -> (String, Tone) {
        if let Some(e) = &self.error {
            let what = if self.check.as_ref().is_some_and(|c| c.finished) { "Check incomplete" } else if self.has_plan { "Stopped" } else { "Did not start" };
            return (crate::util::plain_err(format!("{what}: {e}")), Tone::Bad);
        }
        if let Some(c) = &self.check {
            return match (c.finished, c.damaged_count) {
                (false, _) => ("The check was stopped before it finished".into(), Tone::Unsure),
                // a check that read nothing has checked nothing: the library folder was not there or not readable
                (true, 0) if c.files == 0 => ("The check found no files in the library. Is its drive connected?".into(), Tone::Bad),
                (true, 0) if c.complete() =>(format!("Checked {} in {}: all good copies", plural(c.good, "file"), c.time), Tone::Good),
                (true, 0) => (format!("{} of {} copies verified; backup incomplete", c.good, c.files), Tone::Unsure),
                (true, n) => (format!("Checked {}: {}", plural(c.files, "file"), if n == 1 { "1 damaged copy".to_string() } else { format!("{n} damaged copies") }), Tone::Bad),
            };
        }
        if self.recopy {
            let fixed = if self.repaired == 1 { "Repaired 1 copy from the library".to_string() } else { format!("Repaired {} copies from the library", commas(self.repaired)) };
            return match (self.repaired, self.not_repaired_count) {
                (0, 0) => ("There was nothing to repair".into(), Tone::Plain),
                (_, 0) => (fixed, Tone::Good),
                (0, n) => (format!("{} could not be repaired", if n == 1 { "1 copy".to_string() } else { format!("{} copies", commas(n)) }), Tone::Bad),
                (_, n) => (format!("{fixed}, but {} could not be repaired", if n == 1 { "1".to_string() } else { commas(n) }), Tone::Bad),
            };
        }
        if let Some((files, size, time)) = &self.copied {
            let copied = format!("Copied {} ({size})", plural(*files, "file"));
            if self.problems > 0 {
                return (format!("{copied} with {}", plural(self.problems, "problem")), Tone::Bad);
            }
            if self.robocopy_fail {
                return (format!("{copied}, but some files could not be copied"), Tone::Bad);
            }
            if self.bad_count > 0 {
                return (format!("{copied}, but {} not good copies", if self.bad_count == 1 { "1 is".to_string() } else { format!("{} are", self.bad_count) }), Tone::Bad);
            }
            if let Some((part, _)) = self.part_problems.first() {
                return (format!("{copied}. {}", part.failed()), Tone::Bad);
            }
            if !self.step_fails.is_empty() {
                return (format!("{copied}, but {}", self.steps_failed()), Tone::Bad);
            }
            return (format!("{copied} in {time}"), Tone::Good);
        }
        if let Some((part, _)) = self.part_problems.first() {
            return (part.failed().into(), Tone::Bad);
        }
        if !self.step_fails.is_empty() {
            let words = self.steps_failed();
            let mut c = words.chars();
            return (c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or(words), Tone::Bad);
        }
        if self.up_to_date {
            return ("Already up to date".into(), Tone::Good);
        }
        if self.cancelled {
            return ("Canceled, nothing copied".into(), Tone::Plain);
        }
        if self.closed {
            return ("Nothing to copy".into(), Tone::Good);
        }
        ("Stopped before finishing".into(), Tone::Unsure)
    }

    /// "some removed files could not be moved aside (see the log)", for the headline.
    fn steps_failed(&self) -> String {
        let moving = self.step_fails.iter().any(|s| s.starts_with("couldn't move"));
        let clearing = self.step_fails.iter().any(|s| s.starts_with("couldn't clear"));
        match (moving, clearing) {
            (true, false) => "some files no longer in the library could not be moved aside (see the log)".into(),
            (false, true) => "old removed files could not be sent to the Recycle Bin (see the log)".into(),
            _ => "some tidying steps failed (see the log)".into(),
        }
    }

    /// "1 new album, 4 changed albums", or empty.
    fn albums_phrase(&self) -> String {
        let mut parts = vec![];
        if self.new_count > 0 {
            parts.push(format!("{} new", commas(self.new_count as u64)));
        }
        if self.changed_count > 0 {
            parts.push(format!("{} changed", commas(self.changed_count as u64)));
        }
        if parts.is_empty() {
            return String::new();
        }
        let n = self.new_count + self.changed_count;
        format!("{} {}", parts.join(", "), if n == 1 { "album" } else { "albums" })
    }
}

struct Rx {
    header: Regex,
    to_copy: Regex,
    new_head: Regex,
    changed_head: Regex,
    new_item: Regex,
    changed_item: Regex,
    extra_head: Regex,
    copied: Regex,
    problems: Regex,
    legacy_player: Regex,
    cadence: Regex,
    tool: Regex,
    code: Regex,
    moved: Regex,
    kept_old: Regex,
    old_lost: Regex,
    too_many: Regex,
    verified: Regex,
    bad_copies: Regex,
    checked_all: Regex,
    waiting: Regex,
    room_left: Regex,
    repaired: Regex,
    not_repaired: Regex,
    cleared_removed: Regex,
    checking: Regex,
    error: Regex,
    #[cfg(test)]
    robocopy_line: Regex,
}

fn rx() -> &'static Rx {
    static RX: OnceLock<Rx> = OnceLock::new();
    RX.get_or_init(|| {
        let r = |s: &str| Regex::new(s).expect("regex");
        Rx {
            header: r(r"^=+ (\d{4}-\d{2}-\d{2} \d{2}:\d{2}) =+$"),
            to_copy: r(r"^To copy: (\d+) new files, (\d+) changed files, (.+) in total\.$"),
            new_head: r(r"^New albums / folders \((\d+)\):"),
            changed_head: r(r"^Albums with new or changed files \((\d+)\):"),
            new_item: r(r"^  \+ (.+)  \((\d+) files, (.+)\)$"),
            changed_item: r(r"^  ~ (.+)  \((\d+) files\)$"),
            extra_head: r(r"^In the backup but no longer in the library: (\d+) folders, (\d+) files\."),
            copied: r(r"^Copied (\d+) files \((.+)\) in (.+)\.$"),
            problems: r(r"^(\d+) problems:"),
            cadence: r(r"^Cadence: saved .*\((\d+) files, ([^)]+)\) to "),
            legacy_player: r(r"^MusicBee: saved .*\((\d+) files, ([^)]+)\) to "),
            tool: r(r"^Music tool: saved .*\((\d+) files, ([^)]+)\) to "),
            code: r(r"^Code: saved .*\((\d+) files, ([^)]+)\) to "),
            moved: r(r"^Moved (\d+) removed items"),
            kept_old: r(r"^Kept the old copy of (\d+) changed files in "),
            old_lost: r(r"^  (\d+) old copies could not be kept"),
            too_many: r(r"^(\d+) of the backup's (\d+) files are no longer in the library\. That is a lot at once"),
            verified: r(r"^Checked (\d+) copied files against the library in (.+): all good\.$"),
            bad_copies: r(r"^Checked (\d+) copied files: (\d+) are NOT good copies\.$"),
            checked_all: r(r"^Checked (\d+) files in (.+): (\d+) good copies, (?:none damaged|(\d+) DAMAGED)\.$"),
            waiting: r(r"^(\d+) files are new or changed since the last backup"),
            room_left: r(r"^Room left on the second backup drive: (.+)\.$"),
            repaired: r(r"^Repaired (\d+) copies from the library\."),
            not_repaired: r(r"^(\d+) copies could NOT be repaired:"),
            cleared_removed: r(r"^Sent (\d+) sets of removed files older than \d+ days to the Recycle Bin\."),
            // the script's progress while it reads files back: "Checking 120 of 5000 files, 1.2 GB of 40 GB - about 12m 3s left"
            checking: r(r"^Checking (\d+) of (\d+) files, (.+?) of (.+?)(?: - about (.+) left)?$"),
            error: r(r"^ERROR(?: \((\d{4}-\d{2}-\d{2} \d{2}:\d{2})\))?: (.*)$"),
            // the same pattern the script uses to read robocopy's log
            #[cfg(test)]
            robocopy_line: r(r"^\s*(\*?[A-Za-z][A-Za-z ]*?)?\s*(-?\d+)\s+((?:[A-Za-z]:\\|\\\\).*)$"),
        }
    })
}

fn parse_stamp(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").ok()
}

/// Reads the script's summary text (its log, or what it printed) into runs, oldest first.
fn parse_runs(text: &str) -> Vec<Run> {
    #[derive(PartialEq)]
    enum List {
        None,
        New,
        Changed,
        Extra,
    }
    let rx = rx();
    let mut runs: Vec<Run> = vec![];
    let mut list = List::None;
    // the part whose line was read last: a "  problem: " line belongs to it
    let mut part: Option<Part> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            list = List::None;
            continue;
        }
        if let Some(c) = rx.header.captures(line) {
            runs.push(Run { when: parse_stamp(&c[1]), ..Default::default() });
            list = List::None;
            part = None;
            continue;
        }
        if let Some(c) = rx.error.captures(line) {
            // An error before the script got as far as its header (wrong disk, drive missing) has no
            // block of its own in the log: it follows the previous, finished run.
            if runs.last().map(|r| r.closed).unwrap_or(true) {
                runs.push(Run::default());
            }
            let run = runs.last_mut().unwrap();
            if let Some(d) = c.get(1) {
                run.when = run.when.or(parse_stamp(d.as_str()));
            }
            run.error = Some(c[2].trim().to_string());
            run.closed = true;
            continue;
        }
        let Some(run) = runs.last_mut() else { continue };
        if run.closed {
            continue;
        }
        if let Some(c) = rx.to_copy.captures(line) {
            run.has_plan = true;
            run.new_files = c[1].parse().unwrap_or(0);
            run.changed_files = c[2].parse().unwrap_or(0);
            run.size = c[3].to_string();
        } else if let Some(c) = rx.new_head.captures(line) {
            run.new_count = c[1].parse().unwrap_or(0);
            list = List::New;
        } else if let Some(c) = rx.changed_head.captures(line) {
            run.changed_count = c[1].parse().unwrap_or(0);
            list = List::Changed;
        } else if let Some(c) = rx.extra_head.captures(line) {
            run.extra_dirs = c[1].parse().unwrap_or(0);
            run.extra_files = c[2].parse().unwrap_or(0);
            list = List::Extra;
        } else if let (true, Some(c)) = (list == List::New, rx.new_item.captures(line)) {
            let files: u64 = c[2].parse().unwrap_or(0);
            run.new_albums.push(Album { name: album_name(&c[1]), detail: format!("{}, {}", plural(files, "file"), &c[3]) });
        } else if let (true, Some(c)) = (list == List::Changed, rx.changed_item.captures(line)) {
            let files: u64 = c[2].parse().unwrap_or(0);
            run.changed_albums.push(Album { name: album_name(&c[1]), detail: plural(files, "file") });
        } else if list == List::Extra && line.starts_with("  - ") {
            run.extras.push(album_name(line[4..].trim_end_matches('\\')));
        } else if let Some(c) = rx.copied.captures(line) {
            run.copied = Some((c[1].parse().unwrap_or(0), c[2].to_string(), c[3].to_string()));
        } else if let Some(c) = rx.problems.captures(line) {
            run.problems = c[1].parse().unwrap_or(0);
        } else if line.starts_with("robocopy reported a failure") {
            run.robocopy_fail = true;
        } else if let Some(step) = line.strip_prefix("  ").filter(|l| l.starts_with("couldn't move ") || l.starts_with("couldn't clear ") || l.starts_with("couldn't put back ")) {
            run.step_fails.push(step.trim().to_string());
        } else if let Some(c) = rx.cadence.captures(line) {
            run.cadence = Some((c[1].parse().unwrap_or(0), c[2].to_string()));
            part = Some(Part::Cadence);
        } else if let Some(c) = rx.legacy_player.captures(line) {
            run.legacy_player = Some((c[1].parse().unwrap_or(0), c[2].to_string()));
            part = Some(Part::LegacyPlayer);
        } else if let Some(c) = rx.tool.captures(line) {
            run.tool = Some((c[1].parse().unwrap_or(0), c[2].to_string()));
            part = Some(Part::Tool);
        } else if let Some(c) = rx.code.captures(line) {
            run.code = Some((c[1].parse().unwrap_or(0), c[2].to_string()));
            part = Some(Part::Code);
        } else if let Some(what) = line.strip_prefix("  problem: ") {
            // "Data (C:\...): robocopy code 16 (details: C:\...\robocopy_x.txt)": the details are in the log
            let what = what.split(" (details: ").next().unwrap_or(what).trim();
            run.part_problems.push((part.unwrap_or(Part::LegacyPlayer), what.to_string()));
        } else if line.starts_with("Backing up ") {
            run.second = line.ends_with("(second backup)");
        } else if let Some(c) = rx.kept_old.captures(line) {
            run.kept_old = c[1].parse().ok();
        } else if let Some(c) = rx.old_lost.captures(line) {
            run.old_lost = c[1].parse().unwrap_or(0);
        } else if let Some(c) = rx.too_many.captures(line) {
            run.too_many = Some((c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0)));
        } else if let Some(c) = rx.verified.captures(line) {
            run.verified = Some((c[1].parse().unwrap_or(0), c[2].to_string()));
        } else if let Some(c) = rx.bad_copies.captures(line) {
            run.verified = Some((c[1].parse().unwrap_or(0), String::new()));
            run.bad_count = c[2].parse().unwrap_or(0);
        } else if let Some(file) = line.strip_prefix("  bad copy: ") {
            run.bad_copies.push(file.trim().to_string());
        } else if line.starts_with("Checking every file in ") {
            run.check = Some(Check::default());
        } else if let Some(c) = rx.checked_all.captures(line) {
            let check = run.check.get_or_insert_with(Check::default);
            check.finished = true;
            check.files = c[1].parse().unwrap_or(0);
            check.time = c[2].to_string();
            check.good = c[3].parse().unwrap_or(0);
            check.damaged_count = c.get(4).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        } else if let Some(file) = line.strip_prefix("  damaged: ") {
            if let Some(check) = &mut run.check {
                check.damaged.push(file.trim().to_string());
            }
        } else if let Some(n) = line.strip_suffix(" backup files are missing; run a backup, then check again.").and_then(|s| s.parse().ok()) {
            if let Some(check) = &mut run.check { check.missing = n; }
        } else if let Some(n) = line.strip_suffix(" files have different size or date and were not verified; run a backup, then check again.").and_then(|s| s.parse().ok()) {
            if let Some(check) = &mut run.check { check.waiting = n; }
        } else if let Some(c) = rx.waiting.captures(line) {
            if let Some(check) = &mut run.check {
                check.waiting = c[1].parse().unwrap_or(0);
            }
        } else if let Some(c) = rx.room_left.captures(line) {
            run.room_left = Some(c[1].to_string());
        } else if let Some(c) = rx.repaired.captures(line) {
            run.recopy = true;
            run.repaired = c[1].parse().unwrap_or(0);
        } else if let Some(c) = rx.not_repaired.captures(line) {
            run.recopy = true;
            run.not_repaired_count = c[1].parse().unwrap_or(0);
        } else if let Some(file) = line.strip_prefix("  not repaired: ") {
            run.not_repaired.push(file.trim().to_string());
        } else if line.starts_with("The list was empty, nothing repaired.") {
            run.recopy = true;
        } else if let Some(c) = rx.cleared_removed.captures(line) {
            run.cleared_removed = c[1].parse().ok();
        } else if let Some(c) = rx.moved.captures(line) {
            run.moved = c[1].parse().ok();
        } else if line.starts_with("Backup is already up to date.") {
            run.up_to_date = true;
        } else if line.starts_with("Preview only, nothing copied.") {
            run.preview = true;
            run.closed = true;
        } else if line.starts_with("Cancelled, nothing copied.") {
            run.cancelled = true;
            run.closed = true;
        } else if line.starts_with("Log: ") {
            run.closed = true;
        }
    }
    runs
}

/// "Grandaddy\Sumday (2003)" reads better as "Grandaddy / Sumday (2003)".
fn album_name(s: &str) -> String {
    s.replace('\\', " / ")
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

/// "865 MB" back to bytes (rounded, only for the progress line).
fn parse_size(s: &str) -> u64 {
    let mut it = s.split_whitespace();
    let n: f64 = it.next().map(|v| v.replace(',', "")).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let mul = match it.next().unwrap_or("") {
        "TB" => 1u64 << 40,
        "GB" => 1 << 30,
        "MB" => 1 << 20,
        "KB" => 1 << 10,
        _ => 1,
    };
    (n * mul as f64) as u64
}

/// Sizes written the way the script writes them.
fn fmt_size(b: u64) -> String {
    let f = b as f64;
    if b >= 1 << 40 {
        format!("{:.2} TB", f / (1u64 << 40) as f64)
    } else if b >= 1 << 30 {
        format!("{:.1} GB", f / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.0} MB", f / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.0} KB", f / 1024.0)
    } else {
        format!("{b} bytes")
    }
}

/// "Today at 3:01 PM", "Yesterday at 3:00 AM", "Nov 8 at 2:01 PM".
fn when_text(t: NaiveDateTime) -> String {
    use chrono::Datelike;
    let today = Local::now().date_naive();
    let days = (t.date() - today).num_days();
    let time = t.format("%-I:%M %p");
    match days {
        0 => format!("Today at {time}"),
        -1 => format!("Yesterday at {time}"),
        1 => format!("Tomorrow at {time}"),
        _ if t.date().year() == today.year() => format!("{} at {time}", t.format("%b %-d")),
        _ => format!("{} at {time}", t.format("%b %-d, %Y")),
    }
}

// ------------------------------------------------------------------ status (all read-only)

#[derive(Clone, Default, PartialEq)]
enum Drive {
    #[default]
    NoDest,
    Missing(String),
    Wrong {
        drive: String,
        found: String,
        want: String,
    },
    Good {
        drive: String,
        label: String,
    },
}

#[derive(Clone, Default)]
enum Task {
    #[default]
    Unknown,
    Missing,
    Found {
        enabled: bool,
        native: bool,
        running: bool,
        schedule: String,
        next: String,
        result: Option<i64>,
    },
}

#[derive(Clone, Default)]
struct Status {
    tool_missing: bool,
    lib_missing: bool,
    dest: String,
    /// when the script last got to the end of a run (config.json LastRun)
    last_ok: Option<NaiveDateTime>,
    drive: Drive,
    /// free, total
    space: Option<(u64, u64)>,
    dest_exists: bool,
    log_exists: bool,
    /// newest first, previews and checks of the whole backup left out
    runs: Vec<Run>,
    /// the last check of the whole backup, if the log has one
    check: Option<Run>,
    second: Second,
    task: Task,
    /// the script is running outside this page (the nightly task, the desktop shortcut)
    other_running: bool,
}


fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The label of the disk at `root` ("D:\"), or None when no disk is there. Only reads.
fn volume_label(root: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = wide(root);
    let mut name = [0u16; 262];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

fn disk_space(root: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let root = wide(root);
    let (mut free, mut total) = (0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((free, total))
}

/// What the script keeps in its config.json. Only ever read here.
#[derive(Clone, Default, PartialEq, Debug)]
struct Saved {
    dest: String,
    cadence_data: String,
    label: String,
    last: Option<NaiveDateTime>,
    /// the second backup: the id in the file at the top of its folder, the folder without its drive
    /// letter (`\Music Backup`), the drive's name, and when it last ran to the end
    second_id: String,
    second_folder: String,
    second_label: String,
    second_last: Option<NaiveDateTime>,
}

fn read_config(file: &Path) -> Saved {
    let Ok(raw) = std::fs::read_to_string(file) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')) else { return Default::default() };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let time = |k: &str| NaiveDateTime::parse_from_str(&s(k), "%Y-%m-%dT%H:%M:%S").ok();
    Saved { cadence_data: s("CadenceDataDirectory"), dest: s("Destination"), label: s("DestinationLabel"), last: time("LastRun"), second_id: s("SecondaryId"), second_folder: s("SecondaryFolder"), second_label: s("SecondaryLabel"), second_last: time("SecondaryLastRun") }
}

/// The runs in one of the script's summary logs, newest first, previews left out: (the backups, the last
/// check of the whole backup).
fn read_runs(log: &Path) -> (Vec<Run>, Option<Run>) {
    let Some(text) = read_tail(log, 300_000) else { return (vec![], None) };
    // the tail may start in the middle of a run: begin at the first full one
    let start = text.find("\n====================").map(|i| i + 1).unwrap_or(0);
    let mut all: Vec<Run> = parse_runs(&text[start..]).into_iter().filter(|r| !r.preview).collect();
    all.reverse();
    let check = all.iter().find(|r| r.check.is_some()).cloned();
    // (a run that only repaired copies is not a backup either)
    let mut runs: Vec<Run> = all.into_iter().filter(|r| r.check.is_none() && !r.recopy).collect();
    runs.truncate(12);
    (runs, check)
}

// ---- the second backup, on a drive that is plugged in by hand

/// The name of the small file at the top of the second backup's folder that says "this is the one".
const ID_FILE: &str = "_backup-id.txt";
/// The folder a backup goes into on a drive that is chosen here.
const BACKUP_FOLDER: &str = "Music Backup";

/// A drive that can be pulled out, as it is offered for the second backup.
#[derive(Clone, PartialEq, Debug)]
struct Removable {
    /// "G:\" (in a test run, a made-up folder that stands for a drive)
    root: String,
    /// "Travel (G:)"
    name: String,
    free: u64,
    total: u64,
}

impl Removable {
    /// Where the backup goes on it.
    fn dest(&self) -> String {
        Path::new(&self.root).join(BACKUP_FOLDER).to_string_lossy().to_string()
    }
}

/// What is known about the second backup. All of it is only read.
#[derive(Clone, Default)]
struct Second {
    /// a drive was chosen once (the script saved its id)
    set_up: bool,
    /// the drive's name when it was last used
    label: String,
    /// its folder as it is now ("G:\Music Backup"), when the drive is plugged in
    folder: Option<String>,
    /// free, total
    space: Option<(u64, u64)>,
    last: Option<NaiveDateTime>,
    runs: Vec<Run>,
    check: Option<Run>,
    log_exists: bool,
    /// the drives that could be chosen for it
    drives: Vec<Removable>,
}

/// The roots of the drives that are there now: "C:\", "D:\", ...
fn drive_roots() -> Vec<String> {
    let bits = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u8).filter(|i| bits & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i) as char)).collect()
}

/// The second backup's folder on whichever of these drives holds its id file (the way the script finds
/// it, whatever letter the drive has today), or None when the drive is not plugged in.
fn find_second(saved: &Saved, roots: &[String]) -> Option<String> {
    if saved.second_id.is_empty() || saved.second_folder.is_empty() {
        return None;
    }
    roots.iter().map(|root| format!("{}{}", &root[..2.min(root.len())], saved.second_folder)).find(|folder| {
        std::fs::read_to_string(Path::new(folder).join(ID_FILE)).is_ok_and(|id| id.trim_start_matches('\u{feff}').trim() == saved.second_id)
    })
}

/// May the drive at this root be offered for the second backup? Never the library's drive, and never the
/// main backup's.
fn may_offer(root: &str, lib: &str, main_dest: &str) -> bool {
    let letter = |p: &str| p.chars().next().filter(|_| p.as_bytes().get(1) == Some(&b':')).map(|c| c.to_ascii_uppercase());
    let d = letter(root);
    d.is_some() && d != letter(lib) && d != letter(main_dest)
}

/// The drives that can be pulled out and are plugged in now, without the library's and the main backup's.
/// A test run is only ever offered the made-up folders it names in `MUSIC_RECS_BACKUP_DRIVES` (separated
/// by ";"), never a real drive.
fn removable_drives(test: bool, lib: &str, main_dest: &str) -> Vec<Removable> {
    if test {
        let named = std::env::var("MUSIC_RECS_BACKUP_DRIVES").unwrap_or_default();
        return named
            .split(';')
            .map(str::trim)
            .filter(|f| !f.is_empty() && Path::new(f).is_dir())
            .map(|f| {
                let (free, total) = disk_space(f).unwrap_or((0, 0));
                Removable { root: f.to_string(), name: Path::new(f).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), free, total }
            })
            .collect();
    }
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_REMOVABLE: u32 = 2;
    drive_roots()
        .into_iter()
        .filter(|root| may_offer(root, lib, main_dest) && unsafe { GetDriveTypeW(wide(root).as_ptr()) } == DRIVE_REMOVABLE)
        .filter_map(|root| {
            let label = volume_label(&root)?; // no disk in it: nothing to offer
            let (free, total) = disk_space(&root)?;
            let letter = &root[..2];
            Some(Removable { name: if label.is_empty() { format!("Drive {letter}") } else { format!("{label} ({letter})") }, root, free, total })
        })
        .collect()
}

/// Can the drive that holds this folder be pulled out (so that "Eject" makes sense for it)? A test run
/// says yes for its made-up drive, and never touches a real one (see `eject`).
fn can_eject(folder: &str, test: bool) -> bool {
    if test {
        return true;
    }
    let Some(root) = folder.get(..2).filter(|r| r.ends_with(':')).map(|r| format!("{r}\\")) else { return false };
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide(&root).as_ptr()) == 2 }
}

/// Asks Windows to let go of the drive that holds this folder, the way "Eject" in File Explorer does, and
/// then looks whether the drive is really gone. Ok only when it is: nothing is ever claimed that was not
/// seen. A test run asks Windows for nothing: `MUSIC_RECS_BACKUP_EJECT=gone` stands for a drive that went.
fn eject(folder: &str, test: bool) -> Result<(), String> {
    const BUSY: &str = "Windows did not let go of the drive: something is still using it. Close what has it open and try again, or eject it from the taskbar. Do not pull it out yet.";
    if test { return if std::env::var("MUSIC_RECS_BACKUP_EJECT").is_ok_and(|v| v == "gone") { Ok(()) } else { Err(BUSY.into()) }; }
    if !can_eject(folder, false) { return Err("This drive is not one that can be ejected.".into()); }
    use windows_sys::Win32::{Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE}, Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING}, System::IO::DeviceIoControl};
    let name = wide(&format!(r"\\.\{}", &folder[..2]));
    let handle = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if handle == INVALID_HANDLE_VALUE { return Err(BUSY.into()); }
    let mut returned = 0;
    let ok = [0x00090018, 0x00090020, 0x002d4808].into_iter().all(|code| unsafe {
        DeviceIoControl(handle, code, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut()) != 0
    });
    unsafe { CloseHandle(handle); }
    if !ok { return Err(BUSY.into()); }
    for _ in 0..10 {
        if volume_label(&format!("{}\\", &folder[..2])).is_none() { return Ok(()); }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(BUSY.into())
}

/// How much room a folder takes, in bytes (0 when it is not there).
fn folder_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    crate::importer::util::walk_files(dir).iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum()
}

/// "Old copies take 1.2 GB and removed items 14 GB of this backup.", or nothing when both are empty.
fn room_words(changed: u64, deleted: u64) -> Option<String> {
    match (changed, deleted) {
        (0, 0) => None,
        (c, 0) => Some(format!("Old copies of changed files take {} of this backup. They are kept for 30 days.", fmt_size(c))),
        (0, d) => Some(format!("Items that left the library take {} of this backup (in _deleted, kept for 30 days).", fmt_size(d))),
        (c, d) => Some(format!("Old copies of changed files take {} of this backup, and items that left the library take {} (in _deleted). Both are kept for 30 days.", fmt_size(c), fmt_size(d))),
    }
}

fn gather_second(at: &Places, saved: &Saved, lib: &str) -> Second {
    let mut s = Second { set_up: !saved.second_id.is_empty() && !saved.second_folder.is_empty(), label: saved.second_label.clone(), last: saved.second_last, ..Default::default() };
    s.folder = if at.test {
        removable_drives(true, lib, &saved.dest).into_iter().map(|d| Path::new(&d.root).join(BACKUP_FOLDER)).find(|d| {
            !saved.second_id.is_empty() && std::fs::read_to_string(d.join(ID_FILE)).is_ok_and(|id| id.trim() == saved.second_id)
        }).map(|d| d.to_string_lossy().into_owned())
    } else { find_second(saved, &drive_roots()) };
    if let Some(folder) = &s.folder {
        s.space = disk_space(folder);
    }
    let log = at.logs.join(Which::Second.log_name());
    s.log_exists = log.is_file();
    (s.runs, s.check) = read_runs(&log);
    s.drives = removable_drives(at.test, lib, &saved.dest);
    s
}

/// The script's own rule: the disk at the destination's letter must carry the saved label.
fn check_drive(dest: &str, want: &str) -> Drive {
    if dest.is_empty() {
        return Drive::NoDest;
    }
    let b = dest.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        let drive = dest[..2].to_uppercase();
        return match volume_label(&format!("{drive}\\")) {
            None => Drive::Missing(drive),
            // Existing saved labels are compared without case.
            Some(found) if !want.is_empty() && found.to_lowercase() != want.to_lowercase() => Drive::Wrong { drive, found, want: want.to_string() },
            Some(found) => Drive::Good { drive, label: found },
        };
    }
    if Path::new(dest).exists() { Drive::Good { drive: dest.to_string(), label: String::new() } } else { Drive::Missing(dest.to_string()) }
}

/// The last part of a file, as text (the log grows forever; the recent runs are at the end).
fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn query_task() -> Task {
    let out = Command::new("schtasks")
        .args(["/query", "/tn", TASK_NAME, "/fo", "LIST", "/v"])
        .stdin(Stdio::null())
        .creation_flags(NO_WINDOW)
        .output();
    let Ok(out) = out else { return Task::Unknown };
    if !out.status.success() {
        return Task::Missing;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| -> String {
        text.lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    };
    let time = |s: &str| -> String {
        match NaiveDateTime::parse_from_str(s, "%m/%d/%Y %I:%M:%S %p") {
            Ok(t) if chrono::Datelike::year(&t.date()) > 2000 => when_text(t),
            Ok(_) => String::new(), // Task Scheduler's "never"
            Err(_) if s.eq_ignore_ascii_case("N/A") => String::new(),
            Err(_) => s.to_string(),
        }
    };
    let start = field("Start Time");
    let at = NaiveTime::parse_from_str(&start, "%I:%M:%S %p").map(|t| t.format("%-I:%M %p").to_string()).unwrap_or(start);
    let kind = field("Schedule Type");
    let schedule = match (kind.is_empty(), at.is_empty()) {
        (false, false) => format!("{kind} at {at}"),
        (false, true) => kind,
        _ => String::new(),
    };
    let status = field("Status");
    Task::Found {
        native: field("Task To Run").contains("--backup-run"),
        enabled: !field("Scheduled Task State").eq_ignore_ascii_case("Disabled"),
        running: status.eq_ignore_ascii_case("Running"),
        schedule,
        next: time(&field("Next Run Time")),
        result: field("Last Result").parse().ok(),
    }
}

fn gather(lib: &str, look_for_script: bool) -> Status {
    let at = places();
    let mut s = Status { tool_missing: tool_dir().is_none(), lib_missing: !Path::new(lib).is_dir(), ..Default::default() };
    if s.tool_missing {
        return s;
    }
    let saved = read_config(&at.config);
    s.drive = check_drive(&saved.dest, &saved.label);
    if let Drive::Good { drive, .. } = &s.drive {
        s.space = disk_space(&format!("{drive}\\"));
        s.dest_exists = Path::new(&saved.dest).is_dir();
    }
    s.dest = saved.dest.clone();
    s.last_ok = saved.last;
    let log = at.logs.join(Which::Main.log_name());
    s.log_exists = log.is_file();
    (s.runs, s.check) = read_runs(&log);
    s.second = gather_second(&at, &saved, lib);
    s.task = if at.test { Task::Missing } else { query_task() };
    let task_running = matches!(s.task, Task::Found { running: true, .. });
    s.other_running = task_running || (look_for_script && native::busy(&s.dest));
    s
}

// ------------------------------------------------------------------ damaged copies: which file is sound

/// What listening to a library file and its backup copy says about the pair. A check of the whole backup
/// only knows that the two differ.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sound {
    /// the library's file is sound and the copy is damaged: the copy can be made again from the library
    Library,
    /// the copy is sound and the library's file is damaged: the copy can be brought back
    Copy,
    /// both play through cleanly: listening cannot tell which is right
    Both,
    /// both are damaged, and equally so
    Neither,
    /// not an audio file (a cover, a text file): it cannot be judged by listening
    NotAudio,
    /// one of the two is not there any more
    Gone,
}

impl Sound {
    fn words(self) -> &'static str {
        match self {
            Sound::Library => "The library's file is sound. The copy is damaged.",
            Sound::Copy => "The copy is sound. The library's file is damaged.",
            Sound::Both => "Both play through cleanly, so listening cannot tell which is right.",
            Sound::Neither => "Both are damaged.",
            Sound::NotAudio => "Not an audio file, so it cannot be judged by listening.",
            Sound::Gone => "One of the two files is no longer there.",
        }
    }
}

/// The file a "damaged" line of the log names, as a path inside the library: the part before the reason
/// in brackets at its end (album folders have brackets of their own, so it is the last one that counts).
fn damaged_rel(line: &str) -> String {
    let line = line.trim();
    match line.rfind(" (").filter(|_| line.ends_with(')')) {
        Some(at) => line[..at].to_string(),
        None => line.to_string(),
    }
}

/// A path from the log, inside this folder and nowhere else (no "..", no drive of its own).
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    let plain = !rel.as_os_str().is_empty() && rel.components().all(|c| matches!(c, std::path::Component::Normal(_)));
    plain.then(|| root.join(rel))
}

/// What one listening found, as far as it matters here: could the file be decoded at all, how many decode
/// errors, how much of it plays, and whether a lossless file passed its own checksum.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Heard {
    readable: bool,
    errors: u32,
    plays: f64,
    /// what the file itself says its length is
    length: f64,
    checksum_bad: bool,
}

impl Heard {
    fn of(q: &crate::importer::quality::Quality, length: f64) -> Heard {
        Heard { readable: q.check_error.is_empty(), errors: q.error_count, plays: q.decoded_seconds, length, checksum_bad: q.md5_status == "mismatch" }
    }

    /// Damaged by any measure of its own (the rules Health's full check goes by, without the fine print).
    fn damaged(&self) -> bool {
        !self.readable || self.checksum_bad || self.errors >= 10 || (self.length > 5.0 && self.plays < self.length * 0.95 - 2.0 && self.errors > 0)
    }

    /// Clearly worse than the other file of its pair. The two were the same file once, so any difference
    /// in what plays tells them apart: a failed checksum, more decode errors, or less of the song.
    fn worse_than(&self, other: &Heard) -> bool {
        (!self.readable && other.readable) || (self.checksum_bad && !other.checksum_bad) || self.errors > other.errors || self.plays < other.plays - 0.5
    }
}

/// Which of the two is sound, from what was heard of each.
fn verdict(library: &Heard, copy: &Heard) -> Sound {
    match (library.worse_than(copy), copy.worse_than(library)) {
        (true, false) => Sound::Copy,
        (false, true) => Sound::Library,
        // no telling them apart (or each is worse in its own way)
        _ if library.damaged() || copy.damaged() => Sound::Neither,
        _ => Sound::Both,
    }
}

/// Listens to a library file and its copy in the backup and says which is sound. Only reads.
fn judge_pair(ffmpeg: &Path, ffprobe: &Path, library: &Path, copy: &Path) -> Sound {
    if !library.is_file() || !copy.is_file() {
        return Sound::Gone;
    }
    let ext = library.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
    if !crate::importer::scan::is_audio_ext(&ext) {
        return Sound::NotAudio;
    }
    let hear = |file: &Path| {
        let length = crate::healthscan::probe(ffprobe, file).duration;
        Heard::of(&crate::importer::quality::check_fresh(ffmpeg, file, length), length)
    };
    verdict(&hear(library), &hear(copy))
}

/// True when the two files hold the same bytes.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else { return false };
    if fa.metadata().map(|m| m.len()).ok() != fb.metadata().map(|m| m.len()).ok() {
        return false;
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        // (read to the end of each buffer, so both sides are compared at the same place)
        let fill = |f: &mut std::fs::File, buf: &mut [u8]| -> Option<usize> {
            let mut n = 0;
            while n < buf.len() {
                match f.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => return None,
                }
            }
            Some(n)
        };
        let (Some(na), Some(nb)) = (fill(&mut fa, &mut ba), fill(&mut fb, &mut bb)) else { return false };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

/// Of the files a check called damaged, the ones whose copy still differs from the library's file (one
/// that was repaired since, or that is gone from both, is no longer a pair to look at).
fn still_different(library: &Path, backup: &Path, rels: &[String]) -> Vec<String> {
    rels.iter()
        .filter(|rel| match (inside(library, rel), inside(backup, rel)) {
            (Some(a), Some(b)) => (a.is_file() || b.is_file()) && !same_bytes(&a, &b),
            _ => false,
        })
        .cloned()
        .collect()
}

/// Brings sound copies back from the backup over the damaged files in the library, through the undo
/// backups like every other change to the library: each damaged file is kept in the batch and Repair
/// history can put it back. The file that comes in keeps the copy's date (so the next backup has nothing
/// to copy), is compared with the copy before it takes the damaged file's place, and is listened to again so
/// that Health's saved result is the new file's. Nothing in the backup is changed.
fn bring_back(library: &Path, backup: &Path, undo_root: &Path, rels: &[String], ffmpeg: &Path, ffprobe: &Path) -> Result<crate::repair::Done, String> {
    let mut done = crate::repair::Done::default();
    // what can be brought back at all (the record's title says how many)
    let mut pairs: Vec<(&String, PathBuf, PathBuf)> = vec![];
    for rel in rels {
        match (inside(library, rel), inside(backup, rel)) {
            (Some(_), Some(source)) if !source.is_file() => done.problems.push(format!("{rel}: its copy is not in the backup")),
            (Some(original), Some(_)) if !original.is_file() => done.problems.push(format!("{rel}: it is no longer in the library")),
            (Some(original), Some(source)) => pairs.push((rel, original, source)),
            _ => done.problems.push(format!("{rel}: it is not a path inside the library")),
        }
    }
    if pairs.is_empty() {
        return Ok(done);
    }
    let title = format!("Backup: brought back {} the library had damaged", if pairs.len() == 1 { "1 file".to_string() } else { format!("{} files", pairs.len()) });
    // one writer to the library at a time (see `library_lock`): an import or a repair waits until these are back
    let _held = crate::library_lock::hold();
    let batch = crate::undo::Batch::begin(undo_root, library, &title)?;
    for (n, (rel, original, source)) in pairs.into_iter().enumerate() {
        let said = (|| -> Result<PathBuf, String> {
            let copy = batch.dir.join("work").join(format!("back-{n}")).join(original.file_name().unwrap_or_default());
            copy.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::copy(&source, &copy)).map_err(|e| crate::util::plain_err(format!("it could not be copied from the backup: {e}")))?;
            if !same_bytes(&source, &copy) {
                return Err("what was copied from the backup is not the same as the copy there, so nothing was changed".into());
            }
            let date = std::fs::metadata(&source).and_then(|m| m.modified()).map_err(|e| crate::util::plain_err(format!("the copy's date could not be read: {e}")))?;
            batch.replaced(&original, &copy, "this file was damaged and its copy in the backup is sound")?;
            // the copy's own date (the swap stamps a fresh one), so the library's file and its copy agree and
            // the next backup copies nothing. Not being able to set it only costs one more copy.
            let _ = std::fs::File::options().write(true).open(&original).and_then(|f| f.set_modified(date));
            Ok(original)
        })();
        match said {
            Ok(original) => {
                done.changed += 1;
                let length = crate::healthscan::probe(ffprobe, &original).duration;
                crate::importer::quality::recheck(ffmpeg, &original, length);
                crate::links::folder_changed(&original); // Health looks at its folder again
            }
            Err(why) => done.problems.push(format!("{rel}: {why}")),
        }
    }
    if batch.changes() > 0 {
        done.batch = Some(batch.id.clone());
    }
    batch.close();
    Ok(done)
}

// ------------------------------------------------------------------ what needs a look

/// A backup that has not finished well for this long is said out loud.
const MAIN_STALE_DAYS: i64 = 2;
/// The second backup is plugged in by hand: a gentle word once it is this old.
const SECOND_STALE_DAYS: i64 = 14;

/// What the warnings are worked out from.
struct Facts<'a> {
    /// a backup folder has been chosen
    chosen: bool,
    /// the newest run in the main backup's log (not a preview, not a check)
    last_run: Option<&'a Run>,
    /// when the main backup last ran to its end
    last_ok: Option<NaiveDateTime>,
    task: &'a Task,
    /// when the second backup last ran to its end, if one is set up
    second_last: Option<NaiveDateTime>,
}

/// The sentences for what needs a look, most pressing first, each with whether it is serious (a mark in
/// the top row) or only a gentle word.
fn alerts_for(f: &Facts, now: NaiveDateTime) -> Vec<(String, bool)> {
    let mut out = vec![];
    if !f.chosen {
        return out; // nothing has been set up yet: the Backup page says so itself
    }
    // the last run failed, or said something went wrong
    if let Some(run) = f.last_run {
        let (text, tone) = run.headline();
        if tone == Tone::Bad {
            let when = run.when.map(|t| format!(" ({})", lower_first(&when_text(t)))).unwrap_or_default();
            out.push((format!("The last backup{when} had a problem. {}.", text.trim_end_matches('.')), true));
        }
    }
    // no backup has run to its end for two days (the PC was off, the drive was gone, the task did not run)
    match f.last_ok {
        Some(ok) if (now - ok).num_hours() >= MAIN_STALE_DAYS * 24 => {
            let days = (now - ok).num_days();
            out.push((format!("No backup has finished for {days} days. The last one was {}.", lower_first(&when_text(ok))), true));
        }
        None => out.push(("No backup has finished yet.".to_string(), true)),
        _ => {}
    }
    match f.task {
        Task::Missing => out.push(("No nightly backup is set up.".to_string(), true)),
        Task::Found { enabled: false, .. } => out.push(("The nightly backup is turned off in Task Scheduler.".to_string(), true)),
        _ => {}
    }
    if let Some(last) = f.second_last {
        let days = (now.date() - last.date()).num_days();
        if days >= SECOND_STALE_DAYS {
            out.push((format!("The second backup is {days} days old."), false));
        }
    }
    out
}

// ------------------------------------------------------------------ running the script

#[derive(Clone, PartialEq, Debug)]
enum Kind {
    Preview,
    Backup,
    /// a backup with `-Mirror`: what is no longer in the library moves to `_deleted` even when it is a lot
    Mirror,
    /// `-CheckAll`: copies nothing, reads every file in the library and its copy and compares them
    CheckAll,
    /// the second backup's first run, to this folder on the drive that was chosen
    Setup(String),
    /// `-Recopy <list>`: the damaged copies named in this list are made again from the library (the copies
    /// they replace are kept in `_changed`)
    Recopy(PathBuf),
}

impl Kind {
    /// A run that copies (and so writes to the backup).
    fn copies(&self) -> bool {
        matches!(self, Kind::Backup | Kind::Mirror | Kind::Setup(_))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Starting,
    Comparing,
    Copying,
    /// the copied files are read back and compared with the library
    Verifying,
    /// a check of the whole backup
    Checking,
    /// Cadence, the music tool's data and the code are saved
    Saving,
    Finishing,
}

enum Msg {
    Status(Box<Status>),
    Configured(Result<String, String>),
    /// a line from the backup worker
    Line(String),
    /// files and bytes copied and checked so far
    Copied(u64, u64),
    /// how large the library is (asked for once, when a drive is to be chosen for the second backup)
    LibSize(u64),
    /// how much room `_changed` and `_deleted` take in one of the backups
    Room(Which, u64, u64),
    /// how asking Windows to eject the second backup's drive ended
    Ejected(Result<(), String>),
    /// the files of a check's damaged list whose copy still differs from the library's file (for the check
    /// that ran at this time)
    Pairs(Which, Option<NaiveDateTime>, Vec<String>),
    /// what listening to one pair found
    Judged(Which, String, Sound),
    JudgeDone(Which),
    /// how bringing sound copies back into the library ended
    BroughtBack(Which, Result<crate::repair::Done, String>),
    Done {
        code: Option<i32>,
        problem: Option<String>,
    },
}

struct Job {
    which: Which,
    kind: Kind,
    lines: Vec<String>,
    phase: Phase,
    total_files: u64,
    total_bytes: u64,
    total_text: String,
    done_files: u64,
    done_bytes: u64,
    /// while files are read back (after a copy, or in a check of the whole backup): files done and in
    /// all, "1.2 GB" done and in all, and how long is left as the script says it ("" when not known)
    read_files: (u64, u64),
    read_sizes: (String, String),
    read_left: String,
    /// the running script, so a check of the whole backup can be stopped
    stop: Arc<AtomicBool>,
    stopped: bool,
}

impl Job {
    fn new(which: Which, kind: Kind) -> Job {
        Job { which, kind, lines: vec![], phase: Phase::Starting, total_files: 0, total_bytes: 0, total_text: String::new(), done_files: 0, done_bytes: 0, read_files: (0, 0), read_sizes: Default::default(), read_left: String::new(), stop: Default::default(), stopped: false }
    }
}

/// Runs the native worker on its own thread and gives the page its progress.
fn run_job(which: Which, kind: Kind, lib: String, stop: Arc<AtomicBool>, tx: Sender<Msg>, ctx: egui::Context) {
    let mut send = |message| { let _ = tx.send(message); ctx.request_repaint(); };
    let result = native::execute(&places(), which, &kind, Path::new(&lib), &stop, &mut send);
    if kind != Kind::Preview { crate::links::job_ended("backup"); }
    send(Msg::Done { code: Some(if result.is_ok() { 0 } else { 1 }), problem: result.err() });
}

/// The size of the file a robocopy log line says was copied from the library (`lib` in lower case),
/// or None for any other line (folders, skipped files, the previous player copy that shares the log).
#[cfg(test)]
fn copied_file(line: &str, lib: &str) -> Option<u64> {
    let c = rx().robocopy_line.captures(line)?;
    let class = c.get(1).map(|m| m.as_str().trim()).unwrap_or("");
    let path = &c[3];
    let ok = !path.ends_with('\\') && matches!(class, "New File" | "Newer" | "Older" | "Changed") && path.to_lowercase().starts_with(lib);
    ok.then(|| c[2].parse().unwrap_or(0))
}

/// Follows the newest `robocopy_*.txt` (UTF-16) that this run creates and counts copied files.
#[derive(Default)]
struct Pairs {
    /// the check they belong to (a newer check starts this over)
    when: Option<NaiveDateTime>,
    /// the pairs that still differ, once that has been looked at in the background (None until then)
    rels: Option<Vec<String>>,
    asked: bool,
    verdicts: std::collections::HashMap<String, Sound>,
    /// listening is under way: pairs done, pairs in all, and the flag that stops it
    judging: Option<(usize, usize, Arc<AtomicBool>)>,
    /// sound copies are being brought back into the library
    bringing: bool,
    /// how the last repair went, and whether that is good news
    note: Option<(String, bool)>,
}

pub struct BackupPage {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    status: Option<Status>,
    loading: bool,
    refreshed: Option<Instant>,
    /// the library folder, as the app's settings have it
    lib: String,
    job: Option<Job>,
    /// the last preview, of which backup, and when it was made
    preview: Option<(Which, Run, NaiveDateTime)>,
    /// how the job started from this page ended
    finished: Option<(Which, Kind, Run)>,
    debug_preview_done: bool,
    /// the second backup: another drive is being chosen for one that is set up, and the drive picked so far
    choosing: bool,
    chosen: Option<Removable>,
    /// how large the library is, once it has been measured (None until then), and whether that was asked
    lib_bytes: Option<u64>,
    lib_bytes_asked: bool,
    /// "Check the whole backup" was clicked for this backup, and what that means is being said
    asking_check: Option<Which>,
    /// the room `_changed` and `_deleted` take in the main and in the second backup, once measured, and the
    /// folder each was measured (or is being measured) for
    room: [Option<(u64, u64)>; 2],
    room_for: [String; 2],
    /// Windows is being asked to eject the second drive; then how that ended (the words, and whether it went)
    ejecting: bool,
    eject_note: Option<(String, bool)>,
    setup_note: Option<(String, bool)>,
    configuring: bool,
    /// the damaged copies of the main and of the second backup's last whole check
    pairs: [Pairs; 2],
}

fn confirm_folder_choice(library: &str, dest: &Path) -> bool {
    rfd::MessageDialog::new()
        .set_title("Confirm backup folders")
        .set_description(format!("Library:\n{library}\n\nBackup:\n{}\n\nConfirm only if these are the intended folders and drives. Future backups will use this library; files absent from it may be moved into backup history. Preview the changes before running the backup.", dest.display()))
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show() == rfd::MessageDialogResult::Ok
}

impl BackupPage {
    pub fn new(_egui_ctx: &egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { tx, rx, status: None, loading: false, refreshed: None, lib: String::new(), job: None, preview: None, finished: None, debug_preview_done: false, choosing: false, chosen: None, lib_bytes: None, lib_bytes_asked: false, asking_check: None, room: [None, None], room_for: Default::default(), ejecting: false, eject_note: None, setup_note: None, configuring: false, pairs: Default::default() }
    }

    fn choose_main_folder(&mut self, ctx: &egui::Context) {
        if self.configuring || self.busy() { return; }
        self.configuring = true;
        let (tx, ctx, library) = (self.tx.clone(), ctx.clone(), self.lib.clone());
        std::thread::spawn(move || {
            let selected = crate::cards::test_switch("MUSIC_RECS_PICK").map(PathBuf::from)
                .or_else(|| rfd::FileDialog::new().set_title("Choose the main backup folder").pick_folder());
            let result = match selected {
                Some(dest) if confirm_folder_choice(&library, &dest) => native::choose_main(&places(), Path::new(&library), &dest),
                Some(_) => Ok("The backup folder was left as it was.".into()),
                None => Ok("The backup folder was left as it was.".into()),
            };
            let _ = tx.send(Msg::Configured(result)); ctx.request_repaint();
        });
    }
    fn confirm_backup_folders(&mut self, which: Which, folder: String, ctx: &egui::Context) {
        if self.configuring || self.busy() { return; }
        self.configuring = true;
        let (tx, ctx, library) = (self.tx.clone(), ctx.clone(), self.lib.clone());
        std::thread::spawn(move || {
            let dest = Path::new(&folder);
            let result = if confirm_folder_choice(&library, dest) {
                native::confirm_folders(&places(), which, Path::new(&library), dest)
            } else { Ok("The saved folders were left as they were.".into()) };
            let _ = tx.send(Msg::Configured(result)); ctx.request_repaint();
        });
    }
    fn set_up_nightly(&mut self, ctx: &egui::Context) {
        if self.configuring || self.busy() { return; }
        self.configuring = true;
        let (tx, ctx, library) = (self.tx.clone(), ctx.clone(), self.lib.clone());
        std::thread::spawn(move || { let result = schedule::install(&places(), Path::new(&library)); let _ = tx.send(Msg::Configured(result)); ctx.request_repaint(); });
    }

    /// True while a backup or preview is running (the app then asks before closing).
    pub fn busy(&self) -> bool {
        self.job.is_some() || self.configuring
    }

    /// What the backup is doing, in a few words for the top row (None while nothing runs).
    pub fn job(&self) -> Option<String> {
        let j = self.job.as_ref()?;
        let (done, all) = j.read_files;
        Some(match (&j.kind, j.phase) {
            (Kind::Preview, _) => "Previewing the backup".to_string(),
            (Kind::Recopy(_), _) => "Repairing copies in the backup".to_string(),
            (Kind::CheckAll, _) if all > 0 => format!("Checking the backup {} of {}", commas(done.min(all)), commas(all)),
            (Kind::CheckAll, _) => "Checking the backup".to_string(),
            (_, Phase::Verifying) if all > 0 => format!("Checking the copies {} of {}", commas(done.min(all)), commas(all)),
            _ if j.total_files > 0 => format!("Backing up {} of {}", commas(j.done_files.min(j.total_files)), commas(j.total_files)),
            _ => "Backing up".to_string(),
        })
    }

    /// For the Home page: start reading the backup's state if that has not happened yet, or was last done
    /// more than ten minutes ago. Changes nothing.
    pub fn ensure_loaded(&mut self, ctx: &egui::Context, cfg: &Config) {
        let stale = self.tool_folder(cfg) || self.refreshed.is_some_and(|t| t.elapsed() >= Duration::from_secs(600));
        if (self.status.is_none() || stale) && !self.loading && self.job.is_none() {
            if self.lib.is_empty() {
                self.lib = cfg.music_dir.clone();
            }
            self.refresh(ctx, false);
        }
    }

    /// Takes the backup tool's folder from Settings. True when it is another one than before (what was
    /// read about the backup then belongs to the old folder and is read again).
    fn tool_folder(&mut self, cfg: &Config) -> bool {
        let mut dir = TOOL_DIR.write().unwrap_or_else(|e| e.into_inner());
        if *dir == cfg.backup_tool_dir {
            return false;
        }
        *dir = cfg.backup_tool_dir.clone();
        true
    }

    /// For the app's shell: read the backup's state once when the app starts, so the top row can say that
    /// something needs a look before the page was ever opened. Changes nothing.
    pub fn load_once(&mut self, ctx: &egui::Context, cfg: &Config) {
        self.tool_folder(cfg);
        if self.status.is_none() && self.refreshed.is_none() {
            self.ensure_loaded(ctx, cfg);
        }
    }

    /// What about the backups needs a look, most pressing first: (the sentence, whether it is serious
    /// enough for a mark in the top row). Empty until the state has been read, and while all is well.
    pub fn alerts(&self) -> Vec<(String, bool)> {
        let Some(s) = &self.status else { return vec![] };
        if s.tool_missing {
            return vec![];
        }
        alerts_for(&Facts { chosen: !s.dest.is_empty(), last_run: s.runs.first(), last_ok: s.last_ok, task: &s.task, second_last: s.second.set_up.then_some(s.second.last).flatten() }, Local::now().naive_local())
    }

    /// True when something about the backups is wrong enough for a mark beside "Backup" in the top row.
    pub fn needs_look(&self) -> bool {
        self.job.is_none() && self.alerts().iter().any(|a| a.1)
    }

    // (what the Home page's card says and draws is beside this page's own top sentence: `page::in_short`)

    /// Runs every frame, whichever page is showing.
    pub fn tick(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => {
                    self.status = Some(*s);
                    self.loading = false;
                    self.refreshed = Some(Instant::now());
                }
                Msg::LibSize(bytes) => self.lib_bytes = Some(bytes),
                Msg::Room(which, changed, deleted) => self.room[which as usize] = Some((changed, deleted)),
                Msg::Pairs(which, when, rels) => {
                    let p = &mut self.pairs[which as usize];
                    if p.when == when {
                        p.rels = Some(rels);
                    }
                }
                Msg::Judged(which, rel, sound) => {
                    let p = &mut self.pairs[which as usize];
                    p.verdicts.insert(rel, sound);
                    if let Some(j) = &mut p.judging {
                        j.0 += 1;
                    }
                }
                Msg::JudgeDone(which) => self.pairs[which as usize].judging = None,
                Msg::BroughtBack(which, how) => {
                    let p = &mut self.pairs[which as usize];
                    p.bringing = false;
                    p.note = Some(match how {
                        Ok(d) if d.problems.is_empty() => (format!("Brought {} back from the backup. The damaged {} kept in the undo backups; Repair history on the Health page can undo this.", plural(d.changed as u64, "file"), if d.changed == 1 { "file is" } else { "files are" }), true),
                        Ok(d) if d.changed > 0 => (format!("Brought {} back from the backup. Not done: {}", plural(d.changed as u64, "file"), d.problems.join("; ")), false),
                        Ok(d) => (format!("Nothing was brought back: {}", d.problems.join("; ")), false),
                        Err(why) => (format!("Nothing was brought back: {why}"), false),
                    });
                    // look again at which pairs still differ
                    (p.rels, p.asked) = (None, false);
                }
                Msg::Configured(how) => {
                    self.configuring = false;
                    self.setup_note = Some(match how { Ok(words) => (words, true), Err(why) => (why, false) });
                    self.refresh(ctx, false);
                }
                Msg::Ejected(how) => {
                    self.ejecting = false;
                    let name = self.status.as_ref().map(|s| s.second.label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| "The drive".to_string());
                    self.eject_note = Some(match how {
                        Ok(()) => (format!("{name} can be pulled out now."), true),
                        Err(why) => (why, false),
                    });
                    self.refresh(ctx, false);
                }
                Msg::Line(l) => {
                    if let Some(job) = &mut self.job {
                        job.note(&l);
                        job.lines.push(l);
                    }
                }
                Msg::Copied(files, bytes) => {
                    if let Some(job) = &mut self.job {
                        if job.phase == Phase::Copying {
                            job.done_files = files.min(job.total_files);
                            job.done_bytes = bytes;
                        }
                    }
                }
                Msg::Done { code, problem } => {
                    let Some(job) = self.job.take() else { continue };
                    let mut run = parse_runs(&job.lines.join("\n")).pop().unwrap_or_default();
                    // the script also ends with code 1 when copies were bad or damaged: the run says so itself
                    let said = run.robocopy_fail || run.bad_count > 0 || run.check.as_ref().is_some_and(|c| c.finished && c.damaged_count > 0);
                    if job.stopped {
                        // stopped on a click: not an error, and a check then has no closing line
                        run.error = None;
                    } else if run.error.is_none() {
                        if let Some(p) = problem {
                            run.error = Some(p);
                        } else if code != Some(0) && !said {
                            run.error = Some(match code {
                                Some(c) => format!("the backup script stopped early (code {c})"),
                                None => "the backup script was stopped".into(),
                            });
                        }
                    }
                    if job.kind == Kind::CheckAll && run.check.is_none() && run.error.is_none() {
                        run.check = Some(Check::default());
                    }
                    if job.kind.copies() || matches!(job.kind, Kind::Recopy(_)) {
                        // what the run kept and moved changes the room they take: measured again
                        self.room_for[job.which as usize].clear();
                    }
                    if let Kind::Recopy(list) = &job.kind {
                        let _ = std::fs::remove_file(list); // the list was the app's own scratch file
                        let p = &mut self.pairs[job.which as usize];
                        let (text, tone) = run.headline();
                        let more = if run.not_repaired.is_empty() { String::new() } else { format!(": {}", run.not_repaired.join("; ")) };
                        p.note = Some((format!("{text}{more}. The copies that were replaced are kept in _changed in the backup for 30 days."), tone == Tone::Good));
                        (p.rels, p.asked) = (None, false);
                    }
                    match job.kind {
                        Kind::Preview => self.preview = Some((job.which, run, Local::now().naive_local())),
                        kind => self.finished = Some((job.which, kind, run)),
                    }
                    self.refresh(ctx, true);
                }
            }
        }
    }

    /// Reads the drive, the log and the nightly task again, in the background. Changes nothing.
    fn refresh(&mut self, ctx: &egui::Context, look_for_script: bool) {
        if self.loading {
            return;
        }
        self.loading = true;
        let (tx, ctx, lib) = (self.tx.clone(), ctx.clone(), self.lib.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Status(Box::new(gather(&lib, look_for_script))));
            ctx.request_repaint();
        });
    }

    /// Starts the script. Only ever called from a click on the page (and from `--backup-preview`, which
    /// only previews).
    fn start(&mut self, which: Which, kind: Kind, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let job = Job::new(which, kind.clone());
        let stop = job.stop.clone();
        self.job = Some(job);
        self.finished = None;
        self.eject_note = None;
        if kind.copies() {
            self.preview = None; // it describes the backup as it was before this run
        }
        let (tx, ctx, lib) = (self.tx.clone(), ctx.clone(), self.lib.clone());
        std::thread::spawn(move || run_job(which, kind, lib, stop, tx, ctx));
    }

    /// The damaged copies a check of the whole backup found, and what can be done about them. The check
    /// only knows that a file and its copy differ. "Find out which file is sound" listens to both; then
    /// the copies whose library file is sound can be made again from the library, and the library files
    /// whose copy is sound can be brought back from the backup (through the undo backups). Each on a click.
    fn damaged(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which, run: &Run) {
        let Some(check) = run.check.as_ref().filter(|c| c.damaged_count > 0) else { return };
        let at = which as usize;
        // where the two sides are: the library, and this backup's folder (the second one only when plugged in)
        let backup = self.status.as_ref().and_then(|s| match which {
            Which::Main => s.dest_exists.then(|| s.dest.clone()),
            Which::Second => s.second.folder.clone(),
        });
        let library = PathBuf::from(&self.lib);
        if self.pairs[at].when != run.when {
            self.pairs[at] = Pairs { when: run.when, ..Default::default() };
        }
        ui.add_space(6.0);
        let Some(backup) = backup.map(PathBuf::from) else {
            file_list(ui, &check.damaged, check.damaged_count, RED());
            line(ui, "Plug in the drive to find out which file of each pair is sound.", DIM());
            return;
        };
        // which of them still differ (one that was repaired since is not shown again): looked at once, in the background
        if !self.pairs[at].asked {
            self.pairs[at].asked = true;
            let rels: Vec<String> = check.damaged.iter().map(|l| damaged_rel(l)).collect();
            let (tx, ctx2, lib, bk, when) = (self.tx.clone(), ctx.clone(), library.clone(), backup.clone(), run.when);
            std::thread::spawn(move || {
                let _ = tx.send(Msg::Pairs(which, when, still_different(&lib, &bk, &rels)));
                ctx2.request_repaint();
            });
        }
        let idle = self.job.is_none();
        let p = &self.pairs[at];
        if let Some((words, good)) = &p.note {
            line(ui, words, if *good { GREEN() } else { WARN() });
            ui.add_space(4.0);
        }
        let Some(rels) = p.rels.clone() else {
            line(ui, "Looking at the damaged copies", DIM());
            return;
        };
        if rels.is_empty() {
            line(ui, "None of these is damaged any more: each copy is the same as the library's file now.", GREEN());
            return;
        }
        // each pair, and what listening found
        const SHOW: usize = 12;
        let mut open: Option<String> = None;
        for rel in rels.iter().take(SHOW) {
            ui.add(Label::new(RichText::new(rel).size(13.0).color(RED())).wrap());
            if let Some(sound) = p.verdicts.get(rel) {
                ui.add(Label::new(RichText::new(sound.words()).size(12.5).color(if matches!(sound, Sound::Library | Sound::Copy) { TEXT() } else { DIM() })).wrap());
                if matches!(sound, Sound::Both | Sound::Neither | Sound::NotAudio) && theme::link_sized(ui, "Open both folders", 12.5, true).clicked() {
                    open = Some(rel.clone());
                }
                ui.add_space(4.0);
            }
        }
        if rels.len() > SHOW {
            ui.label(RichText::new(format!("and {} more (the log has them all)", commas((rels.len() - SHOW) as u64))).size(12.5).color(FAINT()));
        }
        if let Some(rel) = open {
            for root in [&library, &backup] {
                if let Some(dir) = inside(root, &rel).and_then(|f| f.parent().map(Path::to_path_buf)) {
                    std::thread::spawn(move || {
                        let _ = open::that(dir);
                    });
                }
            }
        }
        ui.add_space(6.0);
        let count = |s: Sound| rels.iter().filter(|r| p.verdicts.get(*r) == Some(&s)).count();
        let (from_library, from_backup) = (count(Sound::Library), count(Sound::Copy));
        let unjudged: Vec<String> = rels.iter().filter(|r| !p.verdicts.contains_key(*r)).cloned().collect();
        let (mut listen, mut stop, mut recopy, mut bring) = (false, false, false, false);
        if let Some((done, total, _)) = &p.judging {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.add(egui::Spinner::new().size(13.0).color(ACCENT2()));
                ui.label(RichText::new(format!("Listening to both files of each pair, {} of {}", done.min(total), total)).color(TEXT()));
                stop = theme::link(ui, "Stop listening").clicked();
            });
        } else if !unjudged.is_empty() {
            line(ui, "The check only knows that each of these differs from its copy, not which of the two is damaged.", DIM());
            ui.add_space(2.0);
            listen = theme::main_action(ui, "Find out which file is sound", true).clicked();
            ui.label(RichText::new("Listens to both files of each pair, as a full check on the Health page does. Nothing is changed.").size(12.0).color(FAINT()));
        }
        if from_library > 0 {
            ui.add_space(6.0);
            let words = if from_library == 1 { "Repair this copy from the library".to_string() } else { format!("Repair these {from_library} copies from the library") };
            recopy = theme::link_sized(ui, &words, 14.0, idle && !p.bringing).clicked();
            ui.label(RichText::new("The library's file is sound: a fresh copy is made and compared. The damaged copy is kept in _changed in the backup for 30 days.").size(12.0).color(FAINT()));
        }
        if from_backup > 0 {
            ui.add_space(6.0);
            let words = if p.bringing { "Bringing them back".to_string() } else if from_backup == 1 { "Bring this file back from the backup".to_string() } else { format!("Bring these {from_backup} files back from the backup") };
            bring = theme::link_sized(ui, &words, 14.0, idle && !p.bringing).clicked();
            ui.label(RichText::new("The copy is sound: it takes the damaged file's place in the library. The damaged file is kept in the undo backups, and Repair history on the Health page can put it back.").size(12.0).color(FAINT()));
        }
        let of = |s: Sound| -> Vec<String> { rels.iter().filter(|r| p.verdicts.get(*r) == Some(&s)).cloned().collect() };
        let (library_sound, copy_sound) = (of(Sound::Library), of(Sound::Copy));
        if stop {
            if let Some((_, _, flag)) = &self.pairs[at].judging {
                flag.store(true, Ordering::Relaxed);
            }
        }
        if listen {
            let flag = Arc::new(AtomicBool::new(false));
            self.pairs[at].judging = Some((0, unjudged.len(), flag.clone()));
            let (tx, ctx2, lib, bk) = (self.tx.clone(), ctx.clone(), library.clone(), backup.clone());
            std::thread::spawn(move || {
                let tools = crate::importer::tools::find_tool("ffmpeg").and_then(|ffmpeg| crate::importer::tools::find_tool("ffprobe").map(|ffprobe| (ffmpeg, ffprobe)));
                if let Ok((ffmpeg, ffprobe)) = tools {
                    for rel in unjudged {
                        if flag.load(Ordering::Relaxed) {
                            break;
                        }
                        let sound = match (inside(&lib, &rel), inside(&bk, &rel)) {
                            (Some(a), Some(b)) => judge_pair(&ffmpeg, &ffprobe, &a, &b),
                            _ => Sound::Gone,
                        };
                        let _ = tx.send(Msg::Judged(which, rel, sound));
                        ctx2.request_repaint();
                    }
                }
                let _ = tx.send(Msg::JudgeDone(which));
                ctx2.request_repaint();
            });
        }
        if recopy {
            // the list the script reads: one path per line, as the check printed them
            let list = std::env::temp_dir().join(format!("music-recs-recopy-{}-{:x}.txt", std::process::id(), crate::importer::scan::rand_id()));
            match std::fs::write(&list, library_sound.join("\r\n")) {
                Ok(()) => {
                    self.pairs[at].note = None;
                    self.start(which, Kind::Recopy(list), ctx);
                }
                Err(e) => self.pairs[at].note = Some((crate::util::plain_err(format!("The list of files to repair could not be written: {e}")), false)),
            }
        }
        if bring {
            self.pairs[at].bringing = true;
            self.pairs[at].note = None;
            let (tx, ctx2, lib, bk) = (self.tx.clone(), ctx.clone(), library.clone(), backup.clone());
            std::thread::spawn(move || {
                let done = crate::importer::tools::find_tool("ffmpeg").and_then(|ffmpeg| {
                    let _backup_lock = native::RunLock::take(&bk)?;
                    native::validate_binding(&places(), which, &lib, &bk)?;
                    let ffprobe = crate::importer::tools::find_tool("ffprobe")?;
                    let done = bring_back(&lib, &bk, &crate::repair::backups_root(&lib), &copy_sound, &ffmpeg, &ffprobe);
                    // what was heard of the files that came in takes the place of the saved results
                    crate::importer::quality::save_cache(&crate::config::cache_dir().join("quality-cache.txt"));
                    done
                });
                let _ = tx.send(Msg::BroughtBack(which, done));
                ctx2.request_repaint();
            });
        }
    }

    /// Ends a check of the whole backup (it only reads, so nothing is left half done).
    fn stop_check(&mut self) {
        if let Some(job) = self.job.as_mut().filter(|j| j.kind == Kind::CheckAll) {
            job.stopped = true;
            job.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Why a backup can't start right now, if it can't.
    fn blocked(&self) -> Option<(String, Color32)> {
        let Some(s) = &self.status else { return Some(("Checking the backup drive".into(), DIM())) };
        if s.lib_missing {
            return Some((format!("{} was not found", self.lib), RED()));
        }
        match &s.drive {
            Drive::NoDest => return Some(("No backup folder has been chosen yet".into(), WARN())),
            Drive::Missing(d) => return Some((format!("{d} is not connected"), WARN())),
            Drive::Wrong { drive, .. } => return Some((format!("The wrong disk is at {drive}"), RED())),
            Drive::Good { .. } => {}
        }
        if s.other_running {
            return Some(("A backup is already running".into(), WARN()));
        }
        None
    }

    /// Draws the page in the space under the app's row of page names.
    pub fn ui(&mut self, ui: &mut Ui, cfg: &Config) {
        let ctx = ui.ctx().clone();
        self.lib = cfg.music_dir.trim().trim_end_matches('\\').to_string();
        if self.lib.len() == 2 && self.lib.ends_with(':') {
            self.lib.push('\\');
        }
        // another folder for the backup tool was chosen in Settings: everything is read again from there
        if self.tool_folder(cfg) && self.job.is_none() {
            self.refreshed = None;
        }
        // Keep the drive check fresh while the page is showing (a disk may be plugged in or pulled out).
        let other = self.status.as_ref().map(|s| s.other_running).unwrap_or(false);
        let every = Duration::from_secs(if other { 5 } else { 30 });
        if self.job.is_none() && !self.loading && self.refreshed.map(|t| t.elapsed() >= every).unwrap_or(true) {
            let first = self.refreshed.is_none();
            self.refresh(&ctx, first || other);
        }
        // debug, like `--screenshot`: `--backup-preview` runs one preview (never a backup) once the page is ready
        if !self.debug_preview_done && self.status.is_some() && self.job.is_none() {
            self.debug_preview_done = true;
            if std::env::args().any(|a| a == "--backup-preview") && self.blocked().is_none() {
                self.start(Which::Main, Kind::Preview, &ctx);
            }
        }
        ctx.request_repaint_after(every);

        egui::CentralPanel::default().frame(theme::page_frame()).show(ui, |ui| {
            ScrollArea::vertical().id_salt("backup-scroll").auto_shrink([false, false]).show(ui, |ui| {
                if self.status.as_ref().map(|s| s.tool_missing).unwrap_or(false) {
                    // the backup tool is not where it is looked for: say where that is, and where to change it
                    let (what, how) = match tool_dir() {
                        Some(dir) => (format!("The backup settings folder was not found in {}", dir.display()), "Choose its current folder under Settings, Files."),
                        None => ("No backup tool folder is chosen".to_string(), "Choose a folder for backup settings under Settings, Files."),
                    };
                    theme::empty(ui, &what);
                    ui.vertical_centered(|ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new(how).color(DIM()));
                        ui.add_space(4.0);
                        if theme::link(ui, "Open Settings").clicked() {
                            crate::links::go(crate::links::Go::SettingsFiles);
                        }
                    });
                    return;
                }
                self.page(ui, &ctx);
            });
        });
    }

    /// Why the second backup can't start right now, if it can't.
    fn second_blocked(&self) -> Option<(String, Color32)> {
        let s = self.status.as_ref()?;
        if s.lib_missing {
            return Some((format!("{} was not found", self.lib), RED()));
        }
        None
    }

    /// The drives that can be pulled out, one row each; a click on a row chooses it, and one more on
    /// "Start" begins the first backup to it.
    fn choose_drive(&mut self, ui: &mut Ui, ctx: &egui::Context, sec: &Second, can: bool) {
        // how large the library is decides which drives have room: measured once, in the background
        if !self.lib_bytes_asked {
            self.lib_bytes_asked = true;
            let (tx, ctx, lib) = (self.tx.clone(), ctx.clone(), self.lib.clone());
            std::thread::spawn(move || {
                let bytes = crate::importer::util::walk_files(Path::new(&lib)).iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
                let _ = tx.send(Msg::LibSize(bytes));
                ctx.request_repaint();
            });
        }
        // the drive the second backup is on now is not offered again
        let drives: Vec<&Removable> = sec.drives.iter().filter(|d| !sec.folder.as_ref().is_some_and(|f| f.eq_ignore_ascii_case(&d.dest()))).collect();
        if drives.is_empty() {
            line(ui, "Plug in the drive you want to use. It shows here when Windows sees it.", DIM());
            return;
        }
        line(ui, if drives.len() == 1 { "This drive is plugged in. Click it to choose it:" } else { "These drives are plugged in. Click one to choose it:" }, DIM());
        ui.add_space(4.0);
        let mut pick: Option<Removable> = None;
        for d in drives {
            let fits = self.lib_bytes.is_none_or(|need| need <= d.total);
            let picked = self.chosen.as_ref() == Some(d);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                let (mark, _) = ui.allocate_exact_size(vec2(3.0, 22.0), Sense::hover());
                if picked {
                    ui.painter().rect_filled(mark, 1, theme::ACCENT());
                }
                if fits {
                    if theme::link_sized(ui, &d.name, 16.0, true).clicked() {
                        pick = Some(d.clone());
                    }
                } else {
                    ui.label(RichText::new(&d.name).size(16.0).color(FAINT()));
                }
                ui.label(RichText::new(format!("{} free of {}", fmt_size(d.free), fmt_size(d.total))).color(DIM()));
                if let (false, Some(need)) = (fits, self.lib_bytes) {
                    ui.label(RichText::new(format!("not enough room: the library is {}", fmt_size(need))).color(WARN()));
                }
            });
        }
        if pick.is_some() {
            self.chosen = pick;
        }
        let Some(d) = self.chosen.clone() else { return };
        ui.add_space(8.0);
        let size = self.lib_bytes.map(|b| format!(" ({})", fmt_size(b))).unwrap_or_default();
        line(ui, &format!("Back up to {}, in the folder \"{BACKUP_FOLDER}\". The first run copies the whole library{size} and then reads every file back to check it, so it takes a while. Nothing else on the drive is touched.", d.name), TEXT());
        if self.lib_bytes.is_some_and(|need| need > d.free) {
            line(ui, &format!("Only {} is free on it now. If part of that is an older backup in that folder, it may still fit; the backup stops before copying if it does not.", fmt_size(d.free)), WARN());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 22.0;
            if theme::main_action(ui, "Start", can).clicked() {
                self.start(Which::Second, Kind::Setup(d.dest()), ctx);
                self.choosing = false;
                self.chosen = None;
            }
            if theme::link(ui, "Not now").clicked() {
                self.chosen = None;
                self.choosing = false;
            }
        });
    }



}

/// What a backup did besides copying, as short phrases for one quiet line: old copies kept, what moved to
/// `_deleted`, what was saved beside the music, and whether the copies were checked.
fn run_notes(run: &Run) -> Vec<String> {
    let mut notes = vec![];
    if let Some(n) = run.kept_old {
        notes.push(format!("Old {} of {} kept", if n == 1 { "copy" } else { "copies" }, plural(n, "changed file")));
    }
    if let Some(n) = run.moved {
        notes.push(format!("{} no longer in the library moved to _deleted", plural(n, "item")));
    }
    let failed = |p: Part| run.part_problems.iter().any(|(part, _)| *part == p);
    for (part, saved, words) in [(Part::Cadence, &run.cadence, "Cadence saved"), (Part::LegacyPlayer, &run.legacy_player, "Previous player data saved"), (Part::Tool, &run.tool, "Music tool data saved"), (Part::Code, &run.code, "Code saved")] {
        if let (Some((files, size)), false) = (saved, failed(part)) {
            notes.push(format!("{words} ({}, {size})", plural(*files, "file")));
        }
    }
    if let (Some((n, _)), 0) = (&run.verified, run.bad_count) {
        notes.push(if *n == 1 { "The copy was checked against the library".to_string() } else { format!("All {} copies checked against the library", commas(*n)) });
    }
    if let Some(n) = run.cleared_removed {
        notes.push(format!("Removed files older than 30 days sent to the Recycle Bin ({})", plural(n, "set")));
    }
    if let Some(room) = &run.room_left {
        notes.push(format!("{room} left on the drive"));
    }
    notes
}

/// What went wrong in a run, each line named for the part it belongs to, with what to do about bad copies.
fn run_troubles(run: &Run) -> Vec<String> {
    let mut out: Vec<String> = run.part_problems.iter().map(|(part, what)| format!("{}: {what}", part.failed())).collect();
    if run.old_lost > 0 {
        out.push(format!("{} could not be kept and {} replaced.", if run.old_lost == 1 { "1 old copy".to_string() } else { format!("{} old copies", run.old_lost) }, if run.old_lost == 1 { "was" } else { "were" }));
    }
    out
}

/// How long ago, in plain words: "today at 3:10 PM", "yesterday", "3 days ago".
fn ago(t: NaiveDateTime) -> String {
    ago_from(t, Local::now().naive_local())
}

fn ago_from(t: NaiveDateTime, now: NaiveDateTime) -> String {
    match (now.date() - t.date()).num_days() {
        d if d <= 0 => format!("today at {}", t.format("%-I:%M %p")),
        1 => "yesterday".to_string(),
        d => format!("{d} days ago"),
    }
}

/// A short list of files from the log, one per line, and how many more there are.
fn file_list(ui: &mut Ui, files: &[String], count: u64, color: Color32) {
    const SHOW: usize = 8;
    for f in files.iter().take(SHOW) {
        ui.add(Label::new(RichText::new(f).size(12.5).color(color)).wrap());
    }
    let more = (count as usize).max(files.len()).saturating_sub(SHOW.min(files.len()));
    if more > 0 {
        ui.label(RichText::new(format!("and {} more (the log has them all)", commas(more as u64))).size(12.5).color(FAINT()));
    }
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 file", "5,000 files".
fn plural_commas(n: u64, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{} {word}s", commas(n)) }
}

/// The albums a run copied (or a preview would copy) as rows of small covers, the new ones first with the
/// blue dot new things carry everywhere. Under each: the album, then the artist. Pointing at one shows
/// how much was copied; clicking it opens the album in the Library.
fn album_covers(ui: &mut Ui, lib: &str, run: &Run, max_rows: usize) {
    let albums: Vec<(&Album, bool)> = run.new_albums.iter().map(|a| (a, true)).chain(run.changed_albums.iter().map(|a| (a, false))).collect();
    let shown = album_tiles(ui, lib, &albums, max_rows, None);
    let more = (run.new_count + run.changed_count).max(albums.len()) - shown;
    if more > 0 {
        ui.label(RichText::new(format!("and {more} more")).size(12.5).color(FAINT()));
    }
}

/// Albums as rows of small covers that reach the right edge; each is `(the album, whether it is new)`.
/// `tip` says more about the one at an index when the pointer rests on it. Returns how many were drawn.
fn album_tiles(ui: &mut Ui, lib: &str, albums: &[(&Album, bool)], max_rows: usize, tip: Option<&dyn Fn(usize) -> String>) -> usize {
    if albums.is_empty() {
        return 0;
    }
    const LINE: f32 = 40.0;
    ui.add_space(10.0);
    let ppp = ui.ctx().pixels_per_point();
    let avail = ui.available_width();
    let t = crate::covers::fill_row(avail, ppp, 104.0, 14.0, 24.0);
    let px = (t.side * ppp).round() as u32;
    let shown = albums.len().min(t.cols * max_rows);
    for (r, row) in albums[..shown].chunks(t.cols).enumerate() {
        let (row_rect, _) = ui.allocate_exact_size(vec2(avail, t.side + LINE), Sense::hover());
        ui.add_space(4.0);
        if !ui.is_rect_visible(row_rect) {
            continue;
        }
        for (j, (a, new)) in row.iter().enumerate() {
            // "Artist / Album (2003)", as the log has it
            let mut parts = a.name.splitn(3, " / ");
            let (artist, album) = (parts.next().unwrap_or("").trim(), parts.next().unwrap_or("").trim());
            let min = row_rect.min + vec2(j as f32 * (t.side + t.gap), 0.0);
            let cover = Rect::from_min_size(min, vec2(t.side, t.side));
            let rect = Rect::from_min_size(min, vec2(t.side, t.side + LINE));
            let id = ui.scope_id().with(("backup-album", &a.name));
            let resp = ui.interact(rect, id, Sense::click());
            let lift = ui.ctx().animate_bool_with_time(id.with("lift"), resp.hovered(), 0.12);
            if lift > 0.0 {
                ui.painter().rect_filled(rect.expand2(vec2(6.0, 6.0)), 8, theme::CARD_HOVER().gamma_multiply(lift));
            }
            let folder = if album.is_empty() { Path::new(lib).join(artist) } else { Path::new(lib).join(artist).join(album) };
            crate::covers::paint(ui, crate::covers::for_folder(ui.ctx(), &folder, px), cover, 5);
            if *new {
                let at = egui::pos2(cover.left() + 10.0, cover.top() + 10.0);
                ui.painter().circle_filled(at, 6.0, theme::BG());
                ui.painter().circle_filled(at, 3.5, ACCENT2());
            }
            let text = |pos: egui::Pos2, words: &str, size: f32, color: Color32, width: f32| {
                let mut job = egui::text::LayoutJob::simple_singleline(words.to_string(), FontId::proportional(size), color);
                job.wrap = egui::text::TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(pos, galley, color);
            };
            if lift > 0.0 && !a.detail.is_empty() {
                let strip = Rect::from_min_max(egui::pos2(cover.left(), cover.bottom() - 22.0), cover.max);
                ui.painter().rect_filled(strip, egui::CornerRadius { nw: 0, ne: 0, sw: 5, se: 5 }, Color32::from_black_alpha((200.0 * lift) as u8));
                text(egui::pos2(strip.left() + 6.0, strip.top() + 4.0), &a.detail, 11.0, Color32::WHITE.gamma_multiply(lift), strip.width() - 12.0);
            }
            let (first, second) = if album.is_empty() { (artist, "") } else { (album, artist) };
            text(egui::pos2(min.x + 1.0, cover.bottom() + 6.0), first, 12.5, TEXT(), t.side - 2.0);
            text(egui::pos2(min.x + 1.0, cover.bottom() + 22.0), second, 11.5, DIM(), t.side - 2.0);
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                crate::links::go(crate::links::Go::LibraryAlbum { artist: artist.to_string(), album: album.to_string() });
            }
            if let Some(tip) = tip {
                // (worked out only when it is shown)
                resp.on_hover_ui(|ui| {
                    ui.label(tip(r * t.cols + j));
                });
            }
        }
    }
    shown
}

impl Job {
    /// Follows the script's progress from the lines it prints.
    fn note(&mut self, line: &str) {
        let rx = rx();
        if line.starts_with("Comparing library with backup") {
            self.phase = Phase::Comparing;
        } else if let Some(c) = rx.to_copy.captures(line) {
            self.total_files = c[1].parse::<u64>().unwrap_or(0) + c[2].parse::<u64>().unwrap_or(0);
            self.total_text = c[3].to_string();
            self.total_bytes = parse_size(&c[3]);
            if self.kind.copies() && self.total_files > 0 {
                self.phase = Phase::Copying;
            }
        } else if let Some(c) = rx.checking.captures(line) {
            // the script's progress while it reads files back and compares them
            self.phase = if self.kind == Kind::CheckAll { Phase::Checking } else { Phase::Verifying };
            self.read_files = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0));
            self.read_sizes = (c[3].to_string(), c[4].to_string());
            self.read_left = c.get(5).map(|m| m.as_str().to_string()).unwrap_or_default();
        } else if line.starts_with("Checking every file in ") {
            self.phase = Phase::Checking;
        } else if rx.copied.is_match(line) || line.starts_with("Backup is already up to date.") || line.starts_with("Checked ") {
            // (a run that checks its copies says "Copied", checks, says "Checked", and only then saves the rest)
            self.phase = Phase::Saving;
        } else if line.starts_with("Log: ") {
            self.phase = Phase::Finishing;
        }
    }
}

// ------------------------------------------------------------------ small pieces

fn line(ui: &mut Ui, text: &str, color: Color32) {
    ui.add(Label::new(RichText::new(text).color(color)).wrap());
}

fn tone_color(tone: Tone, good: Color32) -> Color32 {
    match tone {
        Tone::Good => good,
        Tone::Bad => RED(),
        Tone::Unsure => WARN(),
        Tone::Plain => DIM(),
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // "Today at ..." and "Yesterday at ..." read as part of a sentence; month names keep their capital
        Some(f) if s.starts_with("Today") || s.starts_with("Yesterday") || s.starts_with("Tomorrow") => f.to_lowercase().collect::<String>() + c.as_str(),
        _ => s.to_string(),
    }
}


fn preview_card(ui: &mut Ui, run: &Run, lib: &str) {
    if let Some(e) = &run.error {
        line(ui, &crate::util::plain_err(format!("The preview did not work: {e}")), RED());
        return;
    }
    let files = run.new_files + run.changed_files;
    if files == 0 {
        page::title(ui, "Everything in your library is already in the backup", 20.0, GREEN());
    } else {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 34.0;
            theme::stat(ui, &files.to_string(), if files == 1 { "file to copy" } else { "files to copy" }, TEXT());
            theme::stat(ui, &run.size, "in total", TEXT());
            theme::stat(ui, &run.new_count.to_string(), if run.new_count == 1 { "new album" } else { "new albums" }, if run.new_count > 0 { GREEN() } else { DIM() });
            theme::stat(ui, &run.changed_count.to_string(), if run.changed_count == 1 { "changed album" } else { "changed albums" }, if run.changed_count > 0 { ACCENT2() } else { DIM() });
        });
        album_covers(ui, lib, run, 4);
    }
    if run.extra_dirs + run.extra_files > 0 {
        let mut parts = vec![];
        if run.extra_dirs > 0 {
            parts.push(plural(run.extra_dirs, "folder"));
        }
        if run.extra_files > 0 {
            parts.push(plural(run.extra_files, "file"));
        }
        ui.add_space(12.0);
        line(ui, &format!("In the backup but no longer in your library: {}. The next backup moves them to _deleted in the backup. Nothing is deleted.", parts.join(" and ")), DIM());
        ui.add_space(2.0);
        for e in &run.extras {
            ui.label(RichText::new(e).size(12.5).color(FAINT()));
        }
        let more = (run.extra_dirs + run.extra_files) as usize - run.extras.len().min((run.extra_dirs + run.extra_files) as usize);
        if more > 0 {
            ui.label(RichText::new(format!("and {more} more")).size(12.5).color(FAINT()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_snapshots_are_named_separately_from_legacy_legacy_player_history() {
        let text = LOG.replace("MusicBee", "Cadence");
        let runs = parse_runs(&text);
        assert!(runs[0].cadence.is_some());
        assert!(runs[0].legacy_player.is_none());
        assert!(run_notes(&runs[0]).iter().any(|s| s.contains("Cadence saved")));
        let legacy = parse_runs(LOG);
        let notes = run_notes(&legacy[0]).join(" ");
        assert!(notes.contains("Previous player data saved"));
        assert!(!notes.contains("MusicBee") && !notes.contains("Cadence"));
        assert_eq!(Part::LegacyPlayer.failed(), "The previous player data copy had a problem");
    }

    const LOG: &str = r"
==================== 2026-11-10 03:00 ====================
Backing up M:\Music  ->  N:\Music Backup

To copy: 5 new files, 36 changed files, 366 MB in total.

New albums / folders (1):
  + Grandaddy\Signal to Snow Ratio (1999)  (5 files, 32 MB)

Albums with new or changed files (4):
  ~ Lone\Lemurian (2008)  (8 files)
  ~ Lone\Levitate (2016)  (1 files)

In the backup but no longer in the library: 1 folders, 2 files.
  - Old\Album\
  - a.txt

Also saving today's copy of your MusicBee library, playlists and settings.

Copied 41 files (366 MB) in 0.4s.

MusicBee: saved library, playlists, settings and plugins (132 files, 40 MB) to N:\Music Backup\_MusicBee\2026-11-10
Log: C:\x\backup-log.txt

ERROR (2026-11-11 03:00): Drive N: is now 'Other', not 'Backup One' like last time. Not backing up.

==================== 2026-11-11 09:00 ====================
Backing up M:\Music  ->  N:\Music Backup

To copy: 0 new files, 0 changed files, 0 bytes in total.

Preview only, nothing copied.

==================== 2026-11-11 10:00 ====================
Backing up M:\Music  ->  N:\Music Backup

To copy: 2 new files, 0 changed files, 9 MB in total.

Copied 1 files (4 MB) in 3.0s.
1 problems:
  2026/11/11 10:00:01 ERROR 5 (0x00000005) Copying File M:\Music\a.mp3
robocopy reported a failure (code 8). Details: x
Log: C:\x\backup-log.txt
";

    #[test]
    fn reads_the_scripts_log() {
        let runs = parse_runs(LOG);
        assert_eq!(runs.len(), 4);
        let a = &runs[0];
        assert_eq!((a.new_files, a.changed_files, a.size.as_str()), (5, 36, "366 MB"));
        assert_eq!((a.new_count, a.changed_count, a.new_albums.len(), a.changed_albums.len()), (1, 4, 1, 2));
        assert_eq!(a.new_albums[0].name, "Grandaddy / Signal to Snow Ratio (1999)");
        assert_eq!(a.changed_albums[1].detail, "1 file");
        assert_eq!((a.extra_dirs, a.extra_files, a.extras.len()), (1, 2, 2));
        assert_eq!(a.headline().0, "Copied 41 files (366 MB) in 0.4s");
        assert_eq!(a.legacy_player, Some((132, "40 MB".to_string())));
        // an error from before the script's header is a run of its own, not part of the one before it
        let b = &runs[1];
        assert!(b.error.as_deref().unwrap().starts_with("Drive N: is now 'Other'"));
        assert_eq!(b.when, parse_stamp("2026-11-11 03:00"));
        assert!(b.headline().0.starts_with("Did not start: "));
        assert!(runs[2].preview);
        let d = &runs[3];
        assert_eq!(d.headline(), ("Copied 1 file (4 MB) with 1 problem".to_string(), Tone::Bad));
        assert!(d.robocopy_fail && d.error.is_none());
    }

    const NEW_LOG: &str = r"
==================== 2026-11-11 14:00 ====================
Backing up M:\Music  ->  N:\Music Backup

To copy: 1 new files, 2 changed files, 30 MB in total.

In the backup but no longer in the library: 1 folders, 0 files.
  - Old\Album\

Also saving today's copy of your MusicBee library, playlists and settings.
Kept the old copy of 2 changed files in N:\Music Backup\_changed\2026-11-11_140000
  1 old copies could not be kept and were replaced.

Copied 3 files (30 MB) in 1.2s.
Moved 1 removed items to N:\Music Backup\_deleted\2026-11-11_140001

MusicBee: saved library, playlists, settings and plugins (208 files, 45 MB) to N:\Music Backup\_MusicBee\2026-11-11

Music tool: saved settings, lists and audio checks (61 files, 12 MB) to N:\Music Backup\_MusicTool\2026-11-11
  problem: Data (C:\Users\x\AppData\Local\music-recs): robocopy code 16 (details: C:\x\robocopy_1.txt)

Code: saved the code of the music tool and the backup tool (140 files, 3 MB) to N:\Music Backup\_Code\2026-11-11
Sent 2 sets of old copies older than 30 days to the Recycle Bin.
Log: C:\x\backup-log.txt

==================== 2026-11-11 15:00 ====================
Backing up M:\Music  ->  F:\Music Backup  (second backup)

To copy: 3 new files, 0 changed files, 30 MB in total.

Copied 3 files (30 MB) in 4.0s.

Checked 3 copied files: 2 are NOT good copies.
  bad copy: A\B\01.mp3 (the copy is not the same as the file in the library)
  bad copy: A\B\02.mp3 (the copy is not the same as the file in the library)
  Run the backup again. If the same files fail twice, do not trust this drive.

Code: saved the code of the music tool and the backup tool (140 files, 3 MB) to F:\Music Backup\_Code\2026-11-11
  problem: music-recs (C:\x\music-recs): robocopy code 8 (details: C:\x\robocopy_2.txt)
Room left on the second backup drive: 61.2 GB.
Log: C:\x\backup-log-second.txt

==================== 2026-11-11 16:00 ====================
Backing up M:\Music  ->  F:\Music Backup  (second backup)

To copy: 1 new files, 0 changed files, 4 MB in total.

Copied 1 files (4 MB) in 0.5s.

Checked 1 copied files against the library in 0.3s: all good.
Room left on the second backup drive: 61.2 GB.
Log: C:\x\backup-log-second.txt

==================== 2026-11-11 17:00 ====================
Backing up M:\Music  ->  N:\Music Backup

Checking every file in N:\Music Backup against the library. Nothing is copied or changed.

Checked 24045 files in 1h 12m: 24005 good copies, 2 DAMAGED.
  damaged: A\B\01.mp3 (same size and date, but the contents differ: the copy or the library's file is damaged)
  damaged: A\B\02.mp3 (same size and date, but the contents differ: the copy or the library's file is damaged)
38 files are new or changed since the last backup; the next backup will copy them.
Log: C:\x\backup-log.txt

==================== 2026-11-11 18:00 ====================
Backing up M:\Music  ->  N:\Music Backup

Checking every file in N:\Music Backup against the library. Nothing is copied or changed.

Checked 12 files in 2.0s: 12 good copies, none damaged.
Log: C:\x\backup-log.txt

==================== 2026-11-11 19:00 ====================
Backing up M:\Music  ->  N:\Music Backup

To copy: 0 new files, 0 changed files, 0 bytes in total.

In the backup but no longer in the library: 300 folders, 40 files.
  - Old\Album\

Backup is already up to date.

1240 of the backup's 5100 files are no longer in the library. That is a lot at once, so they were left where they are.
  If you really moved or removed that much, run the backup with -Mirror to move them to _deleted.
Log: C:\x\backup-log.txt

ERROR (2026-11-11 20:00): Another backup to N:\Music Backup is already running. Wait for it to finish, then try again.
";

    #[test]
    fn reads_what_the_script_learned_to_say() {
        let runs = parse_runs(NEW_LOG);
        assert_eq!(runs.len(), 7);
        // old copies kept, removed items moved, and a problem named for the part it follows
        let a = &runs[0];
        assert!(!a.second);
        assert_eq!((a.kept_old, a.old_lost, a.moved), (Some(2), 1, Some(1)));
        assert_eq!((a.legacy_player.clone(), a.tool.clone(), a.code.clone()), (Some((208, "45 MB".into())), Some((61, "12 MB".into())), Some((140, "3 MB".into()))));
        assert_eq!(a.part_problems, vec![(Part::Tool, r"Data (C:\Users\x\AppData\Local\music-recs): robocopy code 16".to_string())]);
        assert_eq!(a.headline(), ("Copied 3 files (30 MB). The music tool's data could not be saved".to_string(), Tone::Bad));
        assert_eq!(run_troubles(a), vec![r"The music tool's data could not be saved: Data (C:\Users\x\AppData\Local\music-recs): robocopy code 16".to_string(), "1 old copy could not be kept and was replaced.".to_string()]);
        // the part that failed is not said to be saved; the others are
        let notes = run_notes(a).join(" | ");
        assert!(notes.contains("Old copies of 2 changed files kept") && notes.contains("1 item no longer in the library moved to _deleted"), "{notes}");
        assert!(notes.contains("Previous player data saved (208 files, 45 MB)") && notes.contains("Code saved") && !notes.contains("Music tool data saved"), "{notes}");

        // the second backup: bad copies, a problem with the code, room left
        let b = &runs[1];
        assert!(b.second);
        assert_eq!((b.verified.clone(), b.bad_count, b.bad_copies.len()), (Some((3, String::new())), 2, 2));
        assert!(b.bad_copies[0].starts_with(r"A\B\01.mp3"));
        assert_eq!(b.part_problems, vec![(Part::Code, r"music-recs (C:\x\music-recs): robocopy code 8".to_string())]);
        assert_eq!(b.headline(), ("Copied 3 files (30 MB), but 2 are not good copies".to_string(), Tone::Bad));
        assert_eq!(b.room_left.as_deref(), Some("61.2 GB"));
        let c = &runs[2];
        assert_eq!((c.verified.clone(), c.bad_count, c.headline().1), (Some((1, "0.3s".into())), 0, Tone::Good));
        assert!(run_notes(c).contains(&"The copy was checked against the library".to_string()));

        // a check of the whole backup: damaged copies, and what waits for the next backup
        let d = runs[3].check.clone().unwrap();
        assert_eq!(d, Check { finished: true, files: 24045, time: "1h 12m".into(), good: 24005, damaged_count: 2, damaged: d.damaged.clone(), waiting: 38, missing: 0 });
        assert_eq!(d.damaged.len(), 2);
        assert_eq!(runs[3].headline(), ("Checked 24045 files: 2 damaged copies".to_string(), Tone::Bad));
        assert_eq!(runs[4].headline(), ("Checked 12 files in 2.0s: all good copies".to_string(), Tone::Good));
        assert_eq!(runs[4].check.as_ref().map(|c| (c.damaged_count, c.waiting)), Some((0, 0)));

        // the safety catch, and a run refused because another one is at work
        let e = &runs[5];
        assert_eq!((e.too_many, e.moved, e.up_to_date), (Some((1240, 5100)), None, true));
        assert!(runs[6].error.as_deref().unwrap().starts_with("Another backup to"));
        assert!(runs[6].headline().0.starts_with("Did not start: "));
    }

    #[test]
    fn a_tidying_step_that_failed_or_a_check_that_read_nothing_is_not_called_good() {
        let log = "==================== 2026-11-12 09:00 ====================
Backing up M:\\Music  ->  N:\\Music Backup

To copy: 1 new files, 0 changed files, 10 MB in total.

Copied 1 files (10 MB) in 2.0s.
  couldn't move N:\\Music Backup\\Old\\Album : The process cannot access the file.
Log: C:\\x\\backup-log.txt

==================== 2026-11-12 10:00 ====================
Backing up M:\\Music  ->  N:\\Music Backup

To copy: 0 new files, 0 changed files, 0 bytes in total.

Backup is already up to date.
  couldn't clear N:\\Music Backup\\_deleted\\2026-10-01_090000: Access is denied.
Log: C:\\x\\backup-log.txt

==================== 2026-11-12 11:00 ====================
Backing up M:\\Music  ->  N:\\Music Backup

Checking every file in N:\\Music Backup against the library. Nothing is copied or changed.

Checked 0 files in 0.1s: 0 good copies, none damaged.
Log: C:\\x\\backup-log.txt
";
        let runs = parse_runs(log);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].headline(), ("Copied 1 file (10 MB), but some files no longer in the library could not be moved aside (see the log)".to_string(), Tone::Bad));
        assert_eq!(runs[1].headline(), ("Old removed files could not be sent to the Recycle Bin (see the log)".to_string(), Tone::Bad));
        assert_eq!(runs[2].headline().1, Tone::Bad);
        assert!(runs[2].headline().0.contains("no files"));
    }

    #[test]
    fn a_test_run_has_its_own_backup_settings_and_log() {
        let tool = Path::new(r"X:\dev\music-backup");
        let real = places_for(tool, None);
        assert_eq!((real.config.as_path(), real.logs.as_path(), real.test), (Path::new(r"X:\dev\music-backup\config.json"), Path::new(r"X:\dev\music-backup\logs"), false));
        assert_eq!(places_for(tool, Some("".into())).config, real.config);
        let test = places_for(tool, Some(r"X:\lab\data".into()));
        assert_eq!((test.config.as_path(), test.logs.as_path(), test.test), (Path::new(r"X:\lab\data\backup\config.json"), Path::new(r"X:\lab\data\backup\logs"), true));
    }

    #[test]
    fn the_second_backup_is_found_by_its_id_file_whatever_the_drive_letter() {
        let dir = std::env::temp_dir().join(format!("music-recs-second-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let folder = dir.join("Music Backup");
        std::fs::create_dir_all(&folder).unwrap();
        // the settings as the script writes them: the id, the folder without its drive letter, when it last ran
        let full = folder.to_string_lossy().to_string();
        let config = dir.join("config.json");
        let write = |id: &str| {
            let text = serde_json::json!({ "Destination": r"N:\Music Backup", "DestinationLabel": "Main", "LastRun": "2026-11-11T03:00:07", "SecondaryId": id, "SecondaryFolder": &full[2..], "SecondaryLabel": "Travel", "SecondaryLastRun": "2026-11-09T18:30:00" });
            std::fs::write(&config, format!("\u{feff}{text}")).unwrap();
        };
        write("abc-123");
        let saved = read_config(&config);
        assert_eq!((saved.dest.as_str(), saved.second_id.as_str(), saved.second_label.as_str()), (r"N:\Music Backup", "abc-123", "Travel"));
        assert_eq!(saved.second_last, NaiveDateTime::parse_from_str("2026-11-09 18:30:00", "%Y-%m-%d %H:%M:%S").ok());
        let here = format!("{}\\", &full[..2]);
        let roots = vec![r"Q:\".to_string(), here.clone()];
        // no id file there yet: the drive is "not plugged in"
        assert_eq!(find_second(&saved, &roots), None);
        std::fs::write(folder.join(ID_FILE), "abc-123\r\n").unwrap();
        assert_eq!(find_second(&saved, &roots).map(|f| f.to_lowercase()), Some(full.to_lowercase()));
        // another backup's id file is not this one, and neither is a drive that is not looked at
        std::fs::write(folder.join(ID_FILE), "other-id").unwrap();
        assert_eq!(find_second(&saved, &roots), None);
        std::fs::write(folder.join(ID_FILE), "abc-123").unwrap();
        assert_eq!(find_second(&saved, &[r"Q:\".to_string()]), None);
        // never set up: nothing is looked for
        assert_eq!(find_second(&Saved::default(), &roots), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_librarys_drive_and_the_main_backups_drive_are_never_offered() {
        assert!(may_offer(r"G:\", r"M:\Music", r"N:\Music Backup"));
        assert!(!may_offer(r"M:\", r"M:\Music", r"N:\Music Backup"));
        assert!(!may_offer(r"m:\", r"M:\Music", r"N:\Music Backup"));
        assert!(!may_offer(r"N:\", r"M:\Music", r"n:\Music Backup"));
        // with no main backup chosen yet only the library's drive is held back
        assert!(may_offer(r"N:\", r"M:\Music", ""));
        assert!(!may_offer("", r"M:\Music", ""));
        let d = Removable { root: r"G:\".into(), name: "Travel (G:)".into(), free: 1, total: 2 };
        assert_eq!(d.dest(), r"G:\Music Backup");
        let at = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        let now = at("2026-11-11 14:00");
        assert_eq!((ago_from(at("2026-11-11 09:05"), now), ago_from(at("2026-11-10 23:59"), now), ago_from(at("2026-10-26 08:00"), now)), ("today at 9:05 AM".to_string(), "yesterday".to_string(), "16 days ago".to_string()));
    }

    #[test]
    fn says_what_about_the_backups_needs_a_look() {
        let at = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        let now = at("2026-11-11 14:00");
        let nightly = Task::Found { native: true, enabled: true, running: false, schedule: "Daily at 3:00 AM".into(), next: String::new(), result: Some(0) };
        let good = Run { when: Some(at("2026-11-11 03:00")), copied: Some((10, "142 MB".into(), "0.3s".into())), closed: true, ..Default::default() };
        let facts = |last_run: Option<&'static Run>, last_ok: Option<NaiveDateTime>, task: &'static Task, second: Option<NaiveDateTime>| Facts { chosen: true, last_run, last_ok, task, second_last: second };
        let leak = |r: Run| -> &'static Run { Box::leak(Box::new(r)) };
        let task = |t: Task| -> &'static Task { Box::leak(Box::new(t)) };
        let ok = task(nightly.clone());
        // all is well: nothing is said
        assert!(alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-11 03:00")), ok, Some(at("2026-11-05 18:00"))), now).is_empty());
        // the last run failed: said, and marked
        let failed = Run { when: Some(at("2026-11-11 03:00")), error: Some("Drive N: is now 'Other'".into()), closed: true, ..Default::default() };
        let said = alerts_for(&facts(Some(leak(failed)), Some(at("2026-11-10 03:00")), ok, None), now);
        assert_eq!(said.len(), 1);
        assert!(said[0].0.starts_with("The last backup") && said[0].0.contains("had a problem") && said[0].0.contains("Drive N: is now 'Other'") && said[0].1, "{said:?}");
        // a part of the run went wrong (the music tool's data): said too
        let part = Run { part_problems: vec![(Part::Tool, "Data: robocopy code 16".into())], ..good.clone() };
        assert!(alerts_for(&facts(Some(leak(part)), Some(at("2026-11-11 03:00")), ok, None), now)[0].0.contains("The music tool's data could not be saved"));
        // no backup has finished for two days, though the last one in the log was fine
        assert!(alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-09 14:01")), ok, None), now).is_empty(), "under two days is not said");
        let stale = alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-08 03:00")), ok, None), now);
        assert!(stale.len() == 1 && stale[0].1 && stale[0].0.starts_with("No backup has finished for 3 days. The last one was Nov 8"), "{stale:?}");
        assert_eq!(alerts_for(&facts(None, None, ok, None), now), vec![("No backup has finished yet.".to_string(), true)]);
        // the nightly task is gone, or turned off
        assert_eq!(alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-11 03:00")), task(Task::Missing), None), now), vec![("No nightly backup is set up.".to_string(), true)]);
        let off = task(Task::Found { native: true, enabled: false, running: false, schedule: String::new(), next: String::new(), result: None });
        assert!(alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-11 03:00")), off, None), now)[0].0.contains("turned off"));
        // the second backup: a gentle word after two weeks, no mark; none when it was never set up
        assert!(alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-11 03:00")), ok, Some(at("2026-10-29 09:00"))), now).is_empty(), "13 days is not said");
        assert_eq!(alerts_for(&facts(Some(leak(good.clone())), Some(at("2026-11-11 03:00")), ok, Some(at("2026-10-26 09:00"))), now), vec![("The second backup is 16 days old.".to_string(), false)]);
        // before any backup folder was chosen nothing is said here (the page says it itself)
        assert!(alerts_for(&Facts { chosen: false, last_run: None, last_ok: None, task: task(Task::Missing), second_last: None }, now).is_empty());
    }

    #[test]
    fn says_which_file_of_a_damaged_pair_is_sound() {
        let whole = Heard { readable: true, errors: 0, plays: 200.0, length: 200.0, checksum_bad: false };
        let cut = Heard { plays: 100.0, errors: 3, ..whole };
        let glitchy = Heard { errors: 2, ..whole };
        let unreadable = Heard { readable: false, plays: 0.0, ..whole };
        let bad_sum = Heard { checksum_bad: true, ..whole };
        // one of the two is clearly worse: the other is the sound one
        assert_eq!(verdict(&whole, &cut), Sound::Library);
        assert_eq!(verdict(&cut, &whole), Sound::Copy);
        assert_eq!(verdict(&whole, &glitchy), Sound::Library, "a few decode errors on one side only tell them apart");
        assert_eq!(verdict(&unreadable, &whole), Sound::Copy);
        assert_eq!(verdict(&whole, &bad_sum), Sound::Library);
        // no telling them apart
        assert_eq!(verdict(&whole, &whole), Sound::Both);
        assert_eq!(verdict(&cut, &cut), Sound::Neither);
        assert_eq!(verdict(&unreadable, &unreadable), Sound::Neither);
        // each worse in its own way: left to him
        assert_eq!(verdict(&Heard { errors: 12, ..whole }, &Heard { plays: 150.0, ..whole }), Sound::Neither);

        // the file a "damaged" line names: album folders have brackets of their own
        assert_eq!(damaged_rel(r"Test Artist\Rocket (2020)\02 Two.mp3 (same size and date, but the contents differ: the copy or the library's file is damaged)"), r"Test Artist\Rocket (2020)\02 Two.mp3");
        assert_eq!(damaged_rel(r"A\B\01.mp3"), r"A\B\01.mp3");
        // a path from the log stays inside the folder it is joined to
        let root = Path::new(r"X:\lib");
        assert_eq!(inside(root, r"A\B\01.mp3"), Some(PathBuf::from(r"X:\lib\A\B\01.mp3")));
        assert!(inside(root, r"..\x.mp3").is_none() && inside(root, r"C:\x.mp3").is_none() && inside(root, r"\x.mp3").is_none() && inside(root, "").is_none());
    }

    /// A made-up track: a tone of this many seconds.
    fn tone(ffmpeg: &Path, out: &Path, secs: u32) {
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let mut c = crate::importer::tools::command(ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={secs}"), "-c:a", "libmp3lame", "-b:a", "128k"]);
        c.arg(out).stdout(Stdio::null()).stderr(Stdio::null());
        assert!(crate::importer::tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
    }

    #[test]
    fn a_damaged_library_file_comes_back_from_the_backup_and_undo_returns_it() {
        let (ffmpeg, ffprobe) = (crate::importer::tools::find_tool("ffmpeg").unwrap(), crate::importer::tools::find_tool("ffprobe").unwrap());
        let dir = std::env::temp_dir().join(format!("music-recs-pairs-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let (lib, bk, undo) = (dir.join("lib"), dir.join("bk"), dir.join("undo"));
        let rel = r"A\Rocket (2020)\01 One.mp3";
        let (in_lib, in_bk) = (lib.join(rel), bk.join(rel));
        tone(&ffmpeg, &in_bk, 20);
        std::fs::create_dir_all(in_lib.parent().unwrap()).unwrap();
        // the library's file is the same song with its second half overwritten: same size, same date
        let sound = std::fs::read(&in_bk).unwrap();
        let mut hurt = sound.clone();
        for b in hurt.iter_mut().skip(sound.len() / 2) {
            *b = 0x55;
        }
        std::fs::write(&in_lib, &hurt).unwrap();
        let date = std::fs::metadata(&in_bk).unwrap().modified().unwrap();
        std::fs::File::options().write(true).open(&in_lib).unwrap().set_modified(date).unwrap();
        // a cover that differs, and a pair that is the same
        std::fs::write(lib.join(r"A\Rocket (2020)\cover.jpg"), b"one picture").unwrap();
        std::fs::write(bk.join(r"A\Rocket (2020)\cover.jpg"), b"two picture").unwrap();
        std::fs::write(lib.join(r"A\Rocket (2020)\same.txt"), b"same").unwrap();
        std::fs::write(bk.join(r"A\Rocket (2020)\same.txt"), b"same").unwrap();

        let rels: Vec<String> = [rel, r"A\Rocket (2020)\cover.jpg", r"A\Rocket (2020)\same.txt", r"A\Rocket (2020)\gone.mp3", r"..\outside.mp3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(still_different(&lib, &bk, &rels), vec![rel.to_string(), r"A\Rocket (2020)\cover.jpg".to_string()]);
        // listening: the copy is sound, the library's file is the damaged one; a picture cannot be heard
        assert_eq!(judge_pair(&ffmpeg, &ffprobe, &in_lib, &in_bk), Sound::Copy);
        assert_eq!(judge_pair(&ffmpeg, &ffprobe, &in_bk, &in_lib), Sound::Library);
        assert_eq!(judge_pair(&ffmpeg, &ffprobe, &in_bk, &in_bk), Sound::Both);
        assert_eq!(judge_pair(&ffmpeg, &ffprobe, &lib.join(r"A\Rocket (2020)\cover.jpg"), &bk.join(r"A\Rocket (2020)\cover.jpg")), Sound::NotAudio);
        assert_eq!(judge_pair(&ffmpeg, &ffprobe, &lib.join("nothing.mp3"), &in_bk), Sound::Gone);

        // the sound copy comes back; a path outside the library and a file with no copy are refused
        let done = bring_back(&lib, &bk, &undo, &[rel.to_string(), r"..\outside.mp3".to_string(), r"A\Rocket (2020)\gone.mp3".to_string()], &ffmpeg, &ffprobe).unwrap();
        assert_eq!((done.changed, done.problems.len()), (1, 2), "{:?}", done.problems);
        assert!(std::fs::read(&in_lib).unwrap() == sound, "the library's file is the backup's copy now");
        assert!(std::fs::read(&in_bk).unwrap() == sound, "the backup is untouched");
        assert_eq!(std::fs::metadata(&in_lib).unwrap().modified().unwrap(), date, "it keeps the copy's date, so the next backup has nothing to copy");
        assert!(still_different(&lib, &bk, &[rel.to_string()]).is_empty());
        // the damaged file is in the undo backups, and Undo puts it back
        let batches = crate::undo::batches(&undo);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].title.starts_with("Backup: brought back 1 file"));
        assert!(crate::undo::undo(&undo, done.batch.as_deref().unwrap()).unwrap().is_empty());
        assert!(std::fs::read(&in_lib).unwrap() == hurt, "Undo returns the file as it was");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_a_repair_of_copies_and_the_clearing_of_removed_files() {
        let log = r"
==================== 2026-11-11 15:00 ====================
Backing up M:\Music  ->  N:\Music Backup

Repaired 2 copies from the library. The copies they replaced are kept in N:\Music Backup\_changed\2026-11-11_150000

1 copies could NOT be repaired:
  not repaired: A\B\03.mp3 (the file is not in the library)
Log: C:\x\backup-log.txt

==================== 2026-11-11 16:00 ====================
Backing up M:\Music  ->  N:\Music Backup

To copy: 0 new files, 0 changed files, 0 bytes in total.

Backup is already up to date.
Sent 1 sets of old copies older than 30 days to the Recycle Bin.
Sent 3 sets of removed files older than 30 days to the Recycle Bin.
Log: C:\x\backup-log.txt
";
        let runs = parse_runs(log);
        let a = &runs[0];
        assert!(a.recopy);
        assert_eq!((a.repaired, a.not_repaired_count, a.not_repaired.clone()), (2, 1, vec![r"A\B\03.mp3 (the file is not in the library)".to_string()]));
        assert_eq!(a.headline(), ("Repaired 2 copies from the library, but 1 could not be repaired".to_string(), Tone::Bad));
        assert_eq!(Run { recopy: true, repaired: 1, ..Default::default() }.headline(), ("Repaired 1 copy from the library".to_string(), Tone::Good));
        let b = &runs[1];
        assert!(!b.recopy && b.cleared_removed == Some(3));
        assert!(run_notes(b).iter().any(|n| n.starts_with("Removed files older than 30 days sent to the Recycle Bin")));
    }

    #[test]
    fn the_backup_tool_is_looked_for_where_settings_say() {
        // nothing chosen: no folder at all (never one fixed when the app was built, which is only there on
        // the PC it was built on)
        assert_eq!(tool_dir_for(""), None);
        assert_eq!(tool_dir_for("   "), None);
        // a folder chosen in Settings is the one, however it was typed
        assert_eq!(tool_dir_for(r"X:\tools\music-backup"), Some(PathBuf::from(r"X:\tools\music-backup")));
        assert_eq!(tool_dir_for(r#" "X:\tools\music-backup\" "#), Some(PathBuf::from(r"X:\tools\music-backup")));
        // and the script, its settings and its logs are looked for there
        let at = places_for(&tool_dir_for(r"X:\tools\music-backup").unwrap(), None);
        assert_eq!(at.config.as_path(), Path::new(r"X:\tools\music-backup\config.json"));
    }

    #[test]
    fn says_how_much_room_old_copies_and_removed_items_take() {
        let dir = std::env::temp_dir().join(format!("music-recs-room-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(dir.join(r"_changed\2026-11-11_030000\A\B")).unwrap();
        std::fs::write(dir.join(r"_changed\2026-11-11_030000\A\B\01.mp3"), vec![0u8; 3000]).unwrap();
        std::fs::write(dir.join(r"_changed\2026-11-11_030000\A\B\02.mp3"), vec![0u8; 2000]).unwrap();
        assert_eq!((folder_bytes(&dir.join("_changed")), folder_bytes(&dir.join("_deleted"))), (5000, 0));
        assert_eq!(room_words(0, 0), None);
        assert!(room_words(5000, 0).unwrap().starts_with("Old copies of changed files take 5 KB of this backup"));
        assert!(room_words(0, 3 << 30).unwrap().contains("take 3.0 GB of this backup (in _deleted, kept for 30 days)"));
        let both = room_words(5000, 3 << 30).unwrap();
        assert!(both.contains("5 KB") && both.contains("3.0 GB") && both.contains("_deleted"), "{both}");
        // a fixed disk (or a path with no drive letter) is never offered "Eject"
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(!can_eject(&format!(r"{system}\Music Backup"), false) && !can_eject(r"\\server\share\x", false) && !can_eject("", false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn follows_the_scripts_progress_while_it_checks() {
        let mut job = Job::new(Which::Second, Kind::Backup);
        for l in ["Comparing library with backup...", "To copy: 3 new files, 0 changed files, 30 MB in total."] {
            job.note(l);
        }
        assert_eq!(job.phase, Phase::Copying);
        job.note("Copied 3 files (30 MB) in 4.0s.");
        job.note("Checking 2 of 3 files, 12 MB of 30 MB");
        assert_eq!((job.phase, job.read_files, job.read_sizes.clone(), job.read_left.as_str()), (Phase::Verifying, (2, 3), ("12 MB".to_string(), "30 MB".to_string()), ""));
        job.note("Checked 3 copied files against the library in 0.3s: all good.");
        assert_eq!(job.phase, Phase::Saving);
        let mut check = Job::new(Which::Main, Kind::CheckAll);
        check.note(r"Checking every file in N:\Music Backup against the library. Nothing is copied or changed.");
        assert_eq!(check.phase, Phase::Checking);
        check.note("Checking 3400 of 24000 files, 61.2 GB of 172.4 GB - about 40m 12s left");
        assert_eq!((check.read_files, check.read_left.as_str()), ((3400, 24000), "40m 12s"));
        assert_eq!((commas(24045), commas(999), plural_commas(1, "file"), plural_commas(5000, "file")), ("24,045".to_string(), "999".to_string(), "1 file".to_string(), "5,000 files".to_string()));
    }

    #[test]
    fn counts_only_music_in_robocopy_log() {
        let lib = r"m:\music";
        let line = |class: &str, size: &str, path: &str| format!("\t    {class}\t\t{size}\t{path}");
        assert_eq!(copied_file(&line("Newer     ", " 4242751", r"M:\Music\Grandaddy\Blu Wav (2024)\01 - blu wav.mp3"), lib), Some(4242751));
        assert_eq!(copied_file(&line("New File  ", "    3622", r"M:\Music\a\b.mp3"), lib), Some(3622));
        assert_eq!(copied_file(&line("Newer     ", " 8192000", r"C:\Users\Sam\Music\MusicBee\MusicBeeLibrary.mbl"), lib), None);
        assert_eq!(copied_file(&line("New Dir   ", "       3", r"M:\Music\a\"), lib), None);
        assert_eq!(copied_file(&line("*EXTRA File ", "  12", r"N:\Music Backup\x.mp3"), lib), None);
        assert_eq!(parse_size("865 MB"), 865 << 20);
        assert_eq!(parse_size("1.3 GB"), (1.3 * (1u64 << 30) as f64) as u64);
    }
}
