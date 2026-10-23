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
	"fmt"
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
	HubID       string // 多中枢部署时的节点标识（缺省 hub-<hostname>）
	PublicAddr  string // 节点对外地址（/api/v1/hubs 展示用）
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

	hubID      string
	publicAddr string
	bus        *CommandBus // 非 nil 时指令走多中枢总线（去粘性）
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
	hubID := cfg.HubID
	if hubID == "" {
		host, _ := os.Hostname()
		hubID = "hub-" + host
	}
	return &Hub{
		store:      cfg.Store,
		auditor:    cfg.Auditor,
		users:      cfg.Users,
		authSvc:    cfg.Auth,
		secret:     cfg.HMACSecret,
		enforce:    cfg.HMACEnforce,
		dirSync:    cfg.DirSync,
		conns:      make(map[string]*ws.Conn),
		pending:    make(map[string]chan proto.ResultPayload),
		sessions:   make(map[string]*RCSession),
		software:   make(map[string]*proto.SoftwareItem),
		repoDir:    cfg.RepoDir,
		hubID:      hubID,
		publicAddr: cfg.PublicAddr,
	}
}

// Handler 返回 HTTP 路由（REST + WS + 控制台静态页）。
func (h *Hub) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws/agent", h.handleAgentWS)
	mux.HandleFunc("/ws/viewer", h.handleViewerWS)
	// 公开
	mux.HandleFunc("/api/v1/login", h.handleLogin)
	// 受保护：自助改密（仅需登录；must_change 账户的唯一放行接口）
	mux.HandleFunc("/api/v1/password", h.guardAuthOnly(h.handleChangePassword))
	// 受保护：资产
	mux.HandleFunc("/api/v1/devices", h.guard(auth.PermDeviceView, h.handleDevices))
	mux.HandleFunc("/api/v1/report", h.guard(auth.PermDeviceView, h.handleReport))
	mux.HandleFunc("/api/v1/devices/", h.guardDeviceByID)
	// 受保护：软件仓库
	mux.HandleFunc("/api/v1/software", h.guardSoftware)
	mux.HandleFunc("/api/v1/repo/", h.handleRepo) // 副端下载（演示放开；生产应短令牌/鉴权）
	// 受保护：审计 / 目录同步
	mux.HandleFunc("/api/v1/audit", h.guard(auth.PermAuditView, h.handleAudit))
	mux.HandleFunc("/api/v1/audit/verify", h.guard(auth.PermAuditView, h.handleAuditVerify))
	mux.HandleFunc("/api/v1/dirsync", h.guard(auth.PermDirSync, h.handleDirSync))
	// 受保护：账号管理（admin）
	mux.HandleFunc("/api/v1/admin/users", h.guard(auth.PermUserManage, h.handleAdminUsers))
	mux.HandleFunc("/api/v1/admin/users/", h.guard(auth.PermUserManage, h.handleAdminUserAction))
	// 受保护：多中枢节点视图（admin）
	mux.HandleFunc("/api/v1/hubs", h.guard(auth.PermUserManage, h.handleHubs))
	mux.HandleFunc("/", h.handleConsole)
	return mux
}

// guard 包裹受保护接口：校验 Bearer 令牌并判定角色权限。
// 携带 must_change 标记的令牌除改密接口外一律 403（强制改密）。
func (h *Hub) guard(perm string, fn func(http.ResponseWriter, *http.Request, crypto.Claims)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		claims, err := h.authSvc.VerifyToken(auth.TokenFromHeader(r.Header.Get("Authorization")))
		if err != nil {
			writeErr(w, 401, "unauthorized")
			return
		}
		if claims.MustChange {
			writeErr(w, 4033, "password_change_required")
			return
		}
		if !h.authSvc.HasPerm(claims.Role, perm) {
			writeErr(w, 403, "forbidden: 需要权限 "+perm)
			return
		}
		fn(w, r, claims)
	}
}

