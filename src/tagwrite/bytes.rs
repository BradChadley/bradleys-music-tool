//! The tag editor's byte work, done by hand and never by a tag library: where a file's audio is (and its
//! fingerprint), whether an MP3's tag can be read whole, a second tag stacked behind the first, the old
//! short tag at an MP3's end, an APE tag, and a missing VBR length header.
use std::path::Path;

pub(super) fn syncsafe(b: &[u8]) -> usize {
    (b[0] as usize & 0x7f) << 21 | (b[1] as usize & 0x7f) << 14 | (b[2] as usize & 0x7f) << 7 | (b[3] as usize & 0x7f)
}
fn be32(b: &[u8]) -> usize {
    (b[0] as usize) << 24 | (b[1] as usize) << 16 | (b[2] as usize) << 8 | b[3] as usize
}
pub(super) fn is_mp3(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
}

/// Whether another program has the file open (it cannot be opened with no sharing at all).
pub(super) fn locked(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(path).is_err()
}

/// The ID3v2 tag at the very start: (where it ends, its version, its flags), or None.
pub(super) fn id3_at_start(b: &[u8]) -> Option<(usize, u8, u8)> {
    if b.len() < 10 || &b[..3] != b"ID3" {
        return None;
    }
    let end = 10 + syncsafe(&b[6..10]) + if b[5] & 0x10 != 0 { 10 } else { 0 };
    Some((end.min(b.len()), b[3], b[5]))
}

/// An MP3's ID3v2 tag must be fully readable before it is rewritten: if part of it cannot be read (a
/// picture whose stated size is wrong, say), saving would quietly drop whatever is behind it. None when it
/// is clean (or there is none), else the reason. Also says whether the tag's frame sizes are written the
/// v2.4 way inside a v2.3 tag (some older iTunes did that), which the reader then needs to be told.
pub(super) fn id3_clean(path: &Path, b: &[u8]) -> (Option<String>, bool) {
    if !is_mp3(path) || b.len() < 10 || &b[..3] != b"ID3" {
        return (None, false);
    }
    let (ver, flags) = (b[3], b[5]);
    if ver < 3 || flags & 0x80 != 0 {
        return (None, false); // v2.2 and unsynchronised tags: the reader handles these itself
    }
    let size = syncsafe(&b[6..10]);
    let tag = &b[10..(10 + size).min(b.len())];
    let tag = if tag.len() < size { [tag, &vec![0u8; size - tag.len()][..]].concat() } else { tag.to_vec() };
    let mut start = 0;
    if flags & 0x40 != 0 && tag.len() >= 4 {
        start = if ver == 4 { syncsafe(&tag[..4]) } else { 4 + be32(&tag[..4]) };
    }
    let mut worst: Option<String> = None;
    for (n, sync) in [ver == 4, ver != 4].into_iter().enumerate() {
        let mut pos = start;
        let mut why = None;
        while pos + 10 <= size {
            if tag[pos] == 0 {
                break; // padding starts
            }
            if !tag[pos..pos + 4].iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                why = Some(format!("its tag is damaged at byte {pos}"));
                break;
            }
            let fsz = if sync { syncsafe(&tag[pos + 4..pos + 8]) } else { be32(&tag[pos + 4..pos + 8]) };
            if pos + 10 + fsz > size {
                why = Some("its tag is damaged (an entry runs past the end)".to_string());
                break;
            }
            pos += 10 + fsz;
        }
        if why.is_none() && tag[pos.min(size)..size].iter().any(|&x| x != 0) {
            why = Some(format!("part of its tag ({} KB) can't be read", ((size - pos) as f64 / 1024.0).round()));
        }
        if why.is_none() {
            // (the second way of reading is the v2.4 way inside a v2.3 tag)
            return (None, n == 1 && ver == 3);
        }
        if worst.is_none() {
            worst = why;
        }
    }
    (Some(format!("{}, so editing it could lose data - left alone", worst.unwrap_or_default())), false)
}

