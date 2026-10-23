// Package hub 实现御端服务端中枢。
// 职责：Agent 长连接纳管（注册/心跳/指令结果）、管理 REST API（RBAC 门禁 + 审计）、
// 远程控制信令与画面中继（M4）、软件仓库与分发（M5）、资产台账与报表。
// M6 新增：登录鉴权 / RBAC 接口门禁 / 审计落库 / AD-LDAP 目录同步。
// 安全基座：关键指令 HMAC 签名（副端验签）；TLS 由入口层（cmd/hub）承载。
package hub

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"yuduan/internal/audit"
	"yuduan/internal/auth"
	"yuduan/internal/crypto"
	"yuduan/internal/directory"
	"yuduan/internal/proto"
	"yuduan/internal/store"
	"yuduan/internal/ws"
)

// RCSession 记录一个进行中的远控会话（Hub 居中中继，局域网不出公网）。
type RCSession struct {
	SessionID string
	DeviceID  string
	Operator  string
	Viewer    *ws.Conn
	StartedAt time.Time
	Consent   bool
}

// HubConfig 是中枢的可选配置；缺省项自动回退到内存实现。
type HubConfig struct {
	RepoDir     string
	Store       store.Store
	Auditor     audit.Auditor
	Users       auth.UserStore
	Auth        *auth.AuthService
	HMACSecret  string
	HMACEnforce bool
	DirSync     *directory.SyncEngine
}

// Hub 是中枢核心结构。
type Hub struct {
	store    store.Store
	auditor  audit.Auditor
	users    auth.UserStore
	authSvc  *auth.AuthService
	secret   string
	enforce  bool
	dirSync  *directory.SyncEngine

	mu       sync.Mutex
	conns    map[string]*ws.Conn   // deviceID -> 控制面长连接
	pending  map[string]chan proto.ResultPayload
	sessions map[string]*RCSession // deviceID -> 远控会话
	software map[string]*proto.SoftwareItem
	repoDir  string
}

// NewHub 构造中枢，缺省回退到内存实现。
func NewHub(cfg HubConfig) *Hub {
	if cfg.RepoDir == "" {
		cfg.RepoDir = "data/repo"
	}
	if cfg.Store == nil {
		cfg.Store = store.NewMemStore()
	}
	if cfg.Auditor == nil {
		cfg.Auditor = audit.NewMemAuditor()
	}
	if cfg.Users == nil {
		cfg.Users = store.NewMemUserStore()
	}
	if cfg.Auth == nil {
		cfg.Auth = auth.NewAuthService(cfg.Users, cfg.HMACSecret, "")
	}
	_ = os.MkdirAll(cfg.RepoDir, 0o755)
	return &Hub{
		store:    cfg.Store,
		auditor:  cfg.Auditor,
		users:    cfg.Users,
		authSvc:  cfg.Auth,
		secret:   cfg.HMACSecret,
		enforce:  cfg.HMACEnforce,
		dirSync:  cfg.DirSync,
		conns:    make(map[string]*ws.Conn),
		pending:  make(map[string]chan proto.ResultPayload),
		sessions: make(map[string]*RCSession),
		software: make(map[string]*proto.SoftwareItem),
		repoDir:  cfg.RepoDir,
	}
}

// Handler 返回 HTTP 路由（REST + WS + 控制台静态页）。
func (h *Hub) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws/agent", h.handleAgentWS)
	mux.HandleFunc("/ws/viewer", h.handleViewerWS)
	// 公开
	mux.HandleFunc("/api/v1/login", h.handleLogin)
	// 受保护：资产
	mux.HandleFunc("/api/v1/devices", h.guard(auth.PermDeviceView, h.handleDevices))
	mux.HandleFunc("/api/v1/report", h.guard(auth.PermDeviceView, h.handleReport))
	mux.HandleFunc("/api/v1/devices/", h.guardDeviceByID)
	// 受保护：软件仓库
	mux.HandleFunc("/api/v1/software", h.guardSoftware)
	mux.HandleFunc("/api/v1/repo/", h.handleRepo) // 副端下载（演示放开；生产应短令牌/鉴权）
	// 受保护：审计 / 目录同步
	mux.HandleFunc("/api/v1/audit", h.guard(auth.PermAuditView, h.handleAudit))
	mux.HandleFunc("/api/v1/dirsync", h.guard(auth.PermDirSync, h.handleDirSync))
	mux.HandleFunc("/", h.handleConsole)
	return mux
}

