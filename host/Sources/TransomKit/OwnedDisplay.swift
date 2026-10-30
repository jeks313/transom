import CoreGraphics
import Foundation

/// A virtual display the session creates for itself, so sharing needs no
/// BetterDisplay and no hand-made display (architecture.md 3.5 used to require
/// one). It sits to the right of the Mac's own displays and is never main: the
/// host already converts through each display's origin (I-3), and keeping the
/// real screen main leaves the menu bar, the Dock and permission prompts where the
/// person at the Mac can see them.
///
/// The display lives in a child process, `transom-display`, installed next to
/// this executable: the process that creates a `CGVirtualDisplay` must run an
/// AppKit event loop, and `serve` has none. The child holds the display (and keeps
/// the Mac from idling into the lock screen, where AX refuses window moves) until
/// its stdin closes, so the display disappears when this object is released or
/// this process dies, and macOS moves its windows back to a real display.
public final class OwnedDisplay: @unchecked Sendable {
    public let info: DisplayInfo
    private let child: Process
    private let stdin: Pipe

    private init(info: DisplayInfo, child: Process, stdin: Pipe) {
        self.info = info
        self.child = child
        self.stdin = stdin
    }

    deinit { close() }

    /// Create a `widthPoints` x `heightPoints` display at 2x and wait until
    /// CoreGraphics reports it at that size. `serial` keeps macOS's remembered
    /// arrangement stable for the same app across sessions.
    public static func create(
        name: String, widthPoints: Int, heightPoints: Int, serial: UInt32
    ) async throws -> OwnedDisplay {
        let helper = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
            .deletingLastPathComponent().appendingPathComponent("transom-display")
        guard FileManager.default.isExecutableFile(atPath: helper.path) else {
            throw ProbeError("transom-display is missing next to this executable (\(helper.path)).")
        }
        let child = Process()
        child.executableURL = helper
        child.arguments = [name, String(widthPoints), String(heightPoints), String(serial)]
        let stdin = Pipe()
        let stdout = Pipe()
        child.standardInput = stdin
        child.standardOutput = stdout
        try child.run()

        let reader = stdout.fileHandleForReading
        let line = await Task.detached { Self.firstLine(of: reader) }.value
        guard let line, line.hasPrefix("ready "),
            let idField = line.split(separator: " ").first(where: { $0.hasPrefix("id=") }),
            let id = CGDirectDisplayID(idField.dropFirst(3))
        else {
            child.terminate()
            throw ProbeError("transom-display failed: \(line ?? "exited without output")")
        }

        // The child already waited for the mode; this process learns of it a
        // moment later.
        let wantWidth = widthPoints * 2
        for _ in 0..<60 {
            if let info = Displays.byID(id), info.pixelWidth == wantWidth {
                return OwnedDisplay(info: info, child: child, stdin: stdin)
            }
            try? await Task.sleep(for: .milliseconds(50))
        }
        child.terminate()
        throw ProbeError("display \(id) did not appear at \(widthPoints)x\(heightPoints) pt @2x.")
    }

    /// Release the display. Safe to call more than once.
    public func close() {
        try? stdin.fileHandleForWriting.close()
        if child.isRunning { child.waitUntilExit() }
    }

    private static func firstLine(of handle: FileHandle) -> String? {
        var bytes = Data()
        while true {
            let chunk = handle.readData(ofLength: 1)
            if chunk.isEmpty || chunk.first == UInt8(ascii: "\n") { break }
            bytes.append(chunk)
        }
        return bytes.isEmpty ? nil : String(decoding: bytes, as: UTF8.self)
    }
}

/// Stage Manager shrinks windows into its strip, where they stop drawing at full
/// size: the stream shows thumbnails at a few frames a second while AX still
/// reports full frames (found in Phase 0). Sharing needs it off.
public enum StageManager {
    public static var isEnabled: Bool {
        CFPreferencesCopyAppValue("GloballyEnabled" as CFString, "com.apple.WindowManager" as CFString)
            as? Bool ?? false
    }
}
