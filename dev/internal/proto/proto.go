// Package proto 定义御端控制面消息信封与各类载荷，对齐 dev/proto/protocol.md（M1 冻结 v1）。
package proto

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
)

// Ver 为当前协议版本，与 protocol.md 保持一致。
const Ver = 1

// Envelope 是 WebSocket 控制消息的统一信封（JSON）。
// 关键指令由服务端签名（Sig），Agent 验签；心跳/回执类携带有效令牌即可。
// Nonce 用于防重放，每条消息唯一。
type Envelope struct {
	Ver     int             `json:"ver"`
	MsgID   string          `json:"msg_id"`
	Ts      int64           `json:"ts"`
	Type    string          `json:"type"`
	Seq     int64           `json:"seq"`
	Payload json.RawMessage `json:"payload"`
	Nonce   string          `json:"nonce,omitempty"`
	Sig     string          `json:"sig,omitempty"`
}

// 消息类型，与 protocol.md §3 对齐。
const (
	TypeRegister    = "register"
	TypeRegisterAck = "register_ack"
	TypeHeartbeat   = "heartbeat"
	TypeCommand     = "command"
	TypeAck         = "ack"
	TypeResult      = "result"
	TypeEvent       = "event"

	// —— M4 远程控制 ——
	TypeRCRequest = "rc_request" // Viewer→Hub / Hub→Agent：请求开启远控会话
	TypeRCReady   = "rc_ready"   // Agent→Hub→Viewer：被控端就绪
	TypeRCFrame   = "rc_frame"   // Agent→Hub→Viewer：屏幕帧（JPEG base64）
	TypeRCInput   = "rc_input"   // Viewer→Hub→Agent：键鼠输入
	TypeRCStop    = "rc_stop"    // 任一端→对端：结束会话
	TypeRCEvent   = "rc_event"   // Agent→Hub→Viewer：合规提示/员工确认事件

	// —— M5 软件分发 ——
	// sw_install 复用 TypeCommand(action="sw_install")，载荷见 CommandPayload 扩展字段
)

// RegisterPayload —— Agent→Hub 的纳管注册信息（首次/重连）。
type RegisterPayload struct {
	Hostname     string `json:"hostname"`
	OS           string `json:"os"` // windows / macos / linux
	OSVersion    string `json:"os_version"`
	Arch         string `json:"arch"` // x86_64 / arm64
	CPU          string `json:"cpu"`
	MemoryGB     int    `json:"memory_gb"`
	DiskGB       int    `json:"disk_gb"`
	SerialNo     string `json:"serial_no"`
	MAC          string `json:"mac"`
	IP           string `json:"ip"`
	AgentVersion string `json:"agent_version"`
	Token        string `json:"token,omitempty"` // 重连时回传以复用设备身份
}

// RegisterAckPayload —— Hub→Agent 的注册回执，分配设备身份与令牌。
type RegisterAckPayload struct {
	DeviceID string `json:"device_id"`
	Token    string `json:"token"`
	OK       bool   `json:"ok"`
}

// HeartbeatPayload —— Agent→Hub 的在线心跳，附带负载与 IP。
type HeartbeatPayload struct {
	Load float64 `json:"load"`
	IP   string  `json:"ip"`
}

// CommandPayload —— Hub→Agent 的指令（shell / collect / sw_install）。
type CommandPayload struct {
	Action     string `json:"action"` // shell / collect / sw_install
	TaskID     string `json:"task_id"`
	Command    string `json:"command,omitempty"`
	SoftwareURL string `json:"software_url,omitempty"` // sw_install：下载地址
	Platform   string `json:"platform,omitempty"`     // sw_install：目标平台 windows/macos/linux
	PkgName    string `json:"pkg_name,omitempty"`     // sw_install：包名/文件名
}

// —— M4 远程控制载荷 ——

// RCRequestPayload —— 请求开启远控会话。Operator 为管理员标识（审计用）。
type RCRequestPayload struct {
	DeviceID string `json:"device_id"`
	SessionID string `json:"session_id"`
	Operator string  `json:"operator"`
}

// RCReadyPayload —— 被控端捕获就绪。
type RCReadyPayload struct {
	SessionID string `json:"session_id"`
	OK        bool   `json:"ok"`
	Reason    string `json:"reason,omitempty"`
}

// RCFramePayload —— 一帧屏幕画面（JPEG 经 base64 编码）。
type RCFramePayload struct {
	SessionID string `json:"session_id"`
	Seq       int64  `json:"seq"`
	Width     int    `json:"width"`
	Height    int    `json:"height"`
	Format    string `json:"format"` // "jpeg"
	Data      string `json:"data"`   // base64(jpeg bytes)
	TS        int64  `json:"ts"`
}

// RCInputPayload —— 键鼠输入事件。
type RCInputPayload struct {
	SessionID string `json:"session_id"`
	Kind      string `json:"kind"` // mouse_move / mouse_down / mouse_up / mouse_wheel / key_down / key_up / key_press
	X         int    `json:"x,omitempty"`
	Y         int    `json:"y,omitempty"`
	Button    int    `json:"button,omitempty"`
	Key       string `json:"key,omitempty"` // 按键（字母/数字/特殊键名）
}

// RCEventPayload —— 合规/确认事件。Kind: consent_required / consent_granted / notice。
type RCEventPayload struct {
	SessionID      string `json:"session_id"`
	Kind           string `json:"kind"`
	Message        string `json:"message"`
	RequireConsent bool   `json:"require_consent,omitempty"`
	Consented      bool   `json:"consented,omitempty"`
}

// —— M5 软件分发载荷 ——

// SoftwareItem 软件仓库目录项（与 hub 内存目录对应）。
type SoftwareItem struct {
	ID         string `json:"id"`
	Name       string `json:"name"`
	Version    string `json:"version"`
	Platform   string `json:"platform"` // windows / macos / linux / any
	FileName   string `json:"file_name"`
	Size       int64  `json:"size"`
	UploadedAt int64  `json:"uploaded_at"`
}

// ResultPayload —— Agent→Hub 的执行结果。
type ResultPayload struct {
	TaskID    string `json:"task_id"`
	State     string `json:"state"` // success / failed
	ExitCode  int    `json:"exit_code"`
	OutputLog string `json:"output_log"`
}

// NewEnvelope 构造一个带版本、消息 ID 与唯一 Nonce 的信封。
func NewEnvelope(msgType string, seq int64, payload any) (Envelope, error) {
	raw, err := json.Marshal(payload)
	if err != nil {
		return Envelope{}, err
	}
	nb := make([]byte, 8)
	rand.Read(nb)
	return Envelope{
		Ver:     Ver,
		MsgID:   newMsgID(),
		Ts:      nowUnix(),
		Type:    msgType,
		Seq:     seq,
		Payload: raw,
		Nonce:   hex.EncodeToString(nb),
	}, nil
}

// DecodePayload 将信封内的 Payload 解析到目标结构。
func (e Envelope) DecodePayload(v any) error {
	return json.Unmarshal(e.Payload, v)
}
