# Changelog

All notable changes to `dicom-lance` are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **WADO-RS `/rendered` Endpoint (`src/render.rs`, `src/store.rs`, `src/api.rs`)**:
  - Added PS3.18 server-side rendering routes at `/dicomweb/studies/{study}/series/{series}/rendered`, `/dicomweb/studies/{study}/series/{series}/instances/{sop}/rendered`, and `/dicomweb/studies/{study}/series/{series}/instances/{sop}/frames/{frames}/rendered`.
  - Supports quality-weighted `Accept` negotiation (`image/png`, `image/jpeg` via existing `jpeg-encoder`, `multipart/related`), `window=center,width[,LINEAR|LINEAR_EXACT|SIGMOID]`, aspect-preserving bilinear `viewport=vw,vh`, `quality=1..100`, `iccprofile`, `MONOCHROME1` inversion, `Cache-Control`, and `Vary: Accept`.
- **10k+ Frame Benchmark Suite & `BENCHMARKS.md` (`src/bin/dicom-lance-bench.rs`)**:
  - Added `dicom-lance-bench` CLI measuring bulk ingest, incremental `MergeInsert`, `take_blobs` full/range reads, WADO-RS `/frames` (`ExplicitVRLE`/`RLE`), `/rendered` (PNG/JPEG), `/frames-manifest`, BM25/Vector/Hybrid search, 768-d slice KNN, and `delete_study` + `compact_and_cleanup`.
  - Recorded `10,240`-frame (`320 MiB` raw pixel) release results (`7,301 frames/s` ingest, `1.14 ms` p50 `take_blobs`, `1.67 ms` p50 PNG `/rendered`, `2.84 ms` p50 768-d slice KNN) in `BENCHMARKS.md`.
- **Docker `cargo-chef` Build Caching (`Dockerfile`, `.dockerignore`, `.github/workflows/ci.yml`)**:
  - Updated multi-stage `Dockerfile` to prepare and cache Rust dependency compilation with `lukemathwalker/cargo-chef:latest-rust-1-bookworm` (`--bin dicom-lance`) and expanded the CI container smoke test to assert HTTP 200 PNG magic bytes from `/rendered`.
- **Interactive SEG Controls & 3D MPR SEG Overlays (`web/src/cornerstone/`)**:
  - Added per-segment visibility toggles (`aria-pressed`), a `10%–100%` overlay opacity slider, and **"Center SEG"** camera alignment.
  - Enabled **SEG Overlay** in **3D MPR** across Axial, Sagittal, and Coronal WebGL viewports via an affine tight-bbox 3D mask reslicer (`buildSegVolumeLayers`, `resliceSegVolumesToCanvasRgba`).
- **Measurement -> DICOM SR (`TID 1500`) Round-Trip (`src/sr.rs`, `src/api.rs`, `web/src/cornerstone/measurements.ts`)**:
  - Added `POST /api/studies/{study}/series/{series}/measurements/sr` supporting both `Accept: application/dicom` (Part-10 `ComprehensiveSRStorage` bytes for client `STOW-RS` upload to `/dicomweb/studies`) and `Accept: application/json` (direct server-side ingest inheriting stored de-identified pseudonyms).
  - Added **Measurements** drawer in the viewer (`Length`, `Bidirectional` RECIST, `EllipticalROI`) with editable labels, clinician findings notes, and one-click `Save DICOM SR (STOW-RS)` persistence indexed immediately into Hybrid BM25 + Vector search.
- **DICOM PS3.15 De-identification (`src/deid.rs`, `src/dicom_ingest.rs`)**:
  - Added deterministic salted SHA-256 DICOM PS3.15 Basic Profile de-identification across file meta and nested sequences (`CT`/`SR`/`SEG` cross-references stay aligned via consistent Study/Series/SOP/FrameOfReference UID remapping while private tags, direct PHI, observer sequences, and sub-study timestamps are stripped or generalized).
  - Configurable globally via `--deidentify` / `DEIDENTIFY=true` and `--deidentify-salt` / `DEIDENTIFY_SALT`, or per ingest request via `?deidentify=true` on `/api/ingest/upload`, `/dicomweb/studies` (STOW-RS), and `/api/ingest/seed-phantoms` (`deidentify: true` on `/api/ingest/directory`).
