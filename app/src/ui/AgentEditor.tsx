import { useEffect, useRef, useState } from "react";
import { agents, ApiError } from "../api";
import { cropAvatarFile } from "../avatar";
import type { AgentView, ReportedOptions } from "../types";
import { shapeAgentPatch } from "./agentForm";
import AgentAvatar from "./AgentAvatar";
import AgentSchedules from "./AgentSchedules";
import { OUTPUT_MODE_OPTIONS, QUEUE_TIMEOUT_OPTIONS } from "./AgentSettings";

function errText(e: unknown): string {
  if (!(e instanceof ApiError)) return e instanceof Error ? e.message : "保存失败";
  if (e.status === 409 || e.message === "name_taken") return "名字已被占用（不区分大小写）";
  const known: Record<string, string> = {
    bad_name: "名字不合法（1–32 位字母数字 _ -，不能以 - 开头）",
    bad_runtime: "命令、参数或工作目录不合法",
    bad_extra_dirs: "额外目录过多或某一行过长（最多 10 行，每行 500 字）",
    bad_instructions: "附加指令超过 8000 字",
    bad_env: "环境变量不合法（最多 50 项，键 100 字、值 4000 字）",
    bad_model: "模型名过长",
    bad_mode: "模式名过长",
    host_immutable: "不能更换宿主",
    bad_output_mode: "输出方式不合法",
    bad_queue_timeout: "排队超时不合法",
    host_offline: "宿主不在线",
  };
  if (e.status === 400) return known[e.message] ?? `参数不合法：${e.message}`;
  return e.message || "保存失败";
}

