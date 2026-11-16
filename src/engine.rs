use crate::config::Config;
use crate::library::Library;
use crate::net::{days, Net};
use crate::sources::*;
use crate::util::{album_key, artist_key, parse_date_earliest, parse_date_latest};
use chrono::{Datelike, Local, Months, NaiveDate};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ArtistStat {
    pub key: String,
    pub name: String,
    pub in_library: bool,
    pub albums: usize,
    pub tracks: usize,
    pub plays_all: u64,
    pub plays_12m: u64,
    pub plays_3m: u64,
    pub plays_recent: u64,
    pub weight: f64,
    pub mbid: String,
    pub url: String,
    /// loose title keys of the album folders you have (for "Owned" badges)
    pub album_keys: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReleaseRec {
    pub key: String,
    pub artist_key: String,
    pub artist: String,
    pub credit: String,
    pub title: String,
    pub date: String,
    pub kind: String,
    pub mb_url: String,
    pub apple_url: String,
    pub art_url: String,
    pub artist_weight: f64,
    /// track count when Apple knows it (0 = unknown)
    pub tracks: u32,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SimilarRec {
    pub key: String,
    pub name: String,
    pub score: f64,
    /// (your artist, contribution), biggest first
    pub because: Vec<(String, f64)>,
    pub url: String,
    pub plays: u64,
    pub tags: Vec<String>,
    pub listeners: u64,
    pub mbid: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Results {
    pub generated: String,
    /// unix seconds of the last finished run
    pub generated_ts: i64,
    pub artists: Vec<ArtistStat>,
    pub similar: Vec<SimilarRec>,
    pub releases: Vec<ReleaseRec>,
    /// release key -> date it first showed up
    pub seen: HashMap<String, String>,
    /// releases that were new in the latest run
    pub new_keys: Vec<String>,
    pub new_artist_keys: Vec<String>,
    /// how far down `artists` the release check has got (0 = unknown, saved by an older version)
    pub rel_cursor: usize,
    /// releases are loaded back to this date (YYYY-MM-DD; empty = just the normal window)
    pub back_to: String,
    /// how many top artists have been used as seeds for similar-artist suggestions
    pub seed_cursor: usize,
    /// release keys MusicBrainz flagged as live/compilation/reissue during this run
    #[serde(skip)]
    pub rejected: std::collections::HashSet<String>,
}

impl Results {
    /// Add a release found by any source, unless MusicBrainz already said it isn't new music.
    pub fn add_release(&mut self, r: ReleaseRec) {
        if r.mb_url.is_empty() && self.rejected.contains(&r.key) {
            return;
        }
        merge_release(&mut self.releases, r);
    }

    /// MusicBrainz flagged this as live/compilation/reissue: drop any copy only Apple vouched for.
    pub fn reject(&mut self, key: String) {
        self.releases.retain(|r| !(r.key == key && r.mb_url.is_empty()));
        self.rejected.insert(key);
    }

    /// One release listed twice becomes one. The sources name a soundtrack or a score differently ("X (Original
    /// Motion Picture Score)" on one, "X: Original Motion Picture Score" on the other) and can date it weeks
    /// apart, so the same-day rule in `merge_release` misses it. The earlier date stands; links and the cover
    /// the kept one lacks are taken from the other.
    pub fn merge_twins(&mut self) {
        let day = |d: &str| chrono::NaiveDate::parse_from_str(d.get(..10).unwrap_or(""), "%Y-%m-%d").ok();
        let mut order: Vec<usize> = (0..self.releases.len()).collect();
        order.sort_by(|a, b| self.releases[*a].date.cmp(&self.releases[*b].date));
        let mut kept: Vec<usize> = vec![];
        let mut gone: Vec<usize> = vec![];
        for i in order {
            let key = twin_key(&self.releases[i]);
            let twin = kept.iter().copied().find(|k| {
                twin_key(&self.releases[*k]) == key
                    && match (day(&self.releases[*k].date), day(&self.releases[i].date)) {
                        (Some(a), Some(b)) => (b - a).num_days().abs() <= 120,
                        _ => false,
                    }
            });
            match twin {
                Some(k) => {
                    let other = self.releases[i].clone();
                    let keep = &mut self.releases[k];
                    for (mine, theirs) in [(&mut keep.apple_url, other.apple_url), (&mut keep.mb_url, other.mb_url), (&mut keep.art_url, other.art_url)] {
                        if mine.is_empty() {
                            *mine = theirs;
                        }
                    }
                    if keep.tracks == 0 {
                        keep.tracks = other.tracks;
                    }
                    gone.push(i);
                }
                None => kept.push(i),
            }
        }
        gone.sort();
        for i in gone.into_iter().rev() {
            self.releases.remove(i);
        }
    }

    /// Call when a run finishes: remembers what's been seen and marks what's new since last time.
    pub fn mark_new(&mut self) {
        self.merge_twins();
        let first_run = self.seen.is_empty();
        let today = Local::now().format("%Y-%m-%d").to_string();
        self.new_keys.clear();
        for r in &self.releases {
            if !self.seen.contains_key(&r.key) {
                self.seen.insert(r.key.clone(), today.clone());
                if !first_run {
                    self.new_keys.push(r.key.clone());
                }
            }
        }
        self.new_artist_keys.clear();
        for s in &self.similar {
            let k = format!("artist|{}", s.key);
            if !self.seen.contains_key(&k) {
                self.seen.insert(k.clone(), today.clone());
                if !first_run {
                    self.new_artist_keys.push(k);
                }
            }
        }
        // forget things not seen for 2 years so the file doesn't grow forever
        let old = (Local::now() - chrono::Duration::days(730)).format("%Y-%m-%d").to_string();
        let current: std::collections::HashSet<String> =
            self.releases.iter().map(|r| r.key.clone()).chain(self.similar.iter().map(|s| format!("artist|{}", s.key))).collect();
        self.seen.retain(|k, d| *d >= old || current.contains(k));
        self.generated = Local::now().format("%b %-d, %Y %H:%M").to_string();
        self.generated_ts = Local::now().timestamp();
    }
}

#[derive(Clone, Default)]
pub struct DetailAlbum {
    pub title: String,
    pub year: String,
    pub kind: String,
    pub plays: u64,
    pub art: String,
    pub url: String,
    pub owned: bool,
    /// first release date as MusicBrainz has it ("2024-06-07", "2024" or empty)
    pub date: String,
    /// plays adjusted for how long the album has been out (see `album_score`)
    pub score: f64,
}

/// Plays per sqrt(years out). Older albums have had more time to collect plays, so raw totals
/// bury anything recent; dividing by the square root of age evens that out without letting a
/// two-week-old album beat an established classic on hype alone.
fn album_score(plays: u64, date: &str, today: NaiveDate) -> f64 {
    let years = parse_date_latest(date)
        .map(|d| ((today - d).num_days() as f64 / 365.25).max(0.25))
        .unwrap_or(5.0);
    plays as f64 / years.sqrt()
}

#[derive(Clone, Default)]
pub struct ArtistDetail {
    pub name: String,
    pub url: String,
    pub listeners: u64,
    pub tags: Vec<String>,
    pub bio: String,
    /// ranked best first (most played on Last.fm)
    pub albums: Vec<DetailAlbum>,
    /// albums MusicBrainz knows about that nobody on Last.fm plays much, oldest first
    pub other: Vec<DetailAlbum>,
    pub note: String,
    /// a lookup failed (e.g. offline): don't keep this in the in-memory cache
    pub failed: bool,
    /// the MusicBrainz artist the albums came from (empty if none)
    pub mbid: String,
}

pub enum Msg {
    Status(String),
    Progress { task: &'static str, done: usize, total: usize },
    Artists(Vec<ArtistStat>),
    Similar(Vec<SimilarRec>),
    /// more suggestions to add to the end of the list
    SimilarMore(Vec<SimilarRec>),
    RelCursor(usize),
    /// releases have now been loaded back to this date (YYYY-MM-DD)
    BackTo(String),
    SeedCursor(usize),
    /// the scanned library, kept so "load more" doesn't rescan the music folder
    Lib(Arc<Library>),
    Release(ReleaseRec),
    Warn(String),
    Error(String),
    TaskDone(&'static str),
    /// MusicBrainz says this release key is live/compilation/reissue: drop copies from other sources
    Reject(String),
    /// a source failed part-way: keep the previous results for anything missing
    Incomplete,
    Finished,
}

/// What two listings of one release share: the artist, single or not, the kind of recording, and the title
/// without edition notes and without the words a soundtrack or score is labelled with.
fn twin_key(r: &ReleaseRec) -> String {
    const LABEL: [&str; 14] = ["original", "official", "motion", "picture", "soundtrack", "score", "ost", "music", "from", "the", "film", "series", "game", "television"];
    let id = crate::albumkind::classify(&r.title);
    let title: String = id.base.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty() && !LABEL.contains(w)).collect();
    let title = if title.is_empty() { crate::util::norm(&r.title) } else { title };
    format!("{}|{title}|{:?}|{}", r.artist_key, id.kind, r.kind == "Single")
}

pub fn results_path() -> std::path::PathBuf {
    crate::config::app_dir().join("results.json")
}

pub fn merge_release(list: &mut Vec<ReleaseRec>, r: ReleaseRec) {
    let title_k = |k: &str| k.split_once('|').map(|x| x.1.to_string()).unwrap_or_default();
    // "STANS (The Official Soundtrack)" from Apple and "STANS: The Official Soundtrack" from MusicBrainz:
    // same artist, same day, and one title is the start of the other
    let prefix_twin = |e: &ReleaseRec| {
        let (a, b) = (title_k(&e.key), title_k(&r.key));
        let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
        e.artist_key == r.artist_key && e.date.len() == 10 && e.date == r.date && short.len() >= 4 && long.starts_with(short.as_str())
    };
    // A single and an album can share a title ("Dying in Reverse" the single in September, the album next February).
    // Apple and MusicBrainz describe the same release a few days apart at most, so a single and a bigger release
    // with dates months apart are two different releases: keep them separate instead of merging the album into the
    // single's (earlier) date.
    let different_release = |e: &ReleaseRec| {
        let single = |k: &str| k == "Single";
        if single(&e.kind) == single(&r.kind) {
            return false;
        }
        match (parse_date_earliest(&e.date), parse_date_earliest(&r.date)) {
            (Some(a), Some(b)) if e.date.len() >= 10 && r.date.len() >= 10 => (a - b).num_days().abs() > 45,
            _ => false,
        }
    };
    let same = |e: &ReleaseRec| {
        !different_release(e)
            && (e.key == r.key
            || prefix_twin(e)
            // the same collab release found under two of your artists
            || (title_k(&e.key) == title_k(&r.key) && e.date.get(..7) == r.date.get(..7) && !title_k(&r.key).is_empty()))
    };
    if let Some(e) = list.iter_mut().find(|e| same(e)) {
        e.artist_weight = e.artist_weight.max(r.artist_weight);
        if e.mb_url.is_empty() {
            e.mb_url = r.mb_url;
        }
        if e.apple_url.is_empty() && !r.apple_url.is_empty() {
            e.apple_url = r.apple_url;
            e.art_url = r.art_url; // Apple art is the most reliable
        } else if e.art_url.is_empty() {
            e.art_url = r.art_url;
        }
        // prefer the more precise date, else the earlier one
        if r.date.len() > e.date.len() || (r.date.len() == e.date.len() && r.date < e.date) {
            e.date = r.date;
        }
        // "Single" from Apple vs "EP"/"Album" from MusicBrainz: keep the bigger one
        let rank = |k: &str| match k {
            "Single" => 0,
            "EP" => 1,
            _ => 2,
        };
        if rank(&r.kind) > rank(&e.kind) {
            e.kind = r.kind;
        }
    } else {
        list.push(r);
    }
}

fn weight(a: &ArtistStat) -> f64 {
    let l = |x: f64| (1.0 + x).ln();
    1.0 * l(a.tracks as f64)
        + 0.6 * l(a.plays_all as f64)
        + 0.4 * l(a.plays_12m as f64)
        + 0.4 * l(a.plays_3m as f64)
        + 0.3 * l(a.plays_recent as f64)
}

pub fn run(cfg: Config, net: Arc<Net>, tx: Sender<Msg>, cancel: Arc<AtomicBool>, repaint: impl Fn() + Send + Sync + Clone + 'static) {
    let send = {
        let tx = tx.clone();
        let repaint = repaint.clone();
        move |m: Msg| {
            // (the refresh has ended: said on the taskbar when the window is not in front)
            if matches!(m, Msg::Finished) {
                crate::links::job_ended("recs refresh");
            }
            let _ = tx.send(m);
            repaint();
        }
    };

    // 1. library
    send(Msg::Status(format!("Scanning {}…", cfg.music_dir)));
    let lib = match Library::scan_or_snapshot(&cfg.music_dir, &cfg.skip_folders, &cfg.alias_file) {
        Ok((l, note)) => {
            if let Some(n) = note {
                send(Msg::Error(n));
            }
            Arc::new(l)
        }
        Err(e) => {
            send(Msg::Error(e));
            send(Msg::Finished);
            return;
        }
    };
    send(Msg::Lib(lib.clone()));

    let mut stats: HashMap<String, ArtistStat> = HashMap::new();
    for (k, a) in &lib.artists {
        stats.insert(
            k.clone(),
            ArtistStat { key: k.clone(), name: a.display().to_string(), in_library: true, albums: a.albums.len(), tracks: a.tracks, album_keys: a.album_keys.iter().cloned().collect(), ..Default::default() },
        );
    }

    // 2. Last.fm listening
    if cfg.has_lastfm() {
        send(Msg::Status("Reading your Last.fm history…".into()));
        let (u, k) = (cfg.lastfm_user.as_str(), cfg.lastfm_api_key.as_str());
        let periods: [(&str, u32); 3] = [("overall", 1000), ("12month", 500), ("3month", 300)];
        for (period, limit) in periods {
            match lfm_top_artists(&net, u, k, period, limit) {
                Ok(list) => {
                    for a in list {
                        let key = lib.key_for(&a.name);
                        let st = stats.entry(key.clone()).or_insert_with(|| ArtistStat { key, name: a.name.clone(), ..Default::default() });
                        match period {
                            "overall" => st.plays_all = a.plays,
                            "12month" => st.plays_12m = a.plays,
                            _ => st.plays_3m = a.plays,
                        }
                        if st.mbid.is_empty() {
                            st.mbid = a.mbid;
                        }
                        if st.url.is_empty() {
                            st.url = a.url;
                        }
                    }
                }
                Err(e) => {
                    send(Msg::Error(crate::util::plain_err(format!("Couldn't read your Last.fm history ({e}), so rankings use your library only this time."))));
                    send(Msg::Incomplete);
                    break;
                }
            }
        }
        match lfm_recent(&net, u, k, 5) {
            Ok(list) => {
                for (name, mbid) in list {
                    let key = lib.key_for(&name);
                    let st = stats.entry(key.clone()).or_insert_with(|| ArtistStat { key, name: name.clone(), ..Default::default() });
                    st.plays_recent += 1;
                    if st.mbid.is_empty() {
                        st.mbid = mbid;
                    }
                }
            }
            Err(e) => send(Msg::Warn(crate::util::plain_err(format!("Couldn't read recent tracks: {e}")))),
        }
    } else {
        send(Msg::Warn("No Last.fm account set, so recommendations use your library only. Add it in Settings.".into()));
    }

    let mut artists: Vec<ArtistStat> = stats.into_values().collect();
    for a in &mut artists {
        if let Some(m) = cfg.pinned_mbid(&a.key) {
            a.mbid = m.to_string();
        }
    }
    for a in &mut artists {
        a.weight = weight(a);
    }
    artists.sort_by(|a, b| b.weight.total_cmp(&a.weight));
    send(Msg::Artists(artists.clone()));
    let artists = Arc::new(artists);

    let (_today, cutoff) = cutoff_date(&cfg);
    // Ask MusicBrainz about three years at once, for the same number of requests as one year: scrolling back
    // for older releases is then answered from the cache instead of waiting on MusicBrainz again.
    let win = Window { fetch_from: first_of_month(cutoff.checked_sub_months(Months::new(36)).unwrap_or(cutoff)), lo: cutoff, hi: None };
    send(Msg::BackTo(cutoff.format("%Y-%m-%d").to_string()));

    let mut handles = vec![];

    // 3. similar artists
    if cfg.has_lastfm() {
        let (cfg, net, artists, lib, send, cancel) = (cfg.clone(), net.clone(), artists.clone(), lib.clone(), send.clone(), cancel.clone());
        handles.push(std::thread::spawn(move || similar_task(&cfg, &net, &artists, &lib, &send, &cancel)));
    }

    // 4. new releases
    // muted artists don't use up lookups
    let (picked, rel_cursor) = pick_artists(&artists, 0, cfg.check_artists, &cfg);
    send(Msg::RelCursor(rel_cursor));
    send(Msg::SeedCursor(cfg.similar_seeds.min(artists.len())));
    let to_check = Arc::new(picked);
    {
        let (cfg, net, list, lib, send, cancel) = (cfg.clone(), net.clone(), to_check.clone(), lib.clone(), send.clone(), cancel.clone());
        handles.push(std::thread::spawn(move || mb_task(&cfg, &net, &list, &lib, win, &send, &cancel)));
    }
    if cfg.use_itunes {
        let (cfg, net, list, lib, send, cancel) = (cfg.clone(), net.clone(), to_check.clone(), lib.clone(), send.clone(), cancel.clone());
        handles.push(std::thread::spawn(move || itunes_task(&cfg, &net, &list, &lib, win, &send, &cancel)));
    }
    send(Msg::Status("Looking for new releases and similar artists…".into()));
    for h in handles {
        let _ = h.join();
    }
    send(Msg::Status(if cancel.load(Ordering::Relaxed) { "Stopped.".into() } else { "Done.".into() }));
    send(Msg::Finished);
}

fn similar_task(cfg: &Config, net: &Net, artists: &[ArtistStat], lib: &Library, send: &(impl Fn(Msg) + ?Sized), cancel: &AtomicBool) {
    let seeds: Vec<&ArtistStat> = artists.iter().take(cfg.similar_seeds).collect();
    let Some(cands) = gather_similar(cfg, net, artists, &seeds, lib, send, cancel) else { return };
    let mut out = rank_similar(cands, lib);
    out.truncate(150);
    annotate_similar(cfg, net, &mut out, send, cancel);
    send(Msg::Similar(out));
    send(Msg::TaskDone("Similar artists (Last.fm)"));
}

/// Asks Last.fm for artists similar to each seed. `None` = Last.fm is down, so keep the previous suggestions.
fn gather_similar(
    cfg: &Config,
    net: &Net,
    artists: &[ArtistStat],
    seeds: &[&ArtistStat],
    lib: &Library,
    send: &(impl Fn(Msg) + ?Sized),
    cancel: &AtomicBool,
) -> Option<HashMap<String, SimilarRec>> {
    let plays: HashMap<&str, u64> = artists.iter().map(|a| (a.key.as_str(), a.plays_all)).collect();
    let mut cands: HashMap<String, SimilarRec> = HashMap::new();
    let mut fails = Failures::new("Last.fm");
    for (i, seed) in seeds.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        send(Msg::Progress { task: "Similar artists (Last.fm)", done: i, total: seeds.len() });
        let list = match lfm_similar(net, &cfg.lastfm_api_key, &seed.name, 50) {
            Ok(l) => {
                fails.ok();
                l
            }
            Err(e) if e.contains("could not be found") => continue, // an artist Last.fm doesn't know
            Err(e) => {
                if fails.fail(&e, send) {
                    return None;
                }
                continue;
            }
        };
        for s in list {
            let key = lib.key_for(&s.name);
            if key.is_empty() || lib.artists.contains_key(&key) || key == seed.key {
                continue;
            }
            let p = plays.get(key.as_str()).copied().unwrap_or(0);
            if cfg.hide_listened_plays > 0 && p >= cfg.hide_listened_plays {
                continue;
            }
            let contrib = seed.weight * s.score;
            let c = cands.entry(key.clone()).or_insert_with(|| SimilarRec { key, name: s.name.clone(), url: s.url.clone(), mbid: s.mbid.clone(), plays: p, ..Default::default() });
            c.score += contrib;
            c.because.push((seed.name.clone(), contrib));
        }
    }
    Some(cands)
}

/// Scores and sorts candidates, best first.
fn rank_similar(cands: HashMap<String, SimilarRec>, lib: &Library) -> Vec<SimilarRec> {
    let mut out: Vec<SimilarRec> = cands
        .into_values()
        .map(|mut c| {
            c.because.sort_by(|a, b| b.1.total_cmp(&a.1));
            // being recommended by several of your artists matters more than one strong match
            c.score *= 1.0 + 0.25 * (c.because.len() as f64 - 1.0).min(8.0);
            c
        })
        .collect();
    // "Freddie Gibbs & Madlib" is just a pairing of an artist already listed (or owned): drop it
    let keys: std::collections::HashSet<String> = out.iter().map(|c| c.key.clone()).chain(lib.artists.keys().cloned()).collect();
    out.retain(|c| {
        !keys.iter().any(|k| k.len() >= 4 && c.key.len() > k.len() && c.key.starts_with(k.as_str()) && c.key[k.len()..].starts_with("and"))
    });
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// genre tags + audience size so each suggestion is easier to judge at a glance
fn annotate_similar(cfg: &Config, net: &Net, out: &mut [SimilarRec], send: &(impl Fn(Msg) + ?Sized), cancel: &AtomicBool) {
    let n = out.len();
    for (i, c) in out.iter_mut().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        send(Msg::Progress { task: "Similar artists (Last.fm)", done: i, total: n });
        if let Ok(info) = lfm_artist_info(net, &cfg.lastfm_api_key, &c.name) {
            c.tags = info.tags.into_iter().take(4).collect();
            c.listeners = info.listeners;
        }
    }
}

fn cutoff_date(cfg: &Config) -> (NaiveDate, NaiveDate) {
    let today = Local::now().date_naive();
    let cutoff = today.checked_sub_months(Months::new(cfg.months_back)).unwrap_or(today);
    (today, NaiveDate::from_ymd_opt(cutoff.year(), cutoff.month(), 1).unwrap_or(cutoff))
}

/// The next `count` artists (not muted) from position `from` in the ranking, and the position after the last one.
fn pick_artists(artists: &[ArtistStat], from: usize, count: usize, cfg: &Config) -> (Vec<ArtistStat>, usize) {
    let mut out = vec![];
    let mut next = from.min(artists.len());
    for (i, a) in artists.iter().enumerate().skip(from) {
        if out.len() >= count {
            break;
        }
        next = i + 1;
        if !cfg.muted.contains_key(&a.key) {
            out.push(a.clone());
        }
    }
    (out, next)
}

/// How many months further back one "load more" of releases goes.
const MORE_MONTHS: u32 = 6;
/// Don't go looking for releases older than this.
pub fn earliest() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()
}
const MORE_SEEDS: usize = 10;

fn first_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

/// The slice of time a release lookup is for. MusicBrainz is queried from `fetch_from` (so one cached answer
/// can serve several slices); only releases dated within `lo..hi` are kept.
#[derive(Clone, Copy)]
struct Window {
    fetch_from: NaiveDate,
    lo: NaiveDate,
    hi: Option<NaiveDate>,
}

impl Window {
    fn contains(&self, d: NaiveDate) -> bool {
        d >= self.lo && self.hi.map(|h| d < h).unwrap_or(true)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum More {
    /// check the next artists down your ranking for new releases
    Releases,
    /// use the next artists down your ranking as seeds for more suggestions
    Similar,
}

/// Infinite scroll: find a bit more and add it to what's already there. `from` is the cursor into `artists`.
#[allow(clippy::too_many_arguments)]
pub fn load_more(
    cfg: Config,
    net: Arc<Net>,
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
    repaint: impl Fn() + Send + Sync + Clone + 'static,
    what: More,
    artists: Vec<ArtistStat>,
    from: usize,
    back_to: Option<NaiveDate>,
    existing: Vec<String>,
    lib: Option<Arc<Library>>,
) {
    let send = {
        let tx = tx.clone();
        let repaint = repaint.clone();
        move |m: Msg| {
            // (loading more has ended: said on the taskbar when the window is not in front, as a refresh is)
            if matches!(m, Msg::Finished) {
                crate::links::job_ended("recs load more");
            }
            let _ = tx.send(m);
            repaint();
        }
    };
    let lib = match lib {
        Some(l) => l,
        None => {
            send(Msg::Status(format!("Scanning {}…", cfg.music_dir)));
            match Library::scan_or_snapshot(&cfg.music_dir, &cfg.skip_folders, &cfg.alias_file) {
                Ok((l, _)) => {
                    let l = Arc::new(l);
                    send(Msg::Lib(l.clone()));
                    l
                }
                Err(e) => {
                    send(Msg::Error(e));
                    send(Msg::Finished);
                    return;
                }
            }
        }
    };
    match what {
        More::Releases => {
            // The same artists as the main refresh, one slice of time further back. MusicBrainz is asked about
            // a 3-year block at a time, so the next few steps back are answered from the cache.
            let (list, _) = pick_artists(&artists, 0, cfg.check_artists, &cfg);
            let (_, base) = cutoff_date(&cfg);
            let hi = back_to.unwrap_or(base);
            let lo = first_of_month(hi.checked_sub_months(Months::new(MORE_MONTHS)).unwrap_or(hi));
            let mut fetch_from = base;
            while fetch_from > lo {
                fetch_from = first_of_month(fetch_from.checked_sub_months(Months::new(36)).unwrap_or(lo));
            }
            let win = Window { fetch_from, lo, hi: Some(hi) };
            send(Msg::Status(format!("Looking for releases from {}…", lo.format("%B %Y"))));
            let list = Arc::new(list);
            let mut handles = vec![];
            {
                let (cfg, net, list, lib, send, cancel) = (cfg.clone(), net.clone(), list.clone(), lib.clone(), send.clone(), cancel.clone());
                handles.push(std::thread::spawn(move || mb_task(&cfg, &net, &list, &lib, win, &send, &cancel)));
            }
            if cfg.use_itunes {
                let (cfg, net, list, lib, send, cancel) = (cfg.clone(), net.clone(), list.clone(), lib.clone(), send.clone(), cancel.clone());
                handles.push(std::thread::spawn(move || itunes_task(&cfg, &net, &list, &lib, win, &send, &cancel)));
            }
            for h in handles {
                let _ = h.join();
            }
            if !cancel.load(Ordering::Relaxed) {
                send(Msg::BackTo(lo.format("%Y-%m-%d").to_string()));
            }
        }
        More::Similar => {
            send(Msg::Status("Asking Last.fm for more similar artists…".into()));
            let end = (from + MORE_SEEDS).min(artists.len());
            let seeds: Vec<&ArtistStat> = artists[from.min(end)..end].iter().collect();
            if let Some(cands) = gather_similar(&cfg, &net, &artists, &seeds, &lib, &send, &cancel) {
                let have: std::collections::HashSet<&str> = existing.iter().map(|s| s.as_str()).collect();
                let mut out = rank_similar(cands, &lib);
                out.retain(|c| !have.contains(c.key.as_str()));
                out.truncate(60);
                annotate_similar(&cfg, &net, &mut out, &send, &cancel);
                if !cancel.load(Ordering::Relaxed) {
                    send(Msg::SimilarMore(out));
                    send(Msg::SeedCursor(end));
                }
                send(Msg::TaskDone("Similar artists (Last.fm)"));
            }
        }
    }
    send(Msg::Status(if cancel.load(Ordering::Relaxed) { "Stopped.".into() } else { "Done.".into() }));
    send(Msg::Finished);
}

static RE_REISSUE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?i)\(live\b|\blive (at|in|from|on)\b|[-:\x{2013}] live\b|\blive\)?$",
        r"|remaster|anniversary|\bbest of\b|greatest hits|\bremixes\b|\bdemos?\)?$|recorded for",
        r"|\bsessions?\)?$|\binstrumentals?\b|\ba cappella\b|\bsped up\b|\bslowed\b|\bcollection\b|\bessentials\b|\bkaraoke\b|\(acoustic",
    ))
        .unwrap()
});

