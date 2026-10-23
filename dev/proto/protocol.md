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
5. 会话结束 → 生成 `sessions` 记录 + 加密录像（留存 ≤90 天自动清理；实现见 §8.3.12，opt-in）。

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
帧循环（可变帧率，§8.3.8）：capture → 按协商 codec 编码 → encode_frame → send_frame(TCP) / 二进制 WS 帧
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
- **原生编码器 C-ABI（已端到端验证：桩 + 真实 VideoToolbox 库）**：
  - 验证桩：`dev/fixtures/yuduan_enc_stub.c`（仅接受 h264，输出 `"STUB"`+RGBA），对端校验载荷首 4 字节 `STUB`、总长 = 宽×高×4+4，3 帧全过。
  - **真实库**：`dev/encoder-vt/`（macOS VideoToolbox 后端，`build.sh` 构建）。出帧模型与参数集处理要求见 §8.3.8（2026-10-24 夜间修正：H.264 可用、P 帧生效、帧率提升 5 倍）。
  - **实测（macOS，真实 HEVC/H.264）**：能力宣告 `codecs=[raw_rgba,jpeg,h264,h265]`；协商后 → RGBA 采集 3584×2240 → 降采样 1728×1080 → VT 编码 → 二进制帧：首帧为 Annex-B 参数集 + IDR（H.264 `00 00 00 01 27...` / HEVC `00 00 00 01 40...`），后续 P 帧 0.7~24 KB（对比 RAW 7.46MB/帧）。探测失败的 codec 诚实回退 JPEG 并告知（不虚标）。
  - 副端探测结果进程级缓存（`native_probe_cached`）——异常编码器服务下单次探测可达数十秒，不重复惩罚。
- **原生码流解码渲染（浏览器参考台已接 WebCodecs）**：`rc.html` 对 h264/h265/vp8/vp9/av1 帧走 `VideoDecoder`（Annex-B 拆 NAL → avcC description（SPS/PPS）→ 4 字节长度前缀送 `EncodedVideoChunk` → `VideoFrame` 绘 canvas）；`VideoDecoder` 不可用/configure/decode 失败自动回退帧统计。NAL 拆分/avcC 组包/AVCC 转换经合成 Annex-B 流（SPS+PPS+IDR）逐字节验证。

#### 8.3.7 仍缺（生产化最后段）

- **其他平台真实编码器**：Linux（VAAPI 或 x264/x265）、Windows（Media Foundation）按 §8.3.4 契约供给动态库即可（macOS VideoToolbox 已落地，见 §8.3.6 / §8.3.8）。
- **Linux/Windows 实跑**：采集（X11/GDI）与输入注入（XTest/SendInput）分支已写，需对应机器实测；本机（macOS）已全量验证。
 - **带宽自适应**：✅ 主端反馈闭环已落地（§8.3.9，rc_qos）；传输级 RTT 档位夹取已落地（§8.3.11，rc_ping/rc_pong）。仍缺丢包率测量与码率动态重参数化（后续扩展）。
 - **JPEG 工具链路径降采样**：✅ 已落地（§8.3.10，内置 JPEG 编码器 + RGBA 采集 + 降采样；CLI 路径保留为兜底）。
 - **录像留存**：§5 加密录像留存策略待产品决策后实现。
