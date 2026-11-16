//! How the Backup page is laid out.
//!
//! Top to bottom, in one centered stretch of the window:
//!   - one large sentence that says whether the music is safe, or what needs a look;
//!   - two cards side by side, the main backup and the second. Both are made of the same three bands
//!     (the drive, the last backup, what can be done), and the bands are drawn a pair at a time so they
//!     stand at the same height in both cards and the two read across;
//!   - only when there is one to show: a preview, the damaged copies a whole check found;
//!   - the albums the most recent copying backup brought, as one row of covers.
//!
//! In a narrow window the cards stand one over the other. Everything here only draws and starts what a
//! click asks for; reading the drives and the log, and running the script, is in `backup.rs`.
use super::*;
use egui::text::{LayoutJob, TextFormat};
use egui::{pos2, CornerRadius, UiBuilder};

/// The page at its widest, the room between its cards, a card's padding, and the room over and under
/// the thin line between two bands.
const PAGE: f32 = 1180.0;
const GAP: f32 = 20.0;
const PAD: f32 = 24.0;
const SEAM: f32 = 16.0;
/// The height of the row of words that start things, and of the progress words that take its place.
const ROW: f32 = 24.0;
/// Under this width the two cards stand one over the other.
const SIDE_BY_SIDE: f32 = 860.0;

/// The three parts every backup's card is made of.
#[derive(Clone, Copy, PartialEq)]
enum Band {
    Drive,
    Last,
    Actions,
}
const BANDS: [Band; 3] = [Band::Drive, Band::Last, Band::Actions];
const BOTH: [Which; 2] = [Which::Main, Which::Second];

