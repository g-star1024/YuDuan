# 御端 · 研发目录（dev）

> M1 设计冻结产物 + M2/M3 可运行实现。本文是研发入口。

## 里程碑状态

| 里程碑 | 状态 | 说明 |
|---|---|---|
| M1 设计冻结 | ✅ | 协议 `proto/protocol.md`、Schema `proto/schema.sql`、接口契约 `agent/INTERFACES.md` |
| M2 底座联调 | ✅ 可运行 | 注册/心跳/指令端到端打通（macOS 实测） |
| M3 资产管理 | ✅ 基础版 | 跨平台采集 + 台账 + 报表（macOS 实测；Linux/Win 待实跑） |
| M4 远程控制 | ⏳ 待启动 | viewer / 信令中继 / 编码 / 录制 / 权限 |
| M5 软件分发 | ⏳ 待启动 | 仓库 + 静默安装 + 状态反馈 |
| M6 管理与审计 | ⏳ 待启动 | RBAC / AD 同步 / 高可用 / 合规上线 |

## 目录结构

```
dev/
├── go.mod                  # Go module: yuduan（零外部依赖）
├── internal/
│   ├── proto/              # 控制面消息信封与载荷（对齐 proto/protocol.md）
│   ├── ws/                 # 最小化 WebSocket（标准库实现，无第三方依赖）
│   ├── store/              # 设备仓储接口 + 内存实现（后续换 PostgreSQL）
│   ├── collect/            # 跨平台资产采集（M3）
│   └── hub/                # 中枢：Agent 长连接 / REST API / 指令下发 / 报表
├── cmd/
│   ├── hub/                # 中枢入口
│   └── agent/              # 副端参考 Agent 入口（Go，生产将用 Rust 重写）
├── console/index.html      # 管理控制台网页（台账 + 报表 + 指令下发）
├── proto/                  # M1 冻结：通信协议 + 数据库 Schema
├── agent/INTERFACES.md     # M1 冻结：副端跨平台接口契约
├── hub/README.md           # 中枢说明
├── agent/README.md         # 副端说明（含 Rust 平移说明）
├── console/README.md       # 控制台说明
└── deploy/README.md        # 部署运维说明
```

## 本地构建与验证

```bash
cd dev
go build -o bin/hub ./cmd/hub
go build -o bin/agent ./cmd/agent

# 终端 1：启动中枢
./bin/hub
# 终端 2、3…：启动多个副端（模拟多台员工电脑）
./bin/agent
# 管理控制台：浏览器打开 http://127.0.0.1:8080/
# 或 REST 查询：
curl http://127.0.0.1:8080/api/v1/devices
curl http://127.0.0.1:8080/api/v1/report
```

## 架构（M2/M3 切片）

- 副端 Agent 通过 `ws://hub/ws/agent` 注册、心跳、接收指令、回传结果。
- 主端（当前以 Web 控制台代指，后续用 Tauri/React 包装）通过 `REST /api/v1/*` 管理。
- 指令（shell / collect）由中枢经长连接同步下发，Agent 执行后结果经同一连接回传，中枢以 taskID 关联并返回给调用方。

## 已知边界（非生产等价）

- **安全**：当前为明文 ws/http（仅本机联调），已做令牌鉴权；生产需全链路 TLS + 指令 HMAC 签名（铁律 4）。
- **存储**：内存实现，重启即丢；生产接 `proto/schema.sql` 的 PostgreSQL。
- **副端语言**：M2/M3 用 Go 参考 Agent 打通协议；生产副端按铁律以 Rust 重写（逻辑等价）。
- **平台实测**：macOS 已实测；Linux / Windows 采集分支已写，需在对应平台验证。

详见根目录《终端统一管理平台-技术开发文档.md》与《开发计划与排期.md》。
