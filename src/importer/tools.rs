//! Finding and running ffmpeg / ffprobe without flashing console windows.
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL: u32 = 0x0000_4000;

/// What the app says, on every page that needs it, when ffmpeg or ffprobe is not there (the word beside it,
/// "Get ffmpeg", fetches both: see `helpers`).
pub const NO_FFMPEG: &str = "ffmpeg is not on this PC. Imports and checks need it.";

/// `ffmpeg` or `ffprobe`: on the PATH first (a PC that has its own keeps using it), then the copy the app
/// fetched for itself (`helpers::ffmpeg_dir`), then among the WinGet packages. A test run can hide all but
/// the app's own copy (`MUSIC_RECS_NO_FFMPEG`, only with `MUSIC_RECS_DATA` set), to try "Get ffmpeg".
pub fn find_tool(name: &str) -> Result<PathBuf, String> {
    let test = |v: &str| std::env::var_os(v).is_some_and(|v| !v.is_empty());
    let hidden = test("MUSIC_RECS_DATA") && test("MUSIC_RECS_NO_FFMPEG");
    let (path, local) = if hidden { (None, None) } else { (std::env::var_os("PATH"), std::env::var_os("LOCALAPPDATA")) };
    look_for_tool(name, path, &crate::helpers::ffmpeg_dir(), local).ok_or_else(|| NO_FFMPEG.to_string())
}

fn look_for_tool(name: &str, path: Option<std::ffi::OsString>, own: &Path, local: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let exe = format!("{name}.exe");
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(&exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let p = own.join(&exe);
    if p.is_file() {
        return Some(p);
    }
    local.and_then(|l| search(&Path::new(&l).join("Microsoft").join("WinGet").join("Packages"), &exe, 6))
}

fn search(dir: &Path, exe: &str, depth: u32) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_file() && e.file_name().to_string_lossy().eq_ignore_ascii_case(exe) {
            return Some(p);
        }
    }
    if depth == 0 {
        return None;
    }
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = search(&p, exe, depth - 1) {
                return Some(f);
            }
        }
    }
    None
}

/// The kinds of file ffmpeg and ffprobe may read a download or a library file as: the audio formats the
/// importer takes (`scan::CONVERT_EXT`, `REMUX_EXT`, `PLAIN_COPY`: MP3, FLAC, WAV, AIFF, Monkey's Audio,
/// WavPack, TTA, DSD, MP4/M4A, Ogg, AAC, WMA) and pictures for covers. ffmpeg goes by a file's content,
/// not its name, so without this a file named `.flac` that is really a playlist or a stream description
/// would be followed to whatever it names. With it, such a file is refused ("Format not on whitelist").
pub const FORMATS: &str = "mp3,flac,wav,w64,aiff,ape,wv,tta,dsf,iff,ogg,aac,asf,mov,image2,jpeg_pipe,png_pipe,webp_pipe,gif,mjpeg";

/// Goes before every input that is a download or a library file (ffmpeg: before its `-i`; ffprobe: before
/// the file): only the formats in `FORMATS`, and only local files and pipes, never a web address or a
/// network share through ffmpeg's own protocols.
pub const LOCAL_ONLY: [&str; 4] = ["-protocol_whitelist", "file,pipe", "-format_whitelist", FORMATS];

/// A command that runs hidden. `low` = below-normal priority so the PC stays responsive.
pub fn command(exe: &Path, low: bool) -> Command {
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW | if low { BELOW_NORMAL } else { 0 });
    c.stdin(Stdio::null());
    c
}

/// Runs to completion and returns stdout (empty on failure, or if it takes over two minutes).
pub fn run_stdout(exe: &Path, args: &[&std::ffi::OsStr]) -> Vec<u8> {
    let mut c = command(exe, false);
    c.args(args).stderr(Stdio::null());
    output_limited(&mut c, 120)
}

