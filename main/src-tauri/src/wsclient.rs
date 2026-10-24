// 御端主端 —— 纯 std WebSocket 客户端（控制面 /ws/viewer，T-2 起承载体）。
// 与副端 WsClient（rust/agent/src/main.rs）同构：握手 + 客户端掩码 + 帧编解码。
// 仅 ws://（明文）；wss 于 T-4 mTLS 里程碑接入。

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub enum Frame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    /// 读超时（轮询窗口内无数据）——不是关闭，调用方应继续循环。
    Timeout,
    Close,
}

pub struct WsClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl WsClient {
    /// 连接 ws://host[:port]/path 并完成握手。
    pub fn connect(url: &str) -> Result<Self, String> {
        let rest = url.strip_prefix("ws://").ok_or_else(|| format!("仅支持 ws:// URL: {}", url))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rfind(':') {
            Some(i) => (&authority[..i], authority[i + 1..].parse::<u16>().map_err(|e| e.to_string())?),
            None => (authority, 80u16),
        };
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {} failed: {}", addr, e))?;
        stream.set_read_timeout(None).ok();
        stream.set_write_timeout(None).ok();
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let mut ws = WsClient { reader: BufReader::new(stream), writer };

        // 随机 Sec-WebSocket-Key = base64(16 字节)（握手用，非密码学即可）
        let key = base64_encode(&lcg_bytes(16));
        let handshake = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        ws.writer
            .write_all(handshake.as_bytes())
            .map_err(|e| format!("handshake write failed: {}", e))?;
        ws.writer.flush().ok();

        // 读 101 响应头直至空行
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = ws.reader.read(&mut byte).map_err(|e| format!("handshake read failed: {}", e))?;
            if n == 0 {
                return Err("connection closed during handshake".into());
            }
            header.push(byte[0]);
            if header.len() >= 4 && &header[header.len() - 4..] == b"\r\n\r\n" {
                break;
            }
            if header.len() > 64 * 1024 {
                return Err("handshake header too large".into());
            }
        }
        let header_str = String::from_utf8_lossy(&header).to_string();
        if !header_str.contains("101") {
            return Err(format!(
                "handshake failed: {}",
                header_str.lines().next().unwrap_or("")
            ));
        }
        Ok(ws)
    }

    pub fn set_read_timeout(&self, secs: Option<u64>) {
        self.reader
            .get_ref()
            .set_read_timeout(secs.map(Duration::from_secs))
            .ok();
    }

    /// 会话线程轮询用的毫秒级读超时。
    pub fn set_read_timeout_millis(&self, ms: u64) {
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(ms)))
            .ok();
    }

    /// 发送掩码文本帧（客户端必须加掩码）。
    pub fn send_text(&mut self, data: &[u8]) -> Result<(), String> {
        self.send_frame(0x1, data)
    }

    /// 发送掩码二进制帧。
    pub fn send_binary(&mut self, data: &[u8]) -> Result<(), String> {
        self.send_frame(0x2, data)
    }

    fn send_frame(&mut self, opcode: u8, data: &[u8]) -> Result<(), String> {
        let len = data.len();
        let mut header = vec![0x80 | opcode]; // FIN + opcode
        let mask_bit = 0x80u8;
        if len < 126 {
            header.push((len as u8) | mask_bit);
        } else if len <= u16::MAX as usize {
            header.push(126 | mask_bit);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            header.push(127 | mask_bit);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask = lcg_bytes(4);
        header.extend_from_slice(&mask);
        let mut masked = Vec::with_capacity(len);
        for (i, &b) in data.iter().enumerate() {
            masked.push(b ^ mask[i % 4]);
        }
        self.writer
            .write_all(&header)
            .and_then(|_| self.writer.write_all(&masked))
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("ws send failed: {}", e))
    }

    /// 读取一帧；自动处理 ping → pong、close → Frame::Close、读超时 → Frame::Timeout。
    pub fn read_frame(&mut self) -> Result<Frame, String> {
        loop {
            let mut b0 = [0u8; 1];
            match self.reader.read_exact(&mut b0) {
                Ok(_) => {}
                Err(e) => {
                    // 超时/WouldBlock = 轮询窗口无数据；其余（连接断开等）视为 Close
                    return Ok(match e.kind() {
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Frame::Timeout,
                        _ => Frame::Close,
                    });
                }
            }
            let mut b1 = [0u8; 1];
            self.reader
                .read_exact(&mut b1)
                .map_err(|e| format!("ws read failed: {}", e))?;
            let opcode = b0[0] & 0x0f;
            let masked = (b1[0] & 0x80) != 0;
            let mut len = (b1[0] & 0x7f) as usize;
            if len == 126 {
                let mut ext = [0u8; 2];
                self.reader.read_exact(&mut ext).map_err(|e| e.to_string())?;
                len = u16::from_be_bytes(ext) as usize;
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.reader.read_exact(&mut ext).map_err(|e| e.to_string())?;
                len = u64::from_be_bytes(ext) as usize;
            }
            let mut mask = [0u8; 4];
            if masked {
                self.reader.read_exact(&mut mask).map_err(|e| e.to_string())?;
            }
            let mut payload = vec![0u8; len];
            self.reader
                .read_exact(&mut payload)
                .map_err(|e| format!("ws read payload failed: {}", e))?;
            if masked {
                for i in 0..len {
                    payload[i] ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Ok(Frame::Close),
                0x9 => {
                    // ping → pong
                    let _ = self.send_frame(0xA, &payload);
                    continue;
                }
                0x1 => return Ok(Frame::Text(payload)),
                0x2 => return Ok(Frame::Binary(payload)),
                _ => continue, // 忽略其他控制帧
            }
        }
    }
}

