//! What the Downloads page knows about the album a search is for, beside the copies Soulseek found:
//! the album itself as Apple Music lists it (its cover, year and how many tracks each edition has), whether
//! the library already holds it and how good that copy is, the edition words in a copy's folder name, and
//! the words to search for when a Spotify, Apple Music or Bandcamp link is pasted in the box.
//!
//! Nothing here asks Soulseek for anything, and nothing here writes to the library.
use crate::net::{days, Net};
use crate::util::{album_key, artist_key, norm};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The album a search is for, as Apple Music lists it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Album {
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub year: String,
    /// how many tracks each edition has, smallest first ("14" for the album, "18" for its deluxe)
    pub editions: Vec<usize>,
    /// a link to a large cover, or empty
    #[serde(default)]
    pub artwork: String,
}

impl Album {
    /// How a copy with this many tracks compares with the album: (the words, whether it is whole).
    /// None when the album's track count is not known.
    pub fn fit(&self, tracks: usize) -> Option<(String, Fit)> {
        let least = *self.editions.first()?;
        Some(if self.editions.contains(&tracks) {
            (format!("{tracks} of {tracks}"), Fit::Whole)
        } else if tracks < least {
            (format!("{tracks} of {least}"), Fit::Short)
        } else {
            // more than one edition, or bonus tracks: whole either way
            (format!("{tracks} of {least}"), Fit::Whole)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fit {
    Whole,
    Short,
}

/// The words of a name, each folded the way the library folds names ("Beyoncé" is "beyonce", "&" is "and").
fn tokens(s: &str) -> Vec<String> {
    s.replace('&', " and ").split(|c: char| !c.is_alphanumeric()).map(norm).filter(|t| !t.is_empty()).collect()
}

/// Picks the album the words mean from Apple Music's answers: the first whose artist and title (without
/// edition notes) are both wholly in the words. Every answer with the same artist and title is an edition.
/// A single or an EP of the album's name ("Fantasy - Single") is not an edition of the album: it is taken
/// only when no full release fits.
pub fn pick(results: &[Value], words: &str) -> Option<Album> {
    let said: Vec<String> = tokens(words);
    let fits = |r: &Value| -> bool {
        let (artist, title) = (r["artistName"].as_str().unwrap_or(""), r["collectionName"].as_str().unwrap_or(""));
        let (a, t) = (tokens(artist), tokens(&crate::importer::util::strip_edition(title)));
        !a.is_empty() && !t.is_empty() && a.iter().chain(t.iter()).all(|w| said.contains(w))
    };
    let short = |r: &Value| {
        let name = r["collectionName"].as_str().unwrap_or("");
        name.ends_with(" - Single") || name.ends_with(" - EP")
    };
    let first = results.iter().find(|r| fits(r) && !short(r)).or_else(|| results.iter().find(|r| fits(r)))?;
    let first_short = short(first);
    let (artist, title) = (first["artistName"].as_str().unwrap_or("").to_string(), first["collectionName"].as_str().unwrap_or("").to_string());
    let same = |r: &Value| artist_key(r["artistName"].as_str().unwrap_or("")) == artist_key(&artist) && album_key(r["collectionName"].as_str().unwrap_or("")) == album_key(&title);
    let mut editions: Vec<usize> = results.iter().filter(|r| fits(r) && same(r) && short(r) == first_short).filter_map(|r| r["trackCount"].as_u64()).map(|n| n as usize).filter(|n| *n > 0).collect();
    editions.sort();
    editions.dedup();
    let artwork = first["artworkUrl100"].as_str().unwrap_or("").replace("100x100bb", "600x600bb");
    let year = first["releaseDate"].as_str().unwrap_or("").chars().take(4).collect();
    Some(Album { artist, title: crate::importer::util::strip_edition(&title), year, editions, artwork })
}

/// Asks Apple Music what album these words mean: Ok(None) when nothing fits them, Err when it did not answer
/// (worth asking again a little later).
pub fn look_up(net: &Net, words: &str) -> Result<Option<Album>, String> {
    let url = format!("https://itunes.apple.com/search?term={}&entity=album&limit=25", crate::importer::lookup::urlenc(words));
    let v = net.get_json("dl-album", &url, days(7), None)?;
    Ok(v["results"].as_array().and_then(|r| pick(r, words)))
}

// ------------------------------------------------------------------ the edition words in a folder name

static MARKS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?i)\bdeluxe\b", "Deluxe"),
        (r"(?i)\bremaster(ed)?\b", "Remaster"),
        (r"(?i)\bexpanded\b", "Expanded"),
        (r"(?i)\banniversary\b", "Anniversary"),
        (r"(?i)\blive\b", "Live"),
        (r"(?i)\binstrumentals?\b", "Instrumental"),
        (r"(?i)\b(vinyl|lp)[\s-]*rip\b|\bvinyl\b", "Vinyl rip"),
        (r"(?i)\bweb\b", "Web"),
        (r"(?i)\b24[\s-]*bit\b|\bhi[\s-]*res\b|\b(88|96|176|192)[\s.,]*(khz|k)\b", "Hi-res"),
    ]
    .into_iter()
    .map(|(r, w)| (Regex::new(r).unwrap(), w))
    .collect()
});
static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[^\d])((?:19[5-9]|20[0-4])\d)(?:[^\d]|$)").unwrap());