- **Bearer Authentication, CORS Allowlist & Directory Ingest Guard (`src/api.rs`, `src/main.rs`)**:
  - Added fixed-time SHA-256 `Authorization: Bearer` middleware (`--api-token` / `API_TOKEN`) protecting all `/api/*` and `/dicomweb/*` endpoints while keeping `/healthz` and the static SPA shell accessible.
  - Added validated HTTP/HTTPS `--cors-origin` / `CORS_ORIGINS` allowlist support (`AllowOrigin::list`) and automatic disabling (`403 Forbidden`) of `/api/ingest/directory` when binding a non-loopback `--host`.
- **Study Deletion & Lance Compaction Lifecycle (`src/store.rs`, `src/api.rs`)**:
  - Added `DELETE /api/studies/{study}` across both `dicom_series.lance` and `dicom_frames.lance` with zero-row index safety and atomic cache updates.
  - Added `POST /api/maintenance/compact` backed by `lance::dataset::optimize::compact_files` + `CleanupPolicyBuilder` to materialize deletions, re-align scalar/FTS/vector indexes, and reclaim old versions and unreferenced `Blob V2` data files.
- **Viewer Hardening Controls (`web/src/`)**:
  - Added RAM-only Bearer Token unlock/lock session workflow (`ApiUnauthorizedError` + Cornerstone `beforeSend`), Upload modal `De-identify PHI` toggle, server directory-ingest policy enforcement, and WCAG 2.2 AA two-step `Delete` -> `Confirm` study removal in the sidebar.
- **Tests**:
  - Added Rust Hegel PBT properties and integration tests for PS3.15 de-identification (including SEG overlay cross-reference preservation), Bearer auth/CORS/directory-guard enforcement, and `DELETE` + `compact_files`/cleanup lifecycle, plus Vitest and hermetic Playwright E2E coverage.

## [0.3.0] - 2026-11-24

### Added
- **Python / AI Ecosystem Interop (`examples/pylance-embeddings/`, `src/api.rs`)**:
  - Added `POST /api/cache/invalidate` so external Python/ML jobs can hot-reload cached Lance `Dataset` handles without restarting the server.
  - Added `uv`-managed `dicom-lance-embed` package (`examples/pylance-embeddings/`) for zero-copy `pylance` `Blob V2` frame reads into NumPy/PyTorch, columnar embedding evolution (`drop_columns` + `add_columns`) leaving `BlobKind::Packed` sidecar files untouched, and pluggable `RadiomicsEmbedder`, OpenAI-compatible `/v1/embeddings`, and optional `BioMedCLIP` backends.
- **Clinical DICOM Format Depth**:
  - Enabled `cmake`-free compressed codecs in `Cargo.toml`: **JPEG 2000 / HTJ2K** (`openjpeg-sys`, `BSD-2-Clause` compiled via `cc`), **Deflated Explicit VR Little Endian** & **Deflated Image Frame Compression** (`deflate` / `flate2`), **JPEG XL** (`jpegxl` / `jxl-oxide`), **RLE Lossless**, and **JPEG** (with optional C++ JPEG-LS gated behind `--features codec-jpegls`).
  - **WADO-RS `Accept` Transfer-Syntax Negotiation (`src/codecs.rs`, `src/api.rs`)**: Quality-weighted PS3.18 media-type negotiation supporting `1.2.840.10008.1.2.1`, `1.2.840.10008.1.2`, `1.2.840.10008.1.2.5` (pure-Rust DICOM Annex G PackBits RLE encoder), and `1.2.840.10008.1.2.8.1`, with structured `406 Not Acceptable` responses on unsupported syntaxes or compressed byte ranges.
  - **Enhanced Multi-Frame CT/MR (`src/dicom_ingest.rs`, `web/src/cornerstone/seriesMapping.ts`)**: Shared (`5200,9229`) and Per-Frame (`5200,9230`) Functional Groups extraction for per-frame 3D normal sorting, VOI LUT, rescale, and Cornerstone3D per-frame metadata projection.
  - **DICOM SEG Storage & Viewer Overlay (`src/seg.rs`, `src/store.rs`, `src/phantom.rs`, `web/src/cornerstone/`)**: 1-bit packed segmentation unpacking, PS3.3 `RecommendedDisplayCIELabValue` -> sRGB conversion, `GET /api/studies/{study}/series/{series}/segmentations`, CT/MR `find_similar_frames` SEG exclusion filter, seeded 16-frame Chest CT RUL Spiculated Nodule DICOM SEG phantom (`4` series / `128` frames total), and Cornerstone3D Stack **"SEG Overlay"** toggle with affine camera-locked mask rendering and lesion legend.
  - Expanded Rust Hegel PBT, multi-codec & Enhanced/SEG integration tests, frontend Vitest/Hegel PBT, and hermetic WCAG 2.2 AA & live WebGL2 Playwright E2E coverage.

