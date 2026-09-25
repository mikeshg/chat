# Devin C2 执行单：群投票

你负责投票相关 migration/model/domain/API/WS 与最小 UI/tests。你不是独占代码库：A/B 已 idle，主 agent 只维护设计/进度，D 后续接手；保留全部现有未提交改动。只做投票，不新增依赖，不重写模块，不 reset/commit/push。

读取主设计 §8.4 与 delivery-A/B。立即停止无效 powershell 嵌套引号探测，使用你的 read/search/edit 工具。多行命令经过 paseo.cmd 曾被截断，以本文件为准。

锁定决策（无需再权衡）：
- 0009_polls.sql 新建 polls(message_id PK/FK, question, allow_multiple, anonymous, closed_at, expires_at, version)、poll_options(id,poll_id,option_index,label)、poll_votes(poll_id,option_id,user_id)。选项/投票复合外键保证归属。
- question 明确落库；MessageView.poll 可选；旧客户端 content='[投票] question'。
- REST POST /conversations/{id}/polls 创建，复用 post_message 事务+client_msg_id；PUT /messages/{id}/poll/votes 替换本人选择（[] 撤票）；POST /messages/{id}/poll/close 仅创建者或群主。
- 群成员才可创建/投票；过期、关闭、删除后拒绝投票；日期按 DB 时间；锁顺序 conversation→message→poll。
- 真正的投票变化同时升 polls.version 和 messages.edit_version；消息 seq 不变。重复相同选项无变更。
- 完整 MessageView 是唯一客户端版本合并契约。创建广播已有 message；投票/关闭广播已有 message_updated 完整个性化 MessageView，避免新造复杂事件版本或冗余 poll_created。
- 无 N+1：在 read model 批量/SQL 聚合投票；匿名不返回 voter IDs/names，仅本人 my_option_ids、各选项 count 和 total_voters。
- UI 最小卡片+创建弹窗（2–10 选项、单/多选、匿名、截止）、投票/撤票/关闭；错误和 busy 态，过期禁用。可复用 REST，无须再造独立 WS mutation API；在 delivery-C 记录此落地口径。
- 测试覆盖权限、约束、重复/并发替换、关闭/过期/删除、匿名、seq 不变、反序全视图合并；运行 Rust 及前端现有测试/build。
- 现有 29 个 Rust / 9 个前端测试须保留通过。只用测试数据库，禁止探测或写入 :8080（是其他项目）。不要 DROP/重建任何已有库。
- 交付 docs/design/delivery-C.md，真实记录命令和结果、未覆盖项；临时文件仅清理你自己创建且确认路径的文件。

先写 migration+model/domain 小步编译，再 UI+测试，不要再花一轮讨论 schema。
