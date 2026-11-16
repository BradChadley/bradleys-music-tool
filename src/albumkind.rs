//! What an album's name says it is: which record, what kind of recording, and whether it is a special edition.
//!
//! "Rocket (2017)", "Rocket (Deluxe Edition) [FLAC]" and "Rocket - EP" are the record "Rocket".
//! "Rocket (Live)", "Rocket (Live at Leeds)" and "Rocket - Live" are a live recording of it: another album,
//! with other songs lengths, that must never be compared with the studio one. The same goes for remixes,
//! instrumentals, demos and acoustic versions. A bracket that says none of these ("Vol. 2", "Part II",
//! "Chopped & Screwed") is part of the title.
//!
//! One rule for the whole app: the importer (is this the album I already have?), the track-list lookups
//! (which official album do I compare with?) and the Health recheck.
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Studio,
    Live,
    Remix,
    Instrumental,
    Demo,
    Acoustic,
    /// a "greatest hits", "best of" or other collection of songs from several records
    Compilation,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Studio => "",
            Kind::Live => "live",
            Kind::Remix => "remix",
            Kind::Instrumental => "instrumental",
            Kind::Demo => "demo",
            Kind::Acoustic => "acoustic",
            Kind::Compilation => "compilation",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct AlbumId {
    /// the title without its year, edition notes and kind notes
    pub base: String,
    pub kind: Kind,
    /// a deluxe, expanded, remastered or anniversary edition (or a format note) was named
    pub edition: bool,
}

/// Words that describe a pressing or a file format of an album, not a different album.
const EDITION_WORDS: [&str; 64] = [
    "deluxe", "super", "edition", "version", "expanded", "extended", "remaster", "remastered", "remasters", "special", "bonus", "track", "tracks",
    "anniversary", "explicit", "clean", "reissue", "reissued", "limited", "collector", "collectors", "japan", "japanese", "uk", "us", "import", "web",
    "flac", "mp3", "aac", "alac", "wav", "ogg", "opus", "m4a", "lossless", "cbr", "vbr", "v0", "v2", "cd", "cdrip", "vinyl", "lp", "digital", "hi", "res",
    "hires", "mono", "stereo", "album", "ep", "single", "the", "and", "with", "original", "motion", "picture", "soundtrack", "ost", "retail", "legacy", "complete",
];

static YEAR_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*[\(\[]\s*(19|20)\d{2}\s*[\)\]]\s*$").unwrap());
static YEAR_LEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[\(\[]\s*(?:19|20)\d{2}\s*[\)\]]\s*-?\s*|(?:19|20)\d{2}\s+-\s+)").unwrap());
static TRAIL_BRACKET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*[\(\[\{]([^\(\)\[\]\{\}]*)[\)\]\}]\s*$").unwrap());
static TRAIL_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:\s+[-\x{2013}\x{2014}]\s+|:\s+)([^-\x{2013}\x{2014}:]+)$").unwrap());
// "live" only where it means a concert: at the start or the end of a note, or before "at", "in", "from", "on"
// (or a word like "album"). "(Long Live the King)" and "Live Through This" are titles, not live records.
static LIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^live\b(\s+(through|and|or|to|for|my|your|our|like|it|forever|again|free|fast|life|love|wire|by)\b)?|\blive\s+(at|in|from|on|album|version|edition|sessions?|recordings?|set|ep)\b|\blive$|\bunplugged\b|\bin concert\b").unwrap());
static COMPILATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgreatest hits\b|\bbest of\b|\bthe (very )?best\b|\bessentials?\b|\banthology\b|\b(singles|complete|definitive|ultimate) collection\b|\bthe collection\b").unwrap());
static REMIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bremix(es|ed)?\b|\brmx\b|\brework(s|ed)?\b").unwrap());
static INSTRUMENTAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\binstrumentals?\b").unwrap());
static DEMO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bdemos?\b").unwrap());
static ACOUSTIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bacoustic\b").unwrap());
/// a whole title that is itself a live record's name: "Live at Leeds", "MTV Unplugged in New York"
static LIVE_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^live!?$|\blive (at|in|from|on)\b|\bunplugged\b|\bin concert\b").unwrap());
/// words after which a closing "live" is a verb or an adjective ("Born to Live", "Long Live"), not a concert
const BEFORE_VERB_LIVE: [&str; 16] = ["to", "long", "i", "we", "you", "they", "gonna", "wanna", "must", "will", "can", "let", "lets", "just", "and", "die"];
/// a title that ends by naming its kind: "Rocket Live", "Rocket Remixes"
static KIND_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+(live|remix(es|ed)?|instrumentals?|demos?)$").unwrap());
/// a track title that says the track is a live take, a remix, and so on
static TRACK_LIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[\(\[]\s*live\b|\s-\s+live\b|\blive (at|in|from|on)\b").unwrap());
static TRACK_REMIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bremix\b|\brmx\b|\b(re)?mix[\)\]]|\brework\b").unwrap());

