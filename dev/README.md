# 御端 · 研发目录（dev）

> M1 设计冻结产物 + M2–M6 可运行实现 + 安全基座 + Rust 生产副端。本文是研发入口。

## 里程碑状态

| 里程碑 | 状态 | 说明 |
|---|---|---|
| M1 设计冻结 | ✅ | 协议 `proto/protocol.md`、Schema `proto/schema.sql`、接口契约 `agent/INTERFACES.md` |
| M2 底座联调 | ✅ 可运行 | 注册/心跳/指令端到端打通（macOS 实测） |
| M3 资产管理 | ✅ 基础版 | 跨平台采集 + 台账 + 报表（macOS 实测；Linux/Win 待实跑） |
| M4 远程控制 | ✅ 基础版 | 中枢中继 + 真实屏幕采集 + viewer + 输入 + 远端提示合规（macOS 实测） |
| M5 软件分发 | ✅ 基础版 | 仓库上传 + 按设备静默分发 + 状态回报（macOS 实测解包） |
| M6 管理与审计 | ✅ 基础版 | RBAC 登录/门禁 + 审计落库 + AD/LDAP 同步 + 高可用设计（`HA.md`）（macOS 实测） |
| 安全里程碑 | ✅ 基础版 | TLS(https/wss) + HMAC 指令签名/验签 + PostgreSQL 持久化（macOS + 本地 PG 实测） |
| Rust 生产副端 | ✅ 参考互通版 | `rust/agent` 纯 std、rustc 离线编译，与 Go 中枢互通实测通过 |

## 目录结构

```
dev/
├── go.mod                  # Go module: yuduan（零外部依赖）
├── internal/
│   ├── proto/              # 控制面消息信封与载荷（对齐 proto/protocol.md，含 nonce）
│   ├── ws/                 # 最小化 WebSocket（标准库，支持 ws/wss）
│   ├── store/              # 设备/账号/审计仓储：内存 + PostgreSQL 两种实现
│   ├── collect/            # 跨平台资产采集（M3）
│   ├── crypto/             # 安全基座：HMAC 指令签名/验签、登录令牌、口令摘要
│   ├── auth/               # RBAC 角色/权限 + 登录鉴权服务
│   ├── audit/              # 审计事件与 Auditor 接口
│   ├── directory/          # AD/LDAP 目录同步引擎（File/LDAP 两种源）
│   ├── pg/                 # 零依赖 PostgreSQL v3 客户端
│   ├── tlsutil/            # 自签证书生成（stdlib）
│   └── hub/                # 中枢：长连接纳管 / RBAC REST / 指令签名下发 / 报表 / 远控中继 / 软件仓库 / 审计 / 目录同步
├── cmd/
│   ├── hub/                # 中枢入口（env 配置存储/安全/目录同步/TLS）
│   └── agent/              # 副端参考 Agent 入口（Go，生产以 Rust 重写）
│       ├── main.go         # 注册/心跳/指令分发 + HMAC 验签
│       ├── rc.go           # M4 远程控制：采集 / 输入 / 远端提示
│       └── swdist.go       # M5 软件分发：下载 / 静默安装 / 回报
├── console/
│   ├── index.html          # 管理控制台（资产 + 指令 + 导航）
│   ├── rc.html             # M4 远程控制 viewer
│   ├── software.html       # M5 软件分发
│   ├── login.html          # M6 登录页
│   ├── audit.html          # M6 审计查询
│   └── auth.js             # M6 Bearer 注入 + 401 跳登录
├── HA.md                   # M6 高可用与合规上线设计
├── directory-sample.json   # AD/LDAP 目录同步样例
├── e2e_m6.py               # M6/安全端到端验证脚本（17 项）
├── proto/                  # M1 冻结：通信协议 + 数据库 Schema
├── agent/INTERFACES.md     # M1 冻结：副端跨平台接口契约
├── hub/README.md           # 中枢说明
├── agent/README.md         # 副端说明（含 Rust 平移说明）
├── console/README.md       # 控制台说明
└── deploy/README.md        # 部署运维说明
```

