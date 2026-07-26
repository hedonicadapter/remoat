# remoat — phone → desktop remote control (POC)

Thin phone client (PWA) ↔ fat desktop daemon. Touchpad, device dropdown, and two
push-to-talk buttons (dictation + natural-language commands). Targets macOS and
Linux; input is injected via `enigo` (macOS) and `ydotool` (Linux, uinput).

The daemon is **Rust** (`src/`), distributed as a single binary via a Nix flake. A
Python proof-of-concept remains under `daemon/` for reference — the Rust daemon speaks
the identical WS protocol and serves the same `web/` PWA. Architecture: **fat daemon /
thin phone / the daemon consumes _text_** so the transcription source stays swappable.

## Architecture

```
 iPhone (Safari PWA)                    Mac / Linux daemon (this repo)
 ┌───────────────────┐   WSS (JSON)    ┌──────────────────────────────┐
 │ touchpad gestures │ ───────────────▶│ input worker: enigo / ydotool│
 │ device dropdown   │ ◀─ devices ─────│ mDNS advertise + browse      │
 │ PTT: dictation    │ ── text ───────▶│ typed on target              │
 │ PTT: command      │ ── command ────▶│ intent router → bounded acts │
 └───────────────────┘                 └──────────────────────────────┘
        Web Speech API                    (LLM fallback = documented seam)
```

- **Transcription happens on the phone** (Web Speech API) in this POC → the daemon
  receives text. To move STT onto the daemon (whisper.cpp etc.) later, add an
  audio→text step in front of the WS handler; nothing else changes because the
  daemon already speaks text.
- **Commands** are routed by a deterministic table first (fast, offline, safe). The
  LLM/MCP fallback is a clearly marked seam in `src/commands.rs:llm_fallback`.
- **Device dropdown** is populated by mDNS peers the daemon discovers (browsers
  can't do mDNS). Selecting a peer reconnects the WS to it.

## Run with Nix (recommended on Linux / Hyprland / NixOS)

```bash
nix run github:hedonicadapter/remoat          # serves https://<lan-ip>:8443
nix run github:hedonicadapter/remoat -- --port 9000
```

The flake builds the Rust daemon as a single binary and bundles the PWA plus the tools
it shells out to on Linux (`ydotool`, `wl-clipboard`, `xdg-utils`) — no `pacman`/`pip`.
Certs and state go in `$XDG_STATE_HOME/remoat` (writable; the store is read-only).

`nix run` cannot grant `/dev/uinput` access, which `ydotool` needs on Wayland — see
[Wayland input permissions](#wayland-input-permissions) below.

Other entry points:
```bash
nix develop github:hedonicadapter/remoat      # dev shell: deps + tools on PATH
nix build  github:hedonicadapter/remoat       # ./result/bin/remoat
```

> Flakes only see git-tracked files — `git add` new files before `nix run` picks
> them up from your repo.

## Run from source (Rust)

```bash
cd ~/Documents/projects/remoat
nix develop .#rust --command cargo run          # or: cargo run -- --port 9000
```
`REMOAT_WEB_DIR` defaults to `./web` from the source tree, so `cargo run` finds the PWA.

<details><summary>Legacy Python POC (<code>daemon/</code>)</summary>

```bash
./run.sh                     # venv-based; serves https://<lan-ip>:8443
```
Kept for reference only; the Rust daemon is the one the flake builds.
</details>

First run generates a self-signed cert (needed so the phone gets a **secure
context** — mic/Web-Speech are blocked over plain http on LAN).

### Grant input permission (macOS)
`pyautogui` needs Accessibility. Run once, then:
System Settings → Privacy & Security → **Accessibility** → enable your terminal
(Terminal.app / iTerm / VS Code — whatever launched `run.sh`). Restart the daemon.

### On the phone (same Wi-Fi)
1. Open `https://<lan-ip>:8443` in Safari.
2. Accept the self-signed cert warning (once).
3. Add to Home Screen for a fullscreen app (optional).

## Run (Linux / Hyprland / Wayland)

The daemon auto-selects an input backend:

| session | backend | how |
|---|---|---|
| macOS | `pyautogui` | Quartz |
| Linux + X11 | `pyautogui` | XTEST |
| Linux + **Wayland** (Hyprland, etc.) | `ydotool` | kernel `uinput` — global, compositor-agnostic |

Override with `REMOAT_BACKEND=pyautogui|ydotool`. Detection uses `WAYLAND_DISPLAY` /
`XDG_SESSION_TYPE`.

<a name="wayland-input-permissions"></a>
**Wayland input permissions (one-time, system-level):** pyautogui/XTEST cannot inject
into a Wayland compositor, so Hyprland uses `ydotool`, which needs the `ydotoold`
daemon + access to `/dev/uinput`. `nix run` ships the `ydotool` binary but can't grant
kernel/device perms — that's system config:

**NixOS (cleanest — sets up the daemon and perms for you):**
```nix
programs.ydotool.enable = true;   # then re-login; ydotoold runs as your user
```

**Non-NixOS (manual udev rule):**
```bash
sudo modprobe uinput
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' | \
  sudo tee /etc/udev/rules.d/80-uinput.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG input "$USER"     # re-login after this
ydotoold &                         # start the daemon (nix provides the binary)
```

Verify ydotool works before wiring the phone:
```bash
ydotool mousemove -x 100 -y 0      # cursor should jump right
```
Dry-run what the daemon *would* send without executing:
```bash
REMOAT_BACKEND=ydotool REMOAT_DRYRUN=1 ./run.sh   # logs `ydotool …` argv instead of injecting
```

> ⚠️ **Untested on real Hyprland** — this was built on macOS. The ydotool argv is
> verified by dry-run, but `ydotool` flag syntax varies by version. If a command
> misbehaves, check `ydotool <subcmd> --help` and adjust `daemon/input.py`
> (`YdotoolBackend`). Scroll uses `mousemove --wheel`, which is the least version-
> stable call — most likely to need a tweak.

## Gestures
| gesture | action |
|---|---|
| one-finger drag | move cursor |
| one-finger tap | left click |
| two-finger tap | right click |
| two-finger drag | scroll |

## Voice
- **Hold to type** → dictation typed into the focused field (via clipboard paste, so
  unicode/punctuation survive).
- **Hold to command** → e.g. "open browser", "scroll down", "go back", "copy".
  See `daemon/commands.py` for the full table.

## Known POC limits / next steps
- **Wayland input injection**: handled via a `ydotool` backend (see Linux run
  section) — auto-selected on Hyprland/Wayland. Written but not yet tested on real
  hardware; `ydotool` version quirks may need small tweaks in `YdotoolBackend`.
- Web Speech API on iOS routes audio to Apple servers and can be flaky; the daemon-
  side whisper.cpp path (see above) is the robust upgrade.
- No pairing/auth yet. Before real use: device pairing + per-device token, and keep
  the action surface bounded (already the case).
- Multi-device: selecting a *different* daemon needs that daemon's cert trusted too
  (self-signed). Fine for single-machine testing.

## Layout
```
src/             Rust daemon (the one the flake builds)
  main.rs        axum: static PWA + /ws + HTTPS wiring
  input.rs       backends: enigo (macOS) + ydotool (Linux) + null; worker thread
  commands.rs    intent router + bounded action allowlist + LLM seam
  discovery.rs   mdns-sd advertise/browse
  tls.rs         self-signed cert (rcgen) in $XDG_STATE_HOME
web/             the PWA (index.html, app.js, style.css, manifest.json)
daemon/          legacy Python POC (reference only)
flake.nix        buildRustPackage + apps.default (nix run)
```

# TODO: is 8443 secure
