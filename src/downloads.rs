//! The Soulseek side of the Downloads page: find an album, pick a copy, watch it arrive.
//!
//! The page reads top to bottom as the trip an album takes: the search box, what is arriving, then (drawn
//! by the importer) what is ready to import. While a search is open its results take the main area. How it
//! is all laid out is in `downloads/page.rs`; this file holds what the page knows and the talking to slskd.
//!
//! All talking to slskd happens on one background thread; the page only shows the latest snapshot.
//! Nothing is ever downloaded by itself: a search only lists copies, and a copy is fetched when its
//! "Download" is clicked.
use crate::dl_facts::{self, Album, Fit, Owned, Quality};
use crate::net::Net;
use crate::slskd::{self, Candidate, Client, ServerState, Settings, Transfer};
use crate::theme::{self, ACCENT2, CARD_HOVER, DIM, FAINT, GREEN, RED, TEXT, WARN};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use egui::{vec2, Align, Color32, Label, Layout, RichText, ScrollArea, Sense, TextEdit, Ui};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

mod filter;
mod page;
pub use page::PAGE;

/// How many times in a row the Soulseek client is started when it never answers, before the app stops trying.
const MAX_STARTS: u32 = 4;

#[derive(Clone, PartialEq)]
enum Setup {
    Starting,
    NotInstalled,
    /// the settings file is damaged or missing
    BadSettings(String),
    NoLogin,
    /// slskd runs but is not signed in to Soulseek (yet)
    Offline(String),
    Ready,
}

/// The downloads of one folder from one person.
#[derive(Clone, PartialEq)]
struct Group {
    user: String,
    folder: String,
    files: usize,
    done: usize,
    failed: usize,
    /// of the failed ones, how many the other person refused outright
    refused: usize,
    bytes: u64,
    done_bytes: u64,
    speed: f64,
    /// every file's id (to remove the rows), and the ones still under way (to cancel)
    all_ids: Vec<String>,
    live_ids: Vec<String>,
    /// files that slskd saves in a "CD1" style folder outside the album's own: (that folder, file name)
    parts: Vec<(String, String, u64)>,
}

impl Group {
    fn finished(&self) -> bool {
        self.done + self.failed == self.files
    }
    fn arrived(&self) -> bool {
        self.finished() && self.failed == 0
    }
    fn key(&self) -> (String, String) {
        (self.user.clone(), self.folder.clone())
    }
    fn frac(&self) -> f32 {
        if self.bytes == 0 { 0.0 } else { (self.done_bytes as f32 / self.bytes as f32).clamp(0.0, 1.0) }
    }
}

#[derive(Clone, PartialEq)]
struct SearchSnap {
    text: String,
    complete: bool,
    answers: u64,
    candidates: Vec<Candidate>,
    error: Option<String>,
    /// the number the page gave this run of the search, so an older run of the same words is not taken for it
    run: u64,
    /// how much of it has ended, 0 to 1: the share of its searches (the looks and the full one) that are in
    part: f32,
}

/// What a search needs from the Soulseek client (slskd, or made-up answers in a test run).
trait Searches {
    fn start(&self, id: &str, text: &str, look: slskd::Look) -> Result<(), String>;
    /// (has it ended, how many people have answered so far)
    fn state(&self, id: &str) -> Result<(bool, u64), String>;
    /// the answers of a search that has ended
    fn responses(&self, id: &str) -> Result<Vec<serde_json::Value>, String>;
    fn forget(&self, id: &str);
}

impl Searches for Client {
    fn start(&self, id: &str, text: &str, look: slskd::Look) -> Result<(), String> {
        self.start_search(id, text, look)
    }
    fn state(&self, id: &str) -> Result<(bool, u64), String> {
        self.search_state(id)
    }
    fn responses(&self, id: &str) -> Result<Vec<serde_json::Value>, String> {
        self.search_responses(id)
    }
    fn forget(&self, id: &str) {
        self.forget_search(id)
    }
}

/// One search as the page sees it, made of several searches for the same words (`slskd::LOOKS`): short
/// looks that end early and the full search. The answers of each join the list when it ends, and the list
/// is ranked over everything known so far. Nothing here can ask for a download.
struct Staged {
    /// each search's id, and whether its answers have been taken (or it failed and is left out)
    parts: Vec<(String, bool)>,
    /// every answer so far, one per person
    responses: Vec<serde_json::Value>,
    snap: SearchSnap,
}

impl Staged {
    fn start(s: &dyn Searches, text: String) -> Staged {
        let mut parts = vec![];
        let mut error = None;
        for (n, look) in slskd::LOOKS.iter().enumerate() {
            let id = slskd::new_id();
            match s.start(&id, &text, *look) {
                Ok(()) => parts.push((id, false)),
                // only the full search has to start: a look that cannot is simply left out
                Err(e) if n + 1 == slskd::LOOKS.len() => error = Some(e),
                Err(_) => {}
            }
        }
        Staged { parts, responses: vec![], snap: SearchSnap { text, complete: false, answers: 0, candidates: vec![], error, run: 0, part: 0.0 } }
    }

    /// Asks how each search that is still running is doing, and takes the answers of those that have ended.
    fn poll(&mut self, s: &dyn Searches) {
        if self.snap.complete || self.snap.error.is_some() {
            return;
        }
        let last = self.parts.len().saturating_sub(1);
        for n in 0..self.parts.len() {
            if self.parts[n].1 {
                continue;
            }
            let id = self.parts[n].0.clone();
            match s.state(&id) {
                Ok((ended, answers)) => {
                    self.snap.answers = self.snap.answers.max(answers);
                    if ended {
                        match s.responses(&id) {
                            Ok(more) => {
                                slskd::join_responses(&mut self.responses, more);
                                self.snap.candidates = slskd::candidates(&self.responses, &self.snap.text);
                                self.parts[n].1 = true;
                                self.snap.complete = n == last;
                            }
                            Err(e) if n == last => self.snap.error = Some(e),
                            Err(_) => self.parts[n].1 = true,
                        }
                    }
                }
                // the full search not answering is said; a look not answering is left out
                Err(e) if n == last => self.snap.error = Some(e),
                Err(_) => self.parts[n].1 = true,
            }
        }
        self.snap.part = self.parts.iter().filter(|p| p.1).count() as f32 / self.parts.len().max(1) as f32;
    }

    /// Stops and forgets every one of its searches.
    fn close(self, s: &dyn Searches) {
        for (id, _) in &self.parts {
            s.forget(id);
        }
    }
}

/// The searches under way on the background thread. One that has ended stays until the page has been sent
/// its end, then it is forgotten by the Soulseek client: the page keeps what it found.
#[derive(Default)]
struct Running(Vec<Staged>);

impl Running {
    /// Starts a search for these words (one already running for the same words is stopped first).
    fn start(&mut self, s: &dyn Searches, text: String, run: u64) {
        self.stop(s, &text);
        let mut staged = Staged::start(s, text);
        staged.snap.run = run;
        self.0.push(staged);
    }
    fn stop(&mut self, s: &dyn Searches, text: &str) {
        while let Some(at) = self.0.iter().position(|x| x.snap.text == text) {
            self.0.remove(at).close(s);
        }
    }
    fn poll(&mut self, s: &dyn Searches) {
        for x in &mut self.0 {
            x.poll(s);
        }
    }
    fn snaps(&self) -> Vec<SearchSnap> {
        self.0.iter().map(|x| x.snap.clone()).collect()
    }
    fn any(&self) -> bool {
        !self.0.is_empty()
    }
    /// Call after the page has been sent `snaps`: the searches that ended (or failed) leave.
    fn reap(&mut self, s: &dyn Searches) {
        let mut n = 0;
        while n < self.0.len() {
            if self.0[n].snap.complete || self.0[n].snap.error.is_some() {
                self.0.remove(n).close(s);
            } else {
                n += 1;
            }
        }
    }
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    setup: Setup,
    /// the searches under way, and each one once more as it ends
    searches: Vec<SearchSnap>,
    groups: Vec<Group>,
    /// folders that arrived complete since the last snapshot (their names in the downloads folder)
    just_arrived: Vec<String>,
    note: Option<String>,
}

enum Cmd {
    /// search for these words; the number names this run of it
    Search(String, u64),
    /// stop the search for these words, if it is still running
    CloseSearch(String),
    Fetch(Box<Candidate>),
    /// stop and forget these transfers: (user, id)
    Remove(Vec<(String, String)>),
    Reconnect,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    All,
    Lossless,
    Mp3,
}

/// The order of the list of copies: best first, or by one column (a click on its name).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sort {
    Best,
    Format,
    Tracks,
    Size,
    Speed,
    Wait,
}

/// Puts copies in the order asked for. Each column's order is the useful one (best sound, most tracks,
/// largest, fastest, shortest wait); copies that tie keep their best-first order.
fn sorted<'a>(list: &'a [Candidate], by: Sort) -> Vec<&'a Candidate> {
    let mut out: Vec<&Candidate> = list.iter().collect();
    match by {
        Sort::Best => {}
        Sort::Format => out.sort_by_key(|c| std::cmp::Reverse(Quality::of_format(&c.format))),
        Sort::Tracks => out.sort_by_key(|c| std::cmp::Reverse(c.tracks)),
        Sort::Size => out.sort_by_key(|c| std::cmp::Reverse(c.bytes)),
        Sort::Speed => out.sort_by_key(|c| std::cmp::Reverse(c.speed)),
        Sort::Wait => out.sort_by_key(|c| if c.free_slot { 0 } else { c.queue + 1 }),
    }
    out
}

/// What is known around the copies of a search: the album it is for, the library's copy of it, and the
/// state of the list (its order, which rows are open, which one the keys are on).
struct Facts {
    /// the album each search is for, by its words in lower case (None: nothing fits the words)
    albums: HashMap<String, Option<Album>>,
    looking: HashSet<String>,
    album_tx: Sender<(String, Result<Option<Album>, String>)>,
    album_rx: Receiver<(String, Result<Option<Album>, String>)>,
    /// searches whose lookup got no answer, and when: asked again a minute later
    album_failed: HashMap<String, Instant>,
    /// the library's copy of an album, by "artist|title", and when it was read (on a thread of its own, so a
    /// sleeping drive never stalls the window)
    owned: HashMap<String, (Instant, Option<Owned>)>,
    owned_looking: HashSet<String>,
    owned_tx: Sender<(String, Option<Owned>)>,
    owned_rx: Receiver<(String, Option<Owned>)>,
    net: Option<Arc<Net>>,
    /// a made-up search's folder: its album is read from `album.json` there
    demo_folder: Option<std::path::PathBuf>,
    sort: Sort,
    whole: bool,
    /// rows whose files are on show, and the row the arrow keys are on: (user, folder)
    open_rows: HashSet<(String, String)>,
    sel: Option<(String, String)>,
    /// the search box had the keyboard this frame (so Enter there is not taken as "download")
    box_busy: bool,
    /// what is typed in the box that narrows the copies (see `filter`), and the search it is typed for;
    /// each of the other searches keeps what was typed for it, until the app is closed
    filter: String,
    filter_for: String,
    filters: HashMap<String, String>,
    /// Ctrl+F was pressed: that box takes the keyboard
    focus_filter: bool,
    /// a pasted link being read
    link: Option<Receiver<Result<String, String>>>,
}

