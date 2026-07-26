"""Input injection with swappable backends.

  pyautogui  -> macOS + Linux/X11 (via Quartz / XTEST)
  ydotool    -> Linux/Wayland incl. Hyprland (via kernel uinput — global, compositor-agnostic)

Selection is automatic (see pick_backend); override with REMOAT_BACKEND=pyautogui|ydotool.
Set REMOAT_DRYRUN=1 to log the ydotool argv instead of executing (for testing without uinput).

Runs on a dedicated worker thread — injection calls block; keep them off the asyncio loop.
"""
import os
import queue
import shutil
import subprocess
import sys
import threading

PLATFORM = sys.platform  # "darwin" | "linux" | "win32"
DRYRUN = bool(os.environ.get("REMOAT_DRYRUN"))


# ─── backend interface ────────────────────────────────────────────────────────
class Backend:
    name = "base"

    def move(self, dx, dy): ...
    def click(self, button, double): ...
    def scroll(self, dy): ...
    def text(self, s): ...
    def hotkey(self, keys): ...   # keys: list of names e.g. ["ctrl","c"]
    def key(self, k): ...         # single name e.g. "enter"


class NullBackend(Backend):
    """No-op backend when no injection backend is available — daemon still serves the UI."""
    name = "null"

    def __init__(self, reason):
        print(f"[input] no injection backend ({reason}) — serving UI only, input disabled",
              file=sys.stderr)


# ─── pyautogui backend (mac / X11) ─────────────────────────────────────────────
class PyAutoGUIBackend(Backend):
    name = "pyautogui"

    def __init__(self):
        import pyautogui  # lazy: on Linux this needs an X display; not imported on Wayland
        pyautogui.PAUSE = 0
        pyautogui.FAILSAFE = False
        self.g = pyautogui

    def move(self, dx, dy):
        self.g.moveRel(dx, dy, duration=0)

    def click(self, button, double):
        (self.g.doubleClick if double else self.g.click)(button=button)

    def scroll(self, dy):
        if dy:
            self.g.scroll(-int(dy))  # natural: swipe up scrolls content up

    def text(self, s):
        if PLATFORM == "darwin":
            subprocess.run(["pbcopy"], input=s.encode(), check=False)
            self.g.hotkey("command", "v")
            return
        for cmd in (["wl-copy"], ["xclip", "-selection", "clipboard"]):  # X11 clipboard paste
            try:
                subprocess.run(cmd, input=s.encode(), check=True)
                self.g.hotkey("ctrl", "v")
                return
            except (FileNotFoundError, subprocess.CalledProcessError):
                continue
        self.g.write(s)  # last resort: keystroke sim (ASCII only)

    def hotkey(self, keys):
        self.g.hotkey(*keys)

    def key(self, k):
        self.g.press(k)


# ─── ydotool backend (Wayland / Hyprland) ──────────────────────────────────────
# Linux input-event keycodes (kernel constants — stable across ydotool versions).
_KC = {
    "ctrl": 29, "control": 29, "alt": 56, "shift": 42, "super": 125, "command": 125, "meta": 125,
    "a": 30, "c": 46, "v": 47, "x": 45, "z": 44,
    "left": 105, "right": 106, "up": 103, "down": 108,
    "enter": 28, "return": 28, "esc": 1, "escape": 1, "tab": 15, "space": 57, "backspace": 14,
}
# ydotool click bitmask: 0x40=down 0x80=up (0xC0=full click), low nibble = button (0 L,1 R,2 M)
_BTN = {"left": "0xC0", "right": "0xC1", "middle": "0xC2"}


class YdotoolBackend(Backend):
    name = "ydotool"

    def __init__(self):
        if not DRYRUN and not shutil.which("ydotool"):
            print("[input] WARNING: ydotool not found — Wayland injection will fail. "
                  "Install ydotool + run ydotoold (see README).", file=sys.stderr)

    def _run(self, *args):
        argv = ["ydotool", *args]
        if DRYRUN:
            print("[dryrun] " + " ".join(argv))
            return
        subprocess.run(argv, check=False)

    def move(self, dx, dy):
        # relative is ydotool's default; -x/-y are deltas
        self._run("mousemove", "-x", str(int(round(dx))), "-y", str(int(round(dy))))

    def click(self, button, double):
        code = _BTN.get(button, "0xC0")
        self._run("click", code)
        if double:
            self._run("click", code)

    def scroll(self, dy):
        # best-effort: wheel support is version-dependent in ydotool; wrapped so it never errors
        if not dy:
            return
        clicks = int(round(-dy / 40)) or (-1 if dy > 0 else 1)
        self._run("mousemove", "--wheel", "-y", str(clicks))

    def text(self, s):
        # clipboard + paste keeps unicode/punctuation intact (matches the mac path)
        if not DRYRUN and shutil.which("wl-copy"):
            subprocess.run(["wl-copy"], input=s.encode(), check=False)
            self._keyseq(["ctrl", "v"])
        else:
            self._run("type", s)

    def hotkey(self, keys):
        self._keyseq(keys)

    def key(self, k):
        self._keyseq([k])

    def _keyseq(self, names):
        codes = [_KC[n] for n in names if n in _KC]
        if not codes:
            return
        press = [f"{c}:1" for c in codes]
        release = [f"{c}:0" for c in reversed(codes)]
        self._run("key", *press, *release)


# ─── selection ──────────────────────────────────────────────────────────────
def pick_backend() -> Backend:
    choice = os.environ.get("REMOAT_BACKEND", "auto").lower()
    if choice == "auto":
        if PLATFORM.startswith("linux") and (
            os.environ.get("WAYLAND_DISPLAY") or os.environ.get("XDG_SESSION_TYPE") == "wayland"
        ):
            choice = "ydotool"
        else:
            choice = "pyautogui"
    try:
        return YdotoolBackend() if choice == "ydotool" else PyAutoGUIBackend()
    except Exception as e:  # missing/broken backend dep must not kill the daemon
        return NullBackend(f"{choice}: {e}")


# ─── worker ───────────────────────────────────────────────────────────────────
class InputWorker:
    """Single-thread input executor. Thread-safe enqueue from asyncio."""

    def __init__(self) -> None:
        self._q: "queue.Queue[dict]" = queue.Queue()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self.backend = pick_backend()
        print(f"[input] backend: {self.backend.name}")

    def start(self) -> None:
        self._thread.start()

    def send(self, cmd: dict) -> None:
        self._q.put_nowait(cmd)

    def _run(self) -> None:
        b = self.backend
        while True:
            c = self._q.get()
            t = c.get("type")
            try:
                if t == "move":
                    b.move(c["dx"], c["dy"])
                elif t == "click":
                    b.click(c.get("button", "left"), c.get("double", False))
                elif t == "scroll":
                    b.scroll(c.get("dy", 0))
                elif t == "text":
                    b.text(c["text"])
                elif t == "hotkey":
                    b.hotkey(c["keys"])
                elif t == "key":
                    b.key(c["key"])
            except Exception as e:  # never let one bad event kill the worker
                print(f"[input] error applying {t}: {e}", file=sys.stderr)
