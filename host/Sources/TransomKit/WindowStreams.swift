import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import Metal

/// Each shared window of a lane encoded on its own (protocol.md §6, v4).
///
/// The Mac has one hardware HEVC encoder and its time is set by pixel count,
/// about 1.45 ms a megapixel on an M5 Pro (measured 2026-10-30): a whole 5K lane
/// took 23 ms even when one caret blinked. So the lane is still captured as one
/// display stream (which keeps a window's menus and popovers in its picture),
/// but only windows that ScreenCaptureKit reports dirty are cropped out, on the
/// GPU, and encoded, each by an encoder of its own size. Typing in one window
/// costs that window's pixels; a still window costs nothing.
///
/// `process` runs on the capture queue.
///
/// A window keeps its encoder through a resize while it fits (v5): the window
/// fills the top left of the encoder's picture, the rest is black, and every
/// frame says how much is window. One that outgrows its encoder gets a new one
/// (`generation` + 1, its first frame a keyframe) with room to grow
/// (`EncoderSizing`), so a drag costs a keyframe every so often instead of one
/// per step, and once the size has settled it goes back to an exact fit.
///
/// Each encoder has at most `maxInFlight` frames in flight. When the encoder
/// cannot keep up with the capture rate, frames would otherwise queue inside
/// VideoToolbox, each waiting on the ones before it: a 7 megapixel window at
/// 120 fps, while the encoder ran slow at 22 ms a frame, fell 42 ms behind. A
/// frame that arrives while its window is full waits instead, replacing any
/// older one, and goes when the encoder has room, so the window runs at the
/// rate its encoder manages.
public final class WindowStreams: @unchecked Sendable {
    public typealias Sink = @Sendable (VideoServer.StreamFrame) -> Void

    private let registry: WindowRegistry
    private let fps: Int
    private let bitrate: Int
    private let format: HEVCEncoder.Format
    private let sink: Sink
    private let cropper: FrameCropper?

    /// Unchecked: `crop` stays on the capture queue and the rest is under `lock`.
    private final class Stream: @unchecked Sendable {
        let encoder: HEVCEncoder
        /// The encoder's picture size.
        let capacity: CGSize
        /// Touched only on the capture queue: the window's part of the lane,
        /// since when it has been this size, and black of `capacity` to pad with.
        var crop: CGRect
        var sizedAt: TimeInterval
        var blank: CVPixelBuffer?
        let generation: UInt32
        let pool: CVPixelBufferPool
        /// Under `WindowStreams.lock`: frames with the encoder, and the newest
        /// frame waiting for it.
        var inFlight = 0
        var waiting: Frame?
        var closed = false
        init(
            encoder: HEVCEncoder, capacity: CGSize, crop: CGRect, generation: UInt32,
            pool: CVPixelBufferPool
        ) {
            self.encoder = encoder
            self.capacity = capacity
            self.crop = crop
            self.sizedAt = ProcessInfo.processInfo.systemUptime
            self.generation = generation
            self.pool = pool
        }
    }
    /// Two keep VideoToolbox's pipeline full: one alone adds about 5 ms a frame
    /// to a big window at 120 fps, and two cost nothing over no limit.
    static let maxInFlight = 2

    private struct Frame {
        let buffer: CVPixelBuffer
        let pts: CMTime
        let capturedMicros: UInt64
        let keyframe: Bool
        /// The window's part of `buffer`, from its top left.
        let content: CGSize
    }
    /// Touched only on the capture queue.
    private var streams: [UInt64: Stream] = [:]
    private var generations: [UInt64: UInt32] = [:]
    /// Sizes an encoder could not be made for, so they are not retried each frame.
    private var failed: [UInt64: CGSize] = [:]

    private let lock = NSLock()
    /// Windows to encode at the next refresh, or every window.
    private var pendingAll = false
    private var pending: Set<UInt64> = []
    private var _usingHardware = true

    /// Whether every encoder made so far runs on the hardware encoder.
    public var usingHardware: Bool { lock.withLock { _usingHardware } }
    /// Called whenever a window gets its first encoder or a new one.
    public var onEncoderCreated: (@Sendable (HEVCEncoder) -> Void)?

