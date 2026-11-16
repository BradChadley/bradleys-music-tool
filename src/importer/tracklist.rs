//! Comparing a download with the album's official track list (port of LibraryRepair.ps1: Norm-Title, Get-Similarity,
//! Get-TitleRelation, Resolve-AlbumTracklist, Find-RealTrack, Get-MusicBrainzTracklist). Same rules as Library Health.
use crate::importer::ctx::Ctx;
use crate::importer::lookup::{self, urlenc};
use crate::importer::rules::short;
use crate::importer::util::{format_time, norm, strip_year};
use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::LazyLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

#[derive(Clone, Debug)]
pub struct RealTrack {
    pub d: u32,
    pub n: u32,
    pub t: String,
    pub s: f64,
}
#[derive(Clone, Debug)]
pub struct RealList {
    pub source: String,
    pub tracks: Vec<RealTrack>,
}

re!(BRACKETS, r"\s*[\(\[{][^\)\]}]*[\)\]}]");
re!(REMASTER_TAIL, r"(?i)\s+-\s+(\d{4}\s+)?(remaster(ed)?|mono|stereo|single version|album version|radio edit).*$");
re!(FEAT_TAIL, r"(?i)\s+(feat|ft|featuring)\.?\s.*$");
re!(PART, r"(?i)\bpart\b");
re!(ROMAN, r"(?i)\b(pt\.?)\s*(x|ix|viii|vii|vi|iv|v|iii|ii|i)\b");
re!(FEAT_BRACKET, r"(?i)\s*[\(\[](feat|ft|with)\.?[^\)\]]*[\)\]]");
re!(OPTIONAL, r"(?i)\b(bonus|live|demo|remix|mix\)|instrumental|acoustic|a ?cappella|alternate|alternative|version|edit\)|reprise|commentary|interview|karaoke|hidden|blank|silence|untitled)\b|^\W*$");
re!(PLACEHOLDER, r"(?i)^(track|piste|titel|pista|faixa)?\s*\d{1,3}$|^(untitled|unknown|audio ?track|title)\s*\d*$");
re!(STARS, r"\*+");

/// Titles compared without case, punctuation, anything in brackets, "feat." tails or remaster suffixes;
/// "Part II" = "Pt. 2"; "&" = "and".
pub fn norm_title(t: &str) -> String {
    let t = BRACKETS.replace_all(t, " ");
    let t = REMASTER_TAIL.replace(&t, "");
    let t = FEAT_TAIL.replace(&t, "");
    let t = PART.replace_all(&t, "pt");
    let t = t.replace('&', " and ");
    let t = ROMAN.replace_all(&t, |c: &Captures| {
        let n = match c[2].to_lowercase().as_str() {
            "i" => 1, "ii" => 2, "iii" => 3, "iv" => 4, "v" => 5, "vi" => 6, "vii" => 7, "viii" => 8, "ix" => 9, _ => 10,
        };
        format!("{} {}", &c[1], n)
    });
    norm(&t)
}

