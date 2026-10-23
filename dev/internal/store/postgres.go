package store

import (
	"fmt"
	"strconv"
	"strings"
	"time"

	"yuduan/internal/audit"
	"yuduan/internal/auth"
	"yuduan/internal/pg"
)

// PostgresStore 以 PostgreSQL 作为持久化后端，实现 Store / Auditor / UserStore。
// 受限环境下使用自研最小 PG 客户端（internal/pg）：简单查询 + 内联转义。
// 生产可替换为 lib/pq 或 pgx 以获得参数化查询与连接池（见安全里程碑文档）。
type PostgresStore struct {
	db *pg.Conn
}

// DSN 形如 host=127.0.0.1 port=5432 user=postgres dbname=yuduan password=postgres
func parseDSN(dsn string) (host string, port int, user, dbname, password string) {
	host = "127.0.0.1"
	port = 5432
	for _, kv := range strings.Fields(dsn) {
		p := strings.SplitN(kv, "=", 2)
		if len(p) != 2 {
			continue
		}
		switch p[0] {
		case "host":
			host = p[1]
		case "port":
			port, _ = strconv.Atoi(p[1])
		case "user":
			user = p[1]
		case "dbname", "database":
			dbname = p[1]
		case "password":
			password = p[1]
		}
	}
	return
}

// NewPostgresStore 连接并建表。
func NewPostgresStore(dsn string) (*PostgresStore, error) {
	host, port, user, dbname, password := parseDSN(dsn)
	db, err := pg.Connect(host, port, user, dbname, password)
	if err != nil {
		return nil, err
	}
	p := &PostgresStore{db: db}
	if err := p.migrate(); err != nil {
		return nil, err
	}
	return p, nil
}

func (p *PostgresStore) migrate() error {
	stmts := []string{
		`CREATE TABLE IF NOT EXISTS devices (
			id TEXT PRIMARY KEY, hostname TEXT, os TEXT, os_version TEXT, arch TEXT,
			cpu TEXT, memory_gb INT, disk_gb INT, serial_no TEXT, mac TEXT, ip TEXT,
			agent_version TEXT, token TEXT UNIQUE, online BOOLEAN, last_seen TIMESTAMP, created_at TIMESTAMP)`,
		`CREATE TABLE IF NOT EXISTS audit_events (
			id BIGSERIAL PRIMARY KEY, time BIGINT, actor TEXT, action TEXT, target TEXT, detail TEXT, result TEXT)`,
		`CREATE TABLE IF NOT EXISTS users (
			username TEXT PRIMARY KEY, display_name TEXT, role TEXT, password TEXT, disabled BOOLEAN)`,
	}
	for _, s := range stmts {
		if err := p.db.Exec(s); err != nil {
			return fmt.Errorf("migrate: %v", err)
		}
	}
	return nil
}

// q 对字符串做 SQL 字面量转义（最小 PG 客户端无参数化，仅限内部可信数据）。
func q(s string) string {
	return "'" + strings.ReplaceAll(s, "'", "''") + "'"
}

func qtime(t time.Time) string {
	return "'" + t.Format("2006-01-02 15:04:05") + "'"
}

func (p *PostgresStore) Upsert(d Device) Device {
	now := time.Now().UTC()
	// 令牌复用：按 token 找回既有设备身份
	if d.Token != "" {
		_, rows, _ := p.db.Query("SELECT id,created_at FROM devices WHERE token=" + q(d.Token))
		if len(rows) > 0 {
			d.ID = rows[0][0]
			if t, err := time.Parse("2006-01-02 15:04:05", rows[0][1]); err == nil {
				d.CreatedAt = t
			}
		}
	}
	if d.ID == "" {
		d.ID = newID()
	}
	if d.Token == "" {
		d.Token = newToken()
	}
	if d.CreatedAt.IsZero() {
		d.CreatedAt = now
	}
	d.Online = true
	d.LastSeen = now
	cols := "id,hostname,os,os_version,arch,cpu,memory_gb,disk_gb,serial_no,mac,ip,agent_version,token,online,last_seen,created_at"
	vals := fmt.Sprintf("%s,%s,%s,%s,%s,%s,%d,%d,%s,%s,%s,%s,%s,true,%s,%s",
		q(d.ID), q(d.Hostname), q(d.OS), q(d.OSVersion), q(d.Arch), q(d.CPU),
		d.MemoryGB, d.DiskGB, q(d.SerialNo), q(d.MAC), q(d.IP), q(d.AgentVersion), q(d.Token), qtime(d.LastSeen), qtime(d.CreatedAt))
	upd := "hostname=EXCLUDED.hostname,os=EXCLUDED.os,os_version=EXCLUDED.os_version,arch=EXCLUDED.arch,cpu=EXCLUDED.cpu," +
		"memory_gb=EXCLUDED.memory_gb,disk_gb=EXCLUDED.disk_gb,serial_no=EXCLUDED.serial_no,mac=EXCLUDED.mac,ip=EXCLUDED.ip," +
		"agent_version=EXCLUDED.agent_version,token=EXCLUDED.token,online=true,last_seen=EXCLUDED.last_seen"
	sql := fmt.Sprintf("INSERT INTO devices (%s) VALUES (%s) ON CONFLICT (id) DO UPDATE SET %s", cols, vals, upd)
	if err := p.db.Exec(sql); err != nil {
		return d
	}
	return d
}

func (p *PostgresStore) Get(id string) (Device, bool) {
	_, rows, err := p.db.Query("SELECT id,hostname,os,os_version,arch,cpu,memory_gb,disk_gb,serial_no,mac,ip,agent_version,token,online,last_seen,created_at FROM devices WHERE id=" + q(id))
	if err != nil || len(rows) == 0 {
		return Device{}, false
	}
	return rowToDevice(rows[0]), true
}

