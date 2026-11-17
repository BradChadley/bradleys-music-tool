//! Test runs only: a list of clicks, scrolls and screenshots carried out by the app itself, so a whole flow
//! can be tried through the real window without a hand on the mouse. Never active unless `--script` is given.
//!
//!   bradleys-music-tool.exe --script "wait:2; click:Quick check; wait:4; shot:C:\tmp\a.png; quit" --script-log C:\tmp\log.txt
//!
//! Steps, separated by ";":
//!   wait:SECONDS            let the app run
//!   click:WORDS             click the control that shows exactly these words (the plain-word controls, tabs
//!                           and anything else that calls `seen`); "click:WORDS#2" is the second one on screen,
//!                           "click:~START" the first whose words begin so (for words that count up)
//!   click:X,Y               click at a point (in the window's points, as in a screenshot at 100%)
//!   scroll:X,Y,DY           turn the wheel with the pointer at a point (negative = down the page)
//!   middle:X,Y              click the middle mouse button to toggle autoscroll
//!   move:X,Y                move the pointer without clicking
//!   key:NAME                press a key (Escape, ArrowDown, ArrowUp, Enter); "key:Ctrl+2" holds Ctrl with it
//!   type:TEXT               type these letters into the box that has the keyboard (no ";" in them)
//!   shot:FILE.png           save a picture of the window
//!   quit                    close the app
//! A click on words waits up to eight seconds for them to appear, and the log says what happened to each step.
use egui::{pos2, Event, Pos2, Rect};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// the words on screen and where: (this frame so far, the last whole frame)
static WORDS: Mutex<(Vec<(String, Rect)>, Vec<(String, Rect)>)> = Mutex::new((Vec::new(), Vec::new()));

/// Called by a control when it draws: notes where its words are, so a script can click them. Costs nothing
/// outside a test run.
pub fn seen(words: &str, rect: Rect) {
    if ACTIVE.load(Ordering::Relaxed) {
        WORDS.lock().unwrap_or_else(|e| e.into_inner()).0.push((words.to_string(), rect));
    }
}

/// True in a scripted test run: a control whose name has to be put together first (`row:<folder>`) asks
/// this before doing so, so that the real app never does.
pub fn on() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

enum Step {
    Wait(f64),
    /// words to find, which of them, and whether to click (or only point at them)
    ClickWords(String, usize, bool),
    ClickAt(Pos2),
    MiddleAt(Pos2),
    MoveAt(Pos2),
    Scroll(Pos2, f32),
    Key(egui::Key, bool),
    Type(String),
    Shot(String),
    Quit,
}

pub struct Script {
    steps: VecDeque<Step>,
    /// events to hand to the window on the next frames, one group a frame
    queue: VecDeque<Vec<Event>>,
    /// the time before which nothing more happens
    hold_until: f64,
    /// when the step at the front was first looked at (for giving up on words that never show)
    since: Option<f64>,
    shot_busy: Arc<AtomicBool>,
    log: Option<std::path::PathBuf>,
}

