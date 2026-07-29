//! Desktop geometry — what the phone needs in order to own an absolute cursor, and what
//! screen capture needs in order to crop the right pixels.
//!
//! Edge crossing between arranged devices only works if someone knows where the cursor IS.
//! Nothing can ask Wayland (uinput is write-only, and there's no protocol for reading the
//! pointer), so the *phone* owns the position: it integrates touch deltas per device and
//! sends `move_abs`. For that it needs each device's bounds, which is this module.
//!
//! Everything here is in the compositor's **logical** (post-scale) coordinate space, because
//! that is the space both pointer positions and `grim -g` live in. Reading *physical* mode
//! sizes and pairing them with logical positions is the bug that made capture crops land off
//! to the side on a mixed-scale setup — a 3840x2160 panel at scale 2 occupies 1920x1080 of
//! layout, and using 3840x2160 inflates the desktop into coordinates no output covers.
//!
//! The union's origin is NOT assumed to be 0,0: a monitor placed left of or above the primary
//! puts it negative. The phone still gets a 0-based `w`x`h`, and `Bounds.ox/oy` translates
//! back when talking to the compositor.
//!
//! `REMOAT_SCREEN=WxH` overrides everything.

// Only the Linux probes shell out; mac reads the display straight from Quartz via enigo.
#[cfg(not(target_os = "macos"))]
use std::process::Command;

/// A rectangle in compositor logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Squared distance from a point to this rect (0 when inside). Used to pick the nearest
    /// output for a cursor that landed in an uncovered gap of the desktop union.
    pub fn dist2(&self, x: i32, y: i32) -> i64 {
        let dx = (self.x - x).max(x - (self.x + self.w - 1)).max(0) as i64;
        let dy = (self.y - y).max(y - (self.y + self.h - 1)).max(0) as i64;
        dx * dx + dy * dy
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    /// Logical origin of the desktop union. Usually 0,0; negative when a monitor sits left of
    /// or above the primary. The phone never sees this — it works in 0..w-1 — but capture does.
    pub ox: i32,
    pub oy: i32,
    pub w: i32,
    pub h: i32,
}

/// The desktop as both one rectangle (what the phone drives a cursor in) and its individual
/// monitors (what capture clamps into, so a crop can't spill into an uncovered gap).
#[derive(Clone, Debug)]
pub struct Layout {
    pub bounds: Bounds,
    pub monitors: Vec<Rect>,
}

/// Anything is better than 0×0 (which would make every edge an edge).
const FALLBACK: Bounds = Bounds::of(1920, 1080);

impl Bounds {
    /// Bounds at the origin — the common case, and what tests want.
    pub const fn of(w: i32, h: i32) -> Bounds {
        Bounds { ox: 0, oy: 0, w, h }
    }

    fn sane(ox: i32, oy: i32, w: i32, h: i32) -> Option<Bounds> {
        (w > 0 && h > 0 && w <= 65535 && h <= 65535).then_some(Bounds { ox, oy, w, h })
    }

    /// Keep a client's absolute move inside the desktop — a bad or stale client must not be
    /// able to park the cursor somewhere unreachable. Desktop-local (0-based), as sent.
    pub fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        (x.clamp(0, self.w - 1), y.clamp(0, self.h - 1))
    }

    /// The union as a logical rect (origin included).
    pub fn rect(&self) -> Rect {
        Rect { x: self.ox, y: self.oy, w: self.w, h: self.h }
    }
}

impl Layout {
    fn single(b: Bounds) -> Layout {
        Layout { bounds: b, monitors: vec![b.rect()] }
    }
}

pub fn detect() -> Layout {
    let found = env_override().map(Layout::single).or_else(platform_layout);
    match found {
        Some(l) => {
            eprintln!(
                "[screen] {}x{} at {},{} ({} monitor{})",
                l.bounds.w,
                l.bounds.h,
                l.bounds.ox,
                l.bounds.oy,
                l.monitors.len(),
                if l.monitors.len() == 1 { "" } else { "s" }
            );
            // Logged per monitor because a wrong crop is almost always a wrong rect here, and
            // this is the cheapest way for someone on real hardware to see which one.
            for m in &l.monitors {
                eprintln!("[screen]   logical {}x{} at {},{}", m.w, m.h, m.x, m.y);
            }
            l
        }
        None => {
            eprintln!(
                "[screen] could not detect size — assuming {}x{} (set REMOAT_SCREEN=WxH)",
                FALLBACK.w, FALLBACK.h
            );
            Layout::single(FALLBACK)
        }
    }
}

