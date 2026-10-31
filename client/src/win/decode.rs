//! HEVC decoding on an MTA worker. Convert the wire's hvcC/length-prefixed
//! samples to Annex B here, without changing the network protocol.
use crate::hevc::HevcConfig;
use crate::latency::{self, FrameStamps};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_USAGE_STAGING,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

// Three access units is enough to absorb a short decoder hiccup without turning
// the queue into visible remote-desktop latency. If we overflow, we discard the
// dependency chain and ask the host for a fresh keyframe.
const MAX_PENDING_FRAMES: usize = 3;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// A decoded frame in the format produced by the Windows HEVC decoder.
/// Keeping NV12 intact lets D3D11 do the YUV→RGB conversion in the pixel
/// shader instead of expanding every 4K frame to a CPU-owned BGRA buffer.
#[derive(Debug)]
pub struct DecodedFrame {
    /// The picture in system memory: always from a software decoder; from a
    /// hardware one only now and then, for the dashboard's previews.
    pub nv12: Vec<u8>,
    pub stride: usize,
    /// A hardware decoder's picture, on the shared device: copy it on the GPU.
    pub texture: Option<GpuTexture>,
    /// The frame's journey so far, `decoded` stamped.
    pub stamps: FrameStamps,
    /// The window's part of the picture, from its top left.
    pub content: (u32, u32),
}

#[derive(Default)]
struct PendingInput {
    frames: VecDeque<EncodedFrame>,
    bytes: usize,
    awaiting_keyframe: bool,
    stopped: bool,
}
struct EncodedFrame {
    data: Vec<u8>,
    keyframe: bool,
    pts_micros: u64,
    reset: bool,
    stamps: FrameStamps,
    content: (u32, u32),
}
impl PendingInput {
    /// Queue one compressed access unit. Returns true when the dependency chain
    /// was discarded and the caller should request a new keyframe.
    fn push(&mut self, mut frame: EncodedFrame) -> bool {
        if self.stopped {
            return false;
        }
        let mut request_keyframe = false;
        if self.frames.len() >= MAX_PENDING_FRAMES
            || self.bytes + frame.data.len() > MAX_PENDING_BYTES
        {
            self.frames.clear();
            self.bytes = 0;
            self.awaiting_keyframe = true;
            request_keyframe = true;
        }
        if frame.data.len() > MAX_PENDING_BYTES {
            self.awaiting_keyframe = true;
            return request_keyframe;
        }
        if self.awaiting_keyframe {
            if !frame.keyframe {
                return request_keyframe;
            }
            frame.reset = true;
            self.awaiting_keyframe = false;
        }
        self.bytes += frame.data.len();
        self.frames.push_back(frame);
        request_keyframe
    }
    fn pop(&mut self) -> Option<EncodedFrame> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.data.len();
        Some(frame)
    }
}

/// A texture of the shared, multithread-protected device, which any thread may
/// use.
#[derive(Debug)]
pub struct GpuTexture(pub ID3D11Texture2D);
unsafe impl Send for GpuTexture {}

/// The renderer's device, handed to the decode thread so the decoder can
/// decode on the GPU into textures the renderer can read.
pub struct GpuDevice(pub ID3D11Device);
unsafe impl Send for GpuDevice {}

/// How often a hardware decoder also reads a frame back for the previews.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);

