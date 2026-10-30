import CoreGraphics
import Testing

@testable import TransomKit

@Suite("Owned display")
struct OwnedDisplayTests {
    @Test("the helper's ready line becomes the display's geometry")
    func parsesReady() throws {
        let info = try #require(
            OwnedDisplay.parseReady("ready id=77 origin=3008,0 pt=2048x1080 px=4096x2160"))
        #expect(info.id == 77)
        #expect(info.originPoints == CGPoint(x: 3008, y: 0))
        #expect(info.pixelWidth == 4096 && info.pixelHeight == 2160)
        #expect(info.scale == 2)
        #expect(!info.isMain)
        #expect(OwnedDisplay.parseReady("error no display") == nil)
        #expect(OwnedDisplay.parseReady("ready id=1 origin=0,0 pt=0x0 px=0x0") == nil)
    }
}
