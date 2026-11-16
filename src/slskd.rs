//! Talks to slskd, the Soulseek client that runs without a window.
//!
//! slskd lives in the app's data folder (`slskd\bin\slskd.exe`), with its settings in `slskd\data\slskd.yml`.
//! It only listens on this computer (127.0.0.1) and only answers requests that carry the key from that
//! settings file. The Soulseek login is kept in Windows Credential Manager (`crate::secrets`) and handed to
//! slskd when the app starts it; a login found in the settings file from before is moved there once. The
//! password is never shown, logged or written to a file by this code.
//!
//! Everything slskd sends back (search results come from strangers) is read leniently and size-capped.
use serde_json::{json, Value};
use std::hash::{BuildHasher, Hasher};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// CREATE_NO_WINDOW
const NO_WINDOW: u32 = 0x0800_0000;
const AUDIO: [&str; 9] = ["mp3", "flac", "m4a", "ogg", "opus", "wav", "aac", "ape", "wv"];

/// Whether a file of this kind (its extension, lower case) is one of the audio files a copy is made of.
pub fn is_audio_ext(ext: &str) -> bool {
    AUDIO.contains(&ext)
}
const PICTURE: [&str; 3] = ["jpg", "jpeg", "png"];

pub fn root() -> PathBuf {
    crate::config::app_dir().join("slskd")
}
pub fn exe() -> PathBuf {
    root().join("bin").join("slskd.exe")
}
pub fn data_dir() -> PathBuf {
    root().join("data")
}
pub fn settings_file() -> PathBuf {
    data_dir().join("slskd.yml")
}

// ------------------------------------------------------------------ "search for this" from any page

static WANTED: Mutex<Option<String>> = Mutex::new(None);

/// Any page can ask for a Soulseek search ("Artist Album"). The app switches to Downloads and runs it.
/// This only searches: nothing is downloaded until a result is clicked.
pub fn ask_for(text: &str) {
    *WANTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.trim().to_string());
}
pub fn take_asked() -> Option<String> {
    WANTED.lock().unwrap_or_else(|e| e.into_inner()).take()
}

// ------------------------------------------------------------------ the settings file

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub port: u16,
    /// the port it takes Soulseek connections on (`soulseek: listen_port`; slskd's own default when absent)
    pub listen_port: u16,
    pub key: String,
    /// both a user name and a password have been typed in
    pub has_login: bool,
    /// changes whenever the login changes (a hash, never the login itself), so slskd is restarted with the new one
    pub login_mark: u64,
}

static OPEN_SETTINGS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A page asks for the Settings panel to be opened (for the Soulseek login).
pub fn ask_open_settings() {
    OPEN_SETTINGS.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn take_open_settings() -> bool {
    OPEN_SETTINGS.swap(false, std::sync::atomic::Ordering::Relaxed)
}

fn yaml_unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        unquote(v).to_string()
    }
}

/// Whether a name and password can be kept and handed to slskd.
fn check_login(user: &str, pass: &str) -> Result<(), String> {
    let bad = |s: &str| s.chars().any(|c| c.is_control());
    if user.trim().is_empty() || pass.is_empty() {
        return Err("Type both a name and a password.".into());
    }
    if bad(user) || bad(pass) || user.chars().count() > 100 || pass.chars().count() > 200 {
        return Err("The name or password has characters that can't be saved.".into());
    }
    Ok(())
}

/// The settings text without the Soulseek name and password lines, and the login those lines held (when
/// both were filled in). Every other line is left as it was. The lines are taken out whole, not emptied:
/// slskd reads its settings file after what the app hands it at start, so an empty line there would win.
pub fn without_login(text: &str) -> (String, Option<(String, String)>) {
    let (mut out, mut section, mut user, mut pass) = (Vec::<&str>::new(), String::new(), String::new(), String::new());
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let is_note = line.trim_start().starts_with('#') || line.trim().is_empty();
        if !is_note && indent == 0 {
            section = line.trim().split(':').next().unwrap_or("").trim().to_string();
        } else if !is_note && section == "soulseek" && indent == 2 {
            if let Some((k, v)) = line.trim().split_once(':') {
                match k.trim() {
                    "username" => {
                        user = yaml_unquote(v);
                        continue;
                    }
                    "password" => {
                        pass = yaml_unquote(v);
                        continue;
                    }
                    _ => {}
                }
            }
        }
        out.push(line);
    }
    let clean = if out.is_empty() { String::new() } else { out.join("\n") + "\n" };
    (clean, (!user.trim().is_empty() && !pass.is_empty()).then_some((user.trim().to_string(), pass)))
}

/// Bumped whenever the kept login changes, so the Downloads page looks again and restarts slskd with it.
static LOGIN_STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn login_stamp() -> u64 {
    LOGIN_STAMP.load(std::sync::atomic::Ordering::Relaxed)
}

/// The settings text without the web sign-in name and password and without any API key (`web`,
/// `authentication`: its `username` and `password` lines and the whole `api_keys` block). The app makes
/// new ones each time it starts and hands them to slskd, and a line left in the file would win over those.
pub fn without_web_secrets(text: &str) -> String {
    let (mut out, mut section, mut in_auth, mut in_keys) = (Vec::<&str>::new(), String::new(), false, false);
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let is_note = line.trim_start().starts_with('#') || line.trim().is_empty();
        if !is_note {
            let key = line.trim().split(':').next().unwrap_or("").trim();
            if indent == 0 {
                (section, in_auth, in_keys) = (key.to_string(), false, false);
            } else if section == "web" && indent == 2 {
                (in_auth, in_keys) = (key == "authentication", false);
            } else if in_auth && indent == 4 {
                in_keys = key == "api_keys";
                if in_keys || key == "username" || key == "password" {
                    continue;
                }
            } else if in_keys && indent > 4 {
                continue;
            }
        } else if in_keys {
            continue; // a note inside the block of keys goes with it
        }
        out.push(line);
    }
    if out.is_empty() { String::new() } else { out.join("\n") + "\n" }
}

/// Takes the old web sign-in and API key out of slskd's settings file (once; after that there is nothing
/// to take). Returns the text the file now holds.
fn settle_web(path: &std::path::Path, text: String) -> String {
    let clean = without_web_secrets(&text);
    if clean == text {
        return text;
    }
    match crate::util::write_atomic(path, clean.as_bytes()) {
        Ok(()) => {
            crate::config::log("soulseek: the client's old web password and key were taken out of its settings file; the app makes new ones at each start");
            clean
        }
        Err(e) => {
            crate::config::log(&format!("soulseek: the client's old web password and key could not be taken out of its settings file ({e})"));
            text
        }
    }
}

/// The key the app and slskd share for this run of the app: made new at each start, handed to slskd when
/// it is started, kept only in memory.
fn session_key() -> &'static str {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| crate::secrets::random_hex(24))
}

