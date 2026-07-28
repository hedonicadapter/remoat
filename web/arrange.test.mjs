// node --test web/
import test from "node:test";
import assert from "node:assert/strict";

import {
  DEFAULT_W, PUSH, advance, center, entryPoint, fitTransform, inside, localOf, neighbor,
  placeNew, rectsFor, snapRect, stepPointer,
} from "./arrange.js";

const mac = { id: "mac", w: 1800, h: 1169 };
const nix = { id: "nixos", w: 2560, h: 1440 };

// mac at origin, nixos abutting its right edge
const side = () => rectsFor([mac, nix], { mac: { x: 0, y: 0 }, nixos: { x: 1800, y: 0 } });

// Keep dragging until something happens (or we give up) — asserting on the frame the
// crossing occurs, not several frames later once the pointer has moved on.
const walk = (state, rects, dx, dy, steps) => {
  let s = state;
  for (let i = 0; i < steps; i++) {
    s = stepPointer(s, dx, dy, rects);
    if (s.crossed || s.blocked) break;
  }
  return s;
};

test("unplaced devices land to the right of what's already placed", () => {
  const rects = rectsFor([mac, nix], { mac: { x: 0, y: 0 } });
  assert.equal(rects[1].x, 1800);
  assert.equal(rects[1].y, 0);
  assert.deepEqual(placeNew([]), { x: 0, y: 0 });
});

test("a device with unknown bounds still gets an arrangeable rect", () => {
  const rects = rectsFor([{ id: "ghost", w: 0, h: 0 }], {});
  assert.equal(rects[0].w, DEFAULT_W);
  assert.equal(rects[0].known, false);
});

test("moving inside a device never crosses", () => {
  const rects = side();
  const s = walk({ id: "mac", gx: 900, gy: 500, overshoot: 0 }, rects, 10, 0, 20);
  assert.equal(s.id, "mac");
  assert.equal(s.crossed, false);
  assert.equal(s.gx, 1100);
});

test("hitting an edge clamps and banks the shove instead of crossing immediately", () => {
  const rects = side();
  const s = stepPointer({ id: "mac", gx: 1799, gy: 500, overshoot: 0 }, 20, 0, rects);
  assert.equal(s.crossed, false, "one nudge must not hand over the pointer");
  assert.equal(s.gx, 1799, "pointer stays pinned to the edge");
  assert.ok(s.overshoot > 0);
});

test("a sustained shove crosses to the bordering device, at the same height", () => {
  const rects = side();
  let s = { id: "mac", gx: 1700, gy: 640, overshoot: 0 };
  s = walk(s, rects, 30, 0, 8); // ~140px of it spent pushing past the edge
  assert.equal(s.crossed, true);
  assert.equal(s.id, "nixos");
  assert.equal(s.gx, 1800, "enters on the neighbour's near edge");
  assert.equal(s.gy, 640, "same global height it left at");
  assert.equal(s.from, "mac");
  assert.equal(s.overshoot, 0, "bank resets after a crossing");
});

test("crossing back the other way returns to the first device", () => {
  const rects = side();
  let s = walk({ id: "mac", gx: 1700, gy: 640, overshoot: 0 }, rects, 30, 0, 8);
  assert.equal(s.id, "nixos");
  s = walk(s, rects, -30, 0, 8);
  assert.equal(s.id, "mac");
  assert.equal(s.gx, 1799, "lands on mac's right edge");
});

test("no device in that direction: pointer stays put, bank stops growing", () => {
  const rects = side();
  const s = walk({ id: "mac", gx: 100, gy: 500, overshoot: 0 }, rects, -50, 0, 10);
  assert.equal(s.id, "mac");
  assert.equal(s.crossed, false);
  assert.equal(s.gx, 0);
  assert.equal(s.overshoot, PUSH, "banked shove is capped, so leaving later needs a fresh push");
});

test("a neighbour that doesn't span the exit point can't catch the pointer", () => {
  // nixos is to the right but dropped 1000px down: it spans y 1000..2440 only.
  const rects = rectsFor([mac, nix], { mac: { x: 0, y: 0 }, nixos: { x: 1800, y: 1000 } });
  const high = walk({ id: "mac", gx: 1700, gy: 100, overshoot: 0 }, rects, 40, 0, 10);
  assert.equal(high.crossed, false, "leaving above the neighbour hits nothing");
  assert.equal(high.id, "mac");
  // ...but leaving where they do overlap works
  const low = walk({ id: "mac", gx: 1700, gy: 1100, overshoot: 0 }, rects, 40, 0, 10);
  assert.equal(low.crossed, true);
  assert.equal(low.id, "nixos");
  assert.equal(low.gy, 1100);
});

test("crossing into a device with unknown bounds is refused, not guessed", () => {
  const rects = rectsFor([mac, { id: "ghost", w: 0, h: 0 }], {
    mac: { x: 0, y: 0 }, ghost: { x: 1800, y: 0 },
  });
  const s = walk({ id: "mac", gx: 1700, gy: 500, overshoot: 0 }, rects, 40, 0, 10);
  assert.equal(s.crossed, false);
  assert.equal(s.blocked, true);
  assert.equal(s.blockedId, "ghost");
  assert.equal(s.id, "mac", "input keeps going to the device we can actually drive");
});

