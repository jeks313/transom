import CoreGraphics
import Testing

@testable import TransomKit

@Suite("Prompt policy")
struct PromptPolicyTests {
    private func window(_ owner: String, _ number: Int, x: Double, w: Double = 260) -> [String: Any] {
        [
            kCGWindowOwnerName as String: owner, kCGWindowNumber as String: number,
            kCGWindowOwnerPID as String: Int32(100 + number), kCGWindowAlpha as String: 1.0,
            kCGWindowBounds as String: ["X": x, "Y": 200.0, "Width": w, "Height": 300.0],
        ]
    }

    @Test("only prompt owners off the session's displays are candidates")
    func candidates() {
        let shared = [CGRect(x: 1728, y: 0, width: 2560, height: 1440)]
        let found = PromptPolicy.candidates(
            windows: [
                window("coreautha", 1, x: 700),  // on the Mac's screen: adopt
                window("coreautha", 2, x: 2000),  // already on a shared display
                window("Safari", 3, x: 700),  // not a prompt
                window("PrivilegeManagement", 4, x: 600),
                window("SecurityAgent", 5, x: 600, w: 20),  // too small to be one
            ], shared: shared)
        #expect(found.map(\.windowNumber) == [1, 4])
        #expect(found.first?.bounds == CGRect(x: 700, y: 200, width: 260, height: 300))
    }

    @Test("slots cascade and stay on the display")
    func slots() {
        let display = CGRect(x: 5000, y: 0, width: 1280, height: 800)
        #expect(PromptPolicy.slot(index: 0, size: CGSize(width: 455, height: 255), display: display) == CGPoint(x: 5032, y: 32))
        #expect(PromptPolicy.slot(index: 1, size: CGSize(width: 455, height: 255), display: display) == CGPoint(x: 5064, y: 64))
        // Too big to cascade: pinned inside.
        #expect(PromptPolicy.slot(index: 3, size: CGSize(width: 1280, height: 800), display: display) == CGPoint(x: 5000, y: 0))
    }
}
