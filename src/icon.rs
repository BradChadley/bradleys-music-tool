//! The app's icon and mark, drawn in code: two wheels that mesh. The big one is a record (coral, with
//! grooves and a blue label) that has the teeth of a cog; a small blue cog sits at its upper right with a
//! tooth in the record's gap. Music, and a tool that works on it.
//!
//! The two are cut as real gears are: teeth of one size on both, twice as many on the record, the cog's
//! tooth pointing at the record's center where the record has a gap, and a hair of room all around so they
//! never touch (`tests` holds that at every size). The teeth are short with rounded corners, so the outline
//! stays smooth and the record stays a record.
//!
//! Small sizes have their own cut: under 32 pixels a tooth of the large drawing is thinner than a pixel, so
//! the small icon has fewer, larger teeth (ten and five in place of fourteen and seven) and no grooves.
//!
//! The window, the taskbar and the shortcut's `assets\bradleys-music-tool.ico` all come from this one drawing
//! (`bradleys-music-tool.exe --write-icon <file.ico or .png>` writes the file). The icon is the two wheels
//! and nothing else: no square behind them, and their holes are real holes, so whatever the icon lies on
//! shows around and through it. Inside the app the same two wheels are the mark (`paint_mark`): in the top
//! row, on the first-start screen and on an empty page.

/// The colors, top to bottom of the picture: coral for the record, blue for the cog and the label.
const CORAL: ([f32; 3], [f32; 3]) = ([255.0, 147.0, 126.0], [255.0, 88.0, 102.0]);
const BLUE: ([f32; 3], [f32; 3]) = ([166.0, 203.0, 255.0], [106.0, 144.0, 255.0]);

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// How the pair is cut, in the units of a 256 by 256 picture.
#[derive(Clone, Copy)]
struct Cut {
    /// teeth on the record and on the cog
    teeth: (u32, u32),
    /// the size of a tooth: a wheel's pitch radius (where the two wheels roll on each other) is this
    /// times half its number of teeth
    module: f32,
    /// how far a tooth stands out past the pitch radius, and how deep the gap goes inside it
    out: f32,
    deep: f32,
    /// half a tooth's width where it leaves the wheel, and at its tip
    half_root: f32,
    half_tip: f32,
    /// the rounding of a tooth's corners
    round: f32,
    /// grooves on the record, and its shine
    rings: u32,
    shine: bool,
    /// the direction from the record to the cog, in degrees (up and to the right). Chosen so that a tooth
    /// of the record points straight along an axis: each wheel is then its own mirror image, which is
    /// what lets a small icon come out even, pixel for pixel.
    way: f32,
}

/// The large cut: fourteen and seven short rounded teeth. (The teeth are as wide as they can be with the
/// two wheels still meshing with a hair of room: 0.4 more at the foot and they touch.)
const LARGE: Cut = Cut { teeth: (14, 7), module: 9.2, out: 5.0, deep: 6.0, half_root: 7.6, half_tip: 5.8, round: 2.0, rings: 3, shine: true, way: -540.0 / 14.0 };
/// The small cut, for under 32 pixels: ten and five larger teeth, and nothing a pixel cannot carry.
const SMALL: Cut = Cut { teeth: (10, 5), module: 12.6, out: 6.4, deep: 7.6, half_root: 10.4, half_tip: 7.6, round: 3.0, rings: 0, shine: false, way: -36.0 };

/// A hole is drawn only where it would be at least this many pixels across its half (a smaller one is a
/// gray speck), never smaller than `HOLE_LEAST`, so that it is a clean dot of whatever lies behind, and
/// only where `HOLE_RIM` pixels of the wheel are left around it (less, and the wheel is a faint ring).
const HOLE_FROM: f32 = 0.55;
const HOLE_LEAST: f32 = 0.85;
const HOLE_RIM: f32 = 1.2;

fn cut_for(n: u32) -> Cut {
    let mut cut = if n >= 32 { LARGE } else { SMALL };
    cut.shine &= n >= 48;
    if n < 48 {
        cut.rings = cut.rings.min(2);
    }
    cut
}

