//! Last.fm, MusicBrainz and iTunes lookups. All go through `Net` (cache + rate limits).
use crate::net::{days, hours, Net};
use crate::util::{artist_key, urlenc};
use serde_json::Value;
use std::time::Duration;

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}
fn num(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}
fn arr(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![v.clone()], // Last.fm returns a bare object when there's one item
        _ => vec![],
    }
}

// ---------------- Last.fm ----------------

pub struct LfmArtist {
    pub name: String,
    pub mbid: String,
    pub plays: u64,
    pub url: String,
}

fn lastfm(net: &Net, key: &str, params: &str, ttl: Duration) -> Result<Value, String> {
    let url = format!("https://ws.audioscrobbler.com/2.0/?{params}&api_key={}&format=json", urlenc(key.trim()));
    let v = net.get_json("lastfm", &url, ttl, Some(&net.lastfm))?;
    if let Some(msg) = v.get("message").filter(|_| v.get("error").is_some()) {
        return Err(format!("Last.fm: {}", s(msg)));
    }
    Ok(v)
}

pub fn lfm_top_artists(net: &Net, user: &str, key: &str, period: &str, limit: u32) -> Result<Vec<LfmArtist>, String> {
    let v = lastfm(
        net,
        key,
        &format!("method=user.gettopartists&user={}&period={period}&limit={limit}", urlenc(user.trim())),
        hours(6),
    )?;
    Ok(arr(&v["topartists"]["artist"])
        .iter()
        .map(|a| LfmArtist { name: s(&a["name"]), mbid: s(&a["mbid"]), plays: num(&a["playcount"]) as u64, url: s(&a["url"]) })
        .collect())
}

/// (artist name, artist mbid) for each of the most recent scrobbles.
pub fn lfm_recent(net: &Net, user: &str, key: &str, pages: u32) -> Result<Vec<(String, String)>, String> {
    let mut out = vec![];
    for page in 1..=pages {
        let v = lastfm(
            net,
            key,
            &format!("method=user.getrecenttracks&user={}&limit=200&page={page}", urlenc(user.trim())),
            hours(1),
        )?;
        let tracks = arr(&v["recenttracks"]["track"]);
        let total_pages = num(&v["recenttracks"]["@attr"]["totalPages"]) as u32;
        for t in &tracks {
            out.push((s(&t["artist"]["#text"]), s(&t["artist"]["mbid"])));
        }
        if tracks.is_empty() || page >= total_pages {
            break;
        }
    }
    Ok(out)
}

pub struct Similar {
    pub name: String,
    pub score: f64,
    pub url: String,
    pub mbid: String,
}

pub fn lfm_similar(net: &Net, key: &str, artist: &str, limit: u32) -> Result<Vec<Similar>, String> {
    let v = lastfm(
        net,
        key,
        &format!("method=artist.getsimilar&artist={}&limit={limit}&autocorrect=1", urlenc(artist)),
        days(30),
    )?;
    Ok(arr(&v["similarartists"]["artist"])
        .iter()
        .map(|a| Similar { name: s(&a["name"]), score: num(&a["match"]), url: s(&a["url"]), mbid: s(&a["mbid"]) })
        .collect())
}

// ---------------- MusicBrainz ----------------

pub struct MbArtist {
    pub id: String,
    pub name: String,
    pub score: f64,
}

