import ApplicationServices
import CoreGraphics
import Foundation

/// System prompts that can block a shared app from somewhere the client cannot
/// see: a Touch ID or password sheet (`coreautha`), an authorization dialog
/// (`SecurityAgent`, or BeyondTrust's `PrivilegeManagement` on managed Macs), a
/// Gatekeeper or notification alert. They appear on the Mac's main display, so
/// the app behind them looks frozen on the client (found 2026-10-30: a 1Password
/// unlock from Safari).
///
/// A session with a prompt display moves each such window onto it and shares it
/// like any other window (`HostSession`), which the probe showed works: both
/// ScreenCaptureKit and AX reach these windows, and AX can move them.
public enum PromptPolicy {
    /// Owners (`kCGWindowOwnerName`) whose windows are prompts.
    public static let owners: Set<String> = [
        "coreautha", "SecurityAgent", "PrivilegeManagement", "UserNotificationCenter",
        "CoreServicesUIAgent", "universalAccessAuthWarn",
    ]

    /// What the client shows as the window's app.
    public static func displayName(owner: String) -> String {
        switch owner {
        case "coreautha": return "Authentication"
        case "SecurityAgent": return "Authorization"
        case "PrivilegeManagement": return "Privilege Management"
        case "UserNotificationCenter": return "Alert"
        case "CoreServicesUIAgent": return "Security"
        case "universalAccessAuthWarn": return "Accessibility"
        default: return owner
        }
    }

    public struct Candidate: Equatable, Sendable {
        public let windowNumber: Int
        public let pid: pid_t
        public let owner: String
        /// Global point space, top-left origin (as AX and CGWindowList use).
        public let bounds: CGRect
    }

    /// The prompt windows in a `CGWindowListCopyWindowInfo` listing that are not
    /// already on one of `shared` (the session's displays, in global points).
    public static func candidates(windows: [[String: Any]], shared: [CGRect]) -> [Candidate] {
        windows.compactMap { w in
            guard let owner = w[kCGWindowOwnerName as String] as? String, owners.contains(owner),
                let number = w[kCGWindowNumber as String] as? Int,
                let pid = w[kCGWindowOwnerPID as String] as? Int32,
                let b = w[kCGWindowBounds as String] as? [String: Any],
                let bounds = CGRect(dictionaryRepresentation: b as CFDictionary),
                bounds.width >= 40, bounds.height >= 40,
                (w[kCGWindowAlpha as String] as? Double ?? 1) > 0
            else { return nil }
            let center = CGPoint(x: bounds.midX, y: bounds.midY)
            guard !shared.contains(where: { $0.contains(center) }) else { return nil }
            return Candidate(windowNumber: number, pid: pid, owner: owner, bounds: bounds)
        }
    }

    /// Where the `index`th prompt goes on a display: cascaded from the top left,
    /// kept wholly on it.
    public static func slot(index: Int, size: CGSize, display: CGRect) -> CGPoint {
        let step = CGFloat(index % 8) * 32
        let x = min(display.minX + 32 + step, display.maxX - size.width)
        let y = min(display.minY + 32 + step, display.maxY - size.height)
        return CGPoint(x: max(display.minX, x), y: max(display.minY, y))
    }

    /// Move the AX window of `candidate` to `origin`. False when no AX window
    /// of its process has its frame, or the move is refused.
    public static func move(_ candidate: Candidate, to origin: CGPoint) -> Bool {
        guard let windows = AXWindow.availableWindows(pid: candidate.pid) else { return false }
        let match = windows.first { win in
            guard let f = win.frame() else { return false }
            return abs(f.minX - candidate.bounds.minX) < 2 && abs(f.minY - candidate.bounds.minY) < 2
                && abs(f.width - candidate.bounds.width) < 2
        }
        guard let element = match?.element else { return false }
        var point = origin
        guard let value = AXValueCreate(.cgPoint, &point) else { return false }
        return AXUIElementSetAttributeValue(element, kAXPositionAttribute as CFString, value)
            == .success
    }
}
