package chatbot

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func sign(secret string, body []byte) string {
	m := hmac.New(sha256.New, []byte(secret))
	m.Write(body)
	return "sha256=" + hex.EncodeToString(m.Sum(nil))
}

func TestVerifyMatchesServer(t *testing.T) {
	// 与服务端 hmac_sha256 单测同一组 RFC 4231 向量
	sig := "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
	if !Verify("Jefe", []byte("what do ya want for nothing?"), sig) {
		t.Fatal("vector rejected")
	}
	if Verify("Jefe", []byte("tampered"), sig) || Verify("Jefe", []byte("x"), "") {
		t.Fatal("bad signature accepted")
	}
}

func TestSend(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]string
		json.NewDecoder(r.Body).Decode(&body)
		if r.URL.Path != "/v1/hooks" || r.Header.Get("Authorization") != "Bearer good" {
			w.WriteHeader(401)
			io.WriteString(w, `{"error":"invalid bot token"}`)
			return
		}
		conv := body["conversation_id"]
		if conv == "" {
			conv = "dm"
		}
		json.NewEncoder(w).Encode(map[string]any{"id": "m1", "conversation_id": conv, "seq": 1})
	}))
	defer srv.Close()

	s, err := New(srv.URL+"/", "good", "").Send("hi")
	if err != nil || s.ConversationID != "dm" || s.Seq != 1 {
		t.Fatalf("send: %v %+v", err, s)
	}
	s, err = New(srv.URL, "good", "").SendTo("c9", "hi")
	if err != nil || s.ConversationID != "c9" {
		t.Fatalf("sendTo: %v %+v", err, s)
	}
	_, err = New(srv.URL, "bad", "").Send("hi")
	var ce *ChatError
	if !errors.As(err, &ce) || ce.Status != 401 || ce.Message != "invalid bot token" {
		t.Fatalf("want 401 ChatError, got %v", err)
	}
}

func TestWebhook(t *testing.T) {
	bot := New("http://unused", "", "s3cret")
	bot.OnMessage(func(ev Event) string {
		if ev.ConversationType == "group" {
			return ""
		}
		if ev.Message.Content == "boom" {
			panic("boom")
		}
		return "pong: " + ev.Message.Content
	})
	srv := httptest.NewServer(bot)
	defer srv.Close()

	call := func(payload, secret string) (int, map[string]string) {
		req, _ := http.NewRequest("POST", srv.URL+"/any", strings.NewReader(payload))
		req.Header.Set("X-Chat-Signature", sign(secret, []byte(payload)))
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		var out map[string]string
		json.NewDecoder(resp.Body).Decode(&out)
		return resp.StatusCode, out
	}
	ev := func(kind, text string) string {
		return `{"bot_id":"b","conversation_id":"c","conversation_type":"` + kind +
			`","message":{"content":"` + text + `","mentions":[]}}`
	}
	if code, out := call(ev("dm", "ping"), "s3cret"); code != 200 || out["content"] != "pong: ping" {
		t.Fatalf("dm: %d %v", code, out)
	}
	if code, out := call(ev("group", "ping"), "s3cret"); code != 200 || len(out) != 0 {
		t.Fatalf("group: %d %v", code, out)
	}
	if code, _ := call(ev("dm", "ping"), "wrong"); code != 401 {
		t.Fatalf("bad sig: %d", code)
	}
	if code, _ := call(`{"x":1}`, "s3cret"); code != 400 {
		t.Fatalf("bad payload: %d", code)
	}
	if code, _ := call(ev("dm", "boom"), "s3cret"); code != 500 {
		t.Fatalf("panic: %d", code)
	}
}
