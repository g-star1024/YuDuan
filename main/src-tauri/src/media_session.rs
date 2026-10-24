// 御端主端 —— 远控媒体会话（T-2 relay 中继 + T-3 P2P 直连 / 原生解码，§8.3）。
//
// 流程（与 p2pviewer / rc.html 同口径）：
//   1. 连 hub /ws/viewer → 发 rc_request{device_id, session_id, operator}
//   2. T-3：若允许 P2P，先起 TCP 监听 → 收集 ICE-lite 候选
//   3. 收 rc_ready / rc_capabilities → 选编解码器（原生码流仅在主端解码器可用时选择，
//      不虚标）→ 发 rc_select{codec, transport, media_addr?}
//      → 紧随 rc_offer（批量候选）+ rc_candidate（增量）供被控端 ICE-lite 升级
//   4. 收帧：① WS 二进制帧（relay）② P2P 直连线程（4B 长度前缀分帧）→ 同一出口
//      → 原生码流经解码器转 RGBA → Tauri 事件推前端 canvas
//   5. 输入/QoS 经 mpsc 通道由会话线程串行发送（hub ws 库非线程安全，统一单线程写）
//   6. RTT 探测（§8.3.11）：每 2s 发 rc_ping，rc_pong 回显 t0 计算 RTT

use crate::decoder;
use crate::media::{
    frame_bytes, LossCounter, MediaFrame, CODEC_H264, CODEC_H265, CODEC_JPEG, CODEC_RAW_RGBA,
};
use crate::p2p::{self, P2pListener};
use crate::wsclient::{Frame, WsClient};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct SessionHandle {
    pub session_id: String,
    stop: Arc<AtomicBool>,
    tx: Arc<Mutex<Option<Sender<OutMsg>>>>,
}

enum OutMsg {
    Input(serde_json::Value),
    Qos(serde_json::Value),
    Stop,
}

/// 会话选项（前端/无头验收可指定）。
#[derive(Clone)]
pub struct SessionOpts {
    /// auto = 优先 P2P 直连（失败自动落中继）；p2p = 仅直连；relay = 仅中继。
    pub transport: String,
    /// P2P 监听地址（默认 0.0.0.0:0 系统分配端口）。
    pub bind: String,
    /// 原生解码器库路径（空 = 不启用原生码流，只选 JPEG/RAW）。
    pub decoder_lib: String,
}

impl Default for SessionOpts {
    fn default() -> Self {
        SessionOpts {
            transport: "auto".into(),
            bind: "0.0.0.0:0".into(),
            decoder_lib: std::env::var("YUDUAN_DECODER_LIB").unwrap_or_default(),
        }
    }
}

#[derive(Serialize, Clone)]
pub struct MediaEvent {
    pub codec: u8,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub pts_ms: u32,
    pub flags: u8,
    pub data: String, // base64(payload)；与 rc.html 带宽口径一致
}

#[derive(Serialize, Clone)]
pub struct StatusEvent {
    pub phase: String, // requesting / ready / negotiating / streaming / stopped / error
    pub detail: String,
    pub session_id: String,
}

#[derive(Serialize, Clone)]
pub struct QosStatsEvent {
    pub fps_recv: f64,
    pub loss_pct: u64,
    pub kbps_recv: u64,
    pub rtt_ms: Option<u64>,
    pub codec: u8,
    pub transport: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn b64(data: &[u8]) -> String {
    crate::wsclient::base64_encode(data)
}

/// 启动远控会话（transport/解码器由 opts 决定）。返回句柄；事件经 Tauri app.emit 推送。
pub fn start(
    app: tauri::AppHandle,
    hub_base: String,
    device_id: String,
    operator: String,
    opts: SessionOpts,
) -> Result<SessionHandle, String> {
    let ws_url = format!(
        "{}/ws/viewer",
        hub_base
            .trim_end_matches('/')
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1)
    );
    let session_id = format!("s-{}-{}", std::process::id(), now_ms() % 1_000_000);

    let (tx, rx) = mpsc::channel::<OutMsg>();
    let (ftx, frx) = mpsc::channel::<MediaFrame>();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = SessionHandle {
        session_id: session_id.clone(),
        stop: stop.clone(),
        tx: Arc::new(Mutex::new(Some(tx))),
    };

    let sid = session_id.clone();
    let stop2 = stop.clone();
    std::thread::spawn(move || {
        session_loop(app, ws_url, device_id, operator, sid, stop2, rx, frx, ftx, opts);
    });
    Ok(handle)
}

