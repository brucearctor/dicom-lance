# dicom-lance Benchmarks

Reproducible storage, DICOMweb retrieval, server-side rendering, hybrid search, and vector KNN benchmarks for `dicom-lance` backed by **Lance v2.2 (`BlobKind::Packed` sidecars + FTS + IVF_FLAT vector + scalar indexes)**.

## Quick Reproduction

Run the bundled release benchmark binary (`src/bin/dicom-lance-bench.rs`):

```bash
cargo run --release --offline --locked --bin dicom-lance-bench -- \
  --studies 40 --slices 256 --dim 128
```

Optional flags:

- `--studies N` (default `40`): Number of synthetic CT/MR + SR study pairs (`2 * N` series total).
- `--slices N` (default `256`): Pixel frames per volumetric series (`studies * slices` total frames).
- `--dim N` (default `128`): Pixel width and height for 16-bit monochrome frames.
- `--iters N` (default `100`): Timed iterations per retrieval/render loop (search/manifest cap `50`, KNN cap `30`).
- `--keep-dir PATH`: Persist generated `.lance` datasets on disk instead of using an auto-cleaned temporary directory.
- `--json-out PATH`: Write structured JSON telemetry alongside the Markdown summary.

---

## Measured 10,240-Frame Run (Release Profile)

### Environment & Dataset Parameters

- **Hardware**: Apple M2 (8 CPU cores), 24 GiB unified RAM (`25,769,803,776` bytes), macOS APFS local NVMe SSD
- **Profile**: `cargo run --release --offline --locked` (`opt-level = 3` crate, `opt-level = 1` workspace dependencies)
- **Dataset**: **40 studies**, **80 series** (40 volumetric CT/MR + 40 paired clinical SR), **10,240 frames** (`128x128` 16-bit LE = **320.0 MiB** raw uncompressed pixel payload + 768-d float32 frame embeddings + scalar/FTS indexes)
- **Lance Storage Layout**: Lance `2.2`, Blob V2 kind = **`Packed`** (`dicom_series.lance`: `0.38 MiB`, `dicom_frames.lance`: `421.34 MiB`)

### Ingestion, Upsert & Storage Lifecycle

| Operation | Measured Time | Notes |
| --- | ---: | --- |
| Bulk Ingest (`10,240` frames + `80` series + embeddings + FTS/scalar index build) | **1,402.6 ms** | **7,301 frames/sec** |
| Incremental 1-study `MergeInsert` upsert (`256` frames + `1` SR) | **606.6 ms** | Keys: `(study_uid, series_uid, sop_uid, frame_number)` |
| Delete Study + `compact_and_cleanup` | **16.1 ms** | Reclaims deleted study rows/fragments and cleans old versions |

### Query, Retrieval & Rendering Latency (10,496 Frames Indexed Prior to Cleanup)

| Operation | Samples | Min (ms) | p50 (ms) | p95 (ms) | p99 (ms) | Max (ms) | Throughput (ops/s) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `Lance take_blobs` full frame | 100 | 1.00 | **1.14** | 1.32 | 1.63 | 1.87 | **860.5** |
| `Lance take_blobs` byte range (`0..4096`) | 100 | 1.01 | **1.16** | 1.31 | 1.36 | 1.44 | **850.9** |
| WADO-RS `/frames` (`ExplicitVRLittleEndian`) | 100 | 1.07 | **1.23** | 1.43 | 1.77 | 2.36 | **797.5** |
| WADO-RS `/frames` (`RLELossless` PackBits Annex G) | 100 | 1.04 | **1.21** | 1.35 | 1.43 | 1.43 | **823.6** |
| WADO-RS `/rendered` (`image/png`, `window=-600,1500`, `viewport=256,256`) | 100 | 1.47 | **1.67** | 1.91 | 1.96 | 2.02 | **592.3** |
| WADO-RS `/rendered` (`image/jpeg`, `quality=85`, `viewport=256,256`) | 100 | 1.63 | **1.80** | 1.98 | 2.04 | 2.04 | **554.7** |
| `GET /frames-manifest` (256-slice series descriptors, zero pixel I/O) | 50 | 0.91 | **1.01** | 1.19 | 1.22 | 1.22 | **978.1** |
| Series Search (`BM25 FTS`) | 50 | 0.69 | **0.76** | 0.86 | 1.14 | 1.14 | **1,295.7** |
| Series Search (`768-d Vector`) | 50 | 0.95 | **1.06** | 1.41 | 1.51 | 1.51 | **917.1** |
| Series Search (`Hybrid RRF`, `k = 60`) | 50 | 1.71 | **1.85** | 2.30 | 2.42 | 2.42 | **524.9** |
| `Find Similar Slices` (`768-d Cosine KNN` across >10k frames) | 30 | 2.57 | **2.84** | 3.23 | 6.71 | 6.71 | **335.5** |

---

## Architectural Comparison

`dicom-lance` replaces the traditional 3-store DICOM stack (**filesystem/S3 per-SOP `.dcm` objects** + **PostgreSQL metadata DB** + **external vector/search service**) with two self-contained Lance v2.2 datasets:

| Concern | Traditional Multi-Store PACS Stack | `dicom-lance` (Lance v2.2 + Blob V2) |
| --- | --- | --- |
| **Pixel Frame Storage** | 1 object/file per SOP instance; `LIST` / open overhead + full-object reads | Packed `Blob V2` sidecars with `take_blobs` O(1) row-ID lookup & byte-range reads (~1.14 ms p50) |
| **WADO-RS Metadata & Manifests** | SQL join + parsing headers across files | Columnar `projection` scans reading zero pixel bytes (`1.01 ms` p50 for 256 slices) |
| **BM25 + Semantic Vector Search** | External sync pipelines (Postgres -> ES + Qdrant/pgvector) | Single-table native Lance BM25 FTS + 768-d Vector + RRF (`1.85 ms` p50 hybrid) |
| **ML / PyTorch / NumPy Access** | Download & parse thousands of `.dcm` files via `pydicom` | Zero-copy `pylance` columnar + Blob V2 frame reads & schema-safe vector column updates |
| **Server-Side Rendering** | Separate render proxy or client-side full pixel download | Built-in `/rendered` PNG/JPEG with VOI LUT (`LINEAR`, `LINEAR_EXACT`, `SIGMOID`) in `~1.7 ms` p50 |

### Known Trade-Offs

- **Single-writer lock**: Concurrent multi-writer ingestion across independent nodes should shard by study or serialize through an ingestion queue; transactional multi-writer catalogs benefit from Lance's dataset versioning.
- **Large in-process volume generation**: At `100k+` frames in production, IVF_PQ/IVF_HNSW tuning (on top of Lance's built-in `IVF_FLAT` above `256` rows) keeps 768-d slice KNN sub-10 ms without scanning full tables.
