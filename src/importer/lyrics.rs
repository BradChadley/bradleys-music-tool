//! Lyrics: what a file carries, and what LRCLIB has for a song.
//!
//! A file's lyrics are in one of three places: the real lyrics field (which players read), a timed
//! lyrics frame (`SYLT`, MP3 only), or a generic text tag that only has the field's name (what ffmpeg
//! leaves when it converts a file, and what no player reads: "hidden" here).
//!
//! LRCLIB (lrclib.net) is a free, open lyrics library that needs no key. Only the artist, the title and
//! the length of a song are sent. Every answer is kept on disk, "nothing there" included, so a song is
//! asked about once; requests go out one at a time, in short bursts that stay under the site's limit,
//! and a "slow down" from the site is obeyed.
use crate::importer::id3;
use crate::importer::rules::{self, LyricJunk};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ------------------------------------------------------------------ what a file carries

/// Where a file's lyrics are.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Place {
    #[default]
    None,
    /// in the field players read
    Field,
    /// only as timed lyrics in their own frame
    TimedFrame,
    /// only in a text tag no player reads
    Hidden,
}

/// One file's lyrics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lyric {
    pub place: Place,
    /// the lines carry times
    pub timed: bool,
    /// what the lyrics check said about the words (see `rules::lyrics_junk`)
    pub junk: LyricJunk,
}

impl Lyric {
    /// The file has words a player can show, or says it has none to show.
    pub fn there(&self) -> bool {
        self.place != Place::None && !self.junk.only
    }
    pub fn has_junk(&self) -> bool {
        !self.junk.lines.is_empty()
    }
    /// A song with nothing for a player to show, where LRCLIB may have something.
    pub fn missing(&self) -> bool {
        (self.place == Place::None || self.junk.only) && !self.junk.instrumental
    }
}

const HIDDEN_NAMES: [&str; 4] = ["uslt", "unsyncedlyrics", "lyrics", "unsynced lyrics"];

fn is_hidden_name(desc: &str) -> bool {
    let d = desc.to_lowercase();
    HIDDEN_NAMES.contains(&d.as_str()) || d.starts_with("lyrics-")
}

/// The words in a file's lyrics field and in a tag that hides them, with whether it has a timed frame:
/// (field, hidden, timed frame). `tags` are the file's tags as ffprobe names them, lower case; for an MP3
/// the tag itself is read, because ffprobe cannot tell the field from a tag that has its name.
pub fn words<'a>(path: &Path, tags: impl Iterator<Item = (&'a str, &'a str)>) -> (String, String, bool) {
    let longest = |a: String, b: &str| if b.trim().len() > a.trim().len() { b.to_string() } else { a };
    let is_mp3 = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"));
    if is_mp3 {
        if let Some(frames) = id3::frames(path) {
            let (mut field, mut hidden, mut timed) = (String::new(), String::new(), false);
            for f in &frames {
                match f.id.as_str() {
                    "USLT" => field = longest(field, &f.text),
                    "SYLT" => timed = true,
                    "TXXX" if is_hidden_name(&f.desc) => hidden = longest(hidden, &f.text),
                    _ => {}
                }
            }
            return (field, hidden, timed);
        }
    }
    // other kinds of file keep lyrics under these names, and there they are the real thing
    let mut field = String::new();
    for (k, v) in tags {
        if k == "lyrics" || k == "unsyncedlyrics" || k.starts_with("lyrics-") {
            field = longest(field, v);
        }
    }
    (field, String::new(), false)
}

/// What a file's lyrics are, from its words (see `words`).
pub fn judge(field: &str, hidden: &str, timed_frame: bool) -> Lyric {
    let (place, text) = if !field.trim().is_empty() {
        (Place::Field, field)
    } else if timed_frame {
        (Place::TimedFrame, "")
    } else if !hidden.trim().is_empty() {
        (Place::Hidden, hidden)
    } else {
        (Place::None, "")
    };
    Lyric { place, timed: timed_frame || rules::lyrics_timed(text), junk: rules::lyrics_junk(text) }
}

pub fn of_file<'a>(path: &Path, tags: impl Iterator<Item = (&'a str, &'a str)>) -> Lyric {
    let (field, hidden, timed) = words(path, tags);
    judge(&field, &hidden, timed)
}

// ------------------------------------------------------------------ what is done about them

