//! Bacon Language Server
use bacon_ls::BaconLs;

#[tokio::main]
async fn main() {
    let args: bacon_ls::Args = argh::from_env();
    if args.version {
        println!("{}", bacon_ls::PKG_VERSION);
    } else {
        BaconLs::serve().await;
    }
}
