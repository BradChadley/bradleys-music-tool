//! Finds the best page to open when you click an album:
//! Wikipedia article, else the Bandcamp album page, else the streaming/MusicBrainz link we already have,
//! else a Bandcamp search.
use crate::net::{days, Net};
use crate::util::{album_key, artist_key, norm, strip_edition, urlenc};
use regex::Regex;
use std::sync::LazyLock;

static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static PAREN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\(([^)]*)\)\s*$").unwrap());
static MB_RG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:musicbrainz\.org|coverartarchive\.org)/release-group/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})").unwrap());
static OG_IMAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<meta property="og:image" content="([^"]+)""#).unwrap());

/// Returns the URL to open. `fallback` is a link we already know (Apple Music / MusicBrainz), may be empty.
/// `mb_link`: the release's MusicBrainz (or Cover Art Archive) link, if any, used to find its Bandcamp page.
pub fn find(net: &Net, artist: &str, title: &str, fallback: &str, mb_link: &str) -> String {
    if let Some(u) = wikipedia(net, artist, title) {
        return u;
    }
    if let Some(u) = bandcamp(net, artist, title, if mb_link.is_empty() { fallback } else { mb_link }) {
        return u;
    }
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    format!("https://bandcamp.com/search?q={}&item_type=a", urlenc(&format!("{artist} {title}")))
}

fn wikipedia(net: &Net, artist: &str, title: &str) -> Option<String> {
    let clean = strip_edition(title);
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&srlimit=6&srprop=snippet&format=json",
        urlenc(&format!("{clean} {artist}"))
    );
    let v = net.get_json("wiki", &url, days(30), None).ok()?;
    let want = norm(&clean);
    let who = norm(artist);
    for item in v["query"]["search"].as_array()? {
        let page = item["title"].as_str()?;
        let (base, paren) = match PAREN.captures(page) {
            Some(c) => (page[..c.get(0)?.start()].to_string(), c[1].to_lowercase()),
            None => (page.to_string(), String::new()),
        };
        if norm(&base) != want {
            continue;
        }
        let snippet = norm(&TAGS.replace_all(item["snippet"].as_str().unwrap_or(""), ""));
        let kind_ok = ["album", "ep", "mixtape", "soundtrack", "single"].iter().any(|k| paren.split_whitespace().any(|w| w == *k));
        let artist_ok = snippet.contains(&who) || norm(&paren).contains(&who);
        if artist_ok && (kind_ok || paren.is_empty()) {
            return Some(format!("https://en.wikipedia.org/wiki/{}", urlenc(&page.replace(' ', "_")).replace("%3A", ":")));
        }
    }
    None
}

/// The MusicBrainz release group in a MusicBrainz or Cover Art Archive link, if there is one.
pub fn mb_release_group(link: &str) -> Option<String> {
    MB_RG.captures(link).map(|c| c[1].to_string())
}

/// The album's Bandcamp page:
///   1. the Bandcamp link MusicBrainz has on one of the release's editions (exact, no guessing)
///   2. Bandcamp's own search: same artist, and the same title or one containing the other
///      ("Babel Archives : Imprisoned" / "Babel Archives: Imprisoned (Original Soundtrack)")
/// (Bandcamp's search *page* now answers scripts with a bot check, so its search API is used instead.)
pub fn bandcamp(net: &Net, artist: &str, title: &str, mb_link: &str) -> Option<String> {
    if let Some(u) = mb_release_group(mb_link).and_then(|id| crate::sources::mb_bandcamp_link(net, &id)) {
        return Some(u);
    }
    bandcamp_search(net, artist, title).map(|(url, _)| url)
}

fn same_title(a: &str, b: &str) -> bool {
    let (ka, kb) = (album_key(a), album_key(b));
    if ka.is_empty() || kb.is_empty() {
        return false;
    }
    if ka == kb {
        return true;
    }
    let (fa, fb) = (norm(a), norm(b));
    fa.chars().count() >= 6 && fb.chars().count() >= 6 && (fa.contains(&fb) || fb.contains(&fa) || fa.contains(&kb) || fb.contains(&ka))
}

/// (album page, cover) from Bandcamp's search API, for this artist only.
fn bandcamp_search(net: &Net, artist: &str, title: &str) -> Option<(String, String)> {
    let body = serde_json::json!({ "search_text": format!("{artist} {}", strip_edition(title)), "search_filter": "a", "full_page": false, "fan_id": null });
    let v = net.post_json("bandcamp", "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic", &body, days(7), &net.bandcamp).ok()?;
    let who = artist_key(artist);
    v["auto"]["results"].as_array()?.iter().find_map(|r| {
        let (name, band, url) = (r["name"].as_str()?, r["band_name"].as_str()?, r["item_url_path"].as_str()?);
        if r["type"].as_str() != Some("a") || artist_key(band) != who || !same_title(name, title) {
            return None;
        }
        let art = r["art_id"].as_u64().map(|id| format!("https://f4.bcbits.com/img/a{id:010}_2.jpg")).unwrap_or_default();
        Some((url.split('?').next().unwrap_or(url).to_string(), art))
    })
}

/// A cover from the album's Bandcamp page (found as above). The page's own preview image is used.
pub fn bandcamp_cover(net: &Net, artist: &str, title: &str, mb_link: &str) -> Option<String> {
    if let Some(page) = mb_release_group(mb_link).and_then(|id| crate::sources::mb_bandcamp_link(net, &id)) {
        let html = String::from_utf8_lossy(&net.get_bytes("bandcamp", &page, days(30), Some(&net.bandcamp)).ok()?).into_owned();
        // "…/img/a2124919287_5.jpg" (700px) -> "_2" (350px), plenty for a thumbnail
        return OG_IMAGE.captures(&html).map(|c| c[1].replace("_5.jpg", "_2.jpg").replace("_10.jpg", "_2.jpg"));
    }
    bandcamp_search(net, artist, title).map(|(_, art)| art).filter(|a| !a.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore]
    fn live() {
        let net = crate::net::Net::new(std::env::temp_dir().join("albumlink-test"));
        for (a, t) in [("Radiohead", "OK Computer"), ("David Bowie", "Heroes"), ("Arcade Fire", "The Suburbs (Deluxe Edition)"), ("Addison Rae", "Addison"), ("Addison Rae", "Diet Pepsi"), ("Addison Rae", "AR +"), ("Some Tiny Band", "Nonexistent Demo")] {
            println!("{a} - {t} => {}", super::find(&net, a, t, "FALLBACK", ""));
        }
    }
}

#[cfg(test)]
mod bandcamp_tests {
    #[test]
    #[ignore]
    fn live_bandcamp() {
        let net = crate::net::Net::new(std::env::temp_dir().join("albumlink-bc-test"));
        let mb = "https://musicbrainz.org/release-group/399f4f51-0dd2-4949-b1fc-d6068acdfbeb";
        let cases = [("death's dynamic shroud", "Babel Archives : Imprisoned", mb), ("death's dynamic shroud", "Heavy Black Heart", ""), ("2slimey", "Totalbass", ""), ("Radiohead", "OK Computer", "")];
        for (a, t, m) in cases {
            println!("{a} - {t}\n   page:  {:?}\n   cover: {:?}", super::bandcamp(&net, a, t, m), super::bandcamp_cover(&net, a, t, m));
        }
    }
}
