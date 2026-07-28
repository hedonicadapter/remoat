//! Input injection with swappable backends.
//!   enigo   -> macOS + Linux/X11 (Quartz / XTEST)
//!   ydotool -> Linux/Wayland incl. Hyprland (kernel uinput, global)
//! Runs on a dedicated thread (enigo is !Send); async handlers enqueue over a channel.

use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::screen::Bounds;

#[derive(Debug, Clone)]
pub enum InputCmd {
    Move { dx: f64, dy: f64 },
    /// Absolute placement. The phone owns the cursor position when devices are arranged, so
    /// edge crossing can warp the pointer to the exact entry point on the next device.
    /// `home` = the pointer is teleporting (a crossing or a device switch), so a backend that
    /// only has relative motion must re-establish where the pointer actually is first.
    MoveAbs { x: i32, y: i32, home: bool },
    Click { button: String, double: bool },
    Scroll { dx: f64, dy: f64 },
    Text(String),
    Hotkey(Vec<String>),
    Key(String),
}

pub trait Backend {
    fn name(&self) -> &'static str;
    fn move_rel(&mut self, dx: f64, dy: f64);
    fn move_abs(&mut self, x: i32, y: i32, home: bool);
    fn click(&mut self, button: &str, double: bool);
    fn scroll(&mut self, dx: f64, dy: f64);
    fn text(&mut self, s: &str);
    fn hotkey(&mut self, keys: &[String]);
    fn key(&mut self, k: &str);

    fn apply(&mut self, cmd: InputCmd) {
        match cmd {
            InputCmd::Move { dx, dy } => self.move_rel(dx, dy),
            InputCmd::MoveAbs { x, y, home } => self.move_abs(x, y, home),
            InputCmd::Click { button, double } => self.click(&button, double),
            InputCmd::Scroll { dx, dy } => self.scroll(dx, dy),
            InputCmd::Text(s) => self.text(&s),
            InputCmd::Hotkey(keys) => self.hotkey(&keys),
            InputCmd::Key(k) => self.key(&k),
        }
    }
}

/// Spawn the input worker thread. Backend is chosen on that thread (enigo is !Send).
pub fn spawn_worker(bounds: Bounds) -> UnboundedSender<InputCmd> {
    let (tx, mut rx) = unbounded_channel::<InputCmd>();
    std::thread::spawn(move || {
        let mut backend = pick_backend(bounds);
        eprintln!("[input] backend: {}", backend.name());
        while let Some(cmd) = rx.blocking_recv() {
            backend.apply(cmd);
        }
    });
    tx
}

fn pick_backend(bounds: Bounds) -> Box<dyn Backend> {
    let dryrun = std::env::var("REMOAT_DRYRUN").is_ok();
    let choice = std::env::var("REMOAT_BACKEND").unwrap_or_else(|_| "auto".into()).to_lowercase();
    match choice.as_str() {
        "ydotool" => Box::new(YdotoolBackend::new(dryrun, bounds)),
        "enigo" => make_enigo(),
        "null" => Box::new(NullBackend::new("forced via REMOAT_BACKEND".into())),
        _ => auto_backend(dryrun, bounds), // macOS -> enigo; Linux -> ydotool (uinput); else null
    }
}

