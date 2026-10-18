# W0 Task 4: cmux-relay opacity spike

Run on 2026-10-18 on macOS. The binaries were built from `shellx-pro` `main` @ `5348097e66`
(`cargo build -p cmux-relay -p cmux-tui` with `CARGO_PROFILE_DEV_DEBUG=0
CARGO_INCREMENTAL=0` to save disk). Reproduce with `spikes/relay/run.sh [target/debug]`.

## Topology

```
cmux-tui server start --relay relay+ws://127.0.0.1:8788   (daemon, Register ticket)
cmux-tui remote connect relay+ws://127.0.0.1:8788          (client, invitation + Connect ticket)
            |                                    |
            +---- tap_proxy.py :8788 (captures) -+--> cmux-relay serve :8787
```

- The relay HMAC secret is `openssl rand -hex 32`. It lives in an owner-only file and is
  passed only as the `CMUX_RELAY_HMAC_SECRET` env var, via a bash export. It is never in argv
  and never printed. Tickets are minted with `cmux-relay ticket --permission register|connect
  --slot <slot>`.
- The daemon has no ws or iroh listener. The invitation uses an explicit `--advertise
  relay+ws://…`, which drops the same-host `unix://` hint that the client would otherwise
  prefer. The relay is therefore the only route. During the workspace/terminal steps, 228
  opaque frames (31 KB) crossed the relay.
- All processes run under `env -i` with HOME, TMPDIR, the config and every state dir inside
  `/tmp/shxrelay.XXXXXX`. None of them touch the user's cmux/ShellX state.

## Result: PASS

The client went through its local mux socket and did four things. It created workspace
`shellx-secret-ws`. It created a terminal tab with `--cwd /tmp/shellx-secret-dir`. It ran
`echo SHELLX-MARKER-7f3a; pwd; hostname -s` (device name = `hostname -s`). It read the
screen back. The screen read showed the prompt `lehuuphu@PhuLes-MacBook-Pro
shellx-secret-dir %`, the marker, the cwd and the hostname. So the content did reach the
client.

| needle | raw wire bytes | unmasked WS payloads |
| --- | --- | --- |
| `SHELLX-MARKER-7f3a` | 0 | 0 |
| `shellx-secret-ws` | 0 | 0 |
| `shellx-secret-dir` | 0 | 0 |
| `hostname -s` (`PhuLes-MacBook-Pro`) | 0 | 0 |
| also: user name, `zsh`, `ws_`/`term_` ids, `/private/tmp` | 0 | 0 |

Masking is a real false-negative risk. Every peer→relay WebSocket frame is masked: the
daemon and the client are both WS *clients*. A grep of the raw bytes alone therefore misses
anything they send. `tap_proxy.py` unmasks frames into `<capture>.decoded`. The positive
controls show the decoder works:

| needle | raw | decoded |
| --- | --- | --- |
| `"type":"connect"` | 0 | 3 |
| the slot | 0 | 10 |
| `cmux-remote-noise` | 2–3 | 6 |

No `permessage-deflate` was negotiated. The upgrade carries no `Sec-WebSocket-Extensions`
header. The relay framing for `relay+ws` is one WS binary message per link frame. The batch
framing (`CMXB` magic) is only for `relay+do`.

## What the relay can see (plaintext on the wire)

1. **HTTP upgrade:** `GET /v1/relay`, `Host`, and `Authorization: Bearer <provider ticket>`.
   The native relay ignores this header. It verifies the copy of the ticket inside the
   control message (`cmux-relay/src/relay.rs:280-300`). A proxy in front could use the header
   for rate limiting.
2. **`RelayControl` JSON text frames** (`cmux-remote-protocol/src/relay.rs:97-152`):
   - `register {protocol, slot, ticket}` and `registered {lease_seconds}` (30)
   - `connect {protocol, slot, ticket, lane, generation}`
   - `allocated` / `incoming {circuit, lane, generation, join_ticket}`
   - `join {protocol, slot, circuit, lane, generation, ticket, role}`
   - `ready {circuit, lane, generation}` and `ping` / `pong {nonce}`
   - `error {code, message, retryable}`, with codes `peer-disconnected`,
     `circuit-not-found` and `slot-offline` observed

   `slot` is the opaque value we chose. `circuit` is a relay-random hex id. `lane` here is a
   random 32-char token, not a lane name. No workspace, path, host or user appears in any
   control message.
3. **Ticket claims** are base64url JSON, readable by anyone on the path:
   `version, issuer, permission, role, slot, circuit, lane, generation, issued_at_unix,
   expires_at_unix`. Opaque ids only.
4. **The first binary frame of every circuit is a plaintext `ClientPrelude`**
   (`cmux-remote/src/crypto.rs:269-279`, sent at `:386-411`, then used as the Noise
   prologue):
   `{name:"cmux-remote-noise", protocol:5, auth:"invitation"|"enrolled",
   invitation_id, session:[16 bytes], lane:"interactive"|"control"|"tunnel", lanes:[…],
   generation, connection_attempt:[16 bytes]}`.

   This leaks some metadata:
   - which traffic class each circuit carries (lane names in clear)
   - whether this is a first enrollment (with its invitation id) or an enrolled device
   - a stable logical session id that links reconnects

   No content, names or paths. ShellX can accept this. If it wants lane names hidden, that
   would be a protocol change.
