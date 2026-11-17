//! Last.fm's desktop browser authorization. Auth responses never pass through the disk cache.
//! Release credentials come from build environment variables, never from a user's setup fields.
use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const VAULT: &str = "Last.fm session";
const ENDPOINT: &str = "https://ws.audioscrobbler.com/2.0/";

#[derive(Clone)]
struct Application { key: String, secret: String }

impl Application {
    fn bundled() -> Option<Self> {
        let key = option_env!("BMT_LASTFM_API_KEY").unwrap_or("").trim();
        let secret = option_env!("BMT_LASTFM_SHARED_SECRET").unwrap_or("").trim();
        (valid_key(key) && valid_key(secret)).then(|| Self { key: key.into(), secret: secret.into() })
    }
}

pub fn configured() -> bool { Application::bundled().is_some() }

fn valid_key(s: &str) -> bool { s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) }

// Authorization tokens and user session keys are opaque. Live Last.fm tokens include letters beyond
// a-f, despite older documentation describing them as MD5 hashes. Encode them when used in a URL.
fn valid_token(s: &str) -> bool { !s.is_empty() && s.len() <= 512 && s.bytes().all(|b| b.is_ascii_graphic()) }

#[derive(Clone, Serialize, Deserialize)]
struct Session { user: String, api_key: String, session_key: String }

pub fn restore(cfg: &mut crate::config::Config) {
    if cfg.lastfm_disconnected { return; }
    if let Some(s) = crate::secrets::get(VAULT).and_then(|e| serde_json::from_str::<Session>(&e.secret).ok()) {
        if !s.user.trim().is_empty() && valid_key(&s.api_key) && valid_token(&s.session_key) {
            cfg.lastfm_user = s.user;
            cfg.lastfm_api_key = s.api_key;
            cfg.lastfm_connected = true;
        }
    }
}

pub fn forget(cfg: &mut crate::config::Config) -> Result<(), String> {
    let mut disconnected = cfg.clone();
    disconnected.lastfm_disconnected = true;
    disconnected.lastfm_connected = false;
    disconnected.lastfm_user.clear();
    disconnected.lastfm_api_key.clear();
    disconnected.save_checked()?;
    *cfg = disconnected;
    crate::secrets::remove(VAULT);
    crate::secrets::remove(crate::secrets::LASTFM);
    Ok(())
}

fn signature(fields: &BTreeMap<&str, &str>, secret: &str) -> String {
    let mut text = String::new();
    for (name, value) in fields {
        if !matches!(*name, "format" | "callback" | "api_sig") { text.push_str(name); text.push_str(value); }
    }
    text.push_str(secret);
    format!("{:x}", md5::compute(text.as_bytes()))
}

#[derive(Debug)]
struct Failure { words: String, pending: bool }

fn failure(code: u64) -> Failure {
    let words = match code {
        14 => "Approve access on Last.fm, then return here.",
        4 | 15 => "This sign-in expired or was declined. Connect again to start over.",
        10 | 13 | 26 => "Last.fm sign-in is unavailable for this app. Please check for an app update.",
        29 => "Last.fm is receiving too many requests. Please try again in a few minutes.",
        _ => "Last.fm could not finish signing in. Please try again.",
    };
    Failure { words: words.into(), pending: code == 14 }
}

fn response(v: Value) -> Result<Value, Failure> {
    // Never display a service's raw error text, which could repeat request credentials.
    if let Some(code) = v.get("error").and_then(Value::as_u64) { Err(failure(code)) } else { Ok(v) }
}

fn call(app: &Application, method: &str, token: Option<&str>) -> Result<Value, Failure> {
    call_at(app, method, token, ENDPOINT)
}

fn call_at(app: &Application, method: &str, token: Option<&str>, endpoint: &str) -> Result<Value, Failure> {
    let mut fields = BTreeMap::from([("api_key", app.key.as_str()), ("method", method)]);
    if let Some(token) = token { fields.insert("token", token); }
    let sig = signature(&fields, &app.secret);
    fields.insert("api_sig", &sig);
    fields.insert("format", "json");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false).max_redirects(0).build().into();
    let mut request = agent.get(endpoint).header("User-Agent", crate::net::USER_AGENT);
    for (key, value) in fields { request = request.query(key, value); }
    let mut reply = request.call().map_err(|_| Failure { words: "Could not reach Last.fm. Check your connection and try again.".into(), pending: false })?;
    let status = reply.status().as_u16();
    if status == 429 { return Err(failure(29)); }
    let json = reply.body_mut().with_config().limit(64_000).read_json::<Value>().map_err(|_| failure(0))?;
    let value = response(json)?;
    if status != 200 { return Err(failure(0)); }
    Ok(value)
}