/// 1.0 = identical; edit distance where swapping two letters counts once
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..=n {
        d[i][0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    1.0 - d[n][m] as f64 / n.max(m) as f64
}

fn strip_article(s: &str) -> String {
    for a in ["the", "a", "an"] {
        if let Some(rest) = s.strip_prefix(a) {
            if rest.chars().count() >= 3 {
                return rest.to_string();
            }
        }
    }
    s.to_string()
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Rel {
    Same,
    Variant,
    Typo,
    Different,
}

/// how two titles relate: the same, the same song named a bit differently, a likely misspelling, or different
pub fn title_relation(ours: &str, official: &str) -> Rel {
    let (a, b) = (norm_title(ours), norm_title(official));
    // censored official titles ("F******", "S**t"): the stars stand for any letters
    if official.contains('*') {
        let pieces: Vec<String> = STARS.split(&BRACKETS.replace_all(official, " ")).map(|p| regex::escape(&norm(p))).collect();
        if let Ok(rx) = Regex::new(&format!("^{}$", pieces.join(r"\w*"))) {
            if rx.is_match(&a) {
                return Rel::Same;
            }
        }
    }
    // with the bracketed part kept, one may contain the other ("Intro" / "Phobia (Intro)")
    let (fa, fb) = (norm(ours), norm(official));
    if !fa.is_empty() && !fb.is_empty() && fa != fb && (fa.contains(&fb) || fb.contains(&fa)) && fa.chars().count().min(fb.chars().count()) >= 4 {
        return Rel::Variant;
    }
    if a.is_empty() || b.is_empty() {
        return Rel::Different;
    }
    if a == b {
        return Rel::Same;
    }
    let (la, lb) = (a.chars().count(), b.chars().count());
    let (sh, lo) = (la.min(lb), la.max(lb));
    if (a.contains(&b) || b.contains(&a)) && sh >= 4 && sh as f64 / lo as f64 >= 0.5 {
        return Rel::Variant; // "My Own Bare Hands" / "With My Own Bare Hands"
    }
    if strip_article(&a) == strip_article(&b) {
        return Rel::Variant; // "The Modern Things" / "Modern Things"
    }
    if sh >= 4 && similarity(&a, &b) >= 0.8 {
        return Rel::Typo;
    }
    Rel::Different
}

fn no_feat(s: &str) -> String {
    norm(&FEAT_BRACKET.replace_all(s, ""))
}

/// the real track for one of ours: same title first (ignoring "feat." bits), then the same disc/number
pub fn find_real_track<'a>(list: &'a [RealTrack], disc: u32, no: u32, title: &str) -> Option<&'a RealTrack> {
    let nt = no_feat(title);
    let by_title: Vec<&RealTrack> = list.iter().filter(|r| !nt.is_empty() && no_feat(&r.t) == nt).collect();
    if by_title.len() == 1 {
        return Some(by_title[0]);
    }
    let mut ds: Vec<u32> = list.iter().map(|r| r.d).collect();
    ds.sort();
    ds.dedup();
    let by_no: Vec<&RealTrack> = list.iter().filter(|r| r.n == no && (ds.len() <= 1 || r.d == disc.max(1))).collect();
    if no > 0 && by_no.len() == 1 {
        return Some(by_no[0]);
    }
    by_title.first().copied()
}

/// One of our tracks, as the comparison sees it.
pub struct OurTrack {
    pub file: String,
    pub title: String,
    pub disc: u32,
    pub no: u32,
    pub secs: f64,
    pub errors: u32,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub category: &'static str,
    pub severity: u8,
    pub detail: String,
    /// for "Wrong track numbers or titles": (file, new title, new track no, new disc no), "" = unchanged
    pub changes: Vec<(String, String, String, String)>,
}

#[derive(Default, Debug)]
pub struct Fit {
    pub fit: bool,
    pub matched: usize,
    pub total: usize,
}

fn leaf(p: &str) -> String {
    p.rsplit(['\\', '/']).next().unwrap_or(p).to_string()
}
fn stem(p: &str) -> String {
    crate::importer::util::file_stem(&leaf(p))
}
fn more(list: &[String], n: usize) -> String {
    let head = list.iter().take(n).cloned().collect::<Vec<_>>().join("; ");
    if list.len() > n { format!("{head}; and {} more", list.len() - n) } else { head }
}