/// 兼容入口：纯 relay 会话（T-2 行为，无 P2P、无原生解码）。
pub fn start_relay(
    app: tauri::AppHandle,
    hub_base: String,
    device_id: String,
    operator: String,
) -> Result<SessionHandle, String> {
    start(
        app,
        hub_base,
        device_id,
        operator,
        SessionOpts {
            transport: "relay".into(),
            bind: "0.0.0.0:0".into(),
            decoder_lib: String::new(),
        },
    )
}

impl SessionHandle {
    pub fn send_input(&self, v: serde_json::Value) {
        if let Ok(g) = self.tx.lock() {
            if let Some(t) = g.as_ref() {
                let _ = t.send(OutMsg::Input(v));
            }
        }
    }
    pub fn send_qos(&self, v: serde_json::Value) {
        if let Ok(g) = self.tx.lock() {
            if let Some(t) = g.as_ref() {
                let _ = t.send(OutMsg::Qos(v));
            }
        }
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(g) = self.tx.lock() {
            if let Some(t) = g.as_ref() {
                let _ = t.send(OutMsg::Stop);
            }
        }
    }
}

fn emit_status(app: &tauri::AppHandle, phase: &str, detail: &str, sid: &str) {
    use tauri::Emitter;
    let _ = app.emit(
        "rc-status",
        StatusEvent {
            phase: phase.into(),
            detail: detail.into(),
            session_id: sid.into(),
        },
    );
}

#[derive(Deserialize)]
struct Caps {
    codecs: Vec<i64>,
}

