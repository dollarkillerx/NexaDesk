//! D3D11 presenter: NV12 in memory to a flip-model swap chain on the view's HWND.
//!
//! The planes go to two dynamic textures (R8 and R8G8), and a pixel shader turns
//! BT.709 video-range YUV into RGB, letterboxed with [`crate::layout::fit_viewport`].
//! Created on the UI thread (`attach`), then used only from the render thread; the
//! device is free-threaded and its immediate context is touched by one thread at a time
//! (every method takes `&mut self`).

use std::time::Instant;

use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
                D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob,
            },
            Direct3D11::{
                D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE, D3D11_SAMPLER_DESC,
                D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11Device,
                ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
                ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
            },
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_UNKNOWN,
                    DXGI_SAMPLE_DESC,
                },
                DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
                DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2,
                IDXGISwapChain1,
            },
        },
    },
    core::{Interface, PCSTR, s},
};

use crate::{
    DecodedFrame, MediaError, NativeImage, PresentMode, PresentStat, Presenter, Result,
    layout::fit_viewport,
};

fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> MediaError {
    move |e| MediaError::Os {
        what,
        status: e.code().0,
    }
}

/// A full-view triangle, and BT.709 video range to RGB.
const SHADERS: &str = r"
Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
SamplerState linear_clamp : register(s0);
struct Out { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
Out vs(uint id : SV_VertexID) {
    Out o;
    float2 t = float2((id << 1) & 2, id & 2);
    o.uv = t;
    o.pos = float4(t * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
float4 ps(Out i) : SV_Target {
    float y = (luma.Sample(linear_clamp, i.uv) - 16.0 / 255.0) * (255.0 / 219.0);
    float2 c = (chroma.Sample(linear_clamp, i.uv) - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    return float4(saturate(rgb), 1);
}
";

struct Planes {
    size: (u32, u32),
    luma: ID3D11Texture2D,
    chroma: ID3D11Texture2D,
    views: [Option<ID3D11ShaderResourceView>; 2],
}

pub struct D3dPresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    planes: Option<Planes>,
    size: (u32, u32),
    mode: PresentMode,
}

// SAFETY: see the module comment: one thread at a time, through `&mut self`.
unsafe impl Send for D3dPresenter {}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source is a valid buffer of its length; the out pointers are locals.
    let result = unsafe {
        D3DCompile(
            SHADERS.as_ptr().cast(),
            SHADERS.len(),
            s!("nexadesk"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors
            .map(|b| {
                // SAFETY: the error blob holds that many bytes of text.
                let bytes = unsafe {
                    std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
                };
                String::from_utf8_lossy(bytes).into_owned()
            })
            .unwrap_or_default();
        return Err(MediaError::Other(format!("shader: {e} {detail}")));
    }
    code.ok_or(MediaError::Other("shader: no code".into()))
}

fn blob(b: &ID3DBlob) -> &[u8] {
    // SAFETY: a compiled blob holds that many bytes.
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }
}

fn device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let (mut device, mut context) = (None, None);
        // SAFETY: documented arguments; the out pointers are locals.
        let made = unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                driver,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if made.is_ok()
            && let (Some(d), Some(c)) = (device, context)
        {
            return Ok((d, c));
        }
    }
    Err(MediaError::Other("no Direct3D 11 device".into()))
}

impl D3dPresenter {
    /// # Safety
    /// `hwnd` must be a live window that outlives the presenter.
    pub unsafe fn attach(hwnd: *mut std::ffi::c_void, width: u32, height: u32) -> Result<Self> {
        let hwnd = HWND(hwnd);
        let (device, context) = device()?;
        let size = (width.max(1), height.max(1));
        // SAFETY: the factory that made the device's adapter; a swap chain for a live
        // window; shaders compiled from the source above.
        unsafe {
            let factory: IDXGIFactory2 = device
                .cast::<IDXGIDevice>()
                .map_err(os("IDXGIDevice"))?
                .GetAdapter()
                .map_err(os("GetAdapter"))?
                .GetParent()
                .map_err(os("IDXGIFactory2"))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                ..Default::default()
            };
            let swap = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
                .map_err(os("CreateSwapChainForHwnd"))?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
            let ps_code = compile(s!("ps"), s!("ps_4_0"))?;
            let mut vs = None;
            device
                .CreateVertexShader(blob(&vs_code), None, Some(&mut vs))
                .map_err(os("CreateVertexShader"))?;
            let mut ps = None;
            device
                .CreatePixelShader(blob(&ps_code), None, Some(&mut ps))
                .map_err(os("CreatePixelShader"))?;
            let mut sampler = None;
            device
                .CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut sampler),
                )
                .map_err(os("CreateSamplerState"))?;
            let missing = || MediaError::Other("Direct3D returned nothing".into());
            Ok(Self {
                device,
                context,
                swap,
                vs: vs.ok_or_else(missing)?,
                ps: ps.ok_or_else(missing)?,
                sampler: sampler.ok_or_else(missing)?,
                planes: None,
                size,
                mode: PresentMode::Vsync,
            })
        }
    }

    fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width.max(1),
            Height: height.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let (mut texture, mut view) = (None, None);
        // SAFETY: a valid description; the out pointers are locals.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(os("CreateTexture2D"))?;
            let texture = texture.ok_or(MediaError::Other("no texture".into()))?;
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
                .map_err(os("CreateShaderResourceView"))?;
            Ok((texture, view.ok_or(MediaError::Other("no view".into()))?))
        }
    }

    fn planes(&mut self, width: u32, height: u32) -> Result<()> {
        if self
            .planes
            .as_ref()
            .is_some_and(|p| p.size == (width, height))
        {
            return Ok(());
        }
        let (luma, luma_view) = self.texture(width, height, DXGI_FORMAT_R8_UNORM)?;
        let (chroma, chroma_view) = self.texture(width / 2, height / 2, DXGI_FORMAT_R8G8_UNORM)?;
        self.planes = Some(Planes {
            size: (width, height),
            luma,
            chroma,
            views: [Some(luma_view), Some(chroma_view)],
        });
        Ok(())
    }

    /// Copies `rows` rows of `row_bytes` from `src` into a mapped dynamic texture.
    fn upload(
        &self,
        texture: &ID3D11Texture2D,
        src: &[u8],
        row_bytes: usize,
        rows: usize,
    ) -> Result<()> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a dynamic texture of at least `rows` rows of `row_bytes`, mapped for
        // writing and unmapped after; rows are copied within the reported pitch.
        unsafe {
            self.context
                .Map(texture, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(os("Map"))?;
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData.cast::<u8>();
            for row in 0..rows {
                let from = &src[row * row_bytes..(row + 1) * row_bytes];
                std::ptr::copy_nonoverlapping(from.as_ptr(), dst.add(row * pitch), row_bytes);
            }
            self.context.Unmap(texture, 0);
        }
        Ok(())
    }
}

