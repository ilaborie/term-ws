use term_ws::launch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // configure tracing
    tracing_subscriber::fmt()
        .pretty()
        .try_init()
        .map_err(|err| anyhow::anyhow!(err))?;

    // TODO args
    let port = 8000;

    launch(port).await?;

    Ok(())
}
