// 御端 Rust 副端 —— 指令验签（铁律 4：关键指令服务端签名、副端验签）。
// 规范串与 Go 中枢一致：msg_id|ts|seq|payload_json|nonce，HMAC-SHA256 十六进制。
// 防重放：nonce 去重（有界环）+ 时间窗 ±300s（与 Go VerifyCommand maxSkew 一致）。

use crate::sha256::{ct_eq, hex_encode, hmac_sha256};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_SKEW_SECS: i64 = 300;

/// nonce 防重放器（有界环，容量满时淘汰最旧）。
pub struct NonceGuard {
    seen: Mutex<VecDeque<String>>,
    cap: usize,
}

impl NonceGuard {
    pub fn new(cap: usize) -> Self {
        NonceGuard {
            seen: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }
    /// 返回 true 表示首次见到（放行）；false 表示重放。
    pub fn check_and_insert(&self, nonce: &str) -> bool {
        if nonce.is_empty() {
            return false; // 无 nonce 一律拒绝（服务端必带）
        }
        let mut seen = self.seen.lock().unwrap();
        if seen.iter().any(|n| n == nonce) {
            return false;
        }
        if seen.len() >= self.cap {
            seen.pop_front();
        }
        seen.push_back(nonce.to_string());
        true
    }
}

/// 从信封原文中提取 payload 的原始 JSON 文本（签名按原文计算，不能重新序列化）。
pub fn extract_raw_payload(frame: &str) -> Option<&str> {
    let key = "\"payload\":";
    let idx = frame.find(key)?;
    let mut i = idx + key.len();
    let bytes = frame.as_bytes();
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'{' {
        return None;
    }
    let start = i;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&frame[start..=i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn json_str_field(env: &crate::Json, key: &str) -> String {
    env.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn json_int_field(env: &crate::Json, key: &str) -> Option<i64> {
    env.get(key).and_then(|v| v.as_i64())
}

/// 验签结果。
pub enum VerifyOutcome {
    Ok,
    NoSecret,          // 未配置 secret：放行（开发态）
    BadSignature,      // 签名不匹配
    Replayed,          // nonce 重放
    TimeSkew,          // 时间窗超限
    Malformed,         // 字段缺失/无法解析
}

/// 校验一条指令信封。secret 为空时返回 NoSecret（不强制）。
pub fn verify_command(
    secret: &str,
    frame: &str,
    env: &crate::Json,
    nonce_guard: &NonceGuard,
) -> VerifyOutcome {
    if secret.is_empty() {
        return VerifyOutcome::NoSecret;
    }
    let msg_id = json_str_field(env, "msg_id");
    let nonce = json_str_field(env, "nonce");
    let sig = json_str_field(env, "sig");
    let ts = match json_int_field(env, "ts") {
        Some(t) => t,
        None => return VerifyOutcome::Malformed,
    };
    let seq = match json_int_field(env, "seq") {
        Some(s) => s,
        None => return VerifyOutcome::Malformed,
    };
    let payload_raw = match extract_raw_payload(frame) {
        Some(p) => p,
        None => return VerifyOutcome::Malformed,
    };
    if msg_id.is_empty() || nonce.is_empty() || sig.is_empty() {
        return VerifyOutcome::Malformed;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if (now - ts).abs() > MAX_SKEW_SECS {
        return VerifyOutcome::TimeSkew;
    }
    let canonical = format!("{}|{}|{}|{}|{}", msg_id, ts, seq, payload_raw, nonce);
    let expected = hex_encode(&hmac_sha256(secret.as_bytes(), canonical.as_bytes()));
    if !ct_eq(expected.as_bytes(), sig.as_bytes()) {
        return VerifyOutcome::BadSignature;
    }
    if !nonce_guard.check_and_insert(&nonce) {
        return VerifyOutcome::Replayed;
    }
    VerifyOutcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::{hex_encode, hmac_sha256};

    fn sign(secret: &str, msg_id: &str, ts: i64, seq: i64, payload: &str, nonce: &str) -> String {
        let canonical = format!("{}|{}|{}|{}|{}", msg_id, ts, seq, payload, nonce);
        hex_encode(&hmac_sha256(secret.as_bytes(), canonical.as_bytes()))
    }

    #[test]
    fn extract_raw_payload_balances_braces() {
        let frame = r#"{"ver":1,"msg_id":"m1","ts":1,"type":"command","seq":1,"payload":{"action":"shell","command":"echo }{"},"nonce":"n","sig":"s"}"#;
        let raw = extract_raw_payload(frame).unwrap();
        assert_eq!(raw, r#"{"action":"shell","command":"echo }{"}"#);
    }

    #[test]
    fn verify_ok_and_replay() {
        let secret = "diagsecret";
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let payload = r#"{"action":"shell","task_id":"t1","command":"echo hi"}"#;
        let sig = sign(secret, "m1", ts, 1, payload, "nonce-1");
        let frame = format!(
            r#"{{"ver":1,"msg_id":"m1","ts":{},"type":"command","seq":1,"payload":{},"nonce":"nonce-1","sig":"{}"}}"#,
            ts, payload, sig
        );
        let env = crate::parse_json(frame.as_bytes()).unwrap();
        let guard = NonceGuard::new(16);
        assert!(matches!(verify_command(secret, &frame, &env, &guard), VerifyOutcome::Ok));
        // 同一帧重放 → 拒绝
        assert!(matches!(
            verify_command(secret, &frame, &env, &guard),
            VerifyOutcome::Replayed
        ));
    }

    #[test]
    fn verify_bad_signature_and_skew() {
        let secret = "diagsecret";
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let payload = r#"{"action":"shell"}"#;
        // 错误签名
        let frame = format!(
            r#"{{"msg_id":"m1","ts":{},"type":"command","seq":1,"payload":{},"nonce":"n2","sig":"deadbeef"}}"#,
            ts, payload
        );
        let env = crate::parse_json(frame.as_bytes()).unwrap();
        let guard = NonceGuard::new(16);
        assert!(matches!(
            verify_command(secret, &frame, &env, &guard),
            VerifyOutcome::BadSignature
        ));
        // 时间偏移过大
        let sig = sign(secret, "m2", ts - 100000, 1, payload, "n3");
        let frame = format!(
            r#"{{"msg_id":"m2","ts":{},"type":"command","seq":1,"payload":{},"nonce":"n3","sig":"{}"}}"#,
            ts - 100000, payload, sig
        );
        let env = crate::parse_json(frame.as_bytes()).unwrap();
        assert!(matches!(
            verify_command(secret, &frame, &env, &guard),
            VerifyOutcome::TimeSkew
        ));
        // 未配置 secret → 放行
        let frame = format!(
            r#"{{"msg_id":"m3","ts":{},"type":"command","seq":1,"payload":{},"nonce":"n4","sig":"x"}}"#,
            ts, payload
        );
        let env = crate::parse_json(frame.as_bytes()).unwrap();
        assert!(matches!(verify_command("", &frame, &env, &guard), VerifyOutcome::NoSecret));
    }
}