    public init(
        registry: WindowRegistry, fps: Int, bitrateBitsPerSecond: Int,
        format: HEVCEncoder.Format, sink: @escaping Sink
    ) {
        self.registry = registry
        self.fps = fps
        self.bitrate = bitrateBitsPerSecond
        self.format = format
        self.sink = sink
        self.cropper = FrameCropper()
    }

    /// Make the next frame of window `id` (every window when nil) a keyframe,
    /// encoded at the next refresh even if it did not change.
    public func requestKeyframe(_ id: UInt64?) {
        lock.withLock {
            if let id { pending.insert(id) } else { pendingAll = true }
        }
    }

    /// One captured frame of the lane. `dirty` is what changed, in its pixels;
    /// nil for a refresh, which encodes the windows a keyframe was asked for.
    public func process(
        _ buffer: CVPixelBuffer, pts: CMTime, capturedMicros: UInt64, dirty: [CGRect]?
    ) {
        guard let cropper else { return }
        let bounds = (w: CVPixelBufferGetWidth(buffer), h: CVPixelBufferGetHeight(buffer))
        let entries = registry.snapshot()
        let live = Set(entries.map(\.id))
        for id in streams.keys where !live.contains(id) {
            if let stream = streams[id] { close(stream) }
            streams[id] = nil
            generations[id] = nil
            failed[id] = nil
        }
        let (all, asked): (Bool, Set<UInt64>) = lock.withLock {
            guard dirty == nil else { return (false, []) }
            defer {
                pendingAll = false
                pending = []
            }
            return (pendingAll, pending)
        }

        let now = ProcessInfo.processInfo.systemUptime
        let lane = CGSize(width: bounds.w, height: bounds.h)
        var work: [(id: UInt64, stream: Stream, keyframe: Bool)] = []
        for entry in entries {
            guard let crop = WindowCrop.clip(entry.rect, width: bounds.w, height: bounds.h)
            else { continue }
            var keyframe = all || asked.contains(entry.id)
            var changed = WindowCrop.isDirty(crop, dirty: dirty)
            var stream = streams[entry.id]
            if let s = stream, s.crop.size != crop.size {
                s.sizedAt = now
                changed = true
            }
            let capacity = EncoderSizing.capacity(
                current: stream?.capacity, window: crop.size,
                settledFor: stream.map { now - $0.sizedAt } ?? 0, lane: lane)
            if let capacity {
                guard failed[entry.id] != capacity else { continue }
                if let old = stream { close(old) }
                stream = makeStream(id: entry.id, capacity: capacity, crop: crop)
                streams[entry.id] = stream
                guard stream != nil else { continue }
                keyframe = true
            }
            guard let stream else { continue }
            // Moved or resized within its encoder: a new part of the lane.
            stream.crop = crop
            if crop.size != stream.capacity && stream.blank == nil {
                stream.blank = FrameCropper.blank(
                    like: format.capturePixelFormat, width: Int(stream.capacity.width),
                    height: Int(stream.capacity.height))
            }
            guard keyframe || changed else { continue }
            work.append((entry.id, stream, keyframe))
        }
        guard !work.isEmpty else { return }

        var targets: [CVPixelBuffer] = []
        for item in work {
            var out: CVPixelBuffer?
            CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, item.stream.pool, &out)
            guard let out else { return }
            targets.append(out)
        }
        guard
            cropper.crop(
                buffer, rects: work.map(\.stream.crop), into: targets,
                blanks: work.map(\.stream.blank))
        else { return }
        for (item, target) in zip(work, targets) {
            let frame = Frame(
                buffer: target, pts: pts, capturedMicros: capturedMicros, keyframe: item.keyframe,
                content: item.stream.crop.size)
            let send: Bool = lock.withLock {
                let stream = item.stream
                guard stream.inFlight >= Self.maxInFlight else {
                    stream.inFlight += 1
                    return true
                }
                // A keyframe asked for must survive being replaced.
                let keyframe = frame.keyframe || (stream.waiting?.keyframe ?? false)
                stream.waiting = Frame(
                    buffer: frame.buffer, pts: frame.pts, capturedMicros: frame.capturedMicros,
                    keyframe: keyframe, content: frame.content)
                return false
            }
            if send { submit(frame, to: item.stream) }
        }
    }

    /// Hand `frame` to the stream's encoder, which the caller has counted in
    /// flight; when it is done, the frame that waited meanwhile goes next.
    private func submit(_ frame: Frame, to stream: Stream) {
        if frame.keyframe { stream.encoder.requestKeyframe() }
        let duration = CMTime(value: 1, timescale: CMTimeScale(fps))
        do {
            try stream.encoder.encode(
                frame.buffer, pts: frame.pts, duration: duration,
                capturedMicros: frame.capturedMicros,
                content: (Int(frame.content.width), Int(frame.content.height))
            ) { [weak self, stream] in self?.next(after: stream) }
        } catch {
            next(after: stream)
        }
    }

    private func next(after stream: Stream) {
        let frame: Frame? = lock.withLock {
            guard !stream.closed, let frame = stream.waiting else {
                stream.inFlight -= 1
                return nil
            }
            stream.waiting = nil
            return frame
        }
        if let frame { submit(frame, to: stream) }
    }

    private func close(_ stream: Stream) {
        lock.withLock {
            stream.closed = true
            stream.waiting = nil
        }
        stream.encoder.finish()
    }

    /// Flush and close every encoder.
    public func finish() {
        for stream in streams.values { close(stream) }
        streams = [:]
    }

    private func makeStream(id: UInt64, capacity: CGSize, crop: CGRect) -> Stream? {
        let width = Int(capacity.width)
        let height = Int(capacity.height)
        do {
            let encoder = try HEVCEncoder(
                config: HEVCEncoder.Config(
                    width: width, height: height, fps: fps, bitrateBitsPerSecond: bitrate,
                    // The stream is reliable and the client asks for a keyframe
                    // when it needs one, so this is only a backstop.
                    maxKeyFrameInterval: fps * 60, format: format))
            encoder.extractFrameData = true
            guard let pool = FrameCropper.pool(like: format.capturePixelFormat, width: width, height: height)
            else { return nil }
            let generation = (generations[id] ?? 0) &+ 1
            generations[id] = generation
            failed[id] = nil
            let sink = self.sink
            encoder.onEncodedFrame = { [weak encoder] frame in
                sink(
                    VideoServer.StreamFrame(
                        stream: id, generation: generation, width: UInt32(width),
                        height: UInt32(height), contentWidth: UInt32(frame.contentWidth),
                        contentHeight: UInt32(frame.contentHeight),
                        hvcc: encoder?.parameterSetsHVCC, frame: frame))
            }
            lock.withLock { _usingHardware = _usingHardware && encoder.usingHardware }
            onEncoderCreated?(encoder)
            return Stream(
                encoder: encoder, capacity: capacity, crop: crop, generation: generation, pool: pool)
        } catch {
            Log.encode.notice(
                "video: no encoder for window \(id, privacy: .public) at \(width, privacy: .public)x\(height, privacy: .public): \(String(describing: error), privacy: .public)"
            )
            failed[id] = capacity
            return nil
        }
    }
}

