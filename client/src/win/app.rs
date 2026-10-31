//! The application: window class, the shared window procedure, the message pump,
//! and the proxy lifecycle driven by the control stream.
//!
//! Threading model: Win32 is single-threaded here. The `Session`'s reader threads
//! decode the protocol and post `SessionEvent`s onto an `mpsc`; the pump drains
//! that channel between message batches and turns `ModelEvent`s into proxy
//! windows. The window procedure reaches back into the `App` through a raw pointer
//! stashed in each window's `GWLP_USERDATA` — the standard Win32-in-Rust pattern,
//! sound because the pump never holds a Rust borrow of the `App` across
//! `DispatchMessageW`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW, IsZoomed,
    LoadCursorW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassW,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, CREATESTRUCTW,
    GWLP_USERDATA, IDC_ARROW, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WM_ACTIVATE, WM_CAPTURECHANGED, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCREATE, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SIZE, WM_SIZING,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetMenu, SC_KEYMENU, WM_COMMAND, WM_ENTERMENULOOP, WM_EXITMENULOOP, WM_INITMENUPOPUP,
    WM_SYSCOMMAND,
};

use super::connect::{Action, Dashboard};
use super::gpu::{Gpu, SourceTexture};
use crate::connections::Connection;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
type ConnectResult = Result<(Connection, Session, Receiver<SessionEvent>), String>;
use super::input;
use super::menu::MenuBar;
use super::proxy::Proxy;
use crate::model::{ModelEvent, Window};
use crate::latency::{self, FrameStamps, LatencyStats, STAGES};
use crate::session::{Session, SessionEvent, VideoEvent, VideoSink};
use crate::wire::{ClientMessage, InputEvent, MenuItem, Rect, ResizePhase, Size, WindowKind};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, IsIconic, IsWindowVisible, KillTimer, SetForegroundWindow, SetTimer, SW_HIDE,
    SW_RESTORE,
};

#[cfg(windows)]
use super::decode::{DecoderInput, DecoderWorker, Wake};

struct NativeEvent {
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    rect: Option<RECT>,
}
thread_local! {
    static NATIVE_EVENTS: RefCell<VecDeque<NativeEvent>> = const { RefCell::new(VecDeque::new()) };
    static APP_POINTER: Cell<*mut App> = const { Cell::new(std::ptr::null_mut()) };
    static IN_TICK: Cell<bool> = const { Cell::new(false) };
}
const CLASS_NAME: PCWSTR = w!("TransomProxyWindow");

/// `MK_LBUTTON | MK_RBUTTON | MK_MBUTTON`: the mouse buttons a button message's
/// `wParam` reports as still down.
const MK_BUTTONS: usize = 0x0001 | 0x0002 | 0x0010;

/// Reconnect backoff after the control channel drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub struct AppConfig {
    pub host: String,
    pub control_port: u16,
    pub video_port: Option<u16>,
    pub checkerboard: bool,
}

pub struct App {
    gpu: Gpu,
    cfg: AppConfig,
    dashboard: Dashboard,
    selected: Option<Connection>,
    connecting: Option<Receiver<ConnectResult>>,
    active: bool,
    notice: Option<String>,
    video_notice: Option<String>,
    session: Option<Session>,
    /// Each lane's decoder input, shared with the session's video thread.
    video_inputs: Arc<Mutex<HashMap<u32, DecoderInput>>>,
    /// Set by a decoder when a frame is ready; the message loop waits on it.
    wake: WakeEvent,
    /// When stage latency was last logged, and the worst lane's median total.
    latency_reported: Instant,
    latency_ms: Option<f32>,
    rx: Option<std::sync::mpsc::Receiver<SessionEvent>>,
    proxies: HashMap<u64, Proxy>,
    windows: HashMap<u64, Window>,
    hwnd_to_id: HashMap<isize, u64>,
    /// One video pipeline per lane (protocol v2): its display size, the texture
    /// its windows sample from, and its decoder.
    lanes: HashMap<u32, LaneVideo>,
    /// Each app's newest menu bar, and the native bar built from it per proxy.
    menus: HashMap<u32, Vec<MenuItem>>,
    menu_bars: HashMap<u64, MenuBar>,
    /// A native menu is open: bars whose items changed wait for it to close.
    in_menu_loop: bool,
    menus_stale: HashSet<u32>,
    /// When each app's menu bar was last asked for again, to ask once a second.
    menu_refreshed: HashMap<u32, Instant>,
    cascade: u32,
    /// Latest unsent hover move per proxy. High-polling-rate mice can emit far
    /// more messages than the control channel or Mac cursor needs; one per pump
    /// preserves the newest position without blocking the Win32 wndproc.
    pending_mouse_moves: HashMap<u64, InputEvent>,
    reconnect_at: Option<Instant>,
    now_epoch: Instant,
}

/// One lane's video: its display, the texture its windows crop from, its decoder.
struct LaneVideo {
    vds: Size,
    source: Option<SourceTexture>,
    decoder: Option<DecoderWorker>,
    /// Video-health counters. If access units keep arriving but none ever decode
    /// (the tell-tale of the in-box HEVC decoder rejecting the host's 4:4:4 10-bit
    /// stream), we warn once instead of silently showing the placeholder forever.
    video_decoded: u64,
    warned_no_decode: bool,
    /// Per-stage latency of presented frames, and the frame uploaded since the
    /// last present (stamped `presented` once drawn).
    latency: LatencyStats,
    shown: Option<FrameStamps>,
}

/// The auto-reset event decoders set when a frame is ready, as a plain value
/// so a decoder thread can hold it.
#[derive(Clone, Copy)]
struct WakeEvent(isize);

/// Append stage-latency lines to `%LOCALAPPDATA%\Transom\latency.log`, kept
/// under about a megabyte.
fn latency_log(lines: &[String]) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let path = std::path::PathBuf::from(dir).join("Transom").join("latency.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for line in lines {
        let _ = writeln!(file, "{stamp} {line}");
    }
}