/// The edition words a copy's folder name holds ("Deluxe", "Vinyl rip", "Hi-res"), then its year.
pub fn marks(folder: &str) -> Vec<String> {
    let mut out: Vec<String> = MARKS.iter().filter(|(r, _)| r.is_match(folder)).map(|(_, w)| w.to_string()).collect();
    if let Some(y) = YEAR.captures(folder) {
        out.push(y[1].to_string());
    }
    out
}

// ------------------------------------------------------------------ how good a copy is

/// How good a copy's sound is, best last: an order to compare by, and words to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Unknown,
    /// a lossy file at this many kilobits a second (0 when not known)
    Lossy(u32),
    /// variable bitrate, about as good as 256 to 320
    Vbr,
    Lossless,
}

impl Quality {
    /// From the format words of a copy on Soulseek: "FLAC", "MP3 320", "M4A".
    pub fn of_format(format: &str) -> Quality {
        let f = format.to_uppercase();
        let kind = f.split_whitespace().next().unwrap_or("");
        if matches!(kind, "FLAC" | "WAV" | "APE" | "WV" | "ALAC" | "AIFF") {
            return Quality::Lossless;
        }
        match f.split_whitespace().nth(1).and_then(|n| n.parse::<u32>().ok()) {
            Some(k) => Quality::Lossy(k),
            None if kind.is_empty() => Quality::Unknown,
            None => Quality::Lossy(0),
        }
    }

    /// A step to compare by: lossless, then 320 or variable, then 256 and up, then the rest.
    fn rank(self) -> u8 {
        match self {
            Quality::Unknown => 0,
            Quality::Lossy(k) if k >= 300 => 3,
            Quality::Vbr => 3,
            Quality::Lossy(k) if k >= 250 => 2,
            Quality::Lossy(_) => 1,
            Quality::Lossless => 4,
        }
    }

    /// True when this is a clear step up from `mine` (not a few kilobits). Nothing is a step up from a copy
    /// whose bitrate is not known.
    pub fn better_than(self, mine: Quality) -> bool {
        !matches!(mine, Quality::Unknown | Quality::Lossy(0)) && self != Quality::Unknown && self.rank() > mine.rank()
    }

    /// Words for it, or nothing when it is not known.
    pub fn words(self) -> String {
        match self {
            Quality::Lossless => "lossless".into(),
            Quality::Vbr => "MP3 VBR".into(),
            Quality::Lossy(0) => "MP3".into(),
            Quality::Unknown => String::new(),
            Quality::Lossy(k) => format!("MP3 {k}"),
        }
    }
}

/// The library's copy of an album.
#[derive(Clone, Debug, PartialEq)]
pub struct Owned {
    pub folder: PathBuf,
    pub tracks: usize,
    pub quality: Quality,
}

const AUDIO: [&str; 9] = ["mp3", "m4a", "flac", "ogg", "opus", "wav", "aac", "alac", "aiff"];

