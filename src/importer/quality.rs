//! Audio quality checks: damaged files and fake lossless (port of SpectrumCheck.cs).
//!   * Damage: decodes the whole file once, collecting decoder errors and how much audio came out.
//!     FLACs also have their built-in MD5 checksum verified.
//!   * Fake quality: samples from three spots in the track go through an FFT, looking for the sharp
//!     "cliff" an MP3 encoder leaves (about 16kHz for 128kbps, 19kHz for 192k, 20kHz for 320k).
//! Results are cached on disk keyed by path + size + modified time, so rescans only look at new files.
use crate::importer::tools;
use regex::Regex;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

const RATE: usize = 44100;
const N: usize = 4096;
const BAND_HZ: usize = 250;
const TIMEOUT: Duration = Duration::from_secs(300);
const CACHE_VERSION: u32 = 5;
/// -80dBFS: quieter than this counts as silence
const SILENT_LEVEL: f32 = 1e-4;
const MAX_HF_GAP_DB: f64 = 85.0;
const STRICT_EXT: [&str; 7] = ["flac", "wav", "aif", "aiff", "ape", "wv", "tta"];

#[derive(Clone, Debug)]
pub struct Quality {
    pub analyzed: bool,
    pub measured: bool,
    pub cutoff_hz: f64,
    pub cliff_db: f64,
    pub decode_ok: bool,
    pub decoded_seconds: f64,
    pub error_count: u32,
    pub first_error: String,
    pub md5_status: String,
    pub check_error: String,
    pub hf_gap_db: f64,
    /// digital silence (below -80dB), in seconds: at the start, at the end, the longest stretch inside the
    /// track and where it begins, and how many stretches of two seconds or more there are
    pub lead_silence: f64,
    pub trail_silence: f64,
    pub max_gap: f64,
    pub max_gap_at: f64,
    pub gaps: u32,
}

impl Default for Quality {
    fn default() -> Self {
        Quality {
            analyzed: false,
            measured: false,
            cutoff_hz: 0.0,
            cliff_db: 0.0,
            decode_ok: true,
            decoded_seconds: 0.0,
            error_count: 0,
            first_error: String::new(),
            md5_status: "n/a".into(),
            check_error: String::new(),
            hf_gap_db: 0.0,
            lead_silence: 0.0,
            trail_silence: 0.0,
            max_gap: 0.0,
            max_gap_at: 0.0,
            gaps: 0,
        }
    }
}

static CACHE: LazyLock<Mutex<HashMap<String, Quality>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static LOADED: OnceLock<()> = OnceLock::new();
/// the keys of the results found by this run (the rest of `CACHE` was read from the file and is already in it)
static FRESH: LazyLock<Mutex<std::collections::HashSet<String>>> = LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
static IMAGE_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\[(png|mjpeg|jpeg|jpegls|image2|bmp|gif|webp|tiff)\s@|Invalid PNG signature|attached pic").unwrap()
});

fn key(file: &Path, analyze: bool) -> Option<String> {
    let md = std::fs::metadata(file).ok()?;
    let ticks = md.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| 621_355_968_000_000_000u128 + d.as_secs() as u128 * 10_000_000 + (d.subsec_nanos() / 100) as u128)?;
    Some(format!("{}|{}|{}|{}|{}", CACHE_VERSION, file.to_string_lossy().to_lowercase(), md.len(), ticks, if analyze { 1 } else { 0 }))
}

