import ApplicationServices
import CoreGraphics
import CoreMedia
import Foundation

/// One lane of a session: the apps shared on one display, captured and encoded
/// on its own (protocol.md §1). The first target is the lane's primary app.
public struct LaneConfig: Sendable {
    public var targets: [TargetApp]
    public var display: DisplayInfo

    public init(targets: [TargetApp], display: DisplayInfo) {
        self.targets = targets
        self.display = display
    }
}

/// Everything needed to start serving apps on their displays: the same knobs the
/// `serve` CLI takes, in one `Sendable` value the host app can also build.
public struct HostConfig: Sendable {
    /// Every lane, lane 0 first. Lane ids on the wire are indices into this.
    public var lanes: [LaneConfig]
    /// Lane 0's primary app, its other apps and its display.
    public var target: TargetApp { lanes[0].targets[0] }
    public var additionalTargets: [TargetApp] { Array(lanes[0].targets.dropFirst()) }
    public var display: DisplayInfo { lanes[0].display }
    public var host: String
    public var controlPort: UInt16
    public var videoPort: UInt16
    public var gutter: Int
    public var tile: Bool
    public var video: Bool
    public var bitrateMbps: Int
    public var fps: Int
    /// Chroma / bit-depth the video is encoded at. Defaults to the mode the
    /// Windows in-box decoder can decode (4:2:0 8-bit); `.hevc444_10bit` is the
    /// crisp-text target for a 4:4:4-capable client (protocol.md §6-7).
    public var videoFormat: HEVCEncoder.Format
    /// Map Windows modifiers namesake (Ctrl→Control) instead of the default swap
    /// (Ctrl→Command). The Cmd-vs-Ctrl product decision for input (issue #7).
    public var namesakeModifiers: Bool
    /// Print the full coordinate/keycode chain for each injected input event.
    public var logInput: Bool
    /// Whether client input is delivered to the shared windows in place
    /// (`.background`, the default) or by raising them and driving the real
    /// cursor (`.foreground`). See `InputDelivery`.
    public var inputDelivery: InputDelivery
    /// Show hidden apps and bring minimized and full-screen windows onto the
    /// display before tiling, so every window of a selected app is shared
    /// (`WindowRestorer`). Off shares only what is already on the display.
    public var restoreWindows: Bool

    /// - Parameters:
    ///   - target, additionalTargets, display: lane 0.
    ///   - moreLanes: further lanes, each on its own display. An app already in an
    ///     earlier lane is dropped from a later one, and a lane left empty with it.
    public init(
        target: TargetApp,
        additionalTargets: [TargetApp] = [],
        display: DisplayInfo,
        moreLanes: [LaneConfig] = [],
        host: String = "127.0.0.1",
        controlPort: UInt16 = TransomPorts.control,
        videoPort: UInt16 = TransomPorts.video,
        gutter: Int = Tiler.defaultGutter,
        tile: Bool = true,
        video: Bool = true,
        bitrateMbps: Int = 40,
        fps: Int = 60,
        videoFormat: HEVCEncoder.Format = .hevc420_8bit,
        namesakeModifiers: Bool = false,
        logInput: Bool = false,
        inputDelivery: InputDelivery = .background,
        restoreWindows: Bool = true
    ) {
        var seen: Set<pid_t> = []
        self.lanes = ([LaneConfig(targets: [target] + additionalTargets, display: display)] + moreLanes)
            .map { LaneConfig(targets: $0.targets.filter { seen.insert($0.pid).inserted }, display: $0.display) }
            .filter { !$0.targets.isEmpty }
        self.host = host
        self.controlPort = controlPort
        self.videoPort = videoPort
        self.gutter = gutter
        self.tile = tile
        self.video = video
        self.bitrateMbps = bitrateMbps
        self.fps = fps
        self.videoFormat = videoFormat
        self.namesakeModifiers = namesakeModifiers
        self.logInput = logInput
        self.inputDelivery = inputDelivery
        self.restoreWindows = restoreWindows
    }
}

/// A live snapshot of a running `HostSession`, cheap to poll from any thread.
///
/// This is what the host app's Status section renders and what a periodic `serve`
/// status line prints. All rate numbers are measured on the host (fps/bitrate over
/// a short sliding window; `encodeLatencyMillis` is the capture→encoded-frame
/// pipeline latency). End-to-end network latency is deliberately absent: the host
/// cannot measure it without the client's cooperation, and reporting a guess would
/// be worse than reporting nothing.
public struct HostStatus: Sendable {
    public var running = false
    public var videoEnabled = false

