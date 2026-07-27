//! Intent router.
//!
//! Tier 1: the command table (`config` = built-in default + user overrides) — deterministic,
//! offline, safe. Tier 2: `llm_fallback` seam. Both tiers land on the same bounded surface:
//! a registry action, a validated chord, literal text, or argv. Never arbitrary shell.
//!
//! Voice text → normalize → match rule (with count) → Vec<Exec> → input worker.
//! A chain ("select all then copy") is all-or-nothing: if any step is unmatched, nothing runs.

use std::process::Command;

use crate::actions;
use crate::config::{self, Body, Rules};
use crate::input::InputCmd;
use crate::voice;

/// One thing to do. `Note` = matched but deliberately no input (clarify, or an app that
/// isn't wired for this platform).
pub enum Exec {
    Input(InputCmd),
    Launch(Vec<String>),
    Note(String),
}

pub struct Resolution {
    pub matched: bool,
    /// Canonical intent for the toast: `delete word ×3`, `select all → copy`.
    pub label: String,
    pub steps: Vec<Exec>,
    /// Set when nothing will run (unmatched, or a clarify prompt).
    pub message: Option<String>,
}

/// Guard against a chain of mishearings turning into a keystroke storm.
const MAX_CMDS: usize = 200;

pub fn route(text: &str) -> Resolution {
    let q = voice::normalize(text);
    if q.is_empty() {
        return unmatched("empty", None);
    }
    let rules = config::rules();

    // Whole utterance first — a rule phrase may itself contain "and"/"then".
    if let Some(plan) = resolve_step(&rules, &q) {
        return finish(vec![plan]);
    }
    let segs = voice::split_steps(&q);
    if segs.len() > 1 {
        let mut plans = Vec::new();
        for s in &segs {
            match resolve_step(&rules, s) {
                Some(p) => plans.push(p),
                None => return llm_fallback(&q, Some(s)),
            }
        }
        return finish(plans);
    }
    llm_fallback(&q, None)
}

struct Plan {
    label: String,
    steps: Vec<Exec>,
}

/// Match one step against the table. `strip_repeat` runs second so a phrase that legitimately
/// ends in "times" still wins over the generic repeat suffix.
fn resolve_step(rules: &Rules, seg: &str) -> Option<Plan> {
    let (base, repeat) = voice::strip_repeat(seg);
    let candidates: Vec<(&str, u32)> = if base == seg {
        vec![(seg, 1)]
    } else {
        vec![(seg, 1), (base.as_str(), repeat)]
    };

    for (text, repeat) in candidates {
        for rule in rules.iter() {
            for m in &rule.matchers {
                let Some(c) = m.captures(text) else { continue };
                // First count-shaped capture is the `{n}` slot; it multiplies the trailing
                // repeat. Scanning all groups (not just #1) keeps `regex = true` rules usable
                // when the count isn't their first group.
                let inner = c
                    .iter()
                    .skip(1)
                    .flatten()
                    .find_map(|g| voice::count_of(g.as_str()))
                    .unwrap_or(1);
                let count = (inner * repeat).clamp(1, voice::MAX_REPEAT);
                match expand(&rule.body, count) {
                    Ok(x) => {
                        let base = rule.label.clone().unwrap_or(x.label);
                        // Toast must echo the count — "delete word ×3" is the whole eyes-off
                        // feedback loop.
                        let label = if x.counted && count > 1 {
                            format!("{base} ×{count}")
                        } else {
                            base
                        };
                        return Some(Plan { label, steps: x.steps });
                    }
                    // Validated at load, so this is unreachable in practice; log and keep
                    // scanning rather than swallowing the utterance.
                    Err(e) => eprintln!("[cmd] rule ({}) failed to expand: {e}", rule.source),
                }
            }
        }
    }
    None
}

struct Expanded {
    label: String,
    steps: Vec<Exec>,
    /// Whether the count actually repeated anything (false for launch/clarify).
    counted: bool,
}

fn ex(label: impl Into<String>, steps: Vec<Exec>, counted: bool) -> Expanded {
    Expanded { label: label.into(), steps, counted }
}