// guard 包裹受保护接口：校验 Bearer 令牌并判定角色权限。
func (h *Hub) guard(perm string, fn func(http.ResponseWriter, *http.Request, crypto.Claims)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		claims, err := h.authSvc.VerifyToken(auth.TokenFromHeader(r.Header.Get("Authorization")))
		if err != nil {
			writeErr(w, 401, "unauthorized")
			return
		}
		if !h.authSvc.HasPerm(claims.Role, perm) {
			writeErr(w, 403, "forbidden: 需要权限 "+perm)
			return
		}
		fn(w, r, claims)
	}
}

// guardDeviceByID 根据子路径选择所需权限（GET 查看 / POST 指令）。
func (h *Hub) guardDeviceByID(w http.ResponseWriter, r *http.Request) {
	switch {
	case r.Method == http.MethodGet:
		h.guard(auth.PermDeviceView, h.handleDeviceGet).ServeHTTP(w, r)
	case r.Method == http.MethodPost:
		h.guard(auth.PermDeviceCmd, h.handleDeviceCmd).ServeHTTP(w, r)
	default:
		http.NotFound(w, r)
	}
}

func (h *Hub) guardSoftware(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		h.guard(auth.PermDeviceView, h.handleSoftwareList).ServeHTTP(w, r)
	case http.MethodPost:
		h.guard(auth.PermSoftwareMgmt, h.handleSoftwareUpload).ServeHTTP(w, r)
	default:
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
	}
}

// audit 写入一条审计记录。
func (h *Hub) audit(actor, action, target, detail, result string) {
	if h.auditor != nil {
		h.auditor.Record(audit.Event{Actor: actor, Action: action, Target: target, Detail: detail, Result: result})
	}
}

// StartReaper 定期检查心跳超时，将超过 90s 未心跳的设备标记为离线。
func (h *Hub) StartReaper() {
	ticker := time.NewTicker(60 * time.Second)
	for range ticker.C {
		now := time.Now().UTC()
		for _, d := range h.store.List() {
			if d.Online && now.Sub(d.LastSeen) > 90*time.Second {
				h.store.MarkOffline(d.ID)
				log.Printf("[hub] reaper: %s marked offline (last seen %s)", d.Hostname, d.LastSeen.Format(time.RFC3339))
			}
		}
	}
}

// RunDirSync 执行一次目录同步并写审计。
func (h *Hub) RunDirSync(actor string) (interface{}, error) {
	if h.dirSync == nil {
		return nil, nil
	}
	res, err := h.dirSync.Sync()
	if err != nil {
		h.audit(actor, "dir_sync", "", "同步失败: "+err.Error(), "failed")
		return nil, err
	}
	h.audit(actor, "dir_sync", "", "同步用户数="+itoa(res.UsersSynced), "success")
	return res, nil
}

// ---- 登录 ----

func (h *Hub) handleLogin(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var body struct {
		Username string `json:"username"`
		Password string `json:"password"`
	}
	_ = json.NewDecoder(r.Body).Decode(&body)
	token, err := h.authSvc.Login(body.Username, body.Password)
	if err != nil {
		h.audit(body.Username, "login", "", "登录失败", "failed")
		writeErr(w, 401, "invalid credentials")
		return
	}
	claims, _ := h.authSvc.VerifyToken(token)
	h.audit(body.Username, "login", "", "角色="+claims.Role, "success")
	writeJSON(w, map[string]any{"code": 0, "token": token, "role": claims.Role})
}

// ---- Agent 长连接 ----

