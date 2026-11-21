#[tokio::main]
async fn main() -> anyhow::Result<()> {
    almighty_server::serve(almighty_server::Config::from_env()).await
}
