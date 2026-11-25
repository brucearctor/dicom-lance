use std::io::Cursor;
use std::sync::OnceLock;

use axum::http::header::RANGE;
use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use dicom_core::value::{DataSetSequence, PixelFragmentSequence};
use dicom_core::{DataElement, Length, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_lance::api::{
    constant_time_token_eq, encode_multipart_related, extract_boundary, parse_cors_origins,
    parse_frame_numbers, parse_range_header, seed_phantoms_into_store, split_multipart_related,
};
use dicom_lance::codecs::{
    encode_rle_lossless_frame, negotiate_wado_accept, NegotiatedTransferSyntax, TS_DEFLATED_IMAGE_FRAME,
    TS_EXPLICIT_VR_LE, TS_IMPLICIT_VR_LE, TS_RLE_LOSSLESS,
};
use dicom_lance::deid::DeidConfig;
use dicom_lance::dicom_ingest::{
    extract_sr_text, parse_dicom_instances, render_thumbnail_png, slice_normal_projection,
};
use dicom_lance::embedding::{rrf_fuse, Embedder, LocalMultimodalEmbedder};
use dicom_lance::phantom::{CT_SERIES_UID, CT_STUDY_UID};
use dicom_lance::schema::{FrameRecord, SeriesRecord, EMBEDDING_DIM};
use dicom_lance::seg::{dicom_cielab_to_rgb, rgb_to_dicom_cielab, unpack_bitpacked_frames};
use dicom_lance::sr::{
    build_tid1500_measurement_sr, MeasurementInput, MeasurementSrRequest, MeasurementToolKind,
};
use dicom_lance::store::{parse_storage_root, validate_sql_literal, DicomLanceStore, SeriesSummary};
use dicom_object::{FileMetaTableBuilder, InMemDicomObject};
use hegel::generators as gs;
use hegel::{Settings, TestCase};
use tempfile::TempDir;

fn put_str(obj: &mut InMemDicomObject, tag: Tag, vr: VR, val: &str) {
    obj.put_element(DataElement::new(tag, vr, PrimitiveValue::from(val)));
}

fn put_u16(obj: &mut InMemDicomObject, tag: Tag, val: u16) {
    obj.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(val)));
}

struct SyntheticCtSlice<'a> {
    study_uid: &'a str,
    series_uid: &'a str,
    sop_uid: &'a str,
    instance_number: i32,
    dim: usize,
    ipp: [f64; 3],
    iop: [f64; 6],
    fill_val: u16,
}

fn build_test_ct_dcm(spec: SyntheticCtSlice<'_>) -> Bytes {
    let SyntheticCtSlice {
        study_uid,
        series_uid,
        sop_uid,
        instance_number,
        dim,
        ipp,
        iop,
        fill_val,
    } = spec;
    let mut obj = InMemDicomObject::new_empty();
    put_str(&mut obj, tags::PATIENT_ID, VR::LO, "PBT-001");
    put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "Hegel^Property");
    put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, study_uid);
    put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, series_uid);
    put_str(&mut obj, tags::MODALITY, VR::CS, "CT");
    put_str(
        &mut obj,
        tags::STUDY_DESCRIPTION,
        VR::LO,
        "Hegel Thoracic Phantom",
    );
    put_str(
        &mut obj,
        tags::SERIES_DESCRIPTION,
        VR::LO,
        "Oblique Helical CT",
    );
    put_str(
        &mut obj,
        tags::SOP_CLASS_UID,
        VR::UI,
        uids::CT_IMAGE_STORAGE,
    );
    put_str(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, sop_uid);
    put_str(
        &mut obj,
        tags::INSTANCE_NUMBER,
        VR::IS,
        &instance_number.to_string(),
    );
    put_str(&mut obj, tags::SERIES_NUMBER, VR::IS, "1");
    put_u16(&mut obj, tags::ROWS, dim as u16);
    put_u16(&mut obj, tags::COLUMNS, dim as u16);
    put_u16(&mut obj, tags::SAMPLES_PER_PIXEL, 1);
    put_str(
        &mut obj,
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        "MONOCHROME2",
    );
    put_u16(&mut obj, tags::BITS_ALLOCATED, 16);
    put_u16(&mut obj, tags::BITS_STORED, 16);
    put_u16(&mut obj, tags::HIGH_BIT, 15);
    put_u16(&mut obj, tags::PIXEL_REPRESENTATION, 0);
    put_str(&mut obj, tags::PIXEL_SPACING, VR::DS, "0.75\\0.75");
    put_str(&mut obj, tags::SLICE_THICKNESS, VR::DS, "2.5");
    put_str(
        &mut obj,
        tags::IMAGE_POSITION_PATIENT,
        VR::DS,
        &format!("{:.6}\\{:.6}\\{:.6}", ipp[0], ipp[1], ipp[2]),
    );
    put_str(
        &mut obj,
        tags::IMAGE_ORIENTATION_PATIENT,
        VR::DS,
        &format!(
            "{:.6}\\{:.6}\\{:.6}\\{:.6}\\{:.6}\\{:.6}",
            iop[0], iop[1], iop[2], iop[3], iop[4], iop[5]
        ),
    );
    put_str(&mut obj, tags::RESCALE_INTERCEPT, VR::DS, "-1024");
    put_str(&mut obj, tags::RESCALE_SLOPE, VR::DS, "1");
    put_str(&mut obj, tags::WINDOW_CENTER, VR::DS, "-600\\40");
    put_str(&mut obj, tags::WINDOW_WIDTH, VR::DS, "1500\\400");

    let mut raw_pixels = Vec::with_capacity(dim * dim * 2);
    for px in 0..(dim * dim) {
        let v = fill_val.wrapping_add((px % 37) as u16);
        raw_pixels.extend_from_slice(&v.to_le_bytes());
    }
    obj.put_element(DataElement::new(
        tags::PIXEL_DATA,
        VR::OW,
        PrimitiveValue::from(raw_pixels),
    ));

    let file_obj = obj
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(sop_uid),
        )
        .unwrap();
    let mut out = Vec::new();
    file_obj.write_all(&mut out).unwrap();
    Bytes::from(out)
}

fn make_sr_item(concept_name: &str, text_val: &str, children: Vec<InMemDicomObject>) -> InMemDicomObject {
    let mut item = InMemDicomObject::new_empty();
    let mut name_item = InMemDicomObject::new_empty();
    put_str(&mut name_item, tags::CODE_MEANING, VR::LO, concept_name);
    item.put_element(DataElement::new(
        tags::CONCEPT_NAME_CODE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![name_item], Length::UNDEFINED),
    ));
    put_str(&mut item, tags::TEXT_VALUE, VR::UT, text_val);
    if !children.is_empty() {
        item.put_element(DataElement::new(
            tags::CONTENT_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(children, Length::UNDEFINED),
        ));
    }
    item
}

#[hegel::test]
fn pbt_extract_boundary_round_trip_and_rejection(tc: TestCase) {
    let token = tc.draw(gs::from_regex(r"[A-Za-z0-9._+-]{1,32}"));
    let key = tc.draw(gs::sampled_from(&[
        "boundary",
        "BOUNDARY",
        "Boundary",
        "bOuNdArY",
    ]));
    let quoted = tc.draw(gs::booleans());
    let param = if quoted {
        format!("{key}=\"{token}\"")
    } else {
        format!("{key}={token}")
    };
    let header = if tc.draw(gs::booleans()) {
        format!("multipart/related; type=\"application/dicom\"; {param}; charset=utf-8")
    } else {
        format!("multipart/related; {param}")
    };
    assert_eq!(extract_boundary(&header), Some(token));

    // Reject empty or CR/LF-containing boundaries, or headers without boundary=
    assert_eq!(extract_boundary("multipart/related; boundary=\"\""), None);
    assert_eq!(extract_boundary("multipart/related; boundary="), None);
    assert_eq!(
        extract_boundary("multipart/related; boundary=\"bad\nline\""),
        None
    );
    assert_eq!(extract_boundary("application/dicom+json"), None);

    let arbitrary = tc.draw(gs::text().max_size(128));
    let _ = extract_boundary(&arbitrary);
}

