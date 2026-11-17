//! Library Health's tag rules, shared so new downloads are judged the same way as the library:
//!   * junk in tags (links, uploader signatures, download-site tags) - port of LibraryScan.cs JunkTagRules
//!   * scrambled text saved in the wrong encoding - port of LibraryRepair.ps1 Test-Garbled / Repair-Text
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, RwLock};

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

// ---------- junk in tags ----------
// fields where junk ends up; for the first four, an odd value repeated across many artists is an uploader's signature
const JUNK_FIELDS: [&str; 18] = [
    "comment", "comments", "description", "encoded_by", "copyright", "publisher", "url", "www", "purl", "website", "uploader", "uploaded_by", "ripper",
    "ripped_by", "tagged_by", "tagger", "release group", "releaser",
];
const SIGNATURE_FIELDS: [&str; 4] = ["comment", "comments", "description", "encoded_by"];
const ALWAYS_JUNK: [&str; 8] = ["uploader", "uploaded_by", "ripper", "ripped_by", "tagged_by", "tagger", "release group", "releaser"];

re!(LINK_RX, r"(?i)https?://|www\.|\b[a-z0-9-]{2,}\.(com|net|org|ru|to|info|biz|ws|cc|me|io|co|eu|xyz|club|site|tk|in|us|fm|pw|top)\b");
re!(
    SCAM_RX,
    r"(?i)torrent|ripped by|\brip(ped)? (by|from)\b|upload(ed)? by|downloaded (from|at|by)|free download|\bpmedia\b|\bwarez\b|\bscene\b|\[vx\]|\bvx\b|kingdom-?leaks|hyperleaks|newalbumreleases|zippyshare|mediafire|datpiff|livemixtapes|mp3va|\bnar\b|\bglt\b|\bmtd\b|\bpsv\b"
);
// Historical encoder names remain recognized so existing tags are not mistaken for uploader junk.
re!(
    SOFTWARE_RX,
    r"(?i)exact audio copy|\beac\b|itunes|dbpoweramp|foobar|lame|winamp|media ?monkey|cadence|musicbee|mp3tag|cdex|\bxld\b|quicktime|windows media|nero|audiograbber|fre:ac|easy cd-da|ffmpeg|lavf|lavc|apple|amazon|spotify|tidal|qobuz|beatport|bandcamp|google|picard|musicbrainz"
);
// real credits keep their comment even with a link in it ("music by X, visual art by Y, www.instagram.com/y")
re!(
    CREDIT_RX,
    r"(?i)\b(music|vocals?|lyrics|words|written|produced|production|mixed|mastered|mastering|recorded|engineered|artwork|art|cover|photos?|photography|designed?|layout|performed|composed|arranged|guitars?|bass|drums|keys|synths?)\s+(by|at)\b|\bthanks to\b"
);
re!(PLACEHOLDER_RX, r"^[\s0\-\.\*_#]{1,3}$");
re!(HEX_RX, r"^[0-9A-F ]{20,}$");
re!(WS_RX, r"\s+");
// tag fields that are normal and may legitimately hold links or odd text
re!(
    GOOD_KEY_RX,
    r"(?i)^(title|artist|album|album_artist|albumartist|date|year|genre|track|disc|composer|performer|conductor|lyricist|lyrics.*|unsyncedlyrics|musicbrainz.*|acoustid.*|replaygain.*|itun.*|encoder|tsrc|isrc|barcode|label|catalognumber|media|script|language|releasecountry|releasetype|releasestatus|originaldate|originalyear|release_date|artists|.*sort|bpm|tbpm|key|initialkey|mood|grouping|compilation|tlen|tmed|tdtg|tdrl|discogs.*|asin|work|movement.*|remixer|producer|engineer|mixer|arranger|writer|djmixer|totaltracks|totaldiscs|tracktotal|disctotal|rating.*|fmps.*|serato.*|traktor.*|ftraktor.*|fbpm.*|vdj.*|cue.*|beatgrid|id3v2_priv.*|wm/.*|sourcedata|purchase.*|major_brand|minor_version|compatible_brands|creation_time|handler_name|vendor_id|iTunes.*|gapless_playback|encoding_params)$"
);

