//! The app updates itself, in the rhythm of the author's music player Cadence: never on the way to opening
//! the window, never a restart nobody asked for.
//!
//! The app is one program file in whatever folder it was unpacked to, with no installer, so it replaces
//! its own file:
//!   1. CHECK   five seconds after the window is up, then every 12 hours (30 minutes after a failure): one
//!              small file, `update.json`, from the latest release on GitHub. 30 seconds at most.
//!   2. FETCH   the new program file, saved beside this one as `<name>.new-<version>.part`. A fetch that
//!              was cut off carries on where it stopped. Ten minutes at most.
//!   3. PROVE   its size and SHA-256 must be the ones in `update.json`, and those must carry the author's
//!              signature (ECDSA P-256, checked by Windows' own cryptography against `PUBLIC_KEY`; the
//!              private half is only on the author's PC). Then the new file is started once, unseen, with
//!              `--self-check` and must answer with its version.
//!   4. WAIT    it lies beside the app as `<name>.new.exe` with `<name>.new.json`. "UPDATE READY" shows in
//!              the top row; nothing else happens.
//!   5. SWAP    on "Restart to update", after the app has closed the usual way, or at the next start
//!              before the window: the staged file is proven again, the running file is renamed to
//!              `<name>.old.exe` (Windows allows renaming a running program file, not writing over it),
//!              the new one takes its name and is started. One old file is kept as a step back.
//!   6. GUARD   the swap leaves `<name>.update-pending.json`, and the copy that made way stays on, unseen, as
//!              a guard (`<name>.old.exe --guard-update`). It starts the new version and waits for it to
//!              have its window up and drawing for five seconds, when the new version removes the note
//!              itself. If the new version ends with an error first, or has not got that far in two
//!              minutes (it is then ended), the guard puts the old file back under the app's name, starts
//!              it, and notes the version in `<name>.update-refused.json` so it is never fetched or put
//!              in again (a later version is). The app that is back says so once, and Settings says so.
//!              If no guard was there (the PC went off, say), the third start of a version that never
//!              got its window up goes back by itself. A version that got its window up once is never
//!              rolled back: the note is gone.
//!
//! Where the app cannot write to its own folder it only says that a version is out, with a way to the
//! download page. A test run (`MUSIC_RECS_DATA` set) and a build in cargo's `target` folder never check
//! and never swap. A lab can stand in for GitHub with a folder (see `source`).
use crate::helpers::{hex, said, Sha256};
use chrono::{DateTime, Local};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// The public repository: its releases are where updates come from, its pages where the links go.
pub const REPO: &str = "https://github.com/BradChadley/bradleys-music-tool";
pub const KOFI: &str = "https://ko-fi.com/bradchadley";
pub const CADENCE: &str = "https://github.com/BradChadley/Cadence-Releases";
/// The public half of the author's signing key: X then Y of a point on P-256, as hex. An update is taken
/// only when its checksum is signed by the private half (`tools\sign-update.ps1` on the author's PC).
const PUBLIC_KEY: &str = "65a23585fb97bae6b74c5d67d997da82c962b6b6a699962288a2e0bf75efc6b7abd3123e3c8d8e6a05154860c9344c977e2f10a0cbb993065ba46f58bd8b1685";
/// The program file's name among a release's files.
const FILE: &str = "bradleys-music-tool.exe";
/// No program file of this app is anywhere near this large; more is refused before it is fetched.
const MOST: u64 = 300 * 1024 * 1024;
const CHECK_WITHIN: Duration = Duration::from_secs(30);
const FETCH_WITHIN: Duration = Duration::from_secs(600);
const AGAIN: Duration = Duration::from_secs(12 * 3600);
const AGAIN_AFTER_FAILURE: Duration = Duration::from_secs(30 * 60);

// ------------------------------------------------------------------ what the user chose

/// What to do about new versions (Settings, About).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Policy {
    /// fetch them and put them in, at a restart the user asks for or the next start
    Auto,
    /// only say that one is out
    Tell,
    /// never ask GitHub
    Off,
}

impl Policy {
    pub fn from_saved(s: &str) -> Self {
        match s {
            "tell" => Policy::Tell,
            "off" => Policy::Off,
            _ => Policy::Auto,
        }
    }

    pub fn saved(self) -> &'static str {
        match self {
            Policy::Auto => "auto",
            Policy::Tell => "tell",
            Policy::Off => "off",
        }
    }
}

// ------------------------------------------------------------------ versions

/// "1.3.0" as three numbers. Anything else is not a version.
fn triple(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split('.');
    let mut next = || parts.next().filter(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<u32>().ok());
    let out = (next()?, next()?, next()?);
    parts.next().is_none().then_some(out)
}

/// True when `a` is a later version than `b`. A version that cannot be read is never later.
fn newer(a: &str, b: &str) -> bool {
    matches!((triple(a), triple(b)), (Some(a), Some(b)) if a > b)
}

/// The version of this program file. A lab can have the app take itself for an older one
/// (`MUSIC_RECS_UPDATE_AS`, only in a test run), so the same build can stand in as its own update.
pub fn current() -> String {
    if let (Some(_), Ok(v)) = (test_data(), std::env::var("MUSIC_RECS_UPDATE_AS")) {
        if triple(&v).is_some() {
            return v;
        }
    }
    env!("CARGO_PKG_VERSION").to_string()
}

fn test_data() -> Option<std::ffi::OsString> {
    std::env::var_os("MUSIC_RECS_DATA").filter(|v| !v.is_empty())
}

// ------------------------------------------------------------------ what a release says of itself

/// `update.json`: which version the release holds, and what its program file must be.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub version: String,
    pub size: u64,
    pub sha256: String,
    pub signature: String,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if !is_hex(s, N * 2) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl Manifest {
    fn read(text: &str) -> Option<Manifest> {
        let m: Manifest = serde_json::from_str(text).ok()?;
        (triple(&m.version).is_some() && m.size > 0 && m.size <= MOST && is_hex(&m.sha256, 64) && is_hex(&m.signature, 128)).then_some(m)
    }

    /// The text that is signed: the version, the size and the checksum, a line each.
    fn signed_text(&self) -> String {
        format!("bradleys-music-tool update\n{}\n{}\n{}\n", self.version, self.size, self.sha256)
    }

    /// True when the signature is the one the holder of `key`'s private half made for this version, size
    /// and checksum.
    fn signed_by(&self, key: &str) -> bool {
        match (unhex::<64>(key), unhex::<64>(&self.signature)) {
            (Some(key), Some(signature)) => signature_holds(&key, self.signed_text().as_bytes(), &signature),
            _ => false,
        }
    }
}