// —— 测试辅助：LCG 伪随机（与副端同款，握手掩码非密码学用途） ——

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 16
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() & 0xFF) as u8).collect()
    }
}

fn lcg_bytes(n: usize) -> Vec<u8> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(42)
        ^ (std::process::id() as u64) << 32;
    Lcg(seed | 1).bytes(n)
}

pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdBufReader};
    use std::net::TcpListener;

    /// 端到端：假 WS 服务端（std 线程）→ 握手 + 文本收发回环。
    #[test]
    fn ws_handshake_and_echo_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut r = StdBufReader::new(sock.try_clone().unwrap());
            // 读握手头
            let mut req = String::new();
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                req.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(req.contains("Upgrade: websocket"));
            // 回 101
            let mut w = sock;
            w.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: x\r\n\r\n",
            )
            .unwrap();
            // 读一帧（掩码文本）并原样回发（不掩码）
            let mut b0 = [0u8; 1];
            let mut b1 = [0u8; 1];
            r.read_exact(&mut b0).unwrap();
            r.read_exact(&mut b1).unwrap();
            assert_eq!(b0[0] & 0x0f, 0x1);
            assert_ne!(b1[0] & 0x80, 0, "客户端帧必须带掩码");
            let len = (b1[0] & 0x7f) as usize;
            let mut mask = [0u8; 4];
            r.read_exact(&mut mask).unwrap();
            let mut payload = vec![0u8; len];
            r.read_exact(&mut payload).unwrap();
            for i in 0..len {
                payload[i] ^= mask[i % 4];
            }
            assert_eq!(payload, "hello 御端".as_bytes());
            // 服务端不掩码回发
            w.write_all(&[0x81, len as u8]).unwrap();
            w.write_all(&payload).unwrap();
            w.flush().unwrap();
        });

        let mut ws = WsClient::connect(&format!("ws://{}", addr)).unwrap();
        ws.send_text("hello 御端".as_bytes()).unwrap();
        match ws.read_frame().unwrap() {
            Frame::Text(t) => assert_eq!(t, "hello 御端".as_bytes()),
            other => panic!("期望 Text 帧，实际 {:?}", other.is_close()),
        }
        server.join().unwrap();
    }

    impl Frame {
        fn is_close(&self) -> bool {
            matches!(self, Frame::Close)
        }
    }
}
