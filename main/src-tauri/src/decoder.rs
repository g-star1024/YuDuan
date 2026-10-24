// 御端主端 —— 原生解码器 FFI（§8.3.18，与副端 rc_media::NativeEncoder 同构）。
//
// 设计要点：
//   - 运行时 dlopen/LoadLibrary 装载系统解码器库（macOS VideoToolbox / Windows MF /
//     Linux VAAPI），**不虚标**：create 返回 NULL 即视为该 codec 不可用；
//   - 输入 Annex-B 码流，输出 RGBA8（紧凑 w*h*4），与主端 canvas putImageData 口径一致；
//   - 解码失败返回 0 —— 调用方按「等下一个关键帧」处理，**严禁**填充黑帧冒充成功
//     （铁律 6：交付须可验证，不造假）；
//   - 三平台同构（铁律 2）：库文件路径由 YUDUAN_DECODER_LIB 指定，加载失败即回退
//     「帧统计模式」，不阻断会话。

use crate::media::{CODEC_H264, CODEC_H265};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};

#[cfg(unix)]
extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[cfg(windows)]
extern "system" {
    fn LoadLibraryA(filename: *const c_char) -> *mut c_void;
    fn GetProcAddress(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[cfg(unix)]
const RTLD_NOW: c_int = 2;

type DecCreate = unsafe extern "C" fn(*const c_char, c_uint, c_uint) -> *mut c_void;
type DecDecode = unsafe extern "C" fn(*mut c_void, *const u8, c_uint, *mut u8, c_uint) -> c_uint;
type DecOutSize = unsafe extern "C" fn(*mut c_void) -> c_uint;
type DecDestroy = unsafe extern "C" fn(*mut c_void);

/// 帧编码值 → 解码器 codec 名（与编码器 dylib 的命名一致）。
pub fn codec_name(codec: u8) -> Option<&'static str> {
    match codec {
        CODEC_H264 => Some("h264"),
        CODEC_H265 => Some("h265"),
        _ => None,
    }
}

/// 运行时装载的原生解码器句柄。
pub struct NativeDecoder {
    dec: *mut c_void,
    decode: DecDecode,
    destroy: DecDestroy,
    width: u32,
    height: u32,
    buf: Vec<u8>,
}

// 解码器只在媒体线程内使用（单线程收帧循环），跨线程移动由调用方保证。
unsafe impl Send for NativeDecoder {}

impl NativeDecoder {
    /// 装载并创建解码器；任一环节失败返回 None（供能力探测使用，不虚标）。
    pub fn try_load(lib: &str, codec: &str, width: u32, height: u32) -> Option<Self> {
        if lib.is_empty() || width == 0 || height == 0 {
            return None;
        }
        unsafe {
            let lib_c = CString::new(lib).ok()?;
            #[cfg(unix)]
            let handle = dlopen(lib_c.as_ptr(), RTLD_NOW);
            #[cfg(windows)]
            let handle = LoadLibraryA(lib_c.as_ptr());
            if handle.is_null() {
                eprintln!("[decoder] 装载失败: {}", lib);
                return None;
            }
            let sym = |name: &str| -> *mut c_void {
                let c = CString::new(name).unwrap();
                #[cfg(unix)]
                let p = dlsym(handle, c.as_ptr());
                #[cfg(windows)]
                let p = GetProcAddress(handle, c.as_ptr()) as *mut c_void;
                p
            };
            let p_create = sym("yuduan_dec_create");
            let p_decode = sym("yuduan_dec_decode");
            let p_size = sym("yuduan_dec_out_size");
            let p_destroy = sym("yuduan_dec_destroy");
            if p_create.is_null() || p_decode.is_null() || p_destroy.is_null() || p_size.is_null() {
                eprintln!("[decoder] C-ABI 符号缺失（{} 不符合 §8.3.18）", lib);
                return None;
            }
            let create: DecCreate = std::mem::transmute(p_create);
            let decode: DecDecode = std::mem::transmute(p_decode);
            let destroy: DecDestroy = std::mem::transmute(p_destroy);
            let out_size: DecOutSize = std::mem::transmute(p_size);

            let c_codec = CString::new(codec).ok()?;
            let dec = create(c_codec.as_ptr(), width as c_uint, height as c_uint);
            if dec.is_null() {
                return None; // 该 codec 不被支持
            }
            let cap = out_size(dec);
            if cap == 0 {
                destroy(dec);
                return None;
            }
            Some(NativeDecoder {
                dec,
                decode,
                destroy,
                width,
                height,
                buf: vec![0u8; cap as usize],
            })
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    /// 解码一帧码流 → RGBA8；失败返回 None（等下一个关键帧）。
    pub fn decode(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.is_empty() || self.buf.is_empty() {
            return None;
        }
        unsafe {
            let n = (self.decode)(
                self.dec,
                data.as_ptr(),
                data.len() as c_uint,
                self.buf.as_mut_ptr(),
                self.buf.len() as c_uint,
            );
            if n == 0 {
                return None;
            }
            Some(self.buf[..n as usize].to_vec())
        }
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.dec) };
    }
}

/// 探测某 codec 是否可用（用于能力宣告与协商，不虚标）。
pub fn probe(lib: &str, codec: &str) -> bool {
    NativeDecoder::try_load(lib, codec, 1920, 1080).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_name_maps_native_only() {
        assert_eq!(codec_name(CODEC_H264), Some("h264"));
        assert_eq!(codec_name(CODEC_H265), Some("h265"));
        assert_eq!(codec_name(crate::media::CODEC_JPEG), None);
    }

    #[test]
    fn try_load_missing_lib_returns_none() {
        assert!(NativeDecoder::try_load("/nonexistent/libyuduan_dec_vt.dylib", "h264", 640, 360).is_none());
    }

    #[test]
    fn probe_missing_lib_is_false() {
        assert!(!probe("/nonexistent/x.dylib", "h264"));
    }
}