/// What happens to lyrics that carry junk (a web address, a site's credit, markup).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Choice {
    /// LRCLIB's lyrics take their place; when it has none, the junk lines are cut
    #[default]
    Replace,
    /// only the junk lines are cut
    Cut,
    /// they stay as they are
    Keep,
}

/// What is done to one file's lyrics.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Nothing,
    /// these words go into the lyrics field. `over`: they take the place of what is there (only ever
    /// lyrics that were flagged, and only when that was chosen); otherwise only a file without lyrics gets them
    Set { text: String, over: bool },
    /// the field is emptied: there was nothing in it but junk
    Clear,
}

/// What is done to a file's lyrics: lyrics that are fine are never touched; missing ones are filled from
/// what LRCLIB has; flagged ones go the way that was chosen.
pub fn act(l: &Lyric, found: Option<&Answer>, choice: Choice, want_timed: bool) -> Act {
    let from_lib = found.and_then(|a| a.text(want_timed)).map(|s| s.to_string());
    if l.has_junk() {
        return match (choice, from_lib) {
            (Choice::Keep, _) => Act::Nothing,
            (Choice::Replace, Some(text)) => Act::Set { text, over: true },
            _ if l.junk.only || l.junk.cleaned.trim().is_empty() => Act::Clear,
            _ => Act::Set { text: l.junk.cleaned.clone(), over: true },
        };
    }
    match from_lib {
        Some(text) if l.missing() => Act::Set { text, over: false },
        _ => Act::Nothing,
    }
}

/// Writes what is to be done about a file's lyrics into the tag editor's instructions for it. Lyrics kept
/// in a tag no player reads always go into the field first.
pub fn into_plan(plan: &mut Value, l: &Lyric, act: &Act) {
    if l.place == Place::Hidden {
        plan["Unhide"] = Value::Bool(true);
    }
    match act {
        Act::Nothing => {}
        Act::Set { text, over } => {
            plan["Lyrics"] = Value::String(text.clone());
            plan["LyricsMode"] = Value::String(if *over { "set" } else { "missing" }.into());
        }
        Act::Clear => plan["LyricsMode"] = Value::String("clear".into()),
    }
}

// ------------------------------------------------------------------ LRCLIB

/// What LRCLIB has for a song.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// the words, plain and (when it has them) with a time on every line
    Found { plain: String, timed: String },
    /// it knows the song and says it has no words
    Instrumental,
    /// it has nothing for this song
    Nothing,
}

impl Answer {
    /// The words to save: timed when there are any and they are wanted, else plain.
    pub fn text(&self, want_timed: bool) -> Option<&str> {
        match self {
            Answer::Found { plain, timed } => Some(if want_timed && !timed.is_empty() || plain.is_empty() { timed.as_str() } else { plain.as_str() }).filter(|t| !t.trim().is_empty()),
            _ => None,
        }
    }
    pub fn is_timed(&self, want_timed: bool) -> bool {
        self.text(want_timed).is_some_and(rules::lyrics_timed)
    }
}

/// "Nothing there" is asked again after this long (new songs get their lyrics in time).
const ASK_AGAIN: Duration = Duration::from_secs(180 * 86400);
/// Requests in any one minute, and the least time between two. LRCLIB allows a client that does not
/// name a home page about 45 a minute and asks batch work to leave 200 to 500 ms between requests.
const PER_MINUTE: usize = 36;
const SPACING: Duration = Duration::from_millis(300);
/// A wait the site asks for is slept through when it is this short; a longer one ends the asking for now.
const SHORT_REST: Duration = Duration::from_secs(90);

struct Pace {
    sent: VecDeque<Instant>,
    rest_until: Option<Instant>,
}
static PACE: LazyLock<Mutex<Pace>> = LazyLock::new(|| Mutex::new(Pace { sent: VecDeque::new(), rest_until: None }));

/// How the app asks LRCLIB.
#[derive(Clone)]
pub struct Lookup {
    /// where answers are kept
    pub dir: PathBuf,
    /// false: nothing is asked; what is already kept is still used
    pub online: bool,
    /// save timed lyrics when there are any (else always the plain words)
    pub timed: bool,
    pub agent: ureq::Agent,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The name a song's answer is kept under: artist, title and length to the second, however they are written.
pub fn key(artist: &str, title: &str, secs: f64) -> String {
    let norm = |s: &str| crate::importer::util::norm(s);
    format!("{:x}", md5::compute(format!("{}|{}|{}", norm(artist), norm(title), secs.round() as i64).as_bytes()))
}

impl Lookup {
    /// The app's own: answers in its cache folder (a test run's is its own), the choice from Settings.
    pub fn for_app(online: bool, timed: bool) -> Lookup {
        Lookup { dir: crate::config::cache_dir().join("lyrics"), online, timed, agent: agent() }
    }