pub fn mb_search_artist(net: &Net, name: &str) -> Result<Vec<MbArtist>, String> {
    let q = format!("artist:\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
    let url = format!("https://musicbrainz.org/ws/2/artist?query={}&fmt=json&limit=8", urlenc(&q));
    let v = net.get_json("mb", &url, days(90), Some(&net.mb))?;
    Ok(arr(&v["artists"])
        .iter()
        .map(|a| {
            // count aliases too, e.g. "death's dynamic shroud.wmv"
            let alias_hit = arr(&a["aliases"]).iter().any(|al| artist_key(&s(&al["name"])) == artist_key(name));
            let mut nm = s(&a["name"]);
            if alias_hit && artist_key(&nm) != artist_key(name) {
                nm = name.to_string();
            }
            MbArtist { id: s(&a["id"]), name: nm, score: num(&a["score"]) }
        })
        .collect())
}

pub struct MbReleaseGroup {
    pub id: String,
    pub title: String,
    pub date: String,
    pub primary: String,
    pub secondary: Vec<String>,
    pub credit: String,
    pub first_artist_id: String,
    pub first_artist_name: String,
}

fn parse_rgs(v: &Value) -> Vec<MbReleaseGroup> {
    arr(&v["release-groups"])
        .iter()
        .map(|r| MbReleaseGroup {
            id: s(&r["id"]),
            title: s(&r["title"]),
            date: s(&r["first-release-date"]),
            primary: s(&r["primary-type"]),
            secondary: arr(&r["secondary-types"]).iter().map(s).collect(),
            credit: arr(&r["artist-credit"]).iter().map(|c| format!("{}{}", s(&c["name"]), s(&c["joinphrase"]))).collect(),
            first_artist_id: s(&r["artist-credit"][0]["artist"]["id"]),
            first_artist_name: s(&r["artist-credit"][0]["name"]),
        })
        .collect()
}

/// Release groups by this artist first released on/after `from` (YYYY-MM-DD).
pub fn mb_recent_release_groups(net: &Net, mbid: &str, from: &str, ttl: Duration) -> Result<Vec<MbReleaseGroup>, String> {
    let q = format!("arid:{mbid} AND firstreleasedate:[{from} TO 2100-12-31]");
    let url = format!("https://musicbrainz.org/ws/2/release-group?query={}&fmt=json&limit=100", urlenc(&q));
    Ok(parse_rgs(&net.get_json("mb", &url, ttl, Some(&net.mb))?))
}

/// Any release groups (used to tell apart artists that share a name).
pub fn mb_some_release_groups(net: &Net, mbid: &str) -> Result<Vec<MbReleaseGroup>, String> {
    let url = format!("https://musicbrainz.org/ws/2/release-group?artist={mbid}&fmt=json&limit=100");
    Ok(parse_rgs(&net.get_json("mb", &url, days(60), Some(&net.mb))?))
}

// ---------------- iTunes ----------------

pub struct ItAlbum {
    pub artist: String,
    pub title: String,
    pub date: String,
    pub url: String,
    pub art: String,
    pub tracks: u32,
    pub artist_id: u64,
}

pub fn itunes_albums(net: &Net, artist: &str, ttl: Duration) -> Result<Vec<ItAlbum>, String> {
    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=album&attribute=artistTerm&limit=200&country={}",
        urlenc(artist),
        crate::util::store_country()
    );
    let v = net.get_json("itunes", &url, ttl, Some(&net.itunes))?;
    Ok(arr(&v["results"])
        .iter()
        .map(|a| ItAlbum {
            artist: s(&a["artistName"]),
            title: s(&a["collectionName"]),
            date: s(&a["releaseDate"]).chars().take(10).collect(),
            url: s(&a["collectionViewUrl"]).replace("?uo=4", ""),
            art: s(&a["artworkUrl100"]).replace("100x100bb", "300x300bb"),
            tracks: num(&a["trackCount"]) as u32,
            artist_id: num(&a["artistId"]) as u64,
        })
        .collect())
}

// ---------------- artist details ----------------

pub struct LfmInfo {
    pub name: String,
    pub mbid: String,
    pub url: String,
    pub listeners: u64,
    pub tags: Vec<String>,
    pub bio: String,
}

static RE_HTML: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]*>").unwrap());

pub fn lfm_artist_info(net: &Net, key: &str, artist: &str) -> Result<LfmInfo, String> {
    let v = lastfm(net, key, &format!("method=artist.getinfo&artist={}&autocorrect=1", urlenc(artist)), days(30))?;
    let a = &v["artist"];
    let bio = s(&a["bio"]["summary"]);
    let bio = bio.split("<a href").next().unwrap_or("");
    let bio = RE_HTML.replace_all(bio, "").replace("&amp;", "&").replace("&quot;", "\"").trim().to_string();
    Ok(LfmInfo {
        name: s(&a["name"]),
        mbid: s(&a["mbid"]),
        url: s(&a["url"]),
        listeners: num(&a["stats"]["listeners"]) as u64,
        tags: arr(&a["tags"]["tag"]).iter().map(|t| s(&t["name"]).to_lowercase()).filter(|t| !t.is_empty() && t != "seen live").collect(),
        bio,
    })
}

