package store

import (
	"sync"

	"yuduan/internal/auth"
	"yuduan/internal/crypto"
)

// MemUserStore 内存版账号存储，实现 auth.UserStore（演示/无 PG 时使用）。
type MemUserStore struct {
	mu    sync.RWMutex
	users map[string]*auth.User
}

// NewMemUserStore 构造内存账号存储。
func NewMemUserStore() *MemUserStore {
	return &MemUserStore{users: make(map[string]*auth.User)}
}

// UpsertUser 新增或更新账号。
func (m *MemUserStore) UpsertUser(u auth.User) {
	m.mu.Lock()
	defer m.mu.Unlock()
	cp := u
	m.users[u.Username] = &cp
}

// GetUserByName 按用户名取账号。
func (m *MemUserStore) GetUserByName(name string) (auth.User, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	u, ok := m.users[name]
	if !ok {
		return auth.User{}, false
	}
	return *u, true
}

// ListUsers 返回全部账号快照。
func (m *MemUserStore) ListUsers() []auth.User {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make([]auth.User, 0, len(m.users))
	for _, u := range m.users {
		out = append(out, *u)
	}
	return out
}

// Count 返回账号总数。
func (m *MemUserStore) Count() int {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return len(m.users)
}

// SeedDefaultAdmin 写入演示用默认账号。
// admin 始终确保存在（若缺失则补建，不覆盖已存在的 admin，例如目录同步/人工改密后），
// 以保证任何情况下都有可登录的超管入口；operator/viewer 仅在账号库为空时播种。
// 生产环境首次部署应强制改密或通过目录同步注入。
func SeedDefaultAdmin(us auth.UserStore, pepper string) {
	if _, ok := us.GetUserByName("admin"); !ok {
		us.UpsertUser(auth.User{
			Username:    "admin",
			DisplayName: "内置管理员",
			Role:        auth.RoleAdmin,
			Password:    crypto.HashPassword("admin123", pepper),
		})
	}
	if us.Count() > 0 {
		return
	}
	us.UpsertUser(auth.User{
		Username:    "operator",
		DisplayName: "运维操作员",
		Role:        auth.RoleOperator,
		Password:    crypto.HashPassword("operator123", pepper),
	})
	us.UpsertUser(auth.User{
		Username:    "viewer",
		DisplayName: "只读访客",
		Role:        auth.RoleViewer,
		Password:    crypto.HashPassword("viewer123", pepper),
	})
}