// ---------- cache on disk (same file format as the PowerShell version) ----------
pub fn load_cache(path: &Path) {
    if LOADED.set(()).is_err() {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let prefix = format!("{CACHE_VERSION}|");
    let mut cache = CACHE.lock().unwrap();
    for line in text.lines() {
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 17 || !p[0].starts_with(&prefix) {
            continue;
        }
        let d = |s: &str| s.parse::<f64>().unwrap_or(0.0);
        cache.insert(
            p[0].to_string(),
            Quality {
                analyzed: p[1] == "1",
                measured: p[2] == "1",
                cutoff_hz: d(p[3]),
                cliff_db: d(p[4]),
                decode_ok: p[5] == "1",
                decoded_seconds: d(p[6]),
                error_count: p[7].parse().unwrap_or(0),
                first_error: p[8].to_string(),
                md5_status: p[9].to_string(),
                check_error: String::new(),
                hf_gap_db: d(p[10]),
                lead_silence: d(p[11]),
                trail_silence: d(p[12]),
                max_gap: d(p[13]),
                max_gap_at: d(p[14]),
                gaps: p[15].parse().unwrap_or(0),
            },
        );
    }
}

pub fn save_cache(path: &Path) {
    let cache = CACHE.lock().unwrap();
    let fresh = FRESH.lock().unwrap();
    // The check and the importer both save here (and a second window of the app may), so lines may have been added since it
    // was read: every line already there is kept, except the ones this run has a newer answer for. Only what this
    // run found is added, so nothing another program wrote is ever lost. Past 200,000 lines the oldest go first.
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let keep: Vec<&str> = old.lines().filter(|l| l.split_once('\t').is_some_and(|(k, _)| !fresh.contains(k))).collect();
    let mut out = String::new();
    for l in keep.iter().skip(keep.len().saturating_sub(200_000)) {
        out.push_str(l);
        out.push('\n');
    }
    for (k, v) in fresh.iter().filter_map(|k| cache.get(k).map(|v| (k, v))) {
        out.push_str(&format!(
            "{k}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\tx\n",
            v.analyzed as u8,
            v.measured as u8,
            v.cutoff_hz,
            v.cliff_db,
            v.decode_ok as u8,
            v.decoded_seconds,
            v.error_count,
            v.first_error.replace(['\t', '\n', '\r'], " "),
            v.md5_status,
            v.hf_gap_db,
            v.lead_silence,
            v.trail_silence,
            v.max_gap,
            v.max_gap_at,
            v.gaps
        ));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// What an earlier check found for this file as it is now, and failing that what was found for the same
/// file at the same size under another modified time. A repair that only rewrites tags, or only touches the file, changes the time and leaves the audio
/// as it was (Library Health's tag fixes did this to over a thousand tracks), and a different file landing
/// on the same path with the same number of bytes does not happen in practice. For the library check only:
/// the importer judges a new download and always wants the exact file.
pub fn cached_by_size(file: &Path, analyze: bool) -> Option<Quality> {
    let k = key(file, analyze)?;
    let cache = CACHE.lock().unwrap();
    if let Some(q) = cache.get(&k) {
        return Some(q.clone());
    }
    // "version|path|size|ticks|analyze": the same but for the ticks, the newest of them
    let mut parts = k.rsplitn(3, '|');
    let (tail, _ticks, head) = (parts.next()?, parts.next()?, parts.next()?);
    let by_size = BY_SIZE.get_or_init(|| {
        let mut m: HashMap<String, (u128, String)> = HashMap::new();
        for key in cache.keys() {
            let mut p = key.rsplitn(3, '|');
            let (Some(tail), Some(ticks), Some(head)) = (p.next(), p.next(), p.next()) else { continue };
            let ticks: u128 = ticks.parse().unwrap_or(0);
            let e = m.entry(format!("{head}|{tail}")).or_insert((ticks, key.clone()));
            if ticks > e.0 {
                *e = (ticks, key.clone());
            }
        }
        m
    });
    by_size.get(&format!("{head}|{tail}")).and_then(|(_, key)| cache.get(key)).cloned()
}
/// built once, from what the cache file held when it was first asked (results found later have exact keys)
static BY_SIZE: OnceLock<HashMap<String, (u128, String)>> = OnceLock::new();

/// Listens to a file now, whatever an earlier check found for it, and keeps nothing. For telling which of a
/// library file and its backup copy is the damaged one: such a pair has the same size and date, so a saved
/// result may be from before the damage.
pub fn check_fresh(ffmpeg: &Path, file: &Path, duration: f64) -> Quality {
    let mut r = Quality::default();
    if let Err(e) = deep_check(ffmpeg, file, duration, false, &mut r) {
        r.check_error = e;
    }
    r
}

/// Listens to a file again and puts what is found in place of the saved result (to be written by the next
/// `save_cache`). For a file that was swapped for another of the same size and date, where the saved
/// result would otherwise go on standing for the old one.
pub fn recheck(ffmpeg: &Path, file: &Path, duration: f64) -> Quality {
    let mut r = Quality::default();
    if let Err(e) = deep_check(ffmpeg, file, duration, true, &mut r) {
        r.check_error = e;
        return r;
    }
    if let Some(k) = key(file, true) {
        CACHE.lock().unwrap().insert(k.clone(), r.clone());
        FRESH.lock().unwrap().insert(k);
    }
    r
}

/// The key a file's result is kept under as the file is now (None if it cannot be read).
pub fn key_of(file: &Path) -> Option<String> {
    key(file, true)
}

/// After a repair that rewrote only a file's tags (the audio was checked to be byte for byte the same):
/// what was found for the file before stands for it now. `old_key` is `key_of` from before the change.
pub fn carry(old_key: Option<String>, file: &Path) {
    let (Some(old), Some(new)) = (old_key, key(file, true)) else { return };
    let mut cache = CACHE.lock().unwrap();
    if let Some(q) = cache.get(&old).cloned() {
        cache.insert(new.clone(), q);
        FRESH.lock().unwrap().insert(new);
    }
}

// ---------- batch entry point ----------
/// Checks many files at once. `analyze[i]` = also run the fake-quality check.
pub fn check_files(ffmpeg: &Path, files: &[PathBuf], durations: &[f64], analyze: &[bool], threads: usize) -> Vec<Quality> {
    let results: Mutex<Vec<Option<Quality>>> = Mutex::new(vec![None; files.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.max(1).min(files.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= files.len() {
                    break;
                }
                let k = key(&files[i], analyze[i]);
                let cached = k.as_ref().and_then(|k| CACHE.lock().unwrap().get(k).cloned());
                let q = match cached {
                    Some(q) => q,
                    None => {
                        let mut r = Quality::default();
                        if let Err(e) = deep_check(ffmpeg, &files[i], durations[i], analyze[i], &mut r) {
                            r.check_error = e;
                        }
                        if let Some(k) = k {
                            if r.check_error.is_empty() {
                                FRESH.lock().unwrap().insert(k.clone());
                                CACHE.lock().unwrap().insert(k, r.clone());
                            }
                        }
                        r
                    }
                };
                results.lock().unwrap()[i] = Some(q);
            });
        }
    });
    results.into_inner().unwrap().into_iter().map(|q| q.unwrap_or_default()).collect()
}

// ---------- the single-pass check ----------
fn deep_check(ffmpeg: &Path, file: &Path, duration: f64, analyze: bool, r: &mut Quality) -> Result<(), String> {
    // where to take spectrum samples: three 15s slices (20%, 50%, 80%), or the first 45s of short tracks
    let mut slices: Vec<(u64, u64)> = vec![];
    if analyze {
        if duration >= 50.0 {
            for f in [0.2, 0.5, 0.8] {
                let s0 = ((duration * f - 7.5).max(0.0) * RATE as f64) as u64;
                slices.push((s0, s0 + 15 * RATE as u64));
            }
        } else {
            slices.push((0, 45 * RATE as u64));
        }
    }
    let mut slice_data: Vec<Vec<f32>> = slices.iter().map(|_| Vec::new()).collect();

    let ext = file.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut cmd = tools::command(ffmpeg, true);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error"]);
    // Strict checking catches flipped bits in lossless files, but on MP3s it complains about harmless padding.
    if STRICT_EXT.contains(&ext.as_str()) {
        cmd.args(["-err_detect", "crccheck+bitstream+buffer"]);
    }
    cmd.args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(file).args(["-map", "0:a:0", "-ac", "1", "-ar", &RATE.to_string(), "-f", "f32le", "-"]);
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // a crafted file can leave ffmpeg running without a word: the read below would wait on it forever
    let dog = tools::watch(&child, TIMEOUT.as_secs());
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        s.push_str(&String::from_utf8_lossy(&buf));
        s
    });
    let started = Instant::now();
    let mut samples: u64 = 0;
    let mut buf = vec![0u8; 1 << 16];
    let mut carry = 0usize;
    // silence, sample by sample: where the current silent stretch began, and the longest one between sounds
    let (mut silent_from, mut in_silence, mut first_sound) = (0u64, true, None::<u64>);
    let (mut max_gap, mut max_gap_at, mut gaps) = (0f64, 0f64, 0u32);
    loop {
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            return Err("took too long to decode".into());
        }
        let n = match stdout.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n + carry,
        };
        let whole = n / 4 * 4;
        let count = (whole / 4) as u64;
        for k in 0..count as usize {
            let x = f32::from_le_bytes([buf[k * 4], buf[k * 4 + 1], buf[k * 4 + 2], buf[k * 4 + 3]]);
            let at = samples + k as u64;
            if x < SILENT_LEVEL && x > -SILENT_LEVEL {
                if !in_silence {
                    in_silence = true;
                    silent_from = at;
                }
            } else if in_silence {
                in_silence = false;
                if first_sound.is_none() {
                    first_sound = Some(at); // the end of the lead-in silence
                } else {
                    let len = (at - silent_from) as f64 / RATE as f64;
                    if len >= 2.0 {
                        gaps += 1;
                        if len > max_gap {
                            max_gap = len;
                            max_gap_at = silent_from as f64 / RATE as f64;
                        }
                    }
                }
            }
        }
        for (si, sl) in slices.iter().enumerate() {
            let from = samples.max(sl.0);
            let to = (samples + count).min(sl.1);
            let mut k = from;
            while k < to {
                let o = ((k - samples) * 4) as usize;
                slice_data[si].push(f32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]));
                k += 1;
            }
        }
        samples += count;
        carry = n - whole;
        if carry > 0 {
            buf.copy_within(whole..n, 0);
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if dog.fired() {
        return Err("took too long to decode".into());
    }
    drop(dog);
    let err = err_thread.join().unwrap_or_default();
    r.decoded_seconds = samples as f64 / RATE as f64;
    let tenth = |x: f64| (x * 10.0).round() / 10.0;
    r.lead_silence = tenth(first_sound.unwrap_or(samples) as f64 / RATE as f64);
    r.trail_silence = if in_silence && first_sound.is_some() { tenth((samples - silent_from) as f64 / RATE as f64) } else { 0.0 };
    r.max_gap = tenth(max_gap);
    r.max_gap_at = tenth(max_gap_at);
    r.gaps = gaps;
    for raw in err.split(['\r', '\n']) {
        let t = raw.trim();
        if t.is_empty() || t.starts_with("Last message repeated") || IMAGE_NOISE.is_match(t) {
            continue; // a broken embedded cover picture isn't an audio problem
        }
        r.error_count += 1;
        if r.first_error.is_empty() {
            r.first_error = t.chars().take(160).collect();
        }
    }
    r.decode_ok = status.success() && r.error_count == 0;

    if ext == "flac" {
        check_flac_md5(ffmpeg, file, r);
    }
    if analyze {
        analyze_spectrum(&slice_data, r);
    }
    Ok(())
}

