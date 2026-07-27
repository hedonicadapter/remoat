//! The command table: embedded default + user TOML, merged and re-read on change.
//!
//! The built-in table IS the default config (`commands.default.toml`, same schema), so
//! there is one matcher, one validator, one place to add a command. User rules are matched
//! BEFORE built-ins, which is what makes a user rule an override.
//!
//! Bounded on purpose: a rule composes primitives (registry action / chord / literal text /
//! argv / sequence) and there is deliberately no `shell = "…"` kind — config can't invent an
//! escape hatch out of the allowlist. Matters more once LAN auth lands.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use regex::Regex;
use serde::Deserialize;

use crate::actions;
use crate::input;
use crate::voice::COUNT;

const DEFAULT_TOML: &str = include_str!("commands.default.toml");

#[derive(Deserialize)]
struct Table {
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Deserialize, Clone, Debug)]
struct Rule {
    #[serde(default)]
    phrases: Vec<String>,
    #[serde(default)]
    regex: bool,
    label: Option<String>,
    #[serde(flatten)]
    body: Body,
}

/// One action kind. Closed set — extend it here, not in config.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Body {
    pub action: Option<String>,
    pub keys: Option<Vec<String>>,
    pub keys_mac: Option<Vec<String>>,
    pub keys_linux: Option<Vec<String>>,
    pub text: Option<String>,
    pub launch: Option<Vec<String>>,
    pub launch_mac: Option<Vec<String>>,
    pub launch_linux: Option<Vec<String>>,
    pub sequence: Option<Vec<Body>>,
    pub clarify: Option<String>,
}

impl Body {
    fn kinds(&self) -> usize {
        [
            self.action.is_some(),
            self.keys.is_some() || self.keys_mac.is_some() || self.keys_linux.is_some(),
            self.text.is_some(),
            self.launch.is_some() || self.launch_mac.is_some() || self.launch_linux.is_some(),
            self.sequence.is_some(),
            self.clarify.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count()
    }

    /// Chord for the running platform (`keys_mac`/`keys_linux` beat the shared `keys`).
    pub fn platform_keys(&self) -> Option<&Vec<String>> {
        let specific = if cfg!(target_os = "macos") { &self.keys_mac } else { &self.keys_linux };
        specific.as_ref().or(self.keys.as_ref())
    }

    /// argv for the running platform. None = this app isn't wired for this platform.
    pub fn platform_launch(&self) -> Option<&Vec<String>> {
        let specific = if cfg!(target_os = "macos") { &self.launch_mac } else { &self.launch_linux };
        specific.as_ref().or(self.launch.as_ref())
    }
}

pub struct Compiled {
    /// Anchored regexes; `{n}` became a count capture group.
    pub matchers: Vec<Regex>,
    pub label: Option<String>,
    pub body: Body,
    pub source: &'static str,
}

/// `$REMOAT_CONFIG`, else `$XDG_CONFIG_HOME/remoat/commands.toml`, else `~/.config/…`.
pub fn user_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("REMOAT_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("remoat").join("commands.toml"))
}

/// User rules then built-ins — match order, and the whole override mechanism.
pub struct Rules {
    user: Vec<Compiled>,
    builtin: Arc<Vec<Compiled>>,
}

impl Rules {
    pub fn iter(&self) -> impl Iterator<Item = &Compiled> {
        self.user.iter().chain(self.builtin.iter())
    }

    /// Table from an in-memory user file — no cache, no env, no global state to race on.
    #[cfg(test)]
    pub fn from_user_src(src: &str) -> Rules {
        let (user, _) = compile_toml(src, "user");
        Rules { user, builtin: builtin() }
    }
}

struct Cache {
    key: (Option<PathBuf>, Option<SystemTime>),
    rules: Arc<Rules>,
}

static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
static BUILTIN: OnceLock<Arc<Vec<Compiled>>> = OnceLock::new();

fn builtin() -> Arc<Vec<Compiled>> {
    BUILTIN
        .get_or_init(|| {
            let (rules, bad) = compile_toml(DEFAULT_TOML, "builtin");
            if bad > 0 {
                eprintln!("[cfg] BUG: {bad} invalid rule(s) in the embedded default table");
            }
            Arc::new(rules)
        })
        .clone()
}

/// Cheap to call per command — the user file is re-read only when its mtime moved
/// (edit, save, speak; no restart).
pub fn rules() -> Arc<Rules> {
    rules_from(user_path())
}

fn rules_from(path: Option<PathBuf>) -> Arc<Rules> {
    let stamp = path.as_ref().and_then(|p| fs::metadata(p).ok()).and_then(|m| m.modified().ok());
    let key = (path.clone(), stamp);

    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if let Some(c) = guard.as_ref() {
        if c.key == key {
            return c.rules.clone();
        }
    }

    let mut user: Vec<Compiled> = Vec::new();
    if let (Some(p), Some(_)) = (path.as_ref(), stamp.as_ref()) {
        match fs::read_to_string(p) {
            Ok(src) => {
                let (loaded, bad) = compile_toml(&src, "user");
                eprintln!(
                    "[cfg] {} user rule(s) from {}{}",
                    loaded.len(),
                    p.display(),
                    if bad > 0 { format!(" ({bad} skipped)") } else { String::new() }
                );
                user = loaded;
            }
            // Fail soft throughout: a broken user file must never brick the daemon.
            Err(e) => eprintln!("[cfg] cannot read {}: {e} — using built-ins only", p.display()),
        }
    }

    let rules = Arc::new(Rules { user, builtin: builtin() });
    *guard = Some(Cache { key, rules: rules.clone() });
    rules
}

/// Parse + validate + compile. Returns (good rules, skipped count). A bad rule is logged
/// and dropped; the rest of the file still loads.
fn compile_toml(src: &str, source: &'static str) -> (Vec<Compiled>, usize) {
    let table: Table = match toml::from_str(src) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[cfg] {source} table parse error: {e}");
            return (Vec::new(), 1);
        }
    };
    let mut out = Vec::new();
    let mut bad = 0;
    for (i, rule) in table.rules.into_iter().enumerate() {
        match compile_rule(rule, source) {
            Ok(c) => out.push(c),
            Err(e) => {
                eprintln!("[cfg] {source} rule #{}: {e} — skipped", i + 1);
                bad += 1;
            }
        }
    }
    (out, bad)
}

