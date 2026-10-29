//! 自前の Windows Graphics Capture（D3D11テクスチャ取得）
//!
//! OBS の win-capture と同じく、WGC のフレームを GPU テクスチャのまま受け取る。
//! M1 ではテクスチャ取得と検証ログのみを担当し、エンコード統合は次の段階で行う。

use windows::core::{factory, Interface};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL};
use windows::core::BOOL;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

pub struct GpuFrame {
    pub texture: ID3D11Texture2D,
    pub width: i32,
    pub height: i32,
}

struct Processor {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: ID3D11Texture2D,
    width: u32,
    height: u32,
}

pub struct WgcGpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    width: i32,
    height: i32,
    frames: u64,
    attempts: u64,
    processor: Option<Processor>,
}

fn init_winrt() {
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
}

fn create_device() -> Option<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = Default::default();
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .ok()?;
        let device = device?;
        let context = context?;
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi).ok()?;
        let winrt_device: IDirect3DDevice = inspectable.cast().ok()?;
        Some((device, context, winrt_device))
    }
}

impl WgcGpu {
    pub fn for_monitor(hmon: isize) -> Option<Self> {
        init_winrt();
        unsafe {
            let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ok()?;
            let item: GraphicsCaptureItem = interop
                .CreateForMonitor(windows::Win32::Graphics::Gdi::HMONITOR(hmon as *mut core::ffi::c_void))
                .ok()?;
            Self::start(item, None)
        }
    }

    pub fn for_window(hwnd: isize) -> Option<Self> {
        init_winrt();
        unsafe {
            let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ok()?;
            let item: GraphicsCaptureItem = interop
                .CreateForWindow(windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void))
                .ok()?;
            Self::start(item, Some(hwnd))
        }
    }

    fn start(item: GraphicsCaptureItem, _hwnd: Option<isize>) -> Option<Self> {
        unsafe {
            let (device, context, winrt_device) = create_device()?;
            let size = item.Size().ok()?;
            if size.Width <= 0 || size.Height <= 0 {
                return None;
            }
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )
            .ok()?;
            let session = pool.CreateCaptureSession(&item).ok()?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            if let Err(e) = session.StartCapture() {
                crate::log(&format!("wgc-gpu: StartCapture failed: {e}"));
                return None;
            }
            Some(Self {
                device,
                context,
                pool,
                session,
                _item: item,
                width: size.Width,
                height: size.Height,
                frames: 0,
                attempts: 0,
                processor: None,
            })
        }
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub fn try_next(&mut self) -> Option<GpuFrame> {
        unsafe {
            self.attempts += 1;
            let frame = match self.pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(e) => {
                    // フレーム未到着は S_OK + null で返るため、正常扱いでスキップする
                    if e.code() != windows::core::HRESULT(0) && self.attempts % 60 == 1 {
                        crate::log(&format!("wgc-gpu: TryGetNextFrame err: {e}"));
                    }
                    return None;
                }
            };
            let surface = match frame.Surface() {
                Ok(s) => s,
                Err(e) => {
                    crate::log(&format!("wgc-gpu: Surface err: {e}"));
                    return None;
                }
            };
            let access: IDirect3DDxgiInterfaceAccess = match surface.cast() {
                Ok(a) => a,
                Err(e) => {
                    crate::log(&format!("wgc-gpu: interface access err: {e}"));
                    return None;
                }
            };
            let texture: ID3D11Texture2D = match access.GetInterface() {
                Ok(t) => t,
                Err(e) => {
                    crate::log(&format!("wgc-gpu: GetInterface err: {e}"));
                    return None;
                }
            };
            self.frames += 1;
            Some(GpuFrame {
                texture,
                width: self.width,
                height: self.height,
            })
        }
    }

    /// 検証用: テクスチャの平均輝度（BGRAの平均）
    pub fn mean_luma(&mut self, frame: &GpuFrame) -> Option<f64> {
        unsafe {
            let mut desc: D3D11_TEXTURE2D_DESC = Default::default();
            frame.texture.GetDesc(&mut desc);
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: D3D11_BIND_FLAG(0).0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .ok()?;
            let staging = staging?;
            self.context.CopyResource(&staging, &frame.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            let rows = frame.height as usize;
            let row_bytes = (frame.width as usize) * 4;
            let mut sum = 0f64;
            let mut count = 0f64;
            let data = mapped.pData as *const u8;
            for y in (0..rows).step_by(16) {
                let row = data.add(y * mapped.RowPitch as usize);
                for x in (0..frame.width as usize).step_by(16) {
                    let px = row.add(x * 4);
                    sum += (*px as f64 + *px.add(1) as f64 + *px.add(2) as f64) / 3.0;
                    count += 1.0;
                }
            }
            self.context.Unmap(&staging, 0);
            if count > 0.0 {
                Some(sum / count)
            } else {
                None
            }
        }
    }

    fn ensure_processor(&mut self, width: u32, height: u32) -> Option<()> {
        if let Some(p) = &self.processor {
            if p.width == width && p.height == height {
                return Some(());
            }
        }
        unsafe {
            let video_device: ID3D11VideoDevice = self.device.cast().ok()?;
            let video_context: ID3D11VideoContext = self.context.cast().ok()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc).ok()?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).ok()?;

            // BT.709 / limited range を明示（RGB入力はフルレンジ）
            let color_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (0 << 0) | (0 << 2) | (1 << 3) | (0 << 4) | (1 << 5),
            };
            let _ = video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &color_space);
            let _ = video_context.VideoProcessorSetOutputColorSpace(&processor, &color_space);

            let out_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut output: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&out_desc, None, Some(&mut output))
                .ok()?;
            let output = output?;

            self.processor = Some(Processor {
                video_device,
                video_context,
                enumerator,
                processor,
                output,
                width,
                height,
            });
            Some(())
        }
    }

    /// BGRA テクスチャを GPU で NV12 に変換して読み出す（M2検証用）
    pub fn to_nv12(&mut self, frame: &GpuFrame) -> Option<Vec<u8>> {
        let width = frame.width as u32;
        let height = frame.height as u32;
        self.ensure_processor(width, height)?;
        unsafe {
            let p = self.processor.as_ref()?;
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            p.video_device
                .CreateVideoProcessorInputView(
                    &frame.texture,
                    &p.enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )
                .ok()?;
            let input_view = input_view?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            p.video_device
                .CreateVideoProcessorOutputView(
                    &p.output,
                    &p.enumerator,
                    &output_desc,
                    Some(&mut output_view),
                )
                .ok()?;
            let output_view = output_view?;
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: core::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: core::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            p.video_context
                .VideoProcessorBlt(&p.processor, &output_view, 0, &[stream])
                .ok()?;

            let mut desc: D3D11_TEXTURE2D_DESC = Default::default();
            p.output.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging: Option<ID3D11Texture2D> = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
            let staging = staging?;
            self.context.CopyResource(&staging, &p.output);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            let w = width as usize;
            let h = height as usize;
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;
            let mut data = vec![0u8; w * h * 3 / 2];
            for y in 0..h {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), data.as_mut_ptr().add(y * w), w);
            }
            let uv = w * h;
            for y in 0..(h / 2) {
                std::ptr::copy_nonoverlapping(
                    src.add((h + y) * pitch),
                    data.as_mut_ptr().add(uv + y * w),
                    w,
                );
            }
            self.context.Unmap(&staging, 0);
            Some(data)
        }
    }
    pub fn stop(&self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
