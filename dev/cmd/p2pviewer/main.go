// Command p2pviewer 是 P2P 二进制媒体环（protocol.md §8.3）的端到端验证 viewer（参考实现）。
//
// 以主端身份经中枢建立远控会话、接收能力宣告并发送 rc_select 协商，随后按 transport
// 收取二进制媒体帧：
//   - p2p：监听媒体端口，接收被控端 dial_direct 直连帧（4B 长度前缀分帧）；
//   - relay：经中枢 viewer 长连接收取二进制帧中继（P2P 不可达时的兜底路径）。
//
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
	"strings"
	"time"

	"yuduan/internal/proto"
	"yuduan/internal/ws"
)

const (
	frameMagic     = 0x59445946 // 'YDYF' 小端（与 Rust rc_media.FRAME_MAGIC 一致）
	frameVersion   = 1
	frameHeaderLen = 24
	codecJPEG      = 1
	codecRawRGBA   = 0
	codecH264      = 2
	codecH265      = 3
)

var codecNames = []string{"raw_rgba", "jpeg", "h264", "h265", "vp8", "vp9", "av1"}

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
	candidates := flag.Bool("candidates", true, "p2p 时广播本机候选地址（ICE-lite：被控端逐个尝试直连）")
	qos := flag.Int("qos", 0, "协商后每 2s 向被控端发送 rc_qos 目标帧率（0=关闭；验证 §8.3.9 带宽自适应闭环）")
	probe := flag.Bool("probe", false, "每 1s 发 rc_ping 探测 RTT（§8.3.11），并按 RTT 档位向被控端反馈帧率上限")
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
	codecAdopted := false
	lastQosByRtt := time.Time{} // §8.3.11：上次按 RTT 反馈的时刻（节流 2s）

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
					// ICE-lite：广播本机候选地址（被控端首跳失败时逐个尝试直连升级）。
					if *transport == "p2p" && *candidates {
						sendCandidates(conn, sessionID, ln)
					}
					// §8.3.9 带宽自适应闭环：反馈目标帧率（被控端逐轮应用，会话中途升降生效）。
					// 首条立即发送（等 2s 的话低速用例收满前反馈根本到不了），此后每 2s 一次。
					if *qos > 0 {
						go func() {
							for i := 0; ; i++ {
								if i > 0 {
									time.Sleep(2 * time.Second)
								}
								q, err := proto.NewEnvelope(proto.TypeRCQos, 0, proto.RCQosPayload{
									SessionID: sessionID, FPS: *qos, Reason: "p2pviewer-feedback",
								})
								if err != nil {
									return
								}
								qB, _ := json.Marshal(q)
								if err := conn.WriteMessage(qB); err != nil {
									return
								}
								logf("已发送 rc_qos fps=%d", *qos)
							}
						}()
					}
					// §8.3.11 传输级自适应：周期 RTT 探测；RTT>80ms 时按档位反馈帧率上限，
					// 让被控端在不依赖渲染统计的情况下感知网络拥塞。
					if *probe {
						go func() {
							for {
								time.Sleep(1 * time.Second)
								p, err := proto.NewEnvelope(proto.TypeRCPing, 0, proto.RCPingPayload{
									SessionID: sessionID, T0: time.Now().UnixMilli(),
								})
								if err != nil {
									return
								}
								pB, _ := json.Marshal(p)
								if err := conn.WriteMessage(pB); err != nil {
									return
								}
							}
						}()
					}
				}
			case proto.TypeRCPong:
				var p proto.RCPingPayload
				_ = env.DecodePayload(&p)
				rtt := time.Now().UnixMilli() - p.T0
				logf("RTT 探测: %d ms", rtt)
				// RTT 越过档位（>80ms）→ 立即反馈一次上限（副端 min(目标, 档位) 生效）
				if *probe && rtt > 80 && time.Since(lastQosByRtt) > 2*time.Second {
					cap := 15
					if rtt > 400 {
						cap = 4
					} else if rtt > 200 {
						cap = 8
					}
					q, err := proto.NewEnvelope(proto.TypeRCQos, 0, proto.RCQosPayload{
						SessionID: sessionID, FPS: cap, Reason: "rtt_probe", RTTMs: rtt,
					})
					if err == nil {
						qB, _ := json.Marshal(q)
						if err := conn.WriteMessage(qB); err == nil {
							logf("已发送 rc_qos（RTT %dms → 上限 %dfps）", rtt, cap)
							lastQosByRtt = time.Now()
						}
					}
				}
			case proto.TypeRCEvent:
				var p proto.RCEventPayload
				_ = env.DecodePayload(&p)
				logf("事件[%s]: %s", p.Kind, p.Message)
				if p.Kind == "error" {
					fatal("被控端报错: %s", p.Message)
				}
				// 被控端可能诚实回退编解码器（如选 h265 但无原生库 → jpeg）：
				// 从 media_selected 事件末尾的 codec 名采纳实际值，避免误判有效帧。
				if p.Kind == "media_selected" && !codecAdopted {
					if name := lastToken(p.Message); name != "" {
						for i, n := range codecNames {
							if n == name {
								*codec = i
								codecAdopted = true
								logf("采纳被控端实际编解码器: %s", name)
								break
							}
						}
					}
				}
			case proto.TypeRCStop:
				fatal("会话被结束（仅 %d/%d 帧）", got, *frames)
			}
		case frame, ok := <-frameCh:
			if !ok {
				fatal("帧通道关闭（仅 %d/%d 帧）", got, *frames)
			}
			codecGot, fw, fh, payload, err := checkFrame(frame, *codec)
			if err != nil {
				logf("丢弃无效帧: %v", err)
				continue
			}
			if got == 0 {
				started = time.Now()
				logf("首帧头 8 字节: % x（%s 帧用于校验编码器输出特征）", payload[:min(8, len(payload))], codecName(codecGot))
				// 首帧 = 关键帧：H.264 首段应为 SPS(7)，HEVC 应为 VPS(32)
				if int(codecGot) == codecH264 || int(codecGot) == codecH265 {
					if err := checkAnnexB(payload, codecGot); err != nil {
						fatal("首帧 Annex-B/参数集校验失败: %v", err)
					}
					logf("首帧 Annex-B 校验通过（起始码 + 参数集在位）")
				}
			}
			got++
			totalBytes += len(payload)
			saved := saveFrame(*out, got, codecGot, payload)
			logf("帧 #%d ok: codec=%s %dx%d %d 字节载荷 → %s", got, codecName(codecGot), fw, fh, len(payload), saved)
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
// 返回（编解码器, 宽, 高, payload）。RAW_RGBA 帧额外校验载荷长度 == 宽*高*4。
func checkFrame(b []byte, wantCodec int) (byte, int, int, []byte, error) {
	if len(b) < frameHeaderLen {
		return 0, 0, 0, nil, fmt.Errorf("帧过短 %d < %d", len(b), frameHeaderLen)
	}
	if binary.LittleEndian.Uint32(b[0:4]) != frameMagic {
		return 0, 0, 0, nil, errors.New("magic 不匹配")
	}
	if b[4] != frameVersion {
		return 0, 0, 0, nil, fmt.Errorf("帧版本 %d 不支持", b[4])
	}
	codec := b[6]
	w := int(binary.LittleEndian.Uint16(b[12:14]))
	h := int(binary.LittleEndian.Uint16(b[14:16]))
	ln := int(binary.LittleEndian.Uint32(b[20:24]))
	if len(b) < frameHeaderLen+ln {
		return 0, 0, 0, nil, fmt.Errorf("载荷截断: 声明 %d 实际 %d", ln, len(b)-frameHeaderLen)
	}
	payload := b[frameHeaderLen : frameHeaderLen+ln]
	if wantCodec > 0 && int(codec) != wantCodec {
		return 0, 0, 0, nil, fmt.Errorf("编解码器 %d 非协商值 %d", codec, wantCodec)
	}
	if codec == codecJPEG && (len(payload) < 2 || payload[0] != 0xFF || payload[1] != 0xD8) {
		return 0, 0, 0, nil, errors.New("JPEG 载荷缺 SOI 标记")
	}
	if int(codec) == codecH264 || int(codec) == codecH265 {
		if err := checkAnnexB(payload, 0); err != nil {
			return 0, 0, 0, nil, fmt.Errorf("%s 载荷非合法 Annex-B: %v", codecNames[codec], err)
		}
	}
	if codec == codecRawRGBA {
		if w == 0 || h == 0 {
			return 0, 0, 0, nil, errors.New("RAW 帧宽高为 0")
		}
		if len(payload) != w*h*4 {
			return 0, 0, 0, nil, fmt.Errorf("RAW 载荷长度 %d != %dx%dx4", len(payload), w, h)
		}
	}
	return codec, w, h, payload, nil
}

