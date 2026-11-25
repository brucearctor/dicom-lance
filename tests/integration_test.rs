use std::io::Read;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use dicom_core::value::{DataSetSequence, PixelFragmentSequence};
use dicom_core::{DataElement, Length, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_lance::api::{
    build_router, build_router_with_config, seed_phantoms_into_store,
    seed_phantoms_into_store_with_deid, ServerConfig,
};
use dicom_lance::codecs::{
    encode_rle_lossless_frame, TS_DEFLATED_IMAGE_FRAME, TS_EXPLICIT_VR_LE, TS_RLE_LOSSLESS,
};
use dicom_lance::deid::DeidConfig;
use dicom_lance::embedding::{Embedder, HttpEmbedder};
use dicom_lance::phantom::{
    generate_phantom_dicom_files, CT_DIM, CT_SEG_FRAMES, CT_SEG_SERIES_UID, CT_SERIES_UID,
    CT_SLICES, CT_STUDY_UID, MR_SLICES, MR_STUDY_UID,
};
use dicom_lance::schema::EMBEDDING_DIM;
use dicom_lance::seg::{rgb_to_dicom_cielab, SEGMENTATION_STORAGE_UID};
use dicom_lance::store::{
    parse_storage_root, DicomLanceStore, SearchMode, StoreStats,
};
use dicom_object::{from_reader, FileMetaTableBuilder, InMemDicomObject};
use dicom_pixeldata::Transcode;
use dicom_transfer_syntax_registry::entries::{
    DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN, DEFLATED_IMAGE_FRAME_COMPRESSION, JPEG_BASELINE,
    JPEG_XL_LOSSLESS,
};
use flate2::read::DeflateDecoder;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

fn extract_multipart_part(body: &[u8], boundary: &str) -> Vec<u8> {
    let delim = format!("--{boundary}").into_bytes();
    let start = body
        .windows(delim.len())
        .position(|w| w == delim.as_slice())
        .expect("opening boundary")
        + delim.len();
    let after_crlf = if &body[start..start + 2] == b"\r\n" {
        start + 2
    } else {
        start
    };
    let hdr_end = body[after_crlf..]
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("part header terminator")
        + after_crlf
        + 4;
    let end_delim = format!("\r\n--{boundary}").into_bytes();
    let payload_end = body[hdr_end..]
        .windows(end_delim.len())
        .position(|w| w == end_delim.as_slice())
        .expect("closing boundary")
        + hdr_end;
    body[hdr_end..payload_end].to_vec()
}

#[tokio::test]
async fn end_to_end_dicom_lance_v2_2_stow_search_and_wado_rs() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let app = build_router(store.clone(), None);

    // 1. Generate real Part-10 .dcm files on disk and ingest via STOW-RS multipart/related
    let dcm_dir = tmp.path().join("raw_phantoms");
    let dcm_paths = generate_phantom_dicom_files(&dcm_dir).unwrap();
    assert_eq!(dcm_paths.len(), CT_SLICES + 2 + MR_SLICES);

    let stow_boundary = "test-stow-boundary";
    let mut stow_body = Vec::new();
    for p in &dcm_paths {
        let bytes = std::fs::read(p).unwrap();
        stow_body.extend_from_slice(
            format!("--{stow_boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
        );
        stow_body.extend_from_slice(&bytes);
        stow_body.extend_from_slice(b"\r\n");
    }
    stow_body.extend_from_slice(format!("--{stow_boundary}--\r\n").as_bytes());

    let stow_req = Request::builder()
        .method("POST")
        .uri("/dicomweb/studies")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/related; type=\"application/dicom\"; boundary=\"{stow_boundary}\""),
        )
        .body(Body::from(stow_body))
        .unwrap();
    let stow_res = app.clone().oneshot(stow_req).await.unwrap();
    assert_eq!(stow_res.status(), StatusCode::OK);
    let stow_json: serde_json::Value =
        serde_json::from_slice(&stow_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stow_json["series_ingested"], 4);
    assert_eq!(stow_json["frames_ingested"], CT_SLICES + MR_SLICES + CT_SEG_FRAMES);

    // 2. Verify Lance v2.2 telemetry, FTS index, and Packed Blob V2 storage
    let stats_res = app
        .clone()
        .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(stats_res.status(), StatusCode::OK);
    let stats: StoreStats =
        serde_json::from_slice(&stats_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stats.series_dataset.row_count, 4);
    assert_eq!(stats.frames_dataset.row_count, CT_SLICES + MR_SLICES + CT_SEG_FRAMES);
    assert_eq!(stats.frames_dataset.storage_version, "2.2");
    assert_eq!(stats.blob_v2_kind_sample.as_deref(), Some("Packed"));
    assert!(
        stats
            .series_dataset
            .indexes
            .iter()
            .any(|n| n == "searchable_text_fts"),
        "expected BM25 FTS index on searchable_text, got {:?}",
        stats.series_dataset.indexes
    );

    // 3. Verify BM25 FTS queries and SQL modality prefiltering
    let ct_search_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/search?q=spiculated%20right%20upper%20lobe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ct_search_res.status(), StatusCode::OK);
    let ct_search: serde_json::Value = serde_json::from_slice(
        &ct_search_res.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    let studies = ct_search["studies"].as_array().unwrap();
    assert_eq!(studies.len(), 1);
    assert_eq!(studies[0]["patient_name"], "Rivera^Elena");
    assert!(studies[0]["max_score"].as_f64().unwrap() > 0.0);

    let mr_search_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/search?q=ring-enhancing%20frontal&modality=MR")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mr_search_res.status(), StatusCode::OK);
    let mr_search: serde_json::Value = serde_json::from_slice(
        &mr_search_res.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    let mr_studies = mr_search["studies"].as_array().unwrap();
    assert_eq!(mr_studies.len(), 1);
    assert_eq!(mr_studies[0]["patient_name"], "Chen^Marcus");

    // 4. Verify WADO-RS metadata projects JSON headers (00020010 present, 7FE00010 stripped)
    let meta_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/metadata"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(meta_res.status(), StatusCode::OK);
    let meta_json: Vec<serde_json::Value> =
        serde_json::from_slice(&meta_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(meta_json.len(), CT_SLICES);
    assert_eq!(meta_json[0]["00020010"]["Value"][0], "1.2.840.10008.1.2.1");
    assert!(meta_json[0].get("7FE00010").is_none());

    // 5. Verify WADO-RS frame retrieval via take_blobs & exact 16-bit HU values on slice 23 (z_idx=22)
    let nodule_sop = format!("{CT_SERIES_UID}.23");
    let frame_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(frame_res.status(), StatusCode::OK);
    let ct_header = frame_res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct_header.contains("multipart/related"));
    let frame_body = frame_res.into_body().collect().await.unwrap().to_bytes();
    let raw_pixels = extract_multipart_part(&frame_body, "dicom-lance-boundary");
    assert_eq!(raw_pixels.len(), CT_DIM * CT_DIM * 2);

    let read_hu = |row: usize, col: usize| -> i32 {
        let idx = (row * CT_DIM + col) * 2;
        let stored = u16::from_le_bytes([raw_pixels[idx], raw_pixels[idx + 1]]) as i32;
        stored - 1024
    };
    // RUL spiculated nodule core at (-56, -12, -25) -> row 86, col 51
    assert_eq!(read_hu(86, 51), 38);
    // Normal aerated right lung at (-40, 0, -25) -> row 96, col 64
    assert_eq!(read_hu(96, 64), -780);

    // 6. Verify HTTP Range read via BlobFile::read_range
    let range_res = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1"
                ))
                .header(header::RANGE, "bytes=0-1023")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range_res.status(), StatusCode::OK);
    let range_body = range_res.into_body().collect().await.unwrap().to_bytes();
    let range_part = extract_multipart_part(&range_body, "dicom-lance-boundary");
    assert_eq!(range_part.len(), 1024);
    assert_eq!(range_part, &raw_pixels[..1024]);
}

