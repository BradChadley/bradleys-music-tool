//! Where the app keeps what must not sit in a file: the Last.fm key and the Soulseek login.
//!
//! The real app keeps them in Windows Credential Manager (Control Panel, "Credential Manager", "Windows
//! Credentials", under "Bradley's Music Tool: ..."). Windows ties those entries to this Windows account on
//! this PC, so they are in no file of the app's and in no backup of its data folder.
//!
//! A test run (`MUSIC_RECS_DATA` set) never touches Credential Manager: it keeps them in `test-secrets.json`
//! in its own data folder. Unit tests keep them in memory.
//!
//! Nothing here logs, prints or returns a secret in an error.
use std::collections::BTreeMap;
use std::sync::Mutex;

pub const LASTFM: &str = "Last.fm";
pub const SOULSEEK: &str = "Soulseek";

/// A name (may be empty) and the secret that goes with it.
#[derive(Clone, PartialEq, Default)]
pub struct Entry {
    pub user: String,
    pub secret: String,
}

/// Whose entries these are: the start of every entry's name in Credential Manager. The Music Tool's are
/// "Bradley's Music Tool: ..."; Cadence's engine keeps its own ("Cadence: ...", see `set_owner`).
const MUSIC_TOOL: &str = "Bradley's Music Tool";
static OWNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Names whose entries this process keeps, for the rest of the run: Cadence's engine says "Cadence" at its
/// start, so its Soulseek login is "Cadence: Soulseek" and never the Music Tool's. Set once, before anything
/// is read; false if it was already set.
pub fn set_owner(owner: &str) -> bool {
    OWNER.set(owner.to_string()).is_ok()
}

fn owner() -> &'static str {
    OWNER.get().map(String::as_str).unwrap_or(MUSIC_TOOL)
}

fn target(name: &str) -> String {
    format!("{}: {name}", owner())
}

/// The name an entry is kept under in a test's file or in memory: the bare name for the Music Tool (as test
/// files from before have it), the whole name for another owner, so the two never share an entry.
fn local_key(name: &str) -> String {
    if owner() == MUSIC_TOOL { name.to_string() } else { target(name) }
}

enum Place {
    Windows,
    File(std::path::PathBuf),
    Memory,
}

fn place() -> Place {
    if cfg!(test) {
        return Place::Memory;
    }
    match std::env::var_os("MUSIC_RECS_DATA") {
        Some(p) if !p.is_empty() => Place::File(std::path::PathBuf::from(p).join("test-secrets.json")),
        _ => Place::Windows,
    }
}

static MEMORY: Mutex<BTreeMap<String, (String, String)>> = Mutex::new(BTreeMap::new());
/// What was last read or written, so the pages can ask on every draw without a call into Windows.
static SEEN: Mutex<BTreeMap<String, Option<Entry>>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
thread_local! {
    /// A test sets this to see what happens when nothing can be kept (only on that test's own thread).
    pub static REFUSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn file_map(path: &std::path::Path) -> BTreeMap<String, (String, String)> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn seen_key(name: &str) -> String {
    match place() {
        Place::File(p) => format!("{}|{}", p.display(), local_key(name)),
        _ => local_key(name),
    }
}

pub fn get(name: &str) -> Option<Entry> {
    let key = seen_key(name);
    if let Some(known) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return known.clone();
    }
    let got = match place() {
        Place::Windows => vault::read(&target(name)),
        Place::File(p) => file_map(&p).get(&local_key(name)).map(|(u, s)| Entry { user: u.clone(), secret: s.clone() }),
        Place::Memory => MEMORY.lock().unwrap_or_else(|e| e.into_inner()).get(&local_key(name)).map(|(u, s)| Entry { user: u.clone(), secret: s.clone() }),
    }
    .filter(|e| !e.secret.is_empty());
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(key, got.clone());
    got
}

/// Keeps `secret` (and the name that goes with it) and reads it back: Ok only when what is kept is what
/// was given.
pub fn set(name: &str, user: &str, secret: &str) -> Result<(), String> {
    let want = Entry { user: user.to_string(), secret: secret.to_string() };
    if get(name).as_ref() == Some(&want) {
        return Ok(());
    }
    let failed = || format!("Windows Credential Manager did not keep the {name} login.");
    #[cfg(test)]
    if REFUSE.with(|r| r.get()) {
        return Err(failed());
    }
    match place() {
        Place::Windows => vault::write(&target(name), user, secret).map_err(|_| failed())?,
        Place::File(p) => {
            let mut map = file_map(&p);
            map.insert(local_key(name), (user.to_string(), secret.to_string()));
            let bytes = serde_json::to_vec_pretty(&map).map_err(|_| failed())?;
            crate::util::write_atomic(&p, &bytes).map_err(|_| failed())?;
        }
        Place::Memory => {
            MEMORY.lock().unwrap_or_else(|e| e.into_inner()).insert(local_key(name), (user.to_string(), secret.to_string()));
        }
    }
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).remove(&seen_key(name));
    if get(name).as_ref() == Some(&want) { Ok(()) } else { Err(failed()) }
}

