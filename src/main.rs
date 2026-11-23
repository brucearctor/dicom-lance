use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut host: IpAddr = std::env::var("HOST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    let mut port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8090);
    let mut data_dir = std::env::var("DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"));
    let mut static_dir = std::env::var("STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./web/dist"));
    let mut seed_on_start = std::env::var("SEED_ON_START")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => {
                if let Some(v) = args.next() {
                    host = v.parse()?;
                }
            }
            "--port" => {
                if let Some(v) = args.next() {
                    port = v.parse()?;
                }
            }
            "--data-dir" => {
                if let Some(v) = args.next() {
                    data_dir = PathBuf::from(v);
                }
            }
            "--static-dir" => {
                if let Some(v) = args.next() {
                    static_dir = PathBuf::from(v);
                }
            }
            "--seed-on-start" => {
                seed_on_start = true;
            }
            _ => {}
        }
    }

    if !static_dir.exists() {
        let crate_dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/dist");
        if crate_dist.exists() {
            static_dir = crate_dist;
        }
    }

    let addr = SocketAddr::new(host, port);
    dicom_lance::api::serve(addr, data_dir, Some(static_dir), seed_on_start).await
}
