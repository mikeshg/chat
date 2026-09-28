-- Agent A23：宿主离线宽限的起点。NULL 表示当前视为在线（或重启后还没观察到离线）。
-- 断开时写下 now()，重连清掉。服务端重启后内存里没有连接，清扫按这一列判断宽限是否到期。

ALTER TABLE agent_hosts
    ADD COLUMN offline_since timestamptz;
