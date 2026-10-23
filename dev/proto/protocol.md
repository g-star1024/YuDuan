# 御端 · 通信协议规范（M1 冻结基线 v1）

> 本规范在 TDD 第 3 章基础上冻结，作为 M1 设计冻结交付物。后续变更须经 Gate 评审并递增 `ver`。

## 1. 传输与端点

| 通道      | 协议                | 端点                     | 用途             |
| ------- | ----------------- | ---------------------- | -------------- |
| 管理 REST | HTTPS             | `https://hub/api/v1/*` | 主端管理操作、查询      |
| 设备长连接   | WSS               | `wss://hub/ws/agent`   | Agent 注册/心跳/指令 |
| 控制台长连接  | WSS               | `wss://hub/ws/console` | 主端实时事件/远控信令    |
| 远控媒体    | UDP/RTP 或 TCP（加密） | 经中枢中继或 LAN 直连          | 画面帧 / 输入事件     |

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

| type                                      | 方向           | 说明                                                                            |
| ----------------------------------------- | ------------ | ----------------------------------------------------------------------------- |
| `register`                                | Agent→Hub    | 首次/重连纳管：指纹、平台、版本                                                              |
| `heartbeat`                               | Agent→Hub    | 在线、负载、IP、最后采集摘要                                                               |
| `command`                                 | Hub→Agent    | `remote_control` / `sw_install` / `file_push` / `script_run` / `policy_apply` |
| `ack`                                     | Agent→Hub    | 指令接收确认                                                                        |
| `result`                                  | Agent→Hub    | 执行结果（exit_code / output_log / state）                                          |
| `rc_offer` / `rc_answer` / `rc_candidate` | 主端↔Hub↔Agent | 远控信令（SDP / ICE-lite 候选）                                                       |
| `rc_frame` / `rc_input`                   | 双向（媒体通道）     | 画面帧 / 输入事件                                                                    |
| `event`                                   | Hub→Console  | 实时事件（在线/离线/告警/任务进度）                                                           |

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

| code | 含义                |
| ---- | ----------------- |
| 0    | 成功                |
| 1001 | 未授权/令牌失效          |
| 1002 | 签名校验失败            |
| 1003 | seq 重放/乱序         |
| 2001 | 设备离线              |
| 2002 | 安装失败（见 exit_code） |
| 3001 | 远控被拒（权限/审批）       |
| 3002 | 媒体协商失败            |

## 7. 心跳策略

- 间隔 30–60s；超时 = 3× 间隔判定离线并告警。
- 首次全量采集；后续仅上报变更（差异上报）。

## 8. M4/M5 实现说明（MVP 切片，对 M1 基线的演进）

> M1 基线（§1/§3/§4/§5）规划了 WebRTC 风格 `rc_offer/answer/candidate` 与二进制媒体帧、P2P/LAN 直连。  
> **为快速打通 MVP 链路**，M4 采用「中枢居中中继」的最小实现，仍在 §1 的局域网与 TLS 范围内；  
> 生产目标（P2P 直连、H.264/H.265 编码、二进制帧、SAS 专用通道、加密录像）在后续里程碑演进。

### 8.1 已落地消息类型（JSON over WebSocket，复用控制长连接）

| type         | 方向               | 说明                                                                |
| ------------ | ---------------- | ----------------------------------------------------------------- |
| `rc_request` | Viewer→Hub→Agent | 发起远控：{device_id, session_id, operator}；中枢校验设备在线后转发被控端             |
| `rc_ready`   | Agent→Hub→Viewer | 被控端采集就绪 {session_id, ok, reason}                                  |
| `rc_frame`   | Agent→Hub→Viewer | 一帧画面 {session_id, seq, width, height, format:"jpeg", data:base64} |
| `rc_input`   | Viewer→Hub→Agent | 键鼠输入 {session_id, kind, x, y, button, key}                        |
| `rc_stop`    | 任一端→对端           | 结束会话                                                              |
| `rc_event`   | Agent→Hub→Viewer | 合规/确认事件 {session_id, kind, message, require_consent, consented}   |

- 会话中继：中枢维护 `deviceID→{viewerConn,session}`；被控端画面/事件按 deviceID 转发给对应 viewer，输入反向转发。
- 合规提示：Agent 收到 `rc_request` 即在被控端弹本地通知（`远端提示`，员工可拒绝）+ 回报 `rc_event(consent_required)`；生产需员工确认后才开流（MVP 开发态自动授权）。

