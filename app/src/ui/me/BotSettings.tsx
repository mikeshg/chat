import { Fragment, useEffect, useState } from "react";
import { bots, hookUrl } from "../../api";
import type { BotToken, BotView } from "../../types";
import SetRow from "./SetRow";

/** 令牌只露头尾，复制按钮拿全文 */
const mask = (t: string) => `${t.slice(0, 6)}…${t.slice(-4)}`;

/** 「我」页 webhook bot：一行一个，点「管理」展开详情；同一时间只展开一个 */
export default function BotSettings() {
  const [list, setList] = useState<BotView[] | null>(null);
  const [open, setOpen] = useState<string | null>(null); // bot id 或 "new"
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
  const toggle = (id: string) => {
    setErr("");
    setOpen((o) => (o === id ? null : id));
  };

  const create = () =>
    run(async () => {
      const b = await bots.create({
        username: username.trim(),
        webhook_url: webhook.trim(),
      });
      setList((l) => [...(l ?? []), b]);
      setUsername("");
      setWebhook("");
      setOpen(b.id); // 接着加令牌
    });

  return (
    <>
      {list?.map((b) => (
        <Fragment key={b.id}>
          <SetRow
            label={
              <>
                {b.display_name} <span className="muted">@{b.username}</span>
              </>
            }
            desc={`${b.tokens.length} 个令牌 · ${b.webhook_url ? "已设回调" : "未设回调"}`}
          >
            <button onClick={() => toggle(b.id)}>{open === b.id ? "收起" : "管理"}</button>
          </SetRow>
          {open === b.id && (
            <BotDetail
              key={b.id}
              bot={b}
              busy={busy}
              onSave={(url) => run(async () => replace(await bots.patch(b.id, { webhook_url: url })))}
              onAddToken={(name) => run(async () => replace(await bots.addToken(b.id, name)))}
              onRemoveToken={(t) =>
                confirm(`删除令牌「${t.name}」？用它的程序立即无法发消息。`) &&
                run(async () => replace(await bots.removeToken(b.id, t.id)))
              }
              onResetSecret={() =>
                confirm("更换签名密钥？回调端要同步改成新密钥才能验签。") &&
                run(async () => replace(await bots.resetSecret(b.id)))
              }
              onDelete={() =>
                confirm(`删除机器人 ${b.display_name}？它会退出所有群，聊天记录保留。`) &&
                run(async () => {
                  await bots.remove(b.id);
                  setList((l) => (l ?? []).filter((x) => x.id !== b.id));
                  setOpen(null);
                })
              }
            />
          )}
        </Fragment>
      ))}
      <SetRow
        label="新建机器人"
        desc="外部程序可以用它的身份发消息；私聊它或在群里 @ 它时，消息会转给你的程序"
      >
        <button onClick={() => toggle("new")}>{open === "new" ? "取消" : "新建"}</button>
      </SetRow>
      {open === "new" && (
        <div className="set-block set-form">
          <input
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="用户名（3-20 位字母数字下划线）"
            maxLength={20}
          />
          <input
            value={webhook}
            onChange={(e) => setWebhook(e.target.value)}
            placeholder="回调地址（可选，http/https）"
          />
          <button
            className="primary"
            disabled={busy || !username.trim()}
            onClick={() => void create()}
          >
            创建
          </button>
        </div>
      )}
      {err && <div className="set-block auth-err">{err}</div>}
    </>
  );
}

const copy = async (text: string) => {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    window.prompt("复制失败，请手动复制：", text);
  }
};

function BotDetail(props: {
  bot: BotView;
  busy: boolean;
  onSave: (url: string) => void;
  onAddToken: (name: string) => void;
  onRemoveToken: (t: BotToken) => void;
  onResetSecret: () => void;
  onDelete: () => void;
}) {
  const { bot, busy } = props;
  const [url, setUrl] = useState(bot.webhook_url);
  const [tokenName, setTokenName] = useState("");
  return (
    <div className="set-block bot-detail">
      <div className="bot-label muted">回调地址</div>
      <div className="row">
        <input
          className="grow"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="空 = 不回调"
        />
        <button
          disabled={busy || url.trim() === bot.webhook_url}
          onClick={() => props.onSave(url.trim())}
        >
          保存
        </button>
      </div>

      <div className="bot-label muted">入站令牌</div>
      {bot.tokens.map((t) => (
        <div key={t.id} className="bot-kv">
          <span className="bot-k">{t.name}</span>
          <code title={t.token}>{mask(t.token)}</code>
          <button onClick={() => void copy(t.token)}>复制</button>
          <button className="danger" disabled={busy} onClick={() => props.onRemoveToken(t)}>
            删除
          </button>
        </div>
      ))}
      <div className="row">
        <input
          className="grow"
          value={tokenName}
          onChange={(e) => setTokenName(e.target.value)}
          placeholder="新令牌名称（如 CI、监控）"
          maxLength={50}
        />
        <button
          disabled={busy || !tokenName.trim()}
          onClick={() => {
            props.onAddToken(tokenName.trim());
            setTokenName("");
          }}
        >
          加令牌
        </button>
      </div>

      <div className="bot-kv">
        <span className="bot-k">签名密钥</span>
        <code title={bot.secret}>{mask(bot.secret)}</code>
        <button onClick={() => void copy(bot.secret)}>复制</button>
        <button disabled={busy} onClick={props.onResetSecret}>
          更换
        </button>
      </div>

      <details className="set-more">
        <summary className="muted">接入说明</summary>
        <div className="bot-help muted">
          <p>
            入站：带令牌 POST 到 <code>{hookUrl()}</code>，默认发到和你的私聊；带 conversation_id
            发到它所在的其他会话。
          </p>
          <code className="push-url-text">
            {`curl -X POST ${hookUrl()} -H 'Authorization: Bearer <令牌>' -H 'Content-Type: application/json' -d '{"content":"部署完成"}'`}
          </code>
          <p>
            回调：服务器 POST JSON {"{"}bot_id, conversation_id, conversation_type, message{"}"}，
            请求头 X-Chat-Signature: sha256=HMAC-SHA256(签名密钥, 请求体)。10 秒内回{" "}
            {'{"content":"..."}'} 即作为它的回复。Python / Go / Java / Rust SDK 见仓库 sdk 目录。
          </p>
        </div>
      </details>

      <div>
        <button className="danger" disabled={busy} onClick={props.onDelete}>
          删除机器人
        </button>
      </div>
    </div>
  );
}
