#[tokio::main]
async fn main() {
    chat_server::run().await;
}
