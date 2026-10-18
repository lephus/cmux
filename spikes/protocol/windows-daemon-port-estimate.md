# W0 Task 5 Step 2: cmux-tui remote protocol as the shared protocol (C16, candidate b)

Read-and-estimate only. Written 2026-10-18 against fork `main` @ `5348097e66` (checkout
`shellx-pro`). Nothing was built or launched. All refs are `cmux-tui/crates/...` unless they
start with `Sources/`, `Packages/`, `.github/` or `scripts/`. A day is one focused engineer-day
and includes unit tests. Ranges show uncertainty.

Input: W0 Task 4 (`spikes/relay/NOTES.md`). On macOS, `cmux-tui server start --relay` plus a
`remote connect` client went through `cmux-relay`, end to end and opaque.

## 0. What the code actually says (summary)

1. **The protocol core already builds on Windows. The edges do not.** These files contain no
   Unix code at all: `cmux-remote/src/{connection,link,session,client,crypto,service}.rs`,
   `provider/{websocket,stream}.rs`, `mux_client.rs`, and the relay provider (except its tests).
   Unix code sits only in the local IPC, the owner-only file checks, the workspace/agent services
   and the daemon runtime.
2. **cmux-remote has never been compiled for Windows.** `cmux-tui/Cargo.toml:55-56` makes
   `cmux-remote` and `cmux-remote-protocol` `cfg(unix)` dependencies. Upstream CI builds
   `cmux-tui.exe` for `x86_64-pc-windows-gnu` and only checks that remote commands *fail clearly*
   (`.github/workflows/cmux-tui-build-package.yml:518-527`). The many `cfg(not(unix))` branches
   in cmux-remote are therefore untested and may have bit-rotted.
3. **The PTY is not the blocker.** `cmux-pty` already opens a ConPTY through `portable-pty` on
   non-Unix (`cmux-pty/src/lib.rs:212-240`). `cmux-tui-core` runs local ConPTY surfaces on
   Windows, and upstream CI smoke-tests them (`cmux-tui-build-package.yml:497-515`,
   `cmux-tui.yml:666-722`). The line "PTYs are explicitly unavailable on non-Unix daemons"
   (`cmux-tui/docs/remote.md:260`) is about the **process service** (`spawn-process` with
   `io: pty`, `cmux-remote/src/workspace/process.rs:1021-1024`), the coding-agent API. It is not
   about mux terminals.
4. **There are two ways a client draws a remote terminal:**
   - `attach-surface` over the mux-control lane, in v6 PTY mode (`vt-state` + raw `output`) or
     v7 render mode (`cmux-tui/spec/events.md:16-18`). The `cmux-tui` TUI uses this, so it is
     what the Mac `RemoteTui` gets when it runs `cmux-tui attach --terminal`
     (`cmux-tui/src/session/remote.rs:3478`). It works for any surface the Mux owns, including
     in-process ConPTY surfaces.
   - `terminal-bytes-v1` (CMTH smart-renderer frames), used by `cmux-terminal-client`, i.e. by
     iOS. It needs a `__terminal-host` behind the surface (`cmux-tui-core/src/surface.rs:5964-5976`
     bails unless `PtyRuntime::Hosted`, and `Hosted` is `cfg(unix)`, `surface.rs:1741-1751`).
5. **The daemon can be embedded.** Enrollment can be done in-process:
   `RemoteDaemon::create_invitation_with_relay_access` / `pending_enrollments` / `approve` /
   `deny` (`cmux-remote/src/daemon.rs:1090-1327`). Relay tickets can come from a callback
   (`cmux-remote/src/provider/relay.rs:70-75`). So ShellX.exe does not need the admin socket or
   ticket files. One catch: the daemon runtime itself (`start_daemon_runtime`, `run_daemon`)
   lives in the `cmux-tui` **binary** crate (`cmux-tui/src/remote_runtime.rs:1995, 2055`) and has
   to be pulled out into a library.

## 1. Windows host: every Unix-only path the daemon needs

