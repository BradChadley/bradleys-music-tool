//! The helper programs the app fetches for itself, so nobody is sent to find them: ffmpeg (with ffprobe),
//! which imports and checks need, and the Soulseek client (slskd), which Downloads drives.
//!
//! One way in for each helper (`start`): a version pinned below with its size and SHA-256, fetched over
//! https only, saved under a temporary name beside where it goes, checked, unpacked, then moved into place.
//! A download that does not match is refused and removed, and nothing already in place is touched. A fetch
//! runs on its own thread; any page can start it, stop it or show how far it is (`state`), and there is
//! only ever one fetch of a helper at a time, whichever page started it.
//!
//! Where they go: ffmpeg into `<data>\tools\ffmpeg` (`importer::tools::find_tool` looks there after the
//! PATH, so a PC that already has ffmpeg keeps using its own), slskd into `<data>\slskd\bin`, with a
//! settings file the app writes when there is none (`slskd_settings`).
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Helper {
    Ffmpeg,
    Soulseek,
}

/// One pinned download: what is fetched, and what it must be to be taken.
pub struct Pin {
    pub version: &'static str,
    pub url: &'static str,
    pub size: u64,
    /// lower case hex
    pub sha256: &'static str,
}

/// ffmpeg 9.0.2, the "essentials" build from gyan.dev (the builds ffmpeg.org points to for Windows), as
/// published on its GitHub releases. 115 MB to fetch; it holds the MP3 encoder and the spectrogram filter
/// the app uses, and the whole test suite passes with it. The checksum is from a download made for this
/// pin, and matches the one GitHub lists for the file.
const FFMPEG: Pin = Pin {
    version: "9.0.2",
    url: "https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-essentials_build.zip",
    size: 114_768_076,
    sha256: "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba",
};

/// slskd 0.26.2 for 64-bit Windows, from slskd's own GitHub releases (the version the app was built
/// against). 56 MB to fetch. Checksum as for ffmpeg.
const SLSKD: Pin = Pin {
    version: "0.26.2",
    url: "https://github.com/slskd/slskd/releases/download/0.26.2/slskd-0.26.2-win-x64.zip",
    size: 55_815_398,
    sha256: "ffe186ecbedce15bf19b6a690c6213c669f34acc793ef53aa92390a5cd9c48f0",
};

/// The sentence for a download that is not the file it should be.
const MISMATCH: &str = "What arrived is not the file the app expects, so it was thrown away. Try again later.";

impl Helper {
    fn index(self) -> usize {
        match self {
            Helper::Ffmpeg => 0,
            Helper::Soulseek => 1,
        }
    }

    /// Its name in a sentence.
    pub fn name(self) -> &'static str {
        match self {
            Helper::Ffmpeg => "ffmpeg",
            Helper::Soulseek => "the Soulseek client",
        }
    }

    /// The word that fetches it.
    pub fn word(self) -> &'static str {
        match self {
            Helper::Ffmpeg => "Get ffmpeg",
            Helper::Soulseek => "Get the Soulseek client",
        }
    }

    pub fn pin(self) -> &'static Pin {
        match self {
            Helper::Ffmpeg => &FFMPEG,
            Helper::Soulseek => &SLSKD,
        }
    }

    /// The folder it is put in.
    pub fn home(self) -> PathBuf {
        match self {
            Helper::Ffmpeg => ffmpeg_dir(),
            Helper::Soulseek => crate::slskd::exe().parent().map(Path::to_path_buf).unwrap_or_else(|| crate::slskd::root().join("bin")),
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Helper::Ffmpeg => "ffmpeg",
            Helper::Soulseek => "slskd",
        }
    }

    /// The files that must be in the unpacked folder for it to be put in place.
    fn needs(self) -> &'static [&'static str] {
        match self {
            Helper::Ffmpeg => &["ffmpeg.exe", "ffprobe.exe"],
            Helper::Soulseek => &["slskd.exe"],
        }
    }

    /// Where a file of the archive goes, inside its folder; None for one that is left out. For ffmpeg only
    /// the two programs and the license (not ffplay, which plays sound, nor the manuals); for slskd all of
    /// it, as it comes.
    fn place(self, inside: &Path) -> Option<PathBuf> {
        let parts: Vec<&str> = inside.components().map(|c| c.as_os_str().to_str()).collect::<Option<_>>()?;
        match self {
            // "ffmpeg-9.0.2-essentials_build\bin\ffmpeg.exe"
            Helper::Ffmpeg => match parts.as_slice() {
                [_, "bin", exe] if ["ffmpeg.exe", "ffprobe.exe"].iter().any(|n| n.eq_ignore_ascii_case(exe)) => Some(PathBuf::from(exe.to_ascii_lowercase())),
                [_, doc] if ["LICENSE", "README.txt"].contains(doc) => Some(PathBuf::from(doc)),
                _ => None,
            },
            Helper::Soulseek => Some(inside.to_path_buf()),
        }
        .filter(|p| plain(p))
    }
}

