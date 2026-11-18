use crate::util::fnv64;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

/// How the app names itself to every web service it asks (MusicBrainz asks for a name, a version and a way
/// to reach the author): the public repository is the contact, never an email address.
pub const USER_AGENT: &str = concat!("BradleysMusicTool/", env!("CARGO_PKG_VERSION"), " ( https://github.com/BradChadley/bradleys-music-tool )");

/// Spaces requests out so we stay under each service's rate limit.
pub struct Limiter {
    gap: Duration,
    next: Mutex<Instant>,
}

impl Limiter {
    pub fn new(gap: Duration) -> Self {
        Self { gap, next: Mutex::new(Instant::now()) }
    }
    pub fn wait(&self) {
        let mut next = self.next.lock().unwrap();
        let now = Instant::now();
        if *next > now {
            sleep(*next - now);
        }
        *next = Instant::now() + self.gap;
    }
    /// Push the next allowed request further out (after a 503/429).
    pub fn back_off(&self, d: Duration) {
        let mut next = self.next.lock().unwrap();
        *next = Instant::now() + d;
    }
}

pub struct Net {
    agent: ureq::Agent,
    pub cache_dir: PathBuf,
    pub mb: Limiter,
    pub itunes: Limiter,
    pub lastfm: Limiter,
    /// Deezer allows 50 requests per 5 seconds; this stays well under
    pub deezer: Limiter,
    pub bandcamp: Limiter,
    /// tests only: every address is asked of this local stand-in server instead (`stand_in`)
    stand_in: Option<String>,
}

impl Net {
    pub fn new(cache_dir: PathBuf) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .https_only(true) // never plain http, a redirect included
            .user_agent(USER_AGENT)
            .build()
            .into();
        Self {
            agent,
            cache_dir,
            mb: Limiter::new(Duration::from_millis(1100)),
            itunes: Limiter::new(Duration::from_millis(3100)),
            lastfm: Limiter::new(Duration::from_millis(220)),
            deezer: Limiter::new(Duration::from_millis(150)),
            bandcamp: Limiter::new(Duration::from_millis(1000)),
            stand_in: None,
        }
    }

    /// Tests only: a `Net` that asks a stand-in server on this PC (`base`, "http://127.0.0.1:<port>") for
    /// every address, as "<base>/<host><path>", with no waiting between requests. The real services are never
    /// reached; the cache is keyed by the real address as always.
    #[cfg(test)]
    pub fn stand_in(cache_dir: PathBuf, base: &str) -> Self {
        assert!(base.starts_with("http://127.0.0.1:"), "a stand-in is on this PC only");
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).http_status_as_error(false).user_agent(USER_AGENT).build().into();
        let quick = || Limiter::new(Duration::from_millis(1));
        Self { agent, cache_dir, mb: quick(), itunes: quick(), lastfm: quick(), deezer: quick(), bandcamp: quick(), stand_in: Some(base.trim_end_matches('/').to_string()) }
    }

    /// The address a request goes to: the real one, or the stand-in's (tests).
    fn to(&self, url: &str) -> String {
        match (&self.stand_in, url.strip_prefix("https://")) {
            (Some(base), Some(rest)) => format!("{base}/{rest}"),
            _ => url.to_string(),
        }
    }

    /// POST a JSON body, with the same disk cache (keyed by URL + body) and limiter as GETs. Only successes are cached.
    pub fn post_json(&self, ns: &str, url: &str, body: &serde_json::Value, ttl: Duration, limiter: &Limiter) -> Result<serde_json::Value, String> {
        only_https(url)?;
        let key = format!("{url}\n{body}");
        if let Some(b) = self.cache_get(ns, &key, ttl) {
            if let Ok(v) = serde_json::from_slice(&b) {
                return Ok(v);
            }
        }
        let mut last_err = String::new();
        for attempt in 0..3 {
            limiter.wait();
            match self.agent.post(self.to(url)).send_json(body) {
                Ok(mut r) if r.status().as_u16() == 200 => {
                    let b = r.body_mut().read_to_vec().map_err(|e| e.to_string())?;
                    let v: serde_json::Value = serde_json::from_slice(&b).map_err(|e| e.to_string())?;
                    self.cache_put(ns, &key, &b);
                    return Ok(v);
                }
                Ok(r) => last_err = format!("HTTP {}", r.status().as_u16()),
                Err(e) => last_err = e.to_string(),
            }
            sleep(Duration::from_secs(2 * (attempt + 1)));
        }
        Err(last_err)
    }

    fn cache_path(&self, ns: &str, key: &str) -> PathBuf {
        self.cache_dir.join(ns).join(format!("{:016x}", fnv64(key)))
    }

    fn cache_get(&self, ns: &str, key: &str, ttl: Duration) -> Option<Vec<u8>> {
        let p = self.cache_path(ns, key);
        let meta = std::fs::metadata(&p).ok()?;
        let age = SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or_default();
        if age > ttl {
            return None;
        }
        std::fs::read(&p).ok()
    }

    fn cache_put(&self, ns: &str, key: &str, data: &[u8]) {
        let p = self.cache_path(ns, key);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::util::write_atomic(&p, data);
    }

    /// GET with disk cache and rate limiting. Only successful responses are cached.
    pub fn get_bytes(&self, ns: &str, url: &str, ttl: Duration, limiter: Option<&Limiter>) -> Result<Vec<u8>, String> {
        only_https(url)?;
        if let Some(b) = self.cache_get(ns, url, ttl) {
            return Ok(b);
        }
        let mut last_err = String::new();
        for attempt in 0..4 {
            if let Some(l) = limiter {
                l.wait();
            }
            let resp = self.agent.get(self.to(url)).call();
            let mut resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.to_string();
                    sleep(Duration::from_secs(2 * (attempt + 1)));
                    continue;
                }
            };
            let status = resp.status().as_u16();
            let body = resp
                .body_mut()
                .with_config()
                .limit(60_000_000)
                .read_to_vec()
                .map_err(|e| e.to_string());
            match status {
                200 => {
                    let body = body?;
                    // Last.fm sometimes answers 200 with an error object
                    if !body.starts_with(b"{\"error\"") {
                        self.cache_put(ns, url, &body);
                    }
                    return Ok(body);
                }
                429 | 403 | 503 | 502 | 500 => {
                    // rate limited or temporary trouble: wait and retry
                    let wait = if ns == "itunes" { 30 } else { 3 } * (attempt + 1);
                    if let Some(l) = limiter {
                        l.back_off(Duration::from_secs(wait));
                    } else {
                        sleep(Duration::from_secs(wait));
                    }
                    last_err = format!("HTTP {status}");
                }
                _ => {
                    let text = body.map(|b| String::from_utf8_lossy(&b).chars().take(200).collect::<String>()).unwrap_or_default();
                    return Err(format!("HTTP {status}: {text}"));
                }
            }
        }
        Err(last_err)
    }

    pub fn get_json(&self, ns: &str, url: &str, ttl: Duration, limiter: Option<&Limiter>) -> Result<serde_json::Value, String> {
        let b = self.get_bytes(ns, url, ttl, limiter)?;
        match serde_json::from_slice(&b) {
            Ok(v) => Ok(v),
            Err(_) => {
                // a damaged cache file (or a one-off bad response): drop it and fetch once more
                let _ = std::fs::remove_file(self.cache_path(ns, url));
                let b = self.get_bytes(ns, url, ttl, limiter)?;
                serde_json::from_slice(&b).map_err(|e| crate::util::plain_err(format!("bad response from {ns}: {e}")))
            }
        }
    }

    /// Keeps the lookup cache from growing forever. Run once at start, in the background: answers older than
    /// two months go (they are long past the point of being used), pictures older than about a year go, and
    /// if what is left is still over 300 MB the oldest go first until it is not.
    pub fn trim_cache(&self) {
        let now = std::time::SystemTime::now();
        let mut kept: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = vec![];
        for ns in std::fs::read_dir(&self.cache_dir).into_iter().flatten().flatten() {
            if !ns.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let max_age = if ns.file_name() == "img" { days(400) } else { days(60) };
            for f in std::fs::read_dir(ns.path()).into_iter().flatten().flatten() {
                let Ok(md) = f.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let modified = md.modified().unwrap_or(now);
                if now.duration_since(modified).map(|age| age > max_age).unwrap_or(false) {
                    let _ = std::fs::remove_file(f.path());
                } else {
                    kept.push((modified, md.len(), f.path()));
                }
            }
        }
        const CAP: u64 = 300 * 1024 * 1024;
        let mut total: u64 = kept.iter().map(|k| k.1).sum();
        if total > CAP {
            kept.sort_by_key(|k| k.0);
            for (_, size, path) in kept {
                if total <= CAP {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total -= size;
                }
            }
        }
    }
    pub fn clear_cache(&self) {
        for ns in ["lastfm", "mb", "itunes", "wiki", "bandcamp", "deezer"] {
            let _ = std::fs::remove_dir_all(self.cache_dir.join(ns));
        }
    }
}

