use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::Write;

use flate2::write::DeflateEncoder;
use flate2::Compression;

pub const TS_IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
pub const TS_EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
pub const TS_DEFLATED_EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1.99";
pub const TS_RLE_LOSSLESS: &str = "1.2.840.10008.1.2.5";
pub const TS_DEFLATED_IMAGE_FRAME: &str = "1.2.840.10008.1.2.8.1";

pub const SUPPORTED_WADO_TRANSFER_SYNTAXES: &[&str] = &[
    TS_EXPLICIT_VR_LE,
    TS_IMPLICIT_VR_LE,
    TS_RLE_LOSSLESS,
    TS_DEFLATED_IMAGE_FRAME,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedTransferSyntax {
    ExplicitVrLittleEndian,
    ImplicitVrLittleEndian,
    RleLossless,
    DeflatedImageFrame,
}

impl NegotiatedTransferSyntax {
    pub fn uid(self) -> &'static str {
        match self {
            Self::ExplicitVrLittleEndian => TS_EXPLICIT_VR_LE,
            Self::ImplicitVrLittleEndian => TS_IMPLICIT_VR_LE,
            Self::RleLossless => TS_RLE_LOSSLESS,
            Self::DeflatedImageFrame => TS_DEFLATED_IMAGE_FRAME,
        }
    }

    pub fn is_compressed(self) -> bool {
        matches!(self, Self::RleLossless | Self::DeflatedImageFrame)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedFrameFormat {
    pub syntax: NegotiatedTransferSyntax,
    pub multipart_type: String,
    pub part_content_type: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    q: f32,
    specificity: u8,
    order: usize,
    format: NegotiatedFrameFormat,
}

pub(crate) fn split_quoted_aware(input: &str, delim: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if in_quotes && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == delim && !in_quotes {
            parts.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    let tail = current.trim();
    if !tail.is_empty() || !parts.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

pub(crate) fn unquote(val: &str) -> String {
    let trimmed = val.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn default_explicit_format() -> NegotiatedFrameFormat {
    NegotiatedFrameFormat {
        syntax: NegotiatedTransferSyntax::ExplicitVrLittleEndian,
        multipart_type: "application/octet-stream".to_string(),
        part_content_type: format!("application/octet-stream; transfer-syntax={TS_EXPLICIT_VR_LE}"),
    }
}

fn match_media_range(
    base_type: &str,
    params: &BTreeMap<String, String>,
) -> Option<(u8, NegotiatedFrameFormat)> {
    let (slash_l, slash_r) = base_type.split_once('/')?;
    if slash_l.is_empty() || slash_r.is_empty() || slash_r.contains('/') {
        return None;
    }

    let is_multipart = base_type == "multipart/related";
    let eff_type = if is_multipart {
        params
            .get("type")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "*/*".to_string())
    } else {
        base_type.to_string()
    };

    let ts_param = params.get("transfer-syntax").map(String::as_str);

    let allows_raw_octet = matches!(
        eff_type.as_str(),
        "application/octet-stream" | "application/*" | "*/*"
    );
    let is_rle_media = matches!(eff_type.as_str(), "image/dicom-rle" | "image/x-dicom-rle");
    let allows_rle_media = is_rle_media || allows_raw_octet;

    match ts_param {
        Some(TS_EXPLICIT_VR_LE) => {
            if is_multipart && allows_raw_octet
                || matches!(
                    base_type,
                    "application/octet-stream" | "application/*" | "*/*"
                )
            {
                return Some((3, default_explicit_format()));
            }
            None
        }
        Some(TS_IMPLICIT_VR_LE) => {
            if is_multipart && allows_raw_octet
                || matches!(
                    base_type,
                    "application/octet-stream" | "application/*" | "*/*"
                )
            {
                return Some((
                    3,
                    NegotiatedFrameFormat {
                        syntax: NegotiatedTransferSyntax::ImplicitVrLittleEndian,
                        multipart_type: "application/octet-stream".to_string(),
                        part_content_type: format!(
                            "application/octet-stream; transfer-syntax={TS_IMPLICIT_VR_LE}"
                        ),
                    },
                ));
            }
            None
        }
        Some(TS_RLE_LOSSLESS) => {
            if (is_multipart && allows_rle_media)
                || matches!(
                    base_type,
                    "image/dicom-rle" | "image/x-dicom-rle" | "application/octet-stream" | "*/*"
                )
            {
                let media = if eff_type == "image/x-dicom-rle" {
                    "image/x-dicom-rle"
                } else {
                    "image/dicom-rle"
                };
                return Some((
                    3,
                    NegotiatedFrameFormat {
                        syntax: NegotiatedTransferSyntax::RleLossless,
                        multipart_type: media.to_string(),
                        part_content_type: format!("{media}; transfer-syntax={TS_RLE_LOSSLESS}"),
                    },
                ));
            }
            None
        }
        Some(TS_DEFLATED_IMAGE_FRAME) => {
            if is_multipart && allows_raw_octet
                || matches!(
                    base_type,
                    "application/octet-stream" | "application/*" | "*/*"
                )
            {
                return Some((
                    3,
                    NegotiatedFrameFormat {
                        syntax: NegotiatedTransferSyntax::DeflatedImageFrame,
                        multipart_type: "application/octet-stream".to_string(),
                        part_content_type: format!(
                            "application/octet-stream; transfer-syntax={TS_DEFLATED_IMAGE_FRAME}"
                        ),
                    },
                ));
            }
            None
        }
        Some("*") => {
            if is_rle_media {
                return Some((
                    2,
                    NegotiatedFrameFormat {
                        syntax: NegotiatedTransferSyntax::RleLossless,
                        multipart_type: eff_type.clone(),
                        part_content_type: format!("{eff_type}; transfer-syntax={TS_RLE_LOSSLESS}"),
                    },
                ));
            }
            if is_multipart && allows_raw_octet
                || matches!(
                    base_type,
                    "multipart/*" | "application/octet-stream" | "application/*" | "*/*"
                )
            {
                return Some((1, default_explicit_format()));
            }
            None
        }
        Some(_unsupported) => None,
        None => {
            if is_rle_media {
                return Some((
                    2,
                    NegotiatedFrameFormat {
                        syntax: NegotiatedTransferSyntax::RleLossless,
                        multipart_type: eff_type.clone(),
                        part_content_type: format!("{eff_type}; transfer-syntax={TS_RLE_LOSSLESS}"),
                    },
                ));
            }
            if is_multipart && eff_type == "application/octet-stream" {
                return Some((2, default_explicit_format()));
            }
            if (is_multipart && allows_raw_octet)
                || matches!(
                    base_type,
                    "application/octet-stream" | "application/*" | "multipart/*" | "*/*"
                )
            {
                return Some((0, default_explicit_format()));
            }
            None
        }
    }
}

/// Negotiates a DICOM PS3.18 WADO-RS pixel frame response format from an HTTP `Accept` header.
pub fn negotiate_wado_accept(accept_header: Option<&str>) -> Result<NegotiatedFrameFormat, String> {
    let raw = match accept_header {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return Ok(default_explicit_format()),
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

        if let Some((specificity, format)) = match_media_range(&base_type, &params) {
            candidates.push(Candidate {
                q,
                specificity,
                order,
                format,
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
        .map(|c| c.format)
        .ok_or_else(|| {
            format!(
                "Accept header {raw:?} is not supported; supported transfer syntaxes: {}",
                SUPPORTED_WADO_TRANSFER_SYNTAXES.join(", ")
            )
        })
}

fn packbits_encode(input: &[u8], out: &mut Vec<u8>) {
    let mut i = 0usize;
    let n = input.len();
    while i < n {
        let mut run = 1usize;
        while i + run < n && run < 128 && input[i + run] == input[i] {
            run += 1;
        }
        if run >= 2 {
            let ctrl = (1isize - run as isize) as i8 as u8;
            out.push(ctrl);
            out.push(input[i]);
            i += run;
        } else {
            let lit_start = i;
            i += 1;
            while i < n && (i - lit_start) < 128 {
                if i + 1 < n && input[i] == input[i + 1] {
                    break;
                }
                i += 1;
            }
            let lit_len = i - lit_start;
            out.push((lit_len - 1) as u8);
            out.extend_from_slice(&input[lit_start..i]);
        }
    }
    if out.len() % 2 == 1 {
        // -128 in PackBits is a documented no-op, preserving even byte alignment.
        out.push(0x80);
    }
}

/// Encodes an uncompressed little-endian frame into a DICOM PS3.5 Annex G RLE Lossless fragment.
pub fn encode_rle_lossless_frame(
    raw_le_pixels: &[u8],
    rows: usize,
    columns: usize,
    samples_per_pixel: usize,
    bits_allocated: usize,
) -> Result<Vec<u8>, String> {
    if rows == 0 || columns == 0 || samples_per_pixel == 0 {
        return Err("RLE Lossless requires non-zero dimensions and samples_per_pixel".to_string());
    }
    if bits_allocated != 8 && bits_allocated != 16 {
        return Err(format!(
            "RLE Lossless only supports 8-bit or 16-bit samples, got {bits_allocated}"
        ));
    }
    let bytes_per_sample = bits_allocated / 8;
    let num_segments = samples_per_pixel
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| "RLE segment count overflow".to_string())?;
    if num_segments > 15 {
        return Err(format!(
            "RLE Lossless supports at most 15 segments, got {num_segments}"
        ));
    }
    let frame_pixels = rows
        .checked_mul(columns)
        .ok_or_else(|| "frame pixel count overflow".to_string())?;
    let expected_len = frame_pixels
        .checked_mul(num_segments)
        .ok_or_else(|| "frame byte length overflow".to_string())?;
    if raw_le_pixels.len() < expected_len {
        return Err(format!(
            "truncated pixel buffer for RLE Lossless: got {} bytes, need {expected_len}",
            raw_le_pixels.len()
        ));
    }

    let mut out = vec![0u8; 64];
    out[..4].copy_from_slice(&(num_segments as u32).to_le_bytes());

    let mut plane = vec![0u8; frame_pixels];
    let mut seg_idx = 0usize;
    for sample in 0..samples_per_pixel {
        // DICOM PS3.5 G.2 orders multi-byte segments from MSB to LSB per sample.
        for byte_pos_le in (0..bytes_per_sample).rev() {
            let offset = u32::try_from(out.len())
                .map_err(|_| "RLE fragment exceeds u32 offset range".to_string())?;
            let hdr_pos = 4 + seg_idx * 4;
            out[hdr_pos..hdr_pos + 4].copy_from_slice(&offset.to_le_bytes());

            let base = sample * bytes_per_sample + byte_pos_le;
            for (px_idx, dst) in plane.iter_mut().enumerate() {
                *dst = raw_le_pixels[px_idx * num_segments + base];
            }
            packbits_encode(&plane, &mut out);
            seg_idx += 1;
        }
    }

    if out.len() % 2 == 1 {
        out.push(0);
    }
    Ok(out)
}

/// Compresses raw frame bytes using raw RFC 1951 deflate (DICOM `1.2.840.10008.1.2.8.1`).
pub fn encode_deflated_image_frame(raw_pixels: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(raw_pixels)
        .map_err(|e| format!("deflate write error: {e}"))?;
    let mut out = encoder
        .finish()
        .map_err(|e| format!("deflate finish error: {e}"))?;
    if out.len() % 2 == 1 {
        out.push(0);
    }
    Ok(out)
}

pub fn transcode_frame_for_wado(
    raw_le_pixels: &[u8],
    rows: usize,
    columns: usize,
    samples_per_pixel: usize,
    bits_allocated: usize,
    syntax: NegotiatedTransferSyntax,
    is_byte_range: bool,
) -> Result<Vec<u8>, String> {
    match syntax {
        NegotiatedTransferSyntax::ExplicitVrLittleEndian
        | NegotiatedTransferSyntax::ImplicitVrLittleEndian => Ok(raw_le_pixels.to_vec()),
        NegotiatedTransferSyntax::RleLossless => {
            if is_byte_range {
                return Err(
                    "HTTP Range requests require an uncompressed transfer syntax (1.2.840.10008.1.2.1)"
                        .to_string(),
                );
            }
            encode_rle_lossless_frame(raw_le_pixels, rows, columns, samples_per_pixel, bits_allocated)
        }
        NegotiatedTransferSyntax::DeflatedImageFrame => {
            if is_byte_range {
                return Err(
                    "HTTP Range requests require an uncompressed transfer syntax (1.2.840.10008.1.2.1)"
                        .to_string(),
                );
            }
            encode_deflated_image_frame(raw_le_pixels)
        }
    }
}