+
+#### 8.3.10 JPEG 路径降采样 + 内置 JPEG 编码器（2026-10-24，端到端已验证）
+
+**原路径**：JPEG 由平台 CLI 采集（macOS `screencapture` / Linux `scrot`/`import`），只能全分辨率出帧
+（5K 屏 2.3MB/帧），且 Windows 无对应 CLI。**新路径**：`capture_rgba`（三平台同构，§8.3.6）
+→ `fit_dims` 按协商目标降采样 → 内置 baseline JPEG 编码器（`rust/agent/src/jpegenc.rs`，纯 std）。
+CLI 路径保留为 RGBA 采集/编码失败时的兜底。实测：3584×2240 屏 → 1728×1080 JPEG **~222KB/帧**
+（10 倍于原 2.3MB 的带宽改善），q80。
+
+**实现要点（写第二个 JPEG 编码器前必读，均为本机实测踩坑）**：
+| 踩坑 | 症状 | 正确做法 |
+|---|---|---|
+| cos 表转置（`cos((2x+1)fπ/16)` 写成 `cos((2f+1)xπ/16)`） | 常数块产生大量假 AC 系数（纯灰 256×256 从 1.6KB 涨到 8.4KB）；且若行列 pass 同步转置，自测体积无异常，**只能靠外部解码器暴露** | 核必须是 `cos((2n+1)·f·π/16)`（n=空间，f=频率）；行/列 pass 统一按 `[频率][空间]` 取索引 |
+| Huffman 码表按 values 数组下标索引 | 符号 0xD1 越界 panic（码表只有 162 项，符号空间是 256） | 构建 256 项「符号值 → (code,len)」直查表 |
+| SOF0 分量三元组少写字节 / 采样因子 Y/色度写反 | ffmpeg `decode_sof0: len mismatch`；自测全绿 | 每分量 3 字节 (id, 采样 h/v, 量化表 id)；4:2:0 必须 **Y=0x22、色度=0x11**（MCU=4Y+1Cb+1Cr） |
+| 色度下采样原位覆盖 | 半分辨率平面首行被污染 | 写入独立半分辨率平面 |
+
+**验证要求**：JPEG 编码器必须过**两个独立解码器**（本机：ffmpeg 零错误 + macOS `sips` 尺寸/解码）
+方可入库——自洽的编码器不等于标准的编码器。端到端：`p2pviewer -codec 1` 收帧头 `ff d8 ff e0 00 10 4a 46`
+（SOI+APP0+JFIF）+ 1728×1080 + ffmpeg 零错误。
+
+#### 8.3.9 带宽自适应闭环：rc_qos 主端反馈（2026-10-24，端到端已验证）
+
+**消息**：`rc_qos`（Viewer→Hub→Agent，JSON 文本信封）：
+
+```json
+{ "session_id": "s-1", "fps": 8, "reason": "decode_slow" }
+```
+
+- `fps`：主端请求的目标帧率（1..=120 之外被控端忽略本条；**0 = 恢复 capabilities.fps 默认**）。
+- `reason`：`user_slider`（操作员手动调节）/ `auto_adapt`（主端自动）/ `decode_slow` 等诊断标注。
+- **副端应用规则**：夹取后写入帧率覆盖（优先级 rc_qos → `YUDUAN_RC_FPS` → capabilities.fps），
+  帧循环**逐轮重算**间隔——会话中途升降立即生效，无需重建媒体通道；新会话开始清零恢复默认。
+- **主端信号**（rc.html 参考台）：每帧测量「到达→绘制完成」耗时（近 20 帧滑动平均）；
+  平均耗时 > 0.8×帧预算 → 降档（×0.7，下限 2fps）；平均 < 0.4×帧预算 连续 5 轮（15s）→ 升档（+2，不超能力上限）；
+  评估节流 ≥3s。手动模式（滑杆）直接反馈。参考 viewer（p2pviewer）`-qos N` 每 2s 周期反馈（联调用）。
+- **为什么不做成单向静态**：同一份 capabilities 在不同主端（浏览器软解/Tauri 硬解/跨洋中继）上
+  解码能力差异巨大，被控端无法自行得知主端状态；由主端反馈是唯一诚实的信号源。
+  传输级自适应（按 RTT/丢包调码率）需编码器参数动态调整，与录像留存一并后续决策。

#### 8.3.8 真实原生编码器量产级修正 + 可变帧率（2026-10-24 夜间，端到端已验证）

**VideoToolbox 出帧模型（新动态库必须遵守，否则库在系统中表现为"卡住"或"带宽暴涨"）**：

| 现象（macOS 实测） | 根因 | 要求做法 |
|---|---|---|
| 提交第 N 帧后同步等它自己的回调，**永不返回**（90s 亦如此） | VT 会话固定 **1 帧管线延迟**：第 N-1 帧的输出是在提交第 N 帧时才被"顶"出来 | 采用队列：回调入队，`encode()` 取队首；输出比输入滞后 1 帧 |
| 每帧调用 `VTCompressionSessionCompleteFrames` 兜底 → **每帧 IDR**（20~40KB/帧） | CompleteFrames 会把被推出的帧**强制成关键帧** | 仅在管线饥饿（超时未出帧，如首帧/断流重启）时兜底一次；稳态不 flush |
| 软件 H.264（`EnableHardwareAcceleratedVideoEncoder=false`）会话**不回调** | 本机软编路径异常 | 默认不指定编码器规格（系统启发式）；`YUDUAN_ENC_HW=0/1` 仅作排障旋钮 |

- **会话预置（prime）**：`create()` 提交两帧黑帧——第 2 帧顶出第 1 帧的输出，借此① 吸收冷启动；② 校验该 codec 真能出码流并取到参数集（`create()` 失败即能力不宣告，不虚标）；③ 第 2 帧的输出留在队列，成为码流首个关键帧。副作用：码流开头有 1 帧黑 IDR（含参数集，解码器可立即起播），属预期行为。
- **参数集**：从输出样缓冲的 `CMFormatDescription` 提取（H.264 SPS/PPS；HEVC VPS/SPS/PPS），**随同步样（关键帧）前置**，保证中途接入的解码器可起播。
- **重投语义**：caller 缓冲不足返回 0 时，该帧保留在 `pending`，扩张后的重试交付**同一帧**（不重复编码、不乱序）。
- **载荷种类守卫（首帧竞态修复）**：帧循环可能在 `rc_select` 到达前采到 JPEG。此时若直接送 H.264 编码器，缓冲区扩张到 4 倍仍不可能成功。约定：通道 codec 与载荷种类不符时返回 `Recapture`——丢弃本帧**不清通道**（区别于 `Broken`），下一轮按已协商 codec 重采。此前靠帧循环首轮 `sleep(500ms)` 侥幸避开该竞态，可变帧率上线后立即暴露。
- **可变帧率（§8.3.7 性能项关闭）**：帧目标取 `capabilities.fps`（默认 15，`YUDUAN_RC_FPS` 可覆盖）→ 每帧预算 `1000/fps` ms；每轮结束算下一次截止时刻，休眠只补足剩余预算——采集/编码慢时自然降帧而不叠加延迟。每 30 帧输出「单帧成本 / 实际 fps」便于判断目标是否可达。
- **实测（macOS 本机，2026-10-24）**：

