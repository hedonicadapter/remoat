//! Screen bounds — what the phone needs in order to own an absolute cursor.
//!
//! Edge crossing between arranged devices only works if someone knows where the cursor IS.
//! Nothing can ask Wayland (uinput is write-only, and there's no protocol for reading the
//! pointer), so the *phone* owns the position: it integrates touch deltas per device and
//! sends `move_abs`. For that it needs each device's bounds, which is this module.
//!
//! Origin is assumed 0,0 and only one desktop rectangle is reported — a multi-monitor
//! desktop is exposed as its bounding box where the platform tells us cheaply, else as the
//! main display. `REMOAT_SCREEN=WxH` overrides everything.

// Only the Linux probes shell out; mac reads the display straight from Quartz via enigo.
#[cfg(not(target_os = "macos"))]
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub w: i32,
    pub h: i32,
}

/// Anything is better than 0×0 (which would make every edge an edge).
const FALLBACK: Bounds = Bounds { w: 1920, h: 1080 };

impl Bounds {
    fn sane(w: i32, h: i32) -> Option<Bounds> {
        (w > 0 && h > 0 && w <= 65535 && h <= 65535).then_some(Bounds { w, h })
    }

    /// Keep a client's absolute move inside the desktop — a bad or stale client must not be
    /// able to park the cursor somewhere unreachable.
    pub fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        (x.clamp(0, self.w - 1), y.clamp(0, self.h - 1))
    }
}

pub fn detect() -> Bounds {
    let found = env_override().or_else(platform_bounds);
    match found {
        Some(b) => {
            eprintln!("[screen] {}x{}", b.w, b.h);
            b
        }
        None => {
            eprintln!(
                "[screen] could not detect size — assuming {}x{} (set REMOAT_SCREEN=WxH)",
                FALLBACK.w, FALLBACK.h
            );
            FALLBACK
        }
    }
}

fn env_override() -> Option<Bounds> {
    let v = std::env::var("REMOAT_SCREEN").ok()?;
    let (w, h) = v.split_once(['x', 'X'])?;
    Bounds::sane(w.trim().parse().ok()?, h.trim().parse().ok()?)
}

#[cfg(target_os = "macos")]
fn platform_bounds() -> Option<Bounds> {
    use enigo::{Enigo, Mouse, Settings};
    // Throwaway instance: main_display() is a Quartz query, not an injection, so this works
    // even without Accessibility granted.
    let enigo = Enigo::new(&Settings::default()).ok()?;
    let (w, h) = enigo.main_display().ok()?;
    Bounds::sane(w, h)
}

#[cfg(not(target_os = "macos"))]
fn platform_bounds() -> Option<Bounds> {
    hyprland_bounds().or_else(wlr_randr_bounds).or_else(xrandr_bounds)
}

/// Hyprland: `hyprctl monitors -j` gives position + size per monitor, so the union is the
/// real desktop rectangle (the phone can then reach a second monitor by moving into it).
#[cfg(not(target_os = "macos"))]
fn hyprland_bounds() -> Option<Bounds> {
    let out = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    union_of(v.as_array()?.iter().filter_map(|m| {
        Some((
            m.get("x")?.as_i64()? as i32,
            m.get("y")?.as_i64()? as i32,
            m.get("width")?.as_i64()? as i32,
            m.get("height")?.as_i64()? as i32,
        ))
    }))
}

#[cfg(not(target_os = "macos"))]
fn wlr_randr_bounds() -> Option<Bounds> {
    let out = Command::new("wlr-randr").arg("--json").output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    union_of(v.as_array()?.iter().filter_map(|m| {
        let pos = m.get("position")?;
        let mode = m.get("modes")?.as_array()?.iter().find(|md| {
            md.get("current").and_then(|c| c.as_bool()).unwrap_or(false)
        })?;
        Some((
            pos.get("x")?.as_i64()? as i32,
            pos.get("y")?.as_i64()? as i32,
            mode.get("width")?.as_i64()? as i32,
            mode.get("height")?.as_i64()? as i32,
        ))
    }))
}

/// X11 fallback: the `current 3840 x 1080` line already IS the desktop union.
#[cfg(not(target_os = "macos"))]
fn xrandr_bounds() -> Option<Bounds> {
    let out = Command::new("xrandr").arg("--query").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let caps = regex::Regex::new(r"current\s+(\d+)\s*x\s*(\d+)").ok()?.captures(&text)?;
    Bounds::sane(caps[1].parse().ok()?, caps[2].parse().ok()?)
}

/// Bounding box of monitor rects, assuming the desktop starts at 0,0.
#[cfg(not(target_os = "macos"))]
fn union_of(rects: impl Iterator<Item = (i32, i32, i32, i32)>) -> Option<Bounds> {
    let (mut w, mut h) = (0, 0);
    for (x, y, mw, mh) in rects {
        w = w.max(x + mw);
        h = h.max(y + mh);
    }
    Bounds::sane(w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_the_cursor_on_screen() {
        let b = Bounds { w: 1920, h: 1080 };
        assert_eq!(b.clamp(-5, 99999), (0, 1079));
        assert_eq!(b.clamp(1919, 0), (1919, 0));
    }

    #[test]
    fn rejects_nonsense_sizes() {
        assert!(Bounds::sane(0, 1080).is_none());
        assert!(Bounds::sane(-1, -1).is_none());
        assert!(Bounds::sane(1920, 1080).is_some());
    }

    #[test]
    fn env_override_parses_wxh() {
        // Serialized by nothing — but this test only reads what it sets, and no other test
        // touches REMOAT_SCREEN.
        std::env::set_var("REMOAT_SCREEN", "2560x1440");
        assert_eq!(env_override(), Some(Bounds { w: 2560, h: 1440 }));
        std::env::set_var("REMOAT_SCREEN", "garbage");
        assert_eq!(env_override(), None);
        std::env::remove_var("REMOAT_SCREEN");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn union_spans_side_by_side_monitors() {
        let rects = vec![(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];
        assert_eq!(union_of(rects.into_iter()), Some(Bounds { w: 4480, h: 1440 }));
    }
}
