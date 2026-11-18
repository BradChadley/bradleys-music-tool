//! What each request does. Every error is a (code, sentence) pair: the code for Cadence to act on, the
//! sentence in plain words for a person to read.
use super::{Env, Job, Out};
use crate::config::Config;
use crate::helpers::{self, Helper, State};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

pub(crate) type Failure = (String, String);

pub(crate) fn fail(code: &str, message: impl Into<String>) -> Failure {
    (code.to_string(), message.into())
}

pub(crate) const OFFLINE: &str = "offline_test";

// ------------------------------------------------------------------ settings

fn settings_file(env: &Env) -> PathBuf {
    env.dir.join("config.json")
}

pub(crate) fn load(env: &Env) -> Result<Config, Failure> {
    match std::fs::read(settings_file(env)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(fail("failed", crate::helpers::said("the engine's settings could not be read.", &e))),
        Ok(b) => serde_json::from_slice(&b).map_err(|e| fail("failed", format!("The engine's settings file is damaged ({e}). Nothing was changed; the file is left as it is."))),
    }
}

/// The settings as they are kept (never a secret: the Last.fm key is not part of what is written or sent).
fn shown(c: &Config) -> Value {
    serde_json::to_value(c).unwrap_or(Value::Null)
}

pub(crate) fn settings_get(env: &Env) -> Result<Value, Failure> {
    Ok(json!({ "settings": shown(&load(env)?) }))
}

/// Changes some settings: `{"changes": {"name": value, ...}}`. Only names the settings already have are
/// taken, each must hold the right kind of value, and the file is replaced whole (written beside it, then
/// renamed), so a failed save leaves the old one.
pub(crate) fn settings_set(env: &Env, params: &Value) -> Result<Value, Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Settings are not saved while Cadence runs as a test."));
    }
    let Some(changes) = params.get("changes").and_then(Value::as_object) else {
        return Err(fail("bad_request", "A settings change needs \"changes\": the settings to change and their new values."));
    };
    let mut now = shown(&load(env)?);
    let Some(fields) = now.as_object_mut() else { return Err(fail("failed", "The engine's settings could not be read.")) };
    for (name, value) in changes {
        let Some(slot) = fields.get_mut(name) else {
            return Err(fail("bad_request", format!("There is no setting called \"{name}\".")));
        };
        *slot = value.clone();
    }
    let new: Config = serde_json::from_value(now).map_err(|e| fail("bad_request", format!("A new value does not fit its setting ({e}). Nothing was changed.")))?;
    if !matches!(new.library_changes.as_str(), "look" | "repair" | "organize") {
        return Err(fail("bad_request", "The library setting (library_changes) is look, repair or organize. Nothing was changed."));
    }
    save(env, &new)?;
    Ok(json!({ "settings": shown(&new) }))
}

/// Writes the settings whole: beside the old file, then renamed over it, so a failed save leaves the old one.
/// (Never a secret: the Last.fm key is not part of what is written.)
pub(crate) fn save(env: &Env, new: &Config) -> Result<(), Failure> {
    let text = serde_json::to_vec_pretty(new).map_err(|e| fail("failed", format!("The settings could not be written out ({e}).")))?;
    let file = settings_file(env);
    let part = file.with_extension("json.part");
    std::fs::create_dir_all(&env.dir)
        .and_then(|_| std::fs::write(&part, &text))
        .and_then(|_| std::fs::rename(&part, &file))
        .map_err(|e| {
            let _ = std::fs::remove_file(&part);
            fail("failed", crate::helpers::said("the settings could not be saved.", &e))
        })
}

// ------------------------------------------------------------------ helpers

const HELPERS: [Helper; 2] = [Helper::Ffmpeg, Helper::Soulseek];

fn state_words(s: &State) -> (&'static str, u64, u64, Option<String>) {
    match s {
        State::Idle => ("idle", 0, 0, None),
        State::Getting(done, total) => ("getting", *done, *total, None),
        State::Unpacking => ("unpacking", 0, 0, None),
        State::Failed(why) => ("failed", 0, 0, Some(why.clone())),
    }
}

