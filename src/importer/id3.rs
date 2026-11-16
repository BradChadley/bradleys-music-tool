//! A small reader for an MP3's ID3v2 tag: which frames it holds, and the text of the ones that are text.
//!
//! ffprobe cannot tell a real lyrics frame (`USLT`) from a generic text tag that only carries the name
//! (`TXXX:USLT`, what ffmpeg writes when it converts a file), and does not report timed lyrics (`SYLT`) at
//! all. This reads the frame list itself. It only reads: pictures and other large frames are stepped over,
//! not loaded, so looking at a file costs a few small reads.
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// One frame of the tag.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// the four-letter name ("TIT2", "USLT", "TXXX"); the three-letter names of ID3v2.2 are given as
    /// their four-letter successors where the lyric rules need them
    pub id: String,
    /// the description of a `TXXX`, `COMM` or `USLT` frame ("" for the others)
    pub desc: String,
    /// the text of a text, comment or lyrics frame ("" for the others)
    pub text: String,
}

/// No text frame worth reading is larger than this; a larger one is named but its text is left out.
const TEXT_MAX: usize = 2_000_000;
/// A tag saved with the old "unsynchronisation" scheme has to be read whole. Never more than this.
const WHOLE_MAX: usize = 64_000_000;

fn syncsafe(b: &[u8]) -> usize {
    (b[0] as usize & 0x7f) << 21 | (b[1] as usize & 0x7f) << 14 | (b[2] as usize & 0x7f) << 7 | (b[3] as usize & 0x7f)
}

fn plain(b: &[u8]) -> usize {
    (b[0] as usize) << 24 | (b[1] as usize) << 16 | (b[2] as usize) << 8 | b[3] as usize
}

/// Takes the 0x00 bytes back out that "unsynchronisation" put after every 0xFF.
fn resync(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        out.push(data[i]);
        if data[i] == 0xFF && i + 1 < data.len() && data[i + 1] == 0 {
            i += 1;
        }
        i += 1;
    }
    out
}

/// Text in one of the four encodings ID3 knows, up to its end mark. Returns the text and what follows it.
fn text_until_end(enc: u8, data: &[u8]) -> (String, &[u8]) {
    let wide = enc == 1 || enc == 2;
    let mut end = data.len();
    let mut rest = data.len();
    if wide {
        let mut i = 0;
        while i + 1 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                end = i;
                rest = i + 2;
                break;
            }
            i += 2;
        }
    } else if let Some(i) = data.iter().position(|b| *b == 0) {
        end = i;
        rest = i + 1;
    }
    (decode(enc, &data[..end]), &data[rest.min(data.len())..])
}

