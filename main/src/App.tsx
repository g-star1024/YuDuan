import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  hubChangePassword,
  hubDeviceCmd,
  hubDevices,
  hubLogin,
  rcInput,
  rcQos,
  rcStart,
  rcStop,
  type Device,
} from "./api";

// 品牌渐变（取自 logo-yuduan.svg：#1E3A8A → #0EA5E9）
function LogoMark({ size = 36 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 120 120" aria-hidden>
      <defs>
        <linearGradient id="lbg" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#1E3A8A" />
          <stop offset="1" stopColor="#0EA5E9" />
        </linearGradient>
        <linearGradient id="lsp" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#38BDF8" />
          <stop offset="1" stopColor="#22D3EE" />
        </linearGradient>
      </defs>
      <rect x="5" y="5" width="110" height="110" rx="26" fill="url(#lbg)" />
      <g stroke="url(#lsp)" strokeWidth="4" strokeLinecap="round">
        <line x1="60" y1="60" x2="34" y2="33" />
        <line x1="60" y1="60" x2="86" y2="33" />
        <line x1="60" y1="60" x2="34" y2="87" />
        <line x1="60" y1="60" x2="86" y2="87" />
      </g>
      <g fill="#fff">
        <circle cx="34" cy="33" r="6.5" />
        <circle cx="86" cy="33" r="6.5" />
        <circle cx="34" cy="87" r="6.5" />
        <circle cx="86" cy="87" r="6.5" />
      </g>
      <circle cx="60" cy="60" r="13.5" fill="#fff" />
      <circle cx="60" cy="60" r="6" fill="url(#lbg)" />
    </svg>
  );
}

type Page = "devices" | "viewer";

interface CmdLogItem {
  time: string;
  device: string;
  action: string;
  ok: boolean;
  detail: string;
}

