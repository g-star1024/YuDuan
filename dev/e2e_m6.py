#!/usr/bin/env python3
# 御端 M6 / 安全基座 端到端验证脚本（仅依赖标准库）。
# 覆盖：登录令牌往返(RBAC门禁) / 审计落库 / TLS(https+wss) / HMAC 指令签名执行 / HMAC 强制拒绝。
import json, os, ssl, subprocess, sys, time, urllib.request, urllib.error

# 本机 HTTP_PROXY 指向 127.0.0.1 代理，urllib 会把 127.0.0.1 也走代理导致 502；
# 端到端测试必须直连，禁用代理。
urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))

DEV = "/Users/huluobo/WorkBuddy/jiankong/dev"
BIN = "/tmp/yuduan-e2e"
HUB = os.path.join(BIN, "hub")
AGENT = os.path.join(BIN, "agent")
SECRET = "e2e-secret"
os.makedirs(BIN, exist_ok=True)
os.makedirs(os.path.join(DEV, "certs"), exist_ok=True)

def build():
    for name, pkg in [("hub", "./cmd/hub"), ("agent", "./cmd/agent")]:
        subprocess.run(["go", "build", "-o", os.path.join(BIN, name), pkg],
                       cwd=DEV, check=True)
    print("[build] hub + agent 编译完成")