#[allow(clippy::too_many_arguments)]
fn session_loop(
    app: tauri::AppHandle,
    ws_url: String,
    device_id: String,
    operator: String,
    session_id: String,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<OutMsg>,
    frx: mpsc::Receiver<MediaFrame>,
    ftx: Sender<MediaFrame>,
    opts: SessionOpts,
) {
    emit_status(&app, "connecting", &ws_url, &session_id);
    let mut ws = match WsClient::connect(&ws_url) {
        Ok(w) => w,
        Err(e) => {
            emit_status(&app, "error", &format!("连接中枢失败: {}", e), &session_id);
            return;
        }
    };

    // T-3：P2P 监听（仅 auto/p2p 模式）
    let listener: Option<P2pListener> = if opts.transport != "relay" {
        match P2pListener::bind(&opts.bind) {
            Ok(ln) => {
                let cands = ln.candidates();
                emit_status(
                    &app,
                    "listening",
                    &format!("P2P 监听 {} 候选={:?}", ln.local_addr(), cands),
                    &session_id,
                );
                Some(ln)
            }
            Err(e) => {
                emit_status(&app, "error", &format!("P2P 监听失败: {}", e), &session_id);
                None
            }
        }
    } else {
        None
    };

    // rc_request
    let req = serde_json::json!({
        "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
        "type": "rc_request", "seq": 1,
        "payload": { "device_id": device_id, "session_id": session_id, "operator": operator }
    });
    if let Err(e) = ws.send_text(req.to_string().as_bytes()) {
        emit_status(&app, "error", &format!("发送 rc_request 失败: {}", e), &session_id);
        return;
    }
    emit_status(&app, "requesting", "等待被控端就绪", &session_id);

    let mut selected_codec: Option<u8> = None;
    let mut transport: String;
    let mut transport_used = "relay".to_string();
    let mut native: Option<crate::decoder::NativeDecoder> = None;
    let mut decoder_pending = 0u64;
    let mut decoded_ok = 0u64;
    let mut loss = LossCounter::default();
    let mut frames: u64 = 0;
    let mut bytes: u64 = 0;
    let mut rate_frames: u64 = 0;
    let mut rate_bytes: u64 = 0;
    let mut last_rate = Instant::now();
    let mut pending_ping: Option<Instant> = None;
    let mut rtt_ms: Option<u64> = None;
    let mut last_ping = Instant::now();
    let mut offers_sent = false;
    let mut p2p_connected = false;

    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.send_text(
                serde_json::json!({
                    "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                    "type": "rc_stop", "seq": 1,
                    "payload": { "session_id": session_id }
                })
                .to_string()
                .as_bytes(),
            );
            emit_status(&app, "stopped", "会话已结束", &session_id);
            return;
        }

        // —— P2P：轮询 accept + 收直连帧 ——
        if let Some(ln) = listener.as_ref() {
            if !p2p_connected {
                match ln.try_accept() {
                    Ok(Some(stream)) => {
                        p2p::spawn_receiver(stream, ftx.clone(), stop.clone(), "p2p");
                        p2p_connected = true;
                        emit_status(&app, "p2p", "被控端已直连（媒体走 LAN 直连）", &session_id);
                    }
                    Ok(None) => {}
                    Err(e) => emit_status(&app, "error", &format!("P2P accept: {}", e), &session_id),
                }
            }
            while let Ok(f) = frx.try_recv() {
                if transport_used != "p2p" {
                    transport_used = "p2p".into();
                    emit_status(&app, "streaming", "媒体已升级为 P2P 直连", &session_id);
                }
                handle_frame(&app, f, &mut native, &opts, &mut loss, &mut frames, &mut rate_frames,
                             &mut bytes, &mut rate_bytes, &mut decoder_pending, &mut decoded_ok,
                             &mut selected_codec);
            }
        }

        // —— 读 WS 帧（100ms 读超时轮询出站队列） ——
        ws.set_read_timeout_millis(100);
        match ws.read_frame() {
            Ok(Frame::Binary(b)) => match MediaFrame::parse(&b) {
                Ok(f) => {
                    if transport_used != "relay" && p2p_connected {
                        // 直连已建立后到达的中继帧：以直连为准，仅计数不渲染
                        bytes += b.len() as u64;
                    } else {
                        transport_used = "relay".into();
                        handle_frame(&app, f, &mut native, &opts, &mut loss, &mut frames,
                                     &mut rate_frames, &mut bytes, &mut rate_bytes,
                                     &mut decoder_pending, &mut decoded_ok, &mut selected_codec);
                    }
                }
                Err(e) => emit_status(&app, "error", &format!("媒体帧解析失败: {}", e), &session_id),
            },
            Ok(Frame::Text(t)) => {
                let v: serde_json::Value = match serde_json::from_slice(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match typ {
                    "rc_ready" => {
                        let ok = v.pointer("/payload/ok").and_then(|x| x.as_bool()).unwrap_or(false);
                        let reason = v.pointer("/payload/reason").and_then(|x| x.as_str()).unwrap_or("");
                        if ok {
                            emit_status(&app, "ready", "被控端就绪", &session_id);
                        } else {
                            emit_status(&app, "error", &format!("被控端拒绝: {}", reason), &session_id);
                            return;
                        }
                    }
                    "rc_capabilities" => {
                        let caps: Caps = serde_json::from_value(
                            v.get("payload").cloned().unwrap_or_default(),
                        )
                        .unwrap_or(Caps { codecs: vec![CODEC_JPEG as i64] });
                        let pick = pick_codec(&caps, &opts);
                        // 原生码流：先建解码器，建不出就诚实不选（不虚标、不造假画面）
                        if let Some(name) = decoder::codec_name(pick) {
                            native = decoder::NativeDecoder::try_load(&opts.decoder_lib, name, 1920, 1080);
                            if native.is_none() {
                                emit_status(
                                    &app,
                                    "notice",
                                    &format!("解码器不可用（{}），回落 JPEG", name),
                                    &session_id,
                                );
                            }
                        }
                        let pick = if native.is_none() {
                            if caps.codecs.contains(&(CODEC_JPEG as i64)) {
                                CODEC_JPEG
                            } else {
                                CODEC_RAW_RGBA
                            }
                        } else {
                            pick
                        };

                        // transport：有候选即宣告 p2p（首跳失败仍可经 ICE-lite 升级）
                        let mut media_addr = String::new();
                        if let Some(ln) = listener.as_ref() {
                            let cands = ln.candidates();
                            if !cands.is_empty() {
                                media_addr = cands[0].clone();
                            }
                        }
                        transport = if media_addr.is_empty() { "relay".into() } else { "p2p".into() };

                        let mut payload = serde_json::json!({
                            "session_id": session_id, "codec": pick, "transport": transport
                        });
                        if !media_addr.is_empty() {
                            payload["media_addr"] = serde_json::Value::String(media_addr);
                        }
                        let _ = ws.send_text(
                            serde_json::json!({
                                "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                                "type": "rc_select", "seq": 1, "payload": payload
                            })
                            .to_string()
                            .as_bytes(),
                        );
                        selected_codec = Some(pick);
                        emit_status(
                            &app,
                            "streaming",
                            &format!("已协商 codec={} transport={}", pick, transport),
                            &session_id,
                        );

                        // ICE-lite 候选广播（rc_offer 批量 + rc_candidate 增量）
                        if !offers_sent {
                            if let Some(ln) = listener.as_ref() {
                                let cands = ln.candidates();
                                if !cands.is_empty() {
                                    let _ = ws.send_text(p2p::offer_envelope(&session_id, &cands).as_bytes());
                                    for c in cands.iter().skip(1) {
                                        let _ = ws.send_text(
                                            p2p::candidate_envelope(&session_id, c).as_bytes(),
                                        );
                                    }
                                    emit_status(
                                        &app,
                                        "offer",
                                        &format!("已广播 {} 个 ICE 候选", cands.len()),
                                        &session_id,
                                    );
                                }
                            }
                            offers_sent = true;
                        }
                    }
                    "rc_event" => {
                        use tauri::Emitter;
                        let _ = app.emit(
                            "rc-event",
                            serde_json::json!({
                                "kind": v.pointer("/payload/kind").cloned().unwrap_or_default(),
                                "message": v.pointer("/payload/message").cloned().unwrap_or_default(),
                            }),
                        );
                    }
                    "rc_pong" => {
                        if let Some(sent_at) = pending_ping.take() {
                            rtt_ms = Some(sent_at.elapsed().as_millis() as u64);
                        }
                    }
                    "rc_frame" => {
                        // JSON base64 兜底（旧路径）：与二进制帧同事件出口
                        use tauri::Emitter;
                        let p = v.get("payload").cloned().unwrap_or_default();
                        let _ = app.emit(
                            "rc-media",
                            MediaEvent {
                                codec: CODEC_JPEG,
                                seq: p.get("seq").and_then(|x| x.as_i64()).unwrap_or(0) as u32,
                                width: p.get("width").and_then(|x| x.as_i64()).unwrap_or(0) as u16,
                                height: p.get("height").and_then(|x| x.as_i64()).unwrap_or(0) as u16,
                                pts_ms: p.get("ts").and_then(|x| x.as_i64()).unwrap_or(0) as u32,
                                flags: 1,
                                data: p.get("data").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                            },
                        );
                        frames += 1;
                        rate_frames += 1;
                    }
                    "rc_stop" => {
                        emit_status(&app, "stopped", "对端结束会话", &session_id);
                        return;
                    }
                    _ => {}
                }
            }
            Ok(Frame::Timeout) => { /* 读超时：轮询出站 */ }
            Ok(Frame::Close) => {
                emit_status(&app, "stopped", "连接关闭", &session_id);
                return;
            }
            Err(_) => { /* 读错误：轮询出站，下轮再试 */ }
        }

        // 出站消息（输入 / QoS / stop）
        while let Ok(msg) = rx.try_recv() {
            match msg {
                OutMsg::Input(v) => {
                    let env = serde_json::json!({
                        "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                        "type": "rc_input", "seq": 1, "payload": v
                    });
                    let _ = ws.send_text(env.to_string().as_bytes());
                }
                OutMsg::Qos(v) => {
                    let env = serde_json::json!({
                        "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                        "type": "rc_qos", "seq": 1, "payload": v
                    });
                    let _ = ws.send_text(env.to_string().as_bytes());
                }
                OutMsg::Stop => stop.store(true, Ordering::SeqCst),
            }
        }

        // 周期任务：RTT 探测（2s）+ 统计事件（1s）
        if last_ping.elapsed() >= Duration::from_secs(2) && pending_ping.is_none() {
            last_ping = Instant::now();
            pending_ping = Some(Instant::now());
            let env = serde_json::json!({
                "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                "type": "rc_ping", "seq": 1,
                "payload": { "session_id": session_id, "t0": now_ms() }
            });
            let _ = ws.send_text(env.to_string().as_bytes());
        }
        if last_rate.elapsed() >= Duration::from_secs(1) {
            let fps = rate_frames as f64 / last_rate.elapsed().as_secs_f64();
            let kbps = rate_bytes * 8 / 1024 / last_rate.elapsed().as_secs_f64().max(0.001) as u64;
            rate_frames = 0;
            rate_bytes = 0;
            last_rate = Instant::now();
            use tauri::Emitter;
            let _ = app.emit(
                "rc-qos",
                QosStatsEvent {
                    fps_recv: (fps * 10.0).round() / 10.0,
                    loss_pct: loss.loss_pct(),
                    kbps_recv: kbps,
                    rtt_ms,
                    codec: selected_codec.unwrap_or(0),
                    transport: transport_used.clone(),
                },
            );
            if decoder_pending > 0 {
                emit_status(
                    &app,
                    "notice",
                    &format!("等待关键帧重建解码器（已跳过 {} 帧，成功 {} 帧）", decoder_pending, decoded_ok),
                    &session_id,
                );
            }
        }
    }
}

