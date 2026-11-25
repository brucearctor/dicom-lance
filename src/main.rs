use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use dicom_lance::api::{parse_cors_origins, ServerConfig};
use dicom_lance::deid::DeidConfig;
use dicom_lance::embedding::HttpEmbedder;
use dicom_lance::store::DicomLanceStore;
use tracing_subscriber::EnvFilter;

fn parse_bool_env(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

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
    let mut seed_on_start = parse_bool_env("SEED_ON_START").unwrap_or(false);
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

    let mut api_token = std::env::var("API_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut cors_origins: Vec<String> = std::env::var("CORS_ORIGINS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut deid_enabled = parse_bool_env("DEIDENTIFY").unwrap_or(false);
    let mut deid_salt = std::env::var("DEIDENTIFY_SALT")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let mut allow_dir_ingest_override = parse_bool_env("ALLOW_DIRECTORY_INGEST");

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
            "--api-token" => {
                if let Some(v) = args.next() {
                    let trimmed = v.trim().to_string();
                    api_token = (!trimmed.is_empty()).then_some(trimmed);
                }
            }
            "--cors-origin" => {
                if let Some(v) = args.next() {
                    let trimmed = v.trim().to_string();
                    if !trimmed.is_empty() {
                        cors_origins.push(trimmed);
                    }
                }
            }
            "--deidentify" => {
                deid_enabled = true;
            }
            "--deidentify-salt" => {
                if let Some(v) = args.next() {
                    if !v.trim().is_empty() {
                        deid_salt = Some(v);
                    }
                }
            }
            "--allow-directory-ingest" => {
                allow_dir_ingest_override = Some(true);
            }
            "--disable-directory-ingest" => {
                allow_dir_ingest_override = Some(false);
            }
            _ => {}
        }
    }

    parse_cors_origins(&cors_origins)?;
    let allow_directory_ingest = allow_dir_ingest_override.unwrap_or_else(|| host.is_loopback());

    let default_deid = DeidConfig::default();
    let deid = DeidConfig {
        enabled: deid_enabled,
        salt: deid_salt.unwrap_or(default_deid.salt),
        ..default_deid
    };

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

    let server_config = ServerConfig {
        api_token,
        cors_origins,
        allow_directory_ingest,
        deid,
    };
    tracing::info!(
        auth_enabled = server_config.api_token.is_some(),
        cors_allowlist_count = server_config.cors_origins.len(),
        allow_directory_ingest = server_config.allow_directory_ingest,
        deidentify_enabled = server_config.deid.enabled,
        "security configuration initialized"
    );

    let addr = SocketAddr::new(host, port);
    dicom_lance::api::serve_with_config(
        addr,
        Arc::new(store),
        Some(static_dir),
        seed_on_start,
        server_config,
    )
    .await
}
