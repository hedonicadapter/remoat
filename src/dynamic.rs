//! The tier between the static table and the LLM fallback: commands whose *object* can't be
//! known ahead of time — the windows you happen to have open, the apps you happen to have
//! installed.
//!
//! Two rules this tier must not break:
//!
//! 1. **Resolution is side-effect free.** It may *read* (list windows) but never act, because
//!    `remoat --check "focus slack"` has to print what would happen without doing it. The
//!    action comes back as `Exec` steps like everything else.
//! 2. **Nothing is cached across utterances.** omniwm window ids are session-scoped and go
//!    stale on restart, so a name is resolved to an id at the moment you speak, never baked
//!    into a generated rule.
//!
//! Matching is fuzzy on purpose. The input is a speech transcript: lowercase, no punctuation,
//! and prone to splitting or mangling proper nouns ("v s code", "slac"). An exact match against
//! a window title would essentially never fire.

use crate::apps;
use crate::commands::Exec;
use crate::wm;

/// Minimum similarity before we'll act. Tuned so a clear prefix or substring wins but an
/// unrelated word doesn't: acting on the wrong window is worse than saying "no match" and
/// letting the utterance fall through to the LLM tier.
const THRESHOLD: f32 = 0.55;

/// How much better the winner must be than the runner-up to be considered unambiguous.
const MARGIN: f32 = 0.08;

pub struct Dynamic {
    pub label: String,
    pub steps: Vec<Exec>,
}

/// Generic nouns people actually say, mapped to the app identities that satisfy them. "focus
/// browser" has to work without anything on the machine literally being called "browser", and
/// fuzzy string matching can never bridge that — it is a synonym problem, not a spelling one.
/// Order within a group is preference: the first one present wins a tie.
const ALIASES: &[(&str, &[&str])] = &[
    ("browser", &["zen", "firefox", "chromium", "chrome", "brave", "safari", "vivaldi", "librewolf", "edge", "arc"]),
    ("terminal", &["ghostty", "alacritty", "kitty", "wezterm", "foot", "iterm", "terminal", "konsole"]),
    ("editor", &["code", "vscode", "zed", "nvim", "neovim", "vim", "emacs", "sublime"]),
    ("music", &["spotify", "music", "tidal"]),
    ("files", &["nautilus", "dolphin", "thunar", "finder", "nemo", "files"]),
    ("chat", &["slack", "discord", "telegram", "signal", "element"]),
    ("mail", &["thunderbird", "mail", "outlook", "geary"]),
];

/// Expand a generic noun into the identities that satisfy it; otherwise just the query.
fn expand(query: &str) -> Vec<String> {
    let q = squash(query);
    for (key, names) in ALIASES {
        if squash(key) == q {
            return names.iter().map(|n| n.to_string()).collect();
        }
    }
    vec![query.to_string()]
}

/// Filler that survives normalization and would otherwise wreck a prefix match.
fn strip_filler(s: &str) -> &str {
    let mut out = s.trim();
    for f in ["please ", "can you ", "could you ", "hey "] {
        if let Some(r) = out.strip_prefix(f) {
            out = r.trim();
        }
    }
    out
}

/// Try to resolve one normalized utterance segment. `None` = not ours, fall through.
///
/// Order matters: "move window to workspace 3" must be read as a move, not as a direction or a
/// window named "workspace 3", and "focus left" is a direction rather than a window called
/// "left".
pub fn resolve(seg: &str) -> Option<Dynamic> {
    let s = strip_filler(seg);

    // --- send the focused window to a workspace ---
    for p in ["move window to workspace ", "move to workspace ", "send window to workspace ",
              "send to workspace ", "move window to ", "send window to ", "move this to workspace "] {
        if let Some(rest) = s.strip_prefix(p) {
            if let Some(n) = leading_number(rest) {
                return act("move to workspace", n.to_string(), |wm| wm.move_to_workspace_argv(n));
            }
        }
    }

    // --- directional move ---
    for p in ["move window ", "move column ", "move this ", "move "] {
        if let Some(rest) = s.strip_prefix(p) {
            if let Some(d) = direction(rest) {
                return act("move", d.word().to_string(), |wm| wm.move_argv(d));
            }
        }
    }

    // --- directional focus ("focus left", "focus window right") ---
    for p in ["focus window ", "focus column ", "focus ", "go "] {
        if let Some(rest) = s.strip_prefix(p) {
            if let Some(d) = direction(rest) {
                return act("focus", d.word().to_string(), |wm| wm.focus_dir_argv(d));
            }
        }
    }

    // --- relative workspace ---
    if let Some(delta) = relative_workspace(s) {
        let word = if delta >= 0 { "next" } else { "previous" };
        return act("workspace", word.to_string(), |wm| wm.workspace_rel_argv(delta));
    }

    // --- absolute workspace, with or without a leading verb ---
    if let Some(n) = workspace_number(s) {
        return act("workspace", n.to_string(), |wm| Some(wm.workspace_argv(n)));
    }

    // --- window state ---
    for (phrases, label, pick) in WINDOW_ACTIONS {
        if phrases.iter().any(|p| *p == s) {
            return act(label, String::new(), |wm| pick(wm));
        }
    }

    // --- launch ---
    if let Some(rest) = strip_prefix_any(s, &["open ", "launch ", "start ", "run "]) {
        return open_app(rest);
    }

    // --- focus by name ---
    if let Some(rest) = strip_prefix_any(s, &["focus ", "switch to ", "go to ", "show ", "activate "]) {
        return focus_window(rest);
    }
    None
}

