use packctl::error::Result;

#[tokio::main]
async fn main() -> Result<()> {
    packctl::cli::run().await
}
