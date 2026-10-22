// Package hub 实现御端服务端中枢。
// 职责：Agent 长连接纳管（注册/心跳/指令结果）、管理 REST API、远程控制信令与画面中继（M4）、
// 软件仓库与分发（M5）、资产台账与报表。存储当前为内存实现，接口化便于后续接入 PostgreSQL。
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

// Hub 是中枢核心结构。
type Hub struct {
	store    *store.MemStore
	mu       sync.Mutex
	conns    map[string]*ws.Conn        // deviceID -> 控制面长连接
	pending  map[string]chan proto.ResultPayload
	sessions map[string]*RCSession      // deviceID -> 远控会话
	software map[string]*proto.SoftwareItem
	repoDir  string
}

// NewHub 构造中枢。repoDir 为软件仓库落盘目录。
func NewHub(repoDir string) *Hub {
	if repoDir == "" {
		repoDir = "data/repo"
	}
	_ = os.MkdirAll(repoDir, 0o755)
	return &Hub{
		store:    store.NewMemStore(),
		conns:    make(map[string]*ws.Conn),
		pending:  make(map[string]chan proto.ResultPayload),
		sessions: make(map[string]*RCSession),
		software: make(map[string]*proto.SoftwareItem),
		repoDir:  repoDir,
	}
}

// Handler 返回 HTTP 路由（REST + WS + 控制台静态页）。
func (h *Hub) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws/agent", h.handleAgentWS)
	mux.HandleFunc("/ws/viewer", h.handleViewerWS)
	mux.HandleFunc("/api/v1/devices", h.handleDevices)
	mux.HandleFunc("/api/v1/report", h.handleReport)
	mux.HandleFunc("/api/v1/software", h.handleSoftware)
	mux.HandleFunc("/api/v1/repo/", h.handleRepo)
	mux.HandleFunc("/api/v1/devices/", h.handleDeviceByID)
	mux.HandleFunc("/", h.handleConsole)
	return mux
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

		// —— M4 远控：被控端回报，中继给 viewer ——
		case proto.TypeRCReady, proto.TypeRCFrame, proto.TypeRCEvent:
			h.relayToViewer(deviceID, raw)
		case proto.TypeRCStop:
			h.endSession(deviceID)

		default:
			// ack / event 等暂不处理
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

// relayToViewer 将以 deviceID 关联的远控会话画面/事件转发给 viewer。
func (h *Hub) relayToViewer(deviceID string, raw []byte) {
	h.mu.Lock()
	s, ok := h.sessions[deviceID]
	h.mu.Unlock()
	if !ok || s.Viewer == nil {
		return
	}
	_ = s.Viewer.WriteMessage(raw)
}

// endSession 关闭并清理一个远控会话。
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
			// 转发给被控端，开启捕获
			_ = agentConn.WriteMessage(raw)
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

// ---- REST 管理 API ----

func (h *Hub) handleDevices(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	list := h.store.List()
	writeJSON(w, map[string]any{"total": len(list), "devices": list})
}

func (h *Hub) handleReport(w http.ResponseWriter, r *http.Request) {
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

func (h *Hub) handleDeviceByID(w http.ResponseWriter, r *http.Request) {
	trimmed := strings.TrimPrefix(r.URL.Path, "/api/v1/devices/")
	parts := strings.Split(trimmed, "/")
	if len(parts) == 0 || parts[0] == "" {
		http.NotFound(w, r)
		return
	}
	id := parts[0]

	if r.Method == http.MethodGet && len(parts) == 1 {
		dev, ok := h.store.Get(id)
		if !ok {
			http.NotFound(w, r)
			return
		}
		writeJSON(w, dev)
		return
	}

	// POST /api/v1/devices/:id/commands  {action, command, software_url, platform, pkg_name}
	if r.Method == http.MethodPost && len(parts) == 2 && parts[1] == "commands" {
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
		cmdBytes, _ := json.Marshal(cmd)
		if err := conn.WriteMessage(cmdBytes); err != nil {
			writeErr(w, 2001, "send failed: "+err.Error())
			return
		}

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
		return
	}

	// POST /api/v1/devices/:id/push  {software_id} —— 便捷分发
	if r.Method == http.MethodPost && len(parts) == 2 && parts[1] == "push" {
		var body struct {
			SoftwareID string `json:"software_id"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		h.mu.Lock()
		item, ok := h.software[body.SoftwareID]
		conn, online := h.conns[id]
		h.mu.Unlock()
		if !ok {
			writeErr(w, 3001, "software not found")
			return
		}
		if !online {
			writeErr(w, 2001, "device offline")
			return
		}
		scheme := "http"
		if r.TLS != nil {
			scheme = "https"
		}
		downloadURL := scheme + "://" + r.Host + "/api/v1/repo/" + item.ID
		taskID := newTaskID()
		cmd, _ := proto.NewEnvelope(proto.TypeCommand, 0, proto.CommandPayload{
			Action: "sw_install", TaskID: taskID, SoftwareURL: downloadURL,
			Platform: item.Platform, PkgName: item.FileName,
		})
		cmdBytes, _ := json.Marshal(cmd)
		if err := conn.WriteMessage(cmdBytes); err != nil {
			writeErr(w, 2001, "send failed: "+err.Error())
			return
		}
		writeJSON(w, map[string]any{"code": 0, "task_id": taskID, "message": "分发任务已下发", "download_url": downloadURL})
		return
	}

	http.NotFound(w, r)
}

// ---- M5 软件仓库 ----

func (h *Hub) handleSoftware(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		h.mu.Lock()
		items := make([]*proto.SoftwareItem, 0, len(h.software))
		for _, it := range h.software {
			items = append(items, it)
		}
		h.mu.Unlock()
		writeJSON(w, map[string]any{"total": len(items), "items": items})
		return
	}
	if r.Method == http.MethodPost {
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
		log.Printf("[hub] software uploaded: %s v%s (%s) %d bytes", name, version, platform, size)
		writeJSON(w, map[string]any{"code": 0, "item": item})
		return
	}
	http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
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

// ---- 控制台静态页 ----

func (h *Hub) handleConsole(w http.ResponseWriter, r *http.Request) {
	switch r.URL.Path {
	case "/", "/index.html":
		http.ServeFile(w, r, "console/index.html")
	case "/rc.html":
		http.ServeFile(w, r, "console/rc.html")
	case "/software.html":
		http.ServeFile(w, r, "console/software.html")
	default:
		http.NotFound(w, r)
	}
}

// ---- helpers ----

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	_ = json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, code int, msg string) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
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
