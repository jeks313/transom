//! The shared D3D11 device and the 1:1 blit pipeline.
//!
//! This is where the product's whole promise is kept or lost. Every choice here
//! exists to guarantee that a source pixel maps to exactly one screen pixel
//! (invariants I-1):
//!
//!  * **`DXGI_SCALING_NONE`** on every swapchain — DXGI must never scale.
//!  * **`D3D11_FILTER_MIN_MAG_MIP_POINT`** sampler — no bilinear anywhere.
//!  * The swapchain is resized to the **exact physical client rect** on `WM_SIZE`
//!    (done in `proxy`), and the quad is drawn at native size, so there is no
//!    intermediate resample.
//!
//! One device is shared by all proxy windows; each window owns only its swapchain
//! and render-target view. The decoded frame (the whole VDS) is one shared
//! texture (`SourceTexture`); each window samples its sub-rect out of it. The
//! device is multithread protected so a hardware decoder can share it and hand
//! frames over GPU to GPU.

use std::mem::size_of;

use windows::core::{s, Interface};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_ENABLE_STRICTNESS};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_WRITE_DISCARD, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

/// Per-draw shader parameters. `#[repr(C)]` and 16-byte aligned to match the HLSL
/// cbuffer layout exactly (32 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    uv_rect: [f32; 4], // xy = uv origin, zw = uv size, into the source texture
    source_size: [u32; 2],
    mode: u32,
    _pad: u32,
}

/// What a proxy window should draw this frame.
#[derive(Clone, Copy)]
pub enum RenderMode {
    /// Sample the shared source texture at `uv_rect` (the real product path).
    Source { uv_rect: [f32; 4] },
    /// 1px checkerboard generated from physical pixel position — the M0 probe for
    /// the 1:1 guarantee (roadmap M0-client). Independent of any host.
    Checkerboard,
    /// A flat diagnostic fill, shown before the first frame arrives.
    Waiting,
}

const SHADER_HLSL: &str = r#"
cbuffer Params : register(b0) {
    float4 uvRect;
    uint2 sourceSize;
    uint mode;
    uint pad;
};
Texture2D<float> lumaTex : register(t0);
Texture2D<float2> chromaTex : register(t1);
SamplerState pointSampler : register(s0);

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut vs_main(uint vid : SV_VertexID) {
    // Fullscreen triangle: uv spans [0,1] over the window, (0,0) at top-left.
    float2 uv = float2((vid << 1) & 2, vid & 2);
    VSOut o;
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float4 ps_main(VSOut i) : SV_Target {
    if (mode == 1u) {
        uint cx = (uint)i.pos.x;
        uint cy = (uint)i.pos.y;
        float c = ((cx + cy) & 1u) ? 1.0 : 0.0;
        return float4(c, c, c, 1.0);
    }
    if (mode == 2u) {
        return float4(0.08, 0.10, 0.14, 1.0);
    }
    float2 uv = uvRect.xy + i.uv * uvRect.zw;
    uint2 p = min(uint2(uv * sourceSize), sourceSize - 1u);
    float y = lumaTex.Load(int3(p, 0)) * 255.0 - 16.0;
    float2 uv2 = chromaTex.Load(int3(p >> 1u, 0)) * 255.0 - 128.0;
    float u = uv2.x;
    float v = uv2.y;
    float b = clamp((298.0 * y + 541.0 * u + 128.0) / (256.0 * 255.0), 0.0, 1.0);
    float g = clamp((298.0 * y - 55.0 * u - 136.0 * v + 128.0) / (256.0 * 255.0), 0.0, 1.0);
    float r = clamp((298.0 * y + 459.0 * v + 128.0) / (256.0 * 255.0), 0.0, 1.0);
    return float4(r, g, b, 1.0);
}
"#;

/// The shared device and pipeline objects.
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuffer: ID3D11Buffer,
}

