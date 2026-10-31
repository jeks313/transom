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
/// `process` runs on the capture queue. A window that changes size gets a new
/// encoder (`generation` + 1), whose first frame is a keyframe.
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
        /// Touched only on the capture queue.
        var crop: CGRect
        let generation: UInt32
        let pool: CVPixelBufferPool
        /// Under `WindowStreams.lock`: frames with the encoder, and the newest
        /// frame waiting for it.
        var inFlight = 0
        var waiting: Frame?
        var closed = false
        init(encoder: HEVCEncoder, crop: CGRect, generation: UInt32, pool: CVPixelBufferPool) {
            self.encoder = encoder
            self.crop = crop
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

        var work: [(id: UInt64, stream: Stream, keyframe: Bool)] = []
        for entry in entries {
            guard let crop = WindowCrop.clip(entry.rect, width: bounds.w, height: bounds.h)
            else { continue }
            var keyframe = all || asked.contains(entry.id)
            let changed = WindowCrop.isDirty(crop, dirty: dirty)
            var stream = streams[entry.id]
            if stream == nil || stream!.crop.size != crop.size {
                guard failed[entry.id] != crop.size else { continue }
                if let old = stream { close(old) }
                stream = makeStream(id: entry.id, crop: crop)
                streams[entry.id] = stream
                guard stream != nil else { continue }
                keyframe = true
            } else if stream!.crop.origin != crop.origin {
                // Moved: same encoder, new place in the lane.
                stream!.crop = crop
            }
            guard let stream, keyframe || changed else { continue }
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
        guard cropper.crop(buffer, rects: work.map(\.stream.crop), into: targets) else { return }
        for (item, target) in zip(work, targets) {
            let frame = Frame(
                buffer: target, pts: pts, capturedMicros: capturedMicros, keyframe: item.keyframe)
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
                    keyframe: keyframe)
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
                capturedMicros: frame.capturedMicros
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

    private func makeStream(id: UInt64, crop: CGRect) -> Stream? {
        let width = Int(crop.width)
        let height = Int(crop.height)
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
                        height: UInt32(height), hvcc: encoder?.parameterSetsHVCC, frame: frame))
            }
            lock.withLock { _usingHardware = _usingHardware && encoder.usingHardware }
            onEncoderCreated?(encoder)
            return Stream(encoder: encoder, crop: crop, generation: generation, pool: pool)
        } catch {
            Log.encode.notice(
                "video: no encoder for window \(id, privacy: .public) at \(width, privacy: .public)x\(height, privacy: .public): \(String(describing: error), privacy: .public)"
            )
            failed[id] = crop.size
            return nil
        }
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

    /// Copy each of `rects` (pixels, even) of `source` into the matching target,
    /// waiting until the GPU has done it.
    func crop(_ source: CVPixelBuffer, rects: [CGRect], into targets: [CVPixelBuffer]) -> Bool {
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
            for (rect, target) in zip(rects, targets) {
                guard let to = texture(target, plane: plane, format: format) else { continue }
                blit.copy(
                    from: from, sourceSlice: 0, sourceLevel: 0,
                    sourceOrigin: MTLOrigin(x: Int(rect.minX) / scale, y: Int(rect.minY) / scale, z: 0),
                    sourceSize: MTLSize(
                        width: min(Int(rect.width) / scale, to.width),
                        height: min(Int(rect.height) / scale, to.height), depth: 1),
                    to: to, destinationSlice: 0, destinationLevel: 0,
                    destinationOrigin: MTLOrigin(x: 0, y: 0, z: 0))
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