/// Live albums, remasters, best-ofs etc. that Apple doesn't label and MusicBrainz sometimes misses.
pub fn looks_like_reissue(title: &str) -> bool {
    RE_REISSUE.is_match(title)
}

fn kind_allowed(cfg: &Config, kind: &str) -> bool {
    match kind {
        "Album" | "Mixtape" => cfg.include_albums,
        "EP" => cfg.include_eps,
        "Single" => cfg.include_singles,
        "Live" | "Compilation" | "Remix" => cfg.include_live_comp,
        _ => false,
    }
}

pub fn rel_key(artist_key: &str, title: &str) -> String {
    format!("{artist_key}|{}", album_key(title))
}

fn resolve_mbid(cfg: &Config, net: &Net, lib: &Library, a: &ArtistStat) -> Option<String> {
    // your pick from the artist panel always wins ("" = you said none of the MusicBrainz artists is right)
    if let Some(m) = cfg.pinned_mbid(&a.key) {
        return if m.is_empty() { None } else { Some(m.to_string()) };
    }
    if !a.mbid.is_empty() {
        return Some(a.mbid.clone());
    }
    let found = mb_search_artist(net, &a.name).ok()?;
    let exact: Vec<MbArtist> = found
        .into_iter()
        .filter(|m| m.score >= 70.0 && (lib.key_for(&m.name) == a.key || artist_key(&m.name) == artist_key(&a.name)))
        .collect();
    if exact.len() <= 1 || !a.in_library {
        return exact.into_iter().next().map(|m| m.id);
    }
    // Several artists share this name: pick the one whose discography overlaps your folders.
    let mut best: Option<(usize, String)> = None;
    for m in exact.iter().take(3) {
        let hits = mb_some_release_groups(net, &m.id)
            .map(|rgs| rgs.iter().filter(|r| lib.owns(&a.key, &r.title, false)).count())
            .unwrap_or(0);
        if best.as_ref().map(|b| hits > b.0).unwrap_or(true) {
            best = Some((hits, m.id.clone()));
        }
    }
    best.map(|b| b.1)
}

