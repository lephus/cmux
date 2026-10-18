#if DEBUG
import CmuxTerminal
import Darwin
import Foundation
import GhosttyKit
import os

// W0 spike (branch shellx/w0-bridge, never merged): the Mac app serves its own
// libghostty terminals to a cmux-tui daemon's "virtual PTY"
// (cmux-tui-core/src/virtual_pty.rs) over a same-host Unix socket.
//
//   libghostty IO thread --tee--> ShellXBridgeTee --'O' frames--> daemon mirror
//   daemon (remote keys)  --'I' frames--> ghostty_surface_text_input (real PTY)
//   daemon (Mux resize)   --'R' frames--> CmuxTerminalSharing participant
//   Mac grid (real PTY)   --'S' frames--> daemon mirror resize
//
// Frame: [type u8][len u32 BE][payload]. One connection = one terminal.
// The socket path is CMUX_SHELLX_BRIDGE_SOCKET, or derived from the tagged
// CMUX_SOCKET_PATH (/tmp/cmux-debug-<tag>.sock -> /tmp/cmux-debug-<tag>-shellx-bridge.sock).

enum ShellXBridgeFrameType {
    // daemon -> app
    static let hello: UInt8 = 0x48 // 'H' JSON {cols, rows, cwd?}
    static let input: UInt8 = 0x49 // 'I' raw bytes for the real PTY
    static let resize: UInt8 = 0x52 // 'R' cols u16 BE, rows u16 BE (Mux wish)
    static let kill: UInt8 = 0x4B // 'K'
    // app -> daemon
    static let ack: UInt8 = 0x41 // 'A' JSON {surface_id, workspace_id, mode}
    static let output: UInt8 = 0x4F // 'O' raw PTY output (tee)
    static let size: UInt8 = 0x53 // 'S' cols u16 BE, rows u16 BE (real PTY grid)
    static let exit: UInt8 = 0x45 // 'E'
}

private let shellXBridgeLog = Logger(subsystem: "dev.cmux", category: "shellx-bridge-spike")

/// One daemon connection. Writes go through a serial queue so tee output from
/// the libghostty IO thread never blocks on the socket.
final class ShellXBridgeConnection: @unchecked Sendable {
    let fd: Int32
    let id: Int
    private let writeQueue: DispatchQueue
    private let lock = NSLock()
    private var holding = true
    private var held: [Data] = []
    private var closed = false
    // Counters for the spike write-up.
    private(set) var outputBytes = 0
    private(set) var inputBytes = 0

    init(fd: Int32, id: Int) {
        self.fd = fd
        self.id = id
        self.writeQueue = DispatchQueue(label: "dev.cmux.shellx-bridge.write.\(id)", qos: .userInitiated)
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
    }

    static func frame(_ type: UInt8, _ payload: Data) -> Data {
        var out = Data(capacity: payload.count + 5)
        out.append(type)
        var len = UInt32(payload.count).bigEndian
        withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
        out.append(payload)
        return out
    }