/// The one-time carry-over, and the tidy-up after a login is saved: a Soulseek login found in slskd's
/// settings file goes into Credential Manager, is read back from there, and only then leaves the file.
/// Returns the text the file now holds. If the login cannot be kept there, or the file cannot be written,
/// the file stays as it was (and slskd goes on reading the login from it).
fn settle_login(path: &std::path::Path, text: String) -> String {
    let (clean, found) = without_login(&text);
    if clean == text {
        return text;
    }
    if let Some((user, pass)) = &found {
        // a login already kept wins over one in the file only when it was saved after the file was written:
        // that cannot be told, so the file's login is taken, as the newest thing a person or a restore put there
        if crate::secrets::set(crate::secrets::SOULSEEK, user, pass).is_err() {
            crate::config::log("soulseek: the login could not be moved to Credential Manager; it stays in the client's settings file");
            return text;
        }
    }
    match crate::util::write_atomic(path, clean.as_bytes()) {
        Ok(()) => {
            crate::config::log(if found.is_some() { "soulseek: the login is in Credential Manager now and was taken out of the client's settings file" } else { "soulseek: empty login lines were taken out of the client's settings file" });
            LOGIN_STAMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            clean
        }
        Err(e) => {
            crate::config::log(&format!("soulseek: the login could not be taken out of the client's settings file ({e})"));
            text
        }
    }
}

/// Keeps the Soulseek login in Credential Manager (the only place it is kept). slskd is given it when the
/// app starts it (`start_env`); it is never written to slskd's settings file.
pub fn save_login(user: &str, pass: &str) -> Result<(), String> {
    save_login_at(&settings_file(), wanted_downloads().as_deref(), user, pass)
}

/// `save_login` for a settings file at `path`. On a PC that has never had the client there is no such
/// file yet: it is written first, as fetching the client would write it (`downloads` is the folder it
/// names), so a login can be saved before the client is there.
fn save_login_at(path: &std::path::Path, downloads: Option<&std::path::Path>, user: &str, pass: &str) -> Result<(), String> {
    check_login(user, pass)?;
    crate::helpers::write_settings(path, downloads)?;
    let text = std::fs::read_to_string(path).map_err(|_| "The Soulseek client's settings file is missing.".to_string())?;
    crate::secrets::set(crate::secrets::SOULSEEK, user.trim(), pass)?;
    // a login still in the file would win over the new one: it has to leave the file
    let (clean, _) = without_login(&text);
    if clean != text {
        crate::util::write_atomic(path, clean.as_bytes()).map_err(|e| crate::util::plain_err(format!("The old login could not be taken out of the Soulseek client's settings file: {e}")))?;
    }
    LOGIN_STAMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// The saved Soulseek name (not the password), to show in Settings.
pub fn login_name() -> Option<String> {
    if let Some(kept) = crate::secrets::get(crate::secrets::SOULSEEK) {
        return Some(kept.user).filter(|s| !s.is_empty());
    }
    without_login(&std::fs::read_to_string(settings_file()).ok()?).1.map(|(user, _)| user)
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    let s = s.split(" #").next().unwrap_or(s).trim();
    s.trim_matches(|c| c == '"' || c == '\'')
}

/// Reads what the app needs from slskd.yml: the web port, and whether a login is still in the file (the
/// password itself is never kept). The key is not read from the file: it is the app's own for this run.
///
/// A file that would open slskd up is refused, so the app never starts it like that: settings that can be
/// changed over its web side, or a web side that asks for no sign-in.
pub fn parse_settings(text: &str) -> Result<Settings, String> {
    let (mut section, mut port, mut user, mut pass, mut in_auth) = (String::new(), None, false, false, false);
    let mut listen: Option<u16> = None;
    let mut mark = std::collections::hash_map::DefaultHasher::new();
    let yes = |v: &str| matches!(v.to_lowercase().as_str(), "true" | "yes" | "on" | "y" | "1");
    let sharing = || Err("The Soulseek client's settings file shares folders with other Soulseek users. The app shares nothing, so it does not start it like that. Empty \"shares: directories\" in slskd.yml.".to_string());
    // under "shares:", "directories:" with a folder after it or listed below it ("- 'D:\Music'")
    let mut in_share_dirs = false;
    for line in text.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if in_share_dirs && indent >= 2 && line.trim_start().starts_with('-') {
            return sharing();
        }
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        if section == "shares" && indent == 2 {
            in_share_dirs = k.trim() == "directories";
            let v = unquote(v);
            if in_share_dirs && !v.is_empty() && v != "[]" {
                return sharing();
            }
        } else if indent <= 2 {
            in_share_dirs = false;
        }
        if indent == 0 {
            section = k.trim().to_string();
            in_auth = false;
            if section == "remote_configuration" && yes(unquote(v)) {
                return Err("The Soulseek client's settings file lets its settings be changed from outside (remote_configuration). The app does not start it like that.".into());
            }
            continue;
        }
        let v = unquote(v);
        if section == "web" && indent == 2 {
            in_auth = k.trim() == "authentication";
        }
        if in_auth && indent == 4 && k.trim() == "disabled" && yes(v) {
            return Err("The Soulseek client's settings file turns off the sign-in on its web side. The app does not start it like that.".into());
        }
        match (section.as_str(), k.trim()) {
            ("soulseek", "username") if indent == 2 => {
                user = !v.is_empty();
                mark.write(line.as_bytes());
            }
            ("soulseek", "password") if indent == 2 => {
                pass = !v.is_empty();
                mark.write(line.as_bytes());
            }
            ("web", "port") if indent == 2 => port = v.parse::<u16>().ok(),
            ("soulseek", "listen_port") if indent == 2 => listen = v.parse::<u16>().ok(),
            _ => {}
        }
    }
    let port = port.ok_or("The slskd settings file has no web port.")?;
    Ok(Settings { port, listen_port: listen.unwrap_or(LISTEN_DEFAULT), key: session_key().to_string(), has_login: user && pass, login_mark: mark.finish() })
}

pub fn read_settings() -> Result<Settings, String> {
    let path = settings_file();
    let text = std::fs::read_to_string(&path).map_err(|_| "The slskd settings file is missing.".to_string())?;
    let mut set = parse_settings(&settle_login(&path, settle_web(&path, text)))?;
    // the login is the one kept in Credential Manager; one still in the file (it could not be moved) counts too
    if let Some(kept) = crate::secrets::get(crate::secrets::SOULSEEK) {
        let mut mark = std::collections::hash_map::DefaultHasher::new();
        mark.write(kept.user.as_bytes());
        mark.write(&[0]);
        mark.write(kept.secret.as_bytes());
        (set.has_login, set.login_mark) = (true, mark.finish());
    }
    Ok(set)
}

// ------------------------------------------------------------------ what slskd tells us

#[derive(Clone, Default, PartialEq)]
pub struct ServerState {
    pub connected: bool,
    pub logged_in: bool,
    pub state: String,
}

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RemoteFile {
    /// the full path on the other person's computer, exactly as slskd gave it (needed to ask for the file)
    pub filename: String,
    pub size: u64,
    /// kilobits a second, and length in seconds, when the other person's client says (0 when not)
    #[serde(default)]
    pub bitrate: u64,
    #[serde(default)]
    pub secs: u64,
}