/// "field|value" -> how many artist folders it appears in (learned by Library Health's scans, saved to junk-signatures.txt)
static SIGNATURES: LazyLock<RwLock<HashMap<String, usize>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn load_signatures(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut m = SIGNATURES.write().unwrap();
    for l in text.lines() {
        if let Some((n, k)) = l.split_once('\t') {
            if let Ok(n) = n.trim().parse() {
                m.insert(k.to_string(), n);
            }
        }
    }
}

fn junk_key(field: &str, value: &str) -> String {
    format!("{}|{}", if field == "comments" { "comment" } else { field }, WS_RX.replace_all(&value.to_lowercase(), " ").trim())
}

/// why a value in one of the usual junk fields is junk, or None if it looks fine
fn junk_reason(field: &str, v: &str, spread: Option<&HashMap<String, usize>>) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || HEX_RX.is_match(v) {
        return None; // (iTunes normalisation data, not text)
    }
    if ALWAYS_JUNK.contains(&field) {
        return Some("uploader tag".into());
    }
    if SCAM_RX.is_match(v) {
        return Some("release-group or download-site tag".into());
    }
    if LINK_RX.is_match(v) {
        return if (SOFTWARE_RX.is_match(v) && field == "encoded_by") || CREDIT_RX.is_match(v) { None } else { Some("web link".into()) };
    }
    if SIGNATURE_FIELDS.contains(&field) {
        if PLACEHOLDER_RX.is_match(v) {
            return Some("placeholder".into());
        }
        let k = junk_key(field, v);
        let n = match spread {
            Some(s) => s.get(&k).copied().unwrap_or(0),
            None => SIGNATURES.read().unwrap().get(&k).copied().unwrap_or(0),
        };
        if n >= 3 && !SOFTWARE_RX.is_match(v) && v.chars().count() <= 40 {
            return Some(format!("same text on {n} different artists' tracks (an uploader's signature)"));
        }
    }
    None
}

fn custom_junk_reason(key: &str, value: &str) -> Option<String> {
    if LINK_RX.is_match(key) {
        return Some("a field named after a website".into());
    }
    if LINK_RX.is_match(value) {
        return Some("web link".into());
    }
    if SCAM_RX.is_match(value) || SCAM_RX.is_match(key) {
        return Some("release-group or download-site tag".into());
    }
    None
}

/// The junk in one file's tags (keys as ffprobe names them, lower case): (field, value, reason).
pub fn find_junk(tags: &HashMap<String, String>) -> Vec<(String, String, String)> {
    find_junk_in(tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), None)
}

/// The key an uploader's signature is counted under: the field and its text, lower case, spaces evened out.
pub fn signature_key(field: &str, value: &str) -> Option<String> {
    SIGNATURE_FIELDS.contains(&field).then(|| junk_key(field, value))
}

/// `find_junk` for any set of tags, judged against the given signature counts (None = the ones loaded).
pub fn find_junk_in<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>, spread: Option<&HashMap<String, usize>>) -> Vec<(String, String, String)> {
    let tags: HashMap<&str, &str> = tags.collect();
    let mut out = vec![];
    let mut keys: Vec<&&str> = tags.keys().collect();
    keys.sort();
    for k in keys {
        let f = k.to_lowercase();
        let v = tags[k].trim();
        if v.is_empty() {
            continue;
        }
        let why = if JUNK_FIELDS.contains(&f.as_str()) {
            junk_reason(&f, v, spread)
        } else if GOOD_KEY_RX.is_match(&f) {
            None
        } else {
            custom_junk_reason(&f, v)
        };
        if let Some(w) = why {
            out.push((k.to_string(), v.to_string(), w));
        }
    }
    out
}

// ---------- scrambled text ----------
//  UTF-8 read as Latin-1/Windows-1252: "CafÃ©", "Donâ€™t";  Russian read as Latin-1: "Òóò òîëüêî";  lost characters: "Bj??rk"
re!(UTF8_RX, r"Ã[\u0080-¿]|â€|Â[ -¿]");
re!(CP1251_RX, r"[À-ÿ]{4,}");