    static func sizePayload(cols: UInt16, rows: UInt16) -> Data {
        var data = Data()
        var c = cols.bigEndian, r = rows.bigEndian
        withUnsafeBytes(of: &c) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: &r) { data.append(contentsOf: $0) }
        return data
    }

    func send(_ type: UInt8, _ payload: Data) {
        let bytes = Self.frame(type, payload)
        writeQueue.async { [self] in
            if isClosed { return }
            bytes.withUnsafeBytes { raw in
                guard var p = raw.baseAddress else { return }
                var left = raw.count
                while left > 0 {
                    let n = Darwin.write(fd, p, left)
                    if n < 0 {
                        if errno == EINTR { continue }
                        shutdownSocket()
                        return
                    }
                    left -= n
                    p = p.advanced(by: n)
                }
            }
        }
    }

    /// Called on the libghostty IO thread.
    func teeOutput(_ data: Data) {
        lock.lock()
        defer { lock.unlock() }
        if closed { return }
        outputBytes += data.count
        if holding {
            held.append(data)
        } else {
            send(ShellXBridgeFrameType.output, data)
        }
    }

    func noteInput(_ count: Int) {
        lock.lock(); inputBytes += count; lock.unlock()
    }

    /// Ends the attach hold: the snapshot goes first, then any bytes the tee
    /// captured while the snapshot was being taken (may duplicate a few).
    func releaseHold(snapshot: Data?) {
        lock.lock()
        defer { lock.unlock() }
        if let snapshot, !snapshot.isEmpty {
            send(ShellXBridgeFrameType.output, snapshot)
        }
        for chunk in held { send(ShellXBridgeFrameType.output, chunk) }
        held.removeAll()
        holding = false
    }

    var isClosed: Bool {
        lock.lock(); defer { lock.unlock() }
        return closed
    }

    func shutdownSocket() {
        lock.lock()
        let wasClosed = closed
        closed = true
        lock.unlock()
        if !wasClosed { Darwin.shutdown(fd, SHUT_RDWR) }
    }

    func closeAfterDrain() {
        shutdownSocket()
        writeQueue.async { [fd] in Darwin.close(fd) }
    }

    private func readExactly(_ count: Int) -> Data? {
        var data = Data(count: count)
        var got = 0
        let ok = data.withUnsafeMutableBytes { raw -> Bool in
            guard let base = raw.baseAddress else { return count == 0 }
            while got < count {
                let n = Darwin.read(fd, base.advanced(by: got), count - got)
                if n < 0 && errno == EINTR { continue }
                if n <= 0 { return false }
                got += n
            }
            return true
        }
        return ok ? data : nil
    }

    /// Blocking frame read on the connection's reader thread.
    func readFrame() -> (UInt8, Data)? {
        guard let header = readExactly(5) else { return nil }
        let type = header[header.startIndex]
        let len = header.dropFirst().reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        guard len <= 16 * 1024 * 1024 else { return nil }
        guard let payload = readExactly(Int(len)) else { return nil }
        return (type, payload)
    }
}

/// Lock-light tee registry read from the libghostty IO thread for every
/// surface (installed in `cmuxTerminalOutputTeeCallback`).
enum ShellXBridgeTee {
    private static let registry = OSAllocatedUnfairLock<[UUID: [ShellXBridgeConnection]]>(initialState: [:])

    static func consume(surfaceID: UUID, buffer: UnsafeBufferPointer<UInt8>) {
        let connections = registry.withLockUnchecked { $0[surfaceID] }
        guard let connections, !connections.isEmpty,
              let base = buffer.baseAddress, buffer.count > 0 else { return }
        let data = Data(bytes: base, count: buffer.count)
        for connection in connections { connection.teeOutput(data) }
    }

    static func register(_ connection: ShellXBridgeConnection, surfaceID: UUID) {
        registry.withLockUnchecked { $0[surfaceID, default: []].append(connection) }
    }

    static func unregister(_ connection: ShellXBridgeConnection) {
        registry.withLockUnchecked { map in
            for key in Array(map.keys) {
                map[key]?.removeAll { $0 === connection }
                if map[key]?.isEmpty == true { map[key] = nil }
            }
        }
    }
}

@MainActor
final class ShellXBridgeSpike {
    static let shared = ShellXBridgeSpike()

    private struct Binding {
        let connection: ShellXBridgeConnection
        let surfaceID: UUID
        let workspaceID: UUID
        let mode: String
        var lastSentSize: (UInt16, UInt16)?
        var viewportGeneration = 0
    }

    private var listenFD: Int32 = -1
    private var socketPath: String?
    /// The first daemon terminal projects the Mac's focused terminal; later
    /// ones are remote-initiated creates that open a new libghostty tab.
    private var projectFocusedPending = true
    private var nextConnectionID = 1
    private var bindings: [Int: Binding] = [:]
    private var sizeTimer: Timer?