/// How big a window's encoder is. Pure, so it is unit tested.
///
/// Encode time follows the encoder's pixels, not the window's, so a still
/// window fits its encoder exactly. One that outgrows it is being resized, and
/// gets room to keep growing; once its size has held for `settle`, it fits
/// exactly again.
public enum EncoderSizing {
    /// Room to grow, on each side, after outgrowing an encoder.
    static let headroom = 1.25
    /// Sides are rounded up to this, which keeps sizes even for 4:2:0.
    static let step = 64
    /// How long a size must hold before an oversized encoder is replaced.
    static let settle: TimeInterval = 1.5

    /// The size of a new encoder for a window of `window`, or nil to keep the
    /// one of `current` size (nil when there is none). `settledFor` is how long
    /// the window has been this size; `lane` bounds the encoder.
    public static func capacity(
        current: CGSize?, window: CGSize, settledFor: TimeInterval, lane: CGSize
    ) -> CGSize? {
        guard let current else { return window }
        if window.width > current.width || window.height > current.height {
            func grow(_ side: CGFloat, _ limit: CGFloat) -> CGFloat {
                let room = (Int((side * headroom).rounded(.up)) + step - 1) / step * step
                return max(side, min(CGFloat(room), CGFloat(Int(limit) & ~1)))
            }
            return CGSize(
                width: grow(window.width, lane.width), height: grow(window.height, lane.height))
        }
        if window != current && settledFor >= settle { return window }
        return nil
    }
}

