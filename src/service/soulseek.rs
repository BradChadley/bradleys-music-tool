//! The engine's own Soulseek client: its slskd, started when it is first needed, signed in with the login the
//! engine keeps (`Cadence: Soulseek` in Credential Manager), and ended with the engine.
//!
//! How slskd is started is behind `Launch`: the real program (`Spawned`), or a stand-in in the tests. Everything
//! after the start talks to it over its web side (`slskd::Client`) the same way for both.
use super::jobs::{fail, Failure};
use super::Job;
use crate::secrets::Entry;
use crate::slskd::{self, Client};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a start may take to be signed in before it counts as offline.
pub(crate) const START_WAIT: Duration = Duration::from_secs(45);
/// A client that stops by itself is started again at most this many times in a row before the engine says
/// why it stopped instead.
const MAX_STARTS: u32 = 4;

/// Starts and stops the Soulseek client.
pub(crate) trait Launch: Send + Sync {
    /// The client is there to be started.
    fn installed(&self) -> bool;
    /// Starts it (ending one that runs) signed in with `login`, saving into `downloads`. Returns how to talk to it.
    fn start(&self, login: &Entry, downloads: &Path) -> Result<Client, String>;
    fn stop(&self);
    /// It was started and has not stopped.
    fn running(&self) -> bool;
    /// Soulseek turned down the login of the latest start.
    fn refused(&self) -> bool;
    /// What it said when it stopped, in words for a person.
    fn why_stopped(&self) -> Option<String>;
}

/// The real slskd, from `<Tools>\slskd` (`slskd::root` is in the engine's folder for the whole run). Windows
/// ends it with the engine whatever happens (`slskd::spawn_as` puts it in a job object).
#[derive(Default)]
pub(crate) struct Spawned {
    child: Mutex<Option<std::process::Child>>,
}

impl Launch for Spawned {
    fn installed(&self) -> bool {
        slskd::exe().is_file()
    }
    fn start(&self, login: &Entry, downloads: &Path) -> Result<Client, String> {
        self.stop();
        std::fs::create_dir_all(downloads).map_err(|e| crate::helpers::said("the engine's downloads folder could not be made.", &e))?;
        crate::helpers::write_settings(&slskd::settings_file(), Some(downloads))?;
        let set = slskd::read_settings()?;
        let (child, ports) = slskd::spawn_as(Some(downloads), &set, Some(login))?;
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        Ok(Client::at(&set, ports.web))
    }
    fn stop(&self) {
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    fn running(&self) -> bool {
        self.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }
    fn refused(&self) -> bool {
        slskd::login_refused()
    }
    fn why_stopped(&self) -> Option<String> {
        slskd::why_it_stopped()
    }
}

/// Where the client stands, as `soulseek.status` says it.
pub(crate) enum State {
    Ready(Client),
    NoHelper,
    NoLogin,
    Starting,
    Refused,
    Offline(String),
}

#[derive(Default)]
struct Inner {
    client: Option<Client>,
    /// when the running client was started, and with which login
    started: Option<Instant>,
    with: Option<Entry>,
    /// starts in a row that never got signed in
    starts: u32,
    /// a new login is being tried: the client is the sign-in's until it ends
    signing: bool,
}

pub(crate) struct Soulseek {
    launch: Arc<dyn Launch>,
    /// the Credential Manager entry the login is kept under
    login: String,
    pub(crate) downloads: PathBuf,
    inner: Mutex<Inner>,
    /// searches, downloads and sign-ins under way that need the client
    busy: std::sync::atomic::AtomicUsize,
    /// when the client was last needed or asked about
    last_use: Mutex<Instant>,
}

/// The client is in use while this lives (see `Soulseek::using`).
pub(crate) struct InUse<'a>(&'a Soulseek);

impl Drop for InUse<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Soulseek {
    pub(crate) fn new(launch: Arc<dyn Launch>, login: String, downloads: PathBuf) -> Soulseek {
        Soulseek { launch, login, downloads, inner: Mutex::default(), busy: Default::default(), last_use: Mutex::new(Instant::now()) }
    }