5. **Noise messages:**
   - Handshake: `Noise_XX_25519_ChaChaPoly_BLAKE2s`, or `Noise_XXpsk3_…` for an invitation
     (`crypto.rs:19-20`). Message 1 is the client ephemeral key in clear. Message 2
     encrypts the daemon static key. Message 3 encrypts `ClientHello {device_name, resume}`,
     so the device name (hostname here) is never in clear.
   - Transport records: an 8-byte big-endian nonce counter in clear, then ChaCha20-Poly1305
     ciphertext with a 16-byte tag (`crypto.rs:336-355`).
6. **Side channels:** timing, frame sizes, circuit count (3 per client in `--lanes auto`),
   peer IPs.

## Ticket format the ShellX API must mint (Register + Connect only)

```
ticket  = "v2." + b64url_nopad(json(claims)) + "." + b64url_nopad(HMAC-SHA256(key, signing_payload))
claims  = {"version":2,"issuer":"cmux-relay","permission":"register"|"connect",
           "role":"daemon"|"client","slot":"<opaque>","circuit":null,"lane":null,
           "generation":null,"issued_at_unix":<now>,"expires_at_unix":<now+<=300>}
signing_payload = "cmux-relay-ticket-v2\n{version}\n{issuer}\n{permission}\n{role}\n{slot}\n"
                  "{circuit or ''}\n{lane or ''}\n{generation or ''}\n{issued_at}\n{expires_at}"
key     = the raw UTF-8 bytes of CMUX_RELAY_HMAC_SECRET (not decoded; >= 32 bytes)
```

- The HMAC covers `signing_payload`, not the JSON. The JSON is only the carrier: the relay
  parses it, rebuilds the payload and verifies.
- Rules:
  - `issuer` must equal the relay's `CMUX_RELAY_ISSUER` (default `cmux-relay`)
  - `register` needs `role=daemon`, with circuit, lane and generation all null
  - `connect` needs `role=client` and circuit null; lane and generation are optional
  - lifetime: `0 < expires-issued <= 300`, `issued_at <= now+30`, `now-issued_at <= 300`
  - each scope string is 1–256 bytes with no CR/LF
  - the whole ticket is at most 4096 bytes
- Code refs:
  - claims: `cmux-remote-protocol/src/relay.rs:34-47`
  - VERSION=2, MAX_LIFETIME 300 s, skew 30 s: `:50-67`
  - signing_payload: `:69-94`
  - issue: `cmux-relay/src/ticket.rs:76-86`
  - verify: `:136-178`
  - shape rules: `:182-234`
  - scope match: `:242-260`
  - env secret: `cmux-relay/src/config.rs:182-184` (min 32 bytes at `:249`)
  - CLI mint: `cmux-relay/src/main.rs:18-46`
- `join` tickets are minted by the relay itself (`relay.rs:1337` `join_claims`). The
  observed TTL is 30 s. They carry circuit, lane and generation. The API never mints them.
- The slot is an opaque string. The daemon and every client of that daemon use the same slot.
  The ShellX API decides who gets a Register ticket for a slot (the owner's machine) and who
  gets Connect tickets (approved devices of the same account).

## Enroll → approve flow (observed)

1. Daemon: `cmux-tui server start --session S --headless --state … --remote-state-dir D
   --relay relay+ws://… --relay-slot SLOT --relay-ticket-file register.ticket`. It prints
   the admin socket `D/sessions/<b64 session>/admin.sock` and its routes (relay plus
   `unix://…/link.sock`).
2. Owner: `cmux-tui remote enroll create --session S --state-dir D --advertise <relay>
   --relay-route <relay> --relay-slot SLOT --relay-ticket-file connect.ticket` prints a
   `cmux://enroll/<b64url(json)>` URI (`remote_cli.rs:2070-2071`). The JSON
   (`cmux-remote/src/identity.rs:65-78`) is:
   `{version:1, id, secret (32-byte PSK), daemon_public_key, daemon_fingerprint,
   daemon_name (= session name), expires_at_unix (<= 5 min), route_hints,
   relay_access:[{route, slot, ticket}], approval_required:true}`.
3. Client: `cmux-tui remote connect <relay> --invite-file inv.txt --relay-slot SLOT
   --relay-ticket-file connect.ticket --state-dir C --device-name N --headless` does a Noise
   XXpsk3 handshake through the relay. The client then waits; nothing is usable yet.
4. Owner: `remote enroll pending --json` lists
   `[{device_fingerprint, device_name, invitation_id, requested_at_unix}]`. Then
   `remote enroll approve <invitation_id>` prints the device record
   `{id = fingerprint, name, public_key, created_at_unix, …}`.
5. The client finishes: it prints its local mux socket and opens the control and tunnel
   lanes with `auth:"enrolled"`. Ordinary `cmux-tui --socket <local>` commands then run on
   the remote daemon.

## Notes for ShellX (spec §8.5)

- Principle 1 holds: the relay sees no content.
- Principle 2 holds as long as the invitation URI never passes through the ShellX server
  in clear. The URI carries the PSK. A server holding it could claim the invitation first.
  On-site approval is then the only guard, so the approval UI must show the device
  fingerprint as well as the device name.
- If the API relays invitations, it should carry only an opaque handle, with the PSK
  transferred out of band (QR or `#fragment`). This matches §8.5 point 3.
- The device name defaults to `$HOSTNAME`, else `cmux-client-<pid>` (`remote_cli.rs:2964`).
  It is encrypted in transit and stored on the daemon.
- In our runs the durable `__terminal-host` processes outlived `server stop`. `run.sh` finds them
  by their isolated HOME and kills them.