/// One wheel: a disc with teeth, somewhere in the picture.
#[derive(Clone, Copy)]
struct Wheel {
    at: (f32, f32),
    teeth: u32,
    /// the radius of the disc the teeth stand on, and of the circle their tips reach
    body: f32,
    tip: f32,
    /// the direction one tooth points in
    phase: f32,
    half_root: f32,
    half_tip: f32,
    round: f32,
}

/// How far a point is from a trapezoid that is `r1` half-wide at its foot and `r2` at its top, `he` half
/// as tall, standing upright on (0, 0): negative inside. (The usual formula for it.)
fn trapezoid(p: (f32, f32), r1: f32, r2: f32, he: f32) -> f32 {
    let px = p.0.abs();
    let k1 = (r2, he);
    let k2 = (r2 - r1, 2.0 * he);
    let ca = (px - px.min(if p.1 < 0.0 { r1 } else { r2 }), p.1.abs() - he);
    let t = (((k1.0 - px) * k2.0 + (k1.1 - p.1) * k2.1) / (k2.0 * k2.0 + k2.1 * k2.1)).clamp(0.0, 1.0);
    let cb = (px - k1.0 + k2.0 * t, p.1 - k1.1 + k2.1 * t);
    let s = if cb.0 < 0.0 && ca.1 < 0.0 { -1.0 } else { 1.0 };
    s * (ca.0 * ca.0 + ca.1 * ca.1).min(cb.0 * cb.0 + cb.1 * cb.1).sqrt()
}

impl Wheel {
    /// How far the point is from this wheel's center.
    fn from_center(&self, x: f32, y: f32) -> f32 {
        ((x - self.at.0).powi(2) + (y - self.at.1).powi(2)).sqrt()
    }

    /// Is the point on the wheel (its disc or one of its teeth)?
    fn covers(&self, x: f32, y: f32) -> bool {
        let r = self.from_center(x, y);
        if r <= self.body {
            return true;
        }
        if r > self.tip {
            return false;
        }
        // the nearest tooth, and the point as that tooth sees it: along its axis, and across it
        let pitch = std::f32::consts::TAU / self.teeth as f32;
        let mut turn = (y - self.at.1).atan2(x - self.at.0) - self.phase;
        turn -= pitch * (turn / pitch).round();
        let (along, across) = (r * turn.cos(), r * turn.sin());
        // a tooth: a trapezoid with rounded corners, its foot a little inside the disc
        let foot = self.body - 2.0 * self.round;
        let half = (self.tip - foot) / 2.0;
        let rho = self.round;
        trapezoid((across, along - foot - half), self.half_root - rho, self.half_tip - rho, half - rho) <= rho
    }
}

/// The pair of wheels in a picture: where they are and what each is made of.
struct Pair {
    record: Wheel,
    cog: Wheel,
    cut: Cut,
    /// the picture's units per pixel were scaled by this (for line widths)
    scale: f32,
    /// the top and the foot of the color ramp
    ramp: (f32, f32),
}

impl Cut {
    /// The pitch radius of the record and of the cog.
    fn pitch(&self) -> (f32, f32) {
        (self.module * self.teeth.0 as f32 / 2.0, self.module * self.teeth.1 as f32 / 2.0)
    }

    /// How far apart the two centers are: the two pitch circles a hair apart, so there is room between
    /// the teeth.
    fn apart(&self) -> f32 {
        let (rp1, rp2) = self.pitch();
        rp1 + rp2 + 0.8
    }
}