    fn file(&self, artist: &str, title: &str, secs: f64) -> PathBuf {
        self.dir.join(format!("{}.json", key(artist, title, secs)))
    }

    /// What is kept for a song, with how long ago it was asked. None when it was never asked.
    pub fn kept(&self, artist: &str, title: &str, secs: f64) -> Option<(Answer, Duration)> {
        let v: Value = serde_json::from_slice(&std::fs::read(self.file(artist, title, secs)).ok()?).ok()?;
        let age = Duration::from_secs(now().saturating_sub(v["at"].as_u64().unwrap_or(0)));
        let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let answer = match v["kind"].as_str()? {
            "found" => Answer::Found { plain: text("plain"), timed: text("timed") },
            "instrumental" => Answer::Instrumental,
            "nothing" => Answer::Nothing,
            _ => return None,
        };
        Some((answer, age))
    }

    /// What is known without asking: a kept answer that is still good ("nothing there" goes stale).
    pub fn known(&self, artist: &str, title: &str, secs: f64) -> Option<Answer> {
        self.kept(artist, title, secs).filter(|(a, age)| *a != Answer::Nothing || *age < ASK_AGAIN).map(|(a, _)| a)
    }

    pub fn keep(&self, artist: &str, title: &str, secs: f64, answer: &Answer) {
        let v = match answer {
            Answer::Found { plain, timed } => serde_json::json!({ "at": now(), "kind": "found", "plain": plain, "timed": timed }),
            Answer::Instrumental => serde_json::json!({ "at": now(), "kind": "instrumental" }),
            Answer::Nothing => serde_json::json!({ "at": now(), "kind": "nothing" }),
        };
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = crate::importer::util::write_atomic(&self.file(artist, title, secs), v.to_string().as_bytes());
    }

    /// What LRCLIB has for a song: from what is kept, else by asking. None means it could not be asked
    /// (offline, switched off, the site is down or asked for a rest, or the song has no artist, title or
    /// length to ask by), which says nothing about the song.
    pub fn find(&self, artist: &str, title: &str, secs: f64) -> Option<Answer> {
        if artist.trim().is_empty() || title.trim().is_empty() || !(1.0..=3600.0).contains(&secs) {
            return None;
        }
        if let Some(a) = self.known(artist, title, secs) {
            return Some(a);
        }
        if !self.online {
            return None;
        }
        let url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}&duration={}",
            crate::importer::lookup::urlenc(artist.trim()),
            crate::importer::lookup::urlenc(title.trim()),
            secs.round() as i64
        );
        let answer = ask(&self.agent, &url)?;
        self.keep(artist, title, secs, &answer);
        Some(answer)
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .https_only(true)
        .user_agent(crate::importer::ctx::USER_AGENT)
        .build()
        .into()
}

/// Waits for this request's turn. False when the site has asked for a rest too long to sit out.
fn turn() -> bool {
    loop {
        let wait = {
            let mut p = PACE.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if let Some(until) = p.rest_until {
                if until > now {
                    if until - now > SHORT_REST {
                        return false;
                    }
                    Some(until - now)
                } else {
                    p.rest_until = None;
                    None
                }
            } else {
                while p.sent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
                    p.sent.pop_front();
                }
                let spaced = p.sent.back().map(|t| (*t + SPACING).saturating_duration_since(now)).filter(|d| !d.is_zero());
                let minute = (p.sent.len() >= PER_MINUTE).then(|| (p.sent[0] + Duration::from_secs(60)).saturating_duration_since(now));
                match spaced.into_iter().chain(minute).max() {
                    Some(d) if !d.is_zero() => Some(d),
                    _ => {
                        p.sent.push_back(now);
                        return true;
                    }
                }
            }
        };
        if let Some(d) = wait {
            std::thread::sleep(d + Duration::from_millis(20));
        }
    }
}

fn rest(for_secs: u64) {
    // (a little longer than asked, as the site suggests)
    let d = Duration::from_secs(for_secs.clamp(5, 3600)).mul_f64(1.2);
    PACE.lock().unwrap_or_else(|e| e.into_inner()).rest_until = Some(Instant::now() + d);
}