    public var controlClientConnected = false
    public var videoClientConnected = false

    /// Frames/sec and Mbps measured over the last ~1.5 s of encoded output, summed
    /// over every lane.
    public var measuredFPS = 0.0
    public var measuredBitrateMbps = 0.0
    /// Mean host-side pipeline latency (capture handoff → encoded frame), in ms.
    public var encodeLatencyMillis = 0.0
    public var totalFramesEncoded = 0

    /// VideoToolbox's own read-back of whether the encoder is on the hardware path
    /// (every lane's).
    public var usingHardware = false
    /// The chroma / bit-depth the encoder was configured to produce. Known before
    /// the first frame, so the UI/CLI can show the mode immediately.
    public var videoFormat: HEVCEncoder.Format = .hevc420_8bit
    /// The codec + chroma the encoder reports it is producing (e.g. "hvc1 … 4:2:0
    /// 8-bit"), captured on the first encoded frame.
    public var encoderFormatSummary = "—"

    /// The startup tile layout with post-clamp actual rects and deltas (I-4/OQ-2),
    /// lane by lane.
    public var tilePlacements: [TilePlacement] = []
    /// Set if the tiler could not lay the window set out (surfaced, not swallowed).
    public var tileError: String?
    /// Live count of windows the AX watchers currently track, in every lane.
    public var liveWindowCount = 0
    /// Things the session did to the Mac's windows that the person at the Mac
    /// should know about: windows it restored, windows it left out.
    public var notices: [String] = []

    public init() {}

    /// Is the encoder healthy — i.e. actually on the hardware path for whatever
    /// chroma was selected? A silent fall to software is the real failure (it can't
    /// keep 60fps); running 4:2:0 by choice is not. The UI shows this as OK/degraded.
    public var encoderHardwareOK: Bool { usingHardware }

    /// Are we specifically on the 4:4:4 10-bit hardware path (the crisp-text
    /// target)? `true` only when hardware *and* 4:4:4 was selected. Distinct from
    /// `encoderHardwareOK`: 4:2:0 is decodable-by-default, not a fallback.
    public var encoderIs444Hardware: Bool {
        usingHardware && videoFormat == .hevc444_10bit
    }
}

/// A cheap, poll-on-demand snapshot for the host app's live stream preview: the
/// frame currently being encoded (nil when video is off or before the first frame)
/// plus every window the AX watcher is tracking, with its **VDS-pixel** rect and id
/// (protocol.md §3). The image is the native-pixel capture (never resampled, I-1);
/// the preview panel scales it down for display. This is a viewport onto the
/// session, never a fork of the capture path.
///
/// Not `Sendable` on purpose — it carries a `CGImage` and is meant to be read
/// synchronously on the main thread (the UI's timer), never sent across a task.
public struct HostPreview {
    public var image: CGImage?
    public var windows: [WindowRegistry.Entry]
    public var displayPixelWidth: Int
    public var displayPixelHeight: Int

    public init(
        image: CGImage?, windows: [WindowRegistry.Entry],
        displayPixelWidth: Int, displayPixelHeight: Int
    ) {
        self.image = image
        self.windows = windows
        self.displayPixelWidth = displayPixelWidth
        self.displayPixelHeight = displayPixelHeight
    }
}

/// The serving pipeline for a session's lanes, extracted from the `serve` command
/// so the CLI and the SwiftUI host app drive the **same** code (the app is a thin
/// shell over `serve`, not a fork of it).
///
/// Each lane is one display with its apps: it tiles their windows once at startup
/// (I-5), watches them via AX, and — with `video` — captures its display and
/// HEVC-encodes it in hardware (chroma per `config.videoFormat`, default 4:2:0
/// 8-bit so the client's in-box decoder shows pixels). Every lane shares one
/// control and one video connection (protocol v2): window ids are unique across
/// lanes, and each video message names its lane. Capture and AX never stop while
/// a client comes and goes; a reconnect resyncs from the registries (see
/// `ControlServer`).
///
/// ### Concurrency
/// `@unchecked Sendable` on the same confinement invariant the rest of this
/// package uses: `start()`/`stop()` are lifecycle calls the owner serializes (the
/// UI disables Start while running), and every field a background thread touches —
/// the encoder/capture callbacks and the connection callbacks — is either itself
/// thread-safe (`WindowRegistry`) or guarded by `statsLock` here. `status()` reads
/// only that locked snapshot, so it is safe to call from the main thread on a timer.
public final class HostSession: @unchecked Sendable {
    public let config: HostConfig