/// ECDSA on P-256 over the SHA-256 of `message`, checked by Windows (bcrypt): `key` is X then Y,
/// `signature` r then s.
fn signature_holds(key: &[u8; 64], message: &[u8], signature: &[u8; 64]) -> bool {
    use windows_sys::Win32::Security::Cryptography::{BCryptDestroyKey, BCryptImportKeyPair, BCryptVerifySignature, BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALG_HANDLE, BCRYPT_ECDSA_PUBLIC_P256_MAGIC};
    let Ok(mut hash) = Sha256::new() else { return false };
    hash.update(message);
    let Ok(digest) = hash.finish() else { return false };
    // the key as Windows takes it: what it is, how long each half is, then the two halves
    let mut blob = Vec::with_capacity(72);
    blob.extend_from_slice(&BCRYPT_ECDSA_PUBLIC_P256_MAGIC.to_le_bytes());
    blob.extend_from_slice(&32u32.to_le_bytes());
    blob.extend_from_slice(key);
    unsafe {
        let mut handle = std::ptr::null_mut();
        if BCryptImportKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, std::ptr::null_mut(), BCRYPT_ECCPUBLIC_BLOB, &mut handle, blob.as_ptr(), blob.len() as u32, 0) != 0 || handle.is_null() {
            return false;
        }
        let holds = BCryptVerifySignature(handle, std::ptr::null(), digest.as_ptr(), digest.len() as u32, signature.as_ptr(), signature.len() as u32, 0) == 0;
        BCryptDestroyKey(handle);
        holds
    }
}

/// The key updates must be signed with. A lab names its own (`MUSIC_RECS_UPDATE_KEY`, only in a test run),
/// so the author's key is never needed to try the updater.
fn trusted_key() -> String {
    if let (Some(_), Ok(k)) = (test_data(), std::env::var("MUSIC_RECS_UPDATE_KEY")) {
        return k;
    }
    PUBLIC_KEY.to_string()
}

// ------------------------------------------------------------------ where updates come from

enum Source {
    /// the public repository's releases
    Web(String),
    /// a folder that holds `update.json` and the program file, standing in for GitHub in a lab
    Folder(PathBuf),
}

/// Where this copy of the app gets updates, or None when it never updates itself: in a unit test, in a
/// test run (unless the lab names a folder with `MUSIC_RECS_UPDATE_FROM`), and for a build that runs from
/// cargo's `target` folder.
fn source() -> Option<Source> {
    if cfg!(test) {
        return None;
    }
    if test_data().is_some() {
        // (a lab may also name a repository's address, to try the real way to GitHub)
        let from = std::env::var_os("MUSIC_RECS_UPDATE_FROM").filter(|v| !v.is_empty())?;
        return Some(match from.to_str().filter(|s| s.starts_with("https://")) {
            Some(repo) => Source::Web(repo.trim_end_matches('/').to_string()),
            None => Source::Folder(PathBuf::from(from)),
        });
    }
    let exe = std::env::current_exe().ok()?;
    if in_build_folder(&exe) {
        return None;
    }
    Some(Source::Web(REPO.to_string()))
}

fn in_build_folder(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target"))
}

/// True when this copy of the app can update itself at all.
pub fn available() -> bool {
    source().is_some()
}

fn agent(within: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(within))
        .http_status_as_error(false)
        .user_agent(crate::net::USER_AGENT)
        .build()
        .into()
}

impl Source {
    /// The latest release's `update.json`. Ok(None): there is no release (yet), which is not a failure.
    fn manifest(&self) -> Result<Option<String>, String> {
        let mut text = String::new();
        match self {
            Source::Web(repo) => {
                let res = agent(CHECK_WITHIN).get(format!("{repo}/releases/latest/download/update.json")).call().map_err(|e| crate::util::plain_err(format!("GitHub could not be reached ({e})")))?;
                match res.status().as_u16() {
                    200 => {}
                    404 => return Ok(None),
                    other => return Err(format!("GitHub answered with error {other}")),
                }
                res.into_body().into_reader().take(64 * 1024).read_to_string(&mut text).map_err(|e| crate::util::plain_err(format!("the answer broke off ({e})")))?;
            }
            Source::Folder(dir) => match std::fs::read_to_string(dir.join("update.json")) {
                Ok(t) => text = t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.to_string()),
            },
        }
        Ok(Some(text))
    }

    /// The program file of `version`, from byte `from` on when the source can do that. Returns the
    /// reader and the byte it really starts at (0 when the source sent the whole file).
    fn program(&self, version: &str, from: u64) -> Result<(Box<dyn Read>, u64), String> {
        match self {
            Source::Web(repo) => {
                let mut ask = agent(FETCH_WITHIN).get(format!("{repo}/releases/download/v{version}/{FILE}"));
                if from > 0 {
                    ask = ask.header("Range", format!("bytes={from}-"));
                }
                let res = ask.call().map_err(|e| crate::util::plain_err(format!("the download could not start ({e})")))?;
                match res.status().as_u16() {
                    206 if from > 0 => Ok((Box::new(res.into_body().into_reader()), from)),
                    200 => Ok((Box::new(res.into_body().into_reader()), 0)),
                    other => Err(format!("the download could not start (error {other})")),
                }
            }
            Source::Folder(dir) => {
                let mut f = std::fs::File::open(dir.join(FILE)).map_err(|e| e.to_string())?;
                f.seek(SeekFrom::Start(from)).map_err(|e| e.to_string())?;
                Ok((Box::new(f), from))
            }
        }
    }
}

// ------------------------------------------------------------------ the files beside the program file

/// The names an update goes by beside the program file `exe`.
#[derive(Clone, Debug)]
struct Places {
    exe: PathBuf,
    /// fetched and proven, waiting to be put in
    new: PathBuf,
    /// the `update.json` it was proven against, to prove it again before the swap
    note: PathBuf,
    /// the program file from before the last swap: one step back
    old: PathBuf,
    /// left by a swap until the new version has its window up (`Pending`)
    pending: PathBuf,
    /// the last version that could not start and was rolled back (`Refused`)
    refused: PathBuf,
    /// a version that could not start, moved aside while the old one goes back
    failed: PathBuf,
}

impl Places {
    fn of(exe: &Path) -> Places {
        Places {
            exe: exe.to_path_buf(),
            new: exe.with_extension("new.exe"),
            note: exe.with_extension("new.json"),
            old: exe.with_extension("old.exe"),
            pending: exe.with_extension("update-pending.json"),
            refused: exe.with_extension("update-refused.json"),
            failed: exe.with_extension("failed.exe"),
        }
    }

    fn here() -> Option<Places> {
        std::env::current_exe().ok().map(|e| Places::of(&e))
    }

    /// A fetch of `version` that is not whole yet.
    fn part(&self, version: &str) -> PathBuf {
        self.exe.with_extension(format!("new-{version}.part"))
    }