/// Waits for a helper program, but not forever: past the limit it is stopped. A crafted file must not be
/// able to leave ffmpeg spinning and the app stuck "busy".
fn wait_limited(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= end {
            // Helpers are started directly, so the owned process handle stops the exact helper.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Runs a prepared command; true only if it finished in time and reported success.
pub fn run_limited(c: &mut Command, secs: u64) -> bool {
    let Ok(mut child) = c.spawn() else { return false };
    wait_limited(&mut child, secs).map(|s| s.success()).unwrap_or(false)
}

/// Runs a prepared command and returns what it printed (at most 64 MB); empty if it failed or ran out of time.
pub fn output_limited(c: &mut Command, secs: u64) -> Vec<u8> {
    use std::io::Read;
    c.stdout(Stdio::piped());
    let Ok(mut child) = c.spawn() else { return vec![] };
    let out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut v = vec![];
        if let Some(o) = out {
            let _ = o.take(64 * 1024 * 1024).read_to_end(&mut v);
        }
        v
    });
    // On a timeout the reader is left to finish on its own: something further down the chain may still hold
    // the pipe open, and waiting for it here would bring the hang straight back.
    if wait_limited(&mut child, secs).is_none() {
        return vec![];
    }
    reader.join().unwrap_or_default()
}

/// Runs a prepared command and returns what it printed and what it complained about (64 MB and 64 KB at
/// most). None if it could not be started or ran out of time.
pub fn capture_limited(c: &mut Command, secs: u64) -> Option<(Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().ok()?;
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let read = |pipe: Option<Box<dyn Read + Send>>, most: u64| {
        std::thread::spawn(move || {
            let mut v = vec![];
            if let Some(mut p) = pipe {
                let _ = (&mut p).take(most).read_to_end(&mut v);
                // past the limit the rest is read and dropped, so the program is never left waiting on a full pipe
                let _ = std::io::copy(&mut p, &mut std::io::sink());
            }
            v
        })
    };
    let out = read(out.map(|o| Box::new(o) as Box<dyn Read + Send>), 64 * 1024 * 1024);
    let err = read(err.map(|e| Box::new(e) as Box<dyn Read + Send>), 64 * 1024);
    wait_limited(&mut child, secs)?;
    Some((out.join().unwrap_or_default(), err.join().unwrap_or_default()))
}

