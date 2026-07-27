//! Voice-text preprocessing: normalize → split chains → pull out repeat counts.
//!
//! Speech arrives as prose, not keystrokes: casing, trailing punctuation, mishearings and
//! number *words* all vary run to run. Everything downstream (rule matching, config rules)
//! sees the normalized form only, so a rule author writes `delete {n} words`, never a regex
//! for "three" vs "3".

use std::sync::OnceLock;

use regex::Regex;

/// Regex fragment a `{n}` placeholder expands to. Digits, plus the homophones a recognizer
/// hands back in a count slot ("delete to words"). Only trusted *inside* `{n}` — a global
/// to→2 rewrite would wreck ordinary phrases.
pub const COUNT: &str = r"(?:\d+|a|an|one|won|two|to|too|three|four|for|five|six|seven|eight|ate|nine|ten|couple)";

/// Repeats are capped so one mishearing can't fire 900 backspaces.
pub const MAX_REPEAT: u32 = 40;

fn rx(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

fn cached(slot: &'static OnceLock<Regex>, p: &str) -> &'static Regex {
    slot.get_or_init(|| rx(p))
}

pub fn count_of(tok: &str) -> Option<u32> {
    if let Ok(n) = tok.parse::<u32>() {
        return Some(n.min(MAX_REPEAT));
    }
    Some(match tok {
        "a" | "an" | "one" | "won" | "once" => 1,
        "two" | "to" | "too" | "couple" | "twice" => 2,
        "three" | "thrice" => 3,
        "four" | "for" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" | "ate" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" | "dozen" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        "twenty" => 20,
        _ => return None,
    })
}

// (mishearing, canonical) — applied as whole-word/phrase rewrites, longest first.
const FIXUPS: &[(&str, &str)] = &[
    ("back space", "backspace"),
    ("backspace bar", "backspace"),
    ("space bar", "space"),
    ("spacebar", "space"),
    ("a scape", "escape"),
    ("excape", "escape"),
    ("control", "ctrl"),
    ("comand", "command"),
    ("kommand", "command"),
    ("sell all", "select all"),
    ("select-all", "select all"),
    ("new lion", "new line"),
    ("scratch dat", "scratch that"),
    ("a couple of", "2"),
    ("a couple", "2"),
];

// Unambiguous number words -> digits. Homophones (to/for/won/ate) are deliberately absent:
// they are only read as counts inside a `{n}` slot.
const NUMS: &[(&str, &str)] = &[
    ("twenty", "20"), ("nineteen", "19"), ("eighteen", "18"), ("seventeen", "17"),
    ("sixteen", "16"), ("fifteen", "15"), ("fourteen", "14"), ("thirteen", "13"),
    ("twelve", "12"), ("eleven", "11"), ("ten", "10"), ("nine", "9"),
    ("seven", "7"), ("six", "6"), ("five", "5"), ("three", "3"), ("two", "2"),
    ("one", "1"),
];

/// Lowercase, drop sentence punctuation, fix known mishearings, number words → digits,
/// collapse whitespace.
pub fn normalize(text: &str) -> String {
    let mut s = text.trim().to_lowercase();
    s = s.replace(['.', ',', '?', '!', ';', ':', '"', '“', '”', '’'], " ");
    s = cached(&WS, r"\s+").replace_all(&s, " ").trim().to_string();
    for (re, to) in rewrites() {
        s = re.replace_all(&s, to.as_str()).into_owned();
    }
    cached(&WS, r"\s+").replace_all(&s, " ").trim().to_string()
}

static WS: OnceLock<Regex> = OnceLock::new();
static REWRITES: OnceLock<Vec<(Regex, String)>> = OnceLock::new();

/// Whole-word rewrites, compiled once ("one" inside "money" must survive).
fn rewrites() -> &'static [(Regex, String)] {
    REWRITES.get_or_init(|| {
        FIXUPS
            .iter()
            .chain(NUMS.iter())
            .map(|(from, to)| (rx(&format!(r"\b{}\b", regex::escape(from))), to.to_string()))
            .collect()
    })
}

/// Split a chained utterance: "select all and copy" / "up 3 then enter" / "copy, paste".
/// Callers try the whole string as one step first, so a rule phrase containing "and" wins.
pub fn split_steps(text: &str) -> Vec<String> {
    cached(&SPLIT, r"\s+and\s+then\s+|\s+then\s+|\s+and\s+|\s*,\s*")
        .split(text)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

static SPLIT: OnceLock<Regex> = OnceLock::new();

/// Peel a trailing repeat off a step: "backspace twice" -> ("backspace", 2).
/// Counts are the whole point of voice over buttons — "up 5" beats five taps.
pub fn strip_repeat(step: &str) -> (String, u32) {
    let times = format!(r"^(?P<base>.+?)\s+(?P<n>{COUNT})\s+times?$");
    let pats: [&str; 3] = [
        &times,
        r"^(?P<base>.+?)\s+(?P<n>once|twice|thrice)$",
        r"^(?P<base>.+?)\s*x\s*(?P<n>\d+)$",
    ];
    for p in pats {
        if let Some(c) = rx(p).captures(step) {
            if let Some(n) = count_of(&c["n"]) {
                return (c["base"].trim().to_string(), n.max(1));
            }
        }
    }
    (step.to_string(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_punct_and_numbers() {
        assert_eq!(normalize("  Delete Three Words. "), "delete 3 words");
        assert_eq!(normalize("Back space!"), "backspace");
        assert_eq!(normalize("select   all?"), "select all");
        assert_eq!(normalize("a couple of words"), "2 words");
    }

    #[test]
    fn keeps_homophones_out_of_prose() {
        // "to" must not become 2 in prose; only a {n} slot may read it as a count.
        assert_eq!(normalize("go to the top"), "go to the top");
    }

    #[test]
    fn splits_chains() {
        assert_eq!(split_steps("select all and copy"), vec!["select all", "copy"]);
        assert_eq!(split_steps("up 3 then enter"), vec!["up 3", "enter"]);
        assert_eq!(split_steps("copy, paste"), vec!["copy", "paste"]);
    }

    #[test]
    fn strips_repeats() {
        assert_eq!(strip_repeat("backspace twice"), ("backspace".into(), 2));
        assert_eq!(strip_repeat("delete word 3 times"), ("delete word".into(), 3));
        assert_eq!(strip_repeat("up x4"), ("up".into(), 4));
        assert_eq!(strip_repeat("copy"), ("copy".into(), 1));
    }

    #[test]
    fn repeat_is_capped() {
        assert_eq!(strip_repeat("backspace 900 times").1, MAX_REPEAT);
    }
}
