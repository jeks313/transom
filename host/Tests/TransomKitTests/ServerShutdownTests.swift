import Foundation
import Testing

@testable import TransomKit

private actor ShutdownTransport: PacketTransport {
    var receiving = false
    var closed = false
    var messages: [Data] = []

    func send(_ payload: Data) { messages.append(payload) }

    func receiveFrame() async throws -> Data? {
        receiving = true
        while !closed {
            try await Task.sleep(for: .milliseconds(5))
        }
        return nil
    }

    func close() { closed = true }

    func waitForReceive() async throws {
        for _ in 0..<200 {
            if receiving { return }
            try await Task.sleep(for: .milliseconds(5))
        }
        try #require(receiving, "Server did not start receiving")
    }

    func waitForClose() async throws {
        for _ in 0..<200 {
            if closed { return }
            try await Task.sleep(for: .milliseconds(5))
        }
        try #require(closed, "Stopped server did not close a queued connection")
    }
}

private final class ConnectionChanges: @unchecked Sendable {
    private let lock = NSLock()
    private var changes: [Bool] = []

    func record(_ connected: Bool) { lock.withLock { changes.append(connected) } }
    var values: [Bool] { lock.withLock { changes } }
}

@Suite("Sharing shutdown")
struct ServerShutdownTests {
    @Test("control stop closes the client and rejects queued connections")
    func controlStop() async throws {
        let server = ControlServer(vdsSize: WireSize(w: 1920, h: 1080), registry: WindowRegistry())
        let changes = ConnectionChanges()
        await server.setOnConnectionChange { changes.record($0) }
        let transport = ShutdownTransport()
        let connection = Task { await server.serveConnection(transport) }
        defer { connection.cancel() }
        try await transport.waitForReceive()
        // The initial hello and empty tile layout were sent successfully.
        #expect(await transport.messages.count == 2)
        await server.stop()
        try #require(await transport.closed)
        await connection.value
        await server.stop()

        let queued = ShutdownTransport()
        let next = Task { await server.serveConnection(queued) }
        defer { next.cancel() }
        try await queued.waitForClose()
        await next.value
        #expect(await queued.messages.isEmpty)
        #expect(changes.values == [true, false])
    }

    @Test("resync describes every lane, each window under its own lane")
    func multiLaneResync() async throws {
        let ids = WindowIDSource()
        let first = WindowRegistry(ids: ids), second = WindowRegistry(ids: ids)
        first.record(id: 1, rect: WireRect(x: 0, y: 0, w: 10, h: 10), title: "a")
        second.record(id: 2, rect: WireRect(x: 5, y: 5, w: 20, h: 20), title: "b")
        let server = ControlServer(lanes: [
            .init(id: 0, vdsSize: WireSize(w: 100, h: 100), registry: first),
            .init(id: 1, vdsSize: WireSize(w: 200, h: 100), registry: second),
        ])
        let transport = ShutdownTransport()
        let connection = Task { await server.serveConnection(transport) }
        defer { connection.cancel() }
        try await transport.waitForReceive()
        let messages = try await transport.messages.map(WireCodec.decodeControl)
        #expect(
            messages == [
                .hello(
                    protocolVersion: transomProtocolVersion,
                    lanes: [
                        WireLane(lane: 0, vdsSize: WireSize(w: 100, h: 100)),
                        WireLane(lane: 1, vdsSize: WireSize(w: 200, h: 100)),
                    ]),
                .windowCreated(
                    id: 1, lane: 0, app: 0, rect: WireRect(x: 0, y: 0, w: 10, h: 10), title: "a",
                    kind: .normal),
                .tileLayout(
                    lane: 0, windows: [WireWindow(id: 1, rect: WireRect(x: 0, y: 0, w: 10, h: 10))],
                    displaySize: WireSize(w: 100, h: 100)),
                .windowCreated(
                    id: 2, lane: 1, app: 0, rect: WireRect(x: 5, y: 5, w: 20, h: 20), title: "b",
                    kind: .normal),
                .tileLayout(
                    lane: 1, windows: [WireWindow(id: 2, rect: WireRect(x: 5, y: 5, w: 20, h: 20))],
                    displaySize: WireSize(w: 200, h: 100)),
            ])
        await server.stop()
        await connection.value
    }

    @Test("video stop closes the client and rejects queued connections")
    func videoStop() async throws {
        let server = VideoServer()
        let changes = ConnectionChanges()
        await server.setOnConnectionChange { changes.record($0) }
        let transport = ShutdownTransport()
        let connection = Task { await server.serveConnection(transport) }
        defer { connection.cancel() }
        try await transport.waitForReceive()
        await server.stop()
        try #require(await transport.closed)
        await connection.value
        await server.stop()

        let queued = ShutdownTransport()
        let next = Task { await server.serveConnection(queued) }
        defer { next.cancel() }
        try await queued.waitForClose()
        await next.value
        #expect(changes.values == [true, false])
    }
}