/// Where the app keeps the ffmpeg it fetched.
pub fn ffmpeg_dir() -> PathBuf {
    crate::config::app_dir().join("tools").join("ffmpeg")
}

/// A sentence for a page from what went wrong on the disk: "The download could not be saved. There is not
/// enough space on the disk." (Windows' own words, without its "(os error 112)").
pub(crate) fn said(what: &str, e: &std::io::Error) -> String {
    let mut first = what.chars();
    let what: String = first.next().map(|c| c.to_uppercase().chain(first).collect()).unwrap_or_default();
    let raw = e.to_string();
    let why = raw.split(" (os error").next().unwrap_or(&raw).trim().trim_end_matches('.');
    if why.is_empty() { what } else { format!("{what} {why}.") }
}

/// A path made only of plain names: nothing that climbs out of a folder or names a drive.
fn plain(p: &Path) -> bool {
    p.components().next().is_some() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

// ------------------------------------------------------------------ the fetch, one at a time per helper

/// Where a fetch stands, for a page to show.
#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// nothing going on: never asked for, done, or stopped
    Idle,
    /// being fetched: bytes so far, of how many
    Getting(u64, u64),
    /// fetched and checked; being unpacked and put in place
    Unpacking,
    /// it did not work: why, in a sentence
    Failed(String),
}

#[derive(Default)]
struct Job {
    done: AtomicU64,
    /// 0 fetching, 1 unpacking, 2 over
    stage: AtomicU8,
    stop: AtomicBool,
    /// set when it is over and did not work (not for a fetch that was stopped)
    why: Mutex<Option<String>>,
}

static JOBS: [Mutex<Option<Arc<Job>>>; 2] = [Mutex::new(None), Mutex::new(None)];
/// Counts each time a helper was put in place, so a page that was waiting for it can look again.
static ARRIVED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

fn job(h: Helper) -> Option<Arc<Job>> {
    JOBS[h.index()].lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn state(h: Helper) -> State {
    let Some(j) = job(h) else { return State::Idle };
    match j.stage.load(Ordering::Relaxed) {
        0 => State::Getting(j.done.load(Ordering::Relaxed), h.pin().size),
        1 => State::Unpacking,
        _ => j.why.lock().unwrap_or_else(|e| e.into_inner()).clone().map(State::Failed).unwrap_or(State::Idle),
    }
}

/// True while it is being fetched.
pub fn busy(h: Helper) -> bool {
    matches!(state(h), State::Getting(..) | State::Unpacking)
}

/// How many times this helper has been put in place since the app started. A page that showed it missing
/// keeps the number it saw and looks again when it changes.
pub fn arrived(h: Helper) -> u64 {
    ARRIVED[h.index()].load(Ordering::Relaxed)
}

/// Starts fetching a helper, unless it is already being fetched. Returns at once; `state` says how it goes.
pub fn start(h: Helper, ctx: &egui::Context) {
    let ctx = ctx.clone();
    start_with(h, move || ctx.request_repaint());
}

/// Whether the helper is there to be used, and where: ffmpeg wherever the importer finds it (the PATH, its
/// own folder, WinGet's), slskd in its own folder only.
pub fn found(h: Helper) -> Option<PathBuf> {
    match h {
        Helper::Ffmpeg => crate::importer::tools::find_tool("ffmpeg").ok().filter(|_| crate::importer::tools::find_tool("ffprobe").is_ok()),
        Helper::Soulseek => Some(crate::slskd::exe()).filter(|p| p.is_file()),
    }
}

/// The name used for it in Cadence's engine conversation ("ffmpeg", "slskd").
pub fn id(h: Helper) -> &'static str {
    h.slug()
}

/// `start` without a window: `changed` is called (at most every 120 ms while bytes arrive, and once at the
/// end) whenever `state` has something new. Cadence's engine uses this. False when a fetch of this helper
/// was already going (that one goes on; `changed` is not called for it).
pub fn start_with(h: Helper, changed: impl Fn() + Send + 'static) -> bool {
    let mut slot = JOBS[h.index()].lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|j| j.stage.load(Ordering::Relaxed) < 2) {
        return false;
    }
    let j = Arc::new(Job::default());
    *slot = Some(j.clone());
    drop(slot);
    std::thread::spawn(move || {
        let pin = h.pin();
        crate::config::log(&format!("helpers: fetching {} {} from {}", h.slug(), pin.version, pin.url));
        let last = Mutex::new(Instant::now());
        let tick = || {
            let mut at = last.lock().unwrap_or_else(|e| e.into_inner());
            if at.elapsed() >= Duration::from_millis(120) {
                *at = Instant::now();
                changed();
            }
        };
        let got = open(pin.url).and_then(|body| install(h, body, &h.home(), &j, &tick));
        match &got {
            // (stopped, whatever happened after the click)
            Err(_) if j.stop.load(Ordering::Relaxed) => crate::config::log(&format!("helpers: fetching {} was stopped", h.slug())),
            Ok(()) => {
                crate::config::log(&format!("helpers: {} {} is in place in {}", h.slug(), pin.version, h.home().display()));
                ARRIVED[h.index()].fetch_add(1, Ordering::Relaxed);
            }
            Err(why) if why.is_empty() => crate::config::log(&format!("helpers: fetching {} was stopped", h.slug())),
            Err(why) => {
                crate::config::log(&format!("helpers: fetching {} did not work: {why}", h.slug()));
                *j.why.lock().unwrap_or_else(|e| e.into_inner()) = Some(why.clone());
            }
        }
        j.stage.store(2, Ordering::Relaxed);
        changed();
    });
    true
}

