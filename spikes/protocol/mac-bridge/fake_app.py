#!/usr/bin/env python3
"""Stand-in for the macOS app side of the bridge (no libghostty).

Speaks the virtual-PTY frame protocol of cmux-tui-core/src/virtual_pty.rs:
acks each terminal, reports a fixed grid, replays a recorded tee file (or a
built-in colored prompt) and echoes every input frame back as output, the way
a shell with echo on would. Logs every frame to --log.

    fake_app.py --socket /tmp/fake-bridge.sock --log /tmp/fake-bridge.log [--tee recorded.bin]
"""
import argparse, json, os, socket, struct, threading, time

ap = argparse.ArgumentParser()
ap.add_argument("--socket", required=True)
ap.add_argument("--log", required=True)
ap.add_argument("--tee")
ap.add_argument("--cols", type=int, default=100)
ap.add_argument("--rows", type=int, default=30)
args = ap.parse_args()
lock = threading.Lock()


def log(msg):
    with lock, open(args.log, "a") as f:
        f.write(f"{time.time():.3f} {msg}\n")


def frame(kind, payload=b""):
    return bytes([ord(kind)]) + struct.pack(">I", len(payload)) + payload


def read_exact(conn, n):
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def serve(conn, n):
    try:
        header = read_exact(conn, 5)
        hello = json.loads(read_exact(conn, struct.unpack(">I", header[1:])[0]))
        log(f"conn {n} hello {hello}")
        conn.sendall(frame("A", json.dumps({"surface_id": f"fake-{n}", "mode": "fake"}).encode()))
        conn.sendall(frame("S", struct.pack(">HH", args.cols, args.rows)))
        tee = open(args.tee, "rb").read() if args.tee else b"\x1b[1;32mfake@mac\x1b[0m:\x1b[34m~\x1b[0m$ \x1b[31mred\x1b[0m\r\n$ "
        conn.sendall(frame("O", tee))
        while True:
            header = read_exact(conn, 5)
            payload = read_exact(conn, struct.unpack(">I", header[1:])[0])
            kind = chr(header[0])
            if kind == "I":
                log(f"conn {n} input {payload!r}")
                conn.sendall(frame("O", payload.replace(b"\r", b"\r\n")))
            elif kind == "R":
                log(f"conn {n} resize-wish {struct.unpack('>HH', payload)}")
            elif kind == "K":
                log(f"conn {n} kill")
                break
    except EOFError:
        log(f"conn {n} eof")
    finally:
        conn.close()


if os.path.exists(args.socket):
    os.unlink(args.socket)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(args.socket)
os.chmod(args.socket, 0o600)
srv.listen(8)
log(f"listening {args.socket}")
n = 0
while True:
    conn, _ = srv.accept()
    n += 1
    threading.Thread(target=serve, args=(conn, n), daemon=True).start()
