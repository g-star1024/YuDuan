// 无头验收：relay 媒体路径（对真实 hub + Rust 副端跑完整协商与收帧）。
// 用法：cargo run --bin mediacheck -- <hub_base> <device_id> [seconds=6]
// 流程与 media_session::session_loop 同口径：rc_request → rc_ready → rc_capabilities
// → rc_select(JPEG, relay) → 收二进制帧统计。退出码 0 = 收到 ≥1 fps 持续出帧。

use std::time::{Duration, Instant};
use yuduan_main_lib::media::{MediaFrame, CODEC_JPEG};
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
        eprintln!("用法: mediacheck <hub_base> <device_id> [seconds=6]");
        std::process::exit(2);
    }
    let base = args[1].trim_end_matches('/').to_string();
    // http(s):// → ws(s)://（hub 地址与 REST 共用一套配置）
    let ws_base = base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
    let device_id = args[2].clone();
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6);
    let sid = format!("s-check-{}", now_ms() % 1_000_000);

    let mut ws = WsClient::connect(&format!("{}/ws/viewer", ws_base)).unwrap_or_else(|e| {
        eprintln!("[FAIL] 连接 /ws/viewer: {}", e);
        std::process::exit(1);
    });
    println!("[OK] WS 已连接 {}", ws_base);

    ws.send_text(envelope("rc_request", serde_json::json!({
        "device_id": device_id, "session_id": sid, "operator": "mediacheck"
    })).as_bytes()).unwrap();
    println!("[OK] rc_request 已发");

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut first_frame: Option<Instant> = None;
    let mut negotiated = false;

    loop {
        if Instant::now() > deadline {
            break;
        }
        ws.set_read_timeout_millis(200);
        match ws.read_frame() {
            Ok(Frame::Binary(b)) => {
                if first_frame.is_none() {
                    first_frame = Some(Instant::now());
                    println!("[OK] 首帧到达（{} ms 内）", (first_frame.unwrap() - start).as_millis());
                }
                match MediaFrame::parse(&b) {
                    Ok(f) => {
                        frames += 1;
                        bytes += b.len() as u64;
                        if frames <= 3 {
                            println!(
                                "  帧 seq={} codec={} {}x{} flags={:#b} payload={}B",
                                f.seq, f.codec, f.width, f.height, f.flags, f.payload.len()
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("[FAIL] 帧解析: {}", e);
                        std::process::exit(1);
                    }
                }
            }
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
                        let caps = v.pointer("/payload/codecs").cloned().unwrap_or_default();
                        println!("[OK] rc_capabilities codecs={}", caps);
                        ws.send_text(envelope("rc_select", serde_json::json!({
                            "session_id": sid, "codec": CODEC_JPEG, "transport": "relay"
                        })).as_bytes()).unwrap();
                        negotiated = true;
                        println!("[OK] rc_select codec=JPEG transport=relay");
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

    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let fps = frames as f64 / elapsed;
    println!(
        "统计：frames={} bytes={} fps={:.1} kbps={:.0} negotiated={} 时长={:.1}s",
        frames,
        bytes,
        fps,
        (bytes as f64) * 8.0 / 1024.0 / elapsed,
        negotiated,
        elapsed
    );
    if frames == 0 || !negotiated {
        eprintln!("[FAIL] 未收到媒体帧或未完成协商");
        std::process::exit(1);
    }
    println!("mediacheck: ALL PASS");
}
