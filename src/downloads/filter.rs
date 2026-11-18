//! The box over a search's copies that is typed in: what the words mean, and which copies they keep.
//! Nothing is searched again; the copies that are already known are narrowed.
//!
//! What can be typed, each part separated by a space, and every part has to hold:
//!   soundtrack 2010        a word: in the folder's name, the person's name or a track's name. It has to
//!                          begin a word there ("live" is not in "Oliver"); capitals, accents and
//!                          punctuation do not count
//!   "original score"       in quotes: these words together, in this order. Quotes also make a word that
//!                          would otherwise be a fact ("free", "flac") a plain word
//!   -game                  a minus leaves out what follows. A word to leave out is looked for in the
//!                          folder's name only: an album is not live because one of its songs is "Live Wire"
//!   flac mp3 m4a ogg ...   the kind of file, by the FORMAT column and not by the folder's name (a folder
//!                          named "[FLAC]" that holds MP3s is not FLAC)
//!   lossless lossy         the same, for all kinds of either sort
//!   320 256 192 ...        the rate in the FORMAT column
//!   v0 v2                  a variable-rate MP3 of that grade: the folder says so, or the rate is one such
//!                          a file has
//!   >500mb <1gb            by size
//!   19 tracks, >12 tracks  by the number of tracks
//!   free                   only people with no wait
use crate::slskd::Candidate;

#[derive(Debug, PartialEq, Clone, Copy)]
enum Cmp {
    Less,
    AtMost,
    Same,
    AtLeast,
    More,
}

impl Cmp {
    /// Reads "<", "<=", "=", ">=" or ">" off the front of a word; a word without one means "exactly".
    fn off(word: &str) -> (Cmp, &str, bool) {
        for (sign, cmp) in [("<=", Cmp::AtMost), (">=", Cmp::AtLeast), ("<", Cmp::Less), (">", Cmp::More), ("=", Cmp::Same)] {
            if let Some(rest) = word.strip_prefix(sign) {
                return (cmp, rest, true);
            }
        }
        (Cmp::Same, word, false)
    }

    fn holds(self, have: u64, want: u64) -> bool {
        match self {
            Cmp::Less => have < want,
            Cmp::AtMost => have <= want,
            Cmp::Same => have == want,
            Cmp::AtLeast => have >= want,
            Cmp::More => have > want,
        }
    }
}

#[derive(Debug, PartialEq)]
enum What {
    /// words that must stand somewhere, folded (see `fold`)
    Words(String),
    Lossless(bool),
    /// the kinds of file that count ("FLAC"; "AAC" and "M4A" for aac)
    Kind(&'static [&'static str]),
    Rate(u64),
    /// a variable-rate MP3 of this grade: its name in a folder ("v0"), and the rates such files average
    Grade(&'static str, std::ops::RangeInclusive<u64>, &'static [u64]),
    Bytes(Cmp, u64),
    Tracks(Cmp, u64),
    Free,
}

#[derive(Debug, PartialEq)]
struct Term {
    not: bool,
    what: What,
}

/// What was typed, read once.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Filter {
    terms: Vec<Term>,
}

/// Lower case, accents folded, every run of anything but letters and digits made one space: the words of
/// a name, as they are compared. "Scott Pilgrim vs. the World" is "scott pilgrim vs the world".
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for part in text.split(|c: char| !c.is_alphanumeric()) {
        let word = crate::util::norm(part);
        if !word.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&word);
        }
    }
    out
}

/// True when folded `words` stand in folded `hay`, beginning at the start of a word.
fn has(hay: &str, words: &str) -> bool {
    hay.match_indices(words).any(|(i, _)| i == 0 || hay.as_bytes()[i - 1] == b' ')
}

/// The parts that were typed: (left out, in quotes, the text).
fn parts(typed: &str) -> Vec<(bool, bool, String)> {
    let mut out = vec![];
    let mut chars = typed.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let not = first == '-';
        if not {
            chars.next();
        }
        let mut text = String::new();
        let quoted = chars.peek() == Some(&'"');
        if quoted {
            chars.next();
            // (a quote that is not closed yet, while typing, takes the rest)
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                text.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                text.push(c);
                chars.next();
            }
        }
        // (a dash that stands alone, as in "Artist - Album", says nothing)
        if !text.trim().is_empty() {
            out.push((not, quoted, text));
        }
    }
    out
}

/// A size such as ">500mb" or "<1.5gb", in bytes (as the SIZE column counts: 1024 to the next unit).
fn bytes(word: &str) -> Option<(Cmp, u64)> {
    let (cmp, rest, signed) = Cmp::off(word);
    let (number, unit) = [("kb", 1024.0f64), ("mb", 1_048_576.0), ("gb", 1_073_741_824.0)].iter().find_map(|(u, by)| rest.strip_suffix(u).map(|n| (n, *by)))?;
    let n: f64 = number.parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
    // (a size with no sign in front is a word like any other: nobody means "exactly 500 MB")
    signed.then_some((cmp, (n * unit) as u64))
}

