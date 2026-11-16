//! Album covers for every page: one cache, one memory budget.
//!
//! This module only ever reads the music folder. Nothing here creates, changes, moves or deletes a file.
//!   * a folder's cover is a well-named picture file in it, else the art stored inside its first track
//!     (mp3, flac, m4a), else whatever other picture is there
//!   * covers are found and decoded on background threads at exactly the size they are drawn, only for what
//!     was asked for this frame, and the least recently shown are dropped when the budget is exceeded
//!   * picture files come from downloads, so they are treated as untrusted: a size check before reading,
//!     decoding with explicit limits, and a decoder panic cannot take a worker thread down
//!
//! Everything public here is called from the UI thread. The shared state sits behind one short lock that is
//! never held while a picture is read or decoded.
use crate::theme::{BORDER, CARD};
use crossbeam_channel::{unbounded, Receiver};
use egui::{Align2, Color32, ColorImage, CornerRadius, FontId, Rect, Stroke, TextureHandle, TextureOptions, Ui};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

/// Decoded covers kept in memory before the least recently shown are dropped.
const COVER_BUDGET: usize = 160 * 1024 * 1024;
/// Seconds a cover takes to fade in once it is decoded.
const FADE: f32 = 0.15;
/// A picture file this big or bigger is not read at all.
const MAX_PICTURE_FILE: u64 = 60_000_000;

pub const AUDIO: [&str; 7] = ["mp3", "m4a", "flac", "ogg", "opus", "wav", "aac"];
pub const PICTURE: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];

/// Where a cover can come from.
#[derive(Clone, PartialEq, Debug)]
pub enum Src {
    /// a picture file in the album folder
    File(PathBuf),
    /// the art stored inside an audio file
    Embedded(PathBuf),
}

pub enum Pic {
    /// the cover, and how far its fade in has come (0 to 1)
    Ready(TextureHandle, f32),
    Loading,
    None,
}

// ------------------------------------------------------------------ finding the cover in a folder

pub fn ext_in(p: &Path, list: &[&str]) -> bool {
    p.extension().map(|e| list.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

/// The audio and picture files of one album folder.
#[derive(Default)]
pub struct Found {
    /// (disc folder, file name, full path)
    pub audio: Vec<(String, String, PathBuf)>,
    /// (how deep, full path)
    pub pictures: Vec<(u8, PathBuf)>,
}

/// Lists a folder's audio and picture files; with `recurse`, also those in "CD1" style subfolders.
pub fn walk(dir: &Path, disc: &str, depth: u8, recurse: bool, out: &mut Found) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if recurse && depth < 2 {
                let sub = if disc.is_empty() { name } else { format!("{disc} / {name}") };
                walk(&p, &sub, depth + 1, true, out);
            }
        } else if ext_in(&p, &AUDIO) {
            out.audio.push((disc.to_string(), name, p));
        } else if ext_in(&p, &PICTURE) {
            out.pictures.push((depth, p));
        }
    }
}

/// Sorts "2 - b" before "10 - a".
pub fn natural_key(name: &str) -> (u64, String) {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    (digits.parse().unwrap_or(u64::MAX), name.to_lowercase())
}

/// How likely a picture file is to be the front cover, by its name. Lower is better.
fn picture_rank(p: &Path) -> u8 {
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if stem.contains("small") {
        return 9;
    }
    match stem.as_str() {
        "cover" => return 0,
        "folder" => return 1,
        "front" => return 2,
        _ => {}
    }
    if ["back", "inside", "inlay", "tray", "booklet", "spine", "matrix", "label", "disc", "cd"].iter().any(|w| stem.contains(w)) {
        return 8;
    }
    if stem.contains("cover") || stem.contains("front") {
        return 3;
    }
    if stem.starts_with("albumart") {
        return 4;
    }
    5
}

/// The places to look for a cover, best first: well-named picture files, then the art inside the
/// first track, then whatever other picture is there.
pub fn art_sources(mut pictures: Vec<(u8, PathBuf)>, first_track: Option<&PathBuf>) -> Vec<Src> {
    pictures.sort_by_key(|(depth, p)| (*depth, picture_rank(p), p.to_string_lossy().to_lowercase()));
    let mut out: Vec<Src> = pictures.iter().filter(|(_, p)| picture_rank(p) <= 5).take(2).map(|(_, p)| Src::File(p.clone())).collect();
    if let Some(t) = first_track {
        out.push(Src::Embedded(t.clone()));
    }
    out.extend(pictures.iter().filter(|(_, p)| picture_rank(p) > 5).take(1).map(|(_, p)| Src::File(p.clone())));
    out
}

/// Where an album folder's cover can come from, best first. Reads the folder: call it off the UI thread.
pub fn find_sources(folder: &Path) -> Vec<Src> {
    let mut found = Found::default();
    walk(folder, "", 0, true, &mut found);
    // the first track, in the order the Library page lists them
    found.audio.sort_by(|a, b| (a.0.to_lowercase(), natural_key(&a.1)).cmp(&(b.0.to_lowercase(), natural_key(&b.1))));
    let first = found.audio.first().map(|a| a.2.clone());
    art_sources(found.pictures, first.as_ref())
}

/// The name a folder's covers are kept under. The same folder written with other capitals, the other
/// kind of slash or a trailing slash gives the same key.
pub fn folder_key(folder: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let s = folder.to_string_lossy().replace('/', "\\").to_lowercase();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.trim_end_matches('\\').hash(&mut h);
    h.finish()
}

// ------------------------------------------------------------------ art stored inside audio files

fn be32(b: &[u8]) -> usize {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
}

fn synchsafe(b: &[u8]) -> usize {
    b[..4].iter().fold(0usize, |n, x| (n << 7) | (*x as usize & 0x7f))
}

fn unsync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        out.push(b[i]);
        if b[i] == 0xff && b.get(i + 1) == Some(&0) {
            i += 1;
        }
        i += 1;
    }
    out
}

/// Where the picture bytes start inside a tag's picture frame (after its text fields).
fn picture_start(d: &[u8]) -> Option<usize> {
    let end = d.len().min(4096);
    (0..end.saturating_sub(4)).find(|&i| d[i..].starts_with(&[0xff, 0xd8, 0xff]) || d[i..].starts_with(&[0x89, b'P', b'N', b'G']))
}

fn frame_id_ok(b: &[u8], at: usize, n: usize) -> bool {
    match b.get(at..at + n) {
        Some(id) => id.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) || id.iter().all(|c| *c == 0),
        None => true, // the end of the tag
    }
}

fn id3_picture(ver: u8, flags: u8, body: Vec<u8>) -> Option<Vec<u8>> {
    let b = if flags & 0x80 != 0 && ver <= 3 { unsync(&body) } else { body };
    let mut i = 0;
    if flags & 0x40 != 0 && ver >= 3 && b.len() >= 4 {
        i += if ver == 4 { synchsafe(&b) } else { be32(&b) + 4 };
    }
    let hdr = if ver == 2 { 6 } else { 10 };
    while i + hdr <= b.len() {
        let id = &b[i..i + if ver == 2 { 3 } else { 4 }];
        if id[0] == 0 {
            break;
        }
        let size = match ver {
            2 => (b[i + 3] as usize) << 16 | (b[i + 4] as usize) << 8 | b[i + 5] as usize,
            4 => {
                // some writers put plain sizes in 2.4 tags: use whichever lands on a real frame
                let (s, p) = (synchsafe(&b[i + 4..]), be32(&b[i + 4..]));
                if s != p && !frame_id_ok(&b, i + hdr + s, 4) && frame_id_ok(&b, i + hdr + p, 4) && i + hdr + p <= b.len() { p } else { s }
            }
            _ => be32(&b[i + 4..]),
        };
        if size == 0 || i + hdr + size > b.len() {
            break;
        }
        if id == b"APIC" || id == b"PIC" {
            let data = &b[i + hdr..i + hdr + size];
            let data = if ver == 4 && b[i + 9] & 0x02 != 0 { unsync(data) } else { data.to_vec() };
            if let Some(at) = picture_start(&data) {
                return Some(data[at..].to_vec());
            }
        }
        i += hdr + size;
    }
    None
}

