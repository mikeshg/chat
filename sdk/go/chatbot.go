// Package chatbot 是聊天服务器 webhook bot 的 Go SDK。只用标准库。
//
//	bot := chatbot.New("https://chat.example.com", "入站令牌", "签名密钥")
//	bot.Send("部署完成")                        // 发到和主人的私聊
//	bot.OnMessage(func(ev chatbot.Event) string { // 私聊它、或在群里 @ 它时调用
//		return "收到：" + ev.Message.Content     // 返回 "" = 不回
//	})
//	log.Fatal(bot.Run(":8080"))                 // 回调地址填 http://<这台机器>:8080/
//
// Bot 本身是 http.Handler，可以挂到已有的服务上。
package chatbot

import (
	"bytes"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"strings"
	"time"
)

// 回调体不会超过这么大
const maxBody = 1 << 20

// Message 是服务器推来的消息，只列常用字段。
type Message struct {
	ID             string   `json:"id"`
	ConversationID string   `json:"conversation_id"`
	Seq            int64    `json:"seq"`
	SenderID       string   `json:"sender_id"`
	SenderUsername string   `json:"sender_username"`
	SenderName     string   `json:"sender_name"`
	Content        string   `json:"content"`
	Kind           string   `json:"kind"` // text / image / file / audio / poll
	Mentions       []string `json:"mentions"`
	ReplyToID      *string  `json:"reply_to_id"`
	CreatedAt      string   `json:"created_at"`
}

// Event 是一次回调：bot 在私聊里收到消息，或在群里被 @。
type Event struct {
	BotID            string  `json:"bot_id"`
	ConversationID   string  `json:"conversation_id"`
	ConversationType string  `json:"conversation_type"` // dm / group
	Message          Message `json:"message"`
}

// Sent 是 Send 的结果。
type Sent struct {
	ID             string `json:"id"`
	ConversationID string `json:"conversation_id"`
	Seq            int64  `json:"seq"`
}

// ChatError 是服务器拒绝了请求。Status 401 = 令牌不对，403 = bot 不在那个会话里。
type ChatError struct {
	Status  int
	Message string
}

func (e *ChatError) Error() string { return fmt.Sprintf("%d: %s", e.Status, e.Message) }

// Verify 校验 X-Chat-Signature 头：sha256=HMAC-SHA256(签名密钥, 请求体)。
func Verify(secret string, body []byte, signature string) bool {
	m := hmac.New(sha256.New, []byte(secret))
	m.Write(body)
	want := "sha256=" + hex.EncodeToString(m.Sum(nil))
	return hmac.Equal([]byte(want), []byte(signature))
}

type Bot struct {
	Server  string // 聊天服务器地址
	Token   string // 入站令牌，发消息用
	Secret  string // 签名密钥，收回调用
	Client  *http.Client
	handler func(Event) string
}

func New(server, token, secret string) *Bot {
	return &Bot{
		Server: strings.TrimRight(server, "/"),
		Token:  token,
		Secret: secret,
		Client: &http.Client{Timeout: 10 * time.Second},
	}
}

// SendOptions 是 SendWith 的可选项，零值 = 发到和主人的私聊、不带幂等键。
type SendOptions struct {
	ConversationID string // 空 = 和主人的私聊
	ClientMsgID    string // 幂等键（1–80 字符）：重试时传同一个值不会重复发
}

// Send 以 bot 身份发到和主人的私聊。
func (b *Bot) Send(content string) (*Sent, error) { return b.SendWith(content, SendOptions{}) }

// SendWith 以 bot 身份发一条文字，可指定会话和幂等键。
func (b *Bot) SendWith(content string, o SendOptions) (*Sent, error) {
	payload := map[string]string{"content": content}
	if o.ConversationID != "" {
		payload["conversation_id"] = o.ConversationID
	}
	if o.ClientMsgID != "" {
		payload["client_msg_id"] = o.ClientMsgID
	}
	body, _ := json.Marshal(payload)
	req, err := http.NewRequest("POST", b.Server+"/v1/hooks", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+b.Token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := b.Client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxBody))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode/100 != 2 {
		var e struct {
			Error string `json:"error"`
		}
		if json.Unmarshal(data, &e) != nil || e.Error == "" {
			e.Error = resp.Status
		}
		return nil, &ChatError{resp.StatusCode, e.Error}
	}
	var s Sent
	if err := json.Unmarshal(data, &s); err != nil {
		return nil, err
	}
	return &s, nil
}

// OnMessage 注册回调处理函数；返回非空字符串即作为回复。服务器只等 10 秒。
func (b *Bot) OnMessage(fn func(Event) string) { b.handler = fn }

// Handle 处理一次回调请求，返回 HTTP 状态码和响应 JSON。用别的 web 框架时直接调它。
func (b *Bot) Handle(body []byte, signature string) (int, map[string]string) {
	if !Verify(b.Secret, body, signature) {
		return 401, map[string]string{"error": "bad signature"}
	}
	var ev Event
	if err := json.Unmarshal(body, &ev); err != nil || ev.BotID == "" {
		return 400, map[string]string{"error": "bad payload"}
	}
	reply := ""
	if b.handler != nil {
		reply = b.handler(ev)
	}
	if reply == "" {
		return 200, map[string]string{}
	}
	return 200, map[string]string{"content": reply}
}

// ServeHTTP 让 Bot 成为 http.Handler，任何路径的 POST 都当回调处理。
func (b *Bot) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	status, out := 405, map[string]string{"error": "POST only"}
	if r.Method == http.MethodPost {
		body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, maxBody))
		if err != nil {
			status, out = 413, map[string]string{"error": "too large"}
		} else {
			status, out = b.safeHandle(body, r.Header.Get("X-Chat-Signature"))
		}
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(out)
}

// 处理函数 panic 不该拖垮服务
func (b *Bot) safeHandle(body []byte, sig string) (status int, out map[string]string) {
	defer func() {
		if p := recover(); p != nil {
			log.Printf("chatbot: handler panicked: %v", p)
			status, out = 500, map[string]string{"error": "handler failed"}
		}
	}()
	return b.Handle(body, sig)
}

// Run 在 addr（如 ":8080"）上收回调，阻塞直到出错。
func (b *Bot) Run(addr string) error {
	log.Printf("bot listening on %s", addr)
	return http.ListenAndServe(addr, b)
}
