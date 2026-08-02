//! remoat daemon — serves the PWA, a WebSocket control channel, over HTTPS.
//!
//! Protocol:
//!   client -> server (JSON): {type:"hello"|"move"|"move_abs"|"click"|"scroll"|"text"
//!                                 |"command"|"wake"|"capture"|"stt_start"|"stt_stop"|"ping", …}
//!   client -> server (binary): microphone PCM (16-bit LE mono) between stt_start and stt_stop.
//!                              The mic is on the phone; the model runs here (see stt.rs).
//!   server -> client (JSON): {type:"devices"|"screen"|"command_result"|"wake_state"
//!                                 |"stt_partial"|"pong", …}
//!   server -> client (binary): a PNG crop of the desktop around the cursor, sent while a
//!                              client has capture on (see capture.rs) — throttled to
//!                              CAPTURE_INTERVAL and driven off move_abs, so it needs no
//!                              timer of its own.
//!
//! Both directions use binary frames, disambiguated by direction alone: up is audio, down is
//! a screen crop.
//!
//! Dictation is typed by the DAEMON, not the phone: with local STT the transcript lives here,
//! so the running-transcript diff that used to run in app.js runs in `emit_transcript`. The
//! `text` message stays for clients without a local model, which still use Web Speech.

mod actions;
mod apps;
mod capture;
mod commands;
mod config;
mod discovery;
mod dynamic;
mod input;
mod screen;
mod stt;
mod tls;
mod voice;
mod wake;
mod wm;

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
    /// Desktop union + its individual monitors. Capture needs the monitors (a crop must stay
    /// inside one output), so the whole layout is carried, not just the bounds.
    layout: Arc<screen::Layout>,
    /// Local streaming STT, or None when no model is installed — in which case the client is
    /// told so and keeps using the browser's Web Speech API.
    stt: Option<Arc<stt::Stt>>,
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
    // `stt` is capability negotiation: with a local model the phone streams raw audio here,
    // without one it falls back to Web Speech and keeps sending text.
    json!({"type":"screen","name":st.name,"platform":discovery::platform_label(),
           "w":st.layout.bounds.w,"h":st.layout.bounds.h,"stt":st.stt.is_some()})
    .to_string()
}

/// Which push-to-talk button is held: dictation types as you speak, command holds until release.
#[derive(Clone, Copy, PartialEq)]
enum SttMode {
    Type,
    Command,
}

/// The write half, shared between the receive loop and detached tasks (capture frames).
type Sink = Arc<tokio::sync::Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>;

/// Interleaved 16-bit little-endian PCM -> f32 in [-1, 1). i16 rather than f32 on the wire
/// halves the uplink for no audible cost at 16 kHz mono, and it is what the AudioWorklet sends.
/// A trailing odd byte (a split frame) is dropped rather than misread as a sample.
fn pcm_i16_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

/// Turn a new transcript into keystrokes (dictation only) and echo it for the button label.
///
/// The diff runs HERE rather than on the phone because the daemon now owns the transcript —
/// this is the server-side port of `streamText()` in app.js, and the reason the wire protocol
/// did not have to change for the Web Speech path.
async fn emit_transcript(
    st: &AppState,
    sink: &Sink,
    u: &mut stt::Utterance,
    transcript: &str,
    mode: SttMode,
) {
    use futures_util::SinkExt;
    let text = stt::spoken_to_text(transcript);
    let edit = u.advance(&text);
    if mode == SttMode::Type && !edit.is_noop() {
        // Deletes before the insert, on the one ordered channel — same invariant as the
        // `text` message from the Web Speech path.
        for _ in 0..edit.backspaces {
            let _ = st.tx.send(InputCmd::Key("backspace".into()));
        }
        if !edit.append.is_empty() {
            let _ = st.tx.send(InputCmd::Text(edit.append));
        }
    }
    let _ = sink
        .lock()
        .await
        .send(Message::Text(json!({"type":"stt_partial","text":text}).to_string()))
        .await;
}