func (h *Hub) handleAgentWS(w http.ResponseWriter, r *http.Request) {
	conn, err := ws.Upgrade(w, r)
	if err != nil {
		return
	}
	defer conn.Close()

	var deviceID string
	for {
		raw, err := conn.ReadMessage()
		if err != nil {
			break
		}
		var env proto.Envelope
		if err := json.Unmarshal(raw, &env); err != nil {
			continue
		}
		switch env.Type {
		case proto.TypeRegister:
			var p proto.RegisterPayload
			if err := env.DecodePayload(&p); err != nil {
				continue
			}
			dev := store.Device{
				Hostname: p.Hostname, OS: p.OS, OSVersion: p.OSVersion, Arch: p.Arch,
				CPU: p.CPU, MemoryGB: p.MemoryGB, DiskGB: p.DiskGB, SerialNo: p.SerialNo,
				MAC: p.MAC, IP: p.IP, AgentVersion: p.AgentVersion, Token: p.Token,
			}
			saved := h.store.Upsert(dev)
			deviceID = saved.ID
			h.mu.Lock()
			h.conns[deviceID] = conn
			h.mu.Unlock()
			ack, _ := proto.NewEnvelope(proto.TypeRegisterAck, 0, proto.RegisterAckPayload{
				DeviceID: saved.ID, Token: saved.Token, OK: true,
			})
			ackBytes, _ := json.Marshal(ack)
			_ = conn.WriteMessage(ackBytes)
			log.Printf("[hub] agent registered: %s (%s/%s) mem=%dGB disk=%dGB", saved.Hostname, saved.OS, saved.Arch, saved.MemoryGB, saved.DiskGB)

		case proto.TypeHeartbeat:
			if deviceID != "" {
				h.store.Touch(deviceID)
			}

		case proto.TypeResult:
			var p proto.ResultPayload
			if err := env.DecodePayload(&p); err != nil {
				continue
			}
			h.mu.Lock()
			ch, ok := h.pending[p.TaskID]
			if ok {
				delete(h.pending, p.TaskID)
			}
			h.mu.Unlock()
			if ok {
				ch <- p
			}
			log.Printf("[hub] result task=%s state=%s exit=%d", p.TaskID, p.State, p.ExitCode)

		case proto.TypeRCReady, proto.TypeRCFrame, proto.TypeRCEvent:
			h.relayToViewer(deviceID, raw)
		case proto.TypeRCStop:
			h.endSession(deviceID)

		default:
		}
	}

	if deviceID != "" {
		h.store.MarkOffline(deviceID)
		h.endSession(deviceID)
		h.mu.Lock()
		delete(h.conns, deviceID)
		h.mu.Unlock()
		log.Printf("[hub] agent offline: %s", deviceID)
	}
}

func (h *Hub) relayToViewer(deviceID string, raw []byte) {
	h.mu.Lock()
	s, ok := h.sessions[deviceID]
	h.mu.Unlock()
	if !ok || s.Viewer == nil {
		return
	}
	_ = s.Viewer.WriteMessage(raw)
}

func (h *Hub) endSession(deviceID string) {
	h.mu.Lock()
	s, ok := h.sessions[deviceID]
	delete(h.sessions, deviceID)
	h.mu.Unlock()
	if ok && s.Viewer != nil {
		_ = s.Viewer.WriteMessage(mustJSON(proto.Envelope{Type: proto.TypeRCStop}))
	}
}

// ---- Viewer 长连接（M4 远控）----

func (h *Hub) handleViewerWS(w http.ResponseWriter, r *http.Request) {
	conn, err := ws.Upgrade(w, r)
	if err != nil {
		return
	}
	defer conn.Close()

	var deviceID string
	for {
		raw, err := conn.ReadMessage()
		if err != nil {
			break
		}
		var env proto.Envelope
		if err := json.Unmarshal(raw, &env); err != nil {
			continue
		}
		switch env.Type {
		case proto.TypeRCRequest:
			var p proto.RCRequestPayload
			if err := env.DecodePayload(&p); err != nil {
				continue
			}
			h.mu.Lock()
			agentConn, ok := h.conns[p.DeviceID]
			h.mu.Unlock()
			if !ok {
				_ = conn.WriteMessage(mustJSON(proto.Envelope{Type: proto.TypeRCEvent, Payload: mustRaw(proto.RCEventPayload{
					SessionID: p.SessionID, Kind: "error", Message: "设备离线，无法建立远控会话",
				})}))
				continue
			}
			h.mu.Lock()
			h.sessions[p.DeviceID] = &RCSession{
				SessionID: p.SessionID, DeviceID: p.DeviceID, Operator: p.Operator,
				Viewer: conn, StartedAt: time.Now().UTC(),
			}
			h.mu.Unlock()
			deviceID = p.DeviceID
			_ = agentConn.WriteMessage(raw)
			h.audit(p.Operator, "rc_start", p.DeviceID, "远控会话开启", "success")
			log.Printf("[hub] rc session start device=%s operator=%s", p.DeviceID, p.Operator)

		case proto.TypeRCInput, proto.TypeRCStop:
			h.mu.Lock()
			agentConn, ok := h.conns[deviceID]
			h.mu.Unlock()
			if ok {
				_ = agentConn.WriteMessage(raw)
			}
			if env.Type == proto.TypeRCStop {
				h.endSession(deviceID)
			}
		}
	}
}