#[hegel::test]
fn pbt_multipart_related_round_trip_and_fuzz(tc: TestCase) {
    let boundary = tc.draw(gs::from_regex(r"bnd-[A-Za-z0-9]{4,16}"));
    let delim = format!("--{boundary}").into_bytes();

    let parts = tc.draw(
        gs::vecs(gs::binary().max_size(256))
            .min_size(1)
            .max_size(6),
    );
    tc.assume(!parts.iter().any(|p| p.windows(delim.len()).any(|w| w == delim.as_slice())));

    let part_specs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|p| ("application/octet-stream", p.as_slice()))
        .collect();
    let encoded = encode_multipart_related(&boundary, &part_specs);
    let decoded = split_multipart_related(&encoded, &boundary);

    assert_eq!(decoded.len(), parts.len());
    for (got, expected) in decoded.iter().zip(parts.iter()) {
        assert_eq!(got.as_ref(), expected.as_slice());
    }

    // Empty/invalid boundaries must return empty cleanly without panicking
    assert!(split_multipart_related(&encoded, "").is_empty());
    assert!(split_multipart_related(&encoded, "bad\r\n").is_empty());

    // Arbitrary byte streams and boundary strings must never panic or hang
    let fuzz_body = tc.draw(gs::binary().max_size(512));
    let fuzz_boundary = tc.draw(gs::text().max_size(32));
    let _ = split_multipart_related(&fuzz_body, &fuzz_boundary);
}

#[hegel::test]
fn pbt_parse_range_header_bounds_and_no_overflow(tc: TestCase) {
    let start = tc.draw(gs::integers::<u64>());
    let end = tc.draw(gs::integers::<u64>());

    let mut headers = HeaderMap::new();
    headers.insert(
        RANGE,
        HeaderValue::from_str(&format!("bytes={start}-{end}")).unwrap(),
    );
    let parsed = parse_range_header(&headers);
    if end >= start && end < u64::MAX {
        assert_eq!(parsed, Some(start..(end + 1)));
    } else {
        assert_eq!(parsed, None);
    }

    // Explicit u64::MAX must never overflow or panic
    let mut max_headers = HeaderMap::new();
    max_headers.insert(
        RANGE,
        HeaderValue::from_str(&format!("bytes=0-{}", u64::MAX)).unwrap(),
    );
    assert_eq!(parse_range_header(&max_headers), None);

    // Open-ended byte range with case-insensitive prefix
    let mut open_headers = HeaderMap::new();
    let prefix = tc.draw(gs::sampled_from(&["bytes=", "BYTES=", "Bytes="]));
    open_headers.insert(
        RANGE,
        HeaderValue::from_str(&format!("{prefix}{start}-")).unwrap(),
    );
    assert_eq!(parse_range_header(&open_headers), Some(start..u64::MAX));

    // Arbitrary header values must never panic
    let raw = tc.draw(gs::text().max_size(96));
    if let Ok(val) = HeaderValue::from_str(&raw) {
        let mut h = HeaderMap::new();
        h.insert(RANGE, val);
        let _ = parse_range_header(&h);
    }
}

#[hegel::test]
fn pbt_parse_frame_numbers_valid_and_invalid(tc: TestCase) {
    let nums = tc.draw(
        gs::vecs(gs::integers::<i32>().min_value(1).max_value(100_000))
            .min_size(1)
            .max_size(16),
    );
    let joined = nums
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(parse_frame_numbers(&joined).unwrap(), nums);

    let bad_token = tc.draw(gs::sampled_from(&["0", "-1", "bogus", "", "999999999999999"]));
    let bad_joined = format!("{joined},{bad_token}");
    assert!(parse_frame_numbers(&bad_joined).is_err());
    assert!(parse_frame_numbers(bad_token).is_err());
}

#[hegel::test]
fn pbt_validate_sql_literal_accepts_uids_and_blocks_sqli(tc: TestCase) {
    let uid = tc.draw(gs::from_regex(r"1\.2\.[0-9]{1,8}(\.[0-9]{1,8}){1,6}"));
    let ok = validate_sql_literal(&uid, "uid").expect("valid UID must be accepted");
    assert_eq!(ok, uid);

    let prefix = tc.draw(gs::from_regex(r"[A-Za-z0-9.]{0,12}"));
    let bad_tok = tc.draw(gs::sampled_from(&[
        "'", "\"", ";", "--", "/*", "\0", "\n", "é", "=", "(", ")",
    ]));
    let suffix = tc.draw(gs::from_regex(r"[A-Za-z0-9.]{0,12}"));
    let injected = format!("{prefix}{bad_tok}{suffix}");
    assert!(
        validate_sql_literal(&injected, "uid").is_err(),
        "SQL/disallowed token {injected:?} must be rejected"
    );

    assert!(validate_sql_literal("", "uid").is_err());
    assert!(validate_sql_literal("   ", "uid").is_err());
    assert!(validate_sql_literal(&"a".repeat(129), "uid").is_err());

    let arb = tc.draw(gs::text().max_size(160));
    if let Ok(validated) = validate_sql_literal(&arb, "uid") {
        assert!(!validated.is_empty() && validated.len() <= 128);
        assert!(!validated.contains("--"));
        assert!(validated
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'^' | b' ')));
    }
}

#[hegel::test(test_cases = 25)]
fn pbt_slice_normal_projection_and_series_permutation_invariance(tc: TestCase) {
    let alpha = tc.draw(
        gs::floats::<f64>()
            .min_value(-std::f64::consts::PI)
            .max_value(std::f64::consts::PI),
    );
    let beta = tc.draw(
        gs::floats::<f64>()
            .min_value(-std::f64::consts::PI)
            .max_value(std::f64::consts::PI),
    );
    let gamma = tc.draw(
        gs::floats::<f64>()
            .min_value(-std::f64::consts::PI)
            .max_value(std::f64::consts::PI),
    );

    // Build orthonormal 3D rotation matrix columns (row_cosines, col_cosines)
    let (sa, ca) = alpha.sin_cos();
    let (sb, cb) = beta.sin_cos();
    let (sg, cg) = gamma.sin_cos();
    let rx = cg * cb;
    let ry = sg * cb;
    let rz = -sb;
    let cx = cg * sb * sa - sg * ca;
    let cy = sg * sb * sa + cg * ca;
    let cz = cb * sa;
    let iop = [rx, ry, rz, cx, cy, cz];

    let row_norm = (rx * rx + ry * ry + rz * rz).sqrt();
    let col_norm = (cx * cx + cy * cy + cz * cz).sqrt();
    let dot_rc = rx * cx + ry * cy + rz * cz;
    assert!((row_norm - 1.0).abs() < 1e-9);
    assert!((col_norm - 1.0).abs() < 1e-9);
    assert!(dot_rc.abs() < 1e-9);

    let nx = ry * cz - rz * cy;
    let ny = rz * cx - rx * cz;
    let nz = rx * cy - ry * cx;

    let ox = tc.draw(gs::floats::<f64>().min_value(-150.0).max_value(150.0));
    let oy = tc.draw(gs::floats::<f64>().min_value(-150.0).max_value(150.0));
    let oz = tc.draw(gs::floats::<f64>().min_value(-150.0).max_value(150.0));
    let dt = tc.draw(gs::floats::<f64>().min_value(0.5).max_value(8.0));
    let n_slices = tc.draw(gs::integers::<usize>().min_value(3).max_value(7));

    let study_uid = "1.2.826.0.1.3680043.10.5432.9001";
    let series_uid = "1.2.826.0.1.3680043.10.5432.9001.1";
    let ordered: Vec<Bytes> = (0..n_slices)
        .map(|k| {
            let t = k as f64 * dt;
            let ipp = [ox + t * nx, oy + t * ny, oz + t * nz];
            build_test_ct_dcm(SyntheticCtSlice {
                study_uid,
                series_uid,
                sop_uid: &format!("{series_uid}.{}", k + 1),
                instance_number: (k + 1) as i32,
                dim: 8,
                ipp,
                iop,
                fill_val: 1000 + (k as u16) * 10,
            })
        })
        .collect();

    let indices: Vec<usize> = (0..n_slices).collect();
    let perm = tc.draw(gs::permutations(indices));
    let permuted: Vec<Bytes> = perm.iter().map(|&idx| ordered[idx].clone()).collect();

    let canonical = parse_dicom_instances(&ordered).unwrap();
    let shuffled = parse_dicom_instances(&permuted).unwrap();
    assert_eq!(canonical.len(), 1);
    assert_eq!(shuffled.len(), 1);

    let c_ser = &canonical[0];
    let s_ser = &shuffled[0];

    // Series metadata and slice ordering must be strictly permutation-invariant
    assert!(c_ser.series.is_volumetric);
    assert_eq!(s_ser.series.is_volumetric, c_ser.series.is_volumetric);
    assert_eq!(s_ser.series.frame_count, n_slices as i32);
    assert!((s_ser.series.spacing_z_mm - dt).abs() < 1e-2);
    assert_eq!(s_ser.series.thumbnail_png, c_ser.series.thumbnail_png);
    assert_eq!(s_ser.series.sr_text, c_ser.series.sr_text);
    assert_eq!(s_ser.series.searchable_text, c_ser.series.searchable_text);

    for idx in 0..n_slices {
        assert_eq!(s_ser.frames[idx].slice_sort_index, idx as i32);
        assert_eq!(
            s_ser.frames[idx].sop_instance_uid,
            c_ser.frames[idx].sop_instance_uid
        );
        assert_eq!(s_ser.frames[idx].pixel_bytes, c_ser.frames[idx].pixel_bytes);
        if idx + 1 < n_slices {
            let p0 = slice_normal_projection(
                [
                    s_ser.frames[idx].ipp_x,
                    s_ser.frames[idx].ipp_y,
                    s_ser.frames[idx].ipp_z,
                ],
                s_ser.frames[idx].iop,
            );
            let p1 = slice_normal_projection(
                [
                    s_ser.frames[idx + 1].ipp_x,
                    s_ser.frames[idx + 1].ipp_y,
                    s_ser.frames[idx + 1].ipp_z,
                ],
                s_ser.frames[idx + 1].iop,
            );
            assert!(p1 > p0, "expected strictly increasing normal projection");
        }
    }
}