pub struct LfmAlbum {
    pub name: String,
    pub plays: u64,
    pub art: String,
    pub url: String,
}

pub fn lfm_top_albums(net: &Net, key: &str, artist: &str) -> Result<Vec<LfmAlbum>, String> {
    let v = lastfm(net, key, &format!("method=artist.gettopalbums&artist={}&autocorrect=1&limit=60", urlenc(artist)), days(14))?;
    Ok(arr(&v["topalbums"]["album"])
        .iter()
        .map(|a| LfmAlbum {
            name: s(&a["name"]),
            plays: num(&a["playcount"]) as u64,
            art: arr(&a["image"]).iter().rev().map(|i| s(&i["#text"])).find(|u| !u.is_empty()).unwrap_or_default(),
            url: s(&a["url"]),
        })
        .collect())
}

/// Albums and EPs credited to the artist (studio + whatever MusicBrainz knows), oldest first.
pub fn mb_albums(net: &Net, mbid: &str) -> Result<Vec<MbReleaseGroup>, String> {
    let url = format!("https://musicbrainz.org/ws/2/release-group?artist={mbid}&type=album%7Cep&fmt=json&limit=100");
    let mut v = parse_rgs(&net.get_json("mb", &url, days(30), Some(&net.mb))?);
    v.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(v)
}

/// Play count and cover for one album (used for recent albums that aren't in the artist's top-60 list yet).
pub fn lfm_album_info(net: &Net, key: &str, artist: &str, album: &str) -> Option<LfmAlbum> {
    let v = lastfm(
        net,
        key,
        &format!("method=album.getinfo&artist={}&album={}&autocorrect=1", urlenc(artist), urlenc(album)),
        days(7),
    )
    .ok()?;
    let a = &v["album"];
    Some(LfmAlbum {
        name: s(&a["name"]),
        plays: num(&a["playcount"]) as u64,
        art: arr(&a["image"]).iter().rev().map(|i| s(&i["#text"])).find(|u| !u.is_empty()).unwrap_or_default(),
        url: s(&a["url"]),
    })
}

/// Quick account check for the Settings "Test" button: "name: 103,621 scrobbles since 2014".
pub fn lfm_user_info(net: &Net, user: &str, key: &str) -> Result<String, String> {
    if user.trim().is_empty() || key.trim().is_empty() {
        return Err("Enter a username and API key first.".into());
    }
    let v = lastfm(net, key, &format!("method=user.getinfo&user={}", urlenc(user.trim())), Duration::from_secs(0))?;
    let u = &v["user"];
    let plays = num(&u["playcount"]) as u64;
    let since = num(&u["registered"]["unixtime"]) as i64;
    let year = chrono::DateTime::from_timestamp(since, 0).map(|d| d.format("%Y").to_string()).unwrap_or_default();
    let mut plays_s = plays.to_string();
    let mut i = plays_s.len() as i64 - 3;
    while i > 0 {
        plays_s.insert(i as usize, ',');
        i -= 3;
    }
    Ok(format!("{}: {plays_s} scrobbles{}", s(&u["name"]), if year.is_empty() { String::new() } else { format!(" since {year}") }))
}

/// An artist MusicBrainz might mean, with enough detail to tell same-named artists apart.
#[derive(Clone, Default)]
pub struct MbCandidate {
    pub id: String,
    pub name: String,
    pub disambiguation: String,
    pub kind: String,
    pub area: String,
    pub begin: String,
    pub score: f64,
}

pub fn mb_artist_candidates(net: &Net, query: &str) -> Result<Vec<MbCandidate>, String> {
    let q = format!("artist:\"{}\"", query.replace('\\', "\\\\").replace('"', "\\\""));
    let url = format!("https://musicbrainz.org/ws/2/artist?query={}&fmt=json&limit=10", urlenc(&q));
    let v = net.get_json("mb", &url, days(30), Some(&net.mb))?;
    Ok(arr(&v["artists"])
        .iter()
        .map(|a| MbCandidate {
            id: s(&a["id"]),
            name: s(&a["name"]),
            disambiguation: s(&a["disambiguation"]),
            kind: s(&a["type"]),
            area: s(&a["area"]["name"]),
            begin: s(&a["life-span"]["begin"]).chars().take(4).collect(),
            score: num(&a["score"]),
        })
        .collect())
}

