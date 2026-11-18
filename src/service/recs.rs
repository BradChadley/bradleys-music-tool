//! Recs through the engine (protocol 4; ENGINE-PROTOCOL.md, "Protocol 4: Recs"). The work is the Music
//! Tool's own refresh (`engine::run_with`), with Cadence's play counts in place of Last.fm's history and
//! Cadence's Last.fm account, asked for at the start of each refresh (`lastfm.account`) and kept in memory for
//! that refresh only: never in a file, never in the log, never in a message.
//!
//! What is kept: `<Tools>\recs.json` (the Music Tool's `Results`, plus whether Last.fm was used, the music
//! folder and the albums removed from Cadence). It is written when a refresh ends, so the covers on screen
//! never lose a row mid-refresh; a stopped refresh keeps what it had found, added to what was there.
//! Not interested and muted artists are the engine's settings (`dismissed`, `muted`), the Music Tool's own.
use super::jobs::{fail, load, Failure, OFFLINE};
use super::{Engine, Env, Job, Out};
use crate::engine::{self, Msg, Played, ReleaseRec, Results, SimilarRec};
use crate::util::{album_key, artist_key};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the rows found are sent while a refresh runs.
const SEND_EVERY: Duration = Duration::from_millis(250);

/// `<Tools>\recs.json`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct RecsFile {
    #[serde(flatten)]
    pub(crate) results: Results,
    /// Artists to try came from Last.fm this time (an account was given)
    pub(crate) lastfm: bool,
    /// the music folder the refresh read
    pub(crate) folder: String,
    /// albums removed from Cadence: (artist, album), as Cadence said
    pub(crate) removed: Vec<(String, String)>,
}

fn recs_file(env: &Env) -> PathBuf {
    env.dir.join("recs.json")
}

pub(crate) fn read(env: &Env) -> RecsFile {
    std::fs::read(recs_file(env)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write(env: &Env, f: &RecsFile) -> Result<(), Failure> {
    let b = serde_json::to_vec(f).map_err(|e| fail("failed", format!("The recommendations could not be written out ({e}).")))?;
    crate::util::write_atomic(&recs_file(env), &b).map_err(|e| fail("failed", crate::helpers::said("the recommendations could not be saved.", &e)))
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string()
}

fn count(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

// ------------------------------------------------------------------ rows as Cadence gets them

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn release_row(r: &ReleaseRec, plays: &HashMap<&str, u64>, in_library: bool) -> Value {
    json!({
        "key": r.key, "artist": r.artist, "title": r.title, "date": r.date, "kind": r.kind.to_lowercase(), "art_url": r.art_url,
        "plays": plays.get(r.artist_key.as_str()).copied().unwrap_or(0), "in_library": in_library, "upcoming": r.date.as_str() > today().as_str(),
        "tracks": r.tracks,
    })
}

fn similar_row(s: &SimilarRec, top: f64) -> Value {
    json!({
        "key": s.key, "name": s.name, "score": if top > 0.0 { (s.score / top * 100.0).round() / 100.0 } else { 0.0 },
        "because": s.because.iter().map(|b| b.0.clone()).collect::<Vec<_>>(), "photo_url": null, "url": s.url, "listeners": s.listeners,
        "tags": s.tags,
    })
}

fn plays_of(r: &Results) -> HashMap<&str, u64> {
    r.artists.iter().map(|a| (a.key.as_str(), a.plays_all)).collect()
}

// ------------------------------------------------------------------ recs.refresh

pub(crate) struct RefreshAsk {
    folder: PathBuf,
    plays: Vec<Played>,
    removed: Vec<(String, String)>,
}

pub(crate) fn refresh_check(env: &Env, params: &Value) -> Result<RefreshAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is looked up while Cadence runs as a test."));
    }
    let folder = super::library::folder_of(params, "A refresh")?;
    if !folder.is_dir() {
        return Err(fail("no_music_folder", format!("The music folder {} could not be found.", folder.display())));
    }
    let plays = params
        .get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| !text(a, "name").is_empty())
        .map(|a| Played { name: text(a, "name"), plays: count(a, "plays"), plays_12m: count(a, "plays_12m"), plays_3m: count(a, "plays_3m"), plays_recent: count(a, "plays_recent") })
        .collect();
    let removed = params.get("removed").and_then(Value::as_array).into_iter().flatten().map(|r| (text(r, "artist"), text(r, "album"))).filter(|(a, b)| !a.is_empty() && !b.is_empty()).collect();
    Ok(RefreshAsk { folder, plays, removed })
}

