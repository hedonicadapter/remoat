// remoat thin client: touchpad + device dropdown + push-to-talk.
(() => {
  "use strict";

  const SENS = 1.7;        // pointer sensitivity
  const SCROLL_SENS = 0.6; // two-finger scroll scaling
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
      if (m.type === "devices") populateDevices(m.devices);
      else if (m.type === "command_result") showToast(m.message || m.label, m.ok ? "ok" : "err");
    };
  }

  function send(obj) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj)); }

  // ---- device dropdown ----
  function populateDevices(devices) {
    if (!devices || !devices.length) return;
    const cur = deviceSel.value;
    deviceSel.innerHTML = "";
    for (const d of devices) {
      const val = `${d.host}:${d.port}`;
      const o = document.createElement("option");
      o.value = val;
      o.textContent = `${d.name} · ${d.platform}${d.self ? "" : ""}`;
      if (d.self && !cur) o.selected = true;
      deviceSel.appendChild(o);
    }
    if (cur) deviceSel.value = cur;
  }
  deviceSel.addEventListener("change", () => {
    const [host] = [deviceSel.value];
    // Reconnect to selected daemon; self keeps current origin
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
  let maxTouches = 0, moved = false, pendDx = 0, pendDy = 0, scrollAcc = 0, rafPending = false;

  function flush() {
    rafPending = false;
    if (pendDx || pendDy) { send({ type: "move", dx: pendDx, dy: pendDy }); pendDx = pendDy = 0; }
    if (Math.abs(scrollAcc) >= 1) { send({ type: "scroll", dy: Math.round(scrollAcc) }); scrollAcc = 0; }
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
    if (e.touches.length >= 2) {
      scrollAcc += dy * SCROLL_SENS;   // two-finger drag = scroll
    } else {
      pendDx += dx * SENS; pendDy += dy * SENS;
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

  // ---- push-to-talk (Web Speech API) ----
  const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
  let rec = null, recMode = null, transcript = "";

  function initPTT(btn, mode) {
    if (!SR) { btn.disabled = true; btn.querySelector(".sub").textContent = "voice unsupported"; return; }
    const start = (ev) => {
      ev.preventDefault();
      if (rec) return;
      transcript = ""; recMode = mode;
      rec = new SR();
      rec.lang = "en-US"; rec.interimResults = true; rec.continuous = false;
      rec.onresult = (e) => {
        transcript = "";
        for (let i = 0; i < e.results.length; i++) transcript += e.results[i][0].transcript;
        btn.querySelector(".lbl").textContent = transcript || "listening…";
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
    const text = transcript.trim();
    btn.classList.remove("rec");
    btn.querySelector(".lbl").textContent = btn.dataset.label;
    const mode = recMode; rec = null; recMode = null;
    if (!text) return;
    if (mode === "type") { send({ type: "text", text }); showToast('typed: "' + text + '"'); }
    else { send({ type: "command", text }); }
  }

  const typeBtn = $("ptt-type"), cmdBtn = $("ptt-cmd");
  typeBtn.dataset.label = "Hold to type";
  cmdBtn.dataset.label = "Hold to command";
  initPTT(typeBtn, "type");
  initPTT(cmdBtn, "command");

  connect(null);
})();
