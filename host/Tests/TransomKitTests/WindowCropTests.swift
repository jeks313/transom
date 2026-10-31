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

@Suite("Encoder sizing")
struct EncoderSizingTests {
    let lane = CGSize(width: 5120, height: 2880)

    @Test("a new window's encoder fits it exactly")
    func exact() {
        #expect(
            EncoderSizing.capacity(
                current: nil, window: CGSize(width: 1568, height: 1420), settledFor: 0, lane: lane)
                == CGSize(width: 1568, height: 1420))
    }

    @Test("a window that shrinks, or grows within its encoder, keeps it")
    func keeps() {
        let current = CGSize(width: 1600, height: 1400)
        for window in [CGSize(width: 1500, height: 1300), CGSize(width: 1600, height: 1400)] {
            #expect(
                EncoderSizing.capacity(current: current, window: window, settledFor: 0.2, lane: lane)
                    == nil)
        }
    }

    @Test("one that outgrows its encoder gets room to grow, within the lane")
    func grows() {
        #expect(
            EncoderSizing.capacity(
                current: CGSize(width: 1568, height: 1420), window: CGSize(width: 1570, height: 1420),
                settledFor: 0, lane: lane)
                == CGSize(width: 1984, height: 1792))
        #expect(
            EncoderSizing.capacity(
                current: CGSize(width: 4800, height: 2800), window: CGSize(width: 4900, height: 2880),
                settledFor: 0, lane: lane)
                == CGSize(width: 5120, height: 2880))
    }

    @Test("once the size has held, an oversized encoder fits exactly again")
    func settles() {
        let current = CGSize(width: 1984, height: 1792)
        let window = CGSize(width: 1570, height: 1420)
        #expect(EncoderSizing.capacity(current: current, window: window, settledFor: 1.0, lane: lane) == nil)
        #expect(EncoderSizing.capacity(current: current, window: window, settledFor: 1.5, lane: lane) == window)
    }
}
