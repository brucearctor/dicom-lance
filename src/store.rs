use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type, Int32Type, UInt64Type};
use arrow_array::{Array, Float32Array, RecordBatch, RecordBatchIterator};
use futures::TryStreamExt;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::{MergeInsertBuilder, WhenMatched, WhenNotMatched, WriteMode, WriteParams};
use lance::index::vector::VectorIndexParams;
use lance::index::DatasetIndexExt;
use lance::Dataset;
use lance_core::datatypes::BlobKind;
use lance_core::ROW_ID;
use lance_file::version::LanceFileVersion;
use lance_index::scalar::{FullTextSearchQuery, InvertedIndexParams, ScalarIndexParams};
use lance_index::IndexType;
use lance_linalg::distance::MetricType;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::dicom_ingest::IngestedSeries;
use crate::embedding::{rrf_fuse, EmbedError, Embedder, LocalMultimodalEmbedder};
use crate::schema::{build_frames_batch, build_series_batch, SchemaError};

const SERIES_TABLE: &str = "dicom_series.lance";
const FRAMES_TABLE: &str = "dicom_frames.lance";
const FTS_INDEX_NAME: &str = "searchable_text_fts";
const SERIES_VEC_INDEX_NAME: &str = "series_embedding_ivf_flat";
const FRAMES_VEC_INDEX_NAME: &str = "frame_embedding_ivf_flat";
const IVF_MIN_ROWS: usize = 256;

const SERIES_PROJECTION: &[&str] = &[
    "study_instance_uid",
    "series_instance_uid",
    "patient_id",
    "patient_name",
    "accession_number",
    "study_date",
    "study_description",
    "modality",
    "body_part_examined",
    "series_number",
    "series_description",
    "instance_count",
    "frame_count",
    "rows",
    "columns",
    "spacing_x_mm",
    "spacing_y_mm",
    "spacing_z_mm",
    "window_center_default",
    "window_width_default",
    "is_volumetric",
    "sr_text",
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),
    #[error("lance error: {0}")]
    Lance(#[from] lance::Error),
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("embedding error: {0}")]
    Embed(#[from] EmbedError),
    #[error("invalid parameter: {0}")]
    InvalidInput(String),
    #[error("not found: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageRoot {
    Local(PathBuf),
    Remote(String),
}

impl StorageRoot {
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(p) => Some(p.as_path()),
            Self::Remote(_) => None,
        }
    }

    pub fn display_uri(&self) -> String {
        match self {
            Self::Local(p) => p.to_string_lossy().to_string(),
            Self::Remote(u) => u.clone(),
        }
    }

    pub fn series_uri(&self) -> String {
        self.table_uri(SERIES_TABLE)
    }

    pub fn frames_uri(&self) -> String {
        self.table_uri(FRAMES_TABLE)
    }

    fn table_uri(&self, table: &str) -> String {
        match self {
            Self::Local(p) => p.join("lance").join(table).to_string_lossy().to_string(),
            Self::Remote(base) => format!("{}/lance/{table}", base.trim_end_matches('/')),
        }
    }
}