fn flac_picture(f: &mut std::fs::File) -> Option<Vec<u8>> {
    loop {
        let mut h = [0u8; 4];
        f.read_exact(&mut h).ok()?;
        let len = (h[1] as usize) << 16 | (h[2] as usize) << 8 | h[3] as usize;
        if h[0] & 0x7f == 6 {
            let mut d = vec![0u8; len];
            f.read_exact(&mut d).ok()?;
            let mime = be32(d.get(4..8)?);
            let desc_at = 8 + mime;
            let desc = be32(d.get(desc_at..desc_at + 4)?);
            let len_at = desc_at + 4 + desc + 16;
            let n = be32(d.get(len_at..len_at + 4)?);
            return d.get(len_at + 4..len_at + 4 + n).map(|p| p.to_vec());
        }
        if h[0] & 0x80 != 0 {
            return None;
        }
        f.seek(SeekFrom::Current(len as i64)).ok()?;
    }
}

/// The contents of the first child box called `name`.
fn mp4_child<'a>(mut b: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
    while b.len() >= 8 {
        let size = be32(b);
        if size < 8 || size > b.len() {
            return None;
        }
        if &b[4..8] == name {
            return Some(&b[8..size]);
        }
        b = &b[size..];
    }
    None
}

fn mp4_picture(f: &mut std::fs::File) -> Option<Vec<u8>> {
    let mut pos = 0u64;
    loop {
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h).ok()?;
        let (mut size, mut head) = (be32(&h) as u64, 8u64);
        if size == 1 {
            let mut big = [0u8; 8];
            f.read_exact(&mut big).ok()?;
            size = u64::from_be_bytes(big);
            head = 16;
        }
        if size < head {
            return None; // 0 means "to the end of the file", and that box is never the one with the tags
        }
        if &h[4..8] == b"moov" {
            if size > 80_000_000 {
                return None;
            }
            let mut moov = vec![0u8; (size - head) as usize];
            f.read_exact(&mut moov).ok()?;
            let meta = mp4_child(mp4_child(&moov, b"udta")?, b"meta")?;
            let covr = mp4_child(mp4_child(meta.get(4..)?, b"ilst")?, b"covr")?;
            return mp4_child(covr, b"data")?.get(8..).map(|p| p.to_vec());
        }
        pos = pos.checked_add(size)?;
    }
}

/// The cover picture stored inside an audio file, if it has one.
fn embedded_art(p: &Path) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut head = [0u8; 10];
    f.read_exact(&mut head).ok()?;
    if &head[..3] == b"ID3" {
        let size = synchsafe(&head[6..]);
        if size > 60_000_000 {
            return None;
        }
        let mut body = vec![0u8; size];
        f.read_exact(&mut body).ok()?;
        if let Some(pic) = id3_picture(head[3], head[5], body) {
            return Some(pic);
        }
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic).ok()?;
        return if &magic == b"fLaC" { flac_picture(&mut f) } else { None };
    }
    if &head[..4] == b"fLaC" {
        f.seek(SeekFrom::Start(4)).ok()?;
        return flac_picture(&mut f);
    }
    if &head[4..8] == b"ftyp" {
        return mp4_picture(&mut f);
    }
    None
}

// ------------------------------------------------------------------ reading and decoding (worker threads)

/// A picture file's bytes, unless it is `max` bytes or bigger (or cannot be read).
fn read_picture_file(p: &Path, max: u64) -> Option<Vec<u8>> {
    std::fs::metadata(p).ok().filter(|m| m.is_file() && m.len() < max).and_then(|_| std::fs::read(p).ok())
}

/// Decodes a picture that may be anything at all: nothing wider or taller than 12,000 pixels, and no
/// more than 256 MB of memory for it.
fn decode_limited(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(12_000);
    limits.max_image_height = Some(12_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()
}

/// The largest a cover is drawn anywhere in the app, in pixels: the size a folder's picture is chosen for.
const CHOICE_PX: u32 = 600;

/// One source's picture, if it can be read and decoded.
fn decode_source(s: &Src) -> Option<image::DynamicImage> {
    let bytes = match s {
        Src::File(p) => read_picture_file(p, MAX_PICTURE_FILE),
        Src::Embedded(p) => embedded_art(p),
    };
    bytes.and_then(|b| decode_limited(&b))
}

/// Which of a folder's pictures is its cover, at every size it is drawn: the first of `sources` that is at
/// least `CHOICE_PX` on its short side, else the largest there is (the earlier one of two the same size).
/// Returns the source and its picture.
fn choose_source(sources: &[Src]) -> Option<(Src, image::DynamicImage)> {
    let mut best: Option<(&Src, image::DynamicImage)> = None;
    for s in sources {
        let Some(img) = decode_source(s) else { continue };
        let side = img.width().min(img.height());
        if best.as_ref().map(|(_, b)| side > b.width().min(b.height())).unwrap_or(true) {
            best = Some((s, img));
        }
        if side >= CHOICE_PX {
            break; // sharp enough at the largest size: no need to look at the other candidates
        }
    }
    best.map(|(s, img)| (s.clone(), img))
}

/// Decodes the folder's cover as a square of `px` pixels (smaller only if the picture itself is smaller).
#[cfg(test)]
fn load_cover(sources: &[Src], px: u32) -> Option<ColorImage> {
    square(choose_source(sources)?.1, px)
}

/// A picture cut square and made `px` pixels (never made bigger than it is).
fn square(img: image::DynamicImage, px: u32) -> Option<ColorImage> {
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    if side == 0 {
        return None;
    }
    let mut img = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    if side > px {
        if side > px * 3 {
            img = img.thumbnail_exact(px * 2, px * 2); // quick first cut for very large scans
        }
        img = img.resize_exact(px, px, image::imageops::FilterType::CatmullRom);
    }
    let rgba = img.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

// ------------------------------------------------------------------ the accent color

/// (hue in degrees, saturation, lightness), each of the last two 0 to 1.
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let (l, d) = ((max + min) / 2.0, max - min);
    if d <= f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-6);
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s.min(1.0), l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

/// A vivid color from a cover that reads on a dark background: the cover's strongest family of colorful
/// pixels, averaged, then made light enough. A cover with next to no color gives a neutral light gray.
fn accent_of(img: &ColorImage) -> Option<Color32> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || img.pixels.len() < w * h {
        return None;
    }
    let step = (w.max(h) / 32).max(1); // a small downscale: about a thousand pixels at most
    let mut bins = [[0f32; 4]; 12]; // per 30 degrees of hue: weight, then weighted red, green, blue
    let mut n = 0usize;
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            let c = img.pixels[y * w + x];
            if c.a() < 128 {
                continue;
            }
            n += 1;
            let (r, g, b) = (c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
            let chroma = r.max(g).max(b) - r.min(g).min(b);
            if chroma < 0.08 {
                continue; // grays, near-blacks and near-whites say nothing about the cover's color
            }
            let weight = chroma * chroma;
            let bin = &mut bins[((rgb_to_hsl(r, g, b).0 / 30.0) as usize).min(11)];
            bin[0] += weight;
            bin[1] += weight * r;
            bin[2] += weight * g;
            bin[3] += weight * b;
        }
    }
    if n == 0 {
        return None;
    }
    let best = bins.iter().fold(bins[0], |a, b| if b[0] > a[0] { *b } else { a });
    if best[0] / (n as f32) < 0.003 {
        return Some(Color32::from_gray(200));
    }
    let (hue, s, l) = rgb_to_hsl(best[1] / best[0], best[2] / best[0], best[3] / best[0]);
    let (r, g, b) = hsl_to_rgb(hue, s.clamp(0.45, 0.85), l.clamp(0.62, 0.78));
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Color32::from_rgb(byte(r), byte(g), byte(b)))
}