/// Body + count -> label + execs. The one place a config rule becomes real input.
fn expand(body: &Body, count: u32) -> Result<Expanded, String> {
    let n = count as usize;
    if let Some(name) = &body.action {
        let (label, cmds) = actions::action(name).ok_or_else(|| format!("unknown action {name:?}"))?;
        return Ok(ex(label, repeat_input(&cmds, n), true));
    }
    if let Some(keys) = body.platform_keys() {
        let label = keys.join("+");
        return Ok(ex(label, repeat_input(&[InputCmd::Hotkey(keys.clone())], n), true));
    }
    if let Some(t) = &body.text {
        return Ok(ex(format!("type {t:?}"), repeat_input(&[InputCmd::Text(t.clone())], n), true));
    }
    if body.launch.is_some() || body.launch_mac.is_some() || body.launch_linux.is_some() {
        // Launch ignores counts — opening an app nine times is never the intent.
        return match body.platform_launch() {
            Some(argv) => Ok(ex(format!("open {}", argv[0]), vec![Exec::Launch(argv.clone())], false)),
            None => Ok(ex(
                "unavailable",
                vec![Exec::Note("not available on this platform".into())],
                false,
            )),
        };
    }
    if let Some(seq) = &body.sequence {
        let mut labels = Vec::new();
        let mut once = Vec::new();
        for step in seq {
            let mut x = expand(step, 1)?;
            labels.push(x.label);
            once.append(&mut x.steps);
        }
        let mut steps = Vec::new();
        for _ in 0..n.max(1) {
            for e in &once {
                steps.push(clone_exec(e));
            }
        }
        return Ok(ex(labels.join(" → "), steps, true));
    }
    if let Some(msg) = &body.clarify {
        return Ok(ex(msg.clone(), vec![Exec::Note(msg.clone())], false));
    }
    Err("rule has no action".into())
}

fn repeat_input(cmds: &[InputCmd], n: usize) -> Vec<Exec> {
    let mut out = Vec::new();
    for _ in 0..n.max(1) {
        for c in cmds {
            out.push(Exec::Input(c.clone()));
        }
    }
    out
}

fn clone_exec(e: &Exec) -> Exec {
    match e {
        Exec::Input(c) => Exec::Input(c.clone()),
        Exec::Launch(a) => Exec::Launch(a.clone()),
        Exec::Note(s) => Exec::Note(s.clone()),
    }
}

fn finish(plans: Vec<Plan>) -> Resolution {
    let label = plans.iter().map(|p| p.label.as_str()).collect::<Vec<_>>().join(" → ");
    let mut steps: Vec<Exec> = plans.into_iter().flat_map(|p| p.steps).collect();
    // Eyes-off safety valve: a chain of counts must not become an unbounded keystroke run.
    if steps.len() > MAX_CMDS {
        return Resolution {
            matched: true,
            label,
            steps: Vec::new(),
            message: Some(format!("too many steps (>{MAX_CMDS}) — ignored")),
        };
    }
    // Clarify-only resolutions carry their prompt as the message so the toast can say it.
    let message = match steps.as_slice() {
        [Exec::Note(m)] => Some(m.clone()),
        _ => None,
    };
    if message.is_some() {
        steps.clear();
    }
    Resolution { matched: true, label, steps, message }
}

fn unmatched(reason: &str, echo: Option<&str>) -> Resolution {
    let message = match echo {
        Some(e) => format!("no rule matched: {e:?}"),
        None => format!("no rule matched ({reason})"),
    };
    Resolution { matched: false, label: String::new(), steps: Vec::new(), message: Some(message) }
}

/// Run a launch action. Input actions dispatch via the worker instead.
pub fn execute_launch(argv: &[String]) -> Result<String, String> {
    if argv.is_empty() {
        return Err("empty argv".into());
    }
    match Command::new(&argv[0]).args(&argv[1..]).spawn() {
        Ok(_) => Ok(format!("launched: {}", argv.join(" "))),
        Err(e) => Err(format!("{}: {e}", argv[0])),
    }
}