#[hegel::test]
fn pbt_render_thumbnail_png_never_panics_and_emits_valid_png(tc: TestCase) {
    let valid_mode = tc.draw(gs::booleans());
    let (rows, cols, spp, bits, pixel_bytes) = if valid_mode {
        let r = tc.draw(gs::integers::<i32>().min_value(1).max_value(16));
        let c = tc.draw(gs::integers::<i32>().min_value(1).max_value(16));
        let s = tc.draw(gs::integers::<i32>().min_value(1).max_value(3));
        let b = tc.draw(gs::sampled_from(&[8i32, 16, 32]));
        let need = (r as usize) * (c as usize) * (s as usize) * (b as usize).div_ceil(8);
        let seed_bytes = tc.draw(gs::binary().min_size(1).max_size(32));
        let mut buf = Vec::with_capacity(need);
        for i in 0..need {
            buf.push(seed_bytes[i % seed_bytes.len()]);
        }
        (r, c, s, b, buf)
    } else {
        let r = tc.draw(gs::integers::<i32>().min_value(-2).max_value(20));
        let c = tc.draw(gs::integers::<i32>().min_value(-2).max_value(20));
        let s = tc.draw(gs::integers::<i32>().min_value(0).max_value(3));
        let b = tc.draw(gs::sampled_from(&[-8i32, 0, 8, 16, 24, 32, 64]));
        let buf = tc.draw(gs::binary().max_size(512));
        (r, c, s, b, buf)
    };

    let pixel_rep = tc.draw(gs::integers::<i32>().min_value(0).max_value(1));
    let rescale_slope = tc.draw(gs::floats::<f64>());
    let rescale_intercept = tc.draw(gs::floats::<f64>());
    let window_center = tc.draw(gs::floats::<f64>());
    let window_width = tc.draw(gs::floats::<f64>());

    let frame = FrameRecord {
        study_instance_uid: "1.2.3".to_string(),
        series_instance_uid: "1.2.3.1".to_string(),
        sop_instance_uid: "1.2.3.1.1".to_string(),
        sop_class_uid: uids::CT_IMAGE_STORAGE.to_string(),
        instance_number: 1,
        frame_number: 1,
        slice_sort_index: 0,
        rows,
        columns: cols,
        samples_per_pixel: spp,
        bits_allocated: bits,
        bits_stored: bits.max(1),
        high_bit: (bits - 1).max(0),
        pixel_representation: pixel_rep,
        photometric_interpretation: "MONOCHROME2".to_string(),
        transfer_syntax_uid: uids::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
        rescale_slope,
        rescale_intercept,
        window_center,
        window_width,
        slice_location: 0.0,
        slice_thickness: 1.0,
        pixel_spacing_row_mm: 1.0,
        pixel_spacing_col_mm: 1.0,
        ipp_x: 0.0,
        ipp_y: 0.0,
        ipp_z: 0.0,
        iop: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        dicom_json_header: "{}".to_string(),
        frame_embedding: None,
        pixel_bytes,
    };

    let res = render_thumbnail_png(&frame).expect("thumbnail encoding must not error");
    if valid_mode {
        assert!(res.is_some(), "valid frame dimensions must emit PNG");
    }
    if let Some(png_bytes) = res {
        let decoder = png::Decoder::new(Cursor::new(&png_bytes));
        let reader = decoder.read_info().expect("valid PNG header");
        let info = reader.info();
        assert_eq!(info.width, 128);
        assert_eq!(info.height, 128);
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
    }
}

#[hegel::test_helper]
fn gen_sr_node(
    tc: &TestCase,
    depth: usize,
    leaves: &mut Vec<(String, String)>,
) -> InMemDicomObject {
    let concept = tc.draw(gs::from_regex(r"Concept_[A-Za-z0-9]{2,10}"));
    let value = tc.draw(gs::from_regex(r"Val_[A-Za-z0-9]{2,12}"));
    leaves.push((concept.clone(), value.clone()));

    let mut children = Vec::new();
    if depth > 0 {
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        for _ in 0..count {
            children.push(gen_sr_node(tc, depth - 1, leaves));
        }
    }
    make_sr_item(&concept, &value, children)
}

#[hegel::test]
fn pbt_sr_recursive_extraction_preserves_all_leaves(tc: TestCase) {
    let depth = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let top_count = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let mut expected_leaves = Vec::new();
    let mut top_items = Vec::new();
    for _ in 0..top_count {
        top_items.push(gen_sr_node(&tc, depth, &mut expected_leaves));
    }

    let mut root = InMemDicomObject::new_empty();
    root.put_element(DataElement::new(
        tags::CONTENT_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(top_items, Length::UNDEFINED),
    ));

    let text = extract_sr_text(&root);
    for (concept, value) in &expected_leaves {
        let line = format!("{concept}: {value}");
        assert!(
            text.contains(&line),
            "expected extracted SR text to contain {line:?}, got:\n{text}"
        );
    }
}

#[test]
fn pbt_lance_blob_range_oracle_and_merge_idempotency() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let store = DicomLanceStore::new(tmp.path());

    let study_uid = "1.2.826.0.1.3680043.10.5432.8801";
    let series_uid = "1.2.826.0.1.3680043.10.5432.8801.1";
    // 64x64x2 = 8192 bytes (> 4096 inline threshold -> Packed Blob V2)
    let raw_slices: Vec<Bytes> = (0..3)
        .map(|k| {
            build_test_ct_dcm(SyntheticCtSlice {
                study_uid,
                series_uid,
                sop_uid: &format!("{series_uid}.{}", k + 1),
                instance_number: k + 1,
                dim: 64,
                ipp: [0.0, 0.0, k as f64 * 2.5],
                iop: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                fill_val: 1200 + (k as u16) * 100,
            })
        })
        .collect();

    let canonical = parse_dicom_instances(&raw_slices).unwrap();
    let expected_pixels: Vec<Vec<u8>> = canonical[0]
        .frames
        .iter()
        .map(|f| f.pixel_bytes.clone())
        .collect();
    assert_eq!(expected_pixels[0].len(), 8192);

    rt.block_on(store.ingest(&canonical)).unwrap();

    hegel::Hegel::new(|tc: TestCase| {
        let slice_idx = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let sop_uid = format!("{series_uid}.{}", slice_idx + 1);
        let full = &expected_pixels[slice_idx];
        let total = full.len() as u64;

        let start = tc.draw(hegel::one_of!(
            gs::integers::<u64>().min_value(0).max_value(total + 64),
            gs::sampled_from(vec![0u64, total.saturating_sub(1), total]),
        ));
        let end = tc.draw(hegel::one_of!(
            gs::integers::<u64>().min_value(0).max_value(total + 64),
            gs::sampled_from(vec![0u64, total.saturating_sub(1), total, u64::MAX]),
        ));

        let mut headers = HeaderMap::new();
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes={start}-{end}")).unwrap(),
        );
        let byte_range = parse_range_header(&headers);

        let payloads = rt
            .block_on(store.read_frame_blobs(study_uid, series_uid, &sop_uid, &[1], byte_range.clone()))
            .unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].blob_kind, "Packed");
        assert_eq!(payloads[0].total_blob_size, total);

        match byte_range {
            None => assert_eq!(payloads[0].bytes, *full),
            Some(r) => {
                let s = r.start.min(total) as usize;
                let e = r.end.min(total) as usize;
                let expected_slice = if s >= e { &[][..] } else { &full[s..e] };
                assert_eq!(payloads[0].bytes.as_slice(), expected_slice);
            }
        }

        // Re-ingest a random non-empty permutation subset and check MergeInsert idempotency
        let subset = tc.draw(gs::subsequences(&[0usize, 1, 2]).min_size(1));
        let perm = tc.draw(gs::permutations(subset));
        let subset_bytes: Vec<Bytes> = perm.iter().map(|&i| raw_slices[i].clone()).collect();
        let re_parsed = parse_dicom_instances(&subset_bytes).unwrap();
        rt.block_on(store.ingest(&re_parsed)).unwrap();

        let stats = rt.block_on(store.get_stats()).unwrap();
        assert_eq!(stats.series_dataset.row_count, 1);
        assert_eq!(stats.frames_dataset.row_count, 3);
        assert_eq!(stats.blob_v2_kind_sample.as_deref(), Some("Packed"));

        let hits = rt
            .block_on(store.search_series(Some("Helical"), Some("CT"), None, 10))
            .unwrap();
        assert_eq!(hits.len(), 1);

        // All 3 original frame pixel blobs remain byte-exact after subset upsert
        for (idx, expected_full) in expected_pixels.iter().enumerate() {
            let check_sop = format!("{series_uid}.{}", idx + 1);
            let check_payload = rt
                .block_on(store.read_frame_blobs(study_uid, series_uid, &check_sop, &[1], None))
                .unwrap();
            assert_eq!(check_payload[0].bytes, *expected_full);
        }
    })
    .settings(Settings::new().test_cases(15))
    .run();
}