/// The texture a window samples from: its lane's.
fn source_for<'a>(
    lanes: &'a HashMap<u32, LaneVideo>,
    windows: &HashMap<u64, Window>,
    id: u64,
) -> Option<&'a SourceTexture> {
    let lane = windows.get(&id)?.lane;
    lanes.get(&lane)?.source.as_ref()
}

impl App {
    pub fn new(gpu: Gpu, cfg: Option<AppConfig>, dashboard: Dashboard) -> App {
        let active = cfg.is_some();
        let cfg = cfg.unwrap_or(AppConfig {
            host: String::new(),
            control_port: crate::wire::DEFAULT_CONTROL_PORT,
            video_port: Some(crate::wire::DEFAULT_VIDEO_PORT),
            checkerboard: false,
        });
        let selected = if active {
            Connection::manual(
                &cfg.host,
                &cfg.control_port.to_string(),
                &cfg.video_port.map(|p| p.to_string()).unwrap_or_default(),
            )
            .ok()
        } else {
            None
        };
        App {
            gpu,
            cfg,
            dashboard,
            selected,
            connecting: None,
            active,
            notice: None,
            video_notice: None,
            session: None,
            video_inputs: Arc::new(Mutex::new(HashMap::new())),
            wake: WakeEvent(
                unsafe { CreateEventW(None, false, false, None) }
                    .map(|h| h.0 as isize)
                    .unwrap_or(0),
            ),
            latency_reported: Instant::now(),
            latency_ms: None,
            rx: None,
            proxies: HashMap::new(),
            windows: HashMap::new(),
            hwnd_to_id: HashMap::new(),
            lanes: HashMap::new(),
            menus: HashMap::new(),
            menu_bars: HashMap::new(),
            in_menu_loop: false,
            menus_stale: HashSet::new(),
            menu_refreshed: HashMap::new(),
            cascade: 0,
            pending_mouse_moves: HashMap::new(),
            reconnect_at: None,
            now_epoch: Instant::now(),
        }
    }

    /// Milliseconds since the app started — the client's monotonic `ts` clock
    /// (protocol.md §4; opaque to the host).
    fn now_ms(&self) -> u64 {
        self.now_epoch.elapsed().as_millis() as u64
    }

    /// Attempt to (re)connect the session. Failures are logged and retried on the
    /// backoff; a live host is not required for the window manager to be up.
    fn connect(&mut self) {
        if !self.active || self.connecting.is_some() {
            return;
        }
        let Some(mut connection) = self.selected.clone() else {
            return;
        };
        self.dashboard
            .set_status(&format!("Connecting to {}…", connection.name), true);
        let (tx, rx) = mpsc::channel();
        self.connecting = Some(rx);
        // Frames go straight from the video thread to their lane's decoder.
        let inputs = Arc::new(Mutex::new(HashMap::<u32, DecoderInput>::new()));
        self.video_inputs = inputs.clone();
        let sink: VideoSink = Arc::new(move |event| {
            let lane = match &event {
                VideoEvent::Frame { lane, .. } => *lane,
                _ => return Some(event),
            };
            let Some(input) = inputs.lock().ok().and_then(|m| m.get(&lane).cloned()) else {
                return Some(event);
            };
            let VideoEvent::Frame {
                keyframe,
                pts_micros,
                stamps,
                data,
                ..
            } = event
            else {
                return None;
            };
            let stamps = FrameStamps {
                submitted: latency::now_micros(),
                ..stamps
            };
            input.submit(data, keyframe, pts_micros, stamps);
            None
        });
        std::thread::spawn(move || {
            let result = (|| {
                // Rediscover saved Bonjour identities before using an endpoint:
                // a DHCP lease may now belong to a different machine.
                if !connection.id.starts_with("manual:") {
                    connection = crate::discovery::scan()
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .find(|c| c.id == connection.id)
                        .ok_or(
                            "Mac is offline or not sharing. Open Transom Host and press Start.",
                        )?;
                }
                let (session, events) = Session::connect_with_sink(
                    &connection.host,
                    connection.control_port,
                    connection.video_port,
                    Some(sink),
                )
                .map_err(|e| e.to_string())?;
                Ok((connection, session, events))
            })();
            // If the user cancelled, dropping the failed delivery closes sockets.
            let _ = tx.send(result);
        });
    }

