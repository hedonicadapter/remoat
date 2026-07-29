//! remoat daemon — serves the PWA, a WebSocket control channel, over HTTPS.
//!
//! Protocol:
//!   client -> server (JSON): {type:"hello"|"move"|"move_abs"|"click"|"scroll"|"text"
//!                                 |"command"|"wake"|"capture"|"ping", ...}
//!   server -> client (JSON): {type:"devices"|"screen"|"command_result"|"wake_state"|"pong", ...}
//!   server -> client (binary): a PNG crop of the desktop around the cursor, sent while a
//!                              client has capture on (see capture.rs) — throttled to
//!                              CAPTURE_INTERVAL and driven off move_abs, so it needs no
//!                              timer of its own.

mod actions;
mod capture;
mod commands;
mod config;
mod discovery;
mod input;
mod screen;
mod tls;
mod voice;
mod wake;

use std::net::SocketAddr;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tower_http::services::ServeDir;

use commands::Exec;
use discovery::Discovery;
use input::InputCmd;
use std::sync::Arc;
use wake::WakeLock;

#[derive(Clone)]
struct AppState {
    tx: UnboundedSender<InputCmd>,
    disc: Arc<Discovery>,
    wake: Arc<WakeLock>,
    screen: screen::Bounds,
    name: String,
}

/// Drop a client's wake lock if it goes quiet this long. The phone holds the lock while
/// focused and heartbeats to keep it; a pocketed phone whose socket never closes (dropped
/// Wi-Fi, killed tab) must not pin the display on indefinitely.
const WAKE_IDLE: std::time::Duration = std::time::Duration::from_secs(90);

/// Minimum gap between capture frames per connection. Shelling out to a screenshot tool on
/// every touchmove would be ~60 spawns/sec; this caps it to a still-live-feeling ~8fps.
/// Picked, not measured — no real hardware to profile against from this dev environment.
const CAPTURE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

fn web_dir() -> String {
    std::env::var("REMOAT_WEB_DIR").unwrap_or_else(|_| "web".into())
}

async fn index() -> impl IntoResponse {
    let path = format!("{}/index.html", web_dir());
    Html(std::fs::read_to_string(path).unwrap_or_else(|_| "<h1>remoat</h1>".into()))
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, st))
}

/// Who am I and how big is my desktop — one frame, sent unprompted on connect so the app
/// never has to ask before arranging.
fn screen_frame(st: &AppState) -> String {
    json!({"type":"screen","name":st.name,"platform":discovery::platform_label(),
           "w":st.screen.w,"h":st.screen.h})
    .to_string()
}