fn env_override() -> Option<Bounds> {
    let v = std::env::var("REMOAT_SCREEN").ok()?;
    let (w, h) = v.split_once(['x', 'X'])?;
    Bounds::sane(0, 0, w.trim().parse().ok()?, h.trim().parse().ok()?)
}

#[cfg(target_os = "macos")]
fn platform_layout() -> Option<Layout> {
    use enigo::{Enigo, Mouse, Settings};
    // Throwaway instance: main_display() is a Quartz query, not an injection, so this works
    // even without Accessibility granted. Quartz reports points, which already are logical.
    let enigo = Enigo::new(&Settings::default()).ok()?;
    let (w, h) = enigo.main_display().ok()?;
    Some(Layout::single(Bounds::sane(0, 0, w, h)?))
}

#[cfg(not(target_os = "macos"))]
fn platform_layout() -> Option<Layout> {
    niri_layout().or_else(hyprland_layout).or_else(wlr_randr_layout).or_else(xrandr_layout)
}

/// niri: `niri msg --json outputs` reports a `logical` rect per output — already post-scale
/// and already positioned in layout space, which is exactly what's wanted, no arithmetic.
#[cfg(not(target_os = "macos"))]
fn niri_layout() -> Option<Layout> {
    let out = Command::new("niri").args(["msg", "--json", "outputs"]).output().ok()?;
    parse_niri(&serde_json::from_slice(&out.stdout).ok()?)
}

/// Split from the shell-out so the parsing — the part that actually had the bug — is testable
/// without the compositor. Same reason `keyargs` is split out in input.rs.
#[cfg(not(target_os = "macos"))]
fn parse_niri(v: &serde_json::Value) -> Option<Layout> {
    // Keyed by output name on current niri; tolerate a bare array in case that changes.
    let items: Vec<&serde_json::Value> = match v {
        serde_json::Value::Object(m) => m.values().collect(),
        serde_json::Value::Array(a) => a.iter().collect(),
        _ => return None,
    };
    layout_of(items.into_iter().filter_map(|o| {
        let l = o.get("logical")?;
        Some(Rect {
            x: l.get("x")?.as_i64()? as i32,
            y: l.get("y")?.as_i64()? as i32,
            w: l.get("width")?.as_i64()? as i32,
            h: l.get("height")?.as_i64()? as i32,
        })
    }))
}

/// Hyprland: `hyprctl monitors -j` positions in logical coordinates but reports `width`/
/// `height` as the *physical* mode, so both must be divided by `scale` to get the logical
/// rect. Mixing the two is what broke capture on a scale-2 monitor.
#[cfg(not(target_os = "macos"))]
fn hyprland_layout() -> Option<Layout> {
    let out = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()?;
    parse_hyprland(&serde_json::from_slice(&out.stdout).ok()?)
}

#[cfg(not(target_os = "macos"))]
fn parse_hyprland(v: &serde_json::Value) -> Option<Layout> {
    layout_of(v.as_array()?.iter().filter_map(|m| {
        let scale = m.get("scale").and_then(|s| s.as_f64()).filter(|s| *s > 0.0).unwrap_or(1.0);
        let pw = m.get("width")?.as_i64()? as f64;
        let ph = m.get("height")?.as_i64()? as f64;
        // transform is 0-7; the odd values are the 90°/270° rotations, which swap w/h.
        let rotated = m.get("transform").and_then(|t| t.as_i64()).unwrap_or(0) % 2 == 1;
        let (pw, ph) = if rotated { (ph, pw) } else { (pw, ph) };
        Some(Rect {
            x: m.get("x")?.as_i64()? as i32,
            y: m.get("y")?.as_i64()? as i32,
            w: (pw / scale).round() as i32,
            h: (ph / scale).round() as i32,
        })
    }))
}