fn decode(enc: u8, data: &[u8]) -> String {
    let s = match enc {
        0 => data.iter().map(|&b| b as char).collect::<String>(),
        1 | 2 => {
            let (mut big, mut body) = (enc == 2, data);
            if body.len() >= 2 && body[0] == 0xFF && body[1] == 0xFE {
                big = false;
                body = &body[2..];
            } else if body.len() >= 2 && body[0] == 0xFE && body[1] == 0xFF {
                big = true;
                body = &body[2..];
            }
            let units: Vec<u16> = body.chunks_exact(2).map(|c| if big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(data).to_string(),
    };
    s.trim_matches(|c: char| c == '\0' || c == '\u{feff}').to_string()
}

/// The text a frame carries, by its kind.
fn read_text(id: &str, body: &[u8]) -> (String, String) {
    if body.is_empty() {
        return (String::new(), String::new());
    }
    let enc = body[0];
    match id {
        "USLT" | "COMM" if body.len() >= 4 => {
            let (desc, rest) = text_until_end(enc, &body[4..]);
            (desc, decode(enc, rest))
        }
        "TXXX" => {
            let (desc, rest) = text_until_end(enc, &body[1..]);
            (desc, decode(enc, rest).replace('\0', "/"))
        }
        _ if id.starts_with('T') => (String::new(), decode(enc, &body[1..]).replace('\0', "/")),
        _ => (String::new(), String::new()),
    }
}

/// ID3v2.2 called these by three letters.
fn long_name(id: &str) -> String {
    match id {
        "ULT" => "USLT",
        "SLT" => "SYLT",
        "TXX" => "TXXX",
        "COM" => "COMM",
        "TT2" => "TIT2",
        "TP1" => "TPE1",
        "TAL" => "TALB",
        other => other,
    }
    .to_string()
}

fn wanted(id: &str) -> bool {
    id.starts_with('T') || id == "USLT" || id == "COMM"
}

/// The frames of the ID3v2 tag at the start of a file. None when the file has no such tag or it cannot
/// be read; a tag that breaks off half way gives the frames up to there.
pub fn frames(path: &Path) -> Option<Vec<Frame>> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut h = [0u8; 10];
    f.read_exact(&mut h).ok()?;
    if &h[..3] != b"ID3" || h[3] < 2 || h[3] > 4 {
        return None;
    }
    let (ver, flags, size) = (h[3], h[5], syncsafe(&h[6..10]));
    // the whole tag was "unsynchronised" (versions 2.2 and 2.3 do it to the tag, 2.4 frame by frame):
    // sizes then count the bytes as they were before, so the tag is read whole and put right first
    if flags & 0x80 != 0 && ver < 4 {
        if size > WHOLE_MAX {
            return None;
        }
        let mut raw = vec![0u8; size];
        f.read_exact(&mut raw).ok()?;
        return Some(walk(&mut std::io::Cursor::new(resync(&raw)), ver, flags, u64::MAX));
    }
    Some(walk(&mut f, ver, flags, size as u64))
}

fn walk<R: Read + Seek>(r: &mut R, ver: u8, flags: u8, size: u64) -> Vec<Frame> {
    let mut out = vec![];
    let start = r.stream_position().unwrap_or(0);
    let end = start.saturating_add(size);
    // an extended header comes first when the tag says so
    if flags & 0x40 != 0 && ver >= 3 {
        let mut b = [0u8; 4];
        if r.read_exact(&mut b).is_err() {
            return out;
        }
        let skip = if ver == 4 { syncsafe(&b).saturating_sub(4) } else { plain(&b) };
        if r.seek(SeekFrom::Current(skip as i64)).is_err() {
            return out;
        }
    }
    let head = if ver == 2 { 6 } else { 10 };
    loop {
        let at = r.stream_position().unwrap_or(u64::MAX);
        if at.saturating_add(head as u64) > end {
            break;
        }
        let mut h = [0u8; 10];
        if r.read_exact(&mut h[..head]).is_err() {
            break;
        }
        let name = &h[..if ver == 2 { 3 } else { 4 }];
        if !name.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            break; // the padding after the last frame, or a damaged tag
        }
        let id = long_name(&String::from_utf8_lossy(name));
        let len = match ver {
            2 => (h[3] as usize) << 16 | (h[4] as usize) << 8 | h[5] as usize,
            // some programs write the sizes of a 2.4 tag the 2.3 way: a size with a high bit set is one of those
            4 if h[4..8].iter().all(|b| b & 0x80 == 0) => syncsafe(&h[4..8]),
            _ => plain(&h[4..8]),
        };
        let format = if ver >= 3 { h[9] } else { 0 };
        // (compressed or encrypted frames are named, their text is not read)
        let packed = if ver == 4 { format & 0x0C != 0 } else { format & 0xC0 != 0 };
        if at.saturating_add((head + len) as u64) > end {
            break;
        }
        let mut frame = Frame { id, ..Default::default() };
        if wanted(&frame.id) && len <= TEXT_MAX && !packed {
            let mut body = vec![0u8; len];
            if r.read_exact(&mut body).is_err() {
                break;
            }
            let mut body = if ver == 4 && format & 0x02 != 0 { resync(&body) } else { body };
            // a 2.4 frame may say how long its data is before anything else
            if ver == 4 && format & 0x01 != 0 && body.len() >= 4 {
                body.drain(..4);
            }
            (frame.desc, frame.text) = read_text(&frame.id, &body);
        } else if r.seek(SeekFrom::Current(len as i64)).is_err() {
            break;
        }
        out.push(frame);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(ver: u8, flags: u8, frames: &[u8]) -> Vec<u8> {
        let n = frames.len();
        let mut out = vec![b'I', b'D', b'3', ver, 0, flags, (n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f];
        out.extend_from_slice(frames);
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // where the audio would start
        out
    }
    fn frame(ver: u8, id: &str, body: &[u8]) -> Vec<u8> {
        let n = body.len();
        let mut out = id.as_bytes().to_vec();
        match ver {
            2 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            4 => out.extend_from_slice(&[(n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f, 0, 0]),
            _ => out.extend_from_slice(&[(n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8, 0, 0]),
        }
        out.extend_from_slice(body);
        out
    }
    fn read(bytes: &[u8]) -> Option<Vec<Frame>> {
        let file = std::env::temp_dir().join(format!("music-recs-id3-{}-{:x}.mp3", std::process::id(), crate::importer::scan::rand_id()));
        std::fs::write(&file, bytes).unwrap();
        let out = frames(&file);
        let _ = std::fs::remove_file(&file);
        out
    }
    fn utf16(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        out.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        out
    }

    #[test]
    fn a_real_lyrics_frame_is_told_from_a_text_tag_that_only_has_its_name() {
        let mut body = vec![];
        body.extend(frame(3, "TIT2", b"\x00Song"));
        // a real lyrics frame, in UTF-16 with a description
        let mut uslt = vec![1, b'e', b'n', b'g'];
        uslt.extend(utf16("words"));
        uslt.extend([0, 0]);
        uslt.extend(utf16("first line\nsecond line"));
        body.extend(frame(3, "USLT", &uslt));
        // what ffmpeg leaves when it converts a file: a text tag named after the frame
        body.extend(frame(3, "TXXX", b"\x03USLT\x00hidden words"));
        // a picture is stepped over, however large it says it is inside the tag
        body.extend(frame(3, "APIC", &vec![7u8; 5000]));
        body.extend(frame(3, "SYLT", b"\x00eng\x02\x01\x00line\x00\x00\x00\x03\xe8"));
        body.extend(frame(3, "COMM", b"\x00eng\x00a comment"));
        body.extend([0u8; 64]); // padding
        let got = read(&tag(3, 0, &body)).unwrap();
        let ids: Vec<&str> = got.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["TIT2", "USLT", "TXXX", "APIC", "SYLT", "COMM"]);
        assert_eq!((got[0].text.as_str(), got[1].desc.as_str(), got[1].text.as_str()), ("Song", "words", "first line\nsecond line"));
        assert_eq!((got[2].desc.as_str(), got[2].text.as_str()), ("USLT", "hidden words"));
        assert_eq!(got[5].text, "a comment");
    }

    #[test]
    fn the_three_versions_and_their_oddities_are_read() {
        // 2.4: sizes that leave the top bit of every byte free, UTF-8
        let got = read(&tag(4, 0, &frame(4, "USLT", "\u{3}eng\0caf\u{e9} words".as_bytes()))).unwrap();
        assert_eq!(got[0].text, "caf\u{e9} words");
        // 2.4 written by a program that counts sizes the 2.3 way (200 has a high bit set)
        let long = "x".repeat(199);
        let mut f = b"TIT2".to_vec();
        f.extend([0, 0, 0, 200, 0, 0, 3]);
        f.extend(long.as_bytes());
        assert_eq!(read(&tag(4, 0, &f)).unwrap()[0].text, long);
        // 2.2: three-letter names
        let got = read(&tag(2, 0, &[frame(2, "TT2", b"\x00Old Song"), frame(2, "ULT", b"\x00eng\x00old words")].concat())).unwrap();
        assert_eq!((got[0].id.as_str(), got[1].id.as_str(), got[1].text.as_str()), ("TIT2", "USLT", "old words"));
        // 2.3 with the whole tag "unsynchronised": a 0x00 after every 0xFF, sizes as before that
        let body = frame(3, "USLT", b"\x00eng\x00\xffwords");
        let mut raw = vec![];
        for b in &body {
            raw.push(*b);
            if *b == 0xFF {
                raw.push(0);
            }
        }
        assert_eq!(read(&tag(3, 0x80, &raw)).unwrap()[0].text, "\u{ff}words");
        // an extended header is stepped over
        let mut ext = vec![0, 0, 0, 6, 0, 0, 0, 0, 0, 0];
        ext.extend(frame(3, "TIT2", b"\x00After"));
        assert_eq!(read(&tag(3, 0x40, &ext)).unwrap()[0].text, "After");
    }

    #[test]
    fn a_file_without_a_tag_or_with_a_broken_one_never_panics() {
        assert!(read(b"").is_none() && read(b"RIFF....WAVE").is_none() && read(b"ID3").is_none());
        // a frame that says it runs past the end of the tag: the frames before it are still given
        let mut body = frame(3, "TIT2", b"\x00Song");
        body.extend(b"USLT\x00\x0f\xff\xff\x00\x00");
        let got = read(&tag(3, 0, &body)).unwrap();
        assert_eq!(got.len(), 1);
        // a tag cut off in the middle of a frame
        let whole = tag(3, 0, &frame(3, "TIT2", b"\x00Song title"));
        assert_eq!(read(&whole[..18]).unwrap().len(), 0);
        // nonsense where a frame should start
        assert_eq!(read(&tag(3, 0, b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c")).unwrap().len(), 0);
    }
}