#[tokio::test]
async fn api_error_paths_and_malformed_inputs() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let app = build_router(store.clone(), None);

    // 1. Empty STOW-RS body -> 400
    let empty_stow = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dicomweb/studies")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty_stow.status(), StatusCode::BAD_REQUEST);

    // 2. Corrupt non-DICOM upload -> 400
    let corrupt_stow = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dicomweb/studies")
                .header(header::CONTENT_TYPE, "application/dicom")
                .body(Body::from(&b"not-a-valid-dicom-part10-stream"[..]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(corrupt_stow.status(), StatusCode::BAD_REQUEST);

    // 3. Seed one study so subsequent negative/range checks hit populated datasets
    let dcm_dir = tmp.path().join("raw_phantoms");
    generate_phantom_dicom_files(&dcm_dir).unwrap();
    let dir_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/directory")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "path": dcm_dir.to_string_lossy() }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dir_res.status(), StatusCode::OK);

    let nodule_sop = format!("{CT_SERIES_UID}.23");

    // 4. Invalid frame lists -> 400
    for bad_frames in ["1,bogus", "0", "-1", "1,,2"] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/{bad_frames}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for frames/{bad_frames}"
        );
    }

    // 5. Unknown study/series/SOP -> 404
    let miss_meta = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dicomweb/studies/1.2.3.404/series/1.2.3.404.1/metadata")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(miss_meta.status(), StatusCode::NOT_FOUND);

    let miss_frame = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dicomweb/studies/1.2.3.404/series/1.2.3.404.1/instances/1.2.3.404.1.1/frames/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(miss_frame.status(), StatusCode::NOT_FOUND);

    let miss_thumb = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/series/1.2.3.404/1.2.3.404.1/thumbnail")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(miss_thumb.status(), StatusCode::NOT_FOUND);

    // 6. SQL-injection attempt in UID path -> 400
    let sqli_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/series/1.2'OR'1'='1/1.2.3/thumbnail")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sqli_res.status(), StatusCode::BAD_REQUEST);

    // 7. u64::MAX Range must not panic and falls back cleanly to full frame; open-ended Range returns exact suffix
    let full_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let full_bytes = extract_multipart_part(
        &full_res.into_body().collect().await.unwrap().to_bytes(),
        "dicom-lance-boundary",
    );
    assert_eq!(full_bytes.len(), CT_DIM * CT_DIM * 2);

    let umax_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1"
                ))
                .header(header::RANGE, format!("bytes=0-{}", u64::MAX))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(umax_res.status(), StatusCode::OK);
    let umax_bytes = extract_multipart_part(
        &umax_res.into_body().collect().await.unwrap().to_bytes(),
        "dicom-lance-boundary",
    );
    assert_eq!(umax_bytes, full_bytes);

    let open_res = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1"
                ))
                .header(header::RANGE, "bytes=1024-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(open_res.status(), StatusCode::OK);
    let open_bytes = extract_multipart_part(
        &open_res.into_body().collect().await.unwrap().to_bytes(),
        "dicom-lance-boundary",
    );
    assert_eq!(open_bytes, &full_bytes[1024..]);
}

