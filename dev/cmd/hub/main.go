// 御端服务端中枢入口（M2 底座联调）。
package main

import (
	"log"
	"net/http"

	"yuduan/internal/hub"
)

func main() {
	h := hub.NewHub()
	go h.StartReaper()
	addr := ":8080"
	log.Printf("[hub] 御端中枢监听 %s (REST /api/v1/* + WS /ws/agent + 控制台 /)", addr)
	log.Fatal(http.ListenAndServe(addr, h.Handler()))
}
