"""Self-signed TLS. Phone mic + Web Speech API require a secure context (HTTPS);
plain http:// over LAN blocks them. Cert generated once via openssl.
"""
import os
import socket
import ssl
import subprocess


def lan_ip() -> str:
    """Primary LAN IP. UDP connect is routing-table lookup only — no packet sent."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))
        return s.getsockname()[0]
    except Exception:
        return "127.0.0.1"
    finally:
        s.close()


def _openssl_config(sans: list) -> str:
    alt = "\n".join(
        f"{'IP' if _is_ip(v) else 'DNS'}.{i} = {v}" for i, v in enumerate(sans, 1)
    )
    return f"""[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = remoat
[v3]
subjectAltName = @alt
basicConstraints = CA:FALSE
[alt]
{alt}
"""


def _is_ip(v: str) -> bool:
    try:
        socket.inet_aton(v)
        return True
    except OSError:
        return False


def ensure_cert(cert_dir: str) -> tuple:
    """Return (certfile, keyfile), generating a self-signed pair if absent."""
    os.makedirs(cert_dir, exist_ok=True)
    certfile = os.path.join(cert_dir, "cert.pem")
    keyfile = os.path.join(cert_dir, "key.pem")
    if os.path.exists(certfile) and os.path.exists(keyfile):
        return certfile, keyfile

    ip = lan_ip()
    host = socket.gethostname()
    sans = ["localhost", "127.0.0.1", ip, host, f"{host}.local"]
    cfg_path = os.path.join(cert_dir, "openssl.cnf")
    with open(cfg_path, "w") as f:
        f.write(_openssl_config(sans))

    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
         "-keyout", keyfile, "-out", certfile, "-days", "825",
         "-config", cfg_path, "-extensions", "v3"],
        check=True, capture_output=True, stdin=subprocess.DEVNULL,  # openssl blocks on inherited stdin
    )
    print(f"[tls] generated self-signed cert (SAN: {', '.join(sans)})")
    return certfile, keyfile


def ssl_context(cert_dir: str) -> ssl.SSLContext:
    certfile, keyfile = ensure_cert(cert_dir)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(certfile, keyfile)
    return ctx
