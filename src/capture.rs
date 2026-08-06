//! Screen capture — a PNG crop of the desktop around the cursor, streamed to the phone while
//! dragging so the touchpad background shows what's actually there instead of a placeholder.
//!
//! Same pattern as `input.rs`: shell out to a platform tool rather than link a capture library.
//!   macOS         -> `screencapture` (built in; needs Screen Recording permission — CONFIRMED
//!                     to fail loud, not silently, and granted to the *responsible process*,
//!                     which over ssh is sshd rather than Terminal — same story as Accessibility
//!                     for enigo. See CLAUDE.md gotchas).
//!   Linux/Wayland -> `grim` (wlr-screencopy; works on niri, Hyprland and other wlroots-style
//!                     compositors, NOT a portal flow, so no permission dialog per frame).
//!
//! Two things make the crop land where the cursor actually is, both learned the hard way:
//!   - The region is in the compositor's **logical** coordinate space (see screen.rs). grim's
//!     `-g` wants logical coords; feeding it physical ones offsets every crop on a scaled
//!     monitor.
//!   - The region is clamped into **the monitor under the cursor**, not the desktop union. A
//!     union is not necessarily fully covered by outputs — mixed sizes or a non-grid
//!     arrangement leave gaps, and grim renders anything uncovered as black bars.

use std::process::Command;

use crate::screen::{Layout, Rect};

/// Crop size around the cursor at zoom 1.0, in logical pixels. `REMOAT_CAPTURE_SIZE=WxH`
/// overrides.
const DEFAULT_W: i32 = 480;
const DEFAULT_H: i32 = 300;

/// Pinch-to-zoom range. Above 1 shows more desktop (a bigger region, scaled down into the same
/// on-screen box by the client's `object-fit: cover`); below 1 shows less, for precision work.
/// Clamped here too, not just client-side — a client is what decides the number, but the
/// daemon is what turns it into a shell-out, so it re-validates rather than trusting the wire.
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 4.0;

pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(ZOOM_MIN, ZOOM_MAX)
    } else {
        1.0
    }
}

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

/// Region of `size` centered on a desktop-local cursor position, translated into compositor
/// logical coordinates and confined to whichever monitor holds the cursor. `zoom` scales the
/// base crop size before any of that — everything downstream (the monitor clamp, the
/// shrink-to-fit) already handles an oversized request, since it was already built for a
/// crop bigger than a small or mismatched monitor.
pub fn region_around_zoomed(cx: i32, cy: i32, base: (i32, i32), zoom: f32, layout: &Layout) -> Rect {
    let z = clamp_zoom(zoom);
    let size = (((base.0 as f32) * z).round() as i32, ((base.1 as f32) * z).round() as i32);
    region_around(cx, cy, size, layout)
}

/// Region of `size` centered on a desktop-local cursor position, translated into compositor
/// logical coordinates and confined to whichever monitor holds the cursor.
pub fn region_around(cx: i32, cy: i32, size: (i32, i32), layout: &Layout) -> Rect {
    // Phone coords are 0-based over the union; the compositor's are not.
    let gx = cx + layout.bounds.ox;
    let gy = cy + layout.bounds.oy;
    // Confining the crop to one output is what guarantees every pixel is covered. The cursor
    // CAN sit outside every monitor — the phone drives it over the union, and a union of
    // mismatched monitors has uncovered gaps — so fall back to the nearest output rather than
    // to the union, which would put the black bars right back.
    let host = layout
        .monitors
        .iter()
        .find(|m| m.contains(gx, gy))
        .or_else(|| layout.monitors.iter().min_by_key(|m| m.dist2(gx, gy)))
        .copied()
        .unwrap_or_else(|| layout.bounds.rect());
    let w = size.0.min(host.w);
    let h = size.1.min(host.h);
    Rect {
        x: (gx - w / 2).clamp(host.x, host.x + host.w - w),
        y: (gy - h / 2).clamp(host.y, host.y + host.h - h),
        w,
        h,
    }
}