impl Default for Facts {
    fn default() -> Facts {
        let (album_tx, album_rx) = unbounded();
        let (owned_tx, owned_rx) = unbounded();
        Facts {
            albums: HashMap::new(),
            looking: HashSet::new(),
            album_tx,
            album_rx,
            album_failed: HashMap::new(),
            owned: HashMap::new(),
            owned_looking: HashSet::new(),
            owned_tx,
            owned_rx,
            net: None,
            demo_folder: None,
            sort: Sort::Best,
            whole: false,
            open_rows: HashSet::new(),
            sel: None,
            box_busy: false,
            filter: String::new(),
            filter_for: String::new(),
            filters: HashMap::new(),
            focus_filter: false,
            link: None,
        }
    }
}

/// What the page's main area can get from the rest of the app while it draws a search's copies.
pub struct Around<'a> {
    pub images: &'a mut crate::images::Images,
    pub music_dir: &'a std::path::Path,
    /// where the import list has a folder: None = not listed, Some(false) = waiting there, Some(true) = imported
    pub stage: &'a dyn Fn(&str) -> Option<bool>,
}

/// An album that has arrived and been handed to the import list.
struct Journey {
    user: String,
    folder: String,
    files: usize,
    bytes: u64,
    /// when the import list reported it imported (it stays on show for a few seconds after)
    filed_at: Option<Instant>,
}

/// Which of the two things the page's main area shows.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Import,
    Results,
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
enum Run {
    /// not running: what is listed is what was found
    #[default]
    Done,
    /// asked for, and the background thread has not reported it yet
    Asked,
    Running,
}

/// A search the page keeps: its words and the copies it found, so that it can be looked at again without
/// running it again.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Kept {
    text: String,
    candidates: Vec<Candidate>,
    /// when these copies were found, in seconds since 1970 (0 = nothing has been found yet)
    found_at: u64,
    /// false when the search was stopped, or the app closed, before it ended: the list may be short
    complete: bool,
    #[serde(skip)]
    run: Run,
    /// the number of its latest run (see `SearchSnap::run`)
    #[serde(skip)]
    run_id: u64,
    #[serde(skip)]
    answers: u64,
    #[serde(skip)]
    error: Option<String>,
    /// while it runs: true once the listed copies are from this run and not from the one before
    #[serde(skip)]
    fresh: bool,
    #[serde(skip)]
    asked_at: Option<Instant>,
    /// changes whenever the list of copies does, and is never the same for two kept searches (so what is
    /// worked out from the list is worked out once per change, not once per frame)
    #[serde(skip)]
    version: u64,
    /// while it runs: how much of it has ended, 0 to 1
    #[serde(skip)]
    part: f32,
}

/// How many searches are kept, how many may run at once, how many copies of each are saved to disk, and
/// how long a saved search is kept.
const KEEP_SEARCHES: usize = 10;
const AT_ONCE: usize = 3;
const SAVED_COPIES: usize = 200;
const KEEP_SECS: u64 = 30 * 24 * 3600;

/// The searches the page keeps, newest first. Nothing here can ask for a download.
struct History {
    list: Vec<Kept>,
    /// the words of the one on show
    open: Option<String>,
    /// the file they are saved in (None: they are not saved)
    store: Option<std::path::PathBuf>,
    next_run: u64,
    /// the last number given to a list of copies (see `Kept::version`)
    versions: u64,
}

impl History {
    fn load(store: Option<std::path::PathBuf>, now: u64) -> History {
        let saved = store.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Vec<Kept>>(&b).ok()).unwrap_or_default();
        let mut list: Vec<Kept> = saved.into_iter().filter(|k| !k.text.trim().is_empty() && k.found_at > 0 && now.saturating_sub(k.found_at) < KEEP_SECS).collect();
        list.truncate(KEEP_SEARCHES);
        for (n, k) in list.iter_mut().enumerate() {
            k.version = n as u64 + 1;
        }
        let versions = list.len() as u64;
        History { list, open: None, store, next_run: 1, versions }
    }

    /// Writes the kept searches that have found something (the best copies of each). One that is still
    /// running is saved as not ended.
    fn save(&self) {
        let Some(path) = &self.store else { return };
        let short: Vec<Kept> = self
            .list
            .iter()
            .filter(|k| k.found_at > 0)
            .map(|k| Kept { candidates: k.candidates.iter().take(SAVED_COPIES).cloned().collect(), complete: k.complete && k.run == Run::Done, ..k.without_copies() })
            .collect();
        let Ok(text) = serde_json::to_vec(&short) else { return };
        let tmp = path.with_extension("json.new");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    fn find(&self, text: &str) -> Option<usize> {
        let want = text.trim().to_lowercase();
        self.list.iter().position(|k| k.text.to_lowercase() == want)
    }

    fn running(&self) -> usize {
        self.list.iter().filter(|k| k.run != Run::Done).count()
    }

    /// A search for these words is about to be run: it goes to the front of the list and on show, keeping
    /// the copies an earlier run found until this one finds some. Returns the words and the run's number to
    /// send to the background thread, or why it cannot be run now.
    fn ask(&mut self, text: &str) -> Result<(String, u64), String> {
        let at = self.find(text);
        let already = at.is_some_and(|i| self.list[i].run != Run::Done);
        if !already && self.running() >= AT_ONCE {
            return Err("Three searches are running. Wait for one to end, or stop one.".into());
        }
        let mut k = match at {
            Some(i) => self.list.remove(i),
            None => Kept { text: text.trim().to_string(), candidates: vec![], found_at: 0, complete: false, run: Run::Done, run_id: 0, answers: 0, error: None, fresh: false, asked_at: None, version: 0, part: 0.0 },
        };
        let id = self.next_run;
        self.next_run += 1;
        self.versions += 1;
        (k.run, k.run_id, k.answers, k.error, k.fresh, k.asked_at, k.version, k.part) = (Run::Asked, id, 0, None, false, Some(Instant::now()), self.versions, 0.0);
        let words = k.text.clone();
        self.list.insert(0, k);
        // over the limit: the oldest one that is not running leaves
        while self.list.len() > KEEP_SEARCHES {
            match self.list.iter().rposition(|k| k.run == Run::Done) {
                Some(i) => drop(self.list.remove(i)),
                None => break,
            }
        }
        self.open = Some(words.clone());
        Ok((words, id))
    }

    /// Takes what the background thread says about the searches under way. Returns true when one ended
    /// (so the list is worth saving).
    fn take(&mut self, snaps: &[SearchSnap], now: u64) -> bool {
        let mut ended = false;
        let mut versions = self.versions;
        for k in self.list.iter_mut().filter(|k| k.run != Run::Done) {
            match snaps.iter().find(|s| s.run == k.run_id) {
                Some(s) => {
                    k.run = Run::Running;
                    (k.answers, k.part) = (s.answers, s.part);
                    // (until this run has found something, the copies of the run before stay listed)
                    if !s.candidates.is_empty() || s.complete {
                        if k.candidates != s.candidates {
                            k.candidates = s.candidates.clone();
                            versions += 1;
                            k.version = versions;
                        }
                        (k.found_at, k.complete, k.fresh) = (now, s.complete, true);
                    }
                    if s.error.is_some() {
                        k.error = s.error.clone();
                    }
                    if s.complete || s.error.is_some() {
                        k.run = Run::Done;
                        ended = true;
                    }
                }
                // it was running and is gone: it was stopped
                None if k.run == Run::Running => {
                    k.run = Run::Done;
                    ended = true;
                }
                // asked for and never heard of
                None => {
                    if k.asked_at.is_none_or(|t| t.elapsed() > Duration::from_secs(15)) {
                        k.run = Run::Done;
                        k.error = Some("The search did not start. Try again.".into());
                    }
                }
            }
        }
        self.versions = versions;
        ended
    }

    /// Drops a kept search. Returns its words if it was still running (so that it can be stopped).
    fn forget(&mut self, text: &str) -> Option<String> {
        let i = self.find(text)?;
        let k = self.list.remove(i);
        if self.open.as_deref() == Some(k.text.as_str()) {
            self.open = self.list.get(i.min(self.list.len().saturating_sub(1))).map(|n| n.text.clone());
        }
        (k.run != Run::Done).then_some(k.text)
    }

    fn shown(&self) -> Option<&Kept> {
        let open = self.open.as_ref()?;
        self.list.iter().find(|k| &k.text == open)
    }
}

impl Kept {
    /// A copy without the list of copies (which can be long).
    fn without_copies(&self) -> Kept {
        Kept { text: self.text.clone(), candidates: vec![], found_at: self.found_at, complete: self.complete, run: self.run, run_id: self.run_id, answers: self.answers, error: self.error.clone(), fresh: self.fresh, asked_at: self.asked_at, version: self.version, part: self.part }
    }
}

/// How far an album asked for by its search words has come (see `DownloadsPanel::trail`).
#[derive(Clone, Debug, PartialEq)]
pub enum Trail {
    /// the search is running
    Searching,
    /// the search ended with this many copies
    Found(usize),
    /// a copy is on its way: files done of files
    Arriving(usize, usize),
    /// a copy arrived whole, in this folder of the downloads folder
    Arrived(String),
}