impl BackupPage {
    /// Draws the whole page into the scrolling area.
    pub(super) fn page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let full = ui.available_width();
        let wide = (full - 24.0).clamp(240.0, PAGE).floor();
        let left = (ui.available_rect_before_wrap().left() + (full - wide) / 2.0).round();
        // how much room old copies and removed files take: measured in the background, said under the pointer
        let folders = self.status.as_ref().map(|s| [s.dest_exists.then(|| s.dest.clone()), s.second.folder.clone()]).unwrap_or_default();
        for (which, folder) in BOTH.into_iter().zip(folders) {
            self.measure_room(ctx, which, folder.as_deref());
        }
        ui.add_space(26.0);
        self.summary(ui, left, wide);
        ui.add_space(28.0);
        self.cards(ui, ctx, left, wide);
        for which in BOTH {
            self.preview_panel(ui, which, left, wide);
            self.damaged_panel(ui, ctx, which, left, wide);
        }
        self.copied(ui, left, wide);
        ui.add_space(28.0);
    }

    /// Asks, once for a folder (and again after a backup to it), how much the old copies (`_changed`) and
    /// what left the library (`_deleted`) take in this backup.
    fn measure_room(&mut self, ctx: &egui::Context, which: Which, folder: Option<&str>) {
        let Some(folder) = folder.filter(|f| !f.is_empty()) else { return };
        let at = which as usize;
        if self.room_for[at] != folder && self.job.is_none() {
            self.room_for[at] = folder.to_string();
            let (tx, ctx, folder) = (self.tx.clone(), ctx.clone(), PathBuf::from(folder));
            std::thread::spawn(move || {
                let _ = tx.send(Msg::Room(which, folder_bytes(&folder.join("_changed")), folder_bytes(&folder.join("_deleted"))));
                ctx.request_repaint();
            });
        }
    }

    /// What a backup is called on the page: its drive's name.
    fn name_of(&self, which: Which) -> String {
        let label = self.status.as_ref().and_then(|s| match which {
            Which::Main => match &s.drive {
                Drive::Good { label, .. } => Some(label.clone()),
                _ => None,
            },
            Which::Second => Some(s.second.label.clone()),
        });
        label.filter(|l| !l.is_empty()).unwrap_or_else(|| if which == Which::Main { "the main backup".to_string() } else { "the second backup".to_string() })
    }

    /// The last check of a whole backup: the one started here just now (one that was stopped is in no
    /// log), else the last one in the log. None while one is running: what the last one found comes back
    /// when it ends.
    fn last_check(&self, which: Which) -> Option<&Run> {
        if self.job.as_ref().is_some_and(|j| j.which == which && j.kind == Kind::CheckAll) {
            return None;
        }
        let here = self.finished.as_ref().filter(|f| f.0 == which && f.1 == Kind::CheckAll).map(|f| &f.2);
        let logged = self.status.as_ref().and_then(|s| match which {
            Which::Main => s.check.as_ref(),
            Which::Second => s.second.check.as_ref(),
        });
        here.or(logged)
    }

    // ------------------------------------------------------------------ the sentence at the top

    /// Whether the music is safe, in one large sentence in the middle; when something needs a look, that
    /// takes its place, with what it is under it.
    fn summary(&self, ui: &mut Ui, left: f32, wide: f32) {
        let mut column = ui.available_rect_before_wrap();
        (column.min.x, column.max.x) = (left, left + wide);
        ui.scope_builder(UiBuilder::new().max_rect(column).layout(Layout::top_down(Align::Center)), |ui| {
            let Some(s) = &self.status else {
                title(ui, "Checking your backups", 34.0, DIM());
                return;
            };
            if s.dest.is_empty() {
                title(ui, "No backup is set up yet", 34.0, WARN());
                return;
            }
            let alerts = self.alerts();
            if alerts.iter().any(|a| a.1) {
                title(ui, "Your backups need a look", 34.0, WARN());
                ui.add_space(2.0);
                for (text, serious) in &alerts {
                    line(ui, text, if *serious { TEXT() } else { DIM() });
                }
                return;
            }
            title(ui, "Your music is backed up", 34.0, TEXT());
            ui.add_space(2.0);
            let second = s.second.set_up.then_some(s.second.last).flatten();
            if let Some(newest) = [s.last_ok, second].into_iter().flatten().max() {
                let when = lower_first(&when_text(newest));
                let copies = if s.last_ok.is_some() && second.is_some() { format!("Two copies, the newest from {when}") } else { format!("One copy, from {when}") };
                line(ui, &copies, DIM());
            }
            // (only gentle words are left here: the second backup getting old)
            for (text, _) in &alerts {
                line(ui, text, DIM());
            }
        });
    }

    /// For the Home page's card: the sentence above in a couple of words, then one line about it, each
    /// with its color. While a backup or a check runs, the line says how far it is.
    pub fn in_short(&self) -> [(String, Color32); 2] {
        let Some(s) = &self.status else { return [("Checking".to_string(), DIM()), (String::new(), DIM())] };
        if s.tool_missing {
            return [("Tool not found".to_string(), WARN()), ("The backup tool's folder was not found.".to_string(), DIM())];
        }
        if s.dest.is_empty() {
            return [("Not set up yet".to_string(), WARN()), (String::new(), DIM())];
        }
        let doing = self.job().map(|words| (words, ACCENT2()));
        if let Some((text, _)) = self.alerts().iter().find(|a| a.1) {
            // (its first sentence: the page has the rest)
            let first = text.split_once(". ").map(|(first, _)| format!("{first}.")).unwrap_or_else(|| text.clone());
            return [("Needs a look".to_string(), WARN()), doing.unwrap_or((first, TEXT()))];
        }
        let second = s.second.set_up.then_some(s.second.last).flatten();
        let about = match [s.last_ok, second].into_iter().flatten().max() {
            Some(newest) => format!("{}  ·  {}", when_text(newest), if s.last_ok.is_some() && second.is_some() { "two copies" } else { "one copy" }),
            None => String::new(),
        };
        [("Backed up".to_string(), TEXT()), doing.unwrap_or((about, DIM()))]
    }

    /// For the Home page's card: the main backup's recent runs, drawn as they are on this page.
    pub fn recent_dots(&self, ui: &mut Ui) {
        if let Some(s) = &self.status {
            dots(ui, &s.runs);
        }
    }

    // ------------------------------------------------------------------ the two cards

    /// False for a band with nothing in it (a second backup that was never set up has nothing to do yet).
    fn has(&self, which: Which, band: Band) -> bool {
        !(which == Which::Second && band == Band::Actions && self.status.as_ref().is_some_and(|s| !s.second.set_up))
    }

    fn band(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which, band: Band) {
        match band {
            Band::Drive => self.drive_band(ui, ctx, which),
            Band::Last => self.last_band(ui, ctx, which),
            Band::Actions => self.actions_band(ui, ctx, which),
        }
    }

    /// The thin line between two bands of a card. The one over the words that start things fills from
    /// the left, in blue, with how far a running backup or check is: progress without a bar of its own,
    /// so nothing on the page moves when something starts or ends.
    fn seam(&self, ui: &Ui, from: f32, to: f32, y: f32, which: Which, band: Band) {
        ui.painter().hline(from..=to, y, Stroke::new(1.0, theme::BORDER()));
        if let (Band::Actions, Some(done)) = (band, self.progress(which)) {
            ui.painter().hline(from..=from + (to - from) * done, y, Stroke::new(2.0, ACCENT2()));
        }
    }

    /// How far what is running for this backup is, 0 to 1. None while nothing runs for it, or while there
    /// is no share to show yet (starting, comparing, saving).
    fn progress(&self, which: Which) -> Option<f32> {
        let job = self.job.as_ref().filter(|j| j.which == which)?;
        let done = match job.phase {
            Phase::Copying if job.total_bytes > 0 => job.done_bytes as f32 / job.total_bytes as f32,
            Phase::Copying => job.done_files as f32 / job.total_files.max(1) as f32,
            Phase::Verifying | Phase::Checking if job.read_files.1 > 0 => {
                // by size when the script says the sizes (a few large files would make a count jump)
                let (done, all) = (parse_size(&job.read_sizes.0), parse_size(&job.read_sizes.1));
                if all > 0 { done as f32 / all as f32 } else { job.read_files.0 as f32 / job.read_files.1 as f32 }
            }
            _ => return None,
        };
        Some(done.clamp(0.0, 1.0))
    }

    fn cards(&mut self, ui: &mut Ui, ctx: &egui::Context, left: f32, wide: f32) {
        if wide < SIDE_BY_SIDE {
            // one over the other, each a card of its own
            for (i, which) in BOTH.into_iter().enumerate() {
                if i > 0 {
                    ui.add_space(GAP - ui.spacing().item_spacing.y);
                }
                panel(ui, left, wide, ("backup-card", i), |ui| {
                    for (n, band) in BANDS.into_iter().enumerate() {
                        if !self.has(which, band) {
                            continue;
                        }
                        if n > 0 {
                            ui.add_space(SEAM - ui.spacing().item_spacing.y);
                            let (at, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                            self.seam(ui, at.left(), at.right(), at.center().y, which, band);
                            ui.add_space(SEAM - ui.spacing().item_spacing.y);
                        }
                        self.band(ui, ctx, which, band);
                    }
                });
            }
            return;
        }
        // side by side: a band of the one and the same band of the other start at the same height, and the
        // next pair starts under the taller of the two
        let top = ui.cursor().top();
        let card = ((wide - GAP) / 2.0).floor();
        let lefts = [left, left + wide - card];
        let fills = [ui.painter().add(egui::Shape::Noop), ui.painter().add(egui::Shape::Noop)];
        let mut y = top + PAD;
        for (n, band) in BANDS.into_iter().enumerate() {
            if n > 0 {
                y += SEAM;
                for (i, which) in BOTH.into_iter().enumerate() {
                    if self.has(which, band) {
                        self.seam(ui, lefts[i] + PAD, lefts[i] + card - PAD, y, which, band);
                    }
                }
                y += SEAM;
            }
            let mut bottom = y;
            for (i, which) in BOTH.into_iter().enumerate() {
                let room = Rect::from_min_max(pos2(lefts[i] + PAD, y), pos2(lefts[i] + card - PAD, y + 100_000.0));
                let mut part = ui.new_child(UiBuilder::new().id_salt(("backup-band", i, n)).max_rect(room).layout(Layout::top_down(Align::Min)));
                self.band(&mut part, ctx, which, band);
                bottom = bottom.max(part.min_rect().bottom());
            }
            y = bottom;
        }
        let bottom = y + PAD;
        for (i, fill) in fills.into_iter().enumerate() {
            let rect = Rect::from_min_max(pos2(lefts[i], top), pos2(lefts[i] + card, bottom));
            ui.painter().set(fill, egui::epaint::RectShape::filled(rect, CornerRadius::same(12), theme::CARD()));
        }
        ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left, top), pos2(left + wide, bottom)));
    }

    // ------------------------------------------------------------------ band 1: the drive

    /// Which drive, whether it is there, and how full it is.
    fn drive_band(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which) {
        self.drive_words(ui, ctx, which);
        let Some(s) = &self.status else {
            head(ui, "Checking the drive", DIM(), None, "");
            gauge(ui, None, None);
            return;
        };
        // what takes room besides the music, said when the pointer is on the free space or the bar
        let room = self.room[which as usize].and_then(|(changed, deleted)| room_words(changed, deleted));
        match which {
            Which::Main => match &s.drive {
                Drive::Good { drive, label } => {
                    head(ui, if label.is_empty() { "The backup drive" } else { label.as_str() }, TEXT(), Some(GREEN()), drive);
                    space_line(ui, s.space, room.as_deref());
                    gauge(ui, s.space, room.as_deref());
                }
                Drive::Missing(drive) => {
                    head(ui, &format!("{drive} is not connected"), WARN(), Some(WARN()), "");
                    line(ui, "Nothing can be copied until the backup drive is back.", DIM());
                    gauge(ui, None, None);
                }
                Drive::Wrong { drive, found, want } => {
                    let found = if found.is_empty() { "a disk with no name".to_string() } else { format!("\"{found}\"") };
                    head(ui, &format!("The disk at {drive} is {found}, not \"{want}\""), RED(), Some(RED()), "");
                    line(ui, "Nothing will be copied to it.", DIM());
                    gauge(ui, None, None);
                }
                Drive::NoDest => {
                    head(ui, "No backup folder yet", WARN(), Some(WARN()), "");
                    line(ui, "Run Backup-Music.ps1 from the backup tool's folder once to choose one; after that this page runs it.", DIM());
                    gauge(ui, None, None);
                }
            },
            Which::Second => match (&s.second.folder, s.second.set_up) {
                (Some(folder), _) => {
                    let label = &s.second.label;
                    head(ui, if label.is_empty() { "The second backup drive" } else { label.as_str() }, TEXT(), Some(GREEN()), folder.get(..2).unwrap_or(""));
                    space_line(ui, s.second.space, room.as_deref());
                    gauge(ui, s.second.space, room.as_deref());
                }
                (None, true) => {
                    // the drive by its name, quiet and with an empty ring: it is away. What is known of
                    // its room is what the last backup to it said
                    let label = &s.second.label;
                    head(ui, if label.is_empty() { "The second backup drive" } else { label.as_str() }, DIM(), None, "");
                    let was = s.second.runs.first().and_then(|r| r.room_left.as_deref());
                    line(ui, &was.map(|room| format!("Not plugged in  ·  {room} was free after the last backup")).unwrap_or_else(|| "Not plugged in".to_string()), DIM());
                    gauge(ui, None, None);
                }
                (None, false) => {
                    head(ui, "No second backup yet", DIM(), None, "");
                    line(ui, "A second copy of the library on a drive you plug in by hand, such as a flash drive. Every file copied to it is read back and checked.", DIM());
                }
            },
        }
    }

    /// The band's heading, and at its right, quietly, what can be done to the drive itself: open the
    /// backup's folder on it, and for the second backup use another drive or let go of this one. They
    /// stand with the drive, not among the words that start a backup.
    fn drive_words(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which) {
        let idle = self.job.is_none();
        let test = places().test;
        let (folder, set_up) = match (&self.status, which) {
            (Some(s), Which::Main) => (s.dest_exists.then(|| s.dest.clone()), true),
            (Some(s), Which::Second) => (s.second.folder.clone(), s.second.set_up),
            (None, _) => (None, false),
        };
        // letting go of the drive: never while something is being copied to it or read from it
        let ejectable = which == Which::Second && folder.as_ref().is_some_and(|f| can_eject(f, test));
        let (mut open, mut other, mut let_go) = (false, false, false);
        caption_row(ui, if which == Which::Main { "MAIN BACKUP" } else { "SECOND BACKUP" }, |ui| {
            // (from the right edge inward)
            if folder.is_some() {
                open = quiet(ui, "Open folder", 12.5, true).clicked();
            }
            if which == Which::Second && set_up && !self.choosing {
                other = quiet(ui, "Use another drive", 12.5, idle).clicked();
            }
            if ejectable {
                let_go = quiet(ui, if self.ejecting { "Ejecting" } else { "Eject" }, 12.5, idle && !self.ejecting).clicked();
            }
        });
        if open {
            if let Some(folder) = folder.clone() {
                std::thread::spawn(move || {
                    let _ = open::that(folder);
                });
            }
        }
        if other {
            self.choosing = true;
            self.chosen = None;
        }
        if let_go {
            if let (Some(shell), Some(folder)) = (self.status.as_ref().and_then(|s| s.shell.clone()), folder) {
                self.ejecting = true;
                self.eject_note = None;
                let (tx, ctx) = (self.tx.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(Msg::Ejected(eject(&shell, &folder, test)));
                    ctx.request_repaint();
                });
            }
        }
    }

    // ------------------------------------------------------------------ band 2: the last backup

    /// When this backup last ran and how it ended, how the recent ones went (a dot each), when it runs,
    /// and what the last check of the whole backup found.
    fn last_band(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which) {
        let Some(s) = &self.status else {
            caption(ui, "LAST BACKUP");
            line(ui, "Reading the backup log", DIM());
            return;
        };
        if which == Which::Second && !s.second.set_up {
            // nothing has run yet: this is where the drive for it is chosen
            let sec = s.second.clone();
            let can = self.job.is_none() && self.second_blocked().is_none();
            caption(ui, "CHOOSE A DRIVE");
            self.choose_drive(ui, ctx, &sec, can);
            return;
        }
        let log_exists = if which == Which::Main { s.log_exists } else { s.second.log_exists };
        caption_row(ui, "LAST BACKUP", |ui| {
            if log_exists && quiet(ui, "Open log", 12.5, true).clicked() {
                let log = places().logs.join(which.log_name());
                std::thread::spawn(move || {
                    let _ = open::that(log);
                });
            }
        });
        let idle = self.job.is_none();
        let (runs, last_ok, can) = match which {
            Which::Main => (&s.runs, s.last_ok, idle && self.blocked().is_none()),
            Which::Second => (&s.second.runs, s.second.last, idle && self.second_blocked().is_none() && s.second.folder.is_some()),
        };
        let mirror = last_backup(ui, runs.first(), last_ok, can);
        if !runs.is_empty() {
            ui.add_space(8.0);
            dots(ui, runs);
        }
        // when it runs: the nightly task's schedule, or by hand
        let (words, color, more, more_color) = match which {
            Which::Main => schedule_words(&s.task),
            Which::Second => ("By hand".to_string(), TEXT(), s.second.last.map(|t| format!("last backed up {}", ago(t))).unwrap_or_else(|| "never backed up".to_string()), DIM()),
        };
        two_tone(ui, &words, color, &more, more_color, 13.5, 10.0);
        if let Some(run) = self.last_check(which) {
            check_line(ui, run);
        }
        if mirror {
            self.start(which, Kind::Mirror, ctx);
        }
    }

    // ------------------------------------------------------------------ band 3: what can be done

    /// The words that start things. While something runs for this backup its progress stands in their
    /// place; a question (start a whole check? which other drive?) takes the whole band until answered.
    fn actions_band(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which) {
        let Some(s) = &self.status else { return };
        let (plugged, set_up, folder) = match which {
            Which::Main => (matches!(s.drive, Drive::Good { .. }), true, s.dest_exists.then(|| s.dest.clone())),
            Which::Second => (s.second.folder.is_some(), s.second.set_up, s.second.folder.clone()),
        };
        if !set_up {
            return;
        }
        let choosing = (which == Which::Second && self.choosing).then(|| s.second.clone());
        let idle = self.job.is_none();
        let blocked = match which {
            Which::Main => self.blocked(),
            Which::Second => self.second_blocked(),
        };
        let can = idle && blocked.is_none() && (which == Which::Main || plugged);

        // a check of the whole backup was asked for: what it is and how long it takes, then one more click
        if self.asking_check == Some(which) && idle {
            ui.add(Label::new(RichText::new("This reads every file in the library and its copy in the backup and compares them. Nothing is copied or changed. It takes about as long as copying the whole library, and it can be stopped at any time.").size(13.5).color(TEXT())).wrap());
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 24.0;
                if theme::main_action_sized(ui, "Start the check", 16.0, can).clicked() {
                    self.asking_check = None;
                    self.start(which, Kind::CheckAll, ctx);
                }
                if theme::link(ui, "Not now").clicked() {
                    self.asking_check = None;
                }
            });
            return;
        }
        // another drive is being chosen for the second backup
        if let Some(sec) = choosing {
            self.choose_drive(ui, ctx, &sec, idle && blocked.is_none());
            ui.add_space(2.0);
            if self.chosen.is_none() && theme::link_sized(ui, "Keep this drive", 12.5, true).clicked() {
                self.choosing = false;
            }
            return;
        }

        // a line or two under the words (so the words of both cards stand at the same height): how what
        // was started here ended, or why nothing can start
        let mut notes: Vec<(String, Color32)> = vec![];
        let here = self.job.as_ref().is_some_and(|j| j.which == which);
        // (how a check of the whole backup or a repair of its copies ended is said where its result is)
        let done = self.finished.as_ref().filter(|f| !here && f.0 == which && f.1 != Kind::CheckAll && !matches!(f.1, Kind::Recopy(_)));
        if let Some((_, _, run)) = done {
            let (text, tone) = run.headline();
            notes.push((if tone == Tone::Good { format!("Backup finished. {text}.") } else { text }, tone_color(tone, GREEN())));
        }
        let test = places().test;
        let ejectable = which == Which::Second && folder.as_ref().is_some_and(|f| can_eject(f, test));
        if which == Which::Second {
            // how asking Windows to eject the drive ended: only "can be pulled out" when it was seen to be gone
            match &self.eject_note {
                Some((words, went)) => notes.push((words.clone(), if *went { GREEN() } else { WARN() })),
                // after a backup to it: a word before the drive is pulled out
                None if idle && ejectable && done.is_some_and(|f| f.1.copies()) => notes.push(("Eject the drive before you pull it out.".to_string(), DIM())),
                None => {}
            }
        }
        match &blocked {
            Some((why, color)) if idle => notes.push((why.clone(), *color)),
            // (said whether or not something else is running: it is about the drive, and the card keeps its height)
            None if which == Which::Second && !plugged => notes.push((format!("Plug in {} to back up to it.", self.name_of(which)), DIM())),
            _ => {}
        }
        if here {
            self.activity(ui, which);
        } else {
            let (mut backup, mut preview, mut check) = (false, false, false);
            ui.horizontal_wrapped(|ui| {
                ui.set_min_height(ROW);
                // one loud word, the one this card is for; the two that only look are quiet beside it
                ui.spacing_mut().item_spacing.x = 26.0;
                backup = theme::main_action_sized(ui, "Back up now", 17.0, can).clicked();
                preview = quiet(ui, "Preview", 14.5, can).clicked();
                check = quiet(ui, "Check the whole backup", 14.5, can && folder.is_some()).clicked();
            });
            if backup {
                self.start(which, Kind::Backup, ctx);
            }
            if preview {
                self.start(which, Kind::Preview, ctx);
            }
            if check {
                self.asking_check = Some(which);
            }
        }

        // (where the backup is, its log and which drive it is on stand beside the headings of the bands
        // they belong to: see `drive_words` and `last_band`)
        if !notes.is_empty() {
            ui.add_space(2.0);
        }
        for (text, color) in &notes {
            if text == crate::importer::tools::NO_PWSH {
                // with the word that installs it
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    crate::helpers::pwsh_line(ui, 12.5, *color, false);
                });
                continue;
            }
            ui.add(Label::new(RichText::new(text).size(12.5).color(*color)).wrap());
        }
    }

    /// What is happening now for this backup: a spinner and the words, in the place of the words that start things.
    fn activity(&mut self, ui: &mut Ui, which: Which) {
        let Some(job) = self.job.as_ref().filter(|j| j.which == which) else { return };
        // files read back so far: "1,200 of 5,000 files, 1.2 GB of 40 GB, about 12m 3s left"
        let reading = |what: &str| {
            let (done, all) = job.read_files;
            if all == 0 {
                return what.to_string();
            }
            let mut t = format!("{what} {} of {}", commas(done.min(all)), plural_commas(all, "file"));
            if !job.read_sizes.1.is_empty() {
                t.push_str(&format!(", {} of {}", job.read_sizes.0, job.read_sizes.1));
            }
            if !job.read_left.is_empty() {
                t.push_str(&format!(", about {} left", job.read_left));
            }
            t
        };
        let text = match (&job.kind, job.phase) {
            (_, _) if job.stopped => "Stopping".to_string(),
            (Kind::Recopy(_), _) => "Making the damaged copies again from the library".to_string(),
            (_, Phase::Starting) => "Starting".to_string(),
            (Kind::CheckAll, Phase::Checking) => reading("Checking"),
            (Kind::CheckAll, _) => "Reading the library and the backup".to_string(),
            (Kind::Preview, _) | (_, Phase::Comparing) => "Comparing your library with the backup".to_string(),
            (_, Phase::Copying) => {
                let mut t = format!("Copying {} of {}", commas(job.done_files), plural_commas(job.total_files, "file"));
                if job.total_bytes > 0 {
                    t.push_str(&format!(", {} of {}", fmt_size(job.done_bytes.min(job.total_bytes)), job.total_text));
                }
                t
            }
            (_, Phase::Verifying) | (_, Phase::Checking) => reading("Checking the copies against the library:"),
            (_, Phase::Saving) => "Saving MusicBee, the music tool's data and the code".to_string(),
            (_, Phase::Finishing) => "Finishing".to_string(),
        };
        let can_stop = job.kind == Kind::CheckAll && !job.stopped;
        let mut stop = false;
        // (how far it is shows on the line over this band: see `seam`)
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(ROW);
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.add(egui::Spinner::new().size(13.0).color(ACCENT2()));
            ui.label(RichText::new(text).color(TEXT()));
            stop = can_stop && theme::link(ui, "Stop").clicked();
        });
        if stop {
            self.stop_check();
        }
    }

    // ------------------------------------------------------------------ under the cards

    /// What a preview found, until it is hidden or a backup makes it out of date.
    fn preview_panel(&mut self, ui: &mut Ui, which: Which, left: f32, wide: f32) {
        let name = self.name_of(which).to_uppercase();
        let Some((_, run, at)) = self.preview.as_ref().filter(|p| p.0 == which) else { return };
        let (made, mut hide) = (*at, false);
        ui.add_space(GAP - ui.spacing().item_spacing.y);
        let top = ui.cursor().top();
        panel(ui, left, wide, ("backup-preview", which as usize), |ui| {
            ui.horizontal(|ui| {
                caption(ui, &format!("PREVIEW FOR {name}  ·  {}", at.format("%-I:%M %p")));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| hide = theme::link_sized(ui, "Hide", 12.5, true).clicked());
            });
            ui.add_space(4.0);
            preview_card(ui, run, &self.lib);
        });
        // a preview that has just arrived is scrolled into view once: in a short window it is under the cards
        let seen = egui::Id::unique("backup-preview-seen");
        if ui.ctx().data(|d| d.get_temp::<NaiveDateTime>(seen)) != Some(made) {
            ui.ctx().data_mut(|d| d.insert_temp(seen, made));
            ui.scroll_to_rect(Rect::from_min_max(pos2(left, top), pos2(left + wide, ui.cursor().top())), None);
        }
        if hide {
            self.preview = None;
        }
    }

    /// The damaged copies the last check of the whole backup found, and what can be done about them.
    fn damaged_panel(&mut self, ui: &mut Ui, ctx: &egui::Context, which: Which, left: f32, wide: f32) {
        let Some(run) = self.last_check(which).filter(|r| r.check.as_ref().is_some_and(|c| c.damaged_count > 0)).cloned() else { return };
        let name = self.name_of(which).to_uppercase();
        ui.add_space(GAP - ui.spacing().item_spacing.y);
        panel(ui, left, wide, ("backup-damaged", which as usize), |ui| {
            caption(ui, &format!("DAMAGED COPIES IN {name}"));
            self.damaged(ui, ctx, which, &run);
        });
    }

    /// What the backups copied lately, as one row of covers: the albums of the recent runs of both
    /// backups, the newest run's first, each album once. Pointing at one says which backup took it and
    /// when; clicking it opens the album in the Library.
    fn copied(&self, ui: &mut Ui, left: f32, wide: f32) {
        // more than a row ever holds, so the row is full whenever there is that much to show
        const MOST: usize = 24;
        let Some(s) = &self.status else { return };
        let mut runs: Vec<(Which, &Run)> = s.runs.iter().map(|r| (Which::Main, r)).chain(s.second.runs.iter().map(|r| (Which::Second, r))).filter(|(_, r)| !(r.new_albums.is_empty() && r.changed_albums.is_empty())).collect();
        if runs.is_empty() {
            return;
        }
        runs.sort_by(|a, b| b.1.when.cmp(&a.1.when));
        let mut seen = std::collections::HashSet::new();
        let mut albums: Vec<(&Album, bool)> = vec![];
        let mut from: Vec<(Which, Option<NaiveDateTime>)> = vec![];
        'runs: for (which, run) in &runs {
            for (album, new) in run.new_albums.iter().map(|a| (a, true)).chain(run.changed_albums.iter().map(|a| (a, false))) {
                if albums.len() == MOST {
                    break 'runs;
                }
                if seen.insert(album.name.as_str()) {
                    albums.push((album, new));
                    from.push((*which, run.when));
                }
            }
        }
        let tip = |i: usize| {
            let (album, new) = albums[i];
            let (which, when) = from[i];
            let when = when.map(|t| format!(" {}", lower_first(&when_text(t)))).unwrap_or_default();
            let what = if new { "New in the library" } else { "Changed in the library" };
            let detail = if album.detail.is_empty() { String::new() } else { format!("\n{}", album.detail) };
            format!("{what}\nCopied to {}{when}{detail}", self.name_of(which))
        };
        ui.add_space(GAP - ui.spacing().item_spacing.y);
        panel(ui, left, wide, "backup-copied", |ui| {
            ui.horizontal(|ui| {
                caption(ui, "RECENTLY COPIED");
                // what the blue dot on a cover means, when one is there to explain
                if albums.iter().any(|a| a.1) {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new("new in the library").size(12.0).color(FAINT()));
                        let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 3.5, ACCENT2());
                    });
                }
            });
            album_tiles(ui, &self.lib, &albums, 1, Some(&tip));
        });
    }
}

