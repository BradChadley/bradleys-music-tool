//! Moves inside the music folder, and who is told about them (ENGINE-PROTOCOL.md, "New asks").
//!
//! Whatever moves a track or an album inside the library (a repair that files loose tracks or merges an
//! artist, Organize, the Undo of either) tells three parties:
//!   * Cadence, with the ask `library.moved`, after the last move and before `library.release`, so play
//!     counts, playlists and the rest follow the files (`tell`);
//!   * itself, when Cadence could not be told: the moves are written to `<Tools>\moves-unacknowledged.jsonl`,
//!     one batch per line, until Cadence asks for them (`library.unacknowledged`) and says it has them
//!     (`library.acknowledge`);
//!   * Backup, always: `<Tools>\backup-moves.jsonl` gets the batch's moves, so Backup renames its copies to
//!     match before it copies anything (`for_backup`).
//! Organize writes each album's move down as soon as its rename is done, before its "done" line, under its
//! own key ("<batch>#<step>"): into the unacknowledged list (`keep`) and Backup's list (`for_backup`). It
//! tells Cadence in small batches as it goes (`ask_moved`), and takes what Cadence took off the
//! unacknowledged list again (`forget`). So a crash at any point leaves every finished move in both lists.
use super::jobs::{fail, Failure};
use super::Out;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// One move: a file, or a whole folder (then every track under `from\` is now under `to\`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Move {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
    pub(crate) folder: bool,
}

#[derive(Serialize, Deserialize)]
struct Line {
    batch: String,
    #[serde(default)]
    when: String,
    moves: Vec<Move>,
}

/// One change at a time to the two lists (jobs run side by side).
static LISTS: Mutex<()> = Mutex::new(());

fn unacknowledged_file(dir: &Path) -> PathBuf {
    dir.join("moves-unacknowledged.jsonl")
}

fn backup_file(dir: &Path) -> PathBuf {
    dir.join("backup-moves.jsonl")
}