// ---- REST 管理 API（受保护）----

func (h *Hub) handleDevices(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	list := h.store.List()
	writeJSON(w, map[string]any{"total": len(list), "devices": list})
}

func (h *Hub) handleReport(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	list := h.store.List()
	online := 0
	byOS := map[string]int{}
	for _, d := range list {
		if d.Online {
			online++
		}
		byOS[d.OS]++
	}
	writeJSON(w, map[string]any{
		"total": len(list), "online": online, "offline": len(list) - online, "by_os": byOS,
	})
}

func (h *Hub) handleDeviceGet(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	id := deviceIDFromPath(r.URL.Path)
	dev, ok := h.store.Get(id)
	if !ok {
		http.NotFound(w, r)
		return
	}
	writeJSON(w, dev)
}

func (h *Hub) handleDeviceCmd(w http.ResponseWriter, r *http.Request, claims crypto.Claims) {
	id := deviceIDFromPath(r.URL.Path)
	var body struct {
		Action      string `json:"action"`
		Command     string `json:"command"`
		SoftwareURL string `json:"software_url"`
		Platform    string `json:"platform"`
		PkgName     string `json:"pkg_name"`
	}
	_ = json.NewDecoder(r.Body).Decode(&body)

	h.mu.Lock()
	conn, ok := h.conns[id]
	h.mu.Unlock()
	if !ok {
		writeErr(w, 2001, "device offline")
		return
	}
	taskID := newTaskID()
	cmd, _ := proto.NewEnvelope(proto.TypeCommand, 0, proto.CommandPayload{
		Action: body.Action, TaskID: taskID, Command: body.Command,
		SoftwareURL: body.SoftwareURL, Platform: body.Platform, PkgName: body.PkgName,
	})
	h.sign(&cmd) // 安全基座：关键指令 HMAC 签名
	cmdBytes, _ := json.Marshal(cmd)
	if err := conn.WriteMessage(cmdBytes); err != nil {
		writeErr(w, 2001, "send failed: "+err.Error())
		return
	}
	h.audit(claims.Username, "command", id, "action="+body.Action+" cmd="+body.Command, "success")

	ch := make(chan proto.ResultPayload, 1)
	h.mu.Lock()
	h.pending[taskID] = ch
	h.mu.Unlock()
	defer func() {
		h.mu.Lock()
		delete(h.pending, taskID)
		h.mu.Unlock()
	}()

	timeout := 15 * time.Second
	if body.Action == "sw_install" {
		timeout = 120 * time.Second
	}
	select {
	case res := <-ch:
		writeJSON(w, res)
	case <-time.After(timeout):
		writeErr(w, 2001, "command timeout")
	}
}

func (h *Hub) handleSoftwareList(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	h.mu.Lock()
	items := make([]*proto.SoftwareItem, 0, len(h.software))
	for _, it := range h.software {
		items = append(items, it)
	}
	h.mu.Unlock()
	writeJSON(w, map[string]any{"total": len(items), "items": items})
}

func (h *Hub) handleSoftwareUpload(w http.ResponseWriter, r *http.Request, claims crypto.Claims) {
	if err := r.ParseMultipartForm(50 << 20); err != nil {
		writeErr(w, 3002, "parse form failed: "+err.Error())
		return
	}
	file, header, err := r.FormFile("file")
	if err != nil {
		writeErr(w, 3003, "missing file field")
		return
	}
	defer file.Close()
	name := r.FormValue("name")
	version := r.FormValue("version")
	platform := r.FormValue("platform")
	if name == "" {
		name = strings.TrimSuffix(header.Filename, filepath.Ext(header.Filename))
	}
	if version == "" {
		version = "1.0.0"
	}
	if platform == "" {
		platform = "any"
	}
	id := "sw-" + randHex(8)
	dst := filepath.Join(h.repoDir, id)
	out, err := os.Create(dst)
	if err != nil {
		writeErr(w, 3004, "save failed: "+err.Error())
		return
	}
	size, err := io.Copy(out, file)
	out.Close()
	if err != nil {
		writeErr(w, 3004, "save failed: "+err.Error())
		return
	}
	item := &proto.SoftwareItem{
		ID: id, Name: name, Version: version, Platform: platform,
		FileName: header.Filename, Size: size, UploadedAt: time.Now().Unix(),
	}
	h.mu.Lock()
	h.software[id] = item
	h.mu.Unlock()
	h.audit(claims.Username, "software_upload", id, "name="+name+" v"+version, "success")
	log.Printf("[hub] software uploaded: %s v%s (%s) %d bytes", name, version, platform, size)
	writeJSON(w, map[string]any{"code": 0, "item": item})
}