/// Release groups first released between `from` and `to` (both YYYY-MM-DD, inclusive).
pub fn mb_release_groups_between(net: &Net, mbid: &str, from: &str, to: &str, ttl: Duration) -> Result<Vec<MbReleaseGroup>, String> {
    let q = format!("arid:{mbid} AND firstreleasedate:[{from} TO {to}]");
    let url = format!("https://musicbrainz.org/ws/2/release-group?query={}&fmt=json&limit=100", urlenc(&q));
    Ok(parse_rgs(&net.get_json("mb", &url, ttl, Some(&net.mb))?))
}

// ---------------- Deezer (pictures only: artist photos, and covers MusicBrainz/Last.fm don't have) ----------------
// Free and keyless. Matches are by exact (normalised) name, so a same-named stranger's photo isn't used for a near miss.

fn deezer(net: &Net, path_and_query: &str) -> Option<Value> {
    let v = net.get_json("deezer", &format!("https://api.deezer.com/{path_and_query}"), days(7), Some(&net.deezer)).ok()?;
    if v.get("error").is_some() { None } else { Some(v) }
}

/// Deezer's stand-in picture for artists without a photo has an empty image id ("/images/artist//...")
fn real_picture(url: &str) -> bool {
    !url.is_empty() && !url.contains("/artist//")
}

/// The photo of the artist with exactly this name (the best-known one, if several share it).
pub fn deezer_artist_photo(net: &Net, name: &str) -> Option<String> {
    let want = artist_key(name);
    if want.is_empty() {
        return None;
    }
    let v = deezer(net, &format!("search/artist?q={}&limit=10", urlenc(name)))?;
    arr(&v["data"])
        .into_iter()
        .filter(|a| artist_key(&s(&a["name"])) == want && real_picture(&s(&a["picture_medium"])))
        .max_by_key(|a| num(&a["nb_fan"]) as u64)
        .map(|a| s(&a["picture_medium"]))
}

/// A cover for this artist's release: the same title first, then one whose title contains it or vice versa
/// ("Rare N' Deluxe" / "Detour (Rare N' Deluxe)").
pub fn deezer_cover(net: &Net, artist: &str, title: &str) -> Option<String> {
    let (want_artist, want_title) = (artist_key(artist), crate::util::album_key(title));
    if want_artist.is_empty() || want_title.is_empty() {
        return None;
    }
    let q = format!("artist:\"{}\" album:\"{}\"", artist.replace('"', ""), title.replace('"', ""));
    let mut found = arr(&deezer(net, &format!("search/album?q={}&limit=15", urlenc(&q)))?["data"]);
    if found.is_empty() {
        // the strict search can miss on punctuation; a plain one, still filtered by artist below
        found = arr(&deezer(net, &format!("search/album?q={}&limit=15", urlenc(&format!("{artist} {title}"))))?["data"]);
    }
    let mine: Vec<Value> = found.into_iter().filter(|a| artist_key(&s(&a["artist"]["name"])) == want_artist && real_picture(&s(&a["cover_medium"]))).collect();
    let full = crate::util::norm(title);
    let exact = mine.iter().find(|a| crate::util::album_key(&s(&a["title"])) == want_title);
    let close = || {
        mine.iter().find(|a| {
            let t = crate::util::norm(&s(&a["title"]));
            full.chars().count() >= 4 && t.chars().count() >= 4 && (t.contains(&full) || full.contains(&t))
        })
    };
    exact.or_else(close).map(|a| s(&a["cover_medium"]))
}

/// The Bandcamp album page MusicBrainz links from any edition of this release group (cached a month).
pub fn mb_bandcamp_link(net: &Net, release_group: &str) -> Option<String> {
    let url = format!("https://musicbrainz.org/ws/2/release?release-group={release_group}&inc=url-rels&fmt=json&limit=25");
    let v = net.get_json("mb", &url, days(30), Some(&net.mb)).ok()?;
    arr(&v["releases"]).iter().flat_map(|r| arr(&r["relations"])).map(|rel| s(&rel["url"]["resource"])).find(|u| crate::util::is_bandcamp_page(u))
}