/// wlr-randr: same shape as Hyprland — logical `position`, physical current `mode`, separate
/// `scale`. Same division, same reason.
#[cfg(not(target_os = "macos"))]
fn wlr_randr_layout() -> Option<Layout> {
    let out = Command::new("wlr-randr").arg("--json").output().ok()?;
    parse_wlr_randr(&serde_json::from_slice(&out.stdout).ok()?)
}

#[cfg(not(target_os = "macos"))]
fn parse_wlr_randr(v: &serde_json::Value) -> Option<Layout> {
    layout_of(v.as_array()?.iter().filter_map(|m| {
        if m.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
            return None;
        }
        let pos = m.get("position")?;
        let mode = m
            .get("modes")?
            .as_array()?
            .iter()
            .find(|md| md.get("current").and_then(|c| c.as_bool()).unwrap_or(false))?;
        let scale = m.get("scale").and_then(|s| s.as_f64()).filter(|s| *s > 0.0).unwrap_or(1.0);
        let pw = mode.get("width")?.as_i64()? as f64;
        let ph = mode.get("height")?.as_i64()? as f64;
        // wlr-randr spells transform as a string ("90", "flipped-270", …).
        let t = m.get("transform").and_then(|t| t.as_str()).unwrap_or("normal");
        let rotated = t.contains("90") || t.contains("270");
        let (pw, ph) = if rotated { (ph, pw) } else { (pw, ph) };
        Some(Rect {
            x: pos.get("x")?.as_i64()? as i32,
            y: pos.get("y")?.as_i64()? as i32,
            w: (pw / scale).round() as i32,
            h: (ph / scale).round() as i32,
        })
    }))
}

/// X11 fallback: the `current 3840 x 1080` line already IS the desktop union, in pixels, at
/// the origin — X has no per-output scale to undo.
#[cfg(not(target_os = "macos"))]
fn xrandr_layout() -> Option<Layout> {
    let out = Command::new("xrandr").arg("--query").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let caps = regex::Regex::new(r"current\s+(\d+)\s*x\s*(\d+)").ok()?.captures(&text)?;
    Some(Layout::single(Bounds::sane(0, 0, caps[1].parse().ok()?, caps[2].parse().ok()?)?))
}