/// Which pixels of a lane frame a window's stream takes. Pure, so it is unit
/// tested.
public enum WindowCrop {
    /// Smaller crops are not worth an encoder (and VideoToolbox refuses some).
    static let minimumSide = 32

    /// The part of `rect` inside a `width` x `height` frame, on even pixels for
    /// 4:2:0 chroma; nil when too little of it is on the frame.
    public static func clip(_ rect: WireRect, width: Int, height: Int) -> CGRect? {
        let x0 = max(0, Int(rect.x)) & ~1
        let y0 = max(0, Int(rect.y)) & ~1
        let x1 = min(width, Int(rect.x) + Int(rect.w)) & ~1
        let y1 = min(height, Int(rect.y) + Int(rect.h)) & ~1
        guard x1 - x0 >= minimumSide, y1 - y0 >= minimumSide else { return nil }
        return CGRect(x: x0, y: y0, width: x1 - x0, height: y1 - y0)
    }

    /// Whether anything in `crop` changed. A refresh (no dirty rects) changed
    /// nothing; it encodes only the windows a keyframe was asked for.
    public static func isDirty(_ crop: CGRect, dirty: [CGRect]?) -> Bool {
        guard let dirty else { return false }
        return dirty.contains { $0.intersects(crop) }
    }
}

/// Copies sub-rectangles of a captured frame into window-sized buffers with a
/// Metal blit: the frame never leaves the GPU.
final class FrameCropper {
    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let cache: CVMetalTextureCache

    init?() {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue()
        else { return nil }
        var cache: CVMetalTextureCache?
        guard CVMetalTextureCacheCreate(nil, nil, device, nil, &cache) == kCVReturnSuccess,
            let cache
        else { return nil }
        self.device = device
        self.queue = queue
        self.cache = cache
    }

