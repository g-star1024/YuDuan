// Package ws 是零依赖的最小化 WebSocket 实现，仅覆盖御端控制面所需能力：
// 文本帧的收发、ping/pong、close。用于 M2/M3 联调，避免在受限环境拉取第三方依赖。
// 生产环境将替换为带 TLS 的成熟实现（见铁律 4：全链路 TLS）。
package ws

import (
	"bufio"
	"crypto/rand"
	"crypto/sha1"
	"encoding/base64"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"net/http"
	"strings"
)

const guid = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

// Conn 表示一个 WebSocket 连接；isClient 决定写帧是否加掩码、读帧是否要求掩码。
type Conn struct {
	conn    net.Conn
	buf     *bufio.Reader
	isClient bool
}

// Upgrade 在服务端将 HTTP 连接升级为 WebSocket（HTTP Hijack 实现）。
func Upgrade(w http.ResponseWriter, r *http.Request) (*Conn, error) {
	key := r.Header.Get("Sec-WebSocket-Key")
	if key == "" {
		http.Error(w, "missing Sec-WebSocket-Key", http.StatusBadRequest)
		return nil, errors.New("missing key")
	}
	h := sha1.Sum([]byte(key + guid))
	accept := base64.StdEncoding.EncodeToString(h[:])

	hj, ok := w.(http.Hijacker)
	if !ok {
		http.Error(w, "hijack not supported", http.StatusInternalServerError)
		return nil, errors.New("hijack not supported")
	}
	raw, _, err := hj.Hijack()
	if err != nil {
		return nil, err
	}
	resp := "HTTP/1.1 101 Switching Protocols\r\n" +
		"Upgrade: websocket\r\n" +
		"Connection: Upgrade\r\n" +
		"Sec-WebSocket-Accept: " + accept + "\r\n\r\n"
	if _, err := raw.Write([]byte(resp)); err != nil {
		raw.Close()
		return nil, err
	}
	return &Conn{conn: raw, buf: bufio.NewReader(raw), isClient: false}, nil
}

// Dial 作为客户端连接 WebSocket 服务端并完成握手。支持 ws://host:port/path。
func Dial(urlStr string) (*Conn, error) {
	// 解析 ws://host:port/path
	if !strings.HasPrefix(urlStr, "ws://") {
		return nil, errors.New("only ws:// supported in M2 PoC")
	}
	rest := strings.TrimPrefix(urlStr, "ws://")
	path := "/"
	if idx := strings.Index(rest, "/"); idx >= 0 {
		path = rest[idx:]
		rest = rest[:idx]
	}
	host := rest
	if !strings.Contains(host, ":") {
		host += ":80"
	}
	netConn, err := net.Dial("tcp", host)
	if err != nil {
		return nil, err
	}
	key := make([]byte, 16)
	rand.Read(key)
	handshake := "GET " + path + " HTTP/1.1\r\n" +
		"Host: " + rest + "\r\n" +
		"Upgrade: websocket\r\n" +
		"Connection: Upgrade\r\n" +
		"Sec-WebSocket-Key: " + base64.StdEncoding.EncodeToString(key) + "\r\n" +
		"Sec-WebSocket-Version: 13\r\n\r\n"
	if _, err := netConn.Write([]byte(handshake)); err != nil {
		netConn.Close()
		return nil, err
	}
	br := bufio.NewReader(netConn)
	statusLine, err := br.ReadString('\n')
	if err != nil {
		netConn.Close()
		return nil, err
	}
	if !strings.Contains(statusLine, "101") {
		netConn.Close()
		return nil, errors.New("handshake failed: " + strings.TrimSpace(statusLine))
	}
	for {
		line, err := br.ReadString('\n')
		if err != nil {
			netConn.Close()
			return nil, err
		}
		if line == "\r\n" || line == "\n" {
			break
		}
	}
	return &Conn{conn: netConn, buf: br, isClient: true}, nil
}

// ReadMessage 读取一帧文本消息；自动处理 ping/pong/close。
func (c *Conn) ReadMessage() ([]byte, error) {
	for {
		b0, err := c.buf.ReadByte()
		if err != nil {
			return nil, err
		}
		b1, err := c.buf.ReadByte()
		if err != nil {
			return nil, err
		}
		opcode := b0 & 0x0f
		masked := b1&0x80 != 0
		ln := int(b1 & 0x7f)
		switch {
		case ln == 126:
			var ext [2]byte
			io.ReadFull(c.buf, ext[:])
			ln = int(binary.BigEndian.Uint16(ext[:]))
		case ln == 127:
			var ext [8]byte
			io.ReadFull(c.buf, ext[:])
			ln = int(binary.BigEndian.Uint64(ext[:]))
		}
		var mask [4]byte
		if masked {
			io.ReadFull(c.buf, mask[:])
		}
		payload := make([]byte, ln)
		io.ReadFull(c.buf, payload)
		if masked {
			for i := range payload {
				payload[i] ^= mask[i%4]
			}
		}
		switch opcode {
		case 0x8: // close
			c.conn.Close()
			return nil, io.EOF
		case 0x9: // ping -> pong
			_ = c.WritePong(payload)
			continue
		case 0xA: // pong
			continue
		case 0x1, 0x0: // text / continuation
			return payload, nil
		default:
			continue
		}
	}
}

// WriteMessage 发送一帧文本消息。
func (c *Conn) WriteMessage(data []byte) error {
	var hdr []byte
	var maskBit byte
	if c.isClient {
		maskBit = 0x80
	}
	ln := len(data)
	switch {
	case ln < 126:
		hdr = []byte{0x81, byte(ln) | maskBit}
	case ln <= 0xffff:
		h := make([]byte, 4)
		h[0], h[1] = 0x81, 126|maskBit
		binary.BigEndian.PutUint16(h[2:4], uint16(ln))
		hdr = h
	default:
		h := make([]byte, 10)
		h[0], h[1] = 0x81, 127|maskBit
		binary.BigEndian.PutUint64(h[2:10], uint64(ln))
		hdr = h
	}
	if _, err := c.conn.Write(hdr); err != nil {
		return err
	}
	if c.isClient {
		var mask [4]byte
		rand.Read(mask[:])
		if _, err := c.conn.Write(mask[:]); err != nil {
			return err
		}
		masked := make([]byte, ln)
		for i := range data {
			masked[i] = data[i] ^ mask[i%4]
		}
		_, err := c.conn.Write(masked)
		return err
	}
	_, err := c.conn.Write(data)
	return err
}

// WritePong 回复 pong 控制帧。
func (c *Conn) WritePong(data []byte) error {
	return c.writeControl(0xA, data)
}

// WriteClose 发送 close 控制帧。
func (c *Conn) WriteClose() error {
	return c.writeControl(0x8, nil)
}

func (c *Conn) writeControl(opcode byte, data []byte) error {
	hdr := []byte{0x80 | opcode, byte(len(data))}
	if c.isClient {
		hdr[1] |= 0x80
	}
	if _, err := c.conn.Write(hdr); err != nil {
		return err
	}
	if c.isClient {
		var mask [4]byte
		rand.Read(mask[:])
		if _, err := c.conn.Write(mask[:]); err != nil {
			return err
		}
		masked := make([]byte, len(data))
		for i := range data {
			masked[i] = data[i] ^ mask[i%4]
		}
		_, err := c.conn.Write(masked)
		return err
	}
	_, err := c.conn.Write(data)
	return err
}

// Close 关闭底层连接。
func (c *Conn) Close() error {
	return c.conn.Close()
}
