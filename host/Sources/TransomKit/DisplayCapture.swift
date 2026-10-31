import CoreGraphics
import CoreImage
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit

/// A single ScreenCaptureKit display stream, configured at the display's
/// **native pixel size** so that SCK space == the display's pixel space (I-1).
///
/// This is the shared capture primitive behind both the `capture`/`probe` CLI
/// commands and the app's live probe view. It never scales: the stream config
/// width/height are set to the display's exact pixel dimensions, and the command
/// layer verifies the delivered buffer matches (a mismatch means SCK is scaling
/// and I-1 is already violated).
public final class DisplayCapture: NSObject, SCStreamOutput, @unchecked Sendable {

    /// What SCK was actually configured and is delivering, for I-1 verification.
    public struct FrameStats: Sendable {
        public let configuredWidth: Int
        public let configuredHeight: Int
        public let deliveredWidth: Int
        public let deliveredHeight: Int
        public let pixelFormat: OSType
        public var pixelFormatString: String { fourCC(pixelFormat) }
        /// True iff the delivered buffer matches the display's native pixel size.
        public let matchesNativePixels: Bool
    }

    private let display: DisplayInfo
    private let fps: Int
    /// The apps captured, or nil for the whole display. Grows with `include`.
    private var applicationPIDs: Set<pid_t>? {
        get { lock.withLock { _applicationPIDs } }
        set { lock.withLock { _applicationPIDs = newValue } }
    }
    private var _applicationPIDs: Set<pid_t>?
    private let showsCursor: Bool
    private let pixelFormat: OSType
    private let queue = DispatchQueue(label: "one.transom.host.capture")
    // Accessed only on the capture queue, including explicit refreshes.
    private var lastPixelPTS = CMTime.invalid

    private let lock = NSLock()
    private var stream: SCStream?
    /// Which `start` attempt is current; an abandoned one that starts late stops itself.
    private var startAttempt = 0
    private var latestPixelBuffer: CVPixelBuffer?
    private var _stats: FrameStats?
    private let ciContext = CIContext(options: [.cacheIntermediates: false])

    /// Optional per-frame hook, called on the capture queue with a fresh CGImage.
    /// Used by the app's live view. Leave nil for the CLI's poll-on-demand model.
    public var onFrame: (@Sendable (CGImage) -> Void)?

    /// Optional per-frame hook, called on the capture queue with the **raw**
    /// IOSurface-backed pixel buffer and its presentation timestamp — the
    /// zero-copy entry point for the encoder (issue #3 Phase 2). Unlike `onFrame`,
    /// this never touches `CGImage`, so nothing round-trips through the CPU (I-1
    /// pipeline note). The buffer belongs to SCK's pool; use it **synchronously**
    /// (encode it now). Do not retain it past the call or the pool may recycle it
    /// underneath you. The rects are what changed since the last frame, in its
    /// pixels; nil for a refresh of the last frame.
    public var onPixelBuffer: (@Sendable (CVPixelBuffer, CMTime, [CGRect]?) -> Void)?

    public init(
        display: DisplayInfo, fps: Int = 60, applicationPIDs: Set<pid_t>? = nil,
        pixelFormat: OSType = kCVPixelFormatType_32BGRA, showsCursor: Bool = true
    ) {
        self.display = display
        self.fps = fps
        self._applicationPIDs = applicationPIDs
        self.pixelFormat = pixelFormat
        self.showsCursor = showsCursor
        super.init()
    }

    /// Stats from the most recent delivered frame, if any.
    public var stats: FrameStats? {
        lock.withLock { _stats }
    }

    /// Start the stream. Throws if SCK cannot see the display or Screen Recording
    /// permission is absent.
    ///
    /// SCK sometimes never answers a start while another process is capturing
    /// (replayd logs "connection interrupted" and the call's continuation leaks;
    /// seen every few starts on macOS 26). Each attempt is bounded, and a stuck
    /// one is abandoned for a fresh stream.
    public func start() async throws {
        var lastError: Error = CaptureError.startTimedOut
        for attempt in 1...Self.startAttempts {
            do {
                lock.withLock { startAttempt = attempt }
                try await Self.bounded(seconds: Self.startTimeout) {
                    try await self.startOnce(attempt: attempt)
                }
                return
            } catch CaptureError.startTimedOut {
                Log.encode.notice(
                    "capture: start attempt \(attempt, privacy: .public) on display \(self.display.id, privacy: .public) got no answer; retrying"
                )
                lastError = CaptureError.startTimedOut
            }
        }
        throw lastError
    }

    static let startAttempts = 4
    static let startTimeout = 5.0

