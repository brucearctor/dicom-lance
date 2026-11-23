use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::schema::{FrameRecord, SeriesRecord, EMBEDDING_DIM};

pub const TEXT_DIMS: usize = 512;
pub const PIXEL_DIMS: usize = 256;

const _: () = assert!(TEXT_DIMS + PIXEL_DIMS == EMBEDDING_DIM as usize);

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid embedding response: {0}")]
    InvalidResponse(String),
}

pub trait Embedder: Send + Sync {
    fn name(&self) -> &'static str;

    fn embed_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>>;

    fn embed_frame<'a>(
        &'a self,
        frame: &'a FrameRecord,
        series_text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>>;

    fn embed_series<'a>(
        &'a self,
        series: &'a SeriesRecord,
        frames: &'a [FrameRecord],
    ) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>>;
}

/// Sanitizes non-finite values and normalizes `vec` to unit L2 norm.
/// All-zero or degenerate vectors fall back cleanly to the canonical unit vector e_0.
pub fn l2_normalize(vec: &mut [f32]) {
    if vec.is_empty() {
        return;
    }
    let mut norm_sq = 0.0_f64;
    for v in vec.iter_mut() {
        if v.is_finite() {
            let d = f64::from(*v);
            norm_sq += d * d;
        } else {
            *v = 0.0;
        }
    }
    let norm = norm_sq.sqrt();
    if norm > 1e-12 && norm.is_finite() {
        for v in vec.iter_mut() {
            let scaled = (f64::from(*v) / norm) as f32;
            *v = if scaled.is_finite() { scaled } else { 0.0 };
        }
    } else {
        vec.fill(0.0);
        vec[0] = 1.0;
    }
}

fn normalize_slice_in_place(slice: &mut [f32]) {
    let mut norm_sq = 0.0_f64;
    for v in slice.iter_mut() {
        if v.is_finite() {
            let d = f64::from(*v);
            norm_sq += d * d;
        } else {
            *v = 0.0;
        }
    }
    let norm = norm_sq.sqrt();
    if norm > 1e-12 && norm.is_finite() {
        for v in slice.iter_mut() {
            let scaled = (f64::from(*v) / norm) as f32;
            *v = if scaled.is_finite() { scaled } else { 0.0 };
        }
    }
}

fn add_hashed_feature(out: &mut [f32; TEXT_DIMS], namespace: &str, token: &str, weight: f32) {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update(b":");
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let idx = (u16::from_le_bytes([digest[0], digest[1]]) as usize) % TEXT_DIMS;
    out[idx] += weight;
}

/// Deterministic 512-d text projection via non-negative SHA-256 feature hashing over
/// word unigrams, bigrams, word prefixes (p3/p4), and character 4-grams.
pub fn hash_text_features(text: &str) -> [f32; TEXT_DIMS] {
    let mut out = [0.0_f32; TEXT_DIMS];
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(|tok| tok.to_ascii_lowercase())
        .collect();

    if tokens.is_empty() {
        return out;
    }

    for tok in &tokens {
        add_hashed_feature(&mut out, "u", tok, 1.0);
        if tok.len() >= 3 {
            add_hashed_feature(&mut out, "p3", &tok[..3], 0.45);
        }
        if tok.len() >= 4 {
            add_hashed_feature(&mut out, "p4", &tok[..4], 0.65);
            for win in tok.as_bytes().windows(4) {
                if let Ok(gram) = std::str::from_utf8(win) {
                    add_hashed_feature(&mut out, "c4", gram, 0.30);
                }
            }
        }
    }

    for pair in tokens.windows(2) {
        let bigram = format!("{}_{}", pair[0], pair[1]);
        add_hashed_feature(&mut out, "b", &bigram, 0.75);
    }

    normalize_slice_in_place(&mut out);
    out
}

