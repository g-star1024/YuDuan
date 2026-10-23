// Command p2pviewer 是 P2P 二进制媒体环（protocol.md §8.3）的端到端验证 viewer（参考实现）。
//
// 以主端身份经中枢建立远控会话、接收能力宣告并发送 rc_select 协商，随后按 transport
// 收取二进制媒体帧：
//   - p2p：监听媒体端口，接收被控端 dial_direct 直连帧（4B 长度前缀分帧）；
//   - relay：经中枢 viewer 长连接收取二进制帧中继（P2P 不可达时的兜底路径）。
// 校验帧格式（magic/version/头字段）与 JPEG SOI，落盘样例帧并输出统计。
// 用途：CI/联调验证远控媒体运行时，不面向终端用户。
package main

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"time"

	"yuduan/internal/proto"
	"yuduan/internal/ws"
)

const (
	frameMagic     = 0x59445946 // 'YDYF' 小端（与 Rust rc_media.FRAME_MAGIC 一致）
	frameVersion   = 1
	frameHeaderLen = 24
	codecJPEG      = 1
)

func main() {
	hub := flag.String("hub", "ws://127.0.0.1:18080", "中枢 viewer WS 地址（不含路径）")
	device := flag.String("device", "", "被控设备 ID（必填）")
	transport := flag.String("transport", "p2p", "p2p 直连 / relay 中继")
	codec := flag.Int("codec", codecJPEG, "协商编解码器 ID（1=jpeg）")
	media := flag.String("media", "127.0.0.1:19500", "p2p 媒体监听地址（被控端 dial 此地址）")
	selectAddr := flag.String("selectaddr", "", "rc_select 携带的媒体地址（默认同 -media；设为不可达地址可验证 P2P 失败后的中继回退）")
	frames := flag.Int("frames", 5, "收集满 N 帧即结束")
	out := flag.String("out", "/tmp/p2pviewer-out", "样例帧输出目录")
	timeout := flag.Duration("timeout", 25*time.Second, "总体超时")
	flag.Parse()
	if *device == "" {
		fatal("需指定 -device（被控设备 ID）")
	}
	if *transport != "p2p" && *transport != "relay" {
		fatal("-transport 仅支持 p2p / relay")
	}

	sessionID := fmt.Sprintf("s-%d", time.Now().UnixNano())

	// p2p：先监听（被控端收到 rc_select 才会 dial，监听必须先就绪）。
	var ln net.Listener
	if *transport == "p2p" {
		var err error
		ln, err = net.Listen("tcp", *media)
		if err != nil {
			fatal("P2P 媒体监听失败: %v", err)
		}
		defer ln.Close()
		logf("P2P 媒体监听 %s（等待被控端直连）", ln.Addr())
	}

	conn, err := ws.Dial(*hub + "/ws/viewer")
	if err != nil {
		fatal("viewer WS 连接失败: %v", err)
	}
	defer conn.Close()

	// 1) rc_request 建立会话（中枢据此登记 Viewer 并转发给被控端）
	req, err := proto.NewEnvelope(proto.TypeRCRequest, 0, proto.RCRequestPayload{
		DeviceID: *device, SessionID: sessionID, Operator: "p2pviewer",
	})
	if err != nil {
		fatal("构造 rc_request 失败: %v", err)
	}
	reqB, _ := json.Marshal(req)
	if err := conn.WriteMessage(reqB); err != nil {
		fatal("发送 rc_request 失败: %v", err)
	}
	logf("已发送 rc_request device=%s session=%s transport=%s codec=%d", *device, sessionID, *transport, *codec)

	// 2) 读循环：文本信封 → evCh；二进制帧 → frameCh
	type envelope = proto.Envelope
	evCh := make(chan envelope, 32)
	frameCh := make(chan []byte, 64)
	go func() {
		for {
			d, isBinary, err := conn.ReadMessageTyped()
			if err != nil {
				close(evCh)
				close(frameCh)
				return
			}
			if isBinary {
				frameCh <- d
				continue
			}
			var env envelope
			if json.Unmarshal(d, &env) == nil {
				evCh <- env
			}
		}
	}()

	// p2p：accept 被控端直连后按 4B 长度前缀读帧
	if ln != nil {
		go func() {
			c, err := ln.Accept()
			if err != nil {
				logf("P2P accept 失败: %v", err)
				return
			}
			defer c.Close()
			logf("P2P 直连建立: %s", c.RemoteAddr())
			for {
				var lenBuf [4]byte
				if _, err := io.ReadFull(c, lenBuf[:]); err != nil {
					logf("P2P 连接读取结束: %v", err)
					return
				}
				n := binary.LittleEndian.Uint32(lenBuf[:])
				buf := make([]byte, n)
				if _, err := io.ReadFull(c, buf); err != nil {
					logf("P2P 帧体读取失败: %v", err)
					return
				}
				frameCh <- buf
			}
		}()
	}

	// 3) 主循环：等能力宣告→发协商；收帧→校验+落盘；收满即止
	deadline := time.Now().Add(*timeout)
	started := time.Time{}
	got, totalBytes := 0, 0
	selectSent := false

	for got < *frames {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			fatal("超时：仅收到 %d/%d 帧（transport=%s）", got, *frames, *transport)
		}
		select {
		case env, ok := <-evCh:
			if !ok {
				fatal("viewer 连接关闭（仅 %d/%d 帧）", got, *frames)
			}
			switch env.Type {
			case proto.TypeRCReady:
				logf("rc_ready ok=%v", payloadOK(env))
			case proto.TypeRCCapabilities:
				var p proto.RCCapabilitiesPayload
				_ = env.DecodePayload(&p)
				logf("能力宣告: codecs=%v %dx%d@%dfps", p.Codecs, p.MaxW, p.MaxH, p.Fps)
				if !selectSent {
					addr := *media
					if *selectAddr != "" {
						addr = *selectAddr
					}
					// 协商：p2p 带上媒体监听地址；relay 无需地址
					sel, err := proto.NewEnvelope(proto.TypeRCSelect, 0, proto.RCSelectPayload{
						SessionID: sessionID, Codec: *codec, Transport: *transport, MediaAddr: addr,
					})
					if err != nil {
						fatal("构造 rc_select 失败: %v", err)
					}
					selB, _ := json.Marshal(sel)
					if err := conn.WriteMessage(selB); err != nil {
						fatal("发送 rc_select 失败: %v", err)
					}
					selectSent = true
					logf("已发送 rc_select（transport=%s codec=%d media=%s）", *transport, *codec, addr)
				}
			case proto.TypeRCEvent:
				var p proto.RCEventPayload
				_ = env.DecodePayload(&p)
				logf("事件[%s]: %s", p.Kind, p.Message)
				if p.Kind == "error" {
					fatal("被控端报错: %s", p.Message)
				}
			case proto.TypeRCStop:
				fatal("会话被结束（仅 %d/%d 帧）", got, *frames)
			}
		case frame, ok := <-frameCh:
			if !ok {
				fatal("帧通道关闭（仅 %d/%d 帧）", got, *frames)
			}
			codecGot, payload, err := checkFrame(frame, *codec)
			if err != nil {
				logf("丢弃无效帧: %v", err)
				continue
			}
			if got == 0 {
				started = time.Now()
			}
			got++
			totalBytes += len(payload)
			saved := saveFrame(*out, got, codecGot, payload)
			logf("帧 #%d ok: codec=%s %d 字节载荷 → %s", got, codecName(codecGot), len(payload), saved)
		}
	}

	elapsed := time.Since(started)
	// 4) 结束会话（rc_stop 载荷仅携带 session_id，中枢/被控端均不强制解析）
	stop, err := proto.NewEnvelope(proto.TypeRCStop, 0, map[string]string{"session_id": sessionID})
	if err != nil {
		fatal("构造 rc_stop 失败: %v", err)
	}
	stopB, _ := json.Marshal(stop)
	_ = conn.WriteMessage(stopB)

	fmt.Printf("P2PVIEWER_OK frames=%d transport=%s codec=%s bytes=%d elapsed=%s avg_fps=%.1f\n",
		got, *transport, codecName(byte(*codec)), totalBytes,
		elapsed.Round(time.Millisecond), float64(got)/elapsed.Seconds())
}

