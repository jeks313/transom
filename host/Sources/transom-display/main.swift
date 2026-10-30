// transom-display: owns one virtual display for a `serve` session.
//
//   transom-display <name> <widthPt> <heightPt> [serial]
//
// Creates a HiDPI (2x) CGVirtualDisplay, places it to the right of every other display (never
// main, so the Mac's own screen keeps the menu bar, the Dock and any permission prompt), keeps the
// Mac from idling into display sleep and the lock screen, prints one line, and holds it all until
// stdin reaches EOF. `serve` starts this as a child with a pipe on stdin, so the display goes away
// when `serve` stops or dies, and windows on it move back to a real display.
//
// A separate process because the process that creates a virtual display must run an AppKit event
// loop for its own CoreGraphics to see the display's modes (see VirtualDisplay.swift); `serve` is
// an async command without one. Output, on stdout:
//
//   ready id=<displayID> origin=<x>,<y> pt=<w>x<h> px=<w>x<h>
//   error <message>
import AppKit
import IOKit.pwr_mgt

setvbuf(stdout, nil, _IOLBF, 0)

func fail(_ message: String) -> Never {
    print("error \(message)")
    exit(1)
}

let args = CommandLine.arguments
guard args.count >= 4, let widthPt = Int(args[2]), let heightPt = Int(args[3]) else {
    fail("usage: transom-display <name> <widthPt> <heightPt> [serial]")
}
let name = args[1]
let serial = args.count > 4 ? UInt32(args[4]) ?? 1 : 1

let problems = VirtualDisplay.checkPrivateAPI()
guard problems.isEmpty else { fail("virtual display API unavailable: \(problems.joined(separator: "; "))") }

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)

func onlineDisplays() -> [CGDirectDisplayID] {
    var ids = [CGDirectDisplayID](repeating: 0, count: 64)
    var count: UInt32 = 0
    CGGetOnlineDisplayList(64, &ids, &count)
    return Array(ids.prefix(Int(count)))
}

// Held for the life of the process; releasing it removes the display.
var held: VirtualDisplay?
var awake: IOPMAssertionID = 0

Task { @MainActor in
    let display: VirtualDisplay
    do {
        display = try VirtualDisplay(
            name: name, widthPt: widthPt, heightPt: heightPt, scale: 2,
            productID: 0x7472, serialNum: serial)
    } catch {
        fail("\(error)")
    }
    held = display
    guard await display.waitUntilOnline(timeout: 5) else {
        fail("display \(display.displayID) did not come up at \(widthPt)x\(heightPt) pt @2x")
    }

    // To the right of everything else, top-aligned: global coordinates never overlap another
    // display, and the display at (0,0), the main one, is untouched.
    let rightEdge = onlineDisplays().filter { $0 != display.displayID }
        .map { Int32(CGDisplayBounds($0).maxX) }.max() ?? 0
    var config: CGDisplayConfigRef?
    CGBeginDisplayConfiguration(&config)
    CGConfigureDisplayOrigin(config, display.displayID, rightEdge, 0)
    let placed = CGCompleteDisplayConfiguration(config, .forSession)
    guard placed == .success else { fail("could not place display: CGError \(placed.rawValue)") }
    for _ in 0..<40 where Int32(display.bounds.minX) != rightEdge {
        try? await Task.sleep(nanoseconds: 50_000_000)
    }

    // A locked Mac refuses AX window moves, and an idle one locks; hold it awake while sharing.
    IOPMAssertionCreateWithName(
        kIOPMAssertionTypePreventUserIdleDisplaySleep as CFString,
        IOPMAssertionLevel(kIOPMAssertionLevelOn), "Transom is sharing windows" as CFString, &awake)

    let b = display.bounds, px = display.pixelSize
    print("ready id=\(display.displayID) origin=\(Int(b.minX)),\(Int(b.minY)) "
        + "pt=\(Int(b.width))x\(Int(b.height)) px=\(px.width)x\(px.height)")

    Thread.detachNewThread {
        while FileHandle.standardInput.availableData.count > 0 {}
        exit(0)
    }
}
app.run()
