package crypto

import (
	"strings"
	"testing"
)

func TestTokenRoundTrip(t *testing.T) {
	secret := "yuduan-dev-secret"
	tok := SignToken(secret, "admin", "admin", 1792804188)
	t.Logf("token=%s", tok)
	c, err := VerifyToken(secret, tok)
	if err != nil {
		t.Fatalf("verify failed: %v", err)
	}
	if c.Username != "admin" || c.Role != "admin" {
		t.Fatalf("bad claims: %+v", c)
	}
}

func TestTokenTamper(t *testing.T) {
	secret := "yuduan-dev-secret"
	tok := SignToken(secret, "admin", "admin", 1792804188)
	// 篡改 role 部分
	bad := "attacker|admin|1792804188." + tok[strings.Index(tok, ".")+1:]
	if _, err := VerifyToken(secret, bad); err == nil {
		t.Fatalf("tampered token should fail")
	}
}

func TestTokenExpired(t *testing.T) {
	secret := "yuduan-dev-secret"
	tok := SignToken(secret, "admin", "admin", 1) // 1970
	if _, err := VerifyToken(secret, tok); err == nil {
		t.Fatalf("expired token should fail")
	}
}