    // Lifecycle-owned; mutated only inside start()/stop().
    private var lanes: [Lane] = []
    private var watcherRunLoop: CFRunLoop?
    private var controlListener: TCPListener?
    private var videoListener: TCPListener?
    private var controlServer: ControlServer?
    private var videoServer: VideoServer?
    private var eventSink: AsyncStream<(lane: UInt32, event: WindowWatcher.WindowEvent)>.Continuation?
    private var clientSink: AsyncStream<ClientMessage>.Continuation?
    private var tasks: [Task<Void, Never>] = []
    /// Menu bar reads and presses: each can wait on an app for up to a second.
    private let menuQueue = DispatchQueue(label: "one.transom.host.menus", qos: .userInitiated)
    /// Apps with a menu read queued, so a burst of refreshes reads once.
    private var menuReadsQueued: Set<pid_t> = []

    // Everything a background callback writes lives behind this lock.
    private let statsLock = NSLock()
    private var isRunning = false
    private var controlConnected = false
    private var videoConnected = false
    private var totalFrames = 0
    private var encoderFormatSummary = "—"
    private var usingHardware = false
    private var tilePlacements: [UInt32: [TilePlacement]] = [:]
    private var tileError: String?
    private var notices: [String] = []
    private var videoEnabled = false
    /// (uptimeNanos, compressedBytes) of recently encoded frames, trimmed to a
    /// short window so fps/bitrate reflect *now*, not the whole session.
    private var frameSamples: [(t: UInt64, bytes: Int)] = []
    /// Per lane, a FIFO of capture-handoff timestamps awaiting their encoded
    /// output, so each output frame can be matched to its input for a latency
    /// measurement. Encoded output is in input order (frame reordering is off), so
    /// front-matching holds within a lane.
    private var pendingEncodeStarts: [UInt32: [UInt64]] = [:]
    private var latencySamplesMs: [Double] = []

    /// Sliding window for the fps/bitrate estimate.
    private static let rateWindowNanos: UInt64 = 1_500_000_000

    public init(config: HostConfig) {
        self.config = config
    }

    /// A snapshot of the current state. Cheap; safe from any thread.
    public func status() -> HostStatus {
        var s = HostStatus()
        let liveCount = lanes.reduce(0) { $0 + $1.registry.snapshot().count }
        statsLock.withLock {
            trimRateWindowLocked()
            s.running = isRunning
            s.videoEnabled = videoEnabled
            s.controlClientConnected = controlConnected
            s.videoClientConnected = videoConnected
            s.totalFramesEncoded = totalFrames
            s.usingHardware = usingHardware
            s.videoFormat = config.videoFormat
            s.encoderFormatSummary = encoderFormatSummary
            s.tilePlacements = tilePlacements.keys.sorted().flatMap { tilePlacements[$0] ?? [] }
            s.tileError = tileError
            s.notices = notices
            let (fps, mbps) = rateLocked()
            s.measuredFPS = fps
            s.measuredBitrateMbps = mbps
            s.encodeLatencyMillis =
                latencySamplesMs.isEmpty
                ? 0 : latencySamplesMs.reduce(0, +) / Double(latencySamplesMs.count)
        }
        s.liveWindowCount = liveCount
        return s
    }

    /// Lane 0's latest capture frame + its live window rects, for the host app's
    /// stream-preview panel. Cheap enough to poll ~10 times/sec; reads the same
    /// live capture and registry the stream uses, so the panel shows exactly what
    /// is going out. The frame is native pixels (not resampled, I-1) — the panel
    /// scales it for display. `image` is nil when video is off or no frame has
    /// arrived yet; `windows` is still populated (control-only sessions have a
    /// window layout but no pixels). Call on the main thread — it is not `Sendable`.
    public func preview() -> HostPreview {
        HostPreview(
            image: lanes.first?.capture?.latestImage(),
            windows: lanes.first?.registry.snapshot() ?? [],
            displayPixelWidth: config.display.pixelWidth,
            displayPixelHeight: config.display.pixelHeight)
    }

    // MARK: - Lifecycle

    /// Tile, start the AX watchers + control server, and (if `video`) each lane's
    /// capture + encoder and the video server. Returns once everything is
    /// listening and the initial registries are seeded, so a client connecting
    /// immediately gets a full resync. Throws with a plain message if a permission
    /// or bind precondition fails.
    public func start() async throws {
        do {
            try await startImpl()
        } catch {
            // A video bind failure, watcher error, or permission race can happen
            // after part of the session is live. Always tear down the partial
            // graph before surfacing the error so retrying from the UI is safe.
            await stop()
            throw error
        }
    }