fn lost_chars(v: &str) -> bool {
    // "?" between two letters ("Bj??rk", not "Here???") or the replacement character
    if v.contains('\u{FFFD}') {
        return true;
    }
    let c: Vec<char> = v.chars().collect();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '?' {
            let start = i;
            while i < c.len() && c[i] == '?' {
                i += 1;
            }
            if start > 0 && c[start - 1].is_alphabetic() && i < c.len() && c[i].is_alphabetic() {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

fn mostly_accented(v: &str) -> bool {
    let n = v.chars().count();
    CP1251_RX.is_match(v) && v.chars().filter(|&c| ('À'..='ÿ').contains(&c)).count() * 2 > n
}

pub fn test_garbled(v: &str) -> bool {
    UTF8_RX.is_match(v) || lost_chars(v) || mostly_accented(v)
}

/// Windows-1252's characters in the 0x80-0x9F range (where it differs from Latin-1)
const CP1252_HIGH: [(char, u8); 27] = [
    ('€', 0x80), ('‚', 0x82), ('ƒ', 0x83), ('„', 0x84), ('…', 0x85), ('†', 0x86), ('‡', 0x87), ('ˆ', 0x88), ('‰', 0x89), ('Š', 0x8A), ('‹', 0x8B), ('Œ', 0x8C),
    ('Ž', 0x8E), ('‘', 0x91), ('’', 0x92), ('“', 0x93), ('”', 0x94), ('•', 0x95), ('–', 0x96), ('—', 0x97), ('˜', 0x98), ('™', 0x99), ('š', 0x9A), ('›', 0x9B),
    ('œ', 0x9C), ('ž', 0x9E), ('Ÿ', 0x9F),
];
/// the bytes this text was before it was wrongly read as Windows-1252 / Latin-1 (None if it can't have been)
fn to_single_bytes(v: &str) -> Option<Vec<u8>> {
    v.chars()
        .map(|c| {
            let u = c as u32;
            if u <= 0xFF { Some(u as u8) } else { CP1252_HIGH.iter().find(|x| x.0 == c).map(|x| x.1) }
        })
        .collect()
}
/// Windows-1251 (Cyrillic) bytes 0x80-0xBF; 0xC0-0xFF are А..я in order
const CP1251_80: [u16; 64] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D,
    0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F, 0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9,
    0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407, 0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455,
    0x0457,
];
fn cp1251(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0..=0x7F => b as char,
            0x80..=0xBF => char::from_u32(CP1251_80[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}'),
            _ => char::from_u32(0x0410 + (b - 0xC0) as u32).unwrap(),
        })
        .collect()
}

/// The repaired text, or None when it can't be undone (e.g. "Bj??rk": the letters are gone).
pub fn repair_text(v: &str) -> Option<String> {
    if UTF8_RX.is_match(v) {
        if let Some(b) = to_single_bytes(v) {
            if let Ok(out) = String::from_utf8(b) {
                if out != v && !test_garbled(&out) {
                    return Some(out);
                }
            }
        }
    }
    if CP1251_RX.is_match(v) {
        if let Some(b) = to_single_bytes(v) {
            let out = cp1251(&b);
            let cyr = out.chars().filter(|&c| ('\u{0400}'..='\u{04FF}').contains(&c)).count();
            if cyr * 3 > out.chars().count() {
                return Some(out);
            }
        }
    }
    None
}

// ---------- junk in lyrics ----------
// The tag rules above leave the lyrics field alone on purpose: a lyric may name a website. Lyrics get a
// check of their own, line by line, that only calls a line junk when it cannot be a lyric: a web address
// with next to nothing around it, a site's or an uploader's credit, markup, or a placeholder where the
// words should be. Cadence (the player) hides the same lines when it shows lyrics.
re!(LY_ADDRESS_RX, r"(?i)(https?://\S+|www\.\S+|\b[\w-]+(\.[\w-]+)*\.(com|net|org|ru|me|io|info|biz|xyz|club|site|top|cc|to|tv|fm|us|uk|de)(/\S*)?\b|(^|[^\w])@[\w.]{3,})");
// said by sites and uploaders, never sung
re!(LY_CREDIT_RX, r"(?i)\blyrics (provided|powered|licensed|transcribed|submitted) by\b|\blyrics (from|by|at|via) \S+\.(com|net|org)\b|\bfree download\b|\ball rights reserved\b");
// an uploader's credit when the line is little else ("Ripped by XYZ"); in a longer line the same words can be sung
re!(LY_CREDIT_SHORT_RX, r"(?i)\b(downloaded|uploaded|tagged) (from|by|at)\b|\bripped by\b|\b(sync(ed|hronized)?|subtitles?|timed) by\b");
// words that make a line with an address in it an advert, however long it is
re!(LY_ADVERT_RX, r"(?i)\b(visit|join|follow|subscribe|download|telegram|channel|for more|check out|click|available (at|on))\b");
re!(LY_MARKUP_RX, r"(?i)</?(br|div|p|span|a|font|b|i|u|em|strong|script|html|body|table|tr|td|center|img)\b[^>]*>|&(nbsp|amp|quot|lt|gt|apos|#\d{2,5}|#x[0-9a-f]{2,4});");
re!(LY_TAG_RX, r"(?i)<[^>]{1,200}>");
re!(LY_STAMP_RX, r"\[\d{1,3}:\d{1,2}(?:[.:]\d{1,3})?\]|<\d{1,3}:\d{1,2}(?:[.:]\d{1,3})?>");
re!(LY_HEADER_RX, r"^\[[a-zA-Z]+:.*\]$");
re!(LY_PLACEHOLDER_RX, r"(?i)^[\s\[\(\*\-]*(no lyrics( (available|found|yet))?|lyrics (not|are not|aren't) (available|found)( yet)?|lyrics unavailable|n/?a|none|null|undefined|unknown|lyrics|add lyrics|todo|tbd|we (do not|don't) have (this|these|the) lyrics.*|[\s\-\.\*_#?0]{1,5})[\s\]\)\*\-\.!]*$");
re!(LY_INSTRUMENTAL_RX, r"(?i)^[\s\[\(\*\-]*(instrumental|this (song|track) is (an )?instrumental|no vocals)[\s\]\)\*\-\.!]*$");

/// What the lyrics check found in one song's lyrics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LyricJunk {
    /// the lines that are not lyrics, as they stand in the file (without time stamps): (line, why)
    pub lines: Vec<(String, &'static str)>,
    /// nothing that could be a lyric is left beside them
    pub only: bool,
    /// the words say the song has none ("Instrumental"): not junk, and not missing lyrics either
    pub instrumental: bool,
    /// the lyrics without those lines (markup taken out of a line, scrambled text put right), ready to save;
    /// empty when nothing is left
    pub cleaned: String,
}

/// A line's words without the time stamps a timed lyric carries.
fn unstamped(line: &str) -> String {
    LY_STAMP_RX.replace_all(line, "").trim().to_string()
}

/// True when the text carries a time on its lines ("[01:02.50]words").
pub fn lyrics_timed(text: &str) -> bool {
    text.lines().filter(|l| LY_STAMP_RX.is_match(l)).count() >= 2 || (text.lines().count() == 1 && LY_STAMP_RX.is_match(text))
}

/// Why one line of lyrics is no lyric, or None.
fn lyric_line_junk(line: &str) -> Option<&'static str> {
    let text = unstamped(line);
    if text.is_empty() || LY_HEADER_RX.is_match(&text) {
        return None; // blank, or one of a timed lyric's own headers ("[ar:Artist]")
    }
    let has_address = LY_ADDRESS_RX.is_match(&text);
    if LY_CREDIT_RX.is_match(&text) || (LY_CREDIT_SHORT_RX.is_match(&text) && (has_address || text.split_whitespace().count() <= 5)) {
        return Some("site credit");
    }
    if has_address {
        // a lyric can name a website ("meet me at dot com"); an address with next to no words around it,
        // or with an advert's words beside it, is nobody's lyric
        let rest = LY_ADDRESS_RX.replace_all(&text, " ");
        let words = rest.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphabetic())).count();
        if words <= 3 || LY_ADVERT_RX.is_match(&rest) {
            return Some("web address");
        }
    }
    None
}