/// SEAM: route the long tail via an LLM constrained to the SAME bounded surface.
/// Send `text` plus a tool schema whose action parameter is `enum: actions::NAMES` (and an
/// integer count), then map the chosen call back through `actions::action` — so the model
/// cannot emit anything the deterministic tier couldn't. Not wired yet.
///
/// `failing` is the one chain step that didn't match; it's what the user needs to hear.
fn llm_fallback(text: &str, failing: Option<&str>) -> Resolution {
    let mut r = unmatched("no rule", Some(failing.unwrap_or(text)));
    if let Some(m) = r.message.as_mut() {
        m.push_str(" (LLM fallback not configured)");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps_of(text: &str) -> (String, Vec<String>) {
        let r = route(text);
        let desc = r
            .steps
            .iter()
            .map(|s| match s {
                Exec::Input(c) => format!("{c:?}"),
                Exec::Launch(a) => format!("launch {}", a.join(" ")),
                Exec::Note(n) => format!("note {n}"),
            })
            .collect();
        (r.label, desc)
    }

    #[test]
    fn counts_repeat_the_action() {
        let (label, steps) = steps_of("delete three words");
        assert_eq!(label, "delete word ×3");
        assert_eq!(steps.len(), 3);

        let (_, steps) = steps_of("backspace twice");
        assert_eq!(steps.len(), 2);

        let (_, steps) = steps_of("up 5");
        assert_eq!(steps.len(), 5);
    }

    #[test]
    fn chains_run_in_order() {
        let r = route("select all and copy");
        assert!(r.matched);
        assert_eq!(r.label, "select all → copy");
        assert_eq!(r.steps.len(), 2);
    }

    #[test]
    fn chain_with_one_bad_step_runs_nothing() {
        let r = route("copy and frobnicate the gizmo");
        assert!(!r.matched);
        assert!(r.steps.is_empty());
        assert!(r.message.unwrap().contains("frobnicate the gizmo"));
    }

    #[test]
    fn ambiguous_asks_instead_of_guessing() {
        let r = route("select");
        assert!(r.matched);
        assert!(r.steps.is_empty(), "must not guess a selection");
        assert!(r.message.unwrap().contains("select what?"));
    }

    #[test]
    fn unmatched_says_so() {
        let r = route("make me a sandwich");
        assert!(!r.matched);
        assert!(r.message.unwrap().starts_with("no rule matched"));
    }

    #[test]
    fn app_launch_still_resolves_bare_noun() {
        let (label, steps) = steps_of("terminal");
        assert_eq!(label, "open terminal");
        assert_eq!(steps.len(), 1);
        assert!(steps[0].starts_with("launch"));
    }

    #[test]
    fn homophone_reads_as_a_count_only_in_the_slot() {
        let r = route("delete to words"); // recognizer's "two"
        assert_eq!(r.label, "delete word ×2");
        assert_eq!(r.steps.len(), 2);
    }

    #[test]
    fn runaway_counts_are_refused_not_executed() {
        let r = route("backspace 40 times and backspace 40 times and backspace 40 times \
                       and backspace 40 times and backspace 40 times and backspace 40 times");
        assert!(r.steps.is_empty());
        assert!(r.message.unwrap().contains("too many steps"));
    }

    #[test]
    fn user_rules_override_builtins() {
        // Same phrase in a user file must beat the built-in table.
        let rules = Rules::from_user_src("[[rules]]\nphrases = [\"copy\"]\ntext = \"copied!\"\n");
        let plan = resolve_step(&rules, "copy").expect("matched");
        assert_eq!(plan.label, "type \"copied!\"");
    }

    #[test]
    fn user_sequence_rule_composes_actions() {
        let src = r#"
            [[rules]]
            phrases = ["wipe line", "wipe {n} lines"]
            label = "wipe line"
            sequence = [{ action = "select_line" }, { action = "backspace" }]
        "#;
        let rules = Rules::from_user_src(src);
        let plan = resolve_step(&rules, "wipe 2 lines").expect("matched");
        assert_eq!(plan.label, "wipe line ×2");
        assert_eq!(plan.steps.len(), 4);
    }
}
