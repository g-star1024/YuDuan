// 御端主端 —— 极简 HTTP/1.1 客户端（纯 std）。
// 与副端 httpget.rs 同源思路：TcpStream + 手写请求/响应解析（Content-Length / chunked）。
// 在 httpget.rs 基础上扩展：POST + 任意头 + 请求体；仅支持 http://
// （生产 wss/https 见 T-4 mTLS 里程碑；开发期 hub 明文端口与浏览器控制台同口径）。

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub struct HttpReq<'a> {
    pub method: &'a str,
    pub url: &'a str,
    /// 附加头（不含 Host/Content-Length，自动补）
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout_secs: u64,
}

impl<'a> HttpReq<'a> {
    pub fn get(url: &'a str) -> Self {
        HttpReq { method: "GET", url, headers: Vec::new(), body: None, timeout_secs: 10 }
    }
    pub fn post_json(url: &'a str, body: &impl serde::Serialize) -> Self {
        HttpReq {
            method: "POST",
            url,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Some(serde_json::to_vec(body).unwrap_or_default()),
            timeout_secs: 20,
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

/// 解析 http://host[:port]/path → (host, port, path)
pub fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("仅支持 http:// URL: {}", url))?;
    let (authority, path): (&str, &str) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => (
            authority[..i].to_string(),
            authority[i + 1..].parse::<u16>().map_err(|e| format!("bad port: {}", e))?,
        ),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return Err(format!("bad host in URL: {}", url));
    }
    Ok((host, port, path.to_string()))
}

/// 执行请求，返回状态码与响应体。
pub fn request(req: HttpReq) -> Result<HttpResponse, String> {
    let (host, port, path) = parse_url(req.url)?;
    let authority = if port == 80 { host.clone() } else { format!("{}:{}", host, port) };
    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("bad addr {}: {}", addr, e))?,
        Duration::from_secs(req.timeout_secs),
    )
    .map_err(|e| format!("connect {} failed: {}", addr, e))?;
    stream.set_read_timeout(Some(Duration::from_secs(req.timeout_secs))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(req.timeout_secs))).ok();

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: yuduan-main/0.1\r\nConnection: close\r\n",
        req.method, path, authority
    );
    for (k, v) in &req.headers {
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    let body = req.body.clone().unwrap_or_default();
    if !body.is_empty() || req.method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writer.write_all(head.as_bytes()).map_err(|e| format!("write failed: {}", e))?;
    if !body.is_empty() {
        writer.write_all(&body).map_err(|e| format!("write body failed: {}", e))?;
    }
    writer.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => header.push(byte[0]),
            Err(e) => return Err(format!("read header failed: {}", e)),
        }
        if header.len() >= 4 && &header[header.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if header.len() > 64 * 1024 {
            return Err("response header too large".into());
        }
    }
    let header_str = String::from_utf8_lossy(&header).to_string();
    let status = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line: {}", header_str.lines().next().unwrap_or("")))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in header_str.lines().skip(1) {
        let l = line.to_ascii_lowercase();
        if let Some(v) = l.strip_prefix("content-length:") {
            content_length = v.trim().parse::<usize>().ok();
        } else if l.starts_with("transfer-encoding:") && l.contains("chunked") {
            chunked = true;
        }
    }

    let resp_body = if chunked {
        read_chunked(&mut reader)?
    } else if let Some(n) = content_length {
        let mut buf = vec![0u8; n];
        reader.read_exact(&mut buf).map_err(|e| format!("read body failed: {}", e))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(|e| format!("read body failed: {}", e))?;
        buf
    };
    Ok(HttpResponse { status, body: resp_body })
}

fn read_chunked<R: Read>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line = read_line(reader)?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|e| format!("bad chunk size: {}", e))?;
        if size == 0 {
            let _ = read_line(reader);
            break;
        }
        let mut buf = vec![0u8; size];
        reader.read_exact(&mut buf).map_err(|e| format!("read chunk failed: {}", e))?;
        out.extend_from_slice(&buf);
        let _ = read_line(reader)?;
    }
    Ok(out)
}

fn read_line<R: Read>(reader: &mut R) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                line.push(byte[0]);
                if line.len() >= 2 && &line[line.len() - 2..] == b"\r\n" {
                    line.truncate(line.len() - 2);
                    break;
                }
            }
            Err(e) => return Err(format!("read line failed: {}", e)),
        }
        if line.len() > 8 * 1024 {
            return Err("line too long".into());
        }
    }
    Ok(String::from_utf8_lossy(&line).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_forms() {
        let (h, p, path) = parse_url("http://127.0.0.1:18241/api/v1/login").unwrap();
        assert_eq!(h, "127.0.0.1");
        assert_eq!(p, 18241);
        assert_eq!(path, "/api/v1/login");
        let (h, p, path) = parse_url("http://hub.local/").unwrap();
        assert_eq!(h, "hub.local");
        assert_eq!(p, 80);
        assert_eq!(path, "/");
        assert!(parse_url("https://x/y").is_err());
        assert!(parse_url("http://:80/x").is_err());
    }
}
