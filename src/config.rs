use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub lastfm_user: String,
    /// Kept in Windows Credential Manager (`crate::secrets`), never written to the settings file. It is
    /// still read from the file, once, to carry over a key from before that.
    #[serde(skip_serializing)]
    pub lastfm_api_key: String,
    pub music_dir: String,
    /// Where finished Soulseek downloads arrive (the Downloads page imports from here).
    pub downloads_dir: String,
    pub alias_file: String,
    /// Folder names in the music dir that aren't artists.
    pub skip_folders: String,
    /// How far back to look for new releases.
    pub months_back: u32,
    /// How many of your top artists to check for new releases.
    pub check_artists: usize,
    /// How many of your top artists to ask Last.fm for similar artists.
    pub similar_seeds: usize,
    pub include_albums: bool,
    pub include_eps: bool,
    pub include_singles: bool,
    pub include_live_comp: bool,
    /// Include releases where the artist is only featured / not the first credit.
    pub include_features: bool,
    pub use_itunes: bool,
    /// Hide "similar" artists you've already played at least this many times (0 = never hide).
    pub hide_listened_plays: u64,
    /// How long cached release lists stay fresh.
    pub cache_days: u64,
    /// Start a refresh when the app opens if the last one is over 12 hours old.
    pub auto_refresh: bool,
    /// which release types the New releases tab shows: albums, EPs, singles, other (singles off by default)
    pub view_kinds: [bool; 4],
    /// the page that was open when the app was last used ("home", "recs", "downloads", "library", "health", "backup")
    pub last_page: String,
    pub dismissed: BTreeSet<String>,
    /// artist keys whose new releases are hidden (artist_key form) -> display name
    pub muted: std::collections::BTreeMap<String, String>,
    /// artist key -> the MusicBrainz artist you picked for it (fixes same-name mix-ups)
    pub pins: std::collections::BTreeMap<String, Pin>,
    /// the colors chosen in Settings, "#rrggbb" by role (see `crate::theme`); empty = the first theme
    pub colors: std::collections::BTreeMap<String, String>,
    /// a set of colors kept from Settings as "my theme", in the same form; empty = none kept
    pub my_theme: std::collections::BTreeMap<String, String>,
    /// the first-start screen has been gone through. A settings file from before this was kept has no
    /// such entry and counts as set up; only a start with no settings file at all is a first start
    /// (see `load`).
    pub setup_done: bool,
    /// "Look only, change nothing": Health lists what it finds but repairs nothing. For a library that is
    /// not laid out as artist folders holding album folders, until its owner says the repairs may run.
    pub look_only: bool,
    /// how large everything is drawn: 1.0 is the usual size (Settings, "Look")
    pub size: f32,
    /// the folder that holds the backup script (`Backup-Music.ps1`); empty = where it was when the app was built
    pub backup_tool_dir: String,
    /// lyrics fetched from LRCLIB are saved as plain words only (false: with a time on every line when
    /// LRCLIB has them, which a player that knows such lyrics follows as the song plays)
    pub lyrics_plain: bool,
    /// lossless downloads stay lossless when they are imported: a FLAC stays as it is with its tags and
    /// cover put right, another lossless kind becomes FLAC (false: every lossless download becomes a 320 MP3)
    pub keep_lossless: bool,
    /// what to do about new versions of the app: "auto" (fetch them and put them in), "tell" (only say
    /// that one is out) or "off" (never check); see `crate::update::Policy`
    pub updates: String,
    #[serde(skip)]
    pub load_error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Pin {
    /// your name for the artist (folder / Last.fm name)
    pub artist: String,
    /// MusicBrainz artist id; empty = "none of these", don't use MusicBrainz for this artist
    pub mbid: String,
    /// what you picked, for display ("Lucy (hypnagogic pop artist Cooper B. Handy)")
    pub label: String,
}

impl Config {
    /// The MusicBrainz id to use for an artist: Some("") means "don't use MusicBrainz", None = not pinned.
    pub fn pinned_mbid(&self, artist_key: &str) -> Option<&str> {
        self.pins.get(artist_key).map(|p| p.mbid.as_str())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lastfm_user: String::new(),
            lastfm_api_key: String::new(),
            // (none: the first-start screen asks for it, and a page without one says "Choose a folder")
            music_dir: String::new(),
            downloads_dir: std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join("Downloads").join("SoulSeek").to_string_lossy().into_owned(),
            alias_file: app_dir().join("artist-aliases.txt").to_string_lossy().into_owned(),
            skip_folders: "Automatically Add to Apple Music, Compilations, Various Artists, Unknown Artist".into(),
            months_back: 9,
            check_artists: 150,
            similar_seeds: 60,
            include_albums: true,
            include_eps: true,
            include_singles: true,
            include_live_comp: false,
            include_features: false,
            use_itunes: true,
            hide_listened_plays: 30,
            cache_days: 3,
            auto_refresh: true,
            view_kinds: [true, true, false, true],
            last_page: String::new(),
            dismissed: BTreeSet::new(),
            muted: Default::default(),
            pins: Default::default(),
            colors: Default::default(),
            my_theme: Default::default(),
            // (true, so that a settings file from before, which has no such entry, counts as set up)
            setup_done: true,
            look_only: false,
            size: 1.0,
            backup_tool_dir: String::new(),
            lyrics_plain: false,
            keep_lossless: false,
            updates: "auto".into(),
            load_error: None,
        }
    }
}