/// The library's own folder of this album, if it has it: the artist's folder (its name spelled any way the
/// library folds names: "The Beatles" and "Beatles"), then a folder in it whose title, without the year and
/// edition notes, is this one, on the artist's folder itself or one shelf down ("EPs and Singles"). Reads
/// the music folder's list of artists, that artist's folder and the album's.
pub fn owned(music_dir: &Path, artist: &str, title: &str) -> Option<Owned> {
    let (want, who) = (album_key(title), artist_key(artist));
    if want.is_empty() || who.is_empty() {
        return None;
    }
    let dirs = |p: &Path| -> Vec<PathBuf> { std::fs::read_dir(p).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect() };
    let leaf = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let artist_dir = dirs(music_dir).into_iter().find(|d| artist_key(&leaf(d)) == who)?;
    let is_it = |d: &Path| album_key(&crate::importer::util::strip_year(&leaf(d))) == want;
    let top = dirs(&artist_dir);
    let folder = top.iter().find(|d| is_it(d)).cloned().or_else(|| top.iter().flat_map(|d| dirs(d)).find(|d| is_it(d)))?;
    let files = audio_files(&folder);
    if files.is_empty() {
        return None;
    }
    Some(Owned { tracks: files.len(), quality: quality_of(&files), folder })
}

/// The audio files of an album folder and of its disc folders.
fn audio_files(folder: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            if crate::library::is_disc_like(&e.file_name().to_string_lossy()) {
                out.extend(audio_files(&p));
            }
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| AUDIO.contains(&x.to_lowercase().as_str())) {
            out.push(p);
        }
    }
    out
}

/// How good a folder's files are: lossless by their kind, an MP3 by the first file's own header. Anything
/// else (M4A, which may be AAC or ALAC; OGG; Opus) is not judged.
fn quality_of(files: &[PathBuf]) -> Quality {
    let ext = |p: &PathBuf| p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    if files.iter().all(|p| matches!(ext(p).as_str(), "flac" | "wav" | "alac" | "aiff")) {
        return Quality::Lossless;
    }
    if !files.iter().all(|p| ext(p) == "mp3") {
        return Quality::Unknown;
    }
    match files.first().and_then(|p| mp3_start(p)).map(|b| mp3_bitrate(&b)) {
        Some(Some((_, true))) => Quality::Vbr,
        Some(Some((k, false))) => Quality::Lossy(k),
        _ => Quality::Lossy(0),
    }
}

/// The first few kilobytes of an MP3's sound, read past its ID3 tag (which may hold a cover of a megabyte or
/// more) without reading the rest of the file.
fn mp3_start(path: &Path) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![];
    (&mut f).take(10).read_to_end(&mut head).ok()?;
    let skip = if head.len() == 10 && &head[..3] == b"ID3" {
        let size = ((head[6] as u64 & 0x7f) << 21) | ((head[7] as u64 & 0x7f) << 14) | ((head[8] as u64 & 0x7f) << 7) | (head[9] as u64 & 0x7f);
        10 + size + if head[5] & 0x10 != 0 { 10 } else { 0 }
    } else {
        0
    };
    f.seek(SeekFrom::Start(skip)).ok()?;
    let mut buf = vec![];
    f.take(16 * 1024).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// An MP3's bitrate from the start of the file: the first frame's header, after any ID3 tag. (kbps, whether
/// it is variable: a Xing or VBRI header in that frame).
pub fn mp3_bitrate(b: &[u8]) -> Option<(u32, bool)> {
    let mut at = 0usize;
    if b.len() >= 10 && &b[..3] == b"ID3" {
        let size = ((b[6] as usize & 0x7f) << 21) | ((b[7] as usize & 0x7f) << 14) | ((b[8] as usize & 0x7f) << 7) | (b[9] as usize & 0x7f);
        at = 10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 };
    }
    // MPEG-1 layer III bitrates, then MPEG-2 and 2.5 layer III
    const V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    while at + 4 <= b.len() {
        if b[at] == 0xff && b[at + 1] & 0xe0 == 0xe0 && (b[at + 1] >> 1) & 3 == 1 {
            let version = (b[at + 1] >> 3) & 3; // 3 = MPEG-1
            let index = (b[at + 2] >> 4) as usize;
            if index > 0 && index < 15 && (b[at + 2] >> 2) & 3 != 3 {
                let kbps = if version == 3 { V1[index] } else { V2[index] };
                let frame = &b[at..b.len().min(at + 200)];
                let vbr = frame.windows(4).any(|w| w == b"Xing" || w == b"VBRI");
                return Some((kbps, vbr));
            }
        }
        at += 1;
    }
    None
}

