//! Installed applications, discovered rather than hand-listed.
//!
//! The built-in table names a dozen apps by hand (`launch_mac`/`launch_linux`), which is fine
//! for "open browser" and useless for the other two hundred things on a real machine. Both
//! platforms already publish the list in a machine-readable form, so read that instead:
//!   Linux  -> XDG desktop entries under `$XDG_DATA_DIRS/applications` (+ the user's own)
//!   macOS  -> `.app` bundles under /Applications and ~/Applications
//!
//! This is the "generate commands from my config" idea, done against the *evaluated* result.
//! Parsing a nix config would mean `nix eval` on a Turing-complete language; desktop entries
//! are the same information after evaluation, portable, and already the OS's own answer to
//! "what can I launch".

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq)]
pub struct App {
    /// Human name, as shown in a launcher — what someone would actually say.
    pub name: String,
    pub argv: Vec<String>,
}

/// Parse a `.desktop` file. Returns None for anything not meant to be launched by name:
/// `NoDisplay`/`Hidden` entries, non-Application types, and entries needing a terminal.
///
/// Split from the directory walk so the fiddly part is testable without a populated system.
pub fn parse_desktop_entry(src: &str) -> Option<App> {
    let mut name = None;
    let mut exec = None;
    let mut in_entry = false;
    for raw in src.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            // Only the main group; trailing [Desktop Action …] groups have their own Name/Exec
            // and would otherwise overwrite the real ones.
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            // Skip localized keys (`Name[de]`) — we want the default.
            "Name" if name.is_none() => name = Some(v.trim().to_string()),
            "Exec" if exec.is_none() => exec = Some(v.trim().to_string()),
            "NoDisplay" | "Hidden" if v.trim().eq_ignore_ascii_case("true") => return None,
            "Terminal" if v.trim().eq_ignore_ascii_case("true") => return None,
            "Type" if !v.trim().eq_ignore_ascii_case("Application") => return None,
            _ => {}
        }
    }
    let argv = exec_to_argv(&exec?);
    let name = name?;
    (!name.is_empty() && !argv.is_empty()).then_some(App { name, argv })
}

/// `Exec=firefox %u --flag` -> ["firefox", "--flag"].
///
/// The `%f %u %F %U %i %c %k` field codes are placeholders the launcher is supposed to
/// substitute (a file to open, an icon, …). We are launching with no document, so they must be
/// DROPPED — passing `%u` through literally makes some apps open a file named "%u".
fn exec_to_argv(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None::<char>;
    for c in exec.chars() {
        match c {
            '"' | '\'' if quote.is_none() => quote = Some(c),
            c if Some(c) == quote => quote = None,
            ' ' if quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // Field codes also show up EMBEDDED (`--uri=%U`), not just standalone, so strip inside
    // each token and then drop whatever is left hollow. Passing `%U` through literally makes
    // the app try to open a file by that name.
    out.iter_mut().for_each(|t| *t = strip_field_codes(t));
    out.retain(|t| !t.is_empty() && !t.ends_with('='));
    out
}

/// Remove `%f %u %F %U %i %c %k …` while leaving a real `%` alone. Per the spec `%%` is an
/// escaped literal percent, and a `%` not followed by a code letter (`50%`) is just data.
fn strip_field_codes(t: &str) -> String {
    let mut out = String::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('%') => {
                out.push('%');
                it.next();
            }
            Some(n) if "fuFUickdDnNvm".contains(*n) => {
                it.next();
            }
            _ => out.push('%'),
        }
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn scan() -> Vec<App> {
    let mut dirs: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|d| Path::new(d).join("applications"))
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let data = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".local/share"));
        dirs.push(data.join("applications"));
        // NixOS puts the active system profile's entries here, which is where a nix-declared
        // app actually shows up.
        dirs.push(PathBuf::from("/run/current-system/sw/share/applications"));
        dirs.push(PathBuf::from(&home).join(".nix-profile/share/applications"));
    }
    let mut out: Vec<App> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "desktop") {
                if let Some(app) = std::fs::read_to_string(&p).ok().as_deref().and_then(parse_desktop_entry) {
                    out.push(app);
                }
            }
        }
    }
    dedupe(out)
}

