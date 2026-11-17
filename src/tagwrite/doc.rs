//! A file's tags, open for reading and changing: an MP3's ID3v2 tag through the `id3` crate (which keeps
//! the file's own version, v2.3 or v2.4, and every frame in it), a FLAC's comments and pictures, an M4A's
//! boxes, and the other kinds Health can meet, through lofty. Field names follow the existing editor
//! contract in each format, so the files come out the way they did.
use super::bytes;
use super::other::{OggFile, OtherFile};
use id3::{Content, TagLike};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::OggPictureStorage;
use lofty::picture::{MimeType, PictureType};
use lofty::tag::ItemKey;
use std::borrow::Cow;
use std::path::Path;

/// The stable field names the editor reads and writes.
pub(super) const FIELDS: [&str; 36] = [
    "title", "artist", "album", "album artist", "genre", "year", "track", "disc", "track total", "disc total", "composer", "comment", "bpm", "grouping", "conductor", "isrc", "publisher", "key", "remixer", "subtitle", "title sort", "album sort",
    "artist sort", "album artist sort", "composer sort", "musicbrainz artist", "musicbrainz album", "musicbrainz album artist", "musicbrainz track", "musicbrainz release group", "musicbrainz disc", "musicbrainz status", "musicbrainz type",
    "musicbrainz country", "lyrics", "compilation",
];

/// The ID3v2 text frame of a field, where it has one.
fn id3_frame(field: &str) -> Option<&'static str> {
    Some(match field {
        "title" => "TIT2",
        "artist" => "TPE1",
        "album" => "TALB",
        "album artist" => "TPE2",
        "genre" => "TCON",
        "composer" => "TCOM",
        "bpm" => "TBPM",
        "grouping" => "TIT1",
        "conductor" => "TPE3",
        "isrc" => "TSRC",
        "publisher" => "TPUB",
        "key" => "TKEY",
        "remixer" => "TPE4",
        "subtitle" => "TIT3",
        "title sort" => "TSOT",
        "album sort" => "TSOA",
        "artist sort" => "TSOP",
        "album artist sort" => "TSO2",
        "composer sort" => "TSOC",
        "compilation" => "TCMP",
        _ => return None,
    })
}
/// The description of the ID3v2 TXXX frame (an M4A's freeform box name) of a MusicBrainz field.
fn mb_name(field: &str) -> Option<&'static str> {
    Some(match field {
        "musicbrainz artist" => "MusicBrainz Artist Id",
        "musicbrainz album" => "MusicBrainz Album Id",
        "musicbrainz album artist" => "MusicBrainz Album Artist Id",
        "musicbrainz track" => "MusicBrainz Track Id",
        "musicbrainz release group" => "MusicBrainz Release Group Id",
        "musicbrainz disc" => "MusicBrainz Disc Id",
        "musicbrainz status" => "MusicBrainz Album Status",
        "musicbrainz type" => "MusicBrainz Album Type",
        "musicbrainz country" => "MusicBrainz Album Release Country",
        _ => return None,
    })
}
/// A FLAC's (Vorbis comment) name of a field.
fn vorbis_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "title" => "TITLE",
        "artist" => "ARTIST",
        "album" => "ALBUM",
        "album artist" => "ALBUMARTIST",
        "genre" => "GENRE",
        "year" => "DATE",
        "track" => "TRACKNUMBER",
        "disc" => "DISCNUMBER",
        "track total" => "TRACKTOTAL",
        "disc total" => "DISCTOTAL",
        "composer" => "COMPOSER",
        "comment" => "COMMENT",
        "bpm" => "TEMPO",
        "grouping" => "GROUPING",
        "conductor" => "CONDUCTOR",
        "isrc" => "ISRC",
        "publisher" => "ORGANIZATION",
        "key" => "INITIALKEY",
        "remixer" => "REMIXEDBY",
        "subtitle" => "SUBTITLE",
        "title sort" => "TITLESORT",
        "album sort" => "ALBUMSORT",
        "artist sort" => "ARTISTSORT",
        "album artist sort" => "ALBUMARTISTSORT",
        "composer sort" => "COMPOSERSORT",
        "musicbrainz artist" => "MUSICBRAINZ_ARTISTID",
        "musicbrainz album" => "MUSICBRAINZ_ALBUMID",
        "musicbrainz album artist" => "MUSICBRAINZ_ALBUMARTISTID",
        "musicbrainz track" => "MUSICBRAINZ_TRACKID",
        "musicbrainz release group" => "MUSICBRAINZ_RELEASEGROUPID",
        "musicbrainz disc" => "MUSICBRAINZ_DISCID",
        "musicbrainz status" => "MUSICBRAINZ_ALBUMSTATUS",
        "musicbrainz type" => "MUSICBRAINZ_ALBUMTYPE",
        "musicbrainz country" => "RELEASECOUNTRY",
        "lyrics" => "LYRICS",
        "compilation" => "COMPILATION",
        _ => return None,
    })
}
/// An M4A's box of a field: a four-letter one, or a freeform ("----") one under com.apple.iTunes.
fn mp4_ident(field: &str) -> Option<AtomIdent<'static>> {
    let four = |b: &[u8; 4]| AtomIdent::Fourcc(*b);
    let free = |n: &'static str| AtomIdent::Freeform { mean: Cow::Borrowed("com.apple.iTunes"), name: Cow::Borrowed(n) };
    Some(match field {
        "title" => four(b"\xa9nam"),
        "artist" => four(b"\xa9ART"),
        "album" => four(b"\xa9alb"),
        "album artist" => four(b"aART"),
        "genre" => four(b"\xa9gen"),
        "year" => four(b"\xa9day"),
        "composer" => four(b"\xa9wrt"),
        "comment" => four(b"\xa9cmt"),
        "grouping" => four(b"\xa9grp"),
        "conductor" => four(b"cond"),
        "subtitle" => four(b"Subt"),
        "title sort" => four(b"sonm"),
        "album sort" => four(b"soal"),
        "artist sort" => four(b"soar"),
        "album artist sort" => four(b"soaa"),
        "composer sort" => four(b"soco"),
        "lyrics" => four(b"\xa9lyr"),
        "isrc" => free("ISRC"),
        "publisher" => free("publisher"),
        "key" => free("initialkey"),
        "remixer" => free("REMIXEDBY"),
        f => free(mb_name(f)?),
    })
}
/// lofty's name of a field, for the other kinds of file.
/// The other name lofty may give a field, depending on the kind of tag (an ID3 tag's lyrics are
/// `UnsyncLyrics` and its BPM `IntegerBpm`).
fn also(k: ItemKey) -> Option<ItemKey> {
    match k {
        ItemKey::Lyrics => Some(ItemKey::UnsyncLyrics),
        ItemKey::Bpm => Some(ItemKey::IntegerBpm),
        ItemKey::Publisher => Some(ItemKey::Label),
        _ => None,
    }
}
fn item_key(field: &str) -> Option<ItemKey> {
    Some(match field {
        "title" => ItemKey::TrackTitle,
        "artist" => ItemKey::TrackArtist,
        "album" => ItemKey::AlbumTitle,
        "album artist" => ItemKey::AlbumArtist,
        "genre" => ItemKey::Genre,
        "year" => ItemKey::RecordingDate,
        "track" => ItemKey::TrackNumber,
        "disc" => ItemKey::DiscNumber,
        "track total" => ItemKey::TrackTotal,
        "disc total" => ItemKey::DiscTotal,
        "composer" => ItemKey::Composer,
        "comment" => ItemKey::Comment,
        "bpm" => ItemKey::Bpm,
        "grouping" => ItemKey::ContentGroup,
        "conductor" => ItemKey::Conductor,
        "isrc" => ItemKey::Isrc,
        "publisher" => ItemKey::Publisher,
        "key" => ItemKey::InitialKey,
        "remixer" => ItemKey::Remixer,
        "subtitle" => ItemKey::TrackSubtitle,
        "title sort" => ItemKey::TrackTitleSortOrder,
        "album sort" => ItemKey::AlbumTitleSortOrder,
        "artist sort" => ItemKey::TrackArtistSortOrder,
        "album artist sort" => ItemKey::AlbumArtistSortOrder,
        "composer sort" => ItemKey::ComposerSortOrder,
        "musicbrainz artist" => ItemKey::MusicBrainzArtistId,
        "musicbrainz album" => ItemKey::MusicBrainzReleaseId,
        "musicbrainz album artist" => ItemKey::MusicBrainzReleaseArtistId,
        "musicbrainz track" => ItemKey::MusicBrainzRecordingId,
        "musicbrainz release group" => ItemKey::MusicBrainzReleaseGroupId,
        "musicbrainz type" => ItemKey::MusicBrainzReleaseType,
        "musicbrainz country" => ItemKey::ReleaseCountry,
        "lyrics" => ItemKey::Lyrics,
        "compilation" => ItemKey::FlagCompilation,
        _ => return None,
    })
}