/// The most advanced step of the album these words searched for: arrived, then arriving, then the search.
fn trail_of(history: &History, groups: &[Group], text: &str) -> Option<Trail> {
    let k = &history.list[history.find(text)?];
    let mine: Vec<&Group> = groups.iter().filter(|g| k.candidates.iter().any(|c| c.user == g.user && c.name == g.folder)).collect();
    if let Some(g) = mine.iter().find(|g| g.arrived()) {
        return Some(Trail::Arrived(g.folder.clone()));
    }
    if let Some(g) = mine.iter().filter(|g| !g.finished()).max_by(|a, b| a.frac().total_cmp(&b.frac())) {
        return Some(Trail::Arriving(g.done, g.files));
    }
    Some(if k.run != Run::Done { Trail::Searching } else { Trail::Found(k.candidates.len()) })
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How long ago, in plain words.
fn age_words(secs: u64) -> String {
    match secs {
        0..=119 => "just now".to_string(),
        120..=3599 => format!("{} minutes ago", secs / 60),
        3600..=7199 => "an hour ago".to_string(),
        7200..=86_399 => format!("{} hours ago", secs / 3600),
        86_400..=172_799 => "yesterday".to_string(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// The line under "Copies of ...": how the search is going, or how it went and how old the list is.
fn kept_line(k: &Kept, now: u64) -> String {
    let n = k.candidates.len();
    let age = now.saturating_sub(k.found_at);
    let copies = |n: usize| if n == 1 { "1 copy".to_string() } else { format!("{n} copies") };
    if k.run != Run::Done {
        let going = search_line(&SearchSnap { text: String::new(), complete: false, answers: k.answers, candidates: vec![], error: None, run: 0, part: 0.0 });
        return if !k.fresh && n > 0 { format!("{going} Showing the {} found {} until new ones arrive.", copies(n), age_words(age)) } else { going };
    }
    if let Some(e) = &k.error {
        return if n > 0 { crate::util::plain_err(format!("{e} Showing the {} found {}.", copies(n), age_words(age))) } else { e.clone() };
    }
    let old = if age >= 6 * 3600 { ". Some of these people may be offline by now." } else { "" };
    match (n, k.complete) {
        (0, true) if age < 120 => "Nobody online has this right now. Try fewer words.".to_string(),
        (0, true) => format!("Nobody online had this {}. Try fewer words, or search again.", age_words(age)),
        (0, false) => "Stopped before anything was found.".to_string(),
        (n, true) if age < 120 => format!("{n} found, best first"),
        (n, true) => format!("{n} found {}, best first{old}", age_words(age)),
        (n, false) if age < 120 => format!("{n} found before the search was stopped, best first"),
        (n, false) => format!("{n} found {} before the search was stopped, best first{old}", age_words(age)),
    }
}

/// A kept search's words, cut short for the line of searches.
fn short_words(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().count() <= 34 { text.into() } else { format!("{}...", text.chars().take(32).collect::<String>().trim_end()).into() }
}

pub struct DownloadsPanel {
    tx: Sender<Cmd>,
    rx: Receiver<Snapshot>,
    snap: Snapshot,
    query: String,
    /// a search asked for before Soulseek was ready
    waiting: Option<String>,
    /// (user, folder name) of copies already asked for, so their row says so
    asked: HashSet<(String, String)>,
    kind: Kind,
    /// the searches kept, the one on show, and whether the main area shows it or the import list
    history: History,
    view: View,
    /// why a search could not be run just now
    said: Option<String>,
    /// how many albums were ready to import when the import list was last on show
    seen_ready: usize,
    /// the open search's copies as they are listed, and what stands over them: worked out when something
    /// they depend on changes, not every frame (see `page::Listing`, `page::Head`)
    listing: page::Listing,
    head: Option<page::Head>,
    /// count up when the background thread reports, and when an album or the library's copy of one is known
    snap_gen: u64,
    facts_gen: u64,
    /// folders that arrived and have not been handed to the importer yet
    arrived: Vec<String>,
    /// arrived folders already handed over: no longer listed under Arriving
    handed: HashSet<(String, String)>,
    /// albums that have arrived and are now in the import list, shown one step further along until filed
    journeys: Vec<Journey>,
    /// the saved account name, re-read now and then (for "signed in as")
    name: Option<String>,
    name_at: Option<Instant>,
    /// sentences the page shows every frame, put together when what they say changes
    words: page::Words,
    child: Arc<Mutex<Option<std::process::Child>>>,
    facts: Facts,
}

impl DownloadsPanel {
    pub fn new(egui_ctx: &egui::Context) -> Self {
        let (tx, cmd_rx) = unbounded();
        let (snap_tx, rx) = unbounded();
        let child = Arc::new(Mutex::new(None));
        let (ctx, child2) = (egui_ctx.clone(), child.clone());
        std::thread::spawn(move || worker(cmd_rx, snap_tx, child2, ctx));
        Self {
            tx,
            rx,
            snap: Snapshot { setup: Setup::Starting, searches: vec![], groups: vec![], just_arrived: vec![], note: None },
            query: String::new(),
            waiting: None,
            asked: HashSet::new(),
            kind: Kind::All,
            // (a test run on made-up folders never reads or writes the real list of kept searches)
            history: History::load((!crate::importer::test_folders()).then(|| crate::config::app_dir().join("searches.json")), now_secs()),
            view: View::Import,
            said: None,
            seen_ready: 0,
            listing: page::Listing::default(),
            head: None,
            snap_gen: 0,
            facts_gen: 0,
            arrived: vec![],
            handed: HashSet::new(),
            journeys: vec![],
            name: None,
            name_at: None,
            words: page::Words::default(),
            child,
            facts: Facts::default(),
        }
    }

    /// For trying the list of copies without Soulseek (`--search-demo <folder>` with test folders): a made-up
    /// search whose answers are read from `look1.json`, `look2.json` and `full.json` in that folder (each
    /// a list of answers in slskd's shape) and handed out two, five and nine seconds in. Nothing is
    /// contacted, and "Download" on a row sends nothing anywhere.
    pub fn made_up_search(egui_ctx: &egui::Context, folder: std::path::PathBuf) -> Self {
        let mut panel = Self::demo();
        let (tx, cmd_rx) = unbounded();
        let (snap_tx, rx) = unbounded();
        let ctx = egui_ctx.clone();
        // (the kept searches of a made-up run are saved beside its made-up answers)
        panel.history = History::load(Some(folder.join("searches.json")), now_secs());
        // (and the album it is for is read from album.json there, if there is one)
        panel.facts.demo_folder = Some(folder.clone());
        // (`--dl-demo` with it keeps the made-up arriving rows on show; `--search-idle` starts with no search)
        let flag = |name: &str| std::env::args().any(|a| a == name);
        let groups = if flag("--dl-demo") { panel.snap.groups.clone() } else { vec![] };
        if !flag("--dl-demo") {
            panel.journeys = vec![];
        }
        panel.snap = Snapshot { setup: Setup::Ready, searches: vec![], groups: groups.clone(), just_arrived: vec![], note: None };
        std::thread::spawn(move || made_up_worker(cmd_rx, snap_tx, ctx, folder, groups));
        (panel.tx, panel.rx) = (tx, rx);
        if !flag("--search-idle") {
            panel.search("made-up search");
        }
        panel
    }

    /// For looking at the arriving rows without Soulseek (`--dl-demo` with test folders): made-up albums at
    /// every step of the trip. No worker thread, nothing is contacted.
    pub fn demo() -> Self {
        let (tx, _) = unbounded();
        let (_, rx) = unbounded();
        let g = |user: &str, folder: &str, files: usize, done: usize, failed: usize, refused: usize, done_bytes: u64, speed: f64| Group {
            user: user.into(),
            folder: folder.into(),
            files,
            done,
            failed,
            refused,
            bytes: 400_000_000,
            done_bytes,
            speed,
            all_ids: vec![],
            live_ids: vec!["x".into()],
            parts: vec![],
        };
        let groups = vec![
            g("blixquoy", "2026 - Temptation to Exist [Digital Media]", 10, 6, 0, 0, 248_000_000, 3_400_000.0),
            g("Hadmmaster", "Reflektor (2013) [FLAC]", 14, 0, 0, 0, 0, 0.0),
            g("ULTRNS666", "(2026) Temptation to Exist", 10, 0, 10, 10, 0, 0.0),
        ];
        let journey = |folder: &str, filed: bool| Journey { user: "oblenimim".into(), folder: folder.into(), files: 12, bytes: 310_000_000, filed_at: filed.then(Instant::now) };
        Self {
            tx,
            rx,
            snap: Snapshot { setup: Setup::Ready, searches: vec![], groups, just_arrived: vec![], note: None },
            query: String::new(),
            waiting: None,
            asked: HashSet::new(),
            kind: Kind::All,
            history: History::load(None, 0),
            view: View::Import,
            said: None,
            seen_ready: 0,
            listing: page::Listing::default(),
            head: None,
            snap_gen: 0,
            facts_gen: 0,
            arrived: vec![],
            handed: HashSet::new(),
            journeys: vec![journey("Quiet Rooms (2020)", false), journey("Low Tide (2015)", true)],
            name: None,
            name_at: None,
            words: page::Words::default(),
            child: Arc::new(Mutex::new(None)),
            facts: Facts::default(),
        }
    }

    /// Runs every frame, whichever page is showing.
    pub fn tick(&mut self, _ctx: &egui::Context) {
        // a pasted link has been read: search for what it names, or say why not
        if let Some(read) = self.facts.link.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.facts.link = None;
            match read {
                Ok(words) => self.search(&words),
                Err(why) => self.said = Some(why),
            }
        }
        while let Ok((key, album)) = self.facts.album_rx.try_recv() {
            self.facts_gen += 1;
            self.facts.looking.remove(&key);
            match album {
                Ok(album) => drop(self.facts.albums.insert(key, album)),
                // no answer (the network, or Apple busy): asked again a minute later
                Err(_) => drop(self.facts.album_failed.insert(key, Instant::now())),
            }
        }
        while let Ok((key, owned)) = self.facts.owned_rx.try_recv() {
            self.facts_gen += 1;
            self.facts.owned_looking.remove(&key);
            self.facts.owned.insert(key, (Instant::now(), owned));
        }
        while let Ok(s) = self.rx.try_recv() {
            self.arrived.extend(s.just_arrived.iter().cloned());
            if self.history.take(&s.searches, now_secs()) {
                self.history.save();
            }
            self.snap = s;
            self.snap_gen += 1;
        }
        // (a search that was asked for and never started is given up on, not left "searching")
        if self.history.list.iter().any(|k| k.run == Run::Asked) {
            self.history.take(&[], now_secs());
        }
        if self.snap.setup == Setup::Ready {
            if let Some(q) = self.waiting.take() {
                self.search(&q);
            }
        }
        if self.name_at.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.name_at = Some(Instant::now());
            self.name = slskd::login_name();
        }
    }

    /// Lets the page look things up online (the album a search is for, a pasted link). Kept from the first call.
    pub fn attach(&mut self, net: &Arc<Net>) {
        if self.facts.net.is_none() {
            self.facts.net = Some(net.clone());
        }
    }

    /// The album a search is for: asked for once per search (from Apple Music, or from `album.json` beside
    /// a made-up search), then known. None while it is being asked, or when nothing fits the words.
    fn album_for(&mut self, words: &str, ctx: &egui::Context) -> Option<Album> {
        let key = words.trim().to_lowercase();
        if let Some(known) = self.facts.albums.get(&key) {
            return known.clone();
        }
        if self.facts.looking.contains(&key) || self.facts.album_failed.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return None;
        }
        if let Some(folder) = &self.facts.demo_folder {
            let album = std::fs::read(folder.join("album.json")).ok().and_then(|b| serde_json::from_slice::<Album>(&b).ok());
            self.facts.albums.insert(key, album.clone());
            return album;
        }
        let Some(net) = self.facts.net.clone().filter(|_| !crate::importer::test_folders()) else {
            self.facts.albums.insert(key, None);
            return None;
        };
        self.facts.looking.insert(key.clone());
        let (tx, ctx) = (self.facts.album_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let album = dl_facts::look_up(&net, &key);
            let _ = tx.send((key, album));
            ctx.request_repaint();
        });
        None
    }

    /// The library's copy of an album, read on a thread of its own and again at most every half minute (an
    /// import may have just added it). Until the first answer, None; while it is read again, the last answer.
    fn owned_for(&mut self, album: &Album, around: &Around, ctx: &egui::Context) -> Option<Owned> {
        let key = format!("{}|{}", album.artist, album.title);
        let known = self.facts.owned.get(&key).cloned();
        let fresh = known.as_ref().is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(30));
        if !fresh && !self.facts.owned_looking.contains(&key) && !around.music_dir.as_os_str().is_empty() {
            self.facts.owned_looking.insert(key.clone());
            let (tx, ctx, dir, artist, title) = (self.facts.owned_tx.clone(), ctx.clone(), around.music_dir.to_path_buf(), album.artist.clone(), album.title.clone());
            std::thread::spawn(move || {
                let _ = tx.send((key, dl_facts::owned(&dir, &artist, &title)));
                ctx.request_repaint();
            });
        }
        known.and_then(|(_, o)| o)
    }

    /// Folders that have arrived complete and are waiting to be shown to the importer.
    pub fn arrived(&self) -> &[String] {
        &self.arrived
    }

    /// The importer has been told about the arrived folders and is rescanning: stop listing them as arriving.
    pub fn arrived_handed_over(&mut self) {
        self.arrived.clear();
        let done: Vec<&Group> = self.snap.groups.iter().filter(|g| g.arrived()).collect();
        let ids: Vec<(String, String)> = done.iter().flat_map(|g| g.all_ids.iter().map(|id| (g.user.clone(), id.clone()))).collect();
        for g in &done {
            if !self.journeys.iter().any(|j| j.user == g.user && j.folder == g.folder) {
                self.journeys.push(Journey { user: g.user.clone(), folder: g.folder.clone(), files: g.files, bytes: g.bytes, filed_at: None });
            }
        }
        self.handed.extend(done.iter().map(|g| g.key()));
        if !ids.is_empty() {
            let _ = self.tx.send(Cmd::Remove(ids)); // tidy slskd's own list too
        }
    }

    /// For the Home page: one small ring per album on its way (how much has arrived), then a full green ring
    /// for each one waiting to be imported. Draws nothing when there is nothing.
    pub fn paint_rings(&self, ui: &mut Ui, max: usize) -> usize {
        let live: Vec<&Group> = self.snap.groups.iter().filter(|g| !g.finished()).collect();
        let ready = self.journeys.iter().filter(|j| j.filed_at.is_none()).count();
        let mut drawn = 0;
        for g in live.iter().take(max) {
            ring(ui, g.frac(), ACCENT2());
            drawn += 1;
        }
        for _ in 0..ready.min(max.saturating_sub(drawn)) {
            ring(ui, 1.0, GREEN());
            drawn += 1;
        }
        drawn
    }

    /// How many folders are on their way right now.
    pub fn arriving_count(&self) -> usize {
        self.snap.groups.iter().filter(|g| !g.finished()).count()
    }

    /// How many folders ended without arriving (and have not been cleared).
    pub fn failed_count(&self) -> usize {
        self.snap.groups.iter().filter(|g| g.finished() && g.failed > 0).count()
    }

    /// How far the album these search words are for has come, for a row that asked for it (Cadence's
    /// list): the kept search with exactly these words, and a download of one of its copies, matched by the
    /// person and the folder's name as the copies' rows match them. None when no such search is kept.
    pub fn trail(&self, text: &str) -> Option<Trail> {
        trail_of(&self.history, &self.snap.groups, text)
    }

    /// Puts the import list in the page's main area (the kept searches stay a click away).
    pub fn show_import_list(&mut self) {
        self.view = View::Import;
    }

    /// True while a search's copies are on show in the page's main area (the import list is one click away).
    pub fn showing_results(&self) -> bool {
        self.view == View::Results && self.history.shown().is_some()
    }

    /// Where Soulseek stands, in a few words, with the color to show them in (for Settings).
    pub fn status_words(&self) -> (String, Color32) {
        match &self.snap.setup {
            Setup::Starting => ("Starting the Soulseek client".into(), DIM()),
            Setup::NotInstalled => ("The Soulseek client is not installed.".into(), RED()),
            Setup::BadSettings(why) => (why.clone(), RED()),
            Setup::NoLogin => ("No login saved yet.".into(), DIM()),
            Setup::Offline(why) => (why.clone(), WARN()),
            Setup::Ready => ("Signed in to Soulseek.".into(), GREEN()),
        }
    }

    /// Search for this text (from the box here, or asked for by another page).
    pub fn search(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.query = text.to_string();
        // a link to an album on Spotify, Apple Music or Bandcamp: its page is read for the words to search for
        if dl_facts::shop_of(text).is_some() {
            match self.facts.net.clone().filter(|_| !crate::importer::test_folders()) {
                Some(net) => {
                    let (tx, rx) = unbounded();
                    let link = text.to_string();
                    std::thread::spawn(move || {
                        let _ = tx.send(dl_facts::words_from_link(&net, &link));
                    });
                    (self.facts.link, self.said) = (Some(rx), None);
                }
                None => self.said = Some("Links are not read in a test run.".into()),
            }
            return;
        }
        if self.snap.setup != Setup::Ready {
            // asked for before Soulseek was ready (the client is still starting): run it as soon as it is
            self.waiting = Some(text.to_string());
            return;
        }
        // the search joins the kept ones and goes on show; the import list stays one click away
        match self.history.ask(text) {
            Ok((words, run)) => {
                (self.said, self.view) = (None, View::Results);
                let _ = self.tx.send(Cmd::Search(words, run));
            }
            Err(why) => self.said = Some(why),
        }
    }

    /// Stop slskd if this app started it. Downloads in flight resume next time.
    pub fn on_exit(&mut self) {
        self.history.save(); // (a search still running is kept with what it has found so far)
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Closes a kept search, stopping it first if it is still running. Another search, or the import list,
    /// goes on show in its place.
    fn close_search(&mut self, text: &str, ctx: &egui::Context) {
        if let Some(words) = self.history.forget(text) {
            let _ = self.tx.send(Cmd::CloseSearch(words));
        }
        self.history.save();
        // (the box no longer offers the words of a search that is gone)
        if self.query.trim().eq_ignore_ascii_case(text.trim()) {
            self.query.clear();
        }
        if self.history.shown().is_none() {
            self.view = View::Import;
        } else if let (View::Results, Some(open)) = (self.view, &self.history.open) {
            self.query = open.clone();
        }
        ctx.request_repaint();
    }

    /// Asks for a copy: the one place a download starts (a click on "Download", or Enter on the row the
    /// arrow keys are on).
    fn fetch(&mut self, c: &Candidate) {
        self.asked.insert((c.user.clone(), c.name.clone()));
        let _ = self.tx.send(Cmd::Fetch(Box::new(c.clone())));
    }
}

