use regex::Regex;
use std::sync::LazyLock;

/// Lowercase, fold common accents, keep only letters and digits.
/// Curly vs straight apostrophes don't matter because all punctuation is dropped.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        match c {
            'œ' => { out.push_str("oe"); continue; }
            'æ' => { out.push_str("ae"); continue; }
            'ß' => { out.push_str("ss"); continue; }
            _ => {}
        }
        let c = fold_accent(c);
        if c.is_alphanumeric() {
            out.push(c);
        }
    }
    out
}

/// Artist-name key: like `norm`, but "&" == "and" and a leading "the" is ignored.
pub fn artist_key(s: &str) -> String {
    let s = s.trim().replace('&', " and ");
    let lower = s.to_lowercase();
    let s = lower.strip_prefix("the ").unwrap_or(&lower);
    let n = norm(s);
    if n.is_empty() { norm(&lower) } else { n }
}

fn fold_accent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        'ł' => 'l',
        _ => c,
    }
}

static RE_LEAD_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\[\(]\s*\d{4}\s*[\]\)]\s*-?\s*|^\s*\d{4}\s+-\s+").unwrap());
static RE_TRAIL_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[\{][^\(\)\[\]\{\}]*[\)\]\}]\s*$").unwrap());
static RE_TRAIL_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*(-\s*)?\b(ep|single|deluxe( edition| version)?|expanded edition|remaster(ed)?|special edition|bonus track version)\s*$").unwrap()
});
static RE_YEAR_PAREN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\(\[]((19|20)\d{2})[\)\]]").unwrap());
static RE_FEAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+(feat\.?|ft\.?|featuring)\s+.*$").unwrap());

/// "Album - EP", "Album (Deluxe)", "Album [Web FLAC]", "[2006] Album" -> "Album"
pub fn strip_edition(s: &str) -> String {
    let mut cur = RE_LEAD_YEAR.replace(s, "").to_string();
    loop {
        let before = cur.clone();
        cur = RE_TRAIL_BRACKET.replace(&cur, "").to_string();
        let t = RE_TRAIL_SUFFIX.replace(&cur, "").to_string();
        // keep a lone "EP"/"Single" title intact
        if !t.trim().is_empty() {
            cur = t;
        }
        cur = RE_FEAT.replace(&cur, "").to_string();
        cur = cur.trim().trim_end_matches(['-', ',', ':']).trim().to_string();
        if cur == before || cur.is_empty() {
            break;
        }
    }
    if cur.is_empty() { s.trim().to_string() } else { cur }
}

/// Key used to compare album titles loosely. "&" counts as "and" (as in `artist_key`), since a folder name
/// often spells out what the release title writes as a sign.
pub fn album_key(s: &str) -> String {
    let s = s.replace('&', " and ");
    let k = norm(&strip_edition(&s));
    if k.is_empty() { norm(&s) } else { k }
}

/// Year found in a folder name like "Album (2017)" or "[2006] Album".
pub fn folder_year(s: &str) -> Option<i32> {
    RE_YEAR_PAREN.captures(s).and_then(|c| c[1].parse().ok())
}

/// Track title guessed from a file name: "01 - Title.mp3", "Artist - 03 - Title.m4a", "1-02 Title.mp3".
pub fn track_key_from_file(stem: &str) -> String {
    let last = stem.rsplit(" - ").next().unwrap_or(stem);
    let t = last.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '.' || c == '_' || c == ' ');
    let t = if t.trim().is_empty() { last } else { t };
    album_key(t)
}

/// Parse a partial date ("2025", "2025-06", "2025-06-14", or ISO timestamp) to the latest day it could mean.
pub fn parse_date_latest(s: &str) -> Option<chrono::NaiveDate> {
    use chrono::NaiveDate;
    let s = s.get(..10).unwrap_or(s);
    let parts: Vec<&str> = s.split('-').collect();
    let y: i32 = parts.first()?.parse().ok()?;
    match parts.len() {
        1 => NaiveDate::from_ymd_opt(y, 12, 31),
        2 => {
            let m: u32 = parts[1].parse().ok()?;
            let next = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, m + 1, 1) }?;
            next.pred_opt()
        }
        _ => NaiveDate::from_ymd_opt(y, parts[1].parse().ok()?, parts[2].parse().ok()?),
    }
}

/// Parse a partial date to the earliest day it could mean ("2026" -> Jan 1). Used for "is it out yet?".
pub fn parse_date_earliest(s: &str) -> Option<chrono::NaiveDate> {
    use chrono::NaiveDate;
    let s = s.get(..10).unwrap_or(s);
    let parts: Vec<&str> = s.split('-').collect();
    let y: i32 = parts.first()?.parse().ok()?;
    let m: u32 = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(1);
    let d: u32 = parts.get(2).and_then(|d| d.parse().ok()).unwrap_or(1);
    NaiveDate::from_ymd_opt(y, m, d)
}