    /// Removes the parts of fetches of other versions (and of every version with `keep` None).
    fn clear_parts(&self, keep: Option<&str>) {
        let (Some(dir), Some(stem)) = (self.exe.parent(), self.exe.file_stem().map(|s| s.to_string_lossy().to_string())) else { return };
        let kept = keep.map(|v| self.part(v));
        let Ok(list) = std::fs::read_dir(dir) else { return };
        for e in list.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&format!("{stem}.new-")) && name.ends_with(".part") && kept.as_deref() != Some(e.path().as_path()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    fn forget_staged(&self) {
        let _ = std::fs::remove_file(&self.new);
        let _ = std::fs::remove_file(&self.note);
    }

    /// Whether the app may write beside its program file (it may not in, say, Program Files).
    fn writable(&self) -> bool {
        let probe = self.exe.with_extension("new-probe");
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    }
}

fn file_sha256(path: &Path, upto: u64) -> Option<(String, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut hash = Sha256::new().ok()?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0u64;
    while got < upto {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        got += n as u64;
    }
    Some((hex(&hash.finish().ok()?), got))
}

/// The update that lies ready beside the program file, proven again: the note carries a signature that
/// holds under `key`, names a later version than `than`, and the file is exactly what the note says.
/// Anything less and both files are removed.
fn staged(p: &Places, key: &str, than: &str) -> Option<Manifest> {
    if !p.new.exists() && !p.note.exists() {
        return None;
    }
    let good = (|| {
        let m = Manifest::read(&std::fs::read_to_string(&p.note).ok()?)?;
        if !m.signed_by(key) || !newer(&m.version, than) || is_refused(p, &m.version) || std::fs::metadata(&p.new).ok()?.len() != m.size {
            return None;
        }
        let (sha, _) = file_sha256(&p.new, u64::MAX)?;
        (sha == m.sha256).then_some(m)
    })();
    if good.is_none() {
        crate::config::log("update: what lay ready beside the app did not hold up and was removed");
        p.forget_staged();
    }
    good
}

/// Why a fetch ended without the file.
enum Halt {
    Stopped,
    Failed(String),
}

/// Fetches the program file of `m` into its part file, carrying on from what an earlier try left, and
/// takes it only when it is exactly the size and checksum the signed manifest names. `progress` hears
/// how many bytes are there.
fn fetch(src: &Source, p: &Places, m: &Manifest, stop: &AtomicBool, progress: &dyn Fn(u64)) -> Result<PathBuf, Halt> {
    p.clear_parts(Some(&m.version));
    let part = p.part(&m.version);
    let have = std::fs::metadata(&part).map(|d| d.len()).unwrap_or(0);
    let have = if have >= m.size { 0 } else { have };
    let (mut body, from) = src.program(&m.version, have).map_err(Halt::Failed)?;
    let mut hash = Sha256::new().map_err(Halt::Failed)?;
    let mut out = if from > 0 {
        // what is already here counts toward the checksum
        let mut f = std::fs::File::open(&part).map_err(|e| Halt::Failed(e.to_string()))?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut left = from;
        while left > 0 {
            let want = buf.len().min(left as usize);
            let n = f.read(&mut buf[..want]).map_err(|e| Halt::Failed(e.to_string()))?;
            if n == 0 {
                return Err(Halt::Failed("the part that was fetched before could not be read".into()));
            }
            hash.update(&buf[..n]);
            left -= n as u64;
        }
        std::fs::OpenOptions::new().append(true).open(&part).map_err(|e| Halt::Failed(said("the download could not be saved.", &e)))?
    } else {
        std::fs::File::create(&part).map_err(|e| Halt::Failed(said("the download could not be saved.", &e)))?
    };
    let mut got = from;
    progress(got);
    let began = Instant::now();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            // (the part stays: the next try carries on from it)
            return Err(Halt::Stopped);
        }
        if began.elapsed() > FETCH_WITHIN {
            return Err(Halt::Failed("the download took too long".into()));
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Halt::Failed(crate::util::plain_err(format!("the download broke off ({e})")))),
        };
        got += n as u64;
        if got > m.size {
            drop(out);
            let _ = std::fs::remove_file(&part);
            return Err(Halt::Failed("more arrived than the release names; thrown away".into()));
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| Halt::Failed(said("the download could not be saved.", &e)))?;
        progress(got);
        if slow() {
            // about 3 MB a second, so a lab can see the progress and "Stop"
            std::thread::sleep(Duration::from_micros(n as u64 / 3));
        }
    }
    let flushed = out.flush().and_then(|_| out.sync_all());
    drop(out);
    if got < m.size {
        return Err(Halt::Failed("the download ended early".into()));
    }
    let sha = hex(&hash.finish().map_err(Halt::Failed)?);
    if sha != m.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(Halt::Failed("what arrived is not the file the release names; thrown away".into()));
    }
    flushed.map_err(|e| Halt::Failed(said("the download could not be saved.", &e)))?;
    Ok(part)
}

fn slow() -> bool {
    test_data().is_some() && std::env::var_os("MUSIC_RECS_SLOW_FETCH").is_some_and(|v| !v.is_empty())
}

/// What a program file answers to `--self-check`.
fn self_check_answer(version: &str) -> String {
    format!("bradleys-music-tool {version}")
}

/// Starts `file` once, unseen, with `--self-check`: it must answer with `version` within 20 seconds.
fn runs_as(file: &Path, version: &str) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let started = std::process::Command::new(file)
        .arg("--self-check")
        .env_remove("MUSIC_RECS_UPDATE_AS")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let Ok(mut child) = started else { return false };
    let began = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut said = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut said);
                }
                return status.success() && said.trim() == self_check_answer(version);
            }
            Ok(None) if began.elapsed() < Duration::from_secs(20) => std::thread::sleep(Duration::from_millis(40)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Puts the staged update in: the running program file becomes the old one, the new one takes its name.
/// If the second step fails the first is undone, so the app's name always has a program file behind it
/// afterwards. The old file from an earlier swap goes to the Recycle Bin first.
fn swap(p: &Places) -> Result<(), String> {
    if p.old.exists() {
        // (a unit test's made-up file is simply removed, so the tests leave nothing in the Recycle Bin)
        if cfg!(test) || trash::delete(&p.old).is_err() {
            std::fs::remove_file(&p.old).map_err(|e| said("the old program file could not be moved away.", &e))?;
        }
    }
    std::fs::rename(&p.exe, &p.old).map_err(|e| said("the program file could not be renamed.", &e))?;
    if let Err(e) = std::fs::rename(&p.new, &p.exe) {
        let back = std::fs::rename(&p.old, &p.exe);
        return Err(format!("{} {}", said("the new program file could not take its place.", &e), if back.is_ok() { "The old one is back in place." } else { "THE OLD ONE COULD NOT BE PUT BACK." }));
    }
    let _ = std::fs::remove_file(&p.note);
    Ok(())
}

// ------------------------------------------------------------------ the worker, and what the pages ask

/// Why a version that is out is not being put in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Why {
    /// the user asked only to be told
    TellOnly,
    /// the app cannot write beside its program file
    CannotWrite,
    /// the user stopped the fetch
    Stopped,
    /// it was put in once, could not start on this PC, and the app went back (see `Refused`)
    Refused,
}

/// Where updating stands, for Settings and the top row.
#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// nothing has been looked for yet (or checking is switched off)
    Idle,
    Checking,
    /// the last check found nothing newer: when that was
    UpToDate(DateTime<Local>),
    /// a newer version is being fetched: its name, bytes so far, of how many
    Fetching(String, u64, u64),
    /// fetched and proven; it goes in at a restart
    Ready(String),
    /// a newer version is out and is not being put in
    Out(String, Why),
    /// the last check or fetch did not work; it is tried again by itself
    Failed,
}

static STATE: Mutex<State> = Mutex::new(State::Idle);
static DONE: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static POLICY: AtomicU8 = AtomicU8::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
static WAKE: Mutex<bool> = Mutex::new(false);
static WOKEN: Condvar = Condvar::new();
/// "Restart to update" was clicked: once the app has closed the usual way, `after_close` swaps and starts it again.
static RESTART: AtomicBool = AtomicBool::new(false);

fn policy() -> Policy {
    match POLICY.load(Ordering::Relaxed) {
        1 => Policy::Tell,
        2 => Policy::Off,
        _ => Policy::Auto,
    }
}

