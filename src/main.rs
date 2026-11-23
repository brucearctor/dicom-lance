use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use dicom_lance::embedding::HttpEmbedder;
use dicom_lance::store::DicomLanceStore;
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
    let mut dataset_uri = std::env::var("DATASET_URI")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("DATA_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "./data".to_string());
    let mut static_dir = std::env::var("STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./web/dist"));
    let mut seed_on_start = std::env::var("SEED_ON_START")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let mut embedder_url = std::env::var("EMBEDDER_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let mut embedder_api_key = std::env::var("EMBEDDER_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let mut embedder_model = std::env::var("EMBEDDER_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "text-embedding-3-small".to_string());

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
            "--data-dir" | "--dataset-uri" => {
                if let Some(v) = args.next() {
                    dataset_uri = v;
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
            "--embedder-url" => {
                if let Some(v) = args.next() {
                    embedder_url = Some(v);
                }
            }
            "--embedder-api-key" => {
                if let Some(v) = args.next() {
                    embedder_api_key = Some(v);
                }
            }
            "--embedder-model" => {
                if let Some(v) = args.next() {
                    embedder_model = v;
                }
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

    let mut store = DicomLanceStore::from_uri(&dataset_uri)?;
    if let Some(url) = embedder_url.filter(|u| !u.trim().is_empty()) {
        tracing::info!(embedder_url = %url, model = %embedder_model, "configured external OpenAI-compatible embedder");
        store = store.with_embedder(Arc::new(HttpEmbedder::new(
            url,
            embedder_api_key,
            embedder_model,
        )));
    }

    let addr = SocketAddr::new(host, port);
    dicom_lance::api::serve_with_store(addr, Arc::new(store), Some(static_dir), seed_on_start).await
}
