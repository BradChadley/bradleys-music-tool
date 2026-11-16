//! Background picture loading with a disk cache: album covers (with backup sources) and artist photos.
//!   * a cover whose link fails (no art uploaded yet, a download error) is looked up on Deezer, then Bandcamp
//!   * artist photos come from Deezer (Last.fm stopped giving out artist pictures in 2019)
//!   * a picture that failed is tried once more a little later, in case it was a passing network problem
//!   * pictures for the open artist panel jump the queue ahead of the long release list
use crate::net::{days, Net};
use crate::sources;
use crate::util::{album_key, artist_key};
use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::{ColorImage, TextureHandle, TextureOptions};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a failed picture waits before its one retry.
const RETRY_AFTER: Duration = Duration::from_secs(90);

enum State {
    /// `tries`: how many attempts already failed (0 the first time)
    Loading { tries: u8 },
    Ready(TextureHandle),
    Failed { at: Instant, tries: u8 },
}

/// What a caller can draw right now.
pub enum Pic {
    Ready(TextureHandle),
    /// still downloading or looking for one
    Loading,
    /// there's no picture to be had
    None,
}

#[derive(Clone)]
enum Want {
    /// a cover: its link (may be empty), plus who/what it is so a backup can be found
    Cover { url: String, artist: String, title: String },
    /// an artist's photo
    Artist { name: String },
}

struct Job {
    key: String,
    want: Want,
}

pub struct Images {
    map: HashMap<String, State>,
    prio: Sender<Job>,
    normal: Sender<Job>,
    done: Receiver<(String, Option<ColorImage>)>,
}

fn fetch(net: &Net, url: &str) -> Option<ColorImage> {
    if url.is_empty() {
        return None;
    }
    let b = net.get_bytes("img", url, days(365), None).ok()?;
    // (kept a little larger than the largest tile, so a cover is not blown up on a roomy window)
    let i = image::load_from_memory(&b).ok()?.thumbnail(320, 320).to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw()))
}

fn resolve(net: &Net, want: &Want) -> Option<ColorImage> {
    match want {
        // its own link, then Deezer, then the album's Bandcamp page (MusicBrainz's link to it, or Bandcamp's search)
        Want::Cover { url, artist, title } => fetch(net, url)
            .or_else(|| sources::deezer_cover(net, artist, title).and_then(|u| fetch(net, &u)))
            .or_else(|| crate::albumlink::bandcamp_cover(net, artist, title, url).and_then(|u| fetch(net, &u))),
        Want::Artist { name } => sources::deezer_artist_photo(net, name).and_then(|u| fetch(net, &u)),
    }
}

impl Images {
    pub fn new(net: Arc<Net>, ctx: egui::Context) -> Self {
        let (prio_tx, prio_rx) = unbounded::<Job>();
        let (normal_tx, normal_rx) = unbounded::<Job>();
        let (done_tx, done_rx) = unbounded();
        for _ in 0..4 {
            let (prio_rx, normal_rx, tx, net, ctx) = (prio_rx.clone(), normal_rx.clone(), done_tx.clone(), net.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                // the open panel's pictures first; otherwise whichever queue has work
                let job = match prio_rx.try_recv() {
                    Ok(j) => j,
                    Err(_) => {
                        let r = crossbeam_channel::select! {
                            recv(prio_rx) -> j => j,
                            recv(normal_rx) -> j => j,
                        };
                        match r {
                            Ok(j) => j,
                            Err(_) => break, // the window closed
                        }
                    }
                };
                let img = resolve(&net, &job.want);
                let _ = tx.send((job.key, img));
                ctx.request_repaint();
            });
        }
        Self { map: HashMap::new(), prio: prio_tx, normal: normal_tx, done: done_rx }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, img)) = self.done.try_recv() {
            let Some(entry) = self.map.get_mut(&key) else { continue };
            let tries = match *entry {
                State::Loading { tries } => tries,
                _ => continue,
            };
            *entry = match img {
                Some(ci) => State::Ready(ctx.load_texture(&key, ci, TextureOptions::LINEAR)),
                None => State::Failed { at: Instant::now(), tries: tries + 1 },
            };
        }
    }

    fn want(&mut self, key: String, want: Want, prio: bool) -> Pic {
        // queue it: the first time, or once more a little later after a failure (a passing network problem)
        let start = match self.map.get(&key) {
            None => Some(0),
            Some(State::Failed { at, tries }) if *tries < 2 && at.elapsed() > RETRY_AFTER => Some(*tries),
            _ => None,
        };
        if let Some(tries) = start {
            self.map.insert(key.clone(), State::Loading { tries });
            let job = Job { key: key.clone(), want };
            let _ = if prio { self.prio.send(job) } else { self.normal.send(job) };
        }
        match self.map.get(&key) {
            Some(State::Ready(t)) => Pic::Ready(t.clone()),
            Some(State::Loading { .. }) => Pic::Loading,
            _ => Pic::None,
        }
    }

    /// An album cover. `url` may be empty; if it's missing or broken, Deezer is asked for one.
    pub fn cover(&mut self, url: &str, artist: &str, title: &str, prio: bool) -> Pic {
        if url.is_empty() && (artist.is_empty() || title.is_empty()) {
            return Pic::None;
        }
        let key = if url.is_empty() { format!("cover:{}|{}", artist_key(artist), album_key(title)) } else { url.to_string() };
        self.want(key, Want::Cover { url: url.to_string(), artist: artist.to_string(), title: title.to_string() }, prio)
    }

    /// An artist's photo (from Deezer, matched by exact name).
    pub fn artist_photo(&mut self, name: &str, prio: bool) -> Pic {
        if name.trim().is_empty() {
            return Pic::None;
        }
        self.want(format!("artist:{}", artist_key(name)), Want::Artist { name: name.to_string() }, prio)
    }
}