// ---------------------------------------------------------------------- the pieces

/// A rounded box as wide as the page, as tall as what `add` draws in it.
fn panel<R>(ui: &mut Ui, left: f32, wide: f32, salt: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = ui.painter().add(egui::Shape::Noop);
    let top = ui.cursor().top();
    let room = Rect::from_min_max(pos2(left + PAD, top + PAD), pos2(left + wide - PAD, top + 100_000.0));
    let mut inner = ui.new_child(UiBuilder::new().id_salt(salt).max_rect(room).layout(Layout::top_down(Align::Min)));
    let out = add(&mut inner);
    let rect = Rect::from_min_max(pos2(left, top), pos2(left + wide, inner.min_rect().bottom().max(room.top()) + PAD));
    ui.painter().set(fill, egui::epaint::RectShape::filled(rect, CornerRadius::same(12), theme::CARD()));
    ui.advance_cursor_after_rect(rect);
    out
}

/// A small quiet heading over a part of the page, its letters set a little apart.
fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).extra_letter_spacing(1.4).color(DIM()));
}

/// A heading with a few quiet words at the right end of its line. `words` lays them out from the right
/// edge inward.
fn caption_row(ui: &mut Ui, text: &str, words: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        caption(ui, text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            words(ui);
        });
    });
}

