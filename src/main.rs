//! remoat daemon — serves the PWA, a WebSocket control channel, over HTTPS.
//!
//! Protocol (JSON over WS):
//!   client -> server: {type:"hello"|"move"|"click"|"scroll"|"text"|"command"|"ping", ...}
//!   server -> client: {type:"devices"|"command_result"|"pong", ...}

mod commands;
mod discovery;
mod input;
mod tls;

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

use commands::Action;
use discovery::Discovery;
use input::InputCmd;
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    tx: UnboundedSender<InputCmd>,
    disc: Arc<Discovery>,
}

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

async fn handle_socket(mut socket: WebSocket, st: AppState) {
    // Push device list on connect.
    let _ = socket
        .send(Message::Text(json!({"type":"devices","devices": st.disc.devices()}).to_string()))
        .await;

    while let Some(Ok(msg)) = socket.recv().await {
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
                let (kind, ok, message) = match res.action {
                    Action::Input(cmd) => {
                        let _ = st.tx.send(cmd);
                        ("input", true, format!("did: {}", res.label))
                    }
                    Action::Launch(argv) => {
                        let m = commands::execute_launch(&argv);
                        (("launch"), res.matched && !m.contains("fail"), m)
                    }
                    Action::None(msg) => ("none", false, msg),
                };
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"command_result","ok":ok,"label":res.label,"kind":kind,"message":message})
                            .to_string(),
                    ))
                    .await;
                eprintln!("[cmd] {t:?} -> {message}");
            }
            "hello" => {
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
}

fn parse_args() -> (String, u16) {
    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8443;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => { if let Some(v) = args.get(i + 1) { host = v.clone(); i += 1; } }
            "--port" => { if let Some(v) = args.get(i + 1) { port = v.parse().unwrap_or(8443); i += 1; } }
            _ => {}
        }
        i += 1;
    }
    (host, port)
}

#[tokio::main]
async fn main() {
    let (host, port) = parse_args();
    let ip = tls::lan_ip();
    let name = gethostname::gethostname().to_string_lossy().split('.').next().unwrap_or("remoat").to_string();

    let tx = input::spawn_worker();
    let disc = Discovery::start(name.clone(), ip.clone(), port);
    let (cert_pem, key_pem) = tls::ensure_cert();

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/", get(index))
        .fallback_service(ServeDir::new(web_dir()))
        .with_state(AppState { tx, disc });

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