/// Deterministic 256-d image radiomics & spatial signature extracted from raw DICOM pixels.
pub fn extract_pixel_radiomics(frame: &FrameRecord) -> [f32; PIXEL_DIMS] {
    let mut out = [0.0_f32; PIXEL_DIMS];
    if frame.pixel_bytes.is_empty()
        || frame.rows <= 0
        || frame.columns <= 0
        || frame.samples_per_pixel <= 0
        || frame.bits_allocated <= 0
    {
        return out;
    }

    let rows = frame.rows as usize;
    let cols = frame.columns as usize;
    let spp = frame.samples_per_pixel as usize;
    let bytes_per_sample = ((frame.bits_allocated + 7) / 8) as usize;
    if !matches!(bytes_per_sample, 1 | 2 | 4) {
        return out;
    }
    let Some(expected) = rows
        .checked_mul(cols)
        .and_then(|v| v.checked_mul(spp))
        .and_then(|v| v.checked_mul(bytes_per_sample))
    else {
        return out;
    };
    if frame.pixel_bytes.len() < expected {
        return out;
    }

    let slope = if frame.rescale_slope.is_finite() {
        frame.rescale_slope
    } else {
        1.0
    };
    let intercept = if frame.rescale_intercept.is_finite() {
        frame.rescale_intercept
    } else {
        0.0
    };

    let read_scalar = |idx: usize| -> f64 {
        let base = idx * spp * bytes_per_sample;
        let raw = match bytes_per_sample {
            4 => {
                let b = [
                    frame.pixel_bytes[base],
                    frame.pixel_bytes[base + 1],
                    frame.pixel_bytes[base + 2],
                    frame.pixel_bytes[base + 3],
                ];
                if frame.pixel_representation == 1 {
                    i32::from_le_bytes(b) as f64
                } else {
                    u32::from_le_bytes(b) as f64
                }
            }
            2 => {
                let b = [frame.pixel_bytes[base], frame.pixel_bytes[base + 1]];
                if frame.pixel_representation == 1 {
                    i16::from_le_bytes(b) as f64
                } else {
                    u16::from_le_bytes(b) as f64
                }
            }
            _ => frame.pixel_bytes[base] as f64,
        };
        let val = raw * slope + intercept;
        if val.is_finite() {
            val
        } else {
            0.0
        }
    };

    const GRID: usize = 64;
    let mut raw_grid = [0.0_f64; GRID * GRID];
    let mut min_v = f64::INFINITY;
    let mut max_v = f64::NEG_INFINITY;
    let mut raw_sum = 0.0_f64;

    for gy in 0..GRID {
        let sy = (gy * rows) / GRID;
        for gx in 0..GRID {
            let sx = (gx * cols) / GRID;
            let v = read_scalar(sy * cols + sx);
            raw_grid[gy * GRID + gx] = v;
            if v < min_v {
                min_v = v;
            }
            if v > max_v {
                max_v = v;
            }
            raw_sum += v;
        }
    }

    let n_grid = (GRID * GRID) as f64;
    let raw_mean = raw_sum / n_grid;
    let mut raw_var_acc = 0.0_f64;
    for &v in &raw_grid {
        let d = v - raw_mean;
        raw_var_acc += d * d;
    }
    let raw_std = (raw_var_acc / n_grid).sqrt();

    let (wc, ww) = if frame.window_center.is_finite()
        && frame.window_width.is_finite()
        && frame.window_width > 1.0
    {
        (frame.window_center, frame.window_width)
    } else if min_v.is_finite()
        && max_v.is_finite()
        && max_v > min_v
        && (max_v - min_v).is_finite()
    {
        let span = max_v - min_v;
        (min_v + span * 0.5, span.max(1.0))
    } else {
        (40.0, 400.0)
    };

    let lower = wc - ww * 0.5;
    let inv_ww = 1.0 / ww.max(1.0);
    let mut norm_grid = [0.0_f64; GRID * GRID];
    for (dst, &v) in norm_grid.iter_mut().zip(raw_grid.iter()) {
        let scaled = ((v - lower) * inv_ww).clamp(0.0, 1.0);
        *dst = if scaled.is_finite() { scaled } else { 0.0 };
    }

    // 0..64: 64-bin normalized intensity histogram
    for &v in &norm_grid {
        let bin = ((v * 63.0).round() as usize).min(63);
        out[bin] += 1.0 / (n_grid as f32);
    }

    // 64..128: 4x4 spatial blocks (mean, std, min, max)
    const BLOCK4: usize = GRID / 4;
    for by in 0..4 {
        for bx in 0..4 {
            let mut sum = 0.0_f64;
            let mut bmin = 1.0_f64;
            let mut bmax = 0.0_f64;
            for dy in 0..BLOCK4 {
                let y = by * BLOCK4 + dy;
                for dx in 0..BLOCK4 {
                    let x = bx * BLOCK4 + dx;
                    let v = norm_grid[y * GRID + x];
                    sum += v;
                    bmin = bmin.min(v);
                    bmax = bmax.max(v);
                }
            }
            let cnt = (BLOCK4 * BLOCK4) as f64;
            let mean = sum / cnt;
            let mut var = 0.0_f64;
            for dy in 0..BLOCK4 {
                let y = by * BLOCK4 + dy;
                for dx in 0..BLOCK4 {
                    let x = bx * BLOCK4 + dx;
                    let d = norm_grid[y * GRID + x] - mean;
                    var += d * d;
                }
            }
            let base = 64 + (by * 4 + bx) * 4;
            out[base] = mean as f32;
            out[base + 1] = (var / cnt).sqrt() as f32;
            out[base + 2] = bmin as f32;
            out[base + 3] = bmax as f32;
        }
    }

    // 128..192: 8x8 spatial mean intensity map
    const BLOCK8: usize = GRID / 8;
    for by in 0..8 {
        for bx in 0..8 {
            let mut sum = 0.0_f64;
            for dy in 0..BLOCK8 {
                let y = by * BLOCK8 + dy;
                for dx in 0..BLOCK8 {
                    let x = bx * BLOCK8 + dx;
                    sum += norm_grid[y * GRID + x];
                }
            }
            out[128 + by * 8 + bx] = (sum / ((BLOCK8 * BLOCK8) as f64)) as f32;
        }
    }

    // 192..224: 16 concentric radial ring means + 16 radial ring stds
    let mut ring_sum = [0.0_f64; 16];
    let mut ring_sum_sq = [0.0_f64; 16];
    let mut ring_cnt = [0usize; 16];
    let center = (GRID as f64 - 1.0) * 0.5;
    let max_radius = (2.0_f64 * center * center).sqrt().max(1.0);
    for gy in 0..GRID {
        let dy = gy as f64 - center;
        for gx in 0..GRID {
            let dx = gx as f64 - center;
            let r = (dx * dx + dy * dy).sqrt() / max_radius;
            let bin = ((r * 15.0).floor() as usize).min(15);
            let v = norm_grid[gy * GRID + gx];
            ring_sum[bin] += v;
            ring_sum_sq[bin] += v * v;
            ring_cnt[bin] += 1;
        }
    }
    for r in 0..16 {
        if ring_cnt[r] > 0 {
            let c = ring_cnt[r] as f64;
            let mean = ring_sum[r] / c;
            let var = (ring_sum_sq[r] / c - mean * mean).max(0.0);
            out[192 + r] = mean as f32;
            out[208 + r] = var.sqrt() as f32;
        }
    }

    // 224..240: 16-bin finite-difference gradient orientation histogram
    let mut grad_bins = [0.0_f64; 16];
    let mut grad_total = 0.0_f64;
    for gy in 1..(GRID - 1) {
        for gx in 1..(GRID - 1) {
            let gx_val = norm_grid[gy * GRID + (gx + 1)] - norm_grid[gy * GRID + (gx - 1)];
            let gy_val = norm_grid[(gy + 1) * GRID + gx] - norm_grid[(gy - 1) * GRID + gx];
            let mag = (gx_val * gx_val + gy_val * gy_val).sqrt();
            if mag > 1e-6 && mag.is_finite() {
                let mut angle = gy_val.atan2(gx_val);
                if angle < 0.0 {
                    angle += std::f64::consts::TAU;
                }
                let bin = (((angle / std::f64::consts::TAU) * 16.0).floor() as usize).min(15);
                grad_bins[bin] += mag;
                grad_total += mag;
            }
        }
    }
    if grad_total > 1e-9 {
        for (dst, &val) in out[224..240].iter_mut().zip(grad_bins.iter()) {
            *dst = (val / grad_total) as f32;
        }
    }

    // 240..256: Global summary, quantile proxies & acquisition geometry descriptors
    let mut norm_sum = 0.0_f64;
    let mut active_cnt = 0usize;
    for &v in &norm_grid {
        norm_sum += v;
        if v > 0.05 {
            active_cnt += 1;
        }
    }
    let norm_mean = norm_sum / n_grid;
    let mut norm_var = 0.0_f64;
    for &v in &norm_grid {
        let d = v - norm_mean;
        norm_var += d * d;
    }

    let mut sorted = norm_grid;
    sorted.sort_by(|a, b| a.total_cmp(b));
    let q_idx = |pct: f64| -> f32 {
        let idx = ((pct * (sorted.len() - 1) as f64).round() as usize).min(sorted.len() - 1);
        sorted[idx] as f32
    };

    out[240] = norm_mean as f32;
    out[241] = (norm_var / n_grid).sqrt() as f32;
    out[242] = q_idx(0.10);
    out[243] = q_idx(0.25);
    out[244] = q_idx(0.50);
    out[245] = q_idx(0.75);
    out[246] = q_idx(0.90);
    out[247] = (active_cnt as f64 / n_grid) as f32;
    out[248] = (raw_mean / 1000.0).tanh() as f32;
    out[249] = (raw_std / 1000.0).tanh() as f32;
    out[250] = (wc / 1000.0).tanh() as f32;
    out[251] = (ww / 2000.0).tanh() as f32;
    let thickness = if frame.slice_thickness.is_finite() {
        frame.slice_thickness
    } else {
        1.0
    };
    let row_sp = if frame.pixel_spacing_row_mm.is_finite() {
        frame.pixel_spacing_row_mm
    } else {
        1.0
    };
    let col_sp = if frame.pixel_spacing_col_mm.is_finite() {
        frame.pixel_spacing_col_mm
    } else {
        1.0
    };
    out[252] = (thickness / 10.0).tanh() as f32;
    out[253] = (row_sp / 5.0).tanh() as f32;
    out[254] = (col_sp / 5.0).tanh() as f32;
    out[255] = (rows as f64 / 512.0).tanh() as f32;

    normalize_slice_in_place(&mut out);
    out
}

