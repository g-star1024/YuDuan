// 御端主端 —— P2P 直连媒体（§8.3 ICE-lite 主端侧，T-3）。
//
// 与副端 rc_media::dial_direct/rc.rs::try_candidate 对称：
//   主端监听 TCP → 收集本机非回送 IPv4 候选 → rc_offer（批量）/ rc_candidate（增量）
//   经中枢中继广播 → 被控端逐个 dial，成功即把媒体流从「中枢中继」升级为「直连」。
// 媒体分帧复用 media.rs 的 FrameAssembler（4B 小端长度前缀 + 帧体），与副端同构。
//
// 铁律 1 约束：监听地址仅用于 LAN 直连，不做任何公网穿透/中继。
// 铁律 2 约束：候选枚举三平台各自实现（Unix getifaddrs / Windows GetIpAddrTable）。

use crate::media::{FrameAssembler, MediaFrame};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// 主端媒体监听（P2P 直连的接受方）。
pub struct P2pListener {
    ln: TcpListener,
    /// 监听绑定的地址（用于判定「具体 IP 绑定」还是「通配绑定」）。
    bind: SocketAddr,
}

impl P2pListener {
    /// 绑定监听地址（"0.0.0.0:0" = 通配 + 系统分配端口）。
    pub fn bind(addr: &str) -> Result<Self, String> {
        let ln = TcpListener::bind(addr).map_err(|e| format!("P2P 监听 {} 失败: {}", addr, e))?;
        ln.set_nonblocking(true)
            .map_err(|e| format!("设置非阻塞失败: {}", e))?;
        let bind = ln.local_addr().map_err(|e| format!("取监听地址失败: {}", e))?;
        Ok(P2pListener { ln, bind })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.bind
    }

    /// 收集候选地址（ICE-lite 主端侧）：
    ///   - 绑定具体 IP（如 127.0.0.1）→ 唯一候选即该地址（拨别的地址必然被拒）；
    ///   - 通配绑定（0.0.0.0 / [::]）→ 枚举本机非回送网卡 IPv4 + 监听端口。
    /// 与 dev/cmd/p2pviewer sendCandidates 同口径，保证两端行为一致。
    pub fn candidates(&self) -> Vec<String> {
        let port = self.bind.port();
        match self.bind.ip() {
            IpAddr::V4(v4) if !v4.is_unspecified() => vec![format!("{}:{}", v4, port)],
            IpAddr::V6(v6) if !v6.is_unspecified() => vec![format!("[{}]:{}", v6, port)],
            _ => local_ipv4s()
                .into_iter()
                .map(|ip| format!("{}:{}", ip, port))
                .collect(),
        }
    }

    /// 非阻塞 accept：无连接到来返回 Ok(None)。
    pub fn try_accept(&self) -> Result<Option<TcpStream>, String> {
        match self.ln.accept() {
            Ok((stream, _peer)) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .map_err(|e| format!("设置读超时失败: {}", e))?;
                Ok(Some(stream))
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(format!("accept 失败: {}", e)),
        }
    }
}

/// 枚举本机非回送 IPv4（仅 IPv4：媒体环当前按 IPv4 拨号，与 p2pviewer 一致）。
#[cfg(unix)]
fn local_ipv4s() -> Vec<Ipv4Addr> {
    use libc::{freeifaddrs, getifaddrs, ifaddrs, sockaddr_in, AF_INET};
    const IFF_UP: i32 = 0x1;
    const IFF_LOOPBACK: i32 = 0x8;
    let mut out: Vec<Ipv4Addr> = Vec::new();
    unsafe {
        let mut ifap: *mut ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let flags = (*cur).ifa_flags as i32;
            let addr = (*cur).ifa_addr;
            if flags & IFF_UP != 0 && flags & IFF_LOOPBACK == 0 && !addr.is_null() {
                if (*addr).sa_family as i32 == AF_INET {
                    let sin = addr as *const sockaddr_in;
                    let oct = (*sin).sin_addr.s_addr.to_ne_bytes();
                    let ip = Ipv4Addr::new(oct[0], oct[1], oct[2], oct[3]);
                    if !ip.is_loopback() && !out.contains(&ip) {
                        out.push(ip);
                    }
                }
            }
            cur = (*cur).ifa_next;
        }
        freeifaddrs(ifap);
    }
    out
}

