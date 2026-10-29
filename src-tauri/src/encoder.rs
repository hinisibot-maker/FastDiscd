//! Media Foundation を使った H.264 エンコーダ（ハードウェア優先）
//!
//! - NVENC / Intel Quick Sync / AMD AMF を Media Foundation の MFT 経由で列挙
//! - 見つからなければソフトウェア MFT にフォールバック
//! - 入力は NV12（BGRA から変換）
//! - 出力は Annex-B（SPS/PPS は IDR に付随）

use std::sync::OnceLock;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

/// Media Foundation の初期化（プロセスで一度だけ）
pub fn init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    });
}

#[derive(Clone, Debug)]
pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
}

// COM インターフェースは !Send だが、利用は Mutex で直列化された単一スレッドからのみ
unsafe impl Send for H264Encoder {}

pub struct H264Encoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    width: i32,
    height: i32,
    fps: u32,
    bitrate_kbps: u32,
    frame_index: u64,
    provides_samples: bool,
    buffer_size: u32,
    pub info: EncoderInfo,
}

impl H264Encoder {
    /// 利用可能なエンコーダを列挙し、NVENC → QSV → AMF → ソフトウェアの順で作成する
    pub fn create(width: i32, height: i32, fps: u32, bitrate_kbps: u32) -> Option<Self> {
        init();
        let width = (width.max(16) + 1) & !1;
        let height = (height.max(16) + 1) & !1;

        let mut candidates = enumerate_encoders(true);
        candidates.extend(enumerate_encoders(false));
        for (activate, info) in candidates {
            // ハードウェア MFT は非同期のため、有効化前にロック解除属性を設定する
            let unlocked = unsafe { activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
            crate::log(&format!("encoder activate unlock ({})={unlocked:?}", info.name));
            let unknown: windows::core::IUnknown = match unsafe { activate.ActivateObject() } {
                Ok(t) => t,
                Err(_) => continue,
            };
            // MFT ラッパーを避けるため、IUnknown から属性と変換を直接取得する
            if let Ok(attrs) = unknown.cast::<IMFAttributes>() {
                let r = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
                crate::log(&format!("encoder attribute unlock ({})={r:?}", info.name));
            } else {
                crate::log(&format!("encoder attributes unavailable ({})", info.name));
            }
            let transform: IMFTransform = match unknown.cast() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // ラッパー経由で属性が取れない場合は CLSID から直接生成して解除を試す
            let transform = if let Ok(attrs) = unknown.cast::<IMFAttributes>() {
                let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
                transform
            } else {
                unsafe { co_create_unlocked(&activate).unwrap_or(transform) }
            };
            match unsafe { Self::configure(transform, &info, width, height, fps, bitrate_kbps) } {
                Ok(enc) => return Some(enc),
                Err(e) => {
                    crate::log(&format!("encoder configure failed ({}): {e}", info.name));
                }
            }
        }
        None
    }

    unsafe fn configure(
        transform: IMFTransform,
        info: &EncoderInfo,
        width: i32,
        height: i32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> windows::core::Result<Self> {
        let frame_size = ((width as u64) << 32) | (height as u64);
        let frame_rate = ((fps as u64) << 32) | 1u64;

        // ハードウェア MFT は非同期モードのため、ロック解除しないと構成できない
        match transform.cast::<IMFAttributes>() {
            Ok(attributes) => {
                let r1 = unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
                let r2 = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };
                crate::log(&format!("encoder async unlock: {r1:?} lowlat: {r2:?}"));
            }
            Err(e) => crate::log(&format!("encoder IMFAttributes cast failed: {e}")),
        }

        // 出力タイプ（H.264）
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        out.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        out.SetUINT32(&MF_LOW_LATENCY, 1)?;
        transform.SetOutputType(0, &out, 0)?;

        // 入力タイプ（NV12）
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
        transform.SetInputType(0, &input, 0)?;

        let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

        // レート制御（対応していない MFT では失敗するので無視）
        let codec = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec {
            let _ = unsafe { api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            ) };
            let _ = unsafe { api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(bitrate_kbps.saturating_mul(1000u32)),
            ) };
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps.saturating_mul(2))) };
            let _ = unsafe { api.SetValue(
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                &VARIANT::from(0u32),
            ) };
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncCommonLowLatency, &VARIANT::from(1u32)) };
        }

        let stream_info = transform.GetOutputStreamInfo(0)?;
        let provides_samples =
            (stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        let buffer_size = if stream_info.cbSize > 0 {
            stream_info.cbSize
        } else {
            (width as u32) * (height as u32) * 3 / 2
        };

        Ok(Self {
            transform,
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            frame_index: 0,
            provides_samples,
            buffer_size,
            info: info.clone(),
        })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// 解像度・ビットレートを変更する。失敗した場合は false（呼び出し側で作り直す）
    pub fn reconfigure(&mut self, width: i32, height: i32, bitrate_kbps: u32) -> bool {
        let width = (width.max(16) + 1) & !1;
        let height = (height.max(16) + 1) & !1;
        if width == self.width && height == self.height && bitrate_kbps == self.bitrate_kbps {
            return true;
        }
        if let Some(api) = &self.codec {
            let _ = unsafe { api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(bitrate_kbps.saturating_mul(1000u32)),
            ) };
        }
        self.bitrate_kbps = bitrate_kbps;
        if width == self.width && height == self.height {
            return true;
        }
        let frame_size = ((width as u64) << 32) | (height as u64);
        let frame_rate = ((self.fps as u64) << 32) | 1u64;
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let out = match MFCreateMediaType() {
                Ok(v) => v,
                Err(_) => return false,
            };
            let _ = out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264);
            let _ = out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000));
            let _ = out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            let _ = out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
            let _ = out.SetUINT64(&MF_MT_FRAME_RATE, frame_rate);
            let _ = out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
            if self
                .transform
                .SetOutputType(0, &out, 0)
                .is_err()
            {
                return false;
            }
            let input = match MFCreateMediaType() {
                Ok(v) => v,
                Err(_) => return false,
            };
            let _ = input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
            let _ = input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            let _ = input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
            let _ = input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate);
            if self.transform.SetInputType(0, &input, 0).is_err() {
                return false;
            }
        }
        self.width = width;
        self.height = height;
        self.buffer_size = (width as u32) * (height as u32) * 3 / 2;
        true
    }

    pub fn request_keyframe(&self) {
        if let Some(api) = &self.codec {
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
        }
    }

    /// NV12 フレームをエンコードして Annex-B の NAL を返す
    pub fn encode(&mut self, nv12: &[u8], force_key: bool) -> Vec<Vec<u8>> {
        if force_key {
            self.request_keyframe();
        }
        let mut outputs = Vec::new();
        unsafe {
            let Ok(buffer) = MFCreateMemoryBuffer((nv12.len() as u32).max(1)) else {
                return outputs;
            };
            {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut max_len = 0u32;
                if buffer.Lock(&mut data, Some(&mut max_len), None).is_err() {
                    return outputs;
                }
                let len = (nv12.len() as u32).min(max_len);
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), data, len as usize);
                let _ = buffer.Unlock();
                let _ = buffer.SetCurrentLength(len);
            }

            let Ok(sample) = MFCreateSample() else {
                return outputs;
            };
            if sample.AddBuffer(&buffer).is_err() {
                return outputs;
            }
            let duration = 10_000_000i64 / (self.fps.max(1) as i64);
            let _ = sample.SetSampleTime((self.frame_index as i64) * duration);
            let _ = sample.SetSampleDuration(duration);
            self.frame_index = self.frame_index.wrapping_add(1);

            if self.transform.ProcessInput(0, &sample, 0).is_err() {
                return outputs;
            }

            loop {
                let out_sample: IMFSample = if self.provides_samples {
                    match MFCreateSample() {
                        Ok(s) => s,
                        Err(_) => break,
                    }
                } else {
                    match MFCreateSample() {
                        Ok(s) => s,
                        Err(_) => break,
                    }
                };
                let out_buffer = if self.provides_samples {
                    None
                } else {
                    match MFCreateMemoryBuffer(self.buffer_size.max(1024)) {
                        Ok(b) => Some(b),
                        Err(_) => break,
                    }
                };
                if let Some(b) = &out_buffer {
                    if out_sample.AddBuffer(b).is_err() {
                        break;
                    }
                }

                let mut data_buffer = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: core::mem::ManuallyDrop::new(if self.provides_samples {
                        None
                    } else {
                        Some(out_sample.clone())
                    }),
                    dwStatus: 0,
                    pEvents: core::mem::ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let hr = self.transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut data_buffer),
                    &mut status,
                );

                let produced = if self.provides_samples {
                    data_buffer.pSample.take()
                } else {
                    Some(out_sample.clone())
                };

                for s in produced.into_iter() {
                    if let Ok(contiguous) = s.ConvertToContiguousBuffer() {
                        let mut ptr: *mut u8 = std::ptr::null_mut();
                        let mut cur_len = 0u32;
                        if contiguous.Lock(&mut ptr, None, Some(&mut cur_len)).is_ok() {
                            if !ptr.is_null() && cur_len > 0 {
                                let bytes = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
                                outputs.push(bytes);
                            }
                            let _ = contiguous.Unlock();
                        }
                    }
                }

                if hr.is_err() {
                    break;
                }
            }
        }
        outputs
    }
}