type Picker = fn(&dyn wm::Wm) -> Option<Vec<String>>;

/// Bare window-state commands, matched as whole phrases so "close window" can't be mistaken for
/// a window named "window".
const WINDOW_ACTIONS: &[(&[&str], &str, Picker)] = &[
    (&["close window", "close this window"], "close window", |w| w.close_argv()),
    (&["fullscreen", "full screen", "toggle fullscreen", "make fullscreen"], "fullscreen", |w| w.fullscreen_argv()),
    (&["float", "toggle floating", "toggle float", "make floating"], "toggle floating", |w| w.float_argv()),
];

/// Build a step from a backend capability, or report that this WM can't do it. Saying so beats
/// silently falling through to "no rule matched", which would send the user hunting for a typo.
fn act(
    verb: &str,
    arg: String,
    pick: impl Fn(&dyn wm::Wm) -> Option<Vec<String>>,
) -> Option<Dynamic> {
    let backend = wm::detect()?;
    let label = if arg.is_empty() { verb.to_string() } else { format!("{verb} {arg}") };
    match pick(backend.as_ref()) {
        Some(argv) => Some(Dynamic { label, steps: vec![Exec::Run(argv)] }),
        None => Some(Dynamic {
            label: label.clone(),
            steps: vec![Exec::Note(format!("{} can't {label}", backend.name()))],
        }),
    }
}

fn strip_prefix_any<'a>(s: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| s.strip_prefix(p)).map(str::trim).filter(|r| !r.is_empty())
}

/// A direction, optionally dressed up: "left", "to the left", "one left".
fn direction(s: &str) -> Option<wm::Dir> {
    let t = s
        .trim()
        .trim_start_matches("to the ")
        .trim_start_matches("to ")
        .trim_start_matches("the ")
        .trim_start_matches("one ")
        .trim();
    wm::Dir::parse(t)
}

fn leading_number(s: &str) -> Option<i32> {
    s.split_whitespace().next()?.parse::<i32>().ok().filter(|n| (1..=99).contains(n))
}

/// "next workspace", "previous workspace", "workspace next".
fn relative_workspace(s: &str) -> Option<i32> {
    const NEXT: &[&str] = &["next workspace", "workspace next", "next desktop", "workspace right", "workspace down"];
    const PREV: &[&str] = &["previous workspace", "prev workspace", "workspace previous", "last workspace",
                            "previous desktop", "workspace left", "workspace up"];
    if NEXT.contains(&s) {
        return Some(1);
    }
    if PREV.contains(&s) {
        return Some(-1);
    }
    None
}

/// "workspace 3", and the same with any of the verbs people actually use in front of it.
/// The verb list is separate from the focus prefixes on purpose: "switch workspace 3" has no
/// "to", which is exactly the phrasing that failed before.
fn workspace_number(s: &str) -> Option<i32> {
    let mut t = s;
    for v in ["go to ", "switch to ", "switch ", "change to ", "change ", "jump to ", "move to ", "show "] {
        if let Some(r) = t.strip_prefix(v) {
            t = r.trim();
            break;
        }
    }
    let rest = strip_prefix_any(t, &["workspace ", "desktop ", "space ", "ws "])?;
    leading_number(rest)
}

fn open_app(query: &str) -> Option<Dynamic> {
    let apps = apps::installed();
    let names = expand(query);
    // Group by app name: two entries for the same app are not a real ambiguity.
    let (app, _) = best_of(&names, apps.iter().map(|a| (a, vec![a.name.as_str()])), |a| a.name.clone())?;
    Some(Dynamic { label: format!("open {}", app.name), steps: vec![Exec::Launch(app.argv.clone())] })
}