/// A word you can click that does not ask for attention: gray, and bright and underlined under the
/// pointer (the way the page names and filters are). For what is done now and then (open a folder, look
/// before backing up), so that the one colored word in a card is the one it is for.
fn quiet(ui: &mut Ui, text: &str, size: f32, enabled: bool) -> egui::Response {
    let font = FontId::proportional(size);
    let words = ui.painter().layout_no_wrap(text.to_string(), font.clone(), DIM());
    let (rect, r) = ui.allocate_exact_size(words.size(), if enabled { Sense::click() } else { Sense::hover() });
    crate::script::seen(text, rect);
    let over = enabled && r.hovered();
    let color = if !enabled { FAINT() } else if over { TEXT() } else { DIM() };
    ui.painter().text(rect.min, egui::Align2::LEFT_TOP, text, font, color);
    if over {
        ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, DIM()));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// Words in the light cut of the font, for what a part of the page is about (the state, a time).
pub(super) fn title(ui: &mut Ui, text: &str, size: f32, color: Color32) -> egui::Response {
    ui.add(Label::new(RichText::new(text).font(FontId::new(size, crate::display_font())).color(color)).wrap())
}

/// One line in two colors: the fact, then a quieter (or louder) word about it.
fn two_tone(ui: &mut Ui, first: &str, first_color: Color32, second: &str, second_color: Color32, size: f32, gap: f32) -> egui::Response {
    let font = FontId::proportional(size);
    let mut job = LayoutJob::default();
    job.append(first, 0.0, TextFormat { font_id: font.clone(), color: first_color, ..Default::default() });
    if !second.is_empty() {
        job.append(second, if first.is_empty() { 0.0 } else { gap }, TextFormat { font_id: font, color: second_color, ..Default::default() });
    }
    ui.add(Label::new(job).wrap())
}

