//! Name helpers shared by the scan, lookups and import.
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        pub static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

re!(RE_COLON, r":\s+");
re!(RE_BAD_CHARS, r#"[<>:"/\\|?*]"#);
re!(RE_EP_SINGLE, r"(?i)\s+-\s+(EP|Single)$");
re!(RE_TRAIL_BRACKET, r"\s*[\(\[\{][^\)\]\}]*[\)\]\}]\s*$");
re!(RE_YEAR_PAREN_END, r"\s*\(\d{4}\)\s*$");
re!(RE_LEAD_YEAR_DASH, r"^\d{4}\s+-\s+");

/// Lowercase letters and digits only ("Don't Stop" == "dont stop").
pub fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric() && !matches!(c, '’' | '‘' | '`')).collect()
}

/// Safe folder/file name: "A: B" -> "A - B", illegal characters -> "_".
pub fn clean_name(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let s = RE_COLON.replace_all(s, " - ");
    let s = RE_BAD_CHARS.replace_all(&s, "_");
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    let s = s.trim().trim_end_matches(['.', ' ']).to_string();
    // Windows device names ("CON", "NUL", "COM1", also with an extension) can't be folders or files
    let stem = s.split('.').next().unwrap_or("").trim().to_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit()));
    if reserved { format!("_{s}") } else { s }
}

#[cfg(test)]
mod clean_name_tests {
    use super::clean_name;

    #[test]
    fn names_are_safe_for_windows() {
        assert_eq!(clean_name("A: B"), "A - B");
        assert_eq!(clean_name("What?"), "What_");
        assert_eq!(clean_name("Ends with dots..."), "Ends with dots");
        assert_eq!(clean_name("."), "");
        assert_eq!(clean_name("CON"), "_CON");
        assert_eq!(clean_name("nul.mp3"), "_nul.mp3");
        assert_eq!(clean_name("com1"), "_com1");
        assert_eq!(clean_name("Console"), "Console");
        assert_eq!(clean_name("Complete"), "Complete");
    }
}

/// "Album - EP", "Album (Deluxe)", "Album [Web FLAC]" -> "Album"
pub fn strip_edition(s: &str) -> String {
    let s = RE_EP_SINGLE.replace(s, "");
    RE_TRAIL_BRACKET.replace(&s, "").trim().to_string()
}

pub fn strip_year(s: &str) -> String {
    let s = RE_YEAR_PAREN_END.replace(s, "");
    RE_LEAD_YEAR_DASH.replace(&s, "").trim().to_string()
}

/// What decides whether two folder names are the same album: the title and the kind of recording, without
/// the year and without notes that only name an edition or a format (see `crate::albumkind`). "Rocket",
/// "Rocket (2017)" and "Rocket (Deluxe) [FLAC]" share a key; "Rocket (Live)" and "Rocket (Remixes)" each
/// have their own.
pub fn album_key(folder: &str) -> String {
    crate::albumkind::classify(folder).key()
}

#[cfg(test)]
mod album_key_tests {
    use super::*;

    #[test]
    fn editions_and_formats_are_the_same_album() {
        for same in [
            "Rocket (2017)",
            "2017 - Rocket",
            "Rocket (Deluxe Edition)",
            "Rocket (Deluxe) [FLAC]",
            "Rocket [Web FLAC] (2017)",
            "Rocket (2011 Remaster)",
            "Rocket (10th Anniversary Edition)",
            "Rocket [24bit-96kHz]",
            "Rocket (Bonus Track Version)",
            "Rocket (Explicit)",
            "Rocket (Collector's Edition)",
            "Rocket - EP",
            "ROCKET {MP3 320}",
        ] {
            assert_eq!(album_key(same), album_key("Rocket"), "{same}");
        }
    }

    #[test]
    fn other_recordings_are_other_albums() {
        for other in ["Rocket (Live)", "Rocket (Live at Leeds)", "Rocket (Remixes)", "Rocket (Instrumentals)", "Rocket (Demos)", "Rocket (Acoustic)", "Rocket [Vol. 2]", "Rocket (Part II)", "Rocket (Chopped & Screwed)", "Rocket II"] {
            assert_ne!(album_key(other), album_key("Rocket"), "{other}");
        }
        // a name that is nothing but a bracket keeps it
        assert_eq!(album_key("(Deluxe)"), "deluxe");
        assert_ne!(album_key("(2017)"), "");
    }
}

/// Most frequent non-empty value (case-insensitive); ties go to whichever came first.
pub fn most_common<I: IntoIterator<Item = String>>(values: I) -> Option<String> {
    let mut order: Vec<(String, String, usize)> = vec![]; // (lowercase key, first spelling, count)
    for v in values {
        if v.is_empty() {
            continue;
        }
        let k = v.to_lowercase();
        match order.iter_mut().find(|e| e.0 == k) {
            Some(e) => e.2 += 1,
            None => order.push((k, v, 1)),
        }
    }
    let mut best: Option<&(String, String, usize)> = None;
    for e in &order {
        if best.map(|b| e.2 > b.2).unwrap_or(true) {
            best = Some(e);
        }
    }
    best.map(|b| b.1.clone())
}

pub fn format_time(sec: f64) -> String {
    format!("{}:{:02}", (sec / 60.0).floor() as i64, (sec % 60.0).round() as i64)
}

/// "{:0.#}" - one decimal, dropped when it's .0
pub fn trim_num(x: f64) -> String {
    // halves round away from zero ("17.25" -> "17.3")
    let s = format!("{:.1}", (x * 10.0).round() / 10.0);
    s.strip_suffix(".0").map(|s| s.to_string()).unwrap_or(s)
}

pub fn file_stem(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

pub fn ext_lower(p: &Path) -> String {
    p.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default()
}

/// Parent part of a "a\b\c.mp3" style relative path ("" when there is none).
pub fn rel_parent(rel: &str) -> String {
    rel.rsplit_once('\\').map(|x| x.0.to_string()).unwrap_or_default()
}
pub fn rel_leaf(rel: &str) -> String {
    rel.rsplit_once('\\').map(|x| x.1.to_string()).unwrap_or_else(|| rel.to_string())
}

/// Sorted list of the entries of a folder (case-insensitive name order, like Explorer).
pub fn read_dir_sorted(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut v: Vec<std::fs::DirEntry> = std::fs::read_dir(dir).map(|r| r.flatten().collect()).unwrap_or_default();
    v.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    v
}

/// Every file under `dir`: this folder's files first, then each subfolder's.
pub fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    let mut subs = vec![];
    for e in read_dir_sorted(dir) {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => subs.push(p),
            Ok(_) => files.push(p),
            Err(_) => {}
        }
    }
    for s in subs {
        files.extend(walk_files(&s));
    }
    files
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Group case-insensitively, keeping first-seen order: (first spelling, items).
pub fn group_by_key<T, K: Fn(&T) -> String>(items: Vec<T>, key: K) -> Vec<(String, Vec<T>)> {
    let mut out: Vec<(String, Vec<T>)> = vec![];
    let mut idx: HashMap<String, usize> = HashMap::new();
    for it in items {
        let k = key(&it);
        let kl = k.to_lowercase();
        match idx.get(&kl) {
            Some(&i) => out[i].1.push(it),
            None => {
                idx.insert(kl, out.len());
                out.push((k, vec![it]));
            }
        }
    }
    out
}
