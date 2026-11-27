use std::cmp::Ordering;
use std::collections::BTreeMap;

use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEncoder};

use crate::codecs::{split_quoted_aware, unquote};

pub const SUPPORTED_RENDERED_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg"];
const MAX_DIMENSION: u32 = 16384;
const MAX_VIEWPORT_DIMENSION: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedImageFormat {
    Png,
    Jpeg,
}

impl RenderedImageFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedRenderedFormat {
    pub format: RenderedImageFormat,
    pub multipart: bool,
}

#[derive(Debug, Clone, Copy)]
struct RenderedCandidate {
    q: f32,
    specificity: u8,
    order: usize,
    negotiated: NegotiatedRenderedFormat,
}

fn match_rendered_media_range(
    base_type: &str,
    params: &BTreeMap<String, String>,
    force_multipart: bool,
) -> Option<(u8, NegotiatedRenderedFormat)> {
    let (slash_l, slash_r) = base_type.split_once('/')?;
    if slash_l.is_empty() || slash_r.is_empty() || slash_r.contains('/') {
        return None;
    }

    if base_type == "multipart/related" {
        let part_type = params
            .get("type")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "image/png".to_string());
        let format = match part_type.as_str() {
            "image/png" => RenderedImageFormat::Png,
            "image/jpeg" | "image/jpg" => RenderedImageFormat::Jpeg,
            "image/*" | "*/*" => RenderedImageFormat::Png,
            _ => return None,
        };
        let spec = if part_type == "image/png" || part_type == "image/jpeg" || part_type == "image/jpg" {
            3
        } else {
            1
        };
        return Some((
            spec,
            NegotiatedRenderedFormat {
                format,
                multipart: true,
            },
        ));
    }

    let (spec, format) = match base_type {
        "image/png" => (3, RenderedImageFormat::Png),
        "image/jpeg" | "image/jpg" => (3, RenderedImageFormat::Jpeg),
        "image/*" => (1, RenderedImageFormat::Png),
        "*/*" => (0, RenderedImageFormat::Png),
        _ => return None,
    };

    Some((
        spec,
        NegotiatedRenderedFormat {
            format,
            multipart: force_multipart,
        },
    ))
}