/// A drive's name, large, with its letter small beside it. The dot hangs in the card's margin so the name
/// starts where every other line does: filled (in `dot`'s color) when there is something to say about the
/// drive, an empty ring when it is away. A sentence in the name's place ("D: is not connected") may take
/// two lines.
fn head(ui: &mut Ui, text: &str, color: Color32, dot: Option<Color32>, letter: &str) {
    let avail = ui.available_width();
    let small = ui.painter().layout_no_wrap(letter.to_string(), FontId::proportional(14.0), DIM());
    let beside = if letter.is_empty() { 0.0 } else { small.size().x + 10.0 };
    let mut job = LayoutJob::simple(text.to_string(), FontId::new(28.0, crate::display_font()), color, (avail - beside).max(60.0));
    job.wrap.max_rows = 2;
    let words = ui.painter().layout_job(job);
    let (size, row) = (words.size(), words.size().y / words.rows.len().max(1) as f32);
    let (rect, _) = ui.allocate_exact_size(vec2(avail, size.y), Sense::hover());
    let at = pos2(rect.left() - 13.0, rect.top() + row / 2.0 + 2.0);
    match dot {
        Some(c) => ui.painter().circle_filled(at, 4.5, c),
        None => ui.painter().circle_stroke(at, 4.0, Stroke::new(1.5, FAINT())),
    };
    ui.painter().galley(rect.min, words, color);
    if !letter.is_empty() {
        // (set low, so it sits on the name's line of writing rather than in the middle of its height)
        ui.painter().galley(pos2(rect.left() + size.x + 10.0, rect.top() + 0.78 * (row - small.size().y)), small, DIM());
    }
}