// ------------------------------------------------------------------ the shared engine

/// (the folder's key, pixels square)
type CoverKey = (u64, u32);

struct Job {
    key: CoverKey,
    /// tells this piece of work from an earlier one for the same cover (see `forget_folder`)
    id: u64,
    /// where the art is, when that is known already
    sources: Option<Vec<Src>>,
    /// else the folder to look in
    folder: Option<PathBuf>,
    /// the one source this folder's covers come from, once that has been chosen
    chosen: Option<Src>,
}

struct Done {
    key: CoverKey,
    id: u64,
    /// the sources, if this job had to look for them
    found: Option<Vec<Src>>,
    /// the source chosen for the folder, if this job had to choose it
    picked: Option<Src>,
    img: Option<ColorImage>,
    accent: Option<Color32>,
}

/// What a job brings back: (the sources if it looked for them, the source if it chose it, the cover, its color).
type Outcome = (Option<Vec<Src>>, Option<Src>, Option<ColorImage>, Option<Color32>);

/// Everything slow, on a worker thread: looking through the folder, choosing the picture (once per folder),
/// reading, decoding, the accent color.
fn run_job(job: &Job) -> Outcome {
    if let Some(src) = &job.chosen {
        // the folder's picture was chosen when an earlier size was decoded: every size comes from that one
        let img = decode_source(src).and_then(|img| square(img, job.key.1));
        let accent = img.as_ref().and_then(accent_of);
        return (None, None, img, accent);
    }
    let found = match (&job.sources, &job.folder) {
        (None, Some(folder)) => Some(find_sources(folder)),
        _ => None,
    };
    let sources: &[Src] = job.sources.as_deref().or(found.as_deref()).unwrap_or(&[]);
    let (picked, img) = match choose_source(sources) {
        Some((src, img)) => (Some(src), square(img, job.key.1)),
        None => (None, None),
    };
    let accent = img.as_ref().and_then(accent_of);
    (found, picked, img, accent)
}

enum SlotState {
    /// being worked on, by the job with this id
    Loading(u64),
    /// the texture, its size in bytes, and when it arrived (for the fade in)
    Ready(TextureHandle, usize, f64),
    Failed,
}

struct Slot {
    state: SlotState,
    /// the last frame this cover was asked for
    used: u64,
}

type Queue = Arc<(Mutex<VecDeque<Job>>, Condvar)>;

struct Engine {
    slots: HashMap<CoverKey, Slot>,
    /// where each folder's art is, once somebody has said or a worker has looked
    known: HashMap<u64, Vec<Src>>,
    /// the one of them each folder's covers are decoded from, at every size: chosen when the first is decoded
    chosen: HashMap<u64, Src>,
    /// one color per folder, kept when its textures are dropped
    accents: HashMap<u64, Color32>,
    queue: Queue,
    done: Receiver<Done>,
    next_id: u64,
    frame: u64,
    /// covers asked for this frame that are not decoded yet
    wanted: HashSet<CoverKey>,
    bytes: usize,
    /// the time of the frame being drawn
    now: f64,
    /// finished work has been taken in for this frame
    polled: bool,
    /// a cover drawn this frame is still fading in
    fading: bool,
}

static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

fn lock(m: &'static Mutex<Engine>) -> MutexGuard<'static, Engine> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn engine(ctx: &egui::Context) -> MutexGuard<'static, Engine> {
    lock(ENGINE.get_or_init(|| Mutex::new(Engine::new(ctx))))
}

/// The engine if any cover has ever been asked for.
fn started() -> Option<MutexGuard<'static, Engine>> {
    ENGINE.get().map(lock)
}