/// Takes an entry out (Cadence's engine signing out, and tests).
pub fn remove(name: &str) {
    match place() {
        Place::Windows => vault::delete(&target(name)),
        Place::File(p) => {
            let mut map = file_map(&p);
            map.remove(&local_key(name));
            if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
                let _ = crate::util::write_atomic(&p, &bytes);
            }
        }
        Place::Memory => {
            MEMORY.lock().unwrap_or_else(|e| e.into_inner()).remove(&local_key(name));
        }
    }
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).remove(&seen_key(name));
}

/// Random bytes from Windows' own source, as hex: for keys nobody has to type or remember.
pub fn random_hex(bytes: usize) -> String {
    use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut buf = vec![0u8; bytes];
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    // without Windows' random source there is no safe key to make: stop rather than make a weak one
    assert!(status == 0, "Windows gave no random numbers");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Windows Credential Manager itself: generic entries, kept for this Windows account on this PC.
mod vault {
    use super::Entry;
    use windows_sys::Win32::Security::Credentials::{CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read(target: &str) -> Option<Entry> {
        let name = wide(target);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        unsafe {
            if CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) == 0 || found.is_null() {
                return None;
            }
            let c = &*found;
            let secret = if c.CredentialBlob.is_null() { String::new() } else { String::from_utf8_lossy(std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)).into_owned() };
            let user = if c.UserName.is_null() {
                String::new()
            } else {
                let mut len = 0;
                while *c.UserName.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(c.UserName, len))
            };
            CredFree(found as *const core::ffi::c_void);
            Some(Entry { user, secret })
        }
    }

    pub fn write(target: &str, user: &str, secret: &str) -> Result<(), ()> {
        let (mut name, mut user, mut blob) = (wide(target), wide(user), secret.as_bytes().to_vec());
        let mut c: CREDENTIALW = unsafe { std::mem::zeroed() };
        c.Type = CRED_TYPE_GENERIC;
        c.TargetName = name.as_mut_ptr();
        c.CredentialBlobSize = blob.len() as u32;
        c.CredentialBlob = blob.as_mut_ptr();
        c.Persist = CRED_PERSIST_LOCAL_MACHINE;
        c.UserName = user.as_mut_ptr();
        if unsafe { CredWriteW(&c, 0) } != 0 { Ok(()) } else { Err(()) }
    }

    pub fn delete(target: &str) {
        let name = wide(target);
        unsafe {
            CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The one test that touches the real Credential Manager: an entry that is plainly a test's, with a
        /// made-up secret, removed again at the end.
        #[test]
        fn credential_manager_keeps_and_gives_back() {
            let target = format!("music-recs-test-entry-{}", std::process::id());
            delete(&target);
            assert!(read(&target).is_none());
            write(&target, "made-up name", "made-up secret: \"é\" 123").unwrap();
            let got = read(&target).unwrap();
            assert_eq!((got.user.as_str(), got.secret.as_str()), ("made-up name", "made-up secret: \"é\" 123"));
            // writing again replaces it
            write(&target, "", "other").unwrap();
            let got = read(&target).unwrap();
            assert_eq!((got.user.as_str(), got.secret.as_str()), ("", "other"));
            delete(&target);
            assert!(read(&target).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_kept_and_given_back() {
        // a unit test never reaches Windows, whatever data folder another test has named
        assert!(matches!(place(), Place::Memory));
        let name = format!("unit-{}", random_hex(4));
        assert!(get(&name).is_none());
        set(&name, "me", "s3cret").unwrap();
        assert_eq!(get(&name).map(|e| (e.user, e.secret)), Some(("me".to_string(), "s3cret".to_string())));
        set(&name, "", "other").unwrap();
        assert_eq!(get(&name).map(|e| e.secret), Some("other".to_string()));
        remove(&name);
        assert!(get(&name).is_none());
    }

    #[test]
    fn random_keys_differ_and_are_long_enough() {
        let (a, b) = (random_hex(24), random_hex(24));
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
