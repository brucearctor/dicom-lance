use std::io::Cursor;

use axum::http::header::RANGE;
use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use dicom_core::value::DataSetSequence;
use dicom_core::{DataElement, Length, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_lance::api::{
    encode_multipart_related, extract_boundary, parse_frame_numbers, parse_range_header,
    split_multipart_related,
};
use dicom_lance::dicom_ingest::{
    extract_sr_text, parse_dicom_instances, render_thumbnail_png, slice_normal_projection,
};
use dicom_lance::embedding::{rrf_fuse, Embedder, LocalMultimodalEmbedder};
use dicom_lance::schema::{FrameRecord, SeriesRecord, EMBEDDING_DIM};
use dicom_lance::store::{parse_storage_root, validate_sql_literal, DicomLanceStore};
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