fn set(s: State) {
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

pub fn state() -> State {
    match STATE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        State::Fetching(v, _, total) => State::Fetching(v, DONE.load(Ordering::Relaxed), total),
        other => other,
    }
}

/// The version that is ready to go in, when one is.
pub fn ready() -> Option<String> {
    match state() {
        State::Ready(v) => Some(v),
        _ => None,
    }
}

fn wake() {
    *WAKE.lock().unwrap_or_else(|e| e.into_inner()) = true;
    WOKEN.notify_all();
}

/// Waits `most`, or until something asks for a look now. True when it was asked.
fn pause(most: Duration) -> bool {
    let until = Instant::now() + most;
    let mut asked = WAKE.lock().unwrap_or_else(|e| e.into_inner());
    while !*asked {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        asked = WOKEN.wait_timeout(asked, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    std::mem::replace(&mut *asked, false)
}

/// The user's choice changed (or is told for the first time). A changed choice is acted on at once.
pub fn set_policy(p: Policy) {
    let n = match p {
        Policy::Auto => 0,
        Policy::Tell => 1,
        Policy::Off => 2,
    };
    if POLICY.swap(n, Ordering::Relaxed) != n && STARTED.load(Ordering::Relaxed) {
        wake();
    }
}

/// "Check now" was clicked and has not been acted on yet (a changed choice wakes the worker too, and
/// must not make it look when the choice is never to).
static NOW: AtomicBool = AtomicBool::new(false);

/// "Check now" (also "Carry on" after a fetch was stopped).
pub fn check_now() {
    STOP.store(false, Ordering::Relaxed);
    NOW.store(true, Ordering::Relaxed);
    wake();
}

/// "Stop" while a version is being fetched. What has arrived stays for the next try.
pub fn stop_fetch() {
    STOP.store(true, Ordering::Relaxed);
}

/// Starts the worker, once the window is up. Nothing here is on the way to opening the window: the first
/// look is five seconds later.
pub fn start(ctx: &egui::Context, p: Policy) {
    // (with every frame, whatever the choice: a first start after a swap says when it is up and working)
    settle(ctx);
    set_policy(p);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(src) = source() else { return };
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("update".into())
        .spawn(move || {
            pause(Duration::from_secs(5));
            loop {
                let next = match policy() {
                    // never check: nothing is asked of GitHub unless "Check now" is clicked. (An update
                    // that already lies ready still shows, and still goes in at the next start.)
                    Policy::Off if !NOW.swap(false, Ordering::Relaxed) => {
                        let lying = Places::here().and_then(|p| staged(&p, &trusted_key(), &current()));
                        set(lying.map_or(State::Idle, |m| State::Ready(m.version)));
                        Duration::from_secs(365 * 24 * 3600)
                    }
                    p => {
                        NOW.store(false, Ordering::Relaxed);
                        let found = round(&src, p, &ctx);
                        if found == State::Failed { AGAIN_AFTER_FAILURE } else { AGAIN }
                    }
                };
                ctx.request_repaint();
                pause(next);
            }
        })
        .ok();
}

/// One look for a newer version and, when the user wants that, its fetch. Leaves the state it ends in.
fn round(src: &Source, p: Policy, ctx: &egui::Context) -> State {
    let end = |s: State| {
        set(s.clone());
        s
    };
    let Some(places) = Places::here() else { return end(State::Failed) };
    let (key, mine) = (trusted_key(), current());
    if let Some(m) = staged(&places, &key, &mine) {
        return end(State::Ready(m.version));
    }
    set(State::Checking);
    ctx.request_repaint();
    let text = match src.manifest() {
        Ok(Some(t)) => t,
        Ok(None) => return end(State::UpToDate(Local::now())),
        Err(why) => {
            crate::config::log(&format!("update: the check did not get through: {why}"));
            return end(State::Failed);
        }
    };
    let Some(m) = Manifest::read(&text) else {
        crate::config::log("update: the release's update.json could not be read; nothing was fetched");
        return end(State::Failed);
    };
    if !m.signed_by(&key) {
        crate::config::log(&format!("update: version {} is NOT signed by the app's author; nothing was fetched", m.version));
        return end(State::Failed);
    }
    if !newer(&m.version, &mine) {
        places.clear_parts(None);
        return end(State::UpToDate(Local::now()));
    }
    if is_refused(&places, &m.version) {
        // (it could not start here once: it is not fetched again, only a later version is)
        places.clear_parts(None);
        return end(State::Out(m.version, Why::Refused));
    }
    if p != Policy::Auto {
        return end(State::Out(m.version, Why::TellOnly));
    }
    if !places.writable() {
        return end(State::Out(m.version, Why::CannotWrite));
    }
    STOP.store(false, Ordering::Relaxed);
    DONE.store(0, Ordering::Relaxed);
    set(State::Fetching(m.version.clone(), 0, m.size));
    crate::config::log(&format!("update: fetching version {}", m.version));
    let last = Mutex::new(Instant::now());
    let progress = |got: u64| {
        DONE.store(got, Ordering::Relaxed);
        let mut at = last.lock().unwrap_or_else(|e| e.into_inner());
        if at.elapsed() >= Duration::from_millis(120) {
            *at = Instant::now();
            ctx.request_repaint();
        }
    };
    let part = match fetch(src, &places, &m, &STOP, &progress) {
        Ok(part) => part,
        Err(Halt::Stopped) => {
            crate::config::log("update: the fetch was stopped");
            return end(State::Out(m.version, Why::Stopped));
        }
        Err(Halt::Failed(why)) => {
            crate::config::log(&format!("update: fetching version {} did not work: {why}", m.version));
            return end(State::Failed);
        }
    };
    places.forget_staged();
    if std::fs::rename(&part, &places.new).is_err() {
        return end(State::Failed);
    }
    if !runs_as(&places.new, &m.version) {
        crate::config::log(&format!("update: version {} did not start on this PC or did not say it is {}; removed", m.version, m.version));
        places.forget_staged();
        return end(State::Failed);
    }
    let note = serde_json::to_string_pretty(&m).unwrap_or_default();
    if crate::util::write_atomic(&places.note, note.as_bytes()).is_err() {
        places.forget_staged();
        return end(State::Failed);
    }
    crate::config::log(&format!("update: version {} is ready beside the app", m.version));
    end(State::Ready(m.version))
}

// ------------------------------------------------------------------ at the start and the end of the app

/// `--self-check`: the program file says which version it is and ends, without a window. An updating copy
/// of the app asks a freshly fetched file this before it lets it take its place.
pub fn answered_self_check() -> bool {
    if !std::env::args().any(|a| a == "--self-check") {
        return false;
    }
    println!("{}", self_check_answer(env!("CARGO_PKG_VERSION")));
    true
}

/// `--after-update <process number>`: this program file was just put in place by the copy with that
/// number, which is closing. Wait for it to be gone (15 seconds at most), so the two never run side by
/// side. True when the app was started this way. (`--after-update guarded`: started by the guard of a
/// first start, which holds nothing of the app's and stays on; there is nothing to wait for.)
pub fn started_after_update() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--after-update") else { return false };
    if let Some(pid) = args.get(i + 1).and_then(|n| n.parse::<u32>().ok()) {
        wait_for_gone(pid);
    }
    true
}

/// Waits for the process with that number to end, 15 seconds at most.
fn wait_for_gone(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 15_000);
            CloseHandle(h);
        }
    }
}

