use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use axum::Router;
use dicom_lance::api::build_router;
use dicom_lance::dicom_ingest::{render_thumbnail_png, IngestedSeries};
use dicom_lance::schema::{FrameRecord, SeriesRecord, EMBEDDING_DIM};
use dicom_lance::store::{DicomLanceStore, MaintenanceOptions, SearchMode};
use serde::Serialize;
use tower::ServiceExt;

struct BenchTempDir(PathBuf);

impl BenchTempDir {
    fn new() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("dicom-lance-bench-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(Self(dir))
    }
}

impl Drop for BenchTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn send_ok_body(app: &Router, req: Request<Body>) -> Result<usize, String> {
    let Ok(resp) = app.clone().oneshot(req).await;
    if resp.status() != StatusCode::OK {
        return Err(format!("unexpected HTTP status {}", resp.status()));
    }
    let body = to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    Ok(body.len())
}

const CLINICAL_SNIPPETS: &[&str] = &[
    "Spiculated right upper lobe pulmonary nodule measuring 18 mm; Lung-RADS 4B; recommend PET-CT.",
    "Ring-enhancing left frontal lobe mass with surrounding vasogenic edema and midline shift.",
    "Bilateral ground-glass opacities in dependent lower lobes with small pleural effusion.",
    "Calcified coronary plaque and mild aortic ectasia without pericardial effusion.",
];

struct BenchConfig {
    studies: usize,
    slices: usize,
    dim: usize,
    iters: usize,
    keep_dir: Option<PathBuf>,
    json_out: Option<PathBuf>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            studies: 40,
            slices: 256,
            dim: 128,
            iters: 100,
            keep_dir: None,
            json_out: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct LatencyStats {
    name: String,
    samples: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    ops_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
struct BenchmarkReport {
    studies: usize,
    slices_per_study: usize,
    frame_dim: usize,
    total_series: usize,
    total_frames: usize,
    raw_pixel_bytes: usize,
    ingest_ms: f64,
    ingest_frames_per_sec: f64,
    incremental_upsert_frames: usize,
    incremental_upsert_ms: f64,
    series_dataset_bytes: u64,
    frames_dataset_bytes: u64,
    blob_v2_kind_sample: Option<String>,
    delete_and_compact_ms: f64,
    latencies: Vec<LatencyStats>,
}

fn parse_args() -> Result<BenchConfig, String> {
    let mut cfg = BenchConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--studies" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--studies requires a value".to_string())?;
                cfg.studies = v.parse::<usize>().map_err(|_| format!("invalid --studies {v}"))?;
            }
            "--slices" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--slices requires a value".to_string())?;
                cfg.slices = v.parse::<usize>().map_err(|_| format!("invalid --slices {v}"))?;
            }
            "--dim" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--dim requires a value".to_string())?;
                cfg.dim = v.parse::<usize>().map_err(|_| format!("invalid --dim {v}"))?;
            }
            "--iters" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--iters requires a value".to_string())?;
                cfg.iters = v.parse::<usize>().map_err(|_| format!("invalid --iters {v}"))?;
            }
            "--keep-dir" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--keep-dir requires a path".to_string())?;
                cfg.keep_dir = Some(PathBuf::from(v));
            }
            "--json-out" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--json-out requires a path".to_string())?;
                cfg.json_out = Some(PathBuf::from(v));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: dicom-lance-bench [--studies N] [--slices N] [--dim N] [--iters N] [--keep-dir PATH] [--json-out PATH]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if cfg.studies == 0 || cfg.slices == 0 || cfg.dim < 16 || cfg.iters == 0 {
        return Err("studies, slices, iters must be > 0 and dim >= 16".to_string());
    }
    Ok(cfg)
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summarize_latencies(name: &str, durations: &[Duration]) -> LatencyStats {
    let mut ms: Vec<f64> = durations
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .collect();
    ms.sort_by(|a, b| a.total_cmp(b));
    let total_secs: f64 = durations.iter().map(|d| d.as_secs_f64()).sum();
    let ops_per_sec = if total_secs > 0.0 {
        durations.len() as f64 / total_secs
    } else {
        0.0
    };
    LatencyStats {
        name: name.to_string(),
        samples: ms.len(),
        min_ms: ms.first().copied().unwrap_or(0.0),
        p50_ms: percentile_sorted(&ms, 0.50),
        p95_ms: percentile_sorted(&ms, 0.95),
        p99_ms: percentile_sorted(&ms, 0.99),
        max_ms: ms.last().copied().unwrap_or(0.0),
        ops_per_sec,
    }
}

