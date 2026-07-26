//! Input injection with swappable backends.
//!   enigo   -> macOS + Linux/X11 (Quartz / XTEST)
//!   ydotool -> Linux/Wayland incl. Hyprland (kernel uinput, global)
//! Runs on a dedicated thread (enigo is !Send); async handlers enqueue over a channel.

use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

#[derive(Debug, Clone)]
pub enum InputCmd {
    Move { dx: f64, dy: f64 },
    Click { button: String, double: bool },
    Scroll { dx: f64, dy: f64 },
    Text(String),
    Hotkey(Vec<String>),
    Key(String),
}

pub trait Backend {
    fn name(&self) -> &'static str;
    fn move_rel(&mut self, dx: f64, dy: f64);
    fn click(&mut self, button: &str, double: bool);
    fn scroll(&mut self, dx: f64, dy: f64);
    fn text(&mut self, s: &str);
    fn hotkey(&mut self, keys: &[String]);
    fn key(&mut self, k: &str);

    fn apply(&mut self, cmd: InputCmd) {
        match cmd {
            InputCmd::Move { dx, dy } => self.move_rel(dx, dy),
            InputCmd::Click { button, double } => self.click(&button, double),
            InputCmd::Scroll { dx, dy } => self.scroll(dx, dy),
            InputCmd::Text(s) => self.text(&s),
            InputCmd::Hotkey(keys) => self.hotkey(&keys),
            InputCmd::Key(k) => self.key(&k),
        }
    }
}

/// Spawn the input worker thread. Backend is chosen on that thread (enigo is !Send).
pub fn spawn_worker() -> UnboundedSender<InputCmd> {
    let (tx, mut rx) = unbounded_channel::<InputCmd>();
    std::thread::spawn(move || {
        let mut backend = pick_backend();
        eprintln!("[input] backend: {}", backend.name());
        while let Some(cmd) = rx.blocking_recv() {
            backend.apply(cmd);
        }
    });
    tx
}

fn pick_backend() -> Box<dyn Backend> {
    let dryrun = std::env::var("REMOAT_DRYRUN").is_ok();
    let choice = std::env::var("REMOAT_BACKEND").unwrap_or_else(|_| "auto".into()).to_lowercase();
    match choice.as_str() {
        "ydotool" => Box::new(YdotoolBackend { dryrun }),
        "enigo" => make_enigo(),
        "null" => Box::new(NullBackend::new("forced via REMOAT_BACKEND".into())),
        _ => auto_backend(dryrun), // macOS -> enigo; Linux -> ydotool (uinput); else null
    }
}

#[cfg(target_os = "macos")]
fn auto_backend(_dryrun: bool) -> Box<dyn Backend> {
    make_enigo()
}
#[cfg(target_os = "linux")]
fn auto_backend(dryrun: bool) -> Box<dyn Backend> {
    Box::new(YdotoolBackend { dryrun })
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn auto_backend(_dryrun: bool) -> Box<dyn Backend> {
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
        "a" => 30, "c" => 46, "v" => 47, "x" => 45, "z" => 44,
        "left" => 105, "right" => 106, "up" => 103, "down" => 108,
        "enter" | "return" => 28, "esc" | "escape" => 1, "tab" => 15, "space" => 57, "backspace" => 14,
        _ => return None,
    })
}

struct YdotoolBackend {
    dryrun: bool,
}

impl YdotoolBackend {
    fn run(&self, args: &[String]) {
        if self.dryrun {
            println!("[dryrun] ydotool {}", args.join(" "));
            return;
        }
        let _ = Command::new("ydotool").args(args).status();
    }
    fn keyseq(&self, names: &[String]) {
        let codes: Vec<i32> = names.iter().filter_map(|n| kc(n)).collect();
        if codes.is_empty() { return; }
        let mut args = vec!["key".to_string()];
        for c in &codes { args.push(format!("{c}:1")); }
        for c in codes.iter().rev() { args.push(format!("{c}:0")); }
        self.run(&args);
    }
}

impl Backend for YdotoolBackend {
    fn name(&self) -> &'static str { "ydotool" }
    fn move_rel(&mut self, dx: f64, dy: f64) {
        self.run(&["mousemove".into(), "-x".into(), (dx.round() as i32).to_string(),
                   "-y".into(), (dy.round() as i32).to_string()]);
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
    fn click(&mut self, _: &str, _: bool) {}
    fn scroll(&mut self, _: f64, _: f64) {}
    fn text(&mut self, _: &str) {}
    fn hotkey(&mut self, _: &[String]) {}
    fn key(&mut self, _: &str) {}
}