#[tokio::test]
async fn qido_rs_upload_directory_ingest_and_duplicate_frames() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let app = build_router(store.clone(), None);

    // 1. Directory ingest error cases
    let bad_dir = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/directory")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "path": tmp.path().join("does-not-exist").to_string_lossy() })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_dir.status(), StatusCode::BAD_REQUEST);

    let empty_dir = tmp.path().join("empty_dir");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let empty_dir_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/directory")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "path": empty_dir.to_string_lossy() }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty_dir_res.status(), StatusCode::BAD_REQUEST);

    // 2. Upload via POST /api/ingest/upload (multipart/form-data)
    let dcm_dir = tmp.path().join("phantoms");
    let dcm_paths = generate_phantom_dicom_files(&dcm_dir).unwrap();
    let boundary = "form-upload-boundary";
    let mut form_body = Vec::new();
    for p in dcm_paths.iter().take(5) {
        let filename = p.file_name().unwrap().to_string_lossy();
        let bytes = std::fs::read(p).unwrap();
        form_body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{filename}\"\r\nContent-Type: application/dicom\r\n\r\n"
            )
            .as_bytes(),
        );
        form_body.extend_from_slice(&bytes);
        form_body.extend_from_slice(b"\r\n");
    }
    form_body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let upload_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/upload")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(form_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload_res.status(), StatusCode::OK);
    let upload_json: serde_json::Value =
        serde_json::from_slice(&upload_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(upload_json["series_ingested"], 1);
    assert_eq!(upload_json["frames_ingested"], 5);

    // 3. Ingest all phantoms via POST /api/ingest/directory
    let dir_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/directory")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "path": dcm_dir.to_string_lossy() }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dir_res.status(), StatusCode::OK);

    // 4. QIDO-RS studies & series endpoints
    let qido_studies_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dicomweb/studies?q=nodule&modality=CT")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(qido_studies_res.status(), StatusCode::OK);
    let qido_studies: Vec<serde_json::Value> = serde_json::from_slice(
        &qido_studies_res.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    assert_eq!(qido_studies.len(), 1);
    assert_eq!(qido_studies[0]["0020000D"]["Value"][0], CT_STUDY_UID);
    assert_eq!(
        qido_studies[0]["00100010"]["Value"][0]["Alphabetic"],
        "Rivera^Elena"
    );
    assert!(qido_studies[0]["00080061"]["Value"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "CT"));

    let qido_series_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/dicomweb/studies/{CT_STUDY_UID}/series"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(qido_series_res.status(), StatusCode::OK);
    let qido_series: Vec<serde_json::Value> = serde_json::from_slice(
        &qido_series_res.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    assert_eq!(qido_series.len(), 3);
    assert_eq!(qido_series[0]["0020000E"]["Value"][0], CT_SERIES_UID);

    // 5. Series PNG thumbnail endpoint
    let thumb_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/series/{CT_STUDY_UID}/{CT_SERIES_UID}/thumbnail"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(thumb_res.status(), StatusCode::OK);
    assert_eq!(
        thumb_res.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    let thumb_bytes = thumb_res.into_body().collect().await.unwrap().to_bytes();
    let decoder = png::Decoder::new(std::io::Cursor::new(&thumb_bytes));
    let reader = decoder.read_info().unwrap();
    assert_eq!((reader.info().width, reader.info().height), (128, 128));

    // 6. Duplicate frame request /frames/1,1 returns 2 identical multipart parts
    let nodule_sop = format!("{CT_SERIES_UID}.23");
    let dup_res = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1,1"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dup_res.status(), StatusCode::OK);
    let dup_body = dup_res.into_body().collect().await.unwrap().to_bytes();
    let parts = dicom_lance::api::split_multipart_related(&dup_body, "dicom-lance-boundary");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], parts[1]);
    assert_eq!(parts[0].len(), CT_DIM * CT_DIM * 2);
}

#[tokio::test]
async fn hybrid_vector_search_similar_slices_and_dataset_cache() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let report = seed_phantoms_into_store(&store).await.unwrap();
    assert_eq!(report.series_ingested, 4);
    assert_eq!(report.frames_ingested, CT_SLICES + MR_SLICES + CT_SEG_FRAMES);

    let app = build_router(store.clone(), None);

    // 1. Subword vector recall: "nodular" has zero exact BM25 token match for "nodule",
    // yet 768-d multimodal char-4gram embeddings recall Chest CT / Lung-RADS SR.
    for mode in ["vector", "hybrid"] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/search?q=nodular&mode={mode}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["mode"], mode);
        let studies = body["studies"].as_array().unwrap();
        assert!(
            !studies.is_empty(),
            "expected non-empty {mode} results for subword query"
        );
        assert_eq!(studies[0]["patient_name"], "Rivera^Elena");
    }

    // Explicit BM25 mode returns only BM25 keyword matches
    let bm25_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/search?q=spiculated&mode=bm25")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bm25_res.status(), StatusCode::OK);
    let bm25_body: serde_json::Value =
        serde_json::from_slice(&bm25_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(bm25_body["mode"], "bm25");
    assert_eq!(bm25_body["studies"][0]["patient_name"], "Rivera^Elena");

    // 2. Find Similar Slices via 768-d Cosine KNN on Chest CT nodule slice 23
    let nodule_sop = format!("{CT_SERIES_UID}.23");
    let sim_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/instances/{nodule_sop}/frames/1/similar?limit=5"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sim_res.status(), StatusCode::OK);
    let sim_json: serde_json::Value =
        serde_json::from_slice(&sim_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let hits = sim_json["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 5);
    for hit in hits {
        assert_eq!(hit["series_instance_uid"], CT_SERIES_UID);
        assert_ne!(hit["sop_instance_uid"], nodule_sop);
        let sim = hit["similarity"].as_f64().unwrap();
        assert!(sim > 0.85, "expected high intra-series cosine similarity, got {sim}");
        let inst = hit["instance_number"].as_i64().unwrap();
        assert!(
            (inst - 23).abs() <= 6,
            "expected close axial neighbor near slice 23, got slice {inst}"
        );
    }

    // 3. Verify dataset handle caching increments hits and refreshes atomically after ingest
    let (s_before, f_before) = store.cache_hits();
    let _ = store.get_series_wado_metadata(CT_STUDY_UID, CT_SERIES_UID).await.unwrap();
    let _ = store
        .read_frame_blobs(CT_STUDY_UID, CT_SERIES_UID, &nodule_sop, &[1], None)
        .await
        .unwrap();
    let _ = store.get_stats().await.unwrap();
    let (s_after, f_after) = store.cache_hits();
    assert!(s_after > s_before, "expected series cache hit increment");
    assert!(f_after > f_before, "expected frames cache hit increment");

    let stats_v1 = store.get_stats().await.unwrap();
    assert_eq!(stats_v1.embedder, "local-multimodal-768");
    let _ = seed_phantoms_into_store(&store).await.unwrap();
    let stats_v2 = store.get_stats().await.unwrap();
    assert!(stats_v2.series_dataset.version > stats_v1.series_dataset.version);
    assert!(stats_v2.frames_dataset.version > stats_v1.frames_dataset.version);

    store.invalidate_cache().await;
    let stats_reopened = store.get_stats().await.unwrap();
    assert_eq!(stats_reopened.series_dataset.version, stats_v2.series_dataset.version);

    let inv_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/cache/invalidate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inv_res.status(), StatusCode::OK);
    let inv_stats: StoreStats =
        serde_json::from_slice(&inv_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(inv_stats.series_dataset.version, stats_v2.series_dataset.version);

    // 4. Verify HttpEmbedder against an ephemeral OpenAI-compatible mock endpoint
    let mock_router = Router::new().route(
        "/v1/embeddings",
        post(|headers: axum::http::HeaderMap, Json(payload): Json<serde_json::Value>| async move {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert_eq!(auth, "Bearer test-api-key");
            assert_eq!(payload["model"], "biomed-clip-test");
            let mut vec = vec![0.0_f32; 64];
            vec[0] = 3.0;
            vec[1] = 4.0;
            Json(serde_json::json!({
                "data": [{ "embedding": vec }]
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn(async move {
        axum::serve(listener, mock_router).await.unwrap();
    });

    let http_embedder = HttpEmbedder::new(
        format!("http://{addr}/v1/embeddings"),
        Some("test-api-key".to_string()),
        "biomed-clip-test".to_string(),
    );
    let emb = http_embedder.embed_text("pulmonary nodule").await.unwrap();
    assert_eq!(emb.len(), EMBEDDING_DIM as usize);
    let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4);
    assert!((emb[0] - 0.6).abs() < 1e-4);
    assert!((emb[1] - 0.8).abs() < 1e-4);
    srv_handle.abort();
}

#[tokio::test]
async fn remote_shared_memory_uri_end_to_end() {
    if cfg!(not(feature = "cloud-s3")) {
        let s3_err = parse_storage_root("s3://clinical-dicom-bucket/archive").unwrap_err();
        assert!(
            s3_err.to_string().contains("cloud-s3"),
            "expected helpful cloud-s3 build hint, got: {s3_err}"
        );
    }

    let remote_uri = "shared-memory://dicom-lance-integration/v2-suite";
    let store = Arc::new(DicomLanceStore::from_uri(remote_uri).unwrap());
    assert!(store.local_root_dir().is_none());
    assert!(store.storage_root().is_remote());

    let report = seed_phantoms_into_store(&store).await.unwrap();
    assert_eq!(report.series_ingested, 4);
    assert_eq!(report.frames_ingested, CT_SLICES + MR_SLICES + CT_SEG_FRAMES);

    let stats = store.get_stats().await.unwrap();
    assert_eq!(stats.storage_uri, remote_uri);
    assert_eq!(stats.frames_dataset.storage_version, "2.2");
    assert_eq!(stats.blob_v2_kind_sample.as_deref(), Some("Packed"));

    let hybrid = store
        .search_series_with_mode(Some("ring-enhancing"), None, None, 10, SearchMode::Hybrid)
        .await
        .unwrap();
    assert_eq!(hybrid.len(), 1);
    assert_eq!(hybrid[0].patient_name, "Chen^Marcus");

    let nodule_sop = format!("{CT_SERIES_UID}.23");
    let blobs = store
        .read_frame_blobs(CT_STUDY_UID, CT_SERIES_UID, &nodule_sop, &[1], None)
        .await
        .unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0].bytes.len(), CT_DIM * CT_DIM * 2);

    let sim_hits = store
        .find_similar_frames(CT_STUDY_UID, CT_SERIES_UID, &nodule_sop, 1, 4, true)
        .await
        .unwrap();
    assert_eq!(sim_hits.len(), 4);
    assert!(sim_hits.iter().all(|h| h.series_instance_uid == CT_SERIES_UID));
}

fn test_put_str(obj: &mut InMemDicomObject, tag: Tag, vr: VR, val: &str) {
    obj.put_element(DataElement::new(tag, vr, PrimitiveValue::from(val)));
}

fn test_put_u16(obj: &mut InMemDicomObject, tag: Tag, val: u16) {
    obj.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(val)));
}

fn build_uncompressed_test_obj(
    study_uid: &str,
    series_uid: &str,
    sop_uid: &str,
    rows: u16,
    cols: u16,
    bits: u16,
    pixels: Vec<u8>,
) -> dicom_object::DefaultDicomObject {
    let mut obj = InMemDicomObject::new_empty();
    test_put_str(&mut obj, tags::PATIENT_ID, VR::LO, "PAT-CODEC-01");
    test_put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "Codec^Tester");
    test_put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, study_uid);
    test_put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, series_uid);
    test_put_str(&mut obj, tags::SOP_CLASS_UID, VR::UI, uids::CT_IMAGE_STORAGE);
    test_put_str(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, sop_uid);
    test_put_str(&mut obj, tags::MODALITY, VR::CS, "OT");
    test_put_str(&mut obj, tags::SERIES_DESCRIPTION, VR::LO, "Codec Series");
    test_put_str(&mut obj, tags::INSTANCE_NUMBER, VR::IS, "1");
    test_put_str(&mut obj, tags::SERIES_NUMBER, VR::IS, "1");
    test_put_u16(&mut obj, tags::ROWS, rows);
    test_put_u16(&mut obj, tags::COLUMNS, cols);
    test_put_u16(&mut obj, tags::SAMPLES_PER_PIXEL, 1);
    test_put_str(&mut obj, tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
    test_put_u16(&mut obj, tags::BITS_ALLOCATED, bits);
    test_put_u16(&mut obj, tags::BITS_STORED, bits);
    test_put_u16(&mut obj, tags::HIGH_BIT, bits - 1);
    test_put_u16(&mut obj, tags::PIXEL_REPRESENTATION, 0);
    test_put_str(&mut obj, tags::PIXEL_SPACING, VR::DS, "1.0\\1.0");
    test_put_str(&mut obj, tags::SLICE_THICKNESS, VR::DS, "1.0");
    test_put_str(&mut obj, tags::IMAGE_POSITION_PATIENT, VR::DS, "0\\0\\0");
    test_put_str(&mut obj, tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\0\\1\\0");
    obj.put_element(DataElement::new(
        tags::PIXEL_DATA,
        if bits == 8 { VR::OB } else { VR::OW },
        PrimitiveValue::from(pixels),
    ));
    obj.with_meta(
        FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
            .media_storage_sop_instance_uid(sop_uid),
    )
    .unwrap()
}

// Lossless 8x8 grayscale JPEG 2000 codestream (pixels 0..64)
const J2K_LOSSLESS_8X8_RAMP: &[u8] = &[
    0xff, 0x4f, 0xff, 0x51, 0x00, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x01, 0xff, 0x52, 0x00,
    0x0c, 0x00, 0x00, 0x00, 0x01, 0x00, 0x03, 0x04, 0x04, 0x00, 0x01, 0xff, 0x5c, 0x00, 0x0d, 0x40,
    0x40, 0x48, 0x48, 0x50, 0x48, 0x48, 0x50, 0x48, 0x48, 0x50, 0xff, 0x64, 0x00, 0x25, 0x00, 0x01,
    0x43, 0x72, 0x65, 0x61, 0x74, 0x65, 0x64, 0x20, 0x62, 0x79, 0x20, 0x4f, 0x70, 0x65, 0x6e, 0x4a,
    0x50, 0x45, 0x47, 0x20, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6f, 0x6e, 0x20, 0x32, 0x2e, 0x35, 0x2e,
    0x34, 0xff, 0x90, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0x00, 0x01, 0xff, 0x93, 0xcf,
    0xb4, 0x08, 0x08, 0x3f, 0xc0, 0x7c, 0x20, 0xc3, 0xea, 0x02, 0x00, 0x03, 0x04, 0x3f, 0xc0, 0x3a,
    0x1c, 0x1f, 0x38, 0x30, 0x0c, 0xff, 0x7f, 0x0b, 0x3d, 0x87, 0xc0, 0x11, 0x40, 0xf9, 0x01, 0x80,
    0x22, 0x17, 0x03, 0x7f, 0xe7, 0xff, 0xd9,
];

#[tokio::test]
async fn compressed_transfer_syntaxes_and_wado_accept_negotiation() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let app = build_router(store.clone(), None);

    let study_uid = "1.2.826.0.1.3680043.10.5432.7700";

    // Build 16x16 16-bit lossless test pattern (256 pixels = 512 bytes)
    let mut raw_16 = Vec::with_capacity(16 * 16 * 2);
    for i in 0u16..(16 * 16) {
        let v = 300 + (i * 11) % 900;
        raw_16.extend_from_slice(&v.to_le_bytes());
    }

    // 1. Deflated Explicit VR Little Endian (1.2.840.10008.1.2.1.99)
    let mut deflated_ds_obj = build_uncompressed_test_obj(
        study_uid,
        &format!("{study_uid}.1"),
        &format!("{study_uid}.1.1"),
        16,
        16,
        16,
        raw_16.clone(),
    );
    deflated_ds_obj
        .transcode(&DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.erased())
        .unwrap();
    let mut deflated_ds_bytes = Vec::new();
    deflated_ds_obj.write_all(&mut deflated_ds_bytes).unwrap();

    // 2. Deflated Image Frame Compression (1.2.840.10008.1.2.8.1)
    let mut deflated_frame_obj = build_uncompressed_test_obj(
        study_uid,
        &format!("{study_uid}.2"),
        &format!("{study_uid}.2.1"),
        16,
        16,
        16,
        raw_16.clone(),
    );
    deflated_frame_obj
        .transcode(&DEFLATED_IMAGE_FRAME_COMPRESSION.erased())
        .unwrap();
    let mut deflated_frame_bytes = Vec::new();
    deflated_frame_obj.write_all(&mut deflated_frame_bytes).unwrap();

    // 3. RLE Lossless (1.2.840.10008.1.2.5)
    let rle_frag = encode_rle_lossless_frame(&raw_16, 16, 16, 1, 16).unwrap();
    let mut rle_inner = (*build_uncompressed_test_obj(
        study_uid,
        &format!("{study_uid}.3"),
        &format!("{study_uid}.3.1"),
        16,
        16,
        16,
        raw_16.clone(),
    ))
    .clone();
    rle_inner.put_element(DataElement::new_with_len(
        tags::PIXEL_DATA,
        VR::OB,
        Length::UNDEFINED,
        PixelFragmentSequence::new(vec![], vec![rle_frag]),
    ));
    let rle_obj = rle_inner
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(TS_RLE_LOSSLESS)
                .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(format!("{study_uid}.3.1")),
        )
        .unwrap();
    let mut rle_bytes = Vec::new();
    rle_obj.write_all(&mut rle_bytes).unwrap();

    // 4. JPEG Baseline 8-bit (1.2.840.10008.1.2.4.50)
    let raw_8: Vec<u8> = (0..16 * 16).map(|i| ((i * 7) % 256) as u8).collect();
    let mut jpeg_obj = build_uncompressed_test_obj(
        study_uid,
        &format!("{study_uid}.4"),
        &format!("{study_uid}.4.1"),
        16,
        16,
        8,
        raw_8,
    );
    jpeg_obj.transcode(&JPEG_BASELINE.erased()).unwrap();
    let mut jpeg_bytes = Vec::new();
    jpeg_obj.write_all(&mut jpeg_bytes).unwrap();

    // 5. JPEG XL Lossless (1.2.840.10008.1.2.4.110)
    let mut jxl_obj = build_uncompressed_test_obj(
        study_uid,
        &format!("{study_uid}.5"),
        &format!("{study_uid}.5.1"),
        16,
        16,
        16,
        raw_16.clone(),
    );
    jxl_obj.transcode(&JPEG_XL_LOSSLESS.erased()).unwrap();
    let mut jxl_bytes = Vec::new();
    jxl_obj.write_all(&mut jxl_bytes).unwrap();

    // 6. JPEG 2000 Lossless (1.2.840.10008.1.2.4.90) decoded via cmake-free bundled OpenJPEG (openjpeg-sys)
    let mut j2k_inner = (*build_uncompressed_test_obj(
        study_uid,
        &format!("{study_uid}.6"),
        &format!("{study_uid}.6.1"),
        8,
        8,
        8,
        vec![0u8; 64],
    ))
    .clone();
    j2k_inner.put_element(DataElement::new_with_len(
        tags::PIXEL_DATA,
        VR::OB,
        Length::UNDEFINED,
        PixelFragmentSequence::new(vec![], vec![J2K_LOSSLESS_8X8_RAMP.to_vec()]),
    ));
    let j2k_obj = j2k_inner
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::JPEG2000_LOSSLESS)
                .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(format!("{study_uid}.6.1")),
        )
        .unwrap();
    let mut j2k_bytes = Vec::new();
    j2k_obj.write_all(&mut j2k_bytes).unwrap();

    // Ingest all 6 compressed DICOM objects via STOW-RS
    let boundary = "codec-stow-bnd";
    let mut stow_body = Vec::new();
    for part in [
        deflated_ds_bytes,
        deflated_frame_bytes,
        rle_bytes,
        jpeg_bytes,
        jxl_bytes,
        j2k_bytes,
    ] {
        stow_body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
        );
        stow_body.extend_from_slice(&part);
        stow_body.extend_from_slice(b"\r\n");
    }
    stow_body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let stow_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dicomweb/studies")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/related; type=\"application/dicom\"; boundary=\"{boundary}\""),
                )
                .body(Body::from(stow_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stow_res.status(), StatusCode::OK);
    let stow_json: serde_json::Value =
        serde_json::from_slice(&stow_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stow_json["series_ingested"], 6);
    assert_eq!(stow_json["frames_ingested"], 6);

    // Verify lossless pixel fidelity on Deflated Dataset, Deflated Frame, RLE, JXL, and J2K (openjp2)
    for ser_idx in [1, 2, 3, 5] {
        let ser_uid = format!("{study_uid}.{ser_idx}");
        let sop_uid = format!("{ser_uid}.1");
        let blobs = store
            .read_frame_blobs(study_uid, &ser_uid, &sop_uid, &[1], None)
            .await
            .unwrap();
        assert_eq!(
            blobs[0].bytes, raw_16,
            "lossless decode mismatch on series {ser_idx}"
        );
    }

    let j2k_ser = format!("{study_uid}.6");
    let j2k_sop = format!("{j2k_ser}.1");
    let j2k_blobs = store
        .read_frame_blobs(study_uid, &j2k_ser, &j2k_sop, &[1], None)
        .await
        .unwrap();
    let expected_ramp: Vec<u8> = (0u8..64).collect();
    assert_eq!(j2k_blobs[0].bytes, expected_ramp);

    // Verify JPEG Baseline decoded to 256 bytes (8-bit 16x16)
    let jpeg_ser = format!("{study_uid}.4");
    let jpeg_sop = format!("{jpeg_ser}.1");
    let jpeg_blobs = store
        .read_frame_blobs(study_uid, &jpeg_ser, &jpeg_sop, &[1], None)
        .await
        .unwrap();
    assert_eq!(jpeg_blobs[0].bytes.len(), 256);

    // Verify WADO-RS Accept negotiation on series 1
    let s1 = format!("{study_uid}.1");
    let sop1 = format!("{s1}.1");
    let frame_url = format!("/dicomweb/studies/{study_uid}/series/{s1}/instances/{sop1}/frames/1");

    // 1) Cornerstone3D wildcard -> Explicit VR Little Endian
    let wild_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&frame_url)
                .header(
                    header::ACCEPT,
                    "multipart/related; type=application/octet-stream; transfer-syntax=*",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wild_res.status(), StatusCode::OK);
    let wild_body = wild_res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&wild_body[..120]).contains(TS_EXPLICIT_VR_LE));
    assert_eq!(extract_multipart_part(&wild_body, "dicom-lance-boundary"), raw_16);

    // 2) RLE Lossless negotiation (1.2.840.10008.1.2.5) + round-trip decode check
    let rle_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&frame_url)
                .header(
                    header::ACCEPT,
                    format!("multipart/related; type=\"image/dicom-rle\"; transfer-syntax={TS_RLE_LOSSLESS}"),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rle_res.status(), StatusCode::OK);
    assert!(rle_res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("image/dicom-rle"));
    let rle_resp_body = rle_res.into_body().collect().await.unwrap().to_bytes();
    let rle_part = extract_multipart_part(&rle_resp_body, "dicom-lance-boundary");
    assert_eq!(rle_part, encode_rle_lossless_frame(&raw_16, 16, 16, 1, 16).unwrap());

    // 3) Deflated Image Frame negotiation (1.2.840.10008.1.2.8.1) + raw inflate verify
    let def_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&frame_url)
                .header(
                    header::ACCEPT,
                    format!("multipart/related; type=\"application/octet-stream\"; transfer-syntax={TS_DEFLATED_IMAGE_FRAME}"),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(def_res.status(), StatusCode::OK);
    let def_body = def_res.into_body().collect().await.unwrap().to_bytes();
    let def_part = extract_multipart_part(&def_body, "dicom-lance-boundary");
    let mut decoder = DeflateDecoder::new(def_part.as_slice());
    let mut inflated = Vec::new();
    decoder.read_to_end(&mut inflated).unwrap();
    assert_eq!(inflated, raw_16);

    // 4) Unsupported transfer syntax -> 406 Not Acceptable with supported list
    let res_406 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&frame_url)
                .header(
                    header::ACCEPT,
                    "multipart/related; type=\"image/jp2\"; transfer-syntax=1.2.840.10008.1.2.4.90",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_406.status(), StatusCode::NOT_ACCEPTABLE);
    let err_json: serde_json::Value =
        serde_json::from_slice(&res_406.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(err_json["supported_transfer_syntaxes"].as_array().unwrap().len() >= 4);

    // 5) Range + compressed TS -> 406 Not Acceptable
    let range_rle_res = app
        .oneshot(
            Request::builder()
                .uri(&frame_url)
                .header(header::ACCEPT, "image/dicom-rle")
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range_rle_res.status(), StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn enhanced_multiframe_and_dicom_seg_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));

    // 1. Seed phantom dataset (includes bit-packed Chest CT SEG overlay)
    let report = seed_phantoms_into_store(&store).await.unwrap();
    assert_eq!(report.series_ingested, 4);
    assert_eq!(report.frames_ingested, CT_SLICES + MR_SLICES + CT_SEG_FRAMES);

    // 2. Construct & ingest a 6-frame Enhanced Multi-Frame MR instance in reversed Z order
    let enh_study = "1.2.826.0.1.3680043.10.5432.6600";
    let enh_series = "1.2.826.0.1.3680043.10.5432.6600.1";
    let enh_sop = "1.2.826.0.1.3680043.10.5432.6600.1.1";

    let mut enh_obj = InMemDicomObject::new_empty();
    test_put_str(&mut enh_obj, tags::PATIENT_ID, VR::LO, "PAT-ENH-01");
    test_put_str(&mut enh_obj, tags::PATIENT_NAME, VR::PN, "Enhanced^MultiFrame");
    test_put_str(&mut enh_obj, tags::STUDY_INSTANCE_UID, VR::UI, enh_study);
    test_put_str(&mut enh_obj, tags::SERIES_INSTANCE_UID, VR::UI, enh_series);
    test_put_str(&mut enh_obj, tags::SOP_INSTANCE_UID, VR::UI, enh_sop);
    test_put_str(
        &mut enh_obj,
        tags::SOP_CLASS_UID,
        VR::UI,
        uids::ENHANCED_MR_IMAGE_STORAGE,
    );
    test_put_str(&mut enh_obj, tags::MODALITY, VR::CS, "MR");
    test_put_str(
        &mut enh_obj,
        tags::SERIES_DESCRIPTION,
        VR::LO,
        "Enhanced Brain T2 Multi-Frame",
    );
    test_put_str(&mut enh_obj, tags::INSTANCE_NUMBER, VR::IS, "1");
    test_put_str(&mut enh_obj, tags::SERIES_NUMBER, VR::IS, "1");
    test_put_u16(&mut enh_obj, tags::ROWS, 16);
    test_put_u16(&mut enh_obj, tags::COLUMNS, 16);
    test_put_u16(&mut enh_obj, tags::SAMPLES_PER_PIXEL, 1);
    test_put_str(&mut enh_obj, tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
    test_put_u16(&mut enh_obj, tags::BITS_ALLOCATED, 16);
    test_put_u16(&mut enh_obj, tags::BITS_STORED, 12);
    test_put_u16(&mut enh_obj, tags::HIGH_BIT, 11);
    test_put_u16(&mut enh_obj, tags::PIXEL_REPRESENTATION, 0);
    test_put_str(&mut enh_obj, tags::NUMBER_OF_FRAMES, VR::IS, "6");

    let mut shared = InMemDicomObject::new_empty();
    let mut orient = InMemDicomObject::new_empty();
    test_put_str(&mut orient, tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\0\\1\\0");
    shared.put_element(DataElement::new(
        tags::PLANE_ORIENTATION_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![orient], Length::UNDEFINED),
    ));
    let mut measures = InMemDicomObject::new_empty();
    test_put_str(&mut measures, tags::PIXEL_SPACING, VR::DS, "0.9\\0.9");
    test_put_str(&mut measures, tags::SLICE_THICKNESS, VR::DS, "3.0");
    shared.put_element(DataElement::new(
        tags::PIXEL_MEASURES_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![measures], Length::UNDEFINED),
    ));
    enh_obj.put_element(DataElement::new(
        tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![shared], Length::UNDEFINED),
    ));

    let z_order = [5usize, 3, 1, 4, 2, 0];
    let mut pf_items = Vec::with_capacity(6);
    let mut enh_pixels = Vec::with_capacity(6 * 16 * 16 * 2);
    for &z_idx in &z_order {
        let z_mm = (z_idx as f64) * 3.0;
        let mut fg = InMemDicomObject::new_empty();
        let mut ppos = InMemDicomObject::new_empty();
        test_put_str(
            &mut ppos,
            tags::IMAGE_POSITION_PATIENT,
            VR::DS,
            &format!("-7.2\\-7.2\\{z_mm:.2}"),
        );
        fg.put_element(DataElement::new(
            tags::PLANE_POSITION_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(vec![ppos], Length::UNDEFINED),
        ));
        let mut voi = InMemDicomObject::new_empty();
        test_put_str(
            &mut voi,
            tags::WINDOW_CENTER,
            VR::DS,
            &(400 + (z_idx as i32) * 25).to_string(),
        );
        test_put_str(&mut voi, tags::WINDOW_WIDTH, VR::DS, "800");
        fg.put_element(DataElement::new(
            tags::FRAME_VOILUT_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(vec![voi], Length::UNDEFINED),
        ));
        pf_items.push(fg);

        let val = 200u16 + (z_idx as u16) * 50;
        for _ in 0..(16 * 16) {
            enh_pixels.extend_from_slice(&val.to_le_bytes());
        }
    }
    enh_obj.put_element(DataElement::new(
        tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(pf_items, Length::UNDEFINED),
    ));
    enh_obj.put_element(DataElement::new(
        tags::PIXEL_DATA,
        VR::OW,
        PrimitiveValue::from(enh_pixels),
    ));

    let enh_file = enh_obj
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::ENHANCED_MR_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(enh_sop),
        )
        .unwrap();
    let mut enh_bytes = Vec::new();
    enh_file.write_all(&mut enh_bytes).unwrap();

    let app = build_router(store.clone(), None);
    let ingest_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dicomweb/studies")
                .header(header::CONTENT_TYPE, "application/dicom")
                .body(Body::from(enh_bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ingest_res.status(), StatusCode::OK);

    // Check Enhanced Multi-Frame metadata retains functional groups and frames-manifest sorts by Z
    let meta_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/dicomweb/studies/{enh_study}/series/{enh_series}/metadata"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(meta_res.status(), StatusCode::OK);
    let meta_json: Vec<serde_json::Value> =
        serde_json::from_slice(&meta_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(meta_json.len(), 1);
    assert!(meta_json[0].get("52009229").is_some());
    assert_eq!(meta_json[0]["52009230"]["Value"].as_array().unwrap().len(), 6);

    let manifest_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/studies/{enh_study}/series/{enh_series}/frames-manifest"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manifest_res.status(), StatusCode::OK);
    let manifest_json: serde_json::Value =
        serde_json::from_slice(&manifest_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let enh_frames = manifest_json["frames"].as_array().unwrap();
    assert_eq!(enh_frames.len(), 6);
    for (idx, f) in enh_frames.iter().enumerate() {
        assert_eq!(f["slice_sort_index"], idx as i64);
        assert!((f["ipp_z"].as_f64().unwrap() - (idx as f64) * 3.0).abs() < 1e-4);
        assert!((f["window_center"].as_f64().unwrap() - (400.0 + (idx as f64) * 25.0)).abs() < 1e-4);
    }

    // 3. Verify DICOM SEG overlay API + bit-packed mask pixel values on Chest CT slice 23
    let seg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}/segmentations"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(seg_res.status(), StatusCode::OK);
    let seg_json: serde_json::Value =
        serde_json::from_slice(&seg_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let segs = seg_json["segmentations"].as_array().unwrap();
    assert_eq!(segs.len(), 1);
    let seg = &segs[0];
    assert_eq!(seg["seg_series_instance_uid"], CT_SEG_SERIES_UID);
    assert_eq!(seg["segmentation_type"], "BINARY");

    let segments = seg["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0]["segment_label"], "RUL Nodule Solid Core");
    assert_eq!(segments[1]["segment_label"], "Ground-Glass Halo");

    // Verify CIELab round-trip preserved approximate red color [239, 68, 68]
    let lab = rgb_to_dicom_cielab(239, 68, 68);
    assert_eq!(lab.len(), 3);
    let c0 = segs[0]["segments"][0]["color_rgba"].as_array().unwrap();
    assert!((c0[0].as_i64().unwrap() - 239).abs() <= 4);
    assert!((c0[1].as_i64().unwrap() - 68).abs() <= 4);

    let slice23_sop = format!("{CT_SERIES_UID}.23");
    let seg_frames = seg["frames"].as_array().unwrap();
    assert_eq!(seg_frames.len(), CT_SEG_FRAMES);
    let core_slice23 = seg_frames
        .iter()
        .find(|f| {
            f["segment_number"] == 1 && f["referenced_sop_instance_uid"] == slice23_sop.as_str()
        })
        .expect("SEG core mapping for slice 23");
    let seg_frame_num = core_slice23["seg_frame_number"].as_i64().unwrap();
    let seg_sop = seg["seg_sop_instance_uid"].as_str().unwrap();

    // Fetch unpacked 8-bit SEG mask via WADO-RS
    let wado_seg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SEG_SERIES_UID}/instances/{seg_sop}/frames/{seg_frame_num}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wado_seg_res.status(), StatusCode::OK);
    let seg_part = extract_multipart_part(
        &wado_seg_res.into_body().collect().await.unwrap().to_bytes(),
        "dicom-lance-boundary",
    );
    assert_eq!(seg_part.len(), CT_DIM * CT_DIM);
    // RUL nodule core (86, 51) == 255; distant normal right lung (96, 64) == 0
    assert_eq!(seg_part[86 * CT_DIM + 51], 255);
    assert_eq!(seg_part[96 * CT_DIM + 64], 0);

    // Ensure SEG frames are excluded from clinical CT similarity results
    let ct_sim_hits = store
        .find_similar_frames(CT_STUDY_UID, CT_SERIES_UID, &slice23_sop, 1, 12, true)
        .await
        .unwrap();
    assert!(ct_sim_hits
        .iter()
        .all(|h| h.series_instance_uid != CT_SEG_SERIES_UID));

    // Also verify sanity check on from_reader / SEGMENTATION_STORAGE_UID usage
    let parsed_check = from_reader(std::io::Cursor::new(
        std::fs::read(tmp.path().join("phantoms/chest_seg/rul_nodule_seg.dcm")).unwrap(),
    ))
    .unwrap();
    assert_eq!(
        parsed_check
            .element(tags::SOP_CLASS_UID)
            .unwrap()
            .to_str()
            .unwrap()
            .trim_end_matches('\0'),
        SEGMENTATION_STORAGE_UID
    );
}

#[tokio::test]
async fn deidentification_ingest_and_seg_overlay_reference_preservation() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let deid = DeidConfig::enabled_with_salt("test-salt-2026");
    let app = build_router_with_config(
        store.clone(),
        None,
        ServerConfig {
            deid: deid.clone(),
            ..Default::default()
        },
    );

    let report = seed_phantoms_into_store_with_deid(&store, Some(deid.clone()))
        .await
        .unwrap();
    assert_eq!(report.series_ingested, 4);
    assert_eq!(report.frames_ingested, CT_SLICES + CT_SEG_FRAMES + MR_SLICES);

    let expected_ct_study = deid.remap_uid(CT_STUDY_UID);
    let expected_ct_series = deid.remap_uid(CT_SERIES_UID);
    let expected_seg_series = deid.remap_uid(CT_SEG_SERIES_UID);
    assert!(expected_ct_study.starts_with("2.25."));
    assert_ne!(expected_ct_study, CT_STUDY_UID);

    let search_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/search")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search_res.status(), StatusCode::OK);
    let search_bytes = search_res.into_body().collect().await.unwrap().to_bytes();
    let search_text = String::from_utf8_lossy(&search_bytes);
    for leaked in [
        "Rivera^Elena",
        "PAT-ONC-001",
        "Chen^Marcus",
        "PAT-NEURO-002",
        "ACC-CT-2026-0142",
        CT_STUDY_UID,
        CT_SERIES_UID,
    ] {
        assert!(
            !search_text.contains(leaked),
            "de-identified catalog leaked original value: {leaked}"
        );
    }

    // Verify PS3.15 audit tags in WADO-RS metadata for remapped CT series
    let meta_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{expected_ct_study}/series/{expected_ct_series}/metadata"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(meta_res.status(), StatusCode::OK);
    let meta_json: serde_json::Value =
        serde_json::from_slice(&meta_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let first_meta = &meta_json.as_array().unwrap()[0];
    assert_eq!(first_meta["00120062"]["Value"][0], "YES");
    assert_eq!(
        first_meta["00080016"]["Value"][0],
        uids::CT_IMAGE_STORAGE
    );

    // Verify SEG references still resolve against remapped CT series + slice 23 SOP UID
    let seg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/studies/{expected_ct_study}/series/{expected_ct_series}/segmentations"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(seg_res.status(), StatusCode::OK);
    let seg_json: serde_json::Value =
        serde_json::from_slice(&seg_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let seg = &seg_json["segmentations"][0];
    assert_eq!(seg["seg_series_instance_uid"], expected_seg_series);
    assert_eq!(seg["referenced_series_uid"], expected_ct_series);

    let remapped_slice23_sop = deid.remap_uid(&format!("{CT_SERIES_UID}.23"));
    let slice23_core = seg["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            f["segment_number"] == 1
                && f["referenced_sop_instance_uid"] == remapped_slice23_sop.as_str()
        })
        .expect("SEG slice 23 core reference after de-identification");
    let seg_sop = seg["seg_sop_instance_uid"].as_str().unwrap();
    let seg_frame = slice23_core["seg_frame_number"].as_i64().unwrap();

    let wado_seg = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{expected_ct_study}/series/{expected_seg_series}/instances/{seg_sop}/frames/{seg_frame}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wado_seg.status(), StatusCode::OK);
    let seg_part = extract_multipart_part(
        &wado_seg.into_body().collect().await.unwrap().to_bytes(),
        "dicom-lance-boundary",
    );
    assert_eq!(seg_part.len(), CT_DIM * CT_DIM);
    assert_eq!(seg_part[86 * CT_DIM + 51], 255);

    // Also test per-request ?deidentify=true when server default has deid.enabled = false
    let req_tmp = TempDir::new().unwrap();
    let req_store = Arc::new(DicomLanceStore::new(req_tmp.path()));
    let req_app = build_router(req_store.clone(), None);
    let raw_obj = build_uncompressed_test_obj(
        "1.2.826.0.1.3680043.10.5432.8800",
        "1.2.826.0.1.3680043.10.5432.8800.1",
        "1.2.826.0.1.3680043.10.5432.8800.1.1",
        8,
        8,
        16,
        vec![0u8; 128],
    );
    let mut raw_bytes = Vec::new();
    raw_obj.write_all(&mut raw_bytes).unwrap();
    let bnd = "per-req-deid-bnd";
    let mut form_body = Vec::new();
    form_body.extend_from_slice(
        format!("--{bnd}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"s.dcm\"\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    form_body.extend_from_slice(&raw_bytes);
    form_body.extend_from_slice(format!("\r\n--{bnd}--\r\n").as_bytes());
    let up_res = req_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/upload?deidentify=true")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={bnd}"),
                )
                .body(Body::from(form_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(up_res.status(), StatusCode::OK);
    let up_json: serde_json::Value =
        serde_json::from_slice(&up_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let new_uid = up_json["study_uids"][0].as_str().unwrap();
    assert!(new_uid.starts_with("2.25."));
    assert_ne!(new_uid, "1.2.826.0.1.3680043.10.5432.8800");
}

#[tokio::test]
async fn bearer_auth_cors_allowlist_and_directory_ingest_guard() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    seed_phantoms_into_store(&store).await.unwrap();

    let app = build_router_with_config(
        store,
        None,
        ServerConfig {
            api_token: Some("super-secret-token-42".to_string()),
            cors_origins: vec!["https://viewer.example.org".to_string()],
            allow_directory_ingest: false,
            deid: DeidConfig::default(),
        },
    );

    // 1. /healthz stays public without auth
    let health = app
        .clone()
        .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    // 2. Protected routes reject missing/invalid token with 401 + WWW-Authenticate
    for (method, path) in [
        ("GET", "/api/stats"),
        ("GET", "/api/search"),
        ("GET", "/dicomweb/studies"),
        ("DELETE", "/api/studies/1.2.826.0.1.3680043.10.5432.1001"),
    ] {
        let unauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_eq!(
            unauth.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer realm=\"dicom-lance\""
        );
    }

    let wrong = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header(header::AUTHORIZATION, "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // 3. Valid bearer token succeeds & reflects security flags
    let stats_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header(header::AUTHORIZATION, "Bearer super-secret-token-42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_res.status(), StatusCode::OK);
    let stats_json: serde_json::Value =
        serde_json::from_slice(&stats_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stats_json["auth_enabled"], true);
    assert_eq!(stats_json["allow_directory_ingest"], false);

    // 4. Directory ingest blocked with 403 even with valid token when disabled
    let dir_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest/directory")
                .header(header::AUTHORIZATION, "Bearer super-secret-token-42")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"path":"/tmp"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dir_res.status(), StatusCode::FORBIDDEN);

    // 5. CORS preflight echoes allowlisted origin and omits disallowed origins
    let allowed_preflight = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/search")
                .header(header::ORIGIN, "https://viewer.example.org")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(allowed_preflight.status().is_success());
    assert_eq!(
        allowed_preflight
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://viewer.example.org"
    );

    let denied_preflight = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/search")
                .header(header::ORIGIN, "https://evil.attacker.invalid")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(denied_preflight
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
}

#[tokio::test]
async fn delete_study_and_lance_compact_cleanup_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(DicomLanceStore::new(tmp.path()));
    let app = build_router(store.clone(), None);

    seed_phantoms_into_store(&store).await.unwrap();

    // 1. Delete Chest CT study (3 series: CT + SR + SEG, 80 frames)
    let del_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/studies/{CT_STUDY_UID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_res.status(), StatusCode::OK);
    let del_json: serde_json::Value =
        serde_json::from_slice(&del_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(del_json["study_instance_uid"], CT_STUDY_UID);
    assert_eq!(del_json["series_deleted"], 3);
    assert_eq!(del_json["frames_deleted"], CT_SLICES + CT_SEG_FRAMES);

    // Repeat delete returns 404 Not Found
    let repeat_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/studies/{CT_STUDY_UID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repeat_del.status(), StatusCode::NOT_FOUND);

    // Deleted study is absent from hybrid/FTS search immediately
    let sp_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/search?q=spiculated&mode=hybrid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sp_res.status(), StatusCode::OK);
    let sp_json: serde_json::Value =
        serde_json::from_slice(&sp_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(sp_json["studies"].as_array().unwrap().len(), 0);

    // 2. Compact deleted fragments and prune old Lance versions + Blob V2 data files
    let compact_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/maintenance/compact")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"retain_versions":1,"delete_unverified":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(compact_res.status(), StatusCode::OK);
    let compact_json: serde_json::Value =
        serde_json::from_slice(&compact_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(compact_json["series"]["row_count"], 1);
    assert_eq!(compact_json["frames"]["row_count"], MR_SLICES);
    assert!(compact_json["series"]["old_versions_removed"].as_u64().unwrap() > 0);
    assert!(compact_json["frames"]["old_versions_removed"].as_u64().unwrap() > 0);
    assert!(compact_json["series"]["fragment_count"].as_u64().unwrap() <= 1);
    assert!(compact_json["frames"]["fragment_count"].as_u64().unwrap() <= 1);

    // 3. Remaining MR study + Blob V2 Packed reads + Range + Hybrid search still work post-compaction
    let stats_res = app
        .clone()
        .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(stats_res.status(), StatusCode::OK);
    let stats_json: serde_json::Value =
        serde_json::from_slice(&stats_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stats_json["frames_dataset"]["storage_version"], "2.2");
    assert_eq!(stats_json["blob_v2_kind_sample"], "Packed");

    let mr_search = store
        .search_series_with_mode(Some("ring-enhancing"), None, None, 10, SearchMode::Hybrid)
        .await
        .unwrap();
    assert_eq!(mr_search.len(), 1);
    assert_eq!(mr_search[0].study_instance_uid, MR_STUDY_UID);
    let mr_ser = &mr_search[0].series[0].series_instance_uid;

    let mr_manifest = store
        .get_series_frame_descriptors(MR_STUDY_UID, mr_ser)
        .await
        .unwrap();
    assert_eq!(mr_manifest.len(), MR_SLICES);
    let first_sop = &mr_manifest[0].sop_instance_uid;
    let range_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/dicomweb/studies/{MR_STUDY_UID}/series/{mr_ser}/instances/{first_sop}/frames/1"
                ))
                .header(header::RANGE, "bytes=0-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range_res.status(), StatusCode::OK);
    let range_part = extract_multipart_part(
        &range_res.into_body().collect().await.unwrap().to_bytes(),
        "dicom-lance-boundary",
    );
    assert_eq!(range_part.len(), 64);

    // 4. Deleting the last study and compacting 0-row tables succeeds cleanly
    let del_last = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/studies/{MR_STUDY_UID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_last.status(), StatusCode::OK);

    let compact_empty = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/maintenance/compact")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(compact_empty.status(), StatusCode::OK);
    let empty_json: serde_json::Value =
        serde_json::from_slice(&compact_empty.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(empty_json["series"]["row_count"], 0);
    assert_eq!(empty_json["frames"]["row_count"], 0);
}