impl Pair {
    /// The pair for `cut`, scaled by `scale`: the record's center at `at`, the cog `apart` away from it in
    /// the direction `way` (radians; the cog stands up and to the right of the record).
    fn placed(cut: Cut, scale: f32, at: (f32, f32), way: f32) -> Self {
        let (n1, n2) = cut.teeth;
        let (rp1, rp2) = cut.pitch();
        let apart = cut.apart() * scale;
        let wheel = |at: (f32, f32), teeth: u32, rp: f32, phase: f32| Wheel {
            at,
            teeth,
            body: (rp - cut.deep) * scale,
            tip: (rp + cut.out) * scale,
            phase,
            half_root: cut.half_root * scale,
            half_tip: cut.half_tip * scale,
            round: cut.round * scale,
        };
        // the record has a gap where the cog is; the cog has a tooth pointing back at the record
        let record = wheel(at, n1, rp1, way + std::f32::consts::PI / n1 as f32);
        let cog = wheel((at.0 + apart * way.cos(), at.1 + apart * way.sin()), n2, rp2, way + std::f32::consts::PI);
        Self { record, cog, cut, scale, ramp: (cog.at.1 - cog.tip, record.at.1 + record.tip) }
    }

    /// The pair at `scale` with the middle of the box around both wheels at `mid`: the drawing as it is
    /// meant, not yet fitted to any pixels.
    fn new(cut: Cut, scale: f32, mid: (f32, f32)) -> Self {
        let (w, h) = Self::size(cut);
        let tip1 = (cut.pitch().0 + cut.out) * scale;
        Self::placed(cut, scale, (mid.0 - w / 2.0 * scale + tip1, mid.1 + h / 2.0 * scale - tip1), cut.way.to_radians())
    }

    /// The pair fitted to the pixels of an `n` by `n` picture with `room` pixels clear at its sides. Both
    /// centers are put on a pixel's middle or on the line between two pixels, so that each wheel comes
    /// out even on both sides and its hole is a clean dot. That moves the cog by less than half a pixel,
    /// which turns the pair a degree or two at the smallest sizes and makes it a few percent smaller at
    /// most; the teeth still mesh, since they follow the direction between the centers.
    fn fitted(cut: Cut, n: u32, room: f32) -> Self {
        let size = n as f32;
        let avail = (size - 2.0 * room).max(1.0);
        let (w, h) = Self::size(cut);
        let scale0 = avail / w.max(h);
        let (rp1, rp2) = cut.pitch();
        let way0 = cut.way.to_radians();
        let ideal = (cut.apart() * scale0 * way0.cos(), cut.apart() * scale0 * way0.sin());
        // the steps from the record's center to the cog's that are whole or half pixels, near the ideal:
        // the one that turns the pair least and shrinks it least, among those that fit
        let snap = |v: f32| (v * 2.0).round() / 2.0;
        // where a center goes between `low` and `high`: the half pixel nearest the middle that is inside
        let seat = |low: f32, high: f32| [0.0, -0.5, 0.5].iter().map(|d| snap((low + high) / 2.0) + d).find(|v| *v >= low - 0.02 && *v <= high + 0.02);
        let mut best: Option<(f32, f32, (f32, f32), (f32, f32))> = None;
        for i in -2..=2 {
            for j in -2..=2 {
                let step = (snap(ideal.0) + i as f32 * 0.5, snap(ideal.1) + j as f32 * 0.5);
                if step.0 <= 0.0 || step.1 >= 0.0 {
                    continue;
                }
                let scale = step.0.hypot(step.1) / cut.apart();
                let (tip1, tip2) = ((rp1 + cut.out) * scale, (rp2 + cut.out) * scale);
                // the record's center, with both wheels inside the room that is kept clear
                let (Some(x), Some(y)) = (seat(room + tip1, size - room - step.0 - tip2), seat(room - step.1 + tip2, size - room - tip1)) else {
                    continue;
                };
                let turned = (step.1.atan2(step.0) - way0).to_degrees().abs();
                let cost = turned + 60.0 * (1.0 - scale / scale0).abs();
                if best.is_none_or(|(c, ..)| cost < c) {
                    best = Some((cost, scale, (x, y), step));
                }
            }
        }
        match best {
            Some((_, scale, at, step)) => Self::placed(cut, scale, at, step.1.atan2(step.0)),
            None => Self::new(cut, scale0, (size / 2.0, size / 2.0)),
        }
    }

    /// How wide and how tall the pair is, in the units of the 256 picture.
    fn size(cut: Cut) -> (f32, f32) {
        let (rp1, rp2) = cut.pitch();
        let (tip1, tip2, apart, way) = (rp1 + cut.out, rp2 + cut.out, cut.apart(), cut.way.to_radians());
        (tip1 + apart * way.cos() + tip2, tip1 + apart * way.sin().abs() + tip2)
    }