    private func startImpl() async throws {
        try preflight()

        let ids = WindowIDSource()
        let (events, eventSink) = AsyncStream.makeStream(
            of: (lane: UInt32, event: WindowWatcher.WindowEvent).self)
        self.eventSink = eventSink

        for (index, laneConfig) in config.lanes.enumerated() {
            lanes.append(
                try await makeLane(
                    id: UInt32(index), config: laneConfig, registry: WindowRegistry(ids: ids),
                    emit: { lane, event in eventSink.yield((lane, event)) }))
        }
        let lanes = self.lanes

        let (clientMessages, clientSink) = AsyncStream.makeStream(of: ClientMessage.self)
        self.clientSink = clientSink

        let controlServer = ControlServer(
            lanes: lanes.map { .init(id: $0.id, vdsSize: $0.vdsSize, registry: $0.registry) })
        self.controlServer = controlServer
        await controlServer.setOnClientMessage { message in clientSink.yield(message) }
        await controlServer.setOnConnectionChange { [weak self] connected in
            self?.statsLock.withLock { self?.controlConnected = connected }
            // A dropped client leaves no modifier or button held for the next one
            // (issue #7), and no shared app still believing it is the active one.
            if !connected {
                for lane in lanes {
                    lane.injector.reset()
                    lane.injector.releaseFocus()
                }
            }
        }
        let controlListener = try TCPListener(
            host: config.host, port: config.controlPort, label: "control")
        self.controlListener = controlListener

        // Start every lane's AX watchers on one run-loop thread and wait until they
        // have registered + seeded the registries (same handshake the CLI used).
        let ctx = WatcherThreadBox()
        let tile = config.tile
        let gutter = config.gutter
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            let watcherThread = Thread { [weak self] in
                ctx.runLoop = CFRunLoopGetCurrent()
                let watchers = lanes.flatMap(\.watchers)
                do {
                    for watcher in watchers { try watcher.start() }
                    cont.resume()
                } catch {
                    for watcher in watchers { watcher.stop() }
                    cont.resume(throwing: error)
                    return
                }
                // The startup tile only holds until something on the Mac moves or
                // resizes a window (a streamed title-bar drag, a zoom). Re-tile
                // once such a change has settled into an overlap (I-5).
                let layoutTimer =
                    tile
                    ? Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in
                        for lane in lanes {
                            self?.enforceLayout(lane, gutter: gutter)
                        }
                    } : nil
                if let layoutTimer { RunLoop.current.add(layoutTimer, forMode: .default) }
                CFRunLoopRun()
                layoutTimer?.invalidate()
                for watcher in watchers { watcher.stop() }
            }
            watcherThread.stackSize = 1 << 20
            watcherThread.start()
        }
        self.watcherRunLoop = ctx.runLoop

        tasks.append(Task { await controlServer.serve(listener: controlListener) })
        tasks.append(
            Task {
                for await item in events { await controlServer.broadcast(item.event, lane: item.lane) }
            })
        // Drive client requests in arrival order (AX writes serialise on each
        // lane's resize actor), routed to the lane that owns the window, and tick
        // at ~20Hz so a coalesced live from a paused drag still flushes.
        tasks.append(
            Task { [weak self] in
                func lane(owning id: UInt64) -> Lane? {
                    lanes.first { $0.registry.element(for: id) != nil }
                }
                for await message in clientMessages {
                    switch message {
                    case let .requestResize(id, size, phase):
                        await lane(owning: id)?.resize.handle(id: id, size: size, phase: phase)
                    case let .input(id, _, _), let .requestFocus(id):
                        lane(owning: id)?.injector.handle(message)
                    case let .requestClose(id):
                        lane(owning: id)?.injector.close(id: id)
                    case let .menuPick(id, path, title):
                        guard let lane = lane(owning: id) else { break }
                        self?.pressMenu(lane: lane, window: id, path: path.map(Int.init), title: title)
                    case let .menuRefresh(id):
                        guard let lane = lane(owning: id) else { break }
                        self?.readMenus(pid: pid_t(bitPattern: ControlServer.app(of: id, in: lane.registry)))
                    case let .requestKeyframe(laneID):
                        // A client may deliberately drop stale compressed frames
                        // to protect interaction latency. Restart its dependency
                        // chain at the next encoded frame.
                        for lane in lanes where laneID == nil || lane.id == laneID {
                            lane.encoder?.requestKeyframe()
                        }
                    }
                }
            })
        tasks.append(
            Task {
                while !Task.isCancelled {
                    try? await Task.sleep(nanoseconds: 50_000_000)
                    for lane in lanes { await lane.resize.tick() }
                }
            })
        try await controlListener.start()

        for lane in lanes {
            for target in lane.targets { readMenus(pid: target.pid) }
        }

        if config.video {
            try await startVideo(lanes)
            statsLock.withLock { videoEnabled = true }
        }

        controlListener.advertise(
            address: config.host, videoPort: config.video ? config.videoPort : nil)
        statsLock.withLock { isRunning = true }
    }

    /// One lane's AX side: restore and tile its windows, then build (not start)
    /// its watchers, resize service and input injector.
    private func makeLane(
        id: UInt32, config laneConfig: LaneConfig, registry: WindowRegistry,
        emit: @escaping @Sendable (UInt32, WindowWatcher.WindowEvent) -> Void
    ) async throws -> Lane {
        let disp = laneConfig.display
        let targets = laneConfig.targets
        let pids = targets.map(\.pid)
        if config.restoreWindows {
            let report = await WindowRestorer.restore(pids: pids)
            if let summary = report.summary { statsLock.withLock { notices.append(summary) } }
        }

        // Tile once at startup so streamed windows are non-overlapping (I-5), and
        // keep the requested-vs-actual placements for the Status view (I-4/OQ-2).
        // Whatever does not fit is minimized rather than refusing the session,
        // the same treatment a window opened mid-session gets.
        if config.tile {
            switch TileService.admit(pids: pids, display: disp, gutter: config.gutter) {
            case .success(let admission):
                statsLock.withLock { tilePlacements[id] = admission.placements }
                if !admission.overflow.isEmpty {
                    let notice = Self.overflowNotice(admission.overflow)
                    Log.ax.notice("tile: \(notice, privacy: .public)")
                    statsLock.withLock { notices.append(notice) }
                }
            case .failure(let error):
                statsLock.withLock { tileError = error.description }
                throw ProbeError("The selected windows do not fit on the sharing display. Choose fewer apps or a larger display. \(error.description)")
            }
        }

        let watchers = targets.map { target in
            let watcher = WindowWatcher(pid: target.pid, display: disp, registry: registry,
                appName: target.name)
            watcher.onEvent = { event in emit(id, event) }
            if config.tile {
                // Long enough for the layout guard to notice a window that left
                // the display and bring it back before its proxy is closed.
                watcher.offDisplayGrace = 3
                let gutter = config.gutter
                watcher.prepareNewWindow = { [weak self] element in
                    switch TileService.layout(pids: pids, display: disp, gutter: gutter, fit: true) {
                    case .success(let placements):
                        self?.statsLock.withLock { self?.tilePlacements[id] = placements; self?.tileError = nil }
                        return true
                    case .failure(let error):
                        // Keep a rejected new document from covering another
                        // shared crop. The document remains open and can be restored.
                        let minimized = AXUIElementSetAttributeValue(element, kAXMinimizedAttribute as CFString, kCFBooleanTrue)
                        let message = minimized == .success
                            ? "The new window does not fit on the sharing display and was minimized. Share fewer apps, then restore it on the Mac."
                            : "The new window does not fit and could not be minimized. Stop sharing and choose fewer apps."
                        self?.statsLock.withLock { self?.tileError = error.description }
                        emit(id, .sharingFailed(message: message))
                        return false
                    }
                }
            }
            return watcher
        }

        // Phase 4 (issue #6): the geometry roundtrip. Client RequestResize is
        // throttled (~10Hz), written to AX, read back, and emitted as windowMoved
        // (ACTUAL geometry, I-4) through the same ordered event stream, so both this
        // CLI and the host app get resize for free.
        let resize = ResizeService(
            registry: registry, display: disp, gutter: config.gutter,
            emit: { event in emit(id, event) })

        // Phase 5 (issue #7): input injection. Client Input/RequestFocus become
        // CGEvents, translating window-local pixels through the one coordinate
        // function (I-3). Modifier mapping is the Cmd-vs-Ctrl decision (default
        // swaps Ctrl→Command). `inputDelivery` decides whether they reach the
        // window in place or by raising it. Shared by the CLI and the host app.
        let injector = InputInjector(
            display: disp, registry: registry,
            modifierMap: config.namesakeModifiers ? .namesake : .swap,
            delivery: config.inputDelivery)
        if config.logInput { injector.onTrace = { line in print("  \(line)") } }

        return Lane(
            id: id, targets: targets, display: disp, registry: registry, watchers: watchers,
            resize: resize, injector: injector)
    }

    /// Read `pid`'s menu bar off the AX threads and hand it to the control server,
    /// which sends it if it changed.
    private func readMenus(pid: pid_t) {
        guard pid > 0, statsLock.withLock({ menuReadsQueued.insert(pid).inserted }) else { return }
        let namesake = config.namesakeModifiers
        guard let server = controlServer else { return }
        menuQueue.async { [weak self] in
            self?.statsLock.withLock { _ = self?.menuReadsQueued.remove(pid) }
            guard let menus = MenuReader.read(pid: pid, namesake: namesake) else { return }
            Task { await server.setMenuBar(app: UInt32(bitPattern: pid), menus: menus) }
        }
    }

    /// Choose a menu item for window `id`: make the window key in place first, so
    /// the item acts on it, then press it and read the bar again (an item can
    /// toggle a check or change what is enabled).
    private func pressMenu(lane: Lane, window id: UInt64, path: [Int], title: String) {
        let pid = pid_t(bitPattern: ControlServer.app(of: id, in: lane.registry))
        guard pid > 0 else { return }
        lane.injector.requestFocus(id: id)
        // The app validates its menus against its key window, which the focus
        // above sets only once the app has handled the posted events.
        let namesake = config.namesakeModifiers
        let injector = lane.injector
        menuQueue.asyncAfter(deadline: .now() + 0.15) { [weak self] in
            let how: String
            if let item = MenuReader.item(pid: pid, path: path, title: title) {
                // The shortcut goes the way a typed one does, so an app that only
                // enables its window items while frontmost still acts on it.
                let shortcut = MenuReader.shortcut(of: item, namesake: namesake)
                if let keys = MenuShortcut.virtualKeys(shortcut) {
                    for vk in keys { injector.inject(id: id, event: .keyDown(vk: vk), ts: 0) }
                    for vk in keys.reversed() { injector.inject(id: id, event: .keyUp(vk: vk), ts: 0) }
                    how = "typed \(shortcut)"
                } else {
                    how = String(describing: MenuReader.press(item))
                }
            } else {
                how = "changed"
            }
            Log.input.notice(
                "menu: \(title, privacy: .public) at \(path.map(String.init).joined(separator: "/"), privacy: .public): \(how, privacy: .public)"
            )
            self?.menuQueue.asyncAfter(deadline: .now() + 0.3) { self?.readMenus(pid: pid) }
        }
    }

    /// Re-tile a lane whose windows a Mac-side change left overlapping. Runs on
    /// the watcher run-loop thread.
    private func enforceLayout(_ lane: Lane, gutter: Int) {
        let result = TileService.enforceNonOverlap(
            &lane.layoutGuard, pids: lane.targets.map(\.pid), display: lane.display,
            gutter: gutter, pointerDown: lane.injector.isPointerDown)
        switch result {
        case .success(let placements):
            Log.ax.notice("layout: re-tiled lane \(lane.id, privacy: .public) after a Mac-side geometry change")
            statsLock.withLock {
                tilePlacements[lane.id] = placements
                tileError = nil
            }
        case .failure(let error):
            Log.ax.notice(
                "layout: could not restore non-overlap: \(error.description, privacy: .public)"
            )
            statsLock.withLock { tileError = error.description }
        case nil:
            break
        }
    }

    private func startVideo(_ lanes: [Lane]) async throws {
        for lane in lanes {
            let disp = lane.display
            let enc = try HEVCEncoder(
                config: HEVCEncoder.Config(
                    width: disp.pixelWidth, height: disp.pixelHeight, fps: config.fps,
                    bitrateBitsPerSecond: config.bitrateMbps * 1_000_000,
                    maxKeyFrameInterval: config.fps * 2, format: config.videoFormat))
            enc.extractFrameData = true
            lane.encoder = enc
            lane.capture = DisplayCapture(display: disp, fps: config.fps,
                applicationPIDs: Set(lane.targets.map(\.pid)),
                pixelFormat: config.videoFormat.capturePixelFormat,
                // Background input never moves the Mac's cursor, so the one SCK would
                // draw belongs to whoever is at the Mac, not to the client.
                showsCursor: config.inputDelivery == .foreground)
        }
        let hardware = lanes.allSatisfy { $0.encoder?.usingHardware == true }
        statsLock.withLock { usingHardware = hardware }

        var providers: [UInt32: @Sendable () -> Data?] = [:]
        for lane in lanes {
            let enc = lane.encoder
            providers[lane.id] = { enc?.parameterSetsHVCC }
        }
        let videoServer = VideoServer(hvccProviders: providers)
        self.videoServer = videoServer
        await videoServer.setOnConnectionChange { [weak self] connected in
            self?.statsLock.withLock { self?.videoConnected = connected }
            if connected {
                for lane in lanes {
                    lane.encoder?.requestKeyframe()
                    lane.capture?.requestRefresh()
                }
            }
        }
        let listener = try TCPListener(host: config.host, port: config.videoPort, label: "video")
        self.videoListener = listener

        let frameDuration = CMTimeMake(value: 1, timescale: Int32(config.fps))
        // One lane at a time: two captures starting at once in separate host
        // processes hung in startCapture, so never overlap the starts.
        for lane in lanes {
            guard let enc = lane.encoder, let cap = lane.capture else { continue }
            let laneID = lane.id
            let (frames, frameSink) = AsyncStream.makeStream(
                of: HEVCEncoder.EncodedFrame.self, bufferingPolicy: .bufferingNewest(4))
            enc.onEncodedFrame = { [weak self, weak enc] frame in
                // Read the encoder directly instead of the lane's on a VT thread;
                // keep it weak so the callback does not retain its own encoder.
                self?.recordEncodedFrame(
                    frame, lane: laneID, formatSummary: enc?.outputFormatSummary ?? "unknown")
                frameSink.yield(frame)
            }
            cap.onPixelBuffer = { [weak self] pixelBuffer, pts in
                self?.markEncodeStart(lane: laneID)
                try? enc.encode(pixelBuffer, pts: pts, duration: frameDuration)
            }
            try await cap.start()
            tasks.append(Task { for await f in frames { await videoServer.send(f, lane: laneID) } })
        }

        try await listener.start()
        tasks.append(Task { await videoServer.serve(listener: listener) })
        let captures = lanes.compactMap(\.capture)
        tasks.append(Task {
            while !Task.isCancelled {
                do { try await Task.sleep(for: .milliseconds(300)) }
                catch { break }
                if await videoServer.hasClient {
                    for cap in captures { cap.requestIdleRefresh() }
                }
            }
        })
    }

    /// Stop everything and reset to a clean state. Safe to call more than once.
    public func stop() async {
        controlListener?.stop()
        videoListener?.stop()
        await controlServer?.stop()
        await videoServer?.stop()
        for lane in lanes {
            if let capture = lane.capture { await capture.stop() }
            lane.injector.releaseFocus()
            lane.encoder?.finish()
        }
        eventSink?.finish()
        clientSink?.finish()
        for task in tasks { task.cancel() }
        // Stopping its run loop ends the watcher thread; the AXObservers and their
        // sources are released when the watchers deallocate below.
        if let runLoop = watcherRunLoop { CFRunLoopStop(runLoop) }

        tasks = []
        lanes = []
        watcherRunLoop = nil
        controlListener = nil
        videoListener = nil
        controlServer = nil
        videoServer = nil
        eventSink = nil
        clientSink = nil

        statsLock.withLock {
            isRunning = false
            controlConnected = false
            videoConnected = false
            videoEnabled = false
            notices = []
            frameSamples = []
            pendingEncodeStarts = [:]
            latencySamplesMs = []
        }
    }

    /// Minimize the windows the startup layout had no room for, and say which.
    private static func overflowNotice(_ overflow: [AXWindow]) -> String {
        let minimized = overflow.filter { $0.setMinimized(true) == .success }
        let titles = overflow.map { $0.title.isEmpty ? "Untitled" : $0.title }
        let shown = titles.prefix(3).map { "“\($0)”" }.joined(separator: ", ")
            + (titles.count > 3 ? " and \(titles.count - 3) more" : "")
        let verb = minimized.count == overflow.count ? "were minimized" : "could not all be minimized"
        return "\(overflow.count) window\(overflow.count == 1 ? "" : "s") did not fit on the sharing display and \(verb): \(shown). "
            + "Close some windows, lower the gutter, or use a larger display to share them."
    }

    // MARK: - Preconditions

    private func preflight() throws {
        guard AXIsProcessTrusted() else {
            throw ProbeError(
                "Accessibility is not granted. Grant it in System Settings and relaunch "
                    + "(compare against `transom-host doctor`).")
        }
        if config.video && !CGPreflightScreenCaptureAccess() {
            throw ProbeError(
                "Screen Recording is not granted, which is required to capture and encode video.")
        }
        guard PrivateAddress.isPrivateIPv4(config.host) else {
            throw ProbeError(TransportError.refusedPublicBind(config.host).description)
        }
        let addressIsAssigned = config.host.hasPrefix("127.")
            || HostDiscovery.localAddresses().contains(config.host)
        guard addressIsAssigned else {
            throw ProbeError(
                TransportError.addressNotAvailable(
                    config.host, available: HostDiscovery.localAddresses()
                ).description
            )
        }
        if config.video && config.controlPort == config.videoPort {
            throw ProbeError("control and video ports must be different")
        }
    }

    // MARK: - Stats (called from capture / VideoToolbox threads)

    private func markEncodeStart(lane: UInt32) {
        let now = DispatchTime.now().uptimeNanoseconds
        statsLock.withLock {
            pendingEncodeStarts[lane, default: []].append(now)
            // Bound the queue: if output ever lags input, drop the oldest so the
            // latency estimate stays fresh instead of drifting unboundedly.
            if pendingEncodeStarts[lane]!.count > 12 { pendingEncodeStarts[lane]!.removeFirst() }
        }
    }

    private func recordEncodedFrame(
        _ frame: HEVCEncoder.EncodedFrame, lane: UInt32, formatSummary: String
    ) {
        let now = DispatchTime.now().uptimeNanoseconds
        statsLock.withLock {
            totalFrames += 1
            encoderFormatSummary = formatSummary
            frameSamples.append((t: now, bytes: frame.byteCount))
            trimRateWindowLocked()
            if let start = pendingEncodeStarts[lane]?.first {
                pendingEncodeStarts[lane]!.removeFirst()
                if now >= start {
                    latencySamplesMs.append(Double(now - start) / 1_000_000)
                    if latencySamplesMs.count > 60 { latencySamplesMs.removeFirst() }
                }
            }
        }
    }

    /// Drop rate samples older than the window. Caller holds `statsLock`.
    private func trimRateWindowLocked() {
        guard let newest = frameSamples.last?.t else { return }
        let cutoff = newest >= Self.rateWindowNanos ? newest - Self.rateWindowNanos : 0
        while let oldest = frameSamples.first, oldest.t < cutoff {
            frameSamples.removeFirst()
        }
    }

    /// (fps, Mbps) over the retained window. Caller holds `statsLock`.
    private func rateLocked() -> (Double, Double) {
        guard let first = frameSamples.first, let last = frameSamples.last,
            frameSamples.count >= 2, last.t > first.t
        else { return (0, 0) }
        let span = Double(last.t - first.t) / 1_000_000_000
        let fps = Double(frameSamples.count - 1) / span
        // Bytes of every frame *after* the window's opening sample arrived in `span`.
        let bytes = frameSamples.dropFirst().reduce(0) { $0 + $1.bytes }
        let mbps = Double(bytes) * 8 / span / 1_000_000
        return (fps, mbps)
    }
}