/// Compares one album with its official track list: wrong numbers/titles, swapped files, odd lengths, missing songs.
/// Returns nothing (and fit = false) when the official list is a poor fit for this copy.
pub fn resolve(ours_in: &[OurTrack], real: &RealList, fit: &mut Fit) -> Vec<Finding> {
    *fit = Fit::default();
    let ours = ours_in;
    let rl: Vec<&RealTrack> = real.tracks.iter().filter(|r| r.s >= 5.0).collect(); // (tracks with no length can't be compared)
    if ours.is_empty() || rl.is_empty() || (ours.len() as f64) < rl.len() as f64 * 0.5 {
        return vec![];
    }
    let mut used = vec![false; rl.len()];
    let mut real_of: Vec<Option<usize>> = vec![None; ours.len()];
    let mut how: Vec<&'static str> = vec![""; ours.len()];
    let mut ds: Vec<u32> = rl.iter().map(|r| r.d).collect();
    ds.sort();
    ds.dedup();
    let multi = ds.len() > 1;
    let order_by_pos = |c: Vec<usize>, no: u32| -> Vec<usize> {
        let mut a: Vec<usize> = c.iter().cloned().filter(|&i| rl[i].n == no).collect();
        a.extend(c.iter().cloned().filter(|&i| rl[i].n != no));
        a
    };
    // 1) the exact title (brackets kept), then the same title ignoring brackets/"feat.", then a variant of it
    for pass in 0..3 {
        for o in 0..ours.len() {
            if real_of[o].is_some() || ours[o].title.is_empty() {
                continue;
            }
            let x = no_feat(&ours[o].title);
            let c: Vec<usize> = (0..rl.len())
                .filter(|&i| {
                    !used[i]
                        && match pass {
                            0 => !x.is_empty() && no_feat(&rl[i].t) == x,
                            1 => title_relation(&ours[o].title, &rl[i].t) == Rel::Same,
                            _ => title_relation(&ours[o].title, &rl[i].t) == Rel::Variant,
                        }
                })
                .collect();
            let c = if c.len() > 1 { order_by_pos(c, ours[o].no) } else { c };
            if let Some(&i) = c.first() {
                real_of[o] = Some(i);
                used[i] = true;
                how[o] = "title";
            }
        }
    }
    // 2) a likely misspelling of the official title
    for o in 0..ours.len() {
        if real_of[o].is_some() || ours[o].title.is_empty() || PLACEHOLDER.is_match(&ours[o].title) {
            continue;
        }
        let nt = norm_title(&ours[o].title);
        let mut c: Vec<usize> = (0..rl.len()).filter(|&i| !used[i] && title_relation(&ours[o].title, &rl[i].t) == Rel::Typo).collect();
        c.sort_by(|&a, &b| similarity(&nt, &norm_title(&rl[b].t)).total_cmp(&similarity(&nt, &norm_title(&rl[a].t))));
        if let Some(&i) = c.first() {
            real_of[o] = Some(i);
            used[i] = true;
            // suggest the official spelling only when it's a clear improvement (never a censored "F***" title)
            how[o] = if rl[i].t.contains('*') { "title" } else { "typo" };
        }
    }
    // 3) no usable title: same position and about the same length
    for o in 0..ours.len() {
        if real_of[o].is_some() || ours[o].no == 0 {
            continue;
        }
        let r = (0..rl.len()).find(|&i| !used[i] && rl[i].n == ours[o].no && (!multi || rl[i].d == ours[o].disc.max(1)));
        if let Some(i) = r {
            if (rl[i].s - ours[o].secs).abs() <= 5.0 && (ours[o].title.is_empty() || PLACEHOLDER.is_match(&ours[o].title)) {
                real_of[o] = Some(i);
                used[i] = true;
                how[o] = "position";
            }
        }
    }
    // 4) the same length and a loosely related or placeholder name ("Get Your Shit Together" / "Get Ya S**t Together")
    for o in 0..ours.len() {
        if real_of[o].is_some() || ours[o].secs <= 0.0 {
            continue;
        }
        let t = &ours[o].title;
        let nt = norm_title(t);
        let c = (0..rl.len()).find(|&i| {
            !used[i]
                && (rl[i].s - ours[o].secs).abs() <= 2.0
                && (t.is_empty() || PLACEHOLDER.is_match(t) || nt.is_empty() || rl[i].t.contains('*') || similarity(&nt, &norm_title(&rl[i].t)) >= 0.45)
        });
        if let Some(i) = c {
            real_of[o] = Some(i);
            used[i] = true;
            how[o] = "title";
        }
    }
    // 4b) any name, but the same length to the second: the same song under another title ("B2B" / "Back 2 Back").
    //     If the file name has the official title but the tag doesn't, the tag is wrong and the official title is offered.
    for o in 0..ours.len() {
        if real_of[o].is_some() || ours[o].secs <= 0.0 {
            continue;
        }
        let c: Vec<usize> = (0..rl.len()).filter(|&i| !used[i] && (rl[i].s - ours[o].secs).abs() <= 1.5).collect();
        let Some(&i) = order_by_pos(c, ours[o].no).first() else { continue };
        real_of[o] = Some(i);
        used[i] = true;
        let (nf, nt) = (norm_title(&stem(&ours[o].file)), norm_title(&rl[i].t));
        how[o] = if nt.chars().count() >= 4 && nf.contains(&nt) && !nf.contains(&norm_title(&ours[o].title)) && !rl[i].t.contains('*') { "typo" } else { "title" };
    }
    let matched: Vec<usize> = (0..ours.len()).filter(|&o| real_of[o].is_some()).collect();
    if (matched.len() as f64) < (2.0f64).max(ours.len() as f64 * 0.6) {
        return vec![]; // official list doesn't fit this copy: say nothing
    }
    *fit = Fit { fit: true, matched: matched.len(), total: rl.len() };
    let src = real.source.as_str();
    let mut out: Vec<Finding> = vec![];

    // --- wrong track numbers and titles. Numbers only where ours are missing or used twice: a different numbering is
    //     usually just a different edition, not a mistake. ---
    let pos_key = |o: usize| format!("{}-{}", ours[o].disc.max(1), ours[o].no);
    let dup: Vec<bool> = (0..ours.len()).map(|o| ours[o].no > 0 && (0..ours.len()).filter(|&p| ours[p].no > 0 && pos_key(p) == pos_key(o)).count() > 1).collect();
    let mut our_discs: Vec<u32> = ours.iter().map(|o| o.disc.max(1)).collect();
    our_discs.sort();
    our_discs.dedup();
    let mut changes: Vec<(String, String, String, String)> = vec![];
    let mut lines: Vec<String> = vec![];
    for &o in &matched {
        let r = rl[real_of[o].unwrap()];
        let set_title = if (how[o] == "typo" || how[o] == "position") && r.t != ours[o].title { r.t.clone() } else { String::new() };
        let set_no = if ours[o].no == 0 || (dup[o] && ours[o].no != r.n) { r.n.to_string() } else { String::new() };
        let set_disc = if multi && our_discs.len() > 1 && ours[o].disc == 0 && r.d > 0 { r.d.to_string() } else { String::new() };
        if set_title.is_empty() && set_no.is_empty() && set_disc.is_empty() {
            continue;
        }
        let mut bits = vec![];
        if !set_no.is_empty() {
            bits.push(format!("track {} -> {set_no}", if ours[o].no > 0 { ours[o].no.to_string() } else { "(none)".into() }));
        }
        if !set_disc.is_empty() {
            bits.push(format!("disc {} -> {set_disc}", if ours[o].disc > 0 { ours[o].disc.to_string() } else { "(none)".into() }));
        }
        if !set_title.is_empty() {
            bits.push(format!("title '{}' -> '{}'", short(&ours[o].title, 30), short(&set_title, 30)));
        }
        lines.push(format!("{}: {}", leaf(&ours[o].file), bits.join(", ")));
        changes.push((ours[o].file.clone(), set_title, set_no, set_disc));
    }

    // --- wrong or swapped songs (by length). Only a mutual swap counts (A plays B's length and B plays A's): one track
    //     merely matching another's length was nearly always a coincidence or an official list with shuffled titles. ---
    let mut off: Vec<usize> = matched
        .iter()
        .cloned()
        .filter(|&o| ours[o].secs > 0.0 && ours[o].errors == 0 && {
            let r = rl[real_of[o].unwrap()];
            (ours[o].secs - r.s).abs() > (20.0f64).max(r.s * 0.12)
        })
        .collect();
    // the tag is wrong but the file name is right ("08 Bitch.mp3" tagged 'Cum on Everybody', 0:18 like the real 'Bitch')
    for o in off.clone() {
        let nf = norm_title(&stem(&ours[o].file));
        let r2 = (0..rl.len()).find(|&i| !used[i] && (rl[i].s - ours[o].secs).abs() <= 3.0 && norm_title(&rl[i].t).chars().count() >= 3 && nf.ends_with(&norm_title(&rl[i].t)));
        let Some(i) = r2 else { continue };
        used[real_of[o].unwrap()] = false;
        real_of[o] = Some(i);
        used[i] = true;
        off.retain(|&x| x != o);
        let lf = leaf(&ours[o].file);
        changes.retain(|c| c.0 != ours[o].file);
        lines.retain(|l| !l.starts_with(&format!("{lf}:")));
        changes.push((ours[o].file.clone(), rl[i].t.clone(), String::new(), String::new()));
        lines.push(format!("{lf}: title '{}' -> '{}' (the tag names a different song)", short(&ours[o].title, 30), short(&rl[i].t, 30)));
    }
    if !changes.is_empty() {
        out.push(Finding {
            category: "Wrong track numbers or titles",
            severity: 3,
            detail: format!("{} of {} tracks differ from the official list ({src}): {}", changes.len(), ours.len(), more(&lines, 3)),
            changes: changes.clone(),
        });
    }
    let mut paired = vec![false; ours.len()];
    let mut swapped: Vec<String> = vec![];
    for &o in &off {
        if paired[o] {
            continue;
        }
        let ro = rl[real_of[o].unwrap()];
        let p = off.iter().cloned().find(|&p| p != o && !paired[p] && (ours[o].secs - rl[real_of[p].unwrap()].s).abs() <= 1.5 && (ours[p].secs - ro.s).abs() <= 1.5);
        if let Some(p) = p {
            paired[o] = true;
            paired[p] = true;
            let rp = rl[real_of[p].unwrap()];
            // two versions of one song ("Chicago (Acoustic)" / "Chicago (Easy Listening)"): our matching mixed them up
            if norm_title(&ro.t) == norm_title(&rp.t) {
                continue;
            }
            swapped.push(format!(
                "{} and {} are swapped: the first plays {} (the official '{}'), the second {} (the official '{}')",
                leaf(&ours[o].file),
                leaf(&ours[p].file),
                format_time(ours[o].secs),
                short(&rp.t, 30),
                format_time(ours[p].secs),
                short(&ro.t, 30)
            ));
        }
    }
    let versions: Vec<String> = off
        .iter()
        .filter(|&&o| !paired[o])
        .map(|&o| {
            let r = rl[real_of[o].unwrap()];
            format!("{} is {}, the official '{}' is {}", leaf(&ours[o].file), format_time(ours[o].secs), short(&r.t, 30), format_time(r.s))
        })
        .collect();
    if !swapped.is_empty() {
        out.push(Finding { category: "Wrong or swapped songs", severity: 2, detail: format!("{}. Have a listen", more(&swapped, 3)), changes: vec![] });
    }
    if !versions.is_empty() {
        out.push(Finding {
            category: "Different version of a song",
            severity: 3,
            detail: format!("{} ({src}) - an edit, live or extended version, a hidden track, or the wrong file", more(&versions, 3)),
            changes: vec![],
        });
    }

    // --- songs missing from an otherwise matching album (bonus/live/alternate versions don't count) ---
    let mut missing: Vec<usize> = (0..rl.len()).filter(|&i| !used[i] && !OPTIONAL.is_match(&rl[i].t)).collect();
    if !missing.is_empty() && matched.len() as f64 >= ours.len() as f64 * 0.8 && rl.len() >= ours.len() {
        missing.sort_by_key(|&i| (rl[i].d, rl[i].n));
        let names: Vec<String> =
            missing.iter().map(|&i| format!("{}{} '{}' ({})", if multi { format!("{}-", rl[i].d) } else { String::new() }, rl[i].n, short(&rl[i].t, 36), format_time(rl[i].s))).collect();
        // songs missing only after your last track (or on an extra disc) are usually a longer edition's bonus tracks
        let our_max_disc = matched.iter().map(|&o| rl[real_of[o].unwrap()].d.max(1)).max().unwrap_or(1);
        let last_on = |d: u32| matched.iter().map(|&o| rl[real_of[o].unwrap()]).filter(|r| r.d.max(1) == d).map(|r| r.n).max().unwrap_or(0);
        let tail_only = !missing.iter().any(|&i| rl[i].d.max(1) <= our_max_disc && rl[i].n < last_on(rl[i].d.max(1)));
        let list = more(&names, 5);
        out.push(Finding {
            category: "Incomplete album",
            severity: if tail_only { 3 } else { 2 },
            detail: if tail_only {
                format!("You have the album; {src}'s edition adds {} track{} at the end (likely bonus tracks): {list}", missing.len(), if missing.len() > 1 { "s" } else { "" })
            } else {
                format!("You have {} of the {} tracks on {src}'s edition. Missing: {list}", matched.len(), rl.len())
            },
            changes: vec![],
        });
    }
    out
}