/// Stops a helper program that is still running when its time is up, for callers that read its output as it
/// comes (a read from a program that has gone quiet never returns by itself; ending the program ends the read).
/// Dropping the watch calls it off. Keep it no longer than the `Child` it watches.
pub struct Watch {
    done: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub fn watch(child: &std::process::Child, secs: u64) -> Watch {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    let done = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (d, f, pid) = (done.clone(), fired.clone(), child.id());
    let process = child.as_handle().try_clone_to_owned().ok();
    std::thread::spawn(move || {
        let guard = d.0.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = d.1.wait_timeout_while(guard, std::time::Duration::from_secs(secs), |finished| !*finished).unwrap_or_else(|e| e.into_inner());
        if !*guard {
            // still held under the lock, so the watch cannot be called off (and the program's number reused) meanwhile
            f.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(process) = process {
                // Retaining the handle also prevents a recycled process ID from targeting another app.
                unsafe { windows_sys::Win32::System::Threading::TerminateProcess(process.as_raw_handle(), 1); }
            } else {
                let _ = command(Path::new("taskkill.exe"), false).args(["/PID", &pid.to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
    });
    Watch { done, fired }
}

impl Watch {
    /// True if the program ran out of time and was stopped.
    pub fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        *self.done.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.done.1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_on_the_path_comes_before_the_apps_own_copy_and_the_winget_packages() {
        let dir = std::env::temp_dir().join(format!("music-recs-ffmpeg-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let (bin, own, local) = (dir.join("bin"), dir.join("tools").join("ffmpeg"), dir.join("local"));
        let winget = local.join(r"Microsoft\WinGet\Packages\Gyan.FFmpeg\ffmpeg-9-full_build\bin");
        for d in [&bin, &own, &winget, &dir.join("empty")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path = |dirs: &[&Path]| Some(std::env::join_paths(dirs.iter()).unwrap());
        let os = |p: &Path| Some(p.as_os_str().to_os_string());
        // nowhere
        assert_eq!(look_for_tool("ffmpeg", path(&[&dir.join("empty")]), &own, os(&local)), None);
        // among the WinGet packages only
        std::fs::write(winget.join("ffmpeg.exe"), b"x").unwrap();
        assert_eq!(look_for_tool("ffmpeg", path(&[&dir.join("empty")]), &own, os(&local)), Some(winget.join("ffmpeg.exe")));
        // the app's own copy before that
        std::fs::write(own.join("ffmpeg.exe"), b"x").unwrap();
        assert_eq!(look_for_tool("ffmpeg", path(&[&dir.join("empty")]), &own, os(&local)), Some(own.join("ffmpeg.exe")));
        // and the PATH before everything: a PC with its own ffmpeg goes on using it
        std::fs::write(bin.join("ffmpeg.exe"), b"x").unwrap();
        assert_eq!(look_for_tool("ffmpeg", path(&[&dir.join("empty"), &bin]), &own, os(&local)), Some(bin.join("ffmpeg.exe")));
        // ffprobe is looked for the same way, on its own
        assert_eq!(look_for_tool("ffprobe", path(&[&bin]), &own, os(&local)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quiet_helper_being_read_is_stopped_when_its_time_is_up() {
        use std::io::Read;
        let shell = std::path::PathBuf::from(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
        // prints nothing and waits far longer than the limit: the read below would never return by itself
        let mut c = command(&find_tool("ffmpeg").unwrap(), false);
        c.args(["-nostdin", "-v", "quiet", "-re", "-f", "lavfi", "-i", "anullsrc=r=8000:cl=mono", "-t", "60", "-f", "null", "-"]).stdout(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let dog = watch(&child, 1);
        let t = std::time::Instant::now();
        let mut got = vec![];
        let _ = child.stdout.take().unwrap().read_to_end(&mut got);
        let _ = child.wait();
        assert!(dog.fired() && got.is_empty());
        assert!(t.elapsed() < std::time::Duration::from_secs(15), "took {:?}", t.elapsed());

        // a helper that finishes in time is left alone
        let mut c = command(&shell, false);
        c.args(["/c", "echo hi"]).stdout(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let dog = watch(&child, 30);
        let mut got = vec![];
        let _ = child.stdout.take().unwrap().read_to_end(&mut got);
        assert!(child.wait().unwrap().success());
        assert!(!dog.fired());
        assert_eq!(String::from_utf8_lossy(&got).trim(), "hi");
    }

    #[test]
    fn a_file_that_is_really_a_playlist_is_refused_and_every_real_format_is_still_read() {
        use crate::importer::probe::probe;
        let (ffmpeg, ffprobe) = (find_tool("ffmpeg").unwrap(), find_tool("ffprobe").unwrap());
        let dir = std::env::temp_dir().join(format!("music-recs-formats-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let make = |name: &str, more: &[&str]| {
            let out = dir.join(name);
            let mut c = command(&ffmpeg, false);
            c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1"]).args(more).arg(&out);
            assert!(run_limited(&mut c, 60), "ffmpeg could not make {name}");
            out
        };

        // every kind of audio file the importer takes that ffmpeg can make here is still read as audio
        let kinds: [(&str, &[&str]); 11] = [
            ("a.mp3", &["-c:a", "libmp3lame"]),
            ("a.flac", &[]),
            ("a.wav", &[]),
            ("a.aiff", &[]),
            ("a.wv", &[]),
            ("a.tta", &[]),
            ("a.m4a", &["-c:a", "aac"]),
            ("a.ogg", &["-c:a", "libvorbis"]),
            ("a.opus", &["-c:a", "libopus"]),
            ("a.aac", &["-c:a", "aac"]),
            ("a.wma", &["-c:a", "wmav2"]),
        ];
        for (name, more) in kinds {
            let p = probe(&ffprobe, &make(name, more));
            assert!(!p.codec.is_empty() && p.duration > 0.5, "{name} was not read: codec '{}', {} s", p.codec, p.duration);
        }
        // and a WAV-wrapped MP3 (the importer unwraps these), and a picture for a cover
        assert!(!probe(&ffprobe, &make("wrapped.wav", &["-c:a", "libmp3lame", "-f", "wav"])).codec.is_empty());
        let mut c = command(&ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=64x64", "-frames:v", "1"]).arg(dir.join("cover.jpg"));
        assert!(run_limited(&mut c, 60));
        let seen = run_stdout(&ffprobe, &["-v".as_ref(), "error".as_ref(), "-show_entries".as_ref(), "stream=width".as_ref(), "-of".as_ref(), "csv=p=0".as_ref(), LOCAL_ONLY[0].as_ref(), LOCAL_ONLY[1].as_ref(), LOCAL_ONLY[2].as_ref(), LOCAL_ONLY[3].as_ref(), "--".as_ref(), dir.join("cover.jpg").as_os_str()]);
        assert_eq!(String::from_utf8_lossy(&seen).trim(), "64");

        // a text file named .flac that tells ffmpeg to play another file: left to itself ffmpeg follows it
        // (that is the hole), with the limits it reads nothing
        let fake = dir.join("fake.flac");
        std::fs::write(&fake, "ffconcat version 1.0\nfile a.mp3\n").unwrap();
        let unguarded = run_stdout(&ffprobe, &["-v".as_ref(), "error".as_ref(), "-show_entries".as_ref(), "stream=codec_name".as_ref(), "-of".as_ref(), "csv=p=0".as_ref(), "--".as_ref(), fake.as_os_str()]);
        assert_eq!(String::from_utf8_lossy(&unguarded).trim(), "mp3", "this ffmpeg no longer follows such a file by itself");
        let p = probe(&ffprobe, &fake);
        assert!(p.codec.is_empty() && p.duration == 0.0);
        // the same through ffmpeg itself, as the importer's conversions run it
        let mut c = command(&ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y"]).args(LOCAL_ONLY).arg("-i").arg(&fake).arg(dir.join("out.mp3")).stderr(Stdio::null());
        assert!(!run_limited(&mut c, 60) && !dir.join("out.mp3").exists());
        // a web address is not opened at all (refused before any connection is tried)
        let mut c = command(&ffprobe, false);
        c.args(["-v", "error"]).args(LOCAL_ONLY).arg("http://127.0.0.1:9/a.mp3");
        let (_, err) = capture_limited(&mut c, 30).unwrap();
        assert!(String::from_utf8_lossy(&err).to_lowercase().contains("whitelist"), "{}", String::from_utf8_lossy(&err));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stuck_helper_is_stopped() {
        let shell = std::path::PathBuf::from(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
        // prints, then waits far longer than the limit
        let mut slow = command(&find_tool("ffmpeg").unwrap(), false);
        slow.args(["-nostdin", "-v", "quiet", "-re", "-f", "lavfi", "-i", "anullsrc=r=8000:cl=mono", "-t", "30", "-progress", "pipe:1", "-f", "null", "-"]);
        let t = std::time::Instant::now();
        assert!(output_limited(&mut slow, 1).is_empty());
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        let mut quick = command(&shell, false);
        quick.args(["/c", "echo hi"]);
        assert_eq!(String::from_utf8_lossy(&output_limited(&mut quick, 20)).trim(), "hi");
        let mut fails = command(&shell, false);
        fails.args(["/c", "exit 3"]);
        assert!(!run_limited(&mut fails, 20));
    }
}
