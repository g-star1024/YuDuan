// 无头验收：对真实 hub 跑登录 →（强制改密→改密续签）→ 设备列表 →（可选）指令下发。
// 用法：cargo run --bin hubcheck -- <base> <username> <password> [new_password] [device_id action command]
//   例：cargo run --bin hubcheck -- http://127.0.0.1:18241 admin admin123 'Admin@2026x'
// 退出码：0 = 全部通过；非 0 = 任一步失败（CI 可用）。

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法: hubcheck <base> <username> <password> [new_password] [device_id action command]");
        exit(2);
    }
    let base = args[1].clone();
    let (user, pass) = (args[2].clone(), args[3].clone());
    let new_pass = args.get(4).cloned().unwrap_or_default();

    // 1. 登录
    let mut lr = yuduan_main_lib::hub_client::login(&base, &user, &pass).unwrap_or_else(|e| {
        eprintln!("[FAIL] 登录: {}", e);
        exit(1);
    });
    println!(
        "[OK] 登录: role={} must_change_password={} token_len={}",
        lr.role,
        lr.must_change_password,
        lr.token.len()
    );

    // 2. 强制改密 → 改密续签（生产默认姿态下 admin 首登必须改密，改完才放行业务接口）
    if lr.must_change_password {
        if new_pass.is_empty() {
            eprintln!("[FAIL] hub 要求强制改密，但未提供 new_password 参数");
            exit(1);
        }
        lr = yuduan_main_lib::hub_client::change_password(&base, &lr.token, &pass, &new_pass)
            .unwrap_or_else(|e| {
                eprintln!("[FAIL] 强制改密: {}", e);
                exit(1);
            });
        println!("[OK] 强制改密完成，已续签 token");
    }

    // 3. 设备列表
    let devs = yuduan_main_lib::hub_client::devices(&base, &lr.token).unwrap_or_else(|e| {
        eprintln!("[FAIL] 设备列表: {}", e);
        exit(1);
    });
    println!("[OK] 设备列表: {} 台", devs.len());
    for d in devs.iter().take(10) {
        println!(
            "  {} {} {} {}{} ip={} agent={}",
            &d.id[..d.id.len().min(12)],
            d.hostname,
            d.os,
            d.os_version,
            if d.online { " [在线]" } else { " [离线]" },
            d.ip,
            d.agent_version
        );
    }

    // 4. 可选指令下发
    if args.len() >= 7 {
        let (id, action, command) = (
            args[5].clone(),
            args[6].clone(),
            args.get(7).cloned().unwrap_or_default(),
        );
        let r = yuduan_main_lib::hub_client::device_cmd(&base, &lr.token, &id, &action, &command)
            .unwrap_or_else(|e| {
                eprintln!("[FAIL] 指令下发: {}", e);
                exit(1);
            });
        println!("[OK] 指令下发: {}", serde_json::to_string(&r).unwrap_or_default());
    }

    println!("hubcheck: ALL PASS");
}

