# remoat — context for Claude Code

Phone→desktop remote control. iPhone PWA (touchpad + device dropdown + 2 push-to-talk
buttons: dictation + NL commands) ↔ desktop daemon that injects input. Targets macOS
and Linux (incl. Hyprland/Wayland). This file is the handoff so a fresh session (e.g.
cloud) has full context. Read the README too.

## Current status (2026-08-02)

**Voice commands + configurable command table are BUILT** (was the design section below).
`src/voice.rs` (normalize/count/chain), `src/actions.rs` (the allowlist), `src/config.rs`
(embedded default TOML + user overrides, mtime hot-reload), rewritten `src/commands.rs`.
25 unit tests (`cargo test`), `nix build` still hermetic, verified end-to-end over the real
WS/TLS path on this Mac (`REMOAT_BACKEND=null`): counts, chains, clarify, user override,
runaway refusal, and a live config edit picked up with no restart. Linux chords are mapped
but unexercised (no Linux builder here) — see "Open threads".

Debug without speaking: `remoat --check "delete 3 words"` prints the resolution (no
injection); `remoat --actions` lists the allowlist + the user table path.

**Multi-device arrangement + edge crossing BUILT** (replaces the device dropdown). Devices are
laid out like monitors and share ONE cursor: shove it off an edge and it continues onto the
bordering device. Design consequences worth understanding before changing any of it:
- **The app owns the cursor.** Nothing can ask Wayland where the pointer is (uinput is
  write-only), so the phone integrates touch deltas into a global position and sends
  `move_abs`; the daemon reports its bounds (`screen` frame) and clamps. Relative `move` is
  still handled for older clients.
- **On Linux, `move_abs` is emulated with relative deltas** — ydotool's absolute mode is broken
  upstream (see Gotchas). The daemon tracks where it last put the pointer; `home: true` (sent
  when the pointer teleports: a crossing or a switch) re-establishes that by slamming into a
  corner. Pointer acceleration means the model and the real pointer drift apart mid-screen;
  targets on an edge are overshot deliberately so every edge contact re-pins both — and
  reaching an edge is exactly what crossing requires. macOS has real absolute placement, so
  none of this applies there.
- **One coordinate space.** Every device is a rect in the same space, so a crossing needs no
  mapping maths — the global coordinate is continuous across the seam, which is why you exit
  and enter at the same height. Geometry lives in `web/arrange.js` (pure, unit-tested).
- **A socket per device, all open at once**, because a crossing must be instant. Peers need
  their self-signed cert accepted once; a never-opened socket is reported as `untrusted` and
  its tile offers to open that device's page so the browser can take the exception.
- **Crossing takes a deliberate shove** (`PUSH`, 60px banked past the edge) and only where the
  rects actually overlap perpendicular to the seam, so a diagonal neighbour can't catch you.
- Layout + active device persist in `localStorage`; devices are keyed by daemon **name**, not
  host:port, so DHCP churn doesn't scramble the arrangement.
- The wake lock follows the active device: crossing releases the one you left.

Verified end-to-end on this Mac: two daemons (1800x1169 and 2560x1440) on ports 8501/8502 with
`REMOAT_BACKEND=ydotool REMOAT_DRYRUN=1`, driven by the real `arrange.js` through real
WebSockets — the left daemon pinned at `-x 1799`, the right one received `-x 0 -y 585` on the
crossing (same height), the return crossing landed back on the left, and the wake lock moved
with it. 17 geometry tests (`node --test web/arrange.test.mjs`).

**Wake lock BUILT** (`src/wake.rs`): the selected desktop won't auto-lock while the PWA is
focused. Client sends `{type:"wake",on:…}` on focus/visibility change plus a 30s heartbeat;
daemon holds an inhibitor process, refcounted across clients. Verified on this Mac: assertion
appears/disappears with focus, survives one of two clients leaving, and is released on clean
close, abrupt socket drop, 90s client silence, SIGTERM **and SIGKILL**. Linux inhibitor
argv is unexercised (no Linux box) — see "Open threads".

**Touchpad live capture REMOVED** (2026-11-07). The PWA briefly showed a live PNG crop of the
desktop around the cursor (`src/capture.rs`, `#padcap`, the `capture` WS message, binary frames
downstream, `grim`/`screencapture` shell-outs, `REMOAT_CAPTURE_SIZE`). All of it is gone: the
daemon no longer screenshots anything, `grim` is out of the flake's `runtimeTools`, and the only
binary WS frames are now mic PCM going up. The touchpad background is a plain pad again.
Lessons it produced that still apply (they are about geometry and pointer modelling, not about
screenshots):
- **`screen.rs` must report the LOGICAL desktop.** Building it found three stacked bugs: no niri
  support (`hyprctl` → `wlr-randr` → `xrandr` all missed, so a 3840x1080 desktop silently became
  the 1920x1080 FALLBACK — and `wlr-randr` was never on PATH); physical mode sizes paired with
  logical positions (a scale-2 4K panel inflated the union to 5760x2160); and an origin assumed
  at 0,0. niri is now queried first via `niri msg --json outputs`, both other parsers divide by
  `scale` (and swap axes on 90°/270°), and `Bounds` carries `ox`/`oy`. 50 unit tests including
  the mixed-scale JSON for all three parsers. See "Screen geometry" in Gotchas.