#[cfg(target_os = "macos")]
fn scan() -> Vec<App> {
    let mut roots = vec![PathBuf::from("/Applications"), PathBuf::from("/System/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "app") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    // `open -a` by name rather than by path: it is what the OS itself uses,
                    // and it reuses a running instance instead of starting a second copy.
                    out.push(App { name: stem.to_string(), argv: vec!["open".into(), "-a".into(), stem.into()] });
                }
            }
            // One level down catches /System/Applications/Utilities.
            if p.is_dir() && p.extension().is_none() {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for e2 in sub.flatten() {
                        let p2 = e2.path();
                        if p2.extension().is_some_and(|x| x == "app") {
                            if let Some(stem) = p2.file_stem().and_then(|s| s.to_str()) {
                                out.push(App {
                                    name: stem.to_string(),
                                    argv: vec!["open".into(), "-a".into(), stem.into()],
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    dedupe(out)
}

/// Same app from several data dirs (system + user profile) is normal; keep the first.
fn dedupe(mut v: Vec<App>) -> Vec<App> {
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    v
}

/// Installed apps, rescanned occasionally. Not a startup-only snapshot: installing something
/// and then having to restart the daemon before you can say its name would be baffling.
pub fn installed() -> Arc<Vec<App>> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(60);
    static CACHE: OnceLock<Mutex<Option<(std::time::Instant, Arc<Vec<App>>)>>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(g) = cell.lock() {
        if let Some((at, v)) = g.as_ref() {
            if at.elapsed() < TTL {
                return v.clone();
            }
        }
    }
    let fresh = Arc::new(scan());
    if let Ok(mut g) = cell.lock() {
        *g = Some((std::time::Instant::now(), fresh.clone()));
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_and_exec() {
        let a = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox %u\nIcon=firefox\n",
        )
        .unwrap();
        assert_eq!(a.name, "Firefox");
        assert_eq!(a.argv, vec!["firefox"], "field codes must not reach the command line");
    }

    #[test]
    fn drops_entries_that_are_not_launchable_by_name() {
        for src in [
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n",
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nHidden=true\n",
            "[Desktop Entry]\nType=Link\nName=X\nExec=x\n",
            "[Desktop Entry]\nType=Application\nName=X\nExec=htop\nTerminal=true\n",
            "[Desktop Entry]\nType=Application\nName=X\n",
        ] {
            assert!(parse_desktop_entry(src).is_none(), "should have been skipped: {src:?}");
        }
    }

    #[test]
    fn ignores_desktop_action_groups() {
        // Regression guard: a trailing action group also has Name= and Exec=, and taking the
        // last one would make "open firefox" launch a private window.
        let a = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox %u\n\
             [Desktop Action new-private-window]\nName=Private Window\nExec=firefox --private-window\n",
        )
        .unwrap();
        assert_eq!(a.name, "Firefox");
        assert_eq!(a.argv, vec!["firefox"]);
    }

    #[test]
    fn keeps_real_flags_but_strips_field_codes() {
        let a = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Term\nExec=alacritty --class foo %F\n",
        )
        .unwrap();
        assert_eq!(a.argv, vec!["alacritty", "--class", "foo"]);
    }

    #[test]
    fn handles_quoted_exec_paths() {
        let a = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=App\nExec=\"/opt/My App/bin/app\" --run %u\n",
        )
        .unwrap();
        assert_eq!(a.argv, vec!["/opt/My App/bin/app", "--run"]);
    }

    #[test]
    fn strips_field_codes_embedded_in_an_argument() {
        // Caught end-to-end: `Exec=spotify --uri=%U` was passing `%U` straight through, so the
        // app would have tried to open a file called "%U".
        assert_eq!(exec_to_argv("spotify --uri=%U"), vec!["spotify"]);
        assert_eq!(exec_to_argv("app --file=%f --keep"), vec!["app", "--keep"]);
        assert_eq!(exec_to_argv("app %U %i %c"), vec!["app"]);
    }

    #[test]
    fn an_escaped_percent_stays_a_percent() {
        assert_eq!(exec_to_argv("app --fmt %%u"), vec!["app", "--fmt", "%u"]);
    }

    #[test]
    fn a_percent_in_a_real_argument_survives() {
        // Only the two-char field codes are placeholders; `50%` is a value.
        assert_eq!(exec_to_argv("app --zoom 50% %u"), vec!["app", "--zoom", "50%"]);
    }

    #[test]
    fn dedupe_prefers_one_entry_per_name() {
        let v = dedupe(vec![
            App { name: "Firefox".into(), argv: vec!["a".into()] },
            App { name: "firefox".into(), argv: vec!["b".into()] },
            App { name: "Slack".into(), argv: vec!["c".into()] },
        ]);
        assert_eq!(v.len(), 2);
    }
}
