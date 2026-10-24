// 御端主端 —— 远控媒体会话（T-2：relay 中继路径，§8.3）。
//
// 流程（与 p2pviewer / rc.html 同口径）：
//   1. 连 hub /ws/viewer → 发 rc_request{device_id, session_id, operator}
//   2. 收 rc_ready / rc_capabilities → 自动选编解码器（T-2 阶段主端可渲染 JPEG/RAW_RGBA）
//      → 发 rc_select{codec, transport:"relay"}
//   3. 收二进制 WS 帧（24B 头 + payload）或 rc_frame（JSON base64 兜底）→ 经 Tauri 事件
//      推给前端 canvas（MVP 走 base64，与 rc.html 带宽口径一致；GPU 纹理通道为 T-3 优化项）
//   4. 输入/QoS 经 mpsc 通道由会话线程串行发送（MF 副端的 COM 线程约束不涉及主端，
//      但 hub ws 库非线程安全，统一单线程写）
//   5. RTT 探测（§8.3.11）：每 2s 发 rc_ping，rc_pong 回显 t0 计算 RTT

use crate::media::{frame_bytes, LossCounter, MediaFrame, CODEC_H264, CODEC_H265, CODEC_JPEG, CODEC_RAW_RGBA};
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
    Ping,
    Stop,
}

#[derive(Serialize, Clone)]
pub struct MediaEvent {
    pub codec: u8,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub pts_ms: u32,
    pub flags: u8,
    pub data: String, // base64(payload)；MVP 与 rc.html 带宽口径一致
}

