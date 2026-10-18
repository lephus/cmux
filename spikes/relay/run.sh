#!/usr/bin/env bash
# W0 Task 4: does cmux-relay really only see opaque bytes?
#
# Topology (all loopback, all isolated from the user's own cmux/ShellX state):
#
#   cmux-tui daemon --register--> tap :8788 --> cmux-relay :8787
#   cmux-tui client --connect---> tap :8788 --> cmux-relay :8787
#
# tap_proxy.py writes every byte it forwards to $CAPTURE (+ .decoded with
# WebSocket masking removed, + .frames.jsonl per-message log).
#
# Usage: spikes/relay/run.sh [path/to/cmux-tui/target/debug]
# Env:   CAPTURE (default /tmp/relay-capture.bin), KEEP=1 to leave processes up.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="${1:-$HOME/Documents/myProjects/shellx-pro/cmux-tui/target/debug}"
RELAY="$BIN/cmux-relay"
TUI="$BIN/cmux-tui"
CAPTURE="${CAPTURE:-/tmp/relay-capture.bin}"
MARKER="SHELLX-MARKER-7f3a"
WS_NAME="shellx-secret-ws"
WS_DIR="/tmp/shellx-secret-dir"
RELAY_URL="relay+ws://127.0.0.1:8788"
SESSION="shxrelay"

[ -x "$RELAY" ] && [ -x "$TUI" ] || { echo "build first: cargo build -p cmux-relay -p cmux-tui" >&2; exit 2; }

# Short runtime dir: Unix socket paths must stay under 104 bytes on macOS.
RT="$(mktemp -d /tmp/shxrelay.XXXXXX)"
chmod 700 "$RT"
mkdir -p "$RT/home" "$RT/tmp" "$RT/dstate" "$RT/cstate" "$RT/state" "$WS_DIR"
chmod 700 "$RT/home" "$RT/tmp" "$RT/dstate" "$RT/cstate" "$RT/state"
echo '{}' > "$RT/config.json"
rm -f "$CAPTURE" "$CAPTURE.decoded" "$CAPTURE.frames.jsonl"
echo "runtime dir: $RT"

