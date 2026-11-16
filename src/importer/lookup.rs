//! Online lookups: Apple Music (album info, track list, cover), then Bandcamp, then MusicBrainz.
use crate::importer::ctx::Ctx;
use crate::importer::util::{norm, strip_edition};
use regex::Regex;
use serde_json::Value;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct AppleAlbum {
    pub collection_id: u64,
    pub collection_name: String,
    pub artist_name: String,
    pub track_count: u32,
    pub artwork_url100: String,
    pub release_date: String,
    pub genre: String,
}

#[derive(Clone, Debug)]
pub struct AppleTrack {
    pub name: String,
    pub number: u32,
    pub disc_number: u32,
    pub disc_count: u32,
    pub millis: u64,
}

pub struct AppleCat {
    pub ok: bool,
    /// may be cut off (200 results), so absence proves nothing
    pub full: bool,
    pub albums: Vec<AppleAlbum>,
}

#[derive(Clone, Debug)]
pub struct Cover {
    pub source: String,
    pub label: String,
    pub urls: Vec<String>,
    pub thumb: String,
    /// false = offered but not ticked by default
    pub default: bool,
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

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}
fn u(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)).unwrap_or(0)
}

/// GET/POST JSON with a politeness delay and a few retries. 404 = nothing there, so no retry.
pub fn api(ctx: &Ctx, url: &str, key: &'static str, gap: f64, body: Option<&Value>) -> Option<Value> {
    // a run that asks about hundreds of albums (the Health recheck) keeps the answers on disk for a month,
    // so asking again costs nothing and the sites are not asked twice. "Nothing there" is kept too.
    let kept = ctx.lookup_cache.as_ref().filter(|_| body.is_none()).map(|dir| dir.join(format!("{:x}.json", md5::compute(url.as_bytes()))));
    if let Some(file) = &kept {
        let fresh = std::fs::metadata(file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < Duration::from_secs(30 * 86400));
        if fresh {
            if let Some(v) = std::fs::read(file).ok().filter(|b| b.len() < 20_000_000).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                return if v.is_null() { None } else { Some(v) };
            }
        }
    }
    let keep = |v: &Value| {
        if let Some(file) = &kept {
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(file));
            let _ = crate::importer::util::write_atomic(file, v.to_string().as_bytes());
        }
    };
    for attempt in 1..=3u64 {
        ctx.wait_turn(key, gap);
        let resp = match body {
            Some(b) => ctx.api.post(url).send_json(b),
            None => ctx.api.get(url).call(),
        };
        match resp {
            Ok(mut r) => {
                let code = r.status().as_u16();
                if code == 404 {
                    keep(&Value::Null);
                    return None;
                }
                if (200..300).contains(&code) {
                    let v = r.body_mut().read_json::<Value>().ok();
                    if let Some(v) = &v {
                        keep(v);
                    }
                    return v;
                }
            }
            Err(_) => {}
        }
        if attempt == 3 {
            ctx.net_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        std::thread::sleep(Duration::from_secs(3 * attempt));
    }
    None
}

fn apple(ctx: &Ctx, url: &str) -> Option<Value> {
    api(ctx, url, "apple", 3.0, None)
}

static REISSUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)remaster|anniversary|reissue|expanded|deluxe|legacy edition|\b(19|20)\d\d (mix|version|edition)\b").unwrap());
static SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s*(?:&|,|\+|\s/\s|\band\b|\bx\b|\bfeat\.?|\bft\.?|\bfeaturing\b|\bwith\b|\bvs\.?)\s*").unwrap());

pub fn is_reissue(found: &str, mine: &str) -> bool {
    REISSUE.is_match(found) && !REISSUE.is_match(mine)
}

/// "TV Girl & George Clanton" -> tv girl, george clanton
fn split_artists(s: &str) -> Vec<String> {
    SPLIT.split(s).map(norm).filter(|x| !x.is_empty()).collect()
}

pub fn same_artist(ctx: &Ctx, found: &str, want: &str) -> bool {
    let (a, w) = (norm(found), norm(want));
    if a.is_empty() || w.is_empty() {
        return false;
    }
    if a == w {
        return true;
    }
    if ctx.resolve_artist(&[found.to_string()]).map(|r| norm(&r) == w).unwrap_or(false) {
        return true; // alias file, e.g. "death's dynamic shroud.wmv"
    }
    let (fa, wa) = (split_artists(found), split_artists(want));
    fa.contains(&w) || wa.contains(&a)
}

/// The same title and the same kind of recording (see `crate::albumkind`): "Rocket (Deluxe)" is "Rocket",
/// "Rocket (Live)" is not.
pub fn same_album(found: &str, want: &str) -> bool {
    crate::albumkind::classify(want).is(found)
}

