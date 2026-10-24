// 御端主端 —— 中枢控制面客户端（REST）。
// 口径与 hub 侧实现（dev/internal/hub/hub.go）一一对应：
//   POST /api/v1/login            {username,password} → {code,token,role,must_change_password}
//   POST /api/v1/password         {old_password,new_password}（Bearer）→ 续签 token
//   GET  /api/v1/devices          （Bearer）→ {total, devices:[…]}
//   POST /api/v1/devices/{id}     {action,command,…}（Bearer）→ 执行结果
// 设备字段对齐 dev/internal/store/store.go 的 Device struct。

use crate::http::{request, HttpReq};
use serde::{Deserialize, Serialize};

fn join_base(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResult {
    pub token: String,
    pub role: String,
    #[serde(rename = "must_change_password", default)]
    pub must_change_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(rename = "os_version", default)]
    pub os_version: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub cpu: String,
    #[serde(rename = "memory_gb", default)]
    pub memory_gb: i64,
    #[serde(rename = "disk_gb", default)]
    pub disk_gb: i64,
    #[serde(rename = "serial_no", default)]
    pub serial_no: String,
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub ip: String,
    #[serde(rename = "agent_version", default)]
    pub agent_version: String,
    #[serde(default)]
    pub online: bool,
    #[serde(rename = "last_heartbeat", default)]
    pub last_heartbeat: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdResult {
    #[serde(default)]
    pub code: i64,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| format!("响应解析失败: {}（{}…）", e, String::from_utf8_lossy(&body).chars().take(120).collect::<String>()))
}

/// POST /api/v1/login
pub fn login(base: &str, username: &str, password: &str) -> Result<LoginResult, String> {
    let url = join_base(base, "api/v1/login");
    let resp = request(
        HttpReq::post_json(&url, &serde_json::json!({ "username": username, "password": password }))
            .timeout(10),
    )?;
    if resp.status == 401 {
        return Err("用户名或密码错误".into());
    }
    if resp.status != 200 {
        return Err(format!("登录失败：HTTP {}", resp.status));
    }
    let v: serde_json::Value = parse_json(&resp.body)?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(format!("登录失败：{}", v));
    }
    Ok(LoginResult {
        token: v.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        role: v.get("role").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        must_change_password: v.get("must_change_password").and_then(|t| t.as_bool()).unwrap_or(false),
    })
}

/// POST /api/v1/password（Bearer），成功返回续签 token。
pub fn change_password(base: &str, token: &str, old_password: &str, new_password: &str) -> Result<LoginResult, String> {
    let url = join_base(base, "api/v1/password");
    let resp = request(
        HttpReq::post_json(&url, &serde_json::json!({ "old_password": old_password, "new_password": new_password }))
            .header("Authorization", &format!("Bearer {}", token))
            .timeout(10),
    )?;
    if resp.status != 200 {
        return Err(format!("改密失败：HTTP {}", resp.status));
    }
    let v: serde_json::Value = parse_json(&resp.body)?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(format!("改密失败：{}", v.get("message").cloned().unwrap_or(v.clone())));
    }
    // 成功响应携带续签 token（见 hub handleChangePassword：签发新令牌无缝续用）
    Ok(LoginResult {
        token: v.get("token").and_then(|t| t.as_str()).unwrap_or(token).to_string(),
        role: v.get("role").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        must_change_password: false,
    })
}

/// GET /api/v1/devices（Bearer）
pub fn devices(base: &str, token: &str) -> Result<Vec<Device>, String> {
    let url = join_base(base, "api/v1/devices");
    let resp = request(HttpReq::get(&url).header("Authorization", &format!("Bearer {}", token)).timeout(10))?;
    if resp.status == 401 {
        return Err("登录已过期（401），请重新登录".into());
    }
    if resp.status != 200 {
        return Err(format!("获取设备列表失败：HTTP {}", resp.status));
    }
    let v: serde_json::Value = parse_json(&resp.body)?;
    // hub 的 guard 失败可能以 HTTP 200 + code!=0 返回（如 4033 password_change_required）
    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            return Err(format!(
                "获取设备列表被拒绝（code={}）：{}",
                code,
                v.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
    }
    let list = v.get("devices").cloned().unwrap_or(serde_json::Value::Array(vec![]));
    parse_json(list.to_string().as_bytes())
}

/// POST /api/v1/devices/{id}（Bearer）——指令下发。
pub fn device_cmd(base: &str, token: &str, device_id: &str, action: &str, command: &str) -> Result<CmdResult, String> {
    let url = join_base(base, &format!("api/v1/devices/{}", device_id));
    let mut body = serde_json::json!({ "action": action });
    if !command.is_empty() {
        body["command"] = serde_json::Value::String(command.to_string());
    }
    let resp = request(
        HttpReq::post_json(&url, &body)
            .header("Authorization", &format!("Bearer {}", token))
            .timeout(30),
    )?;
    if resp.status == 401 {
        return Err("登录已过期（401），请重新登录".into());
    }
    if resp.status != 200 {
        return Err(format!("指令下发失败：HTTP {}", resp.status));
    }
    parse_json(&resp.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_json_field_names() {
        // 与 store.Device 的 JSON 标签对齐（last_heartbeat / agent_version / memory_gb…）
        let raw = r#"{"id":"abc","hostname":"h","os":"darwin","os_version":"15.0","arch":"arm64","cpu":"x","memory_gb":16,"disk_gb":500,"serial_no":"S","mac":"m","ip":"1.2.3.4","agent_version":"0.3","online":true,"last_heartbeat":"2026-10-24T10:00:00Z"}"#;
        let d: Device = serde_json::from_str(raw).unwrap();
        assert_eq!(d.id, "abc");
        assert!(d.online);
        assert_eq!(d.memory_gb, 16);
    }
}
