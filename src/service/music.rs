//! Get music: `music.search` finds copies of an album on Soulseek, ranked the Music Tool's way, and `music.get`
//! downloads one and saves it into the music folder with the Music Tool's importer. See ENGINE-PROTOCOL.md.
use super::jobs::{fail, load, Failure, OFFLINE};
use super::{Engine, Env, Job, Out};
use crate::dl_facts::{Album, Fit};
use crate::downloads::filter::Filter;
use crate::downloads::{gather_discs, group, Group, Staged};
use crate::importer::ctx::Ctx;
use crate::importer::import::{import_unit, ImportResult};
use crate::importer::model::Action;
use crate::importer::scan::{self, Fate};
use crate::slskd::{self, Candidate, Client};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a search's answer is reused for the same album and filter.
const REUSE_FOR: Duration = Duration::from_secs(10 * 60);
/// The most copies the engine remembers; the oldest are forgotten first.
const MOST_COPIES: usize = 5000;
/// How often a running search or download is looked at.
const LOOK_EVERY: Duration = Duration::from_millis(250);
/// How long a download may take to show up in the client's list once it was asked for.
const SHOW_UP: Duration = Duration::from_secs(60);

/// The copies found so far (each named by its `copy` id for as long as the engine runs) and the searches
/// that can be reused.
#[derive(Default)]
pub(crate) struct Store {
    copies: Mutex<Copies>,
    reuse: Mutex<HashMap<String, (Instant, Value)>>,
    /// one change at a time to the list of kept downloads
    kept: Mutex<()>,
}

#[derive(Default)]
struct Copies {
    next: u64,
    by_id: HashMap<String, Candidate>,
    order: std::collections::VecDeque<String>,
}

impl Store {
    fn remember(&self, c: &Candidate) -> String {
        let mut g = self.copies.lock().unwrap_or_else(|e| e.into_inner());
        g.next += 1;
        let id = format!("c{}", g.next);
        g.by_id.insert(id.clone(), c.clone());
        g.order.push_back(id.clone());
        while g.order.len() > MOST_COPIES {
            if let Some(old) = g.order.pop_front() {
                g.by_id.remove(&old);
            }
        }
        id
    }
    fn copy(&self, id: &str) -> Option<Candidate> {
        self.copies.lock().unwrap_or_else(|e| e.into_inner()).by_id.get(id).cloned()
    }
}

fn text(params: &Value, key: &str) -> String {
    params.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string()
}

// ------------------------------------------------------------------ music.search

pub(crate) struct SearchAsk {
    artist: String,
    album: String,
    filter: String,
    fresh: bool,
}

pub(crate) fn search_check(env: &Env, params: &Value) -> Result<SearchAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Soulseek is not searched while Cadence runs as a test."));
    }
    let ask = SearchAsk { artist: text(params, "artist"), album: text(params, "album"), filter: text(params, "filter"), fresh: params.get("fresh").and_then(Value::as_bool).unwrap_or(false) };
    if ask.artist.is_empty() && ask.album.is_empty() {
        return Err(fail("bad_request", "A search needs an artist or an album."));
    }
    if ask.artist.chars().count() + ask.album.chars().count() > 300 {
        return Err(fail("bad_request", "The artist and album are too long to search for."));
    }
    Ok(ask)
}

/// What a copy is said as. `whole` and `missing` only when the album's track list is known.
fn shown(id: &str, c: &Candidate, album: Option<&Album>, best: bool) -> Value {
    let mut v = json!({
        "copy": id, "user": c.user, "folder": c.name, "format": c.format, "tracks": c.tracks, "bytes": c.bytes,
        "free_slot": c.free_slot, "queue": c.queue, "speed": c.speed, "score": c.score, "best": best,
    });
    if let Some(album) = album {
        if let (Some((_, fit)), Some(least)) = (album.fit(c.tracks), album.editions.first()) {
            v["whole"] = json!(fit == Fit::Whole);
            v["missing"] = json!(least.saturating_sub(c.tracks));
        }
    }
    v
}