fn deterministic_embedding(seed: u64) -> Vec<f32> {
    let dim = EMBEDDING_DIM as usize;
    let mut out = vec![0.0_f32; dim];
    let b1 = (seed % 512) as usize;
    let b2 = ((seed.wrapping_mul(31) + 7) % 512) as usize;
    let b3 = (512 + (seed % 256)) as usize;
    let b4 = (512 + ((seed.wrapping_mul(17) + 3) % 256)) as usize;
    out[b1] += 1.0;
    out[b2] += 0.65;
    out[b3] += 0.85;
    out[b4] += 0.45;
    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
    for v in &mut out {
        *v /= norm;
    }
    out
}

fn generate_synthetic_study_pair(
    study_idx: usize,
    slices: usize,
    dim: usize,
    base_pixel_template: &[u8],
) -> Vec<IngestedSeries> {
    let study_uid = format!("1.2.826.0.1.3680043.10.9000.{study_idx}");
    let img_series_uid = format!("{study_uid}.1");
    let sr_series_uid = format!("{study_uid}.2");
    let is_mr = study_idx % 2 == 1;
    let modality = if is_mr { "MR" } else { "CT" };
    let sop_class = if is_mr {
        "1.2.840.10008.5.1.4.1.1.4"
    } else {
        "1.2.840.10008.5.1.4.1.1.2"
    };
    let snippet = CLINICAL_SNIPPETS[study_idx % CLINICAL_SNIPPETS.len()];
    let patient_id = format!("BENCH-{study_idx:04}");
    let patient_name = format!("Bench^Subject{study_idx:04}");
    let study_desc = if is_mr {
        "Brain MRI Contrast Protocol"
    } else {
        "Thoracic Helical CT"
    };

    let mut frames = Vec::with_capacity(slices);
    for z in 0..slices {
        let mut px = base_pixel_template.to_vec();
        let marker = ((study_idx + z) & 0xff) as u8;
        if px.len() >= 2 {
            px[0] = marker;
            px[1] = 4;
        }
        let sop_uid = format!("{img_series_uid}.{}", z + 1);
        let z_mm = -((slices as f64) * 0.5) + z as f64 * 1.25;
        let frame_seed = ((study_idx as u64) << 16) | (z as u64);
        frames.push(FrameRecord {
            study_instance_uid: study_uid.clone(),
            series_instance_uid: img_series_uid.clone(),
            sop_instance_uid: sop_uid,
            sop_class_uid: sop_class.to_string(),
            instance_number: (z + 1) as i32,
            frame_number: 1,
            slice_sort_index: z as i32,
            rows: dim as i32,
            columns: dim as i32,
            samples_per_pixel: 1,
            bits_allocated: 16,
            bits_stored: 12,
            high_bit: 11,
            pixel_representation: 0,
            photometric_interpretation: "MONOCHROME2".to_string(),
            transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
            rescale_slope: 1.0,
            rescale_intercept: if is_mr { 0.0 } else { -1024.0 },
            window_center: if is_mr { 420.0 } else { -600.0 },
            window_width: if is_mr { 850.0 } else { 1500.0 },
            slice_location: z_mm,
            slice_thickness: 1.25,
            pixel_spacing_row_mm: 0.75,
            pixel_spacing_col_mm: 0.75,
            ipp_x: -90.0,
            ipp_y: -90.0,
            ipp_z: z_mm,
            iop: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            dicom_json_header: r#"{"00280010":{"vr":"US","Value":[128]}}"#.to_string(),
            frame_embedding: Some(deterministic_embedding(frame_seed)),
            pixel_bytes: px,
        });
    }

    let thumb = render_thumbnail_png(&frames[slices / 2]).unwrap_or(None);
    let img_searchable = format!("{patient_id} | {patient_name} | {study_desc} | {modality} | Series 1");
    let img_series = IngestedSeries {
        series: SeriesRecord {
            study_instance_uid: study_uid.clone(),
            series_instance_uid: img_series_uid,
            patient_id: patient_id.clone(),
            patient_name: patient_name.clone(),
            accession_number: format!("ACC-{study_idx:05}"),
            study_date: "20261126".to_string(),
            study_description: study_desc.to_string(),
            modality: modality.to_string(),
            body_part_examined: if is_mr { "HEAD" } else { "CHEST" }.to_string(),
            series_number: 1,
            series_description: format!("{modality} Thin Volume"),
            instance_count: slices as i32,
            frame_count: slices as i32,
            rows: dim as i32,
            columns: dim as i32,
            spacing_x_mm: 0.75,
            spacing_y_mm: 0.75,
            spacing_z_mm: 1.25,
            window_center_default: if is_mr { 420.0 } else { -600.0 },
            window_width_default: if is_mr { 850.0 } else { 1500.0 },
            is_volumetric: slices >= 3,
            sr_text: String::new(),
            searchable_text: img_searchable,
            series_embedding: Some(deterministic_embedding(study_idx as u64 * 97 + 1)),
            thumbnail_png: thumb,
            updated_at_micros: 1_764_180_000_000_000,
        },
        frames,
    };

    let sr_searchable = format!("{patient_id} | {patient_name} | {study_desc} | SR | {snippet}");
    let sr_series = IngestedSeries {
        series: SeriesRecord {
            study_instance_uid: study_uid,
            series_instance_uid: sr_series_uid,
            patient_id,
            patient_name,
            accession_number: format!("ACC-{study_idx:05}"),
            study_date: "20261126".to_string(),
            study_description: study_desc.to_string(),
            modality: "SR".to_string(),
            body_part_examined: if is_mr { "HEAD" } else { "CHEST" }.to_string(),
            series_number: 900,
            series_description: "Clinical Findings SR".to_string(),
            instance_count: 1,
            frame_count: 0,
            rows: 0,
            columns: 0,
            spacing_x_mm: 1.0,
            spacing_y_mm: 1.0,
            spacing_z_mm: 1.0,
            window_center_default: 40.0,
            window_width_default: 400.0,
            is_volumetric: false,
            sr_text: snippet.to_string(),
            searchable_text: sr_searchable,
            series_embedding: Some(deterministic_embedding(study_idx as u64 * 97 + 2)),
            thumbnail_png: None,
            updated_at_micros: 1_764_180_000_000_000,
        },
        frames: Vec::new(),
    };

    vec![img_series, sr_series]
}

