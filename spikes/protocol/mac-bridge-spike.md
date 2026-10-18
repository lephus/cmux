# W0 Task 5: Mac-bridge spike (candidate b, M-B1)

Time-boxed spike that replaces Step 1 of Task 5. Fork branch `shellx/w0-bridge`, based on
`main` @ `5348097e66`. Run on 2026-10-18/19 on the shared Mac, with Debug tag `w0-bridge` and
`CMUX_DEV_BACKEND_MODE=local`. Never merged, never pushed.

**Question.** Can the macOS app (cmux fork) serve one of its own libghostty terminals over the
cmux-tui remote protocol? The setup under test: the Rust daemon gets a "virtual PTY" fed from
the app's PTY tee, and input flows back into the Swift surface.

**Verdict: yes.** This supports finalizing **D-W0-1 = (b) cmux-tui-remote**. The Mac bridge is
no longer the unknown that could sink (b). The remaining open risk for (b) is the Windows
compile and port (Task 6 CI), which this spike does not address.

## 1. What worked (real, end to end)

The topology that ran:

```
tagged app "cmux DEV w0-bridge" (libghostty surface, real zsh)
   | tee bytes (ghostty_surface_set_pty_tee_cb) / input (ghostty_surface_text_input)
   v  Unix socket /tmp/cmux-debug-w0-bridge-shellx-bridge.sock, framed virtual-PTY protocol
patched cmux-tui daemon (Mux + ghostty-vt mirror), `server start --ephemeral --relay ...`
   | Noise over cmux-relay 127.0.0.1:8797 (register / connect tickets, as in spikes/relay)
   v
STOCK cmux-tui (main @ 5348097e66, sha256 daaa35f0...) `remote connect` -> enroll -> approve
   -> `terminal ... screen read|write` and an interactive `cmux-tui attach --terminal <id>`
```

Everything below crossed the relay with a stock client. "Mac pane" means the libghostty pane
in the tagged app. "Remote" means the stock client.