async fn handle_socket(mut socket: WebSocket, st: AppState) {
    // Identify ourselves BEFORE the peer list: the app keys devices by name, so it has to
    // learn who this connection is before the same daemon shows up in `devices` — otherwise
    // it can't tell that entry is us and opens a second socket to itself.
    let _ = socket.send(Message::Text(screen_frame(&st))).await;
    let _ = socket
        .send(Message::Text(json!({"type":"devices","devices": st.disc.devices()}).to_string()))
        .await;

    // At most one wake reference per connection, released on blur, idle, or disconnect.
    let mut holding = false;
    // Capture streaming: on only while this client is actively dragging (see "capture" below).
    let mut capturing = false;
    let mut last_capture: Option<std::time::Instant> = None;

    loop {
        let msg = match tokio::time::timeout(WAKE_IDLE, socket.recv()).await {
            Ok(Some(Ok(m))) => m,
            Ok(_) => break, // closed or errored
            Err(_) => {
                // Gone quiet: give the machine its screen lock back, keep serving. The
                // client's next heartbeat re-acquires.
                if holding {
                    st.wake.release();
                    holding = false;
                    eprintln!("[wake] client idle {}s — released", WAKE_IDLE.as_secs());
                }
                continue;
            }
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "move" => {
                let dx = v.get("dx").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let dy = v.get("dy").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let _ = st.tx.send(InputCmd::Move { dx, dy });
            }
            // Arranged devices: the app tracks the cursor and sends where it should be, so
            // a crossing can land the pointer on the exact entry edge of the next device.
            "move_abs" => {
                let x = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0) as i32;
                let y = v.get("y").and_then(|x| x.as_f64()).unwrap_or(0.0) as i32;
                // `home` = the pointer is teleporting here (crossing in, or a switch), so a
                // relative-only backend must re-find it before moving.
                let home = v.get("home").and_then(|x| x.as_bool()).unwrap_or(false);
                let (x, y) = st.screen.clamp(x, y);
                let _ = st.tx.send(InputCmd::MoveAbs { x, y, home });

                // Piggyback the capture frame on the move itself — the crop is centered on
                // exactly this position, and the drag already produces these messages at a
                // steady rate, so there's no separate timer to drift out of sync.
                if capturing && last_capture.map(|t| t.elapsed() >= CAPTURE_INTERVAL).unwrap_or(true) {
                    last_capture = Some(std::time::Instant::now());
                    let bounds = st.screen;
                    let dryrun = std::env::var("REMOAT_DRYRUN").is_ok();
                    let frame = tokio::task::spawn_blocking(move || {
                        capture::capture_around(x, y, bounds, dryrun)
                    })
                    .await
                    .unwrap_or(None);
                    if let Some(bytes) = frame {
                        let _ = socket.send(Message::Binary(bytes)).await;
                    }
                }
            }
            "click" => {
                let button = v.get("button").and_then(|x| x.as_str()).unwrap_or("left").to_string();
                let double = v.get("double").and_then(|x| x.as_bool()).unwrap_or(false);
                let _ = st.tx.send(InputCmd::Click { button, double });
            }
            "scroll" => {
                let dx = v.get("dx").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let dy = v.get("dy").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let _ = st.tx.send(InputCmd::Scroll { dx, dy });
            }
            "text" => {
                // Live dictation streams a running transcript: delete `backspace` chars
                // the recognizer revised, then type the new tail. Order matters — enqueue
                // deletes before the insert on the same ordered channel.
                let back = v.get("backspace").and_then(|x| x.as_u64()).unwrap_or(0);
                for _ in 0..back {
                    let _ = st.tx.send(InputCmd::Key("backspace".into()));
                }
                if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                    if !t.is_empty() {
                        let _ = st.tx.send(InputCmd::Text(t.to_string()));
                    }
                }
            }
            "command" => {
                let t = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                let res = commands::route(t);

                // A resolution can expand to many steps (counts, chains). Dispatch them in
                // order on the one ordered channel, then report ONE aggregated result — the
                // toast is the eyes-off feedback, so it echoes the canonical intent.
                let mut inputs = 0usize;
                let mut launches = 0usize;
                let mut errors: Vec<String> = Vec::new();
                for step in res.steps {
                    match step {
                        Exec::Input(cmd) => {
                            let _ = st.tx.send(cmd);
                            inputs += 1;
                        }
                        Exec::Launch(argv) => match commands::execute_launch(&argv) {
                            Ok(_) => launches += 1,
                            Err(e) => errors.push(e),
                        },
                        Exec::Note(n) => errors.push(n),
                    }
                }
                let kind = match (inputs, launches) {
                    (0, 0) => "none",
                    (_, 0) => "input",
                    (0, _) => "launch",
                    _ => "mixed",
                };
                let acted = inputs + launches > 0;
                let ok = res.matched && acted && errors.is_empty();
                let message = if ok {
                    res.label.clone()
                } else if !errors.is_empty() {
                    errors.join("; ")
                } else {
                    res.message.clone().unwrap_or_else(|| "nothing to do".into())
                };
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"command_result","ok":ok,"label":res.label,"kind":kind,
                               "steps":inputs + launches,"message":message})
                            .to_string(),
                    ))
                    .await;
                eprintln!("[cmd] {t:?} -> {message}");
            }
            // Focus-follows-wake-lock: the app sends on:true while it's focused (plus a
            // heartbeat), on:false when hidden. One reference per connection, so a repeated
            // on:true is idempotent rather than a leak.
            "wake" => {
                let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
                if on && !holding {
                    st.wake.acquire();
                    holding = true;
                } else if !on && holding {
                    st.wake.release();
                    holding = false;
                }
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"wake_state","held":holding,"available":st.wake.available(),
                               "clients":st.wake.holders()})
                            .to_string(),
                    ))
                    .await;
            }
            // The phone requests frames only while it's actively dragging — on:false both
            // stops new spawns and lets `capturing` gate out any in-flight straggler's effect
            // on the next loop iteration.
            "capture" => {
                capturing = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
            }
            "hello" => {
                let _ = socket.send(Message::Text(screen_frame(&st))).await;
                let _ = socket
                    .send(Message::Text(json!({"type":"devices","devices": st.disc.devices()}).to_string()))
                    .await;
            }
            "ping" => {
                let _ = socket.send(Message::Text(json!({"type":"pong"}).to_string())).await;
            }
            _ => {}
        }
    }

    // Socket closed: phone locked, tab killed, device switched away. Never leave the
    // inhibitor held for a client that's gone.
    if holding {
        st.wake.release();
    }
}