// ---------- FLAC checksum ----------
// A FLAC's STREAMINFO block stores an MD5 of the original audio. Decoding and hashing it again proves the file is intact.
fn check_flac_md5(ffmpeg: &Path, file: &Path, r: &mut Quality) {
    let Some((stored, bits)) = read_flac_streaminfo(file) else {
        r.md5_status = "error".into();
        return;
    };
    if stored.iter().all(|b| *b == 0) {
        r.md5_status = "unset".into();
        return;
    }
    let (fmt, codec) = if bits <= 8 {
        ("s8", "pcm_s8")
    } else if bits <= 16 {
        ("s16le", "pcm_s16le")
    } else if bits <= 24 {
        ("s24le", "pcm_s24le")
    } else {
        ("s32le", "pcm_s32le")
    };
    let mut cmd = tools::command(ffmpeg, true);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error"]).args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(file).args(["-map", "0:a:0", "-c:a", codec, "-f", fmt, "-"]);
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        r.md5_status = "error".into();
        return;
    };
    let dog = tools::watch(&child, TIMEOUT.as_secs());
    let mut stdout = child.stdout.take().unwrap();
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; 1 << 16];
    while let Ok(n) = stdout.read(&mut buf) {
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    let _ = child.wait();
    if dog.fired() {
        r.md5_status = "error".into(); // stopped for taking too long: what was hashed is not the whole file
        return;
    }
    drop(dog);
    let digest = ctx.compute();
    if digest.0[..] == stored[..] {
        r.md5_status = "ok".into();
    } else {
        r.md5_status = "mismatch".into();
        r.decode_ok = false;
    }
}