/// Write via a temp file + rename so a crash mid-write can't leave a half-written file.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Maps a file into memory for reading, for the life of the app. Nothing is loaded until it is looked at, and
/// what is loaded can be dropped by Windows and read again, so a big file costs almost nothing until used.
/// Only for files that do not change while the app runs (the system fonts).
pub fn map_file(path: &str) -> Option<&'static [u8]> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING};
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY};
    let wide: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let file = CreateFileW(wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ, std::ptr::null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
        if file == INVALID_HANDLE_VALUE || file.is_null() {
            return None;
        }
        let mut size: i64 = 0;
        if GetFileSizeEx(file, &mut size) == 0 || size <= 0 {
            CloseHandle(file);
            return None;
        }
        let mapping = CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null());
        CloseHandle(file);
        if mapping.is_null() {
            return None;
        }
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        CloseHandle(mapping);
        if view.Value.is_null() {
            return None;
        }
        // (never unmapped: the fonts are needed until the app closes)
        Some(std::slice::from_raw_parts(view.Value as *const u8, size as usize))
    }
}

#[cfg(test)]
mod map_file_tests {
    #[test]
    fn a_mapped_file_reads_the_same_as_a_read_file() {
        let path = std::env::temp_dir().join(format!("music-recs-map-{}.bin", std::process::id()));
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let mapped = super::map_file(&path.to_string_lossy()).unwrap();
        assert_eq!(mapped, &bytes[..]);
        assert!(super::map_file(r"C:\no\such\file.bin").is_none());
        // (the file stays mapped for the life of the test process, so it is left for the temp folder to clear)
    }
}

/// True for a plain web address: http or https, a host, nothing odd in it. Addresses come from online answers
/// (some of them community-edited), and anything else handed to Windows could open a file or a network share.
pub fn is_web_address(u: &str) -> bool {
    let rest = match u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) {
        Some(r) => r,
        None => return false,
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && host.contains('.') && !host.contains(['@', '\\']) && !u.chars().any(|c| c.is_control() || c == ' ' || c == '"')
}

/// True for an album or track page on Bandcamp itself: https, and a host that ends in ".bandcamp.com".
/// These addresses come from MusicBrainz, which anyone can edit, and the app both fetches the page and
/// offers it as a link, so "somewhere in the address it says bandcamp.com" is not enough.
pub fn is_bandcamp_page(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    is_web_address(u) && host.to_lowercase().ends_with(".bandcamp.com") && (path.starts_with("album/") || path.starts_with("track/"))
}

/// Opens a web address in the browser. Anything that is not a plain web address is ignored.
pub fn open_url(u: &str) {
    if is_web_address(u) {
        let _ = open::that(u);
    }
}

#[cfg(test)]
mod web_address_tests {
    use super::is_web_address;

    #[test]
    fn a_bandcamp_page_is_on_bandcamp_and_https() {
        use super::is_bandcamp_page;
        for ok in ["https://artist.bandcamp.com/album/x", "https://Some-Band.Bandcamp.com/track/y?z=1"] {
            assert!(is_bandcamp_page(ok), "{ok}");
        }
        for bad in [
            "http://artist.bandcamp.com/album/x",
            "https://evil.example/x.bandcamp.com/album/x",
            "https://artist.bandcamp.com.evil.example/album/x",
            "https://evil.example/?u=.bandcamp.com/album/x",
            "https://user@artist.bandcamp.com/album/x",
            "https://bandcamp.com/album/x",
            "https://artist.bandcamp.com/merch",
            r"\\artist.bandcamp.com\album\x",
        ] {
            assert!(!is_bandcamp_page(bad), "{bad}");
        }
    }

    #[test]
    fn only_plain_web_addresses_open() {
        for ok in ["https://artist.bandcamp.com/album/x", "http://www.last.fm/music/A+B", "https://musicbrainz.org/release/1?x=1#y"] {
            assert!(is_web_address(ok), "{ok}");
        }
        for bad in [r"\\host\share\x.exe", "file:///C:/Windows/x.exe", r"C:\x.exe", "https://", "https://user@evil.com/", "https://a.com/ b", "javascript:alert(1)", "ms-settings:", ""] {
            assert!(!is_web_address(bad), "{bad}");
        }
    }
}

