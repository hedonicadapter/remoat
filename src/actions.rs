//! The bounded action allowlist — one platform-mapped surface.
//!
//! Voice grammar, `commands.toml` rules and the LLM fallback all resolve to a name in
//! `NAMES`; nothing else reaches the input worker. `action()` returns a Vec because a few
//! actions need a combo (linux `delete_line` = shift+home then backspace).

use crate::input::InputCmd;

/// Every action name. Also the `enum` a constrained LLM fallback is allowed to emit.
pub const NAMES: &[&str] = &[
    // editing
    "backspace", "forward_delete", "delete_word", "delete_line",
    "undo", "redo", "cut", "copy", "paste", "save", "find",
    "select_all", "select_word", "select_line",
    // caret / nav
    "left", "right", "up", "down", "word_left", "word_right",
    "line_start", "line_end", "doc_start", "doc_end", "page_up", "page_down",
    "enter", "escape", "tab", "space",
    // window / app
    "go_back", "go_forward", "refresh", "new_tab", "close_tab",
    "next_tab", "prev_tab", "switch_app",
    // pointer
    "click", "right_click", "double_click", "scroll_up", "scroll_down",
];

fn hk(keys: &[&str]) -> Vec<InputCmd> {
    vec![InputCmd::Hotkey(keys.iter().map(|s| s.to_string()).collect())]
}
fn key(name: &str) -> Vec<InputCmd> {
    vec![InputCmd::Key(name.into())]
}
/// Platform split for a single chord.
fn chord(mac: &[&str], linux: &[&str]) -> Vec<InputCmd> {
    if cfg!(target_os = "macos") { hk(mac) } else { hk(linux) }
}
/// mod = command on mac, ctrl elsewhere.
fn modk(k: &str) -> Vec<InputCmd> {
    chord(&["command", k], &["ctrl", k])
}

/// Canonical label: `delete_word` -> `delete word`. What the toast echoes.
pub fn label(name: &str) -> String {
    name.replace('_', " ")
}

pub fn action(name: &str) -> Option<(String, Vec<InputCmd>)> {
    let mac = cfg!(target_os = "macos");
    let cmds: Vec<InputCmd> = match name {
        // ─── editing ───
        "backspace" => key("backspace"),
        "forward_delete" => key("delete"),
        "delete_word" => chord(&["alt", "backspace"], &["ctrl", "backspace"]),
        // mac cmd+backspace kills to line start; linux has no single chord for it.
        "delete_line" => {
            if mac {
                hk(&["command", "backspace"])
            } else {
                vec![
                    InputCmd::Hotkey(vec!["shift".into(), "home".into()]),
                    InputCmd::Key("backspace".into()),
                ]
            }
        }
        "undo" => modk("z"),
        "redo" => chord(&["command", "shift", "z"], &["ctrl", "shift", "z"]),
        "cut" => modk("x"),
        "copy" => modk("c"),
        "paste" => modk("v"),
        "save" => modk("s"),
        "find" => modk("f"),
        "select_all" => modk("a"),
        // Extend the selection one word/line backwards from the caret.
        "select_word" => chord(&["alt", "shift", "left"], &["ctrl", "shift", "left"]),
        "select_line" => chord(&["command", "shift", "left"], &["shift", "home"]),

        // ─── caret / nav ───
        "left" => key("left"),
        "right" => key("right"),
        "up" => key("up"),
        "down" => key("down"),
        "word_left" => chord(&["alt", "left"], &["ctrl", "left"]),
        "word_right" => chord(&["alt", "right"], &["ctrl", "right"]),
        "line_start" => {
            if mac { hk(&["command", "left"]) } else { key("home") }
        }
        "line_end" => {
            if mac { hk(&["command", "right"]) } else { key("end") }
        }
        "doc_start" => {
            if mac { hk(&["command", "up"]) } else { hk(&["ctrl", "home"]) }
        }
        "doc_end" => {
            if mac { hk(&["command", "down"]) } else { hk(&["ctrl", "end"]) }
        }
        "page_up" => key("pageup"),
        "page_down" => key("pagedown"),
        "enter" => key("enter"),
        "escape" => key("esc"),
        "tab" => key("tab"),
        "space" => key("space"),

        // ─── window / app ───
        "go_back" => chord(&["command", "left"], &["alt", "left"]),
        "go_forward" => chord(&["command", "right"], &["alt", "right"]),
        "refresh" => modk("r"),
        "new_tab" => modk("t"),
        "close_tab" => modk("w"),
        "next_tab" => hk(&["ctrl", "tab"]),
        "prev_tab" => hk(&["ctrl", "shift", "tab"]),
        "switch_app" => chord(&["command", "tab"], &["alt", "tab"]),

        // ─── pointer ───
        "click" => vec![InputCmd::Click { button: "left".into(), double: false }],
        "right_click" => vec![InputCmd::Click { button: "right".into(), double: false }],
        "double_click" => vec![InputCmd::Click { button: "left".into(), double: true }],
        "scroll_up" => vec![InputCmd::Scroll { dx: 0.0, dy: 120.0 }],
        "scroll_down" => vec![InputCmd::Scroll { dx: 0.0, dy: -120.0 }],

        _ => return None,
    };
    Some((label(name), cmds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_resolves() {
        for n in NAMES {
            assert!(action(n).is_some(), "{n} listed in NAMES but not mapped");
        }
    }

    #[test]
    fn unknown_name_rejected() {
        assert!(action("rm_rf_slash").is_none());
    }

    #[test]
    fn mapped_keys_are_known_to_the_backend() {
        for n in NAMES {
            let (_, cmds) = action(n).unwrap();
            for c in cmds {
                match c {
                    InputCmd::Key(k) => assert!(crate::input::key_known(&k), "{n}: key {k:?}"),
                    InputCmd::Hotkey(ks) => {
                        for k in ks {
                            assert!(crate::input::key_known(&k), "{n}: hotkey {k:?}");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
