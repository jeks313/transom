import Foundation

/// The video channel server (issue #3 Phase 3, second connection): streams
/// encoded HEVC frames to at most one connected client, every window's stream on
/// the one connection, each message tagged with its window (protocol.md §6, v4).
///
/// This is the "frames may be dropped, never delayed" channel (protocol.md §1).
/// On connect (and again on reconnect), and whenever a window gets a new encoder,
/// the stream's parameter sets are sent before its next keyframe, because an
/// `hvc1` stream is undecodable without them.
public actor VideoServer {
    /// One encoded frame of a window's stream. `generation` changes when the
    /// window gets a new encoder (a new size), which needs a new `config`.
    /// `width` x `height` is the encoder's picture; the window fills
    /// `contentWidth` x `contentHeight` of it from the top left.
    public struct StreamFrame: Sendable {
        public let stream: UInt64
        public let generation: UInt32
        public let width: UInt32
        public let height: UInt32
        public let contentWidth: UInt32
        public let contentHeight: UInt32
        /// The encoder's parameter sets, once it has made its first frame.
        public let hvcc: Data?
        public let frame: HEVCEncoder.EncodedFrame
        public init(
            stream: UInt64, generation: UInt32, width: UInt32, height: UInt32,
            contentWidth: UInt32? = nil, contentHeight: UInt32? = nil, hvcc: Data?,
            frame: HEVCEncoder.EncodedFrame
        ) {
            self.stream = stream
            self.generation = generation
            self.width = width
            self.height = height
            self.contentWidth = contentWidth ?? width
            self.contentHeight = contentHeight ?? height
            self.hvcc = hvcc
            self.frame = frame
        }
    }

    private var active: (id: UUID, transport: any PacketTransport)?
    private var stopped = false
    var hasClient: Bool { active != nil }
    /// Per-stream state, reset on every connection.
    private struct StreamState {
        var sentGeneration: UInt32?
        var seq: UInt64 = 0
    }
    private var streams: [UInt64: StreamState] = [:]

    /// Called with `true` when a client connects and `false` when it disconnects
    /// or is dropped, so a status UI can show whether the video client is attached.
    /// Fired from the actor; the closure must be thread-safe.
    public var onConnectionChange: (@Sendable (Bool) -> Void)?

    public init() {}

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
        streams = [:]
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

    /// Send one frame of a window's stream to the connected client, if any. The
    /// stream's config goes first whenever its generation is new to this
    /// connection, and then only from a keyframe on. Returns false when the frame
    /// was held back for want of a keyframe, so the caller can ask for one.
    @discardableResult
    public func send(_ item: StreamFrame) async -> Bool {
        guard let active else { return true }
        var state = streams[item.stream] ?? StreamState()
        do {
            if state.sentGeneration != item.generation {
                guard item.frame.isKeyframe, let hvcc = item.hvcc else { return false }
                try await active.transport.send(
                    VideoWire.encodeConfig(
                        stream: item.stream, width: item.width, height: item.height, hvcc: hvcc))
                state.sentGeneration = item.generation
            }
            let frame = item.frame
            let ptsMicros =
                frame.pts.seconds.isFinite ? UInt64(max(0, frame.pts.seconds * 1_000_000)) : 0
            try await active.transport.send(
                VideoWire.encodeFrame(
                    stream: item.stream, seq: state.seq, ptsMicros: ptsMicros,
                    keyframe: frame.isKeyframe,
                    timing: VideoWire.FrameTiming(
                        capturedMicros: frame.capturedMicros, encodedMicros: frame.encodedMicros,
                        sentMicros: HostClock.micros()),
                    width: item.contentWidth, height: item.contentHeight, data: frame.data))
            state.seq += 1
            // A reconnect during the awaits above reset the states; keep the reset.
            if self.active?.id == active.id { streams[item.stream] = state }
        } catch {
            if self.active?.id == active.id {
                self.active = nil
                onConnectionChange?(false)
            }
            await active.transport.close()
        }
        return true
    }

    /// Forget a stream whose window has gone.
    public func end(stream: UInt64) {
        streams[stream] = nil
    }
}
