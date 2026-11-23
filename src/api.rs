use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, RANGE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::dicom_ingest::{parse_dicom_instances, IngestError};
use crate::phantom::{generate_phantom_dicom_files, PhantomError};
use crate::store::{DicomLanceStore, IngestReport, SearchMode, StoreError, StudySummary};

const WADO_BOUNDARY: &str = "dicom-lance-boundary";

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<DicomLanceStore>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("ingest error: {0}")]
    Ingest(#[from] IngestError),
    #[error("phantom error: {0}")]
    Phantom(Box<PhantomError>),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<PhantomError> for ApiError {
    fn from(err: PhantomError) -> Self {
        Self::Phantom(Box::new(err))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Store(StoreError::NotFound(m)) => (StatusCode::NOT_FOUND, m),
            ApiError::Store(StoreError::InvalidInput(m)) => (StatusCode::BAD_REQUEST, m),
            ApiError::Ingest(IngestError::Invalid(m)) => (StatusCode::BAD_REQUEST, m),
            ApiError::Ingest(IngestError::Parse(e)) => (StatusCode::BAD_REQUEST, e.to_string()),
            ApiError::Ingest(IngestError::PixelDecode(e)) => (StatusCode::BAD_REQUEST, e.to_string()),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct SearchParams {
    pub q: Option<String>,
    pub modality: Option<String>,
    pub patient_id: Option<String>,
    pub limit: Option<usize>,
    pub mode: Option<SearchMode>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SimilarParams {
    pub limit: Option<usize>,
    pub exclude_self: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub mode: SearchMode,
    pub studies: Vec<StudySummary>,
}

#[derive(Debug, Deserialize)]
pub struct DirectoryIngestRequest {
    pub path: String,
}

pub fn build_router(store: Arc<DicomLanceStore>, static_dir: Option<PathBuf>) -> Router {
    let state = AppState { store };

    let api_router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/stats", get(get_stats))
        .route("/api/search", get(search_get).post(search_post))
        .route(
            "/api/studies/{study}/series/{series}/frames-manifest",
            get(get_frames_manifest),
        )
        .route(
            "/api/studies/{study}/series/{series}/instances/{sop}/frames/{frame}/similar",
            get(similar_frames),
        )
        .route(
            "/api/series/{study}/{series}/thumbnail",
            get(get_series_thumbnail),
        )
        .route("/api/ingest/seed-phantoms", post(seed_phantoms))
        .route("/api/ingest/upload", post(ingest_upload))
        .route("/api/ingest/directory", post(ingest_directory))
        .route("/dicomweb/studies", get(qido_studies).post(stow_studies))
        .route("/dicomweb/studies/{study}/series", get(qido_series))
        .route(
            "/dicomweb/studies/{study}/series/{series}/metadata",
            get(wado_series_metadata),
        )
        .route(
            "/dicomweb/studies/{study}/series/{series}/instances/{sop}/frames/{frames}",
            get(wado_instance_frames),
        )
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024))
        .with_state(state);

    let router = if let Some(dir) = static_dir.filter(|p| p.exists()) {
        let index_path = dir.join("index.html");
        let serve = ServeDir::new(&dir).not_found_service(ServeFile::new(index_path));
        api_router.fallback_service(serve)
    } else {
        api_router
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    router
        .layer(cors)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("require-corp"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ))
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn get_stats(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let stats = state.store.get_stats().await?;
    Ok(Json(stats))
}

async fn search_get(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let mode = params.mode.unwrap_or_default();
    let studies = state
        .store
        .search_series_with_mode(
            params.q.as_deref(),
            params.modality.as_deref(),
            params.patient_id.as_deref(),
            params.limit.unwrap_or(50),
            mode,
        )
        .await?;
    Ok(Json(SearchResponse { mode, studies }))
}

async fn search_post(
    State(state): State<AppState>,
    Json(params): Json<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let mode = params.mode.unwrap_or_default();
    let studies = state
        .store
        .search_series_with_mode(
            params.q.as_deref(),
            params.modality.as_deref(),
            params.patient_id.as_deref(),
            params.limit.unwrap_or(50),
            mode,
        )
        .await?;
    Ok(Json(SearchResponse { mode, studies }))
}

async fn similar_frames(
    State(state): State<AppState>,
    AxumPath((study, series, sop, frame)): AxumPath<(String, String, String, i32)>,
    Query(params): Query<SimilarParams>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = params.limit.unwrap_or(6);
    let exclude_self = params.exclude_self.unwrap_or(true);
    let hits = state
        .store
        .find_similar_frames(&study, &series, &sop, frame, limit, exclude_self)
        .await?;
    Ok(Json(serde_json::json!({
        "query": {
            "study_instance_uid": study,
            "series_instance_uid": series,
            "sop_instance_uid": sop,
            "frame_number": frame,
        },
        "hits": hits,
    })))
}

async fn get_frames_manifest(
    State(state): State<AppState>,
    AxumPath((study, series)): AxumPath<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let frames = state
        .store
        .get_series_frame_descriptors(&study, &series)
        .await?;
    Ok(Json(serde_json::json!({ "frames": frames })))
}

async fn get_series_thumbnail(
    State(state): State<AppState>,
    AxumPath((study, series)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let bytes = state
        .store
        .get_series_thumbnail(&study, &series)
        .await?
        .ok_or_else(|| ApiError::NotFound("thumbnail not found".to_string()))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "image/png")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

pub async fn seed_phantoms_into_store(store: &DicomLanceStore) -> Result<IngestReport, ApiError> {
    let (phantom_dir, cleanup_dir) = if let Some(local_root) = store.local_root_dir() {
        (local_root.join("phantoms"), None)
    } else {
        let unique = format!(
            "dicom-lance-phantoms-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmp = std::env::temp_dir().join(unique);
        (tmp.clone(), Some(tmp))
    };

    let gen_dir = phantom_dir.clone();
    let paths = tokio::task::spawn_blocking(move || generate_phantom_dicom_files(&gen_dir))
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))??;
    let result = ingest_paths(store, &paths).await;
    if let Some(dir) = cleanup_dir {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    result
}

async fn seed_phantoms(State(state): State<AppState>) -> Result<Json<IngestReport>, ApiError> {
    let report = seed_phantoms_into_store(&state.store).await?;
    Ok(Json(report))
}

async fn ingest_upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<IngestReport>, ApiError> {
    let mut buffers = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart error: {e}")))?
    {
        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::BadRequest(format!("failed reading multipart bytes: {e}")))?;
        if !data.is_empty() {
            buffers.push(data);
        }
    }
    if buffers.is_empty() {
        return Err(ApiError::BadRequest(
            "no DICOM files uploaded in multipart request".to_string(),
        ));
    }
    let parsed = tokio::task::spawn_blocking(move || parse_dicom_instances(&buffers))
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))??;
    let report = state.store.ingest(&parsed).await?;
    Ok(Json(report))
}

fn collect_dcm_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            collect_dcm_files_recursive(&p, out)?;
        } else if p.is_file() {
            let ext_ok = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("dcm") || e.eq_ignore_ascii_case("dicom"))
                .unwrap_or(true);
            if ext_ok && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                out.push(p);
            }
        }
    }
    Ok(())
}

