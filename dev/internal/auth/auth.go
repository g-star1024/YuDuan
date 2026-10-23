// Package auth 实现御端 M6 管理与审计所需的身份认证与 RBAC。
// 角色：admin（全部权限）/ operator（运维操作）/ viewer（只读）。
// 仅依赖标准库，登录令牌由 internal/crypto 签发。
package auth

import (
	"strings"
	"time"

	"yuduan/internal/crypto"
)

// 角色定义。
const (
	RoleAdmin    = "admin"
	RoleOperator = "operator"
	RoleViewer   = "viewer"
)

// 权限点（用于接口级门禁）。
const (
	PermDeviceView   = "device:view"
	PermDeviceCmd    = "device:command"
	PermRCSession    = "rc:session"
	PermSoftwareMgmt = "software:manage"
	PermAuditView    = "audit:view"
	PermUserManage   = "user:manage"
	PermDirSync      = "dir:sync"
)

// rolePerms 定义各角色拥有的权限集合。
var rolePerms = map[string]map[string]bool{
	RoleAdmin:    {PermDeviceView: true, PermDeviceCmd: true, PermRCSession: true, PermSoftwareMgmt: true, PermAuditView: true, PermUserManage: true, PermDirSync: true},
	RoleOperator: {PermDeviceView: true, PermDeviceCmd: true, PermRCSession: true, PermSoftwareMgmt: true, PermAuditView: true},
	RoleViewer:   {PermDeviceView: true, PermAuditView: true},
}

// User 表示一个后台账号。
type User struct {
	Username    string `json:"username"`
	DisplayName string `json:"display_name"`
	Role        string `json:"role"`
	Password    string `json:"-"` // 仅存储摘要
	Disabled    bool   `json:"disabled"`
}

// UserStore 账号持久化接口；内存与 PostgreSQL 两种实现都遵循它。
type UserStore interface {
	UpsertUser(u User)
	GetUserByName(name string) (User, bool)
	ListUsers() []User
	Count() int
}

// AuthService 提供登录与令牌校验。
type AuthService struct {
	store  UserStore
	secret string
	pepper string
}

// NewAuthService 构造认证服务。
func NewAuthService(store UserStore, secret, pepper string) *AuthService {
	if secret == "" {
		secret = "yuduan-dev-secret"
	}
	if pepper == "" {
		pepper = "yuduan-dev-pepper"
	}
	return &AuthService{store: store, secret: secret, pepper: pepper}
}

// Login 校验口令并签发会话令牌（24h 有效）。
func (s *AuthService) Login(username, password string) (string, error) {
	u, ok := s.store.GetUserByName(username)
	if !ok || u.Disabled {
		return "", fmtError("invalid credentials")
	}
	want := crypto.HashPassword(password, s.pepper)
	if !hmacEqual(want, u.Password) {
		return "", fmtError("invalid credentials")
	}
	exp := time.Now().Add(24 * time.Hour).Unix()
	return crypto.SignToken(s.secret, u.Username, u.Role, exp), nil
}

// VerifyToken 校验并返回声明。
func (s *AuthService) VerifyToken(token string) (crypto.Claims, error) {
	return crypto.VerifyToken(s.secret, token)
}

// HasPerm 判断某角色是否拥有某权限。
func (s *AuthService) HasPerm(role, perm string) bool {
	set, ok := rolePerms[role]
	if !ok {
		return false
	}
	return set[perm]
}

// ValidRole 判断角色名是否合法。
func ValidRole(r string) bool {
	_, ok := rolePerms[r]
	return ok
}

func fmtError(s string) error { return &authErr{s} }

type authErr struct{ m string }

func (e *authErr) Error() string { return e.m }

// hmacEqual 恒定时间比较两摘要。
func hmacEqual(a, b string) bool {
	la, lb := len(a), len(b)
	if la != lb {
		return false
	}
	var diff byte
	for i := 0; i < la; i++ {
		diff |= a[i] ^ b[i]
	}
	return diff == 0
}

// TokenFromHeader 从 Authorization: Bearer <token> 中提取令牌。
func TokenFromHeader(authz string) string {
	if strings.HasPrefix(authz, "Bearer ") {
		return strings.TrimSpace(strings.TrimPrefix(authz, "Bearer "))
	}
	return ""
}