- **Never await a subprocess inline in the per-connection WS loop.** The first cut did, and
  every touchmove queued behind an in-flight screenshot, stalling the cursor itself. The socket
  is split into a shared `Arc<Mutex<SplitSink>>` and a receive-only stream; that split stays,
  since STT replies use it too.
- **Flat pointer accel is a hard requirement for an exact model on Linux, and that is a design
  flaw, not a setting.** The crop made this visible: it was centred on where the daemon
  *believes* the pointer is, and with relative injection under libinput's adaptive profile the
  real pointer drifts hundreds of px from that model mid-screen. Worked example on a 3840x1080
  desktop: `placeCursor` centres at (1920,540) with `home:true`, `home_corner` slams to
  (3839,1079), then walks `dx = -1919` — accel amplifies a delta that large, so the pointer
  overshoots left. **Homing does not fix this** (the walk away from the corner is accelerated
  too); the only in-design resync is a screen edge, which is why `abs_to_rel` overshoots edge
  targets. Touching the real mouse desynchronises the model until the next `home` or edge
  contact. The ydotool backend says so at startup. See the absolute-device item in Open threads.

**Local streaming speech-to-text BUILT** (`src/stt.rs`, `web/pcm-worklet.js`) — **verified
end-to-end on real audio through a real browser**.
- **Why a streaming transducer and not Whisper.** Live typing while you speak is a hard
  requirement. Whisper is batch and pads every inference to 30s, so faking partials means
  re-transcribing a growing buffer — slow and O(n²). A zipformer **transducer** (sherpa-onnx
  `OnlineRecognizer`) decodes frame-synchronously with cached state, so partials are free.
  Measured bonus: it is **append-only** — 0 backspaces across 104 typed chars — so dictation
  rarely retracts characters from your document. Whisper-based options (`whisper-rs`,
  `transcribe-rs`, Handy's stack) were rejected for exactly this; `vosk` and
  `sherpa-transducers` for being stale (Oct 2024 / Apr 2025) and, for vosk, absent from nixpkgs.
- **The mic is on the phone, the model runs on the desktop.** So `cpal` is irrelevant here.
  `web/pcm-worklet.js` captures via AudioWorklet (NOT MediaRecorder — iOS emits mp4/AAC, which
  would need a decoder on the daemon), converts to Int16 in the audio thread, and posts ~100ms
  batches that go up as WS **binary frames** — the only binary traffic on the socket.
- **No `rubato`.** sherpa-onnx ships `LinearResampler`; the phone's 48kHz is converted in-engine.
  One fewer dependency than the plan called for.
- **The daemon types, not the phone.** With a local model the transcript lives on the daemon, so
  `streamText()`'s running-transcript diff was ported into `emit_transcript`/`stt::diff`. The
  diff is **char-wise, not byte-wise** — it drives real backspaces, and a byte split would both
  miscount and panic on a multi-byte boundary. The `text` message stays for Web Speech clients.
- **Graceful degradation is the design, not a fallback bolted on.** No model => `screen` frame
  reports `stt:false` => the phone keeps using Web Speech. `remoat --fetch-model` installs it
  (296MB download, ~71MB int8 weights) into `$XDG_STATE_HOME/remoat/models`, the same
  state-not-store pattern as the TLS certs. Deliberately an explicit command, not a surprise
  first-run download.
- **The model is chosen by measurement, and the first choice was wrong.** v1 shipped the 20M
  zipformer to keep the download small; real use was audibly worse, which the bundled reference
  wavs then confirmed. `REMOAT_BENCH=1 cargo test bench_models -- --nocapture` benchmarks every
  model under the model root (WER + realtime factor):
  ```
  20M-2023-02-17  int8 greedy  7.7% WER  0.051 RTF
  20M-2023-02-17  int8 beam    6.3%      0.060
  2023-06-26      int8 beam    0.0%      0.095   <- default
  2023-06-26      fp32 beam    0.0%      0.137
  ```
  Worth keeping: **int8 costs no accuracy here and is 45% faster than fp32** — don't "upgrade"
  to fp32. **Beam search only helped the small model** (7.7%→6.3%) and bought nothing on this one
  (0.0% either way) while costing ~20% more decode time and re-ranking partials more — which is
  visible churn when the output is live keystrokes. It was briefly the default only because
  hotwords require it; since hotwords are still inert that was a cost with no benefit, and the
  default is back to `greedy_search`.
  Model files are located by PREFIX (`encoder*`/`decoder*`/`joiner*`) since names differ across
  the zoo, so `REMOAT_STT_MODEL` can point at any streaming transducer, by name or absolute
  path; `REMOAT_STT_PRECISION=fp32` and `REMOAT_STT_DECODING=greedy_search` are the other knobs.
  NeMo conformers need `greedy_search` — asking one for beam search **aborts the process** from
  inside sherpa's C++, which no Rust error handling can catch.
- **`PAD_SAMPLES` is load-bearing, not defensive.** The streaming encoder needs warm-up frames:
  with no lead-in silence the model **drops the opening words outright** (0s and 0.3s both gave
  "THE YELLOW LAMPS…"; 0.5s gave the correct "AFTER EARLY NIGHTFALL THE YELLOW LAMPS…"). A user
  presses PTT and talks immediately, so the lead-in a recogniser wants is exactly what live
  dictation never has. A trailing pad likewise flushes the last token. Don't remove either.
- Hotwords (biasing decoding toward the command table — the thing that would replace `voice.rs`'s
  hand-written `FIXUPS`) are plumbed through `start()` but **currently inert**: sherpa only
  applies them under `modified_beam_search`, and we decode greedily for latency. See Open threads.

Verified: 65 unit tests including real decodes of the model's bundled reference wavs (WER < 0.05,
streaming in 100ms chunks, partials confirmed to arrive before 50% of the audio, and the 8kHz wav
exercising the resampler). Then end-to-end over real WS/TLS: injected fragments reassemble
*exactly* to the transcript with **zero backspaces**. Then the real PWA in real Chromium with a
fake audio device feeding a wav into `getUserMedia`: the button subtitle flipped to
"dictation · on-device" and the label showed a live growing transcript while held. The
no-model path was checked the same way — subtitle stays "dictation", buttons stay enabled, no
page errors — so a daemon without a model still dictates via Web Speech.