/// Where settings, cache and results live. `MUSIC_RECS_DATA` points it elsewhere
/// (used for testing, so tests never touch your real settings).
pub fn app_dir() -> PathBuf {
    let d = match std::env::var_os("MUSIC_RECS_DATA") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("music-recs"),
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Where a test run keeps the window's place and size: inside its own data folder (`data` is what
/// `MUSIC_RECS_DATA` says), so a test can never change how the real app opens. None for the real app,
/// which keeps them where the window toolkit puts them (`%APPDATA%\<app name>\data\app.ron`).
pub fn window_state_file(data: Option<std::ffi::OsString>) -> Option<PathBuf> {
    data.filter(|p| !p.is_empty()).map(|p| PathBuf::from(p).join("app.ron"))
}

/// Where what can be worked out again is kept (saved audio checks, tags read, lookups).
pub fn cache_dir() -> PathBuf {
    let d = app_dir().join("cache");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

/// A short history of settings loads/saves (never the API key itself), so if the login ever
/// goes missing again there's a record of when and how.
pub fn log(msg: &str) {
    use std::io::Write;
    let p = app_dir().join("settings.log");
    if std::fs::metadata(&p).map(|m| m.len() > 200_000).unwrap_or(false) {
        let _ = std::fs::rename(&p, p.with_extension("log.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{} [pid {}] {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), std::process::id());
    }
}

/// Pull the Last.fm login out of a settings file that won't parse as a whole.
fn salvage_login(raw: &str) -> (String, String) {
    let grab = |field: &str| {
        regex::Regex::new(&format!(r#""{field}"\s*:\s*"([^"\\]*)""#))
            .ok()
            .and_then(|re| re.captures(raw).map(|c| c[1].to_string()))
            .unwrap_or_default()
    };
    (grab("lastfm_user"), grab("lastfm_api_key"))
}

/// A settings text with the Last.fm key's value taken out and every other byte as it was. None when it
/// holds no key.
fn without_key(raw: &str) -> Option<String> {
    let re = regex::Regex::new(r#"("lastfm_api_key"\s*:\s*")[^"\\]+(")"#).ok()?;
    re.is_match(raw).then(|| re.replace_all(raw, "${1}${2}").into_owned())
}

/// The one-time carry-over: a key found in the settings file goes into Credential Manager, is read back
/// from there, and only then leaves the file and the copies of it the app or its tools have made. If it
/// cannot be kept there, every file stays as it was.
fn move_key_out(path: &std::path::Path, key: &str) -> bool {
    if crate::secrets::set(crate::secrets::LASTFM, "", key).is_err() {
        log("load: the Last.fm key could not be moved to Credential Manager; it stays in the settings file");
        return false;
    }
    for file in [path.to_path_buf(), path.with_extension("json.bad"), path.with_extension("json.before-clean-app")] {
        let Some(clean) = std::fs::read_to_string(&file).ok().and_then(|raw| without_key(&raw)) else { continue };
        let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match crate::util::write_atomic(&file, clean.as_bytes()) {
            Ok(()) => log(&format!("load: the Last.fm key is in Credential Manager now and was taken out of {name}")),
            Err(e) => log(&format!("load: the Last.fm key could not be taken out of {name} ({e})")),
        }
    }
    true
}

fn describe(c: &Config) -> String {
    format!("user={:?} key={}", c.lastfm_user, if c.lastfm_api_key.is_empty() { "empty" } else { "set" })
}

impl Config {
    /// The settings of a PC that has never run the app: the defaults, and the first-start screen still
    /// to be gone through (it takes the window until then).
    pub fn first_start() -> Self {
        Config { setup_done: false, ..Config::default() }
    }

    pub fn load() -> Self {
        let path = config_path();
        let mut c: Config = match std::fs::read(&path) {
            Err(_) => {
                log(&format!("load: no settings file at {}, using defaults", path.display()));
                Config::first_start()
            }
            Ok(b) => match serde_json::from_slice::<Config>(&b) {
                Ok(c) => {
                    log(&format!("load: ok, {}", describe(&c)));
                    c
                }
                Err(e) => {
                    // Keep the broken file for repair, and rescue the login from it if possible.
                    let bad = path.with_extension("json.bad");
                    let _ = std::fs::copy(&path, &bad);
                    let (user, key) = salvage_login(&String::from_utf8_lossy(&b));
                    let rescued = !user.is_empty() && !key.is_empty();
                    let msg = crate::util::plain_err(format!(
                        "Settings file was damaged ({e}). Started with default settings{}; the old file is saved as {}.",
                        if rescued { " but kept your Last.fm login" } else { "" },
                        bad.display()
                    ));
                    log(&format!("load: damaged ({e}); login rescued: {rescued}"));
                    Config { lastfm_user: user, lastfm_api_key: key, load_error: Some(msg), ..Default::default() }
                }
            },
        };
        // the key: from Credential Manager; one still in the file (from before, or from a restored backup)
        // is moved there first
        if !c.lastfm_api_key.trim().is_empty() {
            move_key_out(&path, &c.lastfm_api_key);
        } else if let Some(kept) = crate::secrets::get(crate::secrets::LASTFM) {
            c.lastfm_api_key = kept.secret;
        }
        // env vars fill in anything not set in the app
        if c.lastfm_user.is_empty() {
            c.lastfm_user = std::env::var("LASTFM_USER").unwrap_or_default();
        }
        if c.lastfm_api_key.is_empty() {
            c.lastfm_api_key = std::env::var("LASTFM_API_KEY").unwrap_or_default();
        }
        c
    }
    /// Save settings. A blank Last.fm login never overwrites one that's already on disk: blank
    /// fields here almost always mean this copy of the settings was loaded wrong, not that you
    /// wanted to log out. (To change account, type the new username/key over the old ones.)
    ///
    /// The key goes to Credential Manager, not into the file. Only if Credential Manager refuses it is it
    /// written to the file as it used to be, so it is never lost.
    pub fn save(&self) {
        let mut out = self.clone();
        let (user, key_on_disk) = std::fs::read_to_string(config_path()).map(|raw| salvage_login(&raw)).unwrap_or_default();
        if out.lastfm_user.trim().is_empty() && !user.is_empty() {
            out.lastfm_user = user;
            log("save: kept the Last.fm username already on disk instead of blanking it");
        }
        // a blank key here leaves the kept one alone, wherever it is
        let in_file = if out.lastfm_api_key.trim().is_empty() {
            out.lastfm_api_key = crate::secrets::get(crate::secrets::LASTFM).map(|e| e.secret).unwrap_or_default();
            key_on_disk
        } else if crate::secrets::set(crate::secrets::LASTFM, "", &out.lastfm_api_key).is_ok() {
            String::new()
        } else {
            log("save: Credential Manager did not keep the Last.fm key; it is in the settings file");
            out.lastfm_api_key.clone()
        };
        if out.lastfm_api_key.trim().is_empty() {
            out.lastfm_api_key = in_file.clone();
        }
        let json = serde_json::to_value(&out).map(|mut v| {
            if !in_file.is_empty() {
                v["lastfm_api_key"] = serde_json::Value::String(in_file.clone());
            }
            v
        });
        if let Ok(s) = json.and_then(|v| serde_json::to_vec_pretty(&v)) {
            match crate::util::write_atomic(&config_path(), &s) {
                Ok(()) => log(&format!("save: ok, {}", describe(&out))),
                Err(e) => log(&format!("save: FAILED ({e})")),
            }
        }
    }
    pub fn has_lastfm(&self) -> bool {
        !self.lastfm_user.trim().is_empty() && !self.lastfm_api_key.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_run_keeps_its_own_window_place_and_size() {
        assert_eq!(window_state_file(Some(r"X:\lab\data".into())), Some(PathBuf::from(r"X:\lab\data\app.ron")));
        // the real app: nothing is said, so the window toolkit's own place is used
        assert_eq!(window_state_file(None), None);
        assert_eq!(window_state_file(Some("".into())), None);
    }

    #[test]
    fn only_a_pc_that_never_ran_the_app_gets_the_first_start_screen() {
        // no settings file: nothing is assumed about the PC, and the screen is still to come
        let fresh = Config::first_start();
        assert!(!fresh.setup_done && fresh.music_dir.is_empty());
        assert!(!fresh.skip_folders.contains("Dubstep") && fresh.skip_folders.contains("Various Artists"));
        // a settings file from before these entries existed: it counts as set up, lossless downloads are
        // still converted, repairs still run, and what it holds is kept
        let before: Config = serde_json::from_str(r#"{ "music_dir": "X:\\Music", "skip_folders": "Compilations, Dubstep", "backup_tool_dir": "X:\\tools\\music-backup" }"#).unwrap();
        assert!(before.setup_done && !before.keep_lossless && !before.look_only);
        assert_eq!((before.music_dir.as_str(), before.skip_folders.as_str()), (r"X:\Music", "Compilations, Dubstep"));
        // and what the screen chose comes back from the file
        let chosen: Config = serde_json::from_str(&serde_json::to_string(&Config { keep_lossless: true, look_only: true, ..Config::first_start() }).unwrap()).unwrap();
        assert!(!chosen.setup_done && chosen.keep_lossless && chosen.look_only);
    }

    #[test]
    fn login_is_never_lost() {
        // everything here happens in a throwaway folder, never the real settings
        let dir = std::env::temp_dir().join(format!("music-recs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("MUSIC_RECS_DATA", &dir) };
        let path = dir.join("config.json");

        use crate::secrets::{self, LASTFM};
        let kept = || secrets::get(LASTFM).map(|e| e.secret).unwrap_or_default();
        // no file in the folder holds the key (made up, like every secret in a test)
        let nowhere = |key: &str| std::fs::read_dir(&dir).unwrap().flatten().all(|f| !std::fs::read_to_string(f.path()).unwrap_or_default().contains(key));
        secrets::remove(LASTFM);

        // a normal save + load round-trips the login, and the key is kept outside the file
        let c = Config { lastfm_user: "someone".into(), lastfm_api_key: "abc123".into(), ..Default::default() };
        c.save();
        let back = Config::load();
        assert_eq!((back.lastfm_user.as_str(), back.lastfm_api_key.as_str()), ("someone", "abc123"));
        assert!(kept() == "abc123" && nowhere("abc123"));

        // a copy of the settings with a blank login can't wipe the saved one
        Config { months_back: 4, ..Default::default() }.save();
        let after = Config::load();
        assert_eq!((after.lastfm_user.as_str(), after.lastfm_api_key.as_str(), after.months_back), ("someone", "abc123", 4));
        assert!(nowhere("abc123"));

        // a settings file from before, with the key in it, and the two copies of such a file that may lie
        // beside it: the key is carried over and leaves all three, and nothing else in them changes
        secrets::remove(LASTFM);
        let old = r#"{ "lastfm_user": "someone", "lastfm_api_key": "old-key-456", "months_back": 7 }"#;
        for name in ["config.json", "config.json.bad", "config.json.before-clean-app"] {
            std::fs::write(dir.join(name), old).unwrap();
        }
        let moved = Config::load();
        assert_eq!((moved.lastfm_api_key.as_str(), moved.months_back, kept().as_str()), ("old-key-456", 7, "old-key-456"));
        assert!(nowhere("old-key-456"));
        for name in ["config.json", "config.json.bad", "config.json.before-clean-app"] {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), old.replace("old-key-456", ""));
        }
        // the next start finds it where it now is
        assert_eq!(Config::load().lastfm_api_key, "old-key-456");

        // when the key cannot be kept there, the file stays exactly as it was and the key still works
        secrets::remove(LASTFM);
        std::fs::write(&path, old).unwrap();
        secrets::REFUSE.with(|r| r.set(true));
        let stuck = Config::load();
        assert_eq!(stuck.lastfm_api_key, "old-key-456");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        // and a save then keeps it in the file, as before, instead of losing it
        stuck.save();
        assert!(std::fs::read_to_string(&path).unwrap().contains("old-key-456"));
        secrets::REFUSE.with(|r| r.set(false));
        assert_eq!(Config::load().lastfm_api_key, "old-key-456");
        assert!(kept() == "old-key-456" && nowhere("old-key-456"));

        // a damaged file: defaults, but the login is rescued and the original kept, without the key
        std::fs::write(&path, r#"{ "lastfm_user": "someone", "lastfm_api_key": "abc123", "music_dir": "Z:\Music" }"#).unwrap();
        let damaged = Config::load();
        assert!(damaged.load_error.is_some());
        assert_eq!((damaged.lastfm_user.as_str(), damaged.lastfm_api_key.as_str()), ("someone", "abc123"));
        assert!(dir.join("config.json.bad").exists());
        assert!(kept() == "abc123" && nowhere("abc123"));

        // and the log never contains a key
        let log = std::fs::read_to_string(dir.join("settings.log")).unwrap();
        assert!(log.contains("load: ok") && log.contains("save: ok") && !log.contains("abc123") && !log.contains("old-key-456"));
        secrets::remove(LASTFM);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
