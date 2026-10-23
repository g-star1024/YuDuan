package hub

import (
	"bytes"
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"yuduan/internal/proto"
	"yuduan/internal/ws"
)

func wsURL(srv *httptest.Server, path string) string {
	return "ws" + strings.TrimPrefix(srv.URL, "http") + path
}

// readTypeWithTimeout 在 d 内读取一条消息并返回其 type 字段；超时返回错误（快速失败，避免挂死）。
func readTypeWithTimeout(c *ws.Conn, d time.Duration) (string, error) {
	type res struct {
		typ string
		err error
	}
	ch := make(chan res, 1)
	go func() {
		raw, err := c.ReadMessage()
		if err != nil {
			ch <- res{"", err}
			return
		}
		var env proto.Envelope
		if e := json.Unmarshal(raw, &env); e != nil {
			ch <- res{"", e}
			return
		}
		ch <- res{env.Type, nil}
	}()
	select {
	case r := <-ch:
		return r.typ, r.err
	case <-time.After(d):
		return "", errTimeout
	}
}

var errTimeout = &simpleErr{s: "read timeout"}

// TestRCP2PRelay 验证 P2P 信令 + M4 媒体消息在中枢的中继路由正确：
// Agent→Viewer：rc_ready/rc_frame/rc_event/rc_capabilities/rc_answer/rc_candidate
// Viewer→Agent：rc_request/rc_input/rc_select/rc_offer/rc_candidate
func TestRCP2PRelay(t *testing.T) {
	h := NewHub(HubConfig{})
	srv := httptest.NewServer(h.Handler())
	defer srv.Close()

	// 1) Agent 注册
	aConn, err := ws.Dial(wsURL(srv, "/ws/agent"))
	if err != nil {
		t.Fatalf("agent dial 失败: %v", err)
	}
	defer aConn.Close()
	reg, _ := proto.NewEnvelope(proto.TypeRegister, 0, proto.RegisterPayload{Hostname: "t-agent", OS: "macos", Arch: "arm64"})
	regB, _ := json.Marshal(reg)
	if err := aConn.WriteMessage(regB); err != nil {
		t.Fatalf("写 register 失败: %v", err)
	}
	// 读取 register_ack 拿 device_id
	ackRaw, err := aConn.ReadMessage()
	if err != nil {
		t.Fatalf("读 register_ack 失败: %v", err)
	}
	var ack proto.Envelope
	if err := json.Unmarshal(ackRaw, &ack); err != nil {
		t.Fatalf("解析 ack 失败: %v", err)
	}
	var ackP proto.RegisterAckPayload
	if err := ack.DecodePayload(&ackP); err != nil {
		t.Fatalf("解析 ack payload 失败: %v", err)
	}
	if ackP.DeviceID == "" {
		t.Fatalf("device_id 不应为空")
	}
	deviceID := ackP.DeviceID

	// 2) Viewer 连接并发起 rc_request（建立会话）
	vConn, err := ws.Dial(wsURL(srv, "/ws/viewer"))
	if err != nil {
		t.Fatalf("viewer dial 失败: %v", err)
	}
	defer vConn.Close()
	req, _ := proto.NewEnvelope(proto.TypeRCRequest, 0, proto.RCRequestPayload{
		DeviceID: deviceID, SessionID: "s-1", Operator: "admin",
	})
	reqB, _ := json.Marshal(req)
	if err := vConn.WriteMessage(reqB); err != nil {
		t.Fatalf("写 rc_request 失败: %v", err)
	}
	// 中枢会把 rc_request 转发给 Agent（触发其开启 RC），Agent 侧先消费该消息。
	if typ, err := readTypeWithTimeout(aConn, 2*time.Second); err != nil || typ != proto.TypeRCRequest {
		t.Fatalf("Agent 未收到转发的 rc_request: typ=%q err=%v", typ, err)
	}

	// 3) Viewer → Agent 方向：rc_offer、rc_select、rc_candidate、rc_input 必须送达 Agent
	send := func(conn *ws.Conn, typ string, payload any) {
		env, err := proto.NewEnvelope(typ, 0, payload)
		if err != nil {
			t.Fatalf("构造 %s 失败: %v", typ, err)
		}
		b, _ := json.Marshal(env)
		if err := conn.WriteMessage(b); err != nil {
			t.Fatalf("写 %s 失败: %v", typ, err)
		}
	}
	send(vConn, proto.TypeRCOffer, proto.RCOfferPayload{SessionID: "s-1", Candidates: []string{"10.0.0.5:19001"}})
	if typ, err := readTypeWithTimeout(aConn, 2*time.Second); err != nil || typ != proto.TypeRCOffer {
		t.Fatalf("rc_offer 未中继到 Agent: typ=%q err=%v", typ, err)
	}
	send(vConn, proto.TypeRCSelect, proto.RCSelectPayload{SessionID: "s-1", Codec: proto_codecJPEG(), Transport: "p2p"})
	if typ, err := readTypeWithTimeout(aConn, 2*time.Second); err != nil || typ != proto.TypeRCSelect {
		t.Fatalf("rc_select 未中继到 Agent: typ=%q err=%v", typ, err)
	}
	send(vConn, proto.TypeRCCandidate, proto.RCCandidatePayload{SessionID: "s-1", Candidate: "10.0.0.5:19002"})
	if typ, err := readTypeWithTimeout(aConn, 2*time.Second); err != nil || typ != proto.TypeRCCandidate {
		t.Fatalf("rc_candidate( viewer→agent) 未中继: typ=%q err=%v", typ, err)
	}
	send(vConn, proto.TypeRCInput, proto.RCInputPayload{SessionID: "s-1", Kind: "mouse_move", X: 10, Y: 20})
	if typ, err := readTypeWithTimeout(aConn, 2*time.Second); err != nil || typ != proto.TypeRCInput {
		t.Fatalf("rc_input 未中继到 Agent: typ=%q err=%v", typ, err)
	}
	// §8.3.9 带宽自适应闭环：rc_qos（viewer→agent）必须经中枢中继
	send(vConn, proto.TypeRCQos, proto.RCQosPayload{SessionID: "s-1", FPS: 8, Reason: "decode_slow"})
	if typ, err := readTypeWithTimeout(aConn, 2*time.Second); err != nil || typ != proto.TypeRCQos {
		t.Fatalf("rc_qos 未中继到 Agent: typ=%q err=%v", typ, err)
	}

	// 4) Agent → Viewer 方向：rc_ready/rc_frame/rc_event/rc_capabilities/rc_answer/rc_candidate 必须送达 Viewer
	send(aConn, proto.TypeRCReady, proto.RCReadyPayload{SessionID: "s-1", OK: true})
	if typ, err := readTypeWithTimeout(vConn, 2*time.Second); err != nil || typ != proto.TypeRCReady {
		t.Fatalf("rc_ready 未中继到 Viewer: typ=%q err=%v", typ, err)
	}
	send(aConn, proto.TypeRCFrame, proto.RCFramePayload{SessionID: "s-1", Seq: 1, Width: 100, Height: 100, Format: "jpeg", Data: "AAAA", TS: 1})
	if typ, err := readTypeWithTimeout(vConn, 2*time.Second); err != nil || typ != proto.TypeRCFrame {
		t.Fatalf("rc_frame 未中继到 Viewer: typ=%q err=%v", typ, err)
	}
	send(aConn, proto.TypeRCEvent, proto.RCEventPayload{SessionID: "s-1", Kind: "notice", Message: "x"})
	if typ, err := readTypeWithTimeout(vConn, 2*time.Second); err != nil || typ != proto.TypeRCEvent {
		t.Fatalf("rc_event 未中继到 Viewer: typ=%q err=%v", typ, err)
	}
	send(aConn, proto.TypeRCCapabilities, proto.RCCapabilitiesPayload{SessionID: "s-1", Codecs: []int{0, 1, 2}, MaxW: 1920, MaxH: 1080, Fps: 15})
	if typ, err := readTypeWithTimeout(vConn, 2*time.Second); err != nil || typ != proto.TypeRCCapabilities {
		t.Fatalf("rc_capabilities 未中继到 Viewer: typ=%q err=%v", typ, err)
	}
	send(aConn, proto.TypeRCAnswer, proto.RCAnswerPayload{SessionID: "s-1", Candidates: []string{"10.0.0.9:19001"}})
	if typ, err := readTypeWithTimeout(vConn, 2*time.Second); err != nil || typ != proto.TypeRCAnswer {
		t.Fatalf("rc_answer 未中继到 Viewer: typ=%q err=%v", typ, err)
	}
	send(aConn, proto.TypeRCCandidate, proto.RCCandidatePayload{SessionID: "s-1", Candidate: "10.0.0.9:19002"})
	if typ, err := readTypeWithTimeout(vConn, 2*time.Second); err != nil || typ != proto.TypeRCCandidate {
		t.Fatalf("rc_candidate(agent→viewer) 未中继: typ=%q err=%v", typ, err)
	}
}