impl Engine {
    fn new(ctx: &egui::Context) -> Self {
        let queue: Queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let (tx, done) = unbounded();
        for _ in 0..3 {
            let (queue, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                let job = {
                    let Ok(mut q) = queue.0.lock() else { return };
                    loop {
                        if let Some(j) = q.pop_front() {
                            break j;
                        }
                        q = match queue.1.wait(q) {
                            Ok(q) => q,
                            Err(_) => return,
                        };
                    }
                };
                // a picture that makes a decoder panic costs that one cover, not the thread
                let (found, picked, img, accent) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&job))).unwrap_or((None, None, None, None));
                if tx.send(Done { key: job.key, id: job.id, found, picked, img, accent }).is_err() {
                    return; // the window closed
                }
                ctx.request_repaint();
            });
        }
        Self {
            slots: HashMap::new(),
            known: HashMap::new(),
            chosen: HashMap::new(),
            accents: HashMap::new(),
            queue,
            done,
            next_id: 1,
            frame: 0,
            wanted: HashSet::new(),
            bytes: 0,
            now: 0.0,
            polled: false,
            fading: false,
        }
    }

    /// Takes in what the workers have finished. Done once per frame, before the first cover is handed out.
    fn poll(&mut self, ctx: &egui::Context) {
        if self.polled {
            return;
        }
        self.polled = true;
        self.now = ctx.input(|i| i.time);
        while let Ok(d) = self.done.try_recv() {
            let Some(slot) = self.slots.get_mut(&d.key) else { continue };
            if !matches!(slot.state, SlotState::Loading(id) if id == d.id) {
                continue; // forgotten and asked for again since: a newer job is on its way
            }
            if let Some(found) = d.found {
                self.known.insert(d.key.0, found);
            }
            if let Some(src) = d.picked {
                match self.chosen.get(&d.key.0) {
                    // two sizes were being decoded at once and did not settle on the same picture (the
                    // folder's files changed in between): the first choice stands, and this size is
                    // decoded again from it
                    Some(first) if *first != src => {
                        let id = self.next_id;
                        self.next_id += 1;
                        slot.state = SlotState::Loading(id);
                        if let Ok(mut q) = self.queue.0.lock() {
                            q.push_back(Job { key: d.key, id, sources: None, folder: None, chosen: Some(first.clone()) });
                        }
                        self.queue.1.notify_one();
                        continue;
                    }
                    Some(_) => {}
                    None => {
                        self.chosen.insert(d.key.0, src);
                    }
                }
            }
            slot.state = match d.img {
                Some(ci) => {
                    if let Some(c) = d.accent {
                        self.accents.entry(d.key.0).or_insert(c);
                    }
                    let n = ci.width() * ci.height() * 4;
                    self.bytes += n;
                    SlotState::Ready(ctx.load_texture(format!("cover-{}-{}", d.key.0, d.key.1), ci, TextureOptions::LINEAR), n, self.now)
                }
                None => SlotState::Failed,
            };
        }
    }

    /// The cover at `px` pixels square. Starts the work for it if this is the first time it is wanted.
    fn get(&mut self, ctx: &egui::Context, key: u64, px: u32, sources: Option<&[Src]>, folder: Option<&Path>) -> Pic {
        if px == 0 || sources.is_some_and(|s| s.is_empty()) {
            return Pic::None;
        }
        self.poll(ctx);
        let key = (key, px);
        if let Some(slot) = self.slots.get_mut(&key) {
            slot.used = self.frame;
            return match &slot.state {
                SlotState::Ready(t, _, at) => {
                    let shown = (((self.now - *at) as f32) / FADE).clamp(0.0, 1.0);
                    self.fading |= shown < 1.0;
                    Pic::Ready(t.clone(), shown)
                }
                SlotState::Failed => Pic::None,
                SlotState::Loading(_) => {
                    self.wanted.insert(key);
                    Pic::Loading
                }
            };
        }
        let sources: Option<Vec<Src>> = match sources {
            Some(s) => {
                self.known.insert(key.0, s.to_vec());
                Some(s.to_vec())
            }
            None => self.known.get(&key.0).cloned(),
        };
        if sources.as_ref().is_some_and(|s| s.is_empty()) {
            return Pic::None; // looked before: there is nothing in that folder
        }
        let id = self.next_id;
        self.next_id += 1;
        self.slots.insert(key, Slot { state: SlotState::Loading(id), used: self.frame });
        self.wanted.insert(key);
        let folder = if sources.is_none() { folder.map(|f| f.to_path_buf()) } else { None };
        let chosen = self.chosen.get(&key.0).cloned();
        if let Ok(mut q) = self.queue.0.lock() {
            q.push_back(Job { key, id, sources, folder, chosen });
        }
        self.queue.1.notify_one();
        Pic::Loading
    }

    /// The cover at `px` pixels if it is decoded already. Never starts any work.
    fn ready(&mut self, key: u64, px: u32) -> Option<Pic> {
        self.ready_texture(key, px).map(|t| Pic::Ready(t, 1.0))
    }

    fn ready_texture(&mut self, key: u64, px: u32) -> Option<TextureHandle> {
        let slot = self.slots.get_mut(&(key, px))?;
        let SlotState::Ready(t, _, _) = &slot.state else { return None };
        slot.used = self.frame;
        Some(t.clone())
    }

    fn end_frame(&mut self, ctx: &egui::Context) {
        self.poll(ctx); // nobody asked for a cover this frame: still take in what was finished
        self.polled = false;
        // redraw only while a cover on screen is still fading in
        if std::mem::take(&mut self.fading) {
            ctx.request_repaint();
        }
        // waiting work for covers that scrolled away is dropped, so what is on screen never queues behind it
        if let Ok(mut q) = self.queue.0.lock() {
            let (wanted, slots) = (&self.wanted, &mut self.slots);
            q.retain(|j| {
                let keep = wanted.contains(&j.key);
                if !keep {
                    slots.remove(&j.key);
                }
                keep
            });
        }
        self.wanted.clear();
        if self.bytes > COVER_BUDGET {
            let mut old: Vec<(u64, CoverKey)> =
                self.slots.iter().filter(|(_, s)| matches!(s.state, SlotState::Ready(..)) && s.used + 1 < self.frame).map(|(k, s)| (s.used, *k)).collect();
            old.sort();
            for (_, key) in old {
                if self.bytes <= COVER_BUDGET * 3 / 4 {
                    break;
                }
                if let Some(Slot { state: SlotState::Ready(_, n, _), .. }) = self.slots.remove(&key) {
                    self.bytes -= n;
                }
            }
        }
        self.frame += 1;
    }

    fn forget(&mut self, key: u64) {
        if let Ok(mut q) = self.queue.0.lock() {
            q.retain(|j| j.key.0 != key);
        }
        let mut freed = 0;
        self.slots.retain(|k, s| {
            if k.0 != key {
                return true;
            }
            if let SlotState::Ready(_, n, _) = &s.state {
                freed += *n;
            }
            false
        });
        self.bytes -= freed;
        self.wanted.retain(|k| k.0 != key);
        self.known.remove(&key);
        self.chosen.remove(&key);
        self.accents.remove(&key);
    }
}

// ------------------------------------------------------------------ what the pages call (UI thread)

/// The cover for an album folder at `px` pixels square. Finds the cover itself (in the background) the first
/// time a folder is asked for. Cheap to call every frame.
pub fn for_folder(ctx: &egui::Context, folder: &Path, px: u32) -> Pic {
    engine(ctx).get(ctx, folder_key(folder), px, None, Some(folder))
}

/// Same, when the caller already knows where the art is (the Library page does, from its scan), to skip the
/// search. `key` is `folder_key(folder)`, so every page shares the folder's covers.
pub fn for_sources(ctx: &egui::Context, key: u64, sources: &[Src], px: u32) -> Pic {
    engine(ctx).get(ctx, key, px, Some(sources), None)
}

/// For a cover drawn small beside the same cover drawn bigger (the quarters of an artist's tile): the cover
/// at `px` if that size has been asked for before, else the one at `larger_px` if it is decoded already (to
/// be drawn smaller, instead of decoding it again), else the cover at `px`.
pub fn for_sources_or_larger(ctx: &egui::Context, key: u64, sources: &[Src], px: u32, larger_px: u32) -> Pic {
    let mut e = engine(ctx);
    if sources.is_empty() || px == 0 {
        return Pic::None;
    }
    e.poll(ctx);
    if !e.slots.contains_key(&(key, px)) {
        if let Some(pic) = e.ready(key, larger_px) {
            return pic;
        }
    }
    e.get(ctx, key, px, Some(sources), None)
}

/// A vivid, readable color taken from that folder's cover (for tinting a sidebar), once its cover has been
/// decoded at any size; None until then or if there is no cover. A cover without color gives a light gray.
pub fn accent(folder: &Path) -> Option<Color32> {
    accent_for(folder_key(folder))
}

pub fn accent_for(key: u64) -> Option<Color32> {
    started()?.accents.get(&key).copied()
}

/// Paints a Pic into a rect: the texture (with its fade), or the quiet loading square, or the "no cover" placeholder.
pub fn paint(ui: &Ui, pic: Pic, rect: Rect, radius: impl Into<CornerRadius>) {
    let radius: CornerRadius = radius.into();
    match pic {
        Pic::Ready(t, shown) => {
            let mut tint = Color32::WHITE;
            if shown < 1.0 {
                // fading in over the same quiet square a cover has while it loads
                ui.painter().rect_filled(rect, radius, CARD());
                tint = tint.gamma_multiply(shown * shown * (3.0 - 2.0 * shown));
            }
            egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size())).corner_radius(radius).tint(tint).paint_at(ui, rect);
        }
        Pic::Loading => {
            ui.painter().rect_filled(rect, radius, CARD());
        }
        Pic::None => {
            // no cover anywhere: a quiet placeholder
            ui.painter().rect_filled(rect, radius, CARD());
            ui.painter().rect_stroke(rect, radius, Stroke::new(1.0, BORDER()), egui::StrokeKind::Inside);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, "♪", FontId::proportional(rect.width() * 0.26), BORDER());
        }
    }
}

