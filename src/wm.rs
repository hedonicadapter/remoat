//! Window and workspace control, behind one trait with per-WM backends.
//!
//! Why per-WM IPC rather than a portable protocol: `wlr-foreign-toplevel-management-v1` is
//! implemented by ~20 compositors (niri and Hyprland included) and would cover listing, focus
//! and close with a single implementation — but it has no concept of a **workspace**, which is
//! half of what "control my desktop" means. The compositor's own IPC is strictly more capable,
//! and on both targeted machines it is a cheap JSON shell-out. The portable protocol is the
//! long-tail fallback for everyone else, and it slots in here as another backend without the
//! command layer noticing.
//!
//! Detection probes CAPABILITY, never the compositor's name: "can I list windows?", not "am I
//! on niri?". That is what lets a new backend appear without touching `dynamic.rs`, and what
//! makes a missing capability degrade into "that command doesn't exist" rather than a silent
//! no-op — the `wlr-randr` lesson from screen.rs.
//!
//! The niri parsing is verified against real `niri msg --json` output shape. **The omniwm
//! parsing is written from its published IPC docs, not from a live daemon** — treat it as
//! unexercised until someone runs it on a Mac.

use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

/// One open window, normalized across backends.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// Opaque to us on purpose: an integer on niri, `ow_…` on omniwm. NEVER cache these —
    /// omniwm's are session-scoped and go stale when it restarts, so they are resolved fresh
    /// on every utterance.
    pub id: String,
    pub title: String,
    /// Application identity (`app_id` on Wayland, bundle/app name on macOS).
    pub app: String,
}

/// A direction, for moving or focusing relative to the current window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn parse(s: &str) -> Option<Dir> {
        Some(match s {
            "left" => Dir::Left,
            "right" => Dir::Right,
            "up" | "above" => Dir::Up,
            "down" | "below" => Dir::Down,
            _ => return None,
        })
    }
    pub fn word(&self) -> &'static str {
        match self {
            Dir::Left => "left",
            Dir::Right => "right",
            Dir::Up => "up",
            Dir::Down => "down",
        }
    }
}

/// Everything past `windows`/`focus` returns Option so a backend can decline a capability it
/// genuinely lacks, rather than us guessing a subcommand name and emitting an error toast.
pub trait Wm: Send + Sync {
    fn name(&self) -> &'static str;
    fn windows(&self) -> Result<Vec<Window>, String>;
    /// Focus a window, switching workspace if the backend can.
    fn focus_argv(&self, id: &str) -> Vec<String>;
    /// Switch to a workspace by 1-based index.
    fn workspace_argv(&self, n: i32) -> Vec<String>;
    /// Move the focused window in a direction.
    fn move_argv(&self, dir: Dir) -> Option<Vec<String>>;
    /// Move focus in a direction, without naming a window.
    fn focus_dir_argv(&self, dir: Dir) -> Option<Vec<String>>;
    /// Next/previous workspace: +1 / -1.
    fn workspace_rel_argv(&self, delta: i32) -> Option<Vec<String>>;
    fn close_argv(&self) -> Option<Vec<String>> {
        None
    }
    fn fullscreen_argv(&self) -> Option<Vec<String>> {
        None
    }
    fn float_argv(&self) -> Option<Vec<String>> {
        None
    }
    /// Send the focused window to a workspace by 1-based index.
    fn move_to_workspace_argv(&self, _n: i32) -> Option<Vec<String>> {
        None
    }
}

fn niri_action(a: &str) -> Vec<String> {
    vec!["niri".into(), "msg".into(), "action".into(), a.into()]
}

fn omni_command(a: &str) -> Vec<String> {
    vec!["omniwmctl".into(), "command".into(), a.into()]
}

// ─── niri ──────────────────────────────────────────────────────────────────
struct Niri;

impl Wm for Niri {
    fn name(&self) -> &'static str {
        "niri"
    }
    fn windows(&self) -> Result<Vec<Window>, String> {
        let out = Command::new("niri")
            .args(["msg", "--json", "windows"])
            .output()
            .map_err(|e| format!("niri: {e}"))?;
        if !out.status.success() {
            return Err("niri msg failed".into());
        }
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("niri json: {e}"))?;
        Ok(parse_niri_windows(&v))
    }
    fn focus_argv(&self, id: &str) -> Vec<String> {
        vec!["niri".into(), "msg".into(), "action".into(), "focus-window".into(), "--id".into(), id.into()]
    }
    fn workspace_argv(&self, n: i32) -> Vec<String> {
        let mut v = niri_action("focus-workspace");
        v.push(n.to_string());
        v
    }
    /// niri's scrolling layout distinguishes columns from windows-within-a-column: horizontal
    /// motion moves the whole COLUMN, vertical motion moves the window inside it. Using
    /// `move-window-left` here would silently do nothing.
    fn move_argv(&self, dir: Dir) -> Option<Vec<String>> {
        Some(niri_action(match dir {
            Dir::Left => "move-column-left",
            Dir::Right => "move-column-right",
            Dir::Up => "move-window-up",
            Dir::Down => "move-window-down",
        }))
    }
    fn focus_dir_argv(&self, dir: Dir) -> Option<Vec<String>> {
        Some(niri_action(match dir {
            Dir::Left => "focus-column-left",
            Dir::Right => "focus-column-right",
            Dir::Up => "focus-window-up",
            Dir::Down => "focus-window-down",
        }))
    }
    fn workspace_rel_argv(&self, delta: i32) -> Option<Vec<String>> {
        // Workspaces are a vertical stack on niri, so "next" is down.
        Some(niri_action(if delta >= 0 { "focus-workspace-down" } else { "focus-workspace-up" }))
    }
    fn close_argv(&self) -> Option<Vec<String>> {
        Some(niri_action("close-window"))
    }
    fn fullscreen_argv(&self) -> Option<Vec<String>> {
        Some(niri_action("fullscreen-window"))
    }
    fn float_argv(&self) -> Option<Vec<String>> {
        Some(niri_action("toggle-window-floating"))
    }
    fn move_to_workspace_argv(&self, n: i32) -> Option<Vec<String>> {
        let mut v = niri_action("move-window-to-workspace");
        v.push(n.to_string());
        Some(v)
    }
}

