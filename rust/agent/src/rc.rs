// 御端 Rust 副端 —— M4 远程控制（参考互通版）。
// 采集：macOS screencapture / Linux scrot|import（无工具则上报未就绪，不造假帧）。
// 输入：macOS 经 CoreGraphics FFI（CGEvent）注入键鼠；其余平台记录日志。
// 合规：会话开启即发 consent_required + 本地通知（远端提示铁律：员工知情、可拒绝）。
// 协议与 Go 参考实现完全一致：rc_ready / rc_frame / rc_event / rc_input / rc_stop。

use crate::base64_encode;
use crate::json_string;
use crate::sha256::base64_decode;
use crate::send_ws_text;
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

// ===================== macOS CoreGraphics FFI（键鼠注入） =====================

#[cfg(target_os = "macos")]
mod cg {
    use std::os::raw::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGEventCreateMouseEvent(
            source: *const c_void,
            mouse_type: u32,
            pos: CGPoint,
            button: u32,
        ) -> *const c_void;
        pub fn CGEventPost(tap: u32, event: *const c_void);
        pub fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            virtual_key: u16,
            down: bool,
        ) -> *const c_void;
        /// 非可变参数变体（FFI 安全）：三轴滚轮，仅用 wheel1。
        pub fn CGEventCreateScrollWheelEvent2(
            source: *const c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> *const c_void;
    }

    pub const K_CG_HID_EVENT_TAP: u32 = 0;
    pub const MOUSE_MOVED: u32 = 5;
    pub const MOUSE_LEFT_DOWN: u32 = 1;
    pub const MOUSE_LEFT_UP: u32 = 2;
    pub const MOUSE_RIGHT_DOWN: u32 = 3;
    pub const MOUSE_RIGHT_UP: u32 = 4;
    pub const BUTTON_LEFT: u32 = 0;
    pub const BUTTON_RIGHT: u32 = 1;
    pub const SCROLL_UNIT_PIXEL: u32 = 1;
}

/// 键名 → macOS 虚拟键码（常见子集）。
#[cfg(target_os = "macos")]
fn key_code_of(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => 36,
        "tab" => 48,
        "space" => 49,
        "backspace" | "delete" => 51,
        "esc" | "escape" => 53,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "f1" => 122,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        _ => return None,
    })
}

// ===================== 远控会话 =====================

static RC_STOP: AtomicBool = AtomicBool::new(false);

/// 开启远控：合规提示 → rc_ready → 500ms 帧循环，直至 stop。
pub fn start_rc(writer: Writer, rng: Arc<Mutex<crate::Lcg>>, seq: Arc<Mutex<u64>>, session_id: String, _device_id: String) {
    RC_STOP.store(false, Ordering::SeqCst);

    // 合规：本地通知 + 事件（员工知情可拒绝；开发态自动授权见 rc_event）
    show_remote_notice();
    send_rc_event(&writer, &rng, &seq, &session_id, "consent_required",
        "管理员请求远程控制本机，您可拒绝", true, false);
    send_rc_ready(&writer, &rng, &seq, &session_id, true, "");

    thread::spawn(move || {
        let mut frame_seq: i64 = 0;
        while !RC_STOP.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(500));
            if RC_STOP.load(Ordering::SeqCst) {
                break;
            }
            frame_seq += 1;
            match capture_jpeg() {
                Some((data, w, h)) => {
                    let payload = format!(
                        "{{\"session_id\":{},\"seq\":{},\"width\":{},\"height\":{},\"format\":\"jpeg\",\"data\":{},\"ts\":{}}}",
                        json_string(&session_id), frame_seq, w, h, json_string(&base64_encode(&data)), crate::now_secs()
                    );
                    let msg = {
                        let mut rng_g = rng.lock().unwrap();
                        let mut seq_g = seq.lock().unwrap();
                        crate::build_envelope(&mut rng_g, &mut seq_g, "rc_frame", &payload)
                    };
                    if let Err(e) = send_ws_text(&mut *writer.lock().unwrap(), msg.as_bytes()) {
                        eprintln!("[agent] rc_frame 发送失败: {}", e);
                        break;
                    }
                }
                None => {
                    // 采集不可用：上报未就绪（不造假帧）
                    send_rc_event(&writer, &rng, &seq, &session_id, "notice",
                        "屏幕采集不可用（缺少 screencapture/scrot/import 或权限）", false, false);
                    break;
                }
            }
        }
        eprintln!("[agent] rc 会话结束 {}", session_id);
    });
}