/// One folder on one person's computer that matched a search.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    pub user: String,
    /// the folder's own name, for showing
    pub name: String,
    pub files: Vec<RemoteFile>,
    pub tracks: usize,
    pub bytes: u64,
    /// "FLAC", "MP3 320"
    pub format: String,
    pub free_slot: bool,
    /// bytes per second the person usually uploads at
    pub speed: u64,
    pub queue: u64,
    pub score: i64,
}

/// When a search should end by itself: once this many people have answered, or after this many seconds
/// with no new answer. `None` leaves it to slskd (the full search).
pub type Look = Option<(u32, u32)>;

/// slskd hands out a search's answers only when the search has ended (it keeps them in memory until
/// then, and gives only the count). So that the best copies show early, one click runs three searches
/// for the same words: two short looks that end after a few answers, and the full search as it always was.
/// Each one's answers join the list when it ends.
pub const LOOKS: [Look; 3] = [Some((20, 5)), Some((80, 5)), None];

/// What slskd is sent to start a search.
pub fn search_request(id: &str, text: &str, look: Look) -> Value {
    match look {
        Some((answers, quiet_secs)) => json!({ "id": id, "searchText": text, "responseLimit": answers, "searchTimeout": quiet_secs.max(5) }),
        None => json!({ "id": id, "searchText": text }),
    }
}

/// Adds the answers of one more search to those already known. A person who answered both is kept once,
/// with the newer answer.
pub fn join_responses(known: &mut Vec<Value>, more: Vec<Value>) {
    for r in more {
        let user = s(&r, "username");
        match known.iter().position(|k| s(k, "username") == user) {
            Some(at) => known[at] = r,
            None => known.push(r),
        }
    }
}

#[derive(Clone, Default)]
pub struct Transfer {
    pub id: String,
    pub user: String,
    /// the folder the files belong in, in the downloads folder: the remote folder's own name, or for a
    /// file in a "CD1" style folder the name of the album folder above it
    pub folder: String,
    /// the "CD1" style folder slskd saves this file under, straight in the downloads folder; empty otherwise
    pub disc: String,
    /// the file's own name
    pub file: String,
    pub size: u64,
    pub done_bytes: u64,
    pub speed: f64,
    pub state: String,
}

impl Transfer {
    pub fn finished(&self) -> bool {
        self.state.contains("Completed")
    }
    pub fn ok(&self) -> bool {
        self.state.contains("Succeeded")
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string()
}
fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f.max(0.0) as u64))).unwrap_or(0)
}
fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}
fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

/// "C:\Music\Artist\Album\01 Song.flac" -> ("C:\Music\Artist\Album", "01 Song.flac"). Remote paths use backslashes.
fn split_remote(path: &str) -> (&str, &str) {
    match path.rfind(['\\', '/']) {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}
/// "CD1", "cd 2", "Disc 1", "Disk2"
pub fn is_disc_folder(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    ["cd", "disc", "disk"].iter().any(|p| n.strip_prefix(p).is_some_and(|rest| {
        let rest = rest.trim_start_matches([' ', '_', '-', '.']);
        !rest.is_empty() && rest.len() <= 2 && rest.chars().all(|c| c.is_ascii_digit())
    }))
}
fn ext_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}
/// Short, printable, single-line text from a stranger's computer.
fn tidy(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Turns slskd's search responses into folders to choose from, best first.
pub fn candidates(responses: &[Value], query: &str) -> Vec<Candidate> {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(|w| w.to_string()).collect();
    let mut out: Vec<Candidate> = vec![];
    for r in responses.iter().take(2000) {
        let user = tidy(&s(r, "username"), 60);
        if user.is_empty() {
            continue;
        }
        let (free_slot, speed, queue) = (b(r, "hasFreeUploadSlot"), n(r, "uploadSpeed"), n(r, "queueLength"));
        // folder -> (files to fetch, audio count, bytes, extension counts, bitrates)
        let mut dirs: Vec<(String, Vec<RemoteFile>, usize, u64, Vec<String>, Vec<u64>)> = vec![];
        for f in list(r, "files").iter().take(5000) {
            let filename = s(f, "filename");
            if filename.is_empty() || filename.len() > 600 || filename.chars().any(|c| c.is_control()) {
                continue;
            }
            let (dir, leaf) = split_remote(&filename);
            // "Album\CD1\01.flac" and "Album\CD2\01.flac" are one album: group disc folders under their parent
            let dir = if is_disc_folder(split_remote(dir).1) { split_remote(dir).0 } else { dir };
            let ext = ext_of(leaf);
            let (audio, picture) = (AUDIO.contains(&ext.as_str()), PICTURE.contains(&ext.as_str()));
            if !audio && !picture {
                continue;
            }
            let size = n(f, "size");
            let i = match dirs.iter().position(|d| d.0 == dir) {
                Some(i) => i,
                None => {
                    dirs.push((dir.to_string(), vec![], 0, 0, vec![], vec![]));
                    dirs.len() - 1
                }
            };
            let d = &mut dirs[i];
            d.1.push(RemoteFile { filename: filename.clone(), size, bitrate: n(f, "bitRate"), secs: n(f, "length") });
            d.3 += size;
            if audio {
                d.2 += 1;
                d.4.push(ext);
                d.5.push(n(f, "bitRate"));
            }
        }
        for (dir, files, tracks, bytes, exts, mut rates) in dirs {
            if tracks == 0 {
                continue;
            }
            let name = tidy(split_remote(&dir).1, 120);
            let ext = most_common(&exts);
            rates.sort();
            let rate = rates.get(rates.len() / 2).copied().unwrap_or(0);
            let lossless = matches!(ext.as_str(), "flac" | "wav" | "ape" | "wv");
            let format = if lossless || rate == 0 { ext.to_uppercase() } else { format!("{} {rate}", ext.to_uppercase()) };
            let lower = name.to_lowercase();
            let mut score = 0i64;
            score += if free_slot { 1000 } else { 0 };
            score += if lossless || rate >= 320 { 300 } else if rate >= 256 { 150 } else { 0 };
            score += words.iter().filter(|w| lower.contains(w.as_str())).count() as i64 * 60;
            // a whole album beats a folder holding one or two songs from it
            score += tracks.min(16) as i64 * 10;
            score += (speed / 100_000).min(100) as i64;
            score -= queue.min(300) as i64;
            out.push(Candidate { user: user.clone(), name, files, tracks, bytes, format, free_slot, speed, queue, score });
        }
    }
    out.sort_by(|a, b| b.score.cmp(&a.score).then(b.speed.cmp(&a.speed)));
    out.truncate(60);
    out
}

/// slskd's list of downloads, one entry per file.
///
/// slskd saves every remote folder under that folder's own last name, so the discs of "Album\CD1" and
/// "Album\CD2" land as "CD1" and "CD2" straight in the downloads folder. Such files are listed under the
/// album's name (made safe as a folder name) with their disc folder noted, so the album is followed as one
/// and its discs can be gathered under it when they have all arrived.
pub fn transfers(v: &Value) -> Vec<Transfer> {
    use crate::importer::{clean_name, is_folder_name};
    let mut out = vec![];
    for u in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let user = tidy(&s(u, "username"), 60);
        for d in list(u, "directories") {
            let dir = s(d, "directory");
            let (above, last) = split_remote(&dir);
            let album = clean_name(&tidy(split_remote(above).1, 120));
            let (folder, disc) = if is_disc_folder(last) && is_folder_name(last) && is_folder_name(&album) { (album, tidy(last, 20)) } else { (tidy(last, 120), String::new()) };
            for f in list(d, "files") {
                out.push(Transfer {
                    id: s(f, "id"),
                    user: user.clone(),
                    folder: folder.clone(),
                    disc: disc.clone(),
                    file: tidy(split_remote(&s(f, "filename")).1, 300),
                    size: n(f, "size"),
                    done_bytes: n(f, "bytesTransferred"),
                    speed: f.get("averageSpeed").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    state: s(f, "state"),
                });
            }
        }
    }
    out
}

