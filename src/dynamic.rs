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

/// Try to resolve one normalized utterance segment. `None` = not ours, fall through.
pub fn resolve(seg: &str) -> Option<Dynamic> {
    if let Some(rest) = strip_prefix_any(seg, &["open ", "launch ", "start ", "run "]) {
        return open_app(rest);
    }
    if let Some(rest) = strip_prefix_any(seg, &["focus ", "switch to ", "go to ", "show "]) {
        // "go to workspace 3" is a workspace, not a window called "workspace 3".
        if let Some(n) = workspace_number(rest) {
            return workspace(n);
        }
        return focus_window(rest);
    }
    if let Some(n) = workspace_number(seg) {
        return workspace(n);
    }
    None
}

fn strip_prefix_any<'a>(s: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| s.strip_prefix(p)).map(str::trim).filter(|r| !r.is_empty())
}

/// "workspace 3" / "desktop 2" / "workspace three" (voice::normalize already digitised the
/// unambiguous number words).
fn workspace_number(s: &str) -> Option<i32> {
    let rest = strip_prefix_any(s, &["workspace ", "desktop ", "space "])?;
    rest.split_whitespace().next()?.parse::<i32>().ok().filter(|n| (1..=99).contains(n))
}

fn open_app(query: &str) -> Option<Dynamic> {
    let apps = apps::installed();
    let (app, _) = best(query, apps.iter().map(|a| (a, vec![a.name.as_str()])))?;
    Some(Dynamic {
        label: format!("open {}", app.name),
        steps: vec![Exec::Launch(app.argv.clone())],
    })
}

fn focus_window(query: &str) -> Option<Dynamic> {
    let backend = wm::detect()?;
    let windows = wm::windows();
    if windows.is_empty() {
        return None;
    }
    // Match on app id AND title: you might say "slack" (the app) or "general" (the title).
    let (w, _) = best(query, windows.iter().map(|w| (w, vec![w.app.as_str(), w.title.as_str()])))?;
    let shown = if w.app.is_empty() { w.title.clone() } else { w.app.clone() };
    Some(Dynamic {
        label: format!("focus {shown}"),
        steps: vec![Exec::Run(backend.focus_argv(&w.id))],
    })
}

fn workspace(n: i32) -> Option<Dynamic> {
    let backend = wm::detect()?;
    Some(Dynamic { label: format!("workspace {n}"), steps: vec![Exec::Run(backend.workspace_argv(n))] })
}

/// Pick the best candidate, requiring both an absolute floor and a clear margin over the
/// runner-up. Each candidate offers several strings to match against (app id, title, …) and
/// scores as its best one.
fn best<'a, T>(
    query: &str,
    candidates: impl Iterator<Item = (&'a T, Vec<&'a str>)>,
) -> Option<(&'a T, f32)> {
    let mut scored: Vec<(&T, f32)> = candidates
        .map(|(item, fields)| {
            let s = fields.iter().map(|f| score(query, f)).fold(0.0f32, f32::max);
            (item, s)
        })
        .filter(|(_, s)| *s >= THRESHOLD)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (top, top_s) = *scored.first()?;
    // Two equally good matches: refuse rather than coin-flip which window you meant.
    if scored.get(1).is_some_and(|(_, s)| top_s - s < MARGIN) {
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
        best(q, names.iter().map(|i| (i, vec![i.0]))).map(|(i, _)| i)
    }

    #[test]
    fn picks_the_clear_winner() {
        let items = [Item("Firefox"), Item("Slack"), Item("Spotify")];
        assert_eq!(pick("slack", &items), Some(&Item("Slack")));
        assert_eq!(pick("spot", &items), Some(&Item("Spotify")));
    }

    #[test]
    fn refuses_a_tie_rather_than_guessing() {
        // Two terminals open: acting on an arbitrary one is worse than doing nothing and
        // letting the utterance fall through.
        let items = [Item("Alacritty"), Item("Alacritty")];
        assert_eq!(pick("alacritty", &items), None);
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