// ------------------------------------------------------------------ a pasted link

/// Which shop a pasted link is from, if it is one the box can read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shop {
    Spotify,
    Apple,
    Bandcamp,
}

/// Whether the box holds a link to an album on Spotify, Apple Music or Bandcamp. (A song's page names the
/// song, not its album, so a song link is not read.)
pub fn shop_of(text: &str) -> Option<Shop> {
    let t = text.trim().to_lowercase();
    if t.contains(char::is_whitespace) || !(t.starts_with("http://") || t.starts_with("https://")) {
        return None;
    }
    let host = t.split("://").nth(1)?.split('/').next()?;
    // (an Apple Music link to one song of an album reads ".../album/name/123?i=456": that is still the album)
    if host == "open.spotify.com" && t.contains("/album/") {
        Some(Shop::Spotify)
    } else if host == "music.apple.com" && t.contains("/album/") {
        Some(Shop::Apple)
    } else if host.ends_with(".bandcamp.com") && t.contains("/album/") {
        Some(Shop::Bandcamp)
    } else {
        None
    }
}

static OG_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<meta[^>]+property="og:title"[^>]+content="([^"]*)""#).unwrap());
static OG_DESC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<meta[^>]+property="og:description"[^>]+content="([^"]*)""#).unwrap());
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());

/// A page title's text: its HTML escapes undone, and the invisible direction marks Apple puts in front taken out.
fn unescape(s: &str) -> String {
    let s = s.replace("&amp;", "&").replace("&#x27;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&#x200E;", "").replace("&lrm;", "");
    s.chars().filter(|c| !matches!(c, '\u{200e}' | '\u{200f}' | '\u{feff}' | '\u{200b}')).collect::<String>().trim().to_string()
}

/// The words to search for, read from a shop's page: "Artist Album".
pub fn words_from_page(shop: Shop, html: &str) -> Option<String> {
    let og = |r: &Regex| r.captures(html).map(|c| unescape(&c[1]));
    let (artist, album) = match shop {
        // og:title is the album; og:description reads "Chance the Rapper · Mixtape · 2012 · 14 songs."
        Shop::Spotify => (og(&OG_DESC)?.split(" · ").next()?.trim().to_string(), og(&OG_TITLE)?),
        // og:title reads "10 Day, by Chance the Rapper"
        Shop::Bandcamp => {
            let t = og(&OG_TITLE)?;
            let (album, artist) = t.rsplit_once(", by ")?;
            (artist.to_string(), album.to_string())
        }
        // the page's title reads "Acid Rap - Album by Chance the Rapper - Apple Music" (older pages: "10 Day by
        // Chance the Rapper on Apple Music")
        Shop::Apple => {
            let t = og(&OG_TITLE).or_else(|| TITLE.captures(html).map(|c| unescape(&c[1])))?;
            let t = t.trim_end_matches(" on Apple Music").trim_end_matches(" - Apple Music");
            let (album, artist) = t.rsplit_once(" by ")?;
            let album = [" - Album", " - Single", " - EP", " - Mixtape"].iter().fold(album.to_string(), |a, end| a.trim_end_matches(end).to_string());
            (artist.to_string(), album)
        }
    };
    let words = format!("{} {}", artist.trim(), crate::importer::util::strip_edition(&album));
    (!artist.trim().is_empty() && !album.trim().is_empty()).then(|| words.trim().to_string())
}