/// One lane's pipeline. Its AX objects are fixed at creation; `capture` and
/// `encoder` are set once by `startVideo`, and `layoutGuard` is touched only on
/// the watcher run-loop thread.
private final class Lane: @unchecked Sendable {
    let id: UInt32
    let targets: [TargetApp]
    let display: DisplayInfo
    let registry: WindowRegistry
    let watchers: [WindowWatcher]
    let resize: ResizeService
    let injector: InputInjector
    var capture: DisplayCapture?
    var encoder: HEVCEncoder?
    var layoutGuard = LayoutGuard()

    var vdsSize: WireSize {
        WireSize(w: UInt32(display.pixelWidth), h: UInt32(display.pixelHeight))
    }

    init(
        id: UInt32, targets: [TargetApp], display: DisplayInfo, registry: WindowRegistry,
        watchers: [WindowWatcher], resize: ResizeService, injector: InputInjector
    ) {
        self.id = id
        self.targets = targets
        self.display = display
        self.registry = registry
        self.watchers = watchers
        self.resize = resize
        self.injector = injector
    }
}

/// Cross-thread handoff for the watcher run-loop thread's `CFRunLoop`, written on
/// that thread before the continuation resumes, so the caller reads it safely.
private final class WatcherThreadBox: @unchecked Sendable {
    var runLoop: CFRunLoop?
}
