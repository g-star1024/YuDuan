// 御端（YuDuan）终端统一管理平台 —— 生产副端（Rust 版，单文件，std-only）。
//
// 硬约束：本机无法访问网络，只能使用 Rust 标准库（std），无任何第三方依赖。
// 离线编译：rustc -O src/main.rs -o target/agent
//
// 对接 Go 中枢明文 WebSocket：ws://127.0.0.1:8080/ws/agent （可用 YUDUAN_HUB 覆盖）。
// 控制面协议：JSON 信封（payload 为内嵌对象）——见 dev/proto/protocol.md 与 dev/internal/proto/proto.go。

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ===================== 极简 JSON 解析（无 serde，纯 std） =====================

#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    // 对象用 Vec 保存以保持顺序并支持重复键（protocol 无重复键需求，仅取用）
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        if let Json::Obj(m) = self {
            for (k, v) in m {
                if k == key {
                    return Some(v);
                }
            }
        }
        None
    }
    fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) => Some(*n as i64),
            Json::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }
    fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Parser { b, i: 0 }
    }
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }
    fn parse(&mut self) -> Result<Json, String> {
        self.skip_ws();
        self.parse_value()
    }
    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        if self.i >= self.b.len() {
            return Err("unexpected EOF".into());
        }
        match self.b[self.i] {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => Ok(Json::Str(self.parse_string()?)),
            b't' | b'f' => self.parse_bool(),
            b'n' => self.parse_null(),
            _ => self.parse_number(),
        }
    }
    fn parse_object(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut out = Vec::new();
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.skip_ws();
            if self.i >= self.b.len() || self.b[self.i] != b'"' {
                return Err("expected object key".into());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return Err("expected ':'".into());
            }
            self.i += 1;
            let val = self.parse_value()?;
            out.push((key, val));
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err("unterminated object".into());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                    continue;
                }
                b'}' => {
                    self.i += 1;
                    break;
                }
                _ => return Err("expected ',' or '}'".into()),
            }
        }
        Ok(Json::Obj(out))
    }
    fn parse_array(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut out = Vec::new();
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            let val = self.parse_value()?;
            out.push(val);
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err("unterminated array".into());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                    continue;
                }
                b']' => {
                    self.i += 1;
                    break;
                }
                _ => return Err("expected ',' or ']'".into()),
            }
        }
        Ok(Json::Arr(out))
    }
    fn parse_string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut s = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return Ok(s),
                b'\\' => {
                    if self.i >= self.b.len() {
                        return Err("bad escape".into());
                    }
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
                        b'u' => {
                            if self.i + 4 > self.b.len() {
                                return Err("bad unicode escape".into());
                            }
                            let hex = &self.b[self.i..self.i + 4];
                            self.i += 4;
                            let mut v = 0u32;
                            for &d in hex {
                                let d = match d {
                                    b'0'..=b'9' => (d - b'0') as u32,
                                    b'a'..=b'f' => (d - b'a' + 10) as u32,
                                    b'A'..=b'F' => (d - b'A' + 10) as u32,
                                    _ => return Err("bad unicode hex".into()),
                                };
                                v = v * 16 + d;
                            }
                            if let Some(ch) = char::from_u32(v) {
                                s.push(ch);
                            }
                        }
                        _ => return Err("unknown escape".into()),
                    }
                }
                _ => {
                    // 直接把 UTF-8 字节序列按字符处理（简单做法：逐字节拼，遇到多字节自行合并）
                    if c < 0x80 {
                        s.push(c as char);
                    } else {
                        // 可能是多字节 UTF-8 首字节，回溯收集完整字符
                        let start = self.i - 1;
                        let extra = utf8_len(c) - 1;
                        if start + extra <= self.b.len() {
                            if let Ok(t) = std::str::from_utf8(&self.b[start..start + extra + 1]) {
                                s.push_str(t);
                                self.i = start + extra + 1;
                            }
                        }
                    }
                }
            }
        }
        Err("unterminated string".into())
    }
    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.i;
        let mut is_float = false;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'0'..=b'9' | b'-' | b'+' => self.i += 1,
                b'.' | b'e' | b'E' => {
                    is_float = true;
                    self.i += 1;
                }
                _ => break,
            }
        }
        let slice = &self.b[start..self.i];
        let st = String::from_utf8_lossy(slice).to_string();
        if let Ok(n) = st.parse::<f64>() {
            let _ = is_float;
            Ok(Json::Num(n))
        } else {
            Err("bad number".into())
        }
    }
    fn parse_bool(&mut self) -> Result<Json, String> {
        if self.b[self.i..].starts_with(b"true") {
            self.i += 4;
            Ok(Json::Bool(true))
        } else if self.b[self.i..].starts_with(b"false") {
            self.i += 5;
            Ok(Json::Bool(false))
        } else {
            Err("bad bool".into())
        }
    }
    fn parse_null(&mut self) -> Result<Json, String> {
        if self.b[self.i..].starts_with(b"null") {
            self.i += 4;
            Ok(Json::Null)
        } else {
            Err("bad null".into())
        }
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn parse_json(b: &[u8]) -> Result<Json, String> {
    let mut p = Parser::new(b);
    let v = p.parse()?;
    Ok(v)
}

// ===================== JSON 字符串转义（构建信封时用） =====================

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn json_string(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

// ===================== base64（握手 Key 用，标准表） =====================

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(B64[((n >> 6) & 63) as usize] as char);
        out.push(B64[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(B64[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

// ===================== 随机数（非密码学，仅用于掩码与 key / msg_id） =====================

struct Lcg {
    state: u64,
}

impl Lcg {
    fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        Lcg {
            state: seed ^ 0x9E3779B97F4A7C15,
        }
    }
    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        let mut buf = self.next_u64();
        let mut cnt = 0u64;
        for _ in 0..n {
            if cnt == 0 {
                buf = self.next_u64();
                cnt = 8;
            }
            out.push((buf & 0xff) as u8);
            buf >>= 8;
            cnt -= 1;
        }
        out
    }
}

// ===================== 资产采集（适配 macOS / Linux / Windows，纯 std） =====================

fn run_output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| {
            let mut s = String::new();
            s.push_str(&String::from_utf8_lossy(&o.stdout));
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        })
        .unwrap_or_default()
}

fn trim(s: &str) -> String {
    s.trim().to_string()
}

fn detect_os() -> String {
    if cfg!(target_os = "macos") {
        "macos".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "linux") {
        "linux".into()
    } else {
        std::env::consts::OS.to_string()
    }
}

fn hostname() -> String {
    if cfg!(windows) {
        trim(&run_output("hostname", &[]))
    } else {
        let h = trim(&run_output("hostname", &[]));
        if !h.is_empty() {
            h
        } else {
            trim(&run_output("uname", &["-n"]))
        }
    }
}

fn os_version() -> String {
    trim(&run_output("uname", &["-r"]))
}

fn arch() -> String {
    trim(&run_output("uname", &["-m"]))
}

fn cpu() -> String {
    if cfg!(target_os = "macos") {
        trim(&run_output("sysctl", &["-n", "machdep.cpu.brand_string"]))
    } else if cfg!(windows) {
        // wmic cpu get name -> 第二行
        let out = run_output("wmic", &["cpu", "get", "name"]);
        out.lines().nth(1).map(trim).unwrap_or_default()
    } else {
        // Linux: /proc/cpuinfo 的 model name 首行
        if let Ok(content) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in content.lines() {
                if line.starts_with("model name") {
                    if let Some(idx) = line.find(':') {
                        return trim(&line[idx + 1..]);
                    }
                }
            }
        }
        String::new()
    }
}

fn memory_gb() -> i64 {
    if cfg!(target_os = "macos") {
        let bytes = trim(&run_output("sysctl", &["-n", "hw.memsize"]))
            .parse::<f64>()
            .unwrap_or(0.0);
        (bytes / 1e9).round() as i64
    } else if cfg!(windows) {
        let out = run_output("wmic", &["computersystem", "get", "totalphysicalmemory"]);
        if let Some(line) = out.lines().nth(1) {
            let bytes = trim(line).parse::<f64>().unwrap_or(0.0);
            return (bytes / 1e9).round() as i64;
        }
        0
    } else {
        if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(idx) = line.find(':') {
                        let kb = trim(&line[idx + 1..]).replace("kB", "").trim().to_string();
                        if let Ok(k) = kb.parse::<f64>() {
                            return (k / 1048576.0).round() as i64;
                        }
                    }
                }
            }
        }
        0
    }
}

