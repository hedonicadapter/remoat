// Device arrangement + edge crossing. Pure geometry, no DOM and no WebSocket, so the part
// that's easy to get subtly wrong is unit-testable (see arrange.test.mjs).
//
// Model: every device is a rect in ONE shared coordinate space, exactly like monitors in a
// display-arrangement pane. The pointer is a single global (gx, gy) in that space; which
// device it's on is just "which rect contains it". Crossing an edge therefore needs no
// mapping maths — the global coordinate is already continuous across the seam, so you come
// out of the neighbour at the height you went in.

/// Assumed size for a device we haven't connected to yet (its bounds are unknown until its
/// daemon says so). Arrangeable, but not crossable — see `known` below.
export const DEFAULT_W = 1920;
export const DEFAULT_H = 1080;

/// How far past an edge you must shove before the pointer leaves the device. Without this,
/// every overshoot at a screen edge would fling you onto the neighbour.
export const PUSH = 60;

/// Drag snapping distance, in device pixels.
export const SNAP = 90;

const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);

export function inside(r, x, y) {
  return x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
}

export function center(r) {
  return { gx: Math.round(r.x + r.w / 2), gy: Math.round(r.y + r.h / 2) };
}

/// Devices + saved layout -> rects. A device with no saved position is appended to the right
/// of everything placed so far, which is what you'd do by hand anyway.
export function rectsFor(devices, layout = {}) {
  const rects = [];
  for (const d of devices) {
    const w = d.w > 0 ? d.w : DEFAULT_W;
    const h = d.h > 0 ? d.h : DEFAULT_H;
    const at = layout[d.id] || placeNew(rects, w, h);
    rects.push({ id: d.id, x: at.x, y: at.y, w, h, known: d.w > 0 && d.h > 0 });
  }
  return rects;
}

export function placeNew(rects, w = DEFAULT_W, h = DEFAULT_H) {
  if (!rects.length) return { x: 0, y: 0 };
  const right = Math.max(...rects.map((r) => r.x + r.w));
  const top = Math.min(...rects.map((r) => r.y));
  return { x: right, y: top };
}

/// Snap a dragged rect to the others: abut facing edges, and align parallel ones. Returns the
/// adjusted position (never mutates).
export function snapRect(rect, others, threshold = SNAP) {
  let { x, y } = rect;
  const near = (a, b) => Math.abs(a - b) <= threshold;
  for (const o of others) {
    if (o.id === rect.id) continue;
    // abut horizontally
    if (near(x, o.x + o.w)) x = o.x + o.w;
    else if (near(x + rect.w, o.x)) x = o.x - rect.w;
    // abut vertically
    if (near(y, o.y + o.h)) y = o.y + o.h;
    else if (near(y + rect.h, o.y)) y = o.y - rect.h;
    // align edges / centres
    if (near(y, o.y)) y = o.y;
    else if (near(y + rect.h, o.y + o.h)) y = o.y + o.h - rect.h;
    if (near(x, o.x)) x = o.x;
    else if (near(x + rect.w, o.x + o.w)) x = o.x + o.w - rect.w;
  }
  return { x: Math.round(x), y: Math.round(y) };
}

/// Which rect lies `dir` of `from` and actually spans the point where the pointer left?
/// Requiring the span to overlap is what makes a diagonal neighbour NOT catch you.
export function neighbor(rects, from, dir, along) {
  const horizontal = dir === "left" || dir === "right";
  const cands = rects.filter((r) => {
    if (r.id === from.id) return false;
    const spans = horizontal
      ? along >= r.y && along < r.y + r.h
      : along >= r.x && along < r.x + r.w;
    if (!spans) return false;
    switch (dir) {
      case "right": return r.x >= from.x + from.w;
      case "left": return r.x + r.w <= from.x;
      case "down": return r.y >= from.y + from.h;
      case "top":
      case "up": return r.y + r.h <= from.y;
      default: return false;
    }
  });
  if (!cands.length) return null;
  const gap = (r) => {
    switch (dir) {
      case "right": return r.x - (from.x + from.w);
      case "left": return from.x - (r.x + r.w);
      case "down": return r.y - (from.y + from.h);
      default: return from.y - (r.y + r.h);
    }
  };
  return cands.reduce((a, b) => (gap(b) < gap(a) ? b : a));
}