impl Gpu {
    pub fn new() -> windows::core::Result<Gpu> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.expect("D3D11CreateDevice returned no device");
        let context = context.expect("D3D11CreateDevice returned no context");
        // The decoder thread uses this device too.
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }

        // The DXGI factory that created the device is the one that must create its
        // swapchains.
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };

        let vs_blob = compile(SHADER_HLSL, s!("vs_main"), s!("vs_5_0"))?;
        let ps_blob = compile(SHADER_HLSL, s!("ps_main"), s!("ps_5_0"))?;

        let mut vs: Option<ID3D11VertexShader> = None;
        let mut ps: Option<ID3D11PixelShader> = None;
        unsafe {
            device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))?;
            device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))?;
        }

        // Point sampler — anything else resamples (I-1).
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler: Option<ID3D11SamplerState> = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler))? };

        let cbuffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<Params>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cbuffer: Option<ID3D11Buffer> = None;
        unsafe { device.CreateBuffer(&cbuffer_desc, None, Some(&mut cbuffer))? };

        Ok(Gpu {
            device,
            context,
            factory,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            sampler: sampler.unwrap(),
            cbuffer: cbuffer.unwrap(),
        })
    }

    /// Create a flip-model swapchain for a proxy window's HWND, sized to the
    /// window's physical client rect. `DXGI_SCALING_NONE` is the load-bearing flag.
    pub fn create_swapchain(
        &self,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> windows::core::Result<IDXGISwapChain1> {
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            // FLIP_DISCARD + SCALING_NONE: the compositor blits our buffer 1:1.
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Scaling: DXGI_SCALING_NONE,
            ..Default::default()
        };
        unsafe {
            self.factory
                .CreateSwapChainForHwnd(&self.device, hwnd, &desc, None, None)
        }
    }

    /// Bind the pipeline and draw one fullscreen triangle into `rtv` at
    /// `width × height`, in the requested mode.
    pub fn draw(
        &self,
        rtv: &ID3D11RenderTargetView,
        width: u32,
        height: u32,
        mode: RenderMode,
        source: Option<&[Option<ID3D11ShaderResourceView>; 2]>,
        source_size: [u32; 2],
    ) {
        let params = match mode {
            RenderMode::Source { uv_rect } => Params {
                uv_rect,
                source_size,
                mode: 0,
                _pad: 0,
            },
            RenderMode::Checkerboard => Params {
                uv_rect: [0.0, 0.0, 1.0, 1.0],
                source_size: [1, 1],
                mode: 1,
                _pad: 0,
            },
            RenderMode::Waiting => Params {
                uv_rect: [0.0, 0.0, 1.0, 1.0],
                source_size: [1, 1],
                mode: 2,
                _pad: 0,
            },
        };

        unsafe {
            // Upload params.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .context
                .Map(
                    &self.cbuffer,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .is_ok()
            {
                std::ptr::copy_nonoverlapping(
                    &params as *const Params as *const u8,
                    mapped.pData as *mut u8,
                    size_of::<Params>(),
                );
                self.context.Unmap(&self.cbuffer, 0);
            }

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            if let Some(planes) = source {
                self.context.PSSetShaderResources(0, Some(planes));
            }
            self.context.Draw(3, 0);
        }
    }
}

/// A shared NV12 texture holding a lane's frame, with a view of each plane.
/// Each proxy window samples its sub-rect from it and the pixel shader performs
/// BT.709 limited-range conversion. A hardware decoder fills it GPU to GPU; a
/// software one uploads 1.5 bytes/pixel instead of expanding to 4 on the CPU.
pub struct SourceTexture {
    pub width: u32,
    pub height: u32,
    /// The window's part of the frame, from the top left (v5): the stream pads
    /// a window smaller than its encoder.
    pub content: (u32, u32),
    texture: ID3D11Texture2D,
    /// Luma (R8) and chroma (R8G8) views.
    pub planes: [Option<ID3D11ShaderResourceView>; 2],
}

impl SourceTexture {
    /// Create an NV12 source texture, initialized to a neutral checkerboard so
    /// that before any video frame arrives the windows still show a sharp
    /// pattern rather than garbage.
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> windows::core::Result<SourceTexture> {
        // NV12 needs even dimensions.
        let width = (width.max(2) + 1) & !1;
        let height = (height.max(2) + 1) & !1;
        let desc = nv12_desc(width, height);

        // Seed the Y plane with a coarse checkerboard and the UV plane with neutral
        // chroma. An unfed texture is obviously a placeholder, not a decode artifact.
        let packed_height = height + height / 2;
        let mut pixels = vec![128u8; (width as usize) * (packed_height as usize)];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let on = ((x / 32) + (y / 32)) % 2 == 0;
                let v = if on { 40 } else { 24 };
                pixels[y * width as usize + x] = v;
            }
        }
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: width,
            SysMemSlicePitch: 0,
        };

        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            gpu.device
                .CreateTexture2D(&desc, Some(&init), Some(&mut texture))?
        };
        let texture = texture.unwrap();
        let planes = [
            Some(plane_view(gpu, &texture, DXGI_FORMAT_R8_UNORM)?),
            Some(plane_view(gpu, &texture, DXGI_FORMAT_R8G8_UNORM)?),
        ];

        Ok(SourceTexture {
            width,
            height,
            content: (width, height),
            texture,
            planes,
        })
    }

    /// Replace the frame with a software-decoded one: Y rows then UV rows, each
    /// `stride` bytes (the decoder may pad rows beyond the image width).
    pub fn update_nv12(&self, gpu: &Gpu, nv12: &[u8], stride: usize) {
        let needed = stride.saturating_mul(self.height as usize * 3 / 2);
        if stride < self.width as usize || nv12.len() < needed {
            return; // wrong-sized frame: skip rather than corrupt memory
        }
        unsafe {
            gpu.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                nv12.as_ptr() as *const _,
                stride as u32,
                0,
            );
        }
    }

    /// Replace the frame with a hardware-decoded one, GPU to GPU. `frame` must be
    /// an NV12 texture of the same size.
    pub fn copy_from(&self, gpu: &Gpu, frame: &ID3D11Texture2D) {
        unsafe { gpu.context.CopyResource(&self.texture, frame) };
    }

    /// Take the window's part of the newest frame; zero (or too big) is all of it.
    pub fn set_content(&mut self, (w, h): (u32, u32)) {
        self.content = if w == 0 || h == 0 {
            (self.width, self.height)
        } else {
            (w.min(self.width), h.min(self.height))
        };
    }

    /// The UV sub-rect (origin + size in [0,1]) for a window's source rect.
    pub fn uv_rect(&self, x: u32, y: u32, w: u32, h: u32) -> [f32; 4] {
        [
            x as f32 / self.width as f32,
            y as f32 / self.height as f32,
            w as f32 / self.width as f32,
            h as f32 / self.height as f32,
        ]
    }
}

