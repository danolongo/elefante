#[tokio::main]
async fn main() -> anyhow::Result<()> {
    elefante_srv::run_from_env().await
}
