// 御端副端参考 Agent 入口（M2 底座联调）。
// 说明：本文件为 Go 参考实现，用于打通协议；生产副端将按铁律以 Rust 重写（逻辑等价）。
package main

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"
	"runtime"
	"sync/atomic"
	"time"

	"yuduan/internal/collect"
	"yuduan/internal/proto"
	"yuduan/internal/ws"
)

const agentVersion = "0.1.0"

var seq int64

func nextSeq() int64 { return atomic.AddInt64(&seq, 1) }

func main() {
	hubURL := envOr("YUDUAN_HUB", "ws://127.0.0.1:8080/ws/agent")
	token := os.Getenv("YUDUAN_TOKEN")
	for {
		if err := run(hubURL, token); err != nil {
			log.Printf("[agent] 连接错误: %v; 5s 后重连", err)
			time.Sleep(5 * time.Second)
		}
	}
}

func run(hubURL, token string) error {
	conn, err := ws.Dial(hubURL)
	if err != nil {
		return err
	}
	defer conn.Close()

	a := collect.Gather(agentVersion)
	reg := proto.RegisterPayload{
		Hostname:     a.Hostname,
		OS:           a.OS,
		OSVersion:    a.OSVersion,
		Arch:         a.Arch,
		CPU:          a.CPU,
		MemoryGB:     a.MemoryGB,
		DiskGB:       a.DiskGB,
		SerialNo:     a.SerialNo,
		MAC:          a.MAC,
		IP:           a.IP,
		AgentVersion: a.AgentVersion,
		Token:        token,
	}
	if env, err := proto.NewEnvelope(proto.TypeRegister, nextSeq(), reg); err == nil {
		if b, err := json.Marshal(env); err == nil {
			_ = conn.WriteMessage(b)
		}
	}

	raw, err := conn.ReadMessage()
	if err != nil {
		return err
	}
	var ackEnv proto.Envelope
	if err := json.Unmarshal(raw, &ackEnv); err != nil {
		return err
	}
	var ack proto.RegisterAckPayload
	if err := ackEnv.DecodePayload(&ack); err != nil || !ack.OK {
		return fmt.Errorf("register rejected")
	}
	token = ack.Token
	log.Printf("[agent] 注册成功 device_id=%s host=%s os=%s arch=%s", ack.DeviceID, a.Hostname, a.OS, a.Arch)

	// 心跳
	go func() {
		ticker := time.NewTicker(30 * time.Second)
		for range ticker.C {
			hb, _ := proto.NewEnvelope(proto.TypeHeartbeat, nextSeq(), proto.HeartbeatPayload{Load: collect.Load(), IP: a.IP})
			if b, err := json.Marshal(hb); err == nil {
				_ = conn.WriteMessage(b)
			}
		}
	}()

	// 指令循环
	for {
		raw, err := conn.ReadMessage()
		if err != nil {
			return err
		}
		var env proto.Envelope
		if err := json.Unmarshal(raw, &env); err != nil {
			continue
		}
		if env.Type == proto.TypeCommand {
			var c proto.CommandPayload
			if err := env.DecodePayload(&c); err != nil {
				continue
			}
			go handleCommand(conn, c)
		}
	}
}

func handleCommand(conn *ws.Conn, c proto.CommandPayload) {
	res := proto.ResultPayload{TaskID: c.TaskID}
	switch c.Action {
	case "shell":
		out, code := runShell(c.Command)
		res.State = "success"
		if code != 0 {
			res.State = "failed"
		}
		res.ExitCode = code
		res.OutputLog = out
	case "collect":
		a := collect.Gather(agentVersion)
		res.State = "success"
		res.OutputLog = fmt.Sprintf("re-collected: host=%s os=%s mem=%dGB disk=%dGB cpu=%s", a.Hostname, a.OS, a.MemoryGB, a.DiskGB, a.CPU)
	default:
		res.State = "failed"
		res.OutputLog = "unknown action: " + c.Action
	}
	if env, err := proto.NewEnvelope(proto.TypeResult, nextSeq(), res); err == nil {
		if b, err := json.Marshal(env); err == nil {
			_ = conn.WriteMessage(b)
		}
	}
}

func runShell(command string) (string, int) {
	var cmd *exec.Cmd
	if runtime.GOOS == "windows" {
		cmd = exec.Command("cmd", "/c", command)
	} else {
		cmd = exec.Command("sh", "-c", command)
	}
	out, err := cmd.CombinedOutput()
	code := 0
	if err != nil {
		if ee, ok := err.(*exec.ExitError); ok {
			code = ee.ExitCode()
		} else {
			code = -1
		}
	}
	return string(out), code
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}