/// Resolve an utterance through the command table and report one aggregated result. Shared by
/// the `command` message (Web Speech) and the local-STT path, so both behave identically.
async fn run_command(st: &AppState, sink: &Sink, t: &str) {
    use futures_util::SinkExt;
    let res = commands::route(t);

    // A resolution can expand to many steps (counts, chains). Dispatch them in order on the one
    // ordered channel, then report ONE aggregated result — the toast is the eyes-off feedback,
    // so it echoes the canonical intent.
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
            Exec::Run(argv) => match commands::execute_run(&argv) {
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
    let _ = sink
        .lock()
        .await
        .send(Message::Text(
            json!({"type":"command_result","ok":ok,"label":res.label,"kind":kind,
                   "steps":inputs + launches,"message":message})
                .to_string(),
        ))
        .await;
    eprintln!("[cmd] {t:?} -> {message}");
}

async fn handle_socket(socket: WebSocket, st: AppState) {
    // Split so a slow capture (a subprocess screenshot can take well over one frame) never
    // blocks the receive loop below it — otherwise every touchmove queued behind an in-flight
    // capture would stall the cursor itself, not just the image. The sink is shared because
    // both the loop (JSON replies) and detached capture tasks (binary frames) write to it.
    use futures_util::{SinkExt, StreamExt};
    let (sink, mut stream) = socket.split();
    let sink = Arc::new(tokio::sync::Mutex::new(sink));

    // Identify ourselves BEFORE the peer list: the app keys devices by name, so it has to
    // learn who this connection is before the same daemon shows up in `devices` — otherwise
    // it can't tell that entry is us and opens a second socket to itself.
    {
        let mut s = sink.lock().await;
        let _ = s.send(Message::Text(screen_frame(&st))).await;
        let _ = s
            .send(Message::Text(json!({"type":"devices","devices": st.disc.devices()}).to_string()))
            .await;
    }

    // At most one wake reference per connection, released on blur, idle, or disconnect.
    let mut holding = false;
    // Capture streaming: on only while this client is actively dragging (see "capture" below).
    let mut capturing = false;
    let mut last_capture: Option<std::time::Instant> = None;
    // Guards against piling up captures if a screenshot takes longer than CAPTURE_INTERVAL —
    // without it, a slow `grim`/`screencapture` would spawn a growing queue of subprocesses.
    let capture_busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The in-flight dictation, if this client is holding push-to-talk. `None` between presses.
    let mut utter: Option<stt::Utterance> = None;
    let mut utter_mode = SttMode::Type;

    loop {
        let msg = match tokio::time::timeout(WAKE_IDLE, stream.next()).await {
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
            // Binary from the client is microphone PCM (the phone owns the mic). Decoding is
            // CPU-bound, so it goes to a blocking thread; the Utterance moves in and comes back
            // out, which keeps the decoder's state per-connection without a lock.
            Message::Binary(pcm) => {
                let (Some(engine), Some(mut u)) = (st.stt.clone(), utter.take()) else { continue };
                let samples = pcm_i16_to_f32(&pcm);
                let Ok((mut u, transcript)) = tokio::task::spawn_blocking(move || {
                    let t = engine.push(&mut u, &samples);
                    (u, t)
                })
                .await
                else {
                    continue;
                };
                emit_transcript(&st, &sink, &mut u, &transcript, utter_mode).await;
                utter = Some(u);
                continue;
            }
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
                let (x, y) = st.layout.bounds.clamp(x, y);
                let _ = st.tx.send(InputCmd::MoveAbs { x, y, home });

                // Piggyback the capture frame on the move itself — the crop is centered on
                // exactly this position, and the drag already produces these messages at a
                // steady rate, so there's no separate timer to drift out of sync. Spawned
                // detached (not awaited here) so a slow screenshot never delays the NEXT
                // move_abs from reaching the input worker.
                let due = last_capture.map(|t| t.elapsed() >= CAPTURE_INTERVAL).unwrap_or(true);
                if capturing && due && !capture_busy.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    last_capture = Some(std::time::Instant::now());
                    let layout = st.layout.clone();
                    let dryrun = std::env::var("REMOAT_DRYRUN").is_ok();
                    let sink = sink.clone();
                    let busy = capture_busy.clone();
                    tokio::spawn(async move {
                        let frame = tokio::task::spawn_blocking(move || {
                            capture::capture_around(x, y, &layout, dryrun)
                        })
                        .await
                        .unwrap_or(None);
                        busy.store(false, std::sync::atomic::Ordering::Relaxed);
                        if let Some(bytes) = frame {
                            let _ = sink.lock().await.send(Message::Binary(bytes)).await;
                        }
                    });
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
                let t = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
                run_command(&st, &sink, &t).await;
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
                let _ = sink
                    .lock()
                    .await
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
            // Push-to-talk pressed. `rate` is whatever the phone's AudioContext gave us; the
            // engine resamples. A second start without a stop just replaces the utterance.
            "stt_start" => {
                let Some(engine) = st.stt.clone() else { continue };
                utter_mode = match v.get("mode").and_then(|x| x.as_str()) {
                    Some("command") => SttMode::Command,
                    _ => SttMode::Type,
                };
                let rate = v.get("rate").and_then(|x| x.as_i64()).unwrap_or(48000) as i32;
                utter = tokio::task::spawn_blocking(move || engine.start(rate, None)).await.ok();
            }
            // Released. Flush the tail, type whatever is left, and for a command hand the final
            // text to the same router the Web Speech path uses.
            "stt_stop" => {
                let (Some(engine), Some(mut u)) = (st.stt.clone(), utter.take()) else { continue };
                let Ok((mut u, transcript)) =
                    tokio::task::spawn_blocking(move || {
                        let t = engine.finish(&mut u);
                        (u, t)
                    })
                    .await
                else {
                    continue;
                };
                emit_transcript(&st, &sink, &mut u, &transcript, utter_mode).await;
                let final_text = u.emitted().to_string();
                match utter_mode {
                    // Trailing space so the next dictation isn't glued on — matches what the
                    // Web Speech path does in app.js::finish().
                    SttMode::Type => {
                        if !final_text.is_empty() {
                            let _ = st.tx.send(InputCmd::Text(" ".into()));
                        }
                    }
                    SttMode::Command => {
                        if !final_text.trim().is_empty() {
                            run_command(&st, &sink, &final_text).await;
                        }
                    }
                }
            }
            "hello" => {
                let mut s = sink.lock().await;
                let _ = s.send(Message::Text(screen_frame(&st))).await;
                let _ = s
                    .send(Message::Text(json!({"type":"devices","devices": st.disc.devices()}).to_string()))
                    .await;
            }
            "ping" => {
                let _ = sink.lock().await.send(Message::Text(json!({"type":"pong"}).to_string())).await;
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
    /// Download the local speech model so dictation runs offline instead of via Web Speech.
    FetchModel,
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
            "--fetch-model" => mode = Mode::FetchModel,
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
            Exec::Run(a) => println!("  run     {}", a.join(" ")),
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
        Mode::FetchModel => {
            if let Err(e) = stt::fetch_model() {
                eprintln!("fetch-model: {e}");
                std::process::exit(1);
            }
            return;
        }
        Mode::Serve => {}
    }
    let ip = tls::lan_ip();
    let name = gethostname::gethostname().to_string_lossy().split('.').next().unwrap_or("remoat").to_string();

    let layout = Arc::new(screen::detect());
    let tx = input::spawn_worker(layout.bounds);
    // Leave a core for the rest of the daemon; decoding must not starve input injection.
    let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2) as i32 - 1).max(1);
    let stt = stt::Stt::load(threads.min(4)).map(Arc::new);
    let disc = Discovery::start(name.clone(), ip.clone(), port);
    let (cert_pem, key_pem) = tls::ensure_cert();
    let wake = Arc::new(WakeLock::detect());
    eprintln!("[wake] inhibitor: {}", wake.describe());
    spawn_shutdown_release(wake.clone());

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/", get(index))
        .fallback_service(ServeDir::new(web_dir()))
        .with_state(AppState { tx, disc, wake, layout, stt, name: name.clone() });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_decodes_little_endian_and_normalizes() {
        // 0, +1, -1, full-scale negative.
        let bytes = [0x00, 0x00, 0x01, 0x00, 0xff, 0xff, 0x00, 0x80];
        let s = pcm_i16_to_f32(&bytes);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], 0.0);
        assert!((s[1] - 1.0 / 32768.0).abs() < 1e-9);
        assert!((s[2] + 1.0 / 32768.0).abs() < 1e-9);
        assert_eq!(s[3], -1.0);
        assert!(s.iter().all(|v| (-1.0..1.0).contains(v)));
    }

    #[test]
    fn a_split_sample_is_dropped_not_misread() {
        // WS frames can land mid-sample; a stray byte must not shift every later sample.
        assert_eq!(pcm_i16_to_f32(&[0x00, 0x80, 0x11]).len(), 1);
        assert!(pcm_i16_to_f32(&[0x42]).is_empty());
    }
}