    func startIfConfigured() {
        guard listenFD < 0 else { return }
        let env = ProcessInfo.processInfo.environment
        let path: String
        if let explicit = env["CMUX_SHELLX_BRIDGE_SOCKET"], !explicit.isEmpty {
            path = explicit
        } else if let control = env["CMUX_SOCKET_PATH"],
                  control.hasPrefix("/tmp/cmux-debug-"), control.hasSuffix(".sock") {
            path = String(control.dropLast(5)) + "-shellx-bridge.sock"
        } else {
            return
        }
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8)
        guard pathBytes.count < MemoryLayout.size(ofValue: addr.sun_path) else { Darwin.close(fd); return }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: pathBytes)
            raw[pathBytes.count] = 0
        }
        let oldMask = umask(0o077)
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        umask(oldMask)
        guard bound == 0, listen(fd, 8) == 0 else {
            shellXBridgeLog.error("bridge bind/listen failed errno=\(errno)")
            Darwin.close(fd)
            return
        }
        chmod(path, 0o600)
        listenFD = fd
        socketPath = path
        Self.traceLock.lock(); Self.tracePath = path + ".log"; Self.traceLock.unlock()
        ShellXBridgeSpike.trace("shellx-bridge: listening on \(path)")
        let thread = Thread { [fd] in Self.acceptLoop(fd: fd) }
        thread.name = "shellx-bridge-accept"
        thread.start()
        sizeTimer = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { _ in
            MainActor.assumeIsolated { ShellXBridgeSpike.shared.pollSizes() }
        }
    }

    nonisolated private static func acceptLoop(fd: Int32) {
        while true {
            let client = accept(fd, nil, nil)
            if client < 0 {
                if errno == EINTR { continue }
                return
            }
            let id = DispatchQueue.main.sync {
                MainActor.assumeIsolated { ShellXBridgeSpike.shared.allocateConnectionID() }
            }
            let connection = ShellXBridgeConnection(fd: client, id: id)
            let thread = Thread { serve(connection) }
            thread.name = "shellx-bridge-conn-\(id)"
            thread.start()
        }
    }

    private func allocateConnectionID() -> Int {
        defer { nextConnectionID += 1 }
        return nextConnectionID
    }

    nonisolated private static func serve(_ connection: ShellXBridgeConnection) {
        guard let (type, payload) = connection.readFrame(), type == ShellXBridgeFrameType.hello else {
            connection.closeAfterDrain()
            return
        }
        let hello = (try? JSONSerialization.jsonObject(with: payload)) as? [String: Any] ?? [:]
        let bound = DispatchQueue.main.sync {
            MainActor.assumeIsolated { ShellXBridgeSpike.shared.bind(connection, hello: hello) }
        }
        guard bound else {
            connection.send(ShellXBridgeFrameType.exit, Data())
            connection.closeAfterDrain()
            return
        }
        while let (frameType, frame) = connection.readFrame() {
            switch frameType {
            case ShellXBridgeFrameType.input:
                connection.noteInput(frame.count)
                DispatchQueue.main.async {
                    MainActor.assumeIsolated { ShellXBridgeSpike.shared.input(connectionID: connection.id, bytes: frame) }
                }
            case ShellXBridgeFrameType.resize where frame.count == 4:
                let cols = UInt16(frame[frame.startIndex]) << 8 | UInt16(frame[frame.startIndex + 1])
                let rows = UInt16(frame[frame.startIndex + 2]) << 8 | UInt16(frame[frame.startIndex + 3])
                DispatchQueue.main.async {
                    MainActor.assumeIsolated {
                        ShellXBridgeSpike.shared.remoteResize(connectionID: connection.id, cols: cols, rows: rows)
                    }
                }
            case ShellXBridgeFrameType.kill:
                break
            default:
                continue
            }
            if frameType == ShellXBridgeFrameType.kill { break }
        }
        DispatchQueue.main.async {
            MainActor.assumeIsolated { ShellXBridgeSpike.shared.unbind(connectionID: connection.id) }
        }
    }

    // MARK: Main-actor bridge operations

    private func bind(_ connection: ShellXBridgeConnection, hello: [String: Any]) -> Bool {
        let controller = TerminalController.shared
        var mode = "attach"
        var target: ControlTerminalSocketTarget?
        var workspaceID: UUID?
        if projectFocusedPending,
           let resolved = controller.mobileCanonicalTerminalTarget(params: [:]) {
            projectFocusedPending = false
            target = resolved.target
            workspaceID = resolved.workspace.id
        } else if let resolved = controller.mobileResolveWorkspaceAndSurface(params: [:], requireTerminal: false) {
            // Remote-initiated create: open a real libghostty tab for it.
            mode = "create"
            let workspace = resolved.workspace
            guard let pane = workspace.bonsplitController.focusedPaneId
                ?? workspace.bonsplitController.allPaneIds.first,
                let panel = workspace.newTerminalSurface(
                    inPane: pane,
                    focus: true,
                    workingDirectory: hello["cwd"] as? String,
                    autoRefreshMetadata: false,
                    preserveFocusWhenUnfocused: false,
                    allowTextBoxFocusDefault: false,
                    inheritStartupWork: false
                ) else { return false }
            workspaceID = workspace.id
            target = controller.mobileCanonicalTerminalTarget(params: [
                "workspace_id": workspace.id.uuidString,
                "surface_id": panel.id.uuidString,
            ])?.target
        }
        guard let target, let workspaceID else { return false }
        let surfaceID = target.surfaceID
        ShellXBridgeSpike.trace("shellx-bridge: conn \(connection.id) bound mode=\(mode) surface=\(surfaceID.uuidString)")
        ShellXBridgeTee.register(connection, surfaceID: surfaceID)
        bindings[connection.id] = Binding(
            connection: connection,
            surfaceID: surfaceID,
            workspaceID: workspaceID,
            mode: mode
        )
        let ack: [String: Any] = [
            "surface_id": surfaceID.uuidString,
            "workspace_id": workspaceID.uuidString,
            "mode": mode,
        ]
        connection.send(ShellXBridgeFrameType.ack, (try? JSONSerialization.data(withJSONObject: ack)) ?? Data())
        sendSizeIfChanged(connectionID: connection.id, force: true)
        // Snapshot of the existing screen so the daemon mirror is not blank on
        // attach. A created tab has no output yet.
        var snapshot: Data?
        if mode == "attach", let vt = target.surface.readBoundedScreenTailVT(maxRows: 200, maxBytes: 1 << 20) {
            // Start the mirror from a clean screen; the tail VT ends at the cursor.
            snapshot = Data(("\u{1b}[H\u{1b}[2J" + vt).utf8)
            dumpSpikeArtifact(name: "snapshot-\(connection.id).vt", data: snapshot!)
        }
        connection.releaseHold(snapshot: snapshot)
        return true
    }

    private func liveTarget(_ binding: Binding) -> ControlTerminalSocketTarget? {
        TerminalController.shared.mobileCanonicalTerminalTarget(params: [
            "workspace_id": binding.workspaceID.uuidString,
            "surface_id": binding.surfaceID.uuidString,
        ])?.target
    }

    private func input(connectionID: Int, bytes: Data) {
        guard let binding = bindings[connectionID], let target = liveTarget(binding),
              let surface = target.surface.liveSurfaceForGhosttyAccess(reason: "shellx.bridge.input") else { return }
        // Raw bytes, already encoded by the remote's key encoder. text_input
        // writes them to the PTY as-is except '\n' -> '\r'.
        bytes.withUnsafeBytes { raw in
            guard let base = raw.baseAddress?.assumingMemoryBound(to: CChar.self) else { return }
            ghostty_surface_text_input(surface, base, UInt(raw.count))
        }
    }

    private func remoteResize(connectionID: Int, cols: UInt16, rows: UInt16) {
        guard var binding = bindings[connectionID] else { return }
        binding.viewportGeneration += 1
        bindings[connectionID] = binding
        // The remote viewer joins the Mac's shared sizing as one participant;
        // the Mac (PTY host) decides, and pollSizes reports the result back.
        let result = TerminalController.shared.v2MobileTerminalViewport(params: [
            "workspace_id": binding.workspaceID.uuidString,
            "surface_id": binding.surfaceID.uuidString,
            "client_id": "shellx-bridge-\(connectionID)",
            "viewport_columns": Int(cols),
            "viewport_rows": Int(rows),
            "viewport_generation": binding.viewportGeneration,
            "device_kind": "tui",
            "device_name": "cmux-tui via ShellX bridge",
        ])
        switch result {
        case .ok(let payload):
            ShellXBridgeSpike.trace("shellx-bridge: conn \(connectionID) remote resize \(cols)x\(rows) -> \(String(describing: payload))")
        case .err(let code, let message, _):
            ShellXBridgeSpike.trace("shellx-bridge: conn \(connectionID) remote resize \(cols)x\(rows) failed \(code) \(message)")
        }
        // Always echo the effective grid: the Mac owns the PTY, so a Mux wish
        // it could not honor (Mac pane smaller) must be corrected on the mirror.
        sendSizeIfChanged(connectionID: connectionID, force: true)
    }

    private func pollSizes() {
        for id in Array(bindings.keys) {
            guard let binding = bindings[id] else { continue }
            if liveTarget(binding) == nil {
                // The Mac tab closed: end the daemon terminal too.
                ShellXBridgeSpike.trace("shellx-bridge: conn \(id) surface gone, sending exit")
                binding.connection.send(ShellXBridgeFrameType.exit, Data())
                unbind(connectionID: id)
                continue
            }
            sendSizeIfChanged(connectionID: id, force: false)
        }
    }

    private func sendSizeIfChanged(connectionID: Int, force: Bool) {
        guard var binding = bindings[connectionID], let target = liveTarget(binding),
              let surface = target.surface.liveSurfaceForGhosttyAccess(reason: "shellx.bridge.size") else { return }
        let size = ghostty_surface_size(surface)
        let current = (size.columns, size.rows)
        if !force, let last = binding.lastSentSize, last == current { return }
        binding.lastSentSize = current
        bindings[connectionID] = binding
        ShellXBridgeSpike.trace("shellx-bridge: conn \(connectionID) mac grid \(size.columns)x\(size.rows)")
        binding.connection.send(
            ShellXBridgeFrameType.size,
            ShellXBridgeConnection.sizePayload(cols: size.columns, rows: size.rows)
        )
    }

    private func unbind(connectionID: Int) {
        guard let binding = bindings.removeValue(forKey: connectionID) else { return }
        ShellXBridgeTee.unregister(binding.connection)
        _ = TerminalController.shared.v2MobileTerminalViewport(params: [
            "workspace_id": binding.workspaceID.uuidString,
            "surface_id": binding.surfaceID.uuidString,
            "client_id": "shellx-bridge-\(connectionID)",
            "clear": true,
            "viewport_generation": binding.viewportGeneration + 1,
        ])
        ShellXBridgeSpike.trace("shellx-bridge: conn \(connectionID) closed (out=\(binding.connection.outputBytes) B, in=\(binding.connection.inputBytes) B)")
        binding.connection.closeAfterDrain()
    }

    nonisolated static let traceLock = NSLock()
    nonisolated(unsafe) static var tracePath: String?

    nonisolated static func trace(_ message: String) {
        NSLog("%@", message)
        traceLock.lock(); defer { traceLock.unlock() }
        guard let tracePath else { return }
        let line = "\(Date().timeIntervalSince1970) \(message)\n"
        if let handle = FileHandle(forWritingAtPath: tracePath) {
            handle.seekToEndOfFile()
            handle.write(Data(line.utf8))
            try? handle.close()
        } else {
            FileManager.default.createFile(atPath: tracePath, contents: Data(line.utf8), attributes: [.posixPermissions: 0o600])
        }
    }

    private func dumpSpikeArtifact(name: String, data: Data) {
        guard let socketPath else { return }
        let url = URL(fileURLWithPath: socketPath + "." + name)
        try? data.write(to: url)
    }
}
#endif