Two shapes:
- **Embedded (recommended).** ShellX.exe links the daemon as a Rust library: Mux in-process,
  relay registration, in-process enrollment.
- **Sidecar.** Ship `cmux-tui.exe server start` and drive it through its CLI and admin socket.
  This costs extra, listed at the end of the table.

| # | Item | Refs (Unix-only code) | Windows work | Days |
| --- | --- | --- | --- | --- |
| W1 | Compile `cmux-remote` for Windows | `cmux-tui/Cargo.toml:55-65` (unix-only deps); `cmux-remote/src/lib.rs:13-14` (`admin`), `:40-41` (`unix_socket`); **unguarded** `tokio::net::UnixStream` in `provider/dial.rs:21,125` (`SocksDialer`, WireGuard hub); `provider/mod.rs:15-16,47` (`unix` provider); `daemon.rs:40-41,1484-1590` (`UnixServer`); `bridge.rs:174-177`; `services.rs:15-25` (terminal-host imports); `ssh_bootstrap.rs` (15 `cfg(unix)`, 0 non-Unix branches); `workspace/files.rs` (104 `cfg(unix)`/20 not), `workspace/process.rs` (72/28), `workspace/path.rs` (29/6), `workspace/git.rs:6-9`, `http.rs:8-9,123-180` | Drop the `cfg(unix)` target gate. `cfg`-gate the SOCKS hub dialer. Fix the old `cfg(not(unix))` branches until `cargo check`/`clippy` pass on `-msvc` and `-gnu`. Workspace services (files, git, process, SSH bootstrap) stay compiled but return `unsupported`. ShellX does not need them for terminals. | 3 (2-4) |
| W2 | Owner-only state on NTFS | `secure_directory.rs:33-45` (**returns `Unsupported` on non-Unix, so `AuthDatabase` cannot create its state dir: blocker**); `secret_file.rs:68-76` (`read_owner_only*` returns `Unsupported`: relay ticket file, invitation file, WireGuard config, `cmux-tui/src/remote_cli.rs:1709,1956,1993`); `identity.rs:2095-2099` (mode 0600 on create), `:2118-2129` (dir fsync), `:2136-2143` (`restrict_file` is a **no-op** off Unix: the daemon's static X25519 key in `identity.json` would get inherited ACLs); `owner_lock.rs:93-101` (Windows branch checks only `is_file`, no owner check; the Unix branch `:61-90` checks uid, mode, nlink); `cmux-tui-core/src/platform.rs:1527-1530` (`restrict_permissions` no-op); `identity.rs:2145-2167` (`default_state_dir` reads only `XDG_STATE_HOME`/`HOME`, so it returns `None` on Windows) | One `win_acl` helper (`windows-sys` is already in the workspace at `Cargo.toml:111`; add `Win32_Security_Authorization`). It creates dirs and files with a protected DACL holding only the current user's SID, checks the owner SID, refuses reparse points (`FILE_FLAG_OPEN_REPARSE_POINT`), and checks the link count. Wire it into all six sites. Default state dir `%LOCALAPPDATA%\ShellX\remote`. Security-critical, so it needs its own tests. | 3 (2.5-4) |
| W3 | Local IPC (embedded) | Admin socket `admin.rs` (all of it; `tokio::net::UnixStream` `:19`, peer creds `:67,:299`, fd passing `:9-10`); `link.sock` `daemon.rs:1530-1590` + `unix_socket.rs:160-240` (`OwnedUnixListener`, `geteuid`, rlimit); mux-control bridge `services.rs:812-835` and `connect_owned_unix_socket` `:1558` (non-Unix stub rejects at `:836-850`); terminal-bytes `services.rs:649-787` (stub `:789-810`); predecessor recovery `cmux-tui/src/remote_runtime.rs:2106` (`cfg(unix)`) | Admin: call the `RemoteDaemon` API in-process (0.5). `link.sock`: skip on Windows, since it only serves the same-host `unix://` route the relay spike had to suppress anyway (0.25). Mux connector: the core server already listens on Windows AF_UNIX via `uds_windows` (`cmux-tui-core/src/platform.rs:116-160`), but tokio has no AF_UNIX on Windows. Add a `MuxConnector` seam in `services.rs`: an in-memory duplex into the in-process server, or a blocking `uds_windows` stream pumped into `tokio::io::duplex` (1.5). Note `platform.rs:36-39`: Windows sockets get no peer-credential check, so W2's DACL on the socket directory is the only guard. | 2.25 (2-3) |
| W4 | Daemon runtime as a library | `cmux-tui/src/main.rs:47-64` (remote CLI stub), `:66-67` (`remote_runtime` is `cfg(unix)`), `:2519-2539` (`reject_unsupported_remote_options`); `remote_runtime.rs` (6,850 lines: 15 `libc::flock`, 11 `geteuid`, 3 `kill`, 5 `symlink`, 7 `tokio::net::UnixListener`); `run_daemon` `:2055-2380` (lifecycle fence, `persist_runtime_info` `:2611`, `sync_state_directory` `:2754`, `serve_unix_with_shutdown` `:2148`, relay registration `:2193`) | New crate (e.g. `cmux-remote-host`): a trimmed `run_daemon` holding auth load, `RemoteDaemon::with_policy`, relay registration and runtime info. `flock` becomes `fs4` (already a dep), `kill(pid,0)` becomes `OpenProcess`/`GetExitCodeProcess`, `symlink` is dropped, and Unix sockets become optional. The same crate is the Mac embed (M-B2), so the work is shared. | 3.5 (3-5) |
| W5 | Relay tickets from the ShellX API | `provider/relay.rs:201-209` (file source uses `secret_file`); `:70-75` already has `Callback` | Callback into `shellx-core`'s ticket client. Register and connect tickets live ≤300 s. | 0.5 |
| W6 | Signals and process lifetime | `cmux-tui/src/main.rs:108-165` (`SIGNAL_WAKE` self-pipe, `sigaction`); `--exit-with-parent` via kqueue/pidfd (`cmux-tui/src/remote_cli.rs:467,562`; `config.rs:7519-7552`); `setsid` daemonize `remote_cli.rs:2728` | Embedded: the app owns shutdown through a `watch` channel, so nothing else is needed. | 0.25 |
| W7 | PTY | Already done: `cmux-pty/src/lib.rs:212-240` (`native_pty_system` = ConPTY), `cmux-tui-core/src/surface.rs:2214-2228` (ConPTY EOF on close), `:2440` (host runtime only on Unix, so Windows uses `PtyRuntime::Local`) | Nothing for mux terminals. The process-service PTY (`workspace/process.rs:1021-1024`, signals `:2581`; `docs/remote.md:260`) stays unsupported (2-3 d if ever wanted). | 0 |
| W8 | `terminal-bytes-v1` for non-hosted surfaces | `services.rs:649-787` → `mint-terminal-renderer-by-terminal` `cmux-tui-core/src/server.rs:12757-12765` → `surface.rs:5964-5976` (requires `Hosted`) | In-process CMTH renderer tap for `PtyRuntime::Local`: Snapshot, Output, Resized/ResizeAck, ViewerSize, Input (`terminal_host_protocol.rs:382-423`) over an in-memory duplex. **Deferrable**: the MVP needs only `attach-surface`. It is needed for `cmux-terminal-client` users (iOS, a Windows client using that crate). The Mac bridge needs the same code, so it is counted once. | 7 (6-8) |
| W9 | Snapshot codec outside the Unix module | `encode/decode_host_snapshot_payload` live in `mod unix` (`terminal_host_runtime.rs:431-432, 6335, 6344`; re-exports `:10521-10528`); `cmux-terminal-client/src/lib.rs` imports them | Move the pure codec out of `cfg(unix)`. Needed by W8 and by any Windows or Android build of `cmux-terminal-client`. | 0.5 |
| W10 | (Alternative to W8, not recommended) port `__terminal-host` | `cmux-tui/src/main.rs:1649-1655,1789-1795`; `terminal_host_runtime.rs:431-~6800` (~6.4k production lines: `poll`, `flock`, `killpg`/`getpgrp`, `waitid`, `setsid`, `renamex_np`/`renameat2`), `unix_process_scope.rs` (1,689 lines), Windows stubs `:10536-10560` | Detached host process, ConPTY inside the host, `uds_windows` listener (the host uses sync `UnixStream`, so this is nearly drop-in), job objects instead of process groups, handle-inheritance lists instead of fd isolation. This buys only shells that survive an app restart, and U14 rules that out. | 12-16 |
| W11 | Windows CI | `.github/workflows/cmux-tui.yml:666-722`, `cmux-tui-build-package.yml:413-546` | Add `cargo test -p cmux-remote` (portable subset) and a Windows `relay_e2e` (`cmux-remote/tests/relay_e2e.rs`) to the Task 6 workflow. | 2 |
| W12 | Windows *client* (ShellX.exe → a Mac or Windows host) | `cmux-terminal-client` has no Unix code; `Cargo.toml` `crate-type = ["staticlib","rlib"]`; transports are only Iroh/Direct (`src/lib.rs:71-74`) | Use it as an rlib and add a `relay+ws` transport (reuse `provider/relay.rs`). Needs W1 and W9. | 1.5 |
| — | Sidecar shape only (extra) | `remote_cli.rs` (5,557 lines; `kevent`/`pidfd_open`, `setsid`, `mkfifo` tests, 6 `std::os::unix::net::UnixListener`); admin peer creds `admin.rs:67,299` | Admin socket over a named pipe with an owner-only SD plus a `GetNamedPipeClientProcessId` → token-SID check; a link pipe; exit-with-parent via a job object; detach via `DETACHED_PROCESS`. | +6-8 |

**Windows host totals (embedded):**
- **MVP**, Mac and Windows clients using `attach-surface`: W1+W2+W3+W4+W5+W6+W9+W11 = **15 d (12-19)**.
- **Full**, adding `terminal-bytes` (W8) and the in-process Windows client (W12): **23.5 d (20-27)**.
- The sidecar shape adds 6-8 d.

## 2. Mac host: serving libghostty sessions over this protocol

Today the Mac is only a client. `RemoteTui` / `CmuxCloudTui` start the bundled
`Contents/Resources/bin/cmux-tui` (`scripts/install-cmux-tui-client.sh:2-4`) with
`remote connect … --headless --json` (`Packages/macOS/CmuxCloudTui/Sources/CmuxCloudTui/CloudTuiCommandLine.swift:32-51`)
and draw each remote terminal with `cmux-tui attach --terminal <id>` inside a Ghostty pane. The
Mac's own sessions are libghostty surfaces that own their PTYs. The Mac shares them only through
mobile RPC (`MobileTerminalByteTee` → `terminal.bytes`).

What already helps: the byte tap. `Sources/Mobile/MobileTerminalByteTee.swift:6-22` gets raw PTY
output through `ghostty_surface_set_pty_tee_cb` (a cmux fork addition), before the VT parser
sees it. Remote input already has an injection path (`mobile.terminal.input`).

| Option | What | Days |
| --- | --- | --- |
| **M-B1 Bridge: embed the Rust daemon, project Mac surfaces into its Mux (recommended)** | The steps are listed below this table. | **21 (17-25)** |
| M-B2 Server side in Swift | Re-implement Noise XX/XXpsk3 (CryptoKit has no BLAKE2s), 4 d; relay register/lease/circuit/join, 5 d; service multiplexer and lanes, 5 d; identity DB, enroll and approve, 4 d; a mux-control JSON subset (session snapshot, workspace and terminal list/create, `attach-surface` v6 with shared-sizing-v1), 8-10 d; CMTH renderer with snapshot encoding, 6 d; interop tests, 5 d. After that, it must keep pace with a Rust protocol (`protocol: 5`) that upstream changes often. | 37-45 |
| M-B3 Make Mac terminals cmux-tui terminals | Every Mac pane becomes `cmux-tui attach` against a local daemon, as `RemoteTui` does for remote machines. This breaks the app's assumption that it owns the surfaces: notifications, shell integration, agent detection, the CLI socket API, Teams. It parses every byte twice, which costs latency against §2.2. It also gives the largest upstream conflict. Rejected. | 30-45 |

M-B1 steps (21 d, range 17-25):
- **B1. Virtual-PTY seam in `cmux-tui-core`, 3.5 d.** Add a `PtyBackend` to `SurfaceOptions`
  that is used instead of `cmux_pty::open`/`spawn` (spawn path `surface.rs:2440-2530`). Its
  `MasterPty`/`Child`/`ChildKiller` are backed by callbacks: the writer sends input to Swift,
  `resize` sends the size to Swift, and the reader is fed from the tee. A `PtyRuntime::Local`
  surface (`surface.rs:1741-1746`) then works unchanged. The Mux's own `ghostty-vt` mirror
  serves `attach-surface`.
- **B2. Embed the daemon library, 3 d.** Same crate as W4. Add a C ABI, a macOS staticlib or
  xcframework, and a Swift wrapper, with relay tickets from the ShellX API.
- **B3. Wire the tee and input, 1.5 d.** Tee → FFI byte push. Input callback → the existing
  terminal-input path.
- **B4. Project Mac surfaces, 4 d.** Map workspace → pane → tab into Mux resources:
  create/close/rename, title, cwd.
- **B5. Sizing bridge, 2.5 d.** The core picks the smallest viewer, while the Mac already has
  shared sizing (`Packages/macOS/CmuxTerminalSharing`, 1.5k lines). Remote viewers have to
  become sharing participants, so that the two do not fight over one size.
- **B6. Remote-initiated create, 2 d.** When a remote sends `create-terminal`/`workspace`, call
  back into Swift to open a real libghostty tab.
- **B7. Enroll/approve UI, 1.5 d.** Use `daemon.rs:1090-1327`. The approval screen shows the
  device fingerprint, as the relay NOTES require.
- **B8. Tests and performance, 3 d.** Measure the CPU of the second parse and latency over the
  relay.

If Mac-hosted terminals must also reach `terminal-bytes` clients (iOS via
`cmux-terminal-client`), the bridge needs W8 too. W8 is shared with Windows and already counted
there.

Link-time note: ShellX would link `libghostty-vt` (from `ghostty-vt-sys`) next to GhosttyKit.
cmux iOS already does this: `Packages/iOS/CmuxMobileCloudUI/Package.swift:29-42` (through
`CmuxTerminalClient`) together with `Packages/iOS/CmuxMobileTerminal/Package.swift:19-36`
(GhosttyKit). So this is probably fine. The fallback is the bundled `cmux-tui` sidecar.

**(b) totals:**
- MVP: Windows 15 + Mac 21 = **36 d**.
- Full, with terminal-bytes and the Windows in-process client: 23.5 + 21 = **44.5 d (37-52)**.

## 3. Comparison with candidate (a), mobile RPC

What (a) is: Swift-only RPC with 4-byte length-prefixed JSON frames
(`Packages/Shared/CMUXMobileCore/Sources/CMUXMobileCore/MobileSyncProtocol.swift:188-262`) over
`CmxByteTransport` (`CmxTransport.swift:406-411`; factory `:508-520`). The iroh/Tailscale/WebSocket
transports and Stack Auth attach tickets carry all the security; there is no Noise layer. The
release gate lists 70+ methods
(`Packages/iOS/CmuxMobileShell/Sources/CmuxMobileShellReleaseGateSupport/MobileIrohReleaseGateRPCMethodInventory.swift`).

A Mac mirroring another Mac uses about 29 method and event names
(`Sources/Devices/DeviceTerminalMirrorSession.swift:113,306,399,462,478,493`; whole of
`Sources/Devices`):
- `mobile.terminal.{create,close,input,replay,viewport,size_policy.set,size_state,reattach,rename,participant.disconnect,detached}`
- `device.workspace.layout{,.apply,.changed}`, `device.workspace.terminal.create`
- `workspace.{create,close,action,updated}`, `terminal.{bytes,updated}`
- `mobile.sync.{fetch,delta}`, `mobile.events.{subscribe,probe}`, `mobile.host.status`
- `notification.feed.{list,mark_read,changed}`

Replay is a render-grid snapshot followed by `terminal.bytes` chained by sequence.

What a **Windows host for (a)** must write in Rust, with no reference implementation outside
Swift and no written spec (**34 d, range 30-40**):

| Part | Days |
| --- | --- |
| Framing, envelopes, `mobile.rpc.methods`/`host.status` negotiation | 2 |
| Relay + Noise byte channel and lane mapping (Rust can reuse `cmux-remote` crypto and the relay provider) | 4.5 |
| Authorization in place of the attach ticket | 2 |
| The method subset above | 9 |
| Render-grid producer from `ghostty-vt` (Swift reference `MobileTerminalRenderGrid*`, 2.6k lines) | 6 |
| State sync (`MobileStateSync*`, 1.2k lines) | 3 |
| Shared-sizing arbiter (`CmuxTerminalSharing` + `CmuxTerminalSizing`, 2k lines) | 4.5 |
| Interop harness against the Swift client | 3 |

On top of that:
- **Windows client for (a):** a Rust port of `DeviceLink`/`DeviceTerminalMirrorSession`/the
  projection, **12 d (10-14)**.
- **Mac additions for (a):**
  - Step 1's relay+Noise `CmxByteTransport` and host listener: 3 d.
  - Replacing Stack Auth and the iroh grant judge in `Sources/Mobile/MobileHostService+TicketAuthorization.swift`
    and `Sources/Devices/DeviceLinkAuthorization.swift`: 4 d.
  - Lanes and reconnect made production-grade: 3.5 d.
  - Total **10.5 d (9-12)**. The Rust Noise is **not** exposed by the current xcframework:
    `cmux-terminal-client`'s C ABI (`src/lib.rs:1568-3090`) is a complete cmux-tui client, not
    a byte channel. Step 1 has to add a new FFI for this.
- **(a) total: 34 + 12 + 10.5 = 56.5 d (49-66).**

Can iOS and Android clients speak (b)? **Yes, and iOS already does.**
- `cmux-terminal-client` is a Rust C ABI (connect by route and invitation; list/create
  workspaces and terminals; attach, send, key, paste, resize; frame/row copy; WireGuard). It is
  built as `CmuxTerminalClient.xcframework` for iOS 17 / macOS 14
  (`Packages/Shared/CmuxTerminalClient/Package.swift`).
- cmux iOS already uses it for Cloud VMs
  (`Packages/iOS/CmuxMobileCloudUI/Sources/CmuxMobileCloudUI/CmuxTerminalClientCloudAdapter.swift:41`).
- **Android/Flutter:** the same crate compiles for Android as far as the code goes (Android is
  `unix`). The work is a JNI shim or `dart:ffi` binding, 3-5 d, plus W9 and the relay transport
  in W12.
- **Unproven:** building `ghostty-vt-sys` (Zig) for the Android NDK.
- For (a), a Flutter or Android client would need the mobile RPC client, Noise and relay
  re-implemented, 15-25 d. ShellX mobile today is Flutter, not cmux iOS.

Upstream churn in the areas each candidate touches (commits in the last 90 / 30 days on fork
`main`):

| Candidate | Area | 90 d | 30 d |
| --- | --- | --- | --- |
| (a) | `Sources/Mobile` | 269 | 92 |
| (a) | `Sources/Devices` | 88 | 56 |
| (a) | `CMUXMobileCore` | 149 | 50 |
| (a) | `CmuxMobileRPC` | 73 | 22 |
| (b) | `cmux-remote` | 269 | 22 |
| (b) | `surface.rs` | 130 | 10 |
| (b) | `remote_runtime.rs` | 42 | 4 |
| (b) | `cmux-terminal-client` | 8 | 3 |

Upstream says "Windows support via ConPTY is planned for phase 2"
(`cmux-tui/docs/getting-started.md:145`), so the W1-W3 fixes could be offered upstream.

## 4. Score and recommendation

Scores run 1-5, where 5 is the least effort or risk.

| Criterion (spec §13) | (a) mobile RPC | (b) cmux-tui remote |
| --- | --- | --- |
| Windows host effort (incl. its client) | **2**: host 30-40 d + client 10-14 d; reimplements a Swift-only, unspecified protocol | **4**: MVP 12-19 d, full 20-27 d incl. client; the protocol core is already portable Rust |
| Mac host additions | **4**: 9-12 d; Mac already hosts | **2.5**: 17-25 d bridge (virtual PTY + projection + sizing) |
| Reuse by future iOS/Android | **2**: only cmux's Swift iOS client; Flutter/Android start from zero | **4.5**: the Rust C ABI client already ships on iOS; Android/Dart is a binding |
| Upstream-merge risk | **2**: hottest Swift area (92 commits/30 d in `Sources/Mobile`), and Windows must track its semantics by hand | **3.5**: mostly additive `cfg(windows)` and new files; one seam in `surface.rs`; could be upstreamed |
| **Sum** | **10** | **14.5** |
| **Total effort** | **56.5 d (49-66)** | **36 d MVP / 44.5 d (37-52) full** |

**Recommendation: (b) `cmux-tui-remote`.** Confidence: medium, about 70%. The Mac bridge
(M-B1) is the largest unproven piece, and the size of W1 stays unknown until Windows CI compiles
`cmux-remote` (Task 6).

Before D-W0-1, swap or supplement Step 1 with a 2-day **(b) Mac-bridge spike**: one libghostty
surface's tee → B1's virtual PTY → the embedded daemon → relay → a second Mac's `RemoteTui`
`attach --terminal`, with keystrokes going round the loop.

What candidate (a)'s live spike (Step 1) would still have to prove to overturn this:
1. A relay+Noise `CmxByteTransport` serves every lane mobile RPC opens (control, terminal,
   artifact, tunnel) through `MobileHostService` in ≤3 d. Step 1 must also write down the FFI it
   had to add.
2. Stack Auth and the iroh grant judge come out with ShellX approval in their place, touching
   few hot files, and the diff is counted.
3. A bounded Mac-to-Mac method subset works **without** the render-grid replay (bytes-only
   replay is acceptable on a cold attach). If it does, the Windows host for (a) drops to about
   15-20 d.
4. Latency over the relay and shared sizing match iroh. Without all four, (a) stays at about
   1.3-1.5× the cost of (b) and gives mobile nothing.

## 5. Top risks for (b)

1. **The Mac bridge is unproven.** Two sizing authorities (core smallest-viewer vs
   `CmuxTerminalSharing`), a second VT parse that must render the same (Kitty graphics, OSC
   colors), and the remote-create delegate could push M-B1 past 25 d. Mitigation: the 2-day
   spike above.
2. **`cmux-remote` has never compiled for Windows**, and owner-only ACLs are new
   security-critical code. Today `secure_directory`/`secret_file` return `Unsupported` and
   `restrict_file` is a no-op, so a careless port would leave the daemon's private key readable.
   W1 is ±2 d until Task 6's CI runs. W2 needs a security review.
3. **Upstream churn in `cmux-remote`** (269 commits/90 d), plus Android building
   `ghostty-vt-sys` through Zig/NDK. Mitigation:
   - keep the Windows changes as new files plus thin `cfg` hooks, and offer W1-W3 upstream;
   - add an Android cross-compile of `cmux-terminal-client` to CI early, before the mobile plan
     depends on it.