/// The folder slskd saves finished downloads in, from its settings text.
pub fn parse_downloads_dir(text: &str) -> Option<PathBuf> {
    let mut section = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        if indent == 0 {
            section = k.trim().to_string();
        } else if indent == 2 && section == "directories" && k.trim() == "downloads" {
            let v = yaml_unquote(v);
            return (!v.is_empty()).then(|| PathBuf::from(v));
        }
    }
    None
}

pub fn downloads_dir() -> Option<PathBuf> {
    parse_downloads_dir(&std::fs::read_to_string(settings_file()).ok()?)
}

fn most_common(items: &[String]) -> String {
    let mut best = (String::new(), 0usize);
    for it in items {
        let c = items.iter().filter(|x| *x == it).count();
        if c > best.1 {
            best = (it.clone(), c);
        }
    }
    best.0
}

/// A fresh id in the shape slskd wants for a search (a GUID).
pub fn new_id() -> String {
    let r = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    let (a, b) = (r(), r());
    format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xfff, b >> 52, b & 0xffff_ffff_ffff)
}

// ------------------------------------------------------------------ requests

pub struct Client {
    base: String,
    key: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(set: &Settings) -> Self {
        Self::at(set, set.port)
    }

    /// The client of a slskd started on another web port than its settings file names (see `ports_for_start`).
    pub fn at(set: &Settings, web_port: u16) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().into();
        Client { base: format!("http://127.0.0.1:{web_port}"), key: set.key.clone(), agent }
    }

    fn finish(res: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<(u16, Value), String> {
        let mut r = res.map_err(|_| "The Soulseek client is not answering.".to_string())?;
        let status = r.status().as_u16();
        let bytes = r.body_mut().with_config().limit(40_000_000).read_to_vec().unwrap_or_default();
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let (status, v) = Self::finish(self.agent.get(format!("{}{path}", self.base)).header("X-API-Key", &self.key).call())?;
        if (200..300).contains(&status) { Ok(v) } else { Err(format!("The Soulseek client answered with error {status}.")) }
    }
    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), String> {
        Self::finish(self.agent.post(format!("{}{path}", self.base)).header("X-API-Key", &self.key).send_json(body))
    }
    fn put(&self, path: &str) -> Result<u16, String> {
        Self::finish(self.agent.put(format!("{}{path}", self.base)).header("X-API-Key", &self.key).send_empty()).map(|r| r.0)
    }
    fn delete(&self, path: &str) -> Result<u16, String> {
        Self::finish(self.agent.delete(format!("{}{path}", self.base)).header("X-API-Key", &self.key).call()).map(|r| r.0)
    }

    pub fn server(&self) -> Result<ServerState, String> {
        let v = self.get("/api/v0/server")?;
        Ok(ServerState { connected: b(&v, "isConnected"), logged_in: b(&v, "isLoggedIn"), state: s(&v, "state") })
    }
    /// Ask slskd to sign in to Soulseek again.
    pub fn connect(&self) -> Result<(), String> {
        self.put("/api/v0/server").map(|_| ())
    }

    /// Starts a search. `look`: when it should end by itself (see `LOOKS`); the full search names nothing
    /// and ends by slskd's own rules.
    pub fn start_search(&self, id: &str, text: &str, look: Look) -> Result<(), String> {
        let (status, v) = self.post("/api/v0/searches", &search_request(id, text, look))?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let why = v.as_str().map(|t| tidy(t, 160)).unwrap_or_default();
            Err(if why.is_empty() { format!("The search could not start (error {status}).") } else { format!("The search could not start: {why}") })
        }
    }
    /// Whether a search has ended, and how many people have answered it so far. A small answer: slskd
    /// hands out a search's answers only once it has ended, so they are not asked for here.
    pub fn search_state(&self, id: &str) -> Result<(bool, u64), String> {
        let v = self.get(&format!("/api/v0/searches/{id}"))?;
        Ok((b(&v, "isComplete") || s(&v, "state").contains("Completed"), n(&v, "responseCount")))
    }
    /// The answers of a search that has ended (slskd's own shape; see `candidates`).
    pub fn search_responses(&self, id: &str) -> Result<Vec<Value>, String> {
        let v = self.get(&format!("/api/v0/searches/{id}?includeResponses=true"))?;
        Ok(list(&v, "responses").iter().take(2000).cloned().collect())
    }
    /// Clears searches left behind by an earlier run (slskd keeps them forever otherwise).
    pub fn forget_old_searches(&self) {
        if let Ok(v) = self.get("/api/v0/searches") {
            for old in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]).iter().take(200) {
                let id = s(old, "id");
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                    self.forget_search(&id);
                }
            }
        }
    }

    pub fn forget_search(&self, id: &str) {
        let _ = self.put(&format!("/api/v0/searches/{id}")); // stop it if still running
        let _ = self.delete(&format!("/api/v0/searches/{id}"));
    }

    /// Ask for every file of a folder. Returns how many files slskd accepted.
    pub fn enqueue(&self, c: &Candidate) -> Result<usize, String> {
        let body: Vec<Value> = c.files.iter().map(|f| json!({ "filename": f.filename, "size": f.size })).collect();
        let (status, v) = self.post(&format!("/api/v0/transfers/downloads/{}", crate::util::urlenc(&c.user)), &Value::Array(body))?;
        if !(200..300).contains(&status) {
            let why = v.as_str().map(|t| tidy(t, 160)).unwrap_or_default();
            return Err(if why.is_empty() { format!("The download could not start (error {status}).") } else { format!("The download could not start: {why}") });
        }
        let accepted = list(&v, "enqueued").len();
        Ok(if accepted == 0 && list(&v, "failed").is_empty() { c.files.len() } else { accepted })
    }

    pub fn downloads(&self) -> Result<Vec<Transfer>, String> {
        Ok(transfers(&self.get("/api/v0/transfers/downloads")?))
    }
    pub fn cancel(&self, user: &str, id: &str) {
        let _ = self.delete(&format!("/api/v0/transfers/downloads/{}/{id}?remove=true", crate::util::urlenc(user)));
    }
    pub fn clear_finished(&self) {
        let _ = self.delete("/api/v0/transfers/downloads/all/completed");
    }
}

// ------------------------------------------------------------------ the program itself

