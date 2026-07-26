"""Intent router. Tier 1: deterministic table (fast, offline, safe).
Tier 2: LLM fallback seam (stub) for the fuzzy long tail.

Actions are a bounded surface — never arbitrary shell. Add capabilities here on purpose.
"""
import re
import shutil
import subprocess
import sys

PLATFORM = sys.platform

# App name -> per-platform launch argv. Bounded allowlist by design.
_APPS = {
    "browser": {"darwin": ["open", "-a", "Safari"], "linux": ["xdg-open", "https://start.duckduckgo.com"]},
    "terminal": {"darwin": ["open", "-a", "Terminal"], "linux": ["x-terminal-emulator"]},
    "finder": {"darwin": ["open", "-a", "Finder"], "linux": ["xdg-open", "."]},
    "files": {"darwin": ["open", "-a", "Finder"], "linux": ["xdg-open", "."]},
    "notes": {"darwin": ["open", "-a", "Notes"], "linux": []},
    "mail": {"darwin": ["open", "-a", "Mail"], "linux": ["xdg-open", "mailto:"]},
    "music": {"darwin": ["open", "-a", "Music"], "linux": []},
    "calendar": {"darwin": ["open", "-a", "Calendar"], "linux": []},
    "code": {"darwin": ["open", "-a", "Visual Studio Code"], "linux": ["code"]},
    "editor": {"darwin": ["open", "-a", "Visual Studio Code"], "linux": ["code"]},
}

# Phrase triggers -> app key. Longest/most-specific first.
_APP_TRIGGERS = [
    (r"\b(web ?browser|browser|safari|chrome|firefox|internet)\b", "browser"),
    (r"\b(terminal|shell|console|iterm)\b", "terminal"),
    (r"\b(finder|file manager|files|explorer)\b", "finder"),
    (r"\b(notes?)\b", "notes"),
    (r"\b(mail|email)\b", "mail"),
    (r"\b(music|spotify|itunes)\b", "music"),
    (r"\b(calendar|agenda)\b", "calendar"),
    (r"\b(vs ?code|code editor|editor|vscode)\b", "code"),
]

# Direct input-effect commands -> input worker payload.
_INPUT_TRIGGERS = [
    (r"\bscroll (down|up)\b", lambda m: {"type": "scroll", "dy": -120 if m.group(1) == "down" else 120}),
    (r"\bgo back\b", lambda m: {"type": "hotkey", "keys": _nav("back")}),
    (r"\bgo forward\b", lambda m: {"type": "hotkey", "keys": _nav("forward")}),
    (r"\b(copy)\b", lambda m: {"type": "hotkey", "keys": _mod("c")}),
    (r"\b(paste)\b", lambda m: {"type": "hotkey", "keys": _mod("v")}),
    (r"\b(select all)\b", lambda m: {"type": "hotkey", "keys": _mod("a")}),
    (r"\b(enter|return|confirm)\b", lambda m: {"type": "key", "key": "enter"}),
    (r"\b(escape|cancel|dismiss)\b", lambda m: {"type": "key", "key": "esc"}),
]


def _mod(k: str):
    return ["command", k] if PLATFORM == "darwin" else ["ctrl", k]


def _nav(direction: str):
    if PLATFORM == "darwin":
        return ["command", "left" if direction == "back" else "right"]
    return ["alt", "left" if direction == "back" else "right"]


class Resolution:
    def __init__(self, matched, label, kind, payload=None, message=""):
        self.matched = matched
        self.label = label      # human-readable action name
        self.kind = kind        # "launch" | "input" | "none"
        self.payload = payload  # argv (launch) or input dict (input)
        self.message = message

    def as_dict(self):
        return {"matched": self.matched, "label": self.label, "kind": self.kind, "message": self.message}


def route(text: str) -> Resolution:
    q = text.strip().lower()
    if not q:
        return Resolution(False, "", "none", message="empty")

    # Tier 1a: input-effect commands
    for pat, build in _INPUT_TRIGGERS:
        m = re.search(pat, q)
        if m:
            return Resolution(True, m.group(0), "input", payload=build(m))

    # Tier 1b: app launch. Verb optional — bare "browser" resolves too.
    for pat, key in _APP_TRIGGERS:
        if re.search(pat, q):
            argv = _APPS[key].get(PLATFORM) or []
            if not argv or (argv[0] != "open" and not shutil.which(argv[0])):
                return Resolution(True, f"open {key}", "launch", message=f"{key} not available on this platform")
            return Resolution(True, f"open {key}", "launch", payload=argv)

    # Tier 2: LLM fallback (stub)
    return llm_fallback(q)


def execute(res: Resolution) -> str:
    """Run a launch action. Input actions are dispatched by caller to the input worker."""
    if res.kind == "launch" and res.payload:
        try:
            subprocess.Popen(res.payload)  # spawn, non-blocking
            return f"launched: {res.label}"
        except Exception as e:
            return f"failed: {e}"
    return res.message or "no-op"


def llm_fallback(text: str) -> Resolution:
    """SEAM: route the long tail via an LLM constrained to the bounded tool surface.

    Plug in here: send `text` + a tool schema (open_app, focus_window, type_text,
    set_volume, move_mouse, ...) to Claude (or a local model) with tool-calling,
    then map the chosen tool call back to a Resolution. Keep the tool set == the
    allowlist above so the model can never exceed granted capabilities.

    Not wired in this POC — returns unmatched so nothing unexpected fires.
    """
    return Resolution(False, "", "none", message=f"no rule matched: {text!r} (LLM fallback not configured)")