fn read_flac_streaminfo(file: &Path) -> Option<(Vec<u8>, u32)> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(file).ok()?;
    let mut head = [0u8; 10];
    f.read_exact(&mut head).ok()?;
    let mut pos: u64 = 0;
    if &head[..3] == b"ID3" {
        // some FLACs start with an ID3 tag; skip it
        pos = 10 + ((head[6] as u64 & 0x7f) << 21 | (head[7] as u64 & 0x7f) << 14 | (head[8] as u64 & 0x7f) << 7 | (head[9] as u64 & 0x7f));
    }
    f.seek(SeekFrom::Start(pos)).ok()?;
    let mut b = [0u8; 4 + 4 + 34];
    f.read_exact(&mut b).ok()?;
    if &b[..4] != b"fLaC" || (b[4] & 0x7f) != 0 {
        return None;
    }
    let si = 8;
    let bits = ((((b[si + 12] & 0x01) as u32) << 4) | (b[si + 13] >> 4) as u32) + 1;
    Some((b[si + 18..si + 34].to_vec(), bits))
}

// ---------- the spectrum ----------
fn mean(a: &[f64], from: isize, to: isize) -> f64 {
    let (mut s, mut n) = (0.0, 0);
    let lo = from.max(0) as usize;
    let hi = (to.max(0) as usize).min(a.len());
    for v in a.iter().take(hi).skip(lo) {
        s += v;
        n += 1;
    }
    if n > 0 { s / n as f64 } else { f64::MIN }
}