fn start_again(exe: &Path) -> bool {
    std::process::Command::new(exe).arg("--after-update").arg(std::process::id().to_string()).spawn().is_ok()
}

// ------------------------------------------------------------------ the first start of a new version

/// How long a new version has, from the guard starting it, to have its window up and drawing. A lab may
/// make it shorter (`MUSIC_RECS_UPDATE_WITHIN`, seconds, only in a test run).
const FIRST_START_WITHIN: Duration = Duration::from_secs(120);
/// How long the window must have been up and drawing before a new version counts as started properly.
const SETTLED_AFTER: f64 = 5.0;
/// Starts that never got the window up, after which the next start goes back by itself (for when no guard
/// was there to do it).
const TRIES: u32 = 2;

/// `<name>.update-pending.json`: a version was just put in and has not yet started properly.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
struct Pending {
    /// the version that was put in
    version: String,
    /// the version it replaced, which is the old file
    from: String,
    /// how many times it has been started so far
    #[serde(default)]
    starts: u32,
}

/// `<name>.update-refused.json`: the last version that could not start on this PC and was rolled back. It
/// is never fetched or put in again; a later version is.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
struct Refused {
    version: String,
    back_to: String,
    why: String,
    at: String,
    /// the app that went back has said so once
    #[serde(default)]
    told: bool,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> bool {
    serde_json::to_string_pretty(value).is_ok_and(|t| crate::util::write_atomic(path, t.as_bytes()).is_ok())
}

fn pending(p: &Places) -> Option<Pending> {
    read_json(&p.pending)
}

fn refused(p: &Places) -> Option<Refused> {
    read_json(&p.refused)
}

/// True when `version` is the one that could not start here and was rolled back.
fn is_refused(p: &Places, version: &str) -> bool {
    refused(p).is_some_and(|r| r.version == version)
}

/// The swap, and the note that the new version has yet to start properly.
fn put_in(p: &Places, m: &Manifest) -> Result<(), String> {
    swap(p)?;
    if !write_json(&p.pending, &Pending { version: m.version.clone(), from: current(), starts: 0 }) {
        crate::config::log("update: the note that the new version has yet to start could not be written; its first start is not guarded");
    }
    Ok(())
}

/// Puts the old program file back under the app's name and notes the version that could not start as
/// refused. Only while the note of a pending first start is there: a version that started properly once is
/// never rolled back, and a rollback happens once (it removes the note).
fn roll_back(p: &Places, why: &str) -> Result<Refused, String> {
    let Some(note) = pending(p) else { return Err("the new version had started properly, so nothing was rolled back".into()) };
    if !p.old.exists() {
        let _ = std::fs::remove_file(&p.pending);
        return Err("the version before it is no longer there to go back to".into());
    }
    let r = Refused { version: note.version, back_to: note.from, why: why.to_string(), at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), told: false };
    // the note first, so this version is never put in again even if what follows goes wrong
    if !write_json(&p.refused, &r) {
        return Err("the note of the version that could not start could not be written, so nothing was rolled back".into());
    }
    let _ = std::fs::remove_file(&p.failed);
    std::fs::rename(&p.exe, &p.failed).map_err(|e| said("the new program file could not be moved aside.", &e))?;
    if let Err(e) = std::fs::rename(&p.old, &p.exe) {
        let back = std::fs::rename(&p.failed, &p.exe);
        return Err(format!("{} {}", said("the old program file could not take its place again.", &e), if back.is_ok() { "The new one is back in place." } else { "THE NEW ONE COULD NOT BE PUT BACK." }));
    }
    let _ = std::fs::remove_file(&p.pending);
    // (while it still runs it cannot be removed: then it goes at the next start)
    let _ = std::fs::remove_file(&p.failed);
    Ok(r)
}

/// A start of a version whose first start is still pending is counted. Some(starts) when it has had
/// `TRIES` starts already without its window coming up, and should go back now.
fn count_start(p: &Places) -> Option<u32> {
    let mut note = pending(p)?;
    if note.starts >= TRIES {
        return Some(note.starts);
    }
    note.starts += 1;
    write_json(&p.pending, &note);
    None
}

/// How a new version's first start went.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// its window was up and drawing long enough: it removed the note itself
    Started,
    /// it ended on its own, without an error, before that (closed at once, say): no verdict
    LeftEarly,
    /// it ended with an error before that (its exit code)
    Crashed(Option<i32>),
    /// it was not that far in time and was ended
    Hung,
}

/// Watches the first start of `child` until it has started properly, has ended, or `within` has passed.
fn watch(p: &Places, child: &mut std::process::Child, within: Duration) -> Verdict {
    let began = Instant::now();
    loop {
        if !p.pending.exists() {
            return Verdict::Started;
        }
        match child.try_wait() {
            Ok(Some(status)) if p.pending.exists() => return if status.success() { Verdict::LeftEarly } else { Verdict::Crashed(status.code()) },
            Ok(Some(_)) => return Verdict::Started,
            Ok(None) if began.elapsed() < within => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return if p.pending.exists() { Verdict::Hung } else { Verdict::Started };
            }
        }
    }
}

/// Rolls back and starts the version that is back. True when it went back.
fn go_back(p: &Places, why: &str) -> bool {
    match roll_back(p, why) {
        Ok(r) => {
            crate::config::log(&format!("update: version {} could not start ({why}); went back to version {}, and {} is not put in again", r.version, r.back_to, r.version));
            if !start_again(&p.exe) {
                crate::config::log("update: the version that is back could not be started; it starts the next time the app is opened");
            }
            true
        }
        Err(e) => {
            crate::config::log(&format!("update: going back did not work: {e}"));
            false
        }
    }
}

/// Starts the new version under the app's name and watches its first start: if it ends with an error or
/// hangs before its window is up, the old one goes back.
fn guard(p: &Places, within: Duration) {
    let version = pending(p).map(|n| n.version).unwrap_or_default();
    let why = match std::process::Command::new(&p.exe).arg("--after-update").arg("guarded").spawn() {
        Err(e) => crate::util::plain_err(format!("Windows could not start it ({e})")),
        Ok(mut child) => match watch(p, &mut child, within) {
            Verdict::Started => return crate::config::log(&format!("update: version {version} started properly")),
            Verdict::LeftEarly => return crate::config::log(&format!("update: version {version} was closed before its window was up, without an error; it stays, and its next starts tell")),
            Verdict::Crashed(code) => format!("it closed with an error{} before its window was up", code.map(|c| format!(" (code {c:#x})")).unwrap_or_default()),
            Verdict::Hung => format!("its window was not up and working within {} seconds, so it was ended", within.as_secs()),
        },
    };
    go_back(p, &why);
}

/// `--guard-update <program file> <process number>`: started from the old program file by the copy that
/// just put an update in. Waits for that copy to be gone, then starts the new version and guards its first
/// start. True when the app was started this way (it then ends, with no window).
pub fn guarded_update() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--guard-update") else { return false };
    let Some(exe) = args.get(i + 1) else { return true };
    if let Some(pid) = args.get(i + 2).and_then(|n| n.parse::<u32>().ok()) {
        wait_for_gone(pid);
    }
    let within = test_data().and(std::env::var("MUSIC_RECS_UPDATE_WITHIN").ok()).and_then(|s| s.parse::<u64>().ok()).map_or(FIRST_START_WITHIN, Duration::from_secs);
    guard(&Places::of(Path::new(exe)), within);
    true
}

