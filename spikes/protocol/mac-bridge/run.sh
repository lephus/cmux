#!/usr/bin/env bash
# W0 Mac-bridge spike (D-W0-1 candidate b, M-B1): a stock cmux-tui client
# attaches, through cmux-relay, to a terminal that lives in the tagged
# ShellX/cmux dev app (libghostty surface), via the patched daemon's virtual PTY.
#
#   tagged app (cmux DEV w0-bridge)  <--unix socket, virtual PTY frames-->
#   patched cmux-tui daemon (Mux mirror) --register--> cmux-relay :8797
#   stock cmux-tui client --connect--> cmux-relay :8797
#
# Usage:
#   run.sh up    start relay + daemon, enroll + approve a stock client, create a
#                workspace and one terminal (it binds the app's focused terminal);
#                leaves everything running and writes $STATE/env.sh
#   run.sh down  stop everything this script started
# Env:
#   PATCHED_BIN  dir with the patched cmux-tui (default: this worktree's target/debug)
#   STOCK_BIN    dir with stock cmux-tui + cmux-relay (default: main checkout target/debug)
#   BRIDGE_SOCK  app bridge socket (default: /tmp/cmux-debug-w0-bridge-shellx-bridge.sock)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
PATCHED_BIN="${PATCHED_BIN:-$ROOT/cmux-tui/target/debug}"
STOCK_BIN="${STOCK_BIN:-$HOME/Documents/myProjects/shellx-pro/cmux-tui/target/debug}"
BRIDGE_SOCK="${BRIDGE_SOCK:-/tmp/cmux-debug-w0-bridge-shellx-bridge.sock}"
STATE="${STATE:-/tmp/shxbridge}"
RELAY_URL="relay+ws://127.0.0.1:8797"
SESSION="shxbridge"

iso() {
  env -i HOME="$STATE/home" TMPDIR="$STATE/tmp/" PATH="/usr/bin:/bin:/usr/sbin:/sbin" \
    USER="$USER" LOGNAME="$USER" SHELL=/bin/zsh LANG=en_US.UTF-8 TERM=xterm-256color \
    CMUX_TUI_CONFIG="$STATE/config.json" "$@"
}