/// The first number in a text ("03/12" is 3), 0 for none.
pub(super) fn number(s: &str) -> u32 {
    s.trim().split('/').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}
/// The number after a slash ("3/12" is 12), 0 for none.
fn after_slash(s: &str) -> u32 {
    s.split('/').nth(1).and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}
fn year_of(s: &str) -> String {
    let d: String = s.trim().chars().take(4).collect();
    if d.len() == 4 && d.chars().all(|c| c.is_ascii_digit()) && d != "0000" {
        d
    } else {
        String::new()
    }
}
/// A number as TagLib gives it as text: none for 0.
fn num_text(n: u32) -> String {
    if n == 0 { String::new() } else { n.to_string() }
}

/// What a picture file is, by its name (as TagLib tells it).
pub(super) fn mime_of(path: &Path) -> &'static str {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    }
}

/// Where one bit of junk sits, so it can be taken out.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum JunkAt {
    Frame(usize),
    V1Comment,
    Vorbis(String, String),
    Mp4(AtomIdent<'static>),
    Item(ItemKey),
}
/// One (field, value) pair the way ffprobe names it, for matching the scan's exact junk values.
#[derive(Clone, Debug)]
pub(super) struct Junk {
    pub key: String,
    pub value: String,
    pub at: JunkAt,
}

/// An MP3's tags: the ID3v2 tag, and the old ID3v1 one at the end when there is one.
pub(super) struct Mp3 {
    pub tag: id3::Tag,
    pub version: id3::Version,
    bytes: Vec<u8>,
    tag_end: usize,
    pub v1: Option<[u8; 128]>,
    /// A WAV or AIFF file: its ID3 tag is a chunk inside it (written in place by id3), and its RIFF INFO
    /// or AIFF text entries, if it has any, are `info` (lofty, with the ID3 chunk left out of it).
    pub chunked: bool,
    info: Option<Box<OtherFile>>,
}

/// A field from lofty's general view of a file's tags, under whichever name that kind of tag has for it.
fn generic_get(t: &OtherFile, field: &str) -> String {
    let Some(k) = item_key(field) else { return String::new() };
    let views = t.views();
    let v = [k].into_iter().chain(also(k)).find_map(|k| views.iter().find_map(|tag| tag.get_string(k).filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string()))).unwrap_or_default();
    match field {
        "year" => year_of(&v),
        "track" | "disc" | "track total" | "disc total" | "bpm" => num_text(number(&v)),
        "compilation" => if v == "1" { v } else { String::new() },
        _ => v,
    }
}
/// Sets a field in every tag the file has (as TagLib did), under the name that kind of tag has for it.
fn generic_set(t: &mut OtherFile, field: &str, value: &str) {
    let Some(k) = item_key(field) else { return };
    let v = match field {
        "track" | "disc" | "track total" | "disc total" | "bpm" | "year" => num_text(number(value)),
        _ => value.trim().to_string(),
    };
    t.edit(|tag| {
        let keys: Vec<ItemKey> = [k].into_iter().chain(also(k)).collect();
        for k in &keys {
            tag.remove_key(*k);
        }
        if let Some(k) = keys.iter().find(|k| k.map_key(tag.tag_type()).is_some()).filter(|_| !v.is_empty()) {
            tag.insert_text(*k, v.clone());
        }
    });
}

pub(super) enum Doc {
    Mp3(Mp3),
    Flac(Box<FlacFile>),
    Mp4(Box<Mp4File>),
    Ogg(Box<OggFile>),
    Other(Box<OtherFile>),
}

/// Files are read as they are: lofty would otherwise change some values while reading (a FLAC's track
/// "3/12" split in two), which the editor must only do when the plan says so.
fn parse_options() -> ParseOptions {
    ParseOptions::new().read_properties(false).implicit_conversions(false)
}

