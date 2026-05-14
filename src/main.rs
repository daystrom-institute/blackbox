#[tokio::main]
async fn main() -> anyhow::Result<()> {
    blackbox::server::run().await
}
