use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use dicom_lance::api::{build_router, seed_phantoms_into_store};
use dicom_lance::embedding::{Embedder, HttpEmbedder};
use dicom_lance::phantom::{
    generate_phantom_dicom_files, CT_DIM, CT_SERIES_UID, CT_SLICES, CT_STUDY_UID, MR_SLICES,
};
use dicom_lance::schema::EMBEDDING_DIM;
use dicom_lance::store::{
    parse_storage_root, DicomLanceStore, SearchMode, StoreStats,
};
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
    assert_eq!(dcm_paths.len(), CT_SLICES + 1 + MR_SLICES);

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
    assert_eq!(stow_json["series_ingested"], 3);
    assert_eq!(stow_json["frames_ingested"], CT_SLICES + MR_SLICES);

    // 2. Verify Lance v2.2 telemetry, FTS index, and Packed Blob V2 storage
    let stats_res = app
        .clone()
        .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(stats_res.status(), StatusCode::OK);
    let stats: StoreStats =
        serde_json::from_slice(&stats_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stats.series_dataset.row_count, 3);
    assert_eq!(stats.frames_dataset.row_count, CT_SLICES + MR_SLICES);
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
    assert_eq!(qido_series.len(), 2);
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
    assert_eq!(report.series_ingested, 3);
    assert_eq!(report.frames_ingested, CT_SLICES + MR_SLICES);

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
    assert_eq!(report.series_ingested, 3);
    assert_eq!(report.frames_ingested, CT_SLICES + MR_SLICES);

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
