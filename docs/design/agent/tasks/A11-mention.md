# A11 在任意私聊或群里 @ Agent

对应 Gitea issue xiefan/chat#13（先看 issue 里 A01 审查备注）。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q3、Q19）、`CONTEXT.md`「Agent」小节的「谁能调用 / 在哪调用 / @ 解析」、[../delivery/A01.md](../delivery/A01.md)、[../delivery/A02.md](../delivery/A02.md)、[../delivery/A04.md](../delivery/A04.md)。本票**没有迁移**。

## 背景

现在只能在 AgentConversation 里调用 Agent。本票让主人在自己所在的任意私聊 / 群里 `@名字 指令` 调用自己的 Agent，回复出现在原会话，成员都能看到。别人 @ 不触发。

同批并行：A03（输出方式，会改状态标签、`agents.rs` 的帧处理）、A08（使用权）、A09（托盘）。

## 一、服务端

- WS `send_message` 加可选字段 `agent_targets: [agent_id, …]`：客户端从补全里选中的 Agent。
- 发出一条文字消息后（沿用 A01 的 `maybe_invoke` 挂点并扩展，**不要**再建第二个挂点）：
  - AgentConversation：保持现状（主人发的就调用它的 Agent）。
  - 私聊 / 群：目标集合 = `agent_targets` 中**属于发送者且未删除**的 Agent ∪ 正文里手打的 `@token`（没被显式指定过的）按「先匹配发送者自己的 Agent（不区分大小写），再匹配群成员」解析出的 Agent。去重后每个 Agent 建一个调用（`conversation_id` = 该会话、`trigger_message_id` = 这条消息）。
  - 别人的 Agent id 出现在 `agent_targets` 里一律忽略；非主人发的消息不会因为同名而触发任何 Agent。
  - 群成员 @ 提及的解析要同步调整：手打的 `@token` 若被解析成了发送者自己的 Agent，就不再算作对同名群成员的提及（`parse_mentions` 需要知道这些 token）。
  - 只读私聊（好友已解除）里发不出消息，自然不会调用。
- AgentContext：`context_key` 用会话 id（A02 机制不变），所以每个会话各有一条上下文。
- 状态帧：`agent_invocation` 推给**会话全部成员**（A01 的 `handle_finish` 等处写死了 `member_ids = [owner]`，全部改成按会话成员推）。
- Agent 的回复：以 Agent 身份发到原会话。A01 在 `write_message` 里有一条「Agent 消息只能发在它自己的专属会话」的校验，改成「只能发在主人是成员的会话里」。回复计入每个成员的未读（包括主人），`@` 提及解析对 Agent 消息关闭。
- 宿主离线、取消、超时沿用 A04（群里调用同样排队，排队状态群成员可见）；取消接口仍只允许主人。
- `GET /v1/conversations/{id}/agent-invocations` 本来就按成员校验，群 / 私聊直接可用。

## 二、前端

- 输入框的 @ 补全（`ChatView.tsx` 里现有的群成员补全）在私聊和群里都加入**主人自己的 Agent**（从 `GET /agents` 取，已删除的不显示），带 🤖 标记和宿主在线状态；Agent 排在同名成员前面。选中 Agent 时把 id 记进本条消息的 `agent_targets`（删掉对应 `@名字` 文本后要从集合里去掉）。私聊里原来没有 @ 补全，这里只补 Agent。
- 「这条 @ 解析成谁」写成纯函数（输入：正文、显式选中的集合、我的 Agent、群成员；输出：Agent 目标、成员提及），便于 vitest。
- 调用状态行（`InvocationStatus`）在所有会话的触发消息下显示（现在只在 agent 会话显示）；打开任意会话时按需拉一次调用历史。群成员看得到状态，但「取消」按钮只对主人显示。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A11 =====`：

1. 群里主人发 `@bot echo:hi`（手打，不带 `agent_targets`）→ 群里出现 Agent 回复 `hi`，另一个群成员通过 WS 收到这条回复和 `agent_invocation` 帧；该成员和主人的未读都 +1。
2. 群里另一个成员发 `@bot echo:x`（bot 是主人的）→ 不产生调用。
3. 群里有用户名叫 `bot` 的成员、主人也有 Agent 叫 `bot`：主人手打 `@bot echo:a` → 调用 Agent，**不**提及成员 `bot`；其他成员发 `@bot` → 提及成员 `bot`、不调用。（主人想 @ 同名成员时服务端无法区分，按产品决定优先解析为 Agent，这是已知限制，写进交付文档。）
4. `agent_targets` 带别人的 Agent id → 忽略。
5. 一条消息 `@bot1 @bot2 echo:z` → 两个调用、两条回复。
6. 私聊里 `@bot recall` 与专属会话里的上下文互不影响（用 A02 的 `recall`）。
7. 群里调用时宿主离线 → 群成员看到 `queued`；宿主上线后回复出现在群里。
8. 已删除的 Agent 不被触发。

Vitest：@ 解析纯函数（显式选中、手打优先级、同名成员、大小写）。浏览器实测（无头 Playwright）：两个账号在同一个群，主人在补全里选 Agent 发送，另一账号实时看到 Agent 回复与状态；截图放 `server/target/wt/`。

## 不做

- 审批在群里的呈现（A12）；子调用（A14）；别人调用我的 Agent（产品决定不做）。