fn l2_norm(vec: &[f32]) -> f32 {
    vec.iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum::<f64>()
        .sqrt() as f32
}

#[test]
fn pbt_local_embedder_unit_norm_and_determinism() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let embedder = LocalMultimodalEmbedder;

    hegel::Hegel::new(|tc: TestCase| {
        let query = tc.draw(gs::text().max_size(128));
        let v1 = rt.block_on(embedder.embed_text(&query)).unwrap();
        let v2 = rt.block_on(embedder.embed_text(&query)).unwrap();
        assert_eq!(v1.len(), EMBEDDING_DIM as usize);
        assert_eq!(v1, v2);
        assert!(v1.iter().all(|v| v.is_finite()));
        assert!(
            (l2_norm(&v1) - 1.0).abs() < 1e-4,
            "embed_text norm was {}",
            l2_norm(&v1)
        );

        let valid_mode = tc.draw(gs::booleans());
        let (rows, cols, bits, pixel_bytes) = if valid_mode {
            let r = tc.draw(gs::integers::<i32>().min_value(1).max_value(16));
            let c = tc.draw(gs::integers::<i32>().min_value(1).max_value(16));
            let b = tc.draw(gs::sampled_from(&[8i32, 16, 32]));
            let need = (r as usize) * (c as usize) * (b as usize).div_ceil(8);
            let seed = tc.draw(gs::binary().min_size(1).max_size(32));
            let mut buf = Vec::with_capacity(need);
            for i in 0..need {
                buf.push(seed[i % seed.len()]);
            }
            (r, c, b, buf)
        } else {
            let r = tc.draw(gs::integers::<i32>().min_value(-2).max_value(16));
            let c = tc.draw(gs::integers::<i32>().min_value(-2).max_value(16));
            let b = tc.draw(gs::sampled_from(&[0i32, 8, 16, 24, 32]));
            let buf = tc.draw(gs::binary().max_size(128));
            (r, c, b, buf)
        };

        let frame = FrameRecord {
            study_instance_uid: "1.2.3".to_string(),
            series_instance_uid: "1.2.3.1".to_string(),
            sop_instance_uid: "1.2.3.1.1".to_string(),
            sop_class_uid: uids::CT_IMAGE_STORAGE.to_string(),
            instance_number: 1,
            frame_number: 1,
            slice_sort_index: 0,
            rows,
            columns: cols,
            samples_per_pixel: 1,
            bits_allocated: bits,
            bits_stored: bits.max(1),
            high_bit: (bits - 1).max(0),
            pixel_representation: tc.draw(gs::integers::<i32>().min_value(0).max_value(1)),
            photometric_interpretation: "MONOCHROME2".to_string(),
            transfer_syntax_uid: uids::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
            rescale_slope: tc.draw(gs::floats::<f64>()),
            rescale_intercept: tc.draw(gs::floats::<f64>()),
            window_center: tc.draw(gs::floats::<f64>()),
            window_width: tc.draw(gs::floats::<f64>()),
            slice_location: 0.0,
            slice_thickness: 1.5,
            pixel_spacing_row_mm: 0.75,
            pixel_spacing_col_mm: 0.75,
            ipp_x: 0.0,
            ipp_y: 0.0,
            ipp_z: 0.0,
            iop: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            dicom_json_header: "{}".to_string(),
            frame_embedding: None,
            pixel_bytes,
        };

        let f1 = rt.block_on(embedder.embed_frame(&frame, &query)).unwrap();
        let f2 = rt.block_on(embedder.embed_frame(&frame, &query)).unwrap();
        assert_eq!(f1.len(), EMBEDDING_DIM as usize);
        assert_eq!(f1, f2);
        assert!(f1.iter().all(|v| v.is_finite()));
        assert!((l2_norm(&f1) - 1.0).abs() < 1e-4);

        let series = SeriesRecord {
            study_instance_uid: "1.2.3".to_string(),
            series_instance_uid: "1.2.3.1".to_string(),
            patient_id: "PBT".to_string(),
            patient_name: "Hegel".to_string(),
            accession_number: String::new(),
            study_date: String::new(),
            study_description: "Test".to_string(),
            modality: "CT".to_string(),
            body_part_examined: "CHEST".to_string(),
            series_number: 1,
            series_description: query.clone(),
            instance_count: 1,
            frame_count: if valid_mode { 1 } else { 0 },
            rows: rows.max(0),
            columns: cols.max(0),
            spacing_x_mm: 1.0,
            spacing_y_mm: 1.0,
            spacing_z_mm: 1.0,
            window_center_default: 40.0,
            window_width_default: 400.0,
            is_volumetric: false,
            sr_text: query.clone(),
            searchable_text: query,
            series_embedding: None,
            thumbnail_png: None,
            updated_at_micros: 0,
        };
        let empty_s = rt.block_on(embedder.embed_series(&series, &[])).unwrap();
        assert_eq!(empty_s.len(), EMBEDDING_DIM as usize);
        assert!(empty_s.iter().all(|v| v.is_finite()));
        assert!((l2_norm(&empty_s) - 1.0).abs() < 1e-4);

        let frame_slice = [frame];
        let s1 = rt
            .block_on(embedder.embed_series(&series, &frame_slice))
            .unwrap();
        assert_eq!(s1.len(), EMBEDDING_DIM as usize);
        assert!(s1.iter().all(|v| v.is_finite()));
        assert!((l2_norm(&s1) - 1.0).abs() < 1e-4);
    })
    .settings(Settings::new().test_cases(35))
    .run();
}

#[hegel::test]
fn pbt_rrf_fusion_monotonicity_and_symmetry(tc: TestCase) {
    let bm25_ids = tc.draw(gs::vecs(gs::from_regex(r"doc_[a-h]")).max_size(12));
    let vec_ids = tc.draw(gs::vecs(gs::from_regex(r"doc_[a-h]")).max_size(12));
    let k = tc.draw(hegel::one_of!(
        gs::floats::<f32>().min_value(1.0).max_value(200.0),
        gs::sampled_from(vec![0.0f32, -10.0, f32::NAN, f32::INFINITY]),
    ));

    let fused_ab = rrf_fuse(&bm25_ids, &vec_ids, k);
    let fused_ba = rrf_fuse(&vec_ids, &bm25_ids, k);

    // Exact symmetry and deterministic tie-breaking
    assert_eq!(fused_ab, fused_ba);

    for win in fused_ab.windows(2) {
        let (id_a, sc_a) = &win[0];
        let (id_b, sc_b) = &win[1];
        assert!(sc_a.is_finite() && *sc_a > 0.0);
        assert!(sc_b.is_finite() && *sc_b > 0.0);
        assert!(
            *sc_a > *sc_b || ((*sc_a - *sc_b).abs() == 0.0 && id_a < id_b),
            "expected descending score / ascending ID ordering: {:?} vs {:?}",
            win[0],
            win[1]
        );
    }

    // Document appearing at rank 1 in both lists strictly outranks document at rank 1 in one list
    let list_a = vec!["shared".to_string(), "only_a".to_string()];
    let list_b = vec!["only_b".to_string(), "shared".to_string()];
    let mono = rrf_fuse(&list_a, &list_b, 60.0);
    assert_eq!(mono[0].0, "shared");
    assert!(mono[0].1 > mono[1].1);
}

