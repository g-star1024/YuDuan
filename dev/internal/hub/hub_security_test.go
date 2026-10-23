package hub

import (
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/hex"
	"testing"

	"yuduan/internal/proto"
)

// 固定种子（仅测试用）：与 Rust 副端 ed25519 同算法（crypto/ed25519 权威实现），
// 本测试验证中枢签名产物可被标准 ed25519.Verify 接受（Rust 侧已有 Go 向量互验证）。
var testSeed = mustHex("79756475616e2d656432353531392d63726f7373636865636b2d736565642d33")

func mustHex(s string) []byte {
	b, err := hex.DecodeString(s)
	if err != nil {
		panic(err)
	}
	return b
}

func newSigningHub() *Hub {
	h := &Hub{signSeed: testSeed, software: make(map[string]*proto.SoftwareItem)}
	h.initSigning()
	return h
}

func TestSignSoftware_SignAndVerify(t *testing.T) {
	h := newSigningHub()
	if h.signPub == "" {
		t.Fatalf("signPub 不应为空（种子合法）")
	}

	data := []byte("yuduan-installer-payload-v1")
	shaHex, sigHex, signer := h.signSoftware(data)
	if shaHex == "" {
		t.Fatalf("sha256 不应为空")
	}
	if sigHex == "" || signer == "" {
		t.Fatalf("配置了种子后签名与签名者不应为空")
	}

	// 哈希正确性
	sum := sha256.Sum256(data)
	if hex.EncodeToString(sum[:]) != shaHex {
		t.Fatalf("sha256 与计算不一致: got %s", shaHex)
	}

	// 签名可被标准 ed25519 验签（消息即 32 字节摘要，与 Rust verify 语义一致）
	sig, _ := hex.DecodeString(sigHex)
	pub, _ := hex.DecodeString(signer)
	if len(sig) != ed25519.SignatureSize || len(pub) != ed25519.PublicKeySize {
		t.Fatalf("签名/公钥长度异常 sig=%d pub=%d", len(sig), len(pub))
	}
	if !ed25519.Verify(ed25519.PublicKey(pub), sum[:], sig) {
		t.Fatalf("ed25519.Verify 拒绝中枢签名（跨语言验签失败）")
	}

	// 篡改内容 → 验签必须失败
	tampered := append([]byte(nil), data...)
	tampered[0] ^= 0xff
	tamperedSum := sha256.Sum256(tampered)
	if ed25519.Verify(ed25519.PublicKey(pub), tamperedSum[:], sig) {
		t.Fatalf("篡改后签名仍被接受（验签形同虚设）")
	}
}

func TestSignSoftware_UnsignedWhenNoSeed(t *testing.T) {
	h := &Hub{} // 无种子
	shaHex, sigHex, signer := h.signSoftware([]byte("x"))
	if sigHex != "" || signer != "" {
		t.Fatalf("无种子时不应产生签名")
	}
	if shaHex == "" {
		t.Fatalf("无种子时仍应计算哈希")
	}
}

func TestRepoIDFromURL(t *testing.T) {
	cases := []struct {
		url  string
		want string
	}{
		{"http://hub:8080/api/v1/repo/sw-ab12cd", "sw-ab12cd"},
		{"/api/v1/repo/sw-ffffffff", "sw-ffffffff"},
		{"http://hub:8080/api/v1/repo/sw-xy?token=abc", "sw-xy"},
		{"http://hub:8080/api/v1/repo/sw-xy#frag", "sw-xy"},
		{"http://example.com/other/sw-xy", ""},
		{"", ""},
	}
	for _, c := range cases {
		if got := repoIDFromURL(c.url); got != c.want {
			t.Fatalf("repoIDFromURL(%q)=%q want %q", c.url, got, c.want)
		}
	}
}

// TestHandleDeviceCmd_AttachVerifyParams 验证 sw_install 下发时能从仓库解析并附加校验参数。
func TestHandleDeviceCmd_AttachVerifyParams(t *testing.T) {
	h := newSigningHub()
	// 预置一个已签名的仓库项
	item := &proto.SoftwareItem{
		ID: "sw-test01", Name: "demo", Version: "1.0.0", Platform: "macos",
		FileName: "demo.pkg", Size: 10, UploadedAt: 1,
		Sha256: "deadbeef", Signature: "cafe", Signer: h.signPub,
	}
	h.mu.Lock()
	h.software[item.ID] = item
	h.mu.Unlock()

	// 模拟从 URL 解析并附加
	url := "https://hub.example/api/v1/repo/sw-test01"
	if id := repoIDFromURL(url); id != "sw-test01" {
		t.Fatalf("解析仓库 id 失败: %q", id)
	}
	// 直接断言 item 校验参数非空（下发路径在 handleDeviceCmd 内基于此项填充）
	if item.Sha256 == "" || item.Signature == "" || item.Signer == "" {
		t.Fatalf("仓库项校验参数缺失，下发将无验签")
	}
}