**Loosely confirmed on a real iPhone and the nixos box by the user** (2026-08-02) — dictation
works. Not systematically exercised, so iOS Safari edge cases (context suspension on
backgrounding, forced sample rates, standalone-PWA quirks) are still unproven rather than
known-broken.

**Daemon rewritten from Python to Rust** (`src/`). Reason: Python packaging friction
under Nix (pyautogui pulls Linux-only `scrot`, venv/pyobjc mess). Rust = single binary,
clean `buildRustPackage`, `nix run` works on darwin + Linux. The PWA (`web/`) and the WS
protocol are unchanged; the Python POC is kept in `daemon/` for reference only.

Verified on this Mac (aarch64-darwin): clean `cargo build`; `nix build` hermetic (EXIT 0);
daemon serves HTTPS (curl 200), `/ws` returns 101 + devices frame, TLS 1.3 negotiates,
mDNS **discovered the `nixos` peer** at 192.168.1.77:8443. Backend `enigo` on mac.

**Not build-tested here: the Linux package** (no Linux builder on the Mac). enigo is
macOS-only (see Cargo.toml `[target.'cfg(target_os="macos")']`), so the Linux build only
compiles ydotool-via-process + the base deps — lower risk, but confirm on the nixos box.

Earlier Hyprland bring-up (Python daemon, now all working per user): firewall opened
(`networking.firewall.allowedTCPPorts=[8443]`), `programs.ydotool.enable=true`, user
added to `ydotool` group. Those system settings still apply to the Rust daemon (it shells
out to the same `ydotool`).

To ship an update: `git add -A && git commit && git push`, then on each machine
`nix run --refresh github:hedonicadapter/remoat` (‑‑refresh busts the ~1h flake cache).

## Architecture

Fat daemon / thin phone. **Daemon consumes _text_** so the STT source is swappable.
- `src/main.rs` — axum: serves PWA, `/ws` WebSocket, HTTPS (axum-server + rustls).
  Protocol in the module doc (move/click/scroll/text/command/hello/ping).
- `src/input.rs` — backends: `enigo` (macOS/Quartz) + `ydotool` (Linux/uinput, X11+Wayland)
  + `null`. Chosen on a dedicated worker thread (enigo is !Send). Override
  `REMOAT_BACKEND=enigo|ydotool|null`; `REMOAT_DRYRUN=1` logs ydotool argv.
- `src/commands.rs` — intent router: command table first, then `llm_fallback` (documented
  seam, NOT wired). Resolves to `Vec<Exec>` (Input | Launch | Note) so one utterance can
  carry counts and chains; `main.rs` dispatches them in order and returns ONE aggregated
  `command_result`.