    /// The color at a point, or None where neither wheel is (a wheel's hole is such a place).
    fn color(&self, x: f32, y: f32) -> Option<[f32; 3]> {
        let t = ((y - self.ramp.0) / (self.ramp.1 - self.ramp.0)).clamp(0.0, 1.0);
        let (coral, blue) = (mix(CORAL.0, CORAL.1, t), mix(BLUE.0, BLUE.1, t));
        // a hole in a disc of radius `around`, where there is one at all (see `HOLE_FROM`)
        let hole = |meant: f32, around: f32| Some(meant.max(HOLE_LEAST)).filter(|h| meant >= HOLE_FROM && around - h >= HOLE_RIM);
        if self.cog.covers(x, y) {
            let open = hole(self.cog.body * 0.4, self.cog.body).is_some_and(|h| self.cog.from_center(x, y) <= h);
            return (!open).then_some(blue);
        }
        if !self.record.covers(x, y) {
            return None;
        }
        let (r, body) = (self.record.from_center(x, y), self.record.body);
        let label = body * 0.34;
        if r <= label {
            let open = hole(label * 0.26, label).is_some_and(|h| r <= h);
            return (!open).then_some(blue);
        }
        // the grooves: thin darker rings between the label and the teeth
        let line = (3.2 * self.scale).max(0.9);
        for i in 0..self.cut.rings {
            let at = label + (body * 0.9 - label) * (i + 1) as f32 / (self.cut.rings as f32 + 0.6);
            if (r - at).abs() <= line / 2.0 {
                return Some(mix(coral, [0.0; 3], 0.17 - i as f32 * 0.03));
            }
        }
        // the shine: a short bright arc at the upper left, with round ends
        if self.cut.shine {
            let (mid, half) = (body * 0.8, body * 0.08);
            let (dx, dy) = (x - self.record.at.0, y - self.record.at.1);
            let turn = dy.atan2(dx).to_degrees();
            let on_arc = (r - mid).abs() <= half && (-166.0..=-104.0).contains(&turn);
            let end = |deg: f32| {
                let a = deg.to_radians();
                ((dx - mid * a.cos()).powi(2) + (dy - mid * a.sin()).powi(2)).sqrt() <= half
            };
            if on_arc || end(-166.0) || end(-104.0) {
                return Some(mix(coral, [255.0; 3], 0.22));
            }
        }
        Some(coral)
    }
}

/// `n` by `n` pixels of RGBA. A pixel is measured at 8 by 8 points (4 by 4 in a picture larger than any
/// icon, to keep the app from waiting on it), so an edge has 64 shades to be smooth with. Where two
/// colors share a pixel they are mixed as light mixes (on their squares), not as the numbers stand, so
/// the seam between coral and blue does not come out darker than either.
fn render(n: u32, sample: impl Fn(f32, f32) -> ([f32; 3], f32)) -> Vec<u8> {
    let grid = if n <= 256 { 8 } else { 4 };
    let (step, all) = (1.0 / grid as f32, (grid * grid) as f32);
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for py in 0..n {
        for px in 0..n {
            let (mut sum, mut cover) = ([0.0f32; 3], 0.0f32);
            for sy in 0..grid {
                for sx in 0..grid {
                    let (c, a) = sample(px as f32 + (sx as f32 + 0.5) * step, py as f32 + (sy as f32 + 0.5) * step);
                    for k in 0..3 {
                        sum[k] += c[k] * c[k] * a;
                    }
                    cover += a;
                }
            }
            if cover > 0.0 {
                let shade = |k: usize| (sum[k] / cover).sqrt().round() as u8;
                out.extend([shade(0), shade(1), shade(2), (cover / all * 255.0).round() as u8]);
            } else {
                out.extend([0, 0, 0, 0]);
            }
        }
    }
    out
}