fn compose_multimodal_vector(
    text_slice: &[f32; TEXT_DIMS],
    pixel_slice: &[f32; PIXEL_DIMS],
    text_weight: f32,
    pixel_weight: f32,
) -> Vec<f32> {
    let mut out = vec![0.0_f32; EMBEDDING_DIM as usize];
    for (dst, &val) in out[..TEXT_DIMS].iter_mut().zip(text_slice.iter()) {
        *dst = val * text_weight;
    }
    for (dst, &val) in out[TEXT_DIMS..].iter_mut().zip(pixel_slice.iter()) {
        *dst = val * pixel_weight;
    }
    l2_normalize(&mut out);
    out
}

/// Default pure-Rust multimodal embedder combining 512-d text n-gram hashing
/// and 256-d pixel radiomics/spatial descriptors into a unit-norm 768-d vector.
#[derive(Debug, Default, Clone)]
pub struct LocalMultimodalEmbedder;

impl Embedder for LocalMultimodalEmbedder {
    fn name(&self) -> &'static str {
        "local-multimodal-768"
    }

    fn embed_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>> {
        Box::pin(async move {
            let text_feat = hash_text_features(text);
            let zero_pix = [0.0_f32; PIXEL_DIMS];
            Ok(compose_multimodal_vector(&text_feat, &zero_pix, 1.0, 0.0))
        })
    }

    fn embed_frame<'a>(
        &'a self,
        frame: &'a FrameRecord,
        series_text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>> {
        Box::pin(async move {
            let text_feat = hash_text_features(series_text);
            let pixel_feat = extract_pixel_radiomics(frame);
            Ok(compose_multimodal_vector(
                &text_feat,
                &pixel_feat,
                0.4359,
                0.90,
            ))
        })
    }

    fn embed_series<'a>(
        &'a self,
        series: &'a SeriesRecord,
        frames: &'a [FrameRecord],
    ) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>> {
        Box::pin(async move {
            let text_feat = hash_text_features(&series.searchable_text);
            let mut avg_pixel = [0.0_f32; PIXEL_DIMS];
            if !frames.is_empty() {
                let sample_count = frames.len().min(8);
                for s_idx in 0..sample_count {
                    let f_idx = (s_idx * frames.len()) / sample_count;
                    let feat = extract_pixel_radiomics(&frames[f_idx]);
                    for (acc, &v) in avg_pixel.iter_mut().zip(feat.iter()) {
                        *acc += v / (sample_count as f32);
                    }
                }
                normalize_slice_in_place(&mut avg_pixel);
            }
            Ok(compose_multimodal_vector(
                &text_feat,
                &avg_pixel,
                0.85,
                0.5268,
            ))
        })
    }
}

