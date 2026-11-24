use dicom_core::{DicomValue, Tag};
use dicom_dictionary_std::tags;
use dicom_object::InMemDicomObject;
use serde::{Deserialize, Serialize};

use crate::dicom_ingest::IngestError;

pub const SEGMENTATION_STORAGE_UID: &str = "1.2.840.10008.5.1.4.1.1.66.4";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    pub segment_number: u16,
    pub segment_label: String,
    pub segment_description: String,
    pub color_rgba: [u8; 4],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegFrameMapping {
    pub seg_frame_number: i32,
    pub segment_number: u16,
    pub referenced_sop_instance_uid: Option<String>,
    pub ipp: [f64; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentationMetadata {
    pub seg_series_instance_uid: String,
    pub seg_sop_instance_uid: String,
    pub series_description: String,
    pub segmentation_type: String,
    pub referenced_series_uid: Option<String>,
    pub rows: i32,
    pub columns: i32,
    pub segments: Vec<SegmentDescriptor>,
    pub frames: Vec<SegFrameMapping>,
}

/// Converts a DICOM PS3.3 C.10.7.1.1 16-bit encoded CIELab triple (`L*`, `a*`, `b*`) to sRGB.
pub fn dicom_cielab_to_rgb(l_u16: u16, a_u16: u16, b_u16: u16) -> [u8; 3] {
    let l_star = f64::from(l_u16) * 100.0 / 65535.0;
    let a_star = f64::from(a_u16) * 255.0 / 65535.0 - 128.0;
    let b_star = f64::from(b_u16) * 255.0 / 65535.0 - 128.0;

    let fy = (l_star + 16.0) / 116.0;
    let fx = a_star / 500.0 + fy;
    let fz = fy - b_star / 200.0;

    let delta = 6.0 / 29.0;
    let finv = |t: f64| -> f64 {
        if t > delta {
            t.powi(3)
        } else {
            3.0 * delta * delta * (t - 4.0 / 29.0)
        }
    };

    // D65 standard illuminant
    let x = 0.95047 * finv(fx);
    let y = 1.00000 * finv(fy);
    let z = 1.08883 * finv(fz);

    let r_lin = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z;
    let g_lin = -0.9692660 * x + 1.8760108 * y + 0.0415560 * z;
    let b_lin = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z;

    let to_srgb = |u: f64| -> u8 {
        let clamped = u.clamp(0.0, 1.0);
        let gamma = if clamped <= 0.0031308 {
            12.92 * clamped
        } else {
            1.055 * clamped.powf(1.0 / 2.4) - 0.055
        };
        (gamma * 255.0).round().clamp(0.0, 255.0) as u8
    };

    [to_srgb(r_lin), to_srgb(g_lin), to_srgb(b_lin)]
}

/// Converts an sRGB triple to a DICOM PS3.3 C.10.7.1.1 16-bit encoded CIELab triple.
pub fn rgb_to_dicom_cielab(r: u8, g: u8, b: u8) -> [u16; 3] {
    let to_lin = |c: u8| -> f64 {
        let s = f64::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };

    let r_lin = to_lin(r);
    let g_lin = to_lin(g);
    let b_lin = to_lin(b);

    let x = (0.4124564 * r_lin + 0.3575761 * g_lin + 0.1804375 * b_lin) / 0.95047;
    let y = 0.2126729 * r_lin + 0.7151522 * g_lin + 0.0721750 * b_lin;
    let z = (0.0193339 * r_lin + 0.1191920 * g_lin + 0.9503041 * b_lin) / 1.08883;

    let delta3 = (6.0_f64 / 29.0).powi(3);
    let delta2 = (6.0_f64 / 29.0).powi(2);
    let f = |t: f64| -> f64 {
        if t > delta3 {
            t.cbrt()
        } else {
            t / (3.0 * delta2) + 4.0 / 29.0
        }
    };

    let fx = f(x);
    let fy = f(y);
    let fz = f(z);

    let l_star = (116.0 * fy - 16.0).clamp(0.0, 100.0);
    let a_star = (500.0 * (fx - fy)).clamp(-128.0, 127.0);
    let b_star = (200.0 * (fy - fz)).clamp(-128.0, 127.0);

    let l_u16 = (l_star * 65535.0 / 100.0).round().clamp(0.0, 65535.0) as u16;
    let a_u16 = ((a_star + 128.0) * 65535.0 / 255.0)
        .round()
        .clamp(0.0, 65535.0) as u16;
    let b_u16 = ((b_star + 128.0) * 65535.0 / 255.0)
        .round()
        .clamp(0.0, 65535.0) as u16;

    [l_u16, a_u16, b_u16]
}

pub fn fallback_segment_rgba(segment_number: u16) -> [u8; 4] {
    const PALETTE: [[u8; 3]; 6] = [
        [239, 68, 68],  // Red
        [245, 158, 11], // Amber
        [34, 197, 94],  // Emerald
        [56, 189, 248], // Sky
        [168, 85, 247], // Purple
        [236, 72, 153], // Pink
    ];
    let idx = usize::from(segment_number.saturating_sub(1)) % PALETTE.len();
    let [r, g, b] = PALETTE[idx];
    [r, g, b, 140]
}

/// Unpacks 1-bit packed DICOM binary pixel data (LSB-first per DICOM PS3.5 Section 8.1.1)
/// into `n_frames` 8-bit grayscale masks (`0` or `255`).
pub fn unpack_bitpacked_frames(
    raw: &[u8],
    rows: usize,
    cols: usize,
    n_frames: usize,
) -> Result<Vec<Vec<u8>>, IngestError> {
    if rows == 0 || cols == 0 || n_frames == 0 {
        return Err(IngestError::Invalid(
            "SEG bit-packed frames require non-zero dimensions and frame count".to_string(),
        ));
    }
    let frame_pixels = rows
        .checked_mul(cols)
        .ok_or_else(|| IngestError::Invalid("SEG frame dimension overflow".to_string()))?;
    let total_bits = frame_pixels
        .checked_mul(n_frames)
        .ok_or_else(|| IngestError::Invalid("SEG total bit count overflow".to_string()))?;
    let required_bytes = total_bits.div_ceil(8);
    if raw.len() < required_bytes {
        return Err(IngestError::Invalid(format!(
            "truncated 1-bit SEG pixel data: got {} bytes, need {required_bytes}",
            raw.len()
        )));
    }

    let mut frames = Vec::with_capacity(n_frames);
    let mut global_bit = 0usize;
    for _ in 0..n_frames {
        let mut out = Vec::with_capacity(frame_pixels);
        for _ in 0..frame_pixels {
            let byte = raw[global_bit / 8];
            let bit = (byte >> (global_bit % 8)) & 1;
            out.push(if bit != 0 { 255 } else { 0 });
            global_bit += 1;
        }
        frames.push(out);
    }
    Ok(frames)
}

fn opt_str(obj: &InMemDicomObject, tag: Tag) -> Option<String> {
    let elem = obj.element_opt(tag).ok().flatten()?;
    let s = elem.to_str().ok()?;
    let trimmed = s.trim().trim_end_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn opt_i32(obj: &InMemDicomObject, tag: Tag) -> Option<i32> {
    let elem = obj.element_opt(tag).ok().flatten()?;
    if let Ok(v) = elem.to_int::<i32>() {
        return Some(v);
    }
    let s = elem.to_str().ok()?;
    s.split('\\').next()?.trim().parse::<i32>().ok()
}

pub fn extract_seg_summary_text(ds: &InMemDicomObject) -> String {
    let mut lines = Vec::new();
    if let Some(seg_type) = opt_str(ds, tags::SEGMENTATION_TYPE) {
        lines.push(format!("SegmentationType: {seg_type}"));
    }
    if let Ok(Some(ref_seq_el)) = ds.element_opt(tags::REFERENCED_SERIES_SEQUENCE) {
        if let DicomValue::Sequence(seq) = ref_seq_el.value() {
            if let Some(item) = seq.items().first() {
                if let Some(ref_uid) = opt_str(item, tags::SERIES_INSTANCE_UID) {
                    lines.push(format!("ReferencedSeriesUID: {ref_uid}"));
                }
            }
        }
    }
    if let Ok(Some(seg_seq_el)) = ds.element_opt(tags::SEGMENT_SEQUENCE) {
        if let DicomValue::Sequence(seq) = seg_seq_el.value() {
            for item in seq.items() {
                let num = opt_i32(item, tags::SEGMENT_NUMBER).unwrap_or(1);
                let label = opt_str(item, tags::SEGMENT_LABEL).unwrap_or_default();
                let desc = opt_str(item, tags::SEGMENT_DESCRIPTION).unwrap_or_default();
                match (label.is_empty(), desc.is_empty()) {
                    (false, false) => lines.push(format!("Segment {num}: {label} ({desc})")),
                    (false, true) => lines.push(format!("Segment {num}: {label}")),
                    (true, false) => lines.push(format!("Segment {num}: {desc}")),
                    (true, true) => lines.push(format!("Segment {num}")),
                }
            }
        }
    }
    lines.join("\n")
}

fn json_tag_values<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> Option<&'a [serde_json::Value]> {
    obj.get(tag)?.get("Value")?.as_array().map(Vec::as_slice)
}

fn json_first_str(obj: &serde_json::Map<String, serde_json::Value>, tag: &str) -> Option<String> {
    let first = json_tag_values(obj, tag)?.first()?;
    if let Some(s) = first.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else if let Some(n) = first.as_i64() {
        Some(n.to_string())
    } else {
        first.as_f64().map(|f| f.to_string())
    }
}

fn json_f64_vec(obj: &serde_json::Map<String, serde_json::Value>, tag: &str) -> Option<Vec<f64>> {
    let vals = json_tag_values(obj, tag)?;
    let parsed: Vec<f64> = vals
        .iter()
        .filter_map(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
        .collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

fn json_u16_vec(obj: &serde_json::Map<String, serde_json::Value>, tag: &str) -> Option<Vec<u16>> {
    let vals = json_tag_values(obj, tag)?;
    let parsed: Vec<u16> = vals
        .iter()
        .filter_map(|v| {
            v.as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u16>().ok()))
        })
        .collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

fn json_first_seq_item<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    json_tag_values(obj, tag)?.first()?.as_object()
}

/// Parses structured DICOM SEG metadata (`SegmentSequence`, `ReferencedSeriesSequence`,
/// and per-frame segment/reference mappings) from a stored DICOM JSON header.
pub fn parse_segmentation_from_dicom_json(
    seg_series_uid: &str,
    seg_sop_uid: &str,
    series_description: &str,
    rows: i32,
    columns: i32,
    dicom_json: &serde_json::Value,
) -> Option<SegmentationMetadata> {
    let root = dicom_json.as_object()?;
    let segmentation_type =
        json_first_str(root, "00620001").unwrap_or_else(|| "BINARY".to_string());
    let referenced_series_uid =
        json_first_seq_item(root, "00081115").and_then(|item| json_first_str(item, "0020000E"));

    let mut segments = Vec::new();
    if let Some(seg_items) = json_tag_values(root, "00620002") {
        for (idx, val) in seg_items.iter().enumerate() {
            let Some(item) = val.as_object() else {
                continue;
            };
            let segment_number = json_u16_vec(item, "00620004")
                .and_then(|v| v.first().copied())
                .unwrap_or((idx + 1) as u16);
            let segment_label = json_first_str(item, "00620005")
                .unwrap_or_else(|| format!("Segment {segment_number}"));
            let segment_description = json_first_str(item, "00620006").unwrap_or_default();
            let color_rgba = match json_u16_vec(item, "0062000D") {
                Some(lab) if lab.len() >= 3 => {
                    let [r, g, b] = dicom_cielab_to_rgb(lab[0], lab[1], lab[2]);
                    let alpha = if segment_number == 1 { 145 } else { 125 };
                    [r, g, b, alpha]
                }
                _ => fallback_segment_rgba(segment_number),
            };
            segments.push(SegmentDescriptor {
                segment_number,
                segment_label,
                segment_description,
                color_rgba,
            });
        }
    }

    let shared_item = json_first_seq_item(root, "52009229");
    let shared_seg_num = shared_item
        .and_then(|s| json_first_seq_item(s, "0062000A"))
        .and_then(|si| json_u16_vec(si, "0062000B"))
        .and_then(|v| v.first().copied());
    let shared_ipp = shared_item
        .and_then(|s| json_first_seq_item(s, "00209113"))
        .and_then(|pp| json_f64_vec(pp, "00200032"))
        .and_then(|v| (v.len() >= 3).then(|| [v[0], v[1], v[2]]));

    let mut frames = Vec::new();
    if let Some(fg_items) = json_tag_values(root, "52009230") {
        for (idx, val) in fg_items.iter().enumerate() {
            let Some(fg) = val.as_object() else {
                continue;
            };
            let segment_number = json_first_seq_item(fg, "0062000A")
                .and_then(|si| json_u16_vec(si, "0062000B"))
                .and_then(|v| v.first().copied())
                .or(shared_seg_num)
                .unwrap_or(1);

            let referenced_sop_instance_uid = json_first_seq_item(fg, "00089124")
                .and_then(|deriv| json_first_seq_item(deriv, "00082112"))
                .and_then(|src| json_first_str(src, "00081155"))
                .or_else(|| {
                    json_first_seq_item(fg, "00082112").and_then(|src| json_first_str(src, "00081155"))
                });

            let ipp = json_first_seq_item(fg, "00209113")
                .and_then(|pp| json_f64_vec(pp, "00200032"))
                .and_then(|v| (v.len() >= 3).then(|| [v[0], v[1], v[2]]))
                .or(shared_ipp)
                .unwrap_or([0.0, 0.0, 0.0]);

            frames.push(SegFrameMapping {
                seg_frame_number: (idx + 1) as i32,
                segment_number,
                referenced_sop_instance_uid,
                ipp,
            });
        }
    }

    Some(SegmentationMetadata {
        seg_series_instance_uid: seg_series_uid.to_string(),
        seg_sop_instance_uid: seg_sop_uid.to_string(),
        series_description: series_description.to_string(),
        segmentation_type,
        referenced_series_uid,
        rows,
        columns,
        segments,
        frames,
    })
}