    pub(crate) fn kept(&self) -> Option<Entry> {
        crate::secrets::get(&self.login).filter(|e| !e.user.is_empty())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One look, never waiting: with `start`, a client that is not running is started (with the kept login),
    /// and one started with another login is started again.
    /// Marks the client as needed now (an idle stop counts from here).
    pub(crate) fn touch(&self) {
        *self.last_use.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Marks the client as in use until the answer is dropped: it is never stopped for being idle meanwhile.
    pub(crate) fn using(&self) -> InUse<'_> {
        self.busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.touch();
        InUse(self)
    }

    /// Ends the client when it has had nothing to do for `idle`: nothing using it, no look at its state, and
    /// nothing in its list still waiting or arriving. True when it was stopped. The next need starts it again.
    pub(crate) fn stop_if_idle(&self, idle: Duration) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        let quiet = || self.busy.load(SeqCst) == 0 && self.last_use.lock().unwrap_or_else(|e| e.into_inner()).elapsed() >= idle;
        let client = {
            let g = self.lock();
            if g.signing || !quiet() {
                return false;
            }
            match &g.client {
                Some(c) if self.launch.running() => c.clone(),
                _ => return false,
            }
        };
        // (a download left in its list, waiting or arriving, is still something to do)
        if client.downloads().is_ok_and(|t| t.iter().any(|t| !t.finished())) {
            self.touch();
            return false;
        }
        let mut g = self.lock();
        if g.signing || !quiet() {
            return false;
        }
        self.launch.stop();
        *g = Inner::default();
        let secs = idle.as_secs_f64();
        let words = if secs >= 60.0 { format!("{:.0} minutes", secs / 60.0) } else { format!("{secs:.1} seconds") };
        eprintln!("cadence-engine: the Soulseek client had nothing to do for {words} and was stopped; the next need starts it again");
        true
    }

    pub(crate) fn look(&self, start: bool) -> State {
        if !self.launch.installed() {
            return State::NoHelper;
        }
        let Some(login) = self.kept() else { return State::NoLogin };
        let mut just_started = false;
        let (client, started) = {
            let mut g = self.lock();
            if g.signing {
                return State::Starting;
            }
            let running = g.client.is_some() && self.launch.running();
            if running && g.with.as_ref() != Some(&login) {
                self.launch.stop();
                g.client = None;
            }
            if g.client.is_none() || !self.launch.running() {
                if g.client.is_some() {
                    // it stopped by itself
                    g.client = None;
                    crate::config::log(&format!("soulseek (engine): the client stopped: {}", self.launch.why_stopped().unwrap_or_default()));
                }
                if !start {
                    return State::Offline("Soulseek is not connected yet; it connects when it is needed.".into());
                }
                if g.starts >= MAX_STARTS {
                    return State::Offline(self.launch.why_stopped().unwrap_or_else(|| "The Soulseek client stopped each time it was started.".into()));
                }
                eprintln!("cadence-engine: starting the Soulseek client");
                match self.launch.start(&login, &self.downloads) {
                    Ok(c) => {
                        g.client = Some(c);
                        g.started = Some(Instant::now());
                        g.with = Some(login);
                        g.starts += 1;
                        just_started = true;
                    }
                    Err(why) => return State::Offline(why),
                }
            }
            (g.client.clone(), g.started)
        };
        let Some(client) = client else { return State::Starting };
        if just_started {
            // (it cannot be answering yet: said at once)
            return State::Starting;
        }
        let young = started.is_some_and(|t| t.elapsed() < START_WAIT);
        // (still coming up: said at once, not after a request that takes seconds to fail)
        if !client.listening() {
            return if self.launch.refused() { State::Refused } else if young { State::Starting } else { State::Offline("The Soulseek client started but is not answering.".into()) };
        }
        match client.server() {
            Ok(s) if s.logged_in => {
                self.lock().starts = 0;
                State::Ready(client)
            }
            _ if self.launch.refused() => State::Refused,
            _ if young => State::Starting,
            Ok(_) => State::Offline("Soulseek could not be reached. The client is running but is not signed in.".into()),
            Err(_) => State::Offline("The Soulseek client started but is not answering.".into()),
        }
    }

