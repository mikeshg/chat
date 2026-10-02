import { useCallback, useEffect, useRef, useState } from "react";
import { admin } from "../../api";
import type { BuiltinEndpointConf } from "../../types";

/**
 * 一个端点的编辑态（A28）：
 * - modelsText 是输入框原文，保存时按逗号 / 换行拆分去重；
 * - api_key 是密码框原文，空 = 不改（同 ntfy 令牌）；
 * - clearKey = 点了「清除」，保存时提交 api_key: ""；
 * - id 空串 = 新端点，服务端会发新 id。
 */
interface EpRow {
  key: string;
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  api_key: string;
  api_key_set: boolean;
  clearKey: boolean;
  modelsText: string;
}

let rowSeq = 0;
const nextKey = () => `ep-${++rowSeq}`;

function rowFromConf(c: BuiltinEndpointConf): EpRow {
  return {
    key: c.id || nextKey(),
    id: c.id ?? "",
    name: c.name,
    protocol: c.protocol === "responses" ? "responses" : "chat",
    base_url: c.base_url,
    api_key: "",
    api_key_set: c.api_key_set === true,
    clearKey: false,
    modelsText: c.models.join("\n"),
  };
}

function emptyRow(): EpRow {
  return {
    key: nextKey(),
    id: "",
    name: "",
    protocol: "chat",
    base_url: "",
    api_key: "",
    api_key_set: false,
    clearKey: false,
    modelsText: "",
  };
}

/** 模型列表原文 → 数组：逗号 / 换行分隔，去重去空 */
function parseModels(text: string): string[] {
  const out: string[] = [];
  for (const part of text.split(/[\n,]/)) {
    const m = part.trim();
    if (m && !out.includes(m)) out.push(m);
  }
  return out;
}

const MAX_ENDPOINTS = 20;

/** 内置 Agent 的模型端点（A28）。自包含：自己读 /admin/settings，保存走部分更新的整体替换。 */
export default function AdminBuiltinEndpoints() {
  const [rows, setRows] = useState<EpRow[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState("");
  const [saved, setSaved] = useState(false);
  const savedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const load = useCallback(async () => {
    try {
      const s = await admin.settings();
      setRows((s.builtin_endpoints ?? []).map(rowFromConf));
      setErr("");
    } catch (e) {
      setErr(e instanceof Error ? e.message : "加载失败");
    } finally {
      setLoaded(true);
    }
  }, []);

  useEffect(() => {
    void load();
    return () => {
      if (savedTimer.current) clearTimeout(savedTimer.current);
    };
  }, [load]);

  const patch = (key: string, partial: Partial<EpRow>) =>
    setRows((cur) => cur.map((r) => (r.key === key ? { ...r, ...partial } : r)));

  const save = async () => {
    if (saving) return;
    setSaving(true);
    setErr("");
    setSaved(false);
    try {
      const payload: BuiltinEndpointConf[] = rows.map((r) => ({
        ...(r.id ? { id: r.id } : {}),
        name: r.name,
        protocol: r.protocol,
        base_url: r.base_url,
        // key 只写不读：输入了新值就提交；点了清除提交 ""；否则缺省 = 保留
        ...(r.api_key.trim() ? { api_key: r.api_key.trim() } : r.clearKey ? { api_key: "" } : {}),
        models: parseModels(r.modelsText),
      }));
      const s = await admin.updateSettings({ builtin_endpoints: payload });
      setRows((s.builtin_endpoints ?? []).map(rowFromConf));
      setSaved(true);
      savedTimer.current = setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "保存失败");
    } finally {
      setSaving(false);
    }
  };

  return (
    <>
      <h3>内置 Agent 模型端点</h3>
      <div className="a28-eps">
        <span className="am-hint">
          用户创建「内置」Agent 时可用的模型。API Key 只写不读，保存在服务端、只下发给运行 Agent
          的电脑；改完保存，下一次调用生效。最多 {MAX_ENDPOINTS} 个端点。
        </span>
        {rows.map((r, i) => (
          <div className="a28-ep" key={r.key}>
            <div className="a28-ep-head">
              <input
                aria-label={`端点 ${i + 1} 名称`}
                placeholder="名称，如 DeepSeek"
                value={r.name}
                disabled={saving}
                onChange={(e) => patch(r.key, { name: e.target.value })}
              />
              <select
                aria-label={`端点 ${i + 1} 协议`}
                value={r.protocol}
                disabled={saving}
                onChange={(e) => patch(r.key, { protocol: e.target.value })}
              >
                <option value="chat">Chat Completions</option>
                <option value="responses">Responses</option>
              </select>
              <button
                type="button"
                className="danger"
                disabled={saving}
                onClick={() => setRows((cur) => cur.filter((x) => x.key !== r.key))}
              >
                删除
              </button>
            </div>
            <input
              aria-label={`端点 ${i + 1} 地址`}
              placeholder="https://api.deepseek.com/v1"
              value={r.base_url}
              disabled={saving}
              onChange={(e) => patch(r.key, { base_url: e.target.value })}
            />
            <div className="a28-ep-key">
              <input
                className="grow"
                type="password"
                aria-label={`端点 ${i + 1} API Key`}
                placeholder={
                  r.api_key_set && !r.clearKey ? "已设置，留空不改" : "API Key（可空，本地端点可不填）"
                }
                value={r.api_key}
                disabled={saving || r.clearKey}
                onChange={(e) => patch(r.key, { api_key: e.target.value, clearKey: false })}
              />
              {r.api_key_set && !r.clearKey && (
                <button
                  type="button"
                  disabled={saving}
                  onClick={() => patch(r.key, { clearKey: true, api_key: "" })}
                >
                  清除
                </button>
              )}
              {r.clearKey && (
                <button type="button" disabled={saving} onClick={() => patch(r.key, { clearKey: false })}>
                  保留原 Key
                </button>
              )}
            </div>
            {r.clearKey && <span className="am-hint">保存后清除这个端点已设置的 Key</span>}
            <textarea
              aria-label={`端点 ${i + 1} 模型`}
              rows={2}
              placeholder={"模型名，每行或逗号分隔\n例如 deepseek-chat, deepseek-reasoner"}
              value={r.modelsText}
              disabled={saving}
              onChange={(e) => patch(r.key, { modelsText: e.target.value })}
            />
          </div>
        ))}
        {loaded && rows.length === 0 && <div className="muted">还没有端点</div>}
        <div className="row">
          <button
            type="button"
            disabled={saving || rows.length >= MAX_ENDPOINTS}
            onClick={() => setRows((cur) => [...cur, emptyRow()])}
          >
            ＋ 添加端点
          </button>
          <button type="button" className="primary" disabled={saving} onClick={() => void save()}>
            {saving ? "保存中…" : "保存端点"}
          </button>
          {saved && <span className="muted ad-saved">已保存</span>}
        </div>
        {err && <div className="auth-err">{err}</div>}
      </div>
    </>
  );
}