/** 单个 Agent 的编辑表单。保存走 PATCH；删除需再点一次确认。 */
export default function AgentEditor({
  agent,
  hostLabel,
  onSaved,
  onUpdated,
  onDeleted,
  onClose,
}: {
  agent: AgentView;
  hostLabel: string;
  onSaved: (a: AgentView) => void;
  /** 头像等就地更新：不要关掉表单 */
  onUpdated: (a: AgentView) => void;
  onDeleted: (id: string) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(agent.name);
  const [command, setCommand] = useState(agent.runtime?.command ?? "");
  const [argsText, setArgsText] = useState((agent.runtime?.args ?? []).join("\n"));
  const [cwd, setCwd] = useState(agent.cwd ?? "");
  const [extraDirsText, setExtraDirsText] = useState((agent.extra_dirs ?? []).join("\n"));
  const [model, setModel] = useState(agent.model ?? "");
  const [mode, setMode] = useState(agent.mode ?? "");
  const [instructions, setInstructions] = useState(agent.instructions ?? "");
  const [envRows, setEnvRows] = useState(() => {
    const rows = Object.entries(agent.env ?? {}).map(([key, value]) => ({ key, value }));
    return rows.length ? rows : [{ key: "", value: "" }];
  });
  const [outputMode, setOutputMode] = useState(agent.output_mode ?? "");
  const [queueTimeout, setQueueTimeout] = useState(
    agent.queue_timeout_secs == null ? "" : String(agent.queue_timeout_secs),
  );
  const [reported, setReported] = useState<ReportedOptions>(agent.reported_options ?? {});
  const [avatarVersion, setAvatarVersion] = useState(agent.avatar_version ?? 0);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [probing, setProbing] = useState(false);
  const [confirmDel, setConfirmDel] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);

  const reportedKey = JSON.stringify(agent.reported_options ?? {});
  useEffect(() => {
    setReported(agent.reported_options ?? {});
    setAvatarVersion(agent.avatar_version ?? 0);
  }, [agent.id, agent.avatar_version, reportedKey]);

  const modes = reported.modes ?? [];
  const models = reported.models ?? [];
  const hasOptions = modes.length > 0 || models.length > 0;
  const timeoutKnown =
    queueTimeout === "" || QUEUE_TIMEOUT_OPTIONS.some((o) => String(o.v) === queueTimeout);

  const save = async () => {
    if (busy) return;
    setErr("");
    setBusy(true);
    try {
      const updated = await agents.patch(
        agent.id,
        shapeAgentPatch({
          name,
          command,
          argsText,
          cwd,
          extraDirsText,
          model,
          mode,
          instructions,
          envRows,
          outputMode,
          queueTimeout,
        }),
      );
      onSaved(updated);
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  };

  const probe = async () => {
    setErr("");
    setProbing(true);
    try {
      await agents.probe(agent.id);
    } catch (e) {
      setErr(errText(e));
    } finally {
      setProbing(false);
    }
  };

  const onAvatar = async (file: File | undefined) => {
    if (!file) return;
    setErr("");
    setBusy(true);
    try {
      const blob = await cropAvatarFile(file);
      const updated = await agents.uploadAvatar(agent.id, blob);
      setAvatarVersion(updated.avatar_version ?? avatarVersion);
      onUpdated(updated);
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
      if (fileRef.current) fileRef.current.value = "";
    }
  };

  const doDelete = async () => {
    setErr("");
    setBusy(true);
    try {
      await agents.remove(agent.id);
      onDeleted(agent.id);
    } catch (e) {
      setErr(errText(e));
      setBusy(false);
    }
  };

  return (
    <section className="agent-form agent-editor">
      <h4 className="agent-sec">编辑 {agent.name}</h4>
      <div className="row agent-item">
        <button
          type="button"
          className="avatar-pick"
          title="更换头像"
          aria-label="更换 Agent 头像"
          disabled={busy}
          onClick={() => fileRef.current?.click()}
        >
          <AgentAvatar agentId={agent.id} name={name || agent.name} version={avatarVersion} size="lg" />
        </button>
        <span className="muted">点击头像上传 PNG / JPEG / WebP</span>
        <input
          ref={fileRef}
          type="file"
          accept="image/png,image/jpeg,image/webp"
          hidden
          onChange={(e) => void onAvatar(e.target.files?.[0])}
        />
      </div>
      <input
        aria-label="Agent 名字"
        placeholder="名字（1–32 位字母数字 _ -）"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <label className="agent-field">
        <span className="muted">宿主（不可改）</span>
        <input value={hostLabel} readOnly />
      </label>
      <input
        aria-label="启动命令"
        placeholder="启动命令"
        value={command}
        onChange={(e) => setCommand(e.target.value)}
      />
      <textarea
        aria-label="命令参数"
        placeholder="参数（每行一个）"
        value={argsText}
        onChange={(e) => setArgsText(e.target.value)}
        rows={3}
      />
      <input
        aria-label="工作目录"
        placeholder="工作目录（可留空）"
        value={cwd}
        onChange={(e) => setCwd(e.target.value)}
      />
      <textarea
        aria-label="额外目录"
        placeholder="额外目录（每行一个）"
        value={extraDirsText}
        onChange={(e) => setExtraDirsText(e.target.value)}
        rows={2}
      />
      {hasOptions ? (
        <>
          <label className="agent-field">
            <span className="muted">模式</span>
            <select aria-label="模式" value={mode} onChange={(e) => setMode(e.target.value)}>
              <option value="">默认</option>
              {mode && !modes.some((m) => m.id === mode) && <option value={mode}>{mode}</option>}
              {modes.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.name}
                </option>
              ))}
            </select>
          </label>
          <label className="agent-field">
            <span className="muted">模型</span>
            <select aria-label="模型" value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">默认</option>
              {model && !models.some((m) => m.id === model) && <option value={model}>{model}</option>}
              {models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.name}
                </option>
              ))}
            </select>
          </label>
        </>
      ) : (
        <div className="muted">
          运行一次或点“读取可选项”后可选
          <button type="button" disabled={probing || busy} onClick={() => void probe()}>
            {probing ? "读取中…" : "读取可选项"}
          </button>
        </div>
      )}
      <textarea
        aria-label="附加指令"
        placeholder="附加指令（新建会话时放在用户消息前面）"
        value={instructions}
        onChange={(e) => setInstructions(e.target.value)}
        rows={3}
      />
      <div className="agent-env-list">
        <span className="muted">环境变量</span>
        {envRows.map((row, i) => (
          <div className="row agent-env" key={i}>
            <input
              aria-label={`环境变量键 ${i + 1}`}
              placeholder="键"
              value={row.key}
              onChange={(e) =>
                setEnvRows((rows) => rows.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)))
              }
            />
            <input
              aria-label={`环境变量值 ${i + 1}`}
              placeholder="值"
              value={row.value}
              onChange={(e) =>
                setEnvRows((rows) => rows.map((r, j) => (j === i ? { ...r, value: e.target.value } : r)))
              }
            />
            <button
              type="button"
              className="icon-btn"
              aria-label={`删除环境变量 ${i + 1}`}
              onClick={() => setEnvRows((rows) => rows.filter((_, j) => j !== i))}
            >
              ✕
            </button>
          </div>
        ))}
        <button type="button" onClick={() => setEnvRows((rows) => [...rows, { key: "", value: "" }])}>
          添加环境变量
        </button>
      </div>
      <label className="agent-field">
        <span className="muted">输出方式</span>
        <select aria-label="输出方式覆盖" value={outputMode} onChange={(e) => setOutputMode(e.target.value)}>
          <option value="">跟随默认</option>
          {OUTPUT_MODE_OPTIONS.map((o) => (
            <option key={o.v} value={o.v}>
              {o.label}
            </option>
          ))}
        </select>
      </label>
      <label className="agent-field">
        <span className="muted">排队超时</span>
        <select
          aria-label="排队超时覆盖"
          value={queueTimeout}
          onChange={(e) => setQueueTimeout(e.target.value)}
        >
          <option value="">跟随默认</option>
          {!timeoutKnown && <option value={queueTimeout}>{queueTimeout} 秒</option>}
          {QUEUE_TIMEOUT_OPTIONS.map((o) => (
            <option key={o.v} value={o.v}>
              {o.label}
            </option>
          ))}
        </select>
      </label>
      {err && <div className="auth-err">{err}</div>}
      <div className="row end">
        <button type="button" onClick={onClose} disabled={busy}>
          取消
        </button>
        <button type="button" className="primary" disabled={busy || !name.trim() || !command.trim()} onClick={() => void save()}>
          {busy ? "保存中…" : "保存"}
        </button>
      </div>
      {confirmDel ? (
        <div className="agent-del-confirm">
          <span>确认删除？专属会话会保留，但不能再发言。</span>
          <div className="row end">
            <button type="button" disabled={busy} onClick={() => setConfirmDel(false)}>
              取消
            </button>
            <button type="button" className="danger" disabled={busy} onClick={() => void doDelete()}>
              确认删除
            </button>
          </div>
        </div>
      ) : (
        <button type="button" className="danger" disabled={busy} onClick={() => setConfirmDel(true)}>
          删除 Agent
        </button>
      )}
      <AgentSchedules agentId={agent.id} conversationId={agent.conversation_id} />
    </section>
  );
}