/// Where the pointer lands when entering `to` from `dir`: on the facing edge, keeping the
/// global perpendicular coordinate (clamped into the target) so the seam is continuous.
export function entryPoint(to, dir, gx, gy) {
  switch (dir) {
    case "right": return { gx: to.x, gy: clamp(gy, to.y, to.y + to.h - 1) };
    case "left": return { gx: to.x + to.w - 1, gy: clamp(gy, to.y, to.y + to.h - 1) };
    case "down": return { gx: clamp(gx, to.x, to.x + to.w - 1), gy: to.y };
    default: return { gx: clamp(gx, to.x, to.x + to.w - 1), gy: to.y + to.h - 1 };
  }
}

/// Advance the pointer by a delta. Returns the next state plus what happened:
///   crossed  - moved onto another device (`from` says which one it left)
///   blocked  - a neighbour is there but we don't know its bounds yet (not connected)
/// `overshoot` is the banked shove against the current edge; it survives between calls, which
/// is what makes PUSH a deliberate gesture rather than a per-frame threshold.
export function stepPointer(state, dx, dy, rects, opts = {}) {
  const push = opts.push ?? PUSH;
  const cur = rects.find((r) => r.id === state.id);
  if (!cur) {
    // Active device vanished (unplugged, mDNS drop): fall back to the first rect we have.
    const fb = rects[0];
    if (!fb) return { ...state, crossed: false, blocked: false };
    const c = center(fb);
    return { id: fb.id, gx: c.gx, gy: c.gy, overshoot: 0, crossed: true, blocked: false, from: state.id };
  }

  const gx = state.gx + dx;
  const gy = state.gy + dy;
  if (inside(cur, gx, gy)) {
    return { id: cur.id, gx, gy, overshoot: 0, crossed: false, blocked: false };
  }

  // Left the rect: which edge, and by how much?
  const over = {
    left: cur.x - gx,
    right: gx - (cur.x + cur.w - 1),
    up: cur.y - gy,
    down: gy - (cur.y + cur.h - 1),
  };
  const dir = Object.keys(over).reduce((a, b) => (over[b] > over[a] ? b : a));
  const held = {
    gx: clamp(gx, cur.x, cur.x + cur.w - 1),
    gy: clamp(gy, cur.y, cur.y + cur.h - 1),
  };
  const overshoot = state.overshoot + Math.max(0, over[dir]);
  const stay = (extra) => ({ id: cur.id, gx: held.gx, gy: held.gy, overshoot, crossed: false, blocked: false, ...extra });

  if (overshoot < push) return stay();

  const to = neighbor(rects, cur, dir, dir === "left" || dir === "right" ? held.gy : held.gx);
  if (!to) return stay({ overshoot: push }); // edge of the world; don't bank forever
  if (!to.known) return stay({ overshoot: push, blocked: true, blockedId: to.id });

  const e = entryPoint(to, dir, held.gx, held.gy);
  return { id: to.id, gx: e.gx, gy: e.gy, overshoot: 0, crossed: true, blocked: false, from: cur.id, dir };
}

/// Device-space point -> that device's own local coordinates (what move_abs wants).
export function localOf(rects, id, gx, gy) {
  const r = rects.find((x) => x.id === id);
  if (!r) return { x: Math.round(gx), y: Math.round(gy) };
  return {
    x: Math.round(clamp(gx, r.x, r.x + r.w - 1) - r.x),
    y: Math.round(clamp(gy, r.y, r.y + r.h - 1) - r.y),
  };
}

/// Scale + offset that fits every rect inside a panel, for drawing the arrangement.
export function fitTransform(rects, panelW, panelH, pad = 10) {
  if (!rects.length) return { scale: 1, ox: 0, oy: 0 };
  const minX = Math.min(...rects.map((r) => r.x));
  const minY = Math.min(...rects.map((r) => r.y));
  const maxX = Math.max(...rects.map((r) => r.x + r.w));
  const maxY = Math.max(...rects.map((r) => r.y + r.h));
  const bw = Math.max(1, maxX - minX);
  const bh = Math.max(1, maxY - minY);
  const scale = Math.min((panelW - pad * 2) / bw, (panelH - pad * 2) / bh);
  return {
    scale,
    ox: pad + (panelW - pad * 2 - bw * scale) / 2 - minX * scale,
    oy: pad + (panelH - pad * 2 - bh * scale) / 2 - minY * scale,
  };
}
