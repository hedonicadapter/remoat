//! Intent router. Tier 1: deterministic table (fast, offline, safe).
//! Tier 2: llm_fallback seam (stub). Actions are a bounded surface — never arbitrary shell.

use std::process::Command;

use regex::Regex;

use crate::input::InputCmd;

pub enum Action {
    Launch(Vec<String>),
    Input(InputCmd),
    None(String),
}

pub struct Resolution {
    pub matched: bool,
    pub label: String,
    pub action: Action,
}

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

fn modk(k: &str) -> Vec<String> {
    if cfg!(target_os = "macos") { vec!["command".into(), k.into()] } else { vec!["ctrl".into(), k.into()] }
}

fn nav(dir: &str) -> Vec<String> {
    let key = if dir == "back" { "left" } else { "right" };
    if cfg!(target_os = "macos") { vec!["command".into(), key.into()] } else { vec!["alt".into(), key.into()] }
}

fn input_action(q: &str) -> Option<(String, InputCmd)> {
    if let Some(c) = re(r"\bscroll (down|up)\b").captures(q) {
        let dir = &c[1];
        let dy = if dir == "down" { -120.0 } else { 120.0 };
        return Some((format!("scroll {dir}"), InputCmd::Scroll { dy }));
    }
    if re(r"\bgo back\b").is_match(q) { return Some(("go back".into(), InputCmd::Hotkey(nav("back")))); }
    if re(r"\bgo forward\b").is_match(q) { return Some(("go forward".into(), InputCmd::Hotkey(nav("forward")))); }
    if re(r"\bcopy\b").is_match(q) { return Some(("copy".into(), InputCmd::Hotkey(modk("c")))); }
    if re(r"\bpaste\b").is_match(q) { return Some(("paste".into(), InputCmd::Hotkey(modk("v")))); }
    if re(r"\bselect all\b").is_match(q) { return Some(("select all".into(), InputCmd::Hotkey(modk("a")))); }
    if re(r"\b(enter|return|confirm)\b").is_match(q) { return Some(("enter".into(), InputCmd::Key("enter".into()))); }
    if re(r"\b(escape|cancel|dismiss)\b").is_match(q) { return Some(("escape".into(), InputCmd::Key("esc".into()))); }
    None
}

// (pattern, app-key) — most specific first.
const APP_TRIGGERS: &[(&str, &str)] = &[
    (r"\b(web ?browser|browser|safari|chrome|firefox|internet)\b", "browser"),
    (r"\b(terminal|shell|console|iterm)\b", "terminal"),
    (r"\b(finder|file manager|files|explorer)\b", "finder"),
    (r"\b(notes?)\b", "notes"),
    (r"\b(mail|email)\b", "mail"),
    (r"\b(music|spotify|itunes)\b", "music"),
    (r"\b(calendar|agenda)\b", "calendar"),
    (r"\b(vs ?code|code editor|editor|vscode)\b", "code"),
];

fn app_argv(key: &str) -> Vec<String> {
    let mac = cfg!(target_os = "macos");
    let v: Vec<&str> = match key {
        "browser" => if mac { vec!["open", "-a", "Safari"] } else { vec!["xdg-open", "https://start.duckduckgo.com"] },
        "terminal" => if mac { vec!["open", "-a", "Terminal"] } else { vec!["x-terminal-emulator"] },
        "finder" => if mac { vec!["open", "-a", "Finder"] } else { vec!["xdg-open", "."] },
        "notes" => if mac { vec!["open", "-a", "Notes"] } else { vec![] },
        "mail" => if mac { vec!["open", "-a", "Mail"] } else { vec!["xdg-open", "mailto:"] },
        "music" => if mac { vec!["open", "-a", "Music"] } else { vec![] },
        "calendar" => if mac { vec!["open", "-a", "Calendar"] } else { vec![] },
        "code" | "editor" => if mac { vec!["open", "-a", "Visual Studio Code"] } else { vec!["code"] },
        _ => vec![],
    };
    v.into_iter().map(String::from).collect()
}

pub fn route(text: &str) -> Resolution {
    let q = text.trim().to_lowercase();
    if q.is_empty() {
        return Resolution { matched: false, label: String::new(), action: Action::None("empty".into()) };
    }

    // Tier 1a: input-effect commands
    if let Some((label, cmd)) = input_action(&q) {
        return Resolution { matched: true, label, action: Action::Input(cmd) };
    }

    // Tier 1b: app launch (verb optional — bare "browser" resolves too)
    for (pat, key) in APP_TRIGGERS {
        if re(pat).is_match(&q) {
            let argv = app_argv(key);
            let label = format!("open {key}");
            if argv.is_empty() {
                return Resolution { matched: true, label, action: Action::None(format!("{key} not available on this platform")) };
            }
            return Resolution { matched: true, label, action: Action::Launch(argv) };
        }
    }

    // Tier 2: LLM fallback (stub)
    llm_fallback(&q)
}

/// Run a launch action; returns a human-readable result. Input actions dispatch via the worker.
pub fn execute_launch(argv: &[String]) -> String {
    if argv.is_empty() { return "no-op".into(); }
    match Command::new(&argv[0]).args(&argv[1..]).spawn() {
        Ok(_) => format!("launched: {}", argv.join(" ")),
        Err(e) => format!("failed: {e}"),
    }
}

/// SEAM: route the long tail via an LLM constrained to the bounded tool surface.
/// Send `text` + a tool schema (open_app, focus_window, type_text, ...) to Claude/local
/// model with tool-calling, map the chosen call back to a Resolution. Keep the tool set ==
/// the allowlist above. Not wired in this POC.
fn llm_fallback(text: &str) -> Resolution {
    Resolution {
        matched: false,
        label: String::new(),
        action: Action::None(format!("no rule matched: {text:?} (LLM fallback not configured)")),
    }
}