enum Mode {
    Serve,
    /// Resolve a phrase and print what it would do — no injection. For checking commands.toml
    /// edits without speaking into a live machine.
    Check(String),
    /// List the action allowlist (the names a commands.toml rule may reference).
    Actions,
}

fn parse_args() -> (String, u16, Mode) {
    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8443;
    let mut mode = Mode::Serve;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => { if let Some(v) = args.get(i + 1) { host = v.clone(); i += 1; } }
            "--port" => { if let Some(v) = args.get(i + 1) { port = v.parse().unwrap_or(8443); i += 1; } }
            "--check" => { if let Some(v) = args.get(i + 1) { mode = Mode::Check(v.clone()); i += 1; } }
            "--actions" => mode = Mode::Actions,
            _ => {}
        }
        i += 1;
    }
    (host, port, mode)
}

fn print_actions() {
    println!("action                platform mapping");
    for name in actions::NAMES {
        let (_, cmds) = actions::action(name).unwrap();
        let map: Vec<String> = cmds
            .iter()
            .map(|c| match c {
                InputCmd::Hotkey(ks) => ks.join("+"),
                InputCmd::Key(k) => k.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        println!("{name:<21} {}", map.join(", "));
    }
    if let Some(p) = config::user_path() {
        println!("\nuser table: {}", p.display());
    }
}

fn check(phrase: &str) {
    let res = commands::route(phrase);
    println!("{phrase:?} -> {}", if res.matched { res.label.as_str() } else { "(unmatched)" });
    match &res.message {
        Some(m) if *m != res.label => println!("  {m}"),
        _ => {}
    }
    for step in &res.steps {
        match step {
            Exec::Input(c) => println!("  input   {c:?}"),
            Exec::Launch(a) => println!("  launch  {}", a.join(" ")),
            Exec::Note(n) => println!("  note    {n}"),
        }
    }
}

/// Ctrl-C / SIGTERM: drop the wake lock before exiting. The inhibitor is also leashed to
/// this pid, so this is the fast path, not the only one — a SIGKILL still ends up released.
fn spawn_shutdown_release(wake: Arc<WakeLock>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
        wake.release_all();
        std::process::exit(0);
    });
}

#[tokio::main]
async fn main() {
    let (host, port, mode) = parse_args();
    match mode {
        Mode::Actions => return print_actions(),
        Mode::Check(p) => return check(&p),
        Mode::Serve => {}
    }
    let ip = tls::lan_ip();
    let name = gethostname::gethostname().to_string_lossy().split('.').next().unwrap_or("remoat").to_string();

    let bounds = screen::detect();
    let tx = input::spawn_worker(bounds);
    let disc = Discovery::start(name.clone(), ip.clone(), port);
    let (cert_pem, key_pem) = tls::ensure_cert();
    let wake = Arc::new(WakeLock::detect());
    eprintln!("[wake] inhibitor: {}", wake.describe());
    spawn_shutdown_release(wake.clone());

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/", get(index))
        .fallback_service(ServeDir::new(web_dir()))
        .with_state(AppState { tx, disc, wake, screen: bounds, name: name.clone() });

    let config = RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
        .await
        .expect("build rustls config");
    let addr: SocketAddr = format!("{host}:{port}").parse().expect("addr");

    println!("\n  remoat daemon ready");
    println!("  open on your phone:  https://{ip}:{port}");
    println!("  (accept the self-signed cert warning once)\n");

    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
        .expect("server");
}