#[derive(Serialize, Clone)]
pub struct StatusEvent {
    pub phase: String,  // requesting / ready / negotiating / streaming / stopped / error
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn b64(data: &[u8]) -> String {
    crate::wsclient::base64_encode(data)
}

/// 启动 relay 会话线程。返回句柄；事件经 Tauri app.emit 推送。
pub fn start_relay(app: tauri::AppHandle, hub_base: String, device_id: String, operator: String) -> Result<SessionHandle, String> {
    // http(s):// → ws(s)://（hub 地址与 REST 共用一套配置）
    let ws_url = format!(
        "{}/ws/viewer",
        hub_base.trim_end_matches('/').replacen("http://", "ws://", 1).replacen("https://", "wss://", 1)
    );
    let session_id = format!("s-{}-{}", std::process::id(), now_ms() % 1_000_000);

    let (tx, rx) = mpsc::channel::<OutMsg>();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = SessionHandle {
        session_id: session_id.clone(),
        stop: stop.clone(),
        tx: Arc::new(Mutex::new(Some(tx))),
    };

    let sid = session_id.clone();
    let stop2 = stop.clone();
    std::thread::spawn(move || {
        session_loop(app, ws_url, device_id, operator, sid, stop2, rx);
    });
    Ok(handle)
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
    let _ = app.emit("rc-status", StatusEvent { phase: phase.into(), detail: detail.into(), session_id: sid.into() });
}

#[derive(Deserialize)]
struct Caps {
    codecs: Vec<i64>,
}

fn session_loop(
    app: tauri::AppHandle,
    ws_url: String,
    device_id: String,
    operator: String,
    session_id: String,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<OutMsg>,
) {
    emit_status(&app, "connecting", &ws_url, &session_id);
    let mut ws = match WsClient::connect(&ws_url) {
        Ok(w) => w,
        Err(e) => {
            emit_status(&app, "error", &format!("连接中枢失败: {}", e), &session_id);
            return;
        }
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
    let mut transport = "relay".to_string();
    let mut loss = LossCounter::default();
    let mut frames: u64 = 0;
    let mut bytes: u64 = 0;
    let mut last_rate = Instant::now();
    let mut rate_frames: u64 = 0;
    let mut rate_bytes: u64 = 0;
    let mut pending_ping: Option<(u64, Instant)> = None; // (t0, sent_at)
    let mut rtt_ms: Option<u64> = None;
    let mut last_ping = Instant::now();
    let mut last_tick = Instant::now();

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

        // 读帧（100ms 读超时轮询出站队列）
        ws.set_read_timeout_millis(100);
        match ws.read_frame() {
            Ok(Frame::Binary(b)) => {
                match MediaFrame::parse(&b) {
                    Ok(f) => {
                        loss.observe(f.seq);
                        frames += 1;
                        rate_frames += 1;
                        bytes += b.len() as u64;
                        rate_bytes += b.len() as u64;
                        if selected_codec.is_none() {
                            selected_codec = Some(f.codec);
                        }
                        let ev = MediaEvent {
                            codec: f.codec,
                            seq: f.seq,
                            width: f.width,
                            height: f.height,
                            pts_ms: f.pts_ms,
                            flags: f.flags,
                            data: b64(&f.payload),
                        };
                        use tauri::Emitter;
                        let _ = app.emit("rc-media", ev);
                    }
                    Err(e) => {
                        emit_status(&app, "error", &format!("媒体帧解析失败: {}", e), &session_id);
                    }
                }
            }
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
                        let caps: Caps = serde_json::from_value(v.get("payload").cloned().unwrap_or_default())
                            .unwrap_or(Caps { codecs: vec![CODEC_JPEG as i64] });
                        // T-2 主端渲染能力：JPEG 优先，RAW_RGBA 兜底；h264/h265 待 T-3 VT 解码
                        let pick = if caps.codecs.contains(&(CODEC_JPEG as i64)) {
                            CODEC_JPEG
                        } else if caps.codecs.contains(&(CODEC_RAW_RGBA as i64)) {
                            CODEC_RAW_RGBA
                        } else if caps.codecs.contains(&(CODEC_H264 as i64)) {
                            CODEC_H264
                        } else if caps.codecs.contains(&(CODEC_H265 as i64)) {
                            CODEC_H265
                        } else {
                            CODEC_JPEG
                        };
                        let sel = serde_json::json!({
                            "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                            "type": "rc_select", "seq": 1,
                            "payload": { "session_id": session_id, "codec": pick, "transport": "relay" }
                        });
                        let _ = ws.send_text(sel.to_string().as_bytes());
                        selected_codec = Some(pick);
                        transport = "relay".into();
                        emit_status(&app, "streaming", &format!("已协商 codec={} relay", pick), &session_id);
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
                        if let Some((t0, sent_at)) = pending_ping.take() {
                            let _ = t0;
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

        // 出站消息（输入 / QoS / ping / stop）
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
                OutMsg::Ping => {
                    let t0 = now_ms();
                    pending_ping = Some((t0, Instant::now()));
                    let env = serde_json::json!({
                        "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                        "type": "rc_ping", "seq": 1,
                        "payload": { "session_id": session_id, "t0": t0 }
                    });
                    let _ = ws.send_text(env.to_string().as_bytes());
                }
                OutMsg::Stop => stop.store(true, Ordering::SeqCst),
            }
        }

        // 周期任务：RTT 探测（2s）+ 统计事件（1s）
        if last_ping.elapsed() >= Duration::from_secs(2) {
            last_ping = Instant::now();
            if let Ok(g) = rx.try_recv() {
                drop(g); // 竞争不常见，忽略
            }
            // 直接发 ping（借用 pending 逻辑）：为避免递归构造，走内部标记
            if pending_ping.is_none() {
                let t0 = now_ms();
                pending_ping = Some((t0, Instant::now()));
                let env = serde_json::json!({
                    "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
                    "type": "rc_ping", "seq": 1,
                    "payload": { "session_id": session_id, "t0": t0 }
                });
                let _ = ws.send_text(env.to_string().as_bytes());
            }
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
                    transport: transport.clone(),
                },
            );
        }
        let _ = (&mut last_tick, &mut frames, &mut bytes); // 保留计数供调试
    }
}

// 4B 长度前缀出站（P2P 直连 T-3 复用）
#[allow(dead_code)]
fn direct_frame(body: &[u8]) -> Vec<u8> {
    frame_bytes(body)
}