/// Only https addresses are asked for. Some addresses come out of answers (a picture, a page somebody added to
/// a public database): one that is plain http, or no web address at all, is refused before anything is sent.
fn only_https(url: &str) -> Result<(), String> {
    if url.starts_with("https://") && crate::util::is_web_address(url) { Ok(()) } else { Err("not an https address".into()) }
}

pub fn days(n: u64) -> Duration {
    Duration::from_secs(n * 86_400)
}
pub fn hours(n: u64) -> Duration {
    Duration::from_secs(n * 3600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_sent_to_web_services_carries_the_version_and_the_repository() {
        assert!(USER_AGENT.starts_with(&format!("BradleysMusicTool/{} ", env!("CARGO_PKG_VERSION"))));
        assert!(USER_AGENT.contains(crate::update::REPO), "the contact is the one address the app knows");
        assert!(!USER_AGENT.contains('@'));
    }

    #[test]
    fn nothing_is_asked_for_over_plain_http() {
        let dir = std::env::temp_dir().join(format!("music-recs-net-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let net = Net::new(dir.clone());
        let t = Instant::now();
        // refused before a connection is tried (nothing listens on this port, and no time is spent trying)
        for bad in ["http://127.0.0.1:9/cover.jpg", "ftp://example.com/x", "file:///C:/x", r"\server\share\x.jpg", "https://", ""] {
            assert_eq!(net.get_bytes("t", bad, days(1), None), Err("not an https address".to_string()), "{bad}");
        }
        assert!(net.post_json("t", "http://127.0.0.1:9/api", &serde_json::json!({}), days(1), &net.bandcamp).is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(only_https("https://ws.audioscrobbler.com/2.0/?method=x").is_ok());
        // and the agent itself refuses plain http, so a redirect from https to http is not followed either
        let said = net.agent.get("http://127.0.0.1:9/x").call().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(said.to_lowercase().contains("https"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