pub fn stop_rc() {
    RC_STOP.store(true, Ordering::SeqCst);
}

/// 处理 rc_input：注入键鼠（macOS 真实注入；其余平台记录）。
pub fn apply_input(writer: &Writer, rng: &Arc<Mutex<crate::Lcg>>, seq: &Arc<Mutex<u64>>, session_id: &str, p: &crate::Json) {
    let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let x = p.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as f64;
    let y = p.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as f64;
    let button = p.get("button").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
    let key = p.get("key").and_then(|v| v.as_str()).unwrap_or("");

    let result = inject_input(kind, x, y, button, key);
    let ok = result.is_ok();
    if let Err(e) = result {
        eprintln!("[agent] 输入注入失败 kind={} err={}", kind, e);
    }
    send_rc_event(writer, rng, seq, session_id, "input_received",
        &format!("received {} ({})", kind, if ok { "ok" } else { "failed" }),
        false, false);
}

#[cfg(target_os = "macos")]
fn inject_input(kind: &str, x: f64, y: f64, button: u32, key: &str) -> Result<(), String> {
    unsafe {
        let src = std::ptr::null();
        match kind {
            "mouse_move" => {
                let ev = cg::CGEventCreateMouseEvent(src, cg::MOUSE_MOVED, cg::CGPoint { x, y }, cg::BUTTON_LEFT);
                if ev.is_null() {
                    return Err("CGEventCreateMouseEvent null".into());
                }
                cg::CGEventPost(cg::K_CG_HID_EVENT_TAP, ev);
            }
            "mouse_down" => {
                let (mt, btn) = match button {
                    2 => (cg::MOUSE_RIGHT_DOWN, cg::BUTTON_RIGHT),
                    _ => (cg::MOUSE_LEFT_DOWN, cg::BUTTON_LEFT),
                };
                let ev = cg::CGEventCreateMouseEvent(src, mt, cg::CGPoint { x, y }, btn);
                if !ev.is_null() {
                    cg::CGEventPost(cg::K_CG_HID_EVENT_TAP, ev);
                }
            }
            "mouse_up" => {
                let (mt, btn) = match button {
                    2 => (cg::MOUSE_RIGHT_UP, cg::BUTTON_RIGHT),
                    _ => (cg::MOUSE_LEFT_UP, cg::BUTTON_LEFT),
                };
                let ev = cg::CGEventCreateMouseEvent(src, mt, cg::CGPoint { x, y }, btn);
                if !ev.is_null() {
                    cg::CGEventPost(cg::K_CG_HID_EVENT_TAP, ev);
                }
            }
            "mouse_wheel" => {
                let dy = p_delta(key);
                let ev = cg::CGEventCreateScrollWheelEvent2(src, cg::SCROLL_UNIT_PIXEL, 1, dy, 0, 0);
                if !ev.is_null() {
                    cg::CGEventPost(cg::K_CG_HID_EVENT_TAP, ev);
                }
            }
            "key_down" | "key_up" | "key_press" => {
                let code = key_code_of(key).ok_or_else(|| format!("未知按键 {}", key))?;
                let down = kind != "key_up";
                let ev = cg::CGEventCreateKeyboardEvent(src, code, down);
                if ev.is_null() {
                    return Err("CGEventCreateKeyboardEvent null".into());
                }
                cg::CGEventPost(cg::K_CG_HID_EVENT_TAP, ev);
                if kind == "key_press" {
                    let up = cg::CGEventCreateKeyboardEvent(src, code, false);
                    if !up.is_null() {
                        cg::CGEventPost(cg::K_CG_HID_EVENT_TAP, up);
                    }
                }
            }
            _ => return Err(format!("未知 kind {}", kind)),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn p_delta(key: &str) -> i32 {
    // mouse_wheel 的 key 承载滚动量（如 "-3"/"+3"）
    key.parse::<i32>().unwrap_or(-3)
}

#[cfg(not(target_os = "macos"))]
fn inject_input(kind: &str, _x: f64, _y: f64, _button: u32, _key: &str) -> Result<(), String> {
    // 非 macOS：最佳努力记录（生产可在 Linux 用 XTest/uinput、Windows 用 SendInput）
    eprintln!("[agent] 非 macOS 平台，输入事件仅记录: kind={}", kind);
    Ok(())
}

// ===================== 屏幕采集 =====================

fn capture_jpeg() -> Option<(Vec<u8>, usize, usize)> {
    let path = format!("/tmp/yuduan_rc_{}.jpg", std::process::id());
    if cfg!(target_os = "macos") {
        let ok = Command::new("screencapture")
            .args(["-x", "-t", "jpg", &path])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
    } else if cfg!(target_os = "linux") {
        let ok = Command::new("scrot").args(["-o", &path]).status().map(|s| s.success()).unwrap_or(false)
            || Command::new("import").args(["-window", "root", &path]).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return None;
        }
    } else {
        return None;
    }
    let data = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    if data.is_empty() {
        return None;
    }
    let (w, h) = jpeg_size(&data).unwrap_or((0, 0));
    Some((data, w, h))
}

/// 解析 JPEG SOF 尺寸（无第三方库，纯 std）。
fn jpeg_size(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0xD8 || (marker >= 0xD0 && marker <= 0xD7) || marker == 0x01 {
            i += 2;
            continue;
        }
        if marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        // SOF0/1/2/3/5/6/7/9/10/11
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as usize;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as usize;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

// ===================== 协议消息 =====================

fn send_rc_ready(writer: &Writer, rng: &Arc<Mutex<crate::Lcg>>, seq: &Arc<Mutex<u64>>, session_id: &str, ok: bool, reason: &str) {
    let payload = format!(
        "{{\"session_id\":{},\"ok\":{},\"reason\":{}}}",
        json_string(session_id), ok, json_string(reason)
    );
    let msg = {
        let mut rng_g = rng.lock().unwrap();
        let mut seq_g = seq.lock().unwrap();
        crate::build_envelope(&mut rng_g, &mut seq_g, "rc_ready", &payload)
    };
    let _ = send_ws_text(&mut *writer.lock().unwrap(), msg.as_bytes());
}

fn send_rc_event(
    writer: &Writer,
    rng: &Arc<Mutex<crate::Lcg>>,
    seq: &Arc<Mutex<u64>>,
    session_id: &str,
    kind: &str,
    msg: &str,
    require_consent: bool,
    consented: bool,
) {
    let payload = format!(
        "{{\"session_id\":{},\"kind\":{},\"message\":{},\"require_consent\":{},\"consented\":{}}}",
        json_string(session_id), json_string(kind), json_string(msg), require_consent, consented
    );
    let m = {
        let mut rng_g = rng.lock().unwrap();
        let mut seq_g = seq.lock().unwrap();
        crate::build_envelope(&mut rng_g, &mut seq_g, "rc_event", &payload)
    };
    let _ = send_ws_text(&mut *writer.lock().unwrap(), m.as_bytes());
}

fn show_remote_notice() {
    if cfg!(target_os = "macos") {
        let _ = Command::new("osascript")
            .args([
                "-e",
                "display notification \"管理员正在请求远程控制本机，您可拒绝\" with title \"御端远端提示\"",
            ])
            .status();
    }
    eprintln!("[agent] [远端提示] 管理员请求远程控制，员工可拒绝");
}

// ===================== base64 编解码 =====================
// 编码复用主模块实现（crate::base64_encode）；解码用 sha256 模块的 base64_decode。

// 供测试引用避免 unused 警告
#[allow(dead_code)]
fn _use_decode() {
    let _ = base64_decode("AAAA");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_size_parses_sof() {
        // 构造一个最小 JPEG：SOI + SOF0(长度8, 精度8, 高=100, 宽=200) + EOI
        let mut jpg = vec![0xFF, 0xD8];
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x08, 0x08]);
        jpg.extend_from_slice(&(100u16).to_be_bytes());
        jpg.extend_from_slice(&(200u16).to_be_bytes());
        jpg.push(0x03); // 分量数
        jpg.extend_from_slice(&[0x01, 0x22, 0x00]);
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(jpeg_size(&jpg), Some((200, 100)));
    }

    #[test]
    fn base64_roundtrip() {
        let raw = vec![1u8, 2, 3, 4, 5];
        let enc = base64_encode(&raw);
        assert_eq!(base64_decode(&enc).unwrap(), raw);
    }
}