/// Windows：经 IP Helper 的 GetIpAddrTable 取本机 IPv4（结构体扁平，布局可靠）。
#[cfg(windows)]
fn local_ipv4s() -> Vec<Ipv4Addr> {
    #[repr(C)]
    struct MibIpAddrRow {
        addr: u32,
        index: u32,
        mask: u32,
        bcast: u32,
        reasm: u32,
        unused1: u16,
        wtype: u16,
    }
    #[repr(C)]
    struct MibIpAddrTable {
        num: u32,
        rows: [MibIpAddrRow; 1],
    }
    extern "system" {
        fn GetIpAddrTable(table: *mut MibIpAddrTable, size: *mut u32, order: i32) -> u32;
    }
    let mut out: Vec<Ipv4Addr> = Vec::new();
    unsafe {
        let mut size: u32 = 0;
        // 首次调用取所需缓冲（返回 ERROR_INSUFFICIENT_BUFFER=122 属预期）
        let _ = GetIpAddrTable(std::ptr::null_mut(), &mut size, 0);
        if size == 0 {
            return out;
        }
        let cap = (size as usize + 256).max(1024);
        let mut buf = vec![0u8; cap];
        let ret = GetIpAddrTable(buf.as_mut_ptr() as *mut MibIpAddrTable, &mut size, 0);
        if ret != 0 {
            return out;
        }
        let table = buf.as_ptr() as *const MibIpAddrTable;
        let n = (*table).num as usize;
        let rows = std::ptr::addr_of!((*table).rows) as *const MibIpAddrRow;
        for i in 0..n {
            let r = *rows.add(i);
            let ip = Ipv4Addr::from(r.addr.to_ne_bytes());
            if !ip.is_loopback() && !ip.is_unspecified() && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// 直连媒体接收线程：按 4B 长度前缀分帧 → 解析 24B 帧头 → 送主循环。
/// 线程在连接断开或 stop 置位时退出（不 panic，不静默吞错）。
pub fn spawn_receiver(mut stream: TcpStream, tx: Sender<MediaFrame>, stop: Arc<AtomicBool>, tag: &'static str) {
    std::thread::spawn(move || {
        let mut asm = FrameAssembler::new();
        let mut buf = [0u8; 65536];
        while !stop.load(Ordering::SeqCst) {
            match stream.read(&mut buf) {
                Ok(0) => break, // 对端关闭
                Ok(n) => match asm.feed(&buf[..n]) {
                    Ok(frames) => {
                        for body in frames {
                            match MediaFrame::parse(&body) {
                                Ok(f) => {
                                    if tx.send(f).is_err() {
                                        return;
                                    }
                                }
                                Err(e) => eprintln!("[{}] P2P 帧解析失败: {}", tag, e),
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[{}] P2P 分帧错位: {}", tag, e);
                        break;
                    }
                },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => {
                    eprintln!("[{}] P2P 读失败: {}", tag, e);
                    break;
                }
            }
        }
    });
}

/// rc_offer 信封（批量候选，§8.3 ICE-lite）。
pub fn offer_envelope(session_id: &str, candidates: &[String]) -> String {
    envelope(
        "rc_offer",
        serde_json::json!({ "session_id": session_id, "candidates": candidates }),
    )
}

/// rc_candidate 信封（增量单个候选）。
pub fn candidate_envelope(session_id: &str, candidate: &str) -> String {
    envelope(
        "rc_candidate",
        serde_json::json!({ "session_id": session_id, "candidate": candidate }),
    )
}

fn envelope(t: &str, payload: serde_json::Value) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    serde_json::json!({
        "ver": 1, "msg_id": format!("m{}", now), "ts": now as i64,
        "type": t, "seq": 1, "payload": payload
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{frame_bytes, MediaFrame, CODEC_JPEG};

    #[test]
    fn listener_binds_and_collects_candidates() {
        let ln = P2pListener::bind("0.0.0.0:0").unwrap();
        let port = ln.local_addr().port();
        assert!(port > 0, "系统分配端口应 > 0");
        let cands = ln.candidates();
        assert!(!cands.is_empty(), "通配绑定应枚举到 ≥1 个候选");
        for c in &cands {
            let (ip, p) = c.rsplit_once(':').expect("候选格式应为 ip:port");
            assert_eq!(p.parse::<u16>().unwrap(), port);
            assert!(ip.parse::<Ipv4Addr>().is_ok(), "候选应为 IPv4: {}", c);
            assert!(!ip.starts_with("127."), "候选不应含回送地址: {}", c);
        }
    }

    #[test]
    fn specific_bind_yields_single_candidate() {
        let ln = P2pListener::bind("127.0.0.1:0").unwrap();
        let cands = ln.candidates();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0], format!("127.0.0.1:{}", ln.local_addr().port()));
    }

    #[test]
    fn local_ipv4s_excludes_loopback() {
        for ip in local_ipv4s() {
            assert!(!ip.is_loopback());
            assert!(!ip.is_unspecified());
        }
    }

    #[test]
    fn receiver_receives_length_prefixed_frames() {
        // 起一个监听 + 一个客户端，客户端按 4B 长度前缀发 3 帧，验证收端还原
        let ln = P2pListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();

        std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for seq in 0..3u32 {
                let f = MediaFrame {
                    version: 1,
                    stream: 0,
                    codec: CODEC_JPEG,
                    flags: 1,
                    seq,
                    width: 64,
                    height: 48,
                    pts_ms: seq * 33,
                    payload: vec![0xFF, 0xD8, 0xFF, 0xD9],
                };
                use std::io::Write;
                s.write_all(&frame_bytes(&f.encode())).unwrap();
            }
            std::thread::sleep(Duration::from_millis(300));
        });

        // 轮询 accept（非阻塞）
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut stream = None;
        while std::time::Instant::now() < deadline {
            if let Some(s) = ln.try_accept().unwrap() {
                stream = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let s = stream.expect("应在 5s 内接到直连");
        spawn_receiver(s, tx, stop2, "test");

        let mut got = 0;
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < dl && got < 3 {
            if let Ok(f) = rx.recv_timeout(Duration::from_millis(100)) {
                assert_eq!(f.seq, got as u32);
                assert_eq!(f.width, 64);
                assert_eq!(f.payload.len(), 4);
                got += 1;
            }
        }
        stop.store(true, Ordering::SeqCst);
        assert_eq!(got, 3, "应收满 3 帧");
    }

    #[test]
    fn offer_and_candidate_envelopes_shape() {
        let o = offer_envelope("s1", &["192.168.1.5:19500".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&o).unwrap();
        assert_eq!(v["type"], "rc_offer");
        assert_eq!(v["payload"]["session_id"], "s1");
        assert_eq!(v["payload"]["candidates"][0], "192.168.1.5:19500");

        let c = candidate_envelope("s1", "10.0.0.2:19501");
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["type"], "rc_candidate");
        assert_eq!(v["payload"]["candidate"], "10.0.0.2:19501");
    }
}