/// A whole number with an optional sign in front: "19", ">12".
fn count(word: &str) -> Option<(Cmp, u64)> {
    let (cmp, rest, _) = Cmp::off(word);
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| rest.parse().ok()).flatten().map(|n| (cmp, n))
}

const TRACK_WORDS: [&str; 4] = ["tracks", "track", "songs", "song"];

impl Filter {
    pub fn read(typed: &str) -> Filter {
        let parts = parts(typed);
        let mut terms = vec![];
        let mut i = 0;
        while i < parts.len() {
            let (not, quoted, text) = &parts[i];
            i += 1;
            let low = text.to_lowercase();
            let tracks_next = parts.get(i).is_some_and(|(not, quoted, word)| !not && !quoted && TRACK_WORDS.contains(&word.to_lowercase().as_str()));
            let what = if *quoted {
                What::Words(fold(text))
            } else if let (Some((cmp, n)), true) = (count(&low), tracks_next) {
                // "19 tracks": the number and the word after it are one part
                i += 1;
                What::Tracks(cmp, n)
            } else if let Some((cmp, n)) = TRACK_WORDS.iter().find_map(|w| low.strip_suffix(w)).and_then(count) {
                What::Tracks(cmp, n)
            } else if let Some((cmp, n)) = bytes(&low) {
                What::Bytes(cmp, n)
            } else {
                match low.as_str() {
                    "lossless" => What::Lossless(true),
                    "lossy" => What::Lossless(false),
                    "free" => What::Free,
                    "v0" => What::Grade("v0", 200..=290, &[224, 256]),
                    "v2" => What::Grade("v2", 150..=215, &[160, 192]),
                    "flac" => What::Kind(&["FLAC"]),
                    "mp3" => What::Kind(&["MP3"]),
                    "m4a" => What::Kind(&["M4A"]),
                    "aac" => What::Kind(&["AAC", "M4A"]),
                    "ogg" => What::Kind(&["OGG"]),
                    "opus" => What::Kind(&["OPUS"]),
                    "wav" => What::Kind(&["WAV"]),
                    "ape" => What::Kind(&["APE"]),
                    "wma" => What::Kind(&["WMA"]),
                    "320" | "256" | "224" | "192" | "160" | "128" => What::Rate(low.parse().unwrap_or(0)),
                    _ => What::Words(fold(text)),
                }
            };
            // (a part with no letter or digit in it, "!!!", would be in every name)
            if what != What::Words(String::new()) {
                terms.push(Term { not: *not, what });
            }
        }
        Filter { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// None when the copy is left out. Otherwise what the row should say about why it is there: the name
    /// of a track, when a word that was typed is in that track's name and nowhere the row shows; empty when
    /// the row itself shows why.
    pub fn keeps(&self, c: &Candidate) -> Option<String> {
        let (kind, rate) = match c.format.split_once(' ') {
            Some((kind, rate)) => (kind, rate.parse::<u64>().unwrap_or(0)),
            None => (c.format.as_str(), 0),
        };
        let name = fold(&c.name);
        let mut user: Option<String> = None;
        let mut tracks: Option<Vec<(String, &str)>> = None;
        // the words that are in a track's name only
        let mut inside: Vec<&str> = vec![];
        for term in &self.terms {
            let holds = match &term.what {
                What::Words(words) => {
                    if has(&name, words) {
                        true
                    } else if term.not {
                        false
                    } else if has(user.get_or_insert_with(|| fold(&c.user)).as_str(), words) {
                        true
                    } else {
                        let tracks = tracks.get_or_insert_with(|| c.files.iter().filter_map(|f| track_name(&f.filename)).map(|shown| (fold(shown), shown)).collect());
                        let found = tracks.iter().any(|(folded, _)| has(folded, words));
                        if found {
                            inside.push(words.as_str());
                        }
                        found
                    }
                }
                What::Lossless(want) => super::is_lossless(&c.format) == *want,
                What::Kind(kinds) => kinds.iter().any(|k| kind.eq_ignore_ascii_case(k)),
                What::Rate(want) => rate == *want,
                What::Grade(word, rates, fixed) => has(&name, word) || (kind.eq_ignore_ascii_case("MP3") && rates.contains(&rate) && !fixed.contains(&rate)),
                What::Bytes(cmp, want) => cmp.holds(c.bytes, *want),
                What::Tracks(cmp, want) => cmp.holds(c.tracks as u64, *want),
                What::Free => c.free_slot,
            };
            if holds == term.not {
                return None;
            }
        }
        // the track that says most about why: one with every such word in its name, or the first with one
        let tracks = tracks.unwrap_or_default();
        let why = tracks.iter().find(|(folded, _)| !inside.is_empty() && inside.iter().all(|w| has(folded, w))).or_else(|| tracks.iter().find(|(folded, _)| inside.iter().any(|w| has(folded, w))));
        Some(why.map(|(_, shown)| cut(shown, 38)).unwrap_or_default())
    }
}

/// A music file's own name without its folders and its ending: "07 Black Sheep".
fn track_name(path: &str) -> Option<&str> {
    let leaf = path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next()?;
    let (name, ext) = leaf.rsplit_once('.')?;
    (ext.is_ascii() && crate::slskd::is_audio_ext(&ext.to_ascii_lowercase())).then_some(name)
}

/// The text, cut to `most` letters with "…" at its end when it is longer.
fn cut(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(most - 1).collect::<String>().trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slskd::RemoteFile;

    fn copy(user: &str, name: &str, format: &str, files: &[&str], bytes: u64, free: bool) -> Candidate {
        let files: Vec<RemoteFile> = files.iter().map(|f| RemoteFile { filename: format!("@@x\\Music\\{name}\\{f}"), size: 1, bitrate: 0, secs: 0 }).collect();
        let tracks = files.iter().filter(|f| track_name(&f.filename).is_some()).count();
        Candidate { user: user.into(), name: name.into(), files, tracks, bytes, format: format.into(), free_slot: free, speed: 1, queue: if free { 0 } else { 3 }, score: 0 }
    }

    const MB: u64 = 1_048_576;

    fn copies() -> Vec<Candidate> {
        vec![
            copy("tapehiss", "Scott Pilgrim vs. the World - The Game - Original Videogame Soundtrack", "FLAC", &["01 Scott Pilgrim Anthem.flac", "02 Another Winter.flac", "cover.jpg"], 968 * MB, true),
            copy("vinylcrate", "Scott Pilgrim vs. the World (Original Motion Picture Soundtrack) [2010]", "FLAC", &["01 We Are Sex Bob-omb.flac", "07 Black Sheep.flac", "12 Garbage Truck.flac"], 412 * MB, true),
            copy("quietstorm", "Nigel Godrich - Scott Pilgrim vs. the World (Original Score)", "MP3 320", &["01 Universal Theme.mp3", "02 Hillcrest Park.mp3"], 148 * MB, false),
            copy("mapleroad", "Scott Pilgrim OST [FLAC]", "MP3 245", &["07 Black Sheep (Brie Larson Vocal Version).mp3", "08 Live Wire.mp3"], 121 * MB, true),
            copy("oliver", "Beyoncé – Live at Wembley", "MP3 192", &["01 Crazy in Love.mp3"], 90 * MB, false),
        ]
    }

    /// The people whose copies the typed words keep, in order.
    fn kept(typed: &str) -> Vec<String> {
        let filter = Filter::read(typed);
        copies().into_iter().filter(|c| filter.keeps(c).is_some()).map(|c| c.user).collect()
    }

    #[test]
    fn what_is_typed_is_split_into_its_parts() {
        let read = |t: &str| parts(t).into_iter().map(|(not, quoted, text)| format!("{}{}{text}", if not { "-" } else { "" }, if quoted { "q:" } else { "" })).collect::<Vec<_>>().join("|");
        assert_eq!(read("  soundtrack   2010 "), "soundtrack|2010");
        assert_eq!(read("-game \"original score\" -\"live at\""), "-game|q:original score|-q:live at");
        // a dash alone says nothing; a dash inside a word stays; a quote still being typed takes the rest
        assert_eq!(read("godrich - score blink-182 \"sex bob"), "godrich|score|blink-182|q:sex bob");
        assert_eq!(read(""), "");
        assert!(Filter::read("  -  !!! ").is_empty());
        assert!(!Filter::read("x").is_empty());
    }

    #[test]
    fn names_are_compared_by_their_words() {
        assert_eq!(fold("Scott Pilgrim vs. the World [2010]"), "scott pilgrim vs the world 2010");
        assert_eq!(fold("Beyoncé – Live at Wembley"), "beyonce live at wembley");
        assert!(has("beyonce live at wembley", "live") && has("beyonce live at wembley", "wem") && has("beyonce live at wembley", "live at"));
        assert!(!has("oliver tree alive", "live") && !has("beyonce live at wembley", "at live"));
    }

    #[test]
    fn words_must_all_be_there_and_a_minus_leaves_out() {
        assert_eq!(kept(""), ["tapehiss", "vinylcrate", "quietstorm", "mapleroad", "oliver"]);
        assert_eq!(kept("scott"), ["tapehiss", "vinylcrate", "quietstorm", "mapleroad"]);
        assert_eq!(kept("PILGRIM soundtr"), ["tapehiss", "vinylcrate"], "capitals do not count, and a word may still be half typed");
        assert_eq!(kept("scott -game"), ["vinylcrate", "quietstorm", "mapleroad"]);
        assert_eq!(kept("\"original score\""), ["quietstorm"]);
        assert_eq!(kept("\"score original\""), [] as [&str; 0]);
        assert_eq!(kept("beyonce"), ["oliver"], "accents do not count");
        // the person's name counts too
        assert_eq!(kept("vinylcrate"), ["vinylcrate"]);
        // "live" begins no word of "oliver"; and a word to leave out is looked for in the folder's name only,
        // so the copy with a song called "Live Wire" stays
        assert_eq!(kept("live"), ["mapleroad", "oliver"]);
        assert_eq!(kept("-live"), ["tapehiss", "vinylcrate", "quietstorm", "mapleroad"]);
        assert_eq!(kept("-\"live at\" -score"), ["tapehiss", "vinylcrate", "mapleroad"]);
    }

    #[test]
    fn a_word_found_only_in_a_track_names_that_track() {
        let all = copies();
        let why = |typed: &str, at: usize| Filter::read(typed).keeps(&all[at]);
        assert_eq!(kept("black sheep"), ["vinylcrate", "mapleroad"]);
        assert_eq!(why("black sheep", 1), Some("07 Black Sheep".to_string()));
        // a long name is cut
        assert_eq!(why("black sheep", 3), Some("07 Black Sheep (Brie Larson Vocal Ver…".to_string()));
        // a word the folder's name or the person's name shows needs no saying
        assert_eq!(why("soundtrack", 1), Some(String::new()));
        assert_eq!(why("vinylcrate", 1), Some(String::new()));
        // two words in two tracks: the first track that has one
        assert_eq!(why("garbage sheep", 1), Some("07 Black Sheep".to_string()));
        // a picture is not a track
        assert_eq!(kept("cover"), [] as [&str; 0]);
        assert_eq!(track_name("a\\b\\07 Black Sheep.FLAC"), Some("07 Black Sheep"));
        assert_eq!(track_name("a\\b\\cover.jpg"), None);
    }

    #[test]
    fn a_formats_name_goes_by_the_format_column() {
        // the folder named "[FLAC]" that holds MP3s is not FLAC
        assert_eq!(kept("flac"), ["tapehiss", "vinylcrate"]);
        assert_eq!(kept("mp3"), ["quietstorm", "mapleroad", "oliver"]);
        assert_eq!(kept("lossless -game"), ["vinylcrate"]);
        assert_eq!(kept("lossy"), ["quietstorm", "mapleroad", "oliver"]);
        assert_eq!(kept("320"), ["quietstorm"]);
        assert_eq!(kept("-flac -192"), ["quietstorm", "mapleroad"]);
        // a variable-rate MP3: by its rate (245 is none of the fixed ones)
        assert_eq!(kept("v0"), ["mapleroad"]);
        // in quotes it is the word itself, wherever it stands
        assert_eq!(kept("\"flac\""), ["mapleroad"]);
        // a year is a word, not a rate
        assert_eq!(kept("2010"), ["vinylcrate"]);
    }

    #[test]
    fn sizes_tracks_and_the_wait() {
        assert_eq!(kept(">400mb"), ["tapehiss", "vinylcrate"]);
        assert_eq!(kept("<0.2gb"), ["quietstorm", "mapleroad", "oliver"]);
        assert_eq!(kept(">400mb <900mb"), ["vinylcrate"]);
        assert_eq!(kept("3 tracks"), ["vinylcrate"]);
        assert_eq!(kept(">1 tracks"), ["tapehiss", "vinylcrate", "quietstorm", "mapleroad"]);
        assert_eq!(kept("<=1tracks"), ["oliver"]);
        assert_eq!(kept("2 songs free"), ["tapehiss", "mapleroad"]);
        assert_eq!(kept("-free"), ["quietstorm", "oliver"]);
        // a size without a sign, and a number without "tracks", are words like any other
        assert_eq!(Filter::read("500mb 19").terms, vec![Term { not: false, what: What::Words("500mb".into()) }, Term { not: false, what: What::Words("19".into()) }]);
        assert_eq!(Filter::read("19 tracks >1.5gb").terms, vec![Term { not: false, what: What::Tracks(Cmp::Same, 19) }, Term { not: false, what: What::Bytes(Cmp::More, 1_610_612_736) }]);
    }
}