export default function App() {
  // 登录态（T-4 迁 keychain；会话期内存暂存）
  const [base, setBase] = useState("http://127.0.0.1:8081");
  const [username, setUsername] = useState("admin");
  const [password, setPassword] = useState("");
  const [token, setToken] = useState("");
  const [role, setRole] = useState("");
  const [mustChange, setMustChange] = useState(false);
  const [loginErr, setLoginErr] = useState("");
  const [loggingIn, setLoggingIn] = useState(false);

  // 改密表单
  const [oldPw, setOldPw] = useState("");
  const [newPw, setNewPw] = useState("");
  const [newPw2, setNewPw2] = useState("");
  const [pwMsg, setPwMsg] = useState("");

  // 设备页
  const [page, setPage] = useState<Page>("devices");
  const [devices, setDevices] = useState<Device[]>([]);
  const [devErr, setDevErr] = useState("");
  const [selected, setSelected] = useState<Device | null>(null);
  const [action, setAction] = useState("echo");
  const [command, setCommand] = useState("ping");
  const [cmdLog, setCmdLog] = useState<CmdLogItem[]>([]);

  // —— 远控 Viewer（T-2 relay）——
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const frameRef = useRef<{ w: number; h: number }>({ w: 0, h: 0 });
  const [rcStatus, setRcStatus] = useState("");
  const [rcStats, setRcStats] = useState("");
  const [rcConsent, setRcConsent] = useState("");
  const [fpsTarget, setFpsTarget] = useState(15);

  useEffect(() => {
    if (page !== "viewer") return;
    const unsubs: Array<() => void> = [];
    listen<{
      codec: number;
      seq: number;
      width: number;
      height: number;
      data: string;
    }>("rc-media", (e) => {
      const f = e.payload;
      const canvas = canvasRef.current;
      if (!canvas) return;
      if (frameRef.current.w !== f.width || frameRef.current.h !== f.height) {
        canvas.width = f.width;
        canvas.height = f.height;
        frameRef.current = { w: f.width, h: f.height };
      }
      if (f.codec === 1) {
        // JPEG：WebView 原生解码直绘
        const img = new Image();
        img.onload = () => canvas.getContext("2d")?.drawImage(img, 0, 0);
        img.src = `data:image/jpeg;base64,${f.data}`;
      } else if (f.codec === 0) {
        // RAW_RGBA：putImageData
        const raw = Uint8ClampedArray.from(atob(f.data), (c) => c.charCodeAt(0));
        const id = new ImageData(raw, f.width, f.height);
        canvas.getContext("2d")?.putImageData(id, 0, 0);
      } else {
        setRcStats(`（codec=${f.codec} 待 T-3 原生解码，帧序号 ${f.seq}）`);
      }
    }).then((u) => unsubs.push(u));
    listen<{ phase: string; detail: string }>("rc-status", (e) => {
      setRcStatus(`${e.payload.phase}: ${e.payload.detail}`);
    }).then((u) => unsubs.push(u));
    listen<{ kind: string; message: string }>("rc-event", (e) => {
      if (e.payload.kind === "consent_required") setRcConsent(e.payload.message || "被控端需确认");
      if (e.payload.kind === "consent_granted") setRcConsent("");
    }).then((u) => unsubs.push(u));
    listen<{ fps_recv: number; loss_pct: number; kbps_recv: number; rtt_ms?: number; transport: string }>("rc-qos", (e) => {
      const p = e.payload;
      setRcStats(
        `${p.transport} · fps ${p.fps_recv} · 丢包 ${p.loss_pct}% · ${(p.kbps_recv / 1024).toFixed(1)} MB/s` +
          (p.rtt_ms ? ` · RTT ${p.rtt_ms}ms` : "")
      );
      // 周期把统计回传给副端（§8.3.15 码率闭环数据源）
      rcQos(fpsTarget, Number(p.loss_pct) || 0, Number(p.kbps_recv) || 0, Number(p.rtt_ms) || 0);
    }).then((u) => unsubs.push(u));
    return () => {
      unsubs.forEach((u) => u());
      rcStop();
    };
  }, [page, fpsTarget]);

  async function openViewer(d: Device) {
    setPage("viewer");
    setSelected(d);
    setRcStatus("发起中…");
    await rcStart(base, d.id, username);
  }

  function viewerInput(kind: string, e: React.MouseEvent | React.KeyboardEvent | React.WheelEvent) {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const { w, h } = frameRef.current;
    if (!w || !h) return;
    const r = canvas.getBoundingClientRect();
    if ("clientX" in e && kind.startsWith("mouse")) {
      const x = Math.round(((e.clientX - r.left) / r.width) * w);
      const y = Math.round(((e.clientY - r.top) / r.height) * h);
      const btn = (e as React.MouseEvent).button;
      rcInput(kind, x, y, btn, "");
    } else if ("key" in e) {
      rcInput(kind, 0, 0, 0, (e as React.KeyboardEvent).key);
    }
  }

  const refreshDevices = useCallback(
    async (t: string) => {
      try {
        const list = await hubDevices(base, t);
        setDevices(list);
        setDevErr("");
      } catch (e) {
        setDevErr(String(e));
      }
    },
    [base]
  );

  useEffect(() => {
    if (!token) return;
    refreshDevices(token);
    const iv = setInterval(() => refreshDevices(token), 5000);
    return () => clearInterval(iv);
  }, [token, refreshDevices]);

  async function doLogin() {
    setLoggingIn(true);
    setLoginErr("");
    try {
      const r = await hubLogin(base, username, password);
      setToken(r.token);
      setRole(r.role);
      setMustChange(r.must_change_password);
      setPassword("");
    } catch (e) {
      setLoginErr(String(e));
    } finally {
      setLoggingIn(false);
    }
  }

  async function doChangePassword() {
    setPwMsg("");
    if (newPw.length < 8) {
      setPwMsg("新密码至少 8 位");
      return;
    }
    if (newPw !== newPw2) {
      setPwMsg("两次输入的新密码不一致");
      return;
    }
    try {
      const r = await hubChangePassword(base, token, oldPw, newPw);
      setToken(r.token);
      setMustChange(false);
      setPwMsg("改密成功，已续签令牌");
      setOldPw("");
      setNewPw("");
      setNewPw2("");
    } catch (e) {
      setPwMsg(String(e));
    }
  }

  function logout() {
    setToken("");
    setRole("");
    setMustChange(false);
    setDevices([]);
    setSelected(null);
    setCmdLog([]);
  }

  async function sendCmd() {
    if (!selected) return;
    try {
      const r = await hubDeviceCmd(base, token, selected.id, action, command);
      setCmdLog((l) => [
        {
          time: new Date().toLocaleTimeString(),
          device: selected.hostname,
          action,
          ok: r["code"] === 0,
          detail: JSON.stringify(r),
        },
        ...l,
      ]);
    } catch (e) {
      setCmdLog((l) => [
        {
          time: new Date().toLocaleTimeString(),
          device: selected.hostname,
          action,
          ok: false,
          detail: String(e),
        },
        ...l,
      ]);
    }
  }

  // —— 登录页 ——
  if (!token) {
    return (
      <div className="center-wrap">
        <div className="card login-card">
          <div className="brand">
            <LogoMark size={44} />
            <div>
              <h1>御端</h1>
              <p className="sub">企业终端统一管理平台 · 主端</p>
            </div>
          </div>
          <label>
            中枢地址
            <input value={base} onChange={(e) => setBase(e.target.value)} placeholder="http://hub:8081" />
          </label>
          <label>
            用户名
            <input value={username} onChange={(e) => setUsername(e.target.value)} autoComplete="username" />
          </label>
          <label>
            密码
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && doLogin()}
              autoComplete="current-password"
            />
          </label>
          {loginErr && <p className="err">{loginErr}</p>}
          <button className="primary" onClick={doLogin} disabled={loggingIn || !password}>
            {loggingIn ? "登录中…" : "登录"}
          </button>
        </div>
      </div>
    );
  }

  // —— 主界面 ——
  return (
    <div className="app">
      <header>
        <div className="brand-row">
          <LogoMark size={28} />
          <span className="title">御端 · 终端统一管理</span>
          <span className="tag">{role}</span>
        </div>
        <div className="conn">
          <span className="dot ok" /> {base}
          <button className="ghost" onClick={logout}>
            退出
          </button>
        </div>
      </header>

      {mustChange && (
        <div className="card warn-card">
          <h3>首次登录，请修改默认密码</h3>
          <div className="pw-row">
            <input type="password" placeholder="当前密码" value={oldPw} onChange={(e) => setOldPw(e.target.value)} />
            <input type="password" placeholder="新密码（≥8 位）" value={newPw} onChange={(e) => setNewPw(e.target.value)} />
            <input type="password" placeholder="确认新密码" value={newPw2} onChange={(e) => setNewPw2(e.target.value)} />
            <button className="primary" onClick={doChangePassword}>
              修改
            </button>
          </div>
          {pwMsg && <p className={pwMsg.includes("成功") ? "ok-text" : "err"}>{pwMsg}</p>}
        </div>
      )}

      {page === "viewer" && (
        <main className="viewer-wrap">
          <div className="viewer-bar">
            <button className="ghost" onClick={() => setPage("devices")}>
              ← 返回设备列表
            </button>
            <span className="muted">{selected?.hostname}</span>
            <span className="muted">{rcStatus}</span>
            <span className="muted">{rcStats}</span>
            {rcConsent && <span className="consent">⏳ {rcConsent}</span>}
            <label className="fps-label">
              目标帧率
              <input
                type="range"
                min={4}
                max={30}
                value={fpsTarget}
                onChange={(e) => setFpsTarget(Number(e.target.value))}
              />
              {fpsTarget}
            </label>
          </div>
          <div className="viewer-stage">
            <canvas
              ref={canvasRef}
              tabIndex={0}
              onMouseMove={(e) => viewerInput("mouse_move", e)}
              onMouseDown={(e) => viewerInput("mouse_down", e)}
              onMouseUp={(e) => viewerInput("mouse_up", e)}
              onKeyDown={(e) => viewerInput("key_down", e)}
              onKeyUp={(e) => viewerInput("key_up", e)}
              onWheel={(e) => viewerInput("mouse_wheel", e)}
            />
          </div>
        </main>
      )}

      {page === "devices" && (
        <main className="content">
          <section className="card table-card">
            <div className="card-head">
              <h2>设备台账</h2>
              <span className="muted">
                共 {devices.length} 台 · 在线 {devices.filter((d) => d.online).length}
              </span>
            </div>
            {devErr && <p className="err">{devErr}</p>}
            <table>
              <thead>
                <tr>
                  <th>状态</th>
                  <th>主机名</th>
                  <th>系统</th>
                  <th>架构</th>
                  <th>CPU / 内存 / 磁盘</th>
                  <th>IP</th>
                  <th>副端版本</th>
                  <th>最近心跳</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {devices.map((d) => (
                  <tr
                    key={d.id}
                    className={selected?.id === d.id ? "sel" : ""}
                    onClick={() => setSelected(d)}
                  >
                    <td>
                      <span className={"dot " + (d.online ? "ok" : "off")} />
                    </td>
                    <td>{d.hostname}</td>
                    <td>
                      {d.os} {d.os_version}
                    </td>
                    <td>{d.arch}</td>
                    <td className="muted">
                      {d.cpu?.slice(0, 18)} · {d.memory_gb}G · {d.disk_gb}G
                    </td>
                    <td>{d.ip}</td>
                    <td>{d.agent_version}</td>
                    <td className="muted">{d.last_heartbeat?.replace("T", " ").slice(0, 19)}</td>
                    <td>
                      <button className="ghost small" onClick={(e) => { e.stopPropagation(); openViewer(d); }}>
                        远控
                      </button>
                    </td>
                  </tr>
                ))}
                {devices.length === 0 && !devErr && (
                  <tr>
                    <td colSpan={9} className="muted center">
                      暂无设备（启动副端后会自动注册）
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </section>

          <section className="card cmd-card">
            <h2>指令下发 {selected ? `→ ${selected.hostname}` : ""}</h2>
            {!selected ? (
              <p className="muted">在上方选择一台设备</p>
            ) : (
              <>
                <div className="cmd-row">
                  <select value={action} onChange={(e) => setAction(e.target.value)}>
                    <option value="shell">shell（执行命令）</option>
                    <option value="collect">collect（立即采集）</option>
                  </select>
                  {action === "shell" && (
                    <input
                      value={command}
                      onChange={(e) => setCommand(e.target.value)}
                      placeholder="要执行的命令"
                    />
                  )}
                  <button className="primary" onClick={sendCmd}>
                    发送
                  </button>
                </div>
                <div className="cmd-log">
                  {cmdLog.map((l, i) => (
                    <div key={i} className={"log-item " + (l.ok ? "" : "fail")}>
                      <span className="muted">{l.time}</span> [{l.device}] {l.action}{" "}
                      {l.ok ? "✓" : "✗"} <code>{l.detail.slice(0, 160)}</code>
                    </div>
                  ))}
                </div>
              </>
            )}
          </section>
        </main>
      )}
    </div>
  );
}
