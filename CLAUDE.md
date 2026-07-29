# remoat — context for Claude Code

Phone→desktop remote control. iPhone PWA (touchpad + device dropdown + 2 push-to-talk
buttons: dictation + NL commands) ↔ desktop daemon that injects input. Targets macOS
and Linux (incl. Hyprland/Wayland). This file is the handoff so a fresh session (e.g.
cloud) has full context. Read the README too.

## Current status (2026-07-27)

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

**Touchpad live capture BUILT** (`src/capture.rs`), from a cloud session with no display —
verified by `cargo build`/`cargo test` (region-clamping math + argv construction only, same
honesty as the ydotool dry-run caveat below) and by inspecting rendered WS traffic logic, NOT
by watching a real phone show a real screen. Treat platform capture as unexercised until
someone confirms it on hardware.
- The touchpad background used to be a CSS glow at the cursor's relative position; it's now a
  live PNG crop of the actual desktop around the cursor (`#padcap` in `web/app.js`), because a
  glow doesn't answer "what's actually under my finger right now."
- **Shells out to a platform screenshot tool, same pattern as ydotool for input**: `screencapture`
  on macOS, `grim` on Linux (wlr-screencopy — works on Hyprland and other wlroots compositors,
  NOT a portal flow, so no per-frame permission dialog). `grim` is in the flake's Linux
  `runtimeTools`; `screencapture` is a macOS system binary, nothing to add.
- **Driven off `move_abs`, not its own timer.** The client sends `{type:"capture",on:true}` on
  touchstart / `false` on touchend (mirrors the wake-lock on/off pattern); while on, the daemon
  captures a crop centered on the position it *just moved to* and pushes it back as a binary WS
  frame, throttled to `CAPTURE_INTERVAL` (120ms, ~8fps — picked, not profiled). A crossing
  mid-drag moves the capture request to the new active device the same way it moves the wake
  lock (`setActive` in `app.js`).
- Crop size defaults to 480×300, overridable with `REMOAT_CAPTURE_SIZE=WxH` (mirrors
  `REMOAT_SCREEN`). Region math shrinks-and-clamps to fit inside the actual desktop bounds.

First real-hardware feedback (nixos): laggy, and the crop was offset well to the left of the
real cursor. Two different bugs:
- **The lag was real and is fixed.** The first cut awaited the capture subprocess *inline* in
  the same per-connection loop that also reads incoming WS messages — so every touchmove sent
  while a screenshot was mid-flight queued behind it, stalling cursor movement itself, not just
  the image. Fixed by splitting the socket (`futures_util::StreamExt/SinkExt`) into a shared
  `Arc<Mutex<SplitSink>>` and a receive-only stream; `move_abs` now `tokio::spawn`s the capture
  detached and sends the frame whenever it's ready, gated by an `AtomicBool` so a slow capture
  can't pile up a queue of overlapping subprocesses. The receive loop never waits on it.
- **The offset is still open — root cause unconfirmed.** Leading theory: Hyprland positions
  monitors in *physical* pixels (`hyprctl monitors -j`'s x/y/width/height, matching how
  `screen.rs` already reads them), but grim's `-g` geometry — inherited from `slurp`, meant for
  `wl_output`/xdg-output — is generally understood to want *logical* (post-scale) coordinates.
  On an unscaled display the two coincide and nothing would look wrong; on a scaled one
  (fractional or 2x, common on a Linux desktop) they don't, which would produce exactly a
  fixed-direction offset. Second, weaker theory: the already-documented ydotool relative-motion
  drift (see "Multi-device arrangement" above) — the crop is centered on the daemon's *believed*
  position, which can differ from the real one. Distinguishing test: if the offset is present
  immediately after connecting (before any dragging), it's the scale theory; if it's absent
  right after crossing an edge (a `home` resync) and grows the further you drag from there, it's
  drift. Whoever picks this up next: get `hyprctl monitors -j`'s `"scale"` field first.
- **No auth gate on this** (see "Open threads" — it was already the top-priority gap for input;
  this adds live screen contents to what an unauthenticated LAN client can pull).

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
- `src/screen.rs` — desktop bounds (env override → Quartz via enigo → hyprctl/wlr-randr/xrandr
  → 1920x1080). Origin assumed 0,0; also clamps incoming `move_abs`.
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

## Open threads / next steps

- **Security (top priority, raised by user):** no auth — anyone on the LAN can drive the
  machine. Add a shared-token/pairing check. (Port 8443 itself is fine; TLS is on but
  self-signed.)
- Wire the LLM/MCP command fallback (`src/commands.rs:llm_fallback`). The tool schema's
  action parameter should be `enum: actions::NAMES` + an integer count, so the model can
  emit nothing the deterministic tier couldn't.
- Move STT onto the daemon (whisper.cpp) — drop an audio→text step before the WS handler.
- Confirm the Linux/Hyprland Rust build on the nixos box (unbuilt on the dev Mac). Same
  trip: exercise the linux chords in `actions.rs` — `delete_line` (shift+home, backspace),
  `line_start`/`line_end` (home/end), `doc_start`/`doc_end` (ctrl+home/end). Keycodes are
  unit-tested (`input.rs::keyargs`), the *bindings* aren't.
- PWA settings screen that edits `commands.toml` over WS (`{type:"config_set",…}`) — the
  file format is ready for it; the WS message is not implemented.
- Exact pointer tracking on Linux needs a real absolute device: either remoat creating its own
  uinput device WITH an ABS range (needs `/dev/uinput` access, like ydotool's group) or the
  `wlr-virtual-pointer` protocol's `motion_absolute` — niri support unverified. Either removes
  the acceleration drift and the corner-homing slam entirely.
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
- **Touchpad capture (`src/capture.rs`) is entirely unverified on real hardware** — built and
  argv-checked from a cloud session with no display. On macOS, `screencapture` needs Screen
  Recording permission granted to the launching terminal (separate from the Accessibility grant
  input already needs) — untested whether it fails loud or silently without it. On Linux,
  confirm `grim` actually captures under Hyprland from this daemon (not just from a terminal)
  and that `-g` geometry syntax is still correct for the installed grim version. Also unverified:
  whether 8fps/480×300 is remotely the right tradeoff for real Wi-Fi + a real screenshot's
  actual latency — CAPTURE_INTERVAL and the crop size are guesses, not measurements.
- Screen capture currently ships with **no auth**, same LAN-exposure gap as input (see the
  security item above) — now covering live screen contents too, not just injection.

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
- **A dry-run argv check is not hardware verification.** Both ydotool bugs so far (the `-w`
  wheel flags, then this) passed dry-run and failed on the real box.
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