impl Doc {
    /// Opens a file's tags. An error says why they cannot be read.
    pub fn open(path: &Path) -> Result<Doc, String> {
        Self::open_with(path, false)
    }
    /// Opens the tags of the file a conversion started from, only ever read: an ID3 tag with a damaged
    /// entry is read as far as it goes (the file it is written into is read whole or not at all).
    pub fn open_source(path: &Path) -> Result<Doc, String> {
        Self::open_with(path, true)
    }
    fn open_with(path: &Path, lenient: bool) -> Result<Doc, String> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "mp3" => {
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                Self::mp3_from(bytes, lenient)
            }
            "dsf" => {
                // (a DSF file keeps an ID3v2 tag at the end, where its header points; only ever read)
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                if bytes.len() < 28 || &bytes[..4] != b"DSD " {
                    return Err("it is not a DSF file".into());
                }
                let at = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
                if at == 0 || at >= bytes.len() {
                    return Self::mp3_from(vec![], true);
                }
                Self::mp3_from(bytes[at..].to_vec(), true)
            }
            "flac" => {
                let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                Ok(Doc::Flac(Box::new(FlacFile::read_from(&mut f, parse_options()).map_err(|e| e.to_string())?)))
            }
            "m4a" | "mp4" | "m4b" => {
                let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                Ok(Doc::Mp4(Box::new(Mp4File::read_from(&mut f, parse_options()).map_err(|e| e.to_string())?)))
            }
            "wav" | "aif" | "aiff" | "aifc" => Self::chunked(path, &ext, lenient),
            e => {
                if let Some(o) = OggFile::open(path, e, parse_options())? {
                    return Ok(Doc::Ogg(Box::new(o)));
                }
                match OtherFile::open(path, e, parse_options())? {
                    Some(o) => Ok(Doc::Other(Box::new(o))),
                    None => Err(format!("a .{e} file's tags cannot be changed here")),
                }
            }
        }
    }
    /// Whether the editor can change this kind of file's tags (WMA, DSF, DFF and TTA it cannot; DSF it
    /// can read, as the file a conversion started from).
    pub fn writable(path: &Path) -> bool {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        ["mp3", "flac", "m4a", "mp4", "m4b", "ogg", "oga", "opus", "spx", "wav", "aif", "aiff", "aifc", "wv", "ape", "mpc"].contains(&ext.as_str())
    }

    fn mp3_from(bytes: Vec<u8>, lenient: bool) -> Result<Doc, String> {
        let (tag, version, tag_end) = match bytes::id3_at_start(&bytes) {
            Some((end, ver, _)) => {
                // (a v2.3 tag with its frame sizes written the v2.4 way is read with plain sizes)
                let (_, itunes) = bytes::id3_clean(Path::new("x.mp3"), &bytes);
                let src = if itunes { bytes::plain_sizes(&bytes[..end]) } else { bytes[..end].to_vec() };
                let read = id3::Tag::read_from2(std::io::Cursor::new(src));
                let tag = if lenient { id3::partial_tag_ok(read) } else { read }.map_err(|e| e.description.to_string())?;
                let version = if ver == 4 { id3::Version::Id3v24 } else { id3::Version::Id3v23 };
                (tag, version, end)
            }
            None => (id3::Tag::with_version(id3::Version::Id3v23), id3::Version::Id3v23, 0),
        };
        let v1 = bytes::id3v1_block(&bytes).try_into().ok();
        Ok(Doc::Mp3(Mp3 { tag, version, bytes, tag_end, v1, chunked: false, info: None }))
    }

    /// A WAV or AIFF file: its ID3 chunk through id3 (kept frame for frame, as an MP3's tag), the rest of
    /// its tags through lofty.
    fn chunked(path: &Path, ext: &str, lenient: bool) -> Result<Doc, String> {
        let read = id3::Tag::read_from_path(path);
        let tag = match if lenient { id3::partial_tag_ok(read) } else { read } {
            Ok(t) => t,
            Err(e) if matches!(e.kind, id3::ErrorKind::NoTag) => id3::Tag::with_version(id3::Version::Id3v23),
            Err(e) => return Err(e.description.to_string()),
        };
        let version = if tag.version() == id3::Version::Id3v24 { id3::Version::Id3v24 } else { id3::Version::Id3v23 };
        let mut info = OtherFile::open(path, ext, parse_options())?.ok_or("not a WAV or AIFF file")?;
        info.strip_id3();
        Ok(Doc::Mp3(Mp3 { tag, version, bytes: vec![], tag_end: 0, v1: None, chunked: true, info: Some(Box::new(info)) }))
    }

    pub fn is_mp3(&self) -> bool {
        matches!(self, Doc::Mp3(_))
    }
    pub fn is_flac(&self) -> bool {
        matches!(self, Doc::Flac(_))
    }
    pub fn id3(&self) -> Option<&id3::Tag> {
        if let Doc::Mp3(m) = self { Some(&m.tag) } else { None }
    }
    pub fn id3_mut(&mut self) -> Option<&mut id3::Tag> {
        if let Doc::Mp3(m) = self { Some(&mut m.tag) } else { None }
    }
    pub fn mp3(&self) -> Option<&Mp3> {
        if let Doc::Mp3(m) = self { Some(m) } else { None }
    }
    /// A FLAC's (or an Ogg file's) comment block.
    pub fn vorbis(&self) -> Option<&VorbisComments> {
        match self {
            Doc::Flac(f) => f.vorbis_comments(),
            Doc::Ogg(o) => Some(o.comments()),
            _ => None,
        }
    }
    fn vorbis_mut(&mut self) -> Option<&mut VorbisComments> {
        match self {
            Doc::Flac(f) => {
                if f.vorbis_comments().is_none() {
                    f.set_vorbis_comments(VorbisComments::default());
                }
                f.vorbis_comments_mut()
            }
            Doc::Ogg(o) => Some(o.comments_mut()),
            _ => None,
        }
    }
    fn ilst_mut(&mut self) -> Option<&mut Ilst> {
        if let Doc::Mp4(f) = self {
            if f.ilst().is_none() {
                f.set_ilst(Ilst::default());
            }
            f.ilst_mut()
        } else {
            None
        }
    }
    /// Takes every value of a Vorbis comment name out.
    pub fn remove_vorbis(&mut self, key: &str) {
        if let Some(v) = self.vorbis_mut() {
            let _ = v.remove(key).count();
        }
    }
    /// The Vorbis comment value of a name, first one ("" for none).
    pub fn vorbis_first(&self, key: &str) -> String {
        self.vorbis().and_then(|v| v.get(key)).unwrap_or("").to_string()
    }

    // ---------------------------------------------------------------- fields

    /// A field as one line of text ("" = none): more than one value joined with "/", numbers of 0 as none.
    pub fn get(&self, field: &str) -> String {
        match self {
            Doc::Mp3(m) => {
                let v = m.get(field);
                // (a WAV or AIFF file: its INFO or text entries when the ID3 chunk has nothing)
                match &m.info {
                    Some(i) if v.is_empty() => generic_get(i, field),
                    _ => v,
                }
            }
            Doc::Flac(_) | Doc::Ogg(_) => {
                let v = self.vorbis();
                let all = |k: &str| v.map(|v| v.get_all(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")).unwrap_or_default();
                match field {
                    "track" => num_text(number(&self.vorbis_first("TRACKNUMBER"))),
                    "disc" => num_text(number(&self.vorbis_first("DISCNUMBER"))),
                    "track total" => {
                        let t = number(&self.vorbis_first("TRACKTOTAL")).max(number(&self.vorbis_first("TOTALTRACKS")));
                        num_text(if t > 0 { t } else { after_slash(&self.vorbis_first("TRACKNUMBER")) })
                    }
                    "disc total" => {
                        let t = number(&self.vorbis_first("DISCTOTAL")).max(number(&self.vorbis_first("TOTALDISCS")));
                        num_text(if t > 0 { t } else { after_slash(&self.vorbis_first("DISCNUMBER")) })
                    }
                    "year" => year_of(&self.vorbis_first("DATE")),
                    // (TagLib also reads a BPM written under that name)
                    "bpm" => num_text(number(&self.vorbis_first("TEMPO")).max(number(&self.vorbis_first("BPM")))),
                    "album artist" => {
                        let a = all("ALBUMARTIST");
                        if a.is_empty() { all("ALBUM ARTIST") } else { a }
                    }
                    "comment" => {
                        let c = all("COMMENT");
                        if c.is_empty() { all("DESCRIPTION") } else { c }
                    }
                    "compilation" => if self.vorbis_first("COMPILATION").trim() == "1" { "1".into() } else { String::new() },
                    f => vorbis_key(f).map(all).unwrap_or_default(),
                }
            }
            Doc::Mp4(f) => {
                let Some(il) = f.ilst() else { return String::new() };
                let pair = |id: &[u8; 4]| -> (u32, u32) {
                    il.get(&AtomIdent::Fourcc(*id))
                        .and_then(|a| a.data().find_map(|d| if let AtomData::Unknown { data, .. } = d { Some(data.clone()) } else { None }))
                        .filter(|d| d.len() >= 6)
                        .map(|d| (u16::from_be_bytes([d[2], d[3]]) as u32, u16::from_be_bytes([d[4], d[5]]) as u32))
                        .unwrap_or((0, 0))
                };
                match field {
                    "track" => num_text(pair(b"trkn").0),
                    "track total" => num_text(pair(b"trkn").1),
                    "disc" => num_text(pair(b"disk").0),
                    "disc total" => num_text(pair(b"disk").1),
                    "year" => year_of(&text_of(il, &AtomIdent::Fourcc(*b"\xa9day"))),
                    "bpm" => il.get(&AtomIdent::Fourcc(*b"tmpo")).and_then(|a| a.data().find_map(int_of)).map(|n| num_text(n as u32)).unwrap_or_default(),
                    "compilation" => {
                        let on = il.get(&AtomIdent::Fourcc(*b"cpil")).is_some_and(|a| a.data().any(|d| matches!(d, AtomData::Bool(true)) || int_of(d).is_some_and(|n| n != 0)));
                        if on { "1".into() } else { String::new() }
                    }
                    "genre" => {
                        let g = text_of(il, &AtomIdent::Fourcc(*b"\xa9gen"));
                        if !g.is_empty() {
                            return g;
                        }
                        // (an old numbered genre)
                        il.get(&AtomIdent::Fourcc(*b"gnre")).and_then(|a| a.data().find_map(|d| if let AtomData::Unknown { data, .. } = d { Some(data.clone()) } else { None })).filter(|d| d.len() >= 2).and_then(|d| GENRES.get((u16::from_be_bytes([d[0], d[1]]) as usize).wrapping_sub(1)).map(|s| s.to_string())).unwrap_or_default()
                    }
                    f => mp4_ident(f).map(|id| text_of(il, &id)).unwrap_or_default(),
                }
            }
            Doc::Other(t) => generic_get(t, field),
        }
    }

    /// Sets a field to one value ("" or "0" for a number takes it out).
    pub fn set(&mut self, field: &str, value: &str) {
        match self {
            Doc::Mp3(m) => {
                m.set(field, value);
                if let Some(i) = m.info.as_mut().filter(|i| !i.views().is_empty()) {
                    generic_set(i, field, value);
                }
            }
            Doc::Flac(_) | Doc::Ogg(_) => {
                let (no, total) = (self.get(match field { "track" | "track total" => "track", _ => "disc" }), self.get(match field { "track" | "track total" => "track total", _ => "disc total" }));
                let v = self.vorbis_mut().unwrap();
                let put = |v: &mut VorbisComments, k: &str, s: &str| {
                    let _ = v.remove(k).count();
                    if !s.trim().is_empty() {
                        v.insert(k.to_string(), s.to_string());
                    }
                };
                match field {
                    // a FLAC keeps the number and the total apart ("3/12" in one is split)
                    "track" | "disc" => {
                        let (nk, tk, ok) = if field == "track" { ("TRACKNUMBER", "TRACKTOTAL", "TOTALTRACKS") } else { ("DISCNUMBER", "DISCTOTAL", "TOTALDISCS") };
                        put(v, nk, &num_text(number(value)));
                        if !total.is_empty() && v.get(tk).is_none() {
                            let _ = v.remove(ok).count();
                            put(v, tk, &total);
                        }
                    }
                    "track total" | "disc total" => {
                        let (nk, tk, ok) = if field == "track total" { ("TRACKNUMBER", "TRACKTOTAL", "TOTALTRACKS") } else { ("DISCNUMBER", "DISCTOTAL", "TOTALDISCS") };
                        let _ = v.remove(ok).count();
                        put(v, tk, &num_text(number(value)));
                        put(v, nk, &no);
                    }
                    "album artist" => {
                        let _ = v.remove("ALBUM ARTIST").count();
                        put(v, "ALBUMARTIST", value);
                    }
                    "comment" => {
                        let _ = v.remove("DESCRIPTION").count();
                        put(v, "COMMENT", value);
                    }
                    "compilation" => put(v, "COMPILATION", if value.trim() == "1" { "1" } else { "" }),
                    "year" | "bpm" => put(v, vorbis_key(field).unwrap(), &num_text(number(value))),
                    f => {
                        if let Some(k) = vorbis_key(f) {
                            put(v, k, value)
                        }
                    }
                }
            }
            Doc::Mp4(_) => {
                let (no, total) = (number(&self.get(if field.starts_with("track") { "track" } else { "disc" })), number(&self.get(if field.starts_with("track") { "track total" } else { "disc total" })));
                let il = self.ilst_mut().unwrap();
                match field {
                    // (the iTunes form: eight bytes for the track, as TagLib writes the disc too)
                    "track" | "track total" | "disc" | "disc total" => {
                        let (n, t) = if field.ends_with("total") { (no, number(value)) } else { (number(value), total) };
                        let id = if field.starts_with("track") { *b"trkn" } else { *b"disk" };
                        let _ = il.remove(&AtomIdent::Fourcc(id)).count();
                        if n > 0 || t > 0 {
                            let mut d = vec![0u8, 0];
                            d.extend((n as u16).to_be_bytes());
                            d.extend((t as u16).to_be_bytes());
                            d.extend([0, 0]);
                            il.insert(Atom::new(AtomIdent::Fourcc(id), AtomData::Unknown { code: lofty::mp4::DataType::Reserved, data: d }));
                        }
                    }
                    "bpm" => {
                        let _ = il.remove(&AtomIdent::Fourcc(*b"tmpo")).count();
                        if number(value) > 0 {
                            il.insert(Atom::new(AtomIdent::Fourcc(*b"tmpo"), AtomData::SignedInteger(number(value) as i32)));
                        }
                    }
                    "compilation" => {
                        let _ = il.remove(&AtomIdent::Fourcc(*b"cpil")).count();
                        if value.trim() == "1" {
                            il.insert(Atom::new(AtomIdent::Fourcc(*b"cpil"), AtomData::Bool(true)));
                        }
                    }
                    "year" => put_mp4(il, AtomIdent::Fourcc(*b"\xa9day"), &num_text(number(value))),
                    "genre" => {
                        let _ = il.remove(&AtomIdent::Fourcc(*b"gnre")).count();
                        put_mp4(il, AtomIdent::Fourcc(*b"\xa9gen"), value)
                    }
                    f => {
                        if let Some(id) = mp4_ident(f) {
                            put_mp4(il, id, value)
                        }
                    }
                }
            }
            Doc::Other(t) => generic_set(t, field, value),
        }
    }

    // ---------------------------------------------------------------- lyrics

    /// The words in the field players read ("" = none).
    pub fn real_lyrics(&self) -> String {
        let l = self.get("lyrics");
        if !l.trim().is_empty() {
            return l;
        }
        if self.is_flac() {
            for k in ["UNSYNCEDLYRICS", "UNSYNCED LYRICS"] {
                let v = self.vorbis_first(k);
                if !v.trim().is_empty() {
                    return v;
                }
            }
        }
        String::new()
    }
    /// The words in a text tag no player reads ("" = none); the longest, if there is more than one.
    pub fn hidden_lyrics(&self) -> String {
        let mut best = String::new();
        if let Some(t) = self.id3() {
            for x in t.extended_texts() {
                if hidden_name(&x.description) && x.value.trim().len() > best.trim().len() {
                    best = x.value.split('\0').map(str::trim_end).filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n");
                }
            }
        }
        best
    }
    pub fn set_lyrics(&mut self, words: &str) {
        self.set("lyrics", words)
    }
    /// The lyrics out, with any timed copy of them.
    pub fn clear_lyrics(&mut self) {
        self.set("lyrics", "");
        if let Some(t) = self.id3_mut() {
            t.remove("USLT");
            t.remove("SYLT");
        }
        if let Some(v) = self.vorbis_mut() {
            for k in ["UNSYNCEDLYRICS", "UNSYNCED LYRICS"] {
                let _ = v.remove(k).count();
            }
        }
    }

    // ---------------------------------------------------------------- pictures

    pub fn picture_count(&self) -> usize {
        match self {
            Doc::Mp3(m) => m.tag.pictures().count(),
            Doc::Flac(f) => f.pictures().len(),
            Doc::Mp4(f) => f.ilst().and_then(|i| i.pictures()).map(|p| p.count()).unwrap_or(0),
            Doc::Ogg(o) => o.comments().pictures().len(),
            Doc::Other(t) => t.views().iter().map(|t| t.pictures().len()).sum(),
        }
    }
    pub fn has_front(&self) -> bool {
        use id3::frame::PictureType as P;
        match self {
            Doc::Mp3(m) => m.tag.pictures().any(|p| matches!(p.picture_type, P::CoverFront | P::Other)),
            Doc::Flac(f) => f.pictures().iter().any(|(p, _)| matches!(p.pic_type(), PictureType::CoverFront | PictureType::Other)),
            // (TagLib calls every picture in an M4A a front cover)
            Doc::Mp4(_) => self.picture_count() > 0,
            Doc::Ogg(o) => o.comments().pictures().iter().any(|(p, _)| matches!(p.pic_type(), PictureType::CoverFront | PictureType::Other)),
            Doc::Other(t) => t.views().iter().any(|t| t.pictures().iter().any(|p| matches!(p.pic_type(), PictureType::CoverFront | PictureType::Other))),
        }
    }
    /// A new front cover in place of the front (and "other") pictures; back covers, booklets and the rest stay.
    /// `name`: the picture file's name, which TagLib wrote as the picture's description.
    pub fn put_front(&mut self, data: Vec<u8>, mime: &str, name: &str) {
        let lmime = MimeType::from_str(mime);
        let pic = || lofty::picture::Picture::unchecked(data.clone()).pic_type(PictureType::CoverFront).mime_type(lmime.clone()).description(name.to_string()).build();
        match self {
            Doc::Mp3(m) => {
                use id3::frame::PictureType as P;
                m.tag.frames_vec_mut().retain(|f| !matches!(f.content(), Content::Picture(p) if matches!(p.picture_type, P::CoverFront | P::Other)));
                // (put first among the pictures, where TagLib put it)
                let at = m.tag.frames().position(|f| f.id() == "APIC").unwrap_or(m.tag.frames().count());
                let frame = id3::Frame::with_content("APIC", Content::Picture(id3::frame::Picture { mime_type: mime.into(), picture_type: P::CoverFront, description: name.to_string(), data: data.clone() }));
                m.tag.frames_vec_mut().insert(at, frame);
            }
            Doc::Flac(f) => {
                f.remove_picture_type(PictureType::CoverFront);
                f.remove_picture_type(PictureType::Other);
                let _ = f.insert_picture(pic(), None);
            }
            Doc::Mp4(_) => {
                let il = self.ilst_mut().unwrap();
                il.remove_pictures();
                il.insert_picture(pic());
            }
            Doc::Ogg(o) => {
                let c = o.comments_mut();
                c.remove_picture_type(PictureType::CoverFront);
                c.remove_picture_type(PictureType::Other);
                let _ = c.insert_picture(pic(), None);
            }
            Doc::Other(t) => {
                // (into the tags that hold pictures: lofty leaves them out of the others when it saves)
                t.edit(|tag| {
                    tag.remove_picture_type(PictureType::CoverFront);
                    tag.remove_picture_type(PictureType::Other);
                    tag.push_picture(pic());
                });
            }
        }
    }

    // ---------------------------------------------------------------- junk and personal data

    /// The (field, value) pairs the scan may call junk, named the way ffprobe names them.
    pub fn junk_pairs(&self) -> Vec<Junk> {
        let mut out = vec![];
        match self {
            Doc::Mp3(m) => {
                for (i, f) in m.tag.frames().enumerate() {
                    let (key, value) = match (f.id(), f.content()) {
                        ("COMM", Content::Comment(c)) if !c.description.starts_with("iTun") => ("comment".to_string(), c.text.clone()),
                        ("TENC", Content::Text(t)) => ("encoded_by".into(), joined(t)),
                        ("TCOP", Content::Text(t)) => ("copyright".into(), joined(t)),
                        ("TPUB", Content::Text(t)) => ("publisher".into(), joined(t)),
                        ("TXXX", Content::ExtendedText(x)) => (x.description.to_lowercase(), joined(&x.value)),
                        _ => continue,
                    };
                    out.push(Junk { key, value, at: JunkAt::Frame(i) });
                }
                // the old ID3v1 tag at the end: players fall back to it, and junk often hides there
                if let Some(c) = m.v1_comment().filter(|c| !c.is_empty()) {
                    out.push(Junk { key: "comment".into(), value: c, at: JunkAt::V1Comment });
                }
            }
            Doc::Flac(_) | Doc::Ogg(_) => {
                if let Some(v) = self.vorbis() {
                    for (k, val) in v.items() {
                        // (ffprobe calls a FLAC's DESCRIPTION its comment)
                        let key = k.to_lowercase().replace('-', "_");
                        let key = if key == "description" { "comment".to_string() } else { key };
                        out.push(Junk { key, value: val.to_string(), at: JunkAt::Vorbis(k.to_string(), val.to_string()) });
                    }
                }
            }
            Doc::Mp4(f) => {
                if let Some(il) = f.ilst() {
                    for (key, id) in [("comment", AtomIdent::Fourcc(*b"\xa9cmt")), ("copyright", AtomIdent::Fourcc(*b"cprt"))] {
                        let v = text_of(il, &id);
                        if !v.is_empty() {
                            out.push(Junk { key: key.into(), value: v, at: JunkAt::Mp4(id) });
                        }
                    }
                }
            }
            Doc::Other(t) => {
                // every text entry, by its name in the file's own tag (as ffprobe names it)
                for tag in t.views() {
                    for item in tag.items() {
                        let Some(v) = item.value().text() else { continue };
                        let k = item.key();
                        let name = match k {
                            ItemKey::Comment => "comment".to_string(),
                            ItemKey::CopyrightMessage => "copyright".to_string(),
                            ItemKey::EncodedBy => "encoded_by".to_string(),
                            _ => k.map_key(tag.tag_type()).unwrap_or("").to_lowercase().replace('-', "_"),
                        };
                        out.push(Junk { key: if name == "description" { "comment".into() } else { name }, value: v.to_string(), at: JunkAt::Item(k) });
                    }
                }
            }
        }
        out
    }
    pub fn remove_junk(&mut self, list: &[JunkAt]) {
        match self {
            Doc::Mp3(m) => {
                let mut i = 0;
                m.tag.frames_vec_mut().retain(|_| {
                    i += 1;
                    !list.contains(&JunkAt::Frame(i - 1))
                });
                if list.contains(&JunkAt::V1Comment) {
                    m.set_v1_comment("");
                }
            }
            Doc::Flac(_) | Doc::Ogg(_) => {
                let v = self.vorbis_mut().unwrap();
                let keep: Vec<(String, String)> = v.take_items().filter(|(k, val)| !list.contains(&JunkAt::Vorbis(k.clone(), val.clone()))).collect();
                for (k, val) in keep {
                    v.push(k, val);
                }
            }
            Doc::Mp4(_) => {
                let il = self.ilst_mut().unwrap();
                for at in list {
                    if let JunkAt::Mp4(id) = at {
                        let _ = il.remove(id).count();
                    }
                }
            }
            Doc::Other(t) => {
                t.edit(|tag| {
                    for at in list {
                        if let JunkAt::Item(k) = at {
                            tag.remove_key(*k);
                        }
                    }
                });
            }
        }
    }

    /// What another person's player left in a download: their rating, how often they played it, and their
    /// DJ software's cue points and loops. Takes them out (or, with `remove` off, only counts them).
    pub fn personal(&mut self, remove: bool) -> usize {
        let mut n = 0;
        if let Some(t) = self.id3_mut() {
            let before = t.frames().count();
            n += t.frames().filter(|f| personal_frame(f)).count();
            if remove {
                t.frames_vec_mut().retain(|f| !personal_frame(f));
                debug_assert_eq!(t.frames().count() + n, before);
            }
        }
        if let Some(v) = self.vorbis() {
            let names: Vec<String> = v.items().map(|(k, _)| k.to_string()).filter(|k| personal_name(k)).collect();
            n += names.len();
            if remove && !names.is_empty() {
                let v = self.vorbis_mut().unwrap();
                for k in names {
                    let _ = v.remove(&k).count();
                }
            }
        }
        n
    }

    // ---------------------------------------------------------------- saving

    /// Saves the tags into the file at `path` (the working copy).
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        let open = || std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| e.to_string());
        match self {
            Doc::Mp3(m) if m.chunked => {
                // a WAV or AIFF file: its INFO or text entries through lofty (the ID3 chunk left out of
                // it), then the ID3 chunk through id3, which keeps every frame as it is
                if let Some(i) = &m.info {
                    i.save(&mut open()?)?;
                }
                m.tag.write_to_path(path, m.version).map_err(|e| format!("the tag could not be written ({})", e.description))
            }
            Doc::Mp3(m) => {
                let out = m.render()?;
                std::fs::write(path, out).map_err(|e| e.to_string())
            }
            Doc::Flac(f) => {
                // (an ID3 tag stuck in front of a FLAC stays as it is: it is not rewritten)
                f.remove_id3v2();
                f.save_to(&mut open()?, WriteOptions::new()).map_err(|e| e.to_string())
            }
            Doc::Mp4(f) => f.save_to(&mut open()?, WriteOptions::new()).map_err(|e| e.to_string()),
            Doc::Ogg(o) => o.save(&mut open()?),
            Doc::Other(t) => t.save(&mut open()?),
        }
    }
}