### 8.2 M5 软件分发通道

- 仓库：`POST /api/v1/software`（multipart 上传）→ 落盘 `data/repo/<id>` + 内存目录；**上传即计算 SHA-256 并在配置 `YUDUAN_SIGN_SEED` 时用 Ed25519 对「32 字节摘要」签名**，校验参数随目录项持久（`sha256`/`signature`/`signer`）。
- 下载：`GET /api/v1/repo/<id>`（Agent 经 HTTP 拉取，LAN 内）。
- 下发：`POST /api/v1/devices/:id/commands {action:"sw_install", software_url, platform, pkg_name, sha256, signature, signer}`。  
  中枢在 `software_url` 指向本仓库（`/api/v1/repo/<id>`）时，**自动解析仓库项并附加校验参数**；外部 URL 不附加。
- Agent 侧（Rust 生产副端）：下载 → **先 SHA-256 比对、再 Ed25519 验签**（签名为「对 32 字节摘要」的签名，受信公钥优先取 payload `signer`、缺省回退 `YUDUAN_SW_PUBKEY`）→ 校验通过才按扩展名静默安装（`.pkg`→`installer -target /`；`.zip`→解包；`.msi/.exe`→Windows 静默；`.deb/.rpm`→Linux）→ 回报 `result`。
  - 哈希不符 `exit_code=3`、验签失败 `exit_code=4`，均拒绝安装；**只给一半校验参数视为配置错误、同样拒绝**（避免「以为验签了其实没验」）。
  - 未配置校验参数（旧中枢/外部 URL）= 仅记录哈希、放行（兼容态）。
- 编码：画面帧当前为 JPEG base64（清晰、零依赖）；生产演进为 H.264/H.265 二进制帧（见 §4）。

### 8.3 生产化远控（P2P 直连 + 二进制帧 + 编码器协商 + 原生编码器 ABI）

> M4 MVP（§8.1）采用「中枢居中中继 + JPEG base64 文本帧」。本节能演进为 §4 规划的生产目标：  
> **二进制帧 + 编解码器协商 + P2P 直连媒体通道（中继兜底）+ 原生编码器（H.264/H.265/VP8/VP9/AV1）**。  
> 既有 JSON 中继路径保留为 P2P 不可达时的兜底，二者并存、不互斥。  
> **2026-10-23：运行时闭环已接入**（见 8.3.5），端到端验证通过（P2P 直连 / 中继 / 回退 / RAW_RGBA / 原生编码器桩 / 诚实回退 / ICE 候选升级，共 7+ 场景）。

#### 8.3.1 二进制帧（固定头 + payload）

```
[ magic:u32 'YDYF' ][ version:u8=1 ][ stream:u8 ][ codec:u8 ][ flags:u8 ]
[ seq:u32 ][ width:u16 ][ height:u16 ][ pts_ms:u32 ][ len:u32 ][ payload:bytes[len] ]
```

- `stream`：0 屏幕 / 1 输入回显 / 2 音频。
- `codec`：`0=RAW_RGBA 1=JPEG 2=H264 3=H265 4=VP8 5=VP9 6=AV1`。
- `flags`：bit0=I 帧 / bit1=黑屏 / bit2=水印。
- 小端；头固定 24 字节；payload 零拷贝切片。对应实现 `rust/agent/src/rc_media.rs`（`encode_frame`/`decode_frame`）。

#### 8.3.2 编解码器协商

1. 被控端在 `rc_ready` 后发 `rc_capabilities`（JSON）：`{codecs, max_w, max_h, fps}`，codecs 为可产出编解码器 ID。
   - 纯 Rust 回退恒宣告 `RAW_RGBA(0)` + `JPEG(1)`；配置 `YUDUAN_ENCODER_LIB` 时追加原生编解码器（2–6）。
2. 主端回 `rc_select`：`{codec, transport, media_addr?}`。协商规则：主端偏好列表**有序**，取第一个被控端也支持的编解码器（`negotiate()`），无交集则维持中继 + JPEG。
3. `transport`：`p2p` 直连 / `relay` 中枢中继。

#### 8.3.3 P2P 直连信令与媒体通道