    /// Run `operation`, or throw `startTimedOut` after `seconds` without waiting
    /// for it: an operation whose callback never comes is left behind, not awaited.
    static func bounded(seconds: Double, _ operation: @escaping @Sendable () async throws -> Void)
        async throws
    {
        let once = ResumeOnce()
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            once.set(cont)
            Task {
                do {
                    try await operation()
                    once.resume(with: .success(()))
                } catch {
                    once.resume(with: .failure(error))
                }
            }
            Task {
                try? await Task.sleep(for: .seconds(seconds))
                once.resume(with: .failure(CaptureError.startTimedOut))
            }
        }
    }

    /// Whether a stream is running.
    public var isRunning: Bool { lock.withLock { stream != nil } }

    /// Capture `pid`'s windows too. A running stream takes the wider filter in
    /// place; a stopped one uses it when it starts.
    public func include(pid: pid_t) async throws {
        guard var pids = applicationPIDs, !pids.contains(pid) else { return }
        pids.insert(pid)
        applicationPIDs = pids
        guard let stream = lock.withLock({ self.stream }) else { return }
        try await stream.updateContentFilter(try await makeFilter())
    }

    private func makeFilter() async throws -> SCContentFilter {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false, onScreenWindowsOnly: false)
        guard let scDisplay = content.displays.first(where: { $0.displayID == display.id })
        else {
            throw CaptureError.displayNotFound(display.id)
        }
        if let applicationPIDs {
            // Inclusion, not exclusion: other apps, the desktop and this host's
            // own panel must never appear inside a selected window's crop.
            let applications = content.applications.filter { applicationPIDs.contains($0.processID) }
            guard !applications.isEmpty else { throw CaptureError.noSelectedApplications }
            return SCContentFilter(display: scDisplay, including: applications, exceptingWindows: [])
        }
        // Whole-display capture is reserved for the diagnostic CLI/probe.
        return SCContentFilter(display: scDisplay, excludingWindows: [])
    }

    private func startOnce(attempt: Int) async throws {
        let filter = try await makeFilter()

        let config = SCStreamConfiguration()
        // The load-bearing lines for I-1: exact native pixels, no scaling.
        config.width = display.pixelWidth
        config.height = display.pixelHeight
        config.pixelFormat = pixelFormat
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        config.queueDepth = 5
        config.showsCursor = showsCursor
        config.scalesToFit = false
        // ScreenCaptureKit declares backgroundColor as unowned(unsafe). Keep the
        // CGColor alive through SCStream's configuration copy; releasing the
        // temporary immediately after assignment leaves a dangling pointer and
        // crashes in CGColorCreateCopy when the app starts capture in the
        // background.
        let backgroundColor = CGColor(gray: 0, alpha: 1)
        config.backgroundColor = backgroundColor

        let stream = withExtendedLifetime(backgroundColor) {
            SCStream(filter: filter, configuration: config, delegate: nil)
        }
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try await stream.startCapture()
        // An attempt that was given up on may still start late; only the
        // current one keeps its stream.
        let current = lock.withLock {
            guard startAttempt == attempt else { return false }
            self.stream = stream
            return true
        }
        if !current { try? await stream.stopCapture() }
    }

    public func stop() async {
        let s: SCStream? = lock.withLock {
            let current = stream
            stream = nil
            return current
        }
        if let s { try? await s.stopCapture() }
        // Drain an in-flight explicit refresh before the caller finishes the
        // encoder. Refreshes queued after this observe stream == nil.
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            queue.async { continuation.resume() }
        }
    }

    /// SCK emits no complete frames while a display is idle. Feed its retained
    /// native-size image through the encoder on connect or when a keyframe is
    /// asked for (the hook gets no dirty rects). This changes no Mac UI.
    public func requestRefresh() {
        queue.async { [weak self] in self?.refreshOnCaptureQueue() }
    }

    private func refreshOnCaptureQueue() {
        let buffer = lock.withLock { stream == nil ? nil : latestPixelBuffer }
        guard let buffer else { return }
        emitPixelBuffer(buffer, pts: CMClockGetTime(CMClockGetHostTimeClock()), dirty: nil)
    }

    private func emitPixelBuffer(_ buffer: CVPixelBuffer, pts: CMTime, dirty: [CGRect]?) {
        guard let pixelHook = onPixelBuffer else { return }
        // A captured sample can have been queued before an explicit refresh.
        // Keep encoder timestamps monotonic without altering any pixels.
        let timestamp = lastPixelPTS.isValid && CMTimeCompare(pts, lastPixelPTS) <= 0
            ? CMTimeAdd(lastPixelPTS, CMTime(value: 1, timescale: 1_000_000)) : pts
        lastPixelPTS = timestamp
        let signpostState = Log.signposter.beginInterval("capture")
        pixelHook(buffer, timestamp, dirty)
        Log.signposter.endInterval("capture", signpostState)
    }

    /// Latest frame as a CGImage at **native pixels**, converted on demand. Nil
    /// until the first complete frame arrives. Never resamples (I-1): callers that
    /// only need a small preview let the display layer scale it down for drawing.
    ///
    /// The IOSurface-backed buffer is grabbed under the lock and the (potentially
    /// expensive) `createCGImage` runs **outside** it — holding a strong ref keeps
    /// the buffer alive against pool recycling, exactly as the capture callback's
    /// own `onFrame` render does — so a 60fps capture callback is never blocked
    /// waiting on a preview conversion.
    public func latestImage() -> CGImage? {
        let (buffer, ctx): (CVPixelBuffer?, CIContext) = lock.withLock {
            (latestPixelBuffer, ciContext)
        }
        guard let buffer else { return nil }
        return ctx.createCGImage(
            CIImage(cvPixelBuffer: buffer),
            from: CGRect(
                x: 0, y: 0,
                width: CVPixelBufferGetWidth(buffer),
                height: CVPixelBufferGetHeight(buffer)))
    }

    // MARK: - SCStreamOutput

    public func stream(
        _ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of type: SCStreamOutputType
    ) {
        guard type == .screen, sampleBuffer.isValid else { return }
        // Only act on complete frames; SCK also emits idle/blank status frames.
        guard
            let attachments = CMSampleBufferGetSampleAttachmentsArray(
                sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
            let statusRaw = attachments.first?[.status] as? Int,
            let status = SCFrameStatus(rawValue: statusRaw),
            status == .complete
        else { return }

        guard let pixelBuffer = sampleBuffer.imageBuffer else { return }

        let dw = CVPixelBufferGetWidth(pixelBuffer)
        let dh = CVPixelBufferGetHeight(pixelBuffer)
        let fmt = CVPixelBufferGetPixelFormatType(pixelBuffer)
        let stats = FrameStats(
            configuredWidth: display.pixelWidth,
            configuredHeight: display.pixelHeight,
            deliveredWidth: dw,
            deliveredHeight: dh,
            pixelFormat: fmt,
            matchesNativePixels: dw == display.pixelWidth && dh == display.pixelHeight)

        let (ctx, frameHook):
            (
                CIContext, (@Sendable (CGImage) -> Void)?
            ) = lock.withLock {
                latestPixelBuffer = pixelBuffer
                _stats = stats
                return (ciContext, onFrame)
            }

        // Zero-copy tap first: hand the raw IOSurface buffer straight to the
        // encoder before spending anything on the CGImage path (I-1). Signposted
        // so the capture→handoff interval is measurable in Instruments.
        // What changed, in the frame's pixels. Without the attachment, all of it.
        let dirty =
            (attachments.first?[.dirtyRects] as? [NSDictionary])?
            .compactMap { CGRect(dictionaryRepresentation: $0) }
            ?? [CGRect(x: 0, y: 0, width: dw, height: dh)]
        emitPixelBuffer(pixelBuffer, pts: sampleBuffer.presentationTimeStamp, dirty: dirty)

        if let frameHook {
            let image = ctx.createCGImage(
                CIImage(cvPixelBuffer: pixelBuffer),
                from: CGRect(x: 0, y: 0, width: dw, height: dh))
            if let image { frameHook(image) }
        }
    }
}