#[hegel::test]
fn pbt_storage_root_uri_normalization(tc: TestCase) {
    let scheme = tc.draw(gs::sampled_from(&["shared-memory", "memory", "file"]));
    let authority = tc.draw(gs::from_regex(r"[a-z0-9-]{2,14}"));
    let prefix = tc.draw(gs::from_regex(r"[a-z0-9_-]{1,12}"));
    let slashes = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
    let tail = "/".repeat(slashes);

    if scheme == "file" {
        let uri = format!("file:///tmp/{authority}/{prefix}{tail}");
        let root = parse_storage_root(&uri).unwrap();
        assert!(!root.is_remote());
        assert!(root.local_path().is_some());
        assert!(root.series_uri().ends_with("/lance/dicom_series.lance"));
    } else {
        let uri = format!("{scheme}://{authority}/{prefix}{tail}");
        let root = parse_storage_root(&uri).unwrap();
        assert!(root.is_remote());
        assert!(root.local_path().is_none());
        let s_uri = root.series_uri();
        let f_uri = root.frames_uri();
        assert!(!s_uri.contains("//lance/"), "unexpected //lance/ in {s_uri}");
        assert!(!f_uri.contains("//lance/"), "unexpected //lance/ in {f_uri}");
        assert!(s_uri.ends_with("/lance/dicom_series.lance"));
        assert!(f_uri.ends_with("/lance/dicom_frames.lance"));
    }

    let local_raw = format!("/tmp/{authority}/{prefix}{tail}");
    let local_root = parse_storage_root(&local_raw).unwrap();
    assert!(!local_root.is_remote());
    assert!(local_root.local_path().is_some());

    assert!(parse_storage_root("").is_err());
    assert!(parse_storage_root("   ").is_err());
    assert!(parse_storage_root("shared-memory:///leading-slash").is_err());
}

#[hegel::test]
fn pbt_wado_accept_negotiation_never_panics_and_honors_q_values(tc: TestCase) {
    // Cornerstone3D exact Accept header resolves to Explicit VR Little Endian
    let cs = negotiate_wado_accept(Some(
        "multipart/related; type=application/octet-stream; transfer-syntax=*",
    ))
    .unwrap();
    assert_eq!(cs.syntax, NegotiatedTransferSyntax::ExplicitVrLittleEndian);

    // Higher quality weight selects RLE over Explicit VR LE regardless of order
    let prefer_rle = format!(
        "multipart/related; type=\"application/octet-stream\"; transfer-syntax={TS_EXPLICIT_VR_LE}; q=0.4, \
         multipart/related; type=\"image/dicom-rle\"; transfer-syntax={TS_RLE_LOSSLESS}; q=0.9"
    );
    assert_eq!(
        negotiate_wado_accept(Some(&prefer_rle)).unwrap().syntax,
        NegotiatedTransferSyntax::RleLossless
    );

    let prefer_deflate = format!(
        "multipart/related; type=\"application/octet-stream\"; transfer-syntax={TS_DEFLATED_IMAGE_FRAME}; q=0.95, \
         multipart/related; type=\"application/octet-stream\"; transfer-syntax={TS_IMPLICIT_VR_LE}; q=0.5"
    );
    assert_eq!(
        negotiate_wado_accept(Some(&prefer_deflate)).unwrap().syntax,
        NegotiatedTransferSyntax::DeflatedImageFrame
    );

    // Unsupported transfer syntax with no fallback yields Err -> 406
    let unsupported_uid = tc.draw(gs::from_regex(r"1\.2\.840\.10008\.1\.2\.4\.(90|91|100|201)"));
    let strict_unsupported = format!(
        "multipart/related; type=\"application/octet-stream\"; transfer-syntax={unsupported_uid}"
    );
    assert!(negotiate_wado_accept(Some(&strict_unsupported)).is_err());

    // Arbitrary strings must never panic
    let arb = tc.draw(gs::text().max_size(160));
    let _ = negotiate_wado_accept(Some(&arb));
}

#[hegel::test(test_cases = 25)]
fn pbt_rle_lossless_encode_decode_round_trip(tc: TestCase) {
    let rows = tc.draw(gs::integers::<usize>().min_value(2).max_value(24));
    let cols = tc.draw(gs::integers::<usize>().min_value(2).max_value(24));
    let spp = tc.draw(gs::sampled_from(&[1usize, 3]));
    let bits = tc.draw(gs::sampled_from(&[8usize, 16]));
    let bytes_per_sample = bits / 8;
    let total_bytes = rows * cols * spp * bytes_per_sample;
    let seed = tc.draw(gs::binary().min_size(1).max_size(48));

    let mut raw_pixels = Vec::with_capacity(total_bytes);
    for i in 0..total_bytes {
        // Mix repeated runs and gradients to exercise both PackBits replicate and literal runs
        let v = if i % 7 < 3 {
            seed[0]
        } else {
            seed[i % seed.len()]
        };
        raw_pixels.push(v);
    }

    let encoded_fragment =
        encode_rle_lossless_frame(&raw_pixels, rows, cols, spp, bits).unwrap();
    assert_eq!(encoded_fragment.len() % 2, 0);

    let mut obj = InMemDicomObject::new_empty();
    let study_uid = "1.2.826.0.1.3680043.10.5432.9100";
    let series_uid = "1.2.826.0.1.3680043.10.5432.9100.1";
    let sop_uid = "1.2.826.0.1.3680043.10.5432.9100.1.1";
    put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, study_uid);
    put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, series_uid);
    put_str(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, sop_uid);
    put_str(&mut obj, tags::SOP_CLASS_UID, VR::UI, uids::CT_IMAGE_STORAGE);
    put_str(&mut obj, tags::MODALITY, VR::CS, "OT");
    put_u16(&mut obj, tags::ROWS, rows as u16);
    put_u16(&mut obj, tags::COLUMNS, cols as u16);
    put_u16(&mut obj, tags::SAMPLES_PER_PIXEL, spp as u16);
    put_u16(&mut obj, tags::PLANAR_CONFIGURATION, 0);
    put_str(
        &mut obj,
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        if spp == 3 { "RGB" } else { "MONOCHROME2" },
    );
    put_u16(&mut obj, tags::BITS_ALLOCATED, bits as u16);
    put_u16(&mut obj, tags::BITS_STORED, bits as u16);
    put_u16(&mut obj, tags::HIGH_BIT, (bits - 1) as u16);
    put_u16(&mut obj, tags::PIXEL_REPRESENTATION, 0);
    obj.put_element(DataElement::new_with_len(
        tags::PIXEL_DATA,
        VR::OB,
        Length::UNDEFINED,
        PixelFragmentSequence::new(vec![], vec![encoded_fragment]),
    ));

    let file_obj = obj
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::RLE_LOSSLESS)
                .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(sop_uid),
        )
        .unwrap();
    let mut dcm_bytes = Vec::new();
    file_obj.write_all(&mut dcm_bytes).unwrap();

    let parsed = parse_dicom_instances(&[Bytes::from(dcm_bytes)]).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].frames.len(), 1);
    assert_eq!(parsed[0].frames[0].pixel_bytes, raw_pixels);
}