// ---------- MusicBrainz track list (when Apple Music doesn't have the album) ----------
re!(LUCENE, r#"[\\+\-!(){}\[\]^"~*?:/&|]"#);

/// `want` says which record and which kind of recording the copy is; `titles` are the copy's track titles.
/// Of the editions MusicBrainz has, the one whose track list fits those titles best is returned (with no
/// titles: the one whose track count is closest).
pub fn musicbrainz_tracklist(ctx: &Ctx, artist: &str, album: &str, want: &crate::albumkind::AlbumId, titles: &[String]) -> Option<RealList> {
    if ctx.no_online || album.is_empty() {
        return None;
    }
    let clean = |t: &str| LUCENE.replace_all(t, " ").trim().to_string();
    let q = urlenc(&format!("releasegroup:({}) AND artist:({})", clean(&want.base), clean(artist)));
    let r = lookup::api(ctx, &format!("https://musicbrainz.org/ws/2/release-group/?query={q}&fmt=json&limit=10"), "musicbrainz", 1.1, None)?;
    let st = |v: &Value| v.as_str().unwrap_or("").to_string();
    let rg = r["release-groups"].as_array()?.iter().find(|g| want.is(&st(&g["title"])) && lookup::same_artist(ctx, &st(&g["artist-credit"][0]["name"]), artist))?.clone();
    let rel = lookup::api(ctx, &format!("https://musicbrainz.org/ws/2/release?release-group={}&inc=recordings+media&fmt=json&limit=25", st(&rg["id"])), "musicbrainz", 1.1, None)?;
    best_release(rel["releases"].as_array()?, titles)
}

/// The album a song comes from, as MusicBrainz has it: the earliest official album by this artist that
/// holds a recording of this title, a plain album before a live record or a compilation.
pub fn musicbrainz_album_of_song(ctx: &Ctx, artist: &str, title: &str) -> Option<String> {
    if ctx.no_online || title.trim().is_empty() {
        return None;
    }
    let clean = |t: &str| LUCENE.replace_all(t, " ").trim().to_string();
    let q = urlenc(&format!("recording:({}) AND artist:({})", clean(title), clean(artist)));
    let r = lookup::api(ctx, &format!("https://musicbrainz.org/ws/2/recording/?query={q}&fmt=json&limit=25"), "musicbrainz", 1.1, None)?;
    pick_recording_album(ctx, r["recordings"].as_array()?, artist, title)
}

/// Of MusicBrainz's recordings for a search, the album to name (see `musicbrainz_album_of_song`).
pub fn pick_recording_album(ctx: &Ctx, recordings: &[Value], artist: &str, title: &str) -> Option<String> {
    let st = |v: &Value| v.as_str().unwrap_or("").to_string();
    let want = norm_title(title);
    // (has a second kind such as live or compilation, the date, the name)
    let mut found: Vec<(bool, String, String)> = vec![];
    for rec in recordings {
        if norm_title(&st(&rec["title"])) != want || !lookup::same_artist(ctx, &st(&rec["artist-credit"][0]["name"]), artist) {
            continue;
        }
        for rel in rec["releases"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let group = &rel["release-group"];
            let official = rel["status"].as_str().is_none_or(|s| s == "Official");
            if !official || group["primary-type"].as_str() != Some("Album") || st(&rel["title"]).is_empty() {
                continue;
            }
            let second = group["secondary-types"].as_array().is_some_and(|a| !a.is_empty());
            // a release with no date sorts last
            let date = Some(st(&rel["date"])).filter(|d| !d.is_empty()).unwrap_or_else(|| "9999".into());
            found.push((second, date, st(&rel["title"])));
        }
    }
    found.sort();
    found.into_iter().next().map(|f| f.2)
}

/// The track list of the release that fits the copy best (see `musicbrainz_tracklist`).
fn best_release(releases: &[Value], titles: &[String]) -> Option<RealList> {
    let st = |v: &Value| v.as_str().unwrap_or("").to_string();
    let n = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    let named = titles.iter().filter(|t| !t.trim().is_empty()).count();
    // (fit by titles, how far the track count is off, how many titles stand at the same place as in the
    // copy, the tracks). The last one decides between editions that hold the same songs in another order.
    let mut best: Option<((usize, usize), i64, usize, Vec<RealTrack>)> = None;
    for x in releases {
        let mut tracks = vec![];
        let mut all_titles = vec![];
        for m in x["media"].as_array().cloned().unwrap_or_default() {
            for t in m["tracks"].as_array().cloned().unwrap_or_default() {
                all_titles.push(st(&t["title"]));
                let len = t["length"].as_f64().unwrap_or(0.0);
                if len > 0.0 {
                    tracks.push(RealTrack { d: n(&m["position"]) as u32, n: n(&t["position"]) as u32, t: st(&t["title"]), s: (len / 100.0).round() / 10.0 });
                }
            }
        }
        if tracks.is_empty() {
            continue;
        }
        let fit = if named == 0 { (0, 0) } else { lookup::fit_of(titles, &all_titles) };
        let off = (all_titles.len() as i64 - titles.len() as i64).abs();
        let same = titles.iter().zip(&all_titles).filter(|(ours, theirs)| !ours.trim().is_empty() && norm_title(ours) == norm_title(theirs)).count();
        let wins = match &best {
            None => true,
            Some((bf, boff, bsame, _)) => lookup::better_fit(fit, *bf) || (fit == *bf && (off < *boff || (off == *boff && same > *bsame))),
        };
        if wins {
            best = Some((fit, off, same, tracks));
        }
    }
    best.map(|(_, _, _, tracks)| RealList { source: "MusicBrainz".into(), tracks })
}

/// True when a copy's track numbers (disc, number) run 1, 2, 3 and so on with none missing and none used
/// twice, on every disc. Such a copy was numbered on purpose: an official list that has the same songs in
/// another order (editions differ) does not overrule it.
pub fn numbered_cleanly(numbers: &[(u32, Option<u32>)]) -> bool {
    let mut seen: Vec<(u32, u32)> = vec![];
    for (disc, no) in numbers {
        let Some(no) = no else { return false };
        seen.push(((*disc).max(1), *no));
    }
    seen.sort();
    let mut expect = (0, 0);
    for (disc, no) in seen {
        expect = if disc == expect.0 { (disc, expect.1 + 1) } else { (disc, 1) };
        if no != expect.1 {
            return false;
        }
    }
    !numbers.is_empty()
}

/// "Artist Album" for searching Soulseek again (unused here, kept for parity with Library Health's search text)
#[allow(dead_code)]
pub fn search_text(artist: &str, album: &str) -> String {
    format!("{artist} {}", strip_year(album)).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edition_that_fits_the_titles_wins() {
        let release = |titles: &[&str]| {
            let tracks: Vec<Value> = titles.iter().enumerate().map(|(i, t)| serde_json::json!({ "position": i + 1, "title": t, "length": 200000 })).collect();
            serde_json::json!({ "media": [{ "position": 1, "track-count": tracks.len(), "tracks": tracks }] })
        };
        let standard = release(&["One", "Two", "Three", "Four"]);
        let deluxe = release(&["One", "Two", "Three", "Four", "Five (Bonus)", "Six (Demo)"]);
        let other = release(&["One", "Zwei", "Drei", "Four", "Fuenf", "Sechs"]);
        let ours = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pick = |releases: &[Value], titles: &[String]| best_release(releases, titles).map(|l| l.tracks.len());
        // a deluxe copy is judged against the deluxe list, a standard copy against the standard one,
        // whichever comes first
        let deluxe_copy = ours(&["One", "Two", "Three", "Four", "Five", "Six"]);
        assert_eq!(pick(&[standard.clone(), deluxe.clone()], &deluxe_copy), Some(6));
        assert_eq!(pick(&[deluxe.clone(), standard.clone()], &ours(&["One", "Two", "Three", "Four"])), Some(4));
        // the same number of tracks is not enough: the titles decide
        assert_eq!(best_release(&[other.clone(), deluxe.clone()], &deluxe_copy).map(|l| l.tracks[4].t.clone()), Some("Five (Bonus)".to_string()));
        // no titles to go by: the closest track count, as before
        assert_eq!(pick(&[standard.clone(), deluxe.clone()], &ours(&["", "", "", "", "", ""])), Some(6));
        assert_eq!(pick(&[], &deluxe_copy), None);
        // two editions with the same songs in another order: the one that agrees with the copy's order,
        // whichever comes first
        let (swapped, straight) = (release(&["One", "Three", "Two", "Four"]), release(&["One", "Two", "Three", "Four"]));
        let second = |releases: &[Value], titles: &[String]| best_release(releases, titles).map(|l| l.tracks[1].t.clone());
        assert_eq!(second(&[swapped.clone(), straight.clone()], &ours(&["One", "Two", "Three", "Four"])), Some("Two".to_string()));
        assert_eq!(second(&[straight.clone(), swapped.clone()], &ours(&["One", "Three", "Two", "Four"])), Some("Three".to_string()));
        assert_eq!(lookup::fit_of(&deluxe_copy, &ours(&["One", "Two (feat. X)", "Three", "Nine"])), (3, 1));
    }

    #[test]
    fn numbers_that_run_without_gaps_count_as_numbered_on_purpose() {
        let one = |nos: &[u32]| nos.iter().map(|n| (0, Some(*n))).collect::<Vec<_>>();
        assert!(numbered_cleanly(&one(&[1, 2, 3, 4])));
        assert!(numbered_cleanly(&one(&[3, 1, 4, 2])), "the order the files are read in does not matter");
        assert!(!numbered_cleanly(&one(&[1, 2, 3, 5])), "a gap");
        assert!(!numbered_cleanly(&one(&[1, 2, 2, 3])), "a number used twice");
        assert!(!numbered_cleanly(&one(&[2, 3, 4])), "does not start at 1");
        assert!(!numbered_cleanly(&[(1, Some(1)), (1, None)]), "a track with no number");
        assert!(!numbered_cleanly(&[]));
        // two discs, each from 1
        assert!(numbered_cleanly(&[(1, Some(1)), (1, Some(2)), (2, Some(1)), (2, Some(2))]));
        assert!(!numbered_cleanly(&[(1, Some(1)), (1, Some(2)), (2, Some(2)), (2, Some(3))]));
    }

    #[test]
    fn titles() {
        assert_eq!(norm_title("Exogenesis: Symphony Part 1 (Overture)"), norm_title("Exogenesis: Symphony, Pt. 1 (Overture)"));
        assert_eq!(norm_title("Part II"), "pt2");
        assert_eq!(title_relation("Rerviewmirror", "Rearviewmirror"), Rel::Typo);
        assert_eq!(title_relation("Get Your Shit Together", "Get Your S**t Together"), Rel::Same);
        assert_eq!(title_relation("Modern Things", "The Modern Things"), Rel::Variant);
        assert_eq!(title_relation("Santa Claus Is Coming to Town", "Santa Claus Is Coming to Town (feat. Frank Sinatra)"), Rel::Variant);
    }
    #[test]
    fn missing_and_swaps() {
        let real = RealList {
            source: "Apple Music".into(),
            tracks: (1..=6).map(|i| RealTrack { d: 1, n: i, t: format!("Song {}", ["A", "B", "C", "D", "E", "F"][i as usize - 1]), s: 100.0 + 30.0 * i as f64 }).collect(),
        };
        let mk = |n: u32, t: &str, s: f64| OurTrack { file: format!("C:\\x\\{n:02} {t}.mp3"), title: t.into(), disc: 0, no: n, secs: s, errors: 0 };
        // song D missing; B and C swapped
        let ours = vec![mk(1, "Song A", 130.0), mk(2, "Song B", 190.0), mk(3, "Song C", 160.0), mk(5, "Song E", 250.0), mk(6, "Song F", 280.0)];
        let mut fit = Fit::default();
        let f = resolve(&ours, &real, &mut fit);
        assert!(fit.fit);
        assert!(f.iter().any(|x| x.category == "Incomplete album" && x.severity == 2 && x.detail.contains("Song D")), "{f:?}");
        assert!(f.iter().any(|x| x.category == "Wrong or swapped songs"), "{f:?}");
    }
}
