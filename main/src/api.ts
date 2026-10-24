// 主端 ↔ hub 控制面 API 封装（经 Tauri invoke 调用 Rust core）
import { invoke } from "@tauri-apps/api/core";

export interface LoginResult {
  token: string;
  role: string;
  must_change_password: boolean;
}

export interface Device {
  id: string;
  hostname: string;
  os: string;
  os_version: string;
  arch: string;
  cpu: string;
  memory_gb: number;
  disk_gb: number;
  serial_no: string;
  mac: string;
  ip: string;
  agent_version: string;
  online: boolean;
  last_heartbeat: string;
}

export function hubLogin(base: string, username: string, password: string): Promise<LoginResult> {
  return invoke("hub_login", { base, username, password });
}

export function hubChangePassword(
  base: string,
  token: string,
  oldPassword: string,
  newPassword: string
): Promise<LoginResult> {
  return invoke("hub_change_password", {
    base,
    token,
    oldPassword,
    newPassword,
  });
}

export function hubDevices(base: string, token: string): Promise<Device[]> {
  return invoke("hub_devices", { base, token });
}

export function hubDeviceCmd(
  base: string,
  token: string,
  deviceId: string,
  action: string,
  command: string
): Promise<Record<string, unknown>> {
  return invoke("hub_device_cmd", { base, token, deviceId, action, command });
}

// —— 远控会话（T-2 relay） ——

export function rcStart(base: string, deviceId: string, operator: string): Promise<string> {
  return invoke("rc_start", { base, deviceId, operator });
}

export function rcStop(): Promise<void> {
  return invoke("rc_stop");
}

export function rcInput(kind: string, x: number, y: number, button: number, key: string): Promise<void> {
  return invoke("rc_input", { kind, x, y, button, key });
}

export function rcQos(fps: number, lossPct: number, kbps: number, rttMs: number): Promise<void> {
  return invoke("rc_qos", { fps, lossPct, kbps, rttMs });
}
