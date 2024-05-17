#[tokio::main]
async fn main() {
    bacon_ls::BaconLs::serve().await
}