    fn poll_dashboard(&mut self) {
        let app_ptr = self as *mut App;
        match self.dashboard.tick() {
            Some(Action::Connect(c)) => {
                self.disconnect();
                self.cfg.host = c.host.clone();
                self.cfg.control_port = c.control_port;
                self.cfg.video_port = c.video_port;
                self.selected = Some(c);
                self.active = true;
                self.connect();
            }
            Some(Action::Disconnect) => {
                self.disconnect_to_dashboard();
            }
            Some(Action::OpenWindow(id)) => {
                self.open_window(id, app_ptr);
                self.refresh_gallery();
                self.update_status();
            }
            Some(Action::HideWindow(id)) => {
                if let Some(proxy) = self.proxies.get(&id) {
                    unsafe {
                        let _ = ShowWindow(proxy.hwnd, SW_HIDE);
                    }
                }
                self.refresh_gallery();
                self.update_status();
            }
            None => {}
        }
        let result = self.connecting.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.connecting = None;
            match result {
                Ok((c, session, rx)) => {
                    self.cfg.host = c.host.clone();
                    self.cfg.control_port = c.control_port;
                    self.cfg.video_port = c.video_port;
                    self.selected = Some(c.clone());
                    self.session = Some(session);
                    self.rx = Some(rx);
                    self.reconnect_at = None;
                    self.notice = None;
                    self.dashboard.set_connected(true);
                    self.dashboard.remember(c);
                    self.update_status();
                }
                Err(e) => {
                    self.dashboard
                        .set_status(&format!("Could not connect: {e} Retrying…"), true);
                    self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
                }
            }
        }
    }

    fn update_status(&mut self) {
        if self.selected.is_some() {
            let state = if let Some(n) = &self.notice {
                n.clone()
            } else if let Some(n) = &self.video_notice {
                n.clone()
            } else if self.cfg.video_port.is_none() {
                "Control only · video is off".into()
            } else if self.lanes.values().any(|l| l.video_decoded > 0) {
                match self.latency_ms {
                    Some(ms) => format!("Streaming, {ms:.0} ms behind the Mac"),
                    None => "Streaming".into(),
                }
            } else {
                "Waiting for video…".into()
            };
            self.dashboard.set_status(&state, true);
        }
    }

    fn clear_session(&mut self) {
        self.dashboard.set_connected(false);
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
        self.rx = None;
        self.lanes.clear();
        if let Ok(mut inputs) = self.video_inputs.lock() {
            inputs.clear();
        }
        self.menus.clear();
        self.menus_stale.clear();
        self.menu_refreshed.clear();
        self.notice = None;
        self.video_notice = None;
        self.latency_ms = None;
        let ids: Vec<_> = self.proxies.keys().copied().collect();
        for id in ids {
            self.destroy_proxy(id);
        }
        self.windows.clear();
        self.refresh_gallery();
        self.pending_mouse_moves.clear();
        self.cascade = 0;
    }

    fn disconnect(&mut self) {
        self.active = false;
        self.connecting = None;
        self.reconnect_at = None;
        self.clear_session();
    }

    fn disconnect_to_dashboard(&mut self) {
        self.disconnect();
        self.dashboard
            .set_status("Disconnected. Your Mac apps are still open.", false);
        unsafe {
            let hwnd = self.dashboard.hwnd;
            let _ = ShowWindow(
                hwnd,
                if IsIconic(hwnd).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            let _ = SetForegroundWindow(hwnd);
        }
    }

    /// Send a message to the host, if connected.
    fn send(&self, msg: &ClientMessage) {
        if let Some(s) = &self.session {
            if let Err(e) = s.send(msg) {
                eprintln!("send failed: {e}");
            }
        }
    }

    // --- session event handling -----------------------------------------

    fn drain_session(&mut self, app_ptr: *mut App) {
        if !self.active {
            return;
        }
        // Reconnect if it's time; network work never blocks the Win32 pump.
        if self.session.is_none() {
            if self
                .reconnect_at
                .map(|t| Instant::now() >= t)
                .unwrap_or(true)
            {
                self.connect();
            }
            return;
        }

        // Move the receiver out to avoid borrowing self while we mutate it.
        let Some(rx) = self.rx.take() else { return };
        let mut disconnected = false;
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(SessionEvent::Control(ev)) => self.apply_model_event(ev, app_ptr),
                Ok(SessionEvent::Video(v)) => self.apply_video(v),
                Ok(SessionEvent::ControlClosed(reason)) => {
                    self.dashboard.set_status(
                        &format!("Connection lost{}. Retrying…", suffix(reason)),
                        true,
                    );
                    disconnected = true;
                    break;
                }
                Ok(SessionEvent::VideoClosed(reason)) => {
                    self.dashboard.set_status(
                        &format!("Video unavailable{}. Retrying…", suffix(reason)),
                        true,
                    );
                    // A video channel can die independently while control stays
                    // open. Reconnect the whole session so the host sends a fresh
                    // hvcC config and the decoder can recover without restarting
                    // the app.
                    disconnected = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.clear_session();
            self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
        } else {
            self.rx = Some(rx);
        }
    }

    fn apply_model_event(&mut self, ev: ModelEvent, app_ptr: *mut App) {
        match ev {
            ModelEvent::Connected { lanes } => {
                self.lanes.clear();
                if let Ok(mut inputs) = self.video_inputs.lock() {
                    inputs.clear();
                }
                for lane in lanes {
                    self.lanes.insert(
                        lane.lane,
                        LaneVideo {
                            vds: lane.vds,
                            source: None,
                            decoder: None,
                            video_decoded: 0,
                            warned_no_decode: false,
                            latency: LatencyStats::default(),
                            shown: None,
                        },
                    );
                    self.ensure_source(lane.lane);
                }
            }
            ModelEvent::WindowAdded(w) => {
                // A sheet (a system prompt the host moved onto its prompt
                // display) is blocking something: show it at once instead of
                // waiting in the gallery.
                let open = w.kind != WindowKind::Normal;
                let id = w.id;
                self.windows.insert(id, w);
                if open {
                    self.open_window(id, app_ptr);
                }
            }
            ModelEvent::WindowRectChanged { id, source, .. } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.source = source;
                }
                self.update_source_rect(id, source);
            }
            ModelEvent::WindowTitleChanged { id, title } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.title = title.clone();
                }
                self.update_title(id, &title);
            }
            ModelEvent::WindowFocused { .. } => {}
            ModelEvent::WindowRemoved { id } => {
                self.windows.remove(&id);
                self.destroy_proxy(id);
            }
            ModelEvent::Resynced { removed } => {
                for id in removed {
                    self.windows.remove(&id);
                    self.destroy_proxy(id);
                }
            }
            ModelEvent::HostError { code, message } => {
                self.notice = Some(format!("Host error {code}: {message}"));
            }
            ModelEvent::MenuBar { app, menus } => {
                self.menus.insert(app, menus);
                let ids: Vec<u64> = self
                    .proxies
                    .keys()
                    .copied()
                    .filter(|id| self.windows.get(id).map(|w| w.app) == Some(app))
                    .collect();
                for id in ids {
                    self.apply_menu(id);
                }
                return;
            }
        }
        self.refresh_gallery();
        self.update_status();
    }

    /// Show window `id`'s proxy, creating it the first time, and bring it forward.
    fn open_window(&mut self, id: u64, app_ptr: *mut App) {
        if let Some(proxy) = self.proxies.get(&id) {
            unsafe {
                let _ = ShowWindow(
                    proxy.hwnd,
                    if IsIconic(proxy.hwnd).as_bool() {
                        SW_RESTORE
                    } else {
                        SW_SHOW
                    },
                );
                let _ = SetForegroundWindow(proxy.hwnd);
            }
        } else if let Some(w) = self.windows.get(&id).cloned() {
            if let Err(e) = self.create_proxy(id, w.source, &w.title, app_ptr) {
                self.notice = Some(format!("Could not open window: {e}"));
            } else if let Some(proxy) = self.proxies.get(&id) {
                unsafe {
                    let _ = SetForegroundWindow(proxy.hwnd);
                }
            }
        }
    }

    fn refresh_gallery(&mut self) {
        let mut windows: Vec<_> = self.windows.values().cloned().collect();
        windows.sort_by_key(|w| w.id);
        self.dashboard.set_windows(
            windows
                .into_iter()
                .map(|w| {
                    let opened = self
                        .proxies
                        .get(&w.id)
                        .map(|p| unsafe { IsWindowVisible(p.hwnd).as_bool() })
                        .unwrap_or(false);
                    (w, opened)
                })
                .collect(),
        );
    }

    fn apply_video(&mut self, v: VideoEvent) {
        match v {
            VideoEvent::Config { lane, hvcc } => {
                if let Some(l) = self.lanes.get_mut(&lane) {
                    let wake = self.wake;
                    let wake: Wake = Arc::new(move || unsafe {
                        let _ = SetEvent(HANDLE(wake.0 as _));
                    });
                    match DecoderWorker::start(hvcc, l.vds.w, l.vds.h, wake) {
                        Ok(d) => {
                            if let Ok(mut inputs) = self.video_inputs.lock() {
                                inputs.insert(lane, d.input().clone());
                            }
                            l.decoder = Some(d);
                        }
                        Err(e) => {
                            self.video_notice = Some(format!("Cannot start video decoder: {e}"));
                            self.update_status();
                        }
                    }
                }
            }
            // Frames reach a running decoder from the video thread (`connect`);
            // only those that beat their decoder's start come through here.
            VideoEvent::Frame {
                lane,
                data,
                keyframe,
                pts_micros,
                stamps,
                ..
            } => {
                if let Some(decoder) = self.lanes.get(&lane).and_then(|l| l.decoder.as_ref()) {
                    let stamps = FrameStamps {
                        submitted: latency::now_micros(),
                        ..stamps
                    };
                    decoder.input().submit(data, keyframe, pts_micros, stamps);
                }
            }
        }
    }

    /// Upload at most the newest completed decode. The decoder keeps NV12 intact;
    /// the D3D pixel shader performs the color conversion while each proxy window
    /// samples its own crop.
    fn poll_decoder(&mut self) {
        let mut ids: Vec<u32> = self.lanes.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            self.poll_lane_decoder(id);
        }
    }

    fn poll_lane_decoder(&mut self, id: u32) {
        let Some(lane) = self.lanes.get(&id) else {
            return;
        };
        let needs_keyframe = lane
            .decoder
            .as_ref()
            .map(DecoderWorker::take_keyframe_request)
            .unwrap_or(false);
        let error = lane.decoder.as_ref().and_then(DecoderWorker::take_error);
        let frame = lane.decoder.as_ref().and_then(DecoderWorker::take_frame);
        let received = lane.decoder.as_ref().map_or(0, |d| d.input().received());
        // A newly connected client must wait for a complete keyframe.
        // Preserve any more specific error reported by the worker.
        if !lane.warned_no_decode
            && frame.is_none()
            && self.notice.is_none()
            && self.video_notice.is_none()
            && lane.video_decoded == 0
            && received >= 120
        {
            if let Some(l) = self.lanes.get_mut(&id) {
                l.warned_no_decode = true;
            }
            self.video_notice = Some("Video is arriving but no picture has decoded yet. Waiting for a complete keyframe…".into());
            self.update_status();
            eprintln!(
                "video: lane {id} received {received} access units; waiting for a decodable keyframe"
            );
        }
        if needs_keyframe {
            self.send(&ClientMessage::RequestKeyframe { lane: Some(id) });
            self.video_notice = Some("Video fell behind; recovering…".into());
            self.update_status();
            eprintln!("video: lane {id} dropped stale frames; requested a fresh keyframe");
        }
        if let Some(error) = error {
            self.video_notice = Some(error);
            self.update_status();
        }
        let Some(lane) = self.lanes.get_mut(&id) else {
            return;
        };
        if let (Some(frame), Some(source)) = (frame, lane.source.as_ref()) {
            source.update_nv12(&self.gpu, &frame.nv12, frame.stride);
            lane.shown = Some(frame.stamps);
            self.dashboard
                .update_previews_nv12(id, &frame.nv12, frame.stride, lane.vds);
            lane.video_decoded += 1;
            let first = lane.video_decoded == 1;
            let recovered = self.video_notice.take().is_some();
            if first {
                eprintln!("video: lane {id} first decoded frame uploaded to its texture");
            }
            if first || recovered {
                self.update_status();
            }
        }
    }

    fn ensure_source(&mut self, id: u32) {
        let Some(lane) = self.lanes.get_mut(&id) else {
            return;
        };
        let vds = lane.vds;
        let need_new = match &lane.source {
            Some(s) => s.width != vds.w || s.height != vds.h,
            None => true,
        };
        if need_new {
            match SourceTexture::new(&self.gpu, vds.w, vds.h) {
                Ok(t) => lane.source = Some(t),
                Err(e) => eprintln!("lane {id} source texture creation failed: {e}"),
            }
        }
    }

    // --- menu bar ---------------------------------------------------------

    /// Bring proxy `id`'s native menu bar up to its app's newest one: states in
    /// place when only they changed, else a new bar (once no menu is open).
    fn apply_menu(&mut self, id: u64) {
        let Some(app) = self.windows.get(&id).map(|w| w.app) else {
            return;
        };
        let (Some(menus), Some(proxy)) = (self.menus.get(&app), self.proxies.get(&id)) else {
            return;
        };
        if let Some(bar) = self.menu_bars.get_mut(&id) {
            if bar.update_states(menus) {
                return;
            }
        }
        if self.in_menu_loop {
            self.menus_stale.insert(app);
            return;
        }
        let hwnd = proxy.hwnd;
        match MenuBar::build(menus) {
            Ok(bar) => unsafe {
                // Attaching a bar shrinks the client area; the WM_SIZE that
                // follows asks the host for the new size, as any resize does.
                let _ = SetMenu(hwnd, bar.hmenu);
                if let Some(old) = self.menu_bars.insert(id, bar) {
                    old.destroy();
                }
            },
            Err(e) => eprintln!("menu bar for window {id}: {e}"),
        }
    }

    /// Ask the host for `app`'s menu bar again, at most once a second.
    fn refresh_menus(&mut self, id: u64) {
        let Some(app) = self.windows.get(&id).map(|w| w.app) else {
            return;
        };
        let due = self
            .menu_refreshed
            .get(&app)
            .map(|t| t.elapsed() >= Duration::from_secs(1))
            .unwrap_or(true);
        if due {
            self.menu_refreshed.insert(app, Instant::now());
            self.send(&ClientMessage::MenuRefresh { id });
        }
    }

    // --- proxy lifecycle -------------------------------------------------

    fn create_proxy(
        &mut self,
        id: u64,
        source: Rect,
        title: &str,
        app_ptr: *mut App,
    ) -> windows::core::Result<()> {
        let instance = unsafe { GetModuleHandleW(None)? };
        // Cascade the initial desktop position so windows don't stack exactly.
        let offset = (self.cascade % 8) * 48;
        self.cascade += 1;
        let spawn_x = 40 + offset as i32;
        let spawn_y = 40 + offset as i32;

        // Fit an oversized Retina source to a visibly windowed target. The helper
        // preserves exact size whenever it already fits and preserves aspect ratio
        // when it does not; after creation we ask the host to relayout to this
        // physical size so the transient fit snaps back to 1:1.
        let wa = super::dpi::work_area_at(spawn_x, spawn_y);
        let fitted = crate::window_fit::initial_proxy_size(
            Size {
                w: source.w,
                h: source.h,
            },
            Size {
                w: (wa.right - wa.left).max(1) as u32,
                h: (wa.bottom - wa.top).max(1) as u32,
            },
        );
        let win_w = fitted.w;
        let win_h = fitted.h;
        let clamped = win_w != source.w || win_h != source.h;
        let outer = super::frame::outer_size(
            win_w,
            win_h,
            super::dpi::dpi_for_window(self.dashboard.hwnd),
        );
        let x = spawn_x.min(wa.right - win_w as i32).max(wa.left);
        let y = spawn_y.min(wa.bottom - win_h as i32).max(wa.top);

        let title_text = if title.trim().is_empty() {
            "Transom"
        } else {
            title
        };
        let title_w: Vec<u16> = title_text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                CLASS_NAME,
                PCWSTR(title_w.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                x,
                y,
                outer.0,
                outer.1,
                None,
                None,
                HINSTANCE(instance.0),
                Some(app_ptr as *const _),
            )?
        };

        // Rounded corners to eventually match the macOS radius (the region trick
        // for true alpha comes later; HEVC has no alpha, so corners arrive opaque).
        unsafe {
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
        }

        let mut proxy = Proxy::new(&self.gpu, hwnd, source, self.cfg.checkerboard)?;
        // The window's client rect is the fitted size, not the source size, so bring
        // the swapchain to match up front (the creation-time WM_SIZE fires before the
        // proxy is registered and is ignored). Until the host relayouts, the fitted
        // window crops at native scale; the host then relayouts to the requested size.
        let mut actual = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut actual);
        }
        proxy.resize_swapchain(
            &self.gpu,
            actual.right.max(1) as u32,
            actual.bottom.max(1) as u32,
        );
        self.hwnd_to_id.insert(hwnd.0 as isize, id);
        self.proxies.insert(id, proxy);
        self.apply_menu(id);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }

        // If we had to shrink the window to fit the monitor, ask the host to resize
        // the Mac window to the fitted size. The host relayouts natively and reports
        // back the ACTUAL geometry (I-4), which snaps the swapchain to an exact 1:1
        // blit — the product's geometry-mirroring, applied at birth instead of only
        // on a user drag.
        if clamped {
            self.send(&ClientMessage::RequestResize {
                id,
                size: Size { w: win_w, h: win_h },
                phase: ResizePhase::End,
            });
        }

        // Always-visible diagnostic: the source size, the fitted window size, and
        // the DPI/scale the window landed on, so an oversize-source clamp or a
        // resampling regression (window on a scaled monitor) is easy to spot.
        let win_dpi = super::dpi::dpi_for_window(hwnd);
        println!(
            "window {id}: source {}x{} px -> {}x{} px proxy{} on {} DPI ({:.2}x scale)",
            source.w,
            source.h,
            win_w,
            win_h,
            if clamped {
                " [clamped to monitor, requested host resize]"
            } else {
                ""
            },
            win_dpi,
            super::dpi::scale_for_dpi(win_dpi)
        );
        Ok(())
    }

    fn destroy_proxy(&mut self, id: u64) {
        self.pending_mouse_moves.remove(&id);
        // An attached bar is destroyed with its window.
        self.menu_bars.remove(&id);
        if let Some(proxy) = self.proxies.remove(&id) {
            self.hwnd_to_id.remove(&(proxy.hwnd.0 as isize));
            unsafe {
                let _ = DestroyWindow(proxy.hwnd);
            }
            // HWND values may be reused. Discard old notifications before a new
            // proxy can inherit the handle during a fast reconnect.
            NATIVE_EVENTS.with(|q| q.borrow_mut().retain(|e| e.hwnd != proxy.hwnd));
        }
    }

    fn update_title(&mut self, id: u64, title: &str) {
        let Some(proxy) = self.proxies.get(&id) else {
            return;
        };
        let title_text = if title.trim().is_empty() {
            "Transom"
        } else {
            title
        };
        let title_w: Vec<u16> = title_text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = SetWindowTextW(proxy.hwnd, PCWSTR(title_w.as_ptr()));
        }
    }

    /// Host reported ACTUAL geometry for `id`. Update the source sub-rect, and if
    /// the size changed and we're not mid-drag, snap the OS window's client size
    /// to match so the blit is 1:1 (I-4: "asked 2560x1440, got 2560x1438").
    fn update_source_rect(&mut self, id: u64, source: Rect) {
        let Some(proxy) = self.proxies.get_mut(&id) else {
            return;
        };
        proxy.set_source(source);
        if (proxy.width != source.w || proxy.height != source.h) && !proxy.in_size_move {
            let hwnd = proxy.hwnd;
            let inset = super::frame::inset(hwnd, super::dpi::dpi_for_window(hwnd));
            let outer = (source.w as i32 + inset.0, source.h as i32 + inset.1);
            unsafe {
                if IsZoomed(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    outer.0,
                    outer.1,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            // The resulting WM_SIZE resizes the swapchain to the exact rect.
        }
    }

    /// Stamp the frames `render_all` just presented, and every few seconds log
    /// each lane's stage latency and show the whole in the status line.
    fn record_presented(&mut self) {
        let Some(clock) = self.session.as_ref().map(Session::clock) else {
            return;
        };
        let Ok(clock) = clock.lock() else { return };
        let now = latency::now_micros();
        for lane in self.lanes.values_mut() {
            if let Some(stamps) = lane.shown.take() {
                let stamps = FrameStamps {
                    presented: now,
                    ..stamps
                };
                lane.latency.record(&stamps, &clock);
            }
        }
        if self.latency_reported.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.latency_reported = Instant::now();
        let mut lines = Vec::new();
        if let Some(rtt) = clock.min_rtt_micros() {
            lines.push(format!("rtt {:.1} ms", rtt as f64 / 1000.0));
        }
        let mut ids: Vec<_> = self.lanes.keys().copied().collect();
        ids.sort_unstable();
        let mut total = None;
        for id in ids {
            let stats = &self.lanes[&id].latency;
            if let Some(p50) = stats.percentile(STAGES.len() - 1, 0.5) {
                total = Some(total.map_or(p50, |t: f32| t.max(p50)));
                lines.push(format!("lane {id}: {}", stats.summary()));
            }
        }
        drop(clock);
        if total.is_none() {
            return;
        }
        latency_log(&lines);
        self.latency_ms = total;
        self.update_status();
    }

    fn render_all(&mut self) {
        for (id, proxy) in self.proxies.iter_mut() {
            if unsafe { IsWindowVisible(proxy.hwnd).as_bool() && !IsIconic(proxy.hwnd).as_bool() } {
                proxy.render(&self.gpu, source_for(&self.lanes, &self.windows, *id));
            }
        }
    }

    // --- window procedure dispatch --------------------------------------

    /// Handle one message for a proxy window. Returns `Some(lresult)` if handled,
    /// `None` to fall through to `DefWindowProcW`.
    fn handle_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        let id = *self.hwnd_to_id.get(&(hwnd.0 as isize))?;

        match msg {
            // Client pixels exclude the native caption and frame.
            WM_SIZE => {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut r);
                }
                let w = r.right.max(0) as u32;
                let h = r.bottom.max(0) as u32;
                let mut request = false;
                if let Some(proxy) = self.proxies.get_mut(&id) {
                    proxy.resize_swapchain(&self.gpu, w, h);
                    request = w > 0
                        && h > 0
                        && !proxy.in_size_move
                        && (w != proxy.source.w || h != proxy.source.h);
                    proxy.render(&self.gpu, source_for(&self.lanes, &self.windows, id));
                }
                if request {
                    self.send(&ClientMessage::RequestResize {
                        id,
                        size: Size { w, h },
                        phase: ResizePhase::End,
                    });
                }
                Some(LRESULT(0))
            }

            WM_ENTERSIZEMOVE => {
                self.pending_mouse_moves.remove(&id);
                if let Some(proxy) = self.proxies.get_mut(&id) {
                    proxy.begin_size_move();
                }

                Some(LRESULT(0))
            }
            WM_SIZING => {
                let r = unsafe { &*(lparam.0 as *const RECT) };
                // Measured, not computed: a menu bar is part of the chrome.
                let inset = super::frame::inset(hwnd, super::dpi::dpi_for_window(hwnd));
                let size = Size {
                    w: ((r.right - r.left) - inset.0).max(1) as u32,
                    h: ((r.bottom - r.top) - inset.1).max(1) as u32,
                };
                let mut begin = false;
                let mut live = false;
                if let Some(proxy) = self.proxies.get_mut(&id) {
                    begin = !proxy.resizing;
                    proxy.resizing = true;
                    live = proxy.should_send_live(Instant::now());
                }
                if begin {
                    self.send(&ClientMessage::RequestResize {
                        id,
                        size,
                        phase: ResizePhase::Begin,
                    });
                }
                if live {
                    self.send(&ClientMessage::RequestResize {
                        id,
                        size,
                        phase: ResizePhase::Live,
                    });
                }
                Some(LRESULT(1))
            }
            WM_EXITSIZEMOVE => {
                if let Some(proxy) = self.proxies.get_mut(&id) {
                    let resized = proxy.end_size_move();
                    let size = Size {
                        w: proxy.width,
                        h: proxy.height,
                    };
                    let source = proxy.source;
                    if resized {
                        self.send(&ClientMessage::RequestResize {
                            id,
                            size,
                            phase: ResizePhase::End,
                        });
                    } else {
                        // The host changed the window's size while it was being
                        // moved; the snap was deferred until now.
                        self.update_source_rect(id, source);
                    }
                }
                Some(LRESULT(0))
            }

            WM_DPICHANGED => {
                // lParam: suggested new window rect in physical pixels for the new
                // monitor. Trust it; the subsequent WM_SIZE resizes the swapchain.
                let r = unsafe { &*(lparam.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                Some(LRESULT(0))
            }

            WM_PAINT => {
                let source = source_for(&self.lanes, &self.windows, id);
                if let Some(proxy) = self.proxies.get_mut(&id) {
                    proxy.render(&self.gpu, source);
                }
                // Validate the whole window so we don't get flooded with WM_PAINT.
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ValidateRect(hwnd, None);
                }
                Some(LRESULT(0))
            }

            WM_ACTIVATE if (wparam.0 & 0xFFFF) != 0 => {
                // Becoming active: ask the host to raise the Mac window so focus
                // and key routing line up (protocol.md §4 focus/raise).
                self.send(&ClientMessage::RequestFocus { id });
                Some(LRESULT(0))
            }

            WM_MOUSEMOVE => {
                // Coalesce hover motion instead of doing a synchronous TCP write
                // from the UI thread for every raw mouse message.
                if let Some(event) = self.input_event(id, hwnd, msg, wparam, lparam) {
                    self.pending_mouse_moves.insert(id, event);
                }
                // DefWindowProc must still see movement during its modal move /
                // resize loop. We merely keep that local gesture off the wire.
                None
            }

            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if let Some(event) = self.input_event(id, hwnd, msg, wparam, lparam) {
                    // This event already carries its authoritative pointer
                    // coordinates, so an older queued hover can be discarded.
                    self.pending_mouse_moves.remove(&id);
                    self.send_input(id, event);
                }
                None
            }

            WM_CAPTURECHANGED => {
                // Something took the mouse mid-drag (Alt+Tab, a system dialog), so
                // the button-up will never reach this window. Release it for the
                // host; after an ordinary button-up there is nothing left to do.
                let releases = self
                    .proxies
                    .get_mut(&id)
                    .map(|proxy| proxy.pointer.release_all())
                    .unwrap_or_default();
                if !releases.is_empty() {
                    self.pending_mouse_moves.remove(&id);
                }
                for event in releases {
                    self.send_input(id, event);
                }
                Some(LRESULT(0))
            }

            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                if let Some(event) = self.input_event(id, hwnd, msg, wparam, lparam) {
                    self.send_input(id, event);
                }
                // Let DefWindowProc still run for system keys (Alt menu, etc.) by
                // not claiming the message, except we already forwarded it.
                None
            }

            // A menu pick (not an accelerator or a control's notification).
            WM_COMMAND if (wparam.0 >> 16) & 0xFFFF == 0 && lparam.0 == 0 => {
                let pick = self
                    .menu_bars
                    .get(&id)
                    .and_then(|bar| bar.command(wparam.0 & 0xFFFF))
                    .map(|c| (c.path.clone(), c.title.clone()));
                if let Some((path, title)) = pick {
                    self.send(&ClientMessage::MenuPick { id, path, title });
                }
                Some(LRESULT(0))
            }
            WM_INITMENUPOPUP => {
                // Opening a menu reads the Mac's again, so what is enabled
                // follows the app (it lands while this one is open, or next time).
                self.refresh_menus(id);
                Some(LRESULT(0))
            }
            WM_ENTERMENULOOP => {
                self.in_menu_loop = true;
                Some(LRESULT(0))
            }
            WM_EXITMENULOOP => {
                self.in_menu_loop = false;
                for app in std::mem::take(&mut self.menus_stale) {
                    let ids: Vec<u64> = self
                        .proxies
                        .keys()
                        .copied()
                        .filter(|id| self.windows.get(id).map(|w| w.app) == Some(app))
                        .collect();
                    for id in ids {
                        self.apply_menu(id);
                    }
                }
                Some(LRESULT(0))
            }

            WM_CLOSE => {
                // Close the local view, never the remote document. The card can
                // reopen the same HWND without changing its position or size.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                self.pending_mouse_moves.remove(&id);
                self.refresh_gallery();
                Some(LRESULT(0))
            }

            WM_DESTROY => {
                self.hwnd_to_id.remove(&(hwnd.0 as isize));
                self.proxies.remove(&id);
                self.menu_bars.remove(&id);
                Some(LRESULT(0))
            }

            _ => None,
        }
    }

    /// The wire event for one input message. `None` for a pointer message while
    /// the proxy is being moved or resized, or outside the streamed content.
    fn input_event(
        &mut self,
        id: u64,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<InputEvent> {
        let proxy = self.proxies.get_mut(&id)?;
        let is_key = matches!(msg, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP);
        if proxy.in_size_move && !is_key {
            return None;
        }
        let content = Size {
            w: proxy.source.w,
            h: proxy.source.h,
        };
        input::event_for_message(hwnd, msg, wparam, lparam, &mut proxy.pointer, content)
    }

    fn send_input(&self, id: u64, event: InputEvent) {
        self.send(&ClientMessage::Input {
            id,
            event,
            ts: self.now_ms(),
        });
    }

    fn flush_mouse_moves(&mut self) {
        for (id, event) in std::mem::take(&mut self.pending_mouse_moves) {
            self.send_input(id, event);
        }
    }
}

