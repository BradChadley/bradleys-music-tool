//! Shared settings, the artist alias list, the log, and the "leftover downloads" list.
use crate::importer::lookup::AppleCat;
use crate::importer::util::{group_by_key, norm, read_dir_sorted};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = crate::net::USER_AGENT;

pub struct Ctx {
    pub source: PathBuf,
    pub library: PathBuf,
    pub no_online: bool,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub work_dir: PathBuf,
    pub alias_file: PathBuf,
    pub log_file: PathBuf,
    pub pending_file: PathBuf,
    pub quality_cache: PathBuf,
    pub quality_threads: usize,
    /// where a replaced album's old copy is kept (see `crate::undo`)
    pub repair_root: PathBuf,
    /// the albums Health asked a better copy for (see `crate::replace`)
    pub replacements: PathBuf,
    /// the albums asked for because loose tracks of theirs lie in the library (see `crate::loose`)
    pub loose_wants: PathBuf,
    /// how a loose track whose album has arrived leaves the library: to the Windows Recycle Bin (a test's
    /// is a folder of its own)
    pub recycle: fn(&std::path::Path) -> Result<(), String>,
    pub aliases: RwLock<HashMap<String, String>>,
    /// download folders (lowercase names) the app fetched itself and knows are complete
    pub settled: RwLock<std::collections::HashSet<String>>,
    pub existing_artists: RwLock<HashMap<String, String>>,
    /// one timer per site, shared by all the parallel lookups, so together they stay under each site's rate limit
    gate: Mutex<HashMap<&'static str, Instant>>,
    pub apple_cache: Mutex<HashMap<String, Arc<OnceLock<Arc<AppleCat>>>>>,
    /// track lists already fetched this run, by Apple's album number
    pub apple_tracks: Mutex<HashMap<u64, Vec<crate::importer::lookup::AppleTrack>>>,
    /// where online answers are kept between runs (None = they are not: the importer asks afresh)
    pub lookup_cache: Option<PathBuf>,
    /// folders in the library that hold artists, not albums ("Dubstep"): from the Settings list of folders
    /// that are not artists
    pub genre_folders: Vec<String>,
    /// how many lookups got no answer at all (the site or the connection was down): "nothing found" after
    /// one of these is not a fact about the album
    pub net_failures: std::sync::atomic::AtomicUsize,
    pub api: ureq::Agent,
    pub dl: ureq::Agent,
    /// how LRCLIB is asked for lyrics, and where its answers are kept
    pub lyrics: crate::importer::lyrics::Lookup,
    /// lossless downloads stay lossless (a FLAC as it is, any other kind as FLAC) instead of becoming
    /// 320 kbps MP3s: the choice in Settings
    pub keep_lossless: bool,
    /// pending-cleanup.txt is read, changed and rewritten, so one album at a time
    pending_lock: Mutex<()>,
}