#[hegel::test(test_cases = 20)]
fn pbt_enhanced_multiframe_functional_groups_oblique_sorting(tc: TestCase) {
    let n_frames = tc.draw(gs::integers::<usize>().min_value(3).max_value(7));
    let dt = tc.draw(gs::floats::<f64>().min_value(0.5).max_value(6.0));
    let alpha = tc.draw(
        gs::floats::<f64>()
            .min_value(-1.0)
            .max_value(1.0),
    );
    let (sa, ca) = alpha.sin_cos();
    // Orthonormal oblique orientation
    let iop = [1.0, 0.0, 0.0, 0.0, ca, sa];
    let nx = 0.0;
    let ny = -sa;
    let nz = ca;

    let perm = tc.draw(gs::permutations((0..n_frames).collect::<Vec<_>>()));

    let study_uid = "1.2.826.0.1.3680043.10.5432.9200";
    let series_uid = "1.2.826.0.1.3680043.10.5432.9200.1";
    let sop_uid = "1.2.826.0.1.3680043.10.5432.9200.1.1";

    let mut obj = InMemDicomObject::new_empty();
    put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, study_uid);
    put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, series_uid);
    put_str(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, sop_uid);
    put_str(
        &mut obj,
        tags::SOP_CLASS_UID,
        VR::UI,
        uids::ENHANCED_CT_IMAGE_STORAGE,
    );
    put_str(&mut obj, tags::MODALITY, VR::CS, "CT");
    put_u16(&mut obj, tags::ROWS, 8);
    put_u16(&mut obj, tags::COLUMNS, 8);
    put_u16(&mut obj, tags::SAMPLES_PER_PIXEL, 1);
    put_str(&mut obj, tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
    put_u16(&mut obj, tags::BITS_ALLOCATED, 16);
    put_u16(&mut obj, tags::BITS_STORED, 12);
    put_u16(&mut obj, tags::HIGH_BIT, 11);
    put_u16(&mut obj, tags::PIXEL_REPRESENTATION, 0);
    put_str(&mut obj, tags::NUMBER_OF_FRAMES, VR::IS, &n_frames.to_string());

    let mut shared = InMemDicomObject::new_empty();
    let mut orient = InMemDicomObject::new_empty();
    put_str(
        &mut orient,
        tags::IMAGE_ORIENTATION_PATIENT,
        VR::DS,
        &format!("{:.6}\\{:.6}\\{:.6}\\{:.6}\\{:.6}\\{:.6}", iop[0], iop[1], iop[2], iop[3], iop[4], iop[5]),
    );
    shared.put_element(DataElement::new(
        tags::PLANE_ORIENTATION_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![orient], Length::UNDEFINED),
    ));
    let mut measures = InMemDicomObject::new_empty();
    put_str(&mut measures, tags::PIXEL_SPACING, VR::DS, "0.8\\0.8");
    put_str(&mut measures, tags::SLICE_THICKNESS, VR::DS, &format!("{dt:.4}"));
    shared.put_element(DataElement::new(
        tags::PIXEL_MEASURES_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![measures], Length::UNDEFINED),
    ));
    let mut xform = InMemDicomObject::new_empty();
    put_str(&mut xform, tags::RESCALE_INTERCEPT, VR::DS, "-1024");
    put_str(&mut xform, tags::RESCALE_SLOPE, VR::DS, "1");
    shared.put_element(DataElement::new(
        tags::PIXEL_VALUE_TRANSFORMATION_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![xform], Length::UNDEFINED),
    ));
    obj.put_element(DataElement::new(
        tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![shared], Length::UNDEFINED),
    ));

    let mut pf_items = Vec::with_capacity(n_frames);
    let mut raw_pixels = Vec::with_capacity(n_frames * 8 * 8 * 2);
    for &k in &perm {
        let t = k as f64 * dt;
        let ipp = [10.0 + t * nx, 20.0 + t * ny, 30.0 + t * nz];
        let mut fg = InMemDicomObject::new_empty();
        let mut ppos = InMemDicomObject::new_empty();
        put_str(
            &mut ppos,
            tags::IMAGE_POSITION_PATIENT,
            VR::DS,
            &format!("{:.6}\\{:.6}\\{:.6}", ipp[0], ipp[1], ipp[2]),
        );
        fg.put_element(DataElement::new(
            tags::PLANE_POSITION_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(vec![ppos], Length::UNDEFINED),
        ));
        let mut voi = InMemDicomObject::new_empty();
        put_str(&mut voi, tags::WINDOW_CENTER, VR::DS, &(-500 + (k as i32) * 10).to_string());
        put_str(&mut voi, tags::WINDOW_WIDTH, VR::DS, "1400");
        fg.put_element(DataElement::new(
            tags::FRAME_VOILUT_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(vec![voi], Length::UNDEFINED),
        ));
        pf_items.push(fg);

        let val = 500u16 + (k as u16) * 17;
        for _ in 0..64 {
            raw_pixels.extend_from_slice(&val.to_le_bytes());
        }
    }

    obj.put_element(DataElement::new(
        tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(pf_items, Length::UNDEFINED),
    ));
    obj.put_element(DataElement::new(
        tags::PIXEL_DATA,
        VR::OW,
        PrimitiveValue::from(raw_pixels),
    ));

    let file_obj = obj
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::ENHANCED_CT_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(sop_uid),
        )
        .unwrap();
    let mut dcm_bytes = Vec::new();
    file_obj.write_all(&mut dcm_bytes).unwrap();

    let parsed = parse_dicom_instances(&[Bytes::from(dcm_bytes)]).unwrap();
    assert_eq!(parsed.len(), 1);
    let ser = &parsed[0];
    assert!(ser.series.is_volumetric);
    assert!((ser.series.spacing_z_mm - dt).abs() < 1e-2);
    assert!((ser.series.spacing_x_mm - 0.8).abs() < 1e-6);
    assert_eq!(ser.frames.len(), n_frames);

    for (sorted_idx, frame) in ser.frames.iter().enumerate() {
        assert_eq!(frame.slice_sort_index, sorted_idx as i32);
        assert_eq!(frame.rescale_intercept, -1024.0);
        assert!((frame.window_center - (-500.0 + (sorted_idx as f64) * 10.0)).abs() < 1e-6);
        let expected_v = (500u16 + (sorted_idx as u16) * 17).to_le_bytes();
        assert_eq!(&frame.pixel_bytes[..2], &expected_v);
    }
}

#[hegel::test]
fn pbt_seg_bitpack_and_cielab_round_trip(tc: TestCase) {
    let rows = tc.draw(gs::integers::<usize>().min_value(1).max_value(19));
    let cols = tc.draw(gs::integers::<usize>().min_value(1).max_value(19));
    let n_frames = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));

    let total_bits = rows * cols * n_frames;
    let bools = tc.draw(gs::vecs(gs::booleans()).min_size(total_bits).max_size(total_bits));
    let mut packed = vec![0u8; total_bits.div_ceil(8)];
    for (idx, &bit) in bools.iter().enumerate() {
        if bit {
            packed[idx / 8] |= 1u8 << (idx % 8);
        }
    }

    let unpacked = unpack_bitpacked_frames(&packed, rows, cols, n_frames).unwrap();
    assert_eq!(unpacked.len(), n_frames);
    let frame_pixels = rows * cols;
    for (f_idx, frame) in unpacked.iter().enumerate() {
        assert_eq!(frame.len(), frame_pixels);
        for (px_idx, &px) in frame.iter().enumerate() {
            let expected = if bools[f_idx * frame_pixels + px_idx] { 255 } else { 0 };
            assert_eq!(px, expected);
        }
    }

    // Truncated buffer rejected cleanly
    if !packed.is_empty() {
        assert!(unpack_bitpacked_frames(&packed[..packed.len() - 1], rows, cols, n_frames).is_err());
    }

    let r = tc.draw(gs::integers::<u8>());
    let g = tc.draw(gs::integers::<u8>());
    let b = tc.draw(gs::integers::<u8>());
    let lab = rgb_to_dicom_cielab(r, g, b);
    let [r2, g2, b2] = dicom_cielab_to_rgb(lab[0], lab[1], lab[2]);
    assert!((i16::from(r) - i16::from(r2)).abs() <= 3);
    assert!((i16::from(g) - i16::from(g2)).abs() <= 3);
    assert!((i16::from(b) - i16::from(b2)).abs() <= 3);
}

