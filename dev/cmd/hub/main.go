// 御端服务端中枢入口（M2 底座联调 + M4 远控 + M5 软件分发）。
package main

import (
	"log"
	"net/http"
	"os"

	"yuduan/internal/hub"
)

func main() {
	repoDir := os.Getenv("YUDUAN_REPO")
	if repoDir == "" {
		repoDir = "data/repo"
	}
	h := hub.NewHub(repoDir)
	go h.StartReaper()
	addr := envOr("YUDUAN_HUB_ADDR", ":8080")
	log.Printf("[hub] 御端中枢监听 %s (REST /api/v1/* + WS /ws/agent,/ws/viewer + 控制台 /)", addr)
	log.Printf("[hub] 软件仓库目录: %s", repoDir)
	log.Fatal(http.ListenAndServe(addr, h.Handler()))
}

func envOr(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}