/// The lyrics check. `text` is the whole lyrics field, timed or plain.
pub fn lyrics_junk(text: &str) -> LyricJunk {
    let mut out = LyricJunk::default();
    let body = text.replace("\r\n", "\n").replace('\r', "\n");
    let whole = unstamped(&body.lines().map(unstamped).filter(|l| !l.is_empty() && !LY_HEADER_RX.is_match(l)).collect::<Vec<_>>().join(" "));
    if whole.is_empty() {
        return out;
    }
    if LY_INSTRUMENTAL_RX.is_match(&whole) {
        out.instrumental = true;
        out.cleaned = body.trim().to_string();
        return out;
    }
    if LY_PLACEHOLDER_RX.is_match(&whole) {
        out.lines.push((short(&whole, 120), "placeholder"));
        out.only = true;
        return out;
    }
    // scrambled text counts when it is more than a stray character: two lines, or most of a short lyric
    let garbled = body.lines().filter(|l| test_garbled(l)).count();
    let scrambled = garbled >= 2;
    let mut kept: Vec<String> = vec![];
    for line in body.lines() {
        let shown = short(&unstamped(line), 120);
        if let Some(why) = lyric_line_junk(line) {
            out.lines.push((shown, why));
            continue;
        }
        let mut line = line.to_string();
        if LY_MARKUP_RX.is_match(&line) {
            out.lines.push((shown.clone(), "markup"));
            // the words stay; what is cut is the markup around them
            line = LY_TAG_RX.replace_all(&line, " ").to_string();
            for (code, ch) in [("&nbsp;", " "), ("&amp;", "&"), ("&quot;", "\""), ("&lt;", "<"), ("&gt;", ">"), ("&apos;", "'"), ("&#39;", "'"), ("&#039;", "'")] {
                line = line.replace(code, ch);
            }
            line = WS_RX.replace_all(line.trim(), " ").to_string();
            if unstamped(&line).is_empty() {
                continue;
            }
        }
        if scrambled && test_garbled(&line) {
            if !out.lines.iter().any(|l| l.1 == "scrambled characters") {
                out.lines.push((shown, "scrambled characters"));
            }
            if let Some(fixed) = repair_text(&line) {
                line = fixed;
            }
        }
        kept.push(line.trim_end().to_string());
    }
    // no blank lines at the start or end, never two in a row
    let mut tidy: Vec<String> = vec![];
    for l in kept {
        if l.trim().is_empty() && tidy.last().map(|p: &String| p.trim().is_empty()).unwrap_or(true) {
            continue;
        }
        tidy.push(l);
    }
    while tidy.last().is_some_and(|l| l.trim().is_empty()) {
        tidy.pop();
    }
    let real = tidy.iter().filter(|l| unstamped(l).chars().any(|c| c.is_alphabetic()) && !LY_HEADER_RX.is_match(&unstamped(l))).count();
    if !out.lines.is_empty() && real == 0 {
        out.only = true;
    } else {
        out.cleaned = tidy.join("\n");
    }
    out
}