fn append(file: &Path, line: &Line) -> std::io::Result<()> {
    let _g = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

fn read(file: &Path) -> Vec<Line> {
    let bytes = std::fs::read(file).unwrap_or_default();
    // (a torn last line, from a power cut mid-write, hides nothing before it)
    String::from_utf8_lossy(&bytes).lines().filter_map(|l| serde_json::from_str::<Line>(l.trim()).ok()).collect()
}

/// The moves of a batch, from the (from, to) pairs of its record. `folder` is what is at `to` now.
pub(crate) fn from_pairs(pairs: &[(PathBuf, PathBuf)]) -> Vec<Move> {
    pairs.iter().map(|(from, to)| Move { from: from.clone(), to: to.clone(), folder: to.is_dir() }).collect()
}

/// Writes the moves down for Backup. A list that cannot be written is said in the engine's log: Backup
/// then copies the moved files again, which costs time but loses nothing.
pub(crate) fn for_backup(dir: &Path, batch: &str, moves: &[Move]) {
    if moves.is_empty() {
        return;
    }
    if let Err(why) = append(&backup_file(dir), &Line { batch: batch.to_string(), when: chrono::Local::now().to_rfc3339(), moves: moves.to_vec() }) {
        eprintln!("cadence-engine: the moves of {batch} could not be written down for Backup: {why}");
    }
}

/// Every batch of moves written down for Backup, oldest first: (batch, its moves).
pub(crate) fn for_backup_list(dir: &Path) -> Vec<(String, Vec<Move>)> {
    let _g = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    read(&backup_file(dir)).into_iter().map(|l| (l.batch, l.moves)).collect()
}

/// Tells Cadence about moves that are done (`library.moved`), while the library is still held. Cadence
/// answering with an error, or not at all, leaves them in `moves-unacknowledged.jsonl` for its next start.
/// The moves stay done either way; true when Cadence took them. A stop of the job does not cut the ask
/// short: the files have moved, and Cadence must hear of it.
pub(crate) fn tell(out: &Out, dir: &Path, batch: &str, moves: &[Move], wait: Duration) -> bool {
    if ask_moved(out, batch, moves, wait) {
        return true;
    }
    keep(dir, batch, moves);
    false
}

/// Only the ask: `library.moved` with these moves; true when Cadence took them.
pub(crate) fn ask_moved(out: &Out, batch: &str, moves: &[Move], wait: Duration) -> bool {
    if moves.is_empty() {
        return true;
    }
    match out.ask("library.moved", json!({ "batch": batch, "moves": moves }), wait, &|| false) {
        Ok(_) => true,
        Err((_, why)) => {
            eprintln!("cadence-engine: Cadence did not take the moves of {batch} ({why}); they are kept for its next start");
            false
        }
    }
}

/// Writes moves into the unacknowledged list (on the disk before this returns), under `key`, for Cadence's
/// next start or until `forget` takes them off.
pub(crate) fn keep(dir: &Path, key: &str, moves: &[Move]) {
    if moves.is_empty() {
        return;
    }
    if let Err(why) = append(&unacknowledged_file(dir), &Line { batch: key.to_string(), when: chrono::Local::now().to_rfc3339(), moves: moves.to_vec() }) {
        eprintln!("cadence-engine: the moves of {key} could not be kept for Cadence: {why}");
    }
}

/// Takes the lines with these keys off the unacknowledged list: Cadence has taken their moves.
pub(crate) fn forget(dir: &Path, keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    let _g = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    let file = unacknowledged_file(dir);
    let list = read(&file);
    let left: Vec<&Line> = list.iter().filter(|l| !keys.contains(&l.batch)).collect();
    if left.len() == list.len() {
        return;
    }
    let written = if left.is_empty() {
        std::fs::remove_file(&file).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
    } else {
        let text: String = left.iter().filter_map(|l| serde_json::to_string(l).ok()).map(|l| l + "
").collect();
        crate::util::write_atomic(&file, text.as_bytes())
    };
    if let Err(why) = written {
        eprintln!("cadence-engine: moves Cadence took could not be taken off its list ({why}); its next start follows them again, harmlessly");
    }
}

/// `library.unacknowledged`: every batch of moves Cadence has not taken yet, oldest first. Lines kept twice
/// under one key (a crash between writing an album's move down and its "done" line has it said again) come
/// as one batch, each move once.
pub(crate) fn unacknowledged(dir: &Path) -> Value {
    let _g = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut merged: Vec<(String, Vec<Move>)> = vec![];
    for l in read(&unacknowledged_file(dir)) {
        let at = match merged.iter().position(|(b, _)| *b == l.batch) {
            Some(i) => i,
            None => {
                merged.push((l.batch.clone(), vec![]));
                merged.len() - 1
            }
        };
        for m in l.moves {
            if !merged[at].1.contains(&m) {
                merged[at].1.push(m);
            }
        }
    }
    let list: Vec<Value> = merged.into_iter().map(|(batch, moves)| json!({ "batch": batch, "moves": moves })).collect();
    json!({ "batches": list })
}

/// `library.acknowledge {batch}`: Cadence has remapped that batch; it leaves the list. `{"acknowledged":
/// false}` when it was not on it.
pub(crate) fn acknowledge(dir: &Path, params: &Value) -> Result<Value, Failure> {
    let batch = params.get("batch").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if batch.is_empty() {
        return Err(fail("bad_request", "Say which batch of moves Cadence has taken (\"batch\")."));
    }
    let _g = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    let file = unacknowledged_file(dir);
    let list = read(&file);
    let left: Vec<&Line> = list.iter().filter(|l| l.batch != batch).collect();
    if left.len() == list.len() {
        return Ok(json!({ "acknowledged": false }));
    }
    let written = if left.is_empty() {
        std::fs::remove_file(&file).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
    } else {
        let text: String = left.iter().filter_map(|l| serde_json::to_string(l).ok()).map(|l| l + "\n").collect();
        crate::util::write_atomic(&file, text.as_bytes())
    };
    written.map_err(|e| fail("failed", crate::helpers::said("the list of moves Cadence has not taken yet could not be saved.", &e)))?;
    Ok(json!({ "acknowledged": true }))
}