/// Parses a local directory path or Lance object-store URI (`s3://`, `gs://`, `az://`,
/// `shared-memory://`, `memory://`, `file://`), validating required cloud Cargo features.
pub fn parse_storage_root(input: &str) -> Result<StorageRoot, StoreError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidInput(
            "storage path or URI must not be empty".to_string(),
        ));
    }

    if let Some((raw_scheme, raw_rest)) = trimmed.split_once("://") {
        let scheme = raw_scheme.to_ascii_lowercase();
        if scheme.is_empty()
            || !scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            return Err(StoreError::InvalidInput(format!(
                "invalid URI scheme: {raw_scheme}"
            )));
        }

        if scheme == "file" {
            let clean_path = raw_rest.trim();
            if clean_path.is_empty() {
                return Err(StoreError::InvalidInput(
                    "file:// URI must specify a filesystem path".to_string(),
                ));
            }
            return Ok(StorageRoot::Local(PathBuf::from(clean_path)));
        }

        match scheme.as_str() {
            "s3" | "s3a" => {
                #[cfg(not(feature = "cloud-s3"))]
                return Err(StoreError::InvalidInput(
                    "S3 URI requires rebuilding with --features cloud-s3 (or --features cloud)"
                        .to_string(),
                ));
            }
            "gs" | "gcs" => {
                #[cfg(not(feature = "cloud-gcs"))]
                return Err(StoreError::InvalidInput(
                    "GCS URI requires rebuilding with --features cloud-gcs (or --features cloud)"
                        .to_string(),
                ));
            }
            "az" | "abfs" | "abfss" => {
                #[cfg(not(feature = "cloud-azure"))]
                return Err(StoreError::InvalidInput(
                    "Azure URI requires rebuilding with --features cloud-azure".to_string(),
                ));
            }
            "shared-memory" | "memory" | "http" | "https" => {}
            _ => {
                return Err(StoreError::InvalidInput(format!(
                    "unsupported storage URI scheme: {scheme}"
                )));
            }
        }

        let rest_clean = raw_rest.trim().trim_end_matches('/');
        if rest_clean.is_empty() || rest_clean.starts_with('/') {
            return Err(StoreError::InvalidInput(format!(
                "{scheme}:// URI must include an authority/bucket name"
            )));
        }
        return Ok(StorageRoot::Remote(format!("{scheme}://{rest_clean}")));
    }

    Ok(StorageRoot::Local(PathBuf::from(trimmed)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    #[default]
    Hybrid,
    Bm25,
    Vector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesSummary {
    pub study_instance_uid: String,
    pub series_instance_uid: String,
    pub patient_id: String,
    pub patient_name: String,
    pub accession_number: String,
    pub study_date: String,
    pub study_description: String,
    pub modality: String,
    pub body_part_examined: String,
    pub series_number: i32,
    pub series_description: String,
    pub instance_count: i32,
    pub frame_count: i32,
    pub rows: i32,
    pub columns: i32,
    pub spacing_x_mm: f64,
    pub spacing_y_mm: f64,
    pub spacing_z_mm: f64,
    pub window_center_default: f64,
    pub window_width_default: f64,
    pub is_volumetric: bool,
    pub sr_text: String,
    pub thumbnail_url: Option<String>,
    pub score: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudySummary {
    pub study_instance_uid: String,
    pub patient_id: String,
    pub patient_name: String,
    pub accession_number: String,
    pub study_date: String,
    pub study_description: String,
    pub modalities: Vec<String>,
    pub max_score: Option<f32>,
    pub series: Vec<SeriesSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameDescriptor {
    pub sop_instance_uid: String,
    pub sop_class_uid: String,
    pub instance_number: i32,
    pub frame_number: i32,
    pub slice_sort_index: i32,
    pub rows: i32,
    pub columns: i32,
    pub window_center: f64,
    pub window_width: f64,
    pub slice_location: f64,
    pub ipp_z: f64,
    pub transfer_syntax_uid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameBlobPayload {
    pub frame_number: i32,
    pub transfer_syntax_uid: String,
    pub blob_kind: String,
    pub total_blob_size: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarFrameHit {
    pub study_instance_uid: String,
    pub series_instance_uid: String,
    pub sop_instance_uid: String,
    pub frame_number: i32,
    pub slice_sort_index: i32,
    pub instance_number: i32,
    pub slice_location: f64,
    pub ipp_z: f64,
    pub modality: String,
    pub series_description: String,
    pub patient_name: String,
    pub similarity: f32,
    pub distance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetStats {
    pub exists: bool,
    pub version: u64,
    pub row_count: usize,
    pub fragment_count: usize,
    pub storage_version: String,
    pub indexes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreStats {
    pub data_dir: String,
    pub storage_uri: String,
    pub embedder: String,
    pub cache_hits_series: u64,
    pub cache_hits_frames: u64,
    pub blob_v2_kind_sample: Option<String>,
    pub blob_v2_inline_threshold_bytes: usize,
    pub series_dataset: DatasetStats,
    pub frames_dataset: DatasetStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestReport {
    pub series_ingested: usize,
    pub frames_ingested: usize,
    pub study_uids: Vec<String>,
}

pub fn validate_sql_literal(value: &str, field: &str) -> Result<String, StoreError> {
    if value.is_empty()
        || value.len() > 128
        || value != value.trim()
        || value.contains("--")
    {
        return Err(StoreError::InvalidInput(format!("invalid {field}")));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'^' | b' '))
    {
        return Err(StoreError::InvalidInput(format!(
            "disallowed characters in {field}"
        )));
    }
    Ok(value.to_string())
}

fn format_blob_kind(kind: BlobKind) -> &'static str {
    match kind {
        BlobKind::Inline => "Inline",
        BlobKind::Packed => "Packed",
        BlobKind::Dedicated => "Dedicated",
        BlobKind::External => "External",
        BlobKind::Managed => "Managed",
        BlobKind::ManagedWithBase => "ManagedWithBase",
    }
}

fn is_not_found(err: &lance::Error) -> bool {
    matches!(
        err,
        lance::Error::DatasetNotFound { .. } | lance::Error::NotFound { .. }
    )
}

#[derive(Clone)]
pub struct DicomLanceStore {
    root: StorageRoot,
    embedder: Arc<dyn Embedder>,
    write_lock: Arc<Mutex<()>>,
    series_cache: Arc<RwLock<Option<Arc<Dataset>>>>,
    frames_cache: Arc<RwLock<Option<Arc<Dataset>>>>,
    series_cache_hits: Arc<AtomicU64>,
    frames_cache_hits: Arc<AtomicU64>,
}

impl DicomLanceStore {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self::from_storage_root(StorageRoot::Local(root_dir.into()))
    }

    pub fn from_uri(uri: &str) -> Result<Self, StoreError> {
        Ok(Self::from_storage_root(parse_storage_root(uri)?))
    }

    pub fn from_storage_root(root: StorageRoot) -> Self {
        Self {
            root,
            embedder: Arc::new(LocalMultimodalEmbedder),
            write_lock: Arc::new(Mutex::new(())),
            series_cache: Arc::new(RwLock::new(None)),
            frames_cache: Arc::new(RwLock::new(None)),
            series_cache_hits: Arc::new(AtomicU64::new(0)),
            frames_cache_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        self
    }

    pub fn local_root_dir(&self) -> Option<&Path> {
        self.root.local_path()
    }

    pub fn root_uri(&self) -> String {
        self.root.display_uri()
    }

    pub fn storage_root(&self) -> &StorageRoot {
        &self.root
    }

    pub fn embedder_name(&self) -> &'static str {
        self.embedder.name()
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    pub async fn invalidate_cache(&self) {
        *self.series_cache.write().await = None;
        *self.frames_cache.write().await = None;
    }

    pub fn cache_hits(&self) -> (u64, u64) {
        (
            self.series_cache_hits.load(Ordering::Relaxed),
            self.frames_cache_hits.load(Ordering::Relaxed),
        )
    }

    fn series_uri(&self) -> String {
        self.root.series_uri()
    }

    fn frames_uri(&self) -> String {
        self.root.frames_uri()
    }

    async fn open_opt(&self, uri: &str) -> Result<Option<Dataset>, StoreError> {
        match DatasetBuilder::from_uri(uri).load().await {
            Ok(ds) => Ok(Some(ds)),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(StoreError::Lance(err)),
        }
    }

    async fn get_cached_dataset(
        &self,
        uri: &str,
        cache: &RwLock<Option<Arc<Dataset>>>,
        hits: &AtomicU64,
    ) -> Result<Option<Arc<Dataset>>, StoreError> {
        {
            let guard = cache.read().await;
            if let Some(ds) = guard.as_ref() {
                hits.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(Arc::clone(ds)));
            }
        }

        let Some(ds) = self.open_opt(uri).await? else {
            return Ok(None);
        };
        let arc = Arc::new(ds);
        let mut guard = cache.write().await;
        if let Some(existing) = guard.as_ref() {
            return Ok(Some(Arc::clone(existing)));
        }
        *guard = Some(Arc::clone(&arc));
        Ok(Some(arc))
    }

    async fn get_series_dataset(&self) -> Result<Option<Arc<Dataset>>, StoreError> {
        self.get_cached_dataset(&self.series_uri(), &self.series_cache, &self.series_cache_hits)
            .await
    }

    async fn get_frames_dataset(&self) -> Result<Option<Arc<Dataset>>, StoreError> {
        self.get_cached_dataset(&self.frames_uri(), &self.frames_cache, &self.frames_cache_hits)
            .await
    }

    fn v2_2_write_params(mode: WriteMode) -> WriteParams {
        WriteParams {
            mode,
            data_storage_version: Some(LanceFileVersion::V2_2),
            ..Default::default()
        }
    }

    async fn upsert_dataset(
        &self,
        uri: &str,
        batch: RecordBatch,
        merge_keys: &[&str],
    ) -> Result<Dataset, StoreError> {
        match self.open_opt(uri).await? {
            Some(dataset) => {
                let keys: Vec<String> = merge_keys.iter().map(|s| (*s).to_string()).collect();
                let mut builder = MergeInsertBuilder::try_new(Arc::new(dataset), keys)?;
                builder
                    .when_matched(WhenMatched::UpdateAll)
                    .when_not_matched(WhenNotMatched::InsertAll);
                let job = builder.try_build()?;
                let (updated, _) = job.execute_batches(vec![batch]).await?;
                Ok((*updated).clone())
            }
            None => {
                let schema = batch.schema();
                let reader = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);
                let ds = Dataset::write(
                    reader,
                    uri,
                    Some(Self::v2_2_write_params(WriteMode::Create)),
                )
                .await?;
                Ok(ds)
            }
        }
    }

    pub async fn ingest(&self, items: &[IngestedSeries]) -> Result<IngestReport, StoreError> {
        if items.is_empty() {
            return Err(StoreError::InvalidInput("empty ingest batch".to_string()));
        }

        if let Some(local_root) = self.root.local_path() {
            tokio::fs::create_dir_all(local_root.join("lance")).await?;
        }

        let mut series_records = Vec::with_capacity(items.len());
        let mut frame_records = Vec::new();
        for item in items {
            let mut frames = item.frames.clone();
            for frame in &mut frames {
                if frame.frame_embedding.is_none() {
                    let emb = self
                        .embedder
                        .embed_frame(frame, &item.series.searchable_text)
                        .await?;
                    frame.frame_embedding = Some(emb);
                }
            }
            let mut series = item.series.clone();
            if series.series_embedding.is_none() {
                let emb = self.embedder.embed_series(&series, &frames).await?;
                series.series_embedding = Some(emb);
            }
            series_records.push(series);
            frame_records.extend(frames);
        }

        let series_batch = build_series_batch(&series_records)?;
        let frames_batch = if !frame_records.is_empty() {
            Some(build_frames_batch(&frame_records)?)
        } else {
            None
        };

        let _guard = self.write_lock.lock().await;

        let mut series_ds = self
            .upsert_dataset(
                &self.series_uri(),
                series_batch,
                &["study_instance_uid", "series_instance_uid"],
            )
            .await?;

        series_ds
            .create_index(
                &["searchable_text"],
                IndexType::Inverted,
                Some(FTS_INDEX_NAME.to_string()),
                &InvertedIndexParams::default().with_position(true),
                true,
            )
            .await?;
        series_ds
            .create_index(
                &["study_instance_uid"],
                IndexType::BTree,
                Some("series_study_uid_btree".to_string()),
                &ScalarIndexParams::default(),
                true,
            )
            .await?;
        series_ds
            .create_index(
                &["modality"],
                IndexType::BTree,
                Some("series_modality_btree".to_string()),
                &ScalarIndexParams::default(),
                true,
            )
            .await?;

        let series_rows = series_ds.count_rows(None).await?;
        if series_rows >= IVF_MIN_ROWS {
            let partitions = (series_rows / 64).clamp(1, 32);
            let vec_params = VectorIndexParams::ivf_flat(partitions, MetricType::Cosine);
            series_ds
                .create_index(
                    &["series_embedding"],
                    IndexType::Vector,
                    Some(SERIES_VEC_INDEX_NAME.to_string()),
                    &vec_params,
                    true,
                )
                .await?;
        }
        *self.series_cache.write().await = Some(Arc::new(series_ds));

        if let Some(batch) = frames_batch {
            let mut frames_ds = self
                .upsert_dataset(
                    &self.frames_uri(),
                    batch,
                    &[
                        "study_instance_uid",
                        "series_instance_uid",
                        "sop_instance_uid",
                        "frame_number",
                    ],
                )
                .await?;

            frames_ds
                .create_index(
                    &["study_instance_uid"],
                    IndexType::BTree,
                    Some("frames_study_uid_btree".to_string()),
                    &ScalarIndexParams::default(),
                    true,
                )
                .await?;
            frames_ds
                .create_index(
                    &["series_instance_uid"],
                    IndexType::BTree,
                    Some("frames_series_uid_btree".to_string()),
                    &ScalarIndexParams::default(),
                    true,
                )
                .await?;
            frames_ds
                .create_index(
                    &["sop_instance_uid"],
                    IndexType::BTree,
                    Some("frames_sop_uid_btree".to_string()),
                    &ScalarIndexParams::default(),
                    true,
                )
                .await?;

            let frame_rows = frames_ds.count_rows(None).await?;
            if frame_rows >= IVF_MIN_ROWS {
                let partitions = (frame_rows / 64).clamp(4, 64);
                let vec_params = VectorIndexParams::ivf_flat(partitions, MetricType::Cosine);
                frames_ds
                    .create_index(
                        &["frame_embedding"],
                        IndexType::Vector,
                        Some(FRAMES_VEC_INDEX_NAME.to_string()),
                        &vec_params,
                        true,
                    )
                    .await?;
            }
            *self.frames_cache.write().await = Some(Arc::new(frames_ds));
        }

        let mut study_set = HashSet::new();
        for s in &series_records {
            study_set.insert(s.study_instance_uid.clone());
        }
        let mut study_uids: Vec<String> = study_set.into_iter().collect();
        study_uids.sort();

        Ok(IngestReport {
            series_ingested: series_records.len(),
            frames_ingested: frame_records.len(),
            study_uids,
        })
    }

    fn build_series_filter(
        modality: Option<&str>,
        patient_id: Option<&str>,
    ) -> Result<Option<String>, StoreError> {
        let mut clauses = Vec::new();
        if let Some(m) = modality.map(str::trim).filter(|s| !s.is_empty() && *s != "ALL") {
            let safe = validate_sql_literal(m, "modality")?;
            clauses.push(format!("modality = '{safe}'"));
        }
        if let Some(pid) = patient_id.map(str::trim).filter(|s| !s.is_empty()) {
            let safe = validate_sql_literal(pid, "patient_id")?;
            clauses.push(format!("patient_id = '{safe}'"));
        }
        if clauses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(clauses.join(" AND ")))
        }
    }

    fn extract_series_rows(
        batches: &[RecordBatch],
        score_column: Option<&str>,
        is_cosine_distance: bool,
    ) -> Vec<SeriesSummary> {
        let mut flat = Vec::new();
        for batch in batches {
            let study_uids = batch["study_instance_uid"].as_string::<i32>();
            let series_uids = batch["series_instance_uid"].as_string::<i32>();
            let patient_ids = batch["patient_id"].as_string::<i32>();
            let patient_names = batch["patient_name"].as_string::<i32>();
            let accessions = batch["accession_number"].as_string::<i32>();
            let study_dates = batch["study_date"].as_string::<i32>();
            let study_descs = batch["study_description"].as_string::<i32>();
            let modalities = batch["modality"].as_string::<i32>();
            let body_parts = batch["body_part_examined"].as_string::<i32>();
            let series_nums = batch["series_number"].as_primitive::<Int32Type>();
            let series_descs = batch["series_description"].as_string::<i32>();
            let inst_counts = batch["instance_count"].as_primitive::<Int32Type>();
            let frame_counts = batch["frame_count"].as_primitive::<Int32Type>();
            let rows = batch["rows"].as_primitive::<Int32Type>();
            let cols = batch["columns"].as_primitive::<Int32Type>();
            let sp_xs = batch["spacing_x_mm"].as_primitive::<Float64Type>();
            let sp_ys = batch["spacing_y_mm"].as_primitive::<Float64Type>();
            let sp_zs = batch["spacing_z_mm"].as_primitive::<Float64Type>();
            let wcs = batch["window_center_default"].as_primitive::<Float64Type>();
            let wws = batch["window_width_default"].as_primitive::<Float64Type>();
            let is_vols = batch["is_volumetric"].as_boolean();
            let sr_texts = batch["sr_text"].as_string::<i32>();
            let scores = score_column
                .and_then(|col| batch.column_by_name(col))
                .map(|c| c.as_primitive::<Float32Type>());

            for i in 0..batch.num_rows() {
                let fc = frame_counts.value(i);
                let study_uid = study_uids.value(i).to_string();
                let series_uid = series_uids.value(i).to_string();
                let thumbnail_url = if fc > 0 {
                    Some(format!("/api/series/{study_uid}/{series_uid}/thumbnail"))
                } else {
                    None
                };
                let score = scores.map(|s| {
                    let raw = s.value(i);
                    if is_cosine_distance {
                        (1.0 - raw).clamp(-1.0, 1.0)
                    } else {
                        raw
                    }
                });
                flat.push(SeriesSummary {
                    study_instance_uid: study_uid,
                    series_instance_uid: series_uid,
                    patient_id: patient_ids.value(i).to_string(),
                    patient_name: patient_names.value(i).to_string(),
                    accession_number: accessions.value(i).to_string(),
                    study_date: study_dates.value(i).to_string(),
                    study_description: study_descs.value(i).to_string(),
                    modality: modalities.value(i).to_string(),
                    body_part_examined: body_parts.value(i).to_string(),
                    series_number: series_nums.value(i),
                    series_description: series_descs.value(i).to_string(),
                    instance_count: inst_counts.value(i),
                    frame_count: fc,
                    rows: rows.value(i),
                    columns: cols.value(i),
                    spacing_x_mm: sp_xs.value(i),
                    spacing_y_mm: sp_ys.value(i),
                    spacing_z_mm: sp_zs.value(i),
                    window_center_default: wcs.value(i),
                    window_width_default: wws.value(i),
                    is_volumetric: is_vols.value(i),
                    sr_text: sr_texts.value(i).to_string(),
                    thumbnail_url,
                    score,
                });
            }
        }
        flat
    }

    async fn scan_bm25_series(
        &self,
        dataset: &Dataset,
        query: &str,
        filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SeriesSummary>, StoreError> {
        let mut scanner = dataset.scan();
        let mut proj: Vec<&str> = SERIES_PROJECTION.to_vec();
        proj.push("_score");
        scanner.project(&proj)?;
        scanner.disable_scoring_autoprojection();

        if let Some(clause) = filter {
            scanner.filter(clause)?;
            scanner.prefilter(true);
        }

        let fts = FullTextSearchQuery::new(query.to_string())
            .with_column("searchable_text".to_string())?
            .limit(Some(limit.max(1) as i64));
        scanner.full_text_search(fts)?;

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
        Ok(Self::extract_series_rows(&batches, Some("_score"), false))
    }

    async fn scan_vector_series(
        &self,
        dataset: &Dataset,
        query: &str,
        filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SeriesSummary>, StoreError> {
        let q_vec = self.embedder.embed_text(query).await?;
        let q_arr = Float32Array::from(q_vec);

        let mut scanner = dataset.scan();
        let mut proj: Vec<&str> = SERIES_PROJECTION.to_vec();
        proj.push("_distance");
        scanner.project(&proj)?;
        scanner.disable_scoring_autoprojection();

        if let Some(clause) = filter {
            scanner.filter(clause)?;
            scanner.prefilter(true);
        }

        scanner.nearest("series_embedding", &q_arr, limit.max(1))?;
        scanner.distance_metric(MetricType::Cosine);

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
        let mut hits = Self::extract_series_rows(&batches, Some("_distance"), true);
        hits.sort_by(|a, b| {
            b.score
                .unwrap_or(f32::NEG_INFINITY)
                .total_cmp(&a.score.unwrap_or(f32::NEG_INFINITY))
        });
        Ok(hits)
    }

    fn group_studies(series_rows: Vec<SeriesSummary>) -> Vec<StudySummary> {
        let mut by_study: BTreeMap<String, Vec<SeriesSummary>> = BTreeMap::new();
        for s in series_rows {
            by_study
                .entry(s.study_instance_uid.clone())
                .or_default()
                .push(s);
        }

        let mut studies: Vec<StudySummary> = by_study
            .into_iter()
            .map(|(study_uid, mut list)| {
                list.sort_by_key(|s| s.series_number);
                let first = &list[0];
                let mut mods = Vec::new();
                for s in &list {
                    if !mods.contains(&s.modality) {
                        mods.push(s.modality.clone());
                    }
                }
                let max_score = list
                    .iter()
                    .filter_map(|s| s.score)
                    .max_by(|a, b| a.total_cmp(b));
                StudySummary {
                    study_instance_uid: study_uid,
                    patient_id: first.patient_id.clone(),
                    patient_name: first.patient_name.clone(),
                    accession_number: first.accession_number.clone(),
                    study_date: first.study_date.clone(),
                    study_description: first.study_description.clone(),
                    modalities: mods,
                    max_score,
                    series: list,
                }
            })
            .collect();

        studies.sort_by(|a, b| {
            match (a.max_score, b.max_score) {
                (Some(sa), Some(sb)) => sb.total_cmp(&sa),
                _ => b.study_date.cmp(&a.study_date),
            }
            .then_with(|| a.study_instance_uid.cmp(&b.study_instance_uid))
        });

        studies
    }

    pub async fn search_series(
        &self,
        query: Option<&str>,
        modality: Option<&str>,
        patient_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StudySummary>, StoreError> {
        self.search_series_with_mode(query, modality, patient_id, limit, SearchMode::Hybrid)
            .await
    }

    pub async fn search_series_with_mode(
        &self,
        query: Option<&str>,
        modality: Option<&str>,
        patient_id: Option<&str>,
        limit: usize,
        mode: SearchMode,
    ) -> Result<Vec<StudySummary>, StoreError> {
        let Some(dataset) = self.get_series_dataset().await? else {
            return Ok(Vec::new());
        };
        if dataset.count_rows(None).await? == 0 {
            return Ok(Vec::new());
        }

        let filter = Self::build_series_filter(modality, patient_id)?;
        let limit_clamped = limit.max(1);
        let clean_query = query.map(str::trim).filter(|s| !s.is_empty());

        let flat = match (clean_query, mode) {
            (None, _) => {
                let mut scanner = dataset.scan();
                scanner.project(SERIES_PROJECTION)?;
                if let Some(clause) = filter.as_deref() {
                    scanner.filter(clause)?;
                    scanner.prefilter(true);
                }
                scanner.limit(Some(limit_clamped as i64), None)?;
                let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
                Self::extract_series_rows(&batches, None, false)
            }
            (Some(q), SearchMode::Bm25) => {
                self.scan_bm25_series(&dataset, q, filter.as_deref(), limit_clamped)
                    .await?
            }
            (Some(q), SearchMode::Vector) => {
                let hits = self
                    .scan_vector_series(&dataset, q, filter.as_deref(), limit_clamped)
                    .await?;
                hits.into_iter()
                    .filter(|s| s.score.unwrap_or(0.0) >= 0.10)
                    .collect()
            }
            (Some(q), SearchMode::Hybrid) => {
                let candidate_k = (limit_clamped * 2).max(16);
                let bm25_hits = self
                    .scan_bm25_series(&dataset, q, filter.as_deref(), candidate_k)
                    .await?;
                let vec_min_sim = if bm25_hits.is_empty() { 0.10 } else { 0.14 };
                let vec_hits: Vec<SeriesSummary> = self
                    .scan_vector_series(&dataset, q, filter.as_deref(), candidate_k)
                    .await?
                    .into_iter()
                    .filter(|s| s.score.unwrap_or(0.0) >= vec_min_sim)
                    .collect();

                let mut by_uid: HashMap<String, SeriesSummary> = HashMap::new();
                let mut bm25_ids = Vec::with_capacity(bm25_hits.len());
                for hit in bm25_hits {
                    bm25_ids.push(hit.series_instance_uid.clone());
                    by_uid.entry(hit.series_instance_uid.clone()).or_insert(hit);
                }
                let mut vec_ids = Vec::with_capacity(vec_hits.len());
                for hit in vec_hits {
                    vec_ids.push(hit.series_instance_uid.clone());
                    by_uid.entry(hit.series_instance_uid.clone()).or_insert(hit);
                }

                let fused = rrf_fuse(&bm25_ids, &vec_ids, 60.0);
                let mut merged = Vec::with_capacity(fused.len().min(limit_clamped));
                for (uid, score) in fused.into_iter().take(limit_clamped) {
                    if let Some(mut summary) = by_uid.remove(&uid) {
                        summary.score = Some(score);
                        merged.push(summary);
                    }
                }
                merged
            }
        };

        Ok(Self::group_studies(flat))
    }

    pub async fn get_series_thumbnail(
        &self,
        study_uid: &str,
        series_uid: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(dataset) = self.get_series_dataset().await? else {
            return Ok(None);
        };
        let safe_study = validate_sql_literal(study_uid, "study_instance_uid")?;
        let safe_series = validate_sql_literal(series_uid, "series_instance_uid")?;

        let mut scanner = dataset.scan();
        scanner.project(&["thumbnail_png"])?;
        scanner.filter(&format!(
            "study_instance_uid = '{safe_study}' AND series_instance_uid = '{safe_series}'"
        ))?;
        scanner.limit(Some(1), None)?;

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
        for batch in batches {
            let thumbs = batch["thumbnail_png"].as_binary::<i32>();
            if batch.num_rows() > 0 && !thumbs.is_null(0) {
                return Ok(Some(thumbs.value(0).to_vec()));
            }
        }
        Ok(None)
    }

    pub async fn get_series_wado_metadata(
        &self,
        study_uid: &str,
        series_uid: &str,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let Some(dataset) = self.get_frames_dataset().await? else {
            return Ok(Vec::new());
        };
        let safe_study = validate_sql_literal(study_uid, "study_instance_uid")?;
        let safe_series = validate_sql_literal(series_uid, "series_instance_uid")?;

        let mut scanner = dataset.scan();
        // Deliberately project ONLY metadata columns — zero pixel_frame_blob I/O.
        scanner.project(&[
            "sop_instance_uid",
            "frame_number",
            "slice_sort_index",
            "dicom_json_header",
        ])?;
        scanner.filter(&format!(
            "study_instance_uid = '{safe_study}' AND series_instance_uid = '{safe_series}'"
        ))?;

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
        let mut rows: Vec<(i32, i32, String, String)> = Vec::new();
        for batch in batches {
            let sops = batch["sop_instance_uid"].as_string::<i32>();
            let frames = batch["frame_number"].as_primitive::<Int32Type>();
            let sorts = batch["slice_sort_index"].as_primitive::<Int32Type>();
            let jsons = batch["dicom_json_header"].as_string::<i32>();
            for i in 0..batch.num_rows() {
                rows.push((
                    sops_sort_key(sorts.value(i)),
                    frames.value(i),
                    sops.value(i).to_string(),
                    jsons.value(i).to_string(),
                ));
            }
        }

        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (_, _, sop, json_str) in rows {
            if seen.insert(sop) {
                out.push(serde_json::from_str(&json_str)?);
            }
        }
        Ok(out)
    }

    pub async fn get_series_frame_descriptors(
        &self,
        study_uid: &str,
        series_uid: &str,
    ) -> Result<Vec<FrameDescriptor>, StoreError> {
        let Some(dataset) = self.get_frames_dataset().await? else {
            return Ok(Vec::new());
        };
        let safe_study = validate_sql_literal(study_uid, "study_instance_uid")?;
        let safe_series = validate_sql_literal(series_uid, "series_instance_uid")?;

        let mut scanner = dataset.scan();
        scanner.project(&[
            "sop_instance_uid",
            "sop_class_uid",
            "instance_number",
            "frame_number",
            "slice_sort_index",
            "rows",
            "columns",
            "window_center",
            "window_width",
            "slice_location",
            "ipp_z",
            "transfer_syntax_uid",
        ])?;
        scanner.filter(&format!(
            "study_instance_uid = '{safe_study}' AND series_instance_uid = '{safe_series}'"
        ))?;

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
        let mut out = Vec::new();
        for batch in batches {
            let sops = batch["sop_instance_uid"].as_string::<i32>();
            let classes = batch["sop_class_uid"].as_string::<i32>();
            let inst_nums = batch["instance_number"].as_primitive::<Int32Type>();
            let frame_nums = batch["frame_number"].as_primitive::<Int32Type>();
            let sort_idxs = batch["slice_sort_index"].as_primitive::<Int32Type>();
            let rows = batch["rows"].as_primitive::<Int32Type>();
            let cols = batch["columns"].as_primitive::<Int32Type>();
            let wcs = batch["window_center"].as_primitive::<Float64Type>();
            let wws = batch["window_width"].as_primitive::<Float64Type>();
            let locs = batch["slice_location"].as_primitive::<Float64Type>();
            let ipp_zs = batch["ipp_z"].as_primitive::<Float64Type>();
            let tses = batch["transfer_syntax_uid"].as_string::<i32>();

            for i in 0..batch.num_rows() {
                out.push(FrameDescriptor {
                    sop_instance_uid: sops.value(i).to_string(),
                    sop_class_uid: classes.value(i).to_string(),
                    instance_number: inst_nums.value(i),
                    frame_number: frame_nums.value(i),
                    slice_sort_index: sort_idxs.value(i),
                    rows: rows.value(i),
                    columns: cols.value(i),
                    window_center: wcs.value(i),
                    window_width: wws.value(i),
                    slice_location: locs.value(i),
                    ipp_z: ipp_zs.value(i),
                    transfer_syntax_uid: tses.value(i).to_string(),
                });
            }
        }

        out.sort_by(|a, b| {
            a.slice_sort_index
                .cmp(&b.slice_sort_index)
                .then_with(|| a.frame_number.cmp(&b.frame_number))
        });
        Ok(out)
    }

    pub async fn read_frame_blobs(
        &self,
        study_uid: &str,
        series_uid: &str,
        sop_uid: &str,
        frame_numbers: &[i32],
        byte_range: Option<Range<u64>>,
    ) -> Result<Vec<FrameBlobPayload>, StoreError> {
        let Some(dataset) = self.get_frames_dataset().await? else {
            return Err(StoreError::NotFound("frames dataset does not exist".to_string()));
        };
        let safe_study = validate_sql_literal(study_uid, "study_instance_uid")?;
        let safe_series = validate_sql_literal(series_uid, "series_instance_uid")?;
        let safe_sop = validate_sql_literal(sop_uid, "sop_instance_uid")?;

        if frame_numbers.is_empty() {
            return Err(StoreError::InvalidInput("empty frame list".to_string()));
        }
        let mut unique_frames = Vec::new();
        let mut seen_frames = HashSet::new();
        for &n in frame_numbers {
            if seen_frames.insert(n) {
                unique_frames.push(n);
            }
        }
        let in_list = unique_frames
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let mut scanner = dataset.scan();
        scanner.project(&["frame_number", "transfer_syntax_uid"])?;
        scanner.with_row_id();
        scanner.filter(&format!(
            "study_instance_uid = '{safe_study}' AND series_instance_uid = '{safe_series}' AND sop_instance_uid = '{safe_sop}' AND frame_number IN ({in_list})"
        ))?;

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
        let mut by_frame: HashMap<i32, (u64, String)> = HashMap::new();
        for batch in batches {
            let row_ids = batch[ROW_ID].as_primitive::<UInt64Type>();
            let fnums = batch["frame_number"].as_primitive::<Int32Type>();
            let tses = batch["transfer_syntax_uid"].as_string::<i32>();
            for i in 0..batch.num_rows() {
                by_frame.insert(
                    fnums.value(i),
                    (row_ids.value(i), tses.value(i).to_string()),
                );
            }
        }

        let mut requested = Vec::with_capacity(frame_numbers.len());
        let mut unique_row_ids = Vec::new();
        let mut seen_rids = HashSet::new();
        for &fnum in frame_numbers {
            let (rid, ts_uid) = by_frame.get(&fnum).ok_or_else(|| {
                StoreError::NotFound(format!("frame {fnum} not found for {sop_uid}"))
            })?;
            requested.push((fnum, *rid, ts_uid.clone()));
            if seen_rids.insert(*rid) {
                unique_row_ids.push(*rid);
            }
        }

        let blobs = dataset.take_blobs(&unique_row_ids, "pixel_frame_blob").await?;
        let mut cached_payloads: HashMap<u64, (String, u64, Vec<u8>)> = HashMap::new();
        for (rid, blob_opt) in unique_row_ids.into_iter().zip(blobs) {
            let blob = blob_opt.ok_or_else(|| {
                StoreError::NotFound(format!("null pixel blob for {sop_uid} row {rid}"))
            })?;
            let total_blob_size = blob.size();
            let blob_kind = format_blob_kind(blob.kind()).to_string();
            let bytes = match &byte_range {
                Some(r) => {
                    let clamped_end = r.end.min(total_blob_size);
                    if r.start >= clamped_end {
                        Vec::new()
                    } else {
                        blob.read_range(r.start..clamped_end).await?.to_vec()
                    }
                }
                None => blob.read().await?.to_vec(),
            };
            cached_payloads.insert(rid, (blob_kind, total_blob_size, bytes));
        }

        let mut results = Vec::with_capacity(requested.len());
        for (frame_number, rid, ts_uid) in requested {
            let (blob_kind, total_blob_size, bytes) = cached_payloads
                .get(&rid)
                .ok_or_else(|| StoreError::NotFound(format!("missing blob cache for row {rid}")))?;
            results.push(FrameBlobPayload {
                frame_number,
                transfer_syntax_uid: ts_uid,
                blob_kind: blob_kind.clone(),
                total_blob_size: *total_blob_size,
                bytes: bytes.clone(),
            });
        }

        Ok(results)
    }

    pub async fn find_similar_frames(
        &self,
        study_uid: &str,
        series_uid: &str,
        sop_uid: &str,
        frame_number: i32,
        limit: usize,
        exclude_self: bool,
    ) -> Result<Vec<SimilarFrameHit>, StoreError> {
        let Some(frames_ds) = self.get_frames_dataset().await? else {
            return Err(StoreError::NotFound("frames dataset does not exist".to_string()));
        };
        let safe_study = validate_sql_literal(study_uid, "study_instance_uid")?;
        let safe_series = validate_sql_literal(series_uid, "series_instance_uid")?;
        let safe_sop = validate_sql_literal(sop_uid, "sop_instance_uid")?;
        if frame_number < 1 {
            return Err(StoreError::InvalidInput(format!(
                "invalid frame_number {frame_number}"
            )));
        }

        let mut q_scan = frames_ds.scan();
        q_scan.project(&["frame_embedding"])?;
        q_scan.filter(&format!(
            "study_instance_uid = '{safe_study}' AND series_instance_uid = '{safe_series}' AND sop_instance_uid = '{safe_sop}' AND frame_number = {frame_number}"
        ))?;
        q_scan.limit(Some(1), None)?;

        let q_batches: Vec<RecordBatch> = q_scan.try_into_stream().await?.try_collect().await?;
        let mut query_vec: Option<Vec<f32>> = None;
        for batch in q_batches {
            if batch.num_rows() > 0 {
                let fsl = batch["frame_embedding"].as_fixed_size_list();
                if fsl.is_null(0) {
                    return Err(StoreError::NotFound(format!(
                        "frame embedding missing for {sop_uid} frame {frame_number}"
                    )));
                }
                let arr = fsl.value(0);
                let floats = arr.as_primitive::<Float32Type>();
                query_vec = Some(floats.values().to_vec());
                break;
            }
        }
        let query_vec = query_vec.ok_or_else(|| {
            StoreError::NotFound(format!("frame {frame_number} not found for {sop_uid}"))
        })?;

        let limit_clamped = limit.clamp(1, 50);
        let k_fetch = limit_clamped + usize::from(exclude_self);

        let mut knn_scan = frames_ds.scan();
        knn_scan.project(&[
            "study_instance_uid",
            "series_instance_uid",
            "sop_instance_uid",
            "frame_number",
            "slice_sort_index",
            "instance_number",
            "slice_location",
            "ipp_z",
            "_distance",
        ])?;
        knn_scan.disable_scoring_autoprojection();
        let q_arr = Float32Array::from(query_vec);
        knn_scan.nearest("frame_embedding", &q_arr, k_fetch)?;
        knn_scan.distance_metric(MetricType::Cosine);

        let batches: Vec<RecordBatch> = knn_scan.try_into_stream().await?.try_collect().await?;

        let mut series_meta: HashMap<(String, String), (String, String, String)> = HashMap::new();
        if let Some(series_ds) = self.get_series_dataset().await? {
            let mut meta_scan = series_ds.scan();
            meta_scan.project(&[
                "study_instance_uid",
                "series_instance_uid",
                "modality",
                "series_description",
                "patient_name",
            ])?;
            let meta_batches: Vec<RecordBatch> =
                meta_scan.try_into_stream().await?.try_collect().await?;
            for mb in meta_batches {
                let m_studies = mb["study_instance_uid"].as_string::<i32>();
                let m_series = mb["series_instance_uid"].as_string::<i32>();
                let m_mods = mb["modality"].as_string::<i32>();
                let m_descs = mb["series_description"].as_string::<i32>();
                let m_pats = mb["patient_name"].as_string::<i32>();
                for i in 0..mb.num_rows() {
                    series_meta.insert(
                        (m_studies.value(i).to_string(), m_series.value(i).to_string()),
                        (
                            m_mods.value(i).to_string(),
                            m_descs.value(i).to_string(),
                            m_pats.value(i).to_string(),
                        ),
                    );
                }
            }
        }

        let mut hits = Vec::new();
        for batch in batches {
            let studies = batch["study_instance_uid"].as_string::<i32>();
            let series = batch["series_instance_uid"].as_string::<i32>();
            let sops = batch["sop_instance_uid"].as_string::<i32>();
            let fnums = batch["frame_number"].as_primitive::<Int32Type>();
            let sort_idxs = batch["slice_sort_index"].as_primitive::<Int32Type>();
            let inst_nums = batch["instance_number"].as_primitive::<Int32Type>();
            let slice_locs = batch["slice_location"].as_primitive::<Float64Type>();
            let ipp_zs = batch["ipp_z"].as_primitive::<Float64Type>();
            let dists = batch["_distance"].as_primitive::<Float32Type>();

            for i in 0..batch.num_rows() {
                let st = studies.value(i);
                let sr = series.value(i);
                let sp = sops.value(i);
                let fn_i = fnums.value(i);

                if exclude_self
                    && st == study_uid
                    && sr == series_uid
                    && sp == sop_uid
                    && fn_i == frame_number
                {
                    continue;
                }

                let distance = dists.value(i);
                let similarity = (1.0 - distance).clamp(-1.0, 1.0);
                let (modality, series_description, patient_name) = series_meta
                    .get(&(st.to_string(), sr.to_string()))
                    .cloned()
                    .unwrap_or_else(|| ("OT".to_string(), "Series".to_string(), String::new()));

                hits.push(SimilarFrameHit {
                    study_instance_uid: st.to_string(),
                    series_instance_uid: sr.to_string(),
                    sop_instance_uid: sp.to_string(),
                    frame_number: fn_i,
                    slice_sort_index: sort_idxs.value(i),
                    instance_number: inst_nums.value(i),
                    slice_location: slice_locs.value(i),
                    ipp_z: ipp_zs.value(i),
                    modality,
                    series_description,
                    patient_name,
                    similarity,
                    distance,
                });
            }
        }

        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        hits.truncate(limit_clamped);
        Ok(hits)
    }

    async fn dataset_stats(ds_opt: Option<&Arc<Dataset>>) -> Result<DatasetStats, StoreError> {
        let Some(ds) = ds_opt else {
            return Ok(DatasetStats {
                exists: false,
                version: 0,
                row_count: 0,
                fragment_count: 0,
                storage_version: "none".to_string(),
                indexes: Vec::new(),
            });
        };

        let version = ds.version().version;
        let row_count = ds.count_rows(None).await?;
        let fragment_count = ds.get_fragments().len();
        let storage_version = ds
            .manifest()
            .data_storage_format
            .version
            .to_string();
        let indices = ds.load_indices().await?;
        let indexes = indices.iter().map(|idx| idx.name.clone()).collect();

        Ok(DatasetStats {
            exists: true,
            version,
            row_count,
            fragment_count,
            storage_version,
            indexes,
        })
    }

    pub async fn get_stats(&self) -> Result<StoreStats, StoreError> {
        let series_ds = self.get_series_dataset().await?;
        let frames_ds = self.get_frames_dataset().await?;

        let series_dataset = Self::dataset_stats(series_ds.as_ref()).await?;
        let frames_dataset = Self::dataset_stats(frames_ds.as_ref()).await?;

        let blob_v2_kind_sample = if frames_dataset.exists && frames_dataset.row_count > 0 {
            if let Some(ds_arc) = frames_ds {
                let blobs = ds_arc
                    .take_blobs_by_indices(&[0], "pixel_frame_blob")
                    .await?;
                blobs
                    .into_iter()
                    .next()
                    .flatten()
                    .map(|b| format_blob_kind(b.kind()).to_string())
            } else {
                None
            }
        } else {
            None
        };

        let storage_uri = self.root.display_uri();
        let (cache_hits_series, cache_hits_frames) = self.cache_hits();

        Ok(StoreStats {
            data_dir: storage_uri.clone(),
            storage_uri,
            embedder: self.embedder.name().to_string(),
            cache_hits_series,
            cache_hits_frames,
            blob_v2_kind_sample,
            blob_v2_inline_threshold_bytes: 4096,
            series_dataset,
            frames_dataset,
        })
    }
}

fn sops_sort_key(idx: i32) -> i32 {
    idx
}
