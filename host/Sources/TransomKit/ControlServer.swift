import ApplicationServices
import Foundation

/// The control channel server (issue #3 Phase 3): one TCP client at a time,
/// receives lifecycle + geometry events and pushes them as protocol messages;
/// reads client requests back.
///
/// Reconnect is a first-class requirement: if the client drops, the host keeps
/// running (capture and AX never stop), and when the client reconnects it gets a
/// **full resync** — `hello`, a `windowCreated` for every live window, and a
/// `tileLayout` per lane — from the registries, then resumes the live stream. No
/// restart.
///
/// An actor so `broadcast` (driven from the event forwarding task) and the
/// per-connection send/receive interleave safely without a lock.
public actor ControlServer {
    /// One per lane: its id, display size and the registry of its windows.
    public struct Lane: Sendable {
        public let id: UInt32
        public let vdsSize: WireSize
        public let registry: WindowRegistry
        /// What every window of the lane is: `.sheet` for the prompt lane.
        public let kind: WindowKind
        public init(id: UInt32, vdsSize: WireSize, registry: WindowRegistry, kind: WindowKind = .normal) {
            self.id = id
            self.vdsSize = vdsSize
            self.registry = registry
            self.kind = kind
        }
    }

    private let lanes: [Lane]
    /// The newest menu bar of each app, replayed after every resync.
    private var menuBars: [UInt32: [MenuItem]] = [:]
    private var active: (id: UUID, transport: any PacketTransport)?
    private var stopped = false

    /// Called for every decoded client→host message (e.g. `requestResize`,
    /// `input`). Phase 5 wires this to AX + `CGEventPost` via `InputInjector`.
    public var onClientMessage: (@Sendable (ClientMessage) -> Void)?

    /// Called with `true` when a client connects and `false` when it disconnects
    /// or is dropped, so a status UI can show whether a client is attached and the
    /// input layer can reset held modifiers between sessions (issue #7). Fired
    /// from the actor; the closure must be thread-safe.
    public var onConnectionChange: (@Sendable (Bool) -> Void)?

    public init(lanes: [Lane]) {
        self.lanes = lanes
    }

    /// A one-lane server, as a session sharing a single display uses.
    public init(vdsSize: WireSize, registry: WindowRegistry) {
        self.init(lanes: [Lane(id: 0, vdsSize: vdsSize, registry: registry)])
    }

    public func setOnClientMessage(_ handler: @escaping @Sendable (ClientMessage) -> Void) {
        self.onClientMessage = handler
    }

    public func setOnConnectionChange(_ handler: @escaping @Sendable (Bool) -> Void) {
        self.onConnectionChange = handler
    }

    /// Accept connections forever (one active at a time). Each connection runs until
    /// that client disconnects, then the next connection is served.
    public func serve(listener: TCPListener) async {
        for await transport in listener.connections {
            await serveConnection(transport)
        }
    }

    /// Cancelling the listener does not close its accepted connections. Close the
    /// transport explicitly so an outstanding receive wakes up when sharing ends.
    public func stop() async {
        stopped = true
        guard let active else { return }
        self.active = nil
        onConnectionChange?(false)
        await active.transport.close()
    }

    /// Push one observed window event to the connected client, if any. On send
    /// failure the connection is dropped; the next reconnect resyncs from the
    /// registry, so nothing is left half-described.
    public func broadcast(_ event: WindowWatcher.WindowEvent, lane: UInt32 = 0) async {
        guard let active else { return }
        var app: UInt32 = 0
        if case .created(let id, _, _) = event, let registry = registry(of: id) {
            app = Self.app(of: id, in: registry)
        }
        do {
            let kind = lanes.first { $0.id == lane }?.kind ?? .normal
            try await active.transport.send(
                try WireCodec.encode(Self.message(for: event, lane: lane, app: app, kind: kind)))
        } catch {
            Log.general.notice(
                "control: send failed, dropping client: \(error.localizedDescription, privacy: .public)"
            )
            if self.active?.id == active.id {
                self.active = nil
                onConnectionChange?(false)
            }
            await active.transport.close()
        }
    }

    /// Record `app`'s menu bar and send it if it changed.
    public func setMenuBar(app: UInt32, menus: [MenuItem]) async {
        guard menuBars[app] != menus else { return }
        menuBars[app] = menus
        await send(.menuBar(app: app, menus: menus))
    }

    /// The opaque app token of a window: its owner's process id.
    static func app(of id: UInt64, in registry: WindowRegistry) -> UInt32 {
        guard let element = registry.element(for: id) else { return 0 }
        var pid: pid_t = 0
        AXUIElementGetPid(element, &pid)
        return UInt32(bitPattern: pid)
    }

    /// The registry that knows window `id`, among every lane's.
    private func registry(of id: UInt64) -> WindowRegistry? {
        lanes.first { $0.registry.element(for: id) != nil }?.registry
    }

    private func send(_ message: ControlMessage) async {
        guard let active else { return }
        do {
            try await active.transport.send(try WireCodec.encode(message))
        } catch {
            if self.active?.id == active.id {
                self.active = nil
                onConnectionChange?(false)
            }
            await active.transport.close()
        }
    }

    func serveConnection(_ transport: any PacketTransport) async {
        guard !stopped, !Task.isCancelled else {
            await transport.close()
            return
        }
        let connectionID = UUID()
        active = (connectionID, transport)
        Log.general.notice("control: client connected")
        onConnectionChange?(true)

        do {
            try await sendResync(to: transport)
        } catch {
            Log.general.notice(
                "control: resync failed: \(error.localizedDescription, privacy: .public)")
            if active?.id == connectionID {
                active = nil
                onConnectionChange?(false)
            }
            await transport.close()
            return
        }

        // Read client→host messages until the peer closes.
        do {
            while !stopped, let frame = try await transport.receiveFrame() {
                guard !stopped, active?.id == connectionID else { break }
                guard let message = try? WireCodec.decodeClient(frame) else {
                    Log.general.notice("control: undecodable client frame ignored")
                    continue
                }
                if case .ping(let clientMicros) = message {
                    await send(.pong(clientMicros: clientMicros, hostMicros: HostClock.micros()))
                    continue
                }
                onClientMessage?(message)
            }
        } catch {
            Log.general.notice(
                "control: receive ended: \(error.localizedDescription, privacy: .public)")
        }

        Log.general.notice("control: client disconnected")
        if active?.id == connectionID {
            active = nil
            onConnectionChange?(false)
        }
        await transport.close()
    }

    /// hello + a windowCreated per live window + each lane's tile layout.
    private func sendResync(to transport: any PacketTransport) async throws {
        try await transport.send(
            try WireCodec.encode(
                .hello(
                    protocolVersion: transomProtocolVersion,
                    lanes: lanes.map { WireLane(lane: $0.id, vdsSize: $0.vdsSize) })))
        for lane in lanes {
            let entries = lane.registry.snapshot()
            for entry in entries {
                try await transport.send(
                    try WireCodec.encode(
                        .windowCreated(
                            id: entry.id, lane: lane.id, app: Self.app(of: entry.id, in: lane.registry),
                            rect: entry.rect, title: entry.title, kind: lane.kind)))
            }
            let windows = entries.map { WireWindow(id: $0.id, rect: $0.rect) }
            try await transport.send(
                try WireCodec.encode(
                    .tileLayout(lane: lane.id, windows: windows, displaySize: lane.vdsSize)))
        }
        for (app, menus) in menuBars.sorted(by: { $0.key < $1.key }) {
            try await transport.send(try WireCodec.encode(.menuBar(app: app, menus: menus)))
        }
    }

    private static func message(
        for event: WindowWatcher.WindowEvent, lane: UInt32, app: UInt32, kind: WindowKind
    ) -> ControlMessage
    {
        switch event {
        case .sharingFailed(let message):
            return .error(code: 2, message: message)
        case .created(let id, let rect, let title):
            return .windowCreated(
                id: id, lane: lane, app: app, rect: rect, title: title, kind: kind)
        case .moved(let id, let rect):
            return .windowMoved(id: id, rect: rect)
        case .destroyed(let id):
            return .windowDestroyed(id: id)
        case .titleChanged(let id, let title):
            return .windowTitle(id: id, title: title)
        case .focused(let id):
            return .windowFocused(id: id)
        }
    }
}