/// What the line under the album says instead of "best first" when the list is ordered by a column.
fn sort_words(by: Sort) -> &'static str {
    match by {
        Sort::Best => "best first",
        Sort::Format => "best sound first",
        Sort::Tracks => "most tracks first",
        Sort::Size => "largest first",
        Sort::Speed => "fastest first",
        Sort::Wait => "shortest wait first",
    }
}

/// The line beside "Copies of ...": how the search is going. While it runs it says how many people have
/// answered, whether or not copies are already listed under it.
fn search_line(search: &SearchSnap) -> String {
    match (&search.error, search.candidates.len(), search.complete) {
        (Some(e), _, _) => e.clone(),
        (None, _, false) if search.answers > 0 => format!("Searching. {} have answered so far.", if search.answers == 1 { "1 person".to_string() } else { format!("{} people", search.answers) }),
        (None, _, false) => "Searching. This takes about fifteen seconds.".to_string(),
        (None, 0, true) => "Nobody online has this right now. Try fewer words.".to_string(),
        (None, n, true) => format!("{n} found, best first"),
    }
}

/// What the top line says about copies that arrived while the pointer was on the list.
fn late_words(late: usize) -> String {
    if late == 1 { "1 more copy arrived. Move the pointer off the list to put it in order.".to_string() } else { format!("{late} more copies arrived. Move the pointer off the list to put them in order.") }
}

/// Keeps the order of the listed copies steady under the pointer. `ranked` is every copy known now, best
/// first. Not pointing at the list: the order is simply that. Pointing at it: the rows that are there keep
/// their place (one that is gone leaves), and copies that are new go below them, best first. Returns how
/// many were added below.
fn settle(order: &mut Vec<(String, String)>, ranked: &[(String, String)], pointing: bool) -> usize {
    if !pointing {
        *order = ranked.to_vec();
        return 0;
    }
    let known: HashSet<&(String, String)> = ranked.iter().collect();
    order.retain(|k| known.contains(k));
    let have: HashSet<(String, String)> = order.iter().cloned().collect();
    let new: Vec<(String, String)> = ranked.iter().filter(|k| !have.contains(*k)).cloned().collect();
    order.extend(new.iter().cloned());
    new.len()
}

/// A small ring that fills clockwise from the top: how much of an album has arrived.
fn ring(ui: &mut Ui, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    let (c, r) = (rect.center(), 8.0);
    ui.painter().circle_stroke(c, r, egui::Stroke::new(2.5, theme::BORDER()));
    let frac = frac.clamp(0.0, 1.0);
    if frac >= 0.999 {
        ui.painter().circle_stroke(c, r, egui::Stroke::new(2.5, color));
        return;
    }
    if frac > 0.0 {
        let n = ((frac * 48.0).ceil() as usize).max(2);
        let points: Vec<egui::Pos2> = (0..=n)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac * (i as f32 / n as f32);
                c + vec2(a.cos(), a.sin()) * r
            })
            .collect();
        ui.painter().add(egui::Shape::line(points, egui::Stroke::new(2.5, color)));
    }
}

fn is_lossless(format: &str) -> bool {
    ["FLAC", "WAV", "APE", "WV"].iter().any(|f| format.starts_with(f))
}

/// What to say about a folder that is on its way, the color to say it in, and the color of its bar.
fn arriving_words(g: &Group) -> (String, Color32, Color32) {
    if !g.finished() {
        let how = if g.speed > 1.0 {
            format!("{}/s", size_words(g.speed as u64))
        } else if g.done_bytes == 0 {
            "waiting for a free slot on their side".to_string()
        } else {
            "paused on their side".to_string()
        };
        let failed = if g.failed > 0 { format!("  ·  {} failed", g.failed) } else { String::new() };
        return (format!("{} of {} files  ·  {:.0}%  ·  {how}{failed}", g.done, g.files, g.frac() * 100.0), DIM(), ACCENT2());
    }
    if g.failed == 0 {
        return (format!("Arrived: {} files, {}. Ready to import.", g.files, size_words(g.bytes)), GREEN(), GREEN());
    }
    if g.done == 0 && g.refused == g.files {
        return ("Refused: this person does not send to your account. Pick another copy.".into(), RED(), RED());
    }
    if g.done == 0 {
        return ("Their computer did not send it. Pick another copy.".into(), RED(), RED());
    }
    (format!("{} of {} files arrived, {} did not. What arrived is in the downloads folder.", g.done, g.files, g.failed), WARN(), WARN())
}

pub(crate) fn size_words(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1_073_741_824.0 {
        format!("{:.1} GB", b / 1_073_741_824.0)
    } else if b >= 1_048_576.0 {
        format!("{:.0} MB", b / 1_048_576.0)
    } else {
        format!("{:.0} KB", (b / 1024.0).max(1.0))
    }
}