#[cfg(target_os = "macos")]
fn auto_backend(_dryrun: bool, _bounds: Bounds) -> Box<dyn Backend> {
    make_enigo()
}
#[cfg(target_os = "linux")]
fn auto_backend(dryrun: bool, bounds: Bounds) -> Box<dyn Backend> {
    Box::new(YdotoolBackend::new(dryrun, bounds))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn auto_backend(_dryrun: bool, _bounds: Bounds) -> Box<dyn Backend> {
    Box::new(NullBackend::new("unsupported platform".into()))
}

#[cfg(target_os = "macos")]
fn make_enigo() -> Box<dyn Backend> {
    match Enigo::new(&Settings::default()) {
        Ok(enigo) => Box::new(EnigoBackend { enigo }),
        Err(e) => Box::new(NullBackend::new(format!("enigo: {e}"))),
    }
}
#[cfg(not(target_os = "macos"))]
fn make_enigo() -> Box<dyn Backend> {
    Box::new(NullBackend::new("enigo backend is macOS-only".into()))
}

// ─── enigo (macOS) ──────────────────────────────────────────────────────────
#[cfg(target_os = "macos")]
struct EnigoBackend {
    enigo: Enigo,
}

#[cfg(target_os = "macos")]
fn enigo_key(n: &str) -> Option<Key> {
    Some(match n {
        "ctrl" | "control" => Key::Control,
        "alt" => Key::Alt,
        "shift" => Key::Shift,
        "command" | "super" | "meta" => Key::Meta,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        s if s.chars().count() == 1 => Key::Unicode(s.chars().next().unwrap()),
        _ => return None,
    })
}

#[cfg(target_os = "macos")]
impl Backend for EnigoBackend {
    fn name(&self) -> &'static str { "enigo" }
    fn move_rel(&mut self, dx: f64, dy: f64) {
        let _ = self.enigo.move_mouse(dx.round() as i32, dy.round() as i32, Coordinate::Rel);
    }
    fn move_abs(&mut self, x: i32, y: i32, _home: bool) {
        // Quartz takes real absolute coordinates, so there is nothing to re-establish.
        let _ = self.enigo.move_mouse(x, y, Coordinate::Abs);
    }
    fn click(&mut self, button: &str, double: bool) {
        let b = match button { "right" => Button::Right, "middle" => Button::Middle, _ => Button::Left };
        let _ = self.enigo.button(b, Direction::Click);
        if double { let _ = self.enigo.button(b, Direction::Click); }
    }
    fn scroll(&mut self, dx: f64, dy: f64) {
        // px -> notches; signs tuned for natural direction (flip if inverted)
        let v = (-dy / 40.0).round() as i32;
        let h = (-dx / 40.0).round() as i32;
        if v != 0 { let _ = self.enigo.scroll(v, Axis::Vertical); }
        if h != 0 { let _ = self.enigo.scroll(h, Axis::Horizontal); }
    }
    fn text(&mut self, s: &str) { let _ = self.enigo.text(s); }
    fn hotkey(&mut self, keys: &[String]) {
        let ks: Vec<Key> = keys.iter().filter_map(|k| enigo_key(k)).collect();
        for k in &ks { let _ = self.enigo.key(*k, Direction::Press); }
        for k in ks.iter().rev() { let _ = self.enigo.key(*k, Direction::Release); }
    }
    fn key(&mut self, k: &str) {
        if let Some(key) = enigo_key(k) { let _ = self.enigo.key(key, Direction::Click); }
    }
}

// ─── ydotool (Wayland) ────────────────────────────────────────────────────────
// Linux input-event keycodes (kernel constants — stable across ydotool versions).
fn kc(n: &str) -> Option<i32> {
    Some(match n {
        "ctrl" | "control" => 29,
        "alt" => 56,
        "shift" => 42,
        "super" | "command" | "meta" => 125,
        // Full letter row set — command-table rules bind arbitrary chords (mod+s, mod+t, …).
        "a" => 30, "b" => 48, "c" => 46, "d" => 32, "e" => 18, "f" => 33, "g" => 34,
        "h" => 35, "i" => 23, "j" => 36, "k" => 37, "l" => 38, "m" => 50, "n" => 49,
        "o" => 24, "p" => 25, "q" => 16, "r" => 19, "s" => 31, "t" => 20, "u" => 22,
        "v" => 47, "w" => 17, "x" => 45, "y" => 21, "z" => 44,
        "1" => 2, "2" => 3, "3" => 4, "4" => 5, "5" => 6,
        "6" => 7, "7" => 8, "8" => 9, "9" => 10, "0" => 11,
        "minus" => 12, "equal" => 13, "comma" => 51, "dot" => 52, "slash" => 53,
        "left" => 105, "right" => 106, "up" => 103, "down" => 108,
        "home" => 102, "end" => 107, "pageup" | "page_up" => 104, "pagedown" | "page_down" => 109,
        "insert" => 110, "delete" => 111,
        "enter" | "return" => 28, "esc" | "escape" => 1, "tab" => 15, "space" => 57, "backspace" => 14,
        _ => return None,
    })
}

/// Is this key name mappable on the running platform? Config validation gate — a rule with
/// an unknown key is skipped at load instead of silently no-op'ing mid-command.
pub fn key_known(name: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        enigo_key(name).is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        kc(name).is_some()
    }
}