/// Lowercase letters and digits only.
fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric() && !matches!(c, '\u{2019}' | '\u{2018}' | '`')).collect()
}

/// True if the text only says which edition or format this is ("Deluxe Edition", "2011 Remaster",
/// "Web FLAC", "24bit-96kHz").
pub fn is_edition_note(inner: &str) -> bool {
    inner.to_lowercase().replace('\'', "").split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).all(|w| {
        let digits = w.trim_end_matches(|c: char| c.is_alphabetic());
        let unit = &w[digits.len()..];
        (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && matches!(unit, "" | "th" | "st" | "nd" | "rd" | "bit" | "khz" | "hz" | "k" | "kbps"))
            || EDITION_WORDS.contains(&w)
    })
}

/// The kind a bracket or a suffix names, if it names one.
fn kind_of_note(note: &str) -> Option<Kind> {
    // (the regex crate has no "not followed by": a start-of-note "live" that goes on like a sentence is ruled out here)
    let live = LIVE.captures(note).is_some_and(|c| c.get(1).is_none());
    if live {
        Some(Kind::Live)
    } else if REMIX.is_match(note) {
        Some(Kind::Remix)
    } else if INSTRUMENTAL.is_match(note) {
        Some(Kind::Instrumental)
    } else if DEMO.is_match(note) {
        Some(Kind::Demo)
    } else if ACOUSTIC.is_match(note) {
        Some(Kind::Acoustic)
    } else if COMPILATION.is_match(note) {
        Some(Kind::Compilation)
    } else {
        None
    }
}

fn strip_year(s: &str) -> String {
    let s = YEAR_END.replace(s, "");
    YEAR_LEAD.replace(&s, "").trim().to_string()
}

/// Reads an album name (a folder name, an album tag, a store's title).
pub fn classify(name: &str) -> AlbumId {
    let mut cur = strip_year(name);
    let (mut kind, mut edition) = (Kind::Studio, false);
    loop {
        let before = cur.clone();
        // a trailing bracket, then a trailing " - note": taken off while they are edition or kind notes
        for re in [&*TRAIL_BRACKET, &*TRAIL_DASH] {
            let Some(c) = re.captures(&cur) else { continue };
            let (whole, note) = (c.get(0).unwrap(), c[1].to_string());
            if whole.start() == 0 {
                continue; // the name is nothing but this
            }
            if is_edition_note(&note) {
                edition = true;
            } else if let Some(k) = kind_of_note(&note) {
                // the first kind named, reading from the end, stands ("(Live) (Remixes)" is rare enough)
                if kind == Kind::Studio {
                    kind = k;
                }
            } else {
                continue;
            }
            cur = cur[..whole.start()].to_string();
        }
        cur = strip_year(&cur);
        if cur == before {
            break;
        }
    }
    // no note, but the title itself says so: "Live at Leeds" stays whole, "Rocket Live" is "Rocket", live
    if kind == Kind::Studio {
        let ending = KIND_END.captures(&cur).filter(|c| c.get(0).unwrap().start() > 0).filter(|c| {
            // "Born to Live" ends in a verb, not in a kind
            let before = cur[..c.get(0).unwrap().start()].split_whitespace().last().unwrap_or("").to_lowercase();
            !(c[1].eq_ignore_ascii_case("live") && BEFORE_VERB_LIVE.contains(&before.trim_matches(|ch: char| !ch.is_alphanumeric())))
        });
        if let Some(c) = ending {
            kind = kind_of_note(&c[1]).unwrap_or(Kind::Studio);
            cur = cur[..c.get(0).unwrap().start()].to_string();
        } else if LIVE_TITLE.is_match(&cur) {
            kind = Kind::Live;
        } else if COMPILATION.is_match(&cur) {
            kind = Kind::Compilation; // "Greatest Hits", "The Best of Rocket": the whole title stays
        }
    }
    let base = cur.trim().to_string();
    // a name that is nothing but a year or a note keeps its own text: an empty title would match every other
    if norm(&base).is_empty() {
        return AlbumId { base: name.trim().to_string(), kind: Kind::Studio, edition: false };
    }
    AlbumId { base, kind, edition }
}