| 项 | 修正前 | 修正后 |
|---|---|---|
| h264 / h265 真实码流 | 全帧 IDR（30~40KB/帧） | 首帧 IDR + P 帧（0.7~24KB） |
| 端到端实测帧率 | 1.9 fps（固定 sleep 500ms） | 9.4 fps（15 fps 目标，受采集成本限制） |
| 8 帧总带宽（1920×1080 级） | 240,975 B | 222,141 B（含首帧内容帧 210KB；稳态 P 帧 2KB 级） |

- **可复现 fixture**：`dev/encoder-vt/harness.c`（直测 C-ABI，合成渐变+运动方块内容 → 裸流）、`dev/encoder-vt/annexb_stat.py`（Annex-B 切分 + NAL 类型 + ffprobe 交叉验证）、`dev/e2e_vt_encoder.py`（中枢+副端+viewer 全链路，含 ffmpeg 全帧零错误解码校验）。三者均要求：`00 00 00 01` 起始码、无空 NAL、参数集在位、ffmpeg 解码无错、关键帧比例正常。

### 8.3.11 传输级 RTT 自适应（2026-10-24 落地）

媒体路径网络质量探测 + 副端帧率夹取，与 §8.3.9 主端反馈闭环独立生效（两者取 min）：

- **`rc_ping`**（Viewer→Hub→Agent）：`{session_id, t0}`，t0=主端本地毫秒时间戳；主端每 2s 周期发送（rc.html 会话内持续；p2pviewer `-probe` 每 1s）。
- **`rc_pong`**（Agent→Hub→Viewer）：副端**原样回显** `{session_id, t0}`；主端 RTT = now − t0。跨中继/P2P 通道同语义（应用层探测，不依赖 ICMP）。
- **`rc_qos.rtt_ms`**：主端把实测 RTT 随 QoS 反馈顺带上报；副端按档位夹帧率上限：

| RTT | 帧率上限 | 说明 |
|---|---|---|
| ≤80ms | 不限 | LAN 正常 |
| ≤200ms | 15fps | 跨网段/轻度拥塞 |
| ≤400ms | 8fps | 中度拥塞 |
| >400ms | 4fps | 重度拥塞（保交互性） |

- **副端实现**：`RC_RTT_MS` 原子量，新会话清零（等主端重新探测）；合法区间 0..=60000（越界忽略）；`rc_frame_interval_ms() = min(rc_qos.fps / env / caps, rtt_cap)`。RTT 跨档变化打日志。
- **主端实现**：rc.html 状态栏显示 `RTT xx ms`；自适应模式下 RTT>250ms 触发立即降档（不等渲染统计）；每次 rc_qos 顺带携带最新 RTT。
- **尚缺**：丢包率测量（需序列号/到达率统计）与码率动态调整（需编码器运行时重参数化接口），后续扩展。

### 8.3.12 加密录像留存（§5 落地，opt-in）

- **开关**：`YUDUAN_RC_RECORD=1`（默认关）。目录 `YUDUAN_RECORD_DIR`（默认 `data/recordings`，0700）；留存 `YUDUAN_RECORD_RETENTION_DAYS`（默认 90）。
- **文件格式**（`.yrec`，`internal/record`）：行1 `YDREC01` 魔数；行2 会话元数据 JSON（session/device/operator/started_at/consent——明文，供运维检索审计）；其后逐帧块 `[4B 大端明文长度][12B nonce][AES-256-GCM 密文+16B tag]`，明文即 24B 帧头+payload 的原始媒体帧。
- **密钥**：主密钥 `YUDUAN_RECORD_KEY`（32B hex）/ `_FILE`（keystore 收口）；未配置时由 HMAC 密钥派生（`sha256("yuduan-record:"+secret)`，开发态兜底——生产必须显式配置独立密钥）。块密钥按会话派生 `sha256(master‖session_id)`。
- **流程**：中继首帧懒开文件（失败仅记一次日志，不阻断会话）；endSession 落盘 + 审计 `rc_record_saved`；Janitor 每小时按 mtime 清理过期文件。回放 `record.ReadAll(path, master)` 逐帧还原，密钥错误/篡改即 GCM 校验失败。
- **API**：`GET /api/v1/recordings`（PermAuditView）列出 `{name,bytes,modified}`（不含内容）。
- **边界**：仅中继（relay）路径录像——P2P 直连帧不经中枢，属设计预期（P2P 模式如需录像须副端/主端本地实现，待产品决策）。
