//! Background loading of album art: web addresses and local files, turned into textures.
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::{ColorImage, TextureHandle, TextureOptions};
use std::collections::HashMap;
use std::time::Duration;

/// Decodes a picture with hard limits on its dimensions and the memory it may use.
fn decode_limited(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(12_000);
    limits.max_image_height = Some(12_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()
}

/// Whether a file from a download is really a picture: one of the kinds the importer takes (JPEG, PNG,
/// WebP, GIF) by its content, no larger than 60 MB and 12,000 pixels a side. Only its header is read. A
/// file that merely has a picture's name is never copied into the library or put into a track.
pub fn is_picture(path: &std::path::Path) -> bool {
    use image::ImageFormat::{Gif, Jpeg, Png, WebP};
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= 60_000_000) {
        return false;
    }
    let path = path.to_path_buf();
    std::panic::catch_unwind(move || {
        let Some(reader) = image::ImageReader::open(&path).ok().and_then(|r| r.with_guessed_format().ok()) else { return false };
        matches!(reader.format(), Some(Jpeg | Png | WebP | Gif)) && reader.into_dimensions().is_ok_and(|(w, h)| (1..=12_000).contains(&w) && (1..=12_000).contains(&h))
    })
    .unwrap_or(false)
}

enum State {
    Loading,
    Ready(TextureHandle),
    Failed,
}

pub struct Images {
    map: HashMap<String, State>,
    req: Sender<String>,
    done: Receiver<(String, Option<ColorImage>)>,
}

impl Images {
    pub fn new(ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = unbounded::<String>();
        let (done_tx, done_rx) = unbounded();
        for _ in 0..4 {
            let (rx, tx, ctx) = (req_rx.clone(), done_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(30)))
                    .http_status_as_error(false)
                    .https_only(true) // never plain http, a redirect included
                    .user_agent(crate::importer::ctx::USER_AGENT)
                    .build()
                    .into();
                while let Ok(key) = rx.recv() {
                    // Pictures come from strangers (inside downloads) or from online answers. Only https is fetched;
                    // a local path must be a real local file (never a \\server\share path, which would make
                    // Windows try to sign in to that server); both are size-capped before decoding.
                    let bytes = if key.starts_with("https://") {
                        agent.get(&key).call().ok().filter(|r| r.status().as_u16() == 200).and_then(|mut r| r.body_mut().with_config().limit(60_000_000).read_to_vec().ok())
                    } else if key.starts_with("http") || key.starts_with(r"\\") || key.starts_with("//") {
                        None
                    } else {
                        std::fs::metadata(&key).ok().filter(|m| m.is_file() && m.len() <= 60_000_000).and_then(|_| std::fs::read(&key).ok())
                    };
                    // a picture built to exhaust memory, or one that crashes the decoder, just shows as "no picture"
                    let img = bytes.and_then(|b| std::panic::catch_unwind(|| decode_limited(&b)).ok().flatten()).map(|i| {
                        let i = i.thumbnail(640, 640).to_rgba8();
                        ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                    });
                    let _ = tx.send((key, img));
                    ctx.request_repaint();
                }
            });
        }
        Self { map: HashMap::new(), req: req_tx, done: done_rx }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, img)) = self.done.try_recv() {
            let st = match img {
                Some(ci) => State::Ready(ctx.load_texture(&key, ci, TextureOptions::LINEAR)),
                None => State::Failed,
            };
            self.map.insert(key, st);
        }
    }

    /// Texture if ready; starts loading otherwise.
    pub fn get(&mut self, key: &str) -> Option<&TextureHandle> {
        if key.is_empty() {
            return None;
        }
        if !self.map.contains_key(key) {
            self.map.insert(key.to_string(), State::Loading);
            let _ = self.req.send(key.to_string());
        }
        match self.map.get(key) {
            Some(State::Ready(t)) => Some(t),
            _ => None,
        }
    }
}
