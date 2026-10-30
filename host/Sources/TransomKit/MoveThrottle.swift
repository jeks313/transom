/// The pointer-move throttle: decides which of a flood of client `mouseMove`s is
/// posted to the Mac.
///
/// A Windows client sends one `mouseMove` per `WM_MOUSEMOVE`, which with a
/// 1000Hz mouse is one per millisecond. A real Mac mouse reaches an app coalesced
/// to about the display rate; posted events are not coalesced. Measured with
/// Chrome on the host (2026-10-30): at 1000 a second its event queue filled, the
/// post blocked for 4-7s at a time, and every click behind it waited, then landed
/// in a burst. Safari kept up. This limits moves (and so drags) to about
/// `1/interval` Hz.
///
/// Like `ResizeThrottle`, a **pure value type with time injected**, unit tested
/// in `Tests/TransomKitTests/MoveThrottleTests`; `InputInjector` supplies the
/// clock and does the posting.
///
/// Rules:
/// - **A move is posted at once** if none was posted in the last `interval`
///   (leading edge), so a single small move has no added delay.
/// - **The last move of a burst is never dropped.** It is held and released by
///   `onTick` once its window elapses, so the pointer ends where the client's did.
/// - **Any other event releases a held move first**, via `takePending`, so moves
///   never reorder with presses, releases, scrolls or keys.
public struct MoveThrottle {

    /// A held move: its window id, the event and the client timestamp.
    public struct Move: Equatable, Sendable {
        public let id: UInt64
        public let event: InputEvent
        public let ts: UInt64
    }

    /// Minimum seconds between posted moves. 1/120 matches a fast display.
    public let interval: Double

    private var lastPost: Double?
    private var pending: Move?

    public init(interval: Double = 1.0 / 120) {
        self.interval = interval
    }

    public var hasPending: Bool { pending != nil }

    /// Feed one client move. Returns it if it should be posted now, nil if it
    /// is held (replacing any older held move). `now` is monotonic seconds.
    public mutating func onMove(_ move: Move, now: Double) -> Move? {
        if let last = lastPost, now - last < interval {
            pending = move
            return nil
        }
        lastPost = now
        pending = nil
        return move
    }

    /// Seconds from `now` until a held move may be released, or nil if none is held.
    public func delayUntilDue(now: Double) -> Double? {
        guard pending != nil else { return nil }
        return max(0, (lastPost ?? now) + interval - now)
    }

    /// Release the held move once its window has elapsed.
    public mutating func onTick(now: Double) -> Move? {
        guard let move = pending, let last = lastPost, now - last >= interval else {
            return nil
        }
        lastPost = now
        pending = nil
        return move
    }

    /// Release the held move regardless of time, before a non-move event.
    public mutating func takePending(now: Double) -> Move? {
        guard let move = pending else { return nil }
        lastPost = now
        pending = nil
        return move
    }
}