#[derive(Serialize)]
struct EmbeddingApiRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingApiResponse {
    data: Vec<EmbeddingApiItem>,
}

#[derive(Deserialize)]
struct EmbeddingApiItem {
    embedding: Vec<f32>,
}

/// Pluggable OpenAI-compatible `/v1/embeddings` HTTP client for external clinical text
/// or vision-language models (e.g., BioMedCLIP, MedSigLIP, Ollama, TEI).
#[derive(Debug, Clone)]
pub struct HttpEmbedder {
    client: reqwest::Client,
    endpoint_url: String,
    api_key: Option<String>,
    model: String,
}

impl HttpEmbedder {
    pub fn new(endpoint_url: String, api_key: Option<String>, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            endpoint_url,
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            model,
        }
    }

    async fn fetch_normalized_embedding(&self, input: &str) -> Result<Vec<f32>, EmbedError> {
        let mut req = self.client.post(&self.endpoint_url).json(&EmbeddingApiRequest {
            input,
            model: &self.model,
        });
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EmbedError::InvalidResponse(format!(
                "endpoint returned HTTP {status}: {body}"
            )));
        }

        let parsed: EmbeddingApiResponse = resp.json().await?;
        let raw = parsed
            .data
            .into_iter()
            .next()
            .map(|item| item.embedding)
            .ok_or_else(|| {
                EmbedError::InvalidResponse("response missing data[0].embedding".to_string())
            })?;

        if raw.is_empty() || raw.iter().all(|v| !v.is_finite() || *v == 0.0) {
            return Err(EmbedError::InvalidResponse(
                "endpoint returned empty or degenerate embedding vector".to_string(),
            ));
        }

        let dim = EMBEDDING_DIM as usize;
        let mut out = vec![0.0_f32; dim];
        let copy_len = raw.len().min(dim);
        for (dst, &src) in out[..copy_len].iter_mut().zip(raw[..copy_len].iter()) {
            *dst = if src.is_finite() { src } else { 0.0 };
        }
        l2_normalize(&mut out);
        Ok(out)
    }
}

