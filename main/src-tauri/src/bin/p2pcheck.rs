// 无头验收：P2P 直连媒体路径（T-3，对真实 hub + Rust 副端跑 ICE-lite 与收帧/解码）。
//
// 用法：
//   cargo run --bin p2pcheck -- <hub_base> <device_id> [seconds=8] [transport=p2p|relay]
//
// 流程与 media_session::session_loop 同口径：
//   监听 → rc_request → rc_ready → rc_capabilities → rc_select(codec, p2p, media_addr)
//   → rc_offer/rc_candidate（ICE-lite 候选广播）→ 被控端 dial 直连 → 收帧
//   →（原生码流）解码为 RGBA 并校验 w*h*4。
//
// 退出码 0 = 已完成协商 + 收到 ≥1 帧 + （transport=p2p 时）至少 1 帧走直连。

use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use yuduan_main_lib::decoder;
use yuduan_main_lib::media::{MediaFrame, CODEC_H264, CODEC_H265, CODEC_JPEG};
use yuduan_main_lib::p2p::{self, P2pListener};
use yuduan_main_lib::wsclient::{Frame, WsClient};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn envelope(t: &str, payload: serde_json::Value) -> String {
    serde_json::json!({
        "ver": 1, "msg_id": format!("m{}", now_ms()), "ts": now_ms() as i64,
        "type": t, "seq": 1, "payload": payload
    })
    .to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: p2pcheck <hub_base> <device_id> [seconds=8] [transport=p2p]");
        std::process::exit(2);
    }
    let base = args[1].trim_end_matches('/').to_string();
    let ws_base = base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
    let device_id = args[2].clone();
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    let want_transport = args.get(4).cloned().unwrap_or_else(|| "p2p".into());
    let decoder_lib = std::env::var("YUDUAN_DECODER_LIB").unwrap_or_default();
    let bind = std::env::var("YUDUAN_P2P_BIND").unwrap_or_else(|_| "0.0.0.0:0".into());
    let sid = format!("s-p2p-{}", now_ms() % 1_000_000);

    // 1) P2P 监听 + 候选收集
    let mut listener: Option<P2pListener> = None;
    let mut candidates: Vec<String> = Vec::new();
    if want_transport != "relay" {
        match P2pListener::bind(&bind) {
            Ok(ln) => {
                candidates = ln.candidates();
                println!("[OK] P2P 监听 {} 候选={:?}", ln.local_addr(), candidates);
                listener = Some(ln);
            }
            Err(e) => println!("[WARN] P2P 监听失败（将走 relay）: {}", e),
        }
    }

    // 2) 连 hub /ws/viewer
    let mut ws = WsClient::connect(&format!("{}/ws/viewer", ws_base)).unwrap_or_else(|e| {
        eprintln!("[FAIL] 连接 /ws/viewer: {}", e);
        std::process::exit(1);
    });
    println!("[OK] WS 已连接 {}", ws_base);
    ws.send_text(
        envelope(
            "rc_request",
            serde_json::json!({ "device_id": device_id, "session_id": sid, "operator": "p2pcheck" }),
        )
        .as_bytes(),
    )
    .unwrap();
    println!("[OK] rc_request 已发");

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let stop = Arc::new(AtomicBool::new(false));
    let (ftx, frx) = mpsc::channel::<MediaFrame>();

    let mut negotiated = false;
    let mut codec_sel: Option<u8> = None;
    let mut p2p_connected = false;
    let mut frames_p2p = 0u64;
    let mut frames_relay = 0u64;
    let mut bytes_p2p = 0u64;
    let mut first_p2p: Option<Instant> = None;
    let mut decoded = 0u64;
    let mut decode_pending = 0u64;
    let mut native: Option<decoder::NativeDecoder> = None;

    while Instant::now() < deadline {
        // P2P accept + 收帧
        if let Some(ln) = listener.as_ref() {
            if !p2p_connected {
                if let Ok(Some(stream)) = ln.try_accept() {
                    p2p::spawn_receiver(stream, ftx.clone(), stop.clone(), "p2pcheck");
                    p2p_connected = true;
                    println!("[OK] 被控端已直连（{} ms 内）", start.elapsed().as_millis());
                }
            }
            while let Ok(f) = frx.try_recv() {
                if first_p2p.is_none() {
                    first_p2p = Some(Instant::now());
                    println!(
                        "[OK] P2P 首帧 seq={} codec={} {}x{} payload={}B（{} ms 内）",
                        f.seq, f.codec, f.width, f.height, f.payload.len(),
                        (first_p2p.unwrap() - start).as_millis()
                    );
                }
                frames_p2p += 1;
                bytes_p2p += f.payload.len() as u64 + 24;
                if f.codec == CODEC_H264 || f.codec == CODEC_H265 {
                    let name = decoder::codec_name(f.codec).unwrap();
                    if native.is_none() {
                        native = decoder::NativeDecoder::try_load(&decoder_lib, name, f.width as u32, f.height as u32);
                        if native.is_some() {
                            println!("[OK] 原生解码器就绪 codec={} {}x{}", name, f.width, f.height);
                        }
                    }
                    match native.as_mut().and_then(|d| d.decode(&f.payload)) {
                        Some(rgba) => {
                            let expect = f.width as usize * f.height as usize * 4;
                            if rgba.len() != expect {
                                eprintln!("[FAIL] 解码尺寸不符：{} != {}", rgba.len(), expect);
                                std::process::exit(1);
                            }
                            // 非纯色校验（防「假解码」：全 0 缓冲冒充成功）
                            let first = rgba[0];
                            if !rgba.iter().any(|b| *b != first) {
                                eprintln!("[FAIL] 解码输出为纯色（疑似未真实出图）");
                                std::process::exit(1);
                            }
                            decoded += 1;
                        }
                        None => decode_pending += 1,
                    }
                }
            }
        }

        ws.set_read_timeout_millis(200);
        match ws.read_frame() {
            Ok(Frame::Binary(b)) => match MediaFrame::parse(&b) {
                Ok(f) => {
                    if !p2p_connected {
                        frames_relay += 1;
                        if frames_relay <= 2 {
                            println!("  [relay] 帧 seq={} codec={} {}x{} payload={}B",
                                     f.seq, f.codec, f.width, f.height, f.payload.len());
                        }
                    }
                }
                Err(e) => println!("[WARN] 帧解析失败: {}", e),
            },
            Ok(Frame::Text(t)) => {
                let v: serde_json::Value = serde_json::from_slice(&t).unwrap_or_default();
                match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                    "rc_ready" => {
                        let ok = v.pointer("/payload/ok").and_then(|x| x.as_bool()).unwrap_or(false);
                        println!("[{}] rc_ready ok={}", if ok { "OK" } else { "FAIL" }, ok);
                        if !ok {
                            std::process::exit(1);
                        }
                    }
                    "rc_capabilities" => {
                        let codecs: Vec<i64> = v
                            .pointer("/payload/codecs")
                            .and_then(|x| x.as_array())
                            .map(|a| a.iter().filter_map(|y| y.as_i64()).collect())
                            .unwrap_or_default();
                        println!("[OK] rc_capabilities codecs={:?}", codecs);
                        let pick = if !decoder_lib.is_empty() && codecs.contains(&(CODEC_H264 as i64))
                            && decoder::probe(&decoder_lib, "h264")
                        {
                            CODEC_H264
                        } else if !decoder_lib.is_empty() && codecs.contains(&(CODEC_H265 as i64))
                            && decoder::probe(&decoder_lib, "h265")
                        {
                            CODEC_H265
                        } else {
                            CODEC_JPEG
                        };
                        codec_sel = Some(pick);
                        let mut payload = serde_json::json!({
                            "session_id": sid, "codec": pick,
                            "transport": if candidates.is_empty() { "relay" } else { "p2p" }
                        });
                        if !candidates.is_empty() {
                            payload["media_addr"] = serde_json::Value::String(candidates[0].clone());
                            println!("[OK] rc_select codec={} transport=p2p media_addr={}", pick, candidates[0]);
                        } else {
                            println!("[OK] rc_select codec={} transport=relay", pick);
                        }
                        ws.send_text(envelope("rc_select", payload).as_bytes()).unwrap();
                        if !candidates.is_empty() {
                            ws.send_text(p2p::offer_envelope(&sid, &candidates).as_bytes()).unwrap();
                            for c in candidates.iter().skip(1) {
                                ws.send_text(p2p::candidate_envelope(&sid, c).as_bytes()).unwrap();
                            }
                            println!("[OK] 已广播 {} 个 ICE 候选", candidates.len());
                        }
                        negotiated = true;
                    }
                    "rc_event" => println!("[INFO] rc_event: {}", v.pointer("/payload/kind").cloned().unwrap_or_default()),
                    "rc_stop" => break,
                    _ => {}
                }
            }
            Ok(Frame::Timeout) => {}
            Ok(Frame::Close) => break,
            Err(_) => {}
        }
    }

    stop.store(true, Ordering::SeqCst);
    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    println!("\n统计：transport_req={} 协商={} codec={:?} p2p_connected={}",
             want_transport, negotiated, codec_sel, p2p_connected);
    println!("       P2P 帧={} 字节={} fps={:.1} kbps={:.0}",
             frames_p2p, bytes_p2p, frames_p2p as f64 / elapsed,
             (bytes_p2p as f64) * 8.0 / 1024.0 / elapsed);
    println!("       中继帧={}（直连建立后不再渲染）", frames_relay);
    println!("       原生解码成功={} 等待关键帧={}", decoded, decode_pending);

    let mut ok = true;
    if !negotiated {
        eprintln!("[FAIL] 未完成协商");
        ok = false;
    }
    if frames_p2p + frames_relay == 0 {
        eprintln!("[FAIL] 未收到任何媒体帧");
        ok = false;
    }
    if want_transport == "p2p" && frames_p2p == 0 {
        eprintln!("[FAIL] 请求 P2P 但无任何直连帧");
        ok = false;
    }
    if let Some(c) = codec_sel {
        if (c == CODEC_H264 || c == CODEC_H265) && decoded == 0 {
            eprintln!("[FAIL] 选了原生码流但 0 帧解码成功");
            ok = false;
        }
    }
    if !ok {
        std::process::exit(1);
    }
    println!("p2pcheck: ALL PASS");
}