/// Monitor rects -> the layout: their bounding box plus the rects themselves. The box may have
/// a negative origin; capture needs that, the phone works 0-based off `w`/`h`.
fn layout_of(rects: impl Iterator<Item = Rect>) -> Option<Layout> {
    let monitors: Vec<Rect> = rects.filter(|r| r.w > 0 && r.h > 0).collect();
    if monitors.is_empty() {
        return None;
    }
    let ox = monitors.iter().map(|r| r.x).min()?;
    let oy = monitors.iter().map(|r| r.y).min()?;
    let right = monitors.iter().map(|r| r.x + r.w).max()?;
    let bottom = monitors.iter().map(|r| r.y + r.h).max()?;
    Some(Layout { bounds: Bounds::sane(ox, oy, right - ox, bottom - oy)?, monitors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_the_cursor_on_screen() {
        let b = Bounds::of(1920, 1080);
        assert_eq!(b.clamp(-5, 99999), (0, 1079));
        assert_eq!(b.clamp(1919, 0), (1919, 0));
    }

    #[test]
    fn rejects_nonsense_sizes() {
        assert!(Bounds::sane(0, 0, 0, 1080).is_none());
        assert!(Bounds::sane(0, 0, -1, -1).is_none());
        assert!(Bounds::sane(0, 0, 1920, 1080).is_some());
    }

    #[test]
    fn env_override_parses_wxh() {
        // Serialized by nothing — but this test only reads what it sets, and no other test
        // touches REMOAT_SCREEN.
        std::env::set_var("REMOAT_SCREEN", "2560x1440");
        assert_eq!(env_override(), Some(Bounds::of(2560, 1440)));
        std::env::set_var("REMOAT_SCREEN", "garbage");
        assert_eq!(env_override(), None);
        std::env::remove_var("REMOAT_SCREEN");
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn union_spans_side_by_side_monitors() {
        let l = layout_of([rect(0, 0, 1920, 1080), rect(1920, 0, 2560, 1440)].into_iter()).unwrap();
        assert_eq!(l.bounds, Bounds::of(4480, 1440));
        assert_eq!(l.monitors.len(), 2);
    }

    #[test]
    fn union_keeps_a_negative_origin() {
        // A monitor above/left of the primary puts the layout origin negative. Assuming 0,0
        // here would shift every capture crop by the whole offset.
        let l = layout_of([rect(0, 0, 1920, 1080), rect(-2560, -400, 2560, 1440)].into_iter()).unwrap();
        assert_eq!(l.bounds, Bounds { ox: -2560, oy: -400, w: 4480, h: 1480 });
    }

    #[test]
    fn empty_or_degenerate_monitor_lists_are_rejected() {
        assert!(layout_of(std::iter::empty()).is_none());
        assert!(layout_of([rect(0, 0, 0, 0)].into_iter()).is_none());
    }

    // The mixed-scale case that broke capture on real hardware: a laptop panel at scale 1
    // beside a 4K external at scale 2. The external occupies 1920x1080 of LAYOUT, not
    // 3840x2160 — reading the physical mode inflated the desktop into coordinates no output
    // covers, so grim returned black bars and every crop sat off to the side.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn niri_reads_logical_rects_verbatim() {
        // niri keys outputs by name and reports a `logical` rect that is already post-scale.
        let v: serde_json::Value = serde_json::from_str(
            r#"{
              "eDP-1":  {"name":"eDP-1",
                         "logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1.0}},
              "DP-2":   {"name":"DP-2",
                         "logical":{"x":1920,"y":0,"width":1920,"height":1080,"scale":2.0}}
            }"#,
        )
        .unwrap();
        let l = parse_niri(&v).unwrap();
        assert_eq!(l.bounds, Bounds::of(3840, 1080));
        assert_eq!(l.monitors.len(), 2);
        assert!(l.monitors.contains(&rect(1920, 0, 1920, 1080)), "scale-2 output must be logical");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn hyprland_divides_the_physical_mode_by_scale() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[
              {"name":"eDP-1","x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":0},
              {"name":"DP-2","x":1920,"y":0,"width":3840,"height":2160,"scale":2.0,"transform":0}
            ]"#,
        )
        .unwrap();
        let l = parse_hyprland(&v).unwrap();
        assert_eq!(l.bounds, Bounds::of(3840, 1080), "physical mode leaked into the layout");
        assert!(l.monitors.contains(&rect(1920, 0, 1920, 1080)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn hyprland_swaps_axes_on_a_rotated_monitor() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"name":"DP-1","x":0,"y":0,"width":2560,"height":1440,"scale":1.0,"transform":1}]"#,
        )
        .unwrap();
        assert_eq!(parse_hyprland(&v).unwrap().bounds, Bounds::of(1440, 2560));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn wlr_randr_uses_the_current_mode_over_scale_and_skips_disabled() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[
              {"name":"eDP-1","enabled":true,"position":{"x":0,"y":0},"scale":1.0,
               "transform":"normal",
               "modes":[{"width":1280,"height":720,"current":false},
                        {"width":1920,"height":1080,"current":true}]},
              {"name":"DP-2","enabled":true,"position":{"x":1920,"y":0},"scale":2.0,
               "transform":"normal",
               "modes":[{"width":3840,"height":2160,"current":true}]},
              {"name":"HDMI-1","enabled":false,"position":{"x":9999,"y":0},"scale":1.0,
               "transform":"normal","modes":[{"width":1920,"height":1080,"current":true}]}
            ]"#,
        )
        .unwrap();
        let l = parse_wlr_randr(&v).unwrap();
        assert_eq!(l.bounds, Bounds::of(3840, 1080));
        assert_eq!(l.monitors.len(), 2, "a disabled output must not stretch the desktop");
    }
}