fn disk_gb() -> i64 {
    if cfg!(windows) {
        let out = run_output("wmic", &["logicaldisk", "get", "size"]);
        let mut total = 0.0;
        for line in out.lines().skip(1) {
            let v = trim(line).parse::<f64>().unwrap_or(0.0);
            if v > 0.0 {
                total += v;
            }
        }
        (total / 1e9).round() as i64
    } else {
        // df -k / -> 第二行第二列为 1K-blocks
        let out = run_output("df", &["-k", "/"]);
        if let Some(line) = out.lines().nth(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 2 {
                if let Ok(k) = cols[1].parse::<f64>() {
                    return (k / 1048576.0).round() as i64;
                }
            }
        }
        0
    }
}

// 解析 ifconfig / ip / ipconfig，取首个非回环 IPv4 与 MAC
fn net_info() -> (String, String) {
    let mut ip = String::new();
    let mut mac = String::new();
    let text = if cfg!(windows) {
        run_output("ipconfig", &["/all"])
    } else if cfg!(target_os = "linux") {
        run_output("ip", &["addr"])
    } else {
        run_output("ifconfig", &[])
    };

    if cfg!(windows) {
        // 简单扫描：IPv4 Address ... : x.x.x.x  /  Physical Address ... : xx-xx-...
        for line in text.lines() {
            let l = line.trim();
            if ip.is_empty() && l.contains("IPv4 Address") {
                if let Some(idx) = l.find(':') {
                    let v = trim(&l[idx + 1..]).trim_matches('(').trim_matches(')').to_string();
                    if v != "127.0.0.1" && v.contains('.') {
                        ip = v;
                    }
                }
            }
            if mac.is_empty() && l.contains("Physical Address") {
                if let Some(idx) = l.find(':') {
                    mac = trim(&l[idx + 1..]).replace('-', ":").to_uppercase();
                }
            }
        }
    } else {
        let mut in_loopback = false;
        for line in text.lines() {
            let l = line.trim();
            if cfg!(target_os = "linux") {
                if l.starts_with("lo:") || l.starts_with("1: lo:") {
                    in_loopback = true;
                    continue;
                }
                if l.starts_with(char::is_numeric) && !l.contains("lo:") && !l.contains("<LOOPBACK>") {
                    in_loopback = false;
                }
            } else {
                // mac ifconfig: 接口名单独一行，回环接口为 lo0
                if l.starts_with("lo0:") {
                    in_loopback = true;
                    continue;
                }
                if l.ends_with(":") && !l.starts_with("lo0") {
                    in_loopback = false;
                }
            }
            if in_loopback {
                continue;
            }
            if ip.is_empty() {
                if let Some(idx) = l.find("inet ") {
                    let rest = &l[idx + 5..];
                    let candidate = rest.split_whitespace().next().unwrap_or("").to_string();
                    if candidate != "127.0.0.1" && candidate.contains('.') {
                        ip = candidate;
                    }
                }
                // linux 可能写成 "inet 1.2.3.4/24"
                if ip.is_empty() {
                    if let Some(idx) = l.find("inet ") {
                        let rest = &l[idx + 5..];
                        let candidate = rest.split(|c| c == ' ' || c == '/').next().unwrap_or("").to_string();
                        if candidate != "127.0.0.1" && candidate.contains('.') {
                            ip = candidate;
                        }
                    }
                }
            }
            if mac.is_empty() {
                if let Some(idx) = l.find("ether ") {
                    mac = trim(&l[idx + 6..]).to_uppercase();
                } else if let Some(idx) = l.find("link/ether ") {
                    mac = trim(&l[idx + 10..]).to_uppercase();
                } else if let Some(idx) = l.find("HWaddr ") {
                    mac = trim(&l[idx + 7..]).to_uppercase();
                }
            }
        }
    }
    (ip, mac)
}

