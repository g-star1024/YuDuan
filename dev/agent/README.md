# 副端（Agent）说明

副端运行在每台员工电脑上，M2/M3 已实现参考实现并通过 macOS 实测。

## 当前实现（Go 参考 Agent）
- 文件：`cmd/agent/main.go` + `internal/collect`
- 行为：
  - 启动后通过 `ws://hub/ws/agent` 注册（携带资产指纹），获取 `device_id + token`；
  - 每 30s 心跳；长连接断开自动 5s 重连；
  - 接收 `command`，支持 `shell`（远端执行命令）与 `collect`（重新采集资产），结果回传。
  - 资产采集覆盖：hostname / os / os_version / arch / cpu / 内存 / 磁盘 / 序列号 / MAC / IP / 负载（见 `INTERFACES.md` 契约）。

## 平台差异收敛
所有平台返回同一 `collect.Asset` 结构，平台分支内聚在 `collect.go`，便于 Rust 重写时一一对应。
- macOS：已实测。
- Linux：`/etc/os-release`、`/proc/cpuinfo`、`/proc/meminfo`、`df -k`、`/sys/class/dmi/id/product_serial`。
- Windows：`wmic`（cpu/os/memory/logicaldisk/bios）。

## 重要决策（待用户确认）
铁律要求生产副端用 **Rust**；M2/M3 用 Go 参考 Agent 快速打通协议，逻辑与 Rust 版等价。
后续以 Rust 重写时，注册/心跳/指令/采集的字段与流程保持一致即可平移。

## 后续（M4/M5）
- 系统级常驻（Windows 服务 / macOS launchd / Linux systemd）
- 防卸载 / 防篡改（铁律）
- 远控采集与输入注入（M4）
- 软件静默安装（M5）
