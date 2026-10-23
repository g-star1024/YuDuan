// 御端 Rust 副端 —— M5 软件分发安装（参考互通版）。
// 下载：internal httpget（Content-Length/chunked）。安装：按平台调用系统静默安装器。
//   macOS: .pkg → installer -pkg -target / ；.zip → 解包（ditto/unzip），含 .app 则复制到 /Applications
//   Linux: .deb → dpkg -i（无 root 时 sudo -n）；.rpm → rpm -i；.sh → bash
//   Windows: .msi → msiexec /i /qn /norestart；.exe → 尽力以 /S 静默运行
// 真实生产需管理员权限与签名校验（生产化清单见 README）。

use crate::httpget::download_to_temp;
use std::process::Command;

pub struct InstallOutcome {
    pub state: String,
    pub exit_code: i64,
    pub output_log: String,
}

/// 处理 sw_install 指令。
pub fn handle_sw_install(url: &str, platform: &str, pkg_name: &str) -> InstallOutcome {
    if url.is_empty() {
        return InstallOutcome {
            state: "failed".into(),
            exit_code: 1,
            output_log: "sw_install: 缺少 software_url".into(),
        };
    }
    let (local, suggested) = match download_to_temp(url, "yuduan-sw") {
        Ok(p) => p,
        Err(e) => {
            return InstallOutcome {
                state: "failed".into(),
                exit_code: 1,
                output_log: format!("download failed: {}", e),
            }
        }
    };
    // 安装器按扩展名分派（.pkg/.zip/.dmg/.deb/.rpm/.sh/.msi/.exe），
    // 而下载临时文件统一为 .bin：按「服务端建议文件名 → URL → 包名」顺序补扩展名。
    let local = match suggested
        .as_deref()
        .and_then(ext_of)
        .or_else(|| ext_of(url))
        .or_else(|| ext_of(pkg_name))
    {
        Some(e) if !local.to_ascii_lowercase().ends_with(&format!(".{}", e)) => {
            let renamed = format!("{}.{}", local, e);
            match std::fs::rename(&local, &renamed) {
                Ok(()) => renamed,
                Err(_) => local,
            }
        }
        _ => local,
    };
    let out = install_package(&local, platform, pkg_name);
    let _ = std::fs::remove_file(&local);
    out
}

