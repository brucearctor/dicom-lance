use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type, Int32Type, UInt64Type};
use arrow_array::{Array, RecordBatch, RecordBatchIterator};
use futures::TryStreamExt;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::{MergeInsertBuilder, WhenMatched, WhenNotMatched, WriteMode, WriteParams};
use lance::index::DatasetIndexExt;
use lance::Dataset;
use lance_core::datatypes::BlobKind;
use lance_core::ROW_ID;
use lance_file::version::LanceFileVersion;
use lance_index::scalar::{FullTextSearchQuery, InvertedIndexParams, ScalarIndexParams};
use lance_index::IndexType;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::dicom_ingest::IngestedSeries;
use crate::schema::{build_frames_batch, build_series_batch, FrameRecord, SchemaError, SeriesRecord};

const SERIES_TABLE: &str = "dicom_series.lance";
const FRAMES_TABLE: &str = "dicom_frames.lance";
const FTS_INDEX_NAME: &str = "searchable_text_fts";

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
    #[error("invalid filter parameter: {0}")]
    InvalidInput(String),
    #[error("not found: {0}")]
    NotFound(String),
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

fn validate_sql_literal(value: &str, field: &str) -> Result<String, StoreError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(StoreError::InvalidInput(format!("invalid {field}")));
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'^' | b' '))
    {
        return Err(StoreError::InvalidInput(format!(
            "disallowed characters in {field}"
        )));
    }
    Ok(trimmed.to_string())
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
    root_dir: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl DicomLanceStore {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    fn lance_dir(&self) -> PathBuf {
        self.root_dir.join("lance")
    }

    fn series_uri(&self) -> String {
        self.lance_dir()
            .join(SERIES_TABLE)
            .to_string_lossy()
            .to_string()
    }

    fn frames_uri(&self) -> String {
        self.lance_dir()
            .join(FRAMES_TABLE)
            .to_string_lossy()
            .to_string()
    }

    async fn open_opt(&self, uri: &str) -> Result<Option<Dataset>, StoreError> {
        match DatasetBuilder::from_uri(uri).load().await {
            Ok(ds) => Ok(Some(ds)),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(StoreError::Lance(err)),
        }
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

        tokio::fs::create_dir_all(self.lance_dir()).await?;

        let series_records: Vec<SeriesRecord> = items.iter().map(|i| i.series.clone()).collect();
        let frame_records: Vec<FrameRecord> =
            items.iter().flat_map(|i| i.frames.iter().cloned()).collect();

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

    pub async fn search_series(
        &self,
        query: Option<&str>,
        modality: Option<&str>,
        patient_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StudySummary>, StoreError> {
        let Some(dataset) = self.open_opt(&self.series_uri()).await? else {
            return Ok(Vec::new());
        };

        let mut scanner = dataset.scan();
        scanner.project(&[
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
        ])?;

        let mut clauses = Vec::new();
        if let Some(m) = modality.map(str::trim).filter(|s| !s.is_empty() && *s != "ALL") {
            let safe = validate_sql_literal(m, "modality")?;
            clauses.push(format!("modality = '{safe}'"));
        }
        if let Some(pid) = patient_id.map(str::trim).filter(|s| !s.is_empty()) {
            let safe = validate_sql_literal(pid, "patient_id")?;
            clauses.push(format!("patient_id = '{safe}'"));
        }
        if !clauses.is_empty() {
            scanner.filter(&clauses.join(" AND "))?;
            scanner.prefilter(true);
        }

        let clean_query = query.map(str::trim).filter(|s| !s.is_empty());
        if let Some(q) = clean_query {
            let fts = FullTextSearchQuery::new(q.to_string())
                .with_column("searchable_text".to_string())?
                .limit(Some(limit.max(1) as i64));
            scanner.full_text_search(fts)?;
        } else {
            scanner.limit(Some(limit.max(1) as i64), None)?;
        }

        let batches: Vec<RecordBatch> = scanner.try_into_stream().await?.try_collect().await?;
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
            let scores = batch
                .column_by_name("_score")
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
                    score: scores.map(|s| s.value(i)),
                });
            }
        }

        let mut by_study: BTreeMap<String, Vec<SeriesSummary>> = BTreeMap::new();
        for s in flat {
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

        Ok(studies)
    }

    pub async fn get_series_thumbnail(
        &self,
        study_uid: &str,
        series_uid: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(dataset) = self.open_opt(&self.series_uri()).await? else {
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
        let Some(dataset) = self.open_opt(&self.frames_uri()).await? else {
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
        let Some(dataset) = self.open_opt(&self.frames_uri()).await? else {
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
        let Some(dataset) = self.open_opt(&self.frames_uri()).await? else {
            return Err(StoreError::NotFound("frames dataset does not exist".to_string()));
        };
        let safe_study = validate_sql_literal(study_uid, "study_instance_uid")?;
        let safe_series = validate_sql_literal(series_uid, "series_instance_uid")?;
        let safe_sop = validate_sql_literal(sop_uid, "sop_instance_uid")?;

        if frame_numbers.is_empty() {
            return Err(StoreError::InvalidInput("empty frame list".to_string()));
        }
        let in_list = frame_numbers
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
        let mut matched: Vec<(i32, u64, String)> = Vec::new();
        for batch in batches {
            let row_ids = batch[ROW_ID].as_primitive::<UInt64Type>();
            let fnums = batch["frame_number"].as_primitive::<Int32Type>();
            let tses = batch["transfer_syntax_uid"].as_string::<i32>();
            for i in 0..batch.num_rows() {
                matched.push((fnums.value(i), row_ids.value(i), tses.value(i).to_string()));
            }
        }
        if matched.is_empty() {
            return Err(StoreError::NotFound(format!(
                "no matching frames for {sop_uid}"
            )));
        }

        // Preserve caller frame order when possible
        matched.sort_by_key(|(fnum, _, _)| {
            frame_numbers
                .iter()
                .position(|x| x == fnum)
                .unwrap_or(usize::MAX)
        });

        let row_ids: Vec<u64> = matched.iter().map(|(_, rid, _)| *rid).collect();
        let ds_arc = Arc::new(dataset);
        let blobs = ds_arc.take_blobs(&row_ids, "pixel_frame_blob").await?;

        let mut results = Vec::with_capacity(matched.len());
        for ((frame_number, _, ts_uid), blob_opt) in matched.into_iter().zip(blobs.into_iter()) {
            let blob = blob_opt.ok_or_else(|| {
                StoreError::NotFound(format!("null pixel blob for {sop_uid} frame {frame_number}"))
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
            results.push(FrameBlobPayload {
                frame_number,
                transfer_syntax_uid: ts_uid,
                blob_kind,
                total_blob_size,
                bytes,
            });
        }

        Ok(results)
    }

    async fn dataset_stats(&self, uri: &str) -> Result<DatasetStats, StoreError> {
        let Some(ds) = self.open_opt(uri).await? else {
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
        let series_dataset = self.dataset_stats(&self.series_uri()).await?;
        let frames_dataset = self.dataset_stats(&self.frames_uri()).await?;

        let blob_v2_kind_sample = if frames_dataset.exists && frames_dataset.row_count > 0 {
            if let Some(ds) = self.open_opt(&self.frames_uri()).await? {
                let ds_arc = Arc::new(ds);
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

        Ok(StoreStats {
            data_dir: self.root_dir.to_string_lossy().to_string(),
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