fn reuse_key(a: &SearchAsk, prefer: bool) -> String {
    let n = |s: &str| crate::util::norm(s);
    format!("{}|{}|{}|{prefer}", n(&a.artist), n(&a.album), a.filter.to_lowercase())
}

/// The staged search (two short looks and the full search, `slskd::LOOKS`), ranked as the Music Tool ranks
/// (with the engine's `prefer_starts_now`), narrowed by the filter. Copies are sent in progress events as they
/// arrive; the result is the whole ranked list.
pub(crate) fn search(out: &Out, job: &Job, id: u64, eng: &Engine, ask: SearchAsk) -> Result<Value, Failure> {
    let prefer = load(&eng.env)?.prefer_starts_now;
    let key = reuse_key(&ask, prefer);
    if !ask.fresh {
        let kept = eng.music.reuse.lock().unwrap_or_else(|e| e.into_inner()).get(&key).filter(|(at, _)| at.elapsed() < REUSE_FOR).map(|(_, v)| v.clone());
        if let Some(mut v) = kept {
            v["cached"] = json!(true);
            return Ok(v);
        }
    }
    let _use = eng.soulseek.using();
    let query = format!("{} {}", ask.artist, ask.album).trim().to_string();
    // the album's track list is looked up while Soulseek is searched
    let album_of = eng.env.parts.album.clone();
    let words = query.clone();
    let album = std::thread::spawn(move || album_of(&words));
    let client = eng.soulseek.ready(job)?;
    let mut staged = Staged::start(&client, query.clone());
    let filter = Filter::read(&ask.filter);
    let mut ids: HashMap<(String, String), String> = HashMap::new();
    let mut known: Option<Option<Album>> = None;
    let mut album = Some(album);
    let ended = loop {
        if job.stopped() {
            break Err(fail("cancelled", "The search was stopped."));
        }
        staged.poll(&client);
        if known.is_none() && album.as_ref().is_some_and(|a| a.is_finished()) {
            known = Some(album.take().and_then(|a| a.join().ok()).flatten());
        }
        let ranked = slskd::rank(staged.snap.candidates.clone(), prefer);
        let mut new = vec![];
        for c in ranked.iter().filter(|c| filter.keeps(c).is_some()) {
            let k = (c.user.clone(), c.name.clone());
            if !ids.contains_key(&k) {
                let copy = eng.music.remember(c);
                new.push(shown(&copy, c, known.as_ref().and_then(Option::as_ref), false));
                ids.insert(k, copy);
            }
        }
        if !new.is_empty() {
            out.progress(id, json!({ "copies": ids.len(), "new": new }));
        }
        if staged.snap.complete || staged.snap.error.is_some() {
            break Ok(ranked);
        }
        std::thread::sleep(LOOK_EVERY);
    };
    let error = staged.snap.error.clone();
    staged.close(&client);
    let ranked = ended?;
    if let (Some(why), true) = (error, ranked.is_empty()) {
        return Err(fail("failed", why));
    }
    let album = known.unwrap_or_else(|| album.and_then(|a| a.join().ok()).flatten());
    // the one the Music Tool marks: the top of the whole list, when the filter keeps it
    let best = ranked.first().filter(|c| filter.keeps(c).is_some()).map(|c| (c.user.clone(), c.name.clone()));
    let mut copies = vec![];
    for c in ranked.iter().filter(|c| filter.keeps(c).is_some()) {
        let k = (c.user.clone(), c.name.clone());
        let copy = match ids.get(&k) {
            Some(copy) => {
                // (the latest word on it: the list may have changed since it was first sent)
                eng.music.copies.lock().unwrap_or_else(|e| e.into_inner()).by_id.insert(copy.clone(), c.clone());
                copy.clone()
            }
            None => eng.music.remember(c),
        };
        copies.push(shown(&copy, c, album.as_ref(), best.as_ref() == Some(&k)));
    }
    let v = json!({ "query": query, "copies": copies, "cached": false });
    eng.music.reuse.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

// ------------------------------------------------------------------ downloads kept for a later try

/// A download kept in `<Tools>\downloads` for a later try: one the importer doubted (`doubtful`: a person
/// decides, with `music.put_in` or `music.forget`), or one whose saving was stopped or did not work
/// (`stopped`, `failed`: a later `music.get` of the same copy uses the files already there). The list is
/// `<Tools>\kept-downloads.json`, so it outlives the engine. A kept download is named by the person who
/// shared it and their folder's name (`user` and `folder` in a copy), since copy ids last one run only.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Kept {
    pub(crate) user: String,
    /// the folder's name on their computer (`folder` in a copy)
    pub(crate) folder: String,
    /// its name in the downloads folder
    pub(crate) dir: String,
    /// what was asked for: every file of the copy
    pub(crate) copy: Candidate,
    pub(crate) artist: String,
    pub(crate) album: String,
    /// "doubtful", "stopped" or "failed"
    pub(crate) reason: String,
    /// why, in words for a person
    pub(crate) words: String,
}