impl Ctx {
    pub fn new(source: PathBuf, library: PathBuf, no_online: bool, ffmpeg: PathBuf, ffprobe: PathBuf) -> Self {
        // the alias list, the import log and the list of leftover downloads are in the app's data folder, the
        // saved audio checks and the uploader signatures in its cache folder
        let data_dir = crate::config::app_dir();
        let work_dir = std::env::temp_dir().join("music-recs-import");
        let _ = std::fs::create_dir_all(&work_dir);
        let cache_dir = crate::config::cache_dir();
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let mk = |secs: u64| -> ureq::Agent {
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(secs)))
                .http_status_as_error(false)
                .https_only(true) // never plain http, a redirect included
                .user_agent(USER_AGENT)
                .build()
                .into()
        };
        // a test run on copies (`--library <dir>`) keeps its undo backups beside that copy, never at the top of a drive
        let repair_root = match std::env::args().any(|a| a == "--library") {
            true => library.parent().unwrap_or(&library).join(".music-repair-backups"),
            false => crate::undo::root_for(&library),
        };
        let c = Ctx {
            repair_root,
            replacements: crate::replace::file(),
            loose_wants: crate::loose::file(),
            recycle: crate::undo::to_recycle_bin,
            source,
            library,
            no_online,
            ffmpeg,
            ffprobe,
            work_dir,
            alias_file: data_dir.join("artist-aliases.txt"),
            log_file: data_dir.join("import-log.txt"),
            pending_file: data_dir.join("pending-cleanup.txt"),
            quality_cache: cache_dir.join("quality-cache.txt"),
            quality_threads: (cores / 3).max(2),
            aliases: RwLock::new(HashMap::new()),
            settled: RwLock::new(Default::default()),
            existing_artists: RwLock::new(HashMap::new()),
            gate: Mutex::new(HashMap::new()),
            apple_cache: Mutex::new(HashMap::new()),
            apple_tracks: Mutex::new(HashMap::new()),
            lookup_cache: None,
            genre_folders: vec![],
            net_failures: Default::default(),
            lyrics: crate::importer::lyrics::Lookup { dir: cache_dir.join("lyrics"), online: !no_online, timed: true, agent: mk(20) },
            keep_lossless: false,
            api: mk(20),
            dl: mk(90),
            pending_lock: Mutex::new(()),
        };
        c.load_aliases();
        c.refresh_existing_artists();
        // uploader signatures the Health check learned from your library, so new downloads with the same junk get cleaned
        crate::importer::rules::load_signatures(&cache_dir.join("junk-signatures.txt"));
        c
    }

    pub fn log(&self, msg: &str) {
        let line = format!("{}  {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log_file) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    // ---------- aliases: "tag artist = folder artist", one per line ----------
    fn load_aliases(&self) {
        let mut map = self.aliases.write().unwrap();
        map.clear();
        let Ok(text) = std::fs::read_to_string(&self.alias_file) else { return };
        for l in text.lines() {
            let t = l.trim_start();
            if t.starts_with('#') || !l.contains('=') {
                continue;
            }
            if let Some((k, v)) = l.split_once('=') {
                map.insert(norm(k), v.trim().to_string());
            }
        }
    }

    pub fn save_alias(&self, from: &str, to: &str) {
        if norm(from) == norm(to) {
            return;
        }
        // `from` is tag text from a stranger's file. A line break, "=" or a leading "#" in it would write extra
        // or broken lines into the alias file, which every page reads.
        let unsafe_text = |s: &str| s.trim().is_empty() || s.contains('=') || s.trim_start().starts_with('#') || s.chars().any(|c| c.is_control());
        if unsafe_text(from) || unsafe_text(to) {
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.alias_file) {
            let _ = writeln!(f, "{from} = {to}");
        }
        self.aliases.write().unwrap().insert(norm(from), to.to_string());
    }

    pub fn is_alias(&self, name: &str) -> bool {
        self.aliases.read().unwrap().contains_key(&norm(name))
    }

    pub fn refresh_existing_artists(&self) {
        let mut map = self.existing_artists.write().unwrap();
        map.clear();
        for e in read_dir_sorted(&self.library) {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = e.file_name().to_string_lossy().to_string();
                map.insert(norm(&name), name);
            }
        }
    }

    /// Alias file first, then an artist folder you already have, then the most common spelling.
    pub fn resolve_artist(&self, candidates: &[String]) -> Option<String> {
        {
            let aliases = self.aliases.read().unwrap();
            for c in candidates.iter().filter(|c| !c.is_empty()) {
                if let Some(a) = aliases.get(&norm(c)) {
                    return Some(a.clone());
                }
            }
        }
        {
            let ex = self.existing_artists.read().unwrap();
            for c in candidates.iter().filter(|c| !c.is_empty()) {
                if let Some(a) = ex.get(&norm(c)) {
                    return Some(a.clone());
                }
            }
        }
        let groups = group_by_key(candidates.iter().filter(|c| !c.is_empty()).cloned().collect(), |s| s.clone());
        let mut best: Option<&(String, Vec<String>)> = None;
        for g in &groups {
            if best.map(|b| g.1.len() > b.1.len()).unwrap_or(true) {
                best = Some(g);
            }
        }
        best.map(|b| b.0.clone())
    }

    // ---------- downloads Soulseek still had open ----------
    pub fn get_pending(&self) -> Vec<String> {
        std::fs::read_to_string(&self.pending_file).map(|t| t.lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect()).unwrap_or_default()
    }
    pub fn set_pending(&self, path: &str, on: bool) {
        let _lock = self.pending_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<String> = self.get_pending().into_iter().filter(|p| p != path).collect();
        if on {
            list.push(path.to_string());
        }
        if list.is_empty() {
            let _ = std::fs::remove_file(&self.pending_file);
        } else {
            let _ = std::fs::write(&self.pending_file, list.join("\r\n") + "\r\n");
        }
    }

    // ---------- rate limits ----------
    /// Books the next free slot for this site and waits for it.
    pub fn wait_turn(&self, key: &'static str, gap: f64) {
        let wait = {
            let mut g = self.gate.lock().unwrap();
            let now = Instant::now();
            let next = match g.get(key) {
                Some(t) if *t > now => *t,
                _ => now,
            };
            g.insert(key, next + Duration::from_secs_f64(gap));
            next.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}