/// A v2.3 tag whose frame sizes were written the v2.4 way, rewritten in memory with plain sizes (what the
/// v2.3 reader expects). Only called when `id3_clean` found that this is the way it reads whole.
pub(super) fn plain_sizes(b: &[u8]) -> Vec<u8> {
    let mut out = b.to_vec();
    let size = syncsafe(&b[6..10]);
    let mut pos = 10;
    if b[5] & 0x40 != 0 {
        pos += 4 + be32(&b[10..14]);
    }
    while pos + 10 <= 10 + size && b[pos] != 0 {
        let fsz = syncsafe(&b[pos + 4..pos + 8]);
        out[pos + 4..pos + 8].copy_from_slice(&(fsz as u32).to_be_bytes());
        pos += 10 + fsz;
    }
    out
}

/// One text entry of a second, stacked tag.
#[derive(Debug, Clone)]
pub(super) struct StackedFrame {
    pub id: String,
    pub text: String,
}

/// Some rippers leave a SECOND ID3v2 tag right after the first. The editor edits only the first, but
/// players and the scan read both, so junk in the second would stay. Returns where it is, how long, and
/// its entries with their text.
pub(super) fn stacked_id3(b: &[u8]) -> Option<(usize, usize, Vec<StackedFrame>)> {
    if b.len() < 20 || &b[..3] != b"ID3" {
        return None;
    }
    let end1 = 10 + syncsafe(&b[6..10]) + if b[5] & 0x10 != 0 { 10 } else { 0 };
    if end1 + 10 > b.len() || &b[end1..end1 + 3] != b"ID3" {
        return None;
    }
    let ver = b[end1 + 3];
    let size2 = syncsafe(&b[end1 + 6..end1 + 10]);
    let mut frames = vec![];
    let (mut pos, stop) = (end1 + 10, (end1 + 10 + size2).min(b.len()));
    while pos + 10 <= stop {
        let id = &b[pos..pos + 4];
        if !id.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            break;
        }
        let fs = if ver == 4 { syncsafe(&b[pos + 4..pos + 8]) } else { be32(&b[pos + 4..pos + 8]) };
        if fs == 0 || pos + 10 + fs > stop {
            break;
        }
        let id = String::from_utf8_lossy(id).to_string();
        let mut body = &b[pos + 11..pos + 10 + fs];
        if id == "COMM" && body.len() >= 3 {
            body = &body[3..]; // the language
        }
        let mut text = String::new();
        if id.starts_with('T') || id == "COMM" {
            text = decode(b[pos + 10], body).trim_start_matches('\u{feff}').trim_matches('\0').to_string();
            if id == "COMM" || id == "TXXX" {
                text = text.split('\0').filter(|s| !s.is_empty()).next_back().unwrap_or("").to_string();
            }
        }
        frames.push(StackedFrame { id, text: text.trim().to_string() });
        pos += 10 + fs;
    }
    Some((end1, 10 + size2, frames))
}