/// How much is free on a drive, bright, and of how much. `more` is said when the pointer is on it.
fn space_line(ui: &mut Ui, space: Option<(u64, u64)>, more: Option<&str>) {
    let r = match space {
        Some((free, total)) => two_tone(ui, &format!("{} free", fmt_size(free)), TEXT(), &format!("of {}", fmt_size(total)), DIM(), 14.5, 5.0).on_hover_cursor(egui::CursorIcon::Default),
        None => ui.label(RichText::new("Connected").color(DIM())),
    };
    if let Some(more) = more {
        r.on_hover_text(more);
    }
}

/// One thin bar for how full a drive is. With nothing to read (the drive is away) the bar is a dashed
/// line: no reading, rather than an empty drive.
fn gauge(ui: &mut Ui, space: Option<(u64, u64)>, more: Option<&str>) {
    let (hit, r) = ui.allocate_exact_size(vec2(ui.available_width(), 13.0), Sense::hover());
    let bar = Rect::from_center_size(hit.center(), vec2(hit.width(), 5.0));
    match space.filter(|(_, total)| *total > 0) {
        Some((free, total)) => {
            let used = 1.0 - (free as f32 / total as f32).clamp(0.0, 1.0);
            ui.painter().rect_filled(bar, 3, theme::BORDER());
            let mut filled = bar;
            filled.set_width((bar.width() * used).max(5.0));
            // nearly full is worth a glance: amber from 90%, red from 97%
            let color = if used >= 0.97 { RED() } else if used >= 0.9 { WARN() } else { ACCENT2().gamma_multiply(0.75) };
            ui.painter().rect_filled(filled, 3, color);
        }
        None => {
            let mut x = bar.left();
            while x < bar.right() {
                ui.painter().rect_filled(Rect::from_min_max(pos2(x, bar.top() + 1.0), pos2((x + 7.0).min(bar.right()), bar.bottom() - 1.0)), 1, theme::BORDER());
                x += 13.0;
            }
        }
    }
    if let Some(more) = more {
        r.on_hover_text(more);
    }
}

