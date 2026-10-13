import { useEffect, useRef, useState } from "react";
import {
  auth,
  confirmServerRemove,
  getServer,
  inTauri,
  isSameOriginServer,
  listServers,
  removeServer,
  saveSession,
  serverInfo,
  serverLabel,
  serverScope,
  setServer,
  takeKickReason,
} from "../api";
import { readDrafts } from "../drafts";
import { parseInviteParam } from "../invite";
import {
  firstEmptyCredential,
  isImeEnter,
  needsInsecureConfirm,
  planServerEnter,
  requestFormSubmit,
  resolveServerBlur,
  resolveServerInput,
  type ServerEntry,
} from "../servers";
import { setState } from "../store";
import type { RegistrationMode } from "../types";
import { startWs } from "../ws";

export default function AuthScreen() {
  const [mode, setMode] = useState<"login" | "register">(
    // ?invite=CODE → 直接进注册并预填邀请码
    () => (parseInviteParam(window.location.search) ? "register" : "login"),
  );
  const [inviteCode, setInviteCode] = useState(() =>
    parseInviteParam(window.location.search),
  );
  const [regMode, setRegMode] = useState<RegistrationMode>("open");
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [server, setServerUrl] = useState(getServer());
  const [servers, setServers] = useState<ServerEntry[]>(() => listServers());
  const [err, setErr] = useState(takeKickReason);
  const [busy, setBusy] = useState(false);
  // 服务器地址收在卡片下方，默认折叠
  const [editServer, setEditServer] = useState(false);
  const formRef = useRef<HTMLFormElement>(null);
  const usernameRef = useRef<HTMLInputElement>(null);
  const passwordRef = useRef<HTMLInputElement>(null);
  const serverInputRef = useRef<HTMLInputElement>(null);
  // window.confirm 开着时页面失焦会触发地址框 onBlur；旗标挡住失焦提交，
  // 让「https→http 确认取消 / 删除确认取消」不把地址框里未提交的输入落盘（F3）
  const confirmingRef = useRef(false);

  // 挂载时 + 服务器地址输入变化（防抖 ~500ms）探测目标服务器的注册模式；
  // 探测失败按 open（serverInfo 内部已兜底）
  useEffect(() => {
    let stale = false;
    const t = setTimeout(() => {
      void serverInfo(server).then((i) => {
        if (!stale) setRegMode(i.registration_mode);
      });
    }, 500);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [server]);

  // 关闭注册时注册入口整体消失（含已在注册页的情况）
  const effMode = regMode === "closed" ? "login" : mode;

  // 桌面壳的页面来自 tauri://localhost，没有「同源服务器」，地址留空只会连到自己
  const desktop = inTauri();
  const cur = getServer();

  /** 选中列表里的服务器（本标签页换到它；登录成功才会进列表顶部排序） */
  const pick = (url: string) => {
    setServer(url);
    setServerUrl(getServer());
    setErr("");
    setEditServer(false);
  };

  /** 弹原生确认框；期间按住 confirmingRef，地址框的 onBlur 不得提交（F3） */
  const ask = (fn: () => boolean) => {
    confirmingRef.current = true;
    try {
      return fn();
    } finally {
      // 归还焦点时可能补发失焦事件，旗标多活一个宏任务
      setTimeout(() => {
        confirmingRef.current = false;
      }, 0);
    }
  };

  /** 删除已保存的服务器条目：凭据/缓存/未发出的消息一并清掉 */
  const drop = (sv: ServerEntry) => {
    if (!ask(() => confirmServerRemove(sv))) return;
    const fallback = removeServer(sv.url);
    setServers(listServers());
    if (sv.url === cur) {
      // 删的是当前选中的：换到最近使用的另一个（没有则默认）
      setServer(fallback);
      setServerUrl(getServer());
    }
  };

  /** 地址框输入合法时保存为当前服务器并把输入框刷成规范化地址；顺带清掉旧地址错误（F7） */
  const commitServer = (url: string) => {
    setServer(url);
    setServerUrl(url);
    setEditServer(false);
    setErr("");
  };

  /** 焦点移到指定凭据字段（F5/F8 共用） */
  const focusCred = (field: "username" | "password") =>
    (field === "username" ? usernameRef : passwordRef).current?.focus();

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (busy) return;
    setErr("");
    // 提交一律用地址框当前输入：有未保存的改动先提交再登录，不等 blur
    const r0 = resolveServerInput(server, { desktop, origin: location.origin });
    if (!r0.ok) {
      setErr(r0.err);
      // 错误不能藏在折叠面板里：展开并聚焦地址框（F2 + F8）
      setEditServer(true);
      setTimeout(() => serverInputRef.current?.focus(), 0);
      return;
    }
    const norm = r0.url;
    // https 页面连 http 服务器会被浏览器拦截，先说清楚再放行；取消则不保存地址
    if (
      needsInsecureConfirm(desktop, location.protocol, norm) &&
      !ask(() => window.confirm("浏览器会阻止 https 页面访问 http 服务器，仍要继续吗？"))
    ) {
      return;
    }
    setBusy(true);
    try {
      commitServer(norm);
      const r =
        effMode === "login"
          ? await auth.login(username.trim(), password)
          : await auth.register(
              username.trim(),
              password,
              displayName.trim() || undefined,
              inviteCode.trim() || undefined,
            );
      saveSession(r);
      setState({ me: r.user, drafts: readDrafts(localStorage, serverScope(), r.user.id) });
      void startWs();
    } catch (ex) {
      // fetch 连不上时只抛 TypeError（Safari 写作 "Load failed"），带上地址才好排查
      setErr(
        ex instanceof TypeError
          ? `连不上服务器 ${norm}`
          : ex instanceof Error
            ? ex.message
            : "请求失败",
      );
      // 失败后焦点别丢到 body：回第一个空凭据字段，否则密码框（F8）
      focusCred(firstEmptyCredential(username, password) ?? "password");
    } finally {
      setBusy(false);
    }
  };

  // 鼠标按下就拦下默认的焦点转移：地址框不失焦→面板不收起→卡片不位移，
  // mouseup 时按钮还在原处，点击不会落空（T6 桌面端登录按钮失灵）
  const keepFocus = (e: React.MouseEvent) => e.preventDefault();

  // F9：preventDefault 后 Firefox 不出 :active；按下加 .pressed、抬起/移出/取消
  // 移除，样式与 :active 一致且不改布局（卡片不能位移，否则点击又落空）
  const unpress = (e: React.PointerEvent<HTMLElement>) =>
    e.currentTarget.classList.remove("pressed");
  const pressable = {
    onPointerDown: (e: React.PointerEvent<HTMLElement>) =>
      e.currentTarget.classList.add("pressed"),
    onPointerUp: unpress,
    onPointerLeave: unpress,
    onPointerCancel: unpress,
  };

  /** 地址框回车：动作由 planServerEnter 定（IME/忙碌不动、凭据空只存地址、齐全则提交） */
  const onServerEnter = (e: React.KeyboardEvent) => {
    if (e.key !== "Enter") return;
    const plan = planServerEnter({
      composing: isImeEnter(e.nativeEvent), // F1：输入法选词的 Enter 不算提交
      busy, // F6：登录在飞时什么都不做
      input: server,
      username,
      password,
      desktop,
      origin: location.origin,
    });
    if (plan.t === "ignore") return; // 不 preventDefault，完全不动
    e.preventDefault();
    if (plan.t === "err") {
      setErr(plan.err); // 面板仍展开，错误看得见（F2）
      return;
    }
    if (plan.t === "focus") {
      // F5：只提交地址，不发表单；聚焦第一个空凭据字段
      commitServer(plan.url);
      focusCred(plan.field);
      return;
    }
    // submit：地址先不落盘，submit() 里 https→http 确认通过才 commitServer（取消=不保存）
    requestFormSubmit(formRef.current); // F4：老 WebKit 无 requestSubmit 时派发 submit 事件
  };

  return (
    <div className="auth-wrap">
      <div className="auth-col">
        <form className="auth-card" onSubmit={submit} ref={formRef}>
          <div className="auth-head">
            <h1 className="auth-title">Chat</h1>
            {regMode !== "closed" && (
              <button
                type="button"
                className="auth-switch"
                onMouseDown={keepFocus}
                {...pressable}
                onClick={() => setMode(mode === "login" ? "register" : "login")}
              >
                {mode === "login" ? "注册 →" : "← 登录"}
              </button>
            )}
          </div>
          <input
            ref={usernameRef}
            placeholder="用户名（3-20 位字母数字下划线）"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            autoComplete="username"
            required
          />
          {effMode === "register" && (
            <input
              placeholder="昵称（可留空，之后可改）"
              value={displayName}
              onChange={(e) => setDisplayName(e.target.value)}
            />
          )}
          {effMode === "register" && regMode === "invite" && (
            <input
              placeholder="邀请码"
              value={inviteCode}
              onChange={(e) => setInviteCode(e.target.value)}
              required
            />
          )}
          <input
            ref={passwordRef}
            type="password"
            placeholder="密码（至少 6 位）"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete={
              effMode === "login" ? "current-password" : "new-password"
            }
            required
          />
          {regMode === "closed" && (
            <div className="auth-note">管理员已关闭注册</div>
          )}
          {err && (
            <div className="auth-err" role="alert">
              {err}
            </div>
          )}
          <button className="primary" disabled={busy} onMouseDown={keepFocus} {...pressable}>
            {busy ? "请稍候…" : effMode === "login" ? "登录" : "注册并登录"}
          </button>
        </form>
        <div className="auth-server">
          {editServer ? (
            <div
              className="auth-server-form"
              onBlur={(e) => {
                // 焦点移到表单内（列表选择/删除按钮）不收起；点出表单才收起并记住地址
                if (e.currentTarget.contains(e.relatedTarget as Node)) return;
                // 确认框开着时的失焦不是真失焦：跳过提交（F3，取消必须不改已保存地址）
                if (confirmingRef.current) return;
                const r = resolveServerBlur(server, {
                  desktop,
                  origin: location.origin,
                });
                if (r.t === "collapse") return setEditServer(false);
                if (r.t === "err") return setErr(r.err); // 面板保持展开，错误不藏（F2）
                commitServer(r.url); // 不点登录也记住，顺带清掉旧错误（F7）
              }}
            >
              <input
                ref={serverInputRef}
                autoFocus
                onKeyDown={onServerEnter}
                placeholder={
                  desktop
                    ? "服务器地址，如 http://192.168.1.5:28080"
                    : "服务器地址（留空用本站，如 http://192.168.1.5:28080）"
                }
                value={server}
                onChange={(e) => setServerUrl(e.target.value)}
              />
              {servers.length > 0 && (
                <div className="auth-server-list">
                  {servers.map((sv) => (
                    <div className="auth-server-item" key={sv.url}>
                      <button
                        type="button"
                        className="auth-server-pick"
                        onMouseDown={keepFocus}
                        {...pressable}
                        onClick={() => pick(sv.url)}
                      >
                        <span>
                          {serverLabel(sv.url, sv)}
                          {sv.url === cur && <span className="muted">（当前）</span>}
                        </span>
                        <span className="muted">
                          {serverLabel(sv.url, sv) !== sv.url && `${sv.url} · `}
                          {sv.username ? `@${sv.username}` : "未登录"}
                        </span>
                      </button>
                      {!isSameOriginServer(sv.url) && (
                        <button
                          type="button"
                          className="auth-server-del"
                          title={`删除 ${sv.url}`}
                          onMouseDown={keepFocus}
                          {...pressable}
                          onClick={() => drop(sv)}
                        >
                          ×
                        </button>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <button
              type="button"
              className="auth-switch"
              onClick={() => setEditServer(true)}
            >
              服务器：{serverLabel(cur, servers.find((s) => s.url === cur))} · 修改
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
