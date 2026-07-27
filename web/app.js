// remoat thin client: touchpad + device dropdown + push-to-talk.
(() => {
  "use strict";

  const SENS = 1.7;        // pointer sensitivity
  const BAR_SENS = 1.1;    // scroll-bar drag scaling
  const TAP_MS = 250;      // max duration for a tap
  const TAP_SLOP = 10;     // max movement (px) still counted as a tap

  const $ = (id) => document.getElementById(id);
  const pad = $("pad"), statusDot = $("status"), deviceSel = $("device"), toast = $("toast");

  // ---- WebSocket ----
  let ws = null, wantHost = null;

  function wsUrl(host) {
    if (host) return `wss://${host}/ws`;
    return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`;
  }

  function connect(host) {
    wantHost = host || null;
    if (ws) { try { ws.close(); } catch (_) {} }
    ws = new WebSocket(wsUrl(host));
    ws.onopen = () => { statusDot.classList.add("on"); statusDot.title = "connected"; };
    ws.onclose = () => { statusDot.classList.remove("on"); statusDot.title = "disconnected"; setTimeout(() => connect(wantHost), 1500); };
    ws.onerror = () => showToast("connection error", "err");
    ws.onmessage = (e) => {
      let m; try { m = JSON.parse(e.data); } catch (_) { return; }
      if (m.type === "devices") mergeDevices(m.devices);
      else if (m.type === "command_result") showToast(m.message || m.label, m.ok ? "ok" : "err");
    };
  }

  function send(obj) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj)); }

  // ---- device dropdown ----
  // Accumulate every device we've ever seen. Each daemon's `devices` message is only
  // its own momentary mDNS view; rebuilding from it alone would drop peers whose mDNS
  // record briefly lapsed. Merge instead so a device never vanishes mid-session.
  const known = new Map();   // "host:port" -> {name, platform, host, port, val}
  let currentVal = null;     // the daemon the WS is actually connected to

  function mergeDevices(devices) {
    if (!devices) return;
    let connectedTo = null;
    for (const d of devices) {
      const val = `${d.host}:${d.port}`;
      known.set(val, { name: d.name, platform: d.platform, host: d.host, port: d.port, val });
      if (d.self) connectedTo = val;   // the reporting daemon = the one we're on
    }
    if (connectedTo) currentVal = connectedTo;
    renderDevices();
  }

  function renderDevices() {
    deviceSel.innerHTML = "";
    for (const d of known.values()) {
      const o = document.createElement("option");
      o.value = d.val;
      o.textContent = `${d.name} · ${d.platform}`;
      deviceSel.appendChild(o);
    }
    if (currentVal && known.has(currentVal)) deviceSel.value = currentVal;
  }

  deviceSel.addEventListener("change", () => {
    const host = deviceSel.value;
    currentVal = host;   // remember selection so a device-list refresh doesn't reset it
    // Reconnect to selected daemon; the served origin reconnects same-origin.
    connect(host === `${location.hostname}:${location.port || 443}` ? null : host);
    showToast(`switched to ${deviceSel.selectedOptions[0].textContent}`);
  });

  // ---- toast ----
  let toastTimer = null;
  function showToast(msg, kind) {
    toast.textContent = msg;
    toast.className = "show" + (kind ? " " + kind : "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.className = ""; }, 1800);
  }

  // ---- touchpad ----
  let lastX = 0, lastY = 0, startX = 0, startY = 0, startT = 0;
  let maxTouches = 0, moved = false, pendDx = 0, pendDy = 0, scrollAccX = 0, scrollAccY = 0, rafPending = false;

  function flush() {
    rafPending = false;
    if (pendDx || pendDy) { send({ type: "move", dx: pendDx, dy: pendDy }); pendDx = pendDy = 0; }
    if (Math.abs(scrollAccX) >= 1 || Math.abs(scrollAccY) >= 1) {
      send({ type: "scroll", dx: Math.round(scrollAccX), dy: Math.round(scrollAccY) });
      scrollAccX = scrollAccY = 0;
    }
  }
  function schedule() { if (!rafPending) { rafPending = true; requestAnimationFrame(flush); } }

  pad.addEventListener("touchstart", (e) => {
    e.preventDefault();
    pad.classList.add("active");
    const t = e.touches[0];
    lastX = startX = t.clientX; lastY = startY = t.clientY;
    startT = e.timeStamp; moved = false;
    maxTouches = e.touches.length;
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
      if (maxTouches >= 2) send({ type: "click", button: "right" });
      else send({ type: "click", button: "left" });
    }
    maxTouches = 0;
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
    send({ type: "text", text: add, backspace: back });
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
    if (mode === "type" && typed) send({ type: "text", text: " ", backspace: 0 });
    if (mode === "command" && text) send({ type: "command", text });
    rec = null; recMode = null; transcript = ""; typed = "";
  }

  const typeBtn = $("ptt-type"), cmdBtn = $("ptt-cmd");
  typeBtn.dataset.label = "Hold to type";
  cmdBtn.dataset.label = "Hold to command";
  initPTT(typeBtn, "type");
  initPTT(cmdBtn, "command");

  connect(null);
})();