fn focus_window(query: &str) -> Option<Dynamic> {
    let backend = wm::detect()?;
    let windows = wm::windows();
    if windows.is_empty() {
        return None;
    }
    let names = expand(query);
    // Match on app id AND title: you might say "slack" (the app) or "general" (the title).
    // Group by app, so several windows of ONE app are a choice we're allowed to make for you —
    // "focus zen" with three browser windows open means "go to the browser", not "ambiguous".
    let (w, _) = best_of(
        &names,
        windows.iter().map(|w| (w, vec![w.app.as_str(), w.title.as_str()])),
        |w| squash(&w.app),
    )?;
    let shown = if w.app.is_empty() { w.title.clone() } else { w.app.clone() };
    Some(Dynamic { label: format!("focus {shown}"), steps: vec![Exec::Run(backend.focus_argv(&w.id))] })
}

/// Pick the best candidate, requiring both an absolute floor and a clear margin over the
/// runner-up. Each candidate offers several strings to match against (app id, title, …) and
/// scores as its best one.
fn best_of<'a, T>(
    queries: &[String],
    candidates: impl Iterator<Item = (&'a T, Vec<&'a str>)>,
    group_of: impl Fn(&T) -> String,
) -> Option<(&'a T, f32)> {
    let mut scored: Vec<(&T, f32)> = candidates
        .map(|(item, fields)| {
            // Best over (every alias) x (every field). An alias group is a disjunction: any of
            // "zen"/"firefox"/… satisfies "browser".
            let s = queries
                .iter()
                .flat_map(|q| fields.iter().map(move |f| score(q, f)))
                .fold(0.0f32, f32::max);
            (item, s)
        })
        .filter(|(_, s)| *s >= THRESHOLD)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (top, top_s) = *scored.first()?;
    // A near-tie is only a real ambiguity ACROSS groups. Three windows of the same browser tie
    // at 1.0 every time, and refusing that was the bug behind "focus zen" doing nothing.
    let top_group = group_of(top);
    if scored
        .iter()
        .skip(1)
        .any(|(item, s)| top_s - s < MARGIN && group_of(item) != top_group)
    {
        return None;
    }
    Some((top, top_s))
}

/// Similarity in [0,1] between a spoken query and a candidate name.
///
/// Deliberately simple and dependency-free. The cases that matter for speech, in order:
/// exact, prefix, substring, then per-word coverage — "v s code" should still find
/// "Visual Studio Code", and "slack" should find "Slack | general".
pub fn score(query: &str, candidate: &str) -> f32 {
    let q = squash(query);
    let c = squash(candidate);
    if q.is_empty() || c.is_empty() {
        return 0.0;
    }
    if q == c {
        return 1.0;
    }
    if c.starts_with(&q) {
        // Longer queries are stronger evidence: "sl" prefixing "slack" is weak, "slac" is not.
        return 0.80 + 0.15 * (q.len() as f32 / c.len() as f32);
    }
    if c.contains(&q) {
        return 0.65 + 0.15 * (q.len() as f32 / c.len() as f32);
    }
    // Word coverage, so word order and interleaved noise don't matter.
    let words: Vec<&str> = query.split_whitespace().filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return 0.0;
    }
    let hit = words.iter().filter(|w| c.contains(&squash(w))).count();
    let cover = hit as f32 / words.len() as f32;
    // Cap below the substring tier: partial word coverage is a weaker signal.
    cover * 0.7
}