fn analyze_spectrum(slices: &[Vec<f32>], r: &mut Quality) {
    r.analyzed = true;
    let bins = N / 2;
    let mut power = vec![0f64; bins];
    let win: Vec<f64> = (0..N).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (N - 1) as f64).cos()).collect();
    let mut re = vec![0f64; N];
    let mut im = vec![0f64; N];
    let mut used = 0usize;
    for s in slices {
        let mut off = 0;
        while off + N <= s.len() {
            let e: f64 = (0..N).map(|i| (s[off + i] as f64) * (s[off + i] as f64)).sum();
            if e / (N as f64) >= 1e-7 {
                // (skip near-silence so gaps don't drag the average down)
                for i in 0..N {
                    re[i] = s[off + i] as f64 * win[i];
                    im[i] = 0.0;
                }
                fft(&mut re, &mut im);
                for k in 0..bins {
                    power[k] += re[k] * re[k] + im[k] * im[k];
                }
                used += 1;
            }
            off += N;
        }
    }
    if used < 20 {
        r.measured = false;
        return;
    }
    let nb = (RATE / 2) / BAND_HZ; // 88 bands of 250Hz
    let mut bands = vec![0f64; nb];
    let hz_per_bin = RATE as f64 / N as f64;
    for b in 0..nb {
        let k0 = (b as f64 * BAND_HZ as f64 / hz_per_bin) as usize;
        let k1 = bins.min(((b + 1) as f64 * BAND_HZ as f64 / hz_per_bin) as usize);
        let sum: f64 = power[k0..k1.max(k0)].iter().sum();
        bands[b] = 10.0 * (sum / ((k1.saturating_sub(k0)).max(1)) as f64 / used as f64 + 1e-30).log10();
    }
    let mut floor = f64::MAX; // the quietest stretch up top = what "nothing" looks like
    let mut b = 40;
    while b + 4 < nb {
        floor = floor.min(mean(&bands, b as isize, b as isize + 4));
        b += 1;
    }
    let hf_db = mean(&bands, 56, 64); // 14-16kHz, just below where MP3s cut off
    let peak = bands.iter().take(32).cloned().fold(f64::MIN, f64::max); // the loudest part (bass/mids)
    r.hf_gap_db = ((peak - hf_db) * 10.0).round() / 10.0;
    // almost nothing at 14-16kHz (a quiet, very dull or synthetic track): a missing cliff proves nothing either way
    if hf_db - floor < 10.0 || r.hf_gap_db > MAX_HF_GAP_DB {
        r.measured = false;
        return;
    }
    r.measured = true;
    // the biggest drop across ~750Hz between 12kHz and 21.5kHz, where everything above stays down
    let mut best_drop = 0.0;
    let mut best_at: isize = -1;
    for b in 48..=86isize {
        let before = mean(&bands, b - 3, b);
        let after = mean(&bands, b + 1, (nb as isize).min(b + 4));
        let drop = before - after;
        let mut rest_max = f64::MIN;
        for j in (b as usize + 1)..(nb - 1) {
            rest_max = rest_max.max(bands[j]);
        }
        if drop > best_drop && rest_max < after + 8.0 {
            best_drop = drop;
            best_at = b;
        }
    }
    r.cliff_db = (best_drop * 10.0).round() / 10.0;
    r.cutoff_hz = if best_at > 0 { (best_at as usize * BAND_HZ) as f64 } else { 0.0 };
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (i + k, i + k + len / 2);
                let xr = re[b] * cr - im[b] * ci;
                let xi = re[b] * ci + im[b] * cr;
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