- `src/actions.rs` — `NAMES` + `action(name)`: the single platform-mapped allowlist
  (delete_word = ⌥⌫ mac / ctrl+⌫ linux, …). Everything routes through here.
- `src/voice.rs` — normalize (case/punct/mishearings/number words), chain split
  (and/then/comma), repeat suffix (`twice`, `3 times`, `x4`), `{n}` count slot.
- `src/config.rs` — command table: embedded `src/commands.default.toml` + user
  `commands.toml` merged on top, re-read on mtime change, per-rule fail-soft validation.
- `src/screen.rs` — desktop bounds in logical coords (env override → Quartz via enigo → niri /
  hyprctl / wlr-randr / xrandr → 1920x1080). Keeps the real origin (`ox`/`oy`); also clamps
  incoming `move_abs`.
- `web/arrange.js` — arrangement + edge-crossing geometry, pure and DOM-free so it can be
  unit-tested (`web/arrange.test.mjs`). `web/app.js` is an ES module because of this import.
- `src/wake.rs` — wake lock: holds `caffeinate -d -i -w <pid>` (mac) / `systemd-inhibit …
  tail --pid=<pid>` (Linux) while any client says it's focused. Refcounted; released on
  blur, on 90s client silence, on disconnect, and on SIGINT/SIGTERM.
- `src/discovery.rs` — mdns-sd advertise + browse.
- `src/tls.rs` — self-signed cert via rcgen, in `$XDG_STATE_HOME/remoat` (or
  `REMOAT_CERT_DIR`). `REMOAT_WEB_DIR` points at the PWA (flake sets it; defaults `./web`).
- `web/` — the PWA (touchpad gestures, dropdown, PTT via Web Speech API in-browser).

## Run

- `nix run github:hedonicadapter/remoat` (Linux + macOS). `--port N` to change port.
- From source: `nix develop --command cargo run` (devShells.default has cargo + cmake).
- macOS injection needs Accessibility granted to the launching terminal (rotating
  `/nix/store` path makes granting the store binary impractical — grant the terminal).
- `remoat --fetch-model` once per machine for offline on-device dictation. Without it the
  daemon still runs and the phone falls back to the browser's Web Speech API (which sends your
  audio to Apple/Google — the reason local STT exists).

## Open threads / next steps

- **AUTH — top priority, and the gap widened.** There is no authentication of any kind:
  anyone who can reach the port can inject input and drive dictation.
  mDNS advertises the daemon, so they don't even have to look for it. TLS is on but
  self-signed, so it gives confidentiality, not identity. Port 8443 itself is fine.
  Design sketch: random 256-bit token per daemon in `$XDG_STATE_HOME/remoat` (next to the
  certs), shown as a short pairing code at startup; phone stores it in `localStorage` beside
  the layout. Gotchas found while building the rest of this — worth reading before starting:
  - **Browsers cannot set headers on a WebSocket.** So no `Authorization`. Don't put the token
    in the query string either (it lands in logs and history). Do a first-message handshake:
    accept the socket, then ignore every frame until a valid `{type:"auth",token}` arrives,
    with a short timeout and a close on failure.
  - **`screen` and `devices` are currently sent BEFORE the client says anything** (handshake
    order matters to the app — see `handle_socket`). They leak hostname and resolution to an
    unauthenticated peer, so they have to move after auth *without* breaking the ordering the
    client depends on.
  - **Gate every path, not just input.** `stt_start` + binary audio and `wake` are separately
    reachable today.
  - **One token per daemon, and the phone holds sockets to ALL of them** (edge crossing needs
    them open). So pairing is per-device and the arrangement UI already has the right place to
    hang it: the tile's existing `untrusted` state, which currently only means "cert not
    accepted" and would gain a second meaning.
  - **The cert exception flow already sends users to each peer's page once.** Pairing can ride
    along with that rather than inventing a second ceremony.
  - Constant-time compare; rate-limit failures; and a token that never appears in a toast,
    a log line, or the `devices` frame.
  - Local STT means audio no longer leaves the LAN — but an unauthenticated peer can still
    *inject* text through the same path, so it is not a mitigation.
- Wire the LLM/MCP command fallback (`src/commands.rs:llm_fallback`). The tool schema's
  action parameter should be `enum: actions::NAMES` + an integer count, so the model can
  emit nothing the deterministic tier couldn't.
- ~~Move STT onto the daemon~~ — DONE, see "Local streaming speech-to-text" above (streaming
  zipformer transducer, not whisper.cpp; the reasoning is worth reading before revisiting).