/// The two wheels on nothing, in the middle of `n` by `n` pixels, with `room` pixels clear at the left and
/// the right (the pair is wider than it is tall, so more is clear above and below).
fn wheels(n: u32, room: f32) -> Vec<u8> {
    let pair = Pair::fitted(cut_for(n), n, room);
    render(n, |x, y| match pair.color(x, y) {
        Some(c) => (c, 1.0),
        None => ([0.0; 3], 0.0),
    })
}

/// How many pixels the icon keeps clear at its sides: one where every pixel counts, a little more when it
/// is large, so that it does not touch what stands beside it.
fn icon_room(n: u32) -> f32 {
    (n as f32 / 32.0).round().max(1.0)
}

/// The icon at `n` by `n` pixels, as RGBA bytes: the two wheels, with nothing behind them.
pub fn rgba(n: u32) -> Vec<u8> {
    wheels(n, icon_room(n))
}

/// The mark as the app shows it: the same two wheels filling `n` by `n` pixels (a pixel of room at each
/// edge, so no tooth is cut off).
pub fn mark_rgba(n: u32) -> Vec<u8> {
    wheels(n, 1.0)
}

/// Paints the mark into `rect` (a square), drawn for exactly the pixels it covers and kept from then on.
/// `fade` under 1 makes it quieter (an empty page's mark stands back).
pub fn paint_mark(ui: &egui::Ui, rect: egui::Rect, fade: f32) {
    let px = ((rect.width() * ui.ctx().pixels_per_point()).round() as u32).clamp(8, 512);
    let id = egui::Id::unique(("app-mark", px));
    let kept: Option<egui::TextureHandle> = ui.ctx().data(|d| d.get_temp(id));
    let texture = kept.unwrap_or_else(|| {
        let image = egui::ColorImage::from_rgba_unmultiplied([px as usize, px as usize], &mark_rgba(px));
        let made = ui.ctx().load_texture(format!("app-mark-{px}"), image, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(id, made.clone()));
        made
    });
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    ui.painter().image(texture.id(), rect, uv, egui::Color32::WHITE.gamma_multiply(fade.clamp(0.0, 1.0)));
}

/// The icon the window and the taskbar show.
pub fn window_icon() -> egui::IconData {
    egui::IconData { rgba: rgba(64), width: 64, height: 64 }
}

