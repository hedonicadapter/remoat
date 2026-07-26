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
- From source: `nix develop .#rust --command cargo run`.
- macOS injection needs Accessibility granted to the launching terminal (rotating
  `/nix/store` path makes granting the store binary impractical — grant the terminal).

## Open threads / next steps

- **Security (top priority, raised by user):** no auth — anyone on the LAN can drive the
  machine. Add a shared-token/pairing check. (Port 8443 itself is fine; TLS is on but
  self-signed.)
- Wire the LLM/MCP command fallback (`src/commands.rs:llm_fallback`).
- Move STT onto the daemon (whisper.cpp) — drop an audio→text step before the WS handler.
- Confirm the Linux/Hyprland Rust build on the nixos box (unbuilt on the dev Mac).
- ydotool `scroll` uses `mousemove --wheel` — least version-stable call; may need a tweak.

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