/// How well an official track list fits the titles of a copy: (how many of the copy's titles are on it,
/// how many of its titles the copy does not have). Titles are compared the way the track-list check does.
pub fn fit_of(ours: &[String], official: &[String]) -> (usize, usize) {
    use crate::importer::tracklist::norm_title;
    let mut left: Vec<String> = official.iter().map(|t| norm_title(t)).collect();
    let mut covered = 0;
    for t in ours.iter().map(|t| norm_title(t)).filter(|t| !t.is_empty()) {
        if let Some(i) = left.iter().position(|o| *o == t) {
            left.swap_remove(i);
            covered += 1;
        }
    }
    (covered, left.len())
}

/// The better of two fits: more of the copy's titles first, then fewer titles the copy lacks.
pub fn better_fit(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 > b.0 || (a.0 == b.0 && a.1 < b.1)
}

fn parse_apple_album(v: &Value) -> AppleAlbum {
    AppleAlbum {
        collection_id: u(&v["collectionId"]),
        collection_name: s(&v["collectionName"]),
        artist_name: s(&v["artistName"]),
        track_count: u(&v["trackCount"]) as u32,
        artwork_url100: s(&v["artworkUrl100"]),
        release_date: s(&v["releaseDate"]),
        genre: s(&v["primaryGenreName"]),
    }
}

/// Apple's whole catalogue for an artist, fetched once and shared by every parallel lookup.
pub fn apple_artist_albums(ctx: &Ctx, artist: &str) -> Arc<AppleCat> {
    let key = norm(artist);
    let cell = ctx.apple_cache.lock().unwrap().entry(key).or_insert_with(|| Arc::new(OnceLock::new())).clone();
    // one thread fetches; the others for the same artist wait here and reuse its answer
    cell.get_or_init(|| {
        let r = apple(ctx, &format!("https://itunes.apple.com/search?term={}&entity=album&attribute=artistTerm&limit=200", urlenc(artist)));
        let results = r.as_ref().and_then(|r| r["results"].as_array()).cloned().unwrap_or_default();
        Arc::new(AppleCat {
            ok: r.is_some(),
            full: r.as_ref().map(|r| u(&r["resultCount"]) >= 200).unwrap_or(false),
            albums: results.iter().filter(|a| same_artist(ctx, &s(&a["artistName"]), artist)).map(parse_apple_album).collect(),
        })
    })
    .clone()
}

/// Every edition Apple has of this album (the same title and kind of recording), likeliest first: plain
/// editions before reissues when the copy is not one, then the closest track count.
pub fn apple_editions(ctx: &Ctx, artist: &str, album: &str, want: &crate::albumkind::AlbumId, track_count: usize) -> Vec<AppleAlbum> {
    if ctx.no_online || album.is_empty() {
        return vec![];
    }
    let pick = |list: &[AppleAlbum]| -> Vec<AppleAlbum> {
        let mut c: Vec<&AppleAlbum> = list.iter().filter(|a| want.is(&a.collection_name)).collect();
        c.sort_by_key(|a| (is_reissue(&a.collection_name, album) as u8, (a.track_count as i64 - track_count as i64).abs()));
        let mut seen = std::collections::HashSet::new();
        c.into_iter().filter(|a| seen.insert(a.collection_id)).cloned().collect()
    };
    let cat = apple_artist_albums(ctx, artist);
    let found = pick(&cat.albums);
    if !found.is_empty() {
        return found;
    }
    // Apple has this artist's full catalogue and the album isn't in it: no need to search further
    if cat.ok && !cat.albums.is_empty() && !cat.full {
        return vec![];
    }
    let mut terms = vec![format!("{artist} {}", want.base)];
    if cat.albums.is_empty() {
        terms.push(want.base.clone());
    }
    for term in terms {
        let r = apple(ctx, &format!("https://itunes.apple.com/search?term={}&entity=album&limit=25", urlenc(&term)));
        let list: Vec<AppleAlbum> = r
            .as_ref()
            .and_then(|r| r["results"].as_array())
            .map(|a| a.iter().filter(|x| same_artist(ctx, &s(&x["artistName"]), artist)).map(parse_apple_album).collect())
            .unwrap_or_default();
        let found = pick(&list);
        if !found.is_empty() {
            return found;
        }
    }
    vec![]
}

/// The likeliest edition by name and track count alone (for covers, where the edition matters little).
pub fn find_apple_album(ctx: &Ctx, artist: &str, album: &str, track_count: usize) -> Option<AppleAlbum> {
    apple_editions(ctx, artist, album, &crate::albumkind::classify(album), track_count).into_iter().next()
}