fn suffix(reason: Option<String>) -> String {
    reason.map(|r| format!(": {r}")).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Window class + procedure
// ---------------------------------------------------------------------------

/// Register the proxy window class once per process.
pub fn register_class() -> windows::core::Result<()> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW)? };
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: CLASS_NAME,
        hCursor: cursor,
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_NCHITTEST {
            let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
            return LRESULT(super::frame::hit_test(hit.0 as u32) as isize);
        }
        // Stash the App pointer on NCCREATE, before any other message needs it.
        // Alt and F10 are the Mac's Option key and an F-key, not a way into the
        // menu bar: refuse the keyboard's menu activation (the mouse still opens
        // menus, via SC_MOUSEMENU).
        if msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) as u32 == SC_KEYMENU {
            return LRESULT(0);
        }
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let app_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if app_ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        match msg {
            WM_SIZE | WM_ENTERSIZEMOVE | WM_SIZING | WM_EXITSIZEMOVE | WM_DPICHANGED | WM_PAINT
            | WM_ACTIVATE | WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN
            | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL
            | WM_CAPTURECHANGED | WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
            | WM_CLOSE | WM_DESTROY | WM_COMMAND | WM_INITMENUPOPUP | WM_ENTERMENULOOP
            | WM_EXITMENULOOP => {
                let rect = if msg == WM_SIZING || msg == WM_DPICHANGED {
                    Some(*(lparam.0 as *const RECT))
                } else {
                    None
                };
                NATIVE_EVENTS.with(|queue| {
                    let mut q = queue.borrow_mut();
                    // Coalesce paint/size/hover before they can flood the modal tick.
                    if matches!(msg, WM_PAINT | WM_SIZE | WM_MOUSEMOVE) {
                        q.retain(|e| e.hwnd != hwnd || e.msg != msg);
                    }
                    q.push_back(NativeEvent {
                        hwnd,
                        msg,
                        wp: wparam,
                        lp: lparam,
                        rect,
                    });
                });
                // Hold the mouse for the length of a drag: without capture a
                // button released outside the window is never reported, and the
                // Mac keeps dragging. Queued first, so the release above is
                // handled before the WM_CAPTURECHANGED this sends.
                match msg {
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                        SetCapture(hwnd);
                    }
                    WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP
                        if wparam.0 & MK_BUTTONS == 0 && GetCapture() == hwnd =>
                    {
                        let _ = ReleaseCapture();
                    }
                    _ => {}
                }
                if msg == WM_PAINT {
                    let _ = windows::Win32::Graphics::Gdi::ValidateRect(hwnd, None);
                    return LRESULT(0);
                }
                if msg == WM_CLOSE || msg == WM_DPICHANGED {
                    return LRESULT(0);
                }
                if msg == WM_SIZING {
                    return LRESULT(1);
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

// ---------------------------------------------------------------------------
// Message pump
// ---------------------------------------------------------------------------

/// Run the app until all windows close (or `WM_QUIT`). Owns the `App` behind a
/// box so the raw pointer handed to each window stays valid for the whole run.
pub fn run_pump(mut app: Box<App>) {
    let app_ptr: *mut App = &mut *app;

    // Initial connect attempt; the pump keeps retrying on the backoff.
    unsafe { (*app_ptr).connect() };

    APP_POINTER.with(|p| p.set(app_ptr));
    // Thread timer dispatches inside *any* native modal loop, including the
    // dashboard, menus, and proxy movement. Wndprocs only queue owned data.
    let timer = unsafe { SetTimer(None, 0, 16, Some(modal_tick)) };
    loop {
        tick(app_ptr);

        // 2. Pump all pending Win32 messages. No Rust borrow of App is held here,
        //    so the reentrant wndproc's `*app_ptr` access is sound.
        let mut msg = MSG::default();
        let mut quit = false;
        loop {
            let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
            if !got.as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                quit = true;
                break;
            }
            // This pump owns the dashboard and every proxy. Consume the shortcut
            // before TranslateMessage/DispatchMessage so D never reaches the Mac.
            // Socket shutdown also releases the host's held modifier state.
            if input::is_disconnect_message(&msg) {
                unsafe {
                    (*app_ptr).disconnect_to_dashboard();
                }
                continue;
            }
            if unsafe { (*app_ptr).dashboard.dialog_message(&msg) } {
                continue;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if quit {
            break;
        }

        // 4. If every window has closed and we were connected, exit; otherwise
        //    wait briefly for input or the next channel poll.
        unsafe {
            if (*app_ptr).proxies.is_empty() && (*app_ptr).session.is_some() {
                // Still connected, just no windows yet — keep waiting.
            }
            // Wake on new input, or after ~8ms to re-poll the session channel.
            // A decoded frame wakes the loop at once, so it is presented now.
            let wake = [HANDLE((*app_ptr).wake.0 as _)];
            let handles = if wake[0].0.is_null() { None } else { Some(&wake[..]) };
            MsgWaitForMultipleObjectsEx(handles, 8, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }

    unsafe {
        let _ = KillTimer(None, timer);
    }
    APP_POINTER.with(|p| p.set(std::ptr::null_mut()));
    // Clean shutdown of the session's threads.
    unsafe {
        if let Some(s) = (*app_ptr).session.take() {
            s.shutdown();
        }
    }
    let _ = app; // keep the box alive until here
}

fn tick(app_ptr: *mut App) {
    if IN_TICK.with(|busy| busy.replace(true)) {
        return;
    }
    unsafe {
        for _ in 0..256 {
            let event = NATIVE_EVENTS.with(|q| q.borrow_mut().pop_front());
            let Some(event) = event else { break };
            let lp = event
                .rect
                .as_ref()
                .map(|r| LPARAM(r as *const RECT as isize))
                .unwrap_or(event.lp);
            (*app_ptr).handle_message(event.hwnd, event.msg, event.wp, lp);
        }
        (*app_ptr).poll_dashboard();
        (*app_ptr).drain_session(app_ptr);
        (*app_ptr).poll_decoder();
        (*app_ptr).flush_mouse_moves();
        (*app_ptr).render_all();
        (*app_ptr).record_presented();
    }
    IN_TICK.with(|busy| busy.set(false));
}
unsafe extern "system" fn modal_tick(_: HWND, _: u32, _: usize, _: u32) {
    let app = APP_POINTER.with(Cell::get);
    if !app.is_null() {
        tick(app);
    }
}

/// Post `WM_QUIT` (used by a future tray/quit path).
#[allow(dead_code)]
pub fn quit() {
    unsafe { PostQuitMessage(0) };
}
