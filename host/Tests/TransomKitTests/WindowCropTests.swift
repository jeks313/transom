import CoreGraphics
import Testing

@testable import TransomKit

@Suite("Window crop")
struct WindowCropTests {
    @Test("a window inside the frame is its own crop")
    func inside() {
        #expect(
            WindowCrop.clip(WireRect(x: 1720, y: 0, w: 1568, h: 1420), width: 5120, height: 2880)
                == CGRect(x: 1720, y: 0, width: 1568, height: 1420))
    }

    @Test("a window over the frame's edge is cut to it, on even pixels")
    func clipped() {
        #expect(
            WindowCrop.clip(WireRect(x: 5001, y: 2700, w: 400, h: 400), width: 5120, height: 2880)
                == CGRect(x: 5000, y: 2700, width: 120, height: 180))
    }

    @Test("too little on the frame is no crop")
    func tooSmall() {
        #expect(WindowCrop.clip(WireRect(x: 5100, y: 0, w: 400, h: 400), width: 5120, height: 2880) == nil)
        #expect(WindowCrop.clip(WireRect(x: 0, y: 0, w: 66, h: 20), width: 5120, height: 2880) == nil)
    }

    @Test("a window is dirty only where a dirty rect touches it; a refresh touches none")
    func dirty() {
        let crop = CGRect(x: 1720, y: 0, width: 1568, height: 1420)
        #expect(WindowCrop.isDirty(crop, dirty: [CGRect(x: 0, y: 0, width: 1680, height: 1420)]) == false)
        #expect(WindowCrop.isDirty(crop, dirty: [CGRect(x: 1720, y: 0, width: 1568, height: 44)]))
        #expect(WindowCrop.isDirty(crop, dirty: nil) == false)
    }
}