/// Tells a real outage (no internet, service down, bad API key) apart from the odd failed lookup.
/// A source that fails several times in a row before any success is stopped with one clear message.
struct Failures {
    source: &'static str,
    streak: usize,
    any_ok: bool,
}

impl Failures {
    fn new(source: &'static str) -> Self {
        Self { source, streak: 0, any_ok: false }
    }
    fn ok(&mut self) {
        self.streak = 0;
        self.any_ok = true;
    }
    /// Returns true when the caller should give up on this source.
    fn fail(&mut self, err: &str, send: &(impl Fn(Msg) + ?Sized)) -> bool {
        self.streak += 1;
        let limit = if self.any_ok { 12 } else { 5 };
        if self.streak >= limit {
            send(Msg::Error(format!(
                "Stopped checking {}: {} lookups in a row failed (last error: {err}). Check your internet connection{}. Your previous results are kept.",
                self.source,
                self.streak,
                if self.source == "Last.fm" { " and API key" } else { "" }
            )));
            send(Msg::Incomplete);
            return true;
        }
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn mb_task(cfg: &Config, net: &Net, list: &[ArtistStat], lib: &Library, win: Window, send: &(impl Fn(Msg) + ?Sized), cancel: &AtomicBool) {
    let from = win.fetch_from.format("%Y-%m-%d").to_string();
    let mut fails = Failures::new("MusicBrainz");
    for (i, a) in list.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        send(Msg::Progress { task: "New releases (MusicBrainz)", done: i, total: list.len() });
        let Some(mbid) = resolve_mbid(cfg, net, lib, a) else { continue };
        let ttl = days(cfg.cache_days.max(1));
        let rgs = match mb_recent_release_groups(net, &mbid, &from, ttl) {
            Ok(mut r) => {
                fails.ok();
                // 100 is the most MusicBrainz returns, and they come back in no date order: a prolific artist
                // may have been cut off, so ask again for just the slice that's wanted.
                if r.len() >= 100 && win.fetch_from < win.lo {
                    let to = win.hi.map(|h| h.pred_opt().unwrap_or(h)).map(|h| h.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "2100-12-31".into());
                    if let Ok(n) = mb_release_groups_between(net, &mbid, &win.lo.format("%Y-%m-%d").to_string(), &to, ttl) {
                        r = n;
                    }
                }
                r
            }
            Err(e) => {
                if fails.fail(&e, send) {
                    return;
                }
                send(Msg::Warn(crate::util::plain_err(format!("MusicBrainz failed for {}: {e}", a.name))));
                continue;
            }
        };
        for rg in rgs {
            let sec = |t: &str| rg.secondary.iter().any(|s| s == t);
            if sec("Interview") || sec("Audiobook") || sec("Spokenword") || sec("Audio drama") || sec("Field recording") {
                continue;
            }
            let kind = if sec("Live") {
                "Live"
            } else if sec("Compilation") || sec("DJ-mix") {
                "Compilation"
            } else if sec("Remix") {
                "Remix"
            } else if sec("Mixtape/Street") && rg.primary == "Album" {
                "Mixtape"
            } else {
                match rg.primary.as_str() {
                    "Album" => "Album",
                    "EP" => "EP",
                    "Single" => "Single",
                    _ => "",
                }
            };
            let not_new_music = matches!(kind, "Live" | "Compilation" | "Remix") || looks_like_reissue(&rg.title);
            if !cfg.include_live_comp && not_new_music {
                // MusicBrainz knows this is live/compilation/reissue; Apple usually doesn't say, so tell the UI
                // to drop Apple's copy too (e.g. "DarkLIVE 202X").
                send(Msg::Reject(rel_key(&a.key, &rg.title)));
                continue;
            }
            if !kind_allowed(cfg, kind) || rg.date.is_empty() {
                continue;
            }
            // skip songs they're only featured on, unless asked for
            if !cfg.include_features && !rg.first_artist_id.is_empty() && rg.first_artist_id != mbid && lib.key_for(&rg.first_artist_name) != a.key {
                continue;
            }
            match parse_date_latest(&rg.date) {
                Some(d) if win.contains(d) => {}
                _ => continue,
            }
            if lib.owns(&a.key, &rg.title, kind == "Single") {
                continue;
            }
            send(Msg::Release(ReleaseRec {
                key: rel_key(&a.key, &rg.title),
                artist_key: a.key.clone(),
                artist: a.name.clone(),
                credit: rg.credit.clone(),
                title: rg.title.clone(),
                date: rg.date.clone(),
                kind: kind.into(),
                mb_url: format!("https://musicbrainz.org/release-group/{}", rg.id),
                apple_url: String::new(),
                art_url: format!("https://coverartarchive.org/release-group/{}/front-250", rg.id),
                artist_weight: a.weight,
                tracks: 0,
            }));
        }
    }
    send(Msg::TaskDone("New releases (MusicBrainz)"));
}

fn itunes_task(cfg: &Config, net: &Net, list: &[ArtistStat], lib: &Library, win: Window, send: &(impl Fn(Msg) + ?Sized), cancel: &AtomicBool) {
    let mut fails = Failures::new("Apple Music");
    for (i, a) in list.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        send(Msg::Progress { task: "New releases (Apple Music)", done: i, total: list.len() });
        let albums = match itunes_albums(net, &a.name, days(cfg.cache_days.max(1))) {
            Ok(r) => {
                fails.ok();
                r
            }
            Err(e) => {
                if fails.fail(&e, send) {
                    return;
                }
                send(Msg::Warn(crate::util::plain_err(format!("Apple lookup failed for {}: {e}", a.name))));
                continue;
            }
        };
        let name_ok = |artist: &str| {
            let ak = lib.key_for(artist);
            let collab = ak.len() > a.key.len() && a.key.len() >= 5 && ak.starts_with(&a.key) && ak[a.key.len()..].starts_with("and");
            ak == a.key || collab
        };
        let albums: Vec<ItAlbum> = albums.into_iter().filter(|al| name_ok(&al.artist)).collect();
        // Apple has many same-named artists. Keep the Apple artist(s) whose catalogue overlaps your folders.
        let mut by_id: HashMap<u64, (usize, usize)> = HashMap::new(); // id -> (owned, total)
        for al in albums.iter().filter(|al| lib.key_for(&al.artist) == a.key) {
            let e = by_id.entry(al.artist_id).or_default();
            e.1 += 1;
            if lib.owns(&a.key, al.title.trim_end_matches(" - Single").trim_end_matches(" - EP"), false) {
                e.0 += 1;
            }
        }
        let lib_albums = lib.artists.get(&a.key).map(|l| l.albums.len()).unwrap_or(0);
        let ok_ids: Vec<u64> = if by_id.values().any(|v| v.0 > 0) {
            by_id.iter().filter(|(_, v)| v.0 > 0).map(|(k, _)| *k).collect()
        } else if lib_albums >= 2 {
            vec![] // none of your albums are on Apple under this name: probably a different artist
        } else {
            by_id.iter().max_by_key(|(_, v)| v.1).map(|(k, _)| *k).into_iter().collect()
        };
        // earliest date Apple has for each title, to spot reissues ("Glitter (Deluxe Edition)" in 2026 of a 2001 album)
        let mut first_seen: HashMap<String, String> = HashMap::new();
        // (albums only: a title-track single from a year earlier doesn't make the album a reissue)
        for al in albums.iter().filter(|al| !al.title.ends_with(" - Single") && !al.title.ends_with(" - EP") && al.tracks > 3) {
            let k = album_key(&al.title);
            let e = first_seen.entry(k).or_insert_with(|| al.date.clone());
            if al.date < *e {
                *e = al.date.clone();
            }
        }
        for al in albums {
            // collabs are listed under a combined artist id, so check those by name only
            if lib.key_for(&al.artist) == a.key && !ok_ids.contains(&al.artist_id) {
                continue;
            }
            if !cfg.include_live_comp && looks_like_reissue(&al.title) {
                continue;
            }
            let k = album_key(al.title.trim_end_matches(" - Single").trim_end_matches(" - EP"));
            let reissue = first_seen
                .get(&k)
                .and_then(|first| Some((parse_date_earliest(first)?, parse_date_earliest(&al.date)?)))
                .map(|(first, this)| (this - first).num_days() > 365)
                .unwrap_or(false);
            if reissue && !cfg.include_live_comp {
                continue;
            }
            match parse_date_latest(&al.date) {
                Some(d) if win.contains(d) => {}
                _ => continue,
            }
            let (title, kind) = if let Some(t) = al.title.strip_suffix(" - Single") {
                (t.to_string(), "Single")
            } else if let Some(t) = al.title.strip_suffix(" - EP") {
                (t.to_string(), "EP")
            } else if al.tracks <= 3 {
                (al.title.clone(), "Single")
            } else {
                (al.title.clone(), "Album")
            };
            if !kind_allowed(cfg, kind) || lib.owns(&a.key, &title, kind == "Single") {
                continue;
            }
            send(Msg::Release(ReleaseRec {
                key: rel_key(&a.key, &title),
                artist_key: a.key.clone(),
                artist: a.name.clone(),
                credit: al.artist.clone(),
                tracks: al.tracks,
                title,
                date: al.date.clone(),
                kind: kind.into(),
                mb_url: String::new(),
                apple_url: al.url.clone(),
                art_url: al.art.clone(),
                artist_weight: a.weight,
            }));
        }
    }
    send(Msg::TaskDone("New releases (Apple Music)"));
}

