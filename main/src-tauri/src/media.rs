// 御端主端 —— 媒体环主端侧（§8.3）：24B 帧头解析 + 4B 长度前缀分帧。
//
// 帧头布局（protocol.md §8.3.1，小端，固定 24 字节）：
//   [ magic:u32 'YDYF' ][ version:u8=1 ][ stream:u8 ][ codec:u8 ][ flags:u8 ]
//   [ seq:u32 ][ width:u16 ][ height:u16 ][ pts_ms:u32 ][ len:u32 ][ payload:len ]
//
// 传输复用：relay 路径 = WS 二进制帧承载「24B 头 + payload」；
//           P2P 直连 = 4B 小端长度前缀 + 帧体（与副端 send_frame/recv_frame 同构）。

/// 帧魔数：与副端 rc_media.rs FRAME_MAGIC 一致——u32 值 0x59445946
/// （即 "YDYF" 按**大端**读出的数值；线上按小端存储为字节 46 59 44 59）。
pub const MAGIC: u32 = 0x5944_5946;
pub const HEADER_LEN: usize = 24;

pub const STREAM_SCREEN: u8 = 0;
pub const STREAM_INPUT: u8 = 1;
pub const STREAM_AUDIO: u8 = 2;

pub const CODEC_RAW_RGBA: u8 = 0;
pub const CODEC_JPEG: u8 = 1;
pub const CODEC_H264: u8 = 2;
pub const CODEC_H265: u8 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct MediaFrame {
    pub version: u8,
    pub stream: u8,
    pub codec: u8,
    pub flags: u8,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub pts_ms: u32,
    pub payload: Vec<u8>,
}

impl MediaFrame {
    /// 从「24B 头 + payload」完整字节解析。
    pub fn parse(buf: &[u8]) -> Result<MediaFrame, String> {
        if buf.len() < HEADER_LEN {
            return Err(format!("帧过短：{} < {}", buf.len(), HEADER_LEN));
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            return Err(format!("magic 错误：0x{:08X} ≠ YDYF", magic));
        }
        let len = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]) as usize;
        if buf.len() < HEADER_LEN + len {
            return Err(format!("payload 不完整：{} < {}", buf.len() - HEADER_LEN, len));
        }
        Ok(MediaFrame {
            version: buf[4],
            stream: buf[5],
            codec: buf[6],
            flags: buf[7],
            seq: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            width: u16::from_le_bytes([buf[12], buf[13]]),
            height: u16::from_le_bytes([buf[14], buf[15]]),
            pts_ms: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            payload: buf[HEADER_LEN..HEADER_LEN + len].to_vec(),
        })
    }

    /// 编码为「24B 头 + payload」。
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.push(self.version);
        out.push(self.stream);
        out.push(self.codec);
        out.push(self.flags);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.pts_ms.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// flags bit0 = I 帧（关键帧）。
    pub fn is_keyframe(&self) -> bool {
        self.flags & 0b1 != 0
    }
}

/// 丢包统计（§8.3.15 主端侧）：按 seq 缺口统计丢帧率（滑动窗口内）。
#[derive(Default)]
pub struct LossCounter {
    last_seq: Option<u32>,
    lost: u64,
    total: u64,
}

impl LossCounter {
    pub fn observe(&mut self, seq: u32) {
        self.total += 1;
        if let Some(prev) = self.last_seq {
            // seq 回绕安全：差值在 u32 半程内才算前向缺口
            let gap = seq.wrapping_sub(prev);
            if (1..0x8000_0000).contains(&gap) {
                self.lost += (gap - 1) as u64;
            }
        }
        self.last_seq = Some(seq);
    }

    /// 累计丢包率（整数 %，向上取整；与 p2pviewer/rc.html 口径一致）。
    pub fn loss_pct(&self) -> u64 {
        let denom = self.total + self.lost;
        if denom == 0 {
            0
        } else {
            (self.lost * 100 + denom - 1) / denom
        }
    }
}

/// 直连媒体流的分帧读取缓冲（4B 小端长度前缀 + 帧体）。
pub struct FrameAssembler {
    buf: Vec<u8>,
}

impl FrameAssembler {
    pub fn new() -> Self {
        FrameAssembler { buf: Vec::new() }
    }

    /// 喂入网络字节，吐出所有完整帧（每帧 = 4B 长度 + 帧体）。
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let n = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if n > 64 * 1024 * 1024 {
                return Err(format!("帧长异常：{} 字节（>64MiB，流已错位）", n));
            }
            if self.buf.len() < 4 + n {
                break;
            }
            let frame: Vec<u8> = self.buf[4..4 + n].to_vec();
            self.buf.drain(..4 + n);
            out.push(frame);
        }
        Ok(out)
    }
}

/// 出站分帧：4B 小端长度前缀 + 帧体。
pub fn frame_bytes(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame(seq: u32) -> MediaFrame {
        MediaFrame {
            version: 1,
            stream: STREAM_SCREEN,
            codec: CODEC_JPEG,
            flags: 1, // I 帧
            seq,
            width: 1920,
            height: 1080,
            pts_ms: 12345,
            payload: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3],
        }
    }

    #[test]
    fn header_roundtrip() {
        let f = sample_frame(7);
        let bytes = f.encode();
        assert_eq!(bytes.len(), HEADER_LEN + f.payload.len());
        // 线上前 4 字节 = MAGIC 小端 = 46 59 44 59（"YDYF" 大端数值的 LE 存储，与副端一致）
        assert_eq!(&bytes[..4], &[0x46, 0x59, 0x44, 0x59]);
        let g = MediaFrame::parse(&bytes).unwrap();
        assert_eq!(g, f);
        assert!(g.is_keyframe());
        assert_eq!(g.codec, CODEC_JPEG);
        assert_eq!(g.width, 1920);
    }

    #[test]
    fn parse_rejects_bad_magic_and_short() {
        assert!(MediaFrame::parse(&[0u8; 10]).is_err());
        let mut b = sample_frame(1).encode();
        b[0] = b'X';
        assert!(MediaFrame::parse(&b).is_err());
        // len 字段声明大于实际
        let mut b = sample_frame(1).encode();
        b[20..24].copy_from_slice(&9999u32.to_le_bytes());
        assert!(MediaFrame::parse(&b).is_err());
    }

    #[test]
    fn loss_counter_gap_and_wrap() {
        let mut lc = LossCounter::default();
        for s in [0u32, 1, 2, 5, 6] {
            lc.observe(s);
        } // 缺 3、4 → 丢 2
        assert_eq!(lc.loss_pct(), 29); // 2 / 7 = 28.57 → 29（进位）
        // 回绕：u32::MAX → 0 属正常前向
        let mut lc = LossCounter::default();
        lc.observe(u32::MAX);
        lc.observe(0);
        assert_eq!(lc.loss_pct(), 0);
    }

    #[test]
    fn assembler_splits_length_prefixed() {
        let mut fa = FrameAssembler::new();
        let f1 = sample_frame(1).encode();
        let f2 = sample_frame(2).encode();
        let mut wire = frame_bytes(&f1);
        wire.extend(frame_bytes(&f2));
        // 半帧喂入：不应产出
        assert_eq!(fa.feed(&wire[..10]).unwrap().len(), 0);
        let frames = fa.feed(&wire[10..]).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(MediaFrame::parse(&frames[0]).unwrap().seq, 1);
        assert_eq!(MediaFrame::parse(&frames[1]).unwrap().seq, 2);
    }
}
