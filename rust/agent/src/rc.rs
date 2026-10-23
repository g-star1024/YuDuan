// 御端 Rust 副端 —— M4 远程控制（参考互通版）。
// 采集：macOS screencapture / Linux scrot|import（无工具则上报未就绪，不造假帧）。
// 输入：macOS CoreGraphics FFI（CGEvent）/ Linux X11 XTest FFI（xdotool 兜底）/ Windows user32 SendInput FFI。
// 合规：会话开启即发 consent_required + 本地通知（远端提示铁律：员工知情、可拒绝）。
// 协议与 Go 参考实现完全一致：rc_ready / rc_frame / rc_event / rc_input / rc_stop。

use crate::base64_encode;
use crate::json_string;
use crate::rc_media::default_capabilities;
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

// ===================== Linux X11/XTest FFI（键鼠注入） =====================

#[cfg(target_os = "linux")]
mod xt {
    use std::os::raw::{c_char, c_int, c_uint, c_ulong};

    #[link(name = "X11")]
    extern "C" {
        pub fn XOpenDisplay(name: *const c_char) -> *mut std::os::raw::c_void;
        pub fn XCloseDisplay(d: *mut std::os::raw::c_void) -> c_int;
        pub fn XFlush(d: *mut std::os::raw::c_void) -> c_int;
        pub fn XStringToKeysym(s: *const c_char) -> c_ulong;
        pub fn XKeysymToKeycode(d: *mut std::os::raw::c_void, keysym: c_ulong) -> c_uint;
    }

    #[link(name = "Xtst")]
    extern "C" {
        pub fn XTestFakeMotionEvent(
            d: *mut std::os::raw::c_void,
            screen: c_int,
            x: c_int,
            y: c_int,
            delay: c_ulong,
        ) -> c_int;
        pub fn XTestFakeButtonEvent(
            d: *mut std::os::raw::c_void,
            button: c_uint,
            press: c_int,
            delay: c_ulong,
        ) -> c_int;
        pub fn XTestFakeKeyEvent(
            d: *mut std::os::raw::c_void,
            keycode: c_uint,
            press: c_int,
            delay: c_ulong,
        ) -> c_int;
    }
}

/// 键名 → X11 keysym（XK_* 常量，纯函数便于跨平台单测）。
#[allow(dead_code)] // 非 Linux 主机上仅测试引用
fn keysym_of(name: &str) -> Option<u64> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => 0xff0d,  // XK_Return
        "tab" => 0xff09,               // XK_Tab
        "space" => 0x020,              // XK_space
        "backspace" => 0xff08,         // XK_BackSpace
        "delete" => 0xffff,            // XK_Delete
        "esc" | "escape" => 0xff1b,    // XK_Escape
        "left" => 0xff51,              // XK_Left
        "up" => 0xff52,                // XK_Up
        "right" => 0xff53,             // XK_Right
        "down" => 0xff54,              // XK_Down
        "home" => 0xff50,              // XK_Home
        "end" => 0xff57,               // XK_End
        "pageup" => 0xff55,            // XK_Page_Up
        "pagedown" => 0xff56,          // XK_Page_Down
        "f1" => 0xffbe,                // XK_F1
        _ => return None,
    })
}

// ===================== Windows user32 SendInput FFI（键鼠注入） =====================

#[cfg(target_os = "windows")]
mod win32 {
    use std::os::raw::{c_int, c_long, c_uint, c_ulong};

