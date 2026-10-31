import Foundation

/// The video channel server (issue #3 Phase 3, second connection): streams
/// encoded HEVC frames to at most one connected client, every lane's on the one
/// connection, each message tagged with its lane (protocol.md §6).
///
/// This is the "frames may be dropped, never delayed" channel (protocol.md §1).
/// The caller feeds frames through a small most-recent buffer, so if the client
/// or link stalls, old frames are dropped rather than queued. On connect (and
/// again on reconnect) each lane's parameter sets are sent before its first
/// frame, because an `hvc1` stream is undecodable without them.
public actor VideoServer {
    private var active: (id: UUID, transport: any PacketTransport)?
    private var stopped = false
    var hasClient: Bool { active != nil }
    /// Per-lane stream state, reset on every connection.
    private struct LaneState {
        var sentConfig = false
        var waitingForKeyframe = true
        var seq: UInt64 = 0
    }
    private var laneStates: [UInt32: LaneState] = [:]
    private let hvccProviders: [UInt32: @Sendable () -> Data?]

    /// Called with `true` when a client connects and `false` when it disconnects
    /// or is dropped, so a status UI can show whether the video client is attached.
    /// Fired from the actor; the closure must be thread-safe.
    public var onConnectionChange: (@Sendable (Bool) -> Void)?

    /// - Parameter hvccProviders: per lane, returns that encoder's `hvcC`
    ///   parameter sets once its first frame has been encoded (nil before then).
    public init(hvccProviders: [UInt32: @Sendable () -> Data?]) {
        self.hvccProviders = hvccProviders
    }

    /// A one-lane server (lane 0).
    public init(hvccProvider: @escaping @Sendable () -> Data?) {
        self.init(hvccProviders: [0: hvccProvider])
    }

    public func setOnConnectionChange(_ handler: @escaping @Sendable (Bool) -> Void) {
        self.onConnectionChange = handler
    }

    public func serve(listener: TCPListener) async {
        for await transport in listener.connections {
            await serveConnection(transport)
        }
    }

    func serveConnection(_ transport: any PacketTransport) async {
        guard !stopped, !Task.isCancelled else {
            await transport.close()
            return
        }
        let connectionID = UUID()
        active = (connectionID, transport)
        laneStates = [:]
        Log.encode.notice("video: client connected")
        onConnectionChange?(true)
        // The client sends nothing on this channel; the receive loop just detects
        // disconnect so the host can stop targeting a dead socket.
        do {
            while !stopped, try await transport.receiveFrame() != nil {}
        } catch {
            // fall through to cleanup
        }
        Log.encode.notice("video: client disconnected")
        if active?.id == connectionID {
            active = nil
            onConnectionChange?(false)
        }
        await transport.close()
    }

    /// Stop the accepted connection as well as any buffered listener arrivals.
    public func stop() async {
        stopped = true
        guard let active else { return }
        self.active = nil
        onConnectionChange?(false)
        await active.transport.close()
    }

    /// Send one encoded frame of `lane` to the connected client, if any. The
    /// lane's config is sent lazily before its first frame of a connection.
    public func send(_ frame: HEVCEncoder.EncodedFrame, lane: UInt32 = 0) async {
        guard let active, let hvccProvider = hvccProviders[lane] else { return }
        var state = laneStates[lane] ?? LaneState()
        guard !state.waitingForKeyframe || frame.isKeyframe else { return }
        do {
            if !state.sentConfig {
                guard let hvcc = hvccProvider() else { return }
                try await active.transport.send(VideoWire.encodeConfig(lane: lane, hvcc: hvcc))
                state.sentConfig = true
            }
            state.waitingForKeyframe = false
            let ptsMicros =
                frame.pts.seconds.isFinite ? UInt64(max(0, frame.pts.seconds * 1_000_000)) : 0
            try await active.transport.send(
                VideoWire.encodeFrame(
                    lane: lane, seq: state.seq, ptsMicros: ptsMicros, keyframe: frame.isKeyframe,
                    timing: VideoWire.FrameTiming(
                        capturedMicros: frame.capturedMicros, encodedMicros: frame.encodedMicros,
                        sentMicros: HostClock.micros()),
                    data: frame.data))
            state.seq += 1
            // A reconnect during the awaits above reset the states; keep the reset.
            if self.active?.id == active.id { laneStates[lane] = state }
        } catch {
            if self.active?.id == active.id {
                self.active = nil
                onConnectionChange?(false)
            }
            await active.transport.close()
        }
    }
}