fn kept_file(env: &Env) -> PathBuf {
    env.dir.join("kept-downloads.json")
}

fn read_kept(env: &Env) -> Vec<Kept> {
    std::fs::read(kept_file(env)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

impl Store {
    fn lock_kept(&self) -> std::sync::MutexGuard<'_, ()> {
        self.kept.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn find_kept(eng: &Engine, user: &str, folder: &str) -> Option<Kept> {
    let _g = eng.music.lock_kept();
    read_kept(&eng.env).into_iter().find(|k| k.user == user && k.folder == folder)
}

/// Writes the list with `k` in it (in place of an older entry for the same download), or without the
/// download `(user, folder)` when `k` is None.
fn change_kept(eng: &Engine, user: &str, folder: &str, k: Option<Kept>) {
    let _g = eng.music.lock_kept();
    let mut list = read_kept(&eng.env);
    let before = list.len();
    list.retain(|x| !(x.user == user && x.folder == folder));
    if k.is_none() && list.len() == before {
        return;
    }
    list.extend(k);
    let written = serde_json::to_vec_pretty(&list).map_err(|e| e.to_string()).and_then(|b| crate::util::write_atomic(&kept_file(&eng.env), &b).map_err(|e| e.to_string()));
    if let Err(why) = written {
        eprintln!("cadence-engine: the list of kept downloads could not be written: {why}");
    }
}

/// The kept downloads, for Cadence to show (and to name after a restart).
pub(crate) fn kept_list(eng: &Engine) -> Value {
    let downloads = &eng.soulseek.downloads;
    let list: Vec<Value> = {
        let _g = eng.music.lock_kept();
        read_kept(&eng.env)
            .iter()
            .map(|k| {
                json!({
                    "user": k.user, "folder": k.folder, "artist": k.artist, "album": k.album, "reason": k.reason, "words": k.words,
                    "files": k.copy.files.len(), "bytes": k.copy.bytes, "on_disk": downloads.join(&k.dir).is_dir(),
                })
            })
            .collect()
    };
    json!({ "kept": list })
}

// ------------------------------------------------------------------ music.get, music.put_in, music.forget

pub(crate) struct GetAsk {
    copy: Candidate,
    artist: String,
    album: String,
}

/// The copy a request names: `copy` (an id from a search in this run), or `user` and `folder` (a kept
/// download, also after the engine was started again).
fn named(eng: &Engine, params: &Value) -> Result<Candidate, Failure> {
    let id = text(params, "copy");
    if !id.is_empty() {
        return eng.music.copy(&id).ok_or_else(|| fail("bad_request", format!("The engine does not know the copy \"{id}\". Search again.")));
    }
    let (user, folder) = (text(params, "user"), text(params, "folder"));
    if user.is_empty() || folder.is_empty() {
        return Err(fail("bad_request", "Name the copy: its \"copy\" id, or the \"user\" and \"folder\" of a kept download."));
    }
    find_kept(eng, &user, &folder).map(|k| k.copy).ok_or_else(|| fail("bad_request", format!("There is no kept download of \"{folder}\" from {user}. Search again.")))
}

pub(crate) fn get_check(env: &Env, eng: &Engine, params: &Value) -> Result<GetAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is downloaded while Cadence runs as a test."));
    }
    Ok(GetAsk { copy: named(eng, params)?, artist: text(params, "artist"), album: text(params, "album") })
}

pub(crate) struct PutAsk {
    kept: Kept,
    artist: String,
    album: String,
}

pub(crate) fn put_in_check(env: &Env, eng: &Engine, params: &Value) -> Result<PutAsk, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is saved while Cadence runs as a test."));
    }
    let c = named(eng, params)?;
    let kept = find_kept(eng, &c.user, &c.name).filter(|k| eng.soulseek.downloads.join(&k.dir).is_dir());
    let kept = kept.ok_or_else(|| fail("bad_request", format!("There is no kept download of \"{}\" from {} to put in. Get it first.", c.name, c.user)))?;
    Ok(PutAsk { artist: text(params, "artist"), album: text(params, "album"), kept })
}

