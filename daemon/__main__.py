import argparse

from .server import run

if __name__ == "__main__":
    p = argparse.ArgumentParser(prog="remoat", description="remoat remote-control daemon")
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--port", type=int, default=8443)
    args = p.parse_args()
    run(host=args.host, port=args.port)