/// After a swap: the old program file, unseen, guards the new version's first start. If the guard cannot
/// be started the new version is started without one. True when something was started.
fn hand_over(p: &Places) -> bool {
    // (it opens no window: `guarded_update` ends the app's start before any)
    let guard = std::process::Command::new(&p.old).arg("--guard-update").arg(&p.exe).arg(std::process::id().to_string()).spawn();
    if guard.is_ok() {
        return true;
    }
    crate::config::log("update: the guard could not be started; the new version starts without one");
    start_again(&p.exe)
}

/// Where this run stands on confirming its own first start: 0 not looked yet, 1 watching, 2 nothing to do.
static SETTLE: AtomicU8 = AtomicU8::new(0);

/// With every frame: when this is the first start of a version just put in and its window has been up and
/// drawing for five seconds, the note is removed. It started properly and is never rolled back.
fn settle(ctx: &egui::Context) {
    static SINCE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    match SETTLE.load(Ordering::Relaxed) {
        2 => return,
        0 => {
            let watched = Places::here().is_some_and(|p| p.pending.exists());
            SETTLE.store(if watched { 1 } else { 2 }, Ordering::Relaxed);
            if !watched {
                return;
            }
            lab_fault();
        }
        _ => {}
    }
    let now = ctx.input(|i| i.time);
    let since = *SINCE.get_or_init(|| now);
    if now - since < SETTLED_AFTER {
        ctx.request_repaint_after(Duration::from_secs_f64(SETTLED_AFTER - (now - since) + 0.05));
        return;
    }
    SETTLE.store(2, Ordering::Relaxed);
    let Some(p) = Places::here() else { return };
    let version = pending(&p).map(|n| n.version).unwrap_or_default();
    if std::fs::remove_file(&p.pending).is_ok() {
        crate::config::log(&format!("update: version {version} has its window up and working; it stays"));
    }
}

/// A lab can have a new version fail its first start (`MUSIC_RECS_UPDATE_FAULT`, only in a test run):
/// "crash" ends it with an error, "hang" stops its window from drawing, "quiet" closes it without an error.
fn lab_fault() {
    let Some(fault) = test_data().and(std::env::var("MUSIC_RECS_UPDATE_FAULT").ok()) else { return };
    match fault.as_str() {
        "crash" => std::process::abort(),
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        "quiet" => std::process::exit(0),
        _ => {}
    }
}

/// What the app says once after it went back from a version that could not start: the first time this
/// is asked after the rollback, and for the rest of that run.
pub fn notice() -> Option<String> {
    static SAID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SAID.get_or_init(|| {
        let p = Places::here()?;
        let mut r = refused(&p).filter(|r| !r.told)?;
        r.told = true;
        write_json(&p.refused, &r);
        Some(format!("Version {} could not start properly on this PC, so the app went back to version {}. It will not try {} again; a later version will come as usual.", r.version, r.back_to, r.version))
    })
    .clone()
}

// ------------------------------------------------------------------ at the start and the end of the app

/// At a plain start, before the window: an update that lies ready is put in and handed over to. True when
/// that happened, and this copy should end at once. First, when this is a version that has been started
/// `TRIES` times without its window coming up (and no guard went back), the old one goes back now.
pub fn put_in_at_start() -> bool {
    let Some(p) = Places::here() else { return false };
    // (a version that could not start and was still running when it was moved aside goes now)
    if p.failed.exists() {
        let _ = std::fs::remove_file(&p.failed);
    }
    if let Some(tries) = count_start(&p) {
        if go_back(&p, &format!("its window was not up on {tries} starts")) {
            return true;
        }
    }
    if source().is_none() {
        return false;
    }
    let Some(m) = staged(&p, &trusted_key(), &current()) else { return false };
    match put_in(&p, &m) {
        Ok(()) => {
            crate::config::log(&format!("update: version {} put in at the start (the one before is kept as {})", m.version, p.old.file_name().unwrap_or_default().to_string_lossy()));
            if hand_over(&p) {
                return true;
            }
            crate::config::log("update: the new version could not be started; carrying on with this one, which is the old one still running");
            // (this run is the old version: it must not take the note for the new one's)
            SETTLE.store(2, Ordering::Relaxed);
            false
        }
        Err(why) => {
            crate::config::log(&format!("update: version {} could not be put in: {why}", m.version));
            false
        }
    }
}

/// "Restart to update": the app closes the usual way, then `after_close` does the rest.
pub fn ask_restart() {
    RESTART.store(true, Ordering::Relaxed);
}

/// After the window has closed and everything has been saved: if a restart for an update was asked for,
/// put the update in and hand over to it. If it cannot be put in, the app as it was starts again.
pub fn after_close() {
    if !RESTART.load(Ordering::Relaxed) {
        return;
    }
    let Some(p) = Places::here() else { return };
    if let Some(m) = staged(&p, &trusted_key(), &current()) {
        match put_in(&p, &m) {
            Ok(()) => {
                crate::config::log(&format!("update: version {} put in on \"Restart to update\"", m.version));
                hand_over(&p);
                return;
            }
            Err(why) => crate::config::log(&format!("update: version {} could not be put in: {why}", m.version)),
        }
    }
    start_again(&p.exe);
}

#[cfg(test)]
mod tests {
    use super::*;

    // A key made only for these tests (its private half was thrown away) and what it signed.
    const KEY: &str = "83960de7fda01e71609942c5a27c947c0b8999bb09551467f4cb039b491f5ba376ede663bc5d3cc25568c215427d9f37f80a210eaa9e5cc1fc1b047a9b413ae6";
    const BYTES: &[u8] = b"not a real program, only bytes to sign";

    fn signed() -> Manifest {
        Manifest {
            version: "1.3.0".into(),
            size: 38,
            sha256: "54680ea1bb882f6c3d2ca4c4b11869d9e2695b8dada0b6c3a9557f2bd3f8e342".into(),
            signature: "109e8509a6d4f214786c7f25e590620615a9fce1750e6a2f8ce650368d44b5e4bf36f578681e8033bb5df6832ad71eeffca5ce9bb607f30c8f47f4ac8ee2c468".into(),
        }
    }

    fn lab(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("music-tool-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A folder that stands in for GitHub with the signed file in it, and an app folder beside it.
    fn release(name: &str) -> (Source, Places, PathBuf) {
        let dir = lab(name);
        let from = dir.join("release");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join(FILE), BYTES).unwrap();
        std::fs::write(from.join("update.json"), serde_json::to_string(&signed()).unwrap()).unwrap();
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("tool.exe"), b"the app as it is").unwrap();
        (Source::Folder(from), Places::of(&app.join("tool.exe")), dir)
    }

    #[test]
    fn versions_are_three_numbers_and_compared_as_numbers() {
        assert_eq!(triple("1.3.0"), Some((1, 3, 0)));
        for bad in ["1.3", "1.3.0.1", "v1.3.0", "1.3.x", "", "1..0", "1.3.0-beta", " 1.3.0"] {
            assert_eq!(triple(bad), None, "{bad}");
        }
        assert!(newer("1.10.0", "1.9.9"));
        assert!(newer("2.0.0", "1.99.99"));
        assert!(!newer("1.3.0", "1.3.0"));
        assert!(!newer("1.2.9", "1.3.0"));
        assert!(!newer("nonsense", "1.3.0") && !newer("1.3.0", "nonsense"));
    }