fn compile_rule(rule: Rule, source: &'static str) -> Result<Compiled, String> {
    if rule.phrases.is_empty() {
        return Err("no phrases".into());
    }
    validate(&rule.body, true)?;
    let mut matchers = Vec::new();
    for p in &rule.phrases {
        matchers.push(compile_phrase(p, rule.regex).map_err(|e| format!("phrase {p:?}: {e}"))?);
    }
    Ok(Compiled { matchers, label: rule.label, body: rule.body, source })
}

/// `delete {n} words` -> `^delete (\d+|two|…) words$`. Literal unless `regex = true`.
fn compile_phrase(phrase: &str, is_regex: bool) -> Result<Regex, String> {
    let slot = format!("({COUNT})");
    let body: String = phrase
        .split("{n}")
        .map(|part| if is_regex { part.to_string() } else { regex::escape(part) })
        .collect::<Vec<_>>()
        .join(&slot);
    Regex::new(&format!("^(?:{body})$")).map_err(|e| e.to_string())
}

fn validate(body: &Body, allow_sequence: bool) -> Result<(), String> {
    match body.kinds() {
        1 => {}
        0 => return Err("no action (want one of action/keys/text/launch/sequence/clarify)".into()),
        n => return Err(format!("{n} action kinds set; exactly one allowed")),
    }
    if let Some(name) = &body.action {
        if actions::action(name).is_none() {
            return Err(format!("unknown action {name:?}"));
        }
    }
    for list in [&body.keys, &body.keys_mac, &body.keys_linux] {
        if let Some(keys) = list {
            if keys.is_empty() {
                return Err("empty keys".into());
            }
        }
    }
    // Only the running platform's chord can be checked — the other list targets a
    // different keymap, so it's carried through unvalidated by design.
    if let Some(keys) = body.platform_keys() {
        for k in keys {
            if !input::key_known(k) {
                return Err(format!("unknown key {k:?}"));
            }
        }
    }
    for list in [&body.launch, &body.launch_mac, &body.launch_linux] {
        if let Some(argv) = list {
            if argv.is_empty() {
                return Err("empty launch argv".into());
            }
        }
    }
    if let Some(seq) = &body.sequence {
        if !allow_sequence {
            return Err("nested sequence".into());
        }
        if seq.is_empty() {
            return Err("empty sequence".into());
        }
        for step in seq {
            validate(step, false)?;
        }
    }
    if body.clarify.as_deref() == Some("") {
        return Err("empty clarify".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_table_is_valid() {
        let (rules, bad) = compile_toml(DEFAULT_TOML, "builtin");
        assert_eq!(bad, 0, "default table has invalid rules");
        assert!(rules.len() > 30, "expected a fleshed-out default table, got {}", rules.len());
    }

    #[test]
    fn count_slot_matches_words_and_digits() {
        let re = compile_phrase("delete {n} words", false).unwrap();
        assert!(re.is_match("delete 3 words"));
        assert!(re.is_match("delete to words")); // homophone of "two", in a count slot
        assert!(!re.is_match("delete words"));
        assert!(!re.is_match("please delete 3 words")); // anchored
    }

    #[test]
    fn user_file_reloads_when_mtime_moves() {
        // Edit + save + speak, no restart — the point of the mtime check.
        let dir = std::env::temp_dir().join("remoat-cfg-reload");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("commands.toml");
        let rule = |label: &str| {
            format!("[[rules]]\nphrases = [\"zzz\"]\nlabel = \"{label}\"\naction = \"copy\"\n")
        };
        fs::write(&p, rule("first")).unwrap();
        let has = |rs: &Rules, l: &str| rs.iter().any(|c| c.label.as_deref() == Some(l));

        assert!(has(&rules_from(Some(p.clone())), "first"));

        fs::write(&p, rule("second")).unwrap();
        // Force a distinct mtime so the test can't depend on filesystem timestamp resolution.
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(2)).unwrap();
        drop(f);

        let after = rules_from(Some(p.clone()));
        assert!(has(&after, "second"), "stale table after edit");
        assert!(!has(&after, "first"));
    }

    #[test]
    fn bad_rules_are_skipped_not_fatal() {
        let src = r#"
            [[rules]]
            phrases = ["good"]
            action = "copy"

            [[rules]]
            phrases = ["two kinds"]
            action = "copy"
            text = "nope"

            [[rules]]
            phrases = ["bogus"]
            action = "definitely_not_an_action"

            [[rules]]
            phrases = ["shell out"]
            shell = "rm -rf /"
        "#;
        let (rules, bad) = compile_toml(src, "user");
        assert_eq!(rules.len(), 1);
        assert_eq!(bad, 3); // unknown `shell` key falls through to "no action"
    }
}
