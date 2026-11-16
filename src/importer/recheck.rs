//! Judging an album in the library against its official track list.
//!
//! The library check (`healthscan`) hands each album's tracks, as it read them, to `judge_album`: only an
//! official album of the same kind of recording is compared (a live album never against the studio one),
//! and of its editions the one whose track list fits the copy's titles best. What was concluded for each
//! album is kept in the app's own folder (`overlay_file`), in the report's own shape, and stands for a
//! month while the album's audio files are unchanged, so an album is only asked about once.
use crate::albumkind::{self, Kind};
use crate::importer::ctx::Ctx;
use crate::importer::scan::is_audio_ext;
use crate::importer::tracklist::{self, Finding, Fit, OurTrack, RealList, RealTrack};
use crate::importer::util::{ext_lower, most_common, walk_files};
use crate::importer::lookup;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The findings that come from comparing a whole album with an official track list.
pub const WHOLE: [&str; 4] = ["Wrong track numbers or titles", "Wrong or swapped songs", "Different version of a song", "Incomplete album"];
/// Gaps in the track numbers, which the check then tries to confirm against the official list.
pub const GAPS: &str = "Missing tracks";
/// Bumped when the rules change in a way that makes earlier answers worth asking again.
pub const RULES: u64 = 2;

/// One album the report has findings for, named exactly as the report names it.
#[derive(Clone)]
pub struct Album {
    pub artist: String,
    pub album: String,
    pub path: String,
}

/// Where what is known about this library's track lists is kept.
pub fn overlay_file(library: &str) -> PathBuf {
    let key = format!("{:x}", md5::compute(library.trim().trim_end_matches(['\\', '/']).to_lowercase().as_bytes()));
    crate::config::app_dir().join(format!("health-tracklists-{}.json", &key[..10]))
}

/// What an album folder's audio looked like: how many files, their total size and the newest change. A
/// recheck stands for as long as this is the same.
pub fn sig_of(folder: &Path) -> String {
    let (mut count, mut bytes, mut newest) = (0u64, 0u64, 0u64);
    for f in walk_files(folder).into_iter().filter(|p| is_audio_ext(&ext_lower(p))) {
        let Ok(m) = std::fs::metadata(&f) else { continue };
        count += 1;
        bytes += m.len();
        let at = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        newest = newest.max(at);
    }
    format!("{count}|{bytes}|{newest}")
}

pub(crate) fn read_all(file: &Path) -> Map<String, Value> {
    match std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(Value::Object(mut root)) => match root.remove("Albums") {
            Some(Value::Object(a)) => a,
            _ => Map::new(),
        },
        _ => Map::new(),
    }
}

pub(crate) fn save(file: &Path, albums: &Map<String, Value>) {
    let text = json!({ "Albums": albums }).to_string();
    let _ = crate::importer::util::write_atomic(file, text.as_bytes());
}

/// What the recheck concluded for one album, before it is written down.
pub struct Outcome {
    pub kind: Kind,
    /// where the official list came from ("" = none was found)
    pub source: String,
    pub fit: bool,
    /// every one of the copy's tracks is on the list and nothing but optional extras is missing
    pub all_there: bool,
    pub findings: Vec<Finding>,
    /// the official list's own tracks with their lengths, when one list fits the whole album (empty when
    /// none does, and for a box judged disc by disc): what a damaged file's real length is read from
    pub real: Vec<RealTrack>,
}

/// Which of the report's findings a recheck replaces.
///   * a list of the right kind that fits: all the whole-album findings (the new ones stand in), and the
///     numbering gaps too when every song is there
///   * no list of the right kind, or one that does not fit, for a live, remix or other non-studio album:
///     the whole-album findings, which were made against the wrong record
///   * otherwise nothing: the report knows as much as the recheck does
pub fn dropped(o: &Outcome) -> Vec<&'static str> {
    let mut drop: Vec<&'static str> = vec![];
    if o.fit {
        drop.extend(WHOLE);
        if o.all_there {
            drop.push(GAPS);
        }
    } else if o.kind != Kind::Studio {
        drop.extend(WHOLE);
    }
    drop
}

