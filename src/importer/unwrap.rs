//! ".mp3" files that are really MP3 audio inside a WAV ("RIFF") wrapper, often with an ID3 tag stuck in front.
//! Some players can't open them. A copy is unwrapped byte for byte (tag + the raw MP3 frames + any ID3v1 tag) and only
//! used once its audio fingerprint matches the original's. Port of LibraryRepair.ps1 Invoke-UnwrapStep.
use crate::importer::tools;
use std::io::Read;
use std::path::{Path, PathBuf};

/// length of the ID3v2 tag at the start (with its footer, if any), from its 10-byte header; 0 = none
fn id3v2_len(h: &[u8]) -> usize {
    if h.len() < 10 || &h[0..3] != b"ID3" {
        return 0;
    }
    let size = ((h[6] as usize & 0x7f) << 21) | ((h[7] as usize & 0x7f) << 14) | ((h[8] as usize & 0x7f) << 7) | (h[9] as usize & 0x7f);
    10 + size + if h[5] & 0x10 != 0 { 10 } else { 0 }
}

/// true for an .mp3 whose audio sits inside a WAV wrapper, after any ID3 tag
pub fn is_riff_in_mp3(path: &Path) -> bool {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; 10];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    let skip = id3v2_len(&head) as u64;
    let mut b = [0u8; 4];
    f.seek(SeekFrom::Start(skip)).is_ok() && f.read_exact(&mut b).is_ok() && &b == b"RIFF"
}

/// An MP3's own length header (Xing/Info/VBRI frame count), in seconds; 0 if it has none. When the decoded length matches
/// it, the file is complete and players show the right length; a longer guess from ffprobe then only comes from extra
/// tags at the end of the file (e.g. an APE tag holding a picture). Port of LibraryScanner.Mp3HeaderSeconds.
pub fn mp3_header_secs(path: &Path) -> f64 {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return 0.0 };
    let mut head = [0u8; 10];
    if f.read_exact(&mut head).is_err() || f.seek(SeekFrom::Start(id3v2_len(&head) as u64)).is_err() {
        return 0.0;
    }
    let mut b = vec![0u8; 8192];
    let n = f.read(&mut b).unwrap_or(0);
    let be = |i: usize| ((b[i] as u64) << 24) | ((b[i + 1] as u64) << 16) | ((b[i + 2] as u64) << 8) | b[i + 3] as u64;
    for i in 0..n.saturating_sub(4) {
        if b[i] != 0xFF || b[i + 1] & 0xE0 != 0xE0 {
            continue;
        }
        let (ver, layer, sri, mode) = ((b[i + 1] >> 3) & 3, (b[i + 1] >> 1) & 3, (b[i + 2] >> 2) & 3, (b[i + 3] >> 6) & 3);
        if ver == 1 || layer != 1 || sri == 3 {
            continue; // not a valid MPEG layer III frame header
        }
        let rates = match ver {
            3 => [44100.0, 48000.0, 32000.0],
            2 => [22050.0, 24000.0, 16000.0],
            _ => [11025.0, 12000.0, 8000.0],
        };
        let spf = if ver == 3 { 1152.0 } else { 576.0 };
        let side = if ver == 3 { if mode == 3 { 17 } else { 32 } } else if mode == 3 { 9 } else { 17 };
        let x = i + 4 + side;
        let mut frames = 0u64;
        if x + 12 <= n && (&b[x..x + 4] == b"Xing" || &b[x..x + 4] == b"Info") {
            if b[x + 7] & 1 != 0 {
                frames = be(x + 8);
            }
        } else if i + 36 + 18 <= n && &b[i + 36..i + 40] == b"VBRI" {
            frames = be(i + 36 + 14);
        }
        return if frames > 0 { frames as f64 * spf / rates[sri as usize] } else { 0.0 }; // only the first frame can carry it
    }
    0.0
}

/// ffmpeg's fingerprint of the audio packets only (tags ignored)
pub fn audio_md5(ffmpeg: &Path, path: &Path, force_mp3: bool) -> String {
    let mut c = tools::command(ffmpeg, true);
    c.args(["-hide_banner", "-nostdin", "-v", "error"]);
    if force_mp3 {
        c.args(["-f", "mp3"]);
    }
    c.args(crate::importer::tools::LOCAL_ONLY).arg("-i").arg(path).args(["-map", "0:a", "-c", "copy", "-f", "md5", "-"]);
    c.stderr(std::process::Stdio::null());
    String::from_utf8_lossy(&tools::output_limited(&mut c, 600)).trim().to_string()
}

/// Writes an unwrapped copy of `src` to `dest`. Ok only if its audio matches the original exactly.
pub fn unwrap_to(ffmpeg: &Path, src: &Path, dest: &Path) -> Result<(), String> {
    let bytes = std::fs::read(src).map_err(|e| e.to_string())?;
    let tag = id3v2_len(&bytes).min(bytes.len());
    if bytes.len() < tag + 12 || &bytes[tag..tag + 4] != b"RIFF" {
        return Err("not the WAV-wrapped kind this can unwrap".into());
    }
    // walk the RIFF chunks to the "data" chunk
    let mut p = tag + 12;
    let mut data: Option<(usize, usize)> = None;
    while p + 8 <= bytes.len() {
        let id = &bytes[p..p + 4];
        let len = u32::from_le_bytes([bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7]]) as usize;
        if id == b"data" {
            let at = p + 8;
            data = Some((at, len.min(bytes.len() - at)));
            break;
        }
        p += 8 + len + (len % 2);
    }
    let (at, len) = data.filter(|d| d.1 > 0).ok_or("couldn't find the audio inside the wrapper")?;
    let mut out = Vec::with_capacity(tag + len + 128);
    out.extend_from_slice(&bytes[..tag]);
    out.extend_from_slice(&bytes[at..at + len]);
    if bytes.len() > 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG" {
        out.extend_from_slice(&bytes[bytes.len() - 128..]);
    }
    std::fs::write(dest, &out).map_err(|e| e.to_string())?;
    let (h1, h2) = (audio_md5(ffmpeg, src, true), audio_md5(ffmpeg, dest, false));
    if h2.is_empty() || h1 != h2 {
        let _ = std::fs::remove_file(dest);
        return Err("the unwrapped file didn't match the original audio".into());
    }
    Ok(())
}

/// One spot per source file (same file = same spot), so rescans reuse the unwrapped copy.
pub fn unwrapped_copy(ffmpeg: &Path, work_dir: &Path, src: &Path, size: u64, modified: std::time::SystemTime) -> Option<PathBuf> {
    let stamp = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let id = format!("{:x}", md5::compute(format!("{}|{size}|{stamp}", src.to_string_lossy().to_lowercase())));
    let dir = work_dir.join("unwrapped").join(&id[..16]);
    let dest = dir.join(src.file_name()?);
    let done = dir.join(".ok");
    if done.exists() && dest.exists() {
        return Some(dest);
    }
    std::fs::create_dir_all(&dir).ok()?;
    match unwrap_to(ffmpeg, src, &dest) {
        Ok(()) => {
            let _ = std::fs::write(&done, b"");
            Some(dest)
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
}

/// temporary unwrapped copies from runs more than two days ago
pub fn clean_old(work_dir: &Path) {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
    for e in std::fs::read_dir(work_dir.join("unwrapped")).map(|r| r.flatten().collect::<Vec<_>>()).unwrap_or_default() {
        if e.metadata().and_then(|m| m.modified()).map(|t| t < cutoff).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}