/// Paints a cover whose size on screen has just changed (a grid whose covers grow with the window). Until the
/// cover at its new size is decoded and has faded in, the one already decoded at one of `was_px` (the first
/// there is) shows under it, stretched to `rect`, instead of an empty square. With `was_px` empty this is `paint`.
pub fn paint_over(ui: &Ui, pic: Pic, key: u64, was_px: &[u32], rect: Rect, radius: impl Into<CornerRadius>) {
    let radius: CornerRadius = radius.into();
    let waiting = match &pic {
        Pic::Ready(_, shown) => *shown < 1.0,
        Pic::Loading => true,
        Pic::None => false,
    };
    let under = if waiting && !was_px.is_empty() { started().and_then(|mut e| was_px.iter().find_map(|px| e.ready_texture(key, *px))) } else { None };
    let Some(under) = under else { return paint(ui, pic, rect, radius) };
    let image = |t: &TextureHandle, tint: Color32| egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size())).corner_radius(radius).tint(tint).paint_at(ui, rect);
    image(&under, Color32::WHITE);
    if let Pic::Ready(t, shown) = pic {
        image(&t, Color32::WHITE.gamma_multiply(shown * shown * (3.0 - 2.0 * shown)));
    }
}

/// Paints the part `uv` of a cover (0 to 1 across and down) into `rect`, for a cover shown as one piece of
/// a tile: with its fade, and a plain square while it loads or when there is no cover (the note sign is for
/// whole tiles). `was_px` as in `paint_over`.
pub fn paint_part(ui: &Ui, pic: Pic, key: u64, was_px: &[u32], uv: Rect, rect: Rect, radius: impl Into<CornerRadius>) {
    let radius: CornerRadius = radius.into();
    let image = |t: &TextureHandle, tint: Color32| egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size())).uv(uv).corner_radius(radius).tint(tint).paint_at(ui, rect);
    let waiting = !matches!(&pic, Pic::Ready(_, shown) if *shown >= 1.0);
    let under = if waiting && !was_px.is_empty() { started().and_then(|mut e| was_px.iter().find_map(|px| e.ready_texture(key, *px))) } else { None };
    match &under {
        Some(t) => image(t, Color32::WHITE),
        None if waiting => {
            ui.painter().rect_filled(rect, radius, CARD());
        }
        None => {}
    }
    if let Pic::Ready(t, shown) = pic {
        image(&t, Color32::WHITE.gamma_multiply(shown * shown * (3.0 - 2.0 * shown)));
    }
}

// ------------------------------------------------------------------ rows of covers that fill a width

/// A row of square covers: how many, their side and the space between them, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tiles {
    pub cols: usize,
    pub side: f32,
    pub gap: f32,
}

/// Covers are sized in steps of this many screen pixels, so a window being resized asks for few sizes.
const SIDE_STEP: f32 = 4.0;

/// Lays square covers across `avail` points so that the row reaches the right edge.
///   * how many: as many covers `tile` points wide as fit with `gap_min` between them (never none)
///   * the space between them is at most `gap_max`; what is left over makes the covers larger
///   * a cover's side is a whole number of screen pixels (`ppp` pixels per point), and above `tile` a
///     multiple of `SIDE_STEP` of them: the smallest such side whose gap is no more than `gap_max`. The
///     gap takes up the rest, so `cols * side + (cols - 1) * gap` is `avail`.
/// A width too small for two keeps one cover of `tile` points, except that from `2 * tile + 14` two are
/// shown a little closer than `gap_min` (the smallest window with a sidebar open).
pub fn fill_row(avail: f32, ppp: f32, tile: f32, gap_min: f32, gap_max: f32) -> Tiles {
    let ppp = if ppp > 0.0 { ppp } else { 1.0 };
    let base_px = (tile * ppp).round().max(1.0);
    let base = base_px / ppp;
    let cols = (((avail + gap_min) / (tile + gap_min)).floor() as usize).max(1);
    if cols == 1 {
        if avail >= 2.0 * tile + 14.0 {
            return Tiles { cols: 2, side: base, gap: avail - 2.0 * base };
        }
        return Tiles { cols: 1, side: base, gap: 0.0 };
    }
    let between = (cols - 1) as f32;
    // the side, in pixels, at which the gap would be exactly `gap_max`
    let widest_gap = (avail - between * gap_max) * ppp / cols as f32;
    let side = ((widest_gap / SIDE_STEP).ceil() * SIDE_STEP).max(base_px) / ppp;
    Tiles { cols, side, gap: (avail - cols as f32 * side) / between }
}

/// Seconds a grid's width has to stay the same before its covers are decoded at their new size.
const SETTLE: f64 = 0.25;

/// Keeps a grid's covers at the size they are decoded at while its width is changing (a window being
/// resized), so that only the size it ends at is decoded. Meanwhile they are drawn stretched.
#[derive(Default)]
pub struct Settle {
    avail: f32,
    /// when `avail` last changed
    changed: f64,
    px: u32,
    was: u32,
}

impl Settle {
    /// The size, in pixels, to ask covers for this frame: `want` (the size they are drawn at) once the width
    /// `avail` has been the same for a quarter of a second, until then the size asked for before.
    pub fn px(&mut self, ctx: &egui::Context, avail: f32, want: u32) -> u32 {
        if let Some(wait) = self.step(ctx.input(|i| i.time), avail, want) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait)); // nothing else may redraw then
        }
        self.px
    }

    /// The size asked for before the current one (the same when there was none): what can stand in while the
    /// current size is decoded.
    pub fn was(&self) -> u32 {
        self.was
    }

    /// The size last given by `px` (0 before the first).
    pub fn current(&self) -> u32 {
        self.px
    }

    /// Returns how long to wait before looking again, when the size is being held back.
    fn step(&mut self, now: f64, avail: f32, want: u32) -> Option<f64> {
        if (avail - self.avail).abs() > 0.01 {
            self.avail = avail;
            self.changed = now;
        }
        if self.px == 0 {
            (self.px, self.was) = (want, want);
        }
        if want == self.px {
            return None;
        }
        let left = SETTLE - (now - self.changed);
        if left > 0.0 {
            return Some(left);
        }
        (self.was, self.px) = (self.px, want);
        None
    }
}

/// Call once per frame from the app shell, after all pages have drawn: applies the memory budget, forgets queued
/// work nobody asked for this frame, and asks for repaints only while something is fading in.
pub fn end_frame(ctx: &egui::Context) {
    if let Some(mut e) = started() {
        e.end_frame(ctx);
    }
}