/// 从 URL 或包名提取合法扩展名（纯字母数字、≤6 字符；拒绝查询串等噪声）。
fn ext_of(s: &str) -> Option<String> {
    let last = s.rsplit('/').next().unwrap_or(s);
    let idx = last.rfind('.')?;
    let ext = &last[idx + 1..];
    if ext.is_empty() || ext.len() > 6 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn install_package(path: &str, _platform: &str, _pkg: &str) -> InstallOutcome {
    let lower = path.to_ascii_lowercase();
    let (cmd, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        if lower.ends_with(".pkg") {
            ("installer", vec!["-pkg".into(), path.into(), "-target".into(), "/".into()])
        } else if lower.ends_with(".zip") {
            return install_zip(path);
        } else if lower.ends_with(".dmg") {
            return install_dmg(path);
        } else {
            return InstallOutcome {
                state: "failed".into(),
                exit_code: 1,
                output_log: format!("macOS 不支持的安装包类型: {}", path),
            };
        }
    } else if cfg!(target_os = "linux") {
        if lower.ends_with(".deb") {
            // 无 root 时尝试 sudo -n（免密）；失败则原样报错
            let uid = uid_is_root();
            if uid {
                ("dpkg", vec!["-i".into(), path.into()])
            } else {
                ("sudo", vec!["-n".into(), "dpkg".into(), "-i".into(), path.into()])
            }
        } else if lower.ends_with(".rpm") {
            ("rpm", vec!["-i".into(), path.into()])
        } else if lower.ends_with(".sh") {
            ("bash", vec![path.into()])
        } else {
            return InstallOutcome {
                state: "failed".into(),
                exit_code: 1,
                output_log: format!("Linux 不支持的安装包类型: {}", path),
            };
        }
    } else if cfg!(target_os = "windows") {
        if lower.ends_with(".msi") {
            ("msiexec", vec!["/i".into(), path.into(), "/qn".into(), "/norestart".into()])
        } else if lower.ends_with(".exe") {
            (path, vec!["/S".into()])
        } else {
            return InstallOutcome {
                state: "failed".into(),
                exit_code: 1,
                output_log: format!("Windows 不支持的安装包类型: {}", path),
            };
        }
    } else {
        return InstallOutcome {
            state: "failed".into(),
            exit_code: 1,
            output_log: "未知平台，无法安装".into(),
        };
    };
    run_cmd(cmd, &args)
}

fn run_cmd(cmd: &str, args: &[String]) -> InstallOutcome {
    match Command::new(cmd).args(args).output() {
        Ok(o) => {
            let mut log = String::from_utf8_lossy(&o.stdout).to_string();
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            if !err.is_empty() {
                if !log.is_empty() {
                    log.push('\n');
                }
                log.push_str(&err);
            }
            let code = o.status.code().unwrap_or(-1) as i64;
            InstallOutcome {
                state: if code == 0 { "success".into() } else { "failed".into() },
                exit_code: code,
                output_log: log,
            }
        }
        Err(e) => InstallOutcome {
            state: "failed".into(),
            exit_code: 127,
            output_log: format!("执行 {} 失败: {}", cmd, e),
        },
    }
}

fn uid_is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

// macOS .zip：解包，含 .app 则复制到 /Applications
fn install_zip(path: &str) -> InstallOutcome {
    let workdir = format!("/tmp/yuduan-unzip-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&workdir);
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        return InstallOutcome { state: "failed".into(), exit_code: 1, output_log: format!("mkdir failed: {}", e) };
    }
    let unzip = run_cmd("ditto", &["-x".into(), "-k".into(), path.into(), workdir.clone()]);
    if unzip.state != "success" {
        let _ = std::fs::remove_dir_all(&workdir);
        return unzip;
    }
    // 查找 .app 并复制到 /Applications
    let apps = find_apps(&workdir);
    let mut logs = vec![unzip.output_log];
    let mut ok = true;
    for app in apps {
        let dest = format!("/Applications/{}", basename(&app));
        let cp = run_cmd("ditto", &[app.clone(), dest.clone()]);
        logs.push(format!("cp {} -> {}: {}", app, dest, cp.output_log.trim()));
        if cp.state != "success" {
            ok = false;
        }
    }
    let _ = std::fs::remove_dir_all(&workdir);
    InstallOutcome {
        state: if ok { "success".into() } else { "failed".into() },
        exit_code: if ok { 0 } else { 1 },
        output_log: logs.join("\n"),
    }
}

// macOS .dmg：挂载 → 复制 .app → 卸载
fn install_dmg(path: &str) -> InstallOutcome {
    let mount_point = format!("/tmp/yuduan-mnt-{}", std::process::id());
    let _ = std::fs::create_dir_all(&mount_point);
    let attach = run_cmd("hdiutil", &["attach".into(), path.into(), "-nobrowse".into(), "-mountpoint".into(), mount_point.clone()]);
    if attach.state != "success" {
        let _ = std::fs::remove_dir_all(&mount_point);
        return attach;
    }
    let apps = find_apps(&mount_point);
    let mut logs = vec![attach.output_log];
    let mut ok = true;
    for app in apps {
        let dest = format!("/Applications/{}", basename(&app));
        let cp = run_cmd("ditto", &[app.clone(), dest.clone()]);
        logs.push(format!("cp {} -> {}: {}", app, dest, cp.output_log.trim()));
        if cp.state != "success" {
            ok = false;
        }
    }
    let _ = run_cmd("hdiutil", &["detach".into(), mount_point.clone(), "-quiet".into()]);
    let _ = std::fs::remove_dir_all(&mount_point);
    InstallOutcome {
        state: if ok { "success".into() } else { "failed".into() },
        exit_code: if ok { 0 } else { 1 },
        output_log: logs.join("\n"),
    }
}

fn find_apps(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    walk(dir, &mut |p| {
        if p.to_ascii_lowercase().ends_with(".app") {
            out.push(p.to_string());
        }
    });
    out
}

fn walk(dir: &str, f: &mut dyn FnMut(&str)) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(s) = p.to_str() {
                    f(s);
                    walk(s, f);
                }
            }
        }
    }
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_works() {
        assert_eq!(basename("/Applications/Foo.app"), "Foo.app");
    }

    #[test]
    fn ext_of_works() {
        assert_eq!(ext_of("http://h:8080/api/v1/repo/sw-abc123"), None);
        assert_eq!(ext_of("http://h:8080/api/v1/repo/sw-abc123.zip"), Some("zip".into()));
        assert_eq!(ext_of("http://h:8080/x.pkg?token=1"), None);
        assert_eq!(ext_of("my-tool"), None);
        assert_eq!(ext_of("my-tool.pkg"), Some("pkg".into()));
    }

    #[test]
    fn missing_url_fails_fast() {
        let o = handle_sw_install("", "macos", "x");
        assert_eq!(o.state, "failed");
    }
}