func (h *Hub) handleRepo(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	id := strings.TrimPrefix(r.URL.Path, "/api/v1/repo/")
	if id == "" {
		http.NotFound(w, r)
		return
	}
	h.mu.Lock()
	item, ok := h.software[id]
	h.mu.Unlock()
	if !ok {
		http.NotFound(w, r)
		return
	}
	path := filepath.Join(h.repoDir, id)
	w.Header().Set("Content-Disposition", "attachment; filename=\""+item.FileName+"\"")
	w.Header().Set("Content-Type", "application/octet-stream")
	http.ServeFile(w, r, path)
}

func (h *Hub) handleAudit(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	limit := 100
	if v := r.URL.Query().Get("limit"); v != "" {
		if n, err := atoi(v); err == nil && n > 0 {
			limit = n
		}
	}
	writeJSON(w, map[string]any{"total": h.auditor.AuditCount(), "events": h.auditor.Query(limit)})
}

func (h *Hub) handleDirSync(w http.ResponseWriter, r *http.Request, claims crypto.Claims) {
	res, err := h.RunDirSync(claims.Username)
	if err != nil {
		writeErr(w, 5001, err.Error())
		return
	}
	writeJSON(w, map[string]any{"code": 0, "result": res})
}

// ---- 控制台静态页 ----

func (h *Hub) handleConsole(w http.ResponseWriter, r *http.Request) {
	switch r.URL.Path {
	case "/", "/index.html":
		http.ServeFile(w, r, "console/index.html")
	case "/login.html":
		http.ServeFile(w, r, "console/login.html")
	case "/audit.html":
		http.ServeFile(w, r, "console/audit.html")
	case "/rc.html":
		http.ServeFile(w, r, "console/rc.html")
	case "/software.html":
		http.ServeFile(w, r, "console/software.html")
	default:
		http.NotFound(w, r)
	}
}

// ---- 安全基座：指令签名 ----

func (h *Hub) sign(env *proto.Envelope) {
	if h.secret == "" {
		return
	}
	env.Sig = crypto.SignCommand(h.secret, env.MsgID, env.Ts, env.Seq, string(env.Payload), env.Nonce)
}

// ---- helpers ----

func deviceIDFromPath(path string) string {
	trimmed := strings.TrimPrefix(path, "/api/v1/devices/")
	parts := strings.Split(trimmed, "/")
	if len(parts) == 0 {
		return ""
	}
	return parts[0]
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	_ = json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, code int, msg string) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(http.StatusOK)
	_ = json.NewEncoder(w).Encode(map[string]any{"code": code, "message": msg})
}

func newTaskID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return "t-" + hex.EncodeToString(b)
}

func randHex(n int) string {
	b := make([]byte, n)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func mustJSON(v any) []byte {
	b, _ := json.Marshal(v)
	return b
}

func mustRaw(v any) json.RawMessage {
	b, _ := json.Marshal(v)
	return b
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	neg := n < 0
	if neg {
		n = -n
	}
	buf := [12]byte{}
	pos := len(buf)
	for n > 0 {
		pos--
		buf[pos] = byte('0' + n%10)
		n /= 10
	}
	if neg {
		pos--
		buf[pos] = '-'
	}
	return string(buf[pos:])
}

func atoi(s string) (int, error) {
	n := 0
	neg := false
	i := 0
	if i < len(s) && (s[i] == '-' || s[i] == '+') {
		neg = s[i] == '-'
		i++
	}
	for ; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return 0, jsonError("not a number")
		}
		n = n*10 + int(s[i]-'0')
	}
	if neg {
		n = -n
	}
	return n, nil
}

func jsonError(s string) error { return &simpleErr{s} }

type simpleErr struct{ s string }

func (e *simpleErr) Error() string { return e.s }
