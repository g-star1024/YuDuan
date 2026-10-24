// 御端主端 Rust core（Tauri 2）。
//
// 模块划分（对齐《御端-主端Tauri设计文档.md》§4）：
//   http        —— 纯 std HTTP/1.1 客户端（GET/POST + JSON，与副端 httpget.rs 同源思路）
//   wsclient    —— 纯 std WebSocket 客户端（握手/掩码/帧编解码，与副端 WsClient 同构）
//   hub_client  —— 中枢控制面：登录/改密/设备台账/指令下发（REST，hub API 同口径）
//   media       —— 媒体环主端侧（§8.3）：24B 帧头 + 4B 长度前缀分帧 + 丢包统计
//
//   p2p          —— P2P 直连主端侧：监听 + ICE-lite 候选 + 直连收帧（T-3）
//   decoder      —— 原生解码器 FFI（§8.3.18，T-3：macOS VT 先行）
pub mod decoder;
pub mod http;
pub mod hub_client;
pub mod media;
pub mod media_session;
pub mod p2p;
pub mod wsclient;

use media_session::{SessionHandle, SessionOpts};
use std::sync::Mutex as StdMutex;

/// 当前活动远控会话（单会话模型：新会话顶替旧会话）
static SESSION: StdMutex<Option<SessionHandle>> = StdMutex::new(None);

use hub_client::{CmdResult, Device, LoginResult};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            hub_login,
            hub_change_password,
            hub_devices,
            hub_device_cmd,
            rc_start,
            rc_stop,
            rc_input,
            rc_qos
        ])
        .run(tauri::generate_context!())
        .expect("御端主端启动失败");
}

// —— Tauri commands：前端只经此与 core 通信（设计文档 §4 模块间契约） ——

/// 登录中枢（POST /api/v1/login）。
#[tauri::command]
fn hub_login(base: String, username: String, password: String) -> Result<LoginResult, String> {
    hub_client::login(&base, &username, &password)
}

/// 修改当前账号密码（POST /api/v1/password），成功返回续签 token。
#[tauri::command]
fn hub_change_password(
    base: String,
    token: String,
    old_password: String,
    new_password: String,
) -> Result<LoginResult, String> {
    hub_client::change_password(&base, &token, &old_password, &new_password)
}

/// 设备台账（GET /api/v1/devices，Bearer 鉴权）。
#[tauri::command]
fn hub_devices(base: String, token: String) -> Result<Vec<Device>, String> {
    hub_client::devices(&base, &token)
}

/// 指令下发（POST /api/v1/devices/{id}，action: echo|msg|collect|sw_install…）。
#[tauri::command]
fn hub_device_cmd(
    base: String,
    token: String,
    device_id: String,
    action: String,
    command: String,
) -> Result<CmdResult, String> {
    hub_client::device_cmd(&base, &token, &device_id, &action, &command)
}

// —— 远控会话（T-2 relay 路径，§8.3） ——

/// 发起远控。transport: auto（优先 P2P 直连，失败落中继）/ p2p / relay；
/// decoder_lib 为空则只选 JPEG/RAW（不虚标原生码流能力）。
#[tauri::command]
fn rc_start(
    app: tauri::AppHandle,
    base: String,
    device_id: String,
    operator: String,
    transport: Option<String>,
    decoder_lib: Option<String>,
    bind: Option<String>,
) -> Result<String, String> {
    // 单会话：已有会话先停
    if let Some(old) = SESSION.lock().unwrap().take() {
        old.stop();
    }
    let opts = SessionOpts {
        transport: transport.unwrap_or_else(|| "auto".into()),
        bind: bind.filter(|s| !s.is_empty()).unwrap_or_else(|| "0.0.0.0:0".into()),
        decoder_lib: decoder_lib.unwrap_or_else(|| std::env::var("YUDUAN_DECODER_LIB").unwrap_or_default()),
    };
    let h = media_session::start(app, base, device_id, operator, opts)?;
    let sid = h.session_id.clone();
    *SESSION.lock().unwrap() = Some(h);
    Ok(sid)
}

/// 结束当前远控会话。
#[tauri::command]
fn rc_stop() -> Result<(), String> {
    if let Some(h) = SESSION.lock().unwrap().take() {
        h.stop();
    }
    Ok(())
}

/// 键鼠输入事件透传（前端 canvas 坐标已换算到帧分辨率）。
#[tauri::command]
fn rc_input(kind: String, x: i64, y: i64, button: i64, key: String) -> Result<(), String> {
    let payload = serde_json::json!({
        "kind": kind, "x": x, "y": y, "button": button,
        "key": if key.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(key) }
    });
    if let Some(h) = SESSION.lock().unwrap().as_ref() {
        h.send_input(payload);
    }
    Ok(())
}

/// QoS 反馈（§8.3.9/11/15 主端为策略发起方；阈值与 rc.html 同源）。
#[tauri::command]
fn rc_qos(fps: i64, loss_pct: i64, kbps: i64, rtt_ms: i64) -> Result<(), String> {
    let mut payload = serde_json::json!({ "loss_pct": loss_pct, "kbps": kbps });
    if fps > 0 {
        payload["fps"] = serde_json::Value::from(fps);
    }
    if rtt_ms > 0 {
        payload["rtt_ms"] = serde_json::Value::from(rtt_ms);
        // §8.3.11 RTT 档位自动降级（与 rc.html 同参数：>250ms 降到 8fps）
        if rtt_ms > 250 {
            payload["fps"] = serde_json::Value::from(8);
            payload["reason"] = serde_json::Value::from("rtt_auto");
        }
    }
    if let Some(h) = SESSION.lock().unwrap().as_ref() {
        h.send_qos(payload);
    }
    Ok(())
}