- **Wire up hotwords** so decoding is biased toward the command table — this is what would let
  `voice.rs`'s hand-maintained `FIXUPS` mishearing list shrink, and it would update itself when
  the user edits `commands.toml`. Blocked on sherpa only honouring hotwords under
  `modified_beam_search`: the command path would need its own recognizer instance (~40MB more
  resident) so dictation can keep decoding greedily for latency. `Stt::start()` already takes
  the argument.
- **`nix build` is UNVERIFIED for the STT change** — no nix in the cloud container. The
  mechanism it depends on *was* verified directly: with `SHERPA_ONNX_LIB_DIR` set and the
  download cache deleted, `sherpa-onnx-sys` recompiled and linked the system library with no
  network fetch. Confirm on a real machine, and note nixpkgs has sherpa-onnx 1.13.3 vs the
  crate's 1.13.4 — close, but a future bump could desync.
- Confirm the Linux/Hyprland Rust build on the nixos box (unbuilt on the dev Mac). Same
  trip: exercise the linux chords in `actions.rs` — `delete_line` (shift+home, backspace),
  `line_start`/`line_end` (home/end), `doc_start`/`doc_end` (ctrl+home/end). Keycodes are
  unit-tested (`input.rs::keyargs`), the *bindings* aren't.
- PWA settings screen that edits `commands.toml` over WS (`{type:"config_set",…}`) — the
  file format is ready for it; the WS message is not implemented.
- **Exact pointer tracking on Linux needs a real absolute device — the top functional gap.**
  The (since removed) touchpad capture made the drift plainly visible; it remains a
  crossing-reliability problem. **Flat pointer accel is explicitly rejected as the answer** —
  the user's position, and the right one: a remote-control daemon that only works if you
  reconfigure your compositor is broken by design, not configurable. Treat flat accel as a
  diagnostic for confirming the cause, never as the fix. See the worked example under
  "Touchpad live capture REMOVED" for why nothing else *in the current design* can be exact —
  the design has to change.
  Two candidate mechanisms, neither verified:
  - **Own uinput device WITH an ABS range** (`UI_ABS_SETUP` — the call ydotool never makes).
    Needs `/dev/uinput`, i.e. the same group `programs.ydotool.enable` already grants. udev's
    `input_id` classifies ABS_X/ABS_Y + a mouse button as a *mouse*, and libinput does support
    absolute pointers (it is how a QEMU `usb-tablet` gives a 1:1 cursor), so this is well
    trodden — but unknown how niri maps an absolute device across a multi-output layout; if it
    maps to one output, expect a whole-monitor offset rather than a fix.
  - **`wlr-virtual-pointer`'s `motion_absolute`** — no uinput permissions needed, but niri's
    support for `zwlr_virtual_pointer_v1` is unconfirmed; probe for the global before building
    on it.
  Either removes the acceleration drift, the corner-homing slam, and the real-mouse desync.
- Arrangement gaps worth knowing: bounds are ONE rect per device starting at 0,0 — on mac
  that's the main display only (enigo has no virtual-desktop query), so a second monitor on a
  mac isn't reachable by crossing. Linux unions its monitors via hyprctl/wlr-randr, so it is.
  `REMOAT_SCREEN=WxH` is the manual override.
- The layout is per-phone (localStorage). Sharing it across clients means persisting it on the
  daemons — the `config_set`-style seam below is the natural place.
- Devices are keyed by daemon name, so two daemons with the SAME hostname (e.g. two on one
  box, as in the crossing test) collide into one tile. Fine on a real LAN; don't be surprised
  in tests.
- Confirm the wake lock on the nixos box: `systemd-inhibit --what=idle:sleep … tail --pid=N`
  argv is untested, and hypridle only honours logind inhibitors when
  `ignore_dbus_inhibit=false` (its default). `wlinhibit` is the no-logind fallback and is the
  one path with no pid leash, so it leans on the SIGTERM handler alone.