/// Overshoot used when a target sits on an edge, so the compositor's own clamping puts the
/// pointer exactly there. Edges are the only place a relative backend can resynchronise.
const EDGE_PUSH: i32 = 40;

struct YdotoolBackend {
    dryrun: bool,
    /// Desktop size, needed to emulate absolute placement.
    bounds: Bounds,
    /// Where we last commanded the pointer to be. `None` = unknown (nothing sent yet, or a
    /// raw relative move moved it out from under us).
    pos: Option<(i32, i32)>,
}

impl YdotoolBackend {
    fn new(dryrun: bool, bounds: Bounds) -> Self {
        YdotoolBackend { dryrun, bounds, pos: None }
    }

    fn run(&self, args: &[String]) {
        if self.dryrun {
            println!("[dryrun] ydotool {}", args.join(" "));
            return;
        }
        let _ = Command::new("ydotool").args(args).status();
    }
    fn keyseq(&self, names: &[String]) {
        if let Some(args) = keyargs(names) {
            self.run(&args);
        }
    }

    /// Corner to slam into for a known origin: the nearest one, but NEVER top-left — that is
    /// the hot corner on niri and GNOME, and homing there pops the overview open.
    fn home_corner(&self, x: i32, y: i32) -> (i32, i32) {
        let (w, h) = (self.bounds.w - 1, self.bounds.h - 1);
        [(0, h), (w, h), (w, 0)]
            .into_iter()
            .min_by_key(|(cx, cy)| (cx - x).pow(2) + (cy - y).pow(2))
            .unwrap()
    }
}

fn relargs(dx: i32, dy: i32) -> Vec<String> {
    vec!["mousemove".into(), "-x".into(), dx.to_string(), "-y".into(), dy.to_string()]
}

/// Delta that drives the pointer into `corner` from ANYWHERE: both axes always move a full
/// two screens toward it, so the compositor's clamping does the positioning. Deriving the
/// direction from the corner and not from the target matters — a target already sharing an
/// axis with the corner would otherwise leave that axis un-pinned, i.e. still unknown.
fn home_slam(corner: (i32, i32), b: Bounds) -> (i32, i32) {
    let sx = if corner.0 == 0 { -1 } else { 1 };
    let sy = if corner.1 == 0 { -1 } else { 1 };
    (sx * b.w * 2, sy * b.h * 2)
}

/// Delta that walks the pointer from `from` to `to`, overshooting when the target is on an
/// edge. The overshoot is what keeps this honest: pointer acceleration means a delta of N does
/// not move exactly N pixels, so error accumulates — but every edge contact re-pins both the
/// real pointer and our idea of it, and reaching an edge is exactly what crossing requires.
fn abs_to_rel(from: (i32, i32), to: (i32, i32), b: Bounds) -> (i32, i32) {
    let mut dx = to.0 - from.0;
    let mut dy = to.1 - from.1;
    if to.0 <= 0 { dx -= EDGE_PUSH } else if to.0 >= b.w - 1 { dx += EDGE_PUSH }
    if to.1 <= 0 { dy -= EDGE_PUSH } else if to.1 >= b.h - 1 { dy += EDGE_PUSH }
    (dx, dy)
}