/// Forget everything known about one folder (its files changed: "Rescan this album").
pub fn forget_folder(folder: &Path) {
    if let Some(mut e) = started() {
        e.forget(folder_key(folder));
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: [u8; 12] = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8];

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("music-recs-covers-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn flat(w: usize, h: usize, c: Color32) -> ColorImage {
        ColorImage::from_rgba_unmultiplied([w, h], &[c.r(), c.g(), c.b(), 255].repeat(w * h))
    }

    #[test]
    fn ranks_pictures_by_name() {
        let rank = |n: &str| picture_rank(Path::new(n));
        assert_eq!(rank("cover.jpg"), 0);
        assert_eq!(rank("Folder.JPG"), 1);
        assert_eq!(rank("front.png"), 2);
        assert_eq!(rank("album cover.jpg"), 3);
        assert_eq!(rank("AlbumArt_{1234}_Large.jpg"), 4);
        assert_eq!(rank("scan.jpg"), 5);
        assert_eq!(rank("back.jpg"), 8);
        assert_eq!(rank("cd.jpg"), 8);
        assert_eq!(rank("AlbumArtSmall.jpg"), 9);
        assert_eq!(rank("cover_small.jpg"), 9);
    }

    #[test]
    fn orders_sources_best_first() {
        let p = |s: &str| PathBuf::from(s);
        let pictures = vec![(0, p("a/back.jpg")), (0, p("a/scan.jpg")), (1, p("a/CD1/cover.jpg")), (0, p("a/folder.jpg")), (0, p("a/zz.png"))];
        let first = p("a/01 one.mp3");
        let got = art_sources(pictures, Some(&first));
        // the album folder's own pictures come before a disc folder's, the track's art before a back cover
        assert_eq!(got, vec![Src::File(p("a/folder.jpg")), Src::File(p("a/scan.jpg")), Src::Embedded(first.clone()), Src::File(p("a/back.jpg"))]);
        assert_eq!(art_sources(vec![], Some(&first)), vec![Src::Embedded(first)]);
        assert!(art_sources(vec![], None).is_empty());
        assert_eq!(art_sources(vec![(0, p("a/back.jpg"))], None), vec![Src::File(p("a/back.jpg"))]);
    }

    #[test]
    fn finds_sources_in_a_folder() {
        let dir = temp("album");
        std::fs::create_dir_all(dir.join("CD2")).unwrap();
        for f in ["10 ten.mp3", "2 two.mp3", "cover.jpg", "back.jpg", "notes.txt", "CD2/01 x.flac"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let got = find_sources(&dir);
        assert_eq!(got, vec![Src::File(dir.join("cover.jpg")), Src::Embedded(dir.join("2 two.mp3")), Src::File(dir.join("back.jpg"))]);
        assert!(find_sources(&dir.join("not there")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_keys_ignore_spelling_of_the_path() {
        let k = folder_key(Path::new(r"D:\Music\Arcade Fire\Reflektor (2013)"));
        assert_eq!(k, folder_key(Path::new(r"d:\music\arcade fire\reflektor (2013)\")));
        assert_eq!(k, folder_key(Path::new("D:/Music/Arcade Fire/Reflektor (2013)")));
        assert_ne!(k, folder_key(Path::new(r"D:\Music\Arcade Fire\Funeral (2004)")));
    }

    fn apic_frame(ver: u8) -> Vec<u8> {
        let mut data = vec![0u8]; // text encoding
        data.extend(b"image/jpeg\0");
        data.extend([3, 0]); // front cover, empty description
        data.extend(JPEG);
        let mut f = b"TIT2".to_vec();
        f.extend([0, 0, 0, 3, 0, 0, 0, b'h', b'i']);
        f.extend(b"APIC");
        let n = data.len() as u32;
        f.extend(if ver == 4 { [(n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f] } else { n.to_be_bytes() });
        f.extend([0, 0]);
        f.extend(data);
        f.extend([0u8; 16]); // padding
        f
    }

    #[test]
    fn reads_id3_pictures() {
        assert_eq!(id3_picture(3, 0, apic_frame(3)).as_deref(), Some(&JPEG[..]));
        assert_eq!(id3_picture(4, 0, apic_frame(4)).as_deref(), Some(&JPEG[..]));
        // version 2.2: three-letter names, three-byte sizes
        let mut data = vec![0u8];
        data.extend(b"JPG");
        data.extend([3, 0]);
        data.extend(JPEG);
        let mut f = b"PIC".to_vec();
        f.extend([0, 0, data.len() as u8]);
        f.extend(&data);
        assert_eq!(id3_picture(2, 0, f).as_deref(), Some(&JPEG[..]));
        // no picture, cut short, or rubbish: nothing, and no panic
        assert_eq!(id3_picture(3, 0, b"TIT2\0\0\0\x03\0\0\0hi".to_vec()), None);
        let whole = apic_frame(3);
        assert_eq!(id3_picture(3, 0, whole[..whole.len() - 24].to_vec()), None);
        assert_eq!(id3_picture(4, 0x40, vec![0xff; 40]), None);
        assert_eq!(id3_picture(3, 0, vec![]), None);
    }

    #[test]
    fn reads_an_mp3_file_with_a_tag() {
        let body = apic_frame(3);
        let n = body.len() as u32;
        let mut file = b"ID3\x03\0\0".to_vec();
        file.extend([(n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f]);
        file.extend(body);
        file.extend([0xff, 0xfb, 0x90, 0x00]);
        let p = temp("tagged.mp3");
        std::fs::write(&p, file).unwrap();
        assert_eq!(embedded_art(&p).as_deref(), Some(&JPEG[..]));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_flac_pictures() {
        let mut pic = 3u32.to_be_bytes().to_vec();
        pic.extend(10u32.to_be_bytes());
        pic.extend(b"image/jpeg");
        pic.extend(2u32.to_be_bytes());
        pic.extend(b"hi");
        pic.extend([0u8; 16]); // width, height, depth, colors
        pic.extend((JPEG.len() as u32).to_be_bytes());
        pic.extend(JPEG);
        let mut file = b"fLaC".to_vec();
        file.extend([0, 0, 0, 34]); // stream info
        file.extend([0u8; 34]);
        file.extend([0x80 | 6, 0, 0, pic.len() as u8]);
        file.extend(&pic);
        let p = temp("with-art.flac");
        std::fs::write(&p, &file).unwrap();
        assert_eq!(embedded_art(&p).as_deref(), Some(&JPEG[..]));
        // without the picture block, and with a picture block that claims more than it has
        std::fs::write(&p, [&b"fLaC"[..], &[0x80, 0, 0, 34], &[0u8; 34]].concat()).unwrap();
        assert_eq!(embedded_art(&p), None);
        std::fs::write(&p, &file[..file.len() - 4]).unwrap();
        assert_eq!(embedded_art(&p), None);
        let _ = std::fs::remove_file(&p);
    }

    fn bx(name: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut b = ((content.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend(name);
        b.extend(content);
        b
    }

    #[test]
    fn reads_mp4_pictures() {
        let data = bx(b"data", &[&[0, 0, 0, 13, 0, 0, 0, 0][..], &JPEG].concat());
        let ilst = bx(b"ilst", &[bx(b"\xa9nam", &bx(b"data", b"\0\0\0\x01\0\0\0\0Song")), bx(b"covr", &data)].concat());
        let meta = bx(b"meta", &[&[0u8; 4][..], &bx(b"hdlr", &[0u8; 12]), &ilst].concat());
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 20]), bx(b"udta", &meta)].concat());
        let file = [bx(b"ftyp", b"M4A \0\0\0\0"), bx(b"free", &[0u8; 5]), moov].concat();
        let p = temp("with-art.m4a");
        std::fs::write(&p, &file).unwrap();
        assert_eq!(embedded_art(&p).as_deref(), Some(&JPEG[..]));
        // no tags at all, and a file that stops in the middle
        std::fs::write(&p, [bx(b"ftyp", b"M4A \0\0\0\0"), bx(b"moov", &bx(b"mvhd", &[0u8; 20]))].concat()).unwrap();
        assert_eq!(embedded_art(&p), None);
        std::fs::write(&p, &file[..file.len() - 6]).unwrap();
        assert_eq!(embedded_art(&p), None);
        let _ = std::fs::remove_file(&p);
        assert_eq!(embedded_art(&temp("not-there.m4a")), None);
    }

    #[test]
    fn skips_picture_files_that_are_too_big() {
        let p = temp("big.jpg");
        std::fs::write(&p, [7u8; 100]).unwrap();
        assert_eq!(read_picture_file(&p, 101).map(|b| b.len()), Some(100));
        assert_eq!(read_picture_file(&p, 100), None);
        assert_eq!(read_picture_file(&p, 50), None);
        assert_eq!(read_picture_file(&temp("not-there.jpg"), 1000), None);
        assert_eq!(MAX_PICTURE_FILE, 60_000_000);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn decodes_to_the_size_asked_for() {
        let mut png = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(40, 20, image::Rgba([200, 30, 30, 255]))).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (good, bad) = (temp("cover.png"), temp("broken.png"));
        std::fs::write(&good, png.get_ref()).unwrap();
        std::fs::write(&bad, &png.get_ref()[..40]).unwrap();
        // the broken one is passed over; the picture is cut square and made the size asked for
        let img = load_cover(&[Src::File(bad.clone()), Src::File(good.clone())], 8).unwrap();
        assert_eq!((img.width(), img.height()), (8, 8));
        // never made bigger than it is
        let img = load_cover(&[Src::File(good.clone())], 64).unwrap();
        assert_eq!((img.width(), img.height()), (20, 20));
        assert!(load_cover(&[Src::File(bad.clone())], 8).is_none());
        assert!(load_cover(&[], 8).is_none());
        // a picture that claims to be enormous is refused instead of being given the memory
        let mut huge = png.get_ref().clone();
        huge[16..20].copy_from_slice(&50_000u32.to_be_bytes());
        assert!(decode_limited(&huge).is_none());
        // the job a worker runs: finds nothing in a folder that is not there
        let (found, picked, img, accent) = run_job(&Job { key: (1, 8), id: 1, sources: None, folder: Some(temp("no-such-album")), chosen: None });
        assert!(found.is_some_and(|f| f.is_empty()) && picked.is_none() && img.is_none() && accent.is_none());
        let (found, picked, img, accent) = run_job(&Job { key: (1, 8), id: 1, sources: Some(vec![Src::File(good.clone())]), folder: None, chosen: None });
        assert!(found.is_none() && picked == Some(Src::File(good.clone())) && img.is_some() && accent.is_some());
        let _ = std::fs::remove_file(&good);
        let _ = std::fs::remove_file(&bad);
    }

    fn png_file(name: &str, side: u32, color: [u8; 3]) -> PathBuf {
        let mut png = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(side, side, image::Rgba([color[0], color[1], color[2], 255]))).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let p = temp(name);
        std::fs::write(&p, png.get_ref()).unwrap();
        p
    }

    #[test]
    fn a_folder_shows_one_picture_at_every_size() {
        let red = png_file("one-small-red.png", 300, [200, 30, 30]);
        let blue = png_file("one-large-blue.png", 700, [30, 30, 200]);
        let green = png_file("one-larger-green.png", 900, [30, 200, 30]);
        let job = |px: u32, sources: &[&PathBuf], chosen: Option<&PathBuf>| {
            run_job(&Job { key: (1, px), id: 1, sources: Some(sources.iter().map(|p| Src::File((*p).clone())).collect()), folder: None, chosen: chosen.map(|p| Src::File(p.clone())) })
        };
        let is = |img: &Option<ColorImage>, px: usize, c: [u8; 3]| img.as_ref().is_some_and(|i| i.width() == px && i.pixels[0] == Color32::from_rgb(c[0], c[1], c[2]));
        // the first picture is big enough for a tile but not for the largest size: the next one is the
        // cover, at tile size too, and the search stops there
        for px in [100, 164, 328, 600] {
            let (_, picked, img, _) = job(px, &[&red, &blue, &green], None);
            assert_eq!(picked, Some(Src::File(blue.clone())), "at {px}");
            assert!(is(&img, px as usize, [30, 30, 200]), "at {px}");
        }
        // none is big enough: the largest, at every size, and the earlier of two the same size
        let small = png_file("one-smaller-green.png", 200, [30, 200, 30]);
        let twin = png_file("one-small-blue.png", 300, [30, 30, 200]);
        for px in [100, 250, 600] {
            let (_, picked, img, _) = job(px, &[&small, &red, &twin], None);
            assert_eq!(picked, Some(Src::File(red.clone())), "at {px}");
            assert!(is(&img, (px as usize).min(300), [200, 30, 30]), "at {px}");
        }
        // once chosen, that source is the only one read, whatever else is in the list
        let (found, picked, img, accent) = job(500, &[&blue, &green], Some(&red));
        assert!(found.is_none() && picked.is_none() && is(&img, 300, [200, 30, 30]));
        assert!(accent.is_some_and(|c| c.r() > c.b()), "the color is the chosen picture's");
        // a chosen picture that is gone gives no cover, not another picture
        let (_, _, img, _) = job(100, &[&blue], Some(&temp("one-gone.png")));
        assert!(img.is_none());
        for p in [red, blue, green, small, twin] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn the_engine_remembers_the_choice_until_the_folder_is_forgotten() {
        let ctx = egui::Context::default();
        let mut e = Engine::new(&ctx);
        let (a, b) = (Src::File(PathBuf::from("a.jpg")), Src::File(PathBuf::from("b.jpg")));
        let sources = [a.clone(), b.clone()];
        let queued = |e: &Engine| e.queue.0.lock().unwrap().iter().map(|j| (j.key, j.chosen.clone())).collect::<Vec<_>>();
        // the workers are kept out of it: what they would send back is put in by hand
        let take = |e: &mut Engine| e.queue.0.lock().unwrap().drain(..).collect::<Vec<Job>>();
        let (tx, rx) = unbounded();
        e.done = rx;
        e.queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let img = || Some(flat(4, 4, Color32::from_rgb(200, 30, 30)));
        ctx.run_ui(Default::default(), |ui| {
            let ctx = ui.ctx();
            assert!(matches!(e.get(ctx, 7, 100, Some(&sources), None), Pic::Loading));
            assert!(matches!(e.get(ctx, 7, 200, Some(&sources), None), Pic::Loading));
            let jobs = take(&mut e);
            assert!(jobs.len() == 2 && jobs.iter().all(|j| j.chosen.is_none()), "nothing chosen yet");
            // the first size to finish chose b: remembered
            tx.send(Done { key: (7, 100), id: jobs[0].id, found: None, picked: Some(b.clone()), img: img(), accent: Some(Color32::RED) }).unwrap();
            e.polled = false;
            assert!(matches!(e.get(ctx, 7, 100, Some(&sources), None), Pic::Ready(..)));
            assert_eq!(e.chosen.get(&7), Some(&b));
            assert_eq!(e.accents.get(&7), Some(&Color32::RED));
            // the other size came back with another picture: decoded again, from the remembered one
            tx.send(Done { key: (7, 200), id: jobs[1].id, found: None, picked: Some(a.clone()), img: img(), accent: Some(Color32::BLUE) }).unwrap();
            e.polled = false;
            assert!(matches!(e.get(ctx, 7, 200, Some(&sources), None), Pic::Loading));
            assert_eq!(queued(&e), vec![((7, 200), Some(b.clone()))]);
            assert_eq!(e.accents.get(&7), Some(&Color32::RED));
            take(&mut e);
            // every later size is decoded from the remembered source
            assert!(matches!(e.get(ctx, 7, 300, Some(&sources), None), Pic::Loading));
            assert_eq!(queued(&e), vec![((7, 300), Some(b.clone()))]);
            // forgetting the folder forgets the choice
            e.forget(7);
            assert!(e.chosen.is_empty() && e.accents.is_empty() && queued(&e).is_empty());
            assert!(matches!(e.get(ctx, 7, 300, Some(&sources), None), Pic::Loading));
            assert_eq!(queued(&e), vec![((7, 300), None)]);
            take(&mut e);
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn rows_of_covers_fill_the_width() {
        let (tile, gap_min, gap_max) = (164.0f32, 18.0f32, 34.0f32);
        for ppp in [1.0f32, 1.25, 1.5, 2.0] {
            let px = 1.0 / ppp;
            let mut w = 2.0 * tile + gap_min;
            while w < 3000.0 {
                let t = fill_row(w, ppp, tile, gap_min, gap_max);
                let before = ((w + gap_min) / (tile + gap_min)).floor() as usize;
                assert!(t.cols >= before && t.cols >= 2, "{w} at {ppp}: {t:?}");
                let row = t.cols as f32 * t.side + (t.cols - 1) as f32 * t.gap;
                assert!((row - w).abs() < 0.01, "{w} at {ppp}: {t:?} is {row} wide");
                assert!(t.gap >= gap_min - px && t.gap <= gap_max + 0.01, "{w} at {ppp}: {t:?}");
                // never smaller than the tile; at most what a gap of `gap_max` leaves, and one step
                let most = (tile + (tile + gap_max) / t.cols as f32 - (gap_max - gap_min)).max(tile) + SIDE_STEP * px;
                assert!(t.side >= tile - 0.5 * px && t.side <= most, "{w} at {ppp}: {t:?}");
                // whole pixels, and above the tile's own size in steps of four
                let side_px = t.side * ppp;
                assert!((side_px - side_px.round()).abs() < 0.01, "{w} at {ppp}: {t:?}");
                assert!(side_px.round() == (tile * ppp).round() || side_px.round() % SIDE_STEP == 0.0, "{w} at {ppp}: {t:?}");
                w += 0.37;
            }
        }
        // the window at its usual size: six across, a little larger than a tile, the gap under its limit
        let t = fill_row(1236.0, 1.0, tile, gap_min, gap_max);
        assert_eq!((t.cols, t.side), (6, 180.0));
        assert!((t.gap - 31.2).abs() < 0.001);
        // exactly room for three at the smallest gap, and for three at the largest
        assert_eq!(fill_row(3.0 * tile + 2.0 * gap_min, 1.0, tile, gap_min, gap_max), Tiles { cols: 3, side: tile, gap: gap_min });
        assert_eq!(fill_row(3.0 * tile + 2.0 * gap_max, 1.0, tile, gap_min, gap_max), Tiles { cols: 3, side: tile, gap: gap_max });
        // too narrow for two: one, and never none; two a little closer in the narrowest window with a sidebar
        assert_eq!(fill_row(200.0, 1.0, tile, gap_min, gap_max), Tiles { cols: 1, side: tile, gap: 0.0 });
        assert_eq!(fill_row(10.0, 1.0, tile, gap_min, gap_max).cols, 1);
        assert_eq!(fill_row(-5.0, 0.0, tile, gap_min, gap_max).cols, 1);
        assert_eq!(fill_row(2.0 * tile + 14.0, 1.0, tile, gap_min, gap_max), Tiles { cols: 2, side: tile, gap: 14.0 });
    }

    #[test]
    fn a_width_that_is_changing_keeps_the_decoded_size() {
        let mut s = Settle::default();
        // the first size is taken at once
        assert_eq!((s.step(10.0, 1236.0, 180), s.px, s.was), (None, 180, 180));
        assert_eq!((s.step(11.0, 1236.0, 180), s.px), (None, 180));
        // the window is being dragged: the old size stays, with a redraw asked for when it would settle
        let wait = s.step(12.0, 1250.0, 184);
        assert!(wait.is_some_and(|w| (w - SETTLE).abs() < 1e-9) && s.px == 180);
        assert!(s.step(12.1, 1270.0, 188).is_some() && s.px == 180);
        assert!(s.step(12.3, 1270.0, 188).is_some_and(|w| (w - 0.05).abs() < 1e-6) && s.px == 180, "0.2 s after the last change");
        // a quarter of a second after the last change: the size it ended at, and only that one
        assert_eq!((s.step(12.36, 1270.0, 188), s.px, s.was), (None, 188, 180));
        // a width that changes without changing the size holds nothing back
        assert_eq!((s.step(13.0, 1271.0, 188), s.px, s.was), (None, 188, 180));
        // back where it started within the wait: nothing new is decoded
        assert!(s.step(14.0, 1236.0, 180).is_some() && s.px == 188);
        assert_eq!((s.step(14.1, 1270.0, 188), s.px, s.was), (None, 188, 180));
    }

    #[test]
    fn accent_of_gray_with_a_red_patch_is_a_light_red() {
        let mut img = flat(64, 64, Color32::from_gray(128));
        for y in 8..24 {
            for x in 30..46 {
                img.pixels[y * 64 + x] = Color32::from_rgb(190, 20, 25);
            }
        }
        let c = accent_of(&img).unwrap();
        assert!(c.r() > 200, "{c:?}");
        assert!(c.r() as i32 - c.g() as i32 > 60 && c.r() as i32 - c.b() as i32 > 60, "{c:?}");
        let (_, _, l) = rgb_to_hsl(c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
        assert!((0.6..=0.8).contains(&l), "{c:?}");
    }

    #[test]
    fn accent_of_a_gray_cover_is_a_neutral_light_gray() {
        for v in [0, 128, 255] {
            let c = accent_of(&flat(50, 50, Color32::from_gray(v))).unwrap();
            assert!(c.r() == c.g() && c.g() == c.b() && c.r() >= 180, "{c:?}");
        }
        // a few stray colored pixels do not make a cover colorful
        let mut img = flat(64, 64, Color32::from_gray(40));
        img.pixels[0] = Color32::from_rgb(255, 0, 0);
        let c = accent_of(&img).unwrap();
        assert!(c.r() == c.g() && c.g() == c.b(), "{c:?}");
        assert!(accent_of(&ColorImage::from_rgba_unmultiplied([0, 0], &[])).is_none());
    }

    #[test]
    fn accent_takes_the_strongest_color_and_makes_it_readable() {
        // a dark blue cover with a smaller orange part: blue wins, lifted well above its own darkness
        let mut img = flat(60, 60, Color32::from_rgb(10, 20, 90));
        for y in 0..10 {
            for x in 0..20 {
                img.pixels[y * 60 + x] = Color32::from_rgb(230, 120, 20);
            }
        }
        let c = accent_of(&img).unwrap();
        assert!(c.b() > c.r() && c.b() > c.g() && c.b() > 200, "{c:?}");
        // a very pale, bright cover is brought down to something with color in it
        let c = accent_of(&flat(32, 32, Color32::from_rgb(200, 255, 200))).unwrap();
        assert!(c.g() > c.r() && c.g() as i32 - c.r() as i32 > 30, "{c:?}");
        // the same picture at another size gives nearly the same color
        let (a, b) = (accent_of(&flat(328, 328, Color32::from_rgb(200, 40, 160))).unwrap(), accent_of(&flat(82, 82, Color32::from_rgb(200, 40, 160))).unwrap());
        let near = |x: u8, y: u8| (x as i32 - y as i32).abs() <= 2;
        assert!(near(a.r(), b.r()) && near(a.g(), b.g()) && near(a.b(), b.b()), "{a:?} {b:?}");
    }

    #[test]
    fn hsl_round_trips() {
        for (r, g, b) in [(1.0, 0.0, 0.0), (0.2, 0.6, 0.4), (0.1, 0.1, 0.9), (0.5, 0.5, 0.5), (0.9, 0.8, 0.1), (0.7, 0.1, 0.6)] {
            let (h, s, l) = rgb_to_hsl(r, g, b);
            let (r2, g2, b2) = hsl_to_rgb(h, s, l);
            assert!((r - r2).abs() < 1e-3 && (g - g2).abs() < 1e-3 && (b - b2).abs() < 1e-3, "{r} {g} {b} -> {r2} {g2} {b2}");
        }
    }
}