/// Stops a fetch. What was fetched so far is removed; nothing already in place is touched.
pub fn stop(h: Helper) {
    if let Some(j) = job(h) {
        j.stop.store(true, Ordering::Relaxed);
        // the page says so at once, even while a read is still waiting on the network
        if j.stage.load(Ordering::Relaxed) == 0 {
            j.stage.store(2, Ordering::Relaxed);
        }
    }
}

/// The download itself, over https only (a redirect included). Errors are sentences for the page.
fn open(url: &str) -> Result<Box<dyn Read + Send>, String> {
    if !url.starts_with("https://") {
        return Err("Helpers are only fetched over a secure (https) address.".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(Duration::from_secs(3 * 3600)))
        .http_status_as_error(false)
        .user_agent(crate::net::USER_AGENT)
        .build()
        .into();
    let res = agent.get(url).call().map_err(|_| "The download could not start. Check the internet connection, then try again.".to_string())?;
    let status = res.status().as_u16();
    if status != 200 {
        return Err(format!("The download could not start (the site answered with error {status}). Try again later."));
    }
    Ok(Box::new(res.into_body().into_reader()))
}

/// Takes a helper in from `body` (the download as it arrives) and puts it in `home`: saved under a
/// temporary name beside it, checked against its pin, unpacked, then moved into place. On any failure, or
/// when stopped (an empty error), everything it made is removed and `home` is as it was.
fn install(h: Helper, body: impl Read, home: &Path, job: &Job, tick: &dyn Fn()) -> Result<(), String> {
    let base = home.parent().ok_or("The app's data folder could not be found.")?;
    std::fs::create_dir_all(base).map_err(|e| said("The app's data folder could not be written to.", &e))?;
    clear_leftovers(base, h);
    // (named with this app's process number, so a later fetch can tell whether its maker is still running)
    let work = Work::new(base.join(format!(".getting-{}-{}-{:x}", h.slug(), std::process::id(), crate::importer::scan::rand_id())))?;
    let file = work.0.join(format!("{}.zip.part", h.slug()));
    receive(body, &file, h.pin(), job, tick)?;
    job.stage.store(1, Ordering::Relaxed);
    tick();
    let unpacked = work.0.join("unpacked");
    unpack(&file, &unpacked, h, job)?;
    let _ = std::fs::remove_file(&file);
    if job.stop.load(Ordering::Relaxed) {
        return Err(String::new());
    }
    if h == Helper::Soulseek {
        write_settings_if_missing(&crate::slskd::settings_file())?;
    }
    put_in_place(&unpacked, home, &work.0, h)
}

/// A folder of work that is removed with everything in it when it goes out of use.
struct Work(PathBuf);

impl Work {
    fn new(dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&dir).map_err(|e| said("The app's data folder could not be written to.", &e))?;
        Ok(Work(dir))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Removes what a fetch that was cut off (the app closed in the middle) left beside the helper's folder.
/// Each such folder is named with the process number of the app that made it: one whose app is no longer
/// running goes; one of a copy of the app still running, or of this one (a stopped fetch tidies up after
/// itself), is left alone.
fn clear_leftovers(base: &Path, h: Helper) {
    let prefix = format!(".getting-{}-", h.slug());
    let Ok(list) = std::fs::read_dir(base) else { return };
    for e in list.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix).filter(|_| e.path().is_dir()) else { continue };
        let maker = rest.split('-').next().and_then(|n| n.parse::<u32>().ok());
        if maker.is_none_or(|pid| pid != std::process::id() && !running(pid)) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Whether a process with this number is running now.
fn running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        alive
    }
}