#[hegel::test]
fn pbt_deid_uid_determinism_validity_and_salt_isolation(tc: TestCase) {
    let uid1 = tc.draw(gs::from_regex(r"1\.2\.[1-9][0-9]{0,7}(\.[1-9][0-9]{0,7}){2,4}"));
    let uid2 = tc.draw(gs::from_regex(r"1\.2\.[1-9][0-9]{0,7}(\.[1-9][0-9]{0,7}){2,4}"));
    tc.assume(uid1 != uid2);

    let salt1 = tc.draw(gs::from_regex(r"[A-Za-z0-9_-]{4,24}"));
    let salt2 = tc.draw(gs::from_regex(r"[A-Za-z0-9_-]{4,24}"));
    tc.assume(salt1 != salt2);

    let cfg1 = DeidConfig::enabled_with_salt(&salt1);
    let cfg2 = DeidConfig::enabled_with_salt(&salt2);

    let m1 = cfg1.remap_uid(&uid1);
    let m1_repeat = cfg1.remap_uid(&uid1);
    let m2 = cfg1.remap_uid(&uid2);
    let m1_other_salt = cfg2.remap_uid(&uid1);

    assert_eq!(m1, m1_repeat);
    assert_ne!(m1, uid1);
    assert_ne!(m1, m2);
    assert_ne!(m1, m1_other_salt);
    assert!(m1.starts_with("2.25."));
    assert!(m1.len() <= 64);
    assert!(m1.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
    assert!(m1
        .split('.')
        .all(|part| !part.is_empty() && (part == "0" || !part.starts_with('0'))));

    let year = tc.draw(gs::integers::<u16>().min_value(1950).max_value(2035));
    let month = tc.draw(gs::integers::<u8>().min_value(1).max_value(12));
    let day = tc.draw(gs::integers::<u8>().min_value(1).max_value(28));
    let raw_date = format!("{year:04}{month:02}{day:02}");
    assert_eq!(cfg1.generalize_date(&raw_date), format!("{year:04}0101"));

    let mut no_year = cfg1.clone();
    no_year.keep_study_date_year = false;
    assert_eq!(no_year.generalize_date(&raw_date), "");

    assert_eq!(cfg1.pseudonymize_accession("   "), "");
    assert!(cfg1.pseudonymize_accession("ACC-999").starts_with("ACC-"));
}

#[hegel::test]
fn pbt_deid_preserves_ct_seg_cross_references_and_strips_phi(tc: TestCase) {
    let patient_id = tc.draw(gs::from_regex(r"PID-[A-Z0-9]{4,10}"));
    let patient_name = tc.draw(gs::from_regex(r"[A-Z][a-z]{2,8}\^[A-Z][a-z]{2,8}"));
    let institution = tc.draw(gs::from_regex(r"Clinic [A-Z]{2,6}"));
    let physician = tc.draw(gs::from_regex(r"Dr [A-Z][a-z]{3,10}"));
    let salt = tc.draw(gs::from_regex(r"[A-Za-z0-9]{6,16}"));

    let study_uid = tc.draw(gs::from_regex(r"1\.2\.826\.[1-9][0-9]{2,6}\.[1-9][0-9]{1,4}"));
    let ct_series_uid = format!("{study_uid}.1");
    let ct_sop_uid = format!("{ct_series_uid}.1");
    let seg_series_uid = format!("{study_uid}.3");
    let seg_sop_uid = format!("{seg_series_uid}.1");

    let mut ct = InMemDicomObject::new_empty();
    put_str(&mut ct, tags::PATIENT_ID, VR::LO, &patient_id);
    put_str(&mut ct, tags::PATIENT_NAME, VR::PN, &patient_name);
    put_str(&mut ct, tags::INSTITUTION_NAME, VR::LO, &institution);
    put_str(&mut ct, tags::REFERRING_PHYSICIAN_NAME, VR::PN, &physician);
    put_str(&mut ct, tags::PATIENT_BIRTH_DATE, VR::DA, "19740519");
    put_str(&mut ct, tags::STUDY_INSTANCE_UID, VR::UI, &study_uid);
    put_str(&mut ct, tags::SERIES_INSTANCE_UID, VR::UI, &ct_series_uid);
    put_str(&mut ct, tags::SOP_CLASS_UID, VR::UI, uids::CT_IMAGE_STORAGE);
    put_str(&mut ct, tags::SOP_INSTANCE_UID, VR::UI, &ct_sop_uid);
    ct.put_element(DataElement::new(
        Tag(0x0019, 0x1001),
        VR::LO,
        PrimitiveValue::from("PRIVATE-PHI-TOKEN"),
    ));
    let mut ct_file = ct
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(&ct_sop_uid),
        )
        .unwrap();

    let mut seg = InMemDicomObject::new_empty();
    put_str(&mut seg, tags::PATIENT_ID, VR::LO, &patient_id);
    put_str(&mut seg, tags::PATIENT_NAME, VR::PN, &patient_name);
    put_str(&mut seg, tags::STUDY_INSTANCE_UID, VR::UI, &study_uid);
    put_str(&mut seg, tags::SERIES_INSTANCE_UID, VR::UI, &seg_series_uid);
    put_str(&mut seg, tags::SOP_CLASS_UID, VR::UI, uids::SEGMENTATION_STORAGE);
    put_str(&mut seg, tags::SOP_INSTANCE_UID, VR::UI, &seg_sop_uid);
    seg.put_element(DataElement::new(
        Tag(0x0029, 0x0010),
        VR::LO,
        PrimitiveValue::from("PRIVATE-VENDOR-ID"),
    ));
    let mut ref_inst = InMemDicomObject::new_empty();
    put_str(&mut ref_inst, tags::REFERENCED_SOP_CLASS_UID, VR::UI, uids::CT_IMAGE_STORAGE);
    put_str(&mut ref_inst, tags::REFERENCED_SOP_INSTANCE_UID, VR::UI, &ct_sop_uid);
    let mut ref_series = InMemDicomObject::new_empty();
    put_str(&mut ref_series, tags::SERIES_INSTANCE_UID, VR::UI, &ct_series_uid);
    ref_series.put_element(DataElement::new(
        tags::REFERENCED_INSTANCE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![ref_inst], Length::UNDEFINED),
    ));
    seg.put_element(DataElement::new(
        tags::REFERENCED_SERIES_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![ref_series], Length::UNDEFINED),
    ));
    let mut seg_file = seg
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::SEGMENTATION_STORAGE)
                .media_storage_sop_instance_uid(&seg_sop_uid),
        )
        .unwrap();

    let cfg = DeidConfig::enabled_with_salt(&salt);
    cfg.deidentify_file_object(&mut ct_file);
    cfg.deidentify_file_object(&mut seg_file);

    let new_ct_pid = ct_file.element(tags::PATIENT_ID).unwrap().to_str().unwrap();
    let new_seg_pid = seg_file.element(tags::PATIENT_ID).unwrap().to_str().unwrap();
    assert_eq!(new_ct_pid, new_seg_pid);
    assert_ne!(new_ct_pid.as_ref(), patient_id.as_str());
    assert!(ct_file.element_opt(tags::INSTITUTION_NAME).unwrap().is_none());
    assert!(ct_file.element_opt(tags::REFERRING_PHYSICIAN_NAME).unwrap().is_none());
    assert!(ct_file.element_opt(tags::PATIENT_BIRTH_DATE).unwrap().is_none());
    assert!(ct_file.element_opt(Tag(0x0019, 0x1001)).unwrap().is_none());
    assert!(seg_file.element_opt(Tag(0x0029, 0x0010)).unwrap().is_none());
    assert_eq!(
        ct_file.element(tags::PATIENT_IDENTITY_REMOVED).unwrap().to_str().unwrap().as_ref(),
        "YES"
    );
    assert_eq!(
        ct_file.element(tags::SOP_CLASS_UID).unwrap().to_str().unwrap().trim_end_matches('\0'),
        uids::CT_IMAGE_STORAGE
    );

    // Cross-references remain aligned between CT and SEG
    let ct_new_study = ct_file.element(tags::STUDY_INSTANCE_UID).unwrap().to_str().unwrap();
    let seg_new_study = seg_file.element(tags::STUDY_INSTANCE_UID).unwrap().to_str().unwrap();
    assert_eq!(ct_new_study, seg_new_study);
    assert_ne!(ct_new_study.as_ref(), study_uid.as_str());

    let ct_new_series = ct_file.element(tags::SERIES_INSTANCE_UID).unwrap().to_str().unwrap();
    let ct_new_sop = ct_file.element(tags::SOP_INSTANCE_UID).unwrap().to_str().unwrap();
    assert_eq!(
        seg_file.meta().media_storage_sop_instance_uid,
        seg_file.element(tags::SOP_INSTANCE_UID).unwrap().to_str().unwrap().as_ref()
    );

    let rs_items = seg_file.element(tags::REFERENCED_SERIES_SEQUENCE).unwrap().items().unwrap();
    let seg_ref_series = rs_items[0].element(tags::SERIES_INSTANCE_UID).unwrap().to_str().unwrap();
    assert_eq!(seg_ref_series, ct_new_series);
    let ri_items = rs_items[0].element(tags::REFERENCED_INSTANCE_SEQUENCE).unwrap().items().unwrap();
    let seg_ref_sop = ri_items[0].element(tags::REFERENCED_SOP_INSTANCE_UID).unwrap().to_str().unwrap();
    assert_eq!(seg_ref_sop, ct_new_sop);
}

