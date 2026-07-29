// remoat thin client: touchpad + arranged multi-device cursor + push-to-talk.
//
// Devices are arranged like monitors (see arrange.js) and the pointer is ONE cursor in that
// shared space: shove it off an edge and it continues onto whichever device borders that
// edge. That requires knowing where the cursor is, which nothing can ask Wayland — so this
// client owns the position and sends absolute coordinates (`move_abs`) to the device the
// cursor is currently on.
//
// Consequence: we keep a socket open to EVERY device, because a crossing has to be instant.
import {
  advance, center, fitTransform, localOf, rectsFor, snapRect,
} from "./arrange.js";

(() => {
  "use strict";

  const SENS = 1.7;        // pointer sensitivity
  const BAR_SENS = 1.1;    // scroll-bar drag scaling
  const TAP_MS = 250;      // max duration for a tap
  const TAP_SLOP = 10;     // max movement (px) still counted as a tap
  const TILE_TAP_SLOP = 8; // tile drag vs tap
  const LAYOUT_KEY = "remoat.layout";
  const ACTIVE_KEY = "remoat.active";

  const $ = (id) => document.getElementById(id);
  const pad = $("pad"), padcap = $("padcap"), statusDot = $("status"), toast = $("toast");
  const devBtn = $("devbtn"), devName = $("devname"), panel = $("arrange"), stage = $("stage");

  // ---- device pool ----
  // id = the daemon's own name (stable across DHCP churn, unlike host:port).
  const devices = new Map();   // id -> {id, name, platform, host, port, w, h, ws, status}
  let layout = loadLayout();   // id -> {x, y} in the shared device-pixel space
  let activeId = localStorage.getItem(ACTIVE_KEY) || null;
  let originId = null;         // the daemon serving this page
  let cursor = null;           // {id, gx, gy, overshoot}

  function loadLayout() {
    try { return JSON.parse(localStorage.getItem(LAYOUT_KEY)) || {}; } catch (_) { return {}; }
  }
  function saveLayout() {
    localStorage.setItem(LAYOUT_KEY, JSON.stringify(layout));
  }

  const deviceList = () => [...devices.values()];
  const rects = () => rectsFor(deviceList(), layout);
  const rectOf = (id) => rects().find((r) => r.id === id);
  const active = () => devices.get(activeId);

  function isOrigin(d) {
    const port = location.port || "443";
    return d.host === location.hostname && String(d.port) === port;
  }

  function wsUrl(d) {
    // Same-origin for the serving daemon: its certificate is already accepted, and reusing
    // the origin avoids a second trust prompt for the same machine.
    if (d.id === originId || isOrigin(d)) return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`;
    return `wss://${d.host}:${d.port}/ws`;
  }

  function setStatus(d, status) {
    d.status = status;
    if (d.id === activeId) {
      statusDot.className = "dot" + (status === "connected" ? " on" : "");
      statusDot.title = status;
    }
    render();
  }

  function connectDevice(d) {
    if (d.ws && (d.ws.readyState === 0 || d.ws.readyState === 1)) return;
    setStatus(d, "connecting");
    let opened = false;
    let ws;
    try { ws = new WebSocket(wsUrl(d)); } catch (_) { setStatus(d, "untrusted"); return; }
    ws.binaryType = "arraybuffer";   // capture frames arrive as binary PNG, not JSON
    d.ws = ws;

    ws.onopen = () => {
      opened = true;
      setStatus(d, "connected");
      send(d, { type: "hello" });
      if (d.id === activeId) sendWake();
    };
    ws.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) { onFrame(d, e.data); return; }
      let m; try { m = JSON.parse(e.data); } catch (_) { return; }
      onMessage(d, m);
    };
    ws.onerror = () => {};
    ws.onclose = () => {
      // Never opened => almost always an untrusted self-signed cert on that peer (the
      // browser won't tell us why). Slow retry, and the tile offers a way to accept it.
      setStatus(d, opened ? "offline" : "untrusted");
      const delay = opened ? 1500 : 20000;
      setTimeout(() => connectDevice(d), delay);
    };
  }

  function onMessage(d, m) {
    if (m.type === "devices") mergeDevices(m.devices);
    else if (m.type === "screen") {
      // A device's own bounds — until this arrives we can't drive an absolute cursor on it.
      // This frame also names the connection, which is how the bootstrap placeholder becomes
      // a real device (and how a duplicate socket to the same daemon gets collapsed).
      if (d.id !== m.name) {
        const dup = devices.get(m.name);
        if (dup && dup !== d && dup.ws) try { dup.ws.close(); } catch (_) {}
        devices.delete(d.id);
        if (activeId === d.id) activeId = m.name;
        d.id = m.name;
        devices.set(d.id, d);
      }
      d.name = m.name;
      d.platform = m.platform || d.platform;
      d.w = m.w; d.h = m.h;
      if (!originId && isOrigin(d)) originId = d.id;
      if (!activeId || !devices.has(activeId)) setActive(d.id, { quiet: true });
      render();
    } else if (m.type === "command_result") {
      showToast(m.message || m.label, m.ok ? "ok" : "err");
    } else if (m.type === "wake_state") onWakeState(m);
  }

  // Accumulate every device we've ever seen: each daemon's `devices` frame is only its own
  // momentary mDNS view, so rebuilding from one would drop peers whose record briefly lapsed.
  function mergeDevices(list) {
    if (!list) return;
    let added = false;
    for (const p of list) {
      const id = p.name;
      const known = devices.get(id);
      if (known) {
        known.host = p.host || known.host;
        known.port = p.port || known.port;
        known.platform = p.platform || known.platform;
        continue;
      }
      devices.set(id, { id, name: p.name, platform: p.platform, host: p.host, port: p.port, w: 0, h: 0, status: "new" });
      added = true;
    }
    if (added) {
      for (const d of devices.values()) if (!d.ws) connectDevice(d);
    }
    render();
  }

  function send(d, obj) {
    if (d && d.ws && d.ws.readyState === 1) d.ws.send(JSON.stringify(obj));
  }
  const sendTo = (id, obj) => send(devices.get(id), obj);
  const sendActive = (obj) => sendTo(activeId, obj);

  /// `keepCursor` is set when we're arriving by crossing an edge: the pointer is already
  /// exactly where it should be (the entry edge), so re-placing it would undo the crossing.
  function setActive(id, { quiet = false, keepCursor = false } = {}) {
    if (!devices.has(id) || id === activeId) return;
    const prev = activeId;
    activeId = id;
    localStorage.setItem(ACTIVE_KEY, id);
    // The wake lock follows the cursor: the machine you're driving is the one that must not
    // auto-lock, and the one you left should be allowed to.
    if (prev) sendTo(prev, { type: "wake", on: false });
    sendWake();
    // Capture follows it too — a crossing mid-drag must keep streaming from the new device.
    if (capturing) {
      if (prev) sendTo(prev, { type: "capture", on: false });
      sendTo(id, { type: "capture", on: true });
    }
    padcap.classList.remove("show");   // last frame belonged to the device we just left
    const d = devices.get(id);
    setStatus(d, d.status || "connecting");
    if (!quiet) showToast(`→ ${d.name}`);
    if (!keepCursor) placeCursor(id);
    render();
  }

  /// Park the virtual cursor on a device (its middle, unless told otherwise). `home` tells the
  /// daemon to re-establish where the pointer is, which Linux needs whenever it teleports.
  function placeCursor(id, at) {
    const r = rectOf(id);
    if (!r) return;
    const p = at || center(r);
    cursor = { id, gx: p.gx, gy: p.gy, overshoot: 0 };
    if (r.known) {
      const l = localOf(rects(), id, cursor.gx, cursor.gy);
      sendTo(id, { type: "move_abs", x: l.x, y: l.y, home: true });
    }
  }

  // ---- touchpad background: live crop of the desktop around the cursor, sent by the daemon
  // as PNG frames while dragging (see capture.rs / the "capture" WS message).
  let padcapUrl = null;

  function onFrame(d, buf) {
    if (d.id !== activeId) return; // only the device we're actually driving belongs on screen
    const url = URL.createObjectURL(new Blob([buf], { type: "image/png" }));
    padcap.src = url;
    padcap.classList.add("show");
    if (padcapUrl) URL.revokeObjectURL(padcapUrl);
    padcapUrl = url;
  }

  // ---- toast ----
  let toastTimer = null;
  function showToast(msg, kind) {
    toast.textContent = msg;
    toast.className = "show" + (kind ? " " + kind : "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.className = ""; }, 1800);
  }

  // ---- arrangement panel ----
  let dragging = null;   // {id, startX, startY, orig, moved}

  devBtn.addEventListener("click", () => {
    panel.hidden = !panel.hidden;
    devBtn.classList.toggle("open", !panel.hidden);
    if (!panel.hidden) render();
  });

  function render() {
    const d = active();
    devName.textContent = d ? d.name : "connecting…";
    statusDot.className = "dot" + (d && d.status === "connected" ? " on" : "");
    if (panel.hidden) return;

    const rs = rects();
    // Defaults become sticky once drawn, so a device doesn't hop about between sessions.
    for (const r of rs) {
      if (!layout[r.id]) layout[r.id] = { x: r.x, y: r.y };
    }
    const { scale, ox, oy } = fitTransform(rs, stage.clientWidth || 320, stage.clientHeight || 150, 12);
    stage.innerHTML = "";
    for (const r of rs) {
      const d2 = devices.get(r.id);
      const el = document.createElement("div");
      el.className = "tile" + (r.id === activeId ? " active" : "") + (r.known ? "" : " unknown");
      el.dataset.id = r.id;
      el.style.left = `${r.x * scale + ox}px`;
      el.style.top = `${r.y * scale + oy}px`;
      el.style.width = `${r.w * scale}px`;
      el.style.height = `${r.h * scale}px`;
      el.innerHTML =
        `<span class="tname">${d2 ? d2.name : r.id}</span>` +
        `<span class="tres">${r.known ? `${r.w}×${r.h}` : statusText(d2)}</span>` +
        `<span class="tdot ${d2 && d2.status === "connected" ? "on" : ""}"></span>`;
      stage.appendChild(el);
    }
  }

  function statusText(d) {
    if (!d) return "?";
    if (d.status === "untrusted") return "tap to trust";
    if (d.status === "connected") return "…";
    return d.status;
  }

  stage.addEventListener("pointerdown", (e) => {
    const tile = e.target.closest(".tile");
    if (!tile) return;
    e.preventDefault();
    tile.setPointerCapture(e.pointerId);
    const id = tile.dataset.id;
    const r = rectOf(id);
    dragging = { id, startX: e.clientX, startY: e.clientY, orig: { x: r.x, y: r.y }, moved: false, el: tile };
  });

  stage.addEventListener("pointermove", (e) => {
    if (!dragging) return;
    const dx = e.clientX - dragging.startX, dy = e.clientY - dragging.startY;
    if (Math.abs(dx) > TILE_TAP_SLOP || Math.abs(dy) > TILE_TAP_SLOP) dragging.moved = true;
    if (!dragging.moved) return;
    const rs = rects();
    const { scale } = fitTransform(rs, stage.clientWidth, stage.clientHeight, 12);
    const me = rs.find((r) => r.id === dragging.id);
    const moved = { ...me, x: dragging.orig.x + dx / scale, y: dragging.orig.y + dy / scale };
    layout[dragging.id] = snapRect(moved, rs.filter((r) => r.id !== dragging.id));
    render();
  });

  const endDrag = (e) => {
    if (!dragging) return;
    if (!dragging.moved) {
      const d = devices.get(dragging.id);
      // A never-opened peer is almost certainly an untrusted cert: send them to its page so
      // the browser can offer the exception, which is the only way the WS will ever connect.
      if (d && d.status === "untrusted") {
        showToast(`accept ${d.name}'s certificate, then come back`);
        window.open(`https://${d.host}:${d.port}/`, "_blank");
      } else {
        setActive(dragging.id);
      }
    } else {
      saveLayout();
      if (cursor && !rectOf(cursor.id)) placeCursor(activeId);
      else if (cursor) cursor = { ...cursor, overshoot: 0 };
    }
    dragging = null;
    render();
  };
  stage.addEventListener("pointerup", endDrag);
  stage.addEventListener("pointercancel", endDrag);
  window.addEventListener("resize", render);

  // ---- wake lock (hold off the active desktop's auto-lock while this app is focused) ----
  // The daemon holds an inhibitor for as long as we claim focus, and drops it if we go quiet
  // for 90s — so the heartbeat is what keeps a focused app's lock alive, and a phone that
  // dies or drops Wi-Fi gives the desktop its screen lock back on its own.
  const WAKE_BEAT_MS = 30000;
  let wakeWarned = false;

  const appFocused = () =>
    document.visibilityState === "visible" && document.hasFocus?.() !== false;

  function sendWake() { sendActive({ type: "wake", on: appFocused() }); }

  function onWakeState(m) {
    if (m.held && m.available === false && !wakeWarned) {
      wakeWarned = true;   // once per session; nagging every heartbeat is worse than useless
      showToast("this device can't block auto-lock", "err");
    }
  }

  document.addEventListener("visibilitychange", sendWake);
  window.addEventListener("focus", sendWake);
  window.addEventListener("blur", sendWake);
  window.addEventListener("pagehide", () => sendActive({ type: "wake", on: false }));
  setInterval(() => { if (appFocused()) sendWake(); }, WAKE_BEAT_MS);

  // ---- touchpad ----
  let lastX = 0, lastY = 0, startX = 0, startY = 0, startT = 0;
  let maxTouches = 0, moved = false, pendDx = 0, pendDy = 0, scrollAccX = 0, scrollAccY = 0, rafPending = false;
  let blockedAt = 0;
  let capturing = false;   // requesting live capture frames from the active device while dragging

  function flush() {
    rafPending = false;
    if (pendDx || pendDy) {
      moveCursor(pendDx, pendDy);
      pendDx = pendDy = 0;
    }
    if (Math.abs(scrollAccX) >= 1 || Math.abs(scrollAccY) >= 1) {
      sendActive({ type: "scroll", dx: Math.round(scrollAccX), dy: Math.round(scrollAccY) });
      scrollAccX = scrollAccY = 0;
    }
  }
  function schedule() { if (!rafPending) { rafPending = true; requestAnimationFrame(flush); } }

  /// The whole point: integrate the touch delta into the shared space, hand over at edges.
  function moveCursor(dx, dy) {
    const rs = rects();
    if (!cursor || !rs.some((r) => r.id === cursor.id)) placeCursor(activeId);
    if (!cursor) return;

    const step = advance(cursor, dx, dy, rs);
    cursor = step.state;

    if (step.blocked) {
      const now = Date.now();
      if (now - blockedAt > 3000) {   // don't spam while they keep shoving
        blockedAt = now;
        const d = devices.get(step.blockedId);
        showToast(d && d.status === "untrusted"
          ? `${d.name}: accept its certificate to cross`
          : `${d ? d.name : "device"} not connected`, "err");
      }
    }
    // keepCursor: `advance` already put the pointer on the entry edge; centring it here would
    // throw that away and make every crossing land mid-screen.
    if (step.crossed && step.state.id !== activeId) {
      setActive(step.state.id, { keepCursor: true });
    }
    if (step.send) sendTo(step.send.id, { type: "move_abs", ...step.send });
  }

  pad.addEventListener("touchstart", (e) => {
    e.preventDefault();
    pad.classList.add("active");
    const t = e.touches[0];
    lastX = startX = t.clientX; lastY = startY = t.clientY;
    startT = e.timeStamp; moved = false;
    maxTouches = e.touches.length;
    capturing = true;
    sendActive({ type: "capture", on: true });
  }, { passive: false });

  pad.addEventListener("touchmove", (e) => {
    e.preventDefault();
    maxTouches = Math.max(maxTouches, e.touches.length);
    const t = e.touches[0];
    const dx = t.clientX - lastX, dy = t.clientY - lastY;
    lastX = t.clientX; lastY = t.clientY;
    if (Math.abs(t.clientX - startX) > TAP_SLOP || Math.abs(t.clientY - startY) > TAP_SLOP) moved = true;
    if (e.touches.length < 2) {
      pendDx += dx * SENS; pendDy += dy * SENS;   // single finger = cursor (scroll lives on the bars)
    }
    schedule();
  }, { passive: false });

  pad.addEventListener("touchend", (e) => {
    e.preventDefault();
    if (e.touches.length > 0) return; // wait until all fingers up
    const quick = (e.timeStamp - startT) < TAP_MS;
    if (quick && !moved) {
      sendActive({ type: "click", button: maxTouches >= 2 ? "right" : "left" });
    }
    maxTouches = 0;
    capturing = false;
    sendActive({ type: "capture", on: false });
  }, { passive: false });

  // ---- scroll bars (right = vertical, bottom = horizontal) ----
  // Drag toward the bottom/right scrolls the target down/right (scrollbar-style).
  // Signs negate the backend's natural-direction negation; flip a sign if inverted.
  function bar(el, axis) {
    let last = null;
    el.addEventListener("touchstart", (e) => {
      e.preventDefault(); el.classList.add("active");
      const t = e.touches[0]; last = axis === "y" ? t.clientY : t.clientX;
    }, { passive: false });
    el.addEventListener("touchmove", (e) => {
      e.preventDefault();
      const t = e.touches[0];
      const p = axis === "y" ? t.clientY : t.clientX;
      const d = p - last; last = p;
      if (axis === "y") scrollAccY -= d * BAR_SENS; else scrollAccX -= d * BAR_SENS;
      schedule();
    }, { passive: false });
    const end = (e) => { e.preventDefault(); el.classList.remove("active"); last = null; };
    el.addEventListener("touchend", end, { passive: false });
    el.addEventListener("touchcancel", end, { passive: false });
  }
  bar($("vbar"), "y");
  bar($("hbar"), "x");

  // ---- push-to-talk (Web Speech API) ----
  const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
  let rec = null, recMode = null, transcript = "", typed = "";

  // Collapse whitespace so the running transcript diffs cleanly (target field owns spacing).
  const norm = (s) => s.trim().replace(/\s+/g, " ");

  // Live dictation: keep the target field equal to `full` after each partial result.
  // Web Speech rewrites in-flight words, so delete the tail it revised, then type the
  // new tail — that's why the daemon takes a `backspace` count alongside the text.
  function streamText(full) {
    full = norm(full);
    let i = 0;
    const n = Math.min(typed.length, full.length);
    while (i < n && typed[i] === full[i]) i++;
    const back = typed.length - i;   // chars the recognizer took back
    const add = full.slice(i);       // chars to append
    if (back === 0 && add === "") return;
    sendActive({ type: "text", text: add, backspace: back });
    typed = full;
  }

  function initPTT(btn, mode) {
    if (!SR) { btn.disabled = true; btn.querySelector(".sub").textContent = "voice unsupported"; return; }
    const start = (ev) => {
      ev.preventDefault();
      if (rec) return;
      transcript = ""; typed = ""; recMode = mode;
      rec = new SR();
      rec.lang = "en-US"; rec.interimResults = true;
      rec.continuous = (mode === "type");   // dictation streams; command is one utterance
      rec.onresult = (e) => {
        let full = "";                       // whole running transcript (final + interim)
        for (let i = 0; i < e.results.length; i++) full += e.results[i][0].transcript;
        if (mode === "type") streamText(full);   // type interim words live, fix as they settle
        else transcript = full;                  // command: hold until release
        btn.querySelector(".lbl").textContent = norm(full) || (mode === "type" ? "dictating…" : "listening…");
      };
      rec.onerror = (e) => showToast("mic: " + e.error, "err");
      rec.onend = () => finish(btn);
      try { rec.start(); btn.classList.add("rec"); btn.querySelector(".lbl").textContent = "listening…"; }
      catch (_) { rec = null; }
    };
    const stop = (ev) => { ev.preventDefault(); if (rec) try { rec.stop(); } catch (_) {} };
    btn.addEventListener("pointerdown", start);
    btn.addEventListener("pointerup", stop);
    btn.addEventListener("pointerleave", stop);
    btn.addEventListener("pointercancel", stop);
  }

  function finish(btn) {
    btn.classList.remove("rec");
    btn.querySelector(".lbl").textContent = btn.dataset.label;
    const mode = recMode, text = transcript.trim();
    // dictation streamed everything already; add a trailing space so the next word isn't glued on
    if (mode === "type" && typed) sendActive({ type: "text", text: " ", backspace: 0 });
    if (mode === "command" && text) sendActive({ type: "command", text });
    rec = null; recMode = null; transcript = ""; typed = "";
  }

  const typeBtn = $("ptt-type"), cmdBtn = $("ptt-cmd");
  typeBtn.dataset.label = "Hold to type";
  cmdBtn.dataset.label = "Hold to command";
  initPTT(typeBtn, "type");
  initPTT(cmdBtn, "command");

  // Bootstrap: the serving daemon. It reports itself plus every peer it can see over mDNS,
  // and each of those gets its own socket from mergeDevices().
  devices.set("__origin__", {
    id: "__origin__", name: "connecting…", platform: "?",
    host: location.hostname, port: Number(location.port || 443), w: 0, h: 0, status: "new",
  });
  connectDevice(devices.get("__origin__"));
  render();
})();
