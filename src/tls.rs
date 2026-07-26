//! Self-signed TLS. Phone mic + Web Speech API need a secure context (HTTPS).
//! Certs live in a writable state dir (the nix store is read-only).

use std::fs;
use std::net::UdpSocket;
use std::path::PathBuf;

/// Primary LAN IP. UDP connect is a routing-table lookup only — no packet is sent.
pub fn lan_ip() -> String {
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(a) = s.local_addr() {
                return a.ip().to_string();
            }
        }
    }
    "127.0.0.1".into()
}

fn cert_dir() -> PathBuf {
    if let Ok(c) = std::env::var("REMOAT_CERT_DIR") {
        if !c.is_empty() { return PathBuf::from(c); }
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.local/state")))
        .unwrap_or_else(|| ".".into());
    PathBuf::from(base).join("remoat").join("certs")
}

/// Return (cert_pem, key_pem), generating a self-signed pair if absent.
pub fn ensure_cert() -> (String, String) {
    let dir = cert_dir();
    let _ = fs::create_dir_all(&dir);
    let cert_p = dir.join("cert.pem");
    let key_p = dir.join("key.pem");
    if let (Ok(c), Ok(k)) = (fs::read_to_string(&cert_p), fs::read_to_string(&key_p)) {
        return (c, k);
    }

    let host = gethostname::gethostname().to_string_lossy().to_string();
    let sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        lan_ip(),
        host.clone(),
        format!("{host}.local"),
    ];
    let certified = rcgen::generate_simple_self_signed(sans).expect("generate self-signed cert");
    let cert_pem = certified.cert.pem();
    let key_pem = certified.key_pair.serialize_pem();
    let _ = fs::write(&cert_p, &cert_pem);
    let _ = fs::write(&key_p, &key_pem);
    eprintln!("[tls] generated self-signed cert in {}", dir.display());
    (cert_pem, key_pem)
}