- **macOS permissions: Accessibility fails in ABSOLUTE SILENCE, and that silence is
  structural, not a logging gap.** (For contrast, while the daemon still screenshotted,
  `screencapture` without Screen Recording permission printed its own `cannot create image…`
  to stderr — loud. Screen Recording is no longer needed by anything.)
  It took a real debugging session (reproduced below
  because the dead ends are as instructive as the answer) to find why: `CGEventPost` — what
  every enigo mouse/keyboard call bottoms out at — is a `void` C function with **no error
  return**. If the process isn't trusted, macOS silently drops the event; enigo's `move_mouse`
  still returns `Ok(())`. There is no return value, no error, and — unlike Screen Recording's
  request/response flow — no `log stream --predicate 'subsystem == "com.apple.TCC"'` entry
  either, because a raw `CGEventPost` doesn't go through the request-arbitration path that
  generates that log at all. **A blocked cursor and a working one look byte-for-byte identical
  from inside the daemon.** Fixed in `input.rs::check_accessibility_trust`: call
  `AXIsProcessTrustedWithOptions` directly via `core-foundation`, log the real answer at
  startup, and if untrusted, pass the prompt option so macOS raises the correctly-attributed
  system dialog itself instead of us inferring identity from context.
  Dead ends worth knowing so they aren't retried: (1) macOS attributes TCC checks to the
  *responsible process*, not the leaf binary, which sent early debugging toward "grant `sshd`"
  since the user described launching over ssh — wrong in this case, since `ps aux | grep sshd`
  showed no such process running at all. (2) `log stream` itself has three unrelated shell
  landmines that ate several rounds before any real log was seen: smart-quote corruption of the
  `--predicate` string from copy/paste ("too many arguments"), a shell builtin literally named
  `log` in zsh (leftover from the `WATCH`/login-monitoring feature) shadowing `/usr/bin/log`
  (fix: `command log …`), and needing `sudo` for TCC's own log lines specifically. (3) The
  actual live popup during debugging named `skhd` (a hotkey daemon), an apparently unrelated
  process — plausible now given point (1): TCC's responsible-process resolution can name a
  different identity than the terminal you're staring at, and there is no shortcut around
  reading real evidence (`log stream`, then eventually the direct API call) to find out which
  one actually matters for a given code path. (4) The eventual real answer, from the log, was
  that the daemon runs from **Kitty** (`net.kovidgoyal.kitty`, itself a nix store path — same
  rotation problem as remoat's own binary), confirmed for Screen Recording; Accessibility could
  never be confirmed via log at all, which is the whole point above.

**Dynamic command tier BUILT** (`src/apps.rs`, `src/wm.rs`, `src/dynamic.rs`) — commands whose
*object* only exists at runtime: the windows you have open, the apps you have installed.
```
utterance → static table → DYNAMIC TIER → llm_fallback
```
- **`apps.rs`** reads the OS's own answer to "what can I launch": XDG desktop entries on Linux
  (including `/run/current-system/sw/share/applications`, where nix-declared apps land) and
  `.app` bundles on macOS. This is the "generate commands from my config" idea done against the
  *evaluated* result — parsing nix source would mean `nix eval` on a Turing-complete language,
  while desktop entries are the same information already evaluated and portable.
- **`wm.rs`** is one `Wm` trait with per-compositor backends: `niri msg --json` and
  `omniwmctl query windows --json`. Chosen over the portable
  `wlr-foreign-toplevel-management-v1` (which ~20 compositors implement) because that protocol
  has **no concept of a workspace**, and workspaces are half of "control my desktop". The
  portable protocol is the long-tail fallback and slots in as another backend.
- **Detection probes CAPABILITY, never the compositor's name.** "Can I list windows?", not
  "am I on niri?". A missing backend disables only window/workspace commands; apps still work.
- **omniwm ships with IPC disabled by default**, so a failing probe says *"is IPC enabled in
  OmniWM's settings?"* rather than nothing — the `wlr-randr` lesson, applied preemptively.
- **Window ids are never cached.** omniwm's are session-scoped and go stale on restart, so a
  name resolves to an id at the moment you speak. `wm::windows()` has only a 750ms cache so one
  utterance doesn't round-trip the compositor twice.
- **Matching is fuzzy** (`dynamic::score`) because the input is a speech transcript: lowercase,
  unpunctuated, prone to mangling proper nouns. Exact matching against a window title would
  essentially never fire. Two near-equal candidates are REFUSED rather than coin-flipped —
  acting on the wrong window is worse than falling through.
- **Resolution stays side-effect free**, so `remoat --check "focus slack"` prints
  `run niri msg action focus-window --id 7` without doing it. New `Exec::Run` waits for exit
  status (unlike `Exec::Launch`, which spawns and forgets) because for WM IPC whether it
  *worked* is the whole point of the toast.

First real-hardware feedback (both machines) found four separate bugs, all fixed — the failure
modes are worth knowing because each is a different kind of mistake:
- **"focus zen" did nothing while "focus spotify" worked.** Two Zen windows both scored 1.0, and
  the tie-refusal killed it. Refusing a near-tie is right ACROSS apps and wrong WITHIN one: three
  windows of one browser means "go to the browser", not "ambiguous". `best_of` now takes a
  grouping function and only refuses when the tied candidates belong to different apps.
- **"focus browser" could never have worked.** Nothing on the machine is called "browser" — it's
  a synonym problem, and no amount of fuzzy string matching bridges it. `ALIASES` maps generic
  nouns (browser/terminal/editor/music/files/chat/mail) to the identities that satisfy them, and
  an alias group is a disjunction: any member matching is a hit.
- **"switch workspace 3" fell through** because the prefix list had `"switch to "` but not
  `"switch workspace"`. Phrase coverage was too rigid; `workspace_number` now strips a verb list
  (go to / switch / switch to / change / jump to / show) independently of the noun.
- **"move window left" wasn't implemented at all.** The trait gained directional move/focus,
  relative workspace, close, fullscreen, float and move-to-workspace. All return `Option` so a
  backend can DECLINE — omniwm's CLI docs don't cover close/fullscreen/next-workspace, so it
  reports "omniwm can't close window" rather than us guessing a subcommand name.
- Bonus conflict found the same way: the builtin table claimed `"close window"` for `close_tab`,
  and since the static table matches first, "close window" was closing a TAB. Each phrase now has
  exactly one owner.

**niri's scrolling layout needs care**: horizontal motion moves the whole COLUMN
(`move-column-left`), vertical motion moves the window inside it (`move-window-down`). Asking
for `move-window-left` is silently a no-op.

Verified with stand-in `niri`/`omniwmctl` binaries and a fixture desktop-entry dir: both
backends resolve focus/workspace correctly, IPC-disabled and no-WM-at-all both degrade with the
right message, and 27 unit tests cover the parsers and the matcher. Caught end-to-end and fixed:
`Exec=spotify --uri=%U` was passing the field code through, which would have made the app open a
file named `%U`. **The omniwm backend is written from its published IPC docs, not run against a
live daemon** — treat it as unexercised until someone tries it on the Mac.

## Voice commands + command table (as built)

```
voice text → normalize → [try whole utterance, else split chain] → rule match (+count)
           → Vec<Exec> → input worker            └─ no match ─→ llm_fallback (same allowlist)
```
One bounded core: a rule resolves to a registry action, a validated chord, literal text, or
argv — never a shell string. `main.rs` dispatches the steps in order on the one ordered
channel and replies with a single `command_result` whose `message` IS the canonical intent
("delete word ×3", "select all → copy"), because that toast is the whole eyes-off feedback
loop. Unmatched says which step failed.

**Rule schema** — `src/commands.default.toml` is the built-in table AND the documented
example; user file at `$XDG_CONFIG_HOME/remoat/commands.toml` (`REMOAT_CONFIG` overrides)
is merged on top and matched FIRST, which is what makes a user rule an override.

```toml
[[rules]]
phrases = ["delete word", "delete {n} words", "scratch that"]   # aliases; {n} = count slot
action = "delete_word"        # or: keys / keys_mac / keys_linux | text | launch /
                              # launch_mac / launch_linux | sequence = [{…}, …] | clarify
label = "…"                   # optional toast text; regex = true switches phrases to regexes
```

Invariants worth not breaking:
- Counts: `{n}` slot × trailing repeat (`twice`, `3 times`, `x4`). Capped `MAX_REPEAT` 40 per
  step, `MAX_CMDS` 200 per utterance — one mishearing must not fire 900 backspaces.
- Chains are all-or-nothing: if any step of "copy and frobnicate" is unmatched, NOTHING runs.
- The whole utterance is matched before splitting, so a rule phrase may contain "and"/"then".
- Ambiguity is a table entry, not code: `clarify = "select what?"` — bare "select" asks,
  never guesses. Bare "delete" = one character.
- Fail soft: each rule is validated at load (action exists, keys resolvable on this platform,
  one kind only); a bad rule is logged + skipped, the rest of the file still loads.
- No `shell = "…"` kind, ever. Config composes primitives; it cannot invent an escape out of
  the allowlist (matters once LAN auth lands).

Where the implementation deviates from the original design note:
- `actions::action()` returns `Vec<InputCmd>`, not one — linux `delete_line` needs
  shift+home *then* backspace.
- `Resolution.steps: Vec<Exec>` (Input | Launch | Note) instead of `Action::Sequence`, so a
  chain can mix input and launch ("open browser then paste"), and clarify/unavailable-app
  can report without injecting.
- Config kinds gained `launch_mac`/`launch_linux` (the built-in app table is platform-split)
  and `clarify`.
- Number words → digits globally only for unambiguous words; homophones (to/for/won/ate) are
  read as counts ONLY inside a `{n}` slot, so "go to the top" stays prose.
- `--check "<phrase>"` / `--actions` CLI added for checking table edits without a mic.

## Gotchas (hard-won — don't rediscover)

- **`lsof` hangs on the dev Mac** — use `pgrep`/`pkill` or a Python bind-test instead.
- **`ydotool mousemove -a` (absolute) is useless — don't reach for it again.** `ydotoold` never
  calls `UI_ABS_SETUP` (grep the 1.0.4 source: no `absinfo` anywhere), so `ABS_X`/`ABS_Y` keep
  `absmin=absmax=0` and *every* absolute coordinate collapses to 0,0. ABS isn't even enabled by
  default (`ENABLE_REL | ENABLE_KEY`). Upstream: ReimuNotMoe/ydotool#250. Shipped once on the
  strength of a dry-run argv check; on real niri hardware it parked the pointer in the top-left
  corner, which is **niri's default hot corner**, so the Overview opened every time. Absolute
  placement on Linux is therefore emulated from relative deltas (`input.rs::abs_to_rel`).
- **Nothing can pin both pointer axes without visiting a corner**, so relative backends home by
  slamming into one — never the top-left (niri's default hot corner, GNOME's Activities).
  `home_corner()` picks the nearest of the other three; a test asserts it never returns 0,0.
- **`CGEventPost` (macOS input injection) cannot report failure — full stop.** It's a `void` C
  function; without Accessibility trust it silently drops the event and every enigo call still
  returns `Ok(())`. Don't add error handling here expecting it to ever fire — it structurally
  can't. The only way to know trust state is to ask `AXIsProcessTrustedWithOptions` directly
  (`input.rs::check_accessibility_trust`), which is also the only way to raise the *correctly
  attributed* system prompt instead of guessing which process macOS means.
- **A dry-run argv check is not hardware verification.** Both ydotool bugs so far (the `-w`
  wheel flags, then this) passed dry-run and failed on the real box.
- **Screen geometry: everything outside a monitor's own mode is LOGICAL (post-scale).** Pointer
  positions and compositor layout positions live in logical coordinates; only
  `mode.width`/`height` (wlr-randr) and `width`/`height` (hyprctl) are physical. Pairing the two
  is invisible at scale 1 and silently wrong at any other scale — it inflated the desktop and
  put the cursor into uncovered space. Divide by `scale`, and prefer a compositor that hands
  you the logical rect outright (`niri msg --json outputs` → `logical`). Also: don't assume the
  layout origin is 0,0, and don't assume the union is fully covered by outputs.
- **`sherpa-onnx-sys` DOWNLOADS a prebuilt archive from GitHub in its build script** unless
  `SHERPA_ONNX_LIB_DIR` (or `SHERPA_ONNX_ARCHIVE_DIR`) is set — a sandboxed nix build cannot do
  that, so the flake sets it and so does the devShell. It also uses `ureq` with bundled roots,
  so it ignores a proxy CA: behind one, pre-download the archive and point
  `SHERPA_ONNX_ARCHIVE_DIR` at the directory holding it.
- **`cargo run`/`cargo test` find `libsherpa-onnx-c-api.so`, a direct `./target/debug/remoat`
  does not.** Cargo adds native link-search paths to the loader path; running the binary
  yourself needs `LD_LIBRARY_PATH`. Under nix this is a non-issue (rpath is set from the store
  path), but it will look like a broken build in a dev checkout.
- **A tool referenced in code is not a tool on PATH.** `wlr-randr` was in `screen.rs`'s fallback
  chain but never in the flake's `runtimeTools`, so on niri the whole chain missed and the
  1920x1080 default took over — with no error, because every probe "just" returned None. When
  adding a shell-out, add it to `runtimeTools` in the same commit, and make detection log what
  it actually found (`[screen]` now prints every monitor rect).
- **An inhibitor child outlives a killed daemon.** Measured: SIGTERM to the daemon left
  `caffeinate -d -i` asserting forever (nothing runs `Drop`). Hence the pid leash
  (`caffeinate -w <pid>`, `tail --pid=<pid>`) — the signal handler alone can't cover SIGKILL.
- **This Mac already runs an unrelated `caffeinate -d -i -u -t 1800`** (some other tool). When
  checking assertions, match our exact argv (`caffeinate -d -i -w`), or you'll misread it as
  a leak. `pmset -g assertions` also lists recently-dead pids, so confirm with `kill -0`.
- **rustls pulls `aws-lc-sys`** → the nix build needs `cmake` + `perl` in
  `nativeBuildInputs` (already added). A bare `cargo build` only worked locally because
  this Mac had `cmake` on PATH.
- **`Cargo.lock` must be committed** — `buildRustPackage` uses `cargoLock.lockFile`.
- **nix flakes only see git-tracked files**; **`nix run github:` caches ~1h** → use
  `--refresh` after pushing.
- **Certs in `$XDG_STATE_HOME/remoat`** (writable), not the read-only nix store.
- **Phone mic/Web-Speech need a secure context** → HTTPS is mandatory (self-signed cert).
- **ydotool is two parts**: `ydotoold` (daemon, owns socket + uinput) + `ydotool` (client);
  user must be in the socket's group. Set up via `programs.ydotool.enable` on NixOS.
- **enigo is macOS-only here** by design; all Linux uses ydotool (uinput works on X11 too),
  which keeps enigo's X11/Wayland system-lib deps out of the Linux build.