// guardAuthOnly 仅校验令牌有效性（不做权限/改密判定），供自助改密接口使用。
func (h *Hub) guardAuthOnly(fn func(http.ResponseWriter, *http.Request, crypto.Claims)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		claims, err := h.authSvc.VerifyToken(auth.TokenFromHeader(r.Header.Get("Authorization")))
		if err != nil {
			writeErr(w, 401, "unauthorized")
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
	res, err := h.authSvc.Login(body.Username, body.Password)
	if err != nil {
		h.audit(body.Username, "login", "", "登录失败", "failed")
		writeErr(w, 401, "invalid credentials")
		return
	}
	h.audit(body.Username, "login", "", "角色="+res.Role, "success")
	writeJSON(w, map[string]any{"code": 0, "token": res.Token, "role": res.Role, "must_change_password": res.MustChangePassword})
}

// handleChangePassword 自助改密：校验旧口令 + 强度策略；成功后清除 must_change 标记。
func (h *Hub) handleChangePassword(w http.ResponseWriter, r *http.Request, claims crypto.Claims) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var body struct {
		OldPassword string `json:"old_password"`
		NewPassword string `json:"new_password"`
	}
	_ = json.NewDecoder(r.Body).Decode(&body)
	if err := h.authSvc.ChangePassword(claims.Username, body.OldPassword, body.NewPassword); err != nil {
		h.audit(claims.Username, "password_change", claims.Username, "改密失败: "+err.Error(), "failed")
		writeErr(w, 4002, err.Error())
		return
	}
	h.audit(claims.Username, "password_change", claims.Username, "改密成功", "success")
	// 签发新令牌（must_change 已清除），前端无缝续用
	res, err := h.authSvc.Login(claims.Username, body.NewPassword)
	if err != nil {
		writeJSON(w, map[string]any{"code": 0, "message": "password changed"})
		return
	}
	writeJSON(w, map[string]any{"code": 0, "token": res.Token, "role": res.Role})
}

// handleAdminUsers 账号列表（admin）。
func (h *Hub) handleAdminUsers(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	writeJSON(w, map[string]any{"total": h.users.Count(), "users": h.users.ListUsers()})
}

// handleAdminUserAction 账号级管理动作（admin）：
//   POST /api/v1/admin/users/{name}/force-password-change  强制改密
//   POST /api/v1/admin/users/{name}/disable                禁用
//   POST /api/v1/admin/users/{name}/enable                 启用
func (h *Hub) handleAdminUserAction(w http.ResponseWriter, r *http.Request, claims crypto.Claims) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	rest := strings.TrimPrefix(r.URL.Path, "/api/v1/admin/users/")
	parts := strings.Split(rest, "/")
	if len(parts) != 2 || parts[0] == "" {
		writeErr(w, 4001, "usage: /api/v1/admin/users/{name}/{force-password-change|disable|enable}")
		return
	}
	name, action := parts[0], parts[1]
	u, ok := h.users.GetUserByName(name)
	if !ok {
		writeErr(w, 4004, "user not found")
		return
	}
	switch action {
	case "force-password-change":
		if err := h.authSvc.ForcePasswordChange(name); err != nil {
			writeErr(w, 4004, err.Error())
			return
		}
		h.audit(claims.Username, "force_pw_change", name, "管理员设置强制改密", "success")
		writeJSON(w, map[string]any{"code": 0, "message": "ok"})
	case "disable":
		u.Disabled = true
		h.users.UpsertUser(u)
		h.audit(claims.Username, "user_disable", name, "管理员禁用账号", "success")
		writeJSON(w, map[string]any{"code": 0, "message": "ok"})
	case "enable":
		u.Disabled = false
		h.users.UpsertUser(u)
		h.audit(claims.Username, "user_enable", name, "管理员启用账号", "success")
		writeJSON(w, map[string]any{"code": 0, "message": "ok"})
	default:
		writeErr(w, 4001, "unknown action: "+action)
	}
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

	timeout := 15 * time.Second
	if body.Action == "sw_install" {
		timeout = 120 * time.Second
	}

	taskID := newTaskID()
	cmd, _ := proto.NewEnvelope(proto.TypeCommand, 0, proto.CommandPayload{
		Action: body.Action, TaskID: taskID, Command: body.Command,
		SoftwareURL: body.SoftwareURL, Platform: body.Platform, PkgName: body.PkgName,
	})
	h.sign(&cmd) // 安全基座：关键指令 HMAC 签名
	cmdBytes, _ := json.Marshal(cmd)

	auditDetail := "action=" + body.Action + " cmd=" + body.Command
	var res proto.ResultPayload
	var err error
	if h.bus != nil {
		// 多中枢总线：落 outbox → 本地认领或跨实例执行（去粘性）
		res, err = h.bus.Dispatch(id, taskID, cmdBytes, timeout)
	} else {
		// 单实例（内存模式）：本地直发
		h.mu.Lock()
		conn, ok := h.conns[id]
		h.mu.Unlock()
		if !ok {
			writeErr(w, 2001, "device offline")
			return
		}
		if err = conn.WriteMessage(cmdBytes); err != nil {
			writeErr(w, 2001, "send failed: "+err.Error())
			return
		}
		res, err = h.awaitTaskResult(taskID, timeout)
	}
	if err != nil {
		h.audit(claims.Username, "command", id, auditDetail, "failed")
		writeErr(w, 2001, err.Error())
		return
	}
	h.audit(claims.Username, "command", id, auditDetail, "success")
	writeJSON(w, res)
}