pub fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editions() {
        assert_eq!(strip_edition("The Suburbs [Deluxe Edition]"), "The Suburbs");
        assert_eq!(strip_edition("Funeral  [Japanese Special Limited Edition]"), "Funeral");
        assert_eq!(strip_edition("[2005] Five Minutes With Arctic Monkeys [EP]"), "Five Minutes With Arctic Monkeys");
        assert_eq!(strip_edition("Trouble (2004) DELUXE EDITION"), "Trouble");
        assert_eq!(strip_edition("Sixth Saint - EP"), "Sixth Saint");
        assert_eq!(strip_edition("Song (feat. Someone) - Single"), "Song");
        assert_eq!(album_key("I’ll Try Living Like This"), album_key("I'll Try Living Like This (2015)"));
        assert_eq!(strip_edition("EP"), "EP");
        assert_eq!(artist_key("The Beatles"), artist_key("beatles"));
        assert_eq!(track_key_from_file("03 - Ungodly Gaze"), "ungodlygaze");
    }

    #[test]
    fn more_titles() {
        assert_eq!(album_key("Cœur Stone"), album_key("Coeur Stone"));
        assert_eq!(album_key("Björk"), album_key("Bjork"));
        assert_eq!(album_key("Queen of the Clouds (Blueprint Edition)"), album_key("Queen of the Clouds"));
        assert_eq!(album_key("Dirt Femme (stripped)"), album_key("Dirt Femme")); // why live versions need care
        assert_eq!(album_key("Album [Web FLAC]"), "album");
        // "&" and "and" are the same word
        assert_eq!(album_key("Dead Cities, Red Seas & Lost Ghosts"), album_key("Dead Cities, Red Seas and Lost Ghosts (2003)"));
        assert_eq!(album_key("Love&Hate"), album_key("Love and Hate"));
        assert_eq!(album_key("Whatever People Say I Am, That`s What I`m Not"), album_key("Whatever People Say I Am, That's What I'm Not"));
        assert_eq!(artist_key("Death’s Dynamic Shroud"), artist_key("death's dynamic shroud"));
        assert_eq!(artist_key("Simon & Garfunkel"), artist_key("Simon and Garfunkel"));
        assert_eq!(track_key_from_file("1-02 Title"), "title");
        assert_eq!(track_key_from_file("Artist - 03 - Some Song"), "somesong");
        // a title that is only brackets stays non-empty
        assert!(!album_key("(untitled)").is_empty());
    }

    #[test]
    fn dates() {
        use chrono::NaiveDate;
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(parse_date_latest("2026"), Some(d(2026, 12, 31)));
        assert_eq!(parse_date_earliest("2026"), Some(d(2026, 1, 1)));
        assert_eq!(parse_date_latest("2024-02"), Some(d(2024, 2, 29)));
        assert_eq!(parse_date_earliest("2024-02"), Some(d(2024, 2, 1)));
        assert_eq!(parse_date_latest("2026-11-06T07:00:00Z"), Some(d(2026, 11, 6)));
        assert_eq!(parse_date_latest(""), None);
        assert_eq!(parse_date_latest("garbage"), None);
    }
}

/// The two-letter country of the Apple Music store to look in: the one Windows' region is set to, so new
/// releases are the ones on sale where the user lives. "US" when Windows names no country (a world region
/// such as "001", or nothing).
pub fn store_country() -> &'static str {
    static COUNTRY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    COUNTRY.get_or_init(|| {
        let mut buf = [0u16; 16];
        // SAFETY: the buffer and its length are given together; Windows writes at most that many letters
        let n = unsafe { windows_sys::Win32::Globalization::GetUserDefaultGeoName(buf.as_mut_ptr(), buf.len() as i32) };
        let name = if n > 1 { String::from_utf16_lossy(&buf[..(n as usize - 1)]) } else { String::new() };
        country_from(&name)
    })
}

/// "GB" from Windows' region name; "US" for anything that is not two letters.
fn country_from(name: &str) -> String {
    let name = name.trim();
    if name.len() == 2 && name.chars().all(|c| c.is_ascii_alphabetic()) { name.to_ascii_uppercase() } else { "US".to_string() }
}

#[cfg(test)]
mod country_tests {
    #[test]
    fn the_store_country_is_two_letters_or_the_us() {
        assert_eq!(super::country_from("GB"), "GB");
        assert_eq!(super::country_from("de"), "DE");
        assert_eq!(super::country_from("001"), "US");
        assert_eq!(super::country_from(""), "US");
        assert_eq!(super::country_from("XYZ"), "US");
        let here = super::store_country();
        assert!(here.len() == 2 && here.chars().all(|c| c.is_ascii_uppercase()));
    }
}

static RE_OS_ERROR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\(os error -?\d+\)").unwrap());

/// A sentence for a page without Windows' "(os error 5)" after its own words, which say the same thing to
/// a person ("Access is denied.").
pub fn plain_err(s: String) -> String {
    if s.contains("(os error") { RE_OS_ERROR.replace_all(&s, "").into_owned() } else { s }
}

#[cfg(test)]
mod plain_err_tests {
    #[test]
    fn windows_error_numbers_are_left_out_of_sentences() {
        let e = std::io::Error::from_raw_os_error(5);
        assert_eq!(super::plain_err(format!("The list could not be saved: {e}")), "The list could not be saved: Access is denied.");
        assert_eq!(super::plain_err(format!("The helper could not be started ({e}).")), "The helper could not be started (Access is denied.).");
        assert_eq!(super::plain_err("Nothing to strip (here)".into()), "Nothing to strip (here)");
    }
}
