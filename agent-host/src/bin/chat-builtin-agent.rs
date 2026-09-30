// Agent A29：内置 agent 的独立程序入口（ACP over stdio，NDJSON）。
// 宿主按内置形状把它当普通 agent 子进程拉起；全部输出去 stderr，
// stdout 只走协议帧。

#[tokio::main]
async fn main() {
    if let Err(e) = agent_host::builtin::run_stdio().await {
        eprintln!("chat-builtin-agent: {e}");
        std::process::exit(1);
    }
}