pub(crate) fn forget_check(env: &Env, eng: &Engine, params: &Value) -> Result<Candidate, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is changed while Cadence runs as a test."));
    }
    named(eng, params)
}

/// Sends a kept download to the Recycle Bin and takes it off the list. `forgotten` is false when nothing of
/// that copy was kept.
pub(crate) fn forget(eng: &Engine, c: &Candidate) -> Result<Value, Failure> {
    let Some(k) = find_kept(eng, &c.user, &c.name) else { return Ok(json!({ "forgotten": false })) };
    let dir = eng.soulseek.downloads.join(&k.dir);
    if crate::importer::is_folder_name(&k.dir) && dir.is_dir() {
        (eng.env.parts.trash)(&dir).map_err(|why| fail("failed", format!("The kept download could not be sent to the Recycle Bin: {why}")))?;
    }
    change_kept(eng, &k.user, &k.folder, None);
    Ok(json!({ "forgotten": true }))
}

/// The files of `c` that are not in the downloads folder yet (all of them when nothing of it was kept).
fn missing_files(downloads: &Path, kept: Option<&Kept>, c: &Candidate) -> Candidate {
    let Some(k) = kept else { return c.clone() };
    let folder = downloads.join(&k.dir);
    let there = |f: &slskd::RemoteFile| {
        let (dir, leaf) = f.filename.rsplit_once(['\\', '/']).unwrap_or(("", &f.filename));
        let disc = dir.rsplit_once(['\\', '/']).map(|(_, d)| d).unwrap_or(dir);
        let mut places = vec![folder.join(leaf)];
        if slskd::is_disc_folder(disc) && crate::importer::is_folder_name(disc) {
            places.push(folder.join(disc).join(leaf));
        }
        crate::importer::is_folder_name(leaf) && places.iter().any(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && (f.size == 0 || m.len() == f.size)))
    };
    let mut missing = c.clone();
    missing.files.retain(|f| !there(f));
    missing
}

/// This copy's downloads in the client's list (by the person and the folder they arrive in).
fn ours<'a>(groups: &'a [Group], c: &Candidate) -> Option<&'a Group> {
    let clean = crate::importer::clean_name(&c.name);
    groups.iter().find(|g| g.user == c.user && (g.folder == c.name || g.folder == clean))
}