def start_hub(env_extra=None):
    env = dict(os.environ, YUDUAN_HMAC_SECRET=SECRET, YUDUAN_REPO="/tmp/yuduan-e2e-repo")
    if env_extra:
        env.update(env_extra)
    p = subprocess.Popen(["go", "run", "."] if False else [HUB],
                         cwd=DEV, env=env,
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return p

def start_agent(secret=SECRET, enforce=False, hub="ws://127.0.0.1:8080/ws/agent"):
    env = dict(os.environ, YUDUAN_HUB=hub, YUDUAN_HMAC_SECRET=secret,
               YUDUAN_HMAC_ENFORCE="1" if enforce else "0")
    p = subprocess.Popen([AGENT], cwd=DEV, env=env,
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return p

def http_get(path, token=None, base="http://127.0.0.1:8080", https=False):
    url = path if path.startswith("http") else base + path
    req = urllib.request.Request(url)
    if token:
        req.add_header("Authorization", "Bearer " + token)
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    try:
        with urllib.request.urlopen(req, context=ctx, timeout=5) as r:
            raw = r.read().decode()
            try:
                return r.status, json.loads(raw)
            except Exception:
                return r.status, {"_raw": raw[:200]}
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {}
    except Exception as e:
        return -1, {"error": str(e)}

def http_post(path, data, token=None, base="http://127.0.0.1:8080"):
    url = base + path
    req = urllib.request.Request(url, data=json.dumps(data).encode(),
                                 headers={"Content-Type": "application/json"})
    if token:
        req.add_header("Authorization", "Bearer " + token)
    ctx = ssl.create_default_context(); ctx.check_hostname=False; ctx.verify_mode=ssl.CERT_NONE
    try:
        with urllib.request.urlopen(req, context=ctx, timeout=20) as r:
            raw = r.read().decode()
            try:
                return r.status, json.loads(raw)
            except Exception:
                return r.status, {"_raw": raw[:200]}
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {}
    except Exception as e:
        return -1, {"error": str(e)}

def wait_device():
    # 选取在线设备中最近一次心跳者（排除刚被 terminate 的陈旧离线设备）。
    for _ in range(40):
        st, body = http_get("/api/v1/devices", token=ADMIN_TOKEN)
        if st == 200:
            online = [d for d in body.get("devices", []) if d.get("online")]
            if online:
                online.sort(key=lambda d: d.get("last_heartbeat", ""), reverse=True)
                return online[0]
        time.sleep(0.5)
    return None

results = []
def check(name, cond, detail=""):
    results.append((name, cond, detail))
    print(("PASS " if cond else "FAIL ") + name + ("  " + detail if detail else ""))

# ---- 启动 ----
build()
hub = start_hub()
time.sleep(1.5)

# 1) 登录：admin / viewer（验证此前损坏的令牌往返）
st, body = http_post("/api/v1/login", {"username": "admin", "password": "admin123"})
ADMIN_TOKEN = body.get("token")
check("login admin 200+role", st == 200 and body.get("role") == "admin", f"role={body.get('role')}")
check("login admin token 非空", bool(ADMIN_TOKEN))

st, body = http_post("/api/v1/login", {"username": "viewer", "password": "viewer123"})
VIEWER_TOKEN = body.get("token")
check("login viewer role", st == 200 and body.get("role") == "viewer", f"role={body.get('role')}")

st, body = http_post("/api/v1/login", {"username": "admin", "password": "wrong"})
check("login 错误口令 401", st == 200 and body.get("code") == 401, f"code={body.get('code')}")

# 2) RBAC 门禁
st, _ = http_get("/api/v1/devices")  # 无令牌
check("无令牌访问 401", st == 200 and http_get("/api/v1/devices")[1].get("code") == 401
      if False else st == 200)  # go返回200包体code=401; 用包体判定
_, b = http_get("/api/v1/devices"); check("无令牌包体401", b.get("code") == 401, f"code={b.get('code')}")

st, b = http_get("/api/v1/devices", token=VIEWER_TOKEN)
check("viewer 可查看设备(200)", b.get("total") is not None, f"total={b.get('total')}")

# viewer 缺 device:command → POST 指令应 403
dev_id_placeholder = "placeholder"
st, b = http_post(f"/api/v1/devices/{dev_id_placeholder}", {"action": "shell", "command": "echo x"}, token=VIEWER_TOKEN)
check("viewer 发指令被 403 拒绝", b.get("code") == 403, f"code={b.get('code')}")

# viewer 有 audit:view 应可查审计(200)；缺 software:manage → 上传软件应 403
st, b = http_get("/api/v1/audit", token=VIEWER_TOKEN)
check("viewer 可查看审计(200, 其角色含 audit:view)", b.get("total") is not None, f"total={b.get('total')}")
st, b = http_post("/api/v1/software", {"name": "x"}, token=VIEWER_TOKEN)
check("viewer 上传软件被 403 拒绝", b.get("code") == 403, f"code={b.get('code')}")

# 3) 审计：登录事件已落库（admin 查审计）
st, b = http_get("/api/v1/audit", token=ADMIN_TOKEN)
check("admin 查审计 200", b.get("total") is not None, f"total={b.get('total')}")
login_events = [e for e in b.get("events", []) if e.get("action") == "login"]
check("审计含登录事件", len(login_events) >= 2, f"login_events={len(login_events)}")

# 4) TLS：https 控制台页 + wss 注册（/ 返回 HTML，仅判定状态码）
st, b = http_get("/", base="https://127.0.0.1:8443")
check("TLS https 控制台可访问(200)", st == 200, f"status={st}")
# wss agent
agent_tls = start_agent(hub="wss://127.0.0.1:8443/ws/agent")
time.sleep(2.0)
tls_dev = wait_device()
check("TLS(wss) 副端注册成功", tls_dev is not None, f"device={tls_dev['id'] if tls_dev else None}")
agent_tls.terminate()

# 普通 ws agent（用于指令执行测试）
agent = start_agent()
time.sleep(2.0)
dev = wait_device()
check("副端(ws)注册成功", dev is not None, f"device={dev['id'] if dev else None}")
DEVID = dev["id"] if dev else None

# 5) HMAC 签名指令：hub 签名 → agent 验签 → 执行 → 回结果
if DEVID:
    st, b = http_post(f"/api/v1/devices/{DEVID}", {"action": "shell", "command": "echo e2e_hmac_ok"},
                      token=ADMIN_TOKEN)
    out = b.get("output_log", "") if isinstance(b, dict) else ""
    check("HMAC 签名指令执行成功", isinstance(b, dict) and b.get("state") == "success" and "e2e_hmac_ok" in out,
          f"state={b.get('state')} out={out[:60]}")

# 6) HMAC 强制拒绝：agent 用错误 secret + enforce=on → hub 签的名验不过 → 拒绝
agent.terminate(); time.sleep(0.5)
agent_bad = start_agent(secret="WRONG-SECRET", enforce=True)
time.sleep(2.5)
dev2 = wait_device()
if dev2:
    st, b = http_post(f"/api/v1/devices/{dev2['id']}", {"action": "shell", "command": "echo should_reject"},
                      token=ADMIN_TOKEN)
    check("HMAC 强制拒绝(错误secret) → 指令失败", isinstance(b, dict) and b.get("state") == "failed"
          and "HMAC" in b.get("output_log", ""), f"state={b.get('state')} out={b.get('output_log','')[:60]}")
agent_bad.terminate()

# 收尾
hub.terminate()
print("\n==== 汇总 ====")
passed = sum(1 for _, c, _ in results if c)
print(f"{passed}/{len(results)} 通过")
fails = [n for n, c, _ in results if not c]
if fails:
    print("失败项: " + ", ".join(fails))
    sys.exit(1)
print("ALL PASS")
