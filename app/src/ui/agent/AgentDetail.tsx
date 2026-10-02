import { useEffect, useRef, useState } from "react";
import { agents } from "../../api";
import { cropAvatarFile } from "../../avatar";
import type { AgentHostView, AgentUserSettings, AgentView } from "../../types";
import { hostDisplayName } from "./agentHosts";
import {
  OUTPUT_MODE_OPTIONS,
  QUEUE_TIMEOUT_OPTIONS,
  agentDraftDirty,
  agentErrText,
  agentPlaceLine,
  agentSaveBar,
  draftFromAgent,
  followDefaultOutputHint,
  followDefaultQueueOption,
  outputModeLabel,
  platformLabel,
  queueTimeoutLabel,
  type AgentDraftFields,
} from "./agentPage";
import { shapeAgentPatch } from "./agentForm";
import { isBuiltin, useBuiltinModels } from "./agentBuiltin";
import { PERMISSION_TIERS } from "./approval";
import AgentAvatar from "./AgentAvatar";
import AgentSchedules from "./AgentSchedules";
import ChoiceCards from "./ChoiceCards";
import ModelModeFields from "./ModelModeFields";

const TABS = [
  ["basic", "基本"],
  ["prog", "程序"],
  ["perm", "权限"],
  ["reply", "回复"],
  ["sched", "定时任务"],
] as const;

