use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use dicom_core::{DicomValue, Tag};
use dicom_dictionary_std::tags;
use dicom_object::{from_reader, DefaultDicomObject, InMemDicomObject};
use dicom_pixeldata::{PixelDecoder, PixelRepresentation};

use crate::deid::DeidConfig;
use crate::schema::{FrameRecord, SeriesRecord};
use crate::seg::{extract_seg_summary_text, unpack_bitpacked_frames, SEGMENTATION_STORAGE_UID};

pub const EXPLICIT_VR_LITTLE_ENDIAN_UID: &str = "1.2.840.10008.1.2.1";

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("failed to parse DICOM file: {0}")]
    Parse(#[from] dicom_object::ReadError),
    #[error("failed to decode DICOM pixel data: {0}")]
    PixelDecode(#[from] dicom_pixeldata::Error),
    #[error("failed to serialize DICOM JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to encode PNG thumbnail: {0}")]
    Png(#[from] png::EncodingError),
    #[error("invalid DICOM dataset: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct IngestedSeries {
    pub series: SeriesRecord,
    pub frames: Vec<FrameRecord>,
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
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

pub fn opt_f64_first(obj: &InMemDicomObject, tag: Tag) -> Option<f64> {
    let elem = obj.element_opt(tag).ok().flatten()?;
    if let Ok(vals) = elem.to_multi_float64() {
        if let Some(&first) = vals.first() {
            if first.is_finite() {
                return Some(first);
            }
        }
    }
    let s = elem.to_str().ok()?;
    let first = s.split('\\').next()?.trim();
    first.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn opt_f64_vec(obj: &InMemDicomObject, tag: Tag) -> Option<Vec<f64>> {
    let elem = obj.element_opt(tag).ok().flatten()?;
    if let Ok(vals) = elem.to_multi_float64() {
        if !vals.is_empty() {
            return Some(vals);
        }
    }
    let s = elem.to_str().ok()?;
    let parsed: Vec<f64> = s
        .split('\\')
        .filter_map(|p| p.trim().parse::<f64>().ok())
        .collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
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

pub fn extract_sr_recursive(obj: &InMemDicomObject, out: &mut Vec<String>) {
    let concept_name = obj
        .element_opt(tags::CONCEPT_NAME_CODE_SEQUENCE)
        .ok()
        .flatten()
        .and_then(|el| match el.value() {
            DicomValue::Sequence(seq) => seq.items().first(),
            _ => None,
        })
        .and_then(|item| opt_str(item, tags::CODE_MEANING));

    let text_val = opt_str(obj, tags::TEXT_VALUE);
    let code_val = obj
        .element_opt(tags::CONCEPT_CODE_SEQUENCE)
        .ok()
        .flatten()
        .and_then(|el| match el.value() {
            DicomValue::Sequence(seq) => seq.items().first(),
            _ => None,
        })
        .and_then(|item| opt_str(item, tags::CODE_MEANING));

    let measured_val = obj
        .element_opt(tags::MEASURED_VALUE_SEQUENCE)
        .ok()
        .flatten()
        .and_then(|el| match el.value() {
            DicomValue::Sequence(seq) => seq.items().first(),
            _ => None,
        })
        .and_then(|item| {
            let num = opt_str(item, tags::NUMERIC_VALUE)?;
            let unit = item
                .element_opt(tags::MEASUREMENT_UNITS_CODE_SEQUENCE)
                .ok()
                .flatten()
                .and_then(|u| match u.value() {
                    DicomValue::Sequence(seq) => seq.items().first(),
                    _ => None,
                })
                .and_then(|u_item| {
                    opt_str(u_item, tags::CODE_MEANING).or_else(|| opt_str(u_item, tags::CODE_VALUE))
                });
            Some(match unit {
                Some(u) => format!("{num} {u}"),
                None => num,
            })
        });

    let value_part = text_val.or(measured_val).or(code_val);
    match (concept_name, value_part) {
        (Some(name), Some(val)) => out.push(format!("{name}: {val}")),
        (None, Some(val)) => out.push(val),
        _ => {}
    }

    if let Ok(Some(seq_elem)) = obj.element_opt(tags::CONTENT_SEQUENCE) {
        if let DicomValue::Sequence(seq) = seq_elem.value() {
            for item in seq.items() {
                extract_sr_recursive(item, out);
            }
        }
    }
}

pub fn extract_sr_text(obj: &InMemDicomObject) -> String {
    let mut lines = Vec::new();
    extract_sr_recursive(obj, &mut lines);
    lines.join("\n")
}

pub fn slice_normal_projection(ipp: [f64; 3], iop: [f64; 6]) -> f64 {
    let rx = iop[0];
    let ry = iop[1];
    let rz = iop[2];
    let cx = iop[3];
    let cy = iop[4];
    let cz = iop[5];
    let nx = ry * cz - rz * cy;
    let ny = rz * cx - rx * cz;
    let nz = rx * cy - ry * cx;
    ipp[0] * nx + ipp[1] * ny + ipp[2] * nz
}

pub fn render_thumbnail_png(frame: &FrameRecord) -> Result<Option<Vec<u8>>, IngestError> {
    if frame.pixel_bytes.is_empty()
        || frame.rows <= 0
        || frame.columns <= 0
        || frame.samples_per_pixel <= 0
        || frame.bits_allocated <= 0
    {
        return Ok(None);
    }
    let rows = frame.rows as usize;
    let cols = frame.columns as usize;
    let spp = frame.samples_per_pixel as usize;
    let bytes_per_sample = ((frame.bits_allocated + 7) / 8) as usize;
    if !matches!(bytes_per_sample, 1 | 2 | 4) {
        return Ok(None);
    }
    let Some(expected) = rows
        .checked_mul(cols)
        .and_then(|v| v.checked_mul(spp))
        .and_then(|v| v.checked_mul(bytes_per_sample))
    else {
        return Ok(None);
    };
    if frame.pixel_bytes.len() < expected {
        return Ok(None);
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

    let thumb_w = 128usize;
    let thumb_h = 128usize;
    let mut gray = vec![0u8; thumb_w * thumb_h];

    let read_scalar = |idx: usize| -> f64 {
        let base = idx * spp * bytes_per_sample;
        let raw = match bytes_per_sample {
            4 => {
                let bytes = [
                    frame.pixel_bytes[base],
                    frame.pixel_bytes[base + 1],
                    frame.pixel_bytes[base + 2],
                    frame.pixel_bytes[base + 3],
                ];
                if frame.pixel_representation == 1 {
                    i32::from_le_bytes(bytes) as f64
                } else {
                    u32::from_le_bytes(bytes) as f64
                }
            }
            2 => {
                let bytes = [frame.pixel_bytes[base], frame.pixel_bytes[base + 1]];
                if frame.pixel_representation == 1 {
                    i16::from_le_bytes(bytes) as f64
                } else {
                    u16::from_le_bytes(bytes) as f64
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

    let (wc, ww) = if frame.window_center.is_finite()
        && frame.window_width.is_finite()
        && frame.window_width > 1.0
    {
        (frame.window_center, frame.window_width)
    } else {
        let mut min_v = f64::INFINITY;
        let mut max_v = f64::NEG_INFINITY;
        for y in (0..rows).step_by((rows / 32).max(1)) {
            for x in (0..cols).step_by((cols / 32).max(1)) {
                let v = read_scalar(y * cols + x);
                if v.is_finite() {
                    if v < min_v {
                        min_v = v;
                    }
                    if v > max_v {
                        max_v = v;
                    }
                }
            }
        }
        if min_v.is_finite()
            && max_v.is_finite()
            && max_v > min_v
            && (max_v - min_v).is_finite()
        {
            let span = max_v - min_v;
            let mid = min_v + span * 0.5;
            if mid.is_finite() {
                (mid, span.max(1.0))
            } else {
                (40.0, 400.0)
            }
        } else {
            (40.0, 400.0)
        }
    };

    let lower = wc - ww * 0.5;
    let inv_ww = 255.0 / ww.max(1.0);

    for ty in 0..thumb_h {
        let sy = (ty * rows) / thumb_h;
        for tx in 0..thumb_w {
            let sx = (tx * cols) / thumb_w;
            let val = read_scalar(sy * cols + sx);
            let norm = ((val - lower) * inv_ww).clamp(0.0, 255.0) as u8;
            gray[ty * thumb_w + tx] = norm;
        }
    }

    let mut png_buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, thumb_w as u32, thumb_h as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&gray)?;
    }
    Ok(Some(png_buf))
}

struct ParsedInstance {
    study_uid: String,
    series_uid: String,
    sop_uid: String,
    instance_number: i32,
    patient_id: String,
    patient_name: String,
    accession_number: String,
    study_date: String,
    study_description: String,
    modality: String,
    body_part_examined: String,
    series_number: i32,
    series_description: String,
    sr_text: String,
    frames: Vec<FrameRecord>,
}

#[derive(Debug, Clone, Default)]
struct FunctionalGroupAttrs {
    ipp: Option<[f64; 3]>,
    iop: Option<[f64; 6]>,
    pixel_spacing: Option<(f64, f64)>,
    slice_thickness: Option<f64>,
    window_center: Option<f64>,
    window_width: Option<f64>,
    rescale_slope: Option<f64>,
    rescale_intercept: Option<f64>,
}

fn seq_items(obj: &InMemDicomObject, tag: Tag) -> Option<&[InMemDicomObject]> {
    let elem = obj.element_opt(tag).ok().flatten()?;
    match elem.value() {
        DicomValue::Sequence(seq) => Some(seq.items()),
        _ => None,
    }
}

fn first_seq_item(obj: &InMemDicomObject, tag: Tag) -> Option<&InMemDicomObject> {
    seq_items(obj, tag)?.first()
}

fn extract_functional_group(fg: &InMemDicomObject) -> FunctionalGroupAttrs {
    let mut attrs = FunctionalGroupAttrs::default();

    if let Some(plane_pos) = first_seq_item(fg, tags::PLANE_POSITION_SEQUENCE) {
        if let Some(ipp) = opt_f64_vec(plane_pos, tags::IMAGE_POSITION_PATIENT) {
            if ipp.len() >= 3 && ipp[..3].iter().all(|v| v.is_finite()) {
                attrs.ipp = Some([ipp[0], ipp[1], ipp[2]]);
            }
        }
    }

    if let Some(plane_orient) = first_seq_item(fg, tags::PLANE_ORIENTATION_SEQUENCE) {
        if let Some(iop) = opt_f64_vec(plane_orient, tags::IMAGE_ORIENTATION_PATIENT) {
            if iop.len() >= 6 && iop[..6].iter().all(|v| v.is_finite()) {
                attrs.iop = Some([iop[0], iop[1], iop[2], iop[3], iop[4], iop[5]]);
            }
        }
    }

    if let Some(measures) = first_seq_item(fg, tags::PIXEL_MEASURES_SEQUENCE) {
        if let Some(ps) = opt_f64_vec(measures, tags::PIXEL_SPACING) {
            if !ps.is_empty() && ps[0].is_finite() {
                let col = ps.get(1).copied().filter(|v| v.is_finite()).unwrap_or(ps[0]);
                attrs.pixel_spacing = Some((ps[0], col));
            }
        }
        attrs.slice_thickness = opt_f64_first(measures, tags::SLICE_THICKNESS)
            .or_else(|| opt_f64_first(measures, tags::SPACING_BETWEEN_SLICES));
    }

    if let Some(voi) = first_seq_item(fg, tags::FRAME_VOILUT_SEQUENCE) {
        attrs.window_center = opt_f64_first(voi, tags::WINDOW_CENTER);
        attrs.window_width = opt_f64_first(voi, tags::WINDOW_WIDTH);
    }

    if let Some(xf) = first_seq_item(fg, tags::PIXEL_VALUE_TRANSFORMATION_SEQUENCE) {
        attrs.rescale_slope = opt_f64_first(xf, tags::RESCALE_SLOPE);
        attrs.rescale_intercept = opt_f64_first(xf, tags::RESCALE_INTERCEPT);
    }

    attrs
}

fn merge_functional_group(
    shared: &FunctionalGroupAttrs,
    per_frame: Option<FunctionalGroupAttrs>,
) -> FunctionalGroupAttrs {
    let pf = per_frame.unwrap_or_default();
    FunctionalGroupAttrs {
        ipp: pf.ipp.or(shared.ipp),
        iop: pf.iop.or(shared.iop),
        pixel_spacing: pf.pixel_spacing.or(shared.pixel_spacing),
        slice_thickness: pf.slice_thickness.or(shared.slice_thickness),
        window_center: pf.window_center.or(shared.window_center),
        window_width: pf.window_width.or(shared.window_width),
        rescale_slope: pf.rescale_slope.or(shared.rescale_slope),
        rescale_intercept: pf.rescale_intercept.or(shared.rescale_intercept),
    }
}

fn build_header_json(
    file_obj: &DefaultDicomObject,
    stored_ts: &str,
    photometric: Option<&str>,
    normalized_8bit: bool,
) -> Result<String, IngestError> {
    let mut cloned: InMemDicomObject = (**file_obj).clone();
    cloned.remove_element(tags::PIXEL_DATA);
    let mut json_val = dicom_json::to_value(&cloned)?;
    if let Some(map) = json_val.as_object_mut() {
        map.insert(
            "00020010".to_string(),
            serde_json::json!({
                "vr": "UI",
                "Value": [stored_ts],
            }),
        );
        if let Some(photo) = photometric {
            map.insert(
                "00280004".to_string(),
                serde_json::json!({
                    "vr": "CS",
                    "Value": [photo],
                }),
            );
        }
        if normalized_8bit {
            map.insert(
                "00280100".to_string(),
                serde_json::json!({ "vr": "US", "Value": [8] }),
            );
            map.insert(
                "00280101".to_string(),
                serde_json::json!({ "vr": "US", "Value": [8] }),
            );
            map.insert(
                "00280102".to_string(),
                serde_json::json!({ "vr": "US", "Value": [7] }),
            );
            map.insert(
                "00280103".to_string(),
                serde_json::json!({ "vr": "US", "Value": [0] }),
            );
        }
    }
    Ok(serde_json::to_string(&json_val)?)
}

struct DecodedFrames {
    rows: i32,
    columns: i32,
    samples_per_pixel: i32,
    bits_allocated: i32,
    bits_stored: i32,
    high_bit: i32,
    pixel_representation: i32,
    photometric_str: String,
    normalized_8bit: bool,
    frame_buffers: Vec<Vec<u8>>,
}

fn decode_instance_frames(
    file_obj: &DefaultDicomObject,
    is_seg: bool,
) -> Result<DecodedFrames, IngestError> {
    let ds = &**file_obj;
    let raw_bits_allocated = opt_i32(ds, tags::BITS_ALLOCATED).unwrap_or(16);

    if raw_bits_allocated == 1 {
        let rows = opt_i32(ds, tags::ROWS)
            .filter(|&r| r > 0)
            .ok_or_else(|| IngestError::Invalid("missing/invalid Rows (0028,0010)".to_string()))?;
        let columns = opt_i32(ds, tags::COLUMNS)
            .filter(|&c| c > 0)
            .ok_or_else(|| {
                IngestError::Invalid("missing/invalid Columns (0028,0011)".to_string())
            })?;
        let samples_per_pixel = opt_i32(ds, tags::SAMPLES_PER_PIXEL).unwrap_or(1).max(1);
        let n_frames = opt_i32(ds, tags::NUMBER_OF_FRAMES).unwrap_or(1).max(1) as usize;
        let photometric_str = opt_str(ds, tags::PHOTOMETRIC_INTERPRETATION)
            .unwrap_or_else(|| "MONOCHROME2".to_string());

        let px_elem = ds
            .element_opt(tags::PIXEL_DATA)
            .ok()
            .flatten()
            .ok_or_else(|| IngestError::Invalid("missing PixelData (7FE0,0010)".to_string()))?;

        let frame_buffers = match px_elem.value() {
            DicomValue::Primitive(p) => {
                unpack_bitpacked_frames(&p.to_bytes(), rows as usize, columns as usize, n_frames)?
            }
            DicomValue::PixelSequence(_) => {
                let decoded = file_obj.decode_pixel_data()?;
                let raw_data = decoded.data();
                let frame_pixels = (rows as usize)
                    .checked_mul(columns as usize)
                    .ok_or_else(|| IngestError::Invalid("SEG dimension overflow".to_string()))?;
                let unpacked_len = frame_pixels
                    .checked_mul(n_frames)
                    .ok_or_else(|| IngestError::Invalid("SEG total byte overflow".to_string()))?;
                if raw_data.len() == unpacked_len {
                    raw_data
                        .chunks_exact(frame_pixels)
                        .map(|c| c.to_vec())
                        .collect()
                } else {
                    unpack_bitpacked_frames(raw_data, rows as usize, columns as usize, n_frames)?
                }
            }
            DicomValue::Sequence(_) => {
                return Err(IngestError::Invalid(
                    "invalid sequence VR for PixelData (7FE0,0010)".to_string(),
                ));
            }
        };

        return Ok(DecodedFrames {
            rows,
            columns,
            samples_per_pixel,
            bits_allocated: 8,
            bits_stored: 8,
            high_bit: 7,
            pixel_representation: 0,
            photometric_str,
            normalized_8bit: true,
            frame_buffers,
        });
    }

    let decoded = file_obj.decode_pixel_data()?;
    let rows = decoded.rows() as i32;
    let columns = decoded.columns() as i32;
    let samples_per_pixel = decoded.samples_per_pixel() as i32;
    let bits_allocated = decoded.bits_allocated() as i32;
    let bits_stored = decoded.bits_stored() as i32;
    let high_bit = decoded.high_bit() as i32;
    let pixel_representation = match decoded.pixel_representation() {
        PixelRepresentation::Signed => 1,
        PixelRepresentation::Unsigned => 0,
    };
    let photometric_str = if is_seg {
        opt_str(ds, tags::PHOTOMETRIC_INTERPRETATION)
            .unwrap_or_else(|| decoded.photometric_interpretation().as_str().to_string())
    } else {
        decoded.photometric_interpretation().as_str().to_string()
    };

    let n_frames = decoded.number_of_frames().max(1);
    let mut frame_buffers = Vec::with_capacity(n_frames as usize);
    for f_idx in 0..n_frames {
        frame_buffers.push(decoded.frame_data(f_idx)?.to_vec());
    }

    Ok(DecodedFrames {
        rows,
        columns,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        high_bit,
        pixel_representation,
        photometric_str,
        normalized_8bit: false,
        frame_buffers,
    })
}

fn parse_single_dicom(bytes: &[u8], deid: Option<&DeidConfig>) -> Result<ParsedInstance, IngestError> {
    let mut file_obj = from_reader(Cursor::new(bytes))?;
    if let Some(cfg) = deid.filter(|c| c.enabled) {
        cfg.deidentify_file_object(&mut file_obj);
    }
    let ds = &*file_obj;

    let study_uid = opt_str(ds, tags::STUDY_INSTANCE_UID)
        .ok_or_else(|| IngestError::Invalid("missing StudyInstanceUID (0020,000D)".to_string()))?;
    let series_uid = opt_str(ds, tags::SERIES_INSTANCE_UID)
        .ok_or_else(|| IngestError::Invalid("missing SeriesInstanceUID (0020,000E)".to_string()))?;
    let sop_uid = opt_str(ds, tags::SOP_INSTANCE_UID)
        .ok_or_else(|| IngestError::Invalid("missing SOPInstanceUID (0008,0018)".to_string()))?;
    let sop_class_uid = opt_str(ds, tags::SOP_CLASS_UID).unwrap_or_default();

    let patient_id = opt_str(ds, tags::PATIENT_ID).unwrap_or_else(|| "UNKNOWN".to_string());
    let patient_name = opt_str(ds, tags::PATIENT_NAME).unwrap_or_else(|| "Anonymous".to_string());
    let accession_number = opt_str(ds, tags::ACCESSION_NUMBER).unwrap_or_default();
    let study_date = opt_str(ds, tags::STUDY_DATE).unwrap_or_default();
    let study_description =
        opt_str(ds, tags::STUDY_DESCRIPTION).unwrap_or_else(|| "DICOM Study".to_string());
    let modality = opt_str(ds, tags::MODALITY).unwrap_or_else(|| "OT".to_string());
    let body_part_examined = opt_str(ds, tags::BODY_PART_EXAMINED).unwrap_or_default();
    let series_number = opt_i32(ds, tags::SERIES_NUMBER).unwrap_or(1);
    let series_description =
        opt_str(ds, tags::SERIES_DESCRIPTION).unwrap_or_else(|| format!("{modality} Series"));
    let instance_number = opt_i32(ds, tags::INSTANCE_NUMBER).unwrap_or(1);

    let is_seg = modality == "SEG" || sop_class_uid == SEGMENTATION_STORAGE_UID;

    let sr_text = if modality == "SR" || ds.element_opt(tags::CONTENT_SEQUENCE).ok().flatten().is_some()
    {
        extract_sr_text(ds)
    } else if is_seg {
        extract_seg_summary_text(ds)
    } else {
        String::new()
    };

    let has_pixels = ds.element_opt(tags::PIXEL_DATA).ok().flatten().is_some();
    let mut frames = Vec::new();

    if has_pixels {
        let decoded = decode_instance_frames(&file_obj, is_seg)?;
        let stored_ts = EXPLICIT_VR_LITTLE_ENDIAN_UID.to_string();
        let dicom_json_header = build_header_json(
            &file_obj,
            &stored_ts,
            Some(&decoded.photometric_str),
            decoded.normalized_8bit,
        )?;

        let top_rescale_slope = opt_f64_first(ds, tags::RESCALE_SLOPE);
        let top_rescale_intercept = opt_f64_first(ds, tags::RESCALE_INTERCEPT);
        let top_window_center = opt_f64_first(ds, tags::WINDOW_CENTER);
        let top_window_width = opt_f64_first(ds, tags::WINDOW_WIDTH);
        let top_slice_thickness = opt_f64_first(ds, tags::SLICE_THICKNESS)
            .or_else(|| opt_f64_first(ds, tags::SPACING_BETWEEN_SLICES));
        let top_spacing_vec = opt_f64_vec(ds, tags::PIXEL_SPACING);
        let top_iop_vec = opt_f64_vec(ds, tags::IMAGE_ORIENTATION_PATIENT);
        let top_slice_location = opt_f64_first(ds, tags::SLICE_LOCATION);
        let top_ipp_vec = opt_f64_vec(ds, tags::IMAGE_POSITION_PATIENT);

        let shared_fg = first_seq_item(ds, tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE)
            .map(extract_functional_group)
            .unwrap_or_default();
        let per_frame_fgs = seq_items(ds, tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE);

        let default_wc = if is_seg { 128.0 } else { 40.0 };
        let default_ww = if is_seg { 255.0 } else { 400.0 };

        for (f_idx, pixel_bytes) in decoded.frame_buffers.into_iter().enumerate() {
            let fg = merge_functional_group(
                &shared_fg,
                per_frame_fgs
                    .and_then(|items| items.get(f_idx))
                    .map(extract_functional_group),
            );

            let rescale_slope = fg.rescale_slope.or(top_rescale_slope).unwrap_or(1.0);
            let rescale_intercept = fg.rescale_intercept.or(top_rescale_intercept).unwrap_or(0.0);
            let window_center = fg.window_center.or(top_window_center).unwrap_or(default_wc);
            let window_width = fg.window_width.or(top_window_width).unwrap_or(default_ww);
            let slice_thickness = fg.slice_thickness.or(top_slice_thickness).unwrap_or(1.0);

            let (pixel_spacing_row_mm, pixel_spacing_col_mm) = match fg.pixel_spacing {
                Some((r, c)) => (r, c),
                None => match top_spacing_vec.as_deref() {
                    Some([r, c, ..]) => (*r, *c),
                    Some([r]) => (*r, *r),
                    _ => (1.0, 1.0),
                },
            };

            let mut iop = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
            if let Some(fg_iop) = fg.iop {
                iop = fg_iop;
            } else if let Some(vec) = top_iop_vec.as_deref() {
                if vec.len() >= 6 {
                    iop.copy_from_slice(&vec[..6]);
                }
            }

            let fg_ipp_present = fg.ipp.is_some();
            let (ipp_x, ipp_y, ipp_z) = if let Some(fg_ipp) = fg.ipp {
                (fg_ipp[0], fg_ipp[1], fg_ipp[2])
            } else if let Some(vec) = top_ipp_vec.as_deref().filter(|v| v.len() >= 3) {
                (vec[0], vec[1], vec[2])
            } else {
                (
                    0.0,
                    0.0,
                    top_slice_location
                        .unwrap_or((instance_number as f64 + f_idx as f64) * slice_thickness),
                )
            };

            let slice_location = if fg_ipp_present {
                slice_normal_projection([ipp_x, ipp_y, ipp_z], iop)
            } else {
                top_slice_location.unwrap_or(ipp_z)
            };

            frames.push(FrameRecord {
                study_instance_uid: study_uid.clone(),
                series_instance_uid: series_uid.clone(),
                sop_instance_uid: sop_uid.clone(),
                sop_class_uid: sop_class_uid.clone(),
                instance_number,
                frame_number: (f_idx + 1) as i32,
                slice_sort_index: f_idx as i32,
                rows: decoded.rows,
                columns: decoded.columns,
                samples_per_pixel: decoded.samples_per_pixel,
                bits_allocated: decoded.bits_allocated,
                bits_stored: decoded.bits_stored,
                high_bit: decoded.high_bit,
                pixel_representation: decoded.pixel_representation,
                photometric_interpretation: decoded.photometric_str.clone(),
                transfer_syntax_uid: stored_ts.clone(),
                rescale_slope,
                rescale_intercept,
                window_center,
                window_width,
                slice_location,
                slice_thickness,
                pixel_spacing_row_mm,
                pixel_spacing_col_mm,
                ipp_x,
                ipp_y,
                ipp_z,
                iop,
                dicom_json_header: dicom_json_header.clone(),
                frame_embedding: None,
                pixel_bytes,
            });
        }
    }

    Ok(ParsedInstance {
        study_uid,
        series_uid,
        sop_uid,
        instance_number,
        patient_id,
        patient_name,
        accession_number,
        study_date,
        study_description,
        modality,
        body_part_examined,
        series_number,
        series_description,
        sr_text,
        frames,
    })
}

pub fn parse_dicom_instances(raw_files: &[Bytes]) -> Result<Vec<IngestedSeries>, IngestError> {
    parse_dicom_instances_with_deid(raw_files, None)
}

pub fn parse_dicom_instances_with_deid(
    raw_files: &[Bytes],
    deid: Option<&DeidConfig>,
) -> Result<Vec<IngestedSeries>, IngestError> {
    if raw_files.is_empty() {
        return Err(IngestError::Invalid("no DICOM files provided".to_string()));
    }

    let mut grouped: BTreeMap<(String, String), Vec<ParsedInstance>> = BTreeMap::new();
    for bytes in raw_files {
        let parsed = parse_single_dicom(bytes, deid)?;
        grouped
            .entry((parsed.study_uid.clone(), parsed.series_uid.clone()))
            .or_default()
            .push(parsed);
    }

    let updated_at = now_micros();
    let mut output = Vec::with_capacity(grouped.len());

    for ((study_uid, series_uid), mut instances) in grouped {
        instances.sort_by(|a, b| {
            a.instance_number
                .cmp(&b.instance_number)
                .then_with(|| a.sop_uid.cmp(&b.sop_uid))
        });
        let instance_count = instances.len() as i32;
        let first = &instances[0];
        let patient_id = first.patient_id.clone();
        let patient_name = first.patient_name.clone();
        let accession_number = first.accession_number.clone();
        let study_date = first.study_date.clone();
        let study_description = first.study_description.clone();
        let modality = first.modality.clone();
        let body_part_examined = first.body_part_examined.clone();
        let series_number = first.series_number;
        let series_description = first.series_description.clone();

        let sr_parts: Vec<String> = instances
            .iter()
            .filter(|i| !i.sr_text.is_empty())
            .map(|i| i.sr_text.clone())
            .collect();
        let sr_text = sr_parts.join("\n---\n");

        let mut frames: Vec<FrameRecord> = instances
            .into_iter()
            .flat_map(|i| i.frames.into_iter())
            .collect();

        frames.sort_by(|a, b| {
            let za = slice_normal_projection([a.ipp_x, a.ipp_y, a.ipp_z], a.iop);
            let zb = slice_normal_projection([b.ipp_x, b.ipp_y, b.ipp_z], b.iop);
            za.total_cmp(&zb)
                .then_with(|| a.instance_number.cmp(&b.instance_number))
                .then_with(|| a.frame_number.cmp(&b.frame_number))
                .then_with(|| a.sop_instance_uid.cmp(&b.sop_instance_uid))
        });

        for (idx, frame) in frames.iter_mut().enumerate() {
            frame.slice_sort_index = idx as i32;
        }

        let frame_count = frames.len() as i32;
        let (rows, columns, spacing_x_mm, spacing_y_mm, spacing_z_mm, wc, ww, is_volumetric, thumb) =
            if let Some(first_f) = frames.first() {
                let mut diffs = Vec::new();
                for pair in frames.windows(2) {
                    let z0 = slice_normal_projection(
                        [pair[0].ipp_x, pair[0].ipp_y, pair[0].ipp_z],
                        pair[0].iop,
                    );
                    let z1 = slice_normal_projection(
                        [pair[1].ipp_x, pair[1].ipp_y, pair[1].ipp_z],
                        pair[1].iop,
                    );
                    let d = (z1 - z0).abs();
                    if d > 1e-4 {
                        diffs.push(d);
                    }
                }
                let spacing_z = if !diffs.is_empty() {
                    diffs.sort_by(|a, b| a.total_cmp(b));
                    diffs[diffs.len() / 2]
                } else {
                    first_f.slice_thickness.max(1.0)
                };

                let uniform_dims = frames.iter().all(|f| {
                    f.rows == first_f.rows
                        && f.columns == first_f.columns
                        && f.samples_per_pixel == 1
                });
                let volumetric = modality != "SEG" && frames.len() >= 3 && uniform_dims;
                let mid = &frames[frames.len() / 2];
                let thumbnail = render_thumbnail_png(mid)?;

                (
                    first_f.rows,
                    first_f.columns,
                    first_f.pixel_spacing_col_mm,
                    first_f.pixel_spacing_row_mm,
                    spacing_z,
                    first_f.window_center,
                    first_f.window_width,
                    volumetric,
                    thumbnail,
                )
            } else {
                (0, 0, 1.0, 1.0, 1.0, 40.0, 400.0, false, None)
            };

        let searchable_text = [
            patient_id.as_str(),
            patient_name.as_str(),
            accession_number.as_str(),
            study_date.as_str(),
            study_description.as_str(),
            modality.as_str(),
            body_part_examined.as_str(),
            series_description.as_str(),
            sr_text.as_str(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");

        output.push(IngestedSeries {
            series: SeriesRecord {
                study_instance_uid: study_uid,
                series_instance_uid: series_uid,
                patient_id,
                patient_name,
                accession_number,
                study_date,
                study_description,
                modality,
                body_part_examined,
                series_number,
                series_description,
                instance_count,
                frame_count,
                rows,
                columns,
                spacing_x_mm,
                spacing_y_mm,
                spacing_z_mm,
                window_center_default: wc,
                window_width_default: ww,
                is_volumetric,
                sr_text,
                searchable_text,
                series_embedding: None,
                thumbnail_png: thumb,
                updated_at_micros: updated_at,
            },
            frames,
        });
    }

    Ok(output)
}
