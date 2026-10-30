import Testing

@testable import TransomKit

@Suite struct MoveThrottleTests {
    private func move(_ x: UInt32, ts: UInt64 = 0) -> MoveThrottle.Move {
        .init(id: 1, event: .mouseMove(x: x, y: 0), ts: ts)
    }

    @Test func firstMovePostsAtOnce() {
        var t = MoveThrottle(interval: 0.01)
        #expect(t.onMove(move(1), now: 0) == move(1))
        #expect(!t.hasPending)
    }

    @Test func burstHoldsOnlyTheNewest() {
        var t = MoveThrottle(interval: 0.01)
        _ = t.onMove(move(1), now: 0)
        #expect(t.onMove(move(2), now: 0.001) == nil)
        #expect(t.onMove(move(3), now: 0.002) == nil)
        #expect(t.hasPending)
        #expect(t.onTick(now: 0.005) == nil)
        #expect(t.onTick(now: 0.010) == move(3))
        #expect(!t.hasPending)
    }

    @Test func delayCountsFromTheLastPost() {
        var t = MoveThrottle(interval: 0.01)
        #expect(t.delayUntilDue(now: 0) == nil)
        _ = t.onMove(move(1), now: 0)
        _ = t.onMove(move(2), now: 0.004)
        let delay = t.delayUntilDue(now: 0.004)
        #expect(delay != nil && abs(delay! - 0.006) < 1e-9)
    }

    @Test func otherEventReleasesHeldMoveFirst() {
        var t = MoveThrottle(interval: 0.01)
        _ = t.onMove(move(1), now: 0)
        _ = t.onMove(move(2), now: 0.001)
        #expect(t.takePending(now: 0.002) == move(2))
        #expect(t.takePending(now: 0.003) == nil)
        // The released move counts as a post.
        #expect(t.onMove(move(3), now: 0.004) == nil)
    }

    @Test func movesSpacedPastTheIntervalAllPost() {
        var t = MoveThrottle(interval: 0.01)
        for i in 0..<5 {
            #expect(t.onMove(move(UInt32(i)), now: Double(i) * 0.02) == move(UInt32(i)))
        }
    }
}
