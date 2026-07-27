# remoat — context for Claude Code

Phone→desktop remote control. iPhone PWA (touchpad + device dropdown + 2 push-to-talk
buttons: dictation + NL commands) ↔ desktop daemon that injects input. Targets macOS
and Linux (incl. Hyprland/Wayland). This file is the handoff so a fresh session (e.g.
cloud) has full context. Read the README too.

## Current status (2026-07-26)

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
- `src/commands.rs` — intent router: table first, then `llm_fallback` (documented seam,
  NOT wired). Bounded action allowlist by design.
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
- Wire the LLM/MCP command fallback (`src/commands.rs:llm_fallback`).
- Move STT onto the daemon (whisper.cpp) — drop an audio→text step before the WS handler.
- Confirm the Linux/Hyprland Rust build on the nixos box (unbuilt on the dev Mac).
- Build out voice editing commands + make them user-configurable — design below.

## Planned: voice commands + configurable command table (design, not built)

Goal: voice editing commands (backspace, delete-word, select-all, copy, undo, nav, …)
in the deterministic tier, plus a user-editable command table — all funnelling into one
bounded action surface the future LLM fallback also targets.

**Layering (one bounded core, three front-ends):**
```
voice text → normalize → grammar parse → [(action, count), …] → action(name) → InputCmd
                             └─ no match ─→ llm_fallback (emits the SAME (action,count) calls)
buttons (if ever) send `action` directly; nothing bypasses the allowlist.
```
- Add `pub fn action(name) -> Option<(String, InputCmd)>` in `commands.rs`: the single
  platform-mapped allowlist (delete_word = ⌥⌫ mac / ctrl+⌫ linux, copy = mod-chord, etc.).
  `route()`'s existing regex verbs fold into it; `llm_fallback`'s tool schema becomes
  `enum: [action names]`, so the LLM can't emit anything the deterministic tier can't.
- `Action::Sequence(Vec<InputCmd>)` so a command expands to N repeats / chains; the
  `command` arm in `main.rs` dispatches the sequence and sends ONE aggregated
  `command_result`.

**Voice-specific parser needs:**
- Normalize first: lowercase, strip `.,?!`, collapse spaces, canonicalize mishearings
  (`back space`→`backspace`), number-words→digits.
- Counts are the win over buttons: `delete three words`, `up five`, `backspace twice`
  → `{n}` repeat. Parse one..twenty, a/an→1, a couple→2, once/twice.
- Chain on ` and `/` then `/`,` → sequence.
- Eyes-off: `command_result` toast MUST echo the canonical intent (`delete word ×3`), and
  unmatched must say so. Ambiguity rules: bare "delete" = one char; "select" alone = toast
  "select what?", never guess.
- Backend gap: `home`/`end` not yet in `enigo_key`/`kc()` (add enigo `Key::Home/End`,
  ydotool `102`/`107`); `delete_line` linux path needs a combo. Rest of the table maps today.

**Configurable command table:** built-in table IS just the default config, same schema.
- `$XDG_CONFIG_HOME/remoat/commands.toml`; embedded default loaded first, user file merged
  on top. User rules match BEFORE built-ins (so a rule can override a default), then LLM.
- Rule = `phrases = [...]` (aliases + `{n}`) → one of a CLOSED set of action kinds:
  `action` (registry ref, stays platform-mapped) | `keys`/`keys_mac`/`keys_linux` (raw
  chord) | `text` (literal) | `launch` (argv) | `sequence` (compose the above).
  Deliberately NO `shell = "…"` — config composes primitives, can't invent a shell escape,
  preserving the bounded surface even for user rules (matters once auth lands).
- Re-read TOML on each command (mtime check, tiny file) = edit + save + speak, no restart.
- Fail soft: validate each rule's action/keys on load; unknown rule logged + skipped, rest
  still load — a bad line never bricks the daemon.
- `regex = true` per-rule as an opt-in for power users; default matcher stays alias+`{n}`.
- Later: a PWA settings screen edits the same TOML over WS (`{type:"config_set",…}`); the
  LLM can propose a rule that writes the same file. All roads → one config.

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
