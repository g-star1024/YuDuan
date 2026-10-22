package proto

import (
	"crypto/rand"
	"encoding/hex"
	"time"
)

// nowUnix 返回当前 Unix 秒级时间戳。
func nowUnix() int64 { return time.Now().Unix() }

// newMsgID 生成随消息 ID（16 字节十六进制，碰撞概率可忽略）。
func newMsgID() string {
	b := make([]byte, 16)
	rand.Read(b)
	return hex.EncodeToString(b)
}