    /// A pool of IOSurface buffers Metal can write and VideoToolbox can read.
    static func pool(like pixelFormat: OSType, width: Int, height: Int) -> CVPixelBufferPool? {
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: pixelFormat,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any]() as CFDictionary,
            kCVPixelBufferMetalCompatibilityKey: true,
        ]
        var pool: CVPixelBufferPool?
        CVPixelBufferPoolCreate(nil, nil, attrs as CFDictionary, &pool)
        return pool
    }

    /// The Metal format of each plane, and how much smaller than the image it is.
    private static func planes(of pixelFormat: OSType) -> [(MTLPixelFormat, Int)]? {
        switch pixelFormat {
        case kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange:
            return [(.r8Unorm, 1), (.rg8Unorm, 2)]
        case kCVPixelFormatType_32BGRA:
            return [(.bgra8Unorm, 1)]
        default:
            return nil
        }
    }

    /// A buffer of black, to pad a window smaller than its encoder with.
    static func blank(like pixelFormat: OSType, width: Int, height: Int) -> CVPixelBuffer? {
        guard let pool = pool(like: pixelFormat, width: width, height: height) else { return nil }
        var out: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &out)
        guard let out, CVPixelBufferLockBaseAddress(out, []) == kCVReturnSuccess else { return nil }
        defer { CVPixelBufferUnlockBaseAddress(out, []) }
        func fill(_ base: UnsafeMutableRawPointer?, _ bytes: Int, _ pattern: [UInt8]) {
            guard let base else { return }
            let p = base.assumingMemoryBound(to: UInt8.self)
            for i in 0..<bytes { p[i] = pattern[i % pattern.count] }
        }
        if CVPixelBufferIsPlanar(out) {
            // Video-range NV12: Y 16, Cb Cr 128.
            for (plane, value) in [(0, [UInt8(16)]), (1, [UInt8(128)])] {
                fill(
                    CVPixelBufferGetBaseAddressOfPlane(out, plane),
                    CVPixelBufferGetBytesPerRowOfPlane(out, plane)
                        * CVPixelBufferGetHeightOfPlane(out, plane), value)
            }
        } else {
            fill(
                CVPixelBufferGetBaseAddress(out),
                CVPixelBufferGetBytesPerRow(out) * CVPixelBufferGetHeight(out), [0, 0, 0, 255])
        }
        return out
    }

    /// Copy each of `rects` (pixels, even) of `source` into the top left of the
    /// matching target, and the matching blank's black into the rest of it,
    /// waiting until the GPU has done it.
    func crop(
        _ source: CVPixelBuffer, rects: [CGRect], into targets: [CVPixelBuffer],
        blanks: [CVPixelBuffer?]
    ) -> Bool {
        guard let planes = Self.planes(of: CVPixelBufferGetPixelFormatType(source)),
            let commands = queue.makeCommandBuffer(), let blit = commands.makeBlitCommandEncoder()
        else { return false }
        var keep: [CVMetalTexture] = []
        func texture(_ buffer: CVPixelBuffer, plane: Int, format: MTLPixelFormat) -> MTLTexture? {
            let planar = CVPixelBufferIsPlanar(buffer)
            let w = planar ? CVPixelBufferGetWidthOfPlane(buffer, plane) : CVPixelBufferGetWidth(buffer)
            let h = planar ? CVPixelBufferGetHeightOfPlane(buffer, plane) : CVPixelBufferGetHeight(buffer)
            var out: CVMetalTexture?
            guard
                CVMetalTextureCacheCreateTextureFromImage(
                    nil, cache, buffer, nil, format, w, h, plane, &out) == kCVReturnSuccess,
                let out
            else { return nil }
            keep.append(out)
            return CVMetalTextureGetTexture(out)
        }
        for (plane, (format, scale)) in planes.enumerated() {
            guard let from = texture(source, plane: plane, format: format) else {
                blit.endEncoding()
                return false
            }
            for (i, (rect, target)) in zip(rects, targets).enumerated() {
                guard let to = texture(target, plane: plane, format: format) else { continue }
                let w = min(Int(rect.width) / scale, to.width)
                let h = min(Int(rect.height) / scale, to.height)
                blit.copy(
                    from: from, sourceSlice: 0, sourceLevel: 0,
                    sourceOrigin: MTLOrigin(x: Int(rect.minX) / scale, y: Int(rect.minY) / scale, z: 0),
                    sourceSize: MTLSize(width: w, height: h, depth: 1),
                    to: to, destinationSlice: 0, destinationLevel: 0,
                    destinationOrigin: MTLOrigin(x: 0, y: 0, z: 0))
                guard w < to.width || h < to.height, i < blanks.count, let blank = blanks[i],
                    let black = texture(blank, plane: plane, format: format)
                else { continue }
                // Right of the window, then below it.
                for (x, y, bw, bh) in [(w, 0, to.width - w, to.height), (0, h, w, to.height - h)]
                where bw > 0 && bh > 0 {
                    blit.copy(
                        from: black, sourceSlice: 0, sourceLevel: 0,
                        sourceOrigin: MTLOrigin(x: x, y: y, z: 0),
                        sourceSize: MTLSize(width: bw, height: bh, depth: 1),
                        to: to, destinationSlice: 0, destinationLevel: 0,
                        destinationOrigin: MTLOrigin(x: x, y: y, z: 0))
                }
            }
        }
        blit.endEncoding()
        commands.commit()
        commands.waitUntilCompleted()
        withExtendedLifetime(keep) {}
        CVMetalTextureCacheFlush(cache, 0)
        return commands.status == .completed
    }
}