/// Capture the region around a desktop-local (cx, cy) at the given zoom. `None` means the
/// shell-out failed, the tool was missing, or this was a dry run — the caller just skips
/// sending a frame.
pub fn capture_around(cx: i32, cy: i32, zoom: f32, layout: &Layout, dryrun: bool) -> Option<Vec<u8>> {
    platform_capture(region_around_zoomed(cx, cy, capture_size(), zoom, layout), dryrun)
}

#[cfg(target_os = "macos")]
fn platform_capture(r: Rect, dryrun: bool) -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("remoat-cap-{}.png", std::process::id()));
    let region = format!("{},{},{},{}", r.x, r.y, r.w, r.h);
    // -C draws the cursor in, same reason as grim's -c below.
    if dryrun {
        println!("[dryrun] screencapture -x -C -t png -R{region} {}", path.display());
        return None;
    }
    let out = Command::new("screencapture")
        .args(["-x", "-C", "-t", "png", &format!("-R{region}")])
        .arg(&path)
        .output();
    let ok = match &out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    };
    if !ok {
        // screencapture writes its own error to stderr on every single failed attempt — at
        // one call per move_abs (throttled to CAPTURE_INTERVAL, ~8/sec while dragging) that
        // floods the terminal instead of explaining anything. Say it once, clearly, and go
        // quiet: this is almost always Screen Recording permission not granted to whatever
        // process actually launched the daemon (Terminal, or — over ssh — sshd itself; see
        // CLAUDE.md gotchas).
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            let detail = out
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                .filter(|s| !s.is_empty());
            eprintln!(
                "[capture] screencapture failed{} — grant Screen Recording (System Settings > \
                 Privacy & Security) to whatever process launched this daemon; over ssh that's \
                 sshd, not Terminal. Not repeating this per frame.",
                detail.map(|d| format!(": {d}")).unwrap_or_default()
            );
        });
        return None;
    }
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// `-c` draws the real cursor into the capture. That is not cosmetic: on Linux the daemon can
/// only *estimate* where the pointer is (see input.rs — no absolute injection, no way to read
/// the pointer on Wayland), so the crop is centered on an estimate. Rendering the cursor makes
/// any error visible and self-explanatory instead of looking like the wrong region, and when
/// the estimate is exact the cursor sits dead centre — which doubles as the check that it is.
/// Split from the call so the argv is testable, same reason as `input.rs::keyargs`.
#[cfg(not(target_os = "macos"))]
fn grim_args(r: Rect) -> Vec<String> {
    vec!["-c".into(), "-g".into(), format!("{},{} {}x{}", r.x, r.y, r.w, r.h), "-".into()]
}