/// Takes this copy's files off the client's list (stopping any still under way) and removes what of them
/// arrived in the downloads folder. Only this copy's own files are touched.
fn undo_download(client: &Client, downloads: &Path, c: &Candidate) {
    if let Ok(t) = client.downloads() {
        if let Some(g) = ours(&group(&t), c) {
            for id in &g.all_ids {
                client.cancel(&g.user, id);
            }
        }
    }
    let folder = downloads.join(crate::importer::clean_name(&c.name));
    for f in &c.files {
        let (dir, leaf) = f.filename.rsplit_once(['\\', '/']).unwrap_or(("", &f.filename));
        let disc = dir.rsplit_once(['\\', '/']).map(|(_, d)| d).unwrap_or(dir);
        if !crate::importer::is_folder_name(leaf) {
            continue;
        }
        let mut places = vec![folder.join(leaf)];
        if slskd::is_disc_folder(disc) && crate::importer::is_folder_name(disc) {
            places.push(folder.join(disc).join(leaf));
            places.push(downloads.join(disc).join(leaf));
        }
        for p in places {
            if std::fs::metadata(&p).is_ok_and(|m| m.is_file() && (f.size == 0 || m.len() == f.size)) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    // folders this left empty
    for dir in walk_dirs(&folder).into_iter().rev().chain([folder.clone()]) {
        let _ = std::fs::remove_dir(dir);
    }
}

fn walk_dirs(d: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            out.push(e.path());
            out.extend(walk_dirs(&e.path()));
        }
    }
    out
}

fn waiting_words(g: &Group, transfers: &[slskd::Transfer], c: &Candidate) -> Option<String> {
    let mine: Vec<&slskd::Transfer> = transfers.iter().filter(|t| t.user == c.user && t.folder == g.folder && !t.finished()).collect();
    if g.done_bytes > 0 || g.done > 0 || mine.iter().any(|t| t.state.contains("InProgress")) {
        return None;
    }
    let ahead = mine.iter().map(|t| t.place).filter(|p| *p > 0).min();
    Some(match ahead {
        Some(n) if n > 1 => format!("Waits in line ({} ahead)", n - 1),
        Some(_) => "Next in line".to_string(),
        None if mine.iter().any(|t| t.state.contains("Queued")) => "Waits in line".to_string(),
        None => format!("Asking {}", c.user),
    })
}

/// What saving does, in a few words: "Saving as 320 MP3 with tags and cover".
fn saving_words(ctx: &Ctx, u: &crate::importer::model::Unit) -> String {
    let fates: Vec<Fate> = u.tracks.iter().map(|t| scan::fate(t, ctx.keep_lossless)).collect();
    let how = if fates.contains(&Fate::Mp3) {
        format!("as {} MP3 ", ctx.mp3_quality.label())
    } else if fates.iter().any(|f| matches!(f, Fate::Flac | Fate::StaysFlac)) {
        "as FLAC ".to_string()
    } else {
        String::new()
    };
    let cover = if u.cover.is_some() && u.use_cover { " and cover" } else { "" };
    format!("Saving {how}with tags{cover}")
}


/// What saving needs, checked before anything is downloaded.
struct Ready {
    cfg: crate::config::Config,
    music: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    /// the library guard (a network drive is refused before anything is downloaded)
    guard: crate::guard::Guard,
}

fn ready_to_save(env: &Env) -> Result<Ready, Failure> {
    let cfg = load(env)?;
    let music = PathBuf::from(cfg.music_dir.trim());
    if cfg.music_dir.trim().is_empty() {
        return Err(fail("no_music_folder", "There is no music folder to save into yet."));
    }
    if !music.is_dir() {
        return Err(fail("no_music_folder", format!("The music folder {} could not be found.", music.display())));
    }
    // (refused here, before a download, not once the album is downloaded)
    let guard = crate::guard::Guard::for_library(&music).map_err(|(code, m)| if code == "not_found" { fail("no_music_folder", m) } else { (code, m) })?;
    let ffmpeg = crate::importer::tools::find_tool("ffmpeg").map_err(|m| fail("missing_helper", m))?;
    let ffprobe = crate::importer::tools::find_tool("ffprobe").map_err(|m| fail("missing_helper", m))?;
    Ok(Ready { cfg, music, ffmpeg, ffprobe, guard })
}