/// Negotiates a WADO-RS `/rendered` output format (`image/png`, `image/jpeg`, or `multipart/related`)
/// using quality-weighted RFC 9110 / DICOM PS3.18 `Accept` rules.
pub fn negotiate_rendered_accept(
    accept_header: Option<&str>,
    force_multipart: bool,
) -> Result<NegotiatedRenderedFormat, String> {
    let raw = match accept_header {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => {
            return Ok(NegotiatedRenderedFormat {
                format: RenderedImageFormat::Png,
                multipart: force_multipart,
            })
        }
    };

    let mut candidates = Vec::new();
    for (order, range_str) in split_quoted_aware(raw, ',').into_iter().enumerate() {
        if range_str.is_empty() {
            continue;
        }
        let segments = split_quoted_aware(&range_str, ';');
        let Some(base_raw) = segments.first() else {
            continue;
        };
        let base_type = base_raw.trim().to_ascii_lowercase();
        let mut params = BTreeMap::new();
        let mut q = 1.0_f32;
        let mut q_valid = true;

        for seg in segments.iter().skip(1) {
            if seg.is_empty() {
                continue;
            }
            if let Some((k, v)) = seg.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                let val = unquote(v);
                if key == "q" {
                    match val.parse::<f32>() {
                        Ok(parsed_q)
                            if parsed_q.is_finite() && (0.0..=1.0).contains(&parsed_q) =>
                        {
                            q = parsed_q;
                        }
                        _ => {
                            q_valid = false;
                            break;
                        }
                    }
                } else {
                    params.insert(key, val);
                }
            }
        }

        if !q_valid || q <= 0.0 {
            continue;
        }

        if let Some((specificity, negotiated)) =
            match_rendered_media_range(&base_type, &params, force_multipart)
        {
            candidates.push(RenderedCandidate {
                q,
                specificity,
                order,
                negotiated,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.q.total_cmp(&a.q)
            .then_with(|| b.specificity.cmp(&a.specificity))
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| Ordering::Equal)
    });

    candidates
        .into_iter()
        .next()
        .map(|c| c.negotiated)
        .ok_or_else(|| {
            format!(
                "unsupported /rendered Accept header {raw:?}; supported media types: {}",
                SUPPORTED_RENDERED_MEDIA_TYPES.join(", ")
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiLutFunction {
    #[default]
    Linear,
    LinearExact,
    Sigmoid,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSpec {
    pub center: f64,
    pub width: f64,
    pub function: VoiLutFunction,
}

/// Parses PS3.18 `window=center,width[,function]` (`LINEAR`, `LINEAR_EXACT`, `SIGMOID`).
pub fn parse_window_param(raw: &str) -> Result<WindowSpec, String> {
    let parts: Vec<&str> = raw.trim().split(',').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return Err(format!(
            "invalid window parameter {raw:?}: expected center,width[,LINEAR|LINEAR_EXACT|SIGMOID]"
        ));
    }

    let center = parts[0]
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("invalid finite window center in {raw:?}"))?;

    let mut width = parts[1]
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("invalid finite window width in {raw:?}"))?;

    let function = if parts.len() == 3 {
        match parts[2].trim().to_ascii_uppercase().as_str() {
            "LINEAR" => VoiLutFunction::Linear,
            "LINEAR_EXACT" => VoiLutFunction::LinearExact,
            "SIGMOID" => VoiLutFunction::Sigmoid,
            other => {
                return Err(format!(
                    "unsupported VOI LUT function {other:?}; expected LINEAR, LINEAR_EXACT, or SIGMOID"
                ))
            }
        }
    } else {
        VoiLutFunction::Linear
    };

    match function {
        VoiLutFunction::LinearExact => {
            if width < 0.0 {
                return Err(format!("window width must be >= 0 in {raw:?}"));
            }
            width = width.max(1e-6);
        }
        VoiLutFunction::Linear | VoiLutFunction::Sigmoid => {
            if width <= 0.0 {
                return Err(format!("window width must be > 0 in {raw:?}"));
            }
        }
    }

    Ok(WindowSpec {
        center,
        width,
        function,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportSpec {
    pub width: u32,
    pub height: u32,
}

/// Parses PS3.18 `viewport=vw,vh` bounding dimensions in pixels (`1..=4096`).
pub fn parse_viewport_param(raw: &str) -> Result<ViewportSpec, String> {
    let parts: Vec<&str> = raw.trim().split(',').collect();
    if parts.len() != 2 {
        return Err(format!(
            "invalid viewport parameter {raw:?}: expected vw,vh integer pixel bounds"
        ));
    }
    let width = parts[0]
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid viewport width in {raw:?}"))?;
    let height = parts[1]
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid viewport height in {raw:?}"))?;
    if width == 0
        || height == 0
        || width > MAX_VIEWPORT_DIMENSION
        || height > MAX_VIEWPORT_DIMENSION
    {
        return Err(format!(
            "viewport dimensions must be in 1..={MAX_VIEWPORT_DIMENSION}, got {width}x{height}"
        ));
    }
    Ok(ViewportSpec { width, height })
}

/// Validates JPEG encoding quality (`1..=100`, default `85`).
pub fn parse_quality_param(raw: Option<u8>) -> Result<u8, String> {
    let q = raw.unwrap_or(85);
    if !(1..=100).contains(&q) {
        return Err(format!("quality must be in 1..=100, got {q}"));
    }
    Ok(q)
}

/// Validates PS3.18 `iccprofile` query parameter (`no`, `yes`, or `srgb`).
pub fn parse_iccprofile_param(raw: Option<&str>) -> Result<(), String> {
    if let Some(val) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        if !matches!(val.to_ascii_lowercase().as_str(), "no" | "yes" | "srgb") {
            return Err(format!(
                "unsupported iccprofile {val:?}; supported values: no, yes, srgb"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RenderFrameInput {
    pub rows: i32,
    pub columns: i32,
    pub samples_per_pixel: i32,
    pub bits_allocated: i32,
    pub pixel_representation: i32,
    pub photometric_interpretation: String,
    pub rescale_slope: f64,
    pub rescale_intercept: f64,
    pub window_center: f64,
    pub window_width: f64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub window: Option<WindowSpec>,
    pub viewport: Option<ViewportSpec>,
    pub quality: u8,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            window: None,
            viewport: None,
            quality: 85,
        }
    }
}

#[inline]
fn read_monochrome_sample(
    bytes: &[u8],
    px_idx: usize,
    bytes_per_sample: usize,
    signed: bool,
    slope: f64,
    intercept: f64,
) -> f64 {
    let base = px_idx * bytes_per_sample;
    let raw = match bytes_per_sample {
        4 => {
            let arr = [bytes[base], bytes[base + 1], bytes[base + 2], bytes[base + 3]];
            if signed {
                i32::from_le_bytes(arr) as f64
            } else {
                u32::from_le_bytes(arr) as f64
            }
        }
        2 => {
            let arr = [bytes[base], bytes[base + 1]];
            if signed {
                i16::from_le_bytes(arr) as f64
            } else {
                u16::from_le_bytes(arr) as f64
            }
        }
        _ => {
            if signed {
                (bytes[base] as i8) as f64
            } else {
                bytes[base] as f64
            }
        }
    };
    let val = raw * slope + intercept;
    if val.is_finite() {
        val
    } else {
        0.0
    }
}

#[inline]
fn apply_voi_lut(val: f64, spec: WindowSpec, invert_monochrome1: bool) -> u8 {
    let norm = match spec.function {
        VoiLutFunction::Linear => {
            let lower = spec.center - 0.5 - (spec.width - 1.0) * 0.5;
            let denom = (spec.width - 1.0).max(1.0);
            if val <= lower {
                0.0
            } else if val > spec.center - 0.5 + (spec.width - 1.0) * 0.5 {
                255.0
            } else {
                ((val - lower) / denom) * 255.0
            }
        }
        VoiLutFunction::LinearExact => {
            let w = spec.width.max(1e-6);
            ((val - (spec.center - w * 0.5)) / w) * 255.0
        }
        VoiLutFunction::Sigmoid => {
            let w = spec.width.max(1e-6);
            255.0 / (1.0 + (-4.0 * (val - spec.center) / w).exp())
        }
    };
    let clamped = norm.clamp(0.0, 255.0).round() as u8;
    if invert_monochrome1 {
        255 - clamped
    } else {
        clamped
    }
}

fn bilinear_resize(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    channels: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut dst = vec![0u8; dst_w * dst_h * channels];
    let x_ratio = if dst_w > 1 {
        (src_w.saturating_sub(1)) as f64 / (dst_w - 1) as f64
    } else {
        0.0
    };
    let y_ratio = if dst_h > 1 {
        (src_h.saturating_sub(1)) as f64 / (dst_h - 1) as f64
    } else {
        0.0
    };

    for dy in 0..dst_h {
        let sy = (dy as f64 * y_ratio).clamp(0.0, (src_h.saturating_sub(1)) as f64);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(src_h - 1);
        let fy = sy - y0 as f64;
        let wy0 = 1.0 - fy;

        for dx in 0..dst_w {
            let sx = (dx as f64 * x_ratio).clamp(0.0, (src_w.saturating_sub(1)) as f64);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(src_w - 1);
            let fx = sx - x0 as f64;
            let wx0 = 1.0 - fx;

            let idx00 = (y0 * src_w + x0) * channels;
            let idx10 = (y0 * src_w + x1) * channels;
            let idx01 = (y1 * src_w + x0) * channels;
            let idx11 = (y1 * src_w + x1) * channels;
            let dst_idx = (dy * dst_w + dx) * channels;

            for c in 0..channels {
                let top = src[idx00 + c] as f64 * wx0 + src[idx10 + c] as f64 * fx;
                let bot = src[idx01 + c] as f64 * wx0 + src[idx11 + c] as f64 * fx;
                dst[dst_idx + c] = (top * wy0 + bot * fy).clamp(0.0, 255.0).round() as u8;
            }
        }
    }

    dst
}

fn fit_viewport_dims(src_w: u32, src_h: u32, vp: ViewportSpec) -> (u32, u32) {
    let scale = (vp.width as f64 / src_w as f64).min(vp.height as f64 / src_h as f64);
    let out_w = ((src_w as f64 * scale).round() as u32).clamp(1, MAX_VIEWPORT_DIMENSION);
    let out_h = ((src_h as f64 * scale).round() as u32).clamp(1, MAX_VIEWPORT_DIMENSION);
    (out_w, out_h)
}

/// Applies modality rescale + VOI LUT windowing + aspect-preserving viewport scaling and encodes PNG/JPEG.
pub fn render_frame_image(
    frame: &RenderFrameInput,
    format: RenderedImageFormat,
    opts: &RenderOptions,
) -> Result<Vec<u8>, String> {
    if frame.rows <= 0
        || frame.columns <= 0
        || frame.rows as u32 > MAX_DIMENSION
        || frame.columns as u32 > MAX_DIMENSION
    {
        return Err(format!(
            "invalid frame dimensions {}x{}",
            frame.columns, frame.rows
        ));
    }
    if frame.bits_allocated <= 0 {
        return Err(format!("invalid bits_allocated: {}", frame.bits_allocated));
    }
    let quality = parse_quality_param(Some(opts.quality))?;

    let rows = frame.rows as usize;
    let cols = frame.columns as usize;
    let spp = frame.samples_per_pixel;
    let bytes_per_sample = ((frame.bits_allocated + 7) / 8) as usize;

    let (channels, base_pixels): (usize, Vec<u8>) = match spp {
        1 => {
            if !matches!(bytes_per_sample, 1 | 2 | 4) {
                return Err(format!(
                    "unsupported monochrome sample width: {} bytes",
                    bytes_per_sample
                ));
            }
            let expected = rows
                .checked_mul(cols)
                .and_then(|n| n.checked_mul(bytes_per_sample))
                .ok_or_else(|| "pixel buffer size overflow".to_string())?;
            if frame.bytes.len() < expected {
                return Err(format!(
                    "truncated pixel frame buffer: got {} bytes, expected {expected}",
                    frame.bytes.len()
                ));
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
            let signed = frame.pixel_representation == 1;
            let invert_m1 = frame
                .photometric_interpretation
                .trim()
                .eq_ignore_ascii_case("MONOCHROME1");

            let spec = if let Some(w) = opts.window {
                w
            } else if frame.window_center.is_finite()
                && frame.window_width.is_finite()
                && frame.window_width > 1.0
            {
                WindowSpec {
                    center: frame.window_center,
                    width: frame.window_width,
                    function: VoiLutFunction::Linear,
                }
            } else {
                let mut min_v = f64::INFINITY;
                let mut max_v = f64::NEG_INFINITY;
                let total_px = rows * cols;
                let step = (total_px / 2048).max(1);
                for px_idx in (0..total_px).step_by(step) {
                    let v = read_monochrome_sample(
                        &frame.bytes,
                        px_idx,
                        bytes_per_sample,
                        signed,
                        slope,
                        intercept,
                    );
                    if v < min_v {
                        min_v = v;
                    }
                    if v > max_v {
                        max_v = v;
                    }
                }
                if min_v.is_finite() && max_v.is_finite() && max_v > min_v {
                    let span = max_v - min_v;
                    WindowSpec {
                        center: min_v + span * 0.5,
                        width: span.max(1.0),
                        function: VoiLutFunction::Linear,
                    }
                } else {
                    WindowSpec {
                        center: 40.0,
                        width: 400.0,
                        function: VoiLutFunction::Linear,
                    }
                }
            };

            let total_px = rows * cols;
            let mut out = Vec::with_capacity(total_px);
            for px_idx in 0..total_px {
                let v = read_monochrome_sample(
                    &frame.bytes,
                    px_idx,
                    bytes_per_sample,
                    signed,
                    slope,
                    intercept,
                );
                out.push(apply_voi_lut(v, spec, invert_m1));
            }
            (1, out)
        }
        3 => {
            if frame.bits_allocated != 8 {
                return Err(format!(
                    "unsupported 3-channel bits_allocated={} (expected 8)",
                    frame.bits_allocated
                ));
            }
            let expected = rows
                .checked_mul(cols)
                .and_then(|n| n.checked_mul(3))
                .ok_or_else(|| "RGB buffer size overflow".to_string())?;
            if frame.bytes.len() < expected {
                return Err(format!(
                    "truncated RGB frame buffer: got {} bytes, expected {expected}",
                    frame.bytes.len()
                ));
            }
            (3, frame.bytes[..expected].to_vec())
        }
        other => {
            return Err(format!("unsupported samples_per_pixel={other} for /rendered"));
        }
    };

    let (out_w, out_h, final_pixels) = if let Some(vp) = opts.viewport {
        let (tw, th) = fit_viewport_dims(cols as u32, rows as u32, vp);
        let resized = bilinear_resize(&base_pixels, cols, rows, channels, tw as usize, th as usize);
        (tw, th, resized)
    } else {
        (cols as u32, rows as u32, base_pixels)
    };

    match format {
        RenderedImageFormat::Png => {
            let mut png_buf = Vec::new();
            let mut encoder = png::Encoder::new(&mut png_buf, out_w, out_h);
            encoder.set_color(if channels == 3 {
                png::ColorType::Rgb
            } else {
                png::ColorType::Grayscale
            });
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|e| format!("failed writing PNG header: {e}"))?;
            writer
                .write_image_data(&final_pixels)
                .map_err(|e| format!("failed writing PNG payload: {e}"))?;
            drop(writer);
            Ok(png_buf)
        }
        RenderedImageFormat::Jpeg => {
            let w_u16 = u16::try_from(out_w)
                .map_err(|_| format!("rendered width {out_w} exceeds JPEG u16 limit"))?;
            let h_u16 = u16::try_from(out_h)
                .map_err(|_| format!("rendered height {out_h} exceeds JPEG u16 limit"))?;
            let mut jpeg_buf = Vec::new();
            let encoder = JpegEncoder::new(&mut jpeg_buf, quality);
            let color = if channels == 3 {
                JpegColorType::Rgb
            } else {
                JpegColorType::Luma
            };
            encoder
                .encode(&final_pixels, w_u16, h_u16, color)
                .map_err(|e| format!("failed encoding JPEG payload: {e}"))?;
            Ok(jpeg_buf)
        }
    }
}