    #[test]
    fn the_authors_key_is_a_whole_key_and_the_addresses_are_https() {
        assert!(unhex::<64>(PUBLIC_KEY).is_some());
        assert_ne!(PUBLIC_KEY, KEY, "the tests' key must never be the one the app trusts");
        for a in [REPO, KOFI, CADENCE] {
            assert!(a.starts_with("https://") && crate::util::is_web_address(a), "{a}");
        }
    }

    #[test]
    fn a_signature_holds_only_for_exactly_what_was_signed_and_only_under_its_key() {
        let m = signed();
        assert!(m.signed_by(KEY));
        assert!(!m.signed_by(PUBLIC_KEY), "another key");
        let changed = [
            Manifest { version: "1.3.1".into(), ..m.clone() },
            Manifest { size: 39, ..m.clone() },
            Manifest { sha256: m.sha256.replace('5', "6"), ..m.clone() },
            Manifest { signature: format!("{}0", &m.signature[..127]), ..m.clone() },
        ];
        for c in changed {
            assert!(!c.signed_by(KEY), "{c:?}");
        }
        assert!(!m.signed_by("not a key") && !m.signed_by(""));
    }

    #[test]
    fn a_manifest_that_is_not_whole_is_not_read() {
        let good = serde_json::to_string(&signed()).unwrap();
        assert_eq!(Manifest::read(&good), Some(signed()));
        for bad in [
            good.replace("1.3.0", "latest"),
            good.replace("\"size\":38", "\"size\":0"),
            good.replace("\"size\":38", &format!("\"size\":{}", MOST + 1)),
            good.replace("54680ea1", "54680EA1"),
            good.replace("109e8509", "109e85"),
            "{}".to_string(),
            "<html>Not Found</html>".to_string(),
        ] {
            assert_eq!(Manifest::read(&bad), None, "{bad}");
        }
    }

