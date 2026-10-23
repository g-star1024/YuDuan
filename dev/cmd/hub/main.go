// 御端服务端中枢入口（M2 底座 + M4 远控 + M5 分发 + M6 管理与审计 + 安全基座）。
package main

import (
	"log"
	"net/http"
	"os"

	"yuduan/internal/audit"
	"yuduan/internal/auth"
	"yuduan/internal/directory"
	"yuduan/internal/hub"
	"yuduan/internal/store"
	"yuduan/internal/tlsutil"
)

func main() {
	repoDir := envOr("YUDUAN_REPO", "data/repo")
	dsn := os.Getenv("YUDUAN_DB_DSN")
	secret := envOr("YUDUAN_HMAC_SECRET", "")
	enforce := os.Getenv("YUDUAN_HMAC_ENFORCE") == "1"
	pepper := envOr("YUDUAN_PEPPER", envOr("YUDUAN_PW_PEPPER", "yuduan-dev-pepper"))
	dirFile := os.Getenv("YUDUAN_DIR_FILE")
	adminMustChangePW := os.Getenv("YUDUAN_ADMIN_MUST_CHANGE_PW") != "0" // 默认开启（生产安全姿态）

	// 存储 / 审计 / 账号：有 DSN 则统一走 PostgreSQL，否则内存
	var st store.Store = store.NewMemStore()
	var aud audit.Auditor = audit.NewMemAuditor()
	var us auth.UserStore = store.NewMemUserStore()
	var pgStore *store.PostgresStore
	if dsn != "" {
		pg, err := store.NewPostgresStore(dsn)
		if err != nil {
			log.Fatalf("[hub] PostgreSQL 连接失败: %v", err)
		}
		pgStore = pg
		st, aud, us = pg, pg, pg
		log.Printf("[hub] 持久化后端：PostgreSQL")
	} else {
		log.Printf("[hub] 持久化后端：内存（重启即丢；设置 YUDUAN_DB_DSN 切换到 PostgreSQL）")
	}
	store.SeedDefaultAdmin(us, pepper, adminMustChangePW)
	authSvc := auth.NewAuthService(us, secret, pepper)

	var dirSync *directory.SyncEngine
	if dirFile != "" {
		dirSync = directory.NewSyncEngine(&directory.FileDirectory{Path: dirFile}, directory.DefaultRoleMapping(), us, pepper)
		log.Printf("[hub] 目录同步源：%s", dirFile)
	}

	h := hub.NewHub(hub.HubConfig{
		RepoDir: repoDir, Store: st, Auditor: aud, Users: us,
		Auth: authSvc, HMACSecret: secret, HMACEnforce: enforce, DirSync: dirSync,
		HubID: os.Getenv("YUDUAN_HUB_ID"), PublicAddr: envOr("YUDUAN_PUBLIC_ADDR", plainAddrGuess()),
	})
	if adminMustChangePW {
		log.Printf("[hub] 内置 admin 首次登录需强制改密（YUDUAN_ADMIN_MUST_CHANGE_PW=0 可关闭）")
	}

	// 多中枢指令总线（去粘性）：仅 PostgreSQL 后端可用
	if pgStore != nil {
		hubID := os.Getenv("YUDUAN_HUB_ID")
		if err := h.EnableBus(pgStore.Pool(), hubID, dsn); err != nil {
			log.Printf("[hub] 指令总线启用失败（退化为单实例模式）: %v", err)
		}
	}

	if dirSync != nil {
		if res, err := h.RunDirSync("system"); err == nil && res != nil {
			log.Printf("[hub] 目录同步完成: %+v", res)
		} else if err != nil {
			log.Printf("[hub] 目录同步失败: %v", err)
		}
	}

	go h.StartReaper()

	plainAddr := envOr("YUDUAN_HUB_ADDR", ":8080")
	// TLS 监听：未设置该变量时默认 :8443；显式设为空串 = 关闭 TLS（多实例测试/纯内网明文部署）
	tlsAddr := os.Getenv("YUDUAN_TLS_ADDR")
	if _, ok := os.LookupEnv("YUDUAN_TLS_ADDR"); !ok {
		tlsAddr = ":8443"
	}
	cert := envOr("YUDUAN_TLS_CERT", "certs/cert.pem")
	key := envOr("YUDUAN_TLS_KEY", "certs/key.pem")

	if _, err := os.Stat(cert); err != nil {
		if err := tlsutil.GenerateSelfSigned(cert, key); err == nil {
			log.Printf("[hub] 已生成自签证书: %s", cert)
		} else {
			log.Printf("[hub] 生成证书失败，仅启动明文监听: %v", err)
			tlsAddr = ""
		}
	}
	if tlsAddr != "" {
		go func() {
			log.Printf("[hub] TLS 中枢监听 %s（https + wss）", tlsAddr)
			log.Fatal(http.ListenAndServeTLS(tlsAddr, cert, key, h.Handler()))
		}()
	}

	log.Printf("[hub] 明文中枢监听 %s（http + ws）；控制台 / ；软件仓库 %s", plainAddr, repoDir)
	log.Fatal(http.ListenAndServe(plainAddr, h.Handler()))
}

func plainAddrGuess() string {
	return envOr("YUDUAN_HUB_ADDR", ":8080")
}

func envOr(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}
