// Package hub 实现御端服务端中枢（M2 底座联调）。
// 职责：Agent 长连接纳管（注册/心跳/指令结果）、管理 REST API、远控以外的指令下发、
// 资产台账查询与报表。存储当前为内存实现，接口化以便后续接入 PostgreSQL（dev/proto/schema.sql）。
package hub

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"log"
	"net/http"
	"strings"
	"sync"
	"time"

	"yuduan/internal/proto"
	"yuduan/internal/store"
	"yuduan/internal/ws"
)

// Hub 是中枢核心结构。
type Hub struct {
	store   *store.MemStore
	mu      sync.Mutex
	conns   map[string]*ws.Conn // deviceID -> 控制面长连接
	pending map[string]chan proto.ResultPayload // taskID -> 指令结果等待通道
}

// NewHub 构造中枢。
func NewHub() *Hub {
	return &Hub{
		store:   store.NewMemStore(),
		conns:   make(map[string]*ws.Conn),
		pending: make(map[string]chan proto.ResultPayload),
	}
}

// Handler 返回 HTTP 路由（REST + WS + 控制台静态页）。
func (h *Hub) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws/agent", h.handleAgentWS)
	mux.HandleFunc("/api/v1/devices", h.handleDevices)
	mux.HandleFunc("/api/v1/report", h.handleReport)
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
				Hostname:     p.Hostname,
				OS:           p.OS,
				OSVersion:    p.OSVersion,
				Arch:         p.Arch,
				CPU:          p.CPU,
				MemoryGB:     p.MemoryGB,
				DiskGB:       p.DiskGB,
				SerialNo:     p.SerialNo,
				MAC:          p.MAC,
				IP:           p.IP,
				AgentVersion: p.AgentVersion,
				Token:        p.Token,
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

		default:
			// ack / event 等暂不处理
		}
	}

	if deviceID != "" {
		h.store.MarkOffline(deviceID)
		h.mu.Lock()
		delete(h.conns, deviceID)
		h.mu.Unlock()
		log.Printf("[hub] agent offline: %s", deviceID)
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
		"total":   len(list),
		"online":  online,
		"offline": len(list) - online,
		"by_os":   byOS,
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

	// GET /api/v1/devices/:id
	if r.Method == http.MethodGet && len(parts) == 1 {
		dev, ok := h.store.Get(id)
		if !ok {
			http.NotFound(w, r)
			return
		}
		writeJSON(w, dev)
		return
	}

	// POST /api/v1/devices/:id/commands  {action, command}
	if r.Method == http.MethodPost && len(parts) == 2 && parts[1] == "commands" {
		var body struct {
			Action  string `json:"action"`
			Command string `json:"command"`
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
			Action:  body.Action,
			TaskID:  taskID,
			Command: body.Command,
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

		select {
		case res := <-ch:
			writeJSON(w, res)
		case <-time.After(15 * time.Second):
			writeErr(w, 2001, "command timeout")
		}
		return
	}

	http.NotFound(w, r)
}

func (h *Hub) handleConsole(w http.ResponseWriter, r *http.Request) {
	http.ServeFile(w, r, "console/index.html")
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