fn dir_size_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total += dir_size_bytes(&p);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn build_pixel_template(dim: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(dim * dim * 2);
    let c = dim as f64 * 0.5;
    let r_sq = (dim as f64 * 0.25).powi(2);
    for y in 0..dim {
        for x in 0..dim {
            let dx = x as f64 - c;
            let dy = y as f64 - c;
            let hu_shifted: u16 = if dx * dx + dy * dy <= r_sq {
                1100
            } else {
                320
            };
            buf.extend_from_slice(&hu_shifted.to_le_bytes());
        }
    }
    buf
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("benchmark error: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cfg = parse_args()?;
    let temp_dir_guard = if cfg.keep_dir.is_none() {
        Some(BenchTempDir::new()?)
    } else {
        None
    };
    let data_dir = cfg
        .keep_dir
        .clone()
        .unwrap_or_else(|| temp_dir_guard.as_ref().unwrap().0.clone());
    fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;

    let template = build_pixel_template(cfg.dim);
    let mut all_series = Vec::with_capacity(cfg.studies * 2);
    for s in 0..cfg.studies {
        all_series.extend(generate_synthetic_study_pair(
            s,
            cfg.slices,
            cfg.dim,
            &template,
        ));
    }

    let store = Arc::new(DicomLanceStore::new(&data_dir));
    let t_ingest = Instant::now();
    let report = store
        .ingest(&all_series)
        .await
        .map_err(|e| format!("bulk ingest failed: {e}"))?;
    let ingest_dur = t_ingest.elapsed();

    let incremental_study = generate_synthetic_study_pair(
        cfg.studies,
        cfg.slices,
        cfg.dim,
        &template,
    );
    let t_inc = Instant::now();
    store
        .ingest(&incremental_study)
        .await
        .map_err(|e| format!("incremental ingest failed: {e}"))?;
    let incremental_dur = t_inc.elapsed();

    let stats = store
        .get_stats()
        .await
        .map_err(|e| format!("stats failed: {e}"))?;
    let series_dir = data_dir.join("lance").join("dicom_series.lance");
    let frames_dir = data_dir.join("lance").join("dicom_frames.lance");
    let series_bytes = dir_size_bytes(&series_dir);
    let frames_bytes = dir_size_bytes(&frames_dir);

    let app = build_router(store.clone(), None);

    // Warm caches once before timed loops
    let warm_study = "1.2.826.0.1.3680043.10.9000.0";
    let warm_series = "1.2.826.0.1.3680043.10.9000.0.1";
    let warm_sop = "1.2.826.0.1.3680043.10.9000.0.1.1";
    let _ = store
        .read_frame_blobs(warm_study, warm_series, warm_sop, &[1], None)
        .await
        .map_err(|e| format!("warmup failed: {e}"))?;
    let _ = store
        .search_series_with_mode(Some("spiculated nodule"), None, None, 20, SearchMode::Hybrid)
        .await
        .map_err(|e| format!("search warmup failed: {e}"))?;

    let frame_coords: Vec<(String, String, String)> = (0..cfg.iters)
        .map(|i| {
            let s_idx = (i * 7) % cfg.studies;
            let z_idx = (i * 37) % cfg.slices + 1;
            let st = format!("1.2.826.0.1.3680043.10.9000.{s_idx}");
            let ser = format!("{st}.1");
            let sop = format!("{ser}.{z_idx}");
            (st, ser, sop)
        })
        .collect();

    let mut d_blob_full = Vec::with_capacity(cfg.iters);
    for (st, ser, sop) in &frame_coords {
        let t0 = Instant::now();
        let res = store
            .read_frame_blobs(st, ser, sop, &[1], None)
            .await
            .map_err(|e| format!("read_frame_blobs full failed: {e}"))?;
        assert_eq!(res.len(), 1);
        d_blob_full.push(t0.elapsed());
    }

    let mut d_blob_range = Vec::with_capacity(cfg.iters);
    for (st, ser, sop) in &frame_coords {
        let t0 = Instant::now();
        let res = store
            .read_frame_blobs(st, ser, sop, &[1], Some(0..4096))
            .await
            .map_err(|e| format!("read_frame_blobs range failed: {e}"))?;
        assert_eq!(res[0].bytes.len(), 4096);
        d_blob_range.push(t0.elapsed());
    }

    let mut d_wado_raw = Vec::with_capacity(cfg.iters);
    for (st, ser, sop) in &frame_coords {
        let uri = format!("/dicomweb/studies/{st}/series/{ser}/instances/{sop}/frames/1");
        let t0 = Instant::now();
        let n = send_ok_body(&app, Request::builder().uri(uri).body(Body::empty()).unwrap()).await?;
        assert!(n > 0);
        d_wado_raw.push(t0.elapsed());
    }

    let mut d_wado_rle = Vec::with_capacity(cfg.iters);
    for (st, ser, sop) in &frame_coords {
        let uri = format!("/dicomweb/studies/{st}/series/{ser}/instances/{sop}/frames/1");
        let t0 = Instant::now();
        let req = Request::builder()
            .uri(uri)
            .header(header::ACCEPT, "multipart/related; type=\"image/dicom-rle\"; transfer-syntax=1.2.840.10008.1.2.5")
            .body(Body::empty())
            .unwrap();
        let n = send_ok_body(&app, req).await?;
        assert!(n > 0);
        d_wado_rle.push(t0.elapsed());
    }

    let mut d_rendered_png = Vec::with_capacity(cfg.iters);
    for (st, ser, sop) in &frame_coords {
        let uri = format!(
            "/dicomweb/studies/{st}/series/{ser}/instances/{sop}/frames/1/rendered?window=-600,1500&viewport=256,256"
        );
        let t0 = Instant::now();
        let req = Request::builder()
            .uri(uri)
            .header(header::ACCEPT, "image/png")
            .body(Body::empty())
            .unwrap();
        let n = send_ok_body(&app, req).await?;
        assert!(n > 0);
        d_rendered_png.push(t0.elapsed());
    }

    let mut d_rendered_jpeg = Vec::with_capacity(cfg.iters);
    for (st, ser, sop) in &frame_coords {
        let uri = format!(
            "/dicomweb/studies/{st}/series/{ser}/instances/{sop}/frames/1/rendered?window=-600,1500&viewport=256,256&quality=85"
        );
        let t0 = Instant::now();
        let req = Request::builder()
            .uri(uri)
            .header(header::ACCEPT, "image/jpeg")
            .body(Body::empty())
            .unwrap();
        let n = send_ok_body(&app, req).await?;
        assert!(n > 0);
        d_rendered_jpeg.push(t0.elapsed());
    }

    let manifest_iters = cfg.iters.min(50);
    let mut d_manifest = Vec::with_capacity(manifest_iters);
    for (st, ser, _) in frame_coords.iter().take(manifest_iters) {
        let uri = format!("/api/studies/{st}/series/{ser}/frames-manifest");
        let t0 = Instant::now();
        let n = send_ok_body(&app, Request::builder().uri(uri).body(Body::empty()).unwrap()).await?;
        assert!(n > 0);
        d_manifest.push(t0.elapsed());
    }

    let search_iters = cfg.iters.min(50);
    let queries = ["spiculated nodule", "ring-enhancing frontal", "pleural effusion"];
    let mut d_bm25 = Vec::with_capacity(search_iters);
    let mut d_vec = Vec::with_capacity(search_iters);
    let mut d_hybrid = Vec::with_capacity(search_iters);
    for i in 0..search_iters {
        let q = queries[i % queries.len()];
        let t0 = Instant::now();
        let _ = store
            .search_series_with_mode(Some(q), None, None, 20, SearchMode::Bm25)
            .await
            .map_err(|e| e.to_string())?;
        d_bm25.push(t0.elapsed());

        let t1 = Instant::now();
        let _ = store
            .search_series_with_mode(Some(q), None, None, 20, SearchMode::Vector)
            .await
            .map_err(|e| e.to_string())?;
        d_vec.push(t1.elapsed());

        let t2 = Instant::now();
        let _ = store
            .search_series_with_mode(Some(q), None, None, 20, SearchMode::Hybrid)
            .await
            .map_err(|e| e.to_string())?;
        d_hybrid.push(t2.elapsed());
    }

    let knn_iters = cfg.iters.min(30);
    let mut d_knn = Vec::with_capacity(knn_iters);
    for (st, ser, sop) in frame_coords.iter().take(knn_iters) {
        let t0 = Instant::now();
        let hits = store
            .find_similar_frames(st, ser, sop, 1, 6, true)
            .await
            .map_err(|e| format!("find_similar_frames failed: {e}"))?;
        assert!(!hits.is_empty());
        d_knn.push(t0.elapsed());
    }

    let inc_study_uid = format!("1.2.826.0.1.3680043.10.9000.{}", cfg.studies);
    let t_del = Instant::now();
    let _ = store
        .delete_study(&inc_study_uid)
        .await
        .map_err(|e| format!("delete_study failed: {e}"))?;
    let _ = store
        .compact_and_cleanup(&MaintenanceOptions::default())
        .await
        .map_err(|e| format!("compact_and_cleanup failed: {e}"))?;
    let delete_and_compact_ms = t_del.elapsed().as_secs_f64() * 1000.0;

    let total_frames = report.frames_ingested;
    let raw_pixel_bytes = total_frames * cfg.dim * cfg.dim * 2;
    let ingest_ms = ingest_dur.as_secs_f64() * 1000.0;
    let ingest_fps = if ingest_dur.as_secs_f64() > 0.0 {
        total_frames as f64 / ingest_dur.as_secs_f64()
    } else {
        0.0
    };

    let latencies = vec![
        summarize_latencies("Lance take_blobs full frame", &d_blob_full),
        summarize_latencies("Lance take_blobs range (0..4096)", &d_blob_range),
        summarize_latencies("WADO-RS /frames (ExplicitVRLE)", &d_wado_raw),
        summarize_latencies("WADO-RS /frames (RLELossless)", &d_wado_rle),
        summarize_latencies("WADO-RS /rendered (PNG 256x256)", &d_rendered_png),
        summarize_latencies("WADO-RS /rendered (JPEG q=85)", &d_rendered_jpeg),
        summarize_latencies("GET /frames-manifest (series slices)", &d_manifest),
        summarize_latencies("Series Search (BM25 FTS)", &d_bm25),
        summarize_latencies("Series Search (768-d Vector)", &d_vec),
        summarize_latencies("Series Search (Hybrid RRF)", &d_hybrid),
        summarize_latencies("Find Similar Slices (768-d KNN across all frames)", &d_knn),
    ];

    let full_report = BenchmarkReport {
        studies: cfg.studies,
        slices_per_study: cfg.slices,
        frame_dim: cfg.dim,
        total_series: report.series_ingested,
        total_frames,
        raw_pixel_bytes,
        ingest_ms,
        ingest_frames_per_sec: ingest_fps,
        incremental_upsert_frames: cfg.slices,
        incremental_upsert_ms: incremental_dur.as_secs_f64() * 1000.0,
        series_dataset_bytes: series_bytes,
        frames_dataset_bytes: frames_bytes,
        blob_v2_kind_sample: stats.blob_v2_kind_sample.clone(),
        delete_and_compact_ms,
        latencies,
    };

    println!("# dicom-lance Benchmark Results\n");
    println!(
        "- Dataset: **{} studies**, **{} series**, **{} frames** (`{}x{}` 16-bit LE = **{:.1} MiB** raw pixels)",
        full_report.studies,
        full_report.total_series,
        full_report.total_frames,
        full_report.frame_dim,
        full_report.frame_dim,
        full_report.raw_pixel_bytes as f64 / (1024.0 * 1024.0),
    );
    println!(
        "- Storage: Lance `2.2`, Blob V2 kind = `{}` (`dicom_series.lance`: `{:.2} MiB`, `dicom_frames.lance`: `{:.2} MiB`)",
        full_report.blob_v2_kind_sample.as_deref().unwrap_or("unknown"),
        full_report.series_dataset_bytes as f64 / (1024.0 * 1024.0),
        full_report.frames_dataset_bytes as f64 / (1024.0 * 1024.0),
    );
    println!(
        "- Bulk Ingest (with embeddings + FTS/scalar index build): **{:.1} ms** (**{:.0} frames/sec**)",
        full_report.ingest_ms, full_report.ingest_frames_per_sec
    );
    println!(
        "- Incremental 1-study MergeInsert ({} frames + SR): **{:.1} ms**",
        full_report.incremental_upsert_frames, full_report.incremental_upsert_ms
    );
    println!(
        "- Delete Study + `compact_and_cleanup`: **{:.1} ms**\n",
        full_report.delete_and_compact_ms
    );
    println!("| Benchmark | Samples | Min (ms) | p50 (ms) | p95 (ms) | p99 (ms) | Max (ms) | Throughput (ops/s) |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for row in &full_report.latencies {
        println!(
            "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} |",
            row.name,
            row.samples,
            row.min_ms,
            row.p50_ms,
            row.p95_ms,
            row.p99_ms,
            row.max_ms,
            row.ops_per_sec
        );
    }

    if let Some(json_path) = &cfg.json_out {
        let json = serde_json::to_string_pretty(&full_report).map_err(|e| e.to_string())?;
        fs::write(json_path, json).map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_latencies_orders_percentiles_monotonically() {
        let samples = vec![
            Duration::from_millis(12),
            Duration::from_millis(4),
            Duration::from_millis(8),
            Duration::from_millis(25),
            Duration::from_millis(6),
        ];
        let stats = summarize_latencies("test", &samples);
        assert_eq!(stats.samples, 5);
        assert!(stats.min_ms <= stats.p50_ms);
        assert!(stats.p50_ms <= stats.p95_ms);
        assert!(stats.p95_ms <= stats.p99_ms);
        assert!(stats.p99_ms <= stats.max_ms);
        assert!(stats.ops_per_sec > 0.0);
    }

    #[test]
    fn synthetic_study_pair_matches_requested_slice_count() {
        let tpl = build_pixel_template(32);
        let pair = generate_synthetic_study_pair(2, 16, 32, &tpl);
        assert_eq!(pair.len(), 2);
        assert_eq!(pair[0].frames.len(), 16);
        assert_eq!(pair[0].frames[0].pixel_bytes.len(), 32 * 32 * 2);
        assert_eq!(pair[1].series.modality, "SR");
        assert!(pair[1].frames.is_empty());
    }
}
