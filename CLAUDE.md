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