impl Presenter for D3dPresenter {
    fn resize(&mut self, width_px: u32, height_px: u32, _scale: f32) -> Result<()> {
        let size = (width_px.max(1), height_px.max(1));
        if size == self.size {
            return Ok(());
        }
        // SAFETY: nothing holds the back buffers between presents (the render target
        // view is made per frame), so they can be resized.
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.context.Flush();
            self.swap
                .ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(os("ResizeBuffers"))?;
        }
        self.size = size;
        Ok(())
    }

    fn set_mode(&mut self, mode: PresentMode) -> Result<()> {
        self.mode = mode;
        Ok(())
    }

    fn present(&mut self, frame: &DecodedFrame) -> Result<PresentStat> {
        let NativeImage::Cpu(f) = &frame.image else {
            return Err(MediaError::Unsupported(
                "the presenter shows decoded frames in memory",
            ));
        };
        let (w, h) = (f.width & !1, f.height & !1);
        if w == 0 || h == 0 || f.y.len() < (w * h) as usize || f.uv.len() < (w * h / 2) as usize {
            return Err(MediaError::Other("empty or short frame".into()));
        }
        self.planes(w, h)?;
        let planes = self.planes.as_ref().expect("made above");
        self.upload(&planes.luma, &f.y, f.width as usize, h as usize)?;
        self.upload(&planes.chroma, &f.uv, f.width as usize, h as usize / 2)?;
        let started = Instant::now();
        // SAFETY: the back buffer and a view of it for this frame only; the shaders,
        // sampler and plane views made by this presenter.
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0).map_err(os("GetBuffer"))?;
            let mut target: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&back, None, Some(&mut target))
                .map_err(os("CreateRenderTargetView"))?;
            let target = target.ok_or(MediaError::Other("no render target".into()))?;
            self.context
                .ClearRenderTargetView(&target, &[0.0, 0.0, 0.0, 1.0]);
            let (x, y, vw, vh) = fit_viewport(self.size, (w, h));
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: x as f32,
                TopLeftY: y as f32,
                Width: vw as f32,
                Height: vh as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            self.context
                .OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            self.context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(None);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context.PSSetShaderResources(0, Some(&planes.views));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.Draw(3, 0);
            self.context.OMSetRenderTargets(None, None);
            let sync = if self.mode == PresentMode::Immediate {
                0
            } else {
                1
            };
            self.swap
                .Present(sync, DXGI_PRESENT(0))
                .ok()
                .map_err(os("Present"))?;
        }
        Ok(PresentStat {
            present_call_us: started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32,
        })
    }
}