/** 右栏：一个 Agent 的五个标签。前四个共用草稿，一次保存。 */
export default function AgentDetail({
  agent,
  host,
  local,
  settings,
  notice,
  onDirty,
  onSaved,
  onUpdated,
  onDeleted,
  onOpenConv,
  onBack,
}: {
  agent: AgentView;
  host: AgentHostView | null;
  local: boolean;
  settings: AgentUserSettings | null;
  notice: string;
  onDirty: (dirty: boolean) => void;
  onSaved: (agent: AgentView) => void;
  onUpdated: (agent: AgentView) => void;
  onDeleted: (id: string) => void;
  onOpenConv: () => void;
  onBack: () => void;
}) {
  const [tab, setTab] = useState<(typeof TABS)[number][0]>("basic");
  const [draft, setDraft] = useState<AgentDraftFields>(() => draftFromAgent(agent));
  const [registryName, setRegistryName] = useState(agent.runtime?.registry?.id ?? "");
  const [avatarVersion, setAvatarVersion] = useState(agent.avatar_version ?? 0);
  const [reported, setReported] = useState(agent.reported_options ?? {});
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [probing, setProbing] = useState(false);
  const [probeErr, setProbeErr] = useState("");
  const [confirmDel, setConfirmDel] = useState(false);
  const [schedCount, setSchedCount] = useState(0);
  const fileRef = useRef<HTMLInputElement>(null);
  const dirtyRef = useRef(false);

  const saved = draftFromAgent(agent);
  const dirty = agentDraftDirty(saved, draft);
  dirtyRef.current = dirty;
  const bar = agentSaveBar(dirty);
  const pin = agent.runtime?.registry;
  const builtin = isBuiltin(agent.runtime);
  const builtinOpts = useBuiltinModels(builtin);
  const computer = host ? hostDisplayName(host) : "未知电脑";
  const online = host?.online ?? agent.host_online;
  const os = platformLabel(host?.platform);

  useEffect(() => {
    onDirty(dirty);
  }, [dirty, onDirty]);

  useEffect(() => () => onDirty(false), [onDirty]);

  useEffect(() => {
    if (!dirtyRef.current) setDraft(draftFromAgent(agent));
    setAvatarVersion(agent.avatar_version ?? 0);
    setReported(agent.reported_options ?? {});
  }, [agent]);

  useEffect(() => {
    if (!pin || builtin) return;
    let alive = true;
    agents
      .registry(agent.host_id)
      .then((res) => {
        const hit = (res.entries ?? []).find((e) => e.id === pin.id);
        if (alive && hit?.name) setRegistryName(hit.name);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [agent.host_id, pin?.id, pin?.version]);

  const patchDraft = (partial: Partial<AgentDraftFields>) => {
    setDraft((cur) => ({ ...cur, ...partial }));
    setErr("");
  };

  const save = async () => {
    if (busy) return;
    setBusy(true);
    setErr("");
    try {
      const updated = await agents.patch(
        agent.id,
        shapeAgentPatch({
          name: draft.name,
          command: draft.command,
          argsText: draft.argsText,
          cwd: draft.cwd,
          extraDirsText: draft.extraDirs.join("\n"),
          model: draft.model,
          mode: draft.mode,
          instructions: draft.instructions,
          envRows: draft.envRows,
          outputMode: draft.outputMode,
          queueTimeout: draft.queueTimeout,
          permissionTier: draft.permissionTier,
          registry: pin?.id ? pin : undefined,
          builtin,
        }),
      );
      setDraft(draftFromAgent(updated));
      onSaved(updated);
    } catch (e) {
      setErr(agentErrText(e));
    } finally {
      setBusy(false);
    }
  };

  const discard = () => {
    setDraft(draftFromAgent(agent));
    setErr("");
    setConfirmDel(false);
  };

  const probe = async () => {
    setProbeErr("");
    setProbing(true);
    try {
      const res = await agents.probe(agent.id);
      if (res.error) setProbeErr(res.error);
      else setReported({ modes: res.modes, models: res.models });
    } catch (e) {
      setProbeErr(agentErrText(e));
    } finally {
      setProbing(false);
    }
  };

  // 从没读到过可选项（新建的 Agent）且电脑在线：自动读一次。内置的模型列表走服务端，不 probe
  const neverProbed = !builtin && !agent.reported_options?.modes && !agent.reported_options?.models;
  useEffect(() => {
    if (neverProbed && online) void probe();
  }, [agent.id, online, builtin]);

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
      setErr(agentErrText(e));
    } finally {
      setBusy(false);
      if (fileRef.current) fileRef.current.value = "";
    }
  };

  const doDelete = async () => {
    setBusy(true);
    setErr("");
    try {
      await agents.remove(agent.id);
      onDeleted(agent.id);
    } catch (e) {
      setErr(agentErrText(e));
      setBusy(false);
    }
  };

  const outNow = settings ? outputModeLabel(settings.output_mode) : "…";
  const queueNow = settings ? queueTimeoutLabel(settings.queue_timeout_secs) : "…";
  const timeoutKnown =
    draft.queueTimeout === "" || QUEUE_TIMEOUT_OPTIONS.some((o) => String(o.v) === draft.queueTimeout);
  const runsOn = os ? `${computer}（${os}）` : computer;

  return (
    <section className="am-pane">
      <header className="am-head">
        <button type="button" className="am-back" onClick={onBack}>
          ← 返回
        </button>
        <AgentAvatar agentId={agent.id} name={draft.name || agent.name} version={avatarVersion} size="lg" />
        <div className="am-head-text">
          <h2>{draft.name || agent.name}</h2>
          <div className="am-sub">
            <i className={`am-dot${online ? " on" : ""}`} />
            {agentPlaceLine(computer, local, online)}
          </div>
        </div>
        <button type="button" className="primary" onClick={onOpenConv}>
          进入会话 →
        </button>
      </header>
      {notice && <div className="am-ok">{notice}</div>}
      <div className="am-tabs" role="tablist">
        {TABS.map(([id, label]) => (
          <button
            key={id}
            type="button"
            role="tab"
            aria-selected={tab === id}
            onClick={() => {
              setTab(id);
              setConfirmDel(false);
            }}
          >
            {label}
            {id === "sched" && schedCount > 0 ? ` (${schedCount})` : ""}
          </button>
        ))}
      </div>
      <div className="am-scroll">
        <div className="am-form">
          {tab === "basic" && (
            <>
              <div className="am-field">
                <span className="am-lb">头像</span>
                <div className="am-row">
                  <button
                    type="button"
                    className="am-avatar-btn"
                    aria-label="更换 Agent 头像"
                    disabled={busy}
                    onClick={() => fileRef.current?.click()}
                  >
                    <AgentAvatar agentId={agent.id} name={draft.name || agent.name} version={avatarVersion} size="lg" />
                  </button>
                  <button type="button" disabled={busy} onClick={() => fileRef.current?.click()}>
                    更换头像
                  </button>
                  <span className="am-hint">PNG / JPEG / WebP</span>
                  <input
                    ref={fileRef}
                    type="file"
                    accept="image/png,image/jpeg,image/webp"
                    hidden
                    onChange={(e) => void onAvatar(e.target.files?.[0])}
                  />
                </div>
              </div>
              <div className="am-field">
                <label htmlFor="f-name">名字</label>
                <input id="f-name" value={draft.name} onChange={(e) => patchDraft({ name: e.target.value })} />
                <span className="am-hint">
                  在会话里用 @{draft.name.trim() || "名字"} 调用它。1–32 位字母、数字、_ 或 -，不区分大小写。
                </span>
              </div>
              <div className="am-field">
                <span className="am-lb">运行在</span>
                <div className="am-ro">{runsOn} · 不能更换。要换电脑，在那台电脑上重新添加。</div>
              </div>
              <div className="am-danger">
                <b>删除 Agent</b>
                <span className="am-hint">专属会话会保留，但不能再发言。</span>
                {confirmDel ? (
                  <div className="am-row">
                    <span className="am-hint am-grow">确定删除 {draft.name || agent.name}？</span>
                    <button type="button" disabled={busy} onClick={() => setConfirmDel(false)}>
                      取消
                    </button>
                    <button type="button" className="danger" disabled={busy} onClick={() => void doDelete()}>
                      确认删除
                    </button>
                  </div>
                ) : (
                  <div>
                    <button type="button" className="danger" disabled={busy} onClick={() => setConfirmDel(true)}>
                      删除…
                    </button>
                  </div>
                )}
              </div>
            </>
          )}

          {tab === "prog" && (
            <>
              {builtin ? (
                <div className="am-field">
                  <span className="am-lb">程序</span>
                  <div className="am-ro">内置 · Chat 自带，使用管理员配置的模型</div>
                </div>
              ) : pin ? (
                <div className="am-field">
                  <span className="am-lb">程序</span>
                  <div className="am-ro">
                    {registryName || pin.id} {pin.version} · 来自登记表
                  </div>
                </div>
              ) : (
                <>
                  <div className="am-field">
                    <label htmlFor="f-cmd">启动命令</label>
                    <input id="f-cmd" value={draft.command} onChange={(e) => patchDraft({ command: e.target.value })} />
                  </div>
                  <div className="am-field">
                    <label htmlFor="f-args">参数</label>
                    <textarea
                      id="f-args"
                      rows={3}
                      value={draft.argsText}
                      onChange={(e) => patchDraft({ argsText: e.target.value })}
                    />
                    <span className="am-hint">每行一个参数。</span>
                  </div>
                </>
              )}
              <ModelModeFields
                model={draft.model}
                mode={draft.mode}
                options={builtin ? { models: builtinOpts.items } : reported}
                probing={builtin ? builtinOpts.loading : probing}
                error={probeErr}
                disabled={busy}
                hideMode={builtin}
                onModel={(model) => patchDraft({ model })}
                onMode={(mode) => patchDraft({ mode })}
                onReload={builtin ? builtinOpts.reload : () => void probe()}
              />
              <div className="am-field">
                <label htmlFor="f-instr">附加指令</label>
                <textarea
                  id="f-instr"
                  rows={4}
                  placeholder="例如：回答用中文。"
                  value={draft.instructions}
                  onChange={(e) => patchDraft({ instructions: e.target.value })}
                />
                <span className="am-hint">每次开新话题时，放在你第一条消息前面。</span>
              </div>
              <details className="am-adv">
                <summary>环境变量（{draft.envRows.length}）</summary>
                <div className="am-adv-in">
                  {draft.envRows.map((row, i) => (
                    <div className="am-kv" key={i}>
                      <input
                        aria-label={`变量名 ${i + 1}`}
                        value={row.key}
                        onChange={(e) =>
                          patchDraft({
                            envRows: draft.envRows.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)),
                          })
                        }
                      />
                      <input
                        aria-label={`变量值 ${i + 1}`}
                        value={row.value}
                        onChange={(e) =>
                          patchDraft({
                            envRows: draft.envRows.map((r, j) => (j === i ? { ...r, value: e.target.value } : r)),
                          })
                        }
                      />
                      <button
                        type="button"
                        className="am-ghost"
                        aria-label={`删除变量 ${i + 1}`}
                        onClick={() => patchDraft({ envRows: draft.envRows.filter((_, j) => j !== i) })}
                      >
                        ✕
                      </button>
                    </div>
                  ))}
                  <div>
                    <button
                      type="button"
                      onClick={() => patchDraft({ envRows: [...draft.envRows, { key: "", value: "" }] })}
                    >
                      ＋ 添加变量
                    </button>
                  </div>
                  <span className="am-hint">明文保存在服务器上，管理员和备份都能看到。</span>
                </div>
              </details>
            </>
          )}

          {tab === "perm" && (
            <>
              <div className="am-field">
                <label htmlFor="f-cwd">工作目录</label>
                <input
                  id="f-cwd"
                  value={draft.cwd}
                  placeholder="例如 D:\work\project"
                  onChange={(e) => patchDraft({ cwd: e.target.value })}
                />
                <span className="am-hint">
                  {computer} 上的绝对路径，专属会话、群里 @、互调和定时任务在这里干活；工作区对话用工作区的目录。留空则用 Chat 自己的数据目录。
                </span>
              </div>
              <div className="am-field">
                <span className="am-lb">额外目录</span>
                {draft.extraDirs.map((dir, i) => (
                  <div className="am-row" key={i}>
                    <input
                      aria-label={`额外目录 ${i + 1}`}
                      value={dir}
                      onChange={(e) =>
                        patchDraft({
                          extraDirs: draft.extraDirs.map((d, j) => (j === i ? e.target.value : d)),
                        })
                      }
                    />
                    <button
                      type="button"
                      className="am-ghost"
                      aria-label={`删除额外目录 ${i + 1}`}
                      onClick={() => patchDraft({ extraDirs: draft.extraDirs.filter((_, j) => j !== i) })}
                    >
                      ✕
                    </button>
                  </div>
                ))}
                <div>
                  <button type="button" onClick={() => patchDraft({ extraDirs: [...draft.extraDirs, ""] })}>
                    ＋ 添加目录
                  </button>
                </div>
                <span className="am-hint">「可改工作目录」档位下，这些目录里的修改同样自动放行。</span>
              </div>
              <div className="am-field">
                <span className="am-lb">权限档位</span>
                <ChoiceCards
                  label="权限档位"
                  value={draft.permissionTier || "read_only"}
                  options={PERMISSION_TIERS.map((t) => ({ id: t.id, label: t.label, hint: t.hint }))}
                  onChange={(id) => patchDraft({ permissionTier: id })}
                />
                <span className="am-hint">超出档位的操作，会在专属会话里发审批卡片给你。</span>
              </div>
            </>
          )}

          {tab === "reply" && (
            <>
              <div className="am-field">
                <span className="am-lb">输出方式</span>
                <ChoiceCards
                  label="输出方式"
                  value={draft.outputMode}
                  options={[
                    { id: "", label: "跟随默认设置", hint: followDefaultOutputHint(outNow) },
                    ...OUTPUT_MODE_OPTIONS.map((o) => ({ id: o.v, label: o.label, hint: o.hint })),
                  ]}
                  onChange={(id) => patchDraft({ outputMode: id })}
                />
              </div>
              <div className="am-field">
                <label htmlFor="f-queue">电脑离线时，排队多久后取消</label>
                <select
                  id="f-queue"
                  value={draft.queueTimeout}
                  onChange={(e) => patchDraft({ queueTimeout: e.target.value })}
                >
                  <option value="">{followDefaultQueueOption(queueNow)}</option>
                  {!timeoutKnown && <option value={draft.queueTimeout}>{draft.queueTimeout} 秒</option>}
                  {QUEUE_TIMEOUT_OPTIONS.map((o) => (
                    <option key={o.v} value={o.v}>
                      {o.label}
                    </option>
                  ))}
                </select>
                <span className="am-hint">电脑离线时发起的调用先排队，电脑上线后按顺序执行。</span>
              </div>
            </>
          )}

          <div hidden={tab !== "sched"}>
            <AgentSchedules agentId={agent.id} conversationId={agent.conversation_id} onCount={setSchedCount} />
          </div>

          {err && <div className="auth-err">{err}</div>}
        </div>
      </div>
      {bar.visible && (
        <div className="am-savebar">
          <span className="am-grow">{bar.text}</span>
          <button type="button" disabled={busy} onClick={discard}>
            放弃
          </button>
          <button
            type="button"
            className="primary"
            disabled={busy || !draft.name.trim() || (!pin && !builtin && !draft.command.trim())}
            onClick={() => void save()}
          >
            {busy ? "保存中…" : "保存"}
          </button>
        </div>
      )}
    </section>
  );
}
