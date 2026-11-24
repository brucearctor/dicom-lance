# Python / AI Interop (`uv` + `pylance`)

Recompute and push 768-d multimodal embeddings directly into **`dicom_series.lance`** and **`dicom_frames.lance`** (`Lance v2.2` + `Blob V2`) from Python/NumPy/PyTorch while a `dicom-lance` server is live.

## Quickstart

```bash
cd examples/pylance-embeddings
uv sync

# Re-embed with pure-NumPy 768-d radiomics and invalidate live server dataset handles
uv run dicom-lance-embed --data-dir ../../data --reload-url http://127.0.0.1:8090
```

### Foundation Model & HTTP Embedding Backends

1. **BioMedCLIP (vision-language, optional heavy dependency group):**
   ```bash
   uv sync --extra biomedclip
   uv run dicom-lance-embed --backend biomedclip --data-dir ../../data --reload-url http://127.0.0.1:8090
   ```
2. **OpenAI-compatible `/v1/embeddings` (Ollama, vLLM, TEI, OpenAI):**
   ```bash
   uv run dicom-lance-embed --backend openai \
     --embedder-url http://127.0.0.1:11434/v1/embeddings \
     --model nomic-embed-text \
     --data-dir ../../data \
     --reload-url http://127.0.0.1:8090
   ```

## How It Works

1. **Zero-copy `Blob V2` reading (`loader.py`)**: Scans `dicom_frames.lance` metadata + `_rowid`, calls `frames_ds.take_blobs("pixel_frame_blob", ids=row_ids)`, and decodes Explicit VR Little Endian frames directly via `np.frombuffer` without copying raw pixel bytes.
2. **Columnar schema evolution (`writer.py`)**: Evolves ONLY `frame_embedding` / `series_embedding` via `drop_columns` + `add_columns(BatchUDF)`, leaving **`BlobKind::Packed`** `pixel_frame_blob` sidecar files untouched.
3. **Live cache reload**: Sends `POST /api/cache/invalidate` to the running Axum server so WADO-RS, Hybrid RRF Search, and **Similar Slices** immediately query the updated Lance versions.

## Tests & Linting

```bash
uv sync --locked
uv run ruff check
uv run pytest -v
```