/// `niri msg --json windows` -> an array of window objects.
pub fn parse_niri_windows(v: &serde_json::Value) -> Vec<Window> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|w| {
            Some(Window {
                id: w.get("id")?.as_i64()?.to_string(),
                title: w.get("title").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
                app: w.get("app_id").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

// ─── omniwm (macOS) ────────────────────────────────────────────────────────
struct Omni;

impl Wm for Omni {
    fn name(&self) -> &'static str {
        "omniwm"
    }
    fn windows(&self) -> Result<Vec<Window>, String> {
        let out = Command::new("omniwmctl")
            .args(["query", "windows", "--json"])
            .output()
            .map_err(|e| format!("omniwmctl: {e}"))?;
        if !out.status.success() {
            return Err(ipc_hint(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(parse_omni_windows(&String::from_utf8_lossy(&out.stdout)))
    }
    fn focus_argv(&self, id: &str) -> Vec<String> {
        // `navigate` rather than `focus`: it switches workspace if the window is elsewhere,
        // which is what "focus slack" means to a person.
        vec!["omniwmctl".into(), "window".into(), "navigate".into(), id.into()]
    }
    fn workspace_argv(&self, n: i32) -> Vec<String> {
        let mut v = omni_command("switch-workspace");
        v.push(n.to_string());
        v
    }
    fn move_argv(&self, dir: Dir) -> Option<Vec<String>> {
        let mut v = omni_command("move");
        v.push(dir.word().into());
        Some(v)
    }
    fn focus_dir_argv(&self, dir: Dir) -> Option<Vec<String>> {
        let mut v = omni_command("focus");
        v.push(dir.word().into());
        Some(v)
    }
    fn workspace_rel_argv(&self, _delta: i32) -> Option<Vec<String>> {
        // Not in the published CLI reference. Guessing a subcommand would turn "next workspace"
        // into an error toast, so decline and let the phrase go unmatched instead.
        None
    }
    fn float_argv(&self) -> Option<Vec<String>> {
        Some(omni_command("toggle-focused-window-floating"))
    }
    // close/fullscreen/move-to-workspace: likewise undocumented, so deliberately not guessed.
}

/// omniwm speaks newline-delimited JSON, but tolerate a plain array too — the docs describe
/// NDJSON while several subcommands read naturally as one array, and guessing wrong would
/// silently yield zero windows.
pub fn parse_omni_windows(s: &str) -> Vec<Window> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s.trim()) {
        if let Some(arr) = v.as_array() {
            return arr.iter().filter_map(omni_window).collect();
        }
        if let Some(w) = omni_window(&v) {
            return vec![w];
        }
    }
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| omni_window(&v))
        .collect()
}

fn omni_window(w: &serde_json::Value) -> Option<Window> {
    // id is a string here (`ow_…`), unlike niri's integer.
    let id = w.get("id").and_then(|i| i.as_str().map(str::to_string).or_else(|| i.as_i64().map(|n| n.to_string())))?;
    Some(Window {
        id,
        title: w.get("title").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
        // `app` per the CLI's --fields; fall back to app_id so a rename doesn't blank it out.
        app: w
            .get("app")
            .or_else(|| w.get("app_id"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// omniwm ships with IPC **disabled by default**, so "command failed" is overwhelmingly likely
/// to mean "switch it on", not "something broke". Saying so is the difference between a
/// two-second fix and an afternoon — cf. wlr-randr silently missing from PATH for months.
fn ipc_hint(stderr: &str) -> String {
    let s = stderr.trim();
    if s.is_empty() {
        "omniwmctl failed — is IPC enabled in OmniWM's settings?".into()
    } else {
        format!("omniwmctl: {s} (is IPC enabled in OmniWM's settings?)")
    }
}

// ─── detection ─────────────────────────────────────────────────────────────
/// Cached because it shells out; a WM does not appear mid-session.
static DETECTED: OnceLock<Option<Arc<dyn Wm>>> = OnceLock::new();

pub fn detect() -> Option<Arc<dyn Wm>> {
    DETECTED
        .get_or_init(|| {
            for (probe, args, wm) in [
                ("niri", &["msg", "--json", "version"][..], Arc::new(Niri) as Arc<dyn Wm>),
                ("omniwmctl", &["ping"][..], Arc::new(Omni) as Arc<dyn Wm>),
            ] {
                match Command::new(probe).args(args).output() {
                    Ok(o) if o.status.success() => {
                        eprintln!("[wm] backend: {}", wm.name());
                        return Some(wm);
                    }
                    // Present but refusing: worth saying out loud, because it is fixable.
                    Ok(o) => eprintln!(
                        "[wm] {probe} found but not responding — {}",
                        ipc_hint(&String::from_utf8_lossy(&o.stderr))
                    ),
                    Err(_) => {} // not installed; try the next backend
                }
            }
            eprintln!("[wm] no window-manager IPC found — window and workspace commands disabled");
            None
        })
        .clone()
}

/// Live window list, cached very briefly. One utterance can consult it more than once (match,
/// then build the action), and a compositor round-trip per lookup is wasteful — but anything
/// longer than a moment would hand back windows that have since closed.
pub fn windows() -> Vec<Window> {
    const TTL: std::time::Duration = std::time::Duration::from_millis(750);
    static CACHE: OnceLock<Mutex<Option<(std::time::Instant, Arc<Vec<Window>>)>>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(g) = cell.lock() {
        if let Some((at, v)) = g.as_ref() {
            if at.elapsed() < TTL {
                return v.as_ref().clone();
            }
        }
    }
    let fresh = match detect() {
        Some(wm) => wm.windows().unwrap_or_else(|e| {
            eprintln!("[wm] {e}");
            Vec::new()
        }),
        None => Vec::new(),
    };
    if let Ok(mut g) = cell.lock() {
        *g = Some((std::time::Instant::now(), Arc::new(fresh.clone())));
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_niri_window_list() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"id":3,"title":"nvim — src/wm.rs","app_id":"Alacritty","is_focused":true},
                {"id":7,"title":"Slack | general","app_id":"slack"},
                {"id":9,"app_id":"firefox"}]"#,
        )
        .unwrap();
        let w = parse_niri_windows(&v);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0], Window { id: "3".into(), title: "nvim — src/wm.rs".into(), app: "Alacritty".into() });
        assert_eq!(w[2].title, "", "a window with no title must still be listed");
        assert_eq!(w[2].id, "9");
    }

    #[test]
    fn niri_ids_are_stringified_not_dropped() {
        // niri ids are integers, omniwm's are strings; the trait normalizes to String so the
        // matcher never has to care which backend it is talking to.
        let v: serde_json::Value = serde_json::from_str(r#"[{"id":42,"title":"t","app_id":"a"}]"#).unwrap();
        assert_eq!(parse_niri_windows(&v)[0].id, "42");
    }

    #[test]
    fn parses_omniwm_ndjson() {
        let s = "{\"id\":\"ow_1\",\"title\":\"Safari\",\"app\":\"Safari\"}\n\
                 {\"id\":\"ow_2\",\"title\":\"Ghostty\",\"app\":\"Ghostty\"}\n";
        let w = parse_omni_windows(s);
        assert_eq!(w.len(), 2);
        assert_eq!(w[1], Window { id: "ow_2".into(), title: "Ghostty".into(), app: "Ghostty".into() });
    }

    #[test]
    fn parses_omniwm_array_form_too() {
        // The docs say NDJSON; several subcommands read naturally as an array. Guessing one
        // and being wrong would silently produce zero windows, so both are accepted.
        let s = r#"[{"id":"ow_1","title":"Safari","app":"Safari"}]"#;
        assert_eq!(parse_omni_windows(s).len(), 1);
        assert_eq!(parse_omni_windows(r#"{"id":"ow_9","title":"solo","app":"x"}"#).len(), 1);
    }

    #[test]
    fn malformed_output_yields_no_windows_rather_than_panicking() {
        assert!(parse_omni_windows("not json at all").is_empty());
        assert!(parse_omni_windows("").is_empty());
        assert!(parse_niri_windows(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn focus_and_workspace_argv_are_backend_shaped() {
        assert_eq!(
            Niri.focus_argv("5"),
            vec!["niri", "msg", "action", "focus-window", "--id", "5"]
        );
        assert_eq!(Niri.workspace_argv(2), vec!["niri", "msg", "action", "focus-workspace", "2"]);
        // `navigate`, not `focus` — it crosses workspaces, which is what the user means.
        assert_eq!(Omni.focus_argv("ow_3"), vec!["omniwmctl", "window", "navigate", "ow_3"]);
        assert_eq!(
            Omni.workspace_argv(2),
            vec!["omniwmctl", "command", "switch-workspace", "2"]
        );
    }

    #[test]
    fn ipc_hint_points_at_the_actual_fix() {
        assert!(ipc_hint("").contains("IPC"));
        assert!(ipc_hint("connection refused").contains("connection refused"));
    }
}
