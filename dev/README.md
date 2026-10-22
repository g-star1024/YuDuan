# 御端 · 代码仓库（御端终端统一管理平台）

> 仓库脚手架（M1 设计冻结阶段建立）。对应《技术开发文档》v1.0 第 13 章目录结构。
> 开发治理见项目 skill `yuduan-dev` 与《御端-项目开发铁律.md》。

## 目录结构

```
dev/
  hub/            # 服务端中枢（Go）：API Gateway / WS 网关 / 注册 / 任务编排 / 仓库 / 信令中继 / RBAC / 审计
  console/        # 主端客户端（Tauri + React/TS）：控制台 + 远控 viewer
  agent/          # 副端 Agent（Rust）：platform/{win,mac,linux} 三平台实现
  proto/          # 协议定义：protocol.md（消息规范）+ schema.sql（数据模型）
  deploy/         # 部署清单：compose / k8s / ansible（Agent 批量推送）
```

## 技术栈（M1 基线）

- 中枢：Go + PostgreSQL + MinIO/NATS
- 主端：Tauri（Rust）+ React + TypeScript；远控 viewer 用 Rust 原生
- 副端：Rust 常驻服务；Win 服务 / macOS launchd / Linux systemd
- 通信：WebSocket + TLS（控制/心跳/信令）；UDP/RTP 或 TCP（远控媒体）

## 当前里程碑

- 阶段：**M1 设计冻结**（协议 / Schema / 跨平台接口契约 / 脚手架）
- 状态交接文件：`.workbuddy/memory/dev-status.md`

## 关键约束（铁律摘录）

- 本期只做局域网，禁止公网穿透依赖。
- 主端/副端三平台同构，未覆盖三平台不得宣告完成。
- 全链路 TLS；关键指令 HMAC 签名 + seq 防重放；Agent 防卸载/防篡改。
- 远程屏幕/录像默认关闭，远端必弹提示，留存 ≤90 天自动清理。