phase() { python3 -I -c 'import json,sys,time;print(json.dumps({"kind":"phase","name":sys.argv[1],"t":round(time.time(),3)}))' "$1" >> "$CAPTURE.frames.jsonl"; }
PIDS=()
cleanup() {
  if [ "${KEEP:-0}" = 1 ]; then echo "KEEP=1: leaving processes running (runtime $RT)"; return; fi
  rm -f "$RT/hmac.secret" "$RT"/*.ticket
  # Stop the daemon through its own socket first so it reaps terminal hosts
  # and their shells, then kill anything left that belongs to this run.
  iso "$TUI" server stop --session "$SESSION" >/dev/null 2>&1 || true
  for pid in "${PIDS[@]}"; do pkill -P "$pid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true; done
  pkill -f -- "$RT" 2>/dev/null || true
  # Terminal hosts detach from the daemon (durable panes) and their argv does
  # not name $RT, so find ours by their isolated HOME and kill them + shells.
  for pid in $(pgrep -f "__terminal-host" || true); do
    if ps eww -o command= -p "$pid" | grep -q "HOME=$RT/home"; then
      pkill -P "$pid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true
    fi
  done
  pkill -f -- "cmux-relay serve --bind 127.0.0.1:8787" 2>/dev/null || true
  pkill -f -- "tap_proxy.py --listen 127.0.0.1:8788" 2>/dev/null || true
  sleep 1
  left="$(pgrep -fl -- "$RT|cmux-relay serve --bind 127.0.0.1:8787|tap_proxy.py --listen 127.0.0.1:8788" || true)"
  for pid in $(pgrep -f "__terminal-host" || true); do
    ps eww -o command= -p "$pid" | grep -q "HOME=$RT/home" && left="$left $pid(terminal-host)"
  done
  [ -n "${left// /}" ] && echo "WARNING: processes survived cleanup: $left" || echo "cleanup: no spike process left"
}
trap cleanup EXIT

# Clean, isolated environment: no user HOME, no inherited cmux/ghostty vars.
iso() {
  env -i HOME="$RT/home" TMPDIR="$RT/tmp/" PATH="/usr/bin:/bin:/usr/sbin:/sbin" \
    USER="$USER" LOGNAME="$USER" SHELL=/bin/zsh LANG=en_US.UTF-8 TERM=xterm-256color \
    CMUX_TUI_CONFIG="$RT/config.json" "$@"
}

# 1. Random 32-byte HMAC secret (hex-encoded, 64 bytes as the relay sees it),
#    kept in an owner-only file and only ever passed through the environment.
umask 077
openssl rand -hex 32 > "$RT/hmac.secret"
SLOT="$(openssl rand -hex 16)"
# Secret goes through a bash export (no argv, never echoed).
with_secret() ( export CMUX_RELAY_HMAC_SECRET; CMUX_RELAY_HMAC_SECRET="$(cat "$RT/hmac.secret")"; exec "$@"; )

mint() { # permission outfile
  install -m 600 /dev/null "$2"
  with_secret "$RELAY" ticket --permission "$1" --slot "$SLOT" > "$2"
}

with_secret "$RELAY" serve --bind 127.0.0.1:8787 > "$RT/relay.log" 2>&1 &
PIDS+=($!)
python3 -I "$HERE/tap_proxy.py" --listen 127.0.0.1:8788 --upstream 127.0.0.1:8787 \
  --capture "$CAPTURE" > "$RT/tap.log" 2>&1 &
PIDS+=($!)
sleep 1

# 2. Daemon registers through the tap with a Register ticket. No ws/iroh
#    listeners, so the relay is the only network route.
mint register "$RT/register.ticket"
( cd "$WS_DIR" && iso "$TUI" server start --session "$SESSION" --headless \
    --state "$RT/state" --remote-state-dir "$RT/dstate" \
    --relay "$RELAY_URL" --relay-slot "$SLOT" --relay-ticket-file "$RT/register.ticket" \
    > "$RT/daemon.log" 2>&1 ) &
PIDS+=($!)
for _ in $(seq 1 60); do grep -q '"registered"' "$CAPTURE.frames.jsonl" 2>/dev/null && break; sleep 0.5; done
grep -q '"registered"' "$CAPTURE.frames.jsonl" || { echo "daemon never registered"; cat "$RT/daemon.log"; exit 1; }
echo "daemon registered on relay"

# The owner-only admin socket is found from the daemon's runtime metadata.
ADMIN_ARGS=(--session "$SESSION" --state-dir "$RT/dstate")

# 3. Invitation: relay route only (explicit --advertise drops the same-host
#    Unix hint, so the client cannot bypass the relay).
mint connect "$RT/invite-connect.ticket"
install -m 600 /dev/null "$RT/invitation.txt"
iso "$TUI" remote enroll create "${ADMIN_ARGS[@]}" \
  --advertise "$RELAY_URL" \
  --relay-route "$RELAY_URL" --relay-slot "$SLOT" --relay-ticket-file "$RT/invite-connect.ticket" \
  > "$RT/invitation.txt"
echo "invitation created ($(wc -c < "$RT/invitation.txt") bytes, not printed)"

# 4. Client claims the invitation through the relay. The device name is set to
#    the short host name on purpose: it must not show up on the relay.
mint connect "$RT/client-connect.ticket"
iso "$TUI" remote connect "$RELAY_URL" --invite-file "$RT/invitation.txt" \
  --relay-slot "$SLOT" --relay-ticket-file "$RT/client-connect.ticket" \
  --state-dir "$RT/cstate" --local-socket "$RT/client.sock" --headless \
  --device-name "$(hostname -s)" \
  > "$RT/client.log" 2>&1 &
PIDS+=($!)

# 5. Owner approves on the daemon side.
INV_ID=""
for _ in $(seq 1 60); do
  INV_ID="$(iso "$TUI" remote enroll pending "${ADMIN_ARGS[@]}" --json 2>/dev/null \
    | python3 -I -c 'import json,sys
d=json.load(sys.stdin)
items=d if isinstance(d,list) else (d.get("pending") or d.get("invitations") or d.get("requests") or [])
print(items[0].get("invitation_id") or items[0].get("id") if items else "")' 2>/dev/null || true)"
  [ -n "$INV_ID" ] && break
  sleep 0.5
done
[ -n "$INV_ID" ] || { echo "no pending enrollment"; cat "$RT/client.log"; exit 1; }
iso "$TUI" remote enroll pending "${ADMIN_ARGS[@]}" --json > "$RT/pending.json"
iso "$TUI" remote enroll approve "$INV_ID" "${ADMIN_ARGS[@]}"
for _ in $(seq 1 60); do [ -S "$RT/client.sock" ] && break; sleep 0.5; done
[ -S "$RT/client.sock" ] || { echo "client socket never appeared"; cat "$RT/client.log"; exit 1; }
echo "client enrolled and connected; local socket $RT/client.sock"
iso "$TUI" remote enroll devices "${ADMIN_ARGS[@]}" --json > "$RT/devices.json" || true

# 6. Through the client's mux socket (so every byte crosses the relay):
#    workspace named shellx-secret-ws, terminal with cwd /tmp/shellx-secret-dir,
#    run the marker command, read the screen back.
C() { iso "$TUI" --socket "$RT/client.sock" "$@"; }
phase drive-start
jget() { python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1]))["value"][sys.argv[2]])' "$@"; }
C workspace create --name "$WS_NAME" --json > "$RT/ws.json"
WS_ID="$(jget "$RT/ws.json" workspace_id)"
C tab create terminal --workspace "$WS_ID" --cwd "$WS_DIR" --name "$WS_NAME-tab" --json > "$RT/tab.json"
TERM_ID="$(jget "$RT/tab.json" terminal_id)"
sleep 2
C terminal "$TERM_ID" write --text "echo $MARKER; pwd; hostname -s"$'\r' > /dev/null
sleep 2
C terminal "$TERM_ID" screen read > "$RT/screen.txt" 2>&1 || true
C workspace list --json > "$RT/workspaces.json" 2>&1 || true
C terminal list --json > "$RT/terminals.json" 2>&1 || true
phase drive-end
grep -q "$MARKER" "$RT/screen.txt" && echo "marker echoed on remote terminal (seen by client)" \
  || echo "WARNING: marker not seen in client screen read"
sleep 1

# 7. What did the relay see?
HOST="$(hostname -s)"
echo "--- grep counts (raw wire bytes | unmasked WS payloads) ---"
for needle in "$MARKER" "$WS_NAME" "shellx-secret-dir" "$HOST"; do
  raw=$(grep -a -c -- "$needle" "$CAPTURE" || true)
  dec=$(grep -a -c -- "$needle" "$CAPTURE.decoded" || true)
  echo "$needle: raw=$raw decoded=$dec"
done
echo "capture: $(wc -c < "$CAPTURE") bytes; frames: $(wc -l < "$CAPTURE.frames.jsonl") messages"
echo "--- positive controls (decoder works; masking hides client frames in raw) ---"
for needle in cmux-remote-noise '"type":"connect"' "$SLOT"; do
  echo "${needle:0:24}: raw=$(grep -a -c -- "$needle" "$CAPTURE" || true) decoded=$(grep -a -c -- "$needle" "$CAPTURE.decoded" || true)"
done
echo "--- relay-visible message summary ---"
python3 -I - "$CAPTURE.frames.jsonl" <<'PY'
import collections, json, sys
ev = [json.loads(line) for line in open(sys.argv[1])]
phase = {e["name"]: e["t"] for e in ev if e.get("kind") == "phase"}
texts = collections.Counter((e["dir"], e.get("json_type")) for e in ev if e.get("kind") == "text")
print("control (text) messages:", dict(texts))
print("control fields:", {e.get("json_type"): e.get("json_keys") for e in ev if e.get("kind") == "text"})
preludes = [e["plaintext_json"] for e in ev if "plaintext_json" in e]
print("plaintext binary frames (Noise prelude):", len(preludes),
      "fields:", sorted(preludes[0]) if preludes else None,
      "lanes:", sorted({p["lane"] for p in preludes}), "auth:", sorted({p["auth"] for p in preludes}))
binary = [e for e in ev if e.get("kind") == "binary" and "plaintext_json" not in e]
during = [e for e in binary if phase.get("drive-start", 0) <= e["t"] <= phase.get("drive-end", 0) + 0.5]
print(f"opaque binary frames: {len(binary)} ({sum(e['len'] for e in binary)} B); "
      f"during workspace/terminal drive: {len(during)} ({sum(e['len'] for e in during)} B)")
PY

echo "runtime dir kept for inspection: $RT"