/// Downloads a copy (only the files not already kept from an earlier try), then saves it into the music
/// folder with the importer, between Cadence's hold and its release. Cancel works at every stage; once the
/// album is in place it stays.
pub(crate) fn get(out: &Out, job: &Job, id: u64, eng: &Engine, ask: GetAsk) -> Result<Value, Failure> {
    let _use = eng.soulseek.using();
    let ready = ready_to_save(&eng.env)?;
    let c = ask.copy;
    let downloads = eng.soulseek.downloads.clone();
    let kept = find_kept(eng, &c.user, &c.name);
    let missing = missing_files(&downloads, kept.as_ref(), &c);
    let mut said: Option<Value> = None;
    let mut shown = |v: Value| {
        if said.as_ref() != Some(&v) {
            out.progress(id, v.clone());
            said = Some(v);
        }
    };
    let dir = match (&kept, missing.files.is_empty()) {
        // every file is here from an earlier try: nothing to download
        (Some(k), true) => k.dir.clone(),
        _ => {
            let client = eng.soulseek.ready(job)?;
            let _ = std::fs::create_dir_all(&downloads);
            client.enqueue(&missing).map_err(|m| fail("failed", m))?;
            let asked = Instant::now();
            // (what this try brings is all that a stop or a failure removes: files kept from before stay)
            let arrived: Group = loop {
                if job.stopped() {
                    undo_download(&client, &downloads, &missing);
                    return Err(fail("cancelled", "The download was stopped and what had arrived was removed."));
                }
                let transfers = client.downloads().unwrap_or_default();
                let groups = group(&transfers);
                match ours(&groups, &c) {
                    None if asked.elapsed() > SHOW_UP => {
                        undo_download(&client, &downloads, &missing);
                        return Err(fail("failed", format!("The download from {} did not start. Try another copy.", c.user)));
                    }
                    None => shown(json!({ "stage": "waiting", "words": format!("Asking {}", c.user) })),
                    Some(g) if g.finished() && g.failed > 0 => {
                        let (failed, files, refused) = (g.failed, g.files, g.refused);
                        undo_download(&client, &downloads, &missing);
                        let why = if refused > 0 { format!("{} turned the download down", c.user) } else { format!("{} went offline or stopped sharing them", c.user) };
                        return Err(fail("failed", format!("{failed} of the {files} files did not arrive ({why}). Nothing was saved; try another copy.")));
                    }
                    Some(g) if g.finished() => break g.clone(),
                    Some(g) => match waiting_words(g, &transfers, &c) {
                        Some(words) => shown(json!({ "stage": "waiting", "words": words })),
                        None => shown(json!({ "stage": "downloading", "files": g.files, "done_files": g.done, "bytes": g.bytes, "done_bytes": g.done_bytes, "speed": g.speed.max(0.0) as u64 })),
                    },
                }
                std::thread::sleep(LOOK_EVERY);
            };
            if !arrived.parts.is_empty() {
                gather_discs(&downloads, &arrived.folder, &arrived.parts).map_err(|m| fail("failed", m))?;
            }
            // (off the client's list: they are in the downloads folder now)
            for t in &arrived.all_ids {
                client.cancel(&arrived.user, t);
            }
            arrived.folder
        }
    };
    save(out, job, id, eng, &ready, &c, &dir, false, &ask.artist, &ask.album)
}

/// Puts a kept download in anyway: the importer's own path, with every check it makes but its doubt.
pub(crate) fn put_in(out: &Out, job: &Job, id: u64, eng: &Engine, ask: PutAsk) -> Result<Value, Failure> {
    let _use = eng.soulseek.using();
    let ready = ready_to_save(&eng.env)?;
    let (artist, album) = (if ask.artist.is_empty() { ask.kept.artist.clone() } else { ask.artist }, if ask.album.is_empty() { ask.kept.album.clone() } else { ask.album });
    save(out, job, id, eng, &ready, &ask.kept.copy, &ask.kept.dir, true, &artist, &album)
}