/// Last.fm links each name to one MusicBrainz artist, and sometimes it's a same-named stranger (COBRAH, the Swedish
/// electropop artist, is linked to a Kenyan rapper whose one album then became her "latest release").
/// The check: does that artist's MusicBrainz discography include the albums Last.fm actually counts plays for?
/// If it confirms almost none of them and another same-named MusicBrainz artist clearly does, use that one instead.
/// (With little play data to go on, the link is kept as it is.)
fn confirm_mbid(net: &Net, name: &str, mbid: &str, lfm: &[LfmAlbum]) -> String {
    let top = lfm.iter().map(|a| a.plays).max().unwrap_or(0);
    if top < 1000 {
        return mbid.to_string();
    }
    // the most-played Last.fm album this MusicBrainz artist's discography has
    let confirmed = |id: &str| -> u64 {
        let keys: std::collections::HashSet<String> = mb_albums(net, id).unwrap_or_default().iter().map(|r| album_key(&r.title)).collect();
        lfm.iter().filter(|a| keys.contains(&album_key(&a.name))).map(|a| a.plays).max().unwrap_or(0)
    };
    let now = if mbid.is_empty() { 0 } else { confirmed(mbid) };
    if now * 20 >= top {
        return mbid.to_string(); // it has a real share of what Last.fm counts: keep it
    }
    let Ok(found) = mb_search_artist(net, name) else { return mbid.to_string() };
    let best = found
        .into_iter()
        .filter(|m| m.id != mbid && m.score >= 70.0 && artist_key(&m.name) == artist_key(name))
        .take(3)
        .map(|m| {
            let c = confirmed(&m.id);
            (c, m.id)
        })
        .max_by_key(|x| x.0);
    match best {
        Some((c, id)) if c > now && c * 5 >= top => id,
        _ => mbid.to_string(),
    }
}

