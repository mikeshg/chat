import { useEffect, useState } from "react";
import { bots, hookUrl } from "../../api";
import type { BotView } from "../../types";

/** 「我」页 webhook bot：建、改回调地址、复制入站地址、换令牌、删除 */
export default function BotSettings() {
  const [list, setList] = useState<BotView[] | null>(null);
  const [username, setUsername] = useState("");
  const [webhook, setWebhook] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    bots
      .list()
      .then(setList)
      .catch((e) => setErr(e instanceof Error ? e.message : "加载失败"));
  }, []);

  const run = async (f: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    setErr("");
    try {
      await f();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "操作失败");
    } finally {
      setBusy(false);
    }
  };

  const replace = (b: BotView) => setList((l) => (l ?? []).map((x) => (x.id === b.id ? b : x)));

  const create = () =>
    run(async () => {
      const b = await bots.create({ username: username.trim(), webhook_url: webhook.trim() });
      setList((l) => [...(l ?? []), b]);
      setUsername("");
      setWebhook("");
    });

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      window.prompt("复制失败，请手动复制：", text);
    }
  };

  return (
    <section className="card">
      <h3>机器人</h3>
      <div className="muted">
        建好后它会出现在你的联系人和会话里，可以拉进群。外部程序往「入站地址」POST
        就能以它的身份发消息；填了回调地址，私聊它或在群里 @ 它时服务器会把消息 POST 过去。
      </div>
      {list?.map((b) => (
        <BotRow
          key={b.id}
          bot={b}
          busy={busy}
          onCopy={copy}
          onSave={(url) => run(async () => replace(await bots.patch(b.id, { webhook_url: url })))}
          onReset={() =>
            confirm("更换令牌？旧的入站地址立即失效。") &&
            run(async () => replace(await bots.resetToken(b.id)))
          }
          onDelete={() =>
            confirm(`删除机器人 ${b.display_name}？它会退出所有群，聊天记录保留。`) &&
            run(async () => {
              await bots.remove(b.id);
              setList((l) => (l ?? []).filter((x) => x.id !== b.id));
            })
          }
        />
      ))}
      <div className="row">
        <input
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          placeholder="用户名（3-20 位字母数字下划线）"
          maxLength={20}
        />
        <input
          className="grow"
          value={webhook}
          onChange={(e) => setWebhook(e.target.value)}
          placeholder="回调地址（可选，http/https）"
        />
        <button className="primary" disabled={busy || !username.trim()} onClick={() => void create()}>
          新建
        </button>
      </div>
      {err && <div className="auth-err">{err}</div>}
      <details className="am-adv">
        <summary>接入说明</summary>
        <div className="am-adv-in">
          <div className="muted">入站：发到和你的私聊；带 conversation_id 发到它所在的其他会话。</div>
          <code className="push-url-text">
            {`curl -X POST <入站地址> -H 'Content-Type: application/json' -d '{"content":"部署完成"}'`}
          </code>
          <div className="muted">
            回调：服务器 POST JSON {"{"}bot_id, conversation_id, conversation_type, message{"}"}，
            请求头 X-Chat-Signature: sha256=HMAC-SHA256(令牌, 请求体)。10 秒内回{" "}
            {'{"content":"..."}'} 即作为它的回复发回原会话，回别的内容则不回复。
          </div>
        </div>
      </details>
    </section>
  );
}

function BotRow(props: {
  bot: BotView;
  busy: boolean;
  onCopy: (text: string) => void;
  onSave: (url: string) => void;
  onReset: () => void;
  onDelete: () => void;
}) {
  const { bot } = props;
  const [url, setUrl] = useState(bot.webhook_url);
  return (
    <div className="bot-row">
      <div>
        <b>{bot.display_name}</b> <span className="muted">@{bot.username}</span>
      </div>
      <div className="row push-url">
        <code className="push-url-text">{hookUrl(bot.token)}</code>
        <button onClick={() => props.onCopy(hookUrl(bot.token))}>复制入站地址</button>
      </div>
      <div className="row">
        <input
          className="grow"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="回调地址（空 = 不回调）"
        />
        <button disabled={props.busy || url.trim() === bot.webhook_url} onClick={() => props.onSave(url.trim())}>
          保存
        </button>
      </div>
      <div className="row push-actions">
        <button disabled={props.busy} onClick={props.onReset}>
          换令牌
        </button>
        <button className="danger" disabled={props.busy} onClick={props.onDelete}>
          删除
        </button>
      </div>
    </div>
  );
}