/// When a backup last ran and how it ended, anything that went wrong, and one quiet line for what else
/// it did (all of it under the pointer). True when the words that move a great many removed files to
/// `_deleted` were clicked (`can`: a backup may start now).
fn last_backup(ui: &mut Ui, run: Option<&Run>, last_ok: Option<NaiveDateTime>, can: bool) -> bool {
    let Some(run) = run else {
        match last_ok {
            Some(t) => {
                title(ui, &when_text(t), 20.0, TEXT());
            }
            None => line(ui, "No backup has run yet.", DIM()),
        }
        return false;
    };
    // the log stamps a run when it starts; config.json has the second it finished
    let when = match (run.when, last_ok) {
        (Some(w), Some(ok)) if run.error.is_none() && ok >= w && (ok - w).num_hours() < 12 => Some(ok),
        (w, _) => w,
    };
    title(ui, &when.map(when_text).unwrap_or_else(|| "Time not recorded".into()), 20.0, TEXT());
    let (text, tone) = run.headline();
    ui.add(Label::new(RichText::new(text).color(tone_color(tone, GREEN()))).wrap());
    if run.error.is_some() {
        if let Some(ok) = last_ok {
            line(ui, &format!("The last backup that finished was {}.", lower_first(&when_text(ok))), DIM());
        }
    }
    for trouble in run_troubles(run) {
        line(ui, &trouble, RED());
    }
    // copies that are not the same as the library's files: which, and what to do
    if run.bad_count > 0 {
        ui.add_space(4.0);
        line(ui, &format!("{} not {}:", if run.bad_count == 1 { "1 file is".to_string() } else { format!("{} files are", commas(run.bad_count)) }, if run.bad_count == 1 { "a good copy" } else { "good copies" }), RED());
        file_list(ui, &run.bad_copies, run.bad_count, RED());
        line(ui, "Run the backup again. If the same files fail twice, do not trust this drive.", TEXT());
    }
    let brief = run_brief(run);
    if !brief.is_empty() {
        ui.add(Label::new(RichText::new(brief).size(12.5).color(FAINT())).truncate()).on_hover_text(run_notes(run).join("\n"));
    }
    // the safety catch: a great many files left the library at once, so the run moved none of them
    let mut mirror = false;
    if let Some((n, total)) = run.too_many {
        ui.add_space(4.0);
        line(ui, &format!("{} of the backup's {} are no longer in the library. That is a lot at once, so they were left where they are.", commas(n), plural_commas(total, "file")), WARN());
        mirror = theme::link_enabled(ui, "Move them to _deleted", can).clicked();
        ui.add(Label::new(RichText::new("Runs the backup once more and moves them to _deleted in the backup. Nothing is deleted.").size(12.0).color(FAINT())).wrap());
    }
    mirror
}

/// What a backup did besides copying, short enough for one quiet line: "All 28,499 copies checked ·
/// MusicBee, tool data and code saved". (`run_notes` has it in full, with the numbers.)
fn run_brief(run: &Run) -> String {
    let mut parts = vec![];
    let albums = run.albums_phrase();
    if !albums.is_empty() {
        parts.push(albums);
    }
    if let (Some((n, _)), 0) = (&run.verified, run.bad_count) {
        parts.push(if *n == 1 { "The copy was checked".to_string() } else { format!("All {} copies checked", commas(*n)) });
    }
    let failed = |p: Part| run.part_problems.iter().any(|(part, _)| *part == p);
    let saved: Vec<&str> = [(Part::MusicBee, run.musicbee.is_some(), "MusicBee"), (Part::Tool, run.tool.is_some(), "tool data"), (Part::Code, run.code.is_some(), "code")].into_iter().filter(|(part, there, _)| *there && !failed(*part)).map(|p| p.2).collect();
    let list = match saved.as_slice() {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c, ..] => format!("{a}, {b} and {c}"),
    };
    if !list.is_empty() {
        let mut c = list.chars();
        parts.push(format!("{}{} saved", c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default(), c.as_str()));
    }
    if let Some(n) = run.kept_old {
        parts.push(if n == 1 { "1 old copy kept".to_string() } else { format!("{} old copies kept", commas(n)) });
    }
    if let Some(n) = run.moved {
        parts.push(format!("{} moved to _deleted", plural(n, "item")));
    }
    parts.join("\u{a0}\u{a0}·  ")
}