/// The album a song comes from, for a track that has no album tag: (the album's name, where it was found).
/// Apple Music first, then MusicBrainz. Only an album by the same artist that lists a song of the same
/// title counts, a studio album before any other kind, an album before an EP or a single, and of those the
/// earliest (the record the song first came out on). None when neither site has it, or neither answered.
pub fn album_of_song(ctx: &Ctx, artist: &str, title: &str) -> Option<(String, String)> {
    if ctx.no_online || artist.trim().is_empty() || title.trim().is_empty() {
        return None;
    }
    let found = apple(ctx, &format!("https://itunes.apple.com/search?term={}&entity=song&limit=50", urlenc(&format!("{artist} {title}"))));
    if let Some(name) = found.as_ref().and_then(|v| v["results"].as_array()).and_then(|r| pick_song_album(ctx, r, artist, title)) {
        return Some((name, "Apple Music".into()));
    }
    crate::importer::tracklist::musicbrainz_album_of_song(ctx, artist, title).map(|name| (name, "MusicBrainz".into()))
}

/// Of Apple's songs for a search, the album to name for this artist's song (see `album_of_song`).
pub fn pick_song_album(ctx: &Ctx, results: &[Value], artist: &str, title: &str) -> Option<String> {
    use crate::albumkind::{classify, Kind};
    let want = crate::importer::tracklist::norm_title(title);
    if want.is_empty() {
        return None;
    }
    // (not a studio record, a single or an EP, the release date, the name)
    let mut found: Vec<(bool, bool, String, String)> = vec![];
    for r in results {
        let (by, song, album) = (s(&r["artistName"]), s(&r["trackName"]), s(&r["collectionName"]));
        // a collection under another name ("Various Artists") is somebody's compilation, not the song's album
        let collection_by = s(&r["collectionArtistName"]);
        if album.is_empty() || !same_artist(ctx, &by, artist) || crate::importer::tracklist::norm_title(&song) != want || (!collection_by.is_empty() && !same_artist(ctx, &collection_by, artist)) {
            continue;
        }
        let lower = album.to_lowercase();
        let small = lower.ends_with(" - single") || lower.ends_with(" - ep") || u(&r["trackCount"]) <= 3;
        found.push((classify(&album).kind != Kind::Studio, small, s(&r["releaseDate"]), album));
    }
    found.sort();
    found.into_iter().next().map(|f| f.3)
}

/// At most this many editions have their track lists fetched to find the one that fits (each is a request).
const EDITIONS_TRIED: usize = 4;

/// The edition whose track list fits this copy's titles best, with that track list. The likeliest edition
/// is looked at first and taken at once if it is exactly the copy (every title, nothing extra); otherwise
/// the next few are compared and the best fit wins. Without titles the likeliest edition is taken.
pub fn find_apple_edition(ctx: &Ctx, artist: &str, album: &str, want: &crate::albumkind::AlbumId, titles: &[String]) -> Option<(AppleAlbum, Vec<AppleTrack>)> {
    let named = titles.iter().filter(|t| !t.trim().is_empty()).count();
    let mut best: Option<((usize, usize), AppleAlbum, Vec<AppleTrack>)> = None;
    let mut first: Option<AppleAlbum> = None;
    for edition in apple_editions(ctx, artist, album, want, titles.len()).into_iter().take(EDITIONS_TRIED) {
        let tracks = apple_tracks(ctx, edition.collection_id);
        if tracks.is_empty() {
            // known to Apple, but its track list could not be read: still good for the cover and the year
            first.get_or_insert(edition);
            continue;
        }
        if named == 0 {
            return Some((edition, tracks));
        }
        let fit = fit_of(titles, &tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
        let exact = fit == (named, 0);
        if best.as_ref().map(|b| better_fit(fit, b.0)).unwrap_or(true) {
            best = Some((fit, edition, tracks));
        }
        if exact {
            break;
        }
    }
    best.map(|(_, edition, tracks)| (edition, tracks)).or(first.map(|e| (e, vec![])))
}

pub fn apple_tracks(ctx: &Ctx, collection_id: u64) -> Vec<AppleTrack> {
    if let Some(t) = ctx.apple_tracks.lock().unwrap_or_else(|e| e.into_inner()).get(&collection_id) {
        return t.clone();
    }
    let Some(r) = apple(ctx, &format!("https://itunes.apple.com/lookup?id={collection_id}&entity=song")) else { return vec![] };
    let tracks: Vec<AppleTrack> = r["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t["wrapperType"] == "track")
                .map(|t| AppleTrack { name: s(&t["trackName"]), number: u(&t["trackNumber"]) as u32, disc_number: u(&t["discNumber"]) as u32, disc_count: u(&t["discCount"]) as u32, millis: u(&t["trackTimeMillis"]) })
                .collect()
        })
        .unwrap_or_default();
    ctx.apple_tracks.lock().unwrap_or_else(|e| e.into_inner()).insert(collection_id, tracks.clone());
    tracks
}