fn group(transfers: &[Transfer]) -> Vec<Group> {
    let mut out: Vec<Group> = vec![];
    for t in transfers {
        let i = match out.iter().position(|g| g.user == t.user && g.folder == t.folder) {
            Some(i) => i,
            None => {
                out.push(Group { user: t.user.clone(), folder: t.folder.clone(), files: 0, done: 0, failed: 0, refused: 0, bytes: 0, done_bytes: 0, speed: 0.0, all_ids: vec![], live_ids: vec![], parts: vec![] });
                out.len() - 1
            }
        };
        let g = &mut out[i];
        if !t.disc.is_empty() {
            g.parts.push((t.disc.clone(), t.file.clone(), t.size));
        }
        g.files += 1;
        g.bytes += t.size;
        g.done_bytes += t.done_bytes.min(t.size);
        g.all_ids.push(t.id.clone());
        if t.finished() {
            if t.ok() {
                g.done += 1
            } else {
                g.failed += 1;
                if t.state.contains("Rejected") {
                    g.refused += 1;
                }
            }
        } else {
            g.speed += t.speed;
            g.live_ids.push(t.id.clone());
        }
    }
    out
}

/// Folders that have just arrived complete, by the name they have in the downloads folder. A folder name that
/// something else is still writing into (the same album asked for from two people) is held back until that is done too.
fn newly_arrived(groups: &[Group], seen: &HashSet<(String, String)>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for g in groups.iter().filter(|g| g.arrived() && !seen.contains(&g.key())) {
        let shared = groups.iter().any(|o| o.folder == g.folder && !o.finished());
        if !shared && !out.contains(&g.folder) {
            out.push(g.folder.clone());
        }
    }
    out
}

/// Moves the disc folders of an album that has arrived under the album's own name:
/// `<downloads>\CD1\01.flac` becomes `<downloads>\<album>\CD1\01.flac`, so the import list sees one album.
///
/// Only the files this download brought are moved (another album's "CD1" may be in the same folder), and
/// only if every one of them is there and none would land on a file that exists: otherwise nothing is
/// touched. All the names come from a stranger's computer, so each must be exactly one folder level.
fn gather_discs(downloads: &std::path::Path, album: &str, parts: &[(String, String, u64)]) -> Result<(), String> {
    use crate::importer::is_folder_name;
    let not_gathered = || format!("The discs of '{album}' are in the downloads folder as they arrived.");
    if !is_folder_name(album) || parts.iter().any(|(disc, file, _)| !slskd::is_disc_folder(disc) || !is_folder_name(disc) || !is_folder_name(file)) {
        return Err(not_gathered());
    }
    let dest = downloads.join(album);
    let moves: Vec<(std::path::PathBuf, std::path::PathBuf, u64)> = parts.iter().map(|(disc, file, size)| (downloads.join(disc).join(file), dest.join(disc).join(file), *size)).collect();
    // a file of another size in the disc folder is another album's of the same name: that is not ours to move
    let ours = |from: &std::path::Path, size: u64| std::fs::metadata(from).is_ok_and(|m| m.is_file() && (size == 0 || m.len() == size));
    if moves.iter().any(|(from, to, size)| !ours(from, *size) || to.exists()) {
        return Err(not_gathered());
    }
    let mut moved: Vec<&(std::path::PathBuf, std::path::PathBuf, u64)> = vec![];
    for m in &moves {
        let made = m.1.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok());
        if !made || std::fs::rename(&m.0, &m.1).is_err() {
            // put back what was moved, so the folders are as slskd left them
            for back in moved.iter().rev() {
                let _ = std::fs::rename(&back.1, &back.0);
            }
            return Err(not_gathered());
        }
        moved.push(m);
    }
    // the disc folders are removed if this emptied them (a folder with anything left in it stays)
    for (disc, _, _) in parts {
        let _ = std::fs::remove_dir(downloads.join(disc));
    }
    Ok(())
}

// ------------------------------------------------------------------ the background thread

/// A made-up Soulseek for test runs: three searches whose answers come from files and are handed out at
/// set times. It contacts nothing.
struct MadeUp {
    folder: std::path::PathBuf,
    /// each search started: its id, when, and which of the three it is
    started: Mutex<Vec<(String, Instant, usize)>>,
}

impl MadeUp {
    /// when each of the three ends, in seconds, and how many "people" it reports at the end
    const ENDS: [(f32, u64); 3] = [(2.0, 20), (5.0, 80), (9.0, 163)];
    const FILES: [&'static str; 3] = ["look1.json", "look2.json", "full.json"];

    fn find(&self, id: &str) -> Option<(Instant, usize)> {
        self.started.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|s| s.0 == id).map(|s| (s.1, s.2))
    }
}

impl Searches for MadeUp {
    fn start(&self, id: &str, _text: &str, _look: slskd::Look) -> Result<(), String> {
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        let which = started.len() % 3;
        started.push((id.to_string(), Instant::now(), which));
        Ok(())
    }
    fn state(&self, id: &str) -> Result<(bool, u64), String> {
        let (at, which) = self.find(id).ok_or("no such search")?;
        let (ends, people) = Self::ENDS[which];
        let t = at.elapsed().as_secs_f32();
        Ok((t >= ends, ((t / ends).min(1.0) * people as f32) as u64))
    }
    fn responses(&self, id: &str) -> Result<Vec<serde_json::Value>, String> {
        let (_, which) = self.find(id).ok_or("no such search")?;
        let text = std::fs::read(self.folder.join(Self::FILES[which])).map_err(|e| crate::util::plain_err(format!("The made-up answers could not be read: {e}")))?;
        serde_json::from_slice::<Vec<serde_json::Value>>(&text).map_err(|e| crate::util::plain_err(format!("The made-up answers could not be read: {e}")))
    }
    fn forget(&self, id: &str) {
        self.started.lock().unwrap_or_else(|e| e.into_inner()).retain(|s| s.0 != id);
    }
}

/// The worker behind `DownloadsPanel::made_up_search`: the same staged search as the real one, fed by
/// `MadeUp`. A request to download is dropped here: a test run fetches nothing.
fn made_up_worker(cmds: Receiver<Cmd>, out: Sender<Snapshot>, ctx: egui::Context, folder: std::path::PathBuf, groups: Vec<Group>) {
    let made_up = MadeUp { folder, started: Mutex::new(vec![]) };
    let mut searches = Running::default();
    let mut last: Option<Snapshot> = None;
    loop {
        searches.poll(&made_up);
        let snap = Snapshot { setup: Setup::Ready, searches: searches.snaps(), groups: groups.clone(), just_arrived: vec![], note: None };
        if last.as_ref() != Some(&snap) {
            let _ = out.send(snap.clone());
            ctx.request_repaint();
            last = Some(snap);
        }
        searches.reap(&made_up);
        match cmds.recv_timeout(Duration::from_millis(300)) {
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
            Ok(Cmd::Search(text, run)) => searches.start(&made_up, text, run),
            Ok(Cmd::CloseSearch(text)) => searches.stop(&made_up, &text),
            Ok(_) => {}
        }
    }
}

/// Whether the client, started saving into `running_with`, is started again for the downloads folder
/// Settings names now (`want`, unchanged for `steady_for`). Only for a folder that is there (not a name
/// still being typed), once it has stayed put for three seconds, and never while something is arriving or
/// a search is running (`busy`): those would be cut off. A folder taken out of Settings leaves it as it is.
fn restart_for_folder(running_with: Option<&Path>, want: Option<&Path>, steady_for: Duration, busy: bool, exists: impl Fn(&Path) -> bool) -> bool {
    match want {
        Some(w) => running_with != Some(w) && steady_for >= Duration::from_secs(3) && !busy && exists(w),
        None => false,
    }
}

/// Changes when slskd's settings file or the kept Soulseek login does.
fn settings_stamp() -> Option<(SystemTime, u64)> {
    std::fs::metadata(slskd::settings_file()).and_then(|m| m.modified()).ok().map(|t| (t, slskd::login_stamp()))
}