#[hegel::test]
fn pbt_constant_time_token_eq_and_cors_origin_validation(tc: TestCase) {
    let tok1 = tc.draw(gs::text().min_size(1).max_size(64));
    let tok2 = tc.draw(gs::text().min_size(1).max_size(64));

    assert!(constant_time_token_eq(&tok1, &tok1));
    assert_eq!(constant_time_token_eq(&tok1, &tok2), tok1 == tok2);

    let port = tc.draw(gs::integers::<u16>().min_value(1).max_value(65535));
    let scheme = tc.draw(gs::sampled_from(&["http", "https"]));
    let host = tc.draw(gs::from_regex(r"[a-z]{3,10}\.example\.org"));
    let valid_origin = format!("{scheme}://{host}:{port}");
    assert_eq!(parse_cors_origins(&[valid_origin]).unwrap().len(), 1);

    let bad_origin = tc.draw(gs::sampled_from(&[
        "",
        "*",
        "ftp://viewer.example.org",
        "https://viewer.example.org/app",
        "https://user@viewer.example.org",
        "http://:8080",
        "https://bad host.org",
    ]));
    assert!(parse_cors_origins(&[bad_origin.to_string()]).is_err());
}

fn sample_ct_series_summary() -> &'static SeriesSummary {
    static SUMMARY: OnceLock<SeriesSummary> = OnceLock::new();
    SUMMARY.get_or_init(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = TempDir::new().unwrap();
            let store = DicomLanceStore::new(tmp.path());
            seed_phantoms_into_store(&store).await.unwrap();
            store
                .get_series_summary(CT_STUDY_UID, CT_SERIES_UID)
                .await
                .unwrap()
                .unwrap()
        })
    })
}

#[hegel::test]
fn pbt_tid1500_measurement_sr_round_trip_and_extraction(tc: TestCase) {
    let base = sample_ct_series_summary();
    let suffix = tc.draw(gs::from_regex(r"[A-Za-z0-9]{4,12}"));
    let finding = tc.draw(gs::from_regex(r"[A-Za-z0-9][A-Za-z0-9 ]{4,24}[A-Za-z0-9]"));
    let length_mm = tc.draw(gs::floats::<f64>().min_value(1.5).max_value(180.0));
    let long_axis_mm = tc.draw(gs::floats::<f64>().min_value(2.0).max_value(120.0));
    let short_axis_mm = tc.draw(gs::floats::<f64>().min_value(1.0).max_value(80.0));
    let area_mm2 = tc.draw(gs::floats::<f64>().min_value(4.0).max_value(900.0));
    let mean_hu = tc.draw(gs::floats::<f64>().min_value(-900.0).max_value(1200.0));
    let std_dev_hu = tc.draw(gs::floats::<f64>().min_value(0.5).max_value(200.0));
    let slice_z = tc.draw(gs::floats::<f64>().min_value(-250.0).max_value(250.0));

    let mut parent = base.clone();
    let study_uid = format!("1.2.826.0.1.3680043.10.5432.{}", tc.draw(gs::integers::<u32>().min_value(1000).max_value(999999)));
    let series_uid = format!("1.2.826.0.1.3680043.10.5432.{}", tc.draw(gs::integers::<u32>().min_value(1000000).max_value(9999999)));
    let ref_sop = format!("1.2.826.0.1.3680043.10.5432.{}", tc.draw(gs::integers::<u32>().min_value(10000000).max_value(99999999)));
    parent.study_instance_uid = study_uid.clone();
    parent.series_instance_uid = series_uid.clone();
    parent.patient_id = format!("PAT-{suffix}");

    // Validation rejects empty measurements and non-finite metrics
    assert!(build_tid1500_measurement_sr(
        &parent,
        &MeasurementSrRequest {
            series_description: None,
            findings_note: None,
            measurements: vec![],
            deidentify: None,
        },
    )
    .is_err());
    assert!(build_tid1500_measurement_sr(
        &parent,
        &MeasurementSrRequest {
            series_description: None,
            findings_note: None,
            measurements: vec![MeasurementInput {
                tool: MeasurementToolKind::Length,
                label: "Invalid".to_string(),
                sop_instance_uid: Some(ref_sop.clone()),
                frame_number: Some(1),
                slice_z_mm: Some(slice_z),
                length_mm: Some(f64::NAN),
                width_mm: None,
                area_mm2: None,
                mean_hu: None,
                std_dev_hu: None,
                unit: None,
                points_world: vec![],
            }],
            deidentify: None,
        },
    )
    .is_err());

    let l_label = format!("LesionLen-{suffix}");
    let b_label = format!("Recist-{suffix}");
    let r_label = format!("NoduleRoi-{suffix}");
    let sr_desc = format!("TID1500-{suffix}");

    let generated = build_tid1500_measurement_sr(
        &parent,
        &MeasurementSrRequest {
            series_description: Some(sr_desc.clone()),
            findings_note: Some(finding.clone()),
            measurements: vec![
                MeasurementInput {
                    tool: MeasurementToolKind::Length,
                    label: l_label.clone(),
                    sop_instance_uid: Some(ref_sop.clone()),
                    frame_number: Some(1),
                    slice_z_mm: Some(slice_z),
                    length_mm: Some(length_mm),
                    width_mm: None,
                    area_mm2: None,
                    mean_hu: None,
                    std_dev_hu: None,
                    unit: Some("mm".to_string()),
                    points_world: vec![[-10.0, 0.0, slice_z], [10.0, 0.0, slice_z]],
                },
                MeasurementInput {
                    tool: MeasurementToolKind::Bidirectional,
                    label: b_label.clone(),
                    sop_instance_uid: Some(ref_sop.clone()),
                    frame_number: Some(1),
                    slice_z_mm: Some(slice_z),
                    length_mm: Some(long_axis_mm),
                    width_mm: Some(short_axis_mm),
                    area_mm2: None,
                    mean_hu: None,
                    std_dev_hu: None,
                    unit: Some("mm".to_string()),
                    points_world: vec![],
                },
                MeasurementInput {
                    tool: MeasurementToolKind::EllipticalROI,
                    label: r_label.clone(),
                    sop_instance_uid: Some(ref_sop.clone()),
                    frame_number: Some(1),
                    slice_z_mm: Some(slice_z),
                    length_mm: None,
                    width_mm: None,
                    area_mm2: Some(area_mm2),
                    mean_hu: Some(mean_hu),
                    std_dev_hu: Some(std_dev_hu),
                    unit: Some("mm2".to_string()),
                    points_world: vec![],
                },
            ],
            deidentify: None,
        },
    )
    .unwrap();

    assert_eq!(generated.study_instance_uid, study_uid);
    assert_ne!(generated.series_instance_uid, series_uid);
    assert!(generated.series_instance_uid.starts_with("2.25."));
    assert!(generated.sop_instance_uid.starts_with("2.25."));
    assert!(generated.series_instance_uid.len() <= 64);
    assert!(generated.sop_instance_uid.len() <= 64);

    let sr_obj = dicom_object::from_reader(Cursor::new(&generated.dcm_bytes)).unwrap();
    assert_eq!(
        sr_obj.element(tags::SOP_CLASS_UID).unwrap().to_str().unwrap().trim_end_matches('\0'),
        uids::COMPREHENSIVE_SR_STORAGE
    );
    assert_eq!(
        sr_obj.element(tags::SERIES_DESCRIPTION).unwrap().to_str().unwrap().trim_end_matches('\0'),
        sr_desc
    );

    let root_concept = sr_obj
        .element(tags::CONCEPT_NAME_CODE_SEQUENCE)
        .unwrap()
        .items()
        .unwrap()[0]
        .element(tags::CODE_MEANING)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(root_concept.trim_end_matches('\0'), "Imaging Measurement Report");

    let parsed = parse_dicom_instances(&[Bytes::from(generated.dcm_bytes)]).unwrap();
    assert_eq!(parsed.len(), 1);
    let rec = &parsed[0].series;
    assert_eq!(rec.modality, "SR");
    assert_eq!(rec.study_instance_uid, study_uid);
    assert_eq!(rec.series_instance_uid, generated.series_instance_uid);
    assert!(rec.searchable_text.contains(&sr_desc));
    assert!(rec.sr_text.contains(finding.trim()));
    assert!(rec.sr_text.contains(&l_label));
    assert!(rec.sr_text.contains(&b_label));
    assert!(rec.sr_text.contains(&r_label));
    assert!(rec.sr_text.contains("Length"));
    assert!(rec.sr_text.contains("Long Axis"));
    assert!(rec.sr_text.contains("Short Axis"));
    assert!(rec.sr_text.contains("Area"));
    assert!(rec.sr_text.contains("Mean"));
    assert!(rec.sr_text.contains("Standard Deviation"));
}