/// Writes the icon to a file: an `.ico` with every size Windows asks for, or one large `.png`.
pub fn write(path: &std::path::Path) -> Result<(), String> {
    let said = |e: &dyn std::fmt::Display| format!("couldn't write {}: {e}", path.display());
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
        return image::save_buffer(path, &rgba(256), 256, 256, image::ExtendedColorType::Rgba8).map_err(|e| said(&e));
    }
    let frames: Result<Vec<_>, _> = [16u32, 20, 24, 32, 40, 48, 64, 256].iter().map(|&n| image::codecs::ico::IcoFrame::as_png(&rgba(n), n, n, image::ExtendedColorType::Rgba8)).collect();
    let file = std::fs::File::create(path).map_err(|e| said(&e))?;
    image::codecs::ico::IcoEncoder::new(file).encode_images(&frames.map_err(|e| said(&e))?).map_err(|e| said(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_wheel_is_its_own_mirror_image() {
        // the record on both axes, the cog on the one its tooth points along: what lets a small icon come
        // out even, pixel for pixel
        for cut in [LARGE, SMALL] {
            let pair = Pair::new(cut, 1.0, (128.0, 128.0));
            let even = |w: &Wheel, flip: (f32, f32)| {
                (0..4000).all(|i| {
                    let (dx, dy) = ((i % 80) as f32 * 1.1 - 43.7, (i / 80) as f32 * 1.7 - 42.1);
                    // (not on an edge, where rounding may fall either way)
                    let inside = |x: f32, y: f32| [(0.05, 0.0), (-0.05, 0.0), (0.0, 0.05), (0.0, -0.05)].map(|(a, b)| w.covers(x + a, y + b));
                    let (here, there) = (inside(w.at.0 + dx, w.at.1 + dy), inside(w.at.0 + dx * flip.0, w.at.1 + dy * flip.1));
                    here.iter().any(|v| *v != here[0]) || there.iter().any(|v| *v != there[0]) || here[0] == there[0]
                })
            };
            assert!(even(&pair.record, (-1.0, 1.0)) && even(&pair.record, (1.0, -1.0)), "the record ({} teeth)", cut.teeth.0);
            assert!(even(&pair.cog, (-1.0, 1.0)) || even(&pair.cog, (1.0, -1.0)), "the cog ({} teeth)", cut.teeth.1);
        }
    }

    #[test]
    fn the_icon_is_fitted_to_its_pixels() {
        for n in [16u32, 20, 22, 24, 32, 40, 48, 64, 256] {
            let cut = cut_for(n);
            let room = icon_room(n);
            let pair = Pair::fitted(cut, n, room);
            // both centers on a pixel's middle or on the line between two pixels
            for wheel in [pair.record, pair.cog] {
                for v in [wheel.at.0, wheel.at.1] {
                    assert!(((v * 2.0).round() - v * 2.0).abs() < 1e-3, "{n}: a center at {v}");
                }
            }
            // inside the room it was given, in the middle to half a pixel, and nearly as large as it can be
            let (left, right) = (pair.record.at.0 - pair.record.tip, pair.cog.at.0 + pair.cog.tip);
            let (top, foot) = (pair.cog.at.1 - pair.cog.tip, pair.record.at.1 + pair.record.tip);
            let size = n as f32;
            assert!(left >= room - 0.03 && right <= size - room + 0.03 && top >= room - 0.03 && foot <= size - room + 0.03, "{n}: {left} to {right}, {top} to {foot}");
            assert!((left - (size - right)).abs() <= 1.01 && (top - (size - foot)).abs() <= 1.01, "{n}: not in the middle");
            assert!(right - left >= (size - 2.0 * room) * 0.9, "{n}: only {} wide", right - left);
            // turned by a few degrees at most, and still meshing: no point of one wheel on the other
            let turned = ((pair.cog.at.1 - pair.record.at.1).atan2(pair.cog.at.0 - pair.record.at.0).to_degrees() - cut.way).abs();
            assert!(turned < 4.0, "{n}: turned by {turned} degrees");
            let steps = n * 8;
            let both = (0..steps * steps).filter(|i| {
                let (x, y) = ((i % steps) as f32 / 8.0, (i / steps) as f32 / 8.0);
                pair.record.covers(x, y) && pair.cog.covers(x, y)
            });
            assert_eq!(both.count(), 0, "{n}: the wheels touch");
        }
    }

    #[test]
    fn the_two_wheels_mesh_and_never_touch() {
        for cut in [LARGE, SMALL] {
            let pair = Pair::new(cut, 1.0, (128.0, 128.0));
            let (record, cog) = (pair.record, pair.cog);
            // they reach into each other: the tips of one pass the tips of the other
            let apart = record.from_center(cog.at.0, cog.at.1);
            assert!(apart < record.tip + cog.tip - cut.out, "the wheels do not reach into each other");
            // and no point belongs to both, with room to spare: a point on one wheel has none of the other
            // within 0.6 of it (a hair at 256 pixels, and what keeps the two apart when drawn small)
            let (mut both, mut near, mut between) = (0, 0, 0);
            let mut y = 20.0f32;
            while y < 236.0 {
                let mut x = 20.0f32;
                while x < 236.0 {
                    let (a, b) = (record.covers(x, y), cog.covers(x, y));
                    both += (a && b) as u32;
                    if a {
                        near += [(0.6, 0.0), (-0.6, 0.0), (0.0, 0.6), (0.0, -0.6)].iter().any(|(dx, dy)| cog.covers(x + dx, y + dy)) as u32;
                    }
                    // (the dark between the teeth, where the two wheels are within reach of each other)
                    between += (!a && !b && record.from_center(x, y) < record.tip && cog.from_center(x, y) < cog.tip) as u32;
                    x += 0.25;
                }
                y += 0.25;
            }
            assert_eq!((both, near), (0, 0), "the wheels touch ({} teeth)", cut.teeth.0);
            assert!(between > 0);
            // a tooth of the cog stands in a gap of the record, on the line between the two centers
            let on_line = |t: f32| (record.at.0 + (cog.at.0 - record.at.0) * t, record.at.1 + (cog.at.1 - record.at.1) * t);
            let mid = on_line((record.body + (record.tip - record.body) * 0.5) / apart);
            assert!(cog.covers(mid.0, mid.1) && !record.covers(mid.0, mid.1), "the cog's tooth is not in the record's gap");
        }
    }

    #[test]
    fn the_icon_is_a_coral_record_and_a_blue_cog_at_every_size() {
        for n in [16u32, 20, 24, 32, 40, 48, 64, 256] {
            let px = rgba(n);
            assert_eq!(px.len(), (n * n * 4) as usize);
            let at = |x: u32, y: u32| -> [u8; 4] { px[((y * n + x) * 4) as usize..][..4].try_into().unwrap() };
            // nothing is behind the wheels: the corners and the whole top row are clear, and no pixel that
            // is drawn is dark (a square behind them, or a filled hole, would be)
            assert_eq!((at(0, 0)[3], at(n - 1, n - 1)[3]), (0, 0), "{n}");
            assert!((0..n).all(|x| at(x, 0)[3] == 0), "{n}: something is drawn in the top row");
            for (i, c) in px.chunks_exact(4).enumerate() {
                assert!(c[3] == 0 || c[..3].iter().any(|&v| v > 150), "{n}: a dark pixel at {}, {}", i as u32 % n, i as u32 / n);
            }
            // the wheels reach from one side to the other, less the room the icon keeps clear
            let column = |x: u32| (0..n).any(|y| at(x, y)[3] > 0);
            let room = icon_room(n) as u32;
            assert!(!column(room - 1) && !column(n - room), "{n}: the wheels are in the room kept clear");
            assert!((column(room) || column(room + 1)) && (column(n - room - 1) || column(n - room - 2)), "{n}: the wheels do not fill the icon");
            // the middle of the cog is a real hole from 20 pixels up, and the record's from 24 (under that
            // one would be a speck, or leave a faint ring of the wheel): nothing is drawn there
            let pair = Pair::fitted(cut_for(n), n, icon_room(n));
            assert_eq!(pair.color(pair.cog.at.0, pair.cog.at.1).is_none(), n >= 20, "{n}: the cog's hole");
            assert_eq!(pair.color(pair.record.at.0, pair.record.at.1).is_none(), n >= 24, "{n}: the record's hole");
            // more coral than blue, and both are there; the record is at the lower left, the cog at the upper right
            let coral = |c: [u8; 4]| c[0] > 200 && c[2] < 160;
            let blue = |c: [u8; 4]| c[2] > 200 && c[0] < 190;
            let (mut reds, mut blues, mut red_x, mut blue_x, mut red_y, mut blue_y) = (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
            for y in 0..n {
                for x in 0..n {
                    let c = at(x, y);
                    if coral(c) {
                        (reds, red_x, red_y) = (reds + 1, red_x + x, red_y + y);
                    } else if blue(c) {
                        (blues, blue_x, blue_y) = (blues + 1, blue_x + x, blue_y + y);
                    }
                }
            }
            assert!(reds > blues && blues > 0, "{n}: {reds} coral, {blues} blue");
            assert!(blue_x / blues > red_x / reds && blue_y / blues < red_y / reds, "{n}: the cog is not up and to the right of the record");
        }
        // the mark alone has no square: its corners are clear, and the pair (which is wider than it is
        // tall) reaches from the left edge to the right one
        for n in [16u32, 22, 64] {
            let px = mark_rgba(n);
            let alpha = |x: u32, y: u32| px[((y * n + x) * 4 + 3) as usize];
            assert_eq!((alpha(0, 0), alpha(n - 1, n - 1)), (0, 0), "{n}");
            let column = |x: u32| (0..n).any(|y| alpha(x, y) > 0);
            assert!((column(1) || column(2)) && (column(n - 2) || column(n - 3)), "{n}: the mark does not fill its square");
        }
    }
}