/// One request. None when there was no answer to read.
fn ask(agent: &ureq::Agent, url: &str) -> Option<Answer> {
    for attempt in 0..2 {
        if !turn() {
            return None;
        }
        match agent.get(url).call() {
            Ok(mut r) => {
                let code = r.status().as_u16();
                if code == 404 {
                    return Some(Answer::Nothing);
                }
                if code == 429 || code >= 500 {
                    let after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
                    rest(after.unwrap_or(if code == 429 { 60 } else { 300 }));
                    if code == 429 && attempt == 0 {
                        continue; // once more, after the rest it asked for (when that is short)
                    }
                    return None;
                }
                if !(200..300).contains(&code) {
                    return None;
                }
                return r.body_mut().read_json::<Value>().ok().map(|v| read_answer(&v));
            }
            Err(_) => {
                rest(120);
                return None;
            }
        }
    }
    None
}

/// Reads LRCLIB's answer for one song. Its own words go through the lyrics check too.
pub fn read_answer(v: &Value) -> Answer {
    if v["instrumental"].as_bool() == Some(true) {
        return Answer::Instrumental;
    }
    let clean = |k: &str| {
        let text = v[k].as_str().unwrap_or("");
        let j = rules::lyrics_junk(text);
        if j.only || j.instrumental { String::new() } else { j.cleaned }
    };
    let (plain, timed) = (clean("plainLyrics"), clean("syncedLyrics"));
    let timed = if rules::lyrics_timed(&timed) { timed } else { String::new() };
    if plain.is_empty() && timed.is_empty() {
        if rules::lyrics_junk(v["plainLyrics"].as_str().unwrap_or("")).instrumental {
            return Answer::Instrumental;
        }
        return Answer::Nothing;
    }
    Answer::Found { plain, timed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Lookup {
        let dir = std::env::temp_dir().join(format!("music-recs-lyrics-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        Lookup { dir, online: false, timed: true, agent: agent() }
    }

    #[test]
    fn where_the_lyrics_are_decides_what_a_file_has() {
        let words = "first line of the song\nsecond line of the song\nthird line of the song";
        let l = judge(words, "", false);
        assert!(l.place == Place::Field && l.there() && !l.timed && !l.missing());
        // words only in the tag no player reads
        let l = judge("", words, false);
        assert!(l.place == Place::Hidden && !l.missing());
        // timed lyrics in their own frame count, and so do stamps in the words
        assert!(judge("", "", true).place == Place::TimedFrame && judge("", "", true).timed);
        assert!(judge("[00:01.00]a line\n[00:02.00]b line", "", false).timed);
        // nothing, or nothing but an address: missing. "Instrumental": not missing
        assert!(judge("", "", false).missing() && judge("www.t.me/pmedia_music", "", false).missing());
        assert!(!judge("Instrumental", "", false).missing());
        // another kind of file: the names ffprobe gives are the real field
        let tags = [("title", "x"), ("lyrics", words)];
        let (field, hidden, timed) = super::words(Path::new("a.flac"), tags.iter().copied());
        assert!(field == words && hidden.is_empty() && !timed);
    }

    #[test]
    fn good_lyrics_are_never_touched_and_flagged_ones_go_the_way_that_was_chosen() {
        let words = "first line of the song\nsecond line of the song\nthird line of the song";
        let found = Answer::Found { plain: "plain words".into(), timed: "[00:01.00]timed\n[00:02.00]words".into() };
        let set = |text: &str, over: bool| Act::Set { text: text.to_string(), over };
        // fine as they are: nothing, whatever LRCLIB has
        assert_eq!(act(&judge(words, "", false), Some(&found), Choice::Replace, true), Act::Nothing);
        assert_eq!(act(&judge("", "", true), Some(&found), Choice::Replace, true), Act::Nothing);
        assert_eq!(act(&judge("Instrumental", "", false), Some(&found), Choice::Replace, true), Act::Nothing);
        // none: filled, timed or plain as wanted, and only into a file that has none
        assert_eq!(act(&judge("", "", false), Some(&found), Choice::Replace, true), set("[00:01.00]timed\n[00:02.00]words", false));
        assert_eq!(act(&judge("", "", false), Some(&found), Choice::Keep, false), set("plain words", false));
        assert_eq!(act(&judge("", "", false), Some(&Answer::Nothing), Choice::Replace, true), Act::Nothing);
        assert_eq!(act(&judge("", "", false), Some(&Answer::Instrumental), Choice::Replace, true), Act::Nothing);
        assert_eq!(act(&judge("", "", false), None, Choice::Replace, true), Act::Nothing);
        // a credit under real words
        let credit = judge(&format!("{words}\nLyrics provided by SomeSite.com"), "", false);
        assert_eq!(act(&credit, Some(&found), Choice::Replace, false), set("plain words", true));
        assert_eq!(act(&credit, Some(&Answer::Nothing), Choice::Replace, true), set(words, true));
        assert_eq!(act(&credit, Some(&found), Choice::Cut, true), set(words, true));
        assert_eq!(act(&credit, Some(&found), Choice::Keep, true), Act::Nothing);
        // nothing but an address: replaced, or emptied when there is nothing to replace it with
        let only = judge("www.t.me/pmedia_music", "", false);
        assert_eq!(act(&only, Some(&found), Choice::Replace, false), set("plain words", true));
        assert_eq!(act(&only, None, Choice::Replace, true), Act::Clear);
        assert_eq!(act(&only, Some(&found), Choice::Cut, true), Act::Clear);
        assert_eq!(act(&only, Some(&found), Choice::Keep, true), Act::Nothing);
        // the tag editor's instructions
        let mut plan = serde_json::json!({ "File": "x" });
        into_plan(&mut plan, &judge("", words, false), &Act::Nothing);
        assert_eq!(plan, serde_json::json!({ "File": "x", "Unhide": true }));
        let mut plan = serde_json::json!({});
        into_plan(&mut plan, &only, &set("w", true));
        assert_eq!(plan, serde_json::json!({ "Lyrics": "w", "LyricsMode": "set" }));
        let mut plan = serde_json::json!({});
        into_plan(&mut plan, &only, &Act::Clear);
        assert_eq!(plan, serde_json::json!({ "LyricsMode": "clear" }));
    }

    #[test]
    fn answers_are_read_kept_and_used_without_asking_again() {
        let l = lookup("kept");
        let found = read_answer(&serde_json::json!({ "instrumental": false, "plainLyrics": "one line\ntwo line\nthree line", "syncedLyrics": "[00:01.00]one line\n[00:02.00]two line\n[00:03.00]three line" }));
        assert_eq!(found.text(true), Some("[00:01.00]one line\n[00:02.00]two line\n[00:03.00]three line"));
        assert_eq!(found.text(false), Some("one line\ntwo line\nthree line"));
        assert!(found.is_timed(true) && !found.is_timed(false));
        // only plain words: those, whatever is wanted
        let plain = read_answer(&serde_json::json!({ "plainLyrics": "one line\ntwo line", "syncedLyrics": null }));
        assert_eq!(plain.text(true), Some("one line\ntwo line"));
        assert_eq!(read_answer(&serde_json::json!({ "instrumental": true, "plainLyrics": null })), Answer::Instrumental);
        assert_eq!(read_answer(&serde_json::json!({ "plainLyrics": "", "syncedLyrics": "" })), Answer::Nothing);
        // LRCLIB's own words go through the check: a credit line is not saved
        let credit = read_answer(&serde_json::json!({ "plainLyrics": "one line\ntwo line\nLyrics provided by SomeSite.com" }));
        assert_eq!(credit.text(true), Some("one line\ntwo line"));

        // nothing kept and offline: could not be asked
        assert_eq!(l.find("Artist", "Song", 200.4), None);
        // kept: found without asking, however the names are written, to the second
        l.keep("Artist", "Song", 200.4, &found);
        assert_eq!(l.find("ARTIST", "song!", 200.0), Some(found.clone()));
        assert_eq!(l.find("Artist", "Song", 203.0), None);
        l.keep("Artist", "Quiet", 100.0, &Answer::Instrumental);
        l.keep("Artist", "Gone", 100.0, &Answer::Nothing);
        assert_eq!(l.find("Artist", "Quiet", 100.0), Some(Answer::Instrumental));
        assert_eq!(l.find("Artist", "Gone", 100.0), Some(Answer::Nothing));
        // a song with no artist, title or length is never asked about
        assert_eq!(l.find("", "Song", 200.0), None);
        assert_eq!(l.find("Artist", "Song", 0.0), None);
        let _ = std::fs::remove_dir_all(&l.dir);
    }
}