fn session(v: &Value, app: &Application) -> Result<Session, Failure> {
    let user = v["session"]["name"].as_str().unwrap_or("").trim();
    let key = v["session"]["key"].as_str().unwrap_or("");
    if user.is_empty() || !valid_token(key) { return Err(failure(0)); }
    Ok(Session { user: user.into(), api_key: app.key.clone(), session_key: key.into() })
}

enum Reply { Token(String), Session(Session), Pending }

#[derive(Default)]
pub struct Flow {
    rx: Option<Receiver<Result<Reply, Failure>>>,
    token: Option<(String, Instant)>,
    next: Option<Instant>,
    pub error: Option<String>,
}

impl Flow {
    pub fn active(&self) -> bool { self.rx.is_some() || self.token.is_some() }
    pub fn waiting(&self) -> bool { self.token.is_some() }
    pub fn cancel(&mut self) { *self = Self::default(); }

    pub fn start(&mut self, ctx: &egui::Context) {
        self.cancel();
        let Some(app) = Application::bundled() else {
            self.error = Some("Last.fm sign-in is unavailable in this version.".into());
            return;
        };
        self.request(app, None, ctx);
    }

    fn request(&mut self, app: Application, token: Option<String>, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = if let Some(token) = token {
                match call(&app, "auth.getSession", Some(&token)) {
                    Ok(v) => session(&v, &app).map(Reply::Session),
                    Err(e) if e.pending => Ok(Reply::Pending),
                    Err(e) => Err(e),
                }
            } else {
                call(&app, "auth.getToken", None).and_then(|v| {
                    let token = v["token"].as_str().unwrap_or("");
                    if valid_token(token) { Ok(Reply::Token(token.into())) } else { Err(failure(0)) }
                })
            };
            // A cancelled or replaced flow drops its receiver. No worker ever saves credentials.
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub fn open_browser(&mut self) {
        if let (Some((token, _)), Some(app)) = (&self.token, Application::bundled()) {
            let url = format!("https://www.last.fm/api/auth/?api_key={}&token={}", app.key, crate::util::urlenc(token));
            if open::that(url).is_err() {
                self.error = Some("Could not open your browser. Try Open Last.fm again.".into());
            } else { self.error = None; }
        }
    }

    /// Runs on the UI thread even when Settings is closed. True when a newly authorized account is saved.
    pub fn tick(&mut self, cfg: &mut crate::config::Config, ctx: &egui::Context) -> bool {
        let result = self.rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Disconnected) => Some(Err(failure(0))),
            Err(TryRecvError::Empty) => None,
        });
        if let Some(result) = result {
            self.rx = None;
            match result {
                Ok(Reply::Token(token)) => {
                    self.token = Some((token, Instant::now()));
                    self.next = Some(Instant::now() + Duration::from_secs(3));
                    self.open_browser();
                }
                Ok(Reply::Pending) => self.next = Some(Instant::now() + Duration::from_secs(3)),
                Ok(Reply::Session(s)) => {
                    self.cancel();
                    let previous = crate::secrets::get(VAULT);
                    let old_cfg = cfg.clone();
                    let kept = serde_json::to_string(&s).map_err(|_| "Could not save the Last.fm login.".to_string())
                        .and_then(|json| crate::secrets::set(VAULT, &s.user, &json));
                    if let Err(e) = kept { self.error = Some(e); return false; }
                    cfg.lastfm_user = s.user;
                    cfg.lastfm_api_key = s.api_key;
                    cfg.lastfm_connected = true;
                    cfg.lastfm_disconnected = false;
                    if let Err(why) = cfg.save_checked() {
                        *cfg = old_cfg;
                        if let Some(previous) = previous { let _ = crate::secrets::set(VAULT, &previous.user, &previous.secret); }
                        else { crate::secrets::remove(VAULT); }
                        self.error = Some(why);
                        return false;
                    }
                    return true;
                }
                Err(e) => { self.cancel(); self.error = Some(e.words); }
            }
        }
        if let Some((token, since)) = &self.token {
            if since.elapsed() >= Duration::from_secs(3600) {
                self.cancel();
                self.error = Some(failure(15).words);
            } else if self.rx.is_none() && self.next.is_some_and(|next| Instant::now() >= next) {
                if let Some(app) = Application::bundled() { self.request(app, Some(token.clone()), ctx); }
            }
        }
        if self.active() { ctx.request_repaint_after(Duration::from_millis(200)); }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Explicit maintainer check only; ordinary tests never contact Last.fm.
    #[test]
    #[ignore = "requires the registered app credentials and live Last.fm access"]
    fn registered_application_can_begin_browser_authorization() {
        let app = Application::bundled().expect("Build with the registered Last.fm application credentials.");
        let reply = call(&app, "auth.getToken", None).unwrap_or_else(|e| panic!("{}", e.words));
        let token = reply["token"].as_str().unwrap_or("");
        assert!(valid_token(token), "Last.fm returned no valid authorization token.");
        match call(&app, "auth.getSession", Some(token)) {
            Err(e) => assert!(e.pending, "{}", e.words),
            Ok(_) => panic!("A fresh token must require user approval before it can create a session."),
        }
    }

    #[test]
    fn signs_sorted_fields_without_output_options() {
        let fields = BTreeMap::from([("token", "xyz"), ("format", "json"), ("api_key", "abc"), ("method", "auth.getSession"), ("callback", "unused"), ("api_sig", "old")]);
        assert_eq!(signature(&fields, "secret"), format!("{:x}", md5::compute("api_keyabcmethodauth.getSessiontokenxyzsecret")));
    }

    #[test]
    fn pending_expired_and_malformed_replies_do_not_connect() {
        assert!(response(serde_json::json!({"error": 14})).err().unwrap().pending);
        assert!(!response(serde_json::json!({"error": 15})).err().unwrap().pending);
        let app = Application { key: "a".repeat(32), secret: "b".repeat(32) };
        assert!(session(&serde_json::json!({"session": {"name": "listener"}}), &app).is_err());
        assert!(session(&serde_json::json!({"session": {"name": "", "key": "c".repeat(32)}}), &app).is_err());
        let s = session(&serde_json::json!({"session": {"name": "listener", "key": "c".repeat(32)}}), &app).unwrap();
        assert_eq!(s.user, "listener");
        assert_eq!(s.api_key, app.key);
    }

    #[test]
    fn cancellation_discards_a_late_session_without_saving_it() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut flow = Flow { rx: Some(rx), ..Flow::default() };
        flow.cancel();
        assert!(tx.send(Ok(Reply::Session(Session { user: "cancelled".into(), api_key: "a".repeat(32), session_key: "b".repeat(32) }))).is_err());
        let mut cfg = crate::config::Config::default();
        assert!(!flow.tick(&mut cfg, &egui::Context::default()));
        assert!(!cfg.has_lastfm());
    }

    #[test]
    fn authorization_tokens_and_sessions_are_opaque_and_url_encoded() {
        assert!(valid_token("liveTokenWithLettersBeyondHex1234"));
        assert!(!valid_token("") && !valid_token("bad\nvalue") && !valid_token(&"x".repeat(513)));
        let app = Application { key: "a".repeat(32), secret: "b".repeat(32) };
        let s = session(&serde_json::json!({"session": {"name": "listener", "key": "sessionWithLettersBeyondHex12345"}}), &app).unwrap();
        assert_eq!(s.session_key, "sessionWithLettersBeyondHex12345");
        assert_eq!(crate::util::urlenc("opaque+token&x=1"), "opaque%2Btoken%26x%3D1");
    }

    #[test]
    fn account_survives_restart_disconnect_stays_disconnected_and_failed_save_keeps_old_account() {
        let dir = std::env::temp_dir().join(format!("bmt-lastfm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let before = std::env::var_os("MUSIC_RECS_DATA");
        unsafe { std::env::set_var("MUSIC_RECS_DATA", &dir); }
        crate::secrets::remove(VAULT);
        crate::secrets::remove(crate::secrets::LASTFM);
        let ctx = egui::Context::default();
        let mut cfg = crate::config::Config::default();
        let accept = |name: &str| {
            let (tx, rx) = crossbeam_channel::bounded(1);
            tx.send(Ok(Reply::Session(Session { user: name.into(), api_key: "a".repeat(32), session_key: "b".repeat(32) }))).ok().unwrap();
            Flow { rx: Some(rx), ..Flow::default() }
        };
        assert!(accept("first").tick(&mut cfg, &ctx));
        let loaded = crate::config::Config::load();
        assert!(loaded.lastfm_connected && loaded.has_lastfm());
        assert_eq!(loaded.lastfm_user, "first");
        let json = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!json.contains(&"b".repeat(32)));

        crate::secrets::REFUSE.with(|r| r.set(true));
        let mut refused = accept("second");
        assert!(!refused.tick(&mut cfg, &ctx));
        crate::secrets::REFUSE.with(|r| r.set(false));
        assert!(refused.error.is_some());
        assert_eq!(crate::config::Config::load().lastfm_user, "first");

        // An unwritable settings destination must not replace or disconnect the saved account.
        std::fs::rename(dir.join("config.json"), dir.join("saved.json")).unwrap();
        std::fs::create_dir(dir.join("config.json")).unwrap();
        assert!(forget(&mut cfg).is_err());
        assert_eq!(cfg.lastfm_user, "first");
        assert!(!accept("second").tick(&mut cfg, &ctx));
        assert_eq!(cfg.lastfm_user, "first");
        std::fs::remove_dir(dir.join("config.json")).unwrap();
        std::fs::rename(dir.join("saved.json"), dir.join("config.json")).unwrap();
        assert_eq!(crate::config::Config::load().lastfm_user, "first");

        forget(&mut cfg).unwrap();
        // Even if a legacy credential is left behind, an explicit disconnect takes priority.
        crate::secrets::set(crate::secrets::LASTFM, "", &"a".repeat(32)).unwrap();
        let loaded = crate::config::Config::load();
        assert!(!loaded.has_lastfm() && !loaded.lastfm_connected && loaded.lastfm_disconnected);
        assert!(loaded.lastfm_user.is_empty());
        assert!(accept("second").tick(&mut cfg, &ctx));
        assert_eq!(crate::config::Config::load().lastfm_user, "second");
        forget(&mut cfg).unwrap();
        unsafe {
            match before { Some(p) => std::env::set_var("MUSIC_RECS_DATA", p), None => std::env::remove_var("MUSIC_RECS_DATA") }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn http_exchange_signs_requests_handles_pending_and_does_not_leak_server_errors() {
        use std::io::{Read, Write};
        let app = Application { key: "a".repeat(32), secret: "b".repeat(32) };
        let token = "g".repeat(32);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/2.0/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for body in [
                serde_json::json!({"token": "g".repeat(32)}),
                serde_json::json!({"error": 14, "message": "private-server-text"}),
                serde_json::json!({"session": {"name": "listener", "key": "d".repeat(32)}}),
                serde_json::json!({"error": 15, "message": "private-server-text"}),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = body.to_string();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            requests
        });
        assert_eq!(call_at(&app, "auth.getToken", None, &endpoint).unwrap()["token"], token);
        assert!(call_at(&app, "auth.getSession", Some(&token), &endpoint).err().unwrap().pending);
        let v = call_at(&app, "auth.getSession", Some(&token), &endpoint).unwrap();
        assert_eq!(session(&v, &app).unwrap().user, "listener");
        let error = call_at(&app, "auth.getSession", Some(&token), &endpoint).err().unwrap();
        assert!(!error.pending && !error.words.contains("private-server-text"));
        let requests = server.join().unwrap();
        for (i, request) in requests.iter().enumerate() {
            let method = if i == 0 { "auth.getToken" } else { "auth.getSession" };
            let mut fields = BTreeMap::from([("api_key", app.key.as_str()), ("method", method)]);
            if i > 0 { fields.insert("token", token.as_str()); }
            assert!(request.contains(&format!("api_sig={}", signature(&fields, &app.secret))));
            assert!(!request.contains(&app.secret));
        }
    }
}