/// CLSID から MFT を直接生成し、属性が取れれば非同期ロックを解除する
unsafe fn co_create_unlocked(activate: &IMFActivate) -> Option<IMFTransform> {
    let clsid = activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).ok()?;
    let unknown: windows::core::IUnknown =
        CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER).ok()?;
    if let Ok(attrs) = unknown.cast::<IMFAttributes>() {
        let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
        crate::log("encoder co-create unlock applied");
    }
    unknown.cast::<IMFTransform>().ok()
}
/// エンコーダ候補を優先順に返す
fn enumerate_encoders(hardware_only: bool) -> Vec<(IMFActivate, EncoderInfo)> {
    let mut result: Vec<(IMFActivate, EncoderInfo)> = Vec::new();
    unsafe {
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let flags = if hardware_only {
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
        } else {
            MFT_ENUM_FLAG_ALL | MFT_ENUM_FLAG_SORTANDFILTER
        };
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output),
            &mut activates,
            &mut count,
        )
        .is_err()
        {
            return result;
        }
        if activates.is_null() || count == 0 {
            return result;
        }
        for i in 0..count as usize {
            let Some(activate) = (*activates.add(i)).clone() else {
                continue;
            };
            let name = friendly_name(&activate);
            let lower = name.to_lowercase();
            let hardware = lower.contains("nvidia")
                || lower.contains("intel")
                || lower.contains("amd")
                || lower.contains("quick sync")
                || lower.contains("qsv")
                || lower.contains("amf")
                || lower.contains("nvenc")
                || lower.contains("hardware");
            result.push((activate, EncoderInfo { name, hardware }));
        }
        // 解放
        let _ = windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
    }

    let priority = |info: &EncoderInfo| -> u8 {
        let n = info.name.to_lowercase();
        if n.contains("nvidia") || n.contains("nvenc") {
            0
        } else if n.contains("intel") || n.contains("quick sync") || n.contains("qsv") {
            1
        } else if n.contains("amd") || n.contains("amf") {
            2
        } else if info.hardware {
            3
        } else {
            4
        }
    };
    result.sort_by_key(|(_, info)| priority(info));
    result
}

fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut value = PWSTR::null();
        let mut len = 0u32;
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
            .is_ok()
            && !value.is_null()
        {
            let name = value.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(value.0 as *const _));
            return name;
        }
    }
    "Unknown encoder".to_string()
}

/// BGRA → NV12（BT.709 limited range, 2x2 平均でクロマ生成）
pub fn bgra_to_nv12(src: &[u8], width: usize, height: usize) -> Vec<u8> {
    let w = width & !1;
    let h = height & !1;
    let mut out = vec![0u8; w * h * 3 / 2];
    let (y_slice, uv_slice) = out.split_at_mut(w * h);
    let y_plane = y_slice;
    let uv_plane = uv_slice;

    for y in 0..h {
        let row = y * width * 4;
        for x in 0..w {
            let i = row + x * 4;
            let b = src[i] as f32;
            let g = src[i + 1] as f32;
            let r = src[i + 2] as f32;
            let yy = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            y_plane[y * w + x] = (16.0 + 0.8588 * yy).round().clamp(0.0, 255.0) as u8;
        }
    }

    let uv_w = w / 2;
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut r = 0f32;
            let mut g = 0f32;
            let mut b = 0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = (y + dy) * width * 4 + (x + dx) * 4;
                    b += src[i] as f32;
                    g += src[i + 1] as f32;
                    r += src[i + 2] as f32;
                }
            }
            r /= 4.0;
            g /= 4.0;
            b /= 4.0;
            let u = -0.1146 * r - 0.3854 * g + 0.5 * b;
            let v = 0.5 * r - 0.4542 * g - 0.0458 * b;
            let idx = (y / 2) * uv_w + (x / 2);
            uv_plane[idx * 2] = (128.0 + u * 0.8784).round().clamp(0.0, 255.0) as u8;
            uv_plane[idx * 2 + 1] = (128.0 + v * 0.8784).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_size_and_black() {
        let w = 8usize;
        let h = 4usize;
        let src = vec![0u8; w * h * 4];
        let nv12 = bgra_to_nv12(&src, w, h);
        assert_eq!(nv12.len(), w * h * 3 / 2);
        // 黒は limited range の Y=16, UV=128
        assert_eq!(nv12[0], 16);
        assert_eq!(nv12[w * h], 128);
    }
}