// ---------- spectrogram picture for the details panel ----------
pub fn make_spectrogram(ffmpeg: &Path, file: &Path, duration: f64, out_png: &Path) -> Option<PathBuf> {
    let start = if duration > 90.0 { duration * 0.3 } else { 0.0 };
    let mut cmd = tools::command(ffmpeg, true);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-y", "-ss", &format!("{start:.3}"), "-t", "30"]).args(crate::importer::tools::LOCAL_ONLY).arg("-i")
        .arg(file)
        .args(["-filter_complex", "[0:a:0]aresample=44100,showspectrumpic=s=560x200:legend=1:scale=log:color=intensity:fscale=lin[v]", "-map", "[v]", "-frames:v", "1"])
        .arg(out_png)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(60) => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    out_png.exists().then(|| out_png.to_path_buf())
}

// ---------- the verdict for a whole album ----------
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fake {
    None,
    Mild,
    Strong,
}

#[derive(Clone)]
pub struct Verdict {
    pub damaged: Vec<String>,
    /// MP3s with a few (3-9) decode errors: usually harmless junk at the end of the file
    pub glitchy: Vec<String>,
    pub fake: Fake,
    pub cutoff_hz: f64,
    pub est_kbps: u32,
    pub summary: String,
    pub fake_text: String,
    pub short: String,
    pub unchecked: usize,
    /// indexes (into the tracks given) of the tracks that looked fake
    pub flagged: Vec<usize>,
    base_summary: String,
    base_short: String,
}

/// The bits of a track the verdict needs.
pub struct VTrack<'a> {
    pub base: &'a str,
    pub ext: &'a str,
    pub codec: &'a str,
    pub kbps: u32,
    pub duration: f64,
    pub lossless: bool,
    pub quality: Option<&'a Quality>,
}

/// Upscaled lossy files: real upscales in the library sample had 31-64dB cliffs; a test 128->320 re-encode had 24
const LOSSY_CLIFF_MIN: f64 = 22.0;

fn estimate_kbps(cutoff: f64) -> u32 {
    if cutoff <= 17000.0 {
        128
    } else if cutoff <= 18000.0 {
        160
    } else if cutoff <= 19250.0 {
        192
    } else if cutoff <= 19750.0 {
        256
    } else {
        320
    }
}