/// Everything shown when you click an artist: bio, tags and their albums ranked best first.
/// "Best" = most played across all Last.fm listeners, with editions merged and singles/compilations dropped.
/// `mbid_hint` is the MusicBrainz id we already know for this artist (from your Last.fm data or the
/// similar-artists list). Using it avoids landing on a different artist who shares the name.
pub fn load_detail(cfg: &Config, net: &Net, name: &str, mbid_hint: &str, owned_keys: &[String]) -> ArtistDetail {
    let mut d = ArtistDetail { name: name.to_string(), ..Default::default() };
    let key = cfg.lastfm_api_key.as_str();
    let mut errors: Vec<String> = vec![];
    // a pin wins over everything; a pin to "none" means skip MusicBrainz entirely
    let pinned = cfg.pinned_mbid(&artist_key(name)).map(|s| s.to_string());
    let skip_mb = pinned.as_deref() == Some("");
    let mut mbid = pinned.clone().unwrap_or_else(|| mbid_hint.to_string());
    if cfg.has_lastfm() {
        match lfm_artist_info(net, key, name) {
            Ok(info) => {
                if !info.name.is_empty() {
                    d.name = info.name;
                }
                d.url = info.url;
                d.listeners = info.listeners;
                d.tags = info.tags.into_iter().take(6).collect();
                d.bio = info.bio;
                if mbid.is_empty() && !skip_mb {
                    mbid = info.mbid;
                }
            }
            Err(e) if e.contains("could not be found") => {}
            Err(e) => errors.push(e),
        }
    }
    if mbid.is_empty() && !skip_mb {
        match mb_search_artist(net, name) {
            Ok(found) => {
                if let Some(m) = found.into_iter().find(|m| m.score >= 70.0 && artist_key(&m.name) == artist_key(name)) {
                    mbid = m.id;
                }
            }
            Err(e) => errors.push(crate::util::plain_err(format!("MusicBrainz: {e}"))),
        }
    }
    let lfm = if cfg.has_lastfm() {
        lfm_top_albums(net, key, name).unwrap_or_else(|e| {
            if !e.contains("could not be found") {
                errors.push(e);
            }
            vec![]
        })
    } else {
        vec![]
    };
    // Last.fm can link a name to a same-named stranger on MusicBrainz (COBRAH -> a Kenyan rapper): check it
    if pinned.is_none() && !skip_mb {
        mbid = confirm_mbid(net, name, &mbid, &lfm);
    }
    let mb: Vec<MbReleaseGroup> = if mbid.is_empty() {
        vec![]
    } else {
        mb_albums(net, &mbid).unwrap_or_else(|e| {
            errors.push(crate::util::plain_err(format!("MusicBrainz: {e}")));
            vec![]
        })
    };
    let bad_secondary = |r: &MbReleaseGroup| {
        r.secondary.iter().any(|s| matches!(s.as_str(), "Compilation" | "Live" | "Remix" | "DJ-mix" | "Interview" | "Demo" | "Spokenword"))
    };
    // "Dirt Femme (stripped)" (live) and "Dirt Femme" share a key: keep the studio album, else the earliest
    let mut mb_by_key: HashMap<String, &MbReleaseGroup> = HashMap::new();
    for r in &mb {
        let k = album_key(&r.title);
        let better = match mb_by_key.get(&k) {
            None => true,
            Some(e) => (bad_secondary(e) && !bad_secondary(r)) || (bad_secondary(e) == bad_secondary(r) && !r.date.is_empty() && r.date < e.date),
        };
        if better {
            mb_by_key.insert(k, r);
        }
    }
    let owned = |title: &str| {
        let k = album_key(title);
        owned_keys.iter().any(|o| *o == k || (k.len() >= 6 && o.len() >= 6 && (o.starts_with(&k) || k.starts_with(o.as_str()))))
    };

    let max_plays = lfm.iter().map(|a| a.plays).max().unwrap_or(0);
    // Last.fm goes by name, so "Lucy" mixes a techno producer, a hypnagogic pop artist and more.
    // The most-played album MusicBrainz confirms for *this* artist is the yardstick.
    let max_matched = lfm
        .iter()
        .filter(|a| mb_by_key.get(&album_key(&a.name)).map(|r| !bad_secondary(r)).unwrap_or(false))
        .map(|a| a.plays)
        .max()
        .unwrap_or(0);
    let reference = if max_matched > 0 { max_matched } else { max_plays };
    // you picked the MusicBrainz artist yourself: only trust albums it confirms (or that you own)
    let strict = pinned.as_deref().map(|m| !m.is_empty()).unwrap_or(false);
    let mut collision = false;
    let mut ranked: Vec<DetailAlbum> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();
    for a in lfm {
        if a.name.is_empty() || a.name == "(null)" || a.name.eq_ignore_ascii_case("undefined") {
            continue;
        }
        let k = album_key(&a.name);
        if let Some(&i) = index.get(&k) {
            ranked[i].plays += a.plays; // "Deluxe" / "Extended Cut" etc. count toward the original
            continue;
        }
        let rg = mb_by_key.get(&k);
        match rg {
            Some(r) if bad_secondary(r) => continue,
            None if owned(&a.name) => {} // it's in your folder, so it's theirs
            None if looks_like_reissue(&a.name) || a.plays * 50 < reference => continue,
            None if strict && !mb.is_empty() => continue,
            // more popular than everything MusicBrainz confirms: almost certainly a same-named artist
            None if mb.len() >= 5 && max_matched > 0 && a.plays > max_matched => {
                collision = true;
                continue;
            }
            // MusicBrainz knows the artist well but not this title as an album/EP: probably a single
            None if mb.len() >= 3 && a.plays * 10 < reference => continue,
            _ => {}
        }
        index.insert(k, ranked.len());
        ranked.push(DetailAlbum {
            title: rg.map(|r| r.title.clone()).unwrap_or_else(|| crate::util::strip_edition(&a.name)),
            year: rg.map(|r| r.date.chars().take(4).collect()).unwrap_or_default(),
            kind: rg.map(|r| if r.secondary.iter().any(|s| s == "Mixtape/Street") { "Mixtape".to_string() } else { r.primary.clone() }).unwrap_or_default(),
            plays: a.plays,
            art: if a.art.is_empty() { rg.map(|r| format!("https://coverartarchive.org/release-group/{}/front-250", r.id)).unwrap_or_default() } else { a.art },
            url: a.url,
            owned: owned(&a.name),
            date: rg.map(|r| r.date.clone()).unwrap_or_default(),
            score: 0.0,
        });
    }
    let today = Local::now().date_naive();
    let recent_cutoff = today.checked_sub_months(Months::new(24)).unwrap_or(today);
    // the rest of the discography that isn't in Last.fm's top list
    for r in &mb {
        let k = album_key(&r.title);
        if index.contains_key(&k) || bad_secondary(r) {
            continue;
        }
        index.insert(k, usize::MAX);
        let mb_art = format!("https://coverartarchive.org/release-group/{}/front-250", r.id);
        // Recent albums haven't had time to reach the top list: look up their plays and rank them anyway.
        let recent = parse_date_latest(&r.date).map(|dt| dt >= recent_cutoff).unwrap_or(false);
        if recent && cfg.has_lastfm() {
            if let Some(a) = lfm_album_info(net, key, name, &r.title) {
                ranked.push(DetailAlbum {
                    title: r.title.clone(),
                    year: r.date.chars().take(4).collect(),
                    kind: if r.secondary.iter().any(|s| s == "Mixtape/Street") { "Mixtape".into() } else { r.primary.clone() },
                    plays: a.plays,
                    art: if a.art.is_empty() { mb_art } else { a.art },
                    url: if a.url.is_empty() { format!("https://musicbrainz.org/release-group/{}", r.id) } else { a.url },
                    owned: owned(&r.title),
                    date: r.date.clone(),
                    score: 0.0,
                });
                continue;
            }
        }
        d.other.push(DetailAlbum {
            date: r.date.clone(),
            score: 0.0,
            title: r.title.clone(),
            year: r.date.chars().take(4).collect(),
            kind: if r.secondary.iter().any(|s| s == "Mixtape/Street") { "Mixtape".into() } else { r.primary.clone() },
            plays: 0,
            art: format!("https://coverartarchive.org/release-group/{}/front-250", r.id),
            url: format!("https://musicbrainz.org/release-group/{}", r.id),
            owned: owned(&r.title),
        });
    }
    for a in &mut ranked {
        a.score = album_score(a.plays, &a.date, today);
    }
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    d.albums = ranked;
    d.mbid = mbid.clone();
    errors.dedup();
    d.failed = !errors.is_empty();
    if d.albums.is_empty() && d.other.is_empty() {
        d.note = if errors.is_empty() {
            "Couldn't find any albums for this artist on Last.fm or MusicBrainz.".into()
        } else {
            format!("Couldn't load albums: {}. Check your connection and open the artist again.", errors.join("; "))
        };
    } else if !errors.is_empty() {
        d.note = format!("Some details couldn't be loaded ({}), so this list may be incomplete.", errors.join("; "));
    } else if collision {
        d.note = format!(
            "Last.fm mixes several artists called \"{name}\". Albums by the others were left out, but the tags, listener count and bio may describe someone else. If the albums look wrong too, use \"Wrong artist?\"."
        );
    } else if strict {
        d.note = format!("Pinned: albums come only from the MusicBrainz artist you picked. Tags and listener counts are Last.fm's \"{name}\" and may include other artists with this name.");
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{LibArtist, Library};
    use std::collections::HashSet;

    #[test]
    fn one_release_listed_twice_under_two_dates_becomes_one() {
        let mut r = Results::default();
        r.releases = vec![
            ReleaseRec { mb_url: "mb".into(), ..rel("Alex G", "Teenage Sex and Death at Camp Miasma: Original Motion Picture Score", "2026-09-11", "Album") },
            ReleaseRec { apple_url: "apple".into(), art_url: "art".into(), ..rel("Alex G", "Teenage Sex and Death at Camp Miasma (Original Motion Picture Score)", "2026-08-21", "Album") },
            // a single and the album of the same name months apart, a remix album, another artist, a reissue years later
            rel("Cage The Elephant", "Dying in Reverse", "2026-09-10", "Single"),
            rel("Cage The Elephant", "Dying In Reverse", "2027-02-05", "Album"),
            rel("Alex G", "Rocket", "2026-08-01", "Album"),
            rel("Alex G", "Rocket (Remixes)", "2026-08-20", "Album"),
            rel("Alex H", "Rocket", "2026-08-01", "Album"),
            rel("Alex G", "Rocket (Deluxe Edition)", "2027-08-01", "Album"),
        ];
        r.merge_twins();
        assert_eq!(r.releases.len(), 7);
        let score: Vec<&ReleaseRec> = r.releases.iter().filter(|x| x.title.starts_with("Teenage")).collect();
        assert_eq!(score.len(), 1);
        // the earlier date stands, with what the other listing knew
        assert_eq!((score[0].date.as_str(), score[0].apple_url.as_str(), score[0].mb_url.as_str()), ("2026-08-21", "apple", "mb"));
        r.merge_twins();
        assert_eq!(r.releases.len(), 7);
    }

    fn rel(artist: &str, title: &str, date: &str, kind: &str) -> ReleaseRec {
        ReleaseRec { key: rel_key(&artist_key(artist), title), artist_key: artist_key(artist), artist: artist.into(), title: title.into(), date: date.into(), kind: kind.into(), ..Default::default() }
    }

    #[test]
    fn merging() {
        let mut list = vec![];
        // same release from MusicBrainz and Apple, Apple calling it a single
        merge_release(&mut list, ReleaseRec { mb_url: "mb".into(), ..rel("Shygirl", "SWALLOW!", "2026-11-06", "Album") });
        merge_release(&mut list, ReleaseRec { apple_url: "apple".into(), art_url: "art".into(), ..rel("Shygirl", "Swallow! - Single", "2026-11-06", "Single") });
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, "Album");
        assert_eq!(list[0].apple_url, "apple");
        assert_eq!(list[0].mb_url, "mb");
        // a collab found under two of your artists
        merge_release(&mut list, rel("Future", "Ya Hit", "2026-11-06", "Single"));
        merge_release(&mut list, rel("Drake", "Ya Hit", "2026-11-06", "Single"));
        assert_eq!(list.len(), 2);
        // same title, different artist and different month: a different release
        merge_release(&mut list, rel("Someone Else", "Ya Hit", "2025-01-01", "Single"));
        assert_eq!(list.len(), 3);
        // precise date wins over year-only
        merge_release(&mut list, rel("Black Sabbath", "Avro Sessions", "2026", "Album"));
        merge_release(&mut list, rel("Black Sabbath", "Avro Sessions", "2026-03-01", "Album"));
        assert_eq!(list.iter().find(|r| r.title == "Avro Sessions").unwrap().date, "2026-03-01");
    }

    #[test]
    fn reissues() {
        for t in [
            "Bossanova (2026 Remaster)",
            "Merriweather 6/30/85 (Live)",
            "Live at Apple Music Radio",
            "Hidden in Pieces: Live",
            "The Best of Judas Priest",
            "Dracula (Instrumental) [with JENNIE]",
            "Sad Wings of Destiny (50th Anniversary Remixed & Remastered)",
            "Song (Sped Up)",
        ] {
            assert!(looks_like_reissue(t), "{t}");
        }
        for t in ["Live Through This", "I'll Try Living Like This", "Alive", "Deliver", "Sessions of the Mind Pt 2 Demolition"] {
            assert!(!looks_like_reissue(t), "{t}");
        }
    }

    #[test]
    fn scores() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        // same plays: the newer album ranks higher
        assert!(album_score(10_000_000, "2022-10-14", today) > album_score(10_000_000, "2014-09-24", today));
        // a much bigger classic still beats a brand-new album with a fraction of the plays
        assert!(album_score(38_000_000, "2014-09-24", today) > album_score(1_600_000, "2026-09-18", today));
        // brand-new albums are capped at 3 months of age so day-one hype doesn't explode
        assert_eq!(album_score(1000, "2026-11-06", today), album_score(1000, "2026-10-20", today));
        // unknown date: treated as 5 years old
        assert!(album_score(1000, "", today) > 0.0);
    }

    #[test]
    fn outage_detection() {
        let sent = std::cell::RefCell::new(vec![]);
        let send = |m: Msg| sent.borrow_mut().push(matches!(m, Msg::Incomplete));
        // offline from the start: gives up after 5
        let mut f = Failures::new("MusicBrainz");
        assert!((0..4).all(|_| !f.fail("timeout", &send)));
        assert!(f.fail("timeout", &send));
        assert!(sent.borrow().contains(&true));
        // worked for a while, then a few hiccups: keeps going
        let mut f = Failures::new("Apple Music");
        f.ok();
        assert!((0..11).all(|_| !f.fail("HTTP 503", &send)));
        f.ok();
        assert!((0..11).all(|_| !f.fail("HTTP 503", &send)));
        assert!(f.fail("HTTP 503", &send));
    }

    #[test]
    fn ownership() {
        let mut artists = HashMap::new();
        let mk = |albums: &[&str], tracks: &[&str]| LibArtist {
            folder: String::new(), name: String::new(),
            albums: albums.iter().map(|a| (a.to_string(), None)).collect(),
            album_keys: albums.iter().map(|a| album_key(a)).collect::<HashSet<_>>(),
            track_keys: tracks.iter().map(|t| crate::util::track_key_from_file(t)).collect(),
            tracks: tracks.len(),
        };
        artists.insert(artist_key("death's dynamic shroud"), mk(&["Ungodly Gaze (2025)", "Faith in Persona (2021)"], &["01 - Heaven"]));
        artists.insert(artist_key("death's dynamic shroud & galen tipton"), mk(&["Collab Album (2024)"], &[]));
        artists.insert(artist_key("Arcade Fire"), mk(&["Funeral  [Japanese Special Limited Edition] (2004)", "WE (2022)"], &[]));
        let lib = Library { artists, aliases: [(artist_key("death's dynamic shroud.wmv"), artist_key("death's dynamic shroud"))].into() };
        let dds = lib.key_for("death’s dynamic shroud.wmv");
        assert_eq!(dds, artist_key("death's dynamic shroud"));
        assert!(lib.owns(&dds, "Ungodly Gaze", false));
        assert!(lib.owns(&dds, "Faith in Persona [Web FLAC]", false));
        assert!(lib.owns(&dds, "Collab Album", false)); // filed under the collab folder
        assert!(lib.owns(&dds, "Heaven", true)); // single already on an album
        assert!(!lib.owns(&dds, "Heaven", false)); // ...but an album called Heaven is new
        assert!(!lib.owns(&dds, "Perfect Angel Ribbon", false));
        let af = artist_key("Arcade Fire");
        assert!(lib.owns(&af, "Funeral", false));
        assert!(lib.owns(&af, "WE", false));
        assert!(!lib.owns(&af, "Open Your Heart or Die Trying", false));
    }
}