/// Saves the download to `to`, counting and hashing it as it arrives. Taken only when it is exactly the
/// pinned size and its SHA-256 is the pinned one; otherwise the file is removed and the fetch refused.
fn receive(mut body: impl Read, to: &Path, pin: &Pin, job: &Job, tick: &dyn Fn()) -> Result<(), String> {
    let refuse = |why: &str| {
        let _ = std::fs::remove_file(to);
        Err(why.to_string())
    };
    let mut out = std::fs::File::create(to).map_err(|e| said("The download could not be saved.", &e))?;
    let mut hash = Sha256::new()?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0u64;
    loop {
        if job.stop.load(Ordering::Relaxed) {
            drop(out);
            return refuse("");
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                drop(out);
                return refuse("The download broke off. Check the internet connection, then try again.");
            }
        };
        got += n as u64;
        if got > pin.size {
            drop(out);
            crate::config::log(&format!("helpers: {} sent more than the {} bytes pinned; refused", pin.url, pin.size));
            return refuse(MISMATCH);
        }
        hash.update(&buf[..n]);
        if let Err(e) = out.write_all(&buf[..n]) {
            drop(out);
            return refuse(&said("The download could not be saved.", &e));
        }
        job.done.store(got, Ordering::Relaxed);
        tick();
        if slow_test() {
            // about 6 MB a second
            std::thread::sleep(Duration::from_micros(n as u64 / 6));
        }
    }
    let flushed = out.flush().and_then(|_| out.sync_all());
    drop(out);
    if got < pin.size {
        return refuse("The download ended early. Check the internet connection, then try again.");
    }
    let seen = hex(&hash.finish()?);
    if seen != pin.sha256 {
        crate::config::log(&format!("helpers: {} has SHA-256 {seen}, not the pinned {}; refused and removed", pin.url, pin.sha256));
        return refuse(MISMATCH);
    }
    if let Err(e) = flushed {
        return refuse(&said("The download could not be saved.", &e));
    }
    Ok(())
}

/// A test run can make a fetch take its time (`MUSIC_RECS_SLOW_FETCH`, only with `MUSIC_RECS_DATA` set), so
/// its progress and "Stop" can be seen on a fast line.
fn slow_test() -> bool {
    static SLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SLOW.get_or_init(|| {
        let set = |v: &str| std::env::var_os(v).is_some_and(|v| !v.is_empty());
        set("MUSIC_RECS_DATA") && set("MUSIC_RECS_SLOW_FETCH")
    })
}

/// Unpacks the files of the archive that the helper keeps (`Helper::place`) into `into`. A name that
/// would reach outside that folder is never written.
fn unpack(zip_file: &Path, into: &Path, h: Helper, job: &Job) -> Result<(), String> {
    let broken = || format!("The download of {} could not be unpacked. Try again.", h.name());
    let f = std::fs::File::open(zip_file).map_err(|e| said("The download could not be opened.", &e))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(|_| broken())?;
    std::fs::create_dir_all(into).map_err(|e| said("The app's data folder could not be written to.", &e))?;
    for i in 0..zip.len() {
        if job.stop.load(Ordering::Relaxed) {
            return Err(String::new());
        }
        let mut entry = zip.by_index(i).map_err(|_| broken())?;
        if entry.is_dir() {
            continue;
        }
        let Some(rel) = entry.enclosed_name().and_then(|inside| h.place(&inside)) else { continue };
        let to = into.join(&rel);
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| said("The app's data folder could not be written to.", &e))?;
        }
        let mut out = std::fs::File::create(&to).map_err(|e| said(&format!("{} could not be unpacked.", h.name()), &e))?;
        std::io::copy(&mut entry, &mut out).map_err(|_| broken())?;
    }
    match h.needs().iter().find(|n| !into.join(n).is_file()) {
        Some(n) => Err(format!("The download of {} did not hold {n}. Try again later.", h.name())),
        None => Ok(()),
    }
}

/// Moves the unpacked folder to `home`. One already there (an older copy) is moved aside first and
/// removed with the work folder; if the new one cannot go in, the old one is put back.
fn put_in_place(unpacked: &Path, home: &Path, work: &Path, h: Helper) -> Result<(), String> {
    let old = work.join("old");
    let had = home.exists();
    if had {
        std::fs::rename(home, &old).map_err(|_| format!("The folder {} goes in is in use. Close what is using it, then try again.", h.name()))?;
    }
    if let Err(e) = std::fs::rename(unpacked, home) {
        if had {
            let _ = std::fs::rename(&old, home);
        }
        return Err(said(&format!("{} could not be put in place.", h.name()), &e));
    }
    Ok(())
}

// ------------------------------------------------------------------ slskd's settings file