    /// The client, signed in: started if need be, waiting up to `START_WAIT` (a stopped job stops waiting).
    pub(crate) fn ready(&self, job: &Job) -> Result<Client, Failure> {
        let until = Instant::now() + START_WAIT + Duration::from_secs(1);
        loop {
            if job.stopped() {
                return Err(fail("cancelled", "It was stopped before Soulseek was ready."));
            }
            match self.look(true) {
                State::Ready(c) => return Ok(c),
                State::NoHelper => return Err(fail("no_helper", "The Soulseek client is not here yet. Fetch it first.")),
                State::NoLogin => return Err(fail("no_login", "Sign in to Soulseek first.")),
                State::Refused => return Err(fail("sign_in_refused", REFUSED)),
                State::Offline(why) => return Err(fail("offline", why)),
                State::Starting if Instant::now() > until => return Err(fail("offline", "Soulseek could not be reached in 45 seconds.")),
                State::Starting => std::thread::sleep(Duration::from_millis(150)),
            }
        }
    }

    /// Tries a name and password, and keeps them only when Soulseek takes them. A refused or unreachable
    /// login is never kept; the client is then stopped, and the next need starts it with the login kept before.
    pub(crate) fn sign_in(&self, job: &Job, name: &str, password: &str) -> Result<String, Failure> {
        let _use = self.using();
        slskd::check_login(name, password).map_err(|m| fail("bad_request", m))?;
        if !self.launch.installed() {
            return Err(fail("no_helper", "The Soulseek client is not here yet. Fetch it first."));
        }
        let login = Entry { user: name.trim().to_string(), secret: password.to_string() };
        let client = {
            let mut g = self.lock();
            if g.signing {
                return Err(fail("busy", "Soulseek is already signing in."));
            }
            *g = Inner { signing: true, ..Inner::default() };
            match self.launch.start(&login, &self.downloads) {
                Ok(c) => c,
                Err(m) => {
                    g.signing = false;
                    return Err(fail("offline", m));
                }
            }
        };
        let until = Instant::now() + START_WAIT;
        let ended = loop {
            if job.stopped() {
                break Err(fail("cancelled", "Signing in was stopped; the login was not kept."));
            }
            let state = if client.listening() { client.server() } else { Err(String::new()) };
            match state {
                Ok(s) if s.logged_in => break Ok(()),
                _ if self.launch.refused() => break Err(fail("sign_in_refused", REFUSED)),
                _ if !self.launch.running() => {
                    break Err(fail("offline", self.launch.why_stopped().unwrap_or_else(|| "The Soulseek client stopped before it was signed in.".into())))
                }
                _ if Instant::now() > until => break Err(fail("offline", "Soulseek could not be reached in 45 seconds. Check the internet connection and try again.")),
                _ => std::thread::sleep(Duration::from_millis(150)),
            }
        };
        let mut g = self.lock();
        let kept = ended.and_then(|()| crate::secrets::set(&self.login, &login.user, &login.secret).map_err(|why| fail("failed", why)));
        if let Err(f) = kept {
            self.launch.stop();
            *g = Inner::default();
            return Err(f);
        }
        *g = Inner { client: Some(client), started: Some(Instant::now()), with: Some(login.clone()), starts: 0, signing: false };
        Ok(login.user)
    }

    /// Takes the kept login out and stops the client.
    pub(crate) fn sign_out(&self) {
        let mut g = self.lock();
        crate::secrets::remove(&self.login);
        self.launch.stop();
        *g = Inner::default();
    }

    /// Ends the client (the engine is ending).
    pub(crate) fn stop(&self) {
        let mut g = self.lock();
        self.launch.stop();
        *g = Inner::default();
    }
}

pub(crate) const REFUSED: &str = "Soulseek did not take that name and password. The name may belong to someone else, or the password is not the one it was made with.";
