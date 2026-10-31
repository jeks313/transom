import Foundation
import Testing

@testable import TransomKit

/// The wire protocol is a cross-language contract, so its JSON shape and framing
/// are pure and tested here. If one of these fails, the Rust client silently
/// stops understanding the host.
@Suite("WireProtocol")
struct WireProtocolTests {

    // MARK: round-trip

    private func roundTrip(_ message: ControlMessage) throws -> ControlMessage {
        try WireCodec.decodeControl(try WireCodec.encode(message))
    }
    private func roundTrip(_ message: ClientMessage) throws -> ClientMessage {
        try WireCodec.decodeClient(try WireCodec.encode(message))
    }

    @Test("every host->client message round-trips")
    func controlRoundTrips() throws {
        let messages: [ControlMessage] = [
            .hello(
                protocolVersion: 2,
                lanes: [
                    WireLane(lane: 0, vdsSize: WireSize(w: 3840, h: 2160)),
                    WireLane(lane: 1, vdsSize: WireSize(w: 4096, h: 2160)),
                ]),
            .windowCreated(
                id: 1, lane: 1, app: 4242, rect: WireRect(x: 0, y: 60, w: 1312, h: 844), title: "Xcode", kind: .normal),
            .windowMoved(id: 1, rect: WireRect(x: 1512, y: 60, w: 1312, h: 844)),
            .windowDestroyed(id: 7),
            .windowTitle(id: 1, title: "main.swift — edited"),
            .windowFocused(id: 1),
            .tileLayout(
                lane: 1,
                windows: [
                    WireWindow(id: 1, rect: WireRect(x: 0, y: 60, w: 100, h: 100)),
                    WireWindow(id: 2, rect: WireRect(x: 300, y: 60, w: 100, h: 100)),
                ], displaySize: WireSize(w: 3840, h: 2160)),
            .error(code: 42, message: "does not fit"),
            .menuBar(
                app: 4242,
                menus: [
                    MenuItem(
                        title: "File",
                        items: [
                            MenuItem(title: "New Window", shortcut: "Ctrl+N"),
                            .separatorItem,
                            MenuItem(title: "Close", enabled: false, checked: true),
                        ])
                ]),
            .pong(clientMicros: 123_456_789, hostMicros: 987_654_321),
        ]
        for m in messages {
            #expect(try roundTrip(m) == m)
        }
    }

    @Test("every client->host message round-trips")
    func clientRoundTrips() throws {
        let messages: [ClientMessage] = [
            .requestResize(id: 1, size: WireSize(w: 2560, h: 1440), phase: .live),
            .requestResize(id: 1, size: WireSize(w: 2560, h: 1440), phase: .end),
            .requestFocus(id: 3),
            .requestClose(id: 3),
            .requestKeyframe(lane: nil),
            .requestKeyframe(lane: 2),
            .input(id: 7, event: .mouseDown(x: 12, y: 34, button: .left), ts: 1000),
            .input(id: 7, event: .mouseUp(x: 12, y: 34, button: .right), ts: 1001),
            .input(id: 7, event: .mouseMove(x: 640, y: 480), ts: 1002),
            .input(id: 7, event: .scroll(x: 10, y: 20, dx: -3, dy: 5), ts: 1003),
            .input(id: 7, event: .keyDown(vk: 0x41), ts: 1004),
            .input(id: 7, event: .keyUp(vk: 0x41), ts: 1005),
            .menuPick(id: 7, path: [2, 0], title: "New Window"),
            .menuRefresh(id: 7),
            .ping(clientMicros: 123_456_789),
        ]
        for m in messages {
            #expect(try roundTrip(m) == m)
        }
    }

    @Test("every InputEvent kind round-trips")
    func inputEventRoundTrips() throws {
        let events: [InputEvent] = [
            .mouseDown(x: 1, y: 2, button: .middle),
            .mouseUp(x: 3, y: 4, button: .left),
            .mouseMove(x: 5, y: 6),
            .scroll(x: 7, y: 8, dx: 9, dy: -10),
            .keyDown(vk: 0x1B),
            .keyUp(vk: 0x1B),
        ]
        let enc = JSONEncoder()
        let dec = JSONDecoder()
        for e in events {
            #expect(try dec.decode(InputEvent.self, from: enc.encode(e)) == e)
        }
    }

    @Test("input serialises to the documented nested shape")
    func inputShape() throws {
        let data = try WireCodec.encode(
            .input(id: 7, event: .mouseDown(x: 100, y: 200, button: .left), ts: 4242))
        let obj = try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        #expect(obj["type"] as? String == "input")
        #expect(obj["id"] as? UInt64 == 7)
        #expect(obj["ts"] as? UInt64 == 4242)
        let event = try #require(obj["event"] as? [String: Any])
        #expect(event["kind"] as? String == "mouseDown")
        #expect(event["x"] as? Int == 100)
        #expect(event["y"] as? Int == 200)
        #expect(event["button"] as? String == "left")
    }

    // MARK: JSON shape (the actual contract the Rust side sees)