/// Called from the decode thread whenever a new frame is ready, so the UI
/// thread presents it at once instead of on its next poll.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The submitting side of a decoder. The video thread holds a clone, so a
/// frame reaches the decoder as soon as it arrives, not when the UI thread
/// next looks.
#[derive(Clone)]
pub struct DecoderInput {
    input: Arc<(Mutex<PendingInput>, Condvar)>,
    keyframe_request: Arc<AtomicBool>,
    received: Arc<AtomicU64>,
}
impl DecoderInput {
    pub fn submit(
        &self,
        data: Vec<u8>,
        keyframe: bool,
        pts_micros: u64,
        stamps: FrameStamps,
        content: (u32, u32),
    ) {
        let n = self.received.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 3 {
            eprintln!("video: access unit {n} ({} bytes, keyframe={keyframe})", data.len());
        }
        let (lock, ready) = &*self.input;
        let dropped_chain = lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(EncodedFrame {
                data,
                keyframe,
                pts_micros,
                reset: false,
                stamps,
                content,
            });
        if dropped_chain {
            self.keyframe_request.store(true, Ordering::Release);
        }
        ready.notify_one();
    }
    /// Access units submitted so far.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

/// Preserve compressed dependencies in a bounded queue. Only decoded output
/// may safely use a latest-frame mailbox.
pub struct DecoderWorker {
    input: DecoderInput,
    output: Arc<Mutex<Option<DecodedFrame>>>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}
impl DecoderWorker {
    pub fn start(
        hvcc: Vec<u8>,
        width: u32,
        height: u32,
        wake: Wake,
        device: Option<GpuDevice>,
    ) -> std::io::Result<Self> {
        let input = Arc::new((
            Mutex::new(PendingInput {
                awaiting_keyframe: true,
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let output = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let worker_input = Arc::clone(&input);
        let worker_output = Arc::clone(&output);
        let worker_error = Arc::clone(&error);
        let keyframe_request = Arc::new(AtomicBool::new(false));
        let worker_keyframe_request = Arc::clone(&keyframe_request);
        let thread = thread::Builder::new()
            .name("transom-decode".into())
            .spawn(move || {
                let result = (|| -> Result<(), String> {
                    let _com = ComApartment::new()?;
                    let mut decoder =
                        Decoder::new(&hvcc, width, height, device.map(|d| d.0))?;
                    let mut recover = false;
                    let mut decoded = 0;
                    loop {
                        let encoded = {
                            let (lock, ready) = &*worker_input;
                            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                            while pending.frames.is_empty() && !pending.stopped {
                                pending = ready.wait(pending).unwrap_or_else(|e| e.into_inner());
                            }
                            if pending.stopped {
                                break;
                            }
                            pending.pop().expect("queue checked above")
                        };
                        if recover && !encoded.keyframe {
                            continue;
                        }
                        if encoded.reset || recover {
                            eprintln!(
                                "video: resuming decode at keyframe ({} bytes)",
                                encoded.data.len()
                            );
                            decoder
                                .flush()
                                .map_err(|e| format!("HEVC decoder flush: {e}"))?;
                            recover = false;
                        }
                        match decoder.decode(&encoded.data, encoded.keyframe, encoded.pts_micros) {
                            Ok(Some(mut frame)) => {
                                frame.stamps = FrameStamps {
                                    decoded: latency::now_micros(),
                                    ..encoded.stamps
                                };
                                frame.content = encoded.content;
                                decoded += 1;
                                if decoded == 1 {
                                    eprintln!(
                                        "video: first decoded NV12 frame ({} bytes)",
                                        frame.nv12.len()
                                    );
                                }
                                *worker_output.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(frame);
                                wake();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("video: {e}");
                                worker_keyframe_request.store(true, Ordering::Release);
                                *worker_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                                recover = true;
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    eprintln!("video: {e}");
                    *worker_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
                let mut pending = worker_input.0.lock().unwrap_or_else(|e| e.into_inner());
                pending.stopped = true;
                pending.frames.clear();
                pending.bytes = 0;
            })?;
        Ok(Self {
            input: DecoderInput {
                input,
                keyframe_request,
                received: Arc::new(AtomicU64::new(0)),
            },
            output,
            error,
            thread: Some(thread),
        })
    }
    pub fn input(&self) -> &DecoderInput {
        &self.input
    }
    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn take_keyframe_request(&self) -> bool {
        self.input.keyframe_request.swap(false, Ordering::AcqRel)
    }
}
impl Drop for DecoderWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.input.input;
        {
            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            pending.stopped = true;
            pending.frames.clear();
            ready.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ComApartment;
impl ComApartment {
    fn new() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .map_err(|e| format!("Decoder COM initialization: {e}"))?;
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
struct MediaFoundation;
impl MediaFoundation {
    fn new() -> Result<Self, String> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }
            .map_err(|e| format!("Media Foundation initialization: {e}"))?;
        Ok(Self)
    }
}
impl Drop for MediaFoundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

struct Decoder {
    // Release the transform before shutting down Media Foundation.
    transform: IMFTransform,
    /// Set when the decoder decodes on the GPU.
    hardware: Option<Hardware>,
    _runtime: MediaFoundation,
    config: HevcConfig,
    width: u32,
    height: u32,
    /// Rows the decoder's pictures have: the height, padded to its block size.
    coded_height: u32,
    stride: usize,
    provides_samples: bool,
    out_size: u32,
    out_alignment: u32,
}

/// A GPU decoder's output: each picture is copied into `out`, which the
/// renderer copies from, and now and then into `staging` for a preview.
struct Hardware {
    _manager: IMFDXGIDeviceManager,
    context: ID3D11DeviceContext,
    out: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    staging_pending: bool,
    last_preview: Instant,
}

impl Hardware {
    /// Give `transform` the device, if it decodes on one.
    unsafe fn attach(
        transform: &IMFTransform,
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> windows::core::Result<Hardware> {
        let aware = transform
            .GetAttributes()
            .and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE))
            .unwrap_or(0);
        if aware == 0 {
            return Err(windows::core::Error::new(
                MF_E_UNSUPPORTED_D3D_TYPE,
                "decoder is not Direct3D 11 aware",
            ));
        }
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.expect("MFCreateDXGIDeviceManager returned no manager");
        manager.ResetDevice(device, token)?;
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        let mut desc = super::gpu::nv12_desc(width, height);
        let mut out = None;
        device.CreateTexture2D(&desc, None, Some(&mut out))?;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        Ok(Hardware {
            _manager: manager,
            context: device.GetImmediateContext()?,
            out: out.expect("CreateTexture2D returned no texture"),
            staging: staging.expect("CreateTexture2D returned no texture"),
            staging_pending: false,
            last_preview: Instant::now() - PREVIEW_INTERVAL,
        })
    }

    /// Copy a decoded sample into `out`, and pick up a finished preview copy.
    unsafe fn take(
        &mut self,
        sample: &IMFSample,
        width: u32,
        height: u32,
    ) -> Result<DecodedFrame, String> {
        let buffer = sample.GetBufferByIndex(0).map_err(|e| e.to_string())?;
        let dxgi: IMFDXGIBuffer = buffer.cast().map_err(|e| e.to_string())?;
        let mut raw = std::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
            .map_err(|e| format!("Decoded surface: {e}"))?;
        let surface = ID3D11Texture2D::from_raw(raw);
        let index = dxgi.GetSubresourceIndex().map_err(|e| e.to_string())?;
        // The decoder's surfaces can be padded past the picture.
        let picture = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        self.context
            .CopySubresourceRegion(&self.out, 0, 0, 0, 0, &surface, index, Some(&picture));

        let mut frame = DecodedFrame {
            nv12: Vec::new(),
            stride: 0,
            texture: Some(GpuTexture(self.out.clone())),
            stamps: FrameStamps::default(),
            content: (width, height),
        };
        if self.staging_pending {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .context
                .Map(
                    &self.staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
                .is_ok()
            {
                let pitch = mapped.RowPitch as usize;
                let len = pitch * (height as usize * 3 / 2);
                frame.nv12 = std::slice::from_raw_parts(mapped.pData as *const u8, len).to_vec();
                frame.stride = pitch;
                self.context.Unmap(&self.staging, 0);
                self.staging_pending = false;
            }
        } else if self.last_preview.elapsed() >= PREVIEW_INTERVAL {
            self.context.CopyResource(&self.staging, &self.out);
            self.staging_pending = true;
            self.last_preview = Instant::now();
        }
        Ok(frame)
    }
}
impl Decoder {
    fn new(
        hvcc: &[u8],
        width: u32,
        height: u32,
        device: Option<ID3D11Device>,
    ) -> Result<Self, String> {
        let config = HevcConfig::parse(hvcc)?;
        if config.chroma != 1 || config.bit_depth != 8 {
            return Err(format!("Host sent HEVC chroma {} / {}-bit. Select 4:2:0 8-bit in Mac Video settings and restart sharing.", config.chroma, config.bit_depth));
        }
        let runtime = MediaFoundation::new()?;
        let transforms =
            decoder_candidates().map_err(|e| format!("Find Windows HEVC decoder: {e}"))?;
        if transforms.is_empty() {
            return Err(
                "Windows HEVC decoder is missing. Install HEVC Video Extensions, then reconnect."
                    .into(),
            );
        }
        let mut failures = Vec::new();
        for candidate in transforms {
            let configured = unsafe {
                (|| -> Result<(IMFTransform, Option<Hardware>), String> {
                    let transform: IMFTransform = candidate
                        .ActivateObject()
                        .map_err(|e| format!("Activate HEVC decoder: {e}"))?;
                    // Before the media types: a decoder picks its output
                    // memory when its types are set.
                    let hardware = device.as_ref().and_then(|d| {
                        Hardware::attach(&transform, d, width, height)
                            .map_err(|e| eprintln!("video: decoding on the CPU: {e}"))
                            .ok()
                    });
                    if let Ok(attrs) = transform.GetAttributes() {
                        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                    }
                    if let Ok(codec) = transform.cast::<ICodecAPI>() {
                        let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &true.into());
                    }
                    let input = MFCreateMediaType().map_err(|e| e.to_string())?;
                    input
                        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                        .map_err(|e| e.to_string())?;
                    input
                        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
                        .map_err(|e| e.to_string())?;
                    input
                        .SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
                        .map_err(|e| e.to_string())?;
                    input
                        .SetUINT32(&MF_MT_MPEG2_PROFILE, config.profile)
                        .map_err(|e| e.to_string())?;
                    input
                        .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &config.parameter_sets)
                        .map_err(|e| e.to_string())?;
                    transform
                        .SetInputType(0, &input, 0)
                        .map_err(|e| format!("Set HEVC input type: {e}"))?;
                    Ok((transform, hardware))
                })()
            };
            match configured {
                Ok((transform, hardware)) => {
                    let mut decoder = Self {
                        transform,
                        hardware,
                        _runtime: runtime,
                        config,
                        width,
                        height,
                        coded_height: height,
                        stride: width as usize,
                        provides_samples: false,
                        out_size: 0,
                        out_alignment: 0,
                    };
                    unsafe {
                        decoder
                            .reset_output_type()
                            .map_err(|e| format!("Set HEVC output type: {e}"))?;
                        decoder
                            .transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                            .map_err(|e| e.to_string())?;
                        decoder
                            .transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                            .map_err(|e| e.to_string())?;
                    }
                    eprintln!(
                        "video: HEVC decoder ready, {}x{}, Main 4:2:0 8-bit, Annex B, on the {}",
                        width,
                        height,
                        if decoder.hardware.is_some() { "GPU" } else { "CPU" }
                    );
                    return Ok(decoder);
                }
                Err(e) => failures.push(e),
            }
        }
        Err(failures.join("; "))
    }
    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_micros: u64,
    ) -> Result<Option<DecodedFrame>, String> {
        let bytes = self.config.annex_b(au, keyframe)?;
        unsafe {
            let sample = make_input_sample(&bytes, keyframe, pts_micros)
                .map_err(|e| format!("Create HEVC input sample: {e}"))?;
            let mut latest = None;
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    latest = self.drain()?;
                    // NOTACCEPTING did not consume this input. Retry the same
                    // sample after draining to preserve compressed dependencies.
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(|e| format!("Retry HEVC input: {e}"))?;
                }
                Err(e) => return Err(format!("Decode HEVC input: {e}")),
            }
            Ok(self.drain()?.or(latest))
        }
    }
    fn flush(&mut self) -> windows::core::Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }
    unsafe fn drain(&mut self) -> Result<Option<DecodedFrame>, String> {
        let mut latest = None;
        // Bound repeated STREAM_CHANGE responses without progress.
        for _ in 0..64 {
            let out_sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample().map_err(|e| e.to_string())?;
                let buffer = MFCreateAlignedMemoryBuffer(self.out_size.max(1), self.out_alignment)
                    .map_err(|e| e.to_string())?;
                sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
                Some(sample)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(out_sample),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            // These COM outputs are owned on every path, including errors.
            let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        latest = Some(match self.hardware.as_mut() {
                            Some(hw) => hw.take(&sample, self.width, self.height)?,
                            None => self.sample_to_nv12(&sample)?,
                        });
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(latest),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.reset_output_type()
                        .map_err(|e| format!("HEVC output format changed: {e}"))?;
                }
                Err(e) => return Err(format!("Decode HEVC output: {e}")),
            }
        }
        Err("HEVC decoder made no progress while draining output".into())
    }
    unsafe fn reset_output_type(&mut self) -> windows::core::Result<()> {
        let mut index = 0;
        loop {
            let output = self.transform.GetOutputAvailableType(0, index)?;
            index += 1;
            if output.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                continue;
            }
            let size = output
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .unwrap_or(pack_size(self.width, self.height));
            // A window's size need not be a multiple of the codec's blocks; the
            // decoder pads its pictures past it, and the padding is cropped off.
            let (coded_w, coded_h) = ((size >> 32) as u32, size as u32);
            if !padded_by_block(coded_w, self.width) || !padded_by_block(coded_h, self.height) {
                return Err(windows::core::Error::new(
                    MF_E_INVALIDMEDIATYPE,
                    "Decoded size differs from host display; refusing to scale",
                ));
            }
            self.coded_height = coded_h;
            output.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            self.transform.SetOutputType(0, &output, 0)?;
            self.stride = output
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .unwrap_or(self.width) as usize;
            let info = self.transform.GetOutputStreamInfo(0)?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.out_size = info.cbSize;
            self.out_alignment = info.cbAlignment.saturating_sub(1);
            return Ok(());
        }
    }
    unsafe fn sample_to_nv12(&self, sample: &IMFSample) -> Result<DecodedFrame, String> {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| e.to_string())?;
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let mut packed =
                vec![0; two_d.GetContiguousLength().map_err(|e| e.to_string())? as usize];
            two_d
                .ContiguousCopyTo(&mut packed)
                .map_err(|e| e.to_string())?;
            return validate_nv12(packed, self.width, self.height, self.coded_height, self.stride);
        }
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| e.to_string())?;
        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let unlock = buffer.Unlock();
        unlock.map_err(|e| e.to_string())?;
        validate_nv12(bytes, self.width, self.height, self.coded_height, self.stride)
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
    }
}
fn decoder_candidates() -> windows::core::Result<Vec<IMFActivate>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_HEVC,
        };
        let mut array = std::ptr::null_mut();
        let mut count = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut array,
            &mut count,
        )?;
        let mut candidates = Vec::new();
        if !array.is_null() {
            for entry in std::slice::from_raw_parts_mut(array, count as usize) {
                if let Some(activate) = entry.take() {
                    candidates.push(activate);
                }
            }
            CoTaskMemFree(Some(array.cast()));
        }
        Ok(candidates)
    }
}
unsafe fn make_input_sample(
    bytes: &[u8],
    keyframe: bool,
    pts_micros: u64,
) -> windows::core::Result<IMFSample> {
    let sample = MFCreateSample()?;
    let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
    let mut ptr = std::ptr::null_mut();
    buffer.Lock(&mut ptr, None, None)?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    buffer.Unlock()?;
    buffer.SetCurrentLength(bytes.len() as u32)?;
    sample.AddBuffer(&buffer)?;
    sample.SetSampleTime(pts_micros.saturating_mul(10).min(i64::MAX as u64) as i64)?;
    sample.SetUINT32(&MFSampleExtension_CleanPoint, u32::from(keyframe))?;
    Ok(sample)
}
fn pack_size(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

/// Whether a decoder's picture side `coded` is `side` padded up to a block.
fn padded_by_block(coded: u32, side: u32) -> bool {
    coded >= side && coded - side < 64
}

/// Check a CPU decoder's NV12 picture and drop the rows it padded past
/// `height`: its chroma plane starts after `coded_height` rows.
fn validate_nv12(
    nv12: Vec<u8>,
    width: u32,
    height: u32,
    coded_height: u32,
    stride: usize,
) -> Result<DecodedFrame, String> {
    let needed = stride
        .checked_mul(coded_height as usize)
        .and_then(|y| y.checked_add(stride.checked_mul(coded_height as usize / 2)?))
        .ok_or_else(|| "Invalid NV12 output dimensions".to_string())?;
    if width == 0
        || height == 0
        || width % 2 != 0
        || height % 2 != 0
        || coded_height < height
        || stride < width as usize
    {
        return Err("Invalid NV12 output dimensions or stride".into());
    }
    if nv12.len() < needed {
        return Err("Invalid NV12 output buffer size".into());
    }
    let nv12 = if coded_height == height {
        nv12
    } else {
        let luma = stride * height as usize;
        let chroma = stride * coded_height as usize;
        let mut packed = Vec::with_capacity(luma + luma / 2);
        packed.extend_from_slice(&nv12[..luma]);
        packed.extend_from_slice(&nv12[chroma..chroma + luma / 2]);
        packed
    };
    Ok(DecodedFrame {
        nv12,
        stride,
        texture: None,
        stamps: FrameStamps::default(),
        content: (0, 0),
    })
}

/// BT.709 limited-range NV12, respecting padded rows without resizing.
#[cfg(test)]
fn nv12_to_bgra(nv12: &[u8], width: u32, height: u32, stride: usize) -> Option<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || stride < w {
        return None;
    }
    let y_size = stride.checked_mul(h)?;
    let needed = y_size.checked_add(stride.checked_mul(h / 2)?)?;
    if nv12.len() < needed {
        return None;
    }
    let (y_plane, uv_plane) = nv12.split_at(y_size);
    let mut bgra = vec![0; w.checked_mul(h)?.checked_mul(4)?];
    for row in 0..h {
        for col in 0..w {
            let y = i32::from(y_plane[row * stride + col]) - 16;
            let uv = (row / 2) * stride + (col / 2) * 2;
            let u = i32::from(uv_plane[uv]) - 128;
            let v = i32::from(uv_plane[uv + 1]) - 128;
            let o = (row * w + col) * 4;
            bgra[o] = ((298 * y + 541 * u + 128) >> 8).clamp(0, 255) as u8;
            bgra[o + 1] = ((298 * y - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u8;
            bgra[o + 2] = ((298 * y + 459 * v + 128) >> 8).clamp(0, 255) as u8;
            bgra[o + 3] = 255;
        }
    }
    Some(bgra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_reports_invalid_config_without_waiting_for_video_frames() {
        let worker = DecoderWorker::start(vec![1, 2], 128, 96, Arc::new(|| {}), None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(error) = worker.take_error() {
                assert!(error.contains("HEVC configuration"), "{error}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker failed silently"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[test]
    fn nv12_from_a_block_padded_picture_drops_the_padding_rows() {
        // 2x2 picture decoded as 2x4: Y rows 1,2 then padding 9,9; UV row 5,6 then 9,9.
        let coded = vec![1, 1, 2, 2, 9, 9, 9, 9, 5, 6, 9, 9];
        let frame = validate_nv12(coded, 2, 2, 4, 2).unwrap();
        assert_eq!(frame.nv12, vec![1, 1, 2, 2, 5, 6]);
        assert!(padded_by_block(1424, 1420));
        assert!(padded_by_block(1420, 1420));
        assert!(!padded_by_block(1418, 1420));
        assert!(!padded_by_block(2880, 1420));
    }
    #[test]
    fn nv12_handles_padding_and_rejects_short_or_odd_frames() {
        let packed = [16, 235, 16, 235, 128, 128];
        let padded = [16, 235, 99, 99, 16, 235, 99, 99, 128, 128, 99, 99];
        let expected = vec![
            0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
        ];
        assert_eq!(nv12_to_bgra(&packed, 2, 2, 2), Some(expected.clone()));
        assert_eq!(nv12_to_bgra(&padded, 2, 2, 4), Some(expected));
        assert!(nv12_to_bgra(&packed[..5], 2, 2, 2).is_none());
        assert!(nv12_to_bgra(&packed, 1, 2, 2).is_none());
    }
    fn encoded(n: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            data: vec![n as u8],
            keyframe,
            pts_micros: n,
            reset: false,
            stamps: FrameStamps::default(),
            content: (0, 0),
        }
    }
    #[test]
    fn compressed_frames_keep_order_and_recover_at_keyframe_after_overflow() {
        let mut queue = PendingInput {
            awaiting_keyframe: true,
            ..Default::default()
        };
        queue.push(encoded(0, false));
        assert!(queue.pop().is_none());
        queue.push(encoded(1, true));
        queue.push(encoded(2, false));
        assert!(queue.pop().unwrap().reset);
        assert_eq!(queue.pop().unwrap().pts_micros, 2);
        for n in 0..=MAX_PENDING_FRAMES {
            queue.push(encoded(n as u64, false));
        }
        assert!(queue.pop().is_none());
        queue.push(encoded(10, false));
        assert!(queue.pop().is_none());
        queue.push(encoded(11, true));
        assert!(queue.pop().unwrap().reset);
    }
    /// The fixture was recorded in v1 framing: no stream after the tag, no size
    /// in a config, and no stage stamps in a frame.
    fn fixture_to_v4(p: Vec<u8>) -> Vec<u8> {
        let mut out = vec![p[0]];
        out.extend_from_slice(&1u64.to_be_bytes());
        if p[0] == 0x02 {
            out.extend_from_slice(&p[1..18]);
            out.extend_from_slice(&[0; 24]);
            out.extend_from_slice(&128u32.to_be_bytes());
            out.extend_from_slice(&96u32.to_be_bytes());
            out.extend_from_slice(&p[18..]);
        } else {
            out.extend_from_slice(&128u32.to_be_bytes());
            out.extend_from_slice(&96u32.to_be_bytes());
            out.extend_from_slice(&p[1..]);
        }
        out
    }
    #[test]
    #[ignore = "Requires an installed Windows HEVC decoder; run on target PC"]
    fn decodes_hevc_fixture_on_windows() {
        use crate::net::FramedReceiver;
        use crate::wire::VideoMessage;
        let _com = ComApartment::new().unwrap();
        let data = include_bytes!("../../tests/fixtures/hevc-main.wire");
        let mut wire = FramedReceiver::new(&data[..]);
        let v4 = fixture_to_v4;
        let VideoMessage::Config { hvcc, .. } =
            VideoMessage::decode(&v4(wire.recv().unwrap().unwrap())).unwrap()
        else {
            panic!("config expected")
        };
        let mut decoder = Decoder::new(&hvcc, 128, 96, None).unwrap();
        let mut count = 0;
        while let Some(payload) = wire.recv().unwrap() {
            let VideoMessage::Frame {
                data,
                keyframe,
                pts_micros,
                ..
            } = VideoMessage::decode(&v4(payload)).unwrap()
            else {
                panic!("frame expected")
            };
            if let Some(frame) = decoder.decode(&data, keyframe, pts_micros).unwrap() {
                assert!(frame.nv12.len() >= 128 * 96 * 3 / 2);
                let bgra = nv12_to_bgra(&frame.nv12, 128, 96, frame.stride).unwrap();
                assert!(
                    bgra.chunks_exact(4).any(|p| p[0].abs_diff(p[2]) > 100),
                    "test colors must survive decode"
                );
                count += 1;
            }
        }
        eprintln!("Media Foundation decoded {count} real HEVC frames at 128x96");
        assert!(count >= 8, "expected most frames, got {count}");
    }
    #[test]
    #[ignore = "Requires a GPU HEVC decoder; run on target PC"]
    fn decodes_hevc_fixture_on_the_gpu() {
        use crate::net::FramedReceiver;
        use crate::wire::VideoMessage;
        let _com = ComApartment::new().unwrap();
        let gpu = super::super::gpu::Gpu::new().unwrap();
        let data = include_bytes!("../../tests/fixtures/hevc-main.wire");
        let mut wire = FramedReceiver::new(&data[..]);
        let v4 = fixture_to_v4;
        let VideoMessage::Config { hvcc, .. } =
            VideoMessage::decode(&v4(wire.recv().unwrap().unwrap())).unwrap()
        else {
            panic!("config expected")
        };
        let mut decoder = Decoder::new(&hvcc, 128, 96, Some(gpu.device.clone())).unwrap();
        assert!(decoder.hardware.is_some(), "decoder did not take the device");
        let mut count = 0;
        let mut inputs = 0;
        while let Some(payload) = wire.recv().unwrap() {
            let VideoMessage::Frame {
                data,
                keyframe,
                pts_micros,
                ..
            } = VideoMessage::decode(&v4(payload)).unwrap()
            else {
                panic!("frame expected")
            };
            inputs += 1;
            if let Some(frame) = decoder.decode(&data, keyframe, pts_micros).unwrap() {
                assert!(frame.texture.is_some());
                count += 1;
            }
            // No frame is held back: a single change must show at once.
            assert_eq!(count, inputs, "decoder held a frame back");
        }
        // Read the last picture back, waiting this time.
        let hw = decoder.hardware.as_ref().unwrap();
        unsafe {
            hw.context.CopyResource(&hw.staging, &hw.out);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            hw.context
                .Map(&hw.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let pitch = mapped.RowPitch as usize;
            let nv12 =
                std::slice::from_raw_parts(mapped.pData as *const u8, pitch * 96 * 3 / 2).to_vec();
            hw.context.Unmap(&hw.staging, 0);
            let bgra = nv12_to_bgra(&nv12, 128, 96, pitch).unwrap();
            assert!(
                bgra.chunks_exact(4).any(|p| p[0].abs_diff(p[2]) > 100),
                "test colors must survive decode"
            );
        }
        eprintln!("GPU decoded {count} real HEVC frames at 128x96");
    }
}