/// A default-usage NV12 texture the shaders can read.
pub fn nv12_desc(width: u32, height: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    }
}

/// A view of one plane of an NV12 texture: R8 is luma, R8G8 chroma.
fn plane_view(
    gpu: &Gpu,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> windows::core::Result<ID3D11ShaderResourceView> {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    unsafe {
        gpu.device
            .CreateShaderResourceView(texture, Some(&desc), Some(&mut srv))?
    };
    Ok(srv.expect("CreateShaderResourceView returned no view"))
}

/// Compile one HLSL entry point, returning the bytecode blob or a descriptive
/// error built from the compiler's message blob.
fn compile(
    src: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            None,
            None,
            None,
            entry,
            target,
            D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    match result {
        Ok(()) => Ok(code.expect("D3DCompile succeeded but produced no blob")),
        Err(e) => Err(e),
    }
}

/// View a bytecode blob as a byte slice.
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        let ptr = blob.GetBufferPointer() as *const u8;
        let len = blob.GetBufferSize();
        std::slice::from_raw_parts(ptr, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_USAGE_STAGING,
    };

    /// An NV12 frame drawn through the real shader comes out in the right
    /// colors at the right pixels: left half red, right half blue.
    #[test]
    #[ignore = "Requires a D3D11 GPU; run on target PC"]
    fn nv12_planes_render_through_the_shader() {
        let (w, h) = (64u32, 32u32);
        let gpu = Gpu::new().unwrap();
        let source = SourceTexture::new(&gpu, w, h).unwrap();
        // BT.709 limited: red Y=63 U=102 V=240, blue Y=32 U=240 V=118.
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h {
            for x in 0..w {
                nv12[(y * w + x) as usize] = if x < w / 2 { 63 } else { 32 };
            }
        }
        for y in 0..h / 2 {
            for x in (0..w).step_by(2) {
                let (u, v) = if x < w / 2 { (102, 240) } else { (240, 118) };
                let i = (w * h + y * w + x) as usize;
                nv12[i] = u;
                nv12[i + 1] = v;
            }
        }
        source.update_nv12(&gpu, &nv12, w as usize);

        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        unsafe {
            let mut target = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut target)).unwrap();
            let target = target.unwrap();
            let mut rtv = None;
            gpu.device
                .CreateRenderTargetView(&target, None, Some(&mut rtv))
                .unwrap();
            gpu.draw(
                rtv.as_ref().unwrap(),
                w,
                h,
                RenderMode::Source {
                    uv_rect: source.uv_rect(0, 0, w, h),
                },
                Some(&source.planes),
                [source.width, source.height],
            );
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
            let staging = staging.unwrap();
            gpu.context.CopyResource(&staging, &target);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let pixel = |x: u32, y: u32| {
                let p = (mapped.pData as *const u8).add((y * mapped.RowPitch + x * 4) as usize);
                [*p, *p.add(1), *p.add(2)] // B, G, R
            };
            let (left, right) = (pixel(5, 20), pixel(w - 5, 20));
            gpu.context.Unmap(&staging, 0);
            assert!(left[2] > 230 && left[0] < 25 && left[1] < 25, "left {left:?}");
            assert!(right[0] > 230 && right[2] < 25 && right[1] < 25, "right {right:?}");
        }
    }
}