/// `["ctrl","home"]` -> `key 29:1 102:1 102:0 29:1`. Press in order, release in reverse.
/// Split out from the call so the chord wiring is testable without a uinput device.
fn keyargs(names: &[String]) -> Option<Vec<String>> {
    let codes: Vec<i32> = names.iter().filter_map(|n| kc(n)).collect();
    if codes.is_empty() {
        return None;
    }
    let mut args = vec!["key".to_string()];
    for c in &codes { args.push(format!("{c}:1")); }
    for c in codes.iter().rev() { args.push(format!("{c}:0")); }
    Some(args)
}

impl Backend for YdotoolBackend {
    fn name(&self) -> &'static str { "ydotool" }
    fn move_rel(&mut self, dx: f64, dy: f64) {
        self.run(&relargs(dx.round() as i32, dy.round() as i32));
        self.pos = None; // an old-style relative client is driving; we no longer know where we are
    }

    /// ydotool cannot place a pointer absolutely: `ydotoold` never gives its uinput device an
    /// ABS range (no UI_ABS_SETUP anywhere in ydotool 1.0.4), so every absolute event collapses
    /// to 0,0 — upstream issue #250, and on a compositor with a top-left hot corner that means
    /// the pointer parks there and opens an overview. So absolute placement is emulated with
    /// relative deltas from the last commanded position.
    fn move_abs(&mut self, x: i32, y: i32, home: bool) {
        let (x, y) = self.bounds.clamp(x, y);
        if home || self.pos.is_none() {
            // Teleporting (crossing in, or a device switch): slam into a corner so the
            // compositor's clamping gives us a known origin to move from.
            let corner = self.home_corner(x, y);
            let (sx, sy) = home_slam(corner, self.bounds);
            self.run(&relargs(sx, sy));
            self.pos = Some(corner);
        }
        let from = self.pos.unwrap_or((x, y));
        let (dx, dy) = abs_to_rel(from, (x, y), self.bounds);
        if dx != 0 || dy != 0 {
            self.run(&relargs(dx, dy));
        }
        self.pos = Some((x, y));
    }
    fn click(&mut self, button: &str, double: bool) {
        let code = match button { "right" => "0xC1", "middle" => "0xC2", _ => "0xC0" };
        self.run(&["click".into(), code.into()]);
        if double { self.run(&["click".into(), code.into()]); }
    }
    fn scroll(&mut self, dx: f64, dy: f64) {
        // Wheel mode is `-w` (short) on the ReimuNotMoe ydotool; the long `--wheel` is
        // rejected. It also requires BOTH -x and -y present — omitting the zero axis
        // makes it reject the call and dump usage. -x = horizontal, -y = vertical notches.
        let v = (-dy / 40.0).round() as i32;
        let h = (-dx / 40.0).round() as i32;
        if v == 0 && h == 0 {
            return;
        }
        self.run(&[
            "mousemove".into(), "-w".into(),
            "-x".into(), h.to_string(),
            "-y".into(), v.to_string(),
        ]);
    }
    fn text(&mut self, s: &str) {
        if !self.dryrun && wl_copy(s) {
            self.keyseq(&["ctrl".into(), "v".into()]);
        } else {
            self.run(&["type".into(), s.into()]);
        }
    }
    fn hotkey(&mut self, keys: &[String]) { self.keyseq(keys); }
    fn key(&mut self, k: &str) { self.keyseq(&[k.to_string()]); }
}

fn wl_copy(s: &str) -> bool {
    match Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
        Ok(mut child) => {
            if let Some(mut sin) = child.stdin.take() { let _ = sin.write_all(s.as_bytes()); }
            let _ = child.wait();
            true
        }
        Err(_) => false,
    }
}

