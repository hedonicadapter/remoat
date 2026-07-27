//! Wake lock — keep the desktop awake while the phone app is focused.
//!
//! Held by an external inhibitor process (`caffeinate` on mac, `systemd-inhibit` on Linux)
//! rather than by us poking power settings: killing the child is an atomic release.
//!
//! The child is also told to exit when THIS pid exits (`caffeinate -w`, `tail --pid` under
//! systemd-inhibit), because a killed daemon can't run its own cleanup — measured: a plain
//! `caffeinate -d -i` child survives SIGTERM to the daemon and pins the display awake
//! forever. Belt and braces: `main` releases on SIGINT/SIGTERM too.
//!
//! Refcounted across connections (two phones, or a phone plus a desktop tab). Callers must
//! release what they acquire — `main.rs` does it on blur, on client idle, and on disconnect,
//! because a phone going into a pocket must not pin the display on forever.

use std::process::{Child, Command};
use std::sync::Mutex;

pub struct WakeLock {
    method: Method,
    inner: Mutex<State>,
}

#[derive(Default)]
struct State {
    holders: usize,
    child: Option<Child>,
}

enum Method {
    /// argv of a process that holds the inhibit for as long as it runs.
    Spawn(Vec<String>),
    /// Log the argv instead of spawning (REMOAT_DRYRUN, and unit tests).
    Dryrun(Vec<String>),
    /// Nothing available (or turned off) — refcount still works, it just holds nothing.
    Off(String),
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Is `bin` on PATH? Cheaper and quieter than spawning it to find out.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

impl WakeLock {
    pub fn detect() -> WakeLock {
        let dryrun = std::env::var("REMOAT_DRYRUN").is_ok();
        if std::env::var("REMOAT_WAKE").map(|v| v == "off").unwrap_or(false) {
            return WakeLock::with(Method::Off("disabled via REMOAT_WAKE=off".into()));
        }
        let me = std::process::id().to_string();
        let found = if cfg!(target_os = "macos") {
            // -d = no display sleep (also suppresses the screen saver, hence the lock),
            // -i = no idle system sleep, -w = exit when our pid does. Asserts until killed.
            on_path("caffeinate")
                .then(|| argv(&["caffeinate", "-d", "-i", "-w", &me]))
        } else if on_path("systemd-inhibit") {
            // logind inhibitor; hypridle/swayidle honour it (hypridle unless
            // ignore_dbus_inhibit=true). `tail --pid` is the lifetime leash: it blocks
            // until our pid exits, so the inhibit can't outlive the daemon.
            Some(argv(&[
                "systemd-inhibit",
                "--what=idle:sleep",
                "--who=remoat",
                "--why=phone remote is focused",
                "--mode=block",
                "tail",
                &format!("--pid={me}"),
                "-f",
                "/dev/null",
            ]))
        } else {
            // Wayland idle-inhibit protocol client; no logind needed. No lifetime leash
            // available, so this one relies on the signal handler in main.
            on_path("wlinhibit").then(|| argv(&["wlinhibit"]))
        };

        WakeLock::with(match found {
            Some(a) if dryrun => Method::Dryrun(a),
            Some(a) => Method::Spawn(a),
            None => Method::Off(if cfg!(target_os = "macos") {
                "caffeinate not found".into()
            } else {
                "install systemd-inhibit or wlinhibit".into()
            }),
        })
    }

    fn with(method: Method) -> WakeLock {
        WakeLock { method, inner: Mutex::new(State::default()) }
    }

    /// What the daemon reports at startup so a no-op inhibitor isn't a silent surprise.
    pub fn describe(&self) -> String {
        match &self.method {
            Method::Spawn(a) => a.join(" "),
            Method::Dryrun(a) => format!("[dryrun] {}", a.join(" ")),
            Method::Off(why) => format!("unavailable ({why})"),
        }
    }

    pub fn available(&self) -> bool {
        !matches!(self.method, Method::Off(_))
    }

    pub fn holders(&self) -> usize {
        self.inner.lock().unwrap().holders
    }

    /// Take a reference; starts the inhibitor on the 0 -> 1 transition.
    pub fn acquire(&self) {
        let mut st = self.inner.lock().unwrap();
        st.holders += 1;
        if st.holders > 1 || st.child.is_some() {
            return;
        }
        match &self.method {
            Method::Spawn(a) => match Command::new(&a[0]).args(&a[1..]).spawn() {
                Ok(child) => {
                    eprintln!("[wake] holding: {}", a.join(" "));
                    st.child = Some(child);
                }
                // Fail soft: losing a wake lock is an annoyance, not worth dropping input.
                Err(e) => eprintln!("[wake] cannot start {}: {e}", a[0]),
            },
            Method::Dryrun(a) => println!("[dryrun] wake {}", a.join(" ")),
            Method::Off(why) => eprintln!("[wake] requested but {why}"),
        }
    }

    /// Drop a reference; kills the inhibitor on the 1 -> 0 transition.
    pub fn release(&self) {
        let mut st = self.inner.lock().unwrap();
        st.holders = st.holders.saturating_sub(1);
        if st.holders > 0 {
            return;
        }
        if let Some(mut child) = st.child.take() {
            let _ = child.kill();
            let _ = child.wait(); // reap; a zombie inhibitor still looks alive to `ps`
            eprintln!("[wake] released");
        }
    }

    /// Shutdown path: drop the inhibitor whatever the refcount says.
    pub fn release_all(&self) {
        let mut st = self.inner.lock().unwrap();
        st.holders = 0;
        if let Some(mut child) = st.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("[wake] released (shutting down)");
        }
    }
}

impl Drop for WakeLock {
    fn drop(&mut self) {
        if let Ok(mut st) = self.inner.lock() {
            if let Some(mut child) = st.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> WakeLock {
        WakeLock::with(Method::Dryrun(argv(&["true"])))
    }

    #[test]
    fn refcounts_across_connections() {
        let w = lock();
        w.acquire();
        w.acquire();
        assert_eq!(w.holders(), 2);
        w.release();
        assert_eq!(w.holders(), 1, "one client blurring must not wake-unlock the other");
        w.release();
        assert_eq!(w.holders(), 0);
    }

    #[test]
    fn release_without_acquire_cannot_underflow() {
        let w = lock();
        w.release();
        w.release();
        assert_eq!(w.holders(), 0);
        w.acquire();
        assert_eq!(w.holders(), 1, "underflow would have left the count negative-ish");
    }

    #[test]
    fn release_all_ignores_the_refcount() {
        let w = lock();
        w.acquire();
        w.acquire();
        w.release_all();
        assert_eq!(w.holders(), 0, "shutdown must not leave the inhibitor held");
    }

    #[test]
    fn inhibitor_is_leashed_to_our_pid() {
        // Regression guard: a child with no lifetime leash survives SIGTERM to the daemon
        // and pins the display awake forever (measured before this was added).
        let detected = WakeLock::detect();
        let a = match &detected.method {
            Method::Spawn(a) | Method::Dryrun(a) => a.clone(),
            Method::Off(_) => return, // no inhibitor on this box; nothing to leash
        };
        let me = std::process::id().to_string();
        assert!(
            a.iter().any(|arg| arg.contains(&me)),
            "inhibitor argv {a:?} does not reference our pid"
        );
    }

    #[test]
    fn unavailable_inhibitor_still_refcounts() {
        let w = WakeLock::with(Method::Off("test".into()));
        assert!(!w.available());
        w.acquire();
        assert_eq!(w.holders(), 1);
        w.release();
        assert_eq!(w.holders(), 0);
    }
}