func (p *PostgresStore) List() []Device {
	_, rows, err := p.db.Query("SELECT id,hostname,os,os_version,arch,cpu,memory_gb,disk_gb,serial_no,mac,ip,agent_version,token,online,last_seen,created_at FROM devices ORDER BY created_at")
	if err != nil {
		return nil
	}
	out := make([]Device, 0, len(rows))
	for _, r := range rows {
		out = append(out, rowToDevice(r))
	}
	return out
}

func (p *PostgresStore) MarkOffline(id string) {
	_ = p.db.Exec("UPDATE devices SET online=false WHERE id=" + q(id))
}

func (p *PostgresStore) Touch(id string) {
	_ = p.db.Exec("UPDATE devices SET online=true,last_seen=" + qtime(time.Now().UTC()) + " WHERE id=" + q(id))
}

func (p *PostgresStore) Count() int {
	_, rows, err := p.db.Query("SELECT count(*) FROM devices")
	if err != nil || len(rows) == 0 {
		return 0
	}
	n, _ := strconv.Atoi(rows[0][0])
	return n
}

func rowToDevice(r []string) Device {
	gb := func(s string) int { n, _ := strconv.Atoi(s); return n }
	online := r[13] == "t" || r[13] == "true"
	d := Device{
		ID: r[0], Hostname: r[1], OS: r[2], OSVersion: r[3], Arch: r[4], CPU: r[5],
		MemoryGB: gb(r[6]), DiskGB: gb(r[7]), SerialNo: r[8], MAC: r[9], IP: r[10],
		AgentVersion: r[11], Token: r[12], Online: online,
	}
	if t, err := time.Parse("2006-01-02 15:04:05", r[14]); err == nil {
		d.LastSeen = t
	}
	if t, err := time.Parse("2006-01-02 15:04:05", r[15]); err == nil {
		d.CreatedAt = t
	}
	return d
}

// ---- Auditor ----

func (p *PostgresStore) Record(e audit.Event) audit.Event {
	if e.Time == 0 {
		e.Time = time.Now().Unix()
	}
	if e.Result == "" {
		e.Result = "success"
	}
	sql := fmt.Sprintf("INSERT INTO audit_events (time,actor,action,target,detail,result) VALUES (%d,%s,%s,%s,%s,%s)",
		e.Time, q(e.Actor), q(e.Action), q(e.Target), q(e.Detail), q(e.Result))
	if err := p.db.Exec(sql); err != nil {
		return e
	}
	_, rows, _ := p.db.Query("SELECT id FROM audit_events ORDER BY id DESC LIMIT 1")
	if len(rows) > 0 {
		if id, err := strconv.ParseInt(rows[0][0], 10, 64); err == nil {
			e.ID = id
		}
	}
	return e
}

func (p *PostgresStore) Query(limit int) []audit.Event {
	if limit <= 0 {
		limit = 100
	}
	_, rows, err := p.db.Query(fmt.Sprintf("SELECT id,time,actor,action,target,detail,result FROM audit_events ORDER BY id DESC LIMIT %d", limit))
	if err != nil {
		return nil
	}
	out := make([]audit.Event, 0, len(rows))
	for _, r := range rows {
		t, _ := strconv.ParseInt(r[1], 10, 64)
		id, _ := strconv.ParseInt(r[0], 10, 64)
		out = append(out, audit.Event{ID: id, Time: t, Actor: r[2], Action: r[3], Target: r[4], Detail: r[5], Result: r[6]})
	}
	return out
}

func (p *PostgresStore) AuditCount() int64 {
	_, rows, err := p.db.Query("SELECT count(*) FROM audit_events")
	if err != nil || len(rows) == 0 {
		return 0
	}
	n, _ := strconv.ParseInt(rows[0][0], 10, 64)
	return n
}

// ---- UserStore ----

func (p *PostgresStore) UpsertUser(u auth.User) {
	vals := fmt.Sprintf("%s,%s,%s,%s,%t", q(u.Username), q(u.DisplayName), q(u.Role), q(u.Password), u.Disabled)
	upd := "display_name=EXCLUDED.display_name,role=EXCLUDED.role,password=EXCLUDED.password,disabled=EXCLUDED.disabled"
	sql := fmt.Sprintf("INSERT INTO users (username,display_name,role,password,disabled) VALUES (%s) ON CONFLICT (username) DO UPDATE SET %s", vals, upd)
	_ = p.db.Exec(sql)
}

func (p *PostgresStore) GetUserByName(name string) (auth.User, bool) {
	_, rows, err := p.db.Query("SELECT username,display_name,role,password,disabled FROM users WHERE username=" + q(name))
	if err != nil || len(rows) == 0 {
		return auth.User{}, false
	}
	r := rows[0]
	return auth.User{Username: r[0], DisplayName: r[1], Role: r[2], Password: r[3], Disabled: r[4] == "t" || r[4] == "true"}, true
}

func (p *PostgresStore) ListUsers() []auth.User {
	_, rows, err := p.db.Query("SELECT username,display_name,role,password,disabled FROM users ORDER BY username")
	if err != nil {
		return nil
	}
	out := make([]auth.User, 0, len(rows))
	for _, r := range rows {
		out = append(out, auth.User{Username: r[0], DisplayName: r[1], Role: r[2], Password: r[3], Disabled: r[4] == "t" || r[4] == "true"})
	}
	return out
}

func (p *PostgresStore) UserCount() int {
	_, rows, err := p.db.Query("SELECT count(*) FROM users")
	if err != nil || len(rows) == 0 {
		return 0
	}
	n, _ := strconv.Atoi(rows[0][0])
	return n
}