    #[test]
    fn a_fetch_takes_the_named_file_and_carries_on_from_a_part() {
        let (src, p, dir) = release("fetch");
        let m = signed();
        let seen = Mutex::new(vec![]);
        let part = fetch(&src, &p, &m, &AtomicBool::new(false), &|n| seen.lock().unwrap().push(n)).ok().expect("fetched");
        assert_eq!(std::fs::read(&part).unwrap(), BYTES);
        assert_eq!(seen.lock().unwrap().last(), Some(&38));

        // cut off after ten bytes: the next try starts from there, and a part of another version goes
        std::fs::write(&part, &BYTES[..10]).unwrap();
        std::fs::write(p.part("1.2.0"), b"older").unwrap();
        let seen = Mutex::new(vec![]);
        let part = fetch(&src, &p, &m, &AtomicBool::new(false), &|n| seen.lock().unwrap().push(n)).ok().expect("carried on");
        assert_eq!(std::fs::read(&part).unwrap(), BYTES);
        assert_eq!(seen.lock().unwrap().first(), Some(&10), "it began where the part ended");
        assert!(!p.part("1.2.0").exists());

        // a part that does not belong to this file is found out by the checksum and thrown away
        std::fs::write(&part, b"XXXXXXXXXX").unwrap();
        assert!(matches!(fetch(&src, &p, &m, &AtomicBool::new(false), &|_| {}), Err(Halt::Failed(_))));
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_that_is_not_the_one_named_is_refused() {
        let (src, p, dir) = release("refused");
        let Source::Folder(from) = &src else { unreachable!() };
        // same size, other bytes
        std::fs::write(from.join(FILE), b"not a real program, only bytes to SIGN").unwrap();
        assert!(matches!(fetch(&src, &p, &signed(), &AtomicBool::new(false), &|_| {}), Err(Halt::Failed(_))));
        assert!(!p.part("1.3.0").exists());
        // longer than named
        std::fs::write(from.join(FILE), [BYTES, b"and more"].concat()).unwrap();
        assert!(matches!(fetch(&src, &p, &signed(), &AtomicBool::new(false), &|_| {}), Err(Halt::Failed(_))));
        // shorter
        std::fs::write(from.join(FILE), &BYTES[..20]).unwrap();
        assert!(matches!(fetch(&src, &p, &signed(), &AtomicBool::new(false), &|_| {}), Err(Halt::Failed(_))));
        // stopped before anything arrives: nothing is taken
        std::fs::write(from.join(FILE), BYTES).unwrap();
        assert!(matches!(fetch(&src, &p, &signed(), &AtomicBool::new(true), &|_| {}), Err(Halt::Stopped)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn what_lies_ready_is_proven_again_and_removed_when_it_does_not_hold() {
        let (_, p, dir) = release("staged");
        let lay = |bytes: &[u8], note: &Manifest| {
            std::fs::write(&p.new, bytes).unwrap();
            std::fs::write(&p.note, serde_json::to_string(note).unwrap()).unwrap();
        };
        lay(BYTES, &signed());
        assert_eq!(staged(&p, KEY, "1.2.0"), Some(signed()));
        assert!(p.new.exists(), "a good one stays");

        // not later than the app itself
        assert_eq!(staged(&p, KEY, "1.3.0"), None);
        assert!(!p.new.exists() && !p.note.exists());
        // the file was changed after it was proven
        lay(b"not a real program, only bytes to SIGN", &signed());
        assert_eq!(staged(&p, KEY, "1.2.0"), None);
        assert!(!p.new.exists());
        // the note was changed (a version the signature is not for)
        lay(BYTES, &Manifest { version: "9.9.9".into(), ..signed() });
        assert_eq!(staged(&p, KEY, "1.2.0"), None);
        // signed, but not by the key the app trusts
        lay(BYTES, &signed());
        assert_eq!(staged(&p, PUBLIC_KEY, "1.2.0"), None);
        // a file with no note
        std::fs::write(&p.new, BYTES).unwrap();
        assert_eq!(staged(&p, KEY, "1.2.0"), None);
        assert!(!p.new.exists());
        // nothing there at all
        assert_eq!(staged(&p, KEY, "1.2.0"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_swap_keeps_one_old_file_and_undoes_itself_when_it_cannot_finish() {
        let (_, p, dir) = release("swap");
        std::fs::write(&p.new, b"the new app").unwrap();
        std::fs::write(&p.note, b"{}").unwrap();
        swap(&p).unwrap();
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the new app");
        assert_eq!(std::fs::read(&p.old).unwrap(), b"the app as it is");
        assert!(!p.new.exists() && !p.note.exists());

        // the next swap: the old file from before makes way, the one just replaced is the step back
        std::fs::write(&p.new, b"newer still").unwrap();
        swap(&p).unwrap();
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"newer still");
        assert_eq!(std::fs::read(&p.old).unwrap(), b"the new app");

        // no new file to put in: the app's file is back under its name afterwards
        assert!(swap(&p).is_err());
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"newer still");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_names_beside_the_program_file() {
        let p = Places::of(Path::new(r"C:\Apps\Tool\bradleys-music-tool.exe"));
        assert_eq!(p.new, Path::new(r"C:\Apps\Tool\bradleys-music-tool.new.exe"));
        assert_eq!(p.note, Path::new(r"C:\Apps\Tool\bradleys-music-tool.new.json"));
        assert_eq!(p.old, Path::new(r"C:\Apps\Tool\bradleys-music-tool.old.exe"));
        assert_eq!(p.part("1.3.0"), Path::new(r"C:\Apps\Tool\bradleys-music-tool.new-1.3.0.part"));
        assert_eq!(p.pending, Path::new(r"C:\Apps\Tool\bradleys-music-tool.update-pending.json"));
        assert_eq!(p.refused, Path::new(r"C:\Apps\Tool\bradleys-music-tool.update-refused.json"));
        assert_eq!(p.failed, Path::new(r"C:\Apps\Tool\bradleys-music-tool.failed.exe"));
        assert!(in_build_folder(Path::new(r"C:\code\app\target\oneapp\release\bradleys-music-tool.exe")));
        assert!(in_build_folder(Path::new(r"C:\code\app\Target\release\x.exe")));
        assert!(!in_build_folder(Path::new(r"C:\Apps\Tool\bradleys-music-tool.exe")));
    }

    // ---------------------------------------------------------------- the first start of a new version

    /// An app folder just after a swap from 1.2.0 to 1.3.0: the new file under the app's name, the old one
    /// beside it, and the note that the new one has yet to start properly.
    fn swapped(name: &str) -> (Places, PathBuf) {
        let (_, p, dir) = release(name);
        std::fs::write(&p.new, b"the new app").unwrap();
        swap(&p).unwrap();
        assert!(write_json(&p.pending, &Pending { version: "1.3.0".into(), from: "1.2.0".into(), starts: 0 }));
        (p, dir)
    }

    /// A stand-in for the new version's first start: a real process that ends with `code`, or keeps
    /// running for about `seconds`.
    fn first_start(code: Option<i32>, seconds: u32) -> std::process::Child {
        let what = match code {
            Some(c) => format!("exit {c}"),
            None => format!("ping -n {} 127.0.0.1 >nul", seconds + 1),
        };
        std::process::Command::new("cmd").args(["/c", &what]).stdout(std::process::Stdio::null()).spawn().unwrap()
    }

    #[test]
    fn a_first_start_that_gets_its_window_up_stays_and_is_never_rolled_back() {
        let (p, dir) = swapped("started");
        let mut child = first_start(None, 20);
        // the new version removes the note itself once its window has been up for a while
        let note = p.pending.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            std::fs::remove_file(note).unwrap();
        });
        assert_eq!(watch(&p, &mut child, Duration::from_secs(15)), Verdict::Started);
        assert!(child.try_wait().unwrap().is_none(), "a version that started properly is left running");
        let _ = child.kill();
        let _ = child.wait();
        // a later crash is not the first start's: there is nothing to roll back
        assert!(roll_back(&p, "crashed later").is_err());
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the new app");
        assert_eq!(std::fs::read(&p.old).unwrap(), b"the app as it is");
        assert!(refused(&p).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_first_start_that_crashes_goes_back_to_the_old_version() {
        let (p, dir) = swapped("crash");
        let mut child = first_start(Some(3), 0);
        assert_eq!(watch(&p, &mut child, Duration::from_secs(15)), Verdict::Crashed(Some(3)));
        let r = roll_back(&p, "it closed with an error").unwrap();
        assert_eq!((r.version.as_str(), r.back_to.as_str(), r.told), ("1.3.0", "1.2.0", false));
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the app as it is", "the old version is back under the app's name");
        assert!(!p.old.exists() && !p.failed.exists() && !p.pending.exists());
        assert!(is_refused(&p, "1.3.0") && !is_refused(&p, "1.3.1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_first_start_that_hangs_is_ended_and_goes_back() {
        let (p, dir) = swapped("hang");
        let mut child = first_start(None, 30);
        let began = Instant::now();
        assert_eq!(watch(&p, &mut child, Duration::from_secs(1)), Verdict::Hung);
        assert!(began.elapsed() < Duration::from_secs(10));
        assert!(child.try_wait().unwrap().is_some(), "the hung version was ended");
        roll_back(&p, "its window was not up").unwrap();
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the app as it is");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_first_start_closed_without_an_error_is_no_verdict() {
        let (p, dir) = swapped("quiet");
        let mut child = first_start(Some(0), 0);
        assert_eq!(watch(&p, &mut child, Duration::from_secs(15)), Verdict::LeftEarly);
        assert!(p.pending.exists(), "the note stays, so the next starts still tell");
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the new app");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn going_back_happens_once_and_the_refused_version_is_never_put_in_again() {
        let (p, dir) = swapped("loop");
        roll_back(&p, "it closed with an error").unwrap();
        // a second rollback has nothing to go back from: the version that is back stays
        assert!(roll_back(&p, "again").is_err());
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the app as it is");

        // the refused version, signed and fetched again, is not put in: no swap, no loop
        std::fs::write(&p.new, BYTES).unwrap();
        std::fs::write(&p.note, serde_json::to_string(&signed()).unwrap()).unwrap();
        assert_eq!(staged(&p, KEY, "1.2.0"), None);
        assert!(!p.new.exists() && !p.note.exists());
        // with no note of a refused version (another one was refused later), the same files would go in
        std::fs::remove_file(&p.refused).unwrap();
        std::fs::write(&p.new, BYTES).unwrap();
        std::fs::write(&p.note, serde_json::to_string(&signed()).unwrap()).unwrap();
        assert_eq!(staged(&p, KEY, "1.2.0"), Some(signed()));

        // with no old file to go back to, nothing is moved and the note goes, so no start tries again
        let (p, dir2) = swapped("no-old");
        std::fs::remove_file(&p.old).unwrap();
        assert!(roll_back(&p, "it closed with an error").is_err());
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the new app");
        assert!(!p.pending.exists());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn without_a_guard_the_third_start_that_never_got_its_window_up_goes_back() {
        let (p, dir) = swapped("unguarded");
        assert_eq!(count_start(&p), None, "the first start (the guarded one)");
        assert_eq!(count_start(&p), None, "the second");
        assert_eq!(count_start(&p), Some(2), "the third goes back");
        roll_back(&p, "its window was not up on 2 starts").unwrap();
        assert_eq!(count_start(&p), None, "the version that is back is not counted");
        assert_eq!(std::fs::read(&p.exe).unwrap(), b"the app as it is");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_notes_survive_being_read_back() {
        let (p, dir) = swapped("notes");
        assert_eq!(pending(&p), Some(Pending { version: "1.3.0".into(), from: "1.2.0".into(), starts: 0 }));
        // an older note without a count of starts still reads
        std::fs::write(&p.pending, r#"{"version":"1.3.0","from":"1.2.0"}"#).unwrap();
        assert_eq!(pending(&p).map(|n| n.starts), Some(0));
        std::fs::write(&p.pending, "not json").unwrap();
        assert!(pending(&p).is_none(), "a note that cannot be read rolls nothing back");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_choice_is_saved_as_a_word_and_anything_else_means_by_itself() {
        for p in [Policy::Auto, Policy::Tell, Policy::Off] {
            assert_eq!(Policy::from_saved(p.saved()), p);
        }
        assert_eq!(Policy::from_saved(""), Policy::Auto);
        assert_eq!(Policy::from_saved("whatever"), Policy::Auto);
        assert!(source().is_none(), "a unit test never updates anything");
    }
}
