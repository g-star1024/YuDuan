// Package proto 定义御端控制面消息信封与各类载荷，对齐 dev/proto/protocol.md（M1 冻结 v1）。
package proto

import "encoding/json"

// Ver 为当前协议版本，与 protocol.md 保持一致。
const Ver = 1

// Envelope 是 WebSocket 控制消息的统一信封（JSON）。
// 关键指令由服务端签名，Agent 验签；心跳/回执类携带有效令牌即可。
type Envelope struct {
	Ver     int             `json:"ver"`
	MsgID   string          `json:"msg_id"`
	Ts      int64           `json:"ts"`
	Type    string          `json:"type"`
	Seq     int64           `json:"seq"`
	Payload json.RawMessage `json:"payload"`
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

// CommandPayload —— Hub→Agent 的指令（shell / collect / 后续 sw_install 等）。
type CommandPayload struct {
	Action  string `json:"action"` // shell / collect
	TaskID  string `json:"task_id"`
	Command string `json:"command,omitempty"`
}

// ResultPayload —— Agent→Hub 的执行结果。
type ResultPayload struct {
	TaskID    string `json:"task_id"`
	State     string `json:"state"` // success / failed
	ExitCode  int    `json:"exit_code"`
	OutputLog string `json:"output_log"`
}

// NewEnvelope 构造一个带版本与消息 ID 的信封。
func NewEnvelope(msgType string, seq int64, payload any) (Envelope, error) {
	raw, err := json.Marshal(payload)
	if err != nil {
		return Envelope{}, err
	}
	return Envelope{
		Ver:     Ver,
		MsgID:   newMsgID(),
		Ts:      nowUnix(),
		Type:    msgType,
		Seq:     seq,
		Payload: raw,
	}, nil
}

// DecodePayload 将信封内的 Payload 解析到目标结构。
func (e Envelope) DecodePayload(v any) error {
	return json.Unmarshal(e.Payload, v)
}