// ─── null (no backend available) ───────────────────────────────────────────────
struct NullBackend;
impl NullBackend {
    fn new(reason: String) -> Self {
        eprintln!("[input] no injection backend ({reason}) — serving UI only, input disabled");
        NullBackend
    }
}
impl Backend for NullBackend {
    fn name(&self) -> &'static str { "null" }
    fn move_rel(&mut self, _: f64, _: f64) {}
    fn move_abs(&mut self, _: i32, _: i32, _: bool) {}
    fn click(&mut self, _: &str, _: bool) {}
    fn scroll(&mut self, _: f64, _: f64) {}
    fn text(&mut self, _: &str) {}
    fn hotkey(&mut self, _: &[String]) {}
    fn key(&mut self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ydotool_chord_presses_then_releases_in_reverse() {
        assert_eq!(
            keyargs(&names(&["ctrl", "home"])).unwrap(),
            names(&["key", "29:1", "102:1", "102:0", "29:0"])
        );
    }

    #[test]
    fn ydotool_knows_the_keys_the_command_table_uses() {
        // Regression guard: the voice table binds mod+<letter> chords and home/end/page keys,
        // all of which were missing from kc() before the command table existed.
        for k in ["s", "t", "w", "r", "f", "home", "end", "pageup", "pagedown", "delete"] {
            assert!(kc(k).is_some(), "ydotool keycode missing for {k:?}");
        }
    }

    #[test]
    fn unknown_key_yields_no_argv() {
        assert!(keyargs(&names(&["hyperkey"])).is_none());
    }

    const B: Bounds = Bounds { w: 1920, h: 1080 };

    #[test]
    fn absolute_is_emulated_as_a_relative_delta() {
        // ydotool's own absolute mode is unusable (no ABS range upstream), so a target becomes
        // a delta from where we last put the pointer.
        assert_eq!(abs_to_rel((100, 100), (400, 250), B), (300, 150));
        assert_eq!(relargs(300, 150), names(&["mousemove", "-x", "300", "-y", "150"]));
    }

    #[test]
    fn edge_targets_overshoot_so_the_compositor_pins_them() {
        // The only resync a relative backend gets: shove past the edge and let it clamp.
        assert_eq!(abs_to_rel((500, 500), (0, 500), B), (-500 - EDGE_PUSH, 0));
        assert_eq!(abs_to_rel((500, 500), (1919, 500), B), (1419 + EDGE_PUSH, 0));
        assert_eq!(abs_to_rel((500, 500), (500, 1079), B), (0, 579 + EDGE_PUSH));
    }

    #[test]
    fn homing_slams_both_axes_regardless_of_the_target() {
        // Regression: deriving the slam direction from the target meant a target that already
        // shared an axis with the corner (e.g. x=0 entering from the left) left that axis
        // un-pinned — so "homing" produced no known origin at all.
        assert_eq!(home_slam((0, B.h - 1), B), (-3840, 2160));
        assert_eq!(home_slam((B.w - 1, 0), B), (3840, -2160));
        for corner in [(0, B.h - 1), (B.w - 1, B.h - 1), (B.w - 1, 0)] {
            let (sx, sy) = home_slam(corner, B);
            assert!(sx.abs() >= B.w && sy.abs() >= B.h, "slam must overshoot both axes");
        }
    }

    #[test]
    fn homing_never_picks_the_top_left_hot_corner() {
        // Top-left opens the overview on niri and GNOME; homing there would pop it every time.
        let b = YdotoolBackend::new(true, B);
        for (x, y) in [(0, 0), (10, 10), (960, 540), (1919, 0), (0, 1079), (1919, 1079)] {
            assert_ne!(b.home_corner(x, y), (0, 0), "homed into the hot corner from {x},{y}");
        }
        assert_eq!(b.home_corner(10, 10), (0, 1079), "nearest allowed corner");
        assert_eq!(b.home_corner(1900, 20), (1919, 0));
    }
}
