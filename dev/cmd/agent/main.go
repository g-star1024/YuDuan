// 御端副端参考 Agent 入口（M2 底座 + M4 远控 + M5 分发 + M6 安全基座）。
// 说明：本文件为 Go 参考实现，用于打通协议；生产副端将以 Rust 重写（逻辑等价）。
package main

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"
	"runtime"
	"sync"
	"sync/atomic"
	"time"

	"yuduan/internal/collect"
	"yuduan/internal/crypto"
	"yuduan/internal/proto"
	"yuduan/internal/ws"
)

const agentVersion = "0.1.0"

var seq int64
var hmacSecret string
var hmacEnforce bool
var seenNonces sync.Map // 防重放：已见 nonce

func nextSeq() int64 { return atomic.AddInt64(&seq, 1) }

func main() {
	hubURL := envOr("YUDUAN_HUB", "ws://127.0.0.1:8080/ws/agent")
	hmacSecret = os.Getenv("YUDUAN_HMAC_SECRET")
	hmacEnforce = os.Getenv("YUDUAN_HMAC_ENFORCE") == "1"
	for {
		if err := run(hubURL); err != nil {
			log.Printf("[agent] 连接错误: %v; 5s 后重连", err)
			time.Sleep(5 * time.Second)
		}
	}
}

func run(hubURL string) error {
	conn, err := ws.Dial(hubURL)
	if err != nil {
		return err
	}
	defer conn.Close()

	a := collect.Gather(agentVersion)
	reg := proto.RegisterPayload{
		Hostname: a.Hostname, OS: a.OS, OSVersion: a.OSVersion, Arch: a.Arch,
		CPU: a.CPU, MemoryGB: a.MemoryGB, DiskGB: a.DiskGB, SerialNo: a.SerialNo,
		MAC: a.MAC, IP: a.IP, AgentVersion: a.AgentVersion, Token: os.Getenv("YUDUAN_TOKEN"),
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
	os.Setenv("YUDUAN_TOKEN", ack.Token)
	log.Printf("[agent] 注册成功 device_id=%s host=%s os=%s arch=%s", ack.DeviceID, a.Hostname, a.OS, a.Arch)

	go func() {
		ticker := time.NewTicker(30 * time.Second)
		for range ticker.C {
			hb, _ := proto.NewEnvelope(proto.TypeHeartbeat, nextSeq(), proto.HeartbeatPayload{Load: collect.Load(), IP: a.IP})
			if b, err := json.Marshal(hb); err == nil {
				_ = conn.WriteMessage(b)
			}
		}
	}()

	for {
		raw, err := conn.ReadMessage()
		if err != nil {
			return err
		}
		var env proto.Envelope
		if err := json.Unmarshal(raw, &env); err != nil {
			continue
		}
		switch env.Type {
		case proto.TypeCommand:
			var c proto.CommandPayload
			if err := env.DecodePayload(&c); err != nil {
				continue
			}
			// 安全基座：HMAC 验签（默认不强制，便于兼容未签名的演示链路）
			if !verifyCmd(&env) {
				sendResult(conn, proto.ResultPayload{TaskID: c.TaskID, State: "failed", ExitCode: -1, OutputLog: "HMAC 验签失败或重放被拒"})
				continue
			}
			if c.Action == "sw_install" {
				go handleSwInstall(conn, c)
			} else {
				go handleCommand(conn, c)
			}
		case proto.TypeRCRequest:
			var p proto.RCRequestPayload
			if err := env.DecodePayload(&p); err != nil {
				continue
			}
			log.Printf("[agent] rc request session=%s operator=%s", p.SessionID, p.Operator)
			go startRC(conn, p.SessionID)
		case proto.TypeRCInput:
			var p proto.RCInputPayload
			if err := env.DecodePayload(&p); err != nil {
				continue
			}
			applyInput(conn, p.SessionID, p)
		case proto.TypeRCStop:
			stopRC()
		default:
			// 未知消息类型优雅忽略（如 Rust 副端不识别的远控消息）
		}
	}
}

// verifyCmd 校验指令 HMAC 签名与重放；未启用签名时直接放行。
func verifyCmd(env *proto.Envelope) bool {
	if hmacSecret == "" {
		return true
	}
	valid := crypto.VerifyCommand(hmacSecret, env.Sig, env.MsgID, env.Ts, env.Seq, string(env.Payload), env.Nonce, 300)
	if valid {
		if hmacEnforce {
			if _, seen := seenNonces.LoadOrStore(env.Nonce, true); seen {
				log.Printf("[agent] 拒绝重放指令 nonce=%s", env.Nonce)
				return false
			}
		}
		return true
	}
	if hmacEnforce {
		log.Printf("[agent] 拒绝签名无效指令 msg_id=%s", env.MsgID)
		return false
	}
	log.Printf("[agent] 警告：指令签名无效但未强制拒绝（enforce=off）msg_id=%s", env.MsgID)
	return true
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
	sendResult(conn, res)
}

func sendResult(conn *ws.Conn, res proto.ResultPayload) {
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