### Fixed
- Reset active `frames` and `sliceIndex` immediately on series switch in `web/src/cornerstone/DicomWorkspace.tsx` to avoid stale slice positions overwriting slider interactions during asynchronous stack mounting.

## [0.2.0] - 2026-11-23

### Added
- **Multimodal 768-d Embeddings (`src/embedding.rs`)**:
  - Default pure-Rust `LocalMultimodalEmbedder` combining non-negative SHA-256 text n-gram/prefix hashing (`512` dims) and pixel radiomics/spatial/gradient descriptors (`256` dims), L2-normalized to unit norm (`l2_normalize`).
  - Pluggable OpenAI-compatible `HttpEmbedder` configured via `--embedder-url` / `EMBEDDER_URL`, `--embedder-model` / `EMBEDDER_MODEL`, and `--embedder-api-key` / `EMBEDDER_API_KEY`.
- **Hybrid BM25 + Vector Search & "Find Similar Slices" (`src/store.rs`, `src/api.rs`)**:
  - `SearchMode::{Hybrid, Bm25, Vector}` powered by Lance `Inverted` BM25 full-text search, `Scanner::nearest` (`MetricType::Cosine`), and deterministic Reciprocal Rank Fusion (`rrf_fuse`, `k = 60`).
  - Automatic `IVF_FLAT` vector index creation when dataset row counts reach `>= 256` (exact Cosine KNN below threshold).
  - `GET /api/studies/{study}/series/{series}/instances/{sop}/frames/{frame}/similar` endpoint for intra/cross-series similar slice lookup.
- **Dataset Handle Caching & Scalar Indexing (`src/store.rs`)**:
  - Cached open `Arc<Dataset>` handles (`series_cache` / `frames_cache`) with atomic cache-hit counters (`cache_hits_series`, `cache_hits_frames` in `/api/stats`), atomic post-ingest refresh, `invalidate_cache`, and `frames_study_uid_btree`.
- **Remote & Cloud Object Storage URIs (`Cargo.toml`, `src/store.rs`, `src/main.rs`)**:
  - `StorageRoot` & `parse_storage_root` supporting `--dataset-uri` (`DATASET_URI`) and URI-aware `--data-dir` for local paths, `file://`, `shared-memory://`, `memory://`, and feature-gated cloud stores (`--features cloud`, `cloud-s3`, `cloud-gcs`, `cloud-azure`).
  - Remote-safe phantom seeding via temporary scratch directory cleanup.
- **Viewer UI Enhancements (`web/src/`)**:
  - Sidebar `Hybrid` / `BM25` / `Vector` search mode toggle.
  - **"Similar Slices"** toolbar button and accessible side panel (`768-d Cosine KNN`) with match percentages and click-to-jump navigation.
- **Automation & Release Packaging**:
  - `.github/workflows/release.yml` to build and push multi-stage runtime images to `ghcr.io/brucearctor/dicom-lance` on version tags (`v*`).
- **Tests**:
  - Rust Hegel PBT properties for `LocalMultimodalEmbedder`, `rrf_fuse`, and `StorageRoot` normalization, plus integration tests for hybrid/vector search, `/similar`, dataset handle caching, mock `/v1/embeddings` `HttpEmbedder`, and `shared-memory://` object store end-to-end flows.
  - Updated Vitest + `@hegeldev/hegel` PBT and hermetic/live Playwright E2E coverage.

### Fixed
- Added `role="region"` to Cornerstone viewport host containers and darkened active toolbar button color (`#0369a1`) to maintain `0` WCAG 2.2 AA (`aria-prohibited-attr`, `color-contrast`, `target-size`) axe-core violations while viewing DICOM canvases and the Similar Slices panel.

## [0.1.0] - 2026-11-23

### Added
- Initial release of `dicom-lance`: DICOM Part-10 ingestion, 3D oblique slice sorting, recursive DICOM SR extraction, 128×128 PNG thumbnails, Lance v2.2 two-dataset schema (`Blob V2` `Packed` sidecar files + `Inverted` BM25 FTS + `BTree` indexes), Axum DICOMweb server (`STOW-RS`, `QIDO-RS`, `WADO-RS` metadata and `take_blobs`/`read_range` frames), and React 19 + TypeScript + Cornerstone3D v4 2D Stack / 3D MPR viewer.
- Rust `hegeltest` and TypeScript `@hegeldev/hegel` property-based tests, integration tests, hermetic WCAG 2.2 AA & live WebGL2 Playwright E2E suites, `cargo-deny` license gate, and multi-stage non-root Docker image.