fn put_mp4(il: &mut Ilst, id: AtomIdent<'static>, value: &str) {
    let _ = il.remove(&id).count();
    if !value.trim().is_empty() {
        il.insert(Atom::new(id, AtomData::UTF8(value.to_string())));
    }
}
fn text_of(il: &Ilst, id: &AtomIdent<'_>) -> String {
    il.get(id).map(|a| a.data().filter_map(|d| if let AtomData::UTF8(s) = d { Some(s.trim().to_string()) } else { None }).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")).unwrap_or_default()
}
fn int_of(d: &AtomData) -> Option<i64> {
    match d {
        AtomData::SignedInteger(n) => Some(*n as i64),
        AtomData::UnsignedInteger(n) => Some(*n as i64),
        AtomData::Unknown { data, .. } if !data.is_empty() && data.len() <= 8 => Some(data.iter().fold(0i64, |a, b| a << 8 | *b as i64)),
        _ => None,
    }
}
/// The values of an ID3 text joined with "/" (v2.4 keeps them apart with a zero byte).
fn joined(t: &str) -> String {
    t.split('\0').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

/// ffmpeg cannot write a lyrics frame: when it converts a file it leaves the words in a generic text tag
/// named after the field, which no player reads. These are the names it uses.
pub(super) fn hidden_name(d: &str) -> bool {
    let d = d.to_lowercase();
    matches!(d.as_str(), "uslt" | "unsyncedlyrics" | "unsynced lyrics" | "lyrics") || d.starts_with("lyrics-")
}
pub(super) fn personal_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.starts_with("rating") || n.starts_with("fmps_rating") || n.starts_with("fmps_playcount") || matches!(n.as_str(), "playcount" | "play count" | "play_count" | "playcounter" | "play countr" | "play_counter")
}
pub(super) fn personal_frame(f: &id3::Frame) -> bool {
    if matches!(f.id(), "POPM" | "PCNT" | "GEOB") {
        return true;
    }
    match f.content() {
        Content::ExtendedText(x) => personal_name(&x.description),
        Content::Private(p) => {
            let o = p.owner_identifier.to_lowercase();
            o.contains("traktor") || o.contains("serato") || o.contains("rekordbox")
        }
        _ => false,
    }
}

impl Mp3 {
    fn text(&self, id: &str) -> String {
        self.tag.get(id).and_then(|f| f.content().text()).map(joined).unwrap_or_default()
    }
    fn put(&mut self, id: &str, value: &str) {
        self.tag.remove(id);
        if !value.trim().is_empty() {
            self.tag.add_frame(id3::Frame::text(id, value));
        }
    }
    fn get(&self, field: &str) -> String {
        match field {
            "track" => num_text(number(&self.text("TRCK"))),
            "track total" => num_text(after_slash(&self.text("TRCK"))),
            "disc" => num_text(number(&self.text("TPOS"))),
            "disc total" => num_text(after_slash(&self.text("TPOS"))),
            "year" => {
                let d = self.text("TDRC");
                let d = if d.is_empty() { self.text("TYER") } else { d };
                year_of(&d)
            }
            "bpm" => num_text(number(&self.text("TBPM"))),
            "genre" => self.tag.genres_parsed().join("/"),
            "compilation" => if self.text("TCMP") == "1" { "1".into() } else { String::new() },
            "comment" => {
                // the comment players show: no description (the iTunes notes and other named ones are not it)
                let mut c: Vec<&id3::frame::Comment> = self.tag.comments().filter(|c| !c.description.starts_with("iTun")).collect();
                c.sort_by_key(|c| !c.description.is_empty());
                c.first().map(|c| c.text.trim().to_string()).unwrap_or_default()
            }
            "lyrics" => {
                let mut l: Vec<&id3::frame::Lyrics> = self.tag.lyrics().collect();
                l.sort_by_key(|c| !c.description.is_empty());
                l.first().map(|c| c.text.clone()).filter(|t| !t.trim().is_empty()).unwrap_or_default()
            }
            "musicbrainz track" => self.tag.unique_file_identifiers().find(|u| u.owner_identifier == "http://musicbrainz.org").map(|u| String::from_utf8_lossy(&u.identifier).trim_matches('\0').trim().to_string()).unwrap_or_default(),
            f => {
                if let Some(id) = id3_frame(f) {
                    self.text(id)
                } else if let Some(d) = mb_name(f) {
                    self.tag.extended_texts().find(|x| x.description.eq_ignore_ascii_case(d)).map(|x| joined(&x.value)).unwrap_or_default()
                } else {
                    String::new()
                }
            }
        }
    }
    fn set(&mut self, field: &str, value: &str) {
        let pair = |no: u32, of: u32| if no == 0 && of == 0 { String::new() } else if of == 0 { no.to_string() } else { format!("{no}/{of}") };
        match field {
            "track" => {
                let of = after_slash(&self.text("TRCK"));
                self.put("TRCK", &pair(number(value), of));
                self.v1_track(number(value));
            }
            "track total" => {
                let no = number(&self.text("TRCK"));
                self.put("TRCK", &pair(no, number(value)));
            }
            "disc" => {
                let of = after_slash(&self.text("TPOS"));
                self.put("TPOS", &pair(number(value), of));
            }
            "disc total" => {
                let no = number(&self.text("TPOS"));
                self.put("TPOS", &pair(no, number(value)));
            }
            "year" => {
                // (v2.3 has a year frame; v2.4 a date)
                let y = num_text(number(value));
                self.tag.remove("TYER");
                self.tag.remove("TDRC");
                self.tag.remove("TDAT");
                self.tag.remove("TIME");
                self.tag.remove("TRDA");
                if !y.is_empty() {
                    let id = if self.version == id3::Version::Id3v24 { "TDRC" } else { "TYER" };
                    self.tag.add_frame(id3::Frame::text(id, y.clone()));
                }
                self.v1_put(93, 4, &y);
            }
            "bpm" => self.put("TBPM", &num_text(number(value))),
            "compilation" => self.put("TCMP", if value.trim() == "1" { "1" } else { "" }),
            "comment" => {
                self.tag.frames_vec_mut().retain(|f| !matches!(f.content(), Content::Comment(c) if c.description.is_empty()));
                if !value.trim().is_empty() {
                    self.tag.add_frame(id3::frame::Comment { lang: "eng".into(), description: String::new(), text: value.to_string() });
                }
            }
            "lyrics" => {
                self.tag.frames_vec_mut().retain(|f| !matches!(f.content(), Content::Lyrics(l) if l.description.is_empty()));
                if !value.trim().is_empty() {
                    // (put where the old words were, else at the end)
                    self.tag.add_frame(id3::frame::Lyrics { lang: "eng".into(), description: String::new(), text: value.to_string() });
                }
            }
            "musicbrainz track" => {
                self.tag.frames_vec_mut().retain(|f| !matches!(f.content(), Content::UniqueFileIdentifier(u) if u.owner_identifier == "http://musicbrainz.org"));
                if !value.trim().is_empty() {
                    self.tag.add_frame(id3::frame::UniqueFileIdentifier { owner_identifier: "http://musicbrainz.org".into(), identifier: value.as_bytes().to_vec() });
                }
            }
            f => {
                if let Some(id) = id3_frame(f) {
                    self.put(id, value);
                    match f {
                        "title" => self.v1_put(3, 30, value),
                        "artist" => self.v1_put(33, 30, value),
                        "album" => self.v1_put(63, 30, value),
                        "genre" => self.v1_genre(value),
                        _ => {}
                    }
                } else if let Some(d) = mb_name(f) {
                    self.tag.frames_vec_mut().retain(|fr| !matches!(fr.content(), Content::ExtendedText(x) if x.description.eq_ignore_ascii_case(d)));
                    if !value.trim().is_empty() {
                        self.tag.add_frame(id3::frame::ExtendedText { description: d.into(), value: value.into() });
                    }
                }
            }
        }
    }

    // the old ID3v1 tag: kept up to date where there is one, never added
    fn v1_put(&mut self, at: usize, len: usize, value: &str) {
        if let Some(v1) = self.v1.as_mut() {
            let mut b: Vec<u8> = value.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).take(len).collect();
            b.resize(len, 0);
            v1[at..at + len].copy_from_slice(&b);
        }
    }
    fn v1_track(&mut self, no: u32) {
        if let Some(v1) = self.v1.as_mut() {
            // (ID3v1.1: a zero byte before the track's byte at the comment's end)
            v1[125] = 0;
            v1[126] = no.min(255) as u8;
        }
    }
    fn v1_genre(&mut self, name: &str) {
        if let Some(v1) = self.v1.as_mut() {
            v1[127] = GENRES.iter().position(|g| g.eq_ignore_ascii_case(name.trim())).map(|i| i as u8).unwrap_or(255);
        }
    }
    fn v1_comment(&self) -> Option<String> {
        let v1 = self.v1.as_ref()?;
        let len = if v1[125] == 0 && v1[126] != 0 { 28 } else { 30 };
        let s: String = v1[97..97 + len].iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
        Some(s.trim().to_string())
    }
    fn set_v1_comment(&mut self, value: &str) {
        let len = match self.v1.as_ref() {
            Some(v1) if v1[125] == 0 && v1[126] != 0 => 28,
            Some(_) => 30,
            None => return,
        };
        self.v1_put(97, len, value);
    }

    /// The whole file with this tag in front: the new tag fills the old one's room when it fits (its
    /// padding is kept), else it gets 1 KB of padding. The bytes after the old tag are copied as they are.
    fn render(&self) -> Result<Vec<u8>, String> {
        let body = |pad: usize| {
            let mut out = vec![];
            id3::Encoder::new().version(self.version).padding(pad).encode(&self.tag, &mut out).map(|_| out).map_err(|e| format!("the tag could not be written ({})", e.description))
        };
        let bare = body(0)?;
        let pad = if bare.len() <= self.tag_end { self.tag_end - bare.len() } else { 1024 };
        let tag = if pad == 0 { bare } else { body(pad)? };
        let end = self.bytes.len() - if self.v1.is_some() { 128 } else { 0 };
        let mut out = tag;
        out.extend_from_slice(&self.bytes[self.tag_end..end]);
        if let Some(v1) = &self.v1 {
            out.extend_from_slice(v1);
        }
        Ok(out)
    }
}

