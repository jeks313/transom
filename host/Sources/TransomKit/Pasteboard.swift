import AppKit

/// Plain-text clipboard sync with the client. Text copied on the Mac goes to
/// the client; text the client copied goes on the Mac's pasteboard. Text this
/// side wrote, or just received, is not sent back.
public final class PasteboardSync: @unchecked Sendable {
    /// Larger text is not synced.
    public static let maxBytes = 1 << 20

    private let pasteboard: NSPasteboard
    private let queue = DispatchQueue(label: "transom.pasteboard")
    private var timer: DispatchSourceTimer?
    private var seen: Int
    private var last: String?

    public init(pasteboard: NSPasteboard = .general) {
        self.pasteboard = pasteboard
        seen = pasteboard.changeCount
    }

    /// Call `onCopy` with text copied on the Mac, checking every 250 ms.
    public func start(onCopy: @escaping @Sendable (String) -> Void) {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(250))
        timer.setEventHandler { [weak self] in
            if let text = self?.poll() { onCopy(text) }
        }
        timer.resume()
        self.timer = timer
    }

    public func stop() {
        timer?.cancel()
        timer = nil
    }

    /// Text copied since the last call, if it is new and not too large.
    func poll() -> String? {
        dispatchPrecondition(condition: .onQueue(queue))
        guard pasteboard.changeCount != seen else { return nil }
        seen = pasteboard.changeCount
        guard let text = pasteboard.string(forType: .string),
            text.utf8.count <= Self.maxBytes, text != last
        else { return nil }
        last = text
        return text
    }

    /// Put text the client copied on the pasteboard.
    public func set(_ text: String) {
        queue.async { [self] in
            guard text != last, text.utf8.count <= Self.maxBytes else { return }
            pasteboard.clearContents()
            pasteboard.setString(text, forType: .string)
            seen = pasteboard.changeCount
            last = text
        }
    }

    /// Run `body` on the sync's queue (tests).
    func sync<T>(_ body: () -> T) -> T { queue.sync(execute: body) }
}