/// Text in one of ID3's four encodings.
pub(super) fn decode(enc: u8, body: &[u8]) -> String {
    match enc {
        0 => body.iter().map(|&c| c as char).collect(),
        1 | 2 => {
            let mut be = enc == 2;
            let mut s = body;
            if s.len() >= 2 && (s[..2] == [0xff, 0xfe] || s[..2] == [0xfe, 0xff]) {
                be = s[0] == 0xfe;
                s = &s[2..];
            }
            let u: Vec<u16> = s.chunks_exact(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
            String::from_utf16_lossy(&u)
        }
        _ => String::from_utf8_lossy(body).to_string(),
    }
}

/// The old 128-byte ID3v1 tag at the end, or empty.
pub(super) fn id3v1_block(b: &[u8]) -> &[u8] {
    if b.len() > 128 && &b[b.len() - 128..b.len() - 125] == b"TAG" {
        &b[b.len() - 128..]
    } else {
        &[]
    }
}
/// An APE tag before the end (and before an ID3v1 tag of `v1_len` bytes), or empty.
pub(super) fn ape_block(b: &[u8], v1_len: usize) -> &[u8] {
    let end = b.len() - v1_len;
    if end < 32 || &b[end - 32..end - 24] != b"APETAGEX" {
        return &[];
    }
    let size = u32::from_le_bytes(b[end - 20..end - 16].try_into().unwrap()) as usize;
    let header = u32::from_le_bytes(b[end - 12..end - 8].try_into().unwrap()) & 0x8000_0000 != 0;
    let len = size + if header { 32 } else { 0 };
    if len == 0 || len > end {
        return &[];
    }
    &b[end - len..end]
}

/// Where a file's audio is, found by hand: (start, end) of the bytes that editing tags must leave alone,
/// plus, for a FLAC, its first block (what the sound is: its length and a checksum of it), which must stay
/// too. None when the file's layout is not one this knows (then the packet check is used, `packets_md5`).
fn audio_range(path: &Path, b: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut start = 0;
    while b.len() >= start + 10 && &b[start..start + 3] == b"ID3" {
        start += 10 + syncsafe(&b[start + 6..start + 10]) + if b[start + 5] & 0x10 != 0 { 10 } else { 0 };
    }
    let mut end = b.len();
    let v1 = id3v1_block(b).len();
    end -= v1;
    end -= ape_block(b, v1).len();
    match ext.as_str() {
        "mp3" => (start < end).then(|| (start, end, vec![])),
        "flac" => {
            if b.len() < start + 4 || &b[start..start + 4] != b"fLaC" {
                return None;
            }
            let mut p = start + 4;
            let mut info = vec![];
            loop {
                if p + 4 > b.len() {
                    return None;
                }
                let h = &b[p..p + 4];
                let len = (h[1] as usize) << 16 | (h[2] as usize) << 8 | h[3] as usize;
                if p + 4 + len > b.len() {
                    return None;
                }
                if h[0] & 0x7f == 0 {
                    info = b[p + 4..p + 4 + len].to_vec();
                }
                p += 4 + len;
                if h[0] & 0x80 != 0 {
                    break;
                }
            }
            (p < end && !info.is_empty()).then_some((p, end, info))
        }
        _ => None,
    }
}

/// An M4A's sound: what every `mdat` box holds, in order.
fn mdat(b: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![];
    let mut p = 0;
    let mut found = false;
    while p + 8 <= b.len() {
        let mut size = be32(&b[p..p + 4]);
        let mut head = 8;
        if size == 1 {
            if p + 16 > b.len() {
                return None;
            }
            size = u64::from_be_bytes(b[p + 8..p + 16].try_into().unwrap()) as usize;
            head = 16;
        } else if size == 0 {
            size = b.len() - p;
        }
        if size < head || p + size > b.len() {
            return None;
        }
        if &b[p + 4..p + 8] == b"mdat" {
            out.extend_from_slice(&b[p + head..p + size]);
            found = true;
        }
        p += size;
    }
    found.then_some(out)
}

/// The audio's fingerprint: its length and a SHA-256 of it (FLAC: of its first block and its sound; M4A:
/// of its `mdat` boxes), found by hand. Files of other kinds (Ogg, Opus, WAV ...) are fingerprinted by
/// their packets through ffmpeg, since an Ogg file's page headers move when its comments grow.
pub(super) fn audio_hash(path: &Path, ffmpeg: &Path) -> Option<String> {
    let b = std::fs::read(path).ok()?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "m4a" || ext == "mp4" || ext == "m4b" {
        let m = mdat(&b)?;
        return Some(format!("{}:{}", m.len(), sha256_hex(&[&m])));
    }
    if let Some((s, e, info)) = audio_range(path, &b) {
        return Some(format!("{}:{}", e - s, sha256_hex(&[&info, &b[s..e]])));
    }
    if ["mp3", "flac"].contains(&ext.as_str()) {
        return None; // its layout is not what it should be: never edited
    }
    packets_md5(path, ffmpeg, None)
}

/// ffmpeg's MD5 of the audio packets as they are (`-c copy`), not decoded. `force`: the format to read as.
pub(super) fn packets_md5(path: &Path, ffmpeg: &Path, force: Option<&str>) -> Option<String> {
    let mut c = crate::importer::tools::command(ffmpeg, false);
    c.args(["-hide_banner", "-nostdin", "-v", "error"]);
    if let Some(f) = force {
        c.args(["-f", f]);
    }
    c.arg("-i").arg(path).args(["-map", "0:a", "-c", "copy", "-f", "md5", "-"]);
    let out = c.stderr(std::process::Stdio::null()).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && s.starts_with("MD5=")).then_some(s)
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finish().iter().map(|x| format!("{x:02X}")).collect()
}