pub(crate) fn helpers_status() -> Value {
    let list: Vec<Value> = HELPERS
        .iter()
        .map(|&h| {
            let found = helpers::found(h);
            let (state, done, total, why) = state_words(&helpers::state(h));
            json!({
                "id": helpers::id(h),
                "name": h.name(),
                "version": h.pin().version,
                "size": h.pin().size,
                "found": found.is_some(),
                "path": found,
                "state": state,
                "done": done,
                "total": total,
                "why": why,
            })
        })
        .collect();
    json!({ "helpers": list })
}

/// Checks a fetch before it starts: which helper, and not in a test run.
pub(crate) fn helpers_fetch_check(env: &Env, params: &Value) -> Result<Helper, Failure> {
    let asked = params.get("helper").and_then(Value::as_str).unwrap_or_default();
    let h = HELPERS.into_iter().find(|&h| helpers::id(h) == asked).ok_or_else(|| fail("bad_request", format!("There is no helper called \"{asked}\"; the helpers are ffmpeg and slskd.")))?;
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is fetched while Cadence runs as a test."));
    }
    Ok(h)
}

/// Fetches a helper (pinned version, checked, then put in place; see `helpers`), with progress. One that is
/// already there is not fetched again.
pub(crate) fn helpers_fetch(out: &Out, job: &Job, id: u64, h: Helper) -> Result<Value, Failure> {
    if let Some(path) = helpers::found(h) {
        return Ok(json!({ "helper": helpers::id(h), "fetched": false, "path": path }));
    }
    let before = helpers::arrived(h);
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    *job.on_stop.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || helpers::stop(h)));
    if !helpers::start_with(h, move || { let _ = tx.try_send(()); }) {
        return Err(fail("busy", format!("{} is already being fetched.", first_upper(h.name()))));
    }
    let mut shown = None;
    loop {
        let _ = rx.recv_timeout(Duration::from_millis(400));
        let s = helpers::state(h);
        let (stage, done, total, why) = state_words(&s);
        match s {
            State::Getting(..) | State::Unpacking => {
                if shown != Some((stage, done)) {
                    shown = Some((stage, done));
                    out.progress(id, json!({ "stage": stage, "done": done, "total": total }));
                }
            }
            State::Failed(_) => return Err(fail("failed", why.unwrap_or_default())),
            State::Idle if helpers::arrived(h) != before => {
                return Ok(json!({ "helper": helpers::id(h), "fetched": true, "path": helpers::found(h) }));
            }
            State::Idle => return Err(fail("cancelled", "The fetch was stopped; nothing was put in place.")),
        }
    }
}

fn first_upper(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

// ------------------------------------------------------------------ Soulseek

/// Where the engine's Soulseek client stands. With a login kept, a client that is not running is started in
/// the background (the answer is then `starting`), so it is ready by the time the first search is asked for.
/// A test run starts nothing.
pub(crate) fn soulseek_status(eng: &super::Engine) -> Value {
    use super::soulseek::State;
    eng.soulseek.touch();
    let name = eng.soulseek.kept().map(|e| e.user);
    let state = eng.soulseek.look(!eng.env.offline_test);
    let (state, words) = match state {
        State::Ready(_) => ("ready", format!("Signed in to Soulseek as {}.", name.clone().unwrap_or_default())),
        State::NoHelper => ("no_helper", "The Soulseek client is not here yet.".to_string()),
        State::NoLogin => ("no_login", "Sign in to Soulseek to get music.".to_string()),
        State::Starting => ("starting", "Connecting to Soulseek.".to_string()),
        State::Refused => ("offline", "Soulseek did not take the kept name and password. Sign in again.".to_string()),
        State::Offline(_) if eng.env.offline_test => ("offline", "Soulseek is off while Cadence runs as a test.".to_string()),
        State::Offline(why) => ("offline", why),
    };
    json!({ "state": state, "name": name, "words": words })
}

/// Checks a sign-in before it starts: a name and a password, and not in a test run.
pub(crate) fn sign_in_check(env: &Env, params: &Value) -> Result<(String, String), Failure> {
    if env.offline_test {
        return Err(fail(OFFLINE, "Nothing is signed in to while Cadence runs as a test."));
    }
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let password = params.get("password").and_then(Value::as_str).unwrap_or_default().to_string();
    crate::slskd::check_login(&name, &password).map_err(|m| fail("bad_request", m))?;
    Ok((name, password))
}