/// 编解码器选择：原生码流优先（带宽最优），仅在**主端解码器可用**时成立；
/// 否则 JPEG → RAW_RGBA。与 rc.html 的选择口径一致，避免两端策略漂移。
fn pick_codec(caps: &Caps, opts: &SessionOpts) -> u8 {
    let has = |c: u8| caps.codecs.contains(&(c as i64));
    if !opts.decoder_lib.is_empty() {
        if has(CODEC_H264) && decoder::probe(&opts.decoder_lib, "h264") {
            return CODEC_H264;
        }
        if has(CODEC_H265) && decoder::probe(&opts.decoder_lib, "h265") {
            return CODEC_H265;
        }
    }
    if has(CODEC_JPEG) {
        return CODEC_JPEG;
    }
    if has(CODEC_RAW_RGBA) {
        return CODEC_RAW_RGBA;
    }
    CODEC_JPEG
}

#[allow(clippy::too_many_arguments)]
fn handle_frame(
    app: &tauri::AppHandle,
    f: MediaFrame,
    native: &mut Option<decoder::NativeDecoder>,
    opts: &SessionOpts,
    loss: &mut LossCounter,
    frames: &mut u64,
    rate_frames: &mut u64,
    bytes: &mut u64,
    rate_bytes: &mut u64,
    decoder_pending: &mut u64,
    decoded_ok: &mut u64,
    selected_codec: &mut Option<u8>,
) {
    use tauri::Emitter;
    loss.observe(f.seq);
    *frames += 1;
    *rate_frames += 1;
    let n = f.payload.len() as u64 + 24;
    *bytes += n;
    *rate_bytes += n;
    if selected_codec.is_none() {
        *selected_codec = Some(f.codec);
    }

    // 原生码流 → 解码为 RGBA 后渲染（与 rc.html WebCodecs 路径同语义）
    if f.codec == CODEC_H264 || f.codec == CODEC_H265 {
        let name = decoder::codec_name(f.codec).unwrap_or("h264");
        if native.is_none() {
            *native = decoder::NativeDecoder::try_load(&opts.decoder_lib, name, f.width as u32, f.height as u32);
        }
        // 分辨率变化 → 重建解码器（会话中途降采样/换屏）
        if let Some(d) = native.as_ref() {
            if d.width() != f.width as u32 || d.height() != f.height as u32 {
                *native = decoder::NativeDecoder::try_load(&opts.decoder_lib, name, f.width as u32, f.height as u32);
            }
        }
        match native.as_mut().and_then(|d| d.decode(&f.payload)) {
            Some(rgba) => {
                *decoded_ok += 1;
                let _ = app.emit(
                    "rc-media",
                    MediaEvent {
                        codec: CODEC_RAW_RGBA,
                        seq: f.seq,
                        width: f.width,
                        height: f.height,
                        pts_ms: f.pts_ms,
                        flags: f.flags,
                        data: b64(&rgba),
                    },
                );
            }
            None => {
                // 诚实失败：不产出黑帧，等下一个关键帧
                *decoder_pending += 1;
            }
        }
        return;
    }

    let _ = app.emit(
        "rc-media",
        MediaEvent {
            codec: f.codec,
            seq: f.seq,
            width: f.width,
            height: f.height,
            pts_ms: f.pts_ms,
            flags: f.flags,
            data: b64(&f.payload),
        },
    );
}

// 4B 长度前缀出站（P2P 直连复用）
#[allow(dead_code)]
fn direct_frame(body: &[u8]) -> Vec<u8> {
    frame_bytes(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(v: Vec<i64>) -> Caps {
        Caps { codecs: v }
    }

    #[test]
    fn pick_codec_prefers_jpeg_without_decoder() {
        let o = SessionOpts { decoder_lib: String::new(), ..Default::default() };
        assert_eq!(pick_codec(&caps(vec![0, 1, 2]), &o), CODEC_JPEG);
    }

    #[test]
    fn pick_codec_falls_back_to_raw_when_no_jpeg() {
        let o = SessionOpts { decoder_lib: String::new(), ..Default::default() };
        assert_eq!(pick_codec(&caps(vec![0]), &o), CODEC_RAW_RGBA);
    }

    #[test]
    fn pick_codec_ignores_native_when_lib_missing() {
        let o = SessionOpts {
            decoder_lib: "/nonexistent/libyuduan_dec_vt.dylib".into(),
            ..Default::default()
        };
        // 库不存在 → probe 失败 → 不应选原生码流
        assert_eq!(pick_codec(&caps(vec![0, 1, 2, 3]), &o), CODEC_JPEG);
    }
}