// proto_codecJPEG 返回 JPEG 编解码器 ID（与 Rust rc_media.CODEC_JPEG 对齐）。
func proto_codecJPEG() int { return 1 }

// readTypedWithTimeout 在 d 内读取一条消息并返回（载荷, 是否二进制）；超时返回错误。
func readTypedWithTimeout(c *ws.Conn, d time.Duration) ([]byte, bool, error) {
	type res struct {
		data    []byte
		isBinary bool
		err     error
	}
	ch := make(chan res, 1)
	go func() {
		data, isBinary, err := c.ReadMessageTyped()
		ch <- res{data, isBinary, err}
	}()
	select {
	case r := <-ch:
		return r.data, r.isBinary, r.err
	case <-time.After(d):
		return nil, false, errTimeout
	}
}

// TestRCBinaryFrameRelay 验证二进制媒体帧经中枢双向透传（§8.3）：
// Agent→Viewer 二进制帧逐字节一致且 isBinary=true；Viewer→Agent 同。
func TestRCBinaryFrameRelay(t *testing.T) {
	h := NewHub(HubConfig{})
	srv := httptest.NewServer(h.Handler())
	defer srv.Close()

	// 1) Agent 注册
	aConn, err := ws.Dial(wsURL(srv, "/ws/agent"))
	if err != nil {
		t.Fatalf("agent dial 失败: %v", err)
	}
	defer aConn.Close()
	reg, _ := proto.NewEnvelope(proto.TypeRegister, 0, proto.RegisterPayload{Hostname: "t-agent", OS: "macos", Arch: "arm64"})
	regB, _ := json.Marshal(reg)
	if err := aConn.WriteMessage(regB); err != nil {
		t.Fatalf("写 register 失败: %v", err)
	}
	ackRaw, err := aConn.ReadMessage()
	if err != nil {
		t.Fatalf("读 register_ack 失败: %v", err)
	}
	var ack proto.Envelope
	if err := json.Unmarshal(ackRaw, &ack); err != nil {
		t.Fatalf("解析 ack 失败: %v", err)
	}
	var ackP proto.RegisterAckPayload
	if err := ack.DecodePayload(&ackP); err != nil {
		t.Fatalf("解析 ack payload 失败: %v", err)
	}
	deviceID := ackP.DeviceID

	// 2) Viewer 建会话（rc_request 会被中枢转发给 Agent，先消费）
	vConn, err := ws.Dial(wsURL(srv, "/ws/viewer"))
	if err != nil {
		t.Fatalf("viewer dial 失败: %v", err)
	}
	defer vConn.Close()
	req, _ := proto.NewEnvelope(proto.TypeRCRequest, 0, proto.RCRequestPayload{
		DeviceID: deviceID, SessionID: "s-bin", Operator: "admin",
	})
	reqB, _ := json.Marshal(req)
	if err := vConn.WriteMessage(reqB); err != nil {
		t.Fatalf("写 rc_request 失败: %v", err)
	}
	if _, err := aConn.ReadMessage(); err != nil {
		t.Fatalf("Agent 未收到转发的 rc_request: %v", err)
	}

	// 3) Agent → Viewer：二进制媒体帧（构造 §8.3 帧：magic 'YDYF' + 20B 头余部 + JPEG SOI 负载）
	frame := make([]byte, 0, 64)
	frame = append(frame, 0x46, 0x59, 0x44, 0x59, 0x01, 0x00, 0x01, 0x00) // magic/ver/stream/codec/flags
	frame = append(frame, 0x07, 0x00, 0x00, 0x00)                         // seq=7
	frame = append(frame, 0x40, 0x01, 0x20, 0x01)                         // w=320 h=288
	frame = append(frame, 0xE8, 0x03, 0x00, 0x00)                         // pts_ms=1000
	frame = append(frame, 0x04, 0x00, 0x00, 0x00)                         // len=4
	frame = append(frame, 0xFF, 0xD8, 0xFF, 0xD9)                         // JPEG SOI+EOI
	if err := aConn.WriteBinary(frame); err != nil {
		t.Fatalf("Agent WriteBinary 失败: %v", err)
	}
	got, isBinary, err := readTypedWithTimeout(vConn, 2*time.Second)
	if err != nil {
		t.Fatalf("Viewer 未收到二进制帧: %v", err)
	}
	if !isBinary {
		t.Fatalf("中继后应为二进制帧，实际 isBinary=false")
	}
	if !bytes.Equal(got, frame) {
		t.Fatalf("二进制帧中继后不一致: got=%d want=%d 字节", len(got), len(frame))
	}

	// 4) Viewer → Agent：二进制帧反向透传
	if err := vConn.WriteBinary(frame); err != nil {
		t.Fatalf("Viewer WriteBinary 失败: %v", err)
	}
	got2, isBinary2, err := readTypedWithTimeout(aConn, 2*time.Second)
	if err != nil {
		t.Fatalf("Agent 未收到 Viewer 二进制帧: %v", err)
	}
	if !isBinary2 || !bytes.Equal(got2, frame) {
		t.Fatalf("Viewer→Agent 二进制中继失败: isBinary=%v equal=%v", isBinary2, bytes.Equal(got2, frame))
	}
}