/// The settings file the app writes for a Soulseek client it fetched: its web side on this PC only, on the
/// first free port from 5830; sign-in on (the app hands slskd a key and a web password at each start);
/// nothing shared; nothing changeable from outside; no web updates or version checks of its own; finished
/// downloads into `downloads` (the Downloads folder from Settings), the unfinished ones in its data folder.
/// It holds no secret: the Soulseek login stays in Credential Manager.
pub fn slskd_settings(web_port: u16, listen_port: u16, downloads: Option<&Path>, incomplete: &Path) -> String {
    // double quotes, with \ and " escaped: a folder name may hold ' or #
    let quoted = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
    let mut s = String::new();
    s.push_str("# slskd settings, written by Bradley's Music Tool. The app starts and stops slskd itself.\n");
    s.push_str("# The Soulseek login is kept in Windows Credential Manager, not here: save it in the app's Settings.\n");
    s.push_str("soulseek:\n");
    s.push_str(&format!("  listen_port: {listen_port}\n"));
    s.push('\n');
    s.push_str("instance_name: music-recs\n");
    s.push_str("remote_configuration: false\n");
    s.push_str("remote_file_management: false\n");
    s.push_str("flags:\n");
    s.push_str("  no_logo: true\n");
    s.push_str("  no_version_check: true\n");
    s.push('\n');
    s.push_str("# Only this computer can reach slskd (127.0.0.1), and only with the key the app hands it.\n");
    s.push_str("web:\n");
    s.push_str(&format!("  port: {web_port}\n"));
    s.push_str("  ip_address: 127.0.0.1\n");
    s.push_str("  https:\n");
    s.push_str("    disabled: true\n");
    s.push_str("  authentication:\n");
    s.push_str("    disabled: false\n");
    s.push_str("directories:\n");
    if let Some(d) = downloads {
        s.push_str(&format!("  downloads: {}\n", quoted(d)));
    }
    s.push_str(&format!("  incomplete: {}\n", quoted(incomplete)));
    s.push('\n');
    s.push_str("# Nothing is shared.\n");
    s.push_str("shares:\n");
    s.push_str("  directories: []\n");
    s
}

/// The first port from `from` that nothing on this PC is listening on, at `ip` (`from` itself if none of
/// the next hundred is free).
fn free_port(ip: [u8; 4], from: u16) -> u16 {
    (from..from.saturating_add(100)).find(|p| std::net::TcpListener::bind((std::net::Ipv4Addr::from(ip), *p)).is_ok()).unwrap_or(from)
}

/// Writes slskd's settings file when there is none (one that is there, his own included, is never
/// changed). Done before slskd itself is put in place, so the Downloads page never sees one without the other.
fn write_settings_if_missing(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let cfg = crate::config::Config::load();
    let downloads = Some(cfg.downloads_dir.trim()).filter(|d| !d.is_empty()).map(PathBuf::from);
    write_settings(path, downloads.as_deref())
}

/// The same for a known downloads folder. Also used when a Soulseek login is saved before the client
/// has ever been fetched (the first-start screen, Settings): the login needs the file to be there.
pub(crate) fn write_settings(path: &Path, downloads: Option<&Path>) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let data = path.parent().ok_or("The Soulseek client's data folder could not be found.")?;
    std::fs::create_dir_all(data).map_err(|e| said("The Soulseek client's settings could not be written.", &e))?;
    let text = slskd_settings(free_port([127, 0, 0, 1], 5830), free_port([0, 0, 0, 0], 50300), downloads, &data.join("incomplete"));
    // never a file the app would refuse to start slskd with
    crate::slskd::parse_settings(&text)?;
    crate::util::write_atomic(path, text.as_bytes()).map_err(|e| said("The Soulseek client's settings could not be written.", &e))
}

// ------------------------------------------------------------------ SHA-256, by Windows itself

/// SHA-256 through Windows' own cryptography (bcrypt), so no hashing library is built in.
pub(crate) struct Sha256(windows_sys::Win32::Security::Cryptography::BCRYPT_HASH_HANDLE);