/// Lowercase and drop everything that isn't alphanumeric, so "V.S. Code", "vs-code" and
/// "vs code" all collapse to the same key — speech gives us no punctuation anyway.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_prefix_beat_everything() {
        assert_eq!(score("slack", "Slack"), 1.0);
        assert!(score("slac", "Slack") > 0.8);
        assert!(score("slack", "Slack | general") > THRESHOLD);
    }

    #[test]
    fn punctuation_and_spacing_are_irrelevant() {
        // Speech has none of it, so neither can matching.
        assert_eq!(score("vs code", "VS Code"), 1.0);
        assert_eq!(score("v s code", "vs-code"), 1.0);
        assert_eq!(score("firefox", "Firefox"), 1.0);
    }

    #[test]
    fn word_coverage_finds_a_long_title() {
        assert!(score("visual studio", "Visual Studio Code") > THRESHOLD);
        assert!(score("nvim", "nvim — src/wm.rs") > THRESHOLD);
    }

    #[test]
    fn unrelated_words_stay_below_the_floor() {
        for (q, c) in [("spotify", "Firefox"), ("terminal", "Slack"), ("x", "Visual Studio Code")] {
            assert!(score(q, c) < THRESHOLD, "{q:?} should not match {c:?} (got {})", score(q, c));
        }
    }

    #[test]
    fn empty_never_matches() {
        assert_eq!(score("", "Firefox"), 0.0);
        assert_eq!(score("firefox", ""), 0.0);
    }

    // `best` is where a wrong pick becomes a wrong action, so it gets its own tests.
    #[derive(Debug, PartialEq)]
    struct Item(&'static str);

    fn pick<'a>(q: &str, names: &'a [Item]) -> Option<&'a Item> {
        best_of(&[q.to_string()], names.iter().map(|i| (i, vec![i.0])), |i| i.0.to_string())
            .map(|(i, _)| i)
    }

    /// Same shape, but every candidate is one group — i.e. several windows of one app.
    fn pick_same_app<'a>(q: &str, names: &'a [Item]) -> Option<&'a Item> {
        best_of(&[q.to_string()], names.iter().map(|i| (i, vec![i.0])), |_| "one".to_string())
            .map(|(i, _)| i)
    }

    #[test]
    fn picks_the_clear_winner() {
        let items = [Item("Firefox"), Item("Slack"), Item("Spotify")];
        assert_eq!(pick("slack", &items), Some(&Item("Slack")));
        assert_eq!(pick("spot", &items), Some(&Item("Spotify")));
    }

    #[test]
    fn refuses_a_tie_across_different_apps() {
        // Genuinely ambiguous: acting on an arbitrary one is worse than doing nothing.
        let items = [Item("Zed"), Item("Zen")];
        assert_eq!(pick("ze", &items), None);
    }

    #[test]
    fn a_tie_within_one_app_is_not_ambiguous() {
        // The "focus zen" bug: three browser windows all score 1.0, and refusing that made the
        // command do nothing. Several windows of ONE app means "go to that app".
        let items = [Item("zen"), Item("zen"), Item("zen")];
        assert_eq!(pick_same_app("zen", &items), Some(&Item("zen")));
    }

    #[test]
    fn generic_nouns_expand_to_real_app_names() {
        assert!(expand("browser").iter().any(|n| n == "zen"));
        assert!(expand("browser").iter().any(|n| n == "firefox"));
        assert!(expand("terminal").iter().any(|n| n == "ghostty"));
        // Anything not an alias passes through untouched.
        assert_eq!(expand("spotify"), vec!["spotify"]);
    }

    #[test]
    fn workspace_verbs_people_actually_say() {
        // "switch workspace 3" has no "to" — the exact phrasing that used to fall through.
        for p in ["workspace 3", "switch workspace 3", "switch to workspace 3", "go to workspace 3",
                  "change workspace 3", "jump to workspace 3", "desktop 3", "ws 3"] {
            assert_eq!(workspace_number(p), Some(3), "{p:?} should be workspace 3");
        }
        assert_eq!(workspace_number("focus zen"), None);
        assert_eq!(workspace_number("workspace"), None);
    }

    #[test]
    fn relative_workspaces_parse() {
        assert_eq!(relative_workspace("next workspace"), Some(1));
        assert_eq!(relative_workspace("previous workspace"), Some(-1));
        assert_eq!(relative_workspace("workspace 3"), None);
    }

    #[test]
    fn directions_tolerate_filler() {
        for p in ["left", "to the left", "the left", "one left"] {
            assert_eq!(direction(p), Some(wm::Dir::Left), "{p:?}");
        }
        assert_eq!(direction("down"), Some(wm::Dir::Down));
        assert_eq!(direction("below"), Some(wm::Dir::Down));
        assert_eq!(direction("slack"), None);
    }

    #[test]
    fn filler_prefixes_are_dropped() {
        assert_eq!(strip_filler("please focus zen"), "focus zen");
        assert_eq!(strip_filler("can you close window"), "close window");
    }

    #[test]
    fn returns_nothing_when_everything_is_weak() {
        let items = [Item("Firefox"), Item("Slack")];
        assert_eq!(pick("photoshop", &items), None);
    }

    #[test]
    fn workspace_phrases_parse() {
        assert_eq!(workspace_number("workspace 3"), Some(3));
        assert_eq!(workspace_number("desktop 12"), Some(12));
        assert_eq!(workspace_number("space 1"), Some(1));
        assert_eq!(workspace_number("workspace"), None);
        assert_eq!(workspace_number("slack"), None);
        assert_eq!(workspace_number("workspace 0"), None, "1-based");
    }

    #[test]
    fn go_to_workspace_is_a_workspace_not_a_window() {
        // Regression guard: "go to" is also a focus prefix, so without the workspace check
        // this would hunt for a window literally named "workspace 3".
        assert_eq!(strip_prefix_any("go to workspace 3", &["go to "]), Some("workspace 3"));
        assert_eq!(workspace_number("workspace 3"), Some(3));
    }

    #[test]
    fn prefixes_need_a_target() {
        assert_eq!(strip_prefix_any("open ", &["open "]), None);
        assert_eq!(strip_prefix_any("open firefox", &["open "]), Some("firefox"));
    }
}