    @Test("windowMoved serialises to the documented flat shape")
    func windowMovedShape() throws {
        let data = try WireCodec.encode(
            .windowMoved(id: 5, rect: WireRect(x: 10, y: 20, w: 30, h: 40)))
        let obj = try #require(
            try JSONSerialization.jsonObject(with: data) as? [String: Any])
        #expect(obj["type"] as? String == "windowMoved")
        #expect(obj["id"] as? UInt64 == 5)
        let rect = try #require(obj["rect"] as? [String: Any])
        #expect(rect["x"] as? Int == 10)
        #expect(rect["w"] as? Int == 30)
    }

    @Test("hello and windowCreated carry lanes in the documented shape")
    func laneShapes() throws {
        let hello = try #require(
            try JSONSerialization.jsonObject(
                with: WireCodec.encode(
                    .hello(protocolVersion: 2, lanes: [WireLane(lane: 3, vdsSize: WireSize(w: 8, h: 6))])))
                as? [String: Any])
        #expect(hello["protocol"] as? Int == 2)
        let lanes = try #require(hello["lanes"] as? [[String: Any]])
        #expect(lanes.first?["lane"] as? Int == 3)
        #expect((lanes.first?["vdsSize"] as? [String: Any])?["w"] as? Int == 8)
        let created = try #require(
            try JSONSerialization.jsonObject(
                with: WireCodec.encode(
                    .windowCreated(
                        id: 4, lane: 3, app: 9, rect: WireRect(x: 0, y: 0, w: 1, h: 1), title: "t",
                        kind: .normal))) as? [String: Any])
        #expect(created["lane"] as? Int == 3)
        #expect(created["app"] as? Int == 9)
    }

    @Test("a requestKeyframe with no lane means every lane")
    func keyframeWithoutLane() throws {
        let message = try WireCodec.decodeClient(Data(#"{"type":"requestKeyframe"}"#.utf8))
        #expect(message == .requestKeyframe(lane: nil))
    }

    @Test("an unknown type is a decode error, not a crash")
    func unknownType() {
        let data = Data(#"{"type":"nonsense"}"#.utf8)
        #expect(throws: (any Error).self) { try WireCodec.decodeControl(data) }
    }

    // MARK: framing

    @Test("frame then FrameBuffer recovers the same payload")
    func frameRoundTrips() throws {
        let payload = Data("hello world".utf8)
        var buffer = FrameBuffer()
        buffer.append(WireCodec.frame(payload))
        #expect(try buffer.next() == payload)
        #expect(try buffer.next() == nil)
    }

    @Test("FrameBuffer reassembles across arbitrary chunk boundaries")
    func framesAcrossChunks() throws {
        let a = try WireCodec.framed(.windowFocused(id: 1))
        let b = try WireCodec.framed(.windowDestroyed(id: 2))
        let stream = a + b

        var buffer = FrameBuffer()
        var decoded: [ControlMessage] = []
        // Feed the concatenated stream one byte at a time — the worst case.
        for byte in stream {
            buffer.append(Data([byte]))
            while let payload = try buffer.next() {
                decoded.append(try WireCodec.decodeControl(payload))
            }
        }
        #expect(decoded == [.windowFocused(id: 1), .windowDestroyed(id: 2)])
    }

    @Test("multiple frames in one chunk all come out")
    func multipleFramesOneChunk() throws {
        let a = try WireCodec.framed(.windowFocused(id: 9))
        let b = try WireCodec.framed(.windowMoved(id: 9, rect: WireRect(x: 1, y: 2, w: 3, h: 4)))
        var buffer = FrameBuffer()
        buffer.append(a + b)
        // Pop into locals first: `next()` is mutating and cannot be called inside
        // the #require/#expect macro expansion.
        let firstPayload = try #require(try buffer.next())
        let secondPayload = try #require(try buffer.next())
        let thirdPayload = try buffer.next()
        #expect(try WireCodec.decodeControl(firstPayload) == .windowFocused(id: 9))
        #expect(
            try WireCodec.decodeControl(secondPayload)
                == .windowMoved(id: 9, rect: WireRect(x: 1, y: 2, w: 3, h: 4)))
        #expect(thirdPayload == nil)
    }

    @Test("an over-large frame length is rejected, not buffered")
    func rejectsHugeFrame() {
        var buffer = FrameBuffer(maxFrameLength: 16)
        // Claim a 1 MB frame with only the header present.
        buffer.append(Data([0x00, 0x0F, 0x42, 0x40]))
        #expect(throws: FrameBuffer.FrameError.self) { try buffer.next() }
    }

    // MARK: video wire

    @Test("video config round-trips")
    func videoConfigRoundTrips() {
        let hvcc = Data([1, 2, 3, 4, 5])
        #expect(
            VideoWire.decode(VideoWire.encodeConfig(lane: 7, hvcc: hvcc))
                == .config(lane: 7, hvcc: hvcc))
    }

    @Test("video frame round-trips with its header")
    func videoFrameRoundTrips() {
        let payload = Data([0xAB, 0xCD, 0xEF])
        let timing = VideoWire.FrameTiming(
            capturedMicros: 9_876_600, encodedMicros: 9_880_000, sentMicros: 9_880_100)
        let encoded = VideoWire.encodeFrame(
            lane: 0x0102_0304, seq: 12_345, ptsMicros: 9_876_543, keyframe: true, timing: timing,
            data: payload)
        #expect(Array(encoded.prefix(5)) == [0x02, 0x01, 0x02, 0x03, 0x04])
        #expect(encoded.count == 46 + payload.count)
        #expect(
            VideoWire.decode(encoded)
                == .frame(
                    lane: 0x0102_0304, seq: 12_345, ptsMicros: 9_876_543, keyframe: true,
                    timing: timing, data: payload))
    }
}
