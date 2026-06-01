#[tokio::main]
async fn main() {
    std::process::exit(codex_threads::run().await);
}