fn worker(cmds: Receiver<Cmd>, out: Sender<Snapshot>, child: Arc<Mutex<Option<std::process::Child>>>, ctx: egui::Context) {
    let mut settings: Option<Settings> = None;
    let mut stamp: Option<(SystemTime, u64)> = None;
    let mut client: Option<Client> = None;
    let mut searches = Running::default(); // the searches under way
    let mut seen_done: Option<HashSet<(String, String)>> = None; // folders already reported as arrived
    let mut note: Option<String> = None;
    let mut gather_tries: std::collections::HashMap<(String, String), u32> = Default::default();
    let mut started_at: Option<Instant> = None;
    // how many times in a row the client was started without ever answering (see MAX_STARTS)
    let mut starts: u32 = 0;
    let mut tidied = false;
    // the downloads folder the client was started with, the one Settings names and since when, and whether
    // anything was arriving at the last look (see `restart_for_folder`)
    let mut started_with: Option<PathBuf> = None;
    let mut wanted: (Option<PathBuf>, Instant) = (slskd::wanted_downloads(), Instant::now());
    let mut arriving = false;
    // the last thing sent to the page: the screen is only woken when something actually changed
    let mut last_sent: Option<Snapshot> = None;
    let stop_child = |child: &Arc<Mutex<Option<std::process::Child>>>| {
        if let Some(mut c) = child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    };
    let blank = |setup: Setup| Snapshot { setup, searches: vec![], groups: vec![], just_arrived: vec![], note: None };

    loop {
        // ---- the settings file: read it again whenever it changes (that is how a new login is noticed)
        let now_stamp = settings_stamp();
        if now_stamp != stamp {
            stamp = now_stamp;
            let fresh = slskd::read_settings();
            let changed = fresh.as_ref().ok() != settings.as_ref();
            if changed && settings.is_some() {
                stop_child(&child); // start again with the new settings
                client = None;
            }
            match fresh {
                Ok(s) => settings = Some(s),
                Err(e) => {
                    settings = None;
                    let _ = out.send(blank(Setup::BadSettings(e)));
                }
            }
        }
        if settings.is_none() && stamp.is_none() && slskd::exe().exists() {
            let _ = out.send(blank(Setup::BadSettings("The Soulseek client's settings file is missing.".into())));
        }

        // ---- the downloads folder in Settings changed: the client the app started is started again with it
        let want = slskd::wanted_downloads();
        if want != wanted.0 {
            wanted = (want, Instant::now());
        }
        let ours = child.lock().unwrap_or_else(|e| e.into_inner()).as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if ours && restart_for_folder(started_with.as_deref(), wanted.0.as_deref(), wanted.1.elapsed(), arriving || searches.any(), |p| p.is_dir()) {
            crate::config::log("soulseek: the downloads folder in Settings changed; the client is started again with it");
            stop_child(&child);
            client = None;
            (starts, started_at) = (0, None);
        }

        let mut setup = Setup::Starting;
        let mut groups = vec![];
        let mut just_arrived = vec![];
        if !slskd::exe().exists() {
            setup = Setup::NotInstalled;
        } else if let Some(set) = &settings {
            let c = client.get_or_insert_with(|| Client::new(set));
            match c.server() {
                Err(_) => {
                    // not answering: start it (once), then give it time to come up
                    let running = child.lock().unwrap_or_else(|e| e.into_inner()).as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
                    // one that stops each time it is started is started again after 10, 20, then 40 seconds,
                    // and then left alone until "Try again" (each start may sign in to Soulseek)
                    let due = started_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(10 << starts.min(3)));
                    if !running && starts >= MAX_STARTS {
                        setup = Setup::Offline("The Soulseek client stopped each time it was started.".into());
                    } else if !running && due {
                        match slskd::spawn(wanted.0.as_deref()) {
                            Ok(c) => {
                                *child.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
                                started_with = wanted.0.clone();
                                started_at = Some(Instant::now());
                                starts += 1;
                            }
                            Err(e) => setup = Setup::Offline(e),
                        }
                    } else if running && started_at.is_some_and(|t| t.elapsed() > Duration::from_secs(45)) {
                        setup = Setup::Offline("The Soulseek client started but is not answering.".into());
                    }
                }
                Ok(state) => {
                    starts = 0; // (it answers: a later stop starts the count afresh)
                    if !tidied {
                        tidied = true;
                        c.forget_old_searches();
                    }
                    setup = if !set.has_login { Setup::NoLogin } else { server_words(&state) };
                    if let Ok(t) = c.downloads() {
                        groups = group(&t);
                        // the first look only records what had already arrived before the app opened
                        if let Some(seen) = &mut seen_done {
                            just_arrived = newly_arrived(&groups, seen);
                            // an album whose discs arrived as "CD1", "CD2" folders: gather them under its name
                            // first. slskd can report a file done a moment before it is in place, so a
                            // gather that cannot be done yet is tried again on the next few looks.
                            just_arrived.retain(|name| {
                                let Some(g) = groups.iter().find(|g| g.arrived() && &g.folder == name && !g.parts.is_empty()) else { return true };
                                // (where the client saves: the folder it was started with, else its settings file's)
                                let gathered = started_with.clone().filter(|_| ours).or_else(slskd::downloads_dir).ok_or_else(|| "The downloads folder is not in the Soulseek client's settings.".to_string()).and_then(|dl| gather_discs(&dl, &g.folder, &g.parts));
                                match gathered {
                                    Ok(()) => true,
                                    Err(why) => {
                                        let tries = gather_tries.entry(g.key()).or_insert(0u32);
                                        *tries += 1;
                                        if *tries < 8 {
                                            return false;
                                        }
                                        note = Some(why);
                                        true
                                    }
                                }
                            });
                            for g in groups.iter().filter(|g| g.arrived() && just_arrived.contains(&g.folder)) {
                                seen.insert(g.key());
                            }
                        } else {
                            seen_done = Some(groups.iter().filter(|g| g.arrived()).map(|g| g.key()).collect());
                        }
                    }
                    searches.poll(&*c);
                }
            }
        }
        arriving = groups.iter().any(|g| !g.finished());
        if settings.is_some() || setup == Setup::NotInstalled {
            let snap = Snapshot { setup: setup.clone(), searches: searches.snaps(), groups, just_arrived, note: note.clone() };
            if last_sent.as_ref() != Some(&snap) {
                let _ = out.send(snap.clone());
                ctx.request_repaint();
                last_sent = Some(Snapshot { just_arrived: vec![], ..snap });
            }
            // the page now has the end of every search that ended: the client can forget those
            if let Some(c) = &client {
                searches.reap(c);
            }
        }

        // ---- wait for the next thing to do (or a moment, whichever comes first)
        let wait = if searches.any() { Duration::from_millis(700) } else { Duration::from_millis(1200) };
        match cmds.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                stop_child(&child);
                return;
            }
            Ok(cmd) => {
                note = None;
                // "Try again" also lets a client that kept stopping be started again
                if matches!(cmd, Cmd::Reconnect) {
                    (starts, started_at) = (0, None);
                }
                let Some(c) = &client else { continue };
                match cmd {
                    Cmd::Search(text, run) => searches.start(c, text, run),
                    Cmd::CloseSearch(text) => searches.stop(c, &text),
                    Cmd::Fetch(cand) => {
                        if let Err(e) = c.enqueue(&cand) {
                            note = Some(e);
                        }
                    }
                    Cmd::Remove(ids) => {
                        for (user, id) in ids {
                            c.cancel(&user, &id);
                        }
                    }
                    Cmd::Reconnect => {
                        if let Err(e) = c.connect() {
                            note = Some(e);
                        }
                    }
                }
            }
        }
    }
}