impl Embedder for HttpEmbedder {
    fn name(&self) -> &'static str {
        "openai-compatible-http"
    }

    fn embed_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>> {
        Box::pin(async move { self.fetch_normalized_embedding(text).await })
    }

    fn embed_frame<'a>(
        &'a self,
        frame: &'a FrameRecord,
        series_text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>> {
        Box::pin(async move {
            let mut text_feat = [0.0_f32; TEXT_DIMS];
            if !series_text.trim().is_empty() {
                let text_vec = self.fetch_normalized_embedding(series_text).await?;
                text_feat.copy_from_slice(&text_vec[..TEXT_DIMS]);
                normalize_slice_in_place(&mut text_feat);
            }
            let pixel_feat = extract_pixel_radiomics(frame);
            Ok(compose_multimodal_vector(
                &text_feat,
                &pixel_feat,
                0.4359,
                0.90,
            ))
        })
    }

    fn embed_series<'a>(
        &'a self,
        series: &'a SeriesRecord,
        _frames: &'a [FrameRecord],
    ) -> BoxFuture<'a, Result<Vec<f32>, EmbedError>> {
        Box::pin(async move {
            let text = if series.searchable_text.trim().is_empty() {
                series.series_description.as_str()
            } else {
                series.searchable_text.as_str()
            };
            self.fetch_normalized_embedding(text).await
        })
    }
}

/// Reciprocal Rank Fusion over two ranked ID lists (`score = sum 1 / (k + rank)`),
/// preserving each ID's best (1-based) rank within each list and sorting descending by score.
pub fn rrf_fuse(bm25_ids: &[String], vector_ids: &[String], k: f32) -> Vec<(String, f32)> {
    let safe_k = if k.is_finite() && k > 0.0 { k } else { 60.0 };
    let mut scores: HashMap<String, f32> = HashMap::new();

    for list in [bm25_ids, vector_ids] {
        let mut seen = HashSet::new();
        let mut rank = 0usize;
        for id in list {
            if seen.insert(id.as_str()) {
                rank += 1;
                let contrib = 1.0_f32 / (safe_k + rank as f32);
                *scores.entry(id.clone()).or_insert(0.0) += contrib;
            }
        }
    }

    let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}
