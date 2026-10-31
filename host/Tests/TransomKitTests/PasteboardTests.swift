import AppKit
import Testing

@testable import TransomKit

@Suite("pasteboard sync")
struct PasteboardTests {
    @Test("sends what the Mac copies, not what the client sent")
    func echo() {
        let board = NSPasteboard(name: NSPasteboard.Name("transom-test-\(UUID().uuidString)"))
        defer { board.releaseGlobally() }
        let sync = PasteboardSync(pasteboard: board)
        #expect(sync.sync { sync.poll() } == nil)

        board.clearContents()
        board.setString("from the Mac", forType: .string)
        #expect(sync.sync { sync.poll() } == "from the Mac")
        #expect(sync.sync { sync.poll() } == nil)

        sync.set("from the client")
        #expect(sync.sync { sync.poll() } == nil)
        #expect(board.string(forType: .string) == "from the client")

        // The same text copied again is not sent again.
        board.clearContents()
        board.setString("from the client", forType: .string)
        #expect(sync.sync { sync.poll() } == nil)
    }

    @Test("skips text that is too large")
    func large() {
        let board = NSPasteboard(name: NSPasteboard.Name("transom-test-\(UUID().uuidString)"))
        defer { board.releaseGlobally() }
        let sync = PasteboardSync(pasteboard: board)
        board.clearContents()
        board.setString(String(repeating: "x", count: PasteboardSync.maxBytes + 1), forType: .string)
        #expect(sync.sync { sync.poll() } == nil)
    }
}