/// Cadence's Last.fm account, for this refresh only. None when it has none, or does not answer.
fn account(out: &Out, env: &Env, job: &Job) -> Option<(String, String)> {
    let v = out.ask("lastfm.account", json!({}), env.parts.ask_wait, &|| job.stopped()).ok()?;
    let (user, key) = (text(&v, "user"), text(&v, "api_key"));
    (!user.is_empty() && !key.is_empty()).then_some((user, key))
}

/// The Music Tool's refresh with Cadence's plays and account. Rows stream out as they are found; the file is
/// written once, at the end (a stop keeps what was found, added to what was there).
pub(crate) fn refresh(out: &Out, job: &Job, id: u64, eng: &Engine, ask: RefreshAsk) -> Result<Value, Failure> {
    let env = &eng.env;
    let mut cfg = load(env)?;
    let account = account(out, env, job);
    let lastfm = account.is_some();
    cfg.music_dir = ask.folder.to_string_lossy().to_string();
    cfg.lastfm_disconnected = false;
    (cfg.lastfm_user, cfg.lastfm_api_key) = account.unwrap_or_default();
    let old = read(env);
    let (tx, rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker = {
        let (net, cancel) = (env.parts.net.clone(), cancel.clone());
        std::thread::spawn(move || engine::run_with(cfg, net, tx, cancel, || {}, Some(ask.plays)))
    };
    // this refresh's own finds; the old ones stay in the file until it ends
    let mut fresh = Results { seen: old.results.seen.clone(), ..Default::default() };
    let (mut incomplete, mut notes) = (false, Vec::<String>::new());
    let mut sent_rel: HashMap<String, String> = HashMap::new();
    let mut sent_sim: HashSet<String> = HashSet::new();
    let mut step = ("releases", 0usize, 0usize);
    let mut last_sent = Instant::now();
    let mut dirty = false;
    loop {
        if job.stopped() {
            cancel.store(true, Ordering::Relaxed);
        }
        let mut finished = false;
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => match m {
                Msg::Progress { task, done, total } => {
                    step = (if task.contains("Last.fm") { "similar" } else { "releases" }, done, total);
                    dirty = true;
                }
                Msg::Artists(a) => fresh.artists = a,
                Msg::Similar(s) => {
                    fresh.similar = s;
                    dirty = true;
                }
                Msg::SimilarMore(s) => {
                    for x in s {
                        if !fresh.similar.iter().any(|y| y.key == x.key) {
                            fresh.similar.push(x);
                        }
                    }
                    dirty = true;
                }
                Msg::RelCursor(n) => fresh.rel_cursor = n,
                Msg::BackTo(d) => fresh.back_to = d,
                Msg::SeedCursor(n) => fresh.seed_cursor = n,
                Msg::Release(r) => {
                    fresh.add_release(r);
                    dirty = true;
                }
                Msg::Reject(k) => fresh.reject(k),
                Msg::Warn(_) => {}
                Msg::Error(e) => notes.push(e),
                Msg::Incomplete => incomplete = true,
                Msg::Finished => finished = true,
                Msg::Status(_) | Msg::TaskDone(_) | Msg::Lib(_) => {}
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => finished = true,
        }
        if dirty && (finished || last_sent.elapsed() >= SEND_EVERY) {
            // the rows not sent before, or changed since (a cover or a date filled in)
            let plays = plays_of(&fresh);
            let mut new = vec![];
            for r in &fresh.releases {
                let row = release_row(r, &plays, false);
                let said = row.to_string();
                if sent_rel.get(&r.key) != Some(&said) {
                    sent_rel.insert(r.key.clone(), said);
                    new.push(row);
                }
            }
            let top = fresh.similar.iter().map(|s| s.score).fold(0.0, f64::max);
            for s in &fresh.similar {
                if sent_sim.insert(s.key.clone()) {
                    new.push(similar_row(s, top));
                }
            }
            out.progress(id, json!({ "step": step.0, "done": step.1, "total": step.2, "new": new }));
            last_sent = Instant::now();
            dirty = false;
        }
        if finished {
            break;
        }
    }
    let _ = worker.join();
    let stopped = cancel.load(Ordering::Relaxed);
    let mut file = RecsFile { lastfm, folder: ask.folder.to_string_lossy().to_string(), removed: ask.removed, ..Default::default() };
    if stopped || incomplete {
        // what was there stays, with this refresh's finds added and updated
        let mut kept = old.results;
        for r in std::mem::take(&mut fresh.releases) {
            kept.add_release(r);
        }
        for s in std::mem::take(&mut fresh.similar) {
            match kept.similar.iter_mut().find(|k| k.key == s.key) {
                Some(k) => *k = s,
                None => kept.similar.push(s),
            }
        }
        if !fresh.artists.is_empty() {
            kept.artists = fresh.artists;
        }
        file.results = kept;
    } else {
        // Without an account Artists to try is not looked at: the ones there were stay
        if !lastfm && fresh.similar.is_empty() {
            fresh.similar = old.results.similar;
        }
        file.results = fresh;
    }
    if !stopped {
        file.results.mark_new();
    }
    write(env, &file)?;
    if stopped {
        return Err(fail("cancelled", "The refresh was stopped; what it had found is kept."));
    }
    Ok(json!({ "releases": file.results.releases.len(), "similar": file.results.similar.len(), "lastfm": lastfm, "generated": chrono::Local::now().to_rfc3339(), "notes": notes }))
}

// ------------------------------------------------------------------ reading

/// The library as it is now, for "in your library" (directory names only: no tags are read).
fn library_of(folder: &str, cfg: &crate::config::Config) -> Option<crate::library::Library> {
    if folder.is_empty() {
        return None;
    }
    crate::library::Library::scan(folder, &cfg.skip_folders, &cfg.alias_file).ok()
}

fn removed_from(f: &RecsFile) -> HashSet<(String, String)> {
    f.removed.iter().map(|(a, b)| (artist_key(a), album_key(b))).collect()
}

/// `recs.get`: the recommendations, leaving out Not interested, muted artists and albums removed from
/// Cadence; albums now in the library are marked `in_library`.
pub(crate) fn get(env: &Env, params: &Value) -> Result<Value, Failure> {
    let cfg = load(env)?;
    let f = read(env);
    let earlier = params.get("show_earlier").and_then(Value::as_bool).unwrap_or(false);
    let cutoff = {
        let d = chrono::Local::now().date_naive();
        let c = d.checked_sub_months(chrono::Months::new(cfg.months_back)).unwrap_or(d);
        chrono::NaiveDate::from_ymd_opt(chrono::Datelike::year(&c), chrono::Datelike::month(&c), 1).unwrap_or(c).format("%Y-%m-%d").to_string()
    };
    let lib = library_of(&f.folder, &cfg);
    let removed = removed_from(&f);
    let plays = plays_of(&f.results);
    let mut releases: Vec<&ReleaseRec> = f
        .results
        .releases
        .iter()
        .filter(|r| !cfg.dismissed.contains(&r.key) && !cfg.muted.contains_key(&r.artist_key))
        .filter(|r| !removed.contains(&(r.artist_key.clone(), album_key(&r.title))))
        .filter(|r| earlier || r.date.as_str() >= cutoff.as_str())
        .collect();
    releases.sort_by(|a, b| b.date.cmp(&a.date));
    let rows: Vec<Value> = releases.iter().map(|r| release_row(r, &plays, lib.as_ref().is_some_and(|l| l.owns(&r.artist_key, &r.title, r.kind == "Single")))).collect();
    let similar: Vec<&SimilarRec> = f.results.similar.iter().filter(|s| !cfg.dismissed.contains(&format!("artist|{}", s.key)) && !cfg.muted.contains_key(&s.key)).collect();
    let top = similar.iter().map(|s| s.score).fold(0.0, f64::max);
    let generated = (f.results.generated_ts > 0).then(|| chrono::DateTime::from_timestamp(f.results.generated_ts, 0).map(|d| d.with_timezone(&chrono::Local).to_rfc3339())).flatten();
    Ok(json!({
        "generated": generated, "lastfm": f.lastfm, "newest_date": releases.first().map(|r| r.date.clone()),
        "releases": rows, "similar": similar.iter().map(|s| similar_row(s, top)).collect::<Vec<_>>(),
    }))
}

/// `recs.artist`: the artist page's part, from `recs.json` alone (no network): their newest release not in
/// the library, and up to 8 artists to try that are like them. `covered` is false for an artist the last
/// refresh did not look at.
pub(crate) fn artist(env: &Env, params: &Value) -> Result<Value, Failure> {
    let name = text(params, "name");
    if name.is_empty() {
        return Err(fail("bad_request", "Say which artist (\"name\")."));
    }
    let cfg = load(env)?;
    let f = read(env);
    let key = artist_key(&name);
    let owned: HashSet<String> = params.get("owned").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(album_key).collect();
    let removed = removed_from(&f);
    let plays = plays_of(&f.results);
    let new = f
        .results
        .releases
        .iter()
        .filter(|r| r.artist_key == key && !owned.contains(&album_key(&r.title)) && !cfg.dismissed.contains(&r.key) && !removed.contains(&(key.clone(), album_key(&r.title))))
        .max_by(|a, b| a.date.cmp(&b.date))
        .map(|r| release_row(r, &plays, false));
    let top = f.results.similar.iter().map(|s| s.score).fold(0.0, f64::max);
    let tries: Vec<Value> = f
        .results
        .similar
        .iter()
        .filter(|s| s.because.iter().any(|b| artist_key(&b.0) == key) && !cfg.dismissed.contains(&format!("artist|{}", s.key)) && !cfg.muted.contains_key(&s.key))
        .take(8)
        .map(|s| similar_row(s, top))
        .collect();
    let covered = f.results.artists.iter().take(f.results.rel_cursor.max(cfg.check_artists)).any(|a| a.key == key);
    Ok(json!({ "new": new, "try": tries, "covered": covered }))
}

// ------------------------------------------------------------------ Not interested, mute

/// `recs.not_interested {key, on}`: a release (its key) or an artist to try ("artist|<key>").
pub(crate) fn not_interested(env: &Env, params: &Value) -> Result<Value, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is changed while Cadence runs as a test."));
    }
    let key = text(params, "key");
    let Some(on) = params.get("on").and_then(Value::as_bool) else { return Err(fail("bad_request", "Say \"on\": true or false.")) };
    if key.is_empty() || key.len() > 600 {
        return Err(fail("bad_request", "Name the recommendation by its key."));
    }
    let mut cfg = load(env)?;
    if on {
        // (its names are kept beside it, for Settings to list after it has left the recommendations)
        let f = read(env);
        let names = f.results.releases.iter().find(|r| r.key == key).map(|r| (r.artist.clone(), r.title.clone())).or_else(|| key.strip_prefix("artist|").and_then(|k| f.results.similar.iter().find(|s| s.key == k)).map(|s| (s.name.clone(), String::new())));
        cfg.dismissed.insert(key.clone());
        if let Some(n) = names {
            cfg.dismissed_names.insert(key, n);
        }
    } else {
        cfg.dismissed.remove(&key);
        cfg.dismissed_names.remove(&key);
    }
    super::jobs::save(env, &cfg)?;
    Ok(json!({}))
}

