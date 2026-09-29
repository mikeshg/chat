-- Agent A27：每台电脑一个路径为空串的「无工作区」分组。
-- 它下面的对话由电脑放在程序安装目录下的 tmp/<对话 id> 里干活。
ALTER TABLE agent_workspaces DROP CONSTRAINT agent_workspaces_path_check;
ALTER TABLE agent_workspaces
    ADD CONSTRAINT agent_workspaces_path_check CHECK (char_length(path) <= 500);