fn find_bandcamp_cover(ctx: &Ctx, artist: &str, album: &str) -> Option<Cover> {
    if album.is_empty() {
        return None;
    }
    let body = serde_json::json!({ "search_text": format!("{artist} {}", strip_edition(album)), "search_filter": "a", "full_page": false, "fan_id": null });
    let r = api(ctx, "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic", "bandcamp", 1.0, Some(&body))?;
    let hit = r["auto"]["results"].as_array()?.iter().find(|x| {
        x["type"] == "a" && !x["art_id"].is_null() && u(&x["art_id"]) != 0 && same_album(&s(&x["name"]), album) && same_artist(ctx, &s(&x["band_name"]), artist)
    })?;
    let id = u(&hit["art_id"]);
    // "_0" is the image exactly as the artist uploaded it
    Some(Cover {
        source: "Bandcamp".into(),
        label: format!("Bandcamp: '{}' by {}", s(&hit["name"]), s(&hit["band_name"])),
        urls: vec![format!("https://f4.bcbits.com/img/a{id}_0.jpg")],
        thumb: format!("https://f4.bcbits.com/img/a{id}_2.jpg"),
        default: true,
    })
}

fn find_musicbrainz_cover(ctx: &Ctx, artist: &str, album: &str) -> Option<Cover> {
    if album.is_empty() {
        return None;
    }
    let lucene = Regex::new(r#"[\\+\-!(){}\[\]^"~*?:/&|]"#).unwrap();
    let clean = |t: &str| lucene.replace_all(t, " ").trim().to_string();
    let q = urlenc(&format!("releasegroup:({}) AND artist:({})", clean(&strip_edition(album)), clean(artist)));
    let r = api(ctx, &format!("https://musicbrainz.org/ws/2/release-group/?query={q}&fmt=json&limit=5"), "musicbrainz", 1.1, None)?;
    for h in r["release-groups"].as_array()? {
        if !(same_album(&s(&h["title"]), album) && same_artist(ctx, &s(&h["artist-credit"][0]["name"]), artist)) {
            continue;
        }
        let Some(c) = api(ctx, &format!("https://coverartarchive.org/release-group/{}", s(&h["id"])), "musicbrainz", 1.1, None) else { continue };
        let Some(front) = c["images"].as_array().and_then(|a| a.iter().find(|i| i["front"] == true)) else { continue };
        let thumb = if !front["thumbnails"]["500"].is_null() {
            s(&front["thumbnails"]["500"])
        } else if !front["thumbnails"]["large"].is_null() {
            s(&front["thumbnails"]["large"])
        } else {
            s(&front["image"])
        };
        let https = |u: String| u.replacen("http:", "https:", 1);
        return Some(Cover {
            source: "MusicBrainz".into(),
            label: format!("MusicBrainz: '{}' by {}", s(&h["title"]), s(&h["artist-credit"][0]["name"])),
            urls: vec![https(s(&front["image"]))],
            thumb: https(thumb),
            default: true,
        });
    }
    None
}

pub fn find_cover(ctx: &Ctx, artist: &str, album: &str, apple: Option<&AppleAlbum>) -> Option<Cover> {
    if ctx.no_online {
        return None;
    }
    let mut apple_cover = None;
    if let Some(a) = apple {
        // Apple's image server caps any size request at the original upload, and PNG keeps it lossless.
        let base = a.artwork_url100.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default();
        let c = Cover {
            source: "Apple".into(),
            label: format!("Apple Music: '{}' by {}", a.collection_name, a.artist_name),
            urls: vec![format!("{base}/10000x10000bb.png"), format!("{base}/3000x3000bb.png")],
            thumb: format!("{base}/600x600bb.jpg"),
            default: true,
        };
        // reissues often get new artwork: only trust Apple's reissue cover if nothing else has the original
        if !is_reissue(&a.collection_name, album) {
            return Some(c);
        }
        apple_cover = Some(c);
    }
    let mut c = find_bandcamp_cover(ctx, artist, album);
    if c.is_none() {
        c = find_musicbrainz_cover(ctx, artist, album);
    }
    if c.is_none() {
        if let Some(mut a) = apple_cover {
            // last resort: offered, but not used unless you tick it, since the reissue art may differ
            a.source = "Apple (reissue art)".into();
            a.label = format!("{} - a reissue, so the art may differ from your copy", a.label);
            a.default = false;
            c = Some(a);
        }
    }
    c
}