#[cfg(not(target_os = "macos"))]
fn platform_capture(r: Rect, dryrun: bool) -> Option<Vec<u8>> {
    let args = grim_args(r);
    if dryrun {
        println!("[dryrun] grim {}", args.join(" "));
        return None;
    }
    let out = Command::new("grim").args(&args).output().ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Bounds;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    fn layout(bounds: Bounds, monitors: Vec<Rect>) -> Layout {
        Layout { bounds, monitors }
    }
    fn single(w: i32, h: i32) -> Layout {
        let b = Bounds::of(w, h);
        layout(b, vec![b.rect()])
    }

    #[test]
    fn centers_on_the_cursor() {
        assert_eq!(
            region_around(960, 540, (480, 300), &single(1920, 1080)),
            rect(720, 390, 480, 300)
        );
    }

    #[test]
    fn clamps_at_the_edges() {
        let l = single(1920, 1080);
        assert_eq!(region_around(0, 0, (480, 300), &l), rect(0, 0, 480, 300));
        assert_eq!(region_around(1919, 1079, (480, 300), &l), rect(1440, 780, 480, 300));
    }

    #[test]
    fn shrinks_to_fit_a_screen_smaller_than_the_crop() {
        assert_eq!(region_around(50, 50, (480, 300), &single(200, 100)), rect(0, 0, 200, 100));
    }

    #[test]
    fn stays_inside_the_monitor_under_the_cursor() {
        // Two monitors of different heights: the union has an uncovered strip below the short
        // one. A crop near their seam must not reach into it, or grim renders black bars.
        let l = layout(
            Bounds::of(3840, 1440),
            vec![rect(0, 0, 1920, 1080), rect(1920, 0, 1920, 1440)],
        );
        let r = region_around(1900, 1000, (480, 300), &l);
        assert_eq!(r, rect(1440, 780, 480, 300), "crop escaped the left monitor");
        assert!(r.x + r.w <= 1920 && r.y + r.h <= 1080);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn grim_is_asked_for_the_cursor_and_the_exact_region() {
        assert_eq!(
            grim_args(rect(1440, 780, 480, 300)),
            vec!["-c", "-g", "1440,780 480x300", "-"]
        );
    }

    #[test]
    fn a_cursor_in_an_uncovered_gap_snaps_to_the_nearest_output() {
        // The union of mismatched monitors has uncovered space; the phone can drive the cursor
        // into it. Cropping the union there would return black, so the nearest output wins.
        let l = layout(
            Bounds::of(3840, 1440),
            vec![rect(0, 0, 1920, 1080), rect(1920, 0, 1920, 1440)],
        );
        // (500, 1300) is below the short left monitor — inside the union, on no output.
        let r = region_around(500, 1300, (480, 300), &l);
        assert_eq!(r, rect(260, 780, 480, 300));
        assert!(l.monitors[0].contains(r.x, r.y), "crop must land on a real output");
    }

    #[test]
    fn translates_local_coordinates_by_a_negative_origin() {
        // A monitor left of the primary makes the layout origin negative; the phone still
        // sends 0-based coords, so local 0 must land on the real left edge, not on x=0.
        let l = layout(
            Bounds { ox: -1920, oy: 0, w: 3840, h: 1080 },
            vec![rect(-1920, 0, 1920, 1080), rect(0, 0, 1920, 1080)],
        );
        assert_eq!(region_around(0, 540, (480, 300), &l), rect(-1920, 390, 480, 300));
        // Local 1920 is the second monitor's left edge.
        assert_eq!(region_around(1920, 540, (480, 300), &l), rect(0, 390, 480, 300));
    }

    #[test]
    fn zoom_scales_the_base_size_before_the_monitor_clamp() {
        let l = single(1920, 1080);
        assert_eq!(region_around_zoomed(960, 540, (480, 300), 1.0, &l), rect(720, 390, 480, 300));
        // 2x zoom out: double each dimension, still centered.
        assert_eq!(region_around_zoomed(960, 540, (480, 300), 2.0, &l), rect(480, 240, 960, 600));
        // 0.5x zoom in: half each dimension.
        assert_eq!(region_around_zoomed(960, 540, (480, 300), 0.5, &l), rect(720 + 120, 390 + 75, 240, 150));
    }

    #[test]
    fn zoom_is_clamped_both_at_the_edges_and_against_garbage() {
        assert_eq!(clamp_zoom(0.1), ZOOM_MIN);
        assert_eq!(clamp_zoom(50.0), ZOOM_MAX);
        assert_eq!(clamp_zoom(1.0), 1.0);
        // A malformed or absent value from the wire must not panic or produce a 0x0 region.
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
        assert_eq!(clamp_zoom(f32::INFINITY), 1.0);
    }

    #[test]
    fn a_zoomed_out_crop_still_shrinks_to_fit_a_small_monitor() {
        // Zooming out past what a monitor can show must not ask grim/screencapture for a
        // region bigger than the screen — region_around already clamps this; zoom must not
        // bypass it.
        let l = single(600, 400);
        let r = region_around_zoomed(300, 200, (480, 300), 4.0, &l);
        assert_eq!(r, rect(0, 0, 600, 400));
    }
}