fn server_words(s: &ServerState) -> Setup {
    if s.logged_in {
        Setup::Ready
    } else if s.connected || s.state.contains("Connecting") || s.state.contains("LoggingIn") {
        Setup::Offline("Signing in to Soulseek".into())
    } else {
        Setup::Offline("Not signed in to Soulseek. The name or password may be wrong, or the account is in use somewhere else.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_follows_the_downloads_folder_in_settings_once_it_has_settled() {
        let (old, new) = (Path::new(r"C:\Old"), Path::new(r"D:\New"));
        let there = |_: &Path| true;
        let s = Duration::from_secs;
        // the same folder: nothing to do
        assert!(!restart_for_folder(Some(new), Some(new), s(60), false, there));
        // another folder, there, steady for three seconds, nothing under way: started again
        assert!(restart_for_folder(Some(old), Some(new), s(3), false, there));
        // also when it was started before Settings named one
        assert!(restart_for_folder(None, Some(new), s(3), false, there));
        // not while the name may still be typed, not for a folder that is not there
        assert!(!restart_for_folder(Some(old), Some(new), s(1), false, there));
        assert!(!restart_for_folder(Some(old), Some(new), s(60), false, |_| false));
        // not while something is arriving or a search runs: it would be cut off
        assert!(!restart_for_folder(Some(old), Some(new), s(60), true, there));
        // a folder taken out of Settings leaves the client as it is
        assert!(!restart_for_folder(Some(old), None, s(60), false, there));
    }
    use serde_json::{json, Value};
    use std::cell::RefCell;

    /// A Soulseek client for tests: remembers what was started, and ends a search when the test says so.
    #[derive(Default)]
    struct Scripted {
        started: RefCell<Vec<(String, String, slskd::Look)>>,
        ended: RefCell<Vec<(String, Vec<Value>)>>,
        forgotten: RefCell<Vec<String>>,
        people: RefCell<u64>,
        /// searches that fail to start, by their place (0, 1, 2)
        refuse: Vec<usize>,
    }
    impl Scripted {
        fn end(&self, which: usize, answers: Value) {
            let id = self.started.borrow()[which].0.clone();
            self.ended.borrow_mut().push((id, answers.as_array().unwrap().clone()));
        }
    }
    impl Searches for Scripted {
        fn start(&self, id: &str, text: &str, look: slskd::Look) -> Result<(), String> {
            let n = self.started.borrow().len();
            self.started.borrow_mut().push((id.to_string(), text.to_string(), look));
            if self.refuse.contains(&n) { Err("refused".into()) } else { Ok(()) }
        }
        fn state(&self, id: &str) -> Result<(bool, u64), String> {
            Ok((self.ended.borrow().iter().any(|e| e.0 == id), *self.people.borrow()))
        }
        fn responses(&self, id: &str) -> Result<Vec<Value>, String> {
            self.ended.borrow().iter().find(|e| e.0 == id).map(|e| e.1.clone()).ok_or_else(|| "not ended".to_string())
        }
        fn forget(&self, id: &str) {
            self.forgotten.borrow_mut().push(id.to_string());
        }
    }

    /// One person's answer: a folder of this many tracks in this format.
    fn answer(user: &str, folder: &str, ext: &str, rate: u64, tracks: usize, free: bool, queue: u64) -> Value {
        let files: Vec<Value> = (1..=tracks).map(|n| json!({ "filename": format!("C:\\music\\{folder}\\{n:02} song.{ext}"), "size": 5_000_000, "bitRate": rate })).collect();
        json!({ "username": user, "hasFreeUploadSlot": free, "uploadSpeed": 1_000_000, "queueLength": queue, "files": files })
    }

    #[test]
    fn a_search_is_three_searches_and_the_list_fills_in_ranked_as_each_ends() {
        let s = Scripted::default();
        let mut search = Staged::start(&s, "quiet rooms".into());
        // one click: two short looks and the full search, for the same words
        let started: Vec<(String, slskd::Look)> = s.started.borrow().iter().map(|x| (x.1.clone(), x.2)).collect();
        assert_eq!(started, vec![("quiet rooms".to_string(), Some((20, 5))), ("quiet rooms".to_string(), Some((80, 5))), ("quiet rooms".to_string(), None)]);
        assert_eq!(slskd::search_request("id-1", "quiet rooms", Some((20, 5))), json!({ "id": "id-1", "searchText": "quiet rooms", "responseLimit": 20, "searchTimeout": 5 }));
        assert_eq!(slskd::search_request("id-1", "quiet rooms", None), json!({ "id": "id-1", "searchText": "quiet rooms" }));

        // nothing has ended: people are counted, no copies yet, still searching
        *s.people.borrow_mut() = 12;
        search.poll(&s);
        assert_eq!((search.snap.answers, search.snap.candidates.len(), search.snap.complete), (12, 0, false));
        assert_eq!(search_line(&search.snap), "Searching. 12 people have answered so far.");

        // the first look ends: its copies are listed, best first, and the search goes on
        let (slow_mp3, good_flac) = (answer("slow", "Quiet Rooms (2020)", "mp3", 128, 10, false, 40), answer("good", "Quiet Rooms [FLAC]", "flac", 0, 10, true, 0));
        s.end(0, json!([slow_mp3.clone(), good_flac.clone()]));
        *s.people.borrow_mut() = 163;
        search.poll(&s);
        let names = |snap: &SearchSnap| snap.candidates.iter().map(|c| c.user.clone()).collect::<Vec<_>>();
        assert_eq!(names(&search.snap), vec!["good", "slow"]);
        assert!(!search.snap.complete);
        assert_eq!(search_line(&search.snap), "Searching. 163 people have answered so far.", "the line stays while copies are already listed");

        // the second look ends with one more person, a better copy, and the same two again: the joined list
        // is exactly what ranking every answer at once gives, and nobody is listed twice
        let best = answer("best", "Quiet Rooms (2020) [FLAC]", "flac", 0, 12, true, 0);
        s.end(1, json!([good_flac.clone(), best.clone(), slow_mp3.clone()]));
        search.poll(&s);
        let all_at_once = slskd::candidates(&[slow_mp3.clone(), good_flac.clone(), best.clone()], "quiet rooms");
        assert_eq!(names(&search.snap), all_at_once.iter().map(|c| c.user.clone()).collect::<Vec<_>>());
        assert_eq!(names(&search.snap), vec!["best", "good", "slow"]);
        assert!(!search.snap.complete);

        // the full search ends: the search is over
        let late = answer("late", "Quiet Rooms", "mp3", 320, 10, false, 2);
        s.end(2, json!([slow_mp3, good_flac, best, late]));
        search.poll(&s);
        assert!(search.snap.complete && search.snap.candidates.len() == 4 && search.snap.error.is_none());
        assert_eq!(search_line(&search.snap), "4 found, best first");

        // closing stops and forgets all three
        let ids: Vec<String> = s.started.borrow().iter().map(|x| x.0.clone()).collect();
        search.close(&s);
        assert_eq!(*s.forgotten.borrow(), ids);
    }

    #[test]
    fn a_look_that_fails_costs_nothing_and_the_full_search_failing_is_said() {
        // the two looks cannot start: the full search alone still gives the list
        let s = Scripted { refuse: vec![0, 1], ..Default::default() };
        let mut search = Staged::start(&s, "x".into());
        assert!(search.snap.error.is_none() && search.parts.len() == 1);
        s.end(2, json!([answer("only", "X", "mp3", 320, 9, true, 0)]));
        search.poll(&s);
        assert!(search.snap.complete && search.snap.candidates.len() == 1);
        // the full search cannot start: that is an error on the page
        let s = Scripted { refuse: vec![2], ..Default::default() };
        let search = Staged::start(&s, "x".into());
        assert_eq!(search.snap.error.as_deref(), Some("refused"));
    }

    #[test]
    fn the_list_does_not_move_under_the_pointer() {
        let k = |user: &str| (user.to_string(), "Album".to_string());
        let keys = |users: &[&str]| users.iter().map(|u| k(u)).collect::<Vec<_>>();
        let mut order = vec![];
        // not pointing at the list: the order is simply best first
        assert_eq!(settle(&mut order, &keys(&["good", "slow"]), false), 0);
        assert_eq!(order, keys(&["good", "slow"]));
        // pointing at it when better copies arrive: the rows there keep their place, the new ones go below
        assert_eq!(settle(&mut order, &keys(&["best", "good", "fine", "slow"]), true), 2);
        assert_eq!(order, keys(&["good", "slow", "best", "fine"]));
        // still pointing, nothing new: nothing moves, nothing is counted twice
        assert_eq!(settle(&mut order, &keys(&["best", "good", "fine", "slow"]), true), 0);
        assert_eq!(order, keys(&["good", "slow", "best", "fine"]));
        // a copy that is gone leaves, the others stay where they are
        assert_eq!(settle(&mut order, &keys(&["best", "fine", "slow"]), true), 0);
        assert_eq!(order, keys(&["slow", "best", "fine"]));
        // the pointer leaves the list: best first again
        assert_eq!(settle(&mut order, &keys(&["best", "fine", "slow"]), false), 0);
        assert_eq!(order, keys(&["best", "fine", "slow"]));
    }

    #[test]
    fn only_a_click_on_download_asks_for_a_copy() {
        // the search itself has no way to ask for a download: what it needs from the client is to start a
        // search, ask how it is doing, read its answers and forget it (`Searches`), and nothing else
        let s = Scripted::default();
        let mut search = Staged::start(&s, "x".into());
        s.end(0, json!([answer("good", "X", "flac", 0, 10, true, 0)]));
        search.poll(&s);
        search.close(&s);
        // and in the page's code a download request is made in exactly one place, `fetch`, which is called
        // from two: the click on a row's "Download", and Enter on the row the arrow keys are on
        let code = include_str!("downloads.rs");
        let own = &code[..code.find("#[cfg(test)]").unwrap()];
        let drawn = include_str!("downloads/page.rs");
        let drawn = &drawn[..drawn.find("#[cfg(test)]").unwrap()];
        assert_eq!(own.matches("Cmd::Fetch(Box::new(").count() + drawn.matches("Cmd::Fetch(").count(), 1, "a download is asked for in one place only");
        let at = own.find("Cmd::Fetch(Box::new(").unwrap();
        assert!(own[..at].rfind("fn fetch(&mut self, c: &Candidate)").is_some_and(|f| at - f < 300), "and that place is `fetch`");
        // `fetch` is called once, where the list of copies has been drawn, for the copy the frame chose
        assert_eq!(own.matches(".fetch(").count(), 0);
        assert_eq!(drawn.matches(".fetch(").count(), 1, "`fetch` is called in one place");
        let call = drawn.find(".fetch(").unwrap();
        assert!(drawn[..call].rfind("if let Some(at) = get").is_some_and(|g| call - g < 200), "for the copy the frame chose");
        // and a frame chooses a copy in two places: Enter on the row the arrow keys are on, and the click
        // on a row's "Download"
        let chosen: Vec<usize> = drawn.match_indices("get = Some(").map(|(i, _)| i).collect();
        assert_eq!(chosen.len(), 2, "a copy is chosen in two places");
        assert!(drawn[..chosen[0]].rfind("(at, enter)").is_some_and(|e| chosen[0] - e < 400), "one is Enter on the chosen row");
        assert!(drawn[..chosen[1]].rfind("download_word(").is_some_and(|click| chosen[1] - click < 200), "the other is the click on \"Download\"");
        assert_eq!(drawn.matches("download_word(").count(), 2, "the word is drawn in one place (and defined in one)");
    }

    /// What the background thread would report for a run: these copies so far, ended or not.
    fn report(run: u64, text: &str, users: &[&str], complete: bool) -> SearchSnap {
        let answers: Vec<Value> = users.iter().map(|u| answer(u, "Album (2020)", "flac", 0, 10, true, 0)).collect();
        SearchSnap { text: text.into(), complete, answers: users.len() as u64, candidates: slskd::candidates(&answers, "album"), error: None, run, part: 0.0 }
    }

    #[test]
    fn several_searches_run_side_by_side_and_each_is_forgotten_only_when_it_has_ended() {
        let s = Scripted::default();
        let mut running = Running::default();
        running.start(&s, "first".into(), 1);
        running.start(&s, "second".into(), 2);
        assert_eq!(s.started.borrow().len(), 6, "two searches of three looks each");
        // the full search of the first ends: its end is reported, with its run number, then it leaves
        s.end(2, json!([answer("good", "First", "flac", 0, 10, true, 0)]));
        running.poll(&s);
        let snaps = running.snaps();
        assert_eq!(snaps.iter().map(|x| (x.text.as_str(), x.run, x.complete)).collect::<Vec<_>>(), vec![("first", 1, true), ("second", 2, false)]);
        running.reap(&s);
        let ids: Vec<String> = s.started.borrow().iter().map(|x| x.0.clone()).collect();
        assert_eq!(*s.forgotten.borrow(), ids[..3].to_vec(), "only the ended search's three are forgotten");
        assert_eq!(running.snaps().len(), 1);
        // searching the same words again stops the one under way first
        running.start(&s, "second".into(), 3);
        assert_eq!(s.forgotten.borrow().len(), 6);
        assert_eq!(running.snaps().iter().map(|x| x.run).collect::<Vec<_>>(), vec![3]);
        running.stop(&s, "second");
        assert!(!running.any());
    }

    #[test]
    fn an_album_asked_for_by_its_words_is_followed_from_search_to_arrival() {
        let mut h = History::load(None, 0);
        assert_eq!(trail_of(&h, &[], "Amber Relay Signal Garden"), None, "never searched");
        let (_, run) = h.ask("Amber Relay Signal Garden").unwrap();
        assert_eq!(trail_of(&h, &[], "amber relay signal garden"), Some(Trail::Searching));
        h.take(&[report(run, "Amber Relay Signal Garden", &["ann", "bo"], true)], 1000);
        assert_eq!(trail_of(&h, &[], "Amber Relay Signal Garden"), Some(Trail::Found(2)));
        let folder = h.list[0].candidates[0].name.clone();
        let g = |user: &str, folder: &str, done: usize| Group { user: user.into(), folder: folder.into(), files: 10, done, failed: 0, refused: 0, bytes: 100, done_bytes: done as u64 * 10, speed: 0.0, all_ids: vec![], live_ids: vec![], parts: vec![] };
        // a download of the same folder name from someone who is not among its copies is another album's
        assert_eq!(trail_of(&h, &[g("carl", &folder, 6)], "Amber Relay Signal Garden"), Some(Trail::Found(2)));
        assert_eq!(trail_of(&h, &[g("ann", &folder, 6)], "Amber Relay Signal Garden"), Some(Trail::Arriving(6, 10)));
        // two copies on their way: the one furthest along; one arrived whole wins over both
        assert_eq!(trail_of(&h, &[g("ann", &folder, 2), g("bo", &folder, 7)], "Amber Relay Signal Garden"), Some(Trail::Arriving(7, 10)));
        assert_eq!(trail_of(&h, &[g("ann", &folder, 2), g("bo", &folder, 10)], "Amber Relay Signal Garden"), Some(Trail::Arrived(folder.clone())));
        // one that ended with files missing is not arriving any more
        let mut failed = g("ann", &folder, 8);
        failed.failed = 2;
        assert_eq!(trail_of(&h, &[failed], "Amber Relay Signal Garden"), Some(Trail::Found(2)));
    }

    #[test]
    fn a_search_is_kept_and_can_be_looked_at_again_without_running_it() {
        let mut h = History::load(None, 0);
        let (words, run) = h.ask("  Quiet Rooms ").unwrap();
        assert_eq!((words.as_str(), h.shown().unwrap().run), ("Quiet Rooms", Run::Asked));
        // copies arrive, then it ends: it is kept with what it found and when
        assert!(!h.take(&[report(run, "Quiet Rooms", &["a"], false)], 1000));
        assert_eq!((h.list[0].run, h.list[0].candidates.len(), h.list[0].complete), (Run::Running, 1, false));
        assert!(h.take(&[report(run, "Quiet Rooms", &["a", "b"], true)], 1010), "an ended search is worth saving");
        assert_eq!((h.list[0].run, h.list[0].candidates.len(), h.list[0].complete, h.list[0].found_at), (Run::Done, 2, true, 1010));
        // the background thread has forgotten it: nothing changes here
        assert!(!h.take(&[], 2000));
        assert_eq!(h.list[0].candidates.len(), 2);
        assert_eq!(kept_line(&h.list[0], 1010 + 1500), "2 found 25 minutes ago, best first");

        // another search: both are kept, the new one in front and on show, the old one a click away
        let (_, second) = h.ask("low tide").unwrap();
        assert_eq!(h.list.iter().map(|k| k.text.as_str()).collect::<Vec<_>>(), vec!["low tide", "Quiet Rooms"]);
        assert_eq!(h.shown().unwrap().text, "low tide");
        h.open = Some("Quiet Rooms".into());
        assert_eq!(h.shown().unwrap().candidates.len(), 2);
        h.take(&[report(second, "low tide", &[], true)], 3000);
        assert_eq!(kept_line(&h.list[0], 3000), "Nobody online has this right now. Try fewer words.");
    }

    #[test]
    fn searching_the_same_words_again_keeps_the_old_copies_until_new_ones_arrive() {
        let mut h = History::load(None, 0);
        let (_, first) = h.ask("quiet rooms").unwrap();
        h.take(&[report(first, "quiet rooms", &["a", "b"], true)], 1000);
        // the same words in other letters: one entry, not two, and the words as they were kept
        let (words, again) = h.ask("QUIET ROOMS").unwrap();
        assert_eq!((h.list.len(), words.as_str()), (1, "quiet rooms"));
        // a late report of the run before is not taken for this one
        h.take(&[report(first, "quiet rooms", &["a", "b"], true)], 5000);
        assert_eq!(h.list[0].run, Run::Asked);
        // the new run has found nothing yet: the old copies stay listed, and the line says so
        h.take(&[report(again, "quiet rooms", &[], false)], 5000);
        assert_eq!((h.list[0].run, h.list[0].candidates.len(), h.list[0].fresh), (Run::Running, 2, false));
        assert_eq!(kept_line(&h.list[0], 5000), "Searching. This takes about fifteen seconds. Showing the 2 copies found an hour ago until new ones arrive.");
        // it finds one: the list is this run's from here on
        h.take(&[report(again, "quiet rooms", &["c"], false)], 5005);
        assert_eq!((h.list[0].candidates.len(), h.list[0].fresh, h.list[0].found_at), (1, true, 5005));
        // stopped before it ended: kept as it is, and said to be short
        assert!(h.take(&[], 5010));
        assert_eq!((h.list[0].run, h.list[0].complete), (Run::Done, false));
        assert_eq!(kept_line(&h.list[0], 5010), "1 found before the search was stopped, best first");
    }

    #[test]
    fn three_searches_at_once_ten_kept_and_one_can_be_forgotten() {
        let mut h = History::load(None, 0);
        let runs: Vec<u64> = ["a", "b", "c"].iter().map(|t| h.ask(t).unwrap().1).collect();
        assert!(h.ask("d").unwrap_err().starts_with("Three searches are running"));
        assert_eq!(h.list.len(), 3, "a search that could not be run is not listed");
        let b_again = h.ask("B").expect("running one of the three again is allowed").1;
        // they end; more are run one by one: the oldest leaves once there are more than ten
        let mut snaps: Vec<SearchSnap> = runs.iter().zip(["a", "b", "c"]).map(|(r, t)| report(*r, t, &["x"], true)).collect();
        snaps.push(report(b_again, "b", &["x"], true));
        h.take(&snaps, 100);
        assert_eq!(h.running(), 0);
        for n in 0..9 {
            let (words, run) = h.ask(&format!("more {n}")).unwrap();
            h.take(&[report(run, &words, &["x"], true)], 200 + n);
        }
        assert_eq!(h.list.len(), KEEP_SEARCHES);
        assert_eq!((h.list[0].text.as_str(), h.list[9].text.as_str()), ("more 8", "b"));
        assert!(h.find("a").is_none() && h.find("c").is_none());
        // forgetting the one on show puts its neighbor on show; a running one is named so it can be stopped
        h.open = Some("more 8".into());
        assert_eq!(h.forget("more 8"), None);
        assert_eq!((h.list.len(), h.shown().unwrap().text.as_str()), (9, "more 7"));
        h.ask("still going").unwrap();
        assert_eq!(h.forget("STILL GOING"), Some("still going".to_string()));
    }

    #[test]
    fn kept_searches_are_saved_and_read_back() {
        let file = std::env::temp_dir().join(format!("music-recs-searches-{}-{}.json", std::process::id(), slskd::new_id()));
        let mut h = History::load(Some(file.clone()), 0);
        let (_, run) = h.ask("quiet rooms").unwrap();
        h.take(&[report(run, "quiet rooms", &["a", "b"], true)], 1000);
        let (_, going) = h.ask("low tide").unwrap();
        h.take(&[report(going, "low tide", &["c"], false)], 1100);
        h.ask("found nothing yet").unwrap();
        h.save();
        // read back in a new run of the app: nothing is running, the copies and their age are there, the one
        // that was still running is kept as not ended, and the one that had found nothing is not kept
        let back = History::load(Some(file.clone()), 2000);
        assert_eq!(back.list.iter().map(|k| (k.text.as_str(), k.candidates.len(), k.complete, k.run, k.found_at)).collect::<Vec<_>>(), vec![("low tide", 1, false, Run::Done, 1100), ("quiet rooms", 2, true, Run::Done, 1000)]);
        assert!(back.list[1].candidates == h.list[2].candidates, "a saved copy can still be asked for: its files are kept");
        assert!(back.open.is_none(), "the app opens on the import list");
        // a month on they are gone; a damaged file is the same as none
        assert!(History::load(Some(file.clone()), 1100 + KEEP_SECS).list.is_empty());
        std::fs::write(&file, b"not json").unwrap();
        assert!(History::load(Some(file.clone()), 2000).list.is_empty());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn ages_and_old_lists_read_naturally() {
        assert_eq!(["just now", "5 minutes ago", "an hour ago", "3 hours ago", "yesterday", "4 days ago"].to_vec(), [30, 300, 4000, 3 * 3600 + 5, 30 * 3600, 4 * 86_400 + 9].map(age_words).to_vec());
        let mut h = History::load(None, 0);
        let (_, run) = h.ask("x").unwrap();
        h.take(&[report(run, "x", &["a"], true)], 1000);
        assert_eq!(kept_line(&h.list[0], 1000 + 7 * 3600), "1 found 7 hours ago, best first. Some of these people may be offline by now.");
        assert_eq!(short_words("a very long search for an album with a very long name"), "a very long search for an album...");
    }

    fn t(user: &str, folder: &str, state: &str, size: u64, done: u64) -> Transfer {
        Transfer { id: format!("{user}-{folder}-{done}"), user: user.into(), folder: folder.into(), size, done_bytes: done, speed: 10.0, state: state.into(), ..Default::default() }
    }

    #[test]
    fn the_discs_of_an_album_are_gathered_under_its_name() {
        let dl = std::env::temp_dir().join(format!("music-recs-gather-{}-{}", std::process::id(), slskd::new_id()));
        let put = |rel: &str| {
            let p = dl.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, rel).unwrap();
        };
        let part = |disc: &str, file: &str| (disc.to_string(), file.to_string(), 0u64);
        // one album in two disc folders, its cover already in the album's own folder, and somebody else's file in CD1
        for f in [r"CD1\01 a.flac", r"CD1\02 b.flac", r"CD2\01 c.flac", r"CD1\other album.flac", r"Reflektor (2013)\cover.jpg"] {
            put(f);
        }
        // arriving as one album: disc files are followed under the album's name
        let d = |disc: &str, file: &str, state: &str| Transfer { user: "u".into(), folder: "Reflektor (2013)".into(), disc: disc.into(), file: file.into(), size: if disc.is_empty() { 4 } else { format!("{disc}\\{file}").len() as u64 }, done_bytes: 4, state: state.into(), ..Default::default() };
        let g = group(&[d("CD1", "01 a.flac", "Completed, Succeeded"), d("CD1", "02 b.flac", "Completed, Succeeded"), d("CD2", "01 c.flac", "Completed, Succeeded"), d("", "cover.jpg", "Completed, Succeeded")]);
        assert_eq!((g.len(), g[0].files, g[0].parts.len(), g[0].arrived()), (1, 4, 3, true));

        // a file that is not there yet: nothing is touched
        let mut early = g[0].parts.clone();
        early.push(part("CD2", "02 d.flac"));
        assert!(gather_discs(&dl, "Reflektor (2013)", &early).is_err());
        assert!(dl.join(r"CD1\01 a.flac").exists() && !dl.join(r"Reflektor (2013)\CD1").exists());

        // names that are not one folder level are refused
        for (album, disc, file) in [("..", "CD1", "01 a.flac"), (r"x\y", "CD1", "01 a.flac"), ("Reflektor (2013)", "..", "01 a.flac"), ("Reflektor (2013)", "CD1", r"..\01 a.flac"), ("Reflektor (2013)", "Covers", "01 a.flac")] {
            assert!(gather_discs(&dl, album, &[part(disc, file)]).is_err(), "{album} / {disc} / {file}");
        }
        assert!(dl.join(r"CD1\01 a.flac").exists());
        // a file of that name but another size is another album's: nothing is touched
        let mut other = g[0].parts.clone();
        other[0].2 += 1;
        assert!(gather_discs(&dl, "Reflektor (2013)", &other).is_err());
        assert!(dl.join(r"CD1\01 a.flac").exists() && !dl.join(r"Reflektor (2013)\CD1").exists());

        assert_eq!(gather_discs(&dl, "Reflektor (2013)", &g[0].parts), Ok(()));
        for f in [r"Reflektor (2013)\CD1\01 a.flac", r"Reflektor (2013)\CD1\02 b.flac", r"Reflektor (2013)\CD2\01 c.flac", r"Reflektor (2013)\cover.jpg"] {
            assert!(dl.join(f).is_file(), "{f}");
        }
        // the emptied disc folder is gone; the one holding somebody else's file stays, with that file
        assert!(!dl.join("CD2").exists() && dl.join(r"CD1\other album.flac").is_file() && !dl.join(r"CD1\01 a.flac").exists());
        // a second time: the files are no longer where they arrived, and nothing is overwritten
        assert!(gather_discs(&dl, "Reflektor (2013)", &g[0].parts).is_err());
        let _ = std::fs::remove_dir_all(&dl);
    }

    #[test]
    fn transfers_group_by_folder_and_know_when_they_are_done() {
        let g = group(&[
            t("u", "A", "Completed, Succeeded", 10, 10),
            t("u", "A", "InProgress", 10, 5),
            t("u", "B", "Completed, Succeeded", 4, 4),
            t("u", "C", "Completed, Errored", 4, 0),
            t("u", "C", "Completed, Succeeded", 4, 4),
            t("u", "R", "Completed, Rejected", 4, 0),
        ]);
        assert_eq!(g.len(), 4);
        assert!(!g[0].finished() && g[0].done == 1 && g[0].done_bytes == 15 && g[0].live_ids.len() == 1 && g[0].all_ids.len() == 2);
        assert!(g[1].arrived());
        assert!(g[2].finished() && !g[2].arrived() && g[2].failed == 1 && g[2].done == 1 && g[2].refused == 0);
        assert!(g[3].finished() && g[3].refused == 1 && g[3].done == 0);
        assert!(arriving_words(&g[3]).0.starts_with("Refused"));
        assert!(arriving_words(&g[0]).0.contains("1 of 2 files"));
    }

    #[test]
    fn a_folder_is_reported_once_and_only_when_nothing_else_writes_to_it() {
        let mut seen = HashSet::new();
        // the same album from two people lands in one local folder: wait for both
        let both = group(&[t("a", "Album", "Completed, Succeeded", 4, 4), t("b", "Album", "InProgress", 4, 1), t("a", "Other", "Completed, Succeeded", 4, 4)]);
        assert_eq!(newly_arrived(&both, &seen), vec!["Other".to_string()]);
        seen.insert(("a".to_string(), "Other".to_string()));
        let done = group(&[t("a", "Album", "Completed, Succeeded", 4, 4), t("b", "Album", "Completed, Succeeded", 4, 4), t("a", "Other", "Completed, Succeeded", 4, 4)]);
        assert_eq!(newly_arrived(&done, &seen), vec!["Album".to_string()]);
        // a failed copy never counts as arrived
        assert!(newly_arrived(&group(&[t("a", "X", "Completed, Errored", 4, 0)]), &seen).is_empty());
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_words(500), "1 KB");
        assert_eq!(size_words(5 * 1_048_576), "5 MB");
        assert_eq!(size_words(3 * 1_073_741_824 / 2), "1.5 GB");
    }
}