/// One possible MusicBrainz match for the "Wrong artist?" picker.
#[derive(Clone, Default)]
pub struct Candidate {
    pub c: MbCandidate,
    /// a few of their album titles, newest first
    pub titles: Vec<String>,
    /// how many release groups MusicBrainz lists (capped at 100)
    pub releases: usize,
    /// how many of your album folders for this artist match their releases
    pub owned_hits: usize,
}

/// Same-named artists for `query`, best match first: the ones whose releases match your folders rank top.
pub fn find_candidates(net: &Net, query: &str, owned_keys: &[String]) -> Result<Vec<Candidate>, String> {
    let qk = artist_key(query);
    let mut list: Vec<MbCandidate> = mb_artist_candidates(net, query)?;
    // keep close name matches (plus anything scoring very high), at most 6: each costs a 1-second lookup
    list.retain(|c| c.score >= 60.0);
    list.sort_by(|a, b| {
        let ea = artist_key(&a.name) == qk;
        let eb = artist_key(&b.name) == qk;
        eb.cmp(&ea).then(b.score.total_cmp(&a.score))
    });
    list.truncate(6);
    let mut out = vec![];
    for c in list {
        let rgs = mb_some_release_groups(net, &c.id).unwrap_or_default();
        let owned_hits = owned_keys
            .iter()
            .filter(|o| rgs.iter().any(|r| {
                let k = album_key(&r.title);
                k == **o || (k.len() >= 6 && o.len() >= 6 && (o.starts_with(&k) || k.starts_with(o.as_str())))
            }))
            .count();
        let mut albums: Vec<&MbReleaseGroup> = rgs.iter().filter(|r| r.primary == "Album" || r.primary == "EP").collect();
        albums.sort_by(|a, b| b.date.cmp(&a.date));
        let titles = albums.iter().take(4).map(|r| {
            let y: String = r.date.chars().take(4).collect();
            if y.is_empty() { r.title.clone() } else { format!("{} ({y})", r.title) }
        }).collect();
        out.push(Candidate { releases: rgs.len(), titles, owned_hits, c });
    }
    // the artist whose catalogue overlaps your folders is almost certainly the right one
    out.sort_by(|a, b| b.owned_hits.cmp(&a.owned_hits));
    Ok(out)
}
