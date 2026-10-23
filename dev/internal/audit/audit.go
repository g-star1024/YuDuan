// Package audit 提供御端 M6 审计能力：记录并查询关键管理动作。
// Auditor 接口同时被内存实现与 PostgreSQL 实现遵循。
package audit

import (
	"sync"
	"time"
)

// Event 是一条审计记录。
type Event struct {
	ID     int64  `json:"id"`
	Time   int64  `json:"time"`    // unix 秒
	Actor  string `json:"actor"`   // 操作人（登录账号）
	Action string `json:"action"`  // 动作类型，如 login/shell/rc_start/sw_push/software_upload
	Target string `json:"target"`  // 作用对象，如 device_id / software_id
	Detail string `json:"detail"`  // 摘要
	Result string `json:"result"`  // success / failed
}

// Auditor 审计存储接口。
type Auditor interface {
	Record(e Event) Event
	Query(limit int) []Event
	AuditCount() int64
}

// MemAuditor 内存版审计（演示/无 PG 时使用）。
type MemAuditor struct {
	mu     sync.Mutex
	events []Event
	seq    int64
}

// NewMemAuditor 构造内存审计器。
func NewMemAuditor() *MemAuditor { return &MemAuditor{} }

// Record 追加一条审计记录并分配自增 ID。
func (m *MemAuditor) Record(e Event) Event {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.seq++
	e.ID = m.seq
	if e.Time == 0 {
		e.Time = time.Now().Unix()
	}
	if e.Result == "" {
		e.Result = "success"
	}
	m.events = append(m.events, e)
	// 控制内存占用：保留最近 5000 条
	if len(m.events) > 5000 {
		m.events = m.events[len(m.events)-5000:]
	}
	return e
}

// Query 返回最近 limit 条（倒序）。
func (m *MemAuditor) Query(limit int) []Event {
	m.mu.Lock()
	defer m.mu.Unlock()
	n := len(m.events)
	if limit <= 0 || limit > n {
		limit = n
	}
	out := make([]Event, 0, limit)
	for i := n - limit; i < n; i++ {
		out = append(out, m.events[i])
	}
	return out
}

// AuditCount 返回审计总数。
func (m *MemAuditor) AuditCount() int64 {
	m.mu.Lock()
	defer m.mu.Unlock()
	return int64(len(m.events))
}