pub fn verdict(tracks: &[VTrack]) -> Verdict {
    use crate::importer::util::trim_num;
    let mut v = Verdict {
        damaged: vec![],
        glitchy: vec![],
        fake: Fake::None,
        cutoff_hz: 0.0,
        est_kbps: 0,
        summary: String::new(),
        fake_text: String::new(),
        short: String::new(),
        unchecked: 0,
        flagged: vec![],
        base_summary: String::new(),
        base_short: String::new(),
    };
    let lossless: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].lossless).collect();
    let measured: Vec<usize> = lossless.iter().cloned().filter(|&i| tracks[i].quality.map(|q| q.measured).unwrap_or(false)).collect();
    if !lossless.is_empty() {
        let q = |i: usize| tracks[i].quality.unwrap();
        // a track "looks like an MP3" if it has a sharp cliff at or below 20.5kHz
        let strong: Vec<usize> = measured.iter().cloned().filter(|&i| q(i).cliff_db >= 28.0 && q(i).cutoff_hz > 0.0 && q(i).cutoff_hz <= 19500.0).collect();
        let any: Vec<usize> = measured.iter().cloned().filter(|&i| q(i).cliff_db >= 25.0 && q(i).cutoff_hz > 0.0 && q(i).cutoff_hz <= 20500.0).collect();
        let need = ((measured.len() as f64 / 2.0).ceil() as usize).max(1); // majority of the tracks we could judge
        let mut codecs: Vec<String> = lossless.iter().map(|&i| tracks[i].codec.to_uppercase()).collect();
        codecs.sort();
        codecs.dedup();
        let codec = regex::Regex::new(r"PCM_\w+").unwrap().replace_all(&codecs.join("/"), "WAV").to_string();
        if !measured.is_empty() && strong.len() >= need {
            v.fake = Fake::Strong;
            v.flagged = strong;
        } else if !measured.is_empty() && any.len() >= need {
            v.fake = Fake::Mild;
            v.flagged = any;
        }
        if !v.flagged.is_empty() {
            let mut cuts: Vec<f64> = v.flagged.iter().map(|&i| q(i).cutoff_hz).collect();
            cuts.sort_by(|a, b| a.total_cmp(b));
            v.cutoff_hz = cuts[cuts.len() / 2];
            v.est_kbps = estimate_kbps(v.cutoff_hz);
            let khz = format!("{}kHz", trim_num(v.cutoff_hz / 1000.0));
            v.short = format!("Fake {} (from ~{}k MP3)", codec, v.est_kbps);
            v.fake_text = format!(
                "Not real lossless: {} of {} tracks checked stop sharply at {}, the mark of a ~{}kbps MP3 that was converted to {}.",
                v.flagged.len(),
                measured.len(),
                khz,
                v.est_kbps,
                codec
            );
            v.summary = v.fake_text.clone();
        } else if !measured.is_empty() {
            v.short = format!("{} - real lossless", codec);
            v.summary = format!(
                "Real lossless: {} of {} tracks checked show no MP3 cutoff{}.",
                measured.len(),
                lossless.len(),
                if measured.len() < lossless.len() { " (the rest were too quiet or dull to judge)" } else { "" }
            );
        } else {
            v.short = format!("{} - couldn't verify", codec);
            v.summary = "Couldn't tell if this is real lossless: the tracks have too little high-frequency sound to judge.".into();
        }
    } else {
        let kb = (tracks.iter().map(|t| t.kbps as f64).sum::<f64>() / tracks.len().max(1) as f64).round() as u32;
        let mut exts: Vec<String> = tracks.iter().map(|t| t.ext.trim_start_matches('.').to_uppercase()).collect();
        exts.sort();
        exts.dedup();
        let fmt = exts.join("/");
        v.short = format!("{fmt} ~{kb}kbps");
        // upscaled: labelled high bitrate, but the sound stops where a low-bitrate file's would (same rule as Library Health):
        // 250k+ stopping at <=18.5kHz, or 190-249k stopping at <=16.5kHz
        let m: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].kbps >= 190 && tracks[i].quality.map(|q| q.measured).unwrap_or(false)).collect();
        let up: Vec<usize> = m
            .iter()
            .cloned()
            .filter(|&i| {
                let q = tracks[i].quality.unwrap();
                q.cliff_db >= LOSSY_CLIFF_MIN && q.cutoff_hz > 0.0 && q.cutoff_hz <= if tracks[i].kbps >= 250 { 18500.0 } else { 16500.0 }
            })
            .collect();
        if !m.is_empty() && up.len() >= ((m.len() as f64 / 2.0).ceil() as usize).max(1) {
            let mut cuts: Vec<f64> = up.iter().map(|&i| tracks[i].quality.unwrap().cutoff_hz).collect();
            cuts.sort_by(|a, b| a.total_cmp(b));
            v.cutoff_hz = cuts[cuts.len() / 2];
            v.est_kbps = estimate_kbps(v.cutoff_hz);
            v.fake = if v.cutoff_hz <= 17000.0 { Fake::Strong } else { Fake::Mild };
            v.short = format!("Upscaled {fmt} (really ~{}k)", v.est_kbps);
            v.fake_text = format!(
                "Not real {kb}kbps: {} of {} tracks checked stop sharply at {}kHz, the mark of a ~{}kbps file that was re-encoded bigger.",
                up.len(),
                m.len(),
                trim_num(v.cutoff_hz / 1000.0),
                v.est_kbps
            );
            v.summary = v.fake_text.clone();
            v.flagged = up;
        } else if !m.is_empty() {
            v.summary = format!("{fmt} at about {kb}kbps: {} track(s) checked show the full sound of a real {kb}kbps file.", m.len());
        } else {
            v.summary = format!("{fmt} at about {kb}kbps (lossy; too low a bitrate to check for upscaling).");
        }
    }
    v.base_summary = v.summary.clone();
    v.base_short = v.short.clone();
    set_damage(&mut v, tracks, &std::collections::HashSet::new());
    v
}