// awaitTaskResult 注册 taskID 等待通道并阻塞至回执或超时。
func (h *Hub) awaitTaskResult(taskID string, timeout time.Duration) (proto.ResultPayload, error) {
	ch := make(chan proto.ResultPayload, 1)
	h.mu.Lock()
	h.pending[taskID] = ch
	h.mu.Unlock()
	defer func() {
		h.mu.Lock()
		delete(h.pending, taskID)
		h.mu.Unlock()
	}()
	select {
	case res := <-ch:
		return res, nil
	case <-time.After(timeout):
		return proto.ResultPayload{}, fmt.Errorf("command timeout")
	}
}

// deviceOnlineLocal 判断设备是否长连接在本实例。
func (h *Hub) deviceOnlineLocal(id string) bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	_, ok := h.conns[id]
	return ok
}

// sendToDevice 经本实例长连接下发原始信封。
func (h *Hub) sendToDevice(id string, envBytes []byte) bool {
	h.mu.Lock()
	conn, ok := h.conns[id]
	h.mu.Unlock()
	if !ok {
		return false
	}
	return conn.WriteMessage(envBytes) == nil
}

// localDeviceIDs 返回本实例在线设备 ID 列表（总线认领用）。
func (h *Hub) localDeviceIDs() []string {
	h.mu.Lock()
	defer h.mu.Unlock()
	ids := make([]string, 0, len(h.conns))
	for id := range h.conns {
		ids = append(ids, id)
	}
	return ids
}

// handleHubs 多中枢节点视图（集群心跳）。
func (h *Hub) handleHubs(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	if h.bus == nil {
		writeJSON(w, map[string]any{"total": 0, "hubs": []HubNode{}, "note": "bus disabled (memory mode)"})
		return
	}
	nodes := h.bus.Nodes()
	writeJSON(w, map[string]any{"total": len(nodes), "hubs": nodes, "self": h.hubID})
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

// handleAuditVerify 重放审计哈希链，返回 WORM 完整性结论。
func (h *Hub) handleAuditVerify(w http.ResponseWriter, r *http.Request, _ crypto.Claims) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	rep := h.auditor.VerifyChain()
	h.audit("system", "audit_verify", "", fmt.Sprintf("chain valid=%v checked=%d", rep.Valid, rep.Checked), "success")
	writeJSON(w, rep)
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
	case "/change-password.html":
		http.ServeFile(w, r, "console/change-password.html")
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
	n, err := atoi64(s)
	return int(n), err
}

func atoi64(s string) (int64, error) {
	n := int64(0)
	neg := false
	i := 0
	if i < len(s) && (s[i] == '-' || s[i] == '+') {
		neg = s[i] == '-'
		i++
	}
	if i == len(s) {
		return 0, jsonError("not a number")
	}
	for ; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return 0, jsonError("not a number")
		}
		n = n*10 + int64(s[i]-'0')
	}
	if neg {
		n = -n
	}
	return n, nil
}

func jsonError(s string) error { return &simpleErr{s} }

type simpleErr struct{ s string }

func (e *simpleErr) Error() string { return e.s }