impl Sha256 {
    pub(crate) fn new() -> Result<Self, String> {
        use windows_sys::Win32::Security::Cryptography::{BCryptCreateHash, BCRYPT_SHA256_ALG_HANDLE};
        let mut h = std::ptr::null_mut();
        let status = unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) };
        if status != 0 || h.is_null() {
            return Err("Windows could not check the download (its hashing would not start).".into());
        }
        Ok(Sha256(h))
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        use windows_sys::Win32::Security::Cryptography::BCryptHashData;
        for part in data.chunks(u32::MAX as usize) {
            unsafe { BCryptHashData(self.0, part.as_ptr(), part.len() as u32, 0) };
        }
    }

    pub(crate) fn finish(self) -> Result<[u8; 32], String> {
        use windows_sys::Win32::Security::Cryptography::BCryptFinishHash;
        let mut out = [0u8; 32];
        let status = unsafe { BCryptFinishHash(self.0, out.as_mut_ptr(), 32, 0) };
        if status != 0 {
            return Err("Windows could not check the download.".into());
        }
        Ok(out)
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Security::Cryptography::BCryptDestroyHash(self.0) };
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A word that does something: a blue link on the pages built that way (Health, Settings), a quiet gray
/// word on the pages in the newer look. Never broken over two lines: in a narrow row it goes to the next
/// line whole.
fn word(ui: &mut egui::Ui, text: &str, size: f32, blue: bool) -> egui::Response {
    use crate::theme;
    if !blue {
        return theme::quiet(ui, text, size, true);
    }
    let r = ui.add(egui::Label::new(egui::RichText::new(text).size(size).color(theme::ACCENT2())).wrap_mode(egui::TextWrapMode::Extend).selectable(false).sense(egui::Sense::click()));
    crate::script::seen(text, r.rect);
    if r.hovered() {
        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, egui::Stroke::new(1.0, theme::ACCENT2()));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

// ------------------------------------------------------------------ how pages show it

/// "42 of 115 MB"
pub fn amount(done: u64, total: u64) -> String {
    let mb = |b: u64| (b as f64 / 1_000_000.0).round() as u64;
    format!("{} of {} MB", mb(done).min(mb(total)), mb(total))
}

/// What a fetch under way is doing, in a few words ("Getting ffmpeg"), and how far it is (0 to 1; None
/// while it is unpacking, which has no count).
pub fn doing(h: Helper, s: &State) -> Option<(String, Option<f32>)> {
    match s {
        State::Getting(done, total) => Some((format!("Getting {}", h.name()), Some(*done as f32 / (*total).max(1) as f32))),
        State::Unpacking => Some((format!("Unpacking {}", h.name()), None)),
        _ => None,
    }
}

/// What a page shows where a helper is missing, on one line in the direction of `ui` (`rtl`: a
/// right-to-left row, which takes its pieces last first): `missing` (the sentence that says so) and the
/// word that fetches it; while it is fetched, how far it is with a thin line and "Stop"; when it did not
/// work, why and "Try again". `size` is the size of the words; `blue` as for `word`. With `missing` empty
/// only the word is drawn while nothing is going on.
pub fn fetch_line(ui: &mut egui::Ui, h: Helper, missing: &str, missing_color: egui::Color32, size: f32, rtl: bool, blue: bool) {
    use crate::theme;
    use egui::RichText;
    let s = state(h);
    let mut pieces: Vec<Box<dyn FnOnce(&mut egui::Ui)>> = vec![];
    match doing(h, &s) {
        Some((words, part)) => {
            pieces.push(Box::new(move |ui| drop(ui.add(egui::Label::new(RichText::new(words).size(size).color(theme::DIM())).wrap_mode(egui::TextWrapMode::Extend)))));
            match part {
                Some(f) => {
                    let total = h.pin().size;
                    let done = (f as f64 * total as f64) as u64;
                    pieces.push(Box::new(move |ui| {
                        let r = ui.allocate_exact_size(egui::vec2(120.0, size), egui::Sense::hover()).0;
                        let line = egui::Rect::from_center_size(r.center(), egui::vec2(120.0, 3.0));
                        ui.painter().rect_filled(line, egui::CornerRadius::same(2), theme::BORDER());
                        let mut part = line;
                        part.set_width(line.width() * f.clamp(0.0, 1.0));
                        ui.painter().rect_filled(part, egui::CornerRadius::same(2), theme::ACCENT2());
                    }));
                    pieces.push(Box::new(move |ui| drop(ui.add(egui::Label::new(RichText::new(amount(done, total)).size(size).color(theme::FAINT())).wrap_mode(egui::TextWrapMode::Extend)))));
                }
                None => pieces.push(Box::new(move |ui| drop(ui.add(egui::Spinner::new().size(size).color(theme::DIM()))))),
            }
            pieces.push(Box::new(move |ui| {
                if word(ui, "Stop", size, blue).clicked() {
                    stop(h);
                }
            }));
        }
        None => {
            let (said, color, fix) = match &s {
                State::Failed(why) => (why.clone(), theme::WARN(), "Try again"),
                _ => (missing.to_string(), missing_color, h.word()),
            };
            let ctx = ui.ctx().clone();
            if !said.is_empty() {
                pieces.push(Box::new(move |ui| drop(ui.label(RichText::new(said).size(size).color(color)))));
            }
            pieces.push(Box::new(move |ui| {
                if word(ui, fix, size, blue).clicked() {
                    start(h, &ctx);
                }
            }));
        }
    }
    if rtl {
        pieces.reverse();
    }
    for p in pieces {
        p(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("music-recs-helpers-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(bytes: &[u8]) -> String {
        let mut h = Sha256::new().unwrap();
        h.update(bytes);
        hex(&h.finish().unwrap())
    }

    /// A zip made here, laid out as the real ones are.
    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn windows_hashing_gives_the_known_sha256() {
        // the standard test values
        assert_eq!(sha(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // fed in pieces, the same as all at once
        let mut h = Sha256::new().unwrap();
        h.update(b"a");
        h.update(b"bc");
        assert_eq!(hex(&h.finish().unwrap()), sha(b"abc"));
    }

    #[test]
    fn the_pins_are_https_with_a_whole_checksum() {
        for h in [Helper::Ffmpeg, Helper::Soulseek] {
            let p = h.pin();
            assert!(p.url.starts_with("https://github.com/"), "{}", p.url);
            assert!(p.url.contains(p.version) && p.size > 10_000_000);
            assert!(p.sha256.len() == 64 && p.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
        // and nothing else is ever fetched from
        let e = open("http://example.invalid/ffmpeg.zip").err().unwrap();
        assert!(e.contains("https"), "{e}");
    }

    #[test]
    fn a_download_with_the_wrong_checksum_is_refused_and_removed() {
        let dir = lab("wrong");
        let home = dir.join("tools").join("ffmpeg");
        let good = zip_of(&[("ffmpeg-x/bin/ffmpeg.exe", b"ff"), ("ffmpeg-x/bin/ffprobe.exe", b"fp")]);
        let pin = Pin { version: "x", url: "https://x/ffmpeg.zip", size: good.len() as u64, sha256: Box::leak(sha(&good).into_boxed_str()) };
        // one byte changed: same size, other checksum
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let job = Job::default();
        let to = dir.join("got.part");
        assert_eq!(receive(&bad[..], &to, &pin, &job, &|| {}), Err(MISMATCH.to_string()));
        assert!(!to.exists(), "the refused download is removed");
        // too long, and too short, are refused too
        let longer = [&good[..], b"x"].concat();
        assert_eq!(receive(&longer[..], &to, &pin, &job, &|| {}), Err(MISMATCH.to_string()));
        assert!(receive(&good[..good.len() - 1], &to, &pin, &job, &|| {}).unwrap_err().contains("ended early"));
        assert!(!to.exists());
        // the right one is taken
        assert_eq!(receive(&good[..], &to, &pin, &job, &|| {}), Ok(()));
        assert_eq!(std::fs::read(&to).unwrap(), good);

        // the whole way in: a wrong one leaves nothing behind and an old copy in place untouched
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ffmpeg.exe"), b"old").unwrap();
        let wrong = Pin { sha256: "0000000000000000000000000000000000000000000000000000000000000000", ..pin };
        assert!(install_with(Helper::Ffmpeg, &bad, &home, &wrong).is_err());
        assert_eq!(std::fs::read(home.join("ffmpeg.exe")).unwrap(), b"old");
        assert_eq!(leftovers(&dir.join("tools")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `install` with a pin of the test's own (the real ones are fixed in the code).
    fn install_with(h: Helper, bytes: &[u8], home: &Path, pin: &Pin) -> Result<(), String> {
        let base = home.parent().unwrap();
        let work = Work::new(base.join(format!(".getting-{}-test", h.slug())))?;
        let file = work.0.join("t.zip.part");
        let job = Job::default();
        receive(bytes, &file, pin, &job, &|| {})?;
        let unpacked = work.0.join("unpacked");
        unpack(&file, &unpacked, h, &job)?;
        put_in_place(&unpacked, home, &work.0, h)
    }

    fn leftovers(base: &Path) -> usize {
        std::fs::read_dir(base).map(|l| l.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".getting-")).count()).unwrap_or(0)
    }

    #[test]
    fn only_the_wanted_files_are_unpacked_and_nothing_outside_the_folder() {
        let dir = lab("unpack");
        let home = dir.join("tools").join("ffmpeg");
        let bytes = zip_of(&[
            ("ffmpeg-9/bin/ffmpeg.exe", b"ff"),
            ("ffmpeg-9/bin/ffprobe.exe", b"fp"),
            ("ffmpeg-9/bin/ffplay.exe", b"plays sound"),
            ("ffmpeg-9/LICENSE", b"gpl"),
            ("ffmpeg-9/doc/ffmpeg.html", b"manual"),
            ("../../escaped.exe", b"no"),
            ("ffmpeg-9/bin/../../../escaped2.exe", b"no"),
        ]);
        let pin = Pin { version: "9", url: "https://x/f.zip", size: bytes.len() as u64, sha256: Box::leak(sha(&bytes).into_boxed_str()) };
        // an older copy in place is replaced
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ffmpeg.exe"), b"old").unwrap();
        install_with(Helper::Ffmpeg, &bytes, &home, &pin).unwrap();
        let mut got: Vec<String> = std::fs::read_dir(&home).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        got.sort();
        assert_eq!(got, ["LICENSE", "ffmpeg.exe", "ffprobe.exe"]);
        assert_eq!(std::fs::read(home.join("ffmpeg.exe")).unwrap(), b"ff");
        assert!(!dir.join("escaped.exe").exists() && !dir.join("tools").join("escaped2.exe").exists() && !dir.join("escaped2.exe").exists());
        assert_eq!(leftovers(&dir.join("tools")), 0);

        // an archive without the programs is not put in place
        let empty = zip_of(&[("ffmpeg-9/LICENSE", b"gpl")]);
        let pin = Pin { size: empty.len() as u64, sha256: Box::leak(sha(&empty).into_boxed_str()), ..pin };
        assert!(install_with(Helper::Ffmpeg, &empty, &home, &pin).unwrap_err().contains("ffmpeg.exe"));
        assert_eq!(std::fs::read(home.join("ffmpeg.exe")).unwrap(), b"ff");

        // slskd keeps its whole folder as it comes
        let s = zip_of(&[("slskd.exe", b"s"), ("wwwroot/index.html", b"w"), ("config/slskd.example.yml", b"#")]);
        let shome = dir.join("slskd").join("bin");
        let pin = Pin { size: s.len() as u64, sha256: Box::leak(sha(&s).into_boxed_str()), ..pin };
        install_with(Helper::Soulseek, &s, &shome, &pin).unwrap();
        assert!(shome.join("slskd.exe").is_file() && shome.join("wwwroot").join("index.html").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_a_cut_off_fetch_left_is_cleared_once_its_app_is_gone() {
        let dir = lab("leftovers");
        // a program that runs for a while, standing for another copy of the app fetching right now
        let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        let mut other = crate::importer::tools::command(Path::new(&shell), false).args(["/c", "ping -n 30 127.0.0.1 > nul"]).spawn().unwrap();
        // (a process that has ended: its number is not running)
        let mut ended = crate::importer::tools::command(Path::new(&shell), false).args(["/c", "exit 0"]).spawn().unwrap();
        let gone = ended.id();
        ended.wait().unwrap();
        let folder = |pid: u32| dir.join(format!(".getting-ffmpeg-{pid}-abc"));
        for f in [folder(gone), folder(other.id()), folder(std::process::id()), dir.join(".getting-ffmpeg-nonsense"), dir.join(".getting-slskd-1-abc"), dir.join("ffmpeg")] {
            std::fs::create_dir_all(&f).unwrap();
            std::fs::write(f.join("x.part"), b"part").unwrap();
        }
        clear_leftovers(&dir, Helper::Ffmpeg);
        // the ended app's and the unreadable one go
        assert!(!folder(gone).exists() && !dir.join(".getting-ffmpeg-nonsense").exists());
        // a running copy's, this app's own, another helper's and the helper itself stay
        assert!(folder(other.id()).exists() && folder(std::process::id()).exists() && dir.join(".getting-slskd-1-abc").exists() && dir.join("ffmpeg").exists());
        let _ = other.kill();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_fetch_leaves_nothing() {
        let dir = lab("stop");
        let job = Job::default();
        job.stop.store(true, Ordering::Relaxed);
        let to = dir.join("x.part");
        assert_eq!(receive(&b"abc"[..], &to, &FFMPEG, &job, &|| {}), Err(String::new()));
        assert!(!to.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_settings_written_for_slskd_pass_the_apps_own_check() {
        let downloads = Path::new(r"C:\Users\Someone's PC\Downloads\Soul #Seek");
        let text = slskd_settings(5831, 50301, Some(downloads), Path::new(r"C:\data\slskd\data\incomplete"));
        let set = crate::slskd::parse_settings(&text).unwrap();
        assert_eq!((set.port, set.has_login), (5831, false));
        // the downloads folder reads back exactly, an apostrophe and a # included
        assert_eq!(crate::slskd::parse_downloads_dir(&text).as_deref(), Some(downloads));
        // this PC only, sign-in on, nothing shared, nothing changeable from outside, no secret in it
        for line in ["  ip_address: 127.0.0.1", "    disabled: false", "  directories: []", "remote_configuration: false", "remote_file_management: false", "  listen_port: 50301"] {
            assert!(text.lines().any(|l| l == line), "{line}");
        }
        assert!(!text.contains("password") && !text.contains("api_keys") && !text.contains("username"));
        // nothing for the app to take out of it later
        assert_eq!(crate::slskd::without_web_secrets(&text), text);
        assert_eq!(crate::slskd::without_login(&text).0, text);
        // with no downloads folder in Settings, slskd keeps its own
        assert!(crate::slskd::parse_settings(&slskd_settings(5830, 50300, None, Path::new(r"C:\i"))).is_ok());

        // written once, never over one that is there
        let dir = lab("yml");
        let path = dir.join("data").join("slskd.yml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "web:\n  port: 5830\n# his own\n").unwrap();
        write_settings_if_missing(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "web:\n  port: 5830\n# his own\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_free_port_is_found_past_one_in_use() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let got = free_port([127, 0, 0, 1], port);
        assert!(got > port && got < port.saturating_add(100));
    }

    #[test]
    fn progress_words() {
        assert_eq!(amount(42_400_000, 114_768_076), "42 of 115 MB");
        assert_eq!(amount(200_000_000, 114_768_076), "115 of 115 MB");
        assert_eq!(doing(Helper::Ffmpeg, &State::Getting(0, 10)).unwrap().0, "Getting ffmpeg");
        assert_eq!(doing(Helper::Soulseek, &State::Unpacking), Some(("Unpacking the Soulseek client".to_string(), None)));
        assert!(doing(Helper::Ffmpeg, &State::Idle).is_none() && doing(Helper::Ffmpeg, &State::Failed("x".into())).is_none());
    }
}