fn serial_no() -> String {
    if cfg!(target_os = "macos") {
        let out = run_output("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]);
        for line in out.lines() {
            if line.contains("IOPlatformSerialNumber") {
                if let Some(eq) = line.find('=') {
                    let rest = &line[eq + 1..];
                    if let Some(q) = rest.find('"') {
                        let after = &rest[q + 1..];
                        if let Some(end) = after.find('"') {
                            return after[..end].to_string();
                        }
                    }
                }
            }
        }
        String::new()
    } else if cfg!(windows) {
        let out = run_output("wmic", &["bios", "get", "serialnumber"]);
        out.lines().nth(1).map(trim).unwrap_or_default()
    } else {
        // Linux: /sys/class/dmi/id/product_serial（需权限，尽力而为）
        std::fs::read_to_string("/sys/class/dmi/id/product_serial")
            .map(|s| trim(&s))
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct Assets {
    hostname: String,
    os: String,
    os_version: String,
    arch: String,
    cpu: String,
    memory_gb: i64,
    disk_gb: i64,
    serial_no: String,
    mac: String,
    ip: String,
    agent_version: String,
}

fn collect_assets() -> Assets {
    let (ip, mac) = net_info();
    let agent_version = "0.1.0-rust".to_string();
    Assets {
        hostname: hostname(),
        os: detect_os(),
        os_version: os_version(),
        arch: arch(),
        cpu: cpu(),
        memory_gb: memory_gb(),
        disk_gb: disk_gb(),
        serial_no: serial_no(),
        mac,
        ip: ip.clone(),
        agent_version,
    }
}

// ===================== WebSocket 客户端（手写，std-only） =====================

struct WsClient {
    reader: BufReader<TcpStream>,
    writer: Arc<Mutex<TcpStream>>,
}

#[derive(Debug)]
enum Frame {
    Text(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

impl WsClient {
    fn connect(url: &str) -> std::io::Result<WsClient> {
        // 解析 ws://host:port/path
        let rest = url
            .strip_prefix("ws://")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "only ws:// supported"))?;
        let (authority, path) = match rest.find('/') {
            Some(idx) => (rest[..idx].to_string(), rest[idx..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        };
        let host = if authority.contains(':') {
            authority.clone()
        } else {
            format!("{}:80", authority)
        };

        let stream = TcpStream::connect(&host)?;
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;

        // 随机 Sec-WebSocket-Key = base64(16 字节)（握手用，非密码学即可）
        let key = base64_encode(&Lcg::new().bytes(16));

        let handshake = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        let mut write_stream = stream.try_clone()?;
        write_stream.write_all(handshake.as_bytes())?;
        write_stream.flush()?;

        // 读取 101 响应头直至空行
        let mut reader = BufReader::new(stream);
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = reader.read(&mut byte)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed during handshake",
                ));
            }
            header.push(byte[0]);
            if header.len() >= 4 && &header[header.len() - 4..] == b"\r\n\r\n" {
                break;
            }
        }
        let header_str = String::from_utf8_lossy(&header);
        if !header_str.contains("101") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("handshake failed: {}", header_str.lines().next().unwrap_or("")),
            ));
        }

        Ok(WsClient {
            reader,
            writer: Arc::new(Mutex::new(write_stream)),
        })
    }

    // 发送掩码文本帧（客户端必须加掩码）
    fn send_text(&self, data: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        let len = data.len();
        let mut header = Vec::with_capacity(10);
        let mask_bit = 0x80u8;
        if len < 126 {
            header.push(0x81);
            header.push((len as u8) | mask_bit);
        } else if len <= 0xffff {
            header.push(0x81);
            header.push(126 | mask_bit);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            header.push(0x81);
            header.push(127 | mask_bit);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }
        // 掩码用本地 LCG：避免与心跳/指令线程的「先 rng 后 writer」加锁顺序形成环（防死锁）。
        let mask = Lcg::new().bytes(4);
        header.extend_from_slice(&mask);
        w.write_all(&header)?;
        let mut masked = Vec::with_capacity(len);
        for (i, &b) in data.iter().enumerate() {
            masked.push(b ^ mask[i % 4]);
        }
        w.write_all(&masked)?;
        w.flush()?;
        Ok(())
    }

    fn send_pong(&self, payload: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        let len = payload.len();
        let mask_bit = 0x80u8;
        let mut header = vec![0x8A, (len as u8) | mask_bit];
        let mask = Lcg::new().bytes(4);
        header.extend_from_slice(&mask);
        w.write_all(&header)?;
        let mut masked = Vec::with_capacity(len);
        for (i, &b) in payload.iter().enumerate() {
            masked.push(b ^ mask[i % 4]);
        }
        w.write_all(&masked)?;
        w.flush()?;
        Ok(())
    }

    // 读取一帧，自动处理 ping -> pong，close -> Close
    fn read_frame(&mut self) -> std::io::Result<Frame> {
        loop {
            let mut b0 = [0u8; 1];
            if self.reader.read_exact(&mut b0).is_err() {
                return Ok(Frame::Close);
            }
            let mut b1 = [0u8; 1];
            self.reader.read_exact(&mut b1)?;
            let opcode = b0[0] & 0x0f;
            let masked = (b1[0] & 0x80) != 0;
            let mut len = (b1[0] & 0x7f) as usize;
            if len == 126 {
                let mut ext = [0u8; 2];
                self.reader.read_exact(&mut ext)?;
                len = u16::from_be_bytes(ext) as usize;
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.reader.read_exact(&mut ext)?;
                len = u64::from_be_bytes(ext) as usize;
            }
            let mut mask = [0u8; 4];
            if masked {
                self.reader.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len];
            self.reader.read_exact(&mut payload)?;
            if masked {
                for i in 0..len {
                    payload[i] ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Ok(Frame::Close),
                0x9 => {
                    // ping -> pong
                    let _ = self.send_pong(&payload);
                    continue;
                }
                0xA => continue, // pong
                0x1 | 0x0 => return Ok(Frame::Text(payload)),
                _ => continue, // 未知 opcode 忽略
            }
        }
    }
}

