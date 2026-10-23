//go:build ldap

// 此文件实现真实 AD/LDAP 目录源，依赖 github.com/go-ldap/ldap/v3。
// 离线环境（当前受限）通过 `//go:build ldap` 排除编译；联网部署时：
//   1) go get github.com/go-ldap/ldap/v3
//   2) 去掉本文件首行 build tag 或 `go build -tags ldap ./...`
// 同步引擎（SyncEngine）本身不依赖 LDAP，因此离线也可经 FileDirectory 完整验证。
package directory

import (
	"github.com/go-ldap/ldap/v3"
)

// LDAPConfig 是 AD/LDAP 连接配置。
type LDAPConfig struct {
	Host     string
	Port     int
	UseTLS   bool
	BindDN   string
	BindPwd  string
	BaseDN   string
	UserOU   string // 用户搜索基（可空则用 BaseDN）
	GroupOU  string
}

// LDAPDirectory 真实目录源。
type LDAPDirectory struct {
	cfg LDAPConfig
}

// NewLDAPDirectory 构造 LDAP 目录源。
func NewLDAPDirectory(cfg LDAPConfig) *LDAPDirectory { return &LDAPDirectory{cfg: cfg} }

// ListUsers 查询域内用户并映射为 DirUser。
func (l *LDAPDirectory) ListUsers() ([]DirUser, error) {
	conn, err := ldap.DialURL(ldapURL(l.cfg))
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	if err := conn.Bind(l.cfg.BindDN, l.cfg.BindPwd); err != nil {
		return nil, err
	}
	base := l.cfg.UserOU
	if base == "" {
		base = l.cfg.BaseDN
	}
	search := ldap.NewSearchRequest(base,
		ldap.ScopeWholeSubtree, ldap.NeverDerefAliases, 0, 0, false,
		"(&(objectClass=user)(!(userAccountControl:1.2.840.113556.1.4.803:=2)))",
		[]string{"sAMAccountName", "displayName", "mail", "memberOf"}, nil)
	res, err := conn.Search(search)
	if err != nil {
		return nil, err
	}
	out := make([]DirUser, 0, len(res.Entries))
	for _, e := range res.Entries {
		memberOf := []string{}
		for _, dn := range e.GetAttributeValues("memberOf") {
			memberOf = append(memberOf, cnOf(dn))
		}
		out = append(out, DirUser{
			Username:    e.GetAttributeValue("sAMAccountName"),
			DisplayName: e.GetAttributeValue("displayName"),
			Email:       e.GetAttributeValue("mail"),
			MemberOf:    memberOf,
		})
	}
	return out, nil
}

// ListGroups 查询域内组。
func (l *LDAPDirectory) ListGroups() ([]DirGroup, error) {
	conn, err := ldap.DialURL(ldapURL(l.cfg))
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	if err := conn.Bind(l.cfg.BindDN, l.cfg.BindPwd); err != nil {
		return nil, err
	}
	base := l.cfg.GroupOU
	if base == "" {
		base = l.cfg.BaseDN
	}
	search := ldap.NewSearchRequest(base, ldap.ScopeWholeSubtree, ldap.NeverDerefAliases, 0, 0, false,
		"(objectClass=group)", []string{"cn"}, nil)
	res, err := conn.Search(search)
	if err != nil {
		return nil, err
	}
	out := make([]DirGroup, 0, len(res.Entries))
	for _, e := range res.Entries {
		out = append(out, DirGroup{Name: e.GetAttributeValue("cn")})
	}
	return out, nil
}

func ldapURL(c LDAPConfig) string {
	scheme := "ldap"
	if c.UseTLS {
		scheme = "ldaps"
	}
	return scheme + "://" + c.Host + ":" + itoa(c.Port)
}

func cnOf(dn string) string {
	// 取 DN 中第一个 CN= 组件
	for i := 0; i < len(dn); i++ {
		if i+3 <= len(dn) && dn[i:i+3] == "CN=" {
			end := i + 3
			for end < len(dn) && dn[end] != ',' {
				end++
			}
			return dn[i+3 : end]
		}
	}
	return dn
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