public enum CaptureError: Error, CustomStringConvertible {
    case displayNotFound(CGDirectDisplayID)
    case noSelectedApplications
    case startTimedOut

    public var description: String {
        switch self {
        case .startTimedOut:
            return "ScreenCaptureKit did not start capturing. Stop other sharing sessions and try again."
        case .displayNotFound(let id):
            return "ScreenCaptureKit does not see display id \(id)."
        case .noSelectedApplications:
            return "ScreenCaptureKit cannot find the selected apps. Open an app window and try again."
        }
    }
}

/// Decode a four-character-code OSType (e.g. a CoreVideo pixel format) into its
/// printable form like `BGRA`.
public func fourCC(_ code: OSType) -> String {
    let bytes = [
        UInt8((code >> 24) & 0xFF),
        UInt8((code >> 16) & 0xFF),
        UInt8((code >> 8) & 0xFF),
        UInt8(code & 0xFF),
    ]
    let scalars = bytes.map { Character(UnicodeScalar($0)) }
    let s = String(scalars).trimmingCharacters(in: .whitespaces)
    return s.isEmpty ? String(code) : "\(s) (0x\(String(code, radix: 16)))"
}

/// Resumes a continuation exactly once, whichever side gets there first.
private final class ResumeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Error>?

    func set(_ continuation: CheckedContinuation<Void, Error>) {
        lock.withLock { self.continuation = continuation }
    }

    func resume(with result: Result<Void, Error>) {
        let c: CheckedContinuation<Void, Error>? = lock.withLock {
            defer { continuation = nil }
            return continuation
        }
        c?.resume(with: result)
    }
}