/// Ties slskd to this app: when the app ends for any reason (closed, crashed, killed), Windows ends slskd too.
fn tie_to_app(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const core::ffi::c_void, size) != 0 {
            AssignProcessToJobObject(job, child.as_raw_handle() as _);
        }
        // the job's handle is deliberately never closed here: Windows closes it when this app ends, and that is
        // what ends slskd
    }
}

/// slskd's own default for the port it takes Soulseek connections on.
pub const LISTEN_DEFAULT: u16 = 50300;

/// The two ports a start of slskd uses: its web side (where the app talks to it, on this PC only) and the
/// one it takes Soulseek connections on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ports {
    pub web: u16,
    pub listen: u16,
}

/// The ports for this start: the settings file's, unless one of them cannot be listened on now (another
/// program has it, or Windows keeps it for itself: Hyper-V, WSL and Docker reserve blocks of ports, often
/// other ones after each restart). Then the first free port after it, and failing that one Windows hands
/// out. The settings file is not changed: the next start looks again.
pub fn ports_for_start(set: &Settings) -> Ports {
    let free = |ip: [u8; 4], p: u16| std::net::TcpListener::bind((std::net::Ipv4Addr::from(ip), p)).is_ok();
    Ports { web: port_to_use(set.port, |p| free([127, 0, 0, 1], p)), listen: port_to_use(set.listen_port, |p| free([0, 0, 0, 0], p)) }
}

fn port_to_use(want: u16, free: impl Fn(u16) -> bool) -> u16 {
    if want != 0 && free(want) {
        return want;
    }
    // the next hundred ports, skipping 0 and the ones under 1024
    if let Some(p) = (1..=100u16).filter_map(|n| want.checked_add(n)).find(|p| *p >= 1024 && free(*p)) {
        return p;
    }
    // one Windows says is free
    std::net::TcpListener::bind(("0.0.0.0", 0)).ok().and_then(|l| l.local_addr().ok()).map(|a| a.port()).unwrap_or(want)
}

/// The last lines slskd wrote at its latest start (it writes its log to its output): what it said when it
/// stopped. Kept in memory only, at most `TAIL_LINES` lines; everything else it writes is passed over.
static TAIL: Mutex<std::collections::VecDeque<String>> = Mutex::new(std::collections::VecDeque::new());
const TAIL_LINES: usize = 40;

fn keep_line(line: &str) {
    static ESC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
    let line = ESC.replace_all(line, "").trim().to_string();
    if line.is_empty() {
        return;
    }
    let mut t = TAIL.lock().unwrap_or_else(|e| e.into_inner());
    if t.len() >= TAIL_LINES {
        t.pop_front();
    }
    t.push_back(line);
}

/// Why the latest start of slskd stopped, in words for the page, from what it wrote. None when it said
/// nothing that tells.
pub fn why_it_stopped() -> Option<String> {
    let t = TAIL.lock().unwrap_or_else(|e| e.into_inner());
    stopped_words(t.iter().map(String::as_str))
}

fn stopped_words<'a>(lines: impl DoubleEndedIterator<Item = &'a str> + Clone) -> Option<String> {
    let low = |l: &str| l.to_lowercase();
    if lines.clone().any(|l| low(l).contains("already running")) {
        return Some("Another Soulseek client (slskd) is already running on this PC. Close it, then click Try again.".into());
    }
    if lines.clone().any(|l| {
        let l = low(l);
        l.contains("failed to start listening") || l.contains("address already in use") || l.contains("an attempt was made to access a socket in a way forbidden") || l.contains("only one usage of each socket address")
    }) {
        return Some("The Soulseek client could not listen on its network port; another program may have it. Click Try again.".into());
    }
    // the last line that reads as an error, not a line of the program's own trace ("   at slskd...")
    lines.rev().filter(|l| !l.trim_start().starts_with("at ") && !l.contains(" WRN]")).find(|l| {
        let l = low(l);
        l.contains("error") || l.contains("fatal") || l.contains("exception") || l.contains("failed") || l.contains("invalid")
    }).map(|l| {
        // "Soulseek.ListenException: Failed ..." reads as "Failed ..."
        static NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^(\[[^\]]*\]\s*)?([A-Za-z_][\w.]*Exception(\s*\(\d+\))?:\s*)").unwrap());
        let l = NAME.replace(l, "$1").to_string();
        format!("The Soulseek client stopped: {}", crate::util::plain_err(tidy(&l, 160)))
    })
}

/// Starts slskd with no window, saving into `downloads` (see `start_args`), on the ports `ports_for_start`
/// picks. The caller keeps the handle and stops it when the app closes; the web port is where to reach it.
pub fn spawn(downloads: Option<&std::path::Path>, set: &Settings) -> Result<(std::process::Child, Ports), String> {
    let ports = ports_for_start(set);
    if ports.web != set.port || ports.listen != set.listen_port {
        crate::config::log(&format!("soulseek: port {} or {} cannot be used now; starting the client on {} and {}", set.port, set.listen_port, ports.web, ports.listen));
    }
    let child = spawn_plain(downloads, ports)?;
    tie_to_app(&child);
    crate::config::log(&format!("soulseek: the client was started (process {}, ports {} and {})", child.id(), ports.web, ports.listen));
    Ok((child, ports))
}

/// What slskd is handed when the app starts it, as environment variables of that one process (slskd's own
/// names for them): the Soulseek login from Credential Manager, the key the app talks to it with, and a
/// password for its web page. Not on the command line, which Windows shows to every program, and not in
/// a file.
///
/// The key is this run's (`session_key`). The web password is made new for every start and kept nowhere:
/// the app never signs in to that page, so nobody needs to know it.
fn start_env(login: Option<&crate::secrets::Entry>, key: &str, web_password: &str) -> Vec<(&'static str, String)> {
    let mut env = vec![("SLSKD_API_KEY", key.to_string()), ("SLSKD_USERNAME", "music-app".to_string()), ("SLSKD_PASSWORD", web_password.to_string())];
    if let Some(l) = login {
        env.push(("SLSKD_SLSK_USERNAME", l.user.clone()));
        env.push(("SLSKD_SLSK_PASSWORD", l.secret.clone()));
    }
    env
}

/// The command line slskd is started with. `--http-ip-address 127.0.0.1` is always there: the command line
/// wins over slskd's settings file, so whatever that file says, slskd's web side answers only on this PC.
/// So does `--downloads`: the Soulseek downloads folder from Settings (when one is set), so slskd saves
/// where the import list looks, whatever folder its settings file was written with.
/// The ports are given here too, so a port taken since the settings file was written does not stop it.
fn start_args(data: &std::path::Path, downloads: Option<&std::path::Path>, ports: Ports) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec!["--app-dir".into(), data.as_os_str().to_owned(), "--http-ip-address".into(), "127.0.0.1".into()];
    // (and no HTTPS side at all: the app talks plain HTTP on this PC, and a taken port 5031 must not stop it)
    args.extend(["--http-port".into(), ports.web.to_string().into(), "--slsk-listen-port".into(), ports.listen.to_string().into(), "--no-https".into()]);
    if let Some(d) = downloads {
        args.push("--downloads".into());
        args.push(d.as_os_str().to_owned());
    }
    args
}