/// The recent backups as a row of dots, oldest on the left: a dot's size is how much was copied, a ring
/// is a run with nothing to copy, red is a run with a problem. Pointing at one says when and what. Under
/// them, the dates the row runs between. `runs` is newest first.
fn dots(ui: &mut Ui, runs: &[Run]) {
    const SLOT: f32 = 26.0;
    let fit = ((ui.available_width() / SLOT).floor() as usize).max(1);
    let shown: Vec<&Run> = runs.iter().take(fit).rev().collect();
    let Some(first) = shown.first() else { return };
    let bytes = |r: &Run| r.copied.as_ref().map(|c| parse_size(&c.1)).unwrap_or(0);
    let most = shown.iter().map(|r| bytes(r)).max().unwrap_or(0).max(1) as f32;
    let radius = |r: &Run| if bytes(r) > 0 || r.headline().1 == Tone::Bad { 3.5 + 8.0 * (bytes(r) as f32 / most).sqrt() } else { 3.5 };
    // the first dot's edge, whatever its size, stands where the lines of text start
    let lead = SLOT / 2.0 - radius(first);
    let (row, _) = ui.allocate_exact_size(vec2(shown.len() as f32 * SLOT - lead, 25.0), Sense::hover());
    let center = |i: usize| pos2(row.left() - lead + (i as f32 + 0.5) * SLOT, row.center().y);
    for (i, run) in shown.iter().enumerate() {
        let (text, tone) = run.headline();
        let color = tone_color(tone, GREEN());
        if bytes(run) > 0 || tone == Tone::Bad {
            ui.painter().circle_filled(center(i), radius(run), color);
        } else {
            ui.painter().circle_stroke(center(i), 3.5, Stroke::new(1.5, color.gamma_multiply(0.7)));
        }
        let mut about = run.when.map(when_text).unwrap_or_default();
        about.push_str(&format!("\n{text}"));
        let albums = run.albums_phrase();
        if !albums.is_empty() {
            about.push_str(&format!("\n{albums}"));
        }
        ui.interact(Rect::from_center_size(center(i), vec2(SLOT, 25.0)), ui.scope_id().with(("backup-dot", i)), Sense::hover()).on_hover_text(about);
    }
    let day = |r: &Run| r.when.map(|t| t.format("%b %-d").to_string()).unwrap_or_default();
    let (dates, _) = ui.allocate_exact_size(vec2(row.width(), 13.0), Sense::hover());
    ui.painter().text(dates.left_top(), egui::Align2::LEFT_TOP, day(first), FontId::proportional(11.0), FAINT());
    if shown.len() > 3 {
        ui.painter().text(pos2(center(shown.len() - 1).x, dates.top()), egui::Align2::CENTER_TOP, day(shown[shown.len() - 1]), FontId::proportional(11.0), FAINT());
    }
    ui.add_space(2.0);
}

/// The nightly backup in a few words, and what there is to add (when the next one is, or what is wrong).
fn schedule_words(task: &Task) -> (String, Color32, String, Color32) {
    match task {
        Task::Unknown => ("Task Scheduler could not be read".to_string(), WARN(), String::new(), DIM()),
        Task::Missing => ("No nightly backup is set up".to_string(), WARN(), String::new(), DIM()),
        Task::Found { enabled: false, .. } => ("The nightly backup is turned off in Task Scheduler".to_string(), WARN(), String::new(), DIM()),
        Task::Found { running: true, .. } | Task::Found { result: Some(267009), .. } => ("The nightly backup is running now".to_string(), ACCENT2(), String::new(), DIM()),
        Task::Found { schedule, next, result, .. } => {
            let schedule = if schedule.is_empty() { "Nightly".to_string() } else { schedule.clone() };
            match result {
                Some(0) | Some(267011) | None => (schedule, TEXT(), if next.is_empty() { String::new() } else { format!("next {}", lower_first(next)) }, DIM()),
                Some(c) => (schedule, TEXT(), format!("the last one ended with a problem (code {c})"), RED()),
            }
        }
    }
}

/// The last check of the whole backup in one line: when, and what it found. (The damaged copies
/// themselves, and what can be done about them, are in a panel of their own under the cards.)
fn check_line(ui: &mut Ui, run: &Run) {
    let when = run.when.map(|t| format!(" {}", lower_first(&when_text(t)))).unwrap_or_default();
    let (found, tone) = match &run.check {
        Some(c) if c.finished && c.files > 0 && c.damaged_count == 0 => (format!("all {} copies good", commas(c.files)), Tone::Good),
        Some(c) if c.finished && c.damaged_count > 0 => (if c.damaged_count == 1 { "1 damaged copy".to_string() } else { format!("{} damaged copies", commas(c.damaged_count)) }, Tone::Bad),
        // stopped, found no files, or never got as far as checking: the run says it itself
        _ => run.headline(),
    };
    two_tone(ui, &format!("Whole backup checked{when}"), DIM(), &found, tone_color(tone, GREEN()), 13.5, 10.0);
    if let Some(c) = run.check.as_ref().filter(|c| c.finished && c.waiting > 0) {
        let files = if c.waiting == 1 { "1 file is".to_string() } else { format!("{} files are", commas(c.waiting)) };
        ui.add(Label::new(RichText::new(format!("{files} new or changed since the last backup; the next backup copies {}.", if c.waiting == 1 { "it" } else { "them" })).size(12.5).color(FAINT())).wrap());
    }
}
