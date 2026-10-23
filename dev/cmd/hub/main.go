// 御端服务端中枢入口（M2 底座 + M4 远控 + M5 分发 + M6 管理与审计 + 安全基座）。
package main

import (
	"crypto/tls"
	"crypto/x509"
	"log"
	"net/http"
	"os"

	"yuduan/internal/audit"
	"yuduan/internal/auth"
	"yuduan/internal/directory"
	"yuduan/internal/hub"
	"yuduan/internal/keystore"
	"yuduan/internal/store"
	"yuduan/internal/tlsutil"
)

func main() {
	repoDir := envOr("YUDUAN_REPO", "data/repo")
	dsn := os.Getenv("YUDUAN_DB_DSN")
	// 密钥统一经 keystore 收口（KMS/挂载文件解耦点，避免硬编码）
	secret := keystore.LoadHMACSecret()
	enforce := os.Getenv("YUDUAN_HMAC_ENFORCE") == "1"
	pepper := envOr("YUDUAN_PEPPER", envOr("YUDUAN_PW_PEPPER", "yuduan-dev-pepper"))
	dirFile := os.Getenv("YUDUAN_DIR_FILE")
	adminMustChangePW := os.Getenv("YUDUAN_ADMIN_MUST_CHANGE_PW") != "0" // 默认开启（生产安全姿态）

	// 软件包签名种子（Ed25519 32 字节，hex）。生产应来自 KMS/密钥管理（keystore 解耦点）。
	// 缺省空 → 软件包仅记 SHA-256、不签名（副端仍做哈希校验，但无验签）。
	signSeed := keystore.LoadSignSeed()
	if len(signSeed) == 32 {
		log.Printf("[hub] 软件包签名：Ed25519 已启用（种子来自 keystore）")
	} else if os.Getenv("YUDUAN_SIGN_SEED") != "" || os.Getenv("YUDUAN_SIGN_SEED_FILE") != "" {
		log.Printf("[hub] 警告：YUDUAN_SIGN_SEED/_FILE 非法（需 32 字节 hex），已退化为不签名")
	} else {
		log.Printf("[hub] 软件包签名：未启用（设置 YUDUAN_SIGN_SEED 启用 Ed25519 签名）")
	}

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
		SignSeed: signSeed,
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

	// 设备通道 mTLS（可选）：YUDUAN_CLIENT_CA 设置时，单独监听 YUDUAN_MTLS_ADDR（默认 :8444），
	// 要求并验证客户端证书（仅放行 /ws/agent，控制台/管理 API 不在此端口暴露）。
	// 副端经 YUDUAN_CLIENT_CERT/YUDUAN_CLIENT_KEY 呈现客户端证书。
	if clientCA := os.Getenv("YUDUAN_CLIENT_CA"); clientCA != "" {
		mtlsAddr := envOr("YUDUAN_MTLS_ADDR", ":8444")
		serverCert, err := tls.LoadX509KeyPair(cert, key)
		if err != nil {
			log.Printf("[hub] mTLS 加载服务端证书失败，跳过 mTLS 监听: %v", err)
		} else if caPEM, err := os.ReadFile(clientCA); err != nil {
			log.Printf("[hub] mTLS 读取客户端 CA 失败，跳过 mTLS 监听: %v", err)
		} else {
			pool := x509.NewCertPool()
			if !pool.AppendCertsFromPEM(caPEM) {
				log.Printf("[hub] mTLS 客户端 CA 解析为空，跳过 mTLS 监听")
			} else {
				tlsCfg := &tls.Config{
					Certificates: []tls.Certificate{serverCert},
					ClientAuth:   tls.RequireAndVerifyClientCert,
					ClientCAs:    pool,
				}
				ln, err := tls.Listen("tcp", mtlsAddr, tlsCfg)
				if err != nil {
					log.Printf("[hub] mTLS 监听失败: %v", err)
				} else {
					log.Printf("[hub] 设备 mTLS 监听 %s（wss，要求并验证客户端证书，仅 /ws/agent）", mtlsAddr)
					go func() {
						log.Fatal(http.Serve(ln, agentOnlyHandler(h.Handler())))
					}()
				}
			}
		}
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

// agentOnlyHandler 将全路由处理器限定为仅放行设备通道 /ws/agent；其余路径 404。
// 用于 mTLS 监听，确保强制客户端证书的端口不暴露控制台/管理 API。
func agentOnlyHandler(inner http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/ws/agent" {
			inner.ServeHTTP(w, r)
			return
		}
		http.NotFound(w, r)
	})
}