// sendCandidates 广播 ICE-lite 候选：
//   - 监听绑定具体 IP（如 127.0.0.1）→ 唯一候选即该地址（拨别的地址必然被拒）；
//   - 监听绑定通配地址（0.0.0.0 / ::）→ 枚举本机非回送网卡 IPv4 + 监听端口。
//
// 先发 rc_offer（批量），再逐个 rc_candidate（增量，与协议 §8.3 一致）。
func sendCandidates(conn *ws.Conn, sessionID string, ln net.Listener) {
	host, port, err := net.SplitHostPort(ln.Addr().String())
	if err != nil {
		logf("候选收集失败（监听地址解析）: %v", err)
		return
	}
	var cands []string
	if ip := net.ParseIP(host); ip != nil && !ip.IsUnspecified() {
		cands = []string{net.JoinHostPort(ip.String(), port)}
	} else {
		ifaces, err := net.Interfaces()
		if err != nil {
			logf("候选收集失败（枚举网卡）: %v", err)
			return
		}
		for _, ifc := range ifaces {
			if ifc.Flags&net.FlagUp == 0 || ifc.Flags&net.FlagLoopback != 0 {
				continue
			}
			addrs, err := ifc.Addrs()
			if err != nil {
				continue
			}
			for _, a := range addrs {
				var ip net.IP
				switch v := a.(type) {
				case *net.IPNet:
					ip = v.IP
				case *net.IPAddr:
					ip = v.IP
				}
				if ip == nil || ip.IsLoopback() || ip.To4() == nil {
					continue // 仅 IPv4（媒体环当前按 IPv4 拨号）
				}
				cands = append(cands, net.JoinHostPort(ip.String(), port))
			}
		}
	}
	if len(cands) == 0 {
		return
	}
	offer, err := proto.NewEnvelope(proto.TypeRCOffer, 0, proto.RCOfferPayload{SessionID: sessionID, Candidates: cands})
	if err == nil {
		if b, err := json.Marshal(offer); err == nil {
			_ = conn.WriteMessage(b)
		}
	}
	for _, c := range cands {
		cm, err := proto.NewEnvelope(proto.TypeRCCandidate, 0, proto.RCCandidatePayload{SessionID: sessionID, Candidate: c})
		if err != nil {
			continue
		}
		if b, err := json.Marshal(cm); err == nil {
			_ = conn.WriteMessage(b)
		}
	}
	logf("已广播 %d 个 ICE-lite 候选: %v", len(cands), cands)
}