- 信令：`rc_offer`（主端候选地址 `candidates:["ip:port"]`）→ 中枢中继 → 被控端；`rc_answer`（被控端候选）→ 主端；`rc_candidate` 双向增量追加（ICE-lite 风格）。
- 媒体：双方尝试 `dial_direct(media_addr)` 建立 TCP（3s 超时）；**直连失败则回退中枢中继**（既有 JSON `rc_frame` / 二进制帧经中继亦可）。直连媒体流按「4 字节小端长度前缀 + 帧体」切分（`send_frame`/`recv_frame`），对端 `decode_frame` 还原。
- 中枢仅做信令/媒体中继，不解密帧内容；局域网不出公网（铁律 1）。

#### 8.3.4 原生编码器 C-ABI（H.264/H.265/VP8/VP9/AV1）

- 纯 Rust 不内置复杂编码器；由部署期动态库（`.so`/`.dylib`/`.dll`）提供，经系统 libc `dlopen`/`LoadLibrary` 加载（与 rc.rs 链接系统框架一致，零第三方 crate）。
- 约定符号：`yuduan_enc_create(codec, w, h)` / `yuduan_enc_encode(h, rgba, len, out, cap)→written` / `yuduan_enc_destroy(h)`。
- 环境变量 `YUDUAN_ENCODER_LIB` 指向该库；缺失或加载失败时 `make_encoder()` **自动回退** `RAW_RGBA`/`JPEG`，二进制帧通道不中断。
- 编码器协商命中原生编解码器即走原生库；否则回退（仅带宽略高，功能不受影响）。

#### 8.3.5 运行时闭环（2026-10-23 接入，端到端已验证）

**端到端口径**（被控端 = Rust 副端；主端 = Tauri/浏览器）：

```
主端 rc_request ──► 被控端 start_rc：合规提示 + rc_ready + rc_capabilities（扁平载荷）
主端 ◄── 能力告知后回 rc_select{codec, transport, media_addr?}
被控端 handle_rc_select：clamp 协商 codec →（p2p）dial_direct(media_addr) /（relay）二进制 WS 通道
帧循环（500ms）：capture → 按协商 codec 编码 → encode_frame → send_frame(TCP) / 二进制 WS 帧
主端 ◄── 二进制帧（hub 对 0x2 opcode 双向透传，不解内容）
任一端 rc_stop ──► 会话结束（清通道、drop 直连）
```

- **编解码器协商与 clamp（诚实回退，不造假帧）**：协商期被控端做能力探测——`rgba_capture_ok()`（一次真实 RGBA 采集）与 `NativeEncoder::try_load()`（动态库能否创建该编解码器句柄）；`rc_capabilities` 仅宣告真实可产出的编解码器（配置 `YUDUAN_ENCODER_LIB` 时逐个探测，不虚标）。主端选择不可产出的编解码器时回退 JPEG，并以 `rc_event(kind=notice)` 明确告知。能力宣告载荷为扁平字段（与 Go `proto.RCCapabilitiesPayload` 一致）。
- **采集源随协商 codec 切换**：JPEG 走采集工具链（macOS `screencapture` / Linux `scrot|import`）；`RAW_RGBA`/原生走 RGBA 采集源（§8.3.6：macOS CoreGraphics / Linux X11 XGetImage / Windows GDI，统一输出紧凑 RGBA8，BGRA→RGBA 交换）。RAW 数据不会冒充当 JPEG：通道损坏时清通道，下一轮回采 JPEG 走 JSON 兜底。
- **通道选择**：`transport=p2p` 且 `media_addr` 可达 → P2p 直连（`dial_direct` 3s 超时）；首跳失败不放弃——保持 `RC_WANT_P2P`，等待 `rc_offer`/`rc_candidate` 候选到达后逐个 dial（ICE-lite），全部失败才回退经中枢长连接的二进制 WS 帧中继（`ws` 包 opcode `0x2`，hub 双向透传）。会话不中断。
- **主端差异**：浏览器（`console/rc.html`）无法监听裸 TCP，固定 `transport=relay`，按 `ArrayBuffer` 收帧并解码渲染（jpeg→canvas、raw_rgba→`putImageData`、原生码流→WebCodecs `VideoDecoder`：Annex-B 拆 NAL、SPS/PPS 组 avcC description、AVCC 长度前缀送帧，失败自动回退帧统计）；P2P 直连服务原生主端（Tauri）。
- **兜底**：未协商二进制通道（rc_select 未达/前）或通道损坏（编码/发送失败）时，帧循环回退 JSON `rc_frame`（M4 MVP 兼容），损坏时清通道并发 `rc_event(notice)`。
- **会话代次守卫**：`RC_GEN` 代次计数防止上一次会话的帧循环残留（连续会话场景）。
- **参考实现与验证**：`dev/cmd/p2pviewer`（e2e viewer：建会话→等能力→rc_select→广播 ICE 候选→按 transport 收帧→校验 magic/SOI/RAW 尺寸→落盘→统计；`media_selected` 事件可获悉被控端实际 codec）。