| Check | Result |
| --- | --- |
| Attach to an existing Mac terminal (the focused one) | Bound on the first daemon terminal (`mode=attach`). The remote saw the pre-attach screen through a VT snapshot (`readBoundedScreenTailVT`), at the Mac's grid (94x37). |
| Mac output reaches the remote | Yes. Colored prompt; `printf '\e[31mred\e[0m'`; bold, 256-color and truecolor (`evidence/colors.jpg`). |
| Remote input reaches the Mac shell | `terminal write "echo typed-from-remote-via-relay $((6*7))\r"` produced `42` in the Mac pane. Typing in the stock TUI viewer reached the shell, including Escape, arrow keys in vim (cursor-key mode) and Ctrl-C. |
| Full-screen apps | `vim -u NONE` with `syntax on` / `set number`, edited from the remote: identical (`evidence/vim.jpg`). `top -s 1`: identical cell for cell in the stock TUI viewer and in the Mac pane (checked by screenshot; not committed because it lists the machine's processes). |
| Double parse renders identically | Yes for everything tried (SGR 16/256/truecolor, bold, alternate screen, cursor keys, top, vim). Differences seen: the Mac pane's unfocused-split dimming (cmux UI, not parsing), and the attach snapshot turning palette colors into fixed RGB (see §3). |
| Mac-side resize reaches the mirror | Splitting the Mac pane (94→46 cols) and dragging the divider (46→44/49) sent `S` frames, and the mirror resized (`host grid 46x37 applied to mirror: Ok(true)`). |
| A smaller remote viewer joins the Mac's own shared sizing | A 100x30 remote viewer became a CmuxTerminalSharing participant through the existing `v2MobileTerminalViewport`. The Mac pane shrank to 46x30 and drew its hatch, the "46x30 · Fits everyone" chip and a viewer badge on the tab (`evidence/sizing-participant.jpg`). No new sizing code was needed. |
| Remote-initiated create | A stock `tab create terminal` made the daemon dial the app (`mode=create`). The app opened a **real new libghostty tab** (`Workspace.newTerminalSurface`); remote writes ran in that tab's zsh (`tty` = `/dev/ttys005`). |
| Second-parse cost (30.7 MB, `seq 1 3000000` in the bridged Mac pane, remote viewer attached) | Debug app finished in 12.1 s. The remote mirror reached the end marker **+0.06 s** later. CPU deltas: daemon (dev profile, optimized) **5.1 s** (~0.17 s/MB for the second parse plus attach fan-out), stock TUI viewer 5.9 s, `remote connect` client 2.3 s, relay 1.7 s. |

The daemon seam was also checked without the app. `fake_app.py` replays a recorded or built-in
tee stream and echoes input, and the same harness passed: mirror sized by the host to 100x30,
output parsed, input delivered (`evidence/fake-app.log`).

### Reproduce

```bash
cd ~/Documents/myProjects/shellx-pro-wt-bridge
# 1. app (Debug, tagged). reload.sh --launch needs team dev credentials, so launch the
#    tagged bundle directly like scripts/shellx/perf/perf_mac.py does:
CMUX_DEV_BACKEND_MODE=local ./scripts/reload.sh --tag w0-bridge
open -n --env CMUX_DISABLE_SESSION_RESTORE=1 --env CMUX_SOCKET_PATH=/tmp/cmux-debug-w0-bridge.sock \
  "<App path printed by reload.sh>"          # bridge socket: /tmp/cmux-debug-w0-bridge-shellx-bridge.sock
# 2. patched daemon (stock client + relay come from the main checkout's target/debug)
(cd cmux-tui && CARGO_PROFILE_DEV_DEBUG=0 cargo build -p cmux-tui)
DAEMON_STATE_FLAGS=--ephemeral spikes/protocol/mac-bridge/run.sh up   # enroll+approve via relay, binds focused Mac terminal
. /tmp/shxbridge/env.sh; C terminal list --json; C terminal <id> screen read
C terminal <id> write --text $'echo hi\r'; C tab create terminal --workspace "$WS_ID"   # remote create
# interactive stock viewer: run the stock binary from OUTSIDE ~/Documents (macOS TCC, see §3)
spikes/protocol/mac-bridge/run.sh down
# daemon-only check without the app:
python3 -I spikes/protocol/mac-bridge/fake_app.py --socket /tmp/shxfake/b.sock --log /tmp/shxfake/app.log &
STATE=/tmp/shxbridge-fake BRIDGE_SOCK=/tmp/shxfake/b.sock DAEMON_STATE_FLAGS=--ephemeral spikes/protocol/mac-bridge/run.sh up
```

## 2. What was stubbed or not proven

- **Sidecar, not embedded (B2 not exercised).** The daemon was a separate patched `cmux-tui`
  process, and the virtual PTY crossed a Unix socket. The Rust daemon was not linked into the
  app (no C ABI, no xcframework, no `libghostty-vt` + GhosttyKit link test). The sidecar shape
  worked, so it is a real fallback for M-B1.
- **Durable daemon state fails.** With `--state <dir>` (durable sessions), `workspace.create`
  returned `mutation.indeterminate`: the resource-creation settlement
  (`mux/resource_topology.rs:3674` `settle_resource_creation`) does not accept a non-hosted
  terminal in a durable session. With `--ephemeral` it works. For the Mac this is acceptable,
  because the app owns persistence and the daemon's Mux can be ephemeral. Otherwise the
  virtual terminal must be taught to the durable resource journal (+1 d).
- **Projection policy is a spike rule.** The first daemon terminal binds the Mac's focused
  terminal; every later one is a remote create. There is no workspace/pane/tab mapping, and no
  title, cwd or rename sync (B4).
- **Lifecycle.** Closing a Mac tab left its daemon terminal `running`. Unbinding a Mac surface
  is not mapped to a Mux close, and a daemon kill does not close the Mac tab. A fix is in the
  code (`pollSizes` sends exit when the surface is gone). It compiled but was not run live; see
  the last item.
- **Size ownership: one divergence reproduced.** The remote viewer was larger (49 cols) than the
  Mac pane (44 cols). The Mux engine resized the mirror to 49, the Mac correctly stayed at 44,
  and because the Mac grid had not *changed* the bridge did not resend it. The mirror reflowed
  differently from the Mac pane (`evidence/size-divergence.jpg`). Fix: the Mac (the PTY host)
  always echoes its effective grid after every remote resize (`remoteResize` →
  `sendSizeIfChanged(force: true)`), and the daemon applies host sizes directly. This works
  because the Mux engine only resizes when its own decision changes (`mux.rs:9822`
  `apply_terminal_grid`), so it does not fight host resizes. The fix compiled but was **not run
  live**.
- **One participant per daemon connection.** The spike forwards the Mux's combined wish (its
  smallest viewer) as a single "tui" participant per terminal. It does not forward each remote
  viewer as its own participant. A viewer detaching is not forwarded, so its pin stays until the
  bridge connection closes. The contract already defines this "relay" role
  (`docs/shared-terminal-sizing.md`, "A relay never decides... one attached-view lease per
  leaf").
- **Input fidelity.** Input goes through `ghostty_surface_text_input`, which writes the bytes to
  the PTY unchanged except `\n`→`\r` (`ghostty/src/input/text.zig:21`). So Ctrl-J from a remote
  arrives as CR. Production needs a raw PTY-write export in the ghostty fork (a GhosttyKit
  rebuild).
- **Attach snapshot.** The snapshot uses `readBoundedScreenTailVT` (200 rows) and then the live
  tee, with a small capture window that can duplicate a few bytes. It is not sequence-fenced. It
  turns palette colors into fixed RGB (`48;5;27` → `48;2;0;95;255`), so a remote with another
  theme would not remap them. The alternate-screen state and modes at attach time were not
  checked: vim and top were started *after* attach.
- **Not run:** the Rust unit test `virtual_pty::tests::virtual_pty_round_trips_output_input_and_sizes`
  (compiling the `cmux-tui-core` test binary did not fit the disk/CPU budget on the shared Mac;
  `fake_app.py` covered the same path end to end); Kitty graphics; OSC 8 / OSC 52; IME preedit
  (stays Mac-local); `terminal-bytes-v1` (iOS path, W8); latency over a real WAN relay.
- **Why the last build could not be launched.** The incremental rebuild with the two late fixes
  completed ("Build complete"). Free disk then fell below the 8 GB floor (the lead's
  `perf-base` build was running at the same time), and the guard stopped `reload.sh` during its
  final staging step, so the tagged bundle was gone. A rebuild would have been cold, so the
  spike stopped there.

## 3. Findings that change the estimate

1. **Sizing is much smaller than feared.** The Rust sizing reducer is the documented twin of
   Swift `CmuxTerminalSizing` (`sizing_policy.rs:1-12`, shared fixtures). The Mac already
   accepts a remote "tui" viewer as a sharing participant through `v2MobileTerminalViewport`,
   chip and hatch included. The bridge only has to act as a **relay**: forward each leaf viewer
   and apply the Mac host's grid to the mirror.
2. **Double parsing is already a pattern upstream.** Hosted terminals already run the Mux
   `ghostty-vt` as a non-authoritative mirror with query responses disabled
   (`surface.rs:2156` `hosted_terminal_callbacks`, `on_pty_write: None`). The bridge does the
   same (`surface.rs:2574`), so the mirror never double-answers DA/DSR. The cost: ~0.17 CPU-s/MB
   in the daemon while a terminal is projected. Mitigation: project or tee a Mac terminal only
   while a remote viewer is attached (+0.5 d).
3. **Remote create maps onto the Swift API that mobile already uses.** A daemon-side callback
   becomes `Workspace.newTerminalSurface` followed by `mobileCanonicalTerminalTarget`. In the
   embedded shape the socket becomes an FFI callback.
4. **macOS TCC.** Child processes of a dev app that read `~/Documents/...` block silently in
   `dyld open()` behind a privacy prompt. That showed up as a zsh startup stall and as the stock
   client "hanging". The fix was to run the binary from `/private/tmp`. ShellX ships `cmux-tui`
   in `Contents/Resources/bin`, so the product is unaffected; only dev/test setups are.

## 4. Hook points

Swift (app, all `#if DEBUG`):

| What | file:line |
| --- | --- |
| Tee installed per surface (existing) | `Sources/TerminalSurfaceRuntimeWiring.swift:199` (`ghostty_surface_set_pty_tee_cb`) |
| Tee fan-out → bridge (new hook, IO thread) | `Sources/TerminalOutputTeeCallback.swift:17` → `Sources/Mobile/ShellXBridgeSpike.swift:176` (`ShellXBridgeTee.consume`) |
| Bridge start | `Sources/AppDelegate.swift:1695` → `ShellXBridgeSpike.swift:220` (`startIfConfigured`) |
| Bind: project focused terminal / remote create | `ShellXBridgeSpike.swift:333` (`bind`), `:339` (`mobileCanonicalTerminalTarget(params: [:])`), `:349` (`Workspace.newTerminalSurface`) |
| Attach snapshot | `ShellXBridgeSpike.swift:384` (`readBoundedScreenTailVT`) |
| Remote input → real PTY | `ShellXBridgeSpike.swift:407` (`ghostty_surface_text_input`) |
| Remote resize → shared sizing participant | `ShellXBridgeSpike.swift:411` (`remoteResize`), `:417` (`TerminalController.v2MobileTerminalViewport`, existing `Sources/TerminalController.swift:16361`) |
| Mac grid → mirror | `ShellXBridgeSpike.swift:455` (`ghostty_surface_size`), forced echo `:435`, poll + close detection `:438` |
| Unbind / clear participant | `ShellXBridgeSpike.swift:467` |

Rust (`cmux-tui/crates/cmux-tui-core/src`):

| What | file:line |
| --- | --- |
| Module | `lib.rs:55` (`#[cfg(unix)] mod virtual_pty`) |
| Virtual PTY: spec, connect + handshake, master, writer, child | `virtual_pty.rs:44` (`spec_for`), `:124` (`open`), `:222` (`MasterPty::resize`, echo suppression), `:242` (`take_writer` → `I` frames), `:300` (`Child`), test `:333` |
| Seam in surface spawn (skips hosted path, replaces `cmux_pty::open`/`spawn`) | `surface.rs:2443`, `:2495`, local path kept at `:2522` |
| Mirror never answers queries | `surface.rs:2574` |
| Host grid → `Surface::resize` | `surface.rs:2706` |
| Mux engine does not fight host resizes (existing) | `mux.rs:9822` (`apply_terminal_grid`) |
| Durable-state blocker (existing) | `mux/resource_topology.rs:3674` |

Harness: `spikes/protocol/mac-bridge/run.sh` (relay, patched daemon, stock client,
enroll/approve, `up`/`down`) and `spikes/protocol/mac-bridge/fake_app.py`. Evidence:
`spikes/protocol/mac-bridge/evidence/`.

## 5. Revised M-B1 estimate

Was **21 d (17-25)**. Now **20 d (17-23)**. The center barely moves, but the unknowns are now
known items.

| Step | Was | Now | Why |
| --- | --- | --- | --- |
| B1 virtual-PTY seam | 3.5 | **3** (2.5-4) | Seam done in ~350 + ~70 lines with no core redesign. Left: tests, durable-state integration or an explicit ephemeral Mux (+1), seq-fenced snapshot |
| B2 embed daemon library | 3 | **3** (1.5-3.5) | Not exercised. The sidecar plus Unix-socket shape is proven and is the cheap fallback (1.5) |
| B3 tee + input wiring | 1.5 | **1.5** (1-2) | Done for the spike. Left: raw PTY-write export in the ghostty fork (`\n`→`\r`), backpressure off the IO thread |
| B4 project Mac surfaces | 4 | **4** (3.5-4.5) | Unchanged; only focused + create were done. Mapping, titles, cwd, close lifecycle (both directions) remain |
| B5 sizing bridge | 2.5 | **2** (1.5-2.5) | Remote viewer already joins CmuxTerminalSharing. Left: per-leaf participants, detach forwarding, verify the forced echo |
| B6 remote-initiated create | 2 | **1.5** (1-2) | Proven through `newTerminalSurface`. Left: FFI callback, workspace/pane targeting |
| B7 enroll/approve UI | 1.5 | 1.5 | Not touched |
| B8 tests + perf | 3 | 3 | Baseline measured (0.17 CPU-s/MB daemon parse; mirror lag 0.06 s locally) |
| New: lazy projection | — | 0.5 | Tee + parse only while a remote viewer is attached |
| **Total** | **21 (17-25)** | **20 (17-23)** | |

D-W0-1 arithmetic with this: (b) MVP ≈ Windows 15 + Mac 20 = **35 d**; full ≈ 23.5 + 20 =
**43.5 d**. (a) stays 56.5 d. Mac-host score for (b) moves from 2.5 to **3**, so the sum is 15
vs 10.

## 6. Verdict

**Yes: finalize D-W0-1 = (b) cmux-tui-remote.** The four risks the estimate named were all
exercised on the real app:

- terminal size ownership: works through the existing sharing participant model; one
  divergence was reproduced and has a known, compiled fix;
- double-parse fidelity: identical in every case tried; the mirror-never-answers rule follows
  an upstream precedent;
- remote-initiated create: works through the existing Swift API;
- input path: works, with one fidelity gap (`\n`→`\r`) that has a clear fix.

None of them needs a protocol change. The conditions are engineering items already in the
estimate:

- per-leaf sizing participants plus the forced echo;
- an ephemeral Mux, or durable-journal support for virtual terminals;
- a raw PTY-write export in the ghostty fork.

What still gates (b) overall is Task 6 (Windows CI compile of `cmux-remote` / `ghostty-vt-sys`),
not the Mac bridge.