down() {
  [ -d "$STATE" ] || { echo "nothing to stop"; return 0; }
  iso "$PATCHED_BIN/cmux-tui" server stop --session "$SESSION" >/dev/null 2>&1 || true
  for f in "$STATE"/*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null || true; done
  pkill -f -- "$STATE" 2>/dev/null || true
  pkill -f -- "cmux-relay serve --bind 127.0.0.1:8797" 2>/dev/null || true
  sleep 1
  rm -f "${STATE:?}/hmac.secret" "${STATE:?}"/*.ticket "${STATE:?}/invitation.txt"
  left="$(pgrep -fl -- "$STATE|cmux-relay serve --bind 127.0.0.1:8797" || true)"
  [ -n "$left" ] && echo "WARNING: survivors: $left" || echo "down: no spike process left"
}

up() {
  [ -x "$PATCHED_BIN/cmux-tui" ] || { echo "build: (cd cmux-tui && cargo build -p cmux-tui)" >&2; exit 2; }
  [ -x "$STOCK_BIN/cmux-tui" ] && [ -x "$STOCK_BIN/cmux-relay" ] || { echo "need stock cmux-tui + cmux-relay in $STOCK_BIN" >&2; exit 2; }
  [ -S "$BRIDGE_SOCK" ] || { echo "app bridge socket $BRIDGE_SOCK missing: launch the w0-bridge tag first" >&2; exit 2; }
  down >/dev/null 2>&1 || true
  rm -rf "${STATE:?}"; mkdir -p "$STATE"/{home,tmp,dstate,cstate,state}; chmod -R 700 "$STATE"
  echo '{}' > "$STATE/config.json"
  umask 077
  openssl rand -hex 32 > "$STATE/hmac.secret"
  SLOT="$(openssl rand -hex 16)"
  with_secret() ( export CMUX_RELAY_HMAC_SECRET; CMUX_RELAY_HMAC_SECRET="$(cat "$STATE/hmac.secret")"; exec "$@"; )
  mint() { install -m 600 /dev/null "$2"; with_secret "$STOCK_BIN/cmux-relay" ticket --permission "$1" --slot "$SLOT" > "$2"; }

  with_secret "$STOCK_BIN/cmux-relay" serve --bind 127.0.0.1:8797 > "$STATE/relay.log" 2>&1 &
  echo $! > "$STATE/relay.pid"
  sleep 1

  # Patched daemon: every default-shell terminal is a virtual PTY into the app.
  mint register "$STATE/register.ticket"
  iso CMUX_SHELLX_BRIDGE_SOCKET="$BRIDGE_SOCK" "$PATCHED_BIN/cmux-tui" server start \
    --session "$SESSION" --headless ${DAEMON_STATE_FLAGS:---state $STATE/state} --remote-state-dir "$STATE/dstate" \
    --relay "$RELAY_URL" --relay-slot "$SLOT" --relay-ticket-file "$STATE/register.ticket" \
    > "$STATE/daemon.log" 2>&1 &
  echo $! > "$STATE/daemon.pid"
  ADMIN=(--session "$SESSION" --state-dir "$STATE/dstate")
  for _ in $(seq 1 60); do iso "$PATCHED_BIN/cmux-tui" remote enroll devices "${ADMIN[@]}" --json >/dev/null 2>&1 && break; sleep 0.5; done

  mint connect "$STATE/invite-connect.ticket"
  install -m 600 /dev/null "$STATE/invitation.txt"
  iso "$PATCHED_BIN/cmux-tui" remote enroll create "${ADMIN[@]}" --advertise "$RELAY_URL" \
    --relay-route "$RELAY_URL" --relay-slot "$SLOT" --relay-ticket-file "$STATE/invite-connect.ticket" \
    > "$STATE/invitation.txt"

  # STOCK client, through the relay only.
  mint connect "$STATE/client-connect.ticket"
  iso "$STOCK_BIN/cmux-tui" remote connect "$RELAY_URL" --invite-file "$STATE/invitation.txt" \
    --relay-slot "$SLOT" --relay-ticket-file "$STATE/client-connect.ticket" \
    --state-dir "$STATE/cstate" --local-socket "$STATE/client.sock" --headless \
    --device-name "spike-stock-client" > "$STATE/client.log" 2>&1 &
  echo $! > "$STATE/client.pid"

  INV_ID=""
  for _ in $(seq 1 60); do
    INV_ID="$(iso "$PATCHED_BIN/cmux-tui" remote enroll pending "${ADMIN[@]}" --json 2>/dev/null | python3 -I -c 'import json,sys
d=json.load(sys.stdin); items=d if isinstance(d,list) else (d.get("pending") or d.get("invitations") or d.get("requests") or [])
print(items[0].get("invitation_id") or items[0].get("id") if items else "")' 2>/dev/null || true)"
    [ -n "$INV_ID" ] && break; sleep 0.5
  done
  [ -n "$INV_ID" ] || { echo "no pending enrollment"; cat "$STATE/client.log"; exit 1; }
  iso "$PATCHED_BIN/cmux-tui" remote enroll approve "$INV_ID" "${ADMIN[@]}"
  for _ in $(seq 1 60); do [ -S "$STATE/client.sock" ] && break; sleep 0.5; done
  [ -S "$STATE/client.sock" ] || { echo "client socket never appeared"; cat "$STATE/client.log"; exit 1; }
  echo "stock client enrolled through the relay; local socket $STATE/client.sock"

  C() { iso "$STOCK_BIN/cmux-tui" --socket "$STATE/client.sock" "$@"; }
  C workspace list --json > "$STATE/workspaces-before.json" 2>&1 || true
  C terminal list --json > "$STATE/terminals-before.json" 2>&1 || true
  C workspace create --name bridge-ws --json > "$STATE/ws.json"
  WS_ID="$(python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1]))["value"]["workspace_id"])' "$STATE/ws.json")"
  C terminal list --json > "$STATE/terminals-after-ws.json" 2>&1 || true
  cat > "$STATE/env.sh" <<ENV
STATE=$STATE
WS_ID=$WS_ID
STOCK=$STOCK_BIN/cmux-tui
C() { env -i HOME=$STATE/home TMPDIR=$STATE/tmp/ PATH=/usr/bin:/bin:/usr/sbin:/sbin USER=$USER LANG=en_US.UTF-8 TERM=xterm-256color CMUX_TUI_CONFIG=$STATE/config.json $STOCK_BIN/cmux-tui --socket $STATE/client.sock "\$@"; }
ENV
  echo "workspace $WS_ID; source $STATE/env.sh for C()"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  *) echo "usage: $0 up|down" >&2; exit 2 ;;
esac