/// The Soulseek downloads folder from Settings, as the window last saw it (None when Settings has none).
static WANTED_DOWNLOADS: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The window tells the Soulseek side which downloads folder Settings names (called as it draws; cheap
/// when nothing changed). The client is started with it, and started again when it changes.
pub fn want_downloads(dir: &str) {
    let dir = Some(dir.trim()).filter(|d| !d.is_empty()).map(PathBuf::from);
    let mut w = WANTED_DOWNLOADS.lock().unwrap_or_else(|e| e.into_inner());
    if *w != dir {
        *w = dir;
    }
}

pub fn wanted_downloads() -> Option<PathBuf> {
    WANTED_DOWNLOADS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A test run can start slskd with `--no-connect`, so it comes up and answers the app but never goes on the
/// Soulseek network (`MUSIC_RECS_SLSKD_NO_CONNECT`, only with `MUSIC_RECS_DATA` set).
fn test_no_connect() -> bool {
    let test = |v: &str| std::env::var_os(v).is_some_and(|v| !v.is_empty());
    test("MUSIC_RECS_DATA") && test("MUSIC_RECS_SLSKD_NO_CONNECT")
}

fn spawn_plain(downloads: Option<&std::path::Path>, ports: Ports) -> Result<std::process::Child, String> {
    // never with a settings file that would open slskd up (see `parse_settings`)
    read_settings()?;
    TAIL.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let mut child = std::process::Command::new(exe())
        .args(start_args(&data_dir(), downloads, ports))
        .args(test_no_connect().then_some("--no-connect"))
        .envs(start_env(crate::secrets::get(crate::secrets::SOULSEEK).as_ref(), session_key(), &crate::secrets::random_hex(24)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|e| crate::util::plain_err(format!("The Soulseek client could not be started: {e}")))?;
    // what it writes is read as it comes (a full pipe would stop it), and only the last lines are kept
    fn drain(from: impl std::io::Read + Send + 'static) {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(from).split(b'\n').map_while(Result::ok) {
                keep_line(&String::from_utf8_lossy(&line));
            }
        });
    }
    if let Some(out) = child.stdout.take() {
        drain(out);
    }
    if let Some(err) = child.stderr.take() {
        drain(err);
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_read_without_keeping_the_password() {
        let yml = "# note\nsoulseek:\n  username: \"\"\n  password: \"\"\n  listen_port: 50310\nweb:\n  port: 5830\n  authentication:\n    username: music\n    password: abc\n    api_keys:\n      music_app:\n        key: 0123456789abcdef0123\n";
        let set = parse_settings(yml).unwrap();
        assert_eq!((set.port, set.has_login), (5830, false));
        // the key is the app's own for this run, never the one in the file
        assert!(set.key != "0123456789abcdef0123" && set.key.len() == 48 && set.key == parse_settings(yml).unwrap().key);
        // the web password further down must not count as a Soulseek login
        let with = yml.replace("username: \"\"", "username: someone").replace("  password: \"\"", "  password: 'secret'  # mine");
        assert!(parse_settings(&with).unwrap().has_login);
        assert!(parse_settings("web:\n  port: 5830\n").is_ok() && parse_settings("web:\n  https:\n    port: 5831\n").is_err());
    }

    #[test]
    fn the_web_password_and_key_leave_the_file_and_are_made_new_at_each_start() {
        let yml = "soulseek:\n  listen_port: 50310\nremote_configuration: false\nweb:\n  port: 5830\n  ip_address: 127.0.0.1\n  https:\n    disabled: true\n  authentication:\n    disabled: false\n    username: music\n    password: webpw\n    api_keys:\n      # the app's\n      music_app:\n        key: 0123456789abcdef0123\n        role: administrator\n        cidr: 127.0.0.1/32\ndirectories:\n  downloads: 'C:\\x'\n  username: not-a-web-name\nshares:\n  directories: []\n";
        let clean = without_web_secrets(yml);
        assert_eq!(clean, "soulseek:\n  listen_port: 50310\nremote_configuration: false\nweb:\n  port: 5830\n  ip_address: 127.0.0.1\n  https:\n    disabled: true\n  authentication:\n    disabled: false\ndirectories:\n  downloads: 'C:\\x'\n  username: not-a-web-name\nshares:\n  directories: []\n");
        assert!(!clean.contains("webpw") && !clean.contains("0123456789abcdef0123"));
        assert_eq!(without_web_secrets(&clean), clean);
        assert_eq!(parse_settings(&clean).unwrap().port, 5830);

        // on a file: the old ones are taken out, once
        let dir = std::env::temp_dir().join(format!("music-recs-slskd-web-{}-{}", std::process::id(), new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slskd.yml");
        std::fs::write(&path, yml).unwrap();
        assert_eq!(settle_web(&path, yml.to_string()), clean);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), clean);
        let _ = std::fs::remove_dir_all(&dir);

        // what slskd is handed: this run's key, and a web password that is new at every start
        let web = || crate::secrets::random_hex(24);
        let (a, b) = (start_env(None, session_key(), &web()), start_env(None, session_key(), &web()));
        let val = |env: &[(&str, String)], name: &str| env.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(val(&a, "SLSKD_API_KEY"), session_key());
        assert!(session_key().len() >= 32 && val(&a, "SLSKD_PASSWORD").len() >= 32);
        assert_ne!(val(&a, "SLSKD_PASSWORD"), val(&b, "SLSKD_PASSWORD"));
        assert_ne!(val(&a, "SLSKD_PASSWORD"), val(&a, "SLSKD_API_KEY"));
        // with no login kept, no login is handed over
        assert!(a.iter().all(|(k, _)| !k.starts_with("SLSKD_SLSK_")));
    }

    #[test]
    fn a_port_that_cannot_be_listened_on_is_passed_over() {
        // free: kept; taken: the next free one; under 1024 never; nothing free near it: one Windows gives
        assert_eq!(port_to_use(50300, |_| true), 50300);
        assert_eq!(port_to_use(50300, |p| p > 50302), 50303);
        assert_eq!(port_to_use(1000, |p| p > 1000), 1024);
        let given = port_to_use(50300, |_| false);
        assert!(given > 0 && given != 50300, "{given}");
        // a port this PC really has in use is passed over
        // (each held the way slskd uses it: its web side on this PC alone, Soulseek on every address)
        let web_held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let listen_held = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let (web, listen) = (web_held.local_addr().unwrap().port(), listen_held.local_addr().unwrap().port());
        let set = Settings { port: web, listen_port: listen, key: String::new(), has_login: false, login_mark: 0 };
        let ports = ports_for_start(&set);
        assert!(ports.web != web && ports.listen != listen, "{ports:?}");
        drop((web_held, listen_held));
        // the settings file's listen port is read; slskd's own default when there is none
        assert_eq!(parse_settings("soulseek:\n  listen_port: 50310\nweb:\n  port: 5830\n").unwrap().listen_port, 50310);
        assert_eq!(parse_settings("web:\n  port: 5830\n").unwrap().listen_port, LISTEN_DEFAULT);
    }

    #[test]
    fn why_the_client_stopped_is_said_in_words() {
        let say = |lines: &[&str]| stopped_words(lines.iter().copied());
        assert!(say(&["slskd 0.26.2", "An instance of slskd is already running"]).unwrap().starts_with("Another Soulseek client (slskd) is already running"));
        assert!(say(&["[ERR] Failed to start: System.Net.Sockets.SocketException (10013): An attempt was made to access a socket in a way forbidden by its access permissions."]).unwrap().contains("network port"));
        assert!(say(&["Unhandled exception. System.Net.Sockets.SocketException (10048): Only one usage of each socket address (protocol/network address/port) is normally permitted."]).unwrap().contains("network port"));
        assert_eq!(say(&["Starting", "[FTL] Invalid configuration: downloads directory is not writable", "bye"]).unwrap(), "The Soulseek client stopped: [FTL] Invalid configuration: downloads directory is not writable");
        // as slskd 0.26.2 really says it when its port is taken (seen on this PC), its trace after it
        let taken = ["[04:17:52 INF] Application started", "[04:17:52 ERR] Hosting failed to start", "Soulseek.ListenException: Failed to start listening on 0.0.0.0:50300; the IP and/or port may be in use or are otherwise unavailable", "   at slskd.Application.StartAsync(CancellationToken cancellationToken)", "   at Microsoft.Extensions.Hosting.Internal.Host.ForeachService[T](..., Boolean abortOnFirstException, ...)", "[04:17:52 FTL] Application terminated unexpectedly"];
        assert!(say(&taken).unwrap().contains("network port"));
        // an unknown failure: its own words, without the trace and without the exception's name
        assert_eq!(say(&["[ERR] Hosting failed to start", "System.IO.IOException: The disk is full", "   at slskd.Program.Main(String[] args)", "   at Some.abortOnFirstException(x)"]).unwrap(), "The Soulseek client stopped: The disk is full");
        assert_eq!(say(&["Starting", "Listening"]), None);
        // a warning a client that runs well also writes is not why it stopped
        assert_eq!(say(&["[04:19:31 WRN] Error initializing shares: Share cache backup is missing", "Stopped"]), None);
        assert_eq!(say(&[]), None);
    }

    #[test]
    fn slskd_is_only_ever_started_answering_on_this_pc_alone() {
        // the command line wins over the settings file, and it always names 127.0.0.1
        let ports = Ports { web: 5831, listen: 50301 };
        let args = start_args(std::path::Path::new(r"X:\data"), None, ports);
        let at = args.iter().position(|a| a == "--http-ip-address").unwrap();
        assert_eq!(args[at + 1], "127.0.0.1");
        assert_eq!((args[0].to_str(), args[1].to_str()), (Some("--app-dir"), Some(r"X:\data")));
        // the ports of this start win over the settings file's
        assert_eq!(args[4..].iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["--http-port", "5831", "--slsk-listen-port", "50301", "--no-https"]);
        // no secret is ever on the command line
        assert_eq!(args.len(), 9);
        // the downloads folder from Settings, when there is one, wins over the settings file's too
        let with = start_args(std::path::Path::new(r"X:\data"), Some(std::path::Path::new(r"D:\Someone's Downloads\Soul Seek")), ports);
        assert_eq!(with[9..].iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["--downloads", r"D:\Someone's Downloads\Soul Seek"]);
        assert_eq!(with[..9], args[..]);

        // a settings file that would open slskd up is refused, so it is never started with one
        let base = "web:\n  port: 5830\n";
        assert!(parse_settings(&format!("remote_configuration: false\n{base}")).is_ok());
        for open in ["true", "True", "yes", "\"true\"", "on  # for a test"] {
            assert!(parse_settings(&format!("remote_configuration: {open}\n{base}")).is_err(), "{open}");
        }
        assert!(parse_settings("web:\n  port: 5830\n  authentication:\n    disabled: true\n").is_err());
        assert!(parse_settings("web:\n  port: 5830\n  authentication:\n    disabled: false\n  https:\n    disabled: true\n").is_ok());

        // and so is one that shares folders, in any of the ways YAML can list them
        for none in ["shares:\n  directories: []\n", "shares:\n  directories:\n", "shares:\n  filters: []\n", ""] {
            assert!(parse_settings(&format!("{base}{none}")).is_ok(), "{none}");
        }
        for some in ["shares:\n  directories:\n    - 'D:\\Music'\n", "shares:\n  directories:\n  - D:\\Music\n", "shares:\n  directories: ['D:\\Music']\n", "shares:\n  directories: D:\\Music\n"] {
            assert!(parse_settings(&format!("{base}{some}")).is_err(), "{some}");
        }
        // (a list somewhere else is not a share)
        assert!(parse_settings(&format!("{base}filters:\n  search:\n    request:\n      - 'x'\n")).is_ok());
    }

    #[test]
    fn the_login_leaves_the_settings_file_and_is_handed_over_at_start() {
        use crate::secrets::{self, SOULSEEK};
        // made-up values throughout
        let yml = "# note\nsoulseek:\n  username: \"me\"\n  password: \"p\\\"a\\\\ss: #x\"\n  listen_port: 50310\nweb:\n  port: 5830\n  authentication:\n    username: music\n    password: webpw\n    api_keys:\n      music_app:\n        key: 0123456789abcdef0123\n";
        let (clean, found) = without_login(yml);
        assert_eq!(found, Some(("me".to_string(), "p\"a\\ss: #x".to_string())));
        // only the two lines are gone: the web sign-in further down and everything else are untouched
        assert_eq!(clean, yml.replace("  username: \"me\"\n  password: \"p\\\"a\\\\ss: #x\"\n", ""));
        // plain and single-quoted values are read too; empty lines hold no login but still leave the file
        assert_eq!(without_login("soulseek:\n  username: someone\n  password: 'secret'  # mine\n").1, Some(("someone".to_string(), "secret".to_string())));
        assert_eq!(without_login("soulseek:\n  username: \"\"\n  password: \"\"\n  listen_port: 1\n"), ("soulseek:\n  listen_port: 1\n".to_string(), None));
        assert_eq!(without_login(&clean), (clean.clone(), None));

        // the carry-over on a file: into the store, then out of the file
        let dir = std::env::temp_dir().join(format!("music-recs-slskd-{}-{}", std::process::id(), new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slskd.yml");
        secrets::remove(SOULSEEK);

        // when the login cannot be kept, the file stays exactly as it was
        std::fs::write(&path, yml).unwrap();
        secrets::REFUSE.with(|r| r.set(true));
        assert_eq!(settle_login(&path, yml.to_string()), yml);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), yml);
        secrets::REFUSE.with(|r| r.set(false));
        assert!(secrets::get(SOULSEEK).is_none());

        let stamp = login_stamp();
        assert_eq!(settle_login(&path, yml.to_string()), clean);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), clean);
        assert!(!clean.contains("p\\\"a") && login_stamp() > stamp);
        let kept = secrets::get(SOULSEEK).unwrap();
        assert_eq!((kept.user.as_str(), kept.secret.as_str()), ("me", "p\"a\\ss: #x"));
        // a second look changes nothing
        assert_eq!(settle_login(&path, clean.clone()), clean);

        // slskd gets the login as its own environment variables, and nothing when there is none
        let env = start_env(Some(&kept), "k", "w");
        assert!(env.contains(&("SLSKD_SLSK_USERNAME", "me".to_string())) && env.contains(&("SLSKD_SLSK_PASSWORD", "p\"a\\ss: #x".to_string())));

        // bad input is refused before anything is kept
        assert!(check_login("", "x").is_err() && check_login("a", "").is_err() && check_login("a\nb: c", "x").is_err() && check_login("a", "x").is_ok());

        // a login saved on a PC that has never had the client (the first-start screen's "Start", or
        // Settings): there is no settings file yet, so it is written first, as fetching the client would
        // write it, and the login is kept and is not in the file
        secrets::remove(SOULSEEK);
        let fresh = dir.join("never").join("data").join("slskd.yml");
        save_login_at(&fresh, Some(&dir.join("downloads")), "someone-new", "p4ss-w0rd-x").unwrap();
        let written = std::fs::read_to_string(&fresh).unwrap();
        assert!(parse_settings(&written).is_ok() && !written.contains("someone-new") && !written.contains("p4ss-w0rd-x"), "{written}");
        let kept = secrets::get(SOULSEEK).unwrap();
        assert_eq!((kept.user.as_str(), kept.secret.as_str()), ("someone-new", "p4ss-w0rd-x"));
        // a file that is there is left as it is; bad input writes no file and keeps nothing new
        save_login_at(&fresh, None, "another", "p4ss-2").unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), written);
        assert!(save_login_at(&dir.join("bad").join("slskd.yml"), None, "", "x").is_err() && !dir.join("bad").exists());
        assert_eq!(secrets::get(SOULSEEK).unwrap().user, "another");
        secrets::remove(SOULSEEK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_answers_become_ranked_folders() {
        let responses = serde_json::json!([
            { "username": "slow", "hasFreeUploadSlot": false, "uploadSpeed": 50000, "queueLength": 40, "files": [
                { "filename": "C:\\m\\Artist - Quiet Rooms (2020)\\01 a.mp3", "size": 100, "bitRate": 128 },
                { "filename": "C:\\m\\Artist - Quiet Rooms (2020)\\02 b.mp3", "size": 100, "bitRate": 128 } ] },
            { "username": "good", "hasFreeUploadSlot": true, "uploadSpeed": 2000000, "queueLength": 0, "files": [
                { "filename": "D:\\x\\Quiet Rooms [FLAC]\\01 a.flac", "size": 1000 },
                { "filename": "D:\\x\\Quiet Rooms [FLAC]\\cover.jpg", "size": 10 },
                { "filename": "D:\\x\\Quiet Rooms [FLAC]\\notes.exe", "size": 10 },
                { "filename": "D:\\x\\Other\\01 z.flac", "size": 5 } ] },
            { "files": [ { "filename": 7 } ] },
            "nonsense"
        ]);
        let c = candidates(responses.as_array().unwrap(), "quiet rooms");
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].user.as_str(), c[0].name.as_str(), c[0].format.as_str(), c[0].tracks, c[0].files.len()), ("good", "Quiet Rooms [FLAC]", "FLAC", 1, 2));
        assert_eq!(c[2].format, "MP3 128");
        assert!(c.iter().all(|c| c.files.iter().all(|f| !f.filename.ends_with(".exe"))));
    }

    #[test]
    fn disc_folders_are_one_album() {
        assert!(is_disc_folder("CD1") && is_disc_folder("Disc 2") && is_disc_folder("disk_03") && !is_disc_folder("CD") && !is_disc_folder("Discovery") && !is_disc_folder("CD2013"));
        let responses = serde_json::json!([{ "username": "u", "hasFreeUploadSlot": true, "files": [
            { "filename": "m\\Reflektor (2013)\\CD1\\01 a.flac", "size": 1 },
            { "filename": "m\\Reflektor (2013)\\CD2\\01 b.flac", "size": 1 },
            { "filename": "m\\Reflektor (2013)\\cover.jpg", "size": 1 } ] }]);
        let c = candidates(responses.as_array().unwrap(), "reflektor");
        assert_eq!((c.len(), c[0].name.as_str(), c[0].tracks, c[0].files.len()), (1, "Reflektor (2013)", 2, 3));
    }

    #[test]
    fn disc_files_are_listed_under_their_album() {
        let v = serde_json::json!([{ "username": "u", "directories": [
            { "directory": "m\\Reflektor: Deluxe\\CD1", "files": [{ "id": "1", "filename": "m\\Reflektor: Deluxe\\CD1\\01 a.flac", "size": 5, "bytesTransferred": 5, "state": "Completed, Succeeded" }] },
            { "directory": "m\\Reflektor: Deluxe", "files": [{ "id": "2", "filename": "m\\Reflektor: Deluxe\\cover.jpg", "size": 1, "state": "Queued, Remotely" }] },
            { "directory": "..\\CD2", "files": [{ "id": "3", "filename": "..\\CD2\\01 b.flac", "size": 1, "state": "InProgress" }] },
            { "directory": "CD3", "files": [{ "id": "4", "filename": "CD3\\01 c.flac", "size": 1, "state": "InProgress" }] } ] }]);
        let t = transfers(&v);
        let got: Vec<(&str, &str, &str)> = t.iter().map(|t| (t.folder.as_str(), t.disc.as_str(), t.file.as_str())).collect();
        assert_eq!(got, vec![
            // the album name is made safe as a folder name
            ("Reflektor - Deluxe", "CD1", "01 a.flac"),
            ("Reflektor: Deluxe", "", "cover.jpg"),
            // no usable album name above the disc folder: left as it arrives
            ("CD2", "", "01 b.flac"),
            ("CD3", "", "01 c.flac"),
        ]);
        assert!(t[0].ok() && t[0].done_bytes == 5);
    }

    #[test]
    fn the_downloads_folder_is_read_from_the_settings() {
        let text = "web:\n  port: 5830\ndirectories:\n  downloads: 'C:\\Users\\N\\Downloads\\SoulSeek'\n  incomplete: 'C:\\x'\n";
        assert_eq!(parse_downloads_dir(text), Some(PathBuf::from(r"C:\Users\N\Downloads\SoulSeek")));
        assert_eq!(parse_downloads_dir("web:\n  downloads: x\n"), None);
    }

    /// Not part of the normal run: `set MUSIC_RECS_SEARCH_JSON=<file>` then `cargo test real_answer -- --ignored --nocapture`
    /// prints how a saved slskd answer is ranked.
    #[test]
    #[ignore]
    fn real_answer() {
        let Some(path) = std::env::var_os("MUSIC_RECS_SEARCH_JSON") else { return };
        let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let c = candidates(list(&v, "responses"), &s(&v, "searchText"));
        println!("{} candidates", c.len());
        for x in c.iter().take(12) {
            println!("{:5}  {:9} {:3} tracks {:6} MB  slot={} queue={:3} speed={:5} KB/s  {}", x.score, x.format, x.tracks, x.bytes / 1_048_576, x.free_slot, x.queue, x.speed / 1024, x.name);
        }
    }

    #[test]
    fn ids_look_like_guids() {
        let id = new_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_ne!(id, new_id());
    }
}