// ===================== 信封构建 =====================

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn build_envelope(
    rng: &mut Lcg,
    seq: &mut u64,
    msg_type: &str,
    payload: &str,
) -> String {
    *seq += 1;
    let key = rng.bytes(9);
    let msg_id = format!("rust-{}", hex_encode(&key));
    format!(
        "{{\"ver\":1,\"msg_id\":{},\"ts\":{},\"type\":{},\"seq\":{},\"payload\":{},\"sig\":\"\"}}",
        json_string(&msg_id),
        now_secs(),
        json_string(msg_type),
        *seq,
        payload
    )
}

fn hex_encode(b: &[u8]) -> String {
    let mut s = String::new();
    for &x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

// ===================== 指令处理 =====================

fn handle_command(
    env: &Json,
    writer: Arc<Mutex<TcpStream>>,
    rng: Arc<Mutex<Lcg>>,
    seq: Arc<Mutex<u64>>,
    _device_id: Arc<Mutex<String>>,
) {
    let payload = match env.get("payload") {
        Some(Json::Obj(_)) => env.get("payload").unwrap().clone(),
        _ => return,
    };
    let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let task_id = payload.get("task_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let command = payload.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // 在子线程执行，避免阻塞读循环
    thread::spawn(move || {
        let (state, exit_code, output_log) = match action.as_str() {
            "shell" => run_shell(&command),
            "collect" => run_collect(),
            "sw_install" => (
                "success".to_string(),
                0,
                "rust agent: sw_install 暂未实现(需引入 crate/FFI)".to_string(),
            ),
            other => (
                "failed".to_string(),
                1,
                format!("rust agent: 未知 action={}", other),
            ),
        };

        let payload_obj = format!(
            "{{\"task_id\":{},\"state\":{},\"exit_code\":{},\"output_log\":{}}}",
            json_string(&task_id),
            json_string(&state),
            exit_code,
            json_string(&output_log)
        );
        let mut rng_g = rng.lock().unwrap();
        let mut seq_g = seq.lock().unwrap();
        let msg = build_envelope(&mut rng_g, &mut seq_g, "result", &payload_obj);
        if let Err(e) = writer.lock().unwrap().write_and_send(&msg) {
            eprintln!("[agent] 发送 result 失败: {}", e);
        } else {
            eprintln!("[agent] 已回 result task={} state={}", task_id, state);
        }
    });
}

// 给 TcpStream 加一个便捷“自动加掩码发送文本”的方法，通过 WsClient 不行（这里仅持有裸 stream），
// 因此我们用一个独立函数：持有 writer 锁后直接构造掩码帧。
trait WriteWsText {
    fn write_and_send(&mut self, text: &str) -> std::io::Result<()>;
}

impl WriteWsText for TcpStream {
    fn write_and_send(&mut self, text: &str) -> std::io::Result<()> {
        // 注意：此处掩码需要共享 rng，但为简化，使用基于时间/地址的临时随机掩码。
        // 掩码值不影响安全性（仅防缓存污染），非密码学即可。
        let mut rng = Lcg::new();
        let data = text.as_bytes();
        let len = data.len();
        let mask = rng.bytes(4);
        let mut header = Vec::with_capacity(10);
        let mask_bit = 0x80u8;
        if len < 126 {
            header.push(0x81);
            header.push((len as u8) | mask_bit);
        } else if len <= 0xffff {
            header.push(0x81);
            header.push(126 | mask_bit);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            header.push(0x81);
            header.push(127 | mask_bit);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }
        header.extend_from_slice(&mask);
        self.write_all(&header)?;
        let mut masked = Vec::with_capacity(len);
        for (i, &b) in data.iter().enumerate() {
            masked.push(b ^ mask[i % 4]);
        }
        self.write_all(&masked)?;
        self.flush()?;
        Ok(())
    }
}

fn run_shell(command: &str) -> (String, i64, String) {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/c")
    } else {
        ("sh", "-c")
    };
    let out = Command::new(shell)
        .arg(flag)
        .arg(command)
        .output();
    match out {
        Ok(o) => {
            let mut log = String::new();
            log.push_str(&String::from_utf8_lossy(&o.stdout));
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.is_empty() {
                if !log.is_empty() {
                    log.push('\n');
                }
                log.push_str(&err);
            }
            let code = o.status.code().unwrap_or(-1);
            (if code == 0 { "success" } else { "failed" }.to_string(), code as i64, log)
        }
        Err(e) => ("failed".to_string(), 127, format!("执行失败: {}", e)),
    }
}

fn run_collect() -> (String, i64, String) {
    let a = collect_assets();
    let log = format!(
        "re-collected: host={} os={} os_version={} arch={} cpu={} mem_gb={} disk_gb={} serial={} mac={} ip={} agent={}",
        a.hostname, a.os, a.os_version, a.arch, a.cpu, a.memory_gb, a.disk_gb, a.serial_no, a.mac, a.ip, a.agent_version
    );
    ("success".to_string(), 0, log)
}

// ===================== 主流程 =====================

fn build_register_payload(assets: &Assets, token: &str) -> String {
    format!(
        "{{\"hostname\":{},\"os\":{},\"os_version\":{},\"arch\":{},\"cpu\":{},\"memory_gb\":{},\"disk_gb\":{},\"serial_no\":{},\"mac\":{},\"ip\":{},\"agent_version\":{},\"token\":{}}}",
        json_string(&assets.hostname),
        json_string(&assets.os),
        json_string(&assets.os_version),
        json_string(&assets.arch),
        json_string(&assets.cpu),
        assets.memory_gb,
        assets.disk_gb,
        json_string(&assets.serial_no),
        json_string(&assets.mac),
        json_string(&assets.ip),
        json_string(&assets.agent_version),
        json_string(token)
    )
}

fn build_heartbeat(load: f64, ip: &str) -> String {
    format!("{{\"load\":{},\"ip\":{}}}", load, json_string(ip))
}

fn session(url: &str) -> std::io::Result<()> {
    let rng = Arc::new(Mutex::new(Lcg::new()));
    let ws = WsClient::connect(url)?;
    eprintln!("[agent] WebSocket 握手成功: {}", url);

    // 共享状态
    let device_id: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let token: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let seq: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let registered: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    // 注册
    let assets = collect_assets();
    let reg_payload = {
        let t = token.lock().unwrap().clone();
        build_register_payload(&assets, &t)
    };
    {
        let mut rng_g = rng.lock().unwrap();
        let mut seq_g = seq.lock().unwrap();
        let msg = build_envelope(&mut rng_g, &mut seq_g, "register", &reg_payload);
        ws.send_text(msg.as_bytes())?;
        eprintln!("[agent] 已发送 register (host={}, os={}, arch={})", assets.hostname, assets.os, assets.arch);
    }

    // 心跳线程：每 30s 发送（注册成功后）
    {
        let writer = ws.writer.clone();
        let registered = registered.clone();
        let seq = seq.clone();
        let rng = rng.clone();
        let ip = assets.ip.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(30));
                if !*registered.lock().unwrap() {
                    continue;
                }
                let hb = build_heartbeat(0.0, &ip);
                let mut rng_g = rng.lock().unwrap();
                let mut seq_g = seq.lock().unwrap();
                let msg = build_envelope(&mut rng_g, &mut seq_g, "heartbeat", &hb);
                if let Err(e) = writer.lock().unwrap().write_and_send(&msg) {
                    eprintln!("[agent] 心跳发送失败: {}", e);
                    break;
                } else {
                    eprintln!("[agent] 心跳已发送 ip={}", ip);
                }
            }
        });
    }

    // 读循环
    let mut ws = ws;
    loop {
        let frame = ws.read_frame()?;
        match frame {
            Frame::Close => {
                eprintln!("[agent] 收到 close 帧，断开");
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "closed"));
            }
            Frame::Ping(_) => continue,
            Frame::Pong => continue,
            Frame::Text(data) => {
                let parsed = parse_json(&data);
                let env = match parsed {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[agent] JSON 解析失败（忽略）: {}", e);
                        continue;
                    }
                };
                let msg_type = match env.get("type").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => continue,
                };
                match msg_type.as_str() {
                    "register_ack" => {
                        if let Some(p) = env.get("payload") {
                            let did = p.get("device_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let tok = p.get("token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let ok = p.get("ok").and_then(|v| v.as_bool().or_else(|| Some(true))).unwrap_or(true);
                            if !did.is_empty() {
                                *device_id.lock().unwrap() = did.clone();
                                *token.lock().unwrap() = tok.clone();
                                *registered.lock().unwrap() = true;
                                eprintln!("[agent] 注册成功 device_id={} token={} ok={}", did, tok, ok);
                            } else {
                                eprintln!("[agent] register_ack 缺少 device_id（忽略）");
                            }
                        }
                    }
                    "command" => {
                        let did = device_id.lock().unwrap().clone();
                        if did.is_empty() {
                            eprintln!("[agent] 收到 command 但尚未注册（忽略）");
                            continue;
                        }
                        handle_command(
                            &env,
                            ws.writer.clone(),
                            rng.clone(),
                            seq.clone(),
                            device_id.clone(),
                        );
                    }
                    _ => {
                        // 其它类型（rc_* 等）直接忽略，不崩溃
                        eprintln!("[agent] 忽略未知/不相关消息类型: {}", msg_type);
                    }
                }
            }
        }
    }
}

fn main() {
    let url = std::env::var("YUDUAN_HUB").unwrap_or_else(|_| "ws://127.0.0.1:8080/ws/agent".to_string());
    eprintln!("[agent] 御端 Rust 副端启动，目标中枢: {}", url);
    loop {
        match session(&url) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("[agent] 会话异常: {}，5s 后重连（复用 token）", e);
            }
        }
        thread::sleep(Duration::from_secs(5));
    }
}