pub fn short(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}...", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn garbled() {
        assert_eq!(repair_text("CafÃ©").as_deref(), Some("Café"));
        assert_eq!(repair_text("Donâ€™t").as_deref(), Some("Don’t"));
        assert_eq!(repair_text("Òóò òîëüêî").as_deref(), Some("Тут только"));
        assert!(test_garbled("Bj??rk") && repair_text("Bj??rk").is_none());
        assert!(!test_garbled("Here???") && !test_garbled("ŁÖVĚRs QUÄRRĚŁ") && !test_garbled("Beyoncé") && !test_garbled("너 땜에"));
    }
    #[test]
    fn junk() {
        let t = |k: &str, v: &str| find_junk(&HashMap::from([(k.to_string(), v.to_string())]));
        assert_eq!(t("comment", "Visit https://x.bandcamp.com").len(), 1);
        assert_eq!(t("comment", "music by Keith Rankin\nvisual art by midi wizard\nwww.instagram.com/midi_wizard/").len(), 0);
        assert_eq!(t("comment", "ripped by XYZ, art by me www.x.com").len(), 1);
        assert_eq!(t("encoded_by", "LAME 3.100").len(), 0);
        for player in ["Cadence", "MusicBee"] {
            let value = format!("{player} https://example.com");
            assert_eq!(t("encoded_by", &value).len(), 0);
            let spread = HashMap::from([(junk_key("comment", player), 5)]);
            assert!(junk_reason("comment", player, Some(&spread)).is_none());
        }
        assert_eq!(t("download", "freemusic.ru").len(), 1);
        assert_eq!(t("freemusic.ru", "x").len(), 1);
        assert_eq!(t("lyrics", "see www.x.com").len(), 0);
    }
    #[test]
    fn lyrics_are_checked_line_by_line_and_real_lines_are_never_junk() {
        let words = "The lamps along the water\nburn a little lower now\nand every boat that leaves the harbor\ntakes a piece of you somehow";
        // nothing wrong
        let j = lyrics_junk(words);
        assert!(j.lines.is_empty() && !j.only && j.cleaned == words);
        // only a web address (what download sites write into the field)
        for only in ["www.t.me/pmedia_music", "https://vk.com/music", "Visit vk.com/music", "@pmedia_music", "[00:00.00]www.site.com"] {
            let j = lyrics_junk(only);
            assert!(j.only && j.lines.len() == 1 && j.cleaned.is_empty(), "{only}: {j:?}");
        }
        // real words with a site's credit under them: the credit is named, the words stay
        let j = lyrics_junk(&format!("{words}\n\nLyrics provided by SongWords.com\nVisit www.songwords.com for more great lyrics and free music\n"));
        assert_eq!(j.lines.iter().map(|l| l.1).collect::<Vec<_>>(), ["site credit", "web address"]);
        assert!(!j.only && j.cleaned == words, "{:?}", j.cleaned);
        // an uploader's credit, short; and timed lyrics keep their stamps on the lines that stay
        let j = lyrics_junk("[00:01.00]first line here\n[00:03.00]second line here\n[00:05.00]Ripped by XYZ\n[00:06.00]third line");
        assert_eq!(j.lines, [("Ripped by XYZ".to_string(), "site credit")]);
        assert_eq!(j.cleaned, "[00:01.00]first line here\n[00:03.00]second line here\n[00:06.00]third line");
        // lines a song can really have are never junk
        for sung in [
            "Come and join us, come and join us now",
            "I got ripped by the wind and torn by the rain on the road",
            "You were ripped from my arms",
            "Visit me in the morning",
            "Follow me down to the water",
            "Download my heart to your machine tonight",
            "Meet me at the dot com boom, baby, where the money grew on screens",
            "She said www dot nothing matters dot com is where we are all living now",
            "Subscribe to the feeling, baby",
            "I'm calling 555.1234 again",
            "Mr. Jones and me tell each other fairy tales",
            "[ar:Some Artist]",
            "It's 3 a.m. and I'm on the telegram line",
        ] {
            assert!(lyrics_junk(&format!("{words}\n{sung}")).lines.is_empty(), "{sung}");
        }
        // markup: the words stay, the markup goes
        let j = lyrics_junk("first line<br>\n<i>second line</i>\nthird &amp; fourth line");
        assert_eq!(j.lines.len(), 3);
        assert!(j.lines.iter().all(|l| l.1 == "markup"));
        assert_eq!(j.cleaned, "first line\nsecond line\nthird & fourth line");
        // a placeholder where the words should be
        for p in ["No lyrics available", "lyrics not available", "N/A", "...", "-", "none", "Lyrics"] {
            let j = lyrics_junk(p);
            assert!(j.only && j.lines[0].1 == "placeholder", "{p}");
        }
        // "Instrumental" is neither junk nor missing
        for i in ["Instrumental", "[Instrumental]", "(instrumental)"] {
            let j = lyrics_junk(i);
            assert!(j.instrumental && j.lines.is_empty() && !j.only, "{i}");
        }
        // scrambled text, put right where it can be
        let j = lyrics_junk("Donâ€™t go\nCafÃ© at night\nplain line here");
        assert_eq!(j.lines.len(), 1);
        assert_eq!(j.lines[0].1, "scrambled characters");
        assert_eq!(j.cleaned, "Don’t go\nCafé at night\nplain line here");
        // one odd character is not a scrambled lyric
        assert!(lyrics_junk("Donâ€™t go\nplain line\nanother plain line").lines.is_empty());
        assert!(lyrics_timed("[00:01.00]a\n[00:02.00]b") && !lyrics_timed("a\nb") && !lyrics_timed("[Chorus]\nwords"));
    }
}