test("vertical arrangements cross top/bottom", () => {
  const rects = rectsFor([mac, nix], { mac: { x: 0, y: 0 }, nixos: { x: 0, y: 1169 } });
  const s = walk({ id: "mac", gx: 500, gy: 1100, overshoot: 0 }, rects, 0, 30, 8);
  assert.equal(s.id, "nixos");
  assert.equal(s.gy, 1169);
  const back = walk(s, rects, 0, -30, 8);
  assert.equal(back.id, "mac");
});

test("gaps between devices are still crossable", () => {
  const rects = rectsFor([mac, nix], { mac: { x: 0, y: 0 }, nixos: { x: 2400, y: 0 } });
  const s = walk({ id: "mac", gx: 1700, gy: 300, overshoot: 0 }, rects, 40, 0, 10);
  assert.equal(s.id, "nixos");
  assert.equal(s.gx, 2400);
});

test("losing the active device falls back to one that exists", () => {
  const rects = side();
  const s = stepPointer({ id: "vanished", gx: 5, gy: 5, overshoot: 0 }, 1, 0, rects);
  assert.equal(s.id, "mac");
  assert.equal(s.crossed, true);
  assert.deepEqual({ gx: s.gx, gy: s.gy }, center(rects[0]));
});

test("global coords convert to the device's own pixels", () => {
  const rects = side();
  assert.deepEqual(localOf(rects, "nixos", 1800, 640), { x: 0, y: 640 });
  assert.deepEqual(localOf(rects, "mac", 1799, 0), { x: 1799, y: 0 });
  assert.deepEqual(localOf(rects, "nixos", 99999, 99999), { x: 2559, y: 1439 });
});

test("dragging snaps to abut and to align", () => {
  const others = [{ id: "mac", x: 0, y: 0, w: 1800, h: 1169 }];
  const dragged = { id: "nixos", x: 1840, y: 30, w: 2560, h: 1440 };
  assert.deepEqual(snapRect(dragged, others), { x: 1800, y: 0 });
});

test("snapping leaves a deliberately distant rect alone", () => {
  const others = [{ id: "mac", x: 0, y: 0, w: 1800, h: 1169 }];
  const dragged = { id: "nixos", x: 2400, y: 600, w: 2560, h: 1440 };
  assert.deepEqual(snapRect(dragged, others), { x: 2400, y: 600 });
});

test("fitTransform maps the whole arrangement into the panel", () => {
  const rects = side();
  const { scale, ox, oy } = fitTransform(rects, 300, 150, 10);
  const projected = rects.map((r) => ({
    l: r.x * scale + ox, t: r.y * scale + oy,
    r: (r.x + r.w) * scale + ox, b: (r.y + r.h) * scale + oy,
  }));
  for (const p of projected) {
    assert.ok(p.l >= 9.9 && p.r <= 290.1, `x within panel: ${p.l}..${p.r}`);
    assert.ok(p.t >= 9.9 && p.b <= 140.1, `y within panel: ${p.t}..${p.b}`);
  }
  assert.ok(scale > 0 && scale < 1);
});

test("advance sends the pointer to the device it's on", () => {
  const rects = side();
  const a = advance({ id: "mac", gx: 900, gy: 500, overshoot: 0 }, 10, 5, rects);
  assert.deepEqual(a.send, { id: "mac", x: 910, y: 505, home: false });
  assert.equal(a.crossed, false);
});

test("advance lands a crossing on the entry edge of the new device, and homes", () => {
  // Regression guard: the landing point must survive to the wire. An earlier version
  // re-centred the pointer when the active device changed, so crossings arrived mid-screen.
  const rects = side();
  let s = { id: "mac", gx: 1700, gy: 640, overshoot: 0 };
  let a;
  for (let i = 0; i < 8; i++) {
    a = advance(s, 30, 0, rects);
    s = a.state;
    if (a.crossed) break;
  }
  assert.equal(a.crossed, true);
  assert.deepEqual(a.send, { id: "nixos", x: 0, y: 640, home: true },
    "enters at the neighbour's left edge, same height, asking the daemon to re-home");
});

test("advance sends nothing to a device whose bounds are unknown", () => {
  const rects = rectsFor([mac, { id: "ghost", w: 0, h: 0 }], {
    mac: { x: 0, y: 0 }, ghost: { x: 1800, y: 0 },
  });
  let s = { id: "mac", gx: 1700, gy: 500, overshoot: 0 };
  let a;
  for (let i = 0; i < 10; i++) { a = advance(s, 40, 0, rects); s = a.state; if (a.blocked) break; }
  assert.equal(a.blocked, true);
  assert.equal(a.send.id, "mac", "input keeps going to the device we can drive");
});

test("helpers", () => {
  const r = { id: "a", x: 10, y: 10, w: 100, h: 50 };
  assert.equal(inside(r, 10, 10), true);
  assert.equal(inside(r, 110, 10), false);
  assert.deepEqual(center(r), { gx: 60, gy: 35 });
  assert.equal(neighbor([r], r, "right", 20), null);
  assert.deepEqual(entryPoint(r, "right", 0, 5), { gx: 10, gy: 10 });
});
