//! Screen capture — a PNG crop of the desktop around the cursor, streamed to the phone while
//! dragging so the touchpad background shows what's actually there instead of a placeholder.
//!
//! Same pattern as `input.rs`: shell out to a platform tool rather than link a capture library.
//!   macOS         -> `screencapture` (built in; needs Screen Recording permission granted to
//!                     the launching terminal on Catalina+, same story as Accessibility for
//!                     enigo — see CLAUDE.md gotchas).
//!   Linux/Wayland -> `grim` (wlr-screencopy; works on Hyprland and other wlroots compositors,
//!                     NOT under a plain XDG portal flow — no permission dialog per frame).
//! Neither backend has been exercised against real hardware from this dev environment: only
//! the region math and argv construction are unit-tested here (the same honesty as the
//! ydotool dry-run caveat already documented for input).

use std::process::Command;

use crate::screen::Bounds;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Crop size around the cursor. `REMOAT_CAPTURE_SIZE=WxH` overrides.
const DEFAULT_W: i32 = 480;
const DEFAULT_H: i32 = 300;

fn capture_size() -> (i32, i32) {
    if let Ok(v) = std::env::var("REMOAT_CAPTURE_SIZE") {
        if let Some((w, h)) = v.split_once(['x', 'X']) {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<i32>(), h.trim().parse::<i32>()) {
                if w > 0 && h > 0 {
                    return (w, h);
                }
            }
        }
    }
    (DEFAULT_W, DEFAULT_H)
}

/// Region of `size` centered on (cx, cy), clamped inside `bounds` — and shrunk to fit a
/// desktop smaller than the requested crop.
pub fn region_around(cx: i32, cy: i32, size: (i32, i32), bounds: Bounds) -> Rect {
    let w = size.0.min(bounds.w);
    let h = size.1.min(bounds.h);
    let x = (cx - w / 2).clamp(0, bounds.w - w);
    let y = (cy - h / 2).clamp(0, bounds.h - h);
    Rect { x, y, w, h }
}

/// Capture the region around (cx, cy). `None` means the shell-out failed, wasn't available, or
/// this call was a dry run — the caller just skips sending a frame.
pub fn capture_around(cx: i32, cy: i32, bounds: Bounds, dryrun: bool) -> Option<Vec<u8>> {
    let r = region_around(cx, cy, capture_size(), bounds);
    platform_capture(r, dryrun)
}

#[cfg(target_os = "macos")]
fn platform_capture(r: Rect, dryrun: bool) -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("remoat-cap-{}.png", std::process::id()));
    let region = format!("{},{},{},{}", r.x, r.y, r.w, r.h);
    if dryrun {
        println!("[dryrun] screencapture -x -t png -R{region} {}", path.display());
        return None;
    }
    let ok = Command::new("screencapture")
        .args(["-x", "-t", "png", &format!("-R{region}")])
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    bytes
}

#[cfg(not(target_os = "macos"))]
fn platform_capture(r: Rect, dryrun: bool) -> Option<Vec<u8>> {
    let geom = format!("{},{} {}x{}", r.x, r.y, r.w, r.h);
    if dryrun {
        println!("[dryrun] grim -g \"{geom}\" -");
        return None;
    }
    let out = Command::new("grim").args(["-g", &geom, "-"]).output().ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    const B: Bounds = Bounds { w: 1920, h: 1080 };

    #[test]
    fn centers_on_the_cursor() {
        assert_eq!(region_around(960, 540, (480, 300), B), Rect { x: 720, y: 390, w: 480, h: 300 });
    }

    #[test]
    fn clamps_at_the_edges() {
        assert_eq!(region_around(0, 0, (480, 300), B), Rect { x: 0, y: 0, w: 480, h: 300 });
        assert_eq!(region_around(1919, 1079, (480, 300), B), Rect { x: 1440, y: 780, w: 480, h: 300 });
    }

    #[test]
    fn shrinks_to_fit_a_screen_smaller_than_the_crop() {
        let tiny = Bounds { w: 200, h: 100 };
        assert_eq!(region_around(50, 50, (480, 300), tiny), Rect { x: 0, y: 0, w: 200, h: 100 });
    }
}