/// Like `classify`, with the track titles as a second witness: an album whose tracks nearly all say
/// "(Live)" or "Remix" is one, whatever its name says.
pub fn classify_with_titles(name: &str, titles: &[String]) -> AlbumId {
    let mut id = classify(name);
    if id.kind == Kind::Studio && titles.len() >= 4 {
        let share = |re: &Regex| titles.iter().filter(|t| re.is_match(t)).count() as f32 / titles.len() as f32;
        if share(&TRACK_LIVE) >= 0.7 {
            id.kind = Kind::Live;
        } else if share(&TRACK_REMIX) >= 0.7 {
            id.kind = Kind::Remix;
        } else if share(&INSTRUMENTAL) >= 0.7 {
            id.kind = Kind::Instrumental;
        } else if share(&DEMO) >= 0.7 {
            id.kind = Kind::Demo;
        }
    }
    id
}

impl AlbumId {
    /// What decides whether two names are the same album: the title and the kind, not the edition.
    pub fn key(&self) -> String {
        let base = norm(&self.base);
        if self.kind == Kind::Studio { base } else { format!("{base}|{}", self.kind.tag()) }
    }

    /// True if `found` (a store's or a database's title) names this album: the same title and the same
    /// kind of recording. Editions of it count.
    pub fn is(&self, found: &str) -> bool {
        classify(found).key() == self.key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> (String, Kind, bool) {
        let a = classify(name);
        (a.base, a.kind, a.edition)
    }

    #[test]
    fn names_are_read_as_title_kind_and_edition() {
        for (name, base, kind, edition) in [
            ("Rocket", "Rocket", Kind::Studio, false),
            ("Rocket (2017)", "Rocket", Kind::Studio, false),
            ("[2017] Rocket", "Rocket", Kind::Studio, false),
            ("2017 - Rocket", "Rocket", Kind::Studio, false),
            ("Rocket (Deluxe Edition)", "Rocket", Kind::Studio, true),
            ("Rocket (Deluxe) [FLAC] (2017)", "Rocket", Kind::Studio, true),
            ("Rocket - EP", "Rocket", Kind::Studio, true),
            ("Rocket (10th Anniversary Edition)", "Rocket", Kind::Studio, true),
            ("Rocket: Deluxe Edition", "Rocket", Kind::Studio, true),
            ("Rocket (Live)", "Rocket", Kind::Live, false),
            ("Rocket (Live at Leeds)", "Rocket", Kind::Live, false),
            ("Rocket (Live) [Deluxe Edition]", "Rocket", Kind::Live, true),
            ("Rocket - Live", "Rocket", Kind::Live, false),
            ("Rocket Live", "Rocket", Kind::Live, false),
            ("Rocket (MTV Unplugged)", "Rocket", Kind::Live, false),
            ("Live at Leeds", "Live at Leeds", Kind::Live, false),
            ("Live at Leeds (Deluxe Edition)", "Live at Leeds", Kind::Live, true),
            ("MTV Unplugged in New York", "MTV Unplugged in New York", Kind::Live, false),
            ("Rocket (Remixes)", "Rocket", Kind::Remix, false),
            ("Rocket (The Remixes)", "Rocket", Kind::Remix, false),
            ("Rocket Remixed", "Rocket", Kind::Remix, false),
            ("Rocket (Instrumentals)", "Rocket", Kind::Instrumental, false),
            ("Rocket (Demos)", "Rocket", Kind::Demo, false),
            ("Rocket (Acoustic)", "Rocket", Kind::Acoustic, false),
            // not notes: part of the title
            ("Rocket [Vol. 2]", "Rocket [Vol. 2]", Kind::Studio, false),
            ("Rocket (Part II)", "Rocket (Part II)", Kind::Studio, false),
            ("Rocket (Chopped & Screwed)", "Rocket (Chopped & Screwed)", Kind::Studio, false),
            ("Rocket II", "Rocket II", Kind::Studio, false),
            ("Alive", "Alive", Kind::Studio, false),
            ("Olive (2019)", "Olive", Kind::Studio, false),
            ("Kid A - Mnesia", "Kid A - Mnesia", Kind::Studio, false),
            // nothing but a note or a year
            ("(Deluxe)", "(Deluxe)", Kind::Studio, false),
            ("(2017)", "(2017)", Kind::Studio, false),
            ("Live", "Live", Kind::Live, false),
            // "live" as an ordinary word
            ("Live Through This", "Live Through This", Kind::Studio, false),
            ("Rocket (Long Live the King)", "Rocket (Long Live the King)", Kind::Studio, false),
            ("Born to Live", "Born to Live", Kind::Studio, false),
            ("Live and Let Die", "Live and Let Die", Kind::Studio, false),
            ("Live in Paris", "Live in Paris", Kind::Live, false),
            ("Rocket (Deluxe Live Edition)", "Rocket", Kind::Live, false),
            // collections
            ("Greatest Hits", "Greatest Hits", Kind::Compilation, false),
            ("The Best of Rocket (2010)", "The Best of Rocket", Kind::Compilation, false),
            ("Rocket (Greatest Hits)", "Rocket", Kind::Compilation, false),
            ("Rocket: The Singles Collection", "Rocket", Kind::Compilation, false),
        ] {
            assert_eq!(id(name), (base.to_string(), kind, edition), "{name}");
        }
    }

    #[test]
    fn only_the_same_kind_of_the_same_title_is_the_same_album() {
        let studio = classify("Rocket (2017)");
        for same in ["Rocket", "Rocket (Deluxe Edition)", "ROCKET [2011 Remaster]", "Rocket - Single"] {
            assert!(studio.is(same), "{same}");
        }
        for other in ["Rocket (Live)", "Rocket (Remixes)", "Rocket Live", "Rocket (Demos)", "Rocket II", "Rocket (Instrumentals)"] {
            assert!(!studio.is(other), "{other}");
        }
        let live = classify("Rocket (Live at Leeds)");
        assert!(live.is("Rocket (Live)") && live.is("Rocket Live") && live.is("Rocket - Live (Deluxe)"));
        assert!(!live.is("Rocket") && !live.is("Rocket (Remixes)"));
    }

    #[test]
    fn track_titles_can_say_what_the_name_does_not() {
        let t = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let live = t(&["One (Live)", "Two (Live)", "Three - Live at Leeds", "Four (Live)", "Intro"]);
        assert_eq!(classify_with_titles("Rocket (2017)", &live).kind, Kind::Live);
        let remix = t(&["One (X Remix)", "Two (Y Remix)", "Three (Club Mix)", "Four (Z Remix)"]);
        assert_eq!(classify_with_titles("Rocket", &remix).kind, Kind::Remix);
        // a studio album with one live bonus track stays a studio album, and a short list proves nothing
        let studio = t(&["One", "Two", "Three", "Four (Live)"]);
        assert_eq!(classify_with_titles("Rocket", &studio).kind, Kind::Studio);
        assert_eq!(classify_with_titles("Rocket", &t(&["One (Live)", "Two (Live)"])).kind, Kind::Studio);
        // a name that already says its kind keeps it
        assert_eq!(classify_with_titles("Rocket (Remixes)", &live).kind, Kind::Remix);
    }
}