impl Script {
    /// The script from the command line, if there is one.
    pub fn from_args() -> Option<Script> {
        let args: Vec<String> = std::env::args().collect();
        let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned());
        let text = after("--script")?;
        let mut steps = VecDeque::new();
        for part in text.split(';').map(|p| p.trim()).filter(|p| !p.is_empty()) {
            let (verb, rest) = part.split_once(':').unwrap_or((part, ""));
            let nums: Vec<f32> = rest.split(',').filter_map(|n| n.trim().parse().ok()).collect();
            steps.push_back(match verb {
                "wait" => Step::Wait(rest.trim().parse().unwrap_or(1.0)),
                "middle" if nums.len() == 2 => Step::MiddleAt(pos2(nums[0], nums[1])),
                "move" if nums.len() == 2 => Step::MoveAt(pos2(nums[0], nums[1])),
                "click" if nums.len() == 2 && rest.split(',').count() == 2 => Step::ClickAt(pos2(nums[0], nums[1])),
                "click" => match rest.rsplit_once('#') {
                    Some((words, n)) if n.parse::<usize>().is_ok() => Step::ClickWords(words.to_string(), n.parse().unwrap_or(1), true),
                    _ => Step::ClickWords(rest.to_string(), 1, true),
                },
                // the pointer is moved onto the words and left there, with no click (for what shows under it)
                "hover" => Step::ClickWords(rest.to_string(), 1, false),
                "scroll" if nums.len() == 3 => Step::Scroll(pos2(nums[0], nums[1]), nums[2]),
                "key" => {
                    let name = rest.trim();
                    let ctrl = name.strip_prefix("Ctrl+");
                    match egui::Key::from_name(ctrl.unwrap_or(name)) {
                        Some(k) => Step::Key(k, ctrl.is_some()),
                        None => continue,
                    }
                }
                "type" => Step::Type(rest.to_string()),
                "shot" => Step::Shot(rest.to_string()),
                "quit" => Step::Quit,
                _ => continue,
            });
        }
        ACTIVE.store(true, Ordering::Relaxed);
        Some(Script { steps, queue: VecDeque::new(), hold_until: 0.0, since: None, shot_busy: Arc::new(AtomicBool::new(false)), log: after("--script-log").map(Into::into) })
    }

    fn say(&self, line: String) {
        use std::io::Write;
        if let Some(mut f) = self.log.as_ref().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn click(&mut self, at: Pos2) {
        let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        self.queue.extend([vec![Event::PointerMoved(at)], vec![button(true)], vec![button(false)], vec![]]);
    }

    /// Once a frame, before the page is drawn: starts the next step when its turn has come.
    pub fn tick(&mut self, ctx: &egui::Context) {
        // the words drawn last frame are the ones a click can aim at
        {
            let mut w = WORDS.lock().unwrap_or_else(|e| e.into_inner());
            w.1 = std::mem::take(&mut w.0);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(40));
        let now = ctx.input(|i| i.time);
        if !self.queue.is_empty() || now < self.hold_until || self.shot_busy.load(Ordering::Relaxed) {
            return;
        }
        let Some(step) = self.steps.pop_front() else { return };
        let since = *self.since.get_or_insert(now);
        match step {
            Step::Wait(secs) => {
                self.hold_until = now + secs;
                self.say(format!("{now:7.2}  wait {secs}"));
            }
            Step::ClickWords(words, nth, press) => {
                let found = WORDS.lock().unwrap_or_else(|e| e.into_inner()).1.iter().filter(|w| match words.strip_prefix('~') { Some(start) => w.0.starts_with(start), None => w.0 == words }).nth(nth.saturating_sub(1)).map(|w| w.1);
                match found {
                    Some(rect) => {
                        self.say(format!("{now:7.2}  {} '{words}' at {:.0},{:.0}", if press { "click" } else { "hover" }, rect.center().x, rect.center().y));
                        if press {
                            self.click(rect.center());
                        } else {
                            self.queue.extend([vec![Event::PointerMoved(rect.center())], vec![]]);
                        }
                    }
                    None if now - since < 8.0 => {
                        self.steps.push_front(Step::ClickWords(words, nth, press)); // not on screen yet: look again next frame
                        return;
                    }
                    None => self.say(format!("{now:7.2}  MISSING '{words}': it never came on screen")),
                }
            }
            Step::ClickAt(at) => {
                self.say(format!("{now:7.2}  click at {:.0},{:.0}", at.x, at.y));
                self.click(at);
            }
            Step::MiddleAt(at) => {
                self.say(format!("{now:7.2}  middle click at {:.0},{:.0}", at.x, at.y));
                let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Middle, pressed, modifiers: Default::default() };
                self.queue.extend([vec![Event::PointerMoved(at)], vec![button(true)], vec![button(false)], vec![]]);
            }
            Step::MoveAt(at) => { self.queue.extend([vec![Event::PointerMoved(at)], vec![]]); }
            Step::Scroll(at, dy) => {
                self.say(format!("{now:7.2}  scroll {dy} at {:.0},{:.0}", at.x, at.y));
                let wheel = Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, dy), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Wheel, modifiers: Default::default() };
                self.queue.extend([vec![Event::PointerMoved(at)], vec![wheel], vec![]]);
            }
            Step::Key(key, ctrl) => {
                self.say(format!("{now:7.2}  key {}{key:?}", if ctrl { "Ctrl+" } else { "" }));
                let modifiers = if ctrl { egui::Modifiers::COMMAND | egui::Modifiers::CTRL } else { Default::default() };
                let press = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
                self.queue.extend([vec![press(true)], vec![press(false)]]);
            }
            Step::Type(text) => {
                self.say(format!("{now:7.2}  type '{text}'"));
                self.queue.extend([vec![Event::Text(text)], vec![]]);
            }
            Step::Shot(path) => {
                self.say(format!("{now:7.2}  shot {path}"));
                self.shot_busy.store(true, Ordering::Relaxed);
                let (busy, ctx2) = (self.shot_busy.clone(), ctx.clone());
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let [w, h] = img.size;
                    let raw: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
                    if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, raw) {
                        let _ = buf.save(&path);
                    }
                    busy.store(false, Ordering::Relaxed);
                    ctx2.request_repaint();
                })));
            }
            Step::Quit => {
                self.say(format!("{now:7.2}  quit"));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        self.since = None;
    }

    /// Hands the window this frame's share of the scripted input.
    pub fn feed(&mut self, raw: &mut egui::RawInput) {
        if let Some(events) = self.queue.pop_front() {
            raw.events.extend(events);
        }
    }
}