// checkFrame 校验二进制媒体帧格式；wantCodec>0 时还要求编解码器与协商值一致。
func checkFrame(b []byte, wantCodec int) (byte, []byte, error) {
	if len(b) < frameHeaderLen {
		return 0, nil, fmt.Errorf("帧过短 %d < %d", len(b), frameHeaderLen)
	}
	if binary.LittleEndian.Uint32(b[0:4]) != frameMagic {
		return 0, nil, errors.New("magic 不匹配")
	}
	if b[4] != frameVersion {
		return 0, nil, fmt.Errorf("帧版本 %d 不支持", b[4])
	}
	codec := b[6]
	ln := int(binary.LittleEndian.Uint32(b[20:24]))
	if len(b) < frameHeaderLen+ln {
		return 0, nil, fmt.Errorf("载荷截断: 声明 %d 实际 %d", ln, len(b)-frameHeaderLen)
	}
	payload := b[frameHeaderLen : frameHeaderLen+ln]
	if wantCodec > 0 && int(codec) != wantCodec {
		return 0, nil, fmt.Errorf("编解码器 %d 非协商值 %d", codec, wantCodec)
	}
	if codec == codecJPEG && (len(payload) < 2 || payload[0] != 0xFF || payload[1] != 0xD8) {
		return 0, nil, errors.New("JPEG 载荷缺 SOI 标记")
	}
	return codec, payload, nil
}

func saveFrame(dir string, n int, codec byte, payload []byte) string {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		logf("建目录失败: %v", err)
		return "(未落盘)"
	}
	ext := ".bin"
	if codec == codecJPEG {
		ext = ".jpg"
	}
	path := filepath.Join(dir, fmt.Sprintf("frame-%03d%s", n, ext))
	if err := os.WriteFile(path, payload, 0o644); err != nil {
		logf("落盘失败: %v", err)
		return "(未落盘)"
	}
	return path
}

func payloadOK(env proto.Envelope) any {
	var p proto.RCReadyPayload
	if err := env.DecodePayload(&p); err != nil {
		return "?"
	}
	return p.OK
}

func codecName(c byte) string {
	names := []string{"raw_rgba", "jpeg", "h264", "h265", "vp8", "vp9", "av1"}
	if int(c) < len(names) {
		return names[c]
	}
	return fmt.Sprintf("codec:%d", c)
}

func logf(format string, args ...any) {
	fmt.Printf("[p2pviewer %s] %s\n", time.Now().Format("15:04:05"), fmt.Sprintf(format, args...))
}

func fatal(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "P2PVIEWER_FAIL: "+format+"\n", args...)
	os.Exit(1)
}
