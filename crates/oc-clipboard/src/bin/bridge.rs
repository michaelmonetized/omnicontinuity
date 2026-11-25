#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    oc_clipboard::bridge().await
}
