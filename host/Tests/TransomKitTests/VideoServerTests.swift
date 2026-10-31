import CoreMedia
import Foundation
import Testing

@testable import TransomKit

private actor RecordingVideoTransport: PacketTransport {
    var messages: [Data] = []
    var receiving = false
    private var closed = false

    func send(_ payload: Data) { messages.append(payload) }

    func receiveFrame() async throws -> Data? {
        receiving = true
        while !closed {
            try await Task.sleep(for: .milliseconds(5))
        }
        return nil
    }

    func close() { closed = true }
}

@Suite("Video startup")
struct VideoServerTests {
    private func waitForConnection(_ transport: RecordingVideoTransport) async throws {
        for _ in 0..<200 {
            if await transport.receiving { return }
            try await Task.sleep(for: .milliseconds(5))
        }
        Issue.record("Video connection did not start")
    }

    private func item(_ stream: UInt64, keyframe: Bool, generation: UInt32 = 1, hvcc: Data? = Data([9]))
        -> VideoServer.StreamFrame
    {
        VideoServer.StreamFrame(
            stream: stream, generation: generation, width: 640, height: 480, hvcc: hvcc,
            frame: HEVCEncoder.EncodedFrame(
                byteCount: 2, pts: .zero, isKeyframe: keyframe, data: Data([0x26, 1])))
    }

    private func shape(_ messages: [Data]) -> [String] {
        messages.compactMap(VideoWire.decode).map { message -> String in
            switch message {
            case .config(let stream, let w, let h, _): return "config\(stream) \(w)x\(h)"
            case .frame(let stream, let seq, _, let key, _, _):
                return "\(key ? "key" : "delta")\(stream)#\(seq)"
            }
        }
    }

    @Test("each connection starts a stream with config and a keyframe, never a delta")
    func keyframeOnEveryConnection() async throws {
        let server = VideoServer()
        for _ in 0..<2 {
            let transport = RecordingVideoTransport()
            let connection = Task { await server.serveConnection(transport) }
            defer { connection.cancel() }
            try await waitForConnection(transport)
            #expect(await server.send(item(3, keyframe: false)) == false)
            #expect(await transport.messages.isEmpty)
            #expect(await server.send(item(3, keyframe: true)))
            await server.send(item(3, keyframe: false))
            #expect(shape(await transport.messages) == ["config3 640x480", "key3#0", "delta3#1"])
            await transport.close()
            await connection.value
        }
    }

    @Test("each window's stream has its own config and keyframe gate")
    func streamsAreIndependent() async throws {
        let server = VideoServer()
        let transport = RecordingVideoTransport()
        let connection = Task { await server.serveConnection(transport) }
        defer { connection.cancel() }
        try await waitForConnection(transport)
        await server.send(item(1, keyframe: true))
        await server.send(item(2, keyframe: false))
        await server.send(item(2, keyframe: true))
        await server.send(item(1, keyframe: false))
        #expect(
            shape(await transport.messages) == [
                "config1 640x480", "key1#0", "config2 640x480", "key2#0", "delta1#1",
            ])
        await transport.close()
        await connection.value
    }

    @Test("a new encoder (generation) sends its config again before its keyframe")
    func newGenerationResendsConfig() async throws {
        let server = VideoServer()
        let transport = RecordingVideoTransport()
        let connection = Task { await server.serveConnection(transport) }
        defer { connection.cancel() }
        try await waitForConnection(transport)
        await server.send(item(1, keyframe: true))
        #expect(await server.send(item(1, keyframe: false, generation: 2)) == false)
        await server.send(item(1, keyframe: true, generation: 2))
        #expect(
            shape(await transport.messages) == ["config1 640x480", "key1#0", "config1 640x480", "key1#1"])
        await transport.close()
        await connection.value
    }

    @Test("frames cannot precede missing configuration")
    func waitsForConfig() async throws {
        let server = VideoServer()
        let transport = RecordingVideoTransport()
        let connection = Task { await server.serveConnection(transport) }
        defer { connection.cancel() }
        try await waitForConnection(transport)
        #expect(await server.send(item(1, keyframe: true, hvcc: nil)) == false)
        #expect(await transport.messages.isEmpty)
        await transport.close()
        await connection.value
    }
}