    pub const INPUT_MOUSE: c_ulong = 1;
    pub const INPUT_KEYBOARD: c_ulong = 2;
    pub const KEYEVENTF_KEYUP: c_ulong = 0x0002;
    pub const MOUSEEVENTF_MOVE: c_ulong = 0x0001;
    pub const MOUSEEVENTF_ABSOLUTE: c_ulong = 0x8000;
    pub const MOUSEEVENTF_VIRTUALDESK: c_ulong = 0x4000;
    pub const MOUSEEVENTF_LEFTDOWN: c_ulong = 0x0002;
    pub const MOUSEEVENTF_LEFTUP: c_ulong = 0x0004;
    pub const MOUSEEVENTF_RIGHTDOWN: c_ulong = 0x0008;
    pub const MOUSEEVENTF_RIGHTUP: c_ulong = 0x0010;
    pub const MOUSEEVENTF_WHEEL: c_ulong = 0x0800;
    pub const WHEEL_DELTA: c_long = 120;
    pub const SM_XVIRTUALSCREEN: c_int = 76;
    pub const SM_YVIRTUALSCREEN: c_int = 77;
    pub const SM_CXVIRTUALSCREEN: c_int = 78;
    pub const SM_CYVIRTUALSCREEN: c_int = 79;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MOUSEINPUT {
        pub dx: c_long,
        pub dy: c_long,
        pub mouse_data: c_ulong,
        pub dw_flags: c_ulong,
        pub time: c_ulong,
        pub dw_extra_info: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct KEYBDINPUT {
        pub w_vk: u16,
        pub w_scan: u16,
        pub dw_flags: c_ulong,
        pub time: c_ulong,
        pub dw_extra_info: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union INPUTUNION {
        pub mouse: MOUSEINPUT,
        pub keyboard: KEYBDINPUT,
    }

    #[repr(C)]
    pub struct INPUT {
        pub type_: c_ulong,
        pub u: INPUTUNION,
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn SendInput(n: c_uint, inputs: *const INPUT, size: c_int) -> c_uint;
        pub fn VkKeyScanW(ch: u16) -> i16;
        pub fn GetSystemMetrics(index: c_int) -> c_int;
    }
}

/// 键名 → Windows 虚拟键码（VK_*，纯函数便于跨平台单测）。
#[allow(dead_code)] // 非 Windows 主机上仅测试引用
fn vk_of(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => 0x0D, // VK_RETURN
        "tab" => 0x09,              // VK_TAB
        "space" => 0x20,            // VK_SPACE
        "backspace" => 0x08,        // VK_BACK
        "delete" => 0x2E,           // VK_DELETE
        "esc" | "escape" => 0x1B,   // VK_ESCAPE
        "left" => 0x25,             // VK_LEFT
        "up" => 0x26,               // VK_UP
        "right" => 0x27,            // VK_RIGHT
        "down" => 0x28,             // VK_DOWN
        "home" => 0x24,             // VK_HOME
        "end" => 0x23,              // VK_END
        "pageup" => 0x21,           // VK_PRIOR
        "pagedown" => 0x22,         // VK_NEXT
        "f1" => 0x70,               // VK_F1
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

    // 生产化（§8.3）：宣告远控媒体能力（编解码器 / 分辨率 / 帧率），供主端协商 P2P 二进制帧通道。
    let caps = default_capabilities().to_json();
    send_rc_capabilities(&writer, &rng, &seq, &session_id, &caps);

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

/// mouse_wheel 的 key 承载滚动量（如 "-3"/"+3"）；纯函数，全平台共用。
fn p_delta(key: &str) -> i32 {
    key.parse::<i32>().unwrap_or(-3)
}

// ===================== 输入注入分发 =====================

fn inject_input(kind: &str, x: f64, y: f64, button: u32, key: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        return inject_input_macos(kind, x, y, button, key);
    }
    #[cfg(target_os = "linux")]
    {
        return inject_input_linux(kind, x, y, button, key);
    }
    #[cfg(target_os = "windows")]
    {
        return inject_input_windows(kind, x, y, button, key);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        eprintln!("[agent] 平台无输入注入实现，仅记录: kind={}", kind);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn inject_input_macos(kind: &str, x: f64, y: f64, button: u32, key: &str) -> Result<(), String> {
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

#[cfg(target_os = "linux")]
fn inject_input_linux(kind: &str, x: f64, y: f64, button: u32, key: &str) -> Result<(), String> {
    // 首选 XTest FFI；无 X 显示（Wayland/纯头）时退化 xdotool 子进程。
    unsafe {
        let disp = xt::XOpenDisplay(std::ptr::null());
        if disp.is_null() {
            return inject_input_xdotool(kind, x, y, button, key);
        }
        let r = (|| {
            match kind {
                "mouse_move" => {
                    xt::XTestFakeMotionEvent(disp, 0, x as i32, y as i32, 0);
                }
                "mouse_down" | "mouse_up" => {
                    let press = if kind == "mouse_down" { 1 } else { 0 };
                    // X 按钮编号：1=左 2=中 3=右（与 viewer 端 1/2 约定一致，2=右需换算）
                    let xb: u32 = match button {
                        2 => 3,
                        1 => 1,
                        _ => 1,
                    };
                    xt::XTestFakeButtonEvent(disp, xb, press, 0);
                }
                "mouse_wheel" => {
                    // 滚轮：button 4=上 5=下；一次 notch = 一次 press+release
                    let xb: u32 = if p_delta(key) > 0 { 4 } else { 5 };
                    xt::XTestFakeButtonEvent(disp, xb, 1, 0);
                    xt::XTestFakeButtonEvent(disp, xb, 0, 0);
                }
                "key_down" | "key_up" | "key_press" => {
                    let sym = keysym_of(key)
                        .or_else(|| {
                            // 单字符：经 XStringToKeysym（如 "a" → XK_a）
                            let mut it = key.chars();
                            match (it.next(), it.next()) {
                                (Some(c), None) => {
                                    let mut buf = [0u8; 4];
                                    let s: &str = c.encode_utf8(&mut buf);
                                    let cstr = format!("{}\0", s);
                                    Some(xt::XStringToKeysym(cstr.as_ptr() as *const std::os::raw::c_char) as u64)
                                }
                                _ => None,
                            }
                        })
                        .ok_or_else(|| format!("未知按键 {}", key))?;
                    let kc = xt::XKeysymToKeycode(disp, sym as std::os::raw::c_ulong);
                    if kc == 0 {
                        return Err(format!("keysym {:#x} 无对应 keycode", sym));
                    }
                    let down = kind != "key_up";
                    xt::XTestFakeKeyEvent(disp, kc, if down { 1 } else { 0 }, 0);
                    if kind == "key_press" {
                        xt::XTestFakeKeyEvent(disp, kc, 0, 0);
                    }
                }
                _ => return Err(format!("未知 kind {}", kind)),
            }
            xt::XFlush(disp);
            Ok(())
        })();
        xt::XCloseDisplay(disp);
        r
    }
}

/// Linux 无 X 显示时的兜底：xdotool 子进程（存在则用，不存在则明确失败，不造假成功）。
#[cfg(target_os = "linux")]
fn inject_input_xdotool(kind: &str, x: f64, y: f64, button: u32, key: &str) -> Result<(), String> {
    let has = Command::new("xdotool").arg("version").output();
    if has.is_err() {
        return Err("无 X 显示且 xdotool 不可用".into());
    }
    let xi = x as i64;
    let yi = y as i64;
    let out = match kind {
        "mouse_move" => Command::new("xdotool").args(["mousemove", &xi.to_string(), &yi.to_string()]).output(),
        "mouse_down" | "mouse_up" => {
            let xb = if button == 2 { 3 } else { 1 };
            let act = if kind == "mouse_down" { "mousedown" } else { "mouseup" };
            Command::new("xdotool").args([act, &xb.to_string()]).output()
        }
        "mouse_wheel" => {
            let btn = if p_delta(key) > 0 { 4 } else { 5 };
            Command::new("xdotool").args(["click", &btn.to_string()]).output()
        }
        "key_down" | "key_up" | "key_press" => Command::new("xdotool").args(["key", key]).output(),
        _ => return Err(format!("未知 kind {}", kind)),
    };
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("xdotool 退出码 {:?}", o.status.code())),
        Err(e) => Err(format!("xdotool 执行失败: {}", e)),
    }
}

#[cfg(target_os = "windows")]
fn inject_input_windows(kind: &str, x: f64, y: f64, button: u32, key: &str) -> Result<(), String> {
    use std::os::raw::{c_int, c_long, c_uint, c_ulong};
    use win32::*;
    unsafe {
        // 绝对坐标需按虚拟桌面归一化到 0..65535
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN) as f64;
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN) as f64;
        let cw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as f64;
        let ch = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as f64;
        let nx = (((x - vx) / cw) * 65535.0).clamp(0.0, 65535.0) as c_long;
        let ny = (((y - vy) / ch) * 65535.0).clamp(0.0, 65535.0) as c_long;
        let abs = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;

        let inputs: Vec<INPUT> = match kind {
            "mouse_move" => vec![INPUT {
                type_: INPUT_MOUSE,
                u: INPUTUNION {
                    mouse: MOUSEINPUT {
                        dx: nx,
                        dy: ny,
                        mouse_data: 0,
                        dw_flags: abs,
                        time: 0,
                        dw_extra_info: 0,
                    },
                },
            }],
            "mouse_down" | "mouse_up" => {
                let (down_f, up_f) = if button == 2 {
                    (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
                } else {
                    (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
                };
                let flag = if kind == "mouse_down" { down_f } else { up_f };
                vec![INPUT {
                    type_: INPUT_MOUSE,
                    u: INPUTUNION {
                        mouse: MOUSEINPUT {
                            dx: nx,
                            dy: ny,
                            mouse_data: 0,
                            dw_flags: flag,
                            time: 0,
                            dw_extra_info: 0,
                        },
                    },
                }]
            }
            "mouse_wheel" => {
                let delta = (p_delta(key) as c_long) * WHEEL_DELTA;
                vec![INPUT {
                    type_: INPUT_MOUSE,
                    u: INPUTUNION {
                        mouse: MOUSEINPUT {
                            dx: nx,
                            dy: ny,
                            mouse_data: delta as c_ulong,
                            dw_flags: MOUSEEVENTF_WHEEL,
                            time: 0,
                            dw_extra_info: 0,
                        },
                    },
                }]
            }
            "key_down" | "key_up" | "key_press" => {
                let (vk, need_shift) = match vk_of(key) {
                    Some(v) => (v, false),
                    None => {
                        // 可打印字符：VkKeyScanW 低字节=VK，高字节=shift 状态
                        let ch = key.chars().next().ok_or("空按键")? as u16;
                        let r = VkKeyScanW(ch);
                        if r == -1 {
                            return Err(format!("VkKeyScan 无法映射字符 U+{:04X}", ch));
                        }
                        ((r & 0xff) as u16, (r >> 8) & 1 == 1)
                    }
                };
                let mut v = Vec::new();
                if need_shift {
                    v.push(key_input(0x10, true)); // VK_LSHIFT
                }
                let down = kind != "key_up";
                v.push(key_input(vk, down));
                if kind == "key_press" {
                    v.push(key_input(vk, false));
                }
                if need_shift {
                    v.push(key_input(0x10, false));
                }
                v
            }
            _ => return Err(format!("未知 kind {}", kind)),
        };
        let n = SendInput(
            inputs.len() as c_uint,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as c_int,
        );
        if n != inputs.len() as c_uint {
            return Err(format!("SendInput 仅注入 {}/{}", n, inputs.len()));
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
unsafe fn key_input(vk: u16, down: bool) -> win32::INPUT {
    use win32::*;
    INPUT {
        type_: INPUT_KEYBOARD,
        u: INPUTUNION {
            keyboard: KEYBDINPUT {
                w_vk: vk,
                w_scan: 0,
                dw_flags: if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
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

/// 宣告远控媒体能力（§8.3）：编解码器 / 最大分辨率 / 帧率；主端据此协商二进制帧通道。
fn send_rc_capabilities(
    writer: &Writer,
    rng: &Arc<Mutex<crate::Lcg>>,
    seq: &Arc<Mutex<u64>>,
    session_id: &str,
    caps_json: &str,
) {
    let payload = format!(
        "{{\"session_id\":{},\"capabilities\":{}}}",
        json_string(session_id), caps_json
    );
    let msg = {
        let mut rng_g = rng.lock().unwrap();
        let mut seq_g = seq.lock().unwrap();
        crate::build_envelope(&mut rng_g, &mut seq_g, "rc_capabilities", &payload)
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

    #[test]
    fn keysym_map_covers_special_keys() {
        assert_eq!(keysym_of("enter"), Some(0xff0d)); // XK_Return
        assert_eq!(keysym_of("Return"), Some(0xff0d)); // 大小写不敏感
        assert_eq!(keysym_of("space"), Some(0x020));
        assert_eq!(keysym_of("backspace"), Some(0xff08));
        assert_eq!(keysym_of("escape"), Some(0xff1b));
        assert_eq!(keysym_of("left"), Some(0xff51));
        assert_eq!(keysym_of("f1"), Some(0xffbe));
        assert_eq!(keysym_of("pageup"), Some(0xff55));
        assert_eq!(keysym_of(" nonexistent-key "), None);
    }

    #[test]
    fn vk_map_covers_special_keys() {
        assert_eq!(vk_of("enter"), Some(0x0D)); // VK_RETURN
        assert_eq!(vk_of("Escape"), Some(0x1B));
        assert_eq!(vk_of("space"), Some(0x20));
        assert_eq!(vk_of("backspace"), Some(0x08));
        assert_eq!(vk_of("left"), Some(0x25));
        assert_eq!(vk_of("f1"), Some(0x70));
        assert_eq!(vk_of("pageup"), Some(0x21)); // VK_PRIOR
        assert_eq!(vk_of("nope"), None);
    }
}