#### 8.3.6 RGBA 采集源 / ICE-lite / 原生编码器验证（2026-10-23 补齐）

- **RGBA 采集源（已接入，macOS 实测）**：
  - macOS：CoreGraphics `CGDisplayCreateImage` → `CGDataProviderCopyData`（CFData），32bpp BGRA8888 逐像素交换 B/R 得紧凑 RGBA8；
  - Linux：X11 `XGetImage`（ZPixmap，TrueColor 32bpp BGRX/BGRA 或 24bpp 紧凑，交换 B/R）；
  - Windows：GDI `BitBlt` + `GetDIBits`（BI_RGB 32bpp，负高度自顶向下，交换 B/R）。
  - 实测（macOS 3584×2240）：RAW_RGBA 帧载荷 32,112,640 B = 宽×高×4，端到端 3 帧全过。无权限/无显示/布局不符返回 None，协商期探测后诚实回退 JPEG。
- **ICE-lite 候选交换（已接入并实测）**：主端（p2pviewer）按监听绑定方式生成候选——具体 IP 绑定 → 唯一候选；通配绑定（0.0.0.0/::）→ 枚举非回送网卡 IPv4 + 监听端口；`rc_offer`（批量）+ `rc_candidate`（增量）经中枢中继。被控端 `try_candidate`：仅 `RC_WANT_P2P` 且尚无 P2P 通道时逐个 dial，成功即替换通道（保留协商 codec）并发 `media_selected`。实测两型：具体绑定候选 `127.0.0.1:port` 升级成功；通配绑定候选 `192.168.0.109:port`（LAN IP）升级成功。
- **原生编码器 C-ABI（已端到端验证）**：`clang -dynamiclib` 编译验证桩（仅接受 h264，输出 `"STUB"`+RGBA），`YUDUAN_ENCODER_LIB` 指向后：能力探测宣告 `codecs=[raw_rgba,jpeg,h264]`（h265/vp8/vp9/av1 被桩拒绝故不宣告）；协商 h264 → RGBA 采集 → 桩编码 → 二进制帧，对端校验载荷首 4 字节为 `STUB`、总长 = 宽×高×4+4，3 帧全过。真实 H.264/H.265 编码器动态库按同一 ABI 供给即可上线。
- **原生码流解码渲染（浏览器参考台已接 WebCodecs）**：`rc.html` 对 h264/h265/vp8/vp9/av1 帧走 `VideoDecoder`（Annex-B 拆 NAL → avcC description（SPS/PPS）→ 4 字节长度前缀送 `EncodedVideoChunk` → `VideoFrame` 绘 canvas）；`VideoDecoder` 不可用/configure/decode 失败自动回退帧统计。NAL 拆分/avcC 组包/AVCC 转换经合成 Annex-B 流（SPS+PPS+IDR）逐字节验证。

#### 8.3.7 仍缺（生产化最后段）

- **真实原生编码器动态库**：H.264/H.265 等（可用 VideoToolbox/VAAPI/MF 或 x264/x265 封装 C-ABI）；当前仅桩库验证 ABI 契约。
- **Linux/Windows 实跑**：采集（X11/GDI）与输入注入（XTest/SendInput）分支已写，需对应机器实测；本机（macOS）已全量验证。
- **媒体环性能**：帧循环当前 500ms/帧（2fps）、5K 屏 RAW 32MB/帧；生产需协商降采样（capabilities.max_w/h 生效）、可变帧率与带宽自适应。
- **录像留存**：§5 加密录像留存策略待产品决策后实现。
