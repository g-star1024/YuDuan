// Package pg 是御端在受限环境（无法拉取第三方驱动）下自研的最小化 PostgreSQL 客户端。
// 仅覆盖御端所需的「简单查询」能力：trust / 明文口令认证、文本格式行解析。
// 生产环境如需更完整能力，可替换为 github.com/lib/pq 或 pgx（见安全里程碑文档）。
package pg

import (
	"bufio"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
)

// Conn 表示一个 Postgres 连接。
type Conn struct {
	conn net.Conn
	buf  *bufio.Reader
}

// Connect 建立连接并完成启动/认证握手，直到收到 ReadyForQuery。
// 支持 trust（无口令）与 cleartext（明文口令）两种认证方式。
func Connect(host string, port int, user, database, password string) (*Conn, error) {
	nc, err := net.Dial("tcp", net.JoinHostPort(host, strconv.Itoa(port)))
	if err != nil {
		return nil, err
	}
	c := &Conn{conn: nc, buf: bufio.NewReader(nc)}

	// 构造启动消息（无类型字节）：长度 + 协议版本 + 参数键值对 + 终结 \0
	params := encodeParams("user", user, "database", database)
	body := make([]byte, 0, len(params)+4)
	body = append(body, 0x00, 0x03, 0x00, 0x00) // 协议版本 196608
	body = append(body, params...)
	startup := make([]byte, 4+len(body))
	binary.BigEndian.PutUint32(startup[0:4], uint32(len(startup)))
	copy(startup[4:], body)
	if _, err := nc.Write(startup); err != nil {
		return nil, err
	}

	// 读取直到 ReadyForQuery('Z')，期间处理认证请求
	for {
		tag, body, err := c.readMessage()
		if err != nil {
			return nil, err
		}
		switch tag {
		case 'R': // AuthenticationRequest
			authType := int32(binary.BigEndian.Uint32(body[0:4]))
			switch authType {
			case 0: // OK
				continue
			case 3: // CleartextPassword
				if err := c.writeMessage('p', []byte(password+"\x00")); err != nil {
					return nil, err
				}
			default:
				return nil, fmt.Errorf("unsupported auth type %d", authType)
			}
		case 'K', 'S', 'N': // BackendKeyData / ParameterStatus / Notice
			continue
		case 'E': // Error
			return nil, fmt.Errorf("pg error: %s", parseErr(body))
		case 'Z': // ReadyForQuery
			return c, nil
		default:
			continue
		}
	}
}

// Query 执行一条简单查询，返回列名与文本行（二维字符串）。
func (c *Conn) Query(sql string) ([]string, [][]string, error) {
	if err := c.writeMessage('Q', []byte(sql+"\x00")); err != nil {
		return nil, nil, err
	}
	var cols []string
	var rows [][]string
	for {
		tag, body, err := c.readMessage()
		if err != nil {
			return nil, nil, err
		}
		switch tag {
		case 'T': // RowDescription
			cols = parseRowDesc(body)
		case 'D': // DataRow
			rows = append(rows, parseDataRow(body))
		case 'C', 'I': // CommandComplete / EmptyQuery
			continue
		case 'E':
			return nil, nil, fmt.Errorf("pg error: %s", parseErr(body))
		case 'Z':
			return cols, rows, nil
		default:
			continue
		}
	}
}

// Exec 执行不关心结果集的语句（INSERT/UPDATE/CREATE 等）。
func (c *Conn) Exec(sql string) error {
	_, _, err := c.Query(sql)
	return err
}

// Close 关闭连接。
func (c *Conn) Close() error { return c.conn.Close() }

// ---- 内部：消息读写 ----

func (c *Conn) writeMessage(tag byte, body []byte) error {
	msg := make([]byte, 1+4+len(body))
	msg[0] = tag
	binary.BigEndian.PutUint32(msg[1:5], uint32(len(body)+4))
	copy(msg[5:], body)
	_, err := c.conn.Write(msg)
	return err
}

func (c *Conn) readMessage() (byte, []byte, error) {
	tag, err := c.buf.ReadByte()
	if err != nil {
		return 0, nil, err
	}
	var lengthBytes [4]byte
	if _, err := io.ReadFull(c.buf, lengthBytes[:]); err != nil {
		return 0, nil, err
	}
	length := int(binary.BigEndian.Uint32(lengthBytes[:])) - 4
	if length < 0 {
		return 0, nil, errors.New("pg: negative message length")
	}
	body := make([]byte, length)
	if _, err := io.ReadFull(c.buf, body); err != nil {
		return 0, nil, err
	}
	return tag, body, nil
}

// ---- 内部：解析 ----

func encodeParams(kv ...string) []byte {
	var b []byte
	for i := 0; i+1 < len(kv); i += 2 {
		b = append(b, kv[i]...)
		b = append(b, 0)
		b = append(b, kv[i+1]...)
		b = append(b, 0)
	}
	b = append(b, 0)
	return b
}

func parseRowDesc(body []byte) []string {
	count := int(int16(binary.BigEndian.Uint16(body[0:2])))
	off := 2
	cols := make([]string, 0, count)
	for i := 0; i < count; i++ {
		end := off
		for body[end] != 0 {
			end++
		}
		cols = append(cols, string(body[off:end]))
		off = end + 1
		off += 18 // tableOID(4)+col(2)+typeOID(4)+typeSize(2)+typeMod(4)+format(2)
	}
	return cols
}

func parseDataRow(body []byte) []string {
	count := int(int16(binary.BigEndian.Uint16(body[0:2])))
	off := 2
	cells := make([]string, 0, count)
	for i := 0; i < count; i++ {
		l := int(int32(binary.BigEndian.Uint32(body[off : off+4])))
		off += 4
		if l == -1 {
			cells = append(cells, "")
			continue
		}
		cells = append(cells, string(body[off:off+l]))
		off += l
	}
	return cells
}

func parseErr(body []byte) string {
	// 形如：多组 key\0value\0 结尾 \0；提取 M=消息
	fields := strings.Split(string(body), "\x00")
	for i := 0; i+1 < len(fields); i += 2 {
		if fields[i] == "M" {
			return fields[i+1]
		}
	}
	return "unknown error"
}
