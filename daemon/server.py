"""aiohttp daemon: serves the PWA, WebSocket control channel, HTTPS.

Protocol (JSON over WS):
  client -> server:
    {type:"hello"}
    {type:"move", dx, dy}
    {type:"click", button:"left"|"right", double:bool}
    {type:"scroll", dy}
    {type:"text", text}                 # dictation -> typed on target
    {type:"command", text}              # NL command -> routed
    {type:"ping"}
  server -> client:
    {type:"devices", devices:[{name,platform,host,port,self}]}
    {type:"command_result", ok, label, kind, message}
    {type:"pong"}
"""
import os
import socket

from aiohttp import web, WSMsgType

from . import commands, tls
from .discovery import Discovery
from .input import InputWorker

WEB_DIR = os.path.join(os.path.dirname(os.path.dirname(__file__)), "web")

# Certs go in a writable state dir — the package dir is read-only under Nix.
_STATE = os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state")
CERT_DIR = os.environ.get("REMOAT_CERT_DIR") or os.path.join(_STATE, "remoat", "certs")

# Input events forwarded straight to the worker
_PASSTHROUGH = {"move", "click", "scroll", "text"}


async def index(request):
    return web.FileResponse(os.path.join(WEB_DIR, "index.html"))


async def ws_handler(request):
    ws = web.WebSocketResponse(heartbeat=30)
    await ws.prepare(request)
    app = request.app
    worker: InputWorker = app["worker"]
    disc: Discovery = app["discovery"]
    peer = request.remote
    print(f"[ws] connect {peer}")

    # Push device list on connect
    await ws.send_json({"type": "devices", "devices": disc.devices()})

    async for msg in ws:
        if msg.type != WSMsgType.TEXT:
            continue
        try:
            data = msg.json()
        except Exception:
            continue
        t = data.get("type")

        if t in _PASSTHROUGH:
            worker.send(data)
        elif t == "command":
            res = commands.route(data.get("text", ""))
            if res.kind == "input" and res.payload:
                worker.send(res.payload)
                message = f"did: {res.label}"
                ok = True
            elif res.kind == "launch":
                message = commands.execute(res)
                ok = res.matched and "fail" not in message
            else:
                message, ok = res.message, False
            await ws.send_json({"type": "command_result", "ok": ok,
                                "label": res.label, "kind": res.kind, "message": message})
            print(f"[cmd] {data.get('text')!r} -> {message}")
        elif t == "hello":
            await ws.send_json({"type": "devices", "devices": disc.devices()})
        elif t == "ping":
            await ws.send_json({"type": "pong"})

    print(f"[ws] disconnect {peer}")
    return ws


def build_app() -> web.Application:
    app = web.Application()
    app.router.add_get("/", index)
    app.router.add_get("/ws", ws_handler)
    app.router.add_static("/", WEB_DIR, show_index=False)
    return app


def run(host: str = "0.0.0.0", port: int = 8443) -> None:
    ip = tls.lan_ip()
    name = socket.gethostname().split(".")[0]

    worker = InputWorker()
    worker.start()
    disc = Discovery(name, ip, port)
    disc.start()

    app = build_app()
    app["worker"] = worker
    app["discovery"] = disc

    ctx = tls.ssl_context(CERT_DIR)
    print("\n  remoat daemon ready")
    print(f"  open on your phone:  https://{ip}:{port}")
    print("  (accept the self-signed cert warning once)\n")
    try:
        web.run_app(app, host=host, port=port, ssl_context=ctx, print=None)
    finally:
        disc.stop()