func saveFrame(dir string, n int, codec byte, payload []byte) string {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		logf("建目录失败: %v", err)
		return "(未落盘)"
	}
	ext := ".bin"
	if int(codec) < len(codecNames) {
		ext = "." + codecNames[codec]
	}
	path := filepath.Join(dir, fmt.Sprintf("frame-%03d%s", n, ext))
	if err := os.WriteFile(path, payload, 0o644); err != nil {
		logf("落盘失败: %v", err)
		return "(未落盘)"
	}
	return path
}

// lastToken 取消息末尾以空白分隔的最后一个词（media_selected 消息以 codec 名结尾）。
func lastToken(s string) string {
	f := strings.Fields(s)
	if len(f) == 0 {
		return ""
	}
	return f[len(f)-1]
}

// checkAnnexB 校验 H.264/H.265 裸流：Annex-B 起始码切分 NAL，逐段非空，
// 且首 NAL 类型符合关键帧要求（H.264 首段应为 SPS(type 7)；HEVC 应为 VPS(type 32)）。
func checkAnnexB(p []byte, codec byte) error {
	if len(p) < 5 {
		return errors.New("载荷过短")
	}
	var sc0 int
	if p[0] == 0 && p[1] == 0 && p[2] == 0 && p[3] == 1 {
		sc0 = 4
	} else if p[0] == 0 && p[1] == 0 && p[2] == 1 {
		sc0 = 3
	} else {
		return errors.New("缺起始码 00 00 00 01")
	}
	type seg struct{ start, end int }
	var segs []seg
	prev := sc0
	for i := sc0; i+2 < len(p); i++ { // 从首起始码之后开始扫（避免把 4 字节起始码内部的 00 00 01 误判为新起始码）
		if p[i] == 0 && p[i+1] == 0 && p[i+2] == 1 {
			startLen := 3
			end := i
			if p[i-1] == 0 { // 4 字节起始码 00 00 00 01
				end--
				startLen = 4
			}
			if end <= prev {
				return fmt.Errorf("偏移 %d 处空 NAL", prev)
			}
			segs = append(segs, seg{prev, end})
			prev = i + startLen
			i += startLen - 1
		}
	}
	if len(p) <= prev {
		return errors.New("末尾空 NAL")
	}
	segs = append(segs, seg{prev, len(p)})
	if len(segs) == 0 {
		return errors.New("无 NAL")
	}
	// 首 NAL 类型
	head := p[segs[0].start]
	switch {
	case codec == codecH264 && head&0x1F != 7:
		return fmt.Errorf("首 NAL 类型 %d 非 SPS(7)（关键帧应携带参数集）", head&0x1F)
	case codec == codecH265 && (head>>1)&0x3F != 32:
		return fmt.Errorf("首 NAL 类型 %d 非 VPS(32)（关键帧应携带参数集）", (head>>1)&0x3F)
	}
	return nil
}

func payloadOK(env proto.Envelope) any {
	var p proto.RCReadyPayload
	if err := env.DecodePayload(&p); err != nil {
		return "?"
	}
	return p.OK
}

func codecName(c byte) string {
	if int(c) < len(codecNames) {
		return codecNames[c]
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
