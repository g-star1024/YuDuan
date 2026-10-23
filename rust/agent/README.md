# 御端生产副端（Rust 版）

> 依据项目铁律：生产副端用 Rust 重写（捕获/注入/系统服务/防篡改等原生模块）。
> 本目录为 Rust 生产副端的**参考实现**，与 Go 参考副端（`dev/cmd/agent`）协议等价、可互通。

## 硬约束：零依赖、离线构建

本机/受限环境无法访问 crates.io，因此**不使用任何第三方 crate，仅依赖 Rust 标准库**：

- 手写 WebSocket 客户端（TCP + 握手 + 掩码帧）
- 手写极简 JSON 解析/序列化（无 serde）
- 资产采集经系统命令与 `/proc`、`sysctl` 等（跨 macOS / Linux / Windows）

## 构建

不要用 `cargo build`（会因更新 registry 失败），直接用 rustc 离线编译：

```bash
rustc -O src/main.rs -o target/agent
# 运行（默认连 ws://127.0.0.1:8080/ws/agent）
YUDUAN_HUB=ws://127.0.0.1:8080/ws/agent ./target/agent
```

## 与 Go 中枢的互通（已实测）

对接 `dev/cmd/hub` 中枢，控制面协议见 `dev/proto/protocol.md` 与 `dev/internal/proto/proto.go`：

- ✅ WebSocket 握手 / `register` 注册（回传 token 复用设备身份）
- ✅ `register_ack` / `heartbeat`（30s）
- ✅ `command`：`shell`、`collect` 已实现，`result` 回执正常
- ✅ 资产采集：hostname / os / os_version / arch / cpu / mem / disk / serial / mac / ip

实测输出示例（macOS）：

```
[agent] 注册成功 device_id=4b59... ok=true
{"task_id":"t-...","state":"success","exit_code":0,"output_log":"rust_agent_interop_ok\n"}
```

## 待办（生产化 P2）

| 项 | 说明 |
| --- | --- |
| HMAC 指令验签 | 当前未校验 Hub 下发指令的 `sig`/`nonce`；生产需按 `internal/crypto` 等价实现 HMAC-SHA256 + 防重放，并遵循 `YUDUAN_HMAC_ENFORCE` |
| sw_install | 软件分发安装当前为占位；需实现下载 / 校验 / 解包 / 安装 / 状态回报 |
| 远控 M4 | `rc_*` 消息当前忽略；生产需屏幕捕获（H.264/H.265 二进制帧）+ 输入注入 |
| 系统服务化 | Win 服务 / macOS LaunchDaemon / Linux systemd 常驻 + 防篡改 |
| TLS（wss） | 当前仅 `ws://`；生产需 TLS（可借 rustls，需放开依赖） |

## 与 Go 参考副端的分工

- **Go 参考副端**（`dev/cmd/agent`）：打通协议与平台无关逻辑，开发快，逻辑与本 Rust 版等价，便于平移。
- **Rust 生产副端**（本目录）：面向生产的原生模块（捕获/注入/服务化/防篡改）最终落地处。