async fn ingest_paths(store: &DicomLanceStore, paths: &[PathBuf]) -> Result<IngestReport, ApiError> {
    let mut raw_files = Vec::with_capacity(paths.len());
    for p in paths {
        let bytes = tokio::fs::read(p).await?;
        raw_files.push(Bytes::from(bytes));
    }
    let parsed = tokio::task::spawn_blocking(move || parse_dicom_instances(&raw_files))
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))??;
    let report = store.ingest(&parsed).await?;
    Ok(report)
}

async fn ingest_directory(
    State(state): State<AppState>,
    Json(req): Json<DirectoryIngestRequest>,
) -> Result<Json<IngestReport>, ApiError> {
    let dir = PathBuf::from(&req.path);
    if !dir.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "directory does not exist: {}",
            req.path
        )));
    }
    let paths = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>, std::io::Error> {
        let mut out = Vec::new();
        collect_dcm_files_recursive(&dir, &mut out)?;
        out.sort();
        Ok(out)
    })
    .await
    .map_err(|e| ApiError::BadRequest(e.to_string()))??;
    if paths.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "no DICOM files found in {}",
            req.path
        )));
    }
    let report = ingest_paths(&state.store, &paths).await?;
    Ok(Json(report))
}

pub fn extract_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let trimmed = part.trim();
        if let Some((key, val)) = trimmed.split_once('=') {
            if key.trim().eq_ignore_ascii_case("boundary") {
                let val_trimmed = val.trim();
                let unquoted = val_trimmed
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(val_trimmed);
                if unquoted.is_empty() || unquoted.contains(['\r', '\n']) {
                    return None;
                }
                return Some(unquoted.to_string());
            }
        }
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn encode_multipart_related(boundary: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for &(content_type, payload) in parts {
        let part_header = format!(
            "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        body.extend_from_slice(part_header.as_bytes());
        body.extend_from_slice(payload);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

pub fn split_multipart_related(body: &[u8], boundary: &str) -> Vec<Bytes> {
    if boundary.is_empty() || boundary.contains(['\r', '\n']) {
        return Vec::new();
    }
    let delim = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut pos = 0usize;

    while let Some(rel) = find_bytes(&body[pos..], &delim) {
        let start = pos + rel + delim.len();
        if start + 2 <= body.len() && &body[start..start + 2] == b"--" {
            break;
        }
        let header_start = if start + 2 <= body.len() && &body[start..start + 2] == b"\r\n" {
            start + 2
        } else if start < body.len() && body[start] == b'\n' {
            start + 1
        } else {
            start
        };

        let next_match = find_bytes(&body[header_start..], &delim);
        let has_next_delim = next_match.is_some();
        let next_delim = next_match
            .map(|r| header_start + r)
            .unwrap_or(body.len());

        let segment = &body[header_start..next_delim];
        let payload_start = if let Some(idx) = find_bytes(segment, b"\r\n\r\n") {
            Some(idx + 4)
        } else {
            find_bytes(segment, b"\n\n").map(|idx| idx + 2)
        };

        if let Some(p_start) = payload_start {
            let mut p_end = segment.len();
            if has_next_delim {
                if p_end >= p_start + 2 && &segment[p_end - 2..p_end] == b"\r\n" {
                    p_end -= 2;
                } else if p_end > p_start && segment[p_end - 1] == b'\n' {
                    p_end -= 1;
                }
            }
            if p_start <= p_end {
                parts.push(Bytes::copy_from_slice(&segment[p_start..p_end]));
            }
        }
        pos = next_delim;
    }

    parts
}

async fn stow_studies(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let ct = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let buffers = if let Some(boundary) = extract_boundary(ct) {
        let parts: Vec<Bytes> = split_multipart_related(&body, &boundary)
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(ApiError::BadRequest(
                "STOW-RS multipart/related contained zero DICOM parts".to_string(),
            ));
        }
        parts
    } else if !body.is_empty() {
        vec![body]
    } else {
        return Err(ApiError::BadRequest("empty STOW-RS body".to_string()));
    };

    let parsed = tokio::task::spawn_blocking(move || parse_dicom_instances(&buffers))
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))??;
    let report = state.store.ingest(&parsed).await?;

    let resp_json = serde_json::json!({
        "00081190": { "vr": "UR", "Value": report.study_uids.iter().map(|u| format!("/dicomweb/studies/{u}")).collect::<Vec<_>>() },
        "series_ingested": report.series_ingested,
        "frames_ingested": report.frames_ingested,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/dicom+json")
        .body(Body::from(serde_json::to_vec(&resp_json).unwrap_or_default()))
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

async fn qido_studies(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Response, ApiError> {
    let studies = state
        .store
        .search_series_with_mode(
            params.q.as_deref(),
            params.modality.as_deref(),
            params.patient_id.as_deref(),
            params.limit.unwrap_or(50),
            params.mode.unwrap_or_default(),
        )
        .await?;

    let items: Vec<serde_json::Value> = studies
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "0020000D": { "vr": "UI", "Value": [s.study_instance_uid] },
                "00100020": { "vr": "LO", "Value": [s.patient_id] },
                "00100010": { "vr": "PN", "Value": [{ "Alphabetic": s.patient_name }] },
                "00080050": { "vr": "SH", "Value": [s.accession_number] },
                "00080020": { "vr": "DA", "Value": [s.study_date] },
                "00081030": { "vr": "LO", "Value": [s.study_description] },
                "00080061": { "vr": "CS", "Value": s.modalities },
                "00201206": { "vr": "IS", "Value": [s.series.len()] },
            })
        })
        .collect();

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/dicom+json")
        .body(Body::from(serde_json::to_vec(&items).unwrap_or_default()))
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

async fn qido_series(
    State(state): State<AppState>,
    AxumPath(study): AxumPath<String>,
) -> Result<Response, ApiError> {
    let studies = state.store.search_series(None, None, None, 200).await?;
    let series_list = studies
        .into_iter()
        .find(|s| s.study_instance_uid == study)
        .map(|s| s.series)
        .unwrap_or_default();

    let items: Vec<serde_json::Value> = series_list
        .into_iter()
        .map(|ser| {
            serde_json::json!({
                "0020000D": { "vr": "UI", "Value": [ser.study_instance_uid] },
                "0020000E": { "vr": "UI", "Value": [ser.series_instance_uid] },
                "00080060": { "vr": "CS", "Value": [ser.modality] },
                "00200011": { "vr": "IS", "Value": [ser.series_number] },
                "0008103E": { "vr": "LO", "Value": [ser.series_description] },
                "00180015": { "vr": "CS", "Value": [ser.body_part_examined] },
                "00201209": { "vr": "IS", "Value": [ser.instance_count] },
            })
        })
        .collect();

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/dicom+json")
        .body(Body::from(serde_json::to_vec(&items).unwrap_or_default()))
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

async fn wado_series_metadata(
    State(state): State<AppState>,
    AxumPath((study, series)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let meta = state.store.get_series_wado_metadata(&study, &series).await?;
    if meta.is_empty() {
        return Err(ApiError::NotFound(format!(
            "no metadata found for series {series}"
        )));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/dicom+json")
        .body(Body::from(serde_json::to_vec(&meta).unwrap_or_default()))
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

pub fn parse_range_header(headers: &HeaderMap) -> Option<Range<u64>> {
    let raw = headers.get(RANGE)?.to_str().ok()?.trim();
    let prefix = raw.get(..6)?;
    if !prefix.eq_ignore_ascii_case("bytes=") {
        return None;
    }
    let spec = &raw[6..];
    let (start_s, end_s) = spec.split_once('-')?;
    let start = start_s.trim().parse::<u64>().ok()?;
    let end_trimmed = end_s.trim();
    if end_trimmed.is_empty() {
        return Some(start..u64::MAX);
    }
    let end_inclusive = end_trimmed.parse::<u64>().ok()?;
    if end_inclusive < start {
        return None;
    }
    let end_exclusive = end_inclusive.checked_add(1)?;
    Some(start..end_exclusive)
}

pub fn parse_frame_numbers(frames_str: &str) -> Result<Vec<i32>, ApiError> {
    let trimmed = frames_str.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "invalid frame list: {frames_str}"
        )));
    }
    let mut out = Vec::new();
    for seg in trimmed.split(',') {
        let n = seg.trim().parse::<i32>().map_err(|_| {
            ApiError::BadRequest(format!("invalid frame list: {frames_str}"))
        })?;
        if n < 1 {
            return Err(ApiError::BadRequest(format!(
                "invalid frame number {n} in frame list: {frames_str}"
            )));
        }
        out.push(n);
    }
    Ok(out)
}

async fn wado_instance_frames(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((study, series, sop, frames_str)): AxumPath<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    let frame_numbers = parse_frame_numbers(&frames_str)?;

    let byte_range = parse_range_header(&headers);
    let _accept = headers.get(ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let payloads = state
        .store
        .read_frame_blobs(&study, &series, &sop, &frame_numbers, byte_range.clone())
        .await?;

    let part_specs: Vec<(String, &[u8])> = payloads
        .iter()
        .map(|p| {
            (
                format!(
                    "application/octet-stream; transfer-syntax={}",
                    p.transfer_syntax_uid
                ),
                p.bytes.as_slice(),
            )
        })
        .collect();
    let part_refs: Vec<(&str, &[u8])> = part_specs
        .iter()
        .map(|(ct, bytes)| (ct.as_str(), *bytes))
        .collect();
    let body = encode_multipart_related(WADO_BOUNDARY, &part_refs);

    let ct_header = format!(
        "multipart/related; type=\"application/octet-stream\"; boundary=\"{WADO_BOUNDARY}\""
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, ct_header)
        .body(Body::from(body))
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

pub async fn serve(
    addr: SocketAddr,
    data_dir: PathBuf,
    static_dir: Option<PathBuf>,
    seed_on_start: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(DicomLanceStore::new(data_dir));
    serve_with_store(addr, store, static_dir, seed_on_start).await
}

pub async fn serve_with_store(
    addr: SocketAddr,
    store: Arc<DicomLanceStore>,
    static_dir: Option<PathBuf>,
    seed_on_start: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if seed_on_start {
        let stats = store.get_stats().await?;
        if stats.series_dataset.row_count == 0 {
            let report = seed_phantoms_into_store(&store).await?;
            tracing::info!(
                series = report.series_ingested,
                frames = report.frames_ingested,
                "seeded initial 3D DICOM phantoms into Lance v2.2"
            );
        }
    }

    let router = build_router(store, static_dir);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "dicom-lance server listening");
    axum::serve(listener, router).await?;
    Ok(())
}

pub fn collect_study_map(studies: &[StudySummary]) -> BTreeMap<&str, usize> {
    studies
        .iter()
        .map(|s| (s.study_instance_uid.as_str(), s.series.len()))
        .collect()
}