/// The ID3v1 genres, by number.
pub(super) const GENRES: [&str; 148] = [
    "Blues", "Classic Rock", "Country", "Dance", "Disco", "Funk", "Grunge", "Hip-Hop", "Jazz", "Metal", "New Age", "Oldies", "Other", "Pop", "R&B", "Rap", "Reggae", "Rock", "Techno", "Industrial", "Alternative", "Ska", "Death Metal", "Pranks", "Soundtrack",
    "Euro-Techno", "Ambient", "Trip-Hop", "Vocal", "Jazz+Funk", "Fusion", "Trance", "Classical", "Instrumental", "Acid", "House", "Game", "Sound Clip", "Gospel", "Noise", "Alternative Rock", "Bass", "Soul", "Punk", "Space", "Meditative", "Instrumental Pop",
    "Instrumental Rock", "Ethnic", "Gothic", "Darkwave", "Techno-Industrial", "Electronic", "Pop-Folk", "Eurodance", "Dream", "Southern Rock", "Comedy", "Cult", "Gangsta", "Top 40", "Christian Rap", "Pop/Funk", "Jungle", "Native American", "Cabaret",
    "New Wave", "Psychedelic", "Rave", "Showtunes", "Trailer", "Lo-Fi", "Tribal", "Acid Punk", "Acid Jazz", "Polka", "Retro", "Musical", "Rock & Roll", "Hard Rock", "Folk", "Folk-Rock", "National Folk", "Swing", "Fast Fusion", "Bebop", "Latin", "Revival",
    "Celtic", "Bluegrass", "Avantgarde", "Gothic Rock", "Progressive Rock", "Psychedelic Rock", "Symphonic Rock", "Slow Rock", "Big Band", "Chorus", "Easy Listening", "Acoustic", "Humour", "Speech", "Chanson", "Opera", "Chamber Music", "Sonata", "Symphony",
    "Booty Bass", "Primus", "Porn Groove", "Satire", "Slow Jam", "Club", "Tango", "Samba", "Folklore", "Ballad", "Power Ballad", "Rhythmic Soul", "Freestyle", "Duet", "Punk Rock", "Drum Solo", "A Cappella", "Euro-House", "Dance Hall", "Goa", "Drum & Bass",
    "Club-House", "Hardcore", "Terror", "Indie", "BritPop", "Negerpunk", "Polsk Punk", "Beat", "Christian Gangsta Rap", "Heavy Metal", "Black Metal", "Crossover", "Contemporary Christian", "Christian Rock", "Merengue", "Salsa", "Thrash Metal", "Anime", "JPop",
    "Synthpop",
];