/// The recheck's findings in the report's shape (S, C, A, L, P, D, K, F, X).
pub fn issues_of(a: &Album, o: &Outcome, search: &str) -> Vec<Value> {
    o.findings
        .iter()
        .map(|f| {
            let (mut fix, mut files, mut facts) = ("", vec![], Map::new());
            match f.category {
                "Wrong track numbers or titles" => {
                    fix = if f.changes.iter().all(|c| c.2.is_empty() && c.3.is_empty()) { "tracklist-titles" } else { "tracklist" };
                    files = f.changes.iter().map(|c| c.0.clone()).collect();
                    let rows: Vec<Value> = f.changes.iter().map(|c| json!([c.0, c.1, c.2, c.3])).collect();
                    facts.insert("changes".into(), Value::String(Value::Array(rows).to_string()));
                }
                "Incomplete album" => {
                    fix = "redownload";
                    facts.insert("search".into(), Value::String(search.to_string()));
                }
                _ => {}
            }
            json!({ "S": f.severity, "C": f.category, "A": a.artist, "L": a.album, "P": a.path, "D": f.detail, "K": fix, "F": files, "X": facts })
        })
        .collect()
}

/// One track as read from disk, with the tags the comparison needs.
pub struct Read {
    pub track: OurTrack,
    /// the folder inside the album the file is in ("" for the album folder itself)
    pub sub: String,
    pub album_tag: String,
    /// the album artist and artist tags that are filled in
    pub artist_tags: Vec<String>,
}

static RE_ARTIST_SPLIT: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\s*(?:&|,|\+|;|\s/\s|\s+and\s+|\s+x\s+|\s+feat\.?\s+|\s+ft\.?\s+|\s+featuring\s+|\s+with\s+|\s+vs\.?\s+)\s*").unwrap());
/// "Disc 1 - House Of Balloons", "[CD 2] Thursday": the disc of a box that is an album of its own, and its name
static RE_DISC_TITLE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^[\[\(]?\s*(?:cd|disc|disk)\s*\d{1,2}\s*[\]\)]?\s*[-:\x{2013}]?\s*(\S.*)$").unwrap());

/// The artists to look an album up under: the folder's artist first, then the others named in its tags
/// (a split release or a collaboration is often listed under the other artist, or under both).
pub fn lookup_artists(folder_artist: &str, tags: &[String]) -> Vec<String> {
    let key = |s: &str| crate::importer::util::norm(s);
    let mut out = vec![folder_artist.to_string()];
    let mut whole: Vec<&String> = tags.iter().collect();
    whole.sort();
    whole.dedup();
    for t in whole {
        for name in std::iter::once(t.as_str()).chain(RE_ARTIST_SPLIT.split(t)) {
            let name = name.trim();
            if name.chars().count() >= 2 && !key(name).is_empty() && !out.iter().any(|o| key(o) == key(name)) {
                out.push(name.to_string());
            }
        }
    }
    out.truncate(4);
    out
}

/// The name of the album a disc folder holds, if the folder names one ("Disc 1 - House Of Balloons").
pub fn disc_title(folder: &str) -> Option<String> {
    let t = RE_DISC_TITLE.captures(folder.trim())?[1].trim().to_string();
    // "Disc 2 (Bonus)" or "CD1 [FLAC]" name nothing
    (!albumkind::is_edition_note(t.trim_matches(|c: char| "()[]{}".contains(c))) && !crate::library::is_disc_like(&t) && t.chars().filter(|c| c.is_alphabetic()).count() >= 3).then_some(t)
}

/// The best-fitting official list of the right kind for these tracks, and what comparing them finds.
fn judge(c: &Ctx, artists: &[String], name: &str, kind_hint: Kind, ours: &[OurTrack]) -> (Kind, Option<(RealList, Vec<Finding>, Fit)>) {
    let titles: Vec<String> = ours.iter().map(|t| t.title.clone()).collect();
    let mut want = albumkind::classify_with_titles(name, &titles);
    if want.kind == Kind::Studio && kind_hint != Kind::Studio {
        want.kind = kind_hint;
    }
    for artist in artists {
        let mut real: Option<RealList> = None;
        if let Some((_, tracks)) = lookup::find_apple_edition(c, artist, name, &want, &titles) {
            let list: Vec<RealTrack> = tracks.iter().filter(|t| t.millis > 0).map(|t| RealTrack { d: t.disc_number, n: t.number, t: t.name.clone(), s: (t.millis as f64 / 100.0).round() / 10.0 }).collect();
            if !list.is_empty() {
                real = Some(RealList { source: "Apple Music".into(), tracks: list });
            }
        }
        if real.is_none() {
            real = tracklist::musicbrainz_tracklist(c, artist, name, &want, &titles);
        }
        if let Some(real) = real {
            let mut fit = Fit::default();
            let findings = tracklist::resolve(ours, &real, &mut fit);
            return (want.kind, Some((real, findings, fit)));
        }
    }
    (want.kind, None)
}