/// `recs.mute {artist, on}`: an artist's releases are left out, and the refresh spends no lookups on them.
pub(crate) fn mute(env: &Env, params: &Value) -> Result<Value, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is changed while Cadence runs as a test."));
    }
    let name = text(params, "artist");
    let Some(on) = params.get("on").and_then(Value::as_bool) else { return Err(fail("bad_request", "Say \"on\": true or false.")) };
    if artist_key(&name).is_empty() {
        return Err(fail("bad_request", "Say which artist (\"artist\")."));
    }
    let mut cfg = load(env)?;
    if on {
        cfg.muted.insert(artist_key(&name), name);
    } else {
        cfg.muted.remove(&artist_key(&name));
    }
    super::jobs::save(env, &cfg)?;
    Ok(json!({}))
}

/// `recs.hidden`: what Not interested and mute have left out, for Settings' Bring back.
pub(crate) fn hidden(env: &Env) -> Result<Value, Failure> {
    let cfg = load(env)?;
    let list: Vec<Value> = cfg
        .dismissed
        .iter()
        .map(|k| {
            let (artist, title) = cfg.dismissed_names.get(k).cloned().unwrap_or_default();
            json!({ "key": k, "artist": artist, "title": title })
        })
        .collect();
    Ok(json!({ "not_interested": list, "muted": cfg.muted.values().collect::<Vec<_>>() }))
}