/// A small SHA-256 (the app has no other use for one).
struct Sha256 {
    h: [u32; 8],
    buf: Vec<u8>,
    len: u64,
}
impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    fn new() -> Self {
        Sha256 { h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19], buf: Vec::with_capacity(64), len: 0 }
    }
    fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if !self.buf.is_empty() {
            let take = (64 - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == 64 {
                let block: [u8; 64] = self.buf[..].try_into().unwrap();
                self.block(&block);
                self.buf.clear();
            }
        }
        let mut chunks = data.chunks_exact(64);
        for c in &mut chunks {
            self.block(c.try_into().unwrap());
        }
        self.buf.extend_from_slice(chunks.remainder());
    }
    fn block(&mut self, c: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(c[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(Self::K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (h, x) in self.h.iter_mut().zip(v) {
            *h = h.wrapping_add(x);
        }
    }
    fn finish(mut self) -> [u8; 32] {
        let bits = self.len * 8;
        let mut pad = vec![0x80u8];
        while (self.buf.len() + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&bits.to_be_bytes());
        let len = self.len;
        self.update(&pad);
        self.len = len;
        let mut out = [0u8; 32];
        for (i, h) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&h.to_be_bytes());
        }
        out
    }
}

/// Missing VBR length header: ffmpeg rebuilds the audio part with a proper header, then the ORIGINAL tag
/// bytes are put back around it (the ID3v2 tag in front, any APE and ID3v1 tags at the end), and the
/// sound is checked to be the same. None when done, else why not.
pub(super) fn vbr_header(path: &Path, ffmpeg: &Path) -> Option<String> {
    if locked(path) {
        return Some("file is open in another program".into());
    }
    let Ok(bytes) = std::fs::read(path) else { return Some("the file could not be read".into()) };
    let id3 = id3_at_start(&bytes).map(|(end, ..)| &bytes[..end]).unwrap_or(&[]);
    let v1 = id3v1_block(&bytes);
    let ape = ape_block(&bytes, v1.len());
    let tmp = super::temp_path(path);
    let (raw, bare, cmp) = (with_suffix(&tmp, ".audio.mp3"), with_suffix(&tmp, ".bare.mp3"), with_suffix(&tmp, ".cmp.mp3"));
    let cleanup = |files: &[&Path]| {
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    };
    let why = (|| -> Option<String> {
        let failed = |e: std::io::Error| Some(format!("a working copy could not be written ({e})"));
        // the original without its end tags: ffmpeg would otherwise read the APE tag's bytes as broken audio
        let mid = bytes.len() - v1.len() - ape.len();
        if let Err(e) = std::fs::write(&bare, &bytes[..mid]) {
            return failed(e);
        }
        let mut c = crate::importer::tools::command(ffmpeg, false);
        c.args(["-hide_banner", "-nostdin", "-v", "error", "-y", "-i"]).arg(&bare);
        c.args(["-map", "0:a", "-c:a", "copy", "-map_metadata", "-1", "-id3v2_version", "0", "-write_id3v1", "0", "-write_xing", "1", "-f", "mp3", "--"]).arg(&raw);
        let _ = c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        let Ok(audio) = std::fs::read(&raw) else { return Some("ffmpeg couldn't rebuild the file".to_string()) };
        let out = [id3, &audio[..], ape, v1].concat();
        if let Err(e) = std::fs::write(&tmp, &out).and_then(|_| std::fs::write(&cmp, &out[..out.len() - v1.len() - ape.len()])) {
            return failed(e);
        }
        // (the script compared ffmpeg's MD5 of both, read as they are)
        let was = packets_md5(&bare, ffmpeg, None);
        let same = was.is_some() && was == packets_md5(&cmp, ffmpeg, None);
        if !same {
            return Some("the rebuilt file didn't match the original audio, so nothing was saved".to_string());
        }
        std::fs::rename(&tmp, path).err().map(|e| format!("the file could not be replaced ({e})"))
    })();
    cleanup(&[&tmp, &raw, &bare, &cmp]);
    why
}

fn with_suffix(p: &Path, s: &str) -> std::path::PathBuf {
    let mut o = p.as_os_str().to_owned();
    o.push(s);
    o.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard() {
        assert_eq!(sha256_hex(&[b"abc"]), "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD");
        // more than one block, given in pieces that do not line up with blocks
        let a = vec![b'a'; 1000];
        assert_eq!(sha256_hex(&[&a[..3], &a[3..700], &a[700..]]), "41EDECE42D63E8D9BF515A9BA6932E1C20CBC9F5A5D134645ADB5DB1B9737EA3");
    }
}