fn outcome_of(kind: Kind, judged: Option<(RealList, Vec<Finding>, Fit)>, tracks: usize) -> Outcome {
    let Some((real, findings, fit)) = judged else { return Outcome { kind, source: String::new(), fit: false, all_there: false, findings: vec![], real: vec![] } };
    let really_missing = findings.iter().any(|f| f.category == "Incomplete album" && f.severity <= 2);
    Outcome { kind, source: real.source, fit: fit.fit, all_there: fit.fit && !really_missing && fit.matched >= tracks, findings, real: if fit.fit { real.tracks } else { vec![] } }
}

/// Judges an album's tracks, already read, against the best-fitting official list of its own kind: the whole
/// album first, and a box of albums disc by disc. `leaf` is the album folder's name. Returns what was
/// concluded and the words to search for a better copy with.
pub fn judge_album(c: &Ctx, artist: &str, leaf: &str, read: &[Read]) -> (Outcome, String) {
    let name = most_common(read.iter().map(|r| r.album_tag.clone())).unwrap_or_else(|| leaf.to_string());
    // the folder's name may say what the tag leaves out ("Rocket (Live) (2005)" tagged "Rocket")
    let by_folder = albumkind::classify(leaf).kind;
    let artists = lookup_artists(artist, &read.iter().flat_map(|r| r.artist_tags.clone()).collect::<Vec<_>>());
    let search = tracklist::search_text(artist, &name);
    let ours: Vec<&Read> = read.iter().collect();
    let tracks = |list: &[&Read]| -> Vec<OurTrack> { list.iter().map(|r| OurTrack { file: r.track.file.clone(), title: r.track.title.clone(), disc: r.track.disc, no: r.track.no, secs: r.track.secs, errors: 0 }).collect() };

    let (kind, judged) = judge(c, &artists, &name, by_folder, &tracks(&ours));
    let whole = outcome_of(kind, judged, read.len());
    if whole.fit {
        return (whole, search);
    }
    // A box of albums ("Trilogy\Disc 1 - House Of Balloons"): no one list fits the box, but each disc that
    // names an album can be judged as that album.
    let mut discs: Vec<(String, String)> = vec![]; // (folder, the album it names)
    for r in read {
        if !r.sub.is_empty() && !discs.iter().any(|d| d.0 == r.sub) {
            if let Some(title) = disc_title(&r.sub) {
                discs.push((r.sub.clone(), title));
            }
        }
    }
    if discs.len() < 2 {
        return (whole, search);
    }
    let (mut findings, mut source, mut any_fit, mut all_there) = (vec![], String::new(), false, true);
    for (sub, title) in &discs {
        let mut on_disc = tracks(&read.iter().filter(|r| &r.sub == sub).collect::<Vec<_>>());
        on_disc.iter_mut().for_each(|t| t.disc = 0);
        let (k, judged) = judge(c, &artists, title, Kind::Studio, &on_disc);
        let o = outcome_of(k, judged, on_disc.len());
        any_fit |= o.fit;
        all_there &= o.all_there;
        if o.fit {
            source = o.source.clone();
            findings.extend(o.findings.into_iter().map(|mut f| {
                f.detail = format!("{title}: {}", f.detail);
                f
            }));
        }
    }
    if !any_fit {
        return (whole, search);
    }
    // tracks outside the named discs ("Extras") were not judged: the numbering gaps are not vouched for
    let covered = read.iter().all(|r| discs.iter().any(|d| d.0 == r.sub));
    (Outcome { kind: whole.kind, source, fit: true, all_there: all_there && covered, findings, real: vec![] }, search)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(category: &'static str, severity: u8) -> Finding {
        Finding { category, severity, detail: format!("{category}!"), changes: vec![("f.mp3".into(), "New".into(), String::new(), String::new())] }
    }
    fn outcome(kind: Kind, source: &str, fit: bool, all_there: bool, findings: Vec<Finding>) -> Outcome {
        Outcome { kind, source: source.into(), fit, all_there, findings, real: vec![] }
    }

    #[test]
    fn a_recheck_replaces_only_what_it_can_judge_better() {
        // a fitting list: the whole-album findings are replaced; the numbering gaps only when every song is there
        assert_eq!(dropped(&outcome(Kind::Studio, "Apple Music", true, false, vec![])), WHOLE.to_vec());
        let mut all = WHOLE.to_vec();
        all.push(GAPS);
        assert_eq!(dropped(&outcome(Kind::Studio, "Apple Music", true, true, vec![])), all);
        // a live album with no live list (or one that does not fit): what was said against the studio album goes
        assert_eq!(dropped(&outcome(Kind::Live, "", false, false, vec![])), WHOLE.to_vec());
        assert_eq!(dropped(&outcome(Kind::Remix, "MusicBrainz", false, false, vec![])), WHOLE.to_vec());
        // a studio album the recheck cannot place either: the report stands
        assert!(dropped(&outcome(Kind::Studio, "", false, false, vec![])).is_empty());
        assert!(dropped(&outcome(Kind::Studio, "MusicBrainz", false, false, vec![])).is_empty());
    }

    #[test]
    fn findings_are_written_in_the_reports_shape() {
        let a = Album { artist: "A".into(), album: "Rocket (2017)".into(), path: r"X:\Lib\A\Rocket (2017)".into() };
        let o = outcome(Kind::Studio, "Apple Music", true, false, vec![finding("Wrong track numbers or titles", 3), finding("Incomplete album", 2), finding("Different version of a song", 3)]);
        let issues = issues_of(&a, &o, "A Rocket");
        assert_eq!(issues.len(), 3);
        assert_eq!((issues[0]["K"].as_str(), issues[0]["F"][0].as_str(), issues[0]["P"].as_str()), (Some("tracklist-titles"), Some("f.mp3"), Some(r"X:\Lib\A\Rocket (2017)")));
        assert_eq!(issues[0]["X"]["changes"].as_str(), Some(r#"[["f.mp3","New","",""]]"#));
        assert_eq!((issues[1]["K"].as_str(), issues[1]["X"]["search"].as_str(), issues[1]["S"].as_u64()), (Some("redownload"), Some("A Rocket"), Some(2)));
        assert_eq!((issues[2]["K"].as_str(), issues[2]["A"].as_str(), issues[2]["L"].as_str()), (Some(""), Some("A"), Some("Rocket (2017)")));
    }

    #[test]
    fn saved_findings_stand_only_while_the_album_is_unchanged() {
        let dir = std::env::temp_dir().join(format!("music-recs-recheck-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let album = dir.join("A").join("Rocket");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 one.mp3"), b"one").unwrap();
        std::fs::write(album.join("cover.jpg"), b"picture").unwrap();
        let path = album.to_string_lossy().to_string();
        let file = dir.join("overlay.json");
        let was = sig_of(&album);
        let mut albums = Map::new();
        albums.insert(path.to_lowercase(), json!({ "Path": path, "Sig": was, "Drop": ["Incomplete album"], "Issues": [] }));
        save(&file, &albums);
        assert_eq!(read_all(&file).len(), 1);
        // a picture changing is not the album changing; a track changing is
        std::fs::write(album.join("cover.jpg"), b"another picture").unwrap();
        assert_eq!(sig_of(&album), was);
        std::fs::write(album.join("02 two.mp3"), b"two").unwrap();
        assert_ne!(sig_of(&album), was);
        // a damaged or missing file is simply nothing known
        std::fs::write(&file, b"{ not json").unwrap();
        assert!(read_all(&file).is_empty());
        assert!(read_all(&dir.join("nope.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;

    #[test]
    fn a_split_release_is_looked_up_under_each_artist() {
        let tags = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(lookup_artists("The Mountain Goats", &tags(&["The Mountain Goats & Simon Joyner", "The Mountain Goats"])), vec!["The Mountain Goats", "The Mountain Goats & Simon Joyner", "Simon Joyner"]);
        assert_eq!(lookup_artists("A", &tags(&["a", "A"])), vec!["A"]);
        assert_eq!(lookup_artists("Aa", &tags(&["Bb feat. Cc", "Dd x Ee", "Ff"])), vec!["Aa", "Bb feat. Cc", "Bb", "Cc"]);
        assert_eq!(lookup_artists("A", &[]), vec!["A"]);
    }

    #[test]
    fn a_disc_folder_that_names_an_album_is_told_from_one_that_does_not() {
        assert_eq!(disc_title("Disc 1 - House Of Balloons").as_deref(), Some("House Of Balloons"));
        assert_eq!(disc_title("[CD 2] Thursday").as_deref(), Some("Thursday"));
        assert_eq!(disc_title("CD 1 - Dawn To Dusk").as_deref(), Some("Dawn To Dusk"));
        for plain in ["CD1", "Disc 2", "Disc 2 (Bonus)", "CD1 [FLAC]", "Extras", "House Of Balloons", "CD 2 - Vol. 2"] {
            assert_eq!(disc_title(plain), None, "{plain}");
        }
    }
}
