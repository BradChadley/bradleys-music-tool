//! The data the scan produces and the importer consumes.
use crate::importer::lookup::Cover;
use crate::importer::quality::{Quality, Verdict};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Clone, Default)]
pub struct Track {
    pub path: PathBuf,
    pub size: u64,
    /// path inside the download, with backslashes ("CD1\01 Song.flac")
    pub rel: String,
    /// lowercase with the dot (".flac")
    pub ext: String,
    /// file name without the extension
    pub base: String,
    pub old: HashMap<String, String>,
    /// tag fixes, in the order they were found
    pub new: Vec<(String, String)>,
    pub has_art: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub kbps: u32,
    pub bits: u32,
    pub duration: f64,
    pub quality: Option<Quality>,
    pub lossless: bool,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    /// junk in the tags, removed on import: (field, value, why)
    pub junk: Vec<(String, String, String)>,
    /// MP3 audio in a non-.mp3 wrapper (e.g. .wav): taken out into a plain .mp3, not re-encoded
    pub wrapped: bool,
    /// an .mp3 that was really a WAV wrapper: `path` points at a temporary unwrapped copy
    pub unwrapped: bool,
    /// the MP3's own length header in seconds (0 = it has none)
    pub header_secs: f64,
    /// the lyrics the file carries (see `crate::importer::lyrics`)
    pub lyric: crate::importer::lyrics::Lyric,
    /// what LRCLIB has for the song, once it has been asked (None = not asked, or it could not be)
    pub lyric_found: Option<crate::importer::lyrics::Answer>,
}

impl Track {
    pub fn old(&self, k: &str) -> &str {
        self.old.get(k).map(|s| s.as_str()).unwrap_or("")
    }
    pub fn set_new(&mut self, k: &str, v: &str) {
        match self.new.iter_mut().find(|e| e.0 == k) {
            Some(e) => e.1 = v.to_string(),
            None => self.new.push((k.to_string(), v.to_string())),
        }
    }
    pub fn has_new(&self, k: &str) -> bool {
        self.new.iter().any(|e| e.0 == k)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    New,
    Replace,
    Exists,
    Cleanup,
}

#[derive(Clone)]
pub struct Unit {
    pub name: String,
    pub root: PathBuf,
    pub item: PathBuf,
    /// the download is a folder (not a single loose track)
    pub is_dir: bool,
    pub files: Vec<FileEntry>,
    pub tracks: Vec<Track>,
    pub tag_artist: Option<String>,
    pub artist: String,
    pub folder: String,
    pub cover: Option<Cover>,
    pub use_cover: bool,
    pub folder_img: Option<PathBuf>,
    pub fixes: Vec<(String, usize)>,
    pub convert: usize,
    pub kbps: u32,
    pub bytes: u64,
    pub warnings: Vec<String>,
    pub verdict: Verdict,
    pub spectro: Option<PathBuf>,
    pub local_thumb: Option<PathBuf>,
    pub action: Action,
    pub existing: Option<PathBuf>,
    /// the folder the album goes into, when that is not the artist's own folder in the library: a shelf
    /// ("Artist\EPs and Singles") or the artist's folder inside a genre folder
    pub home: Option<PathBuf>,
    pub note: String,
    pub include: bool,
    /// what the import does with lyrics that have lines in them that are not lyrics
    pub lyric_choice: crate::importer::lyrics::Choice,
    /// LRCLIB is still being asked about this album's songs
    pub lyrics_pending: bool,
}

impl Unit {
    pub fn format_fixes(&self) -> String {
        if self.fixes.is_empty() {
            return "No tag changes needed".into();
        }
        self.fixes.iter().map(|(k, n)| format!("{k} ({n})")).collect::<Vec<_>>().join(", ")
    }
}
