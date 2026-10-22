# 御端 · 通信协议规范（M1 冻结基线 v1）

> 本规范在 TDD 第 3 章基础上冻结，作为 M1 设计冻结交付物。后续变更须经 Gate 评审并递增 `ver`。

## 1. 传输与端点

| 通道 | 协议 | 端点 | 用途 |
|---|---|---|---|
| 管理 REST | HTTPS | `https://hub/api/v1/*` | 主端管理操作、查询 |
| 设备长连接 | WSS | `wss://hub/ws/agent` | Agent 注册/心跳/指令 |
| 控制台长连接 | WSS | `wss://hub/ws/console` | 主端实时事件/远控信令 |
| 远控媒体 | UDP/RTP 或 TCP（加密） | 经中枢中继或 LAN 直连 | 画面帧 / 输入事件 |

- 所有通道 TLS；设备/主端持设备令牌或账号令牌。
- 网络范围：仅公司局域网（LAN）。**禁止公网端点**（铁律 1）。

## 2. 控制消息信封（JSON over WebSocket）

```json
{
  "ver": 1,
  "msg_id": "uuid",
  "ts": 1698000000,
  "type": "command",
  "seq": 1001,
  "payload": { "action": "sw_install", "task_id": "t-001" },
  "sig": "hex(hmac_sha256(server_secret, msg_id|ts|type|seq|payload_json))"
}
```

- `seq`：每设备单调递增，用于防重放与顺序校验。
- `sig`：仅**关键指令**（`remote_control` / `uninstall` / `policy_apply`）由服务端签名，Agent 验签；其余消息以会话级 TLS + 令牌保障。
- 心跳/回执类消息不强制签名，但须携带有效令牌。

## 3. 消息类型

| type | 方向 | 说明 |
|---|---|---|
| `register` | Agent→Hub | 首次/重连纳管：指纹、平台、版本 |
| `heartbeat` | Agent→Hub | 在线、负载、IP、最后采集摘要 |
| `command` | Hub→Agent | `remote_control` / `sw_install` / `file_push` / `script_run` / `policy_apply` |
| `ack` | Agent→Hub | 指令接收确认 |
| `result` | Agent→Hub | 执行结果（exit_code / output_log / state） |
| `rc_offer` / `rc_answer` / `rc_candidate` | 主端↔Hub↔Agent | 远控信令（SDP / ICE-lite 候选） |
| `rc_frame` / `rc_input` | 双向（媒体通道） | 画面帧 / 输入事件 |
| `event` | Hub→Console | 实时事件（在线/离线/告警/任务进度） |

## 4. 远控媒体帧（二进制）

```
[ session_id:16 ][ frame_seq:u32 ][ flags:u8 ][ data... ]
```
- `flags`：bit0=关键帧(I)、bit1=黑屏、bit2=水印。
- 编码：H.264/H.265 或 VP8/9（专利评估见 TDD 15）；LAN 默认高画质，弱网降码率。
- 输入事件：本地键鼠序列化后回传远端注入；Windows `Ctrl+Alt+Del`（SAS）走专用服务。

## 5. 会话协商流程

1. 主端 `POST /api/v1/sessions`（含 reason、目标设备、审批人）→ 中枢校验 RBAC/审批。
2. 中枢向 Agent 下发 `command(remote_control)`，Agent 回 `ack` 并启动采集，远端弹提示。
3. 主端↔Agent 经中枢交换 `rc_offer/answer/candidate`，LAN 内直连；跨网段由中枢 Relay 中转。
4. 媒体通道建立 → 画面/输入双向；支持黑屏/锁屏/水印/结束。
5. 会话结束 → 生成 `sessions` 记录 + 加密录像（留存 ≤90 天自动清理）。

## 6. 错误码

| code | 含义 |
|---|---|
| 0 | 成功 |
| 1001 | 未授权/令牌失效 |
| 1002 | 签名校验失败 |
| 1003 | seq 重放/乱序 |
| 2001 | 设备离线 |
| 2002 | 安装失败（见 exit_code） |
| 3001 | 远控被拒（权限/审批） |
| 3002 | 媒体协商失败 |

## 7. 心跳策略

- 间隔 30–60s；超时 = 3× 间隔判定离线并告警。
- 首次全量采集；后续仅上报变更（差异上报）。

## 8. M4/M5 实现说明（MVP 切片，对 M1 基线的演进）

> M1 基线（§1/§3/§4/§5）规划了 WebRTC 风格 `rc_offer/answer/candidate` 与二进制媒体帧、P2P/LAN 直连。
> **为快速打通 MVP 链路**，M4 采用「中枢居中中继」的最小实现，仍在 §1 的局域网与 TLS 范围内；
> 生产目标（P2P 直连、H.264/H.265 编码、二进制帧、SAS 专用通道、加密录像）在后续里程碑演进。

### 8.1 已落地消息类型（JSON over WebSocket，复用控制长连接）

| type | 方向 | 说明 |
|---|---|---|
| `rc_request` | Viewer→Hub→Agent | 发起远控：{device_id, session_id, operator}；中枢校验设备在线后转发被控端 |
| `rc_ready` | Agent→Hub→Viewer | 被控端采集就绪 {session_id, ok, reason} |
| `rc_frame` | Agent→Hub→Viewer | 一帧画面 {session_id, seq, width, height, format:"jpeg", data:base64} |
| `rc_input` | Viewer→Hub→Agent | 键鼠输入 {session_id, kind, x, y, button, key} |
| `rc_stop` | 任一端→对端 | 结束会话 |
| `rc_event` | Agent→Hub→Viewer | 合规/确认事件 {session_id, kind, message, require_consent, consented} |

- 会话中继：中枢维护 `deviceID→{viewerConn,session}`；被控端画面/事件按 deviceID 转发给对应 viewer，输入反向转发。
- 合规提示：Agent 收到 `rc_request` 即在被控端弹本地通知（`远端提示`，员工可拒绝）+ 回报 `rc_event(consent_required)`；生产需员工确认后才开流（MVP 开发态自动授权）。

### 8.2 M5 软件分发通道

- 仓库：`POST /api/v1/software`（multipart 上传）→ 落盘 `data/repo/<id>` + 内存目录。
- 下载：`GET /api/v1/repo/<id>`（Agent 经 HTTP 拉取，LAN 内）。
- 下发：`POST /api/v1/devices/:id/push {software_id}`（便捷）或 `POST /api/v1/devices/:id/commands {action:"sw_install", software_url, platform, pkg_name}`。
- Agent 侧：下载 → 按扩展名静默安装（`.pkg`→`installer -target /`；`.zip`→解包；`.msi/.exe`→Windows 静默；`.deb/.rpm`→Linux）→ 回报 `result`。
- 编码：画面帧当前为 JPEG base64（清晰、零依赖）；生产演进为 H.264/H.265 二进制帧（见 §4）。