/// Reads a pasted link and gives the words to search for, or why it could not.
pub fn words_from_link(net: &Net, link: &str) -> Result<String, String> {
    let shop = shop_of(link).ok_or("That link is not to an album on Spotify, Apple Music or Bandcamp.")?;
    let page = net.get_bytes("dl-link", link.trim(), days(30), None).map_err(|_| "The link's page did not open. Type the artist and album instead.".to_string())?;
    words_from_page(shop, &String::from_utf8_lossy(&page)).ok_or_else(|| "The album's name could not be read from that page. Type the artist and album instead.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer(artist: &str, album: &str, tracks: u64) -> Value {
        json!({"artistName": artist, "collectionName": album, "trackCount": tracks, "releaseDate": "2012-04-03T07:00:00Z", "artworkUrl100": "https://x/100x100bb.jpg"})
    }

    #[test]
    fn the_album_the_words_mean_and_its_editions() {
        let results = vec![answer("Someone Else", "10 Day Remixes", 9), answer("Chance the Rapper", "10 Day (Deluxe)", 18), answer("Chance the Rapper", "10 Day", 14), answer("Chance the Rapper", "Acid Rap", 13)];
        let a = pick(&results, "chance the rapper 10 day").unwrap();
        assert_eq!((a.artist.as_str(), a.title.as_str(), a.year.as_str(), a.editions.clone()), ("Chance the Rapper", "10 Day", "2012", vec![14, 18]));
        assert_eq!(a.artwork, "https://x/600x600bb.jpg");
        // words that name no answer wholly find nothing
        assert!(pick(&results, "chance 10 day").is_none());
        assert!(pick(&[], "anything").is_none());
        // a single of the album's name, listed first, is neither the album nor one of its editions
        let with_single = vec![answer("M83", "Fantasy - Single", 1), answer("M83", "Fantasy", 13)];
        let a = pick(&with_single, "M83 Fantasy").unwrap();
        assert_eq!(a.editions, vec![13]);
        // and when only the single fits, it is what is shown
        assert_eq!(pick(&with_single[..1], "M83 Fantasy").unwrap().editions, vec![1]);
        // "&" in the words and "and" in Apple's name are the same word
        assert!(pick(&[answer("M83", "Dead Cities, Red Seas and Lost Ghosts", 13)], "M83 Dead Cities Red Seas & Lost Ghosts").is_some());
    }

    #[test]
    fn a_copy_is_whole_short_or_has_extra() {
        let a = Album { artist: "A".into(), title: "B".into(), year: String::new(), editions: vec![14, 18], artwork: String::new() };
        assert_eq!(a.fit(14), Some(("14 of 14".to_string(), Fit::Whole)));
        assert_eq!(a.fit(18), Some(("18 of 18".to_string(), Fit::Whole)));
        assert_eq!(a.fit(11), Some(("11 of 14".to_string(), Fit::Short)));
        assert_eq!(a.fit(15).unwrap().1, Fit::Whole);
        assert_eq!(Album { editions: vec![], ..a }.fit(3), None);
    }

    #[test]
    fn edition_words_in_folder_names() {
        assert_eq!(marks("Chance The Rapper - 10 Day (2012) [Deluxe Edition] [24-bit WEB]"), vec!["Deluxe", "Web", "Hi-res", "2012"]);
        assert_eq!(marks("Reflektor (2013) [FLAC] Vinyl Rip"), vec!["Vinyl rip", "2013"]);
        assert_eq!(marks("Abbey Road (2019 Remaster)"), vec!["Remaster", "2019"]);
        assert!(marks("Delivery Man 320").is_empty()); // ("deliver" is not "deluxe", 320 is not a year)
        assert!(marks("Olive Tree").is_empty()); // ("olive" is not "live")
    }

    #[test]
    fn quality_from_format_words_and_what_counts_as_better() {
        assert_eq!(Quality::of_format("FLAC"), Quality::Lossless);
        assert_eq!(Quality::of_format("MP3 320"), Quality::Lossy(320));
        assert_eq!(Quality::of_format("M4A"), Quality::Lossy(0));
        assert!(Quality::Lossless.better_than(Quality::Lossy(320)));
        assert!(Quality::Lossy(320).better_than(Quality::Lossy(192)));
        assert!(!Quality::Lossy(320).better_than(Quality::Vbr));
        assert!(!Quality::Lossy(320).better_than(Quality::Lossy(310)));
        assert!(!Quality::Lossless.better_than(Quality::Unknown));
    }

    #[test]
    fn an_mp3_header_gives_its_bitrate() {
        // an ID3 tag of 20 bytes, then an MPEG-1 layer III frame at 320 (index 14)
        let mut b = b"ID3\x03\x00\x00\x00\x00\x00\x14".to_vec();
        b.extend([0u8; 20]);
        b.extend([0xff, 0xfb, 0xe0, 0x64]);
        b.extend([0u8; 60]);
        assert_eq!(mp3_bitrate(&b), Some((320, false)));
        let mut v = b.clone();
        v.extend(b"Xing");
        assert_eq!(mp3_bitrate(&v), Some((320, true)));
        assert_eq!(mp3_bitrate(b"not an mp3 at all"), None);
    }

    #[test]
    fn the_library_copy_is_found_by_folder_names_and_judged_by_its_files() {
        let lib = std::env::temp_dir().join(format!("dl-facts-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        let mp3 = [0xff, 0xfb, 0xb0, 0x64, 0, 0, 0, 0]; // MPEG-1 layer III at 192
        let album = lib.join("Chance the Rapper").join("10 Day (2012)");
        std::fs::create_dir_all(&album).unwrap();
        for n in 1..=9 {
            std::fs::write(album.join(format!("{n:02} Song.mp3")), mp3).unwrap();
        }
        std::fs::write(album.join("cover.jpg"), b"x").unwrap();
        let shelf = lib.join("Beatles").join("EPs and Singles").join("Twist and Shout (1963)");
        std::fs::create_dir_all(shelf.join("CD1")).unwrap();
        std::fs::write(shelf.join("CD1").join("01 Twist.flac"), b"x").unwrap();
        // found however the artist is spelled, with the year and edition notes left aside
        let o = owned(&lib, "Chance The Rapper", "10 Day (Deluxe)").unwrap();
        assert_eq!((o.tracks, o.quality, o.folder), (9, Quality::Lossy(192), album));
        // one shelf down, with its tracks in a disc folder
        let o = owned(&lib, "The Beatles", "Twist and Shout").unwrap();
        assert_eq!((o.tracks, o.quality), (1, Quality::Lossless));
        assert!(owned(&lib, "Chance the Rapper", "Acid Rap").is_none());
        assert!(owned(&lib, "Nobody", "10 Day").is_none());
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn links_from_the_three_shops_give_search_words() {
        assert_eq!(shop_of("https://open.spotify.com/album/5xyz?si=1"), Some(Shop::Spotify));
        assert_eq!(shop_of("https://music.apple.com/us/album/10-day/1234"), Some(Shop::Apple));
        assert_eq!(shop_of("https://chancetherapper.bandcamp.com/album/10-day"), Some(Shop::Bandcamp));
        assert_eq!(shop_of("chance the rapper 10 day"), None);
        assert_eq!(shop_of("https://example.com/album/x"), None);
        let spotify = r#"<meta property="og:title" content="10 Day"/><meta property="og:description" content="Chance the Rapper · Mixtape · 2012 · 14 songs."/>"#;
        assert_eq!(words_from_page(Shop::Spotify, spotify).as_deref(), Some("Chance the Rapper 10 Day"));
        let bandcamp = r#"<meta property="og:title" content="10 Day, by Chance the Rapper">"#;
        assert_eq!(words_from_page(Shop::Bandcamp, bandcamp).as_deref(), Some("Chance the Rapper 10 Day"));
        let apple = r#"<meta property="og:title" content="10 Day (Deluxe) by Chance the Rapper on Apple Music">"#;
        assert_eq!(words_from_page(Shop::Apple, apple).as_deref(), Some("Chance the Rapper 10 Day"));
        let apple_old = "<title>Acid Rap - Single by Chance the Rapper on Apple Music</title>";
        assert_eq!(words_from_page(Shop::Apple, apple_old).as_deref(), Some("Chance the Rapper Acid Rap"));
        // today's shape, with the invisible mark in front, as a character and as an escape
        let apple_now = "<title>\u{200e}Acid Rap - Album by Chance the Rapper - Apple Music</title>";
        assert_eq!(words_from_page(Shop::Apple, apple_now).as_deref(), Some("Chance the Rapper Acid Rap"));
        let apple_escaped = r#"<meta property="og:title" content="&#x200E;Red Seas &amp; Lost Ghosts - Album by M83 - Apple Music">"#;
        assert_eq!(words_from_page(Shop::Apple, apple_escaped).as_deref(), Some("M83 Red Seas & Lost Ghosts"));
        // song links name a song, not its album
        assert_eq!(shop_of("https://open.spotify.com/track/5xyz"), None);
        assert_eq!(shop_of("https://music.apple.com/us/song/juice/1234"), None);
        assert_eq!(shop_of("https://music.apple.com/us/album/10-day/1234?i=5678"), Some(Shop::Apple));
        assert_eq!(words_from_page(Shop::Bandcamp, "<html></html>"), None);
    }
}