仓库根目录另有 `rust/agent/`：Rust 生产副端（纯 std，rustc 离线编译）。

## 本地构建与验证

```bash
cd dev
go build -o bin/hub ./cmd/hub
go build -o bin/agent ./cmd/agent

# 终端 1：启动中枢（默认明文 :8080 + TLS :8443；内置演示账号 admin/admin123）
./bin/hub
# 终端 2、3…：启动多个副端（模拟多台员工电脑）
./bin/agent
# 管理控制台：浏览器打开 http://127.0.0.1:8080/ （先跳登录页；资产/指令/远控/分发/审计导航）
# 或 REST（需 Bearer 令牌）：
curl http://127.0.0.1:8080/api/v1/devices
curl http://127.0.0.1:8080/api/v1/report
```

### M6 / 安全 / 持久化运行（环境变量）

```bash
# PostgreSQL 持久化 + HMAC 强制 + AD 目录同步 + TLS
YUDUAN_DB_DSN="host=127.0.0.1 port=5432 user=postgres dbname=yuduan" \
YUDUAN_HMAC_SECRET="<secret>" YUDUAN_HMAC_ENFORCE=1 \
YUDUAN_DIR_FILE="$(pwd)/directory-sample.json" \
./bin/hub

# 端到端验证（登录/RBAC/审计/TLS/HMAC，17 项）
python3 e2e_m6.py
```

### Rust 生产副端

```bash
cd ../rust/agent
rustc -O src/main.rs -o target/agent     # 离线编译（勿用 cargo build）
YUDUAN_HUB=ws://127.0.0.1:8080/ws/agent ./target/agent
```

## 架构（M2–M6 切片）

- 副端 Agent 通过 `ws(s)://hub/ws/agent` 注册、心跳、接收指令、回传结果。
- 主端（Web 控制台，后续 Tauri/React 包装）经 `REST /api/v1/*` 管理；先登录取 Bearer 令牌，接口按 RBAC 角色门禁（admin/operator/viewer）。
- 远控 viewer 经 `ws://hub/ws/viewer` 接入，画面/输入由中枢居中中继（局域网不出公网）。
- 指令（shell / collect / sw_install）由中枢经长连接同步下发，**关键指令带 HMAC-SHA256 签名 + nonce**，Agent 验签后执行，结果经同一连接回传，中枢以 taskID 关联返回。
- 存储：有 `YUDUAN_DB_DSN` 走 PostgreSQL（设备/账号/审计持久化，重启不丢），否则内存。
- 目录同步：AD/LDAP 用户按「组→角色」映射 upsert 为 RBAC 账号。
- 软件仓库：上传落盘 `data/repo/`，Agent 经 HTTP 拉取后按平台静默安装。

## 已知边界（非生产等价）

- **安全**：TLS 已支持（自签证书，:8443）；HMAC 指令签名/验签已实现（`YUDUAN_HMAC_ENFORCE=1` 强制）。生产需内网 CA 证书、密钥入 KMS、argon2id 口令、参数化查询（见 `HA.md` 合规清单）。
- **存储**：零依赖 PG 客户端为单连接简单查询 + 内联转义（演示）；生产换 pgx/lib/pq + 连接池 + 参数化。
- **副端语言**：Go 参考 Agent（`cmd/agent`）与 Rust 生产副端（`rust/agent`）协议等价、已互通；Rust 版的 HMAC 验签 / sw_install / 远控 / 系统服务化 / wss 为 P2 待办。
- **平台实测**：macOS 已实测（注册/采集/远控/分发/安全/Rust 互通）；本地 PostgreSQL 15.19 实测持久化。Linux / Windows 分支已写，需在对应平台验证。
- **远控画质**：MVP 用 JPEG base64 中枢中继；生产演进为 P2P 直连 + H.264/H.265 二进制帧（protocol.md §4/§5/§8）。

详见根目录《终端统一管理平台-技术开发文档.md》《开发计划与排期.md》与 `dev/HA.md`。