/// Damaged files. `length_ok`: MP3s (by index) whose decoded length matches the official song, so a shorter-looking file
/// only has a wrong length header. MP3 decode errors use Library Health's thresholds: 10+ = damaged, 3-9 = minor glitches.
pub fn set_damage(v: &mut Verdict, tracks: &[VTrack], length_ok: &std::collections::HashSet<usize>) {
    use crate::importer::util::format_time;
    v.damaged.clear();
    v.glitchy.clear();
    v.unchecked = 0;
    for (i, t) in tracks.iter().enumerate() {
        let Some(q) = t.quality.filter(|q| q.check_error.is_empty()) else {
            v.unchecked += 1;
            continue;
        };
        let (short, bad) = if t.lossless {
            let short = t.duration > 5.0 && q.decoded_seconds < t.duration - 2.0;
            (short, !q.decode_ok || short)
        } else {
            let short = t.duration > 5.0 && q.decoded_seconds < t.duration * 0.95 - 2.0 && !length_ok.contains(&i);
            let bad = q.error_count >= 10 || short;
            if !bad && q.error_count >= 3 {
                v.glitchy.push(t.base.to_string());
            }
            (short, bad)
        };
        if bad {
            let why = if short { format!("stops at {} of {}", format_time(q.decoded_seconds), format_time(t.duration)) } else { "decode errors".to_string() };
            v.damaged.push(format!("{} ({})", t.base, why));
        }
    }
    v.summary = v.base_summary.clone();
    if !v.damaged.is_empty() {
        v.summary.push_str(&format!(" {} damaged file(s).", v.damaged.len()));
    } else if v.unchecked > 0 {
        v.summary.push_str(&format!(" {} file(s) couldn't be checked.", v.unchecked));
    } else if !v.glitchy.is_empty() {
        v.summary.push_str(&format!(" {} file(s) have a few minor decode errors.", v.glitchy.len()));
    } else {
        v.summary.push_str(" All files decode cleanly.");
    }
    v.short = if !v.damaged.is_empty() {
        let n = v.damaged.len();
        format!("{} damaged file{}{}", n, if n > 1 { "s" } else { "" }, if v.fake != Fake::None { format!(", {}", v.base_short) } else { String::new() })
    } else {
        v.base_short.clone()
    };
}