/// Saves the download in `<downloads>\<dir>` into the music folder. `anyway`: a copy the importer doubts
/// is put in all the same (a person said so); otherwise it is kept and the answer is `doubtful`. Whatever
/// does not end in the library leaves the download kept for another try.
#[allow(clippy::too_many_arguments)]
fn save(out: &Out, job: &Job, id: u64, eng: &Engine, ready: &Ready, c: &Candidate, dir: &str, anyway: bool, artist: &str, album: &str) -> Result<Value, Failure> {
    let env = &eng.env;
    let cfg = &ready.cfg;
    let downloads = eng.soulseek.downloads.clone();
    let item = downloads.join(dir);
    let keep = |reason: &str, words: &str| {
        let k = Kept { user: c.user.clone(), folder: c.name.clone(), dir: dir.to_string(), copy: c.clone(), artist: artist.to_string(), album: album.to_string(), reason: reason.to_string(), words: words.to_string() };
        change_kept(eng, &c.user, &c.name, Some(k));
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let flag = stop.clone();
        *job.on_stop.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || flag.store(true, Ordering::Relaxed)));
        if job.stopped() {
            stop.store(true, Ordering::Relaxed);
        }
    }
    let mut ctx = Ctx::new_in(downloads.clone(), ready.music.clone(), !env.parts.online, ready.ffmpeg.clone(), ready.ffprobe.clone(), env.dir.join("import-work"));
    ctx.alias_file = env.dir.join("artist-aliases.txt");
    ctx.log_file = env.dir.join("import-log.txt");
    ctx.pending_file = env.dir.join("pending-cleanup.txt");
    ctx.quality_cache = env.dir.join("cache").join("quality-cache.txt");
    ctx.replacements = env.dir.join("replacements.json");
    ctx.loose_wants = env.dir.join("loose-albums.json");
    ctx.lyrics.dir = env.dir.join("cache").join("lyrics");
    ctx.lyrics.timed = !cfg.lyrics_plain;
    ctx.genre_folders = cfg.skip_folders.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && crate::library::is_genre_folder(s)).collect();
    ctx.keep_lossless = cfg.keep_lossless;
    ctx.mp3_quality = cfg.mp3_quality;
    ctx.save_lyrics = cfg.save_lyrics;
    ctx.keep_originals = Some(!cfg.recycle_originals);
    // filed as Organize files it: compilations into their folder, and with Keep the library organized (which
    // needs "Repairs and organizing") straight into Artist  Album (Year), so no second move is needed
    ctx.compilations_folder = cfg.compilations_folder.clone();
    ctx.keep_organized = cfg.keep_organized && cfg.library_changes() == "organize";
    ctx.trash = env.parts.trash;
    ctx.stop = Some(stop.clone());
    ctx.settled.write().unwrap_or_else(|e| e.into_inner()).insert(dir.to_lowercase());
    crate::importer::quality::load_cache(&ctx.quality_cache);
    out.progress(id, json!({ "stage": "saving", "words": "Reading the download", "done": 0, "total": c.tracks }));
    let scanned = scan::build_unit(&ctx, &item);
    crate::importer::quality::save_cache(&ctx.quality_cache);
    const KEPT_TO_TRY_AGAIN: &str = "The download is kept, so getting this copy again carries on from it.";
    let stopped = || {
        keep("stopped", "Saving was stopped.");
        fail("cancelled", format!("Saving was stopped; nothing was added. {KEPT_TO_TRY_AGAIN}"))
    };
    if ctx.stopped() {
        return Err(stopped());
    }
    let mut u = match scanned {
        Ok(u) => u,
        Err(m) => {
            let words = format!("The download could not be read: {m}");
            keep("failed", &words);
            return Err(fail("failed", words));
        }
    };
    if u.action == Action::Exists || u.action == Action::Cleanup {
        if cfg.recycle_originals {
            let _ = (ctx.trash)(&item);
        }
        change_kept(eng, &c.user, &c.name, None);
        let folder = u.existing.clone().or_else(|| scan::find_existing_album(&ctx, &u.artist, &u.folder));
        return Ok(json!({ "result": "already", "folder": folder, "words": "Already in your library" }));
    }
    if !u.include {
        if anyway {
            // (a person looked at why and said to put it in: everything else the importer checks still holds)
            u.include = true;
        } else {
            let mut why: Vec<String> = vec![];
            if !u.note.trim().is_empty() {
                why.push(u.note.trim().to_string());
            }
            why.extend(u.warnings.iter().cloned());
            let why = if why.is_empty() { "The importer would not take this copy as it is.".to_string() } else { why.join(" ") };
            let why = crate::util::plain_err(why);
            keep("doubtful", &why);
            return Err(fail("doubtful", format!("{why} The download is kept: put it in anyway, or forget it.")));
        }
    }
    let words = saving_words(&ctx, &u);
    let total = u.tracks.len();
    let reason = match (album.is_empty(), artist.is_empty()) {
        (false, false) => format!("Saving {album} by {artist}"),
        _ => format!("Saving {} by {}", u.folder, u.artist),
    };
    // the guard again, right before the hold: the drive may have gone while the copy downloaded or was read
    if let Err(f) = ready.guard.still_there() {
        keep("failed", "The music folder went away before saving.");
        return Err(f);
    }
    // one change to the library at a time in the engine: refused, not queued (the download is kept, so
    // saving again carries on from it)
    let writer = match super::library::Writer::take(env) {
        Ok(w) => w,
        Err(f) => {
            keep("failed", super::library::BUSY_WORDS);
            return Err(f);
        }
    };
    let hold = match out.ask("library.hold", json!({ "reason": reason }), env.parts.ask_wait, &|| ctx.stopped()) {
        Ok(v) => {
            super::backup::changing::enter(&env.dir);
            v.get("hold").cloned().unwrap_or(Value::Null)
        }
        Err(_) if ctx.stopped() => return Err(stopped()),
        Err((_, why)) => {
            eprintln!("cadence-engine: no hold for job {id}: {why}");
            keep("failed", "Cadence did not let the import start.");
            return Err(fail("failed", "Cadence did not let the import start; nothing was changed."));
        }
    };
    let last = Mutex::new(None::<usize>);
    let paused = Mutex::new(false);
    let report = |frac: f64| {
        let done = (frac.clamp(0.0, 1.0) * total as f64).round() as usize;
        let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
        if *l != Some(done) {
            *l = Some(done);
            out.progress(id, json!({ "stage": "saving", "words": words, "done": done, "total": total }));
            if let Some(gate) = &env.parts.saving_gate {
                let mut first = paused.lock().unwrap_or_else(|e| e.into_inner());
                if !*first {
                    *first = true;
                    gate.wait();
                }
            }
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let said = Mutex::new(String::new());
        let r = import_unit(
            &ctx,
            &mut u,
            &|t, f| {
                *said.lock().unwrap_or_else(|e| e.into_inner()) = t.to_string();
                report(f)
            },
            None,
            &|| {},
        );
        (r, said.into_inner().unwrap_or_else(|e| e.into_inner()))
    }));
    // always let go of the hold, whatever happened
    super::backup::changing::leave(&env.dir);
    let _ = out.ask("library.release", json!({ "hold": hold }), env.parts.ask_wait, &|| false);
    drop(writer);
    let Ok((result, said)) = result else {
        keep("failed", "The import hit a fault.");
        return Err(fail("failed", "The import hit a fault and stopped. Nothing was added."));
    };
    let folder = scan::find_existing_album(&ctx, &u.artist, &u.folder);
    match result {
        ImportResult::Ok | ImportResult::OkKept => {
            change_kept(eng, &c.user, &c.name, None);
            Ok(json!({ "result": "imported", "folder": folder, "words": "In your library now" }))
        }
        ImportResult::Exists | ImportResult::Cleaned => {
            change_kept(eng, &c.user, &c.name, None);
            Ok(json!({ "result": "already", "folder": folder, "words": "Already in your library" }))
        }
        ImportResult::Failed if ctx.stopped() => Err(stopped()),
        ImportResult::Failed => {
            let words = crate::util::plain_err(said);
            keep("failed", &words);
            Err(fail("failed", words))
        }
    }
}
