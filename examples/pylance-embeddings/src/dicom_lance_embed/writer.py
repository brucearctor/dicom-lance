"""Columnar Lance v2.2 embedding writer + live dicom-lance cache invalidation."""

from __future__ import annotations

import json
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import lance
import numpy as np
import pyarrow as pa

from dicom_lance_embed.embedders import EMBEDDING_DIM
from dicom_lance_embed.loader import iter_lance_frames

IVF_MIN_ROWS = 256


@dataclass(frozen=True)
class UpdateReport:
    series_updated: int
    frames_updated: int
    series_version: int
    frames_version: int
    reloaded_stats: dict[str, Any] | None = None


def invalidate_server_cache(reload_url: str, timeout: float = 10.0) -> dict[str, Any]:
    """Notify a running dicom-lance server to drop cached Arc<Dataset> handles."""
    url = f"{reload_url.rstrip('/')}/api/cache/invalidate"
    req = urllib.request.Request(url, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _update_embedding_column(
    ds: lance.LanceDataset,
    column_name: str,
    embeddings_by_rowid: dict[int, np.ndarray],
    anchor_column: str,
) -> lance.LanceDataset:
    """Replace an embedding column cleanly via metadata drop + BatchUDF without rewriting blobs."""
    if column_name in ds.schema.names:
        ds.drop_columns([column_name])

    def _populate_batch(batch: pa.RecordBatch) -> pa.RecordBatch:
        rids = batch["_rowid"].to_pylist()
        flat: list[np.ndarray] = []
        for rid in rids:
            vec = embeddings_by_rowid.get(int(rid))
            if vec is None:
                raise KeyError(f"missing computed embedding for _rowid={rid}")
            flat.append(np.asarray(vec, dtype=np.float32).reshape(EMBEDDING_DIM))
        values = pa.array(np.concatenate(flat), type=pa.float32())
        fixed = pa.FixedSizeListArray.from_arrays(values, EMBEDDING_DIM)
        return pa.RecordBatch.from_arrays([fixed], names=[column_name])

    ds.add_columns(_populate_batch, read_columns=[anchor_column, "_rowid"])
    return lance.dataset(ds.uri)


def _maybe_rebuild_vector_index(
    ds: lance.LanceDataset,
    column_name: str,
    index_name: str,
) -> None:
    if ds.count_rows() < IVF_MIN_ROWS:
        return
    partitions = max(4, min(64, ds.count_rows() // 64))
    ds.create_index(
        column_name,
        index_type="IVF_FLAT",
        name=index_name,
        metric="cosine",
        replace=True,
        num_partitions=partitions,
    )


def update_embeddings(
    data_dir: str | Path,
    embedder: Any,
    batch_size: int = 32,
    reload_url: str | None = None,
) -> UpdateReport:
    """Read Blob V2 frames via pylance, recompute 768-d vectors, update Lance, and reload server."""
    lance_root = Path(data_dir) / "lance"
    series_ds = lance.dataset(lance_root / "dicom_series.lance")
    frames_ds = lance.dataset(lance_root / "dicom_frames.lance")

    series_rows = (
        series_ds.scanner(
            columns=["study_instance_uid", "series_instance_uid", "searchable_text"],
            with_row_id=True,
        )
        .to_table()
        .to_pylist()
    )

    series_text_map: dict[tuple[str, str], str] = {}
    for srow in series_rows:
        series_text_map[(srow["study_instance_uid"], srow["series_instance_uid"])] = str(
            srow["searchable_text"]
        )

    frame_embeddings_by_rowid: dict[int, np.ndarray] = {}
    frame_embeddings_by_series: dict[tuple[str, str], list[np.ndarray]] = {}

    for meta, pixels in iter_lance_frames(frames_ds, batch_size=batch_size):
        key = (meta["study_instance_uid"], meta["series_instance_uid"])
        vec = embedder.embed_frame(pixels, meta, series_text_map.get(key, ""))
        frame_embeddings_by_rowid[int(meta["_rowid"])] = vec
        frame_embeddings_by_series.setdefault(key, []).append(vec)

    series_embeddings_by_rowid: dict[int, np.ndarray] = {}
    for srow in series_rows:
        key = (srow["study_instance_uid"], srow["series_instance_uid"])
        svec = embedder.embed_series(
            str(srow["searchable_text"]),
            frame_embeddings_by_series.get(key, []),
        )
        series_embeddings_by_rowid[int(srow["_rowid"])] = svec

    if frame_embeddings_by_rowid:
        frames_ds = _update_embedding_column(
            frames_ds,
            "frame_embedding",
            frame_embeddings_by_rowid,
            anchor_column="sop_instance_uid",
        )
        _maybe_rebuild_vector_index(frames_ds, "frame_embedding", "frame_embedding_ivf_flat")

    if series_embeddings_by_rowid:
        series_ds = _update_embedding_column(
            series_ds,
            "series_embedding",
            series_embeddings_by_rowid,
            anchor_column="series_instance_uid",
        )
        _maybe_rebuild_vector_index(series_ds, "series_embedding", "series_embedding_ivf_flat")

    reloaded_stats = invalidate_server_cache(reload_url) if reload_url else None
    return UpdateReport(
        series_updated=len(series_embeddings_by_rowid),
        frames_updated=len(frame_embeddings_by_rowid),
        series_version=int(series_ds.version),
        frames_version=int(frames_ds.version),
        reloaded_stats=reloaded_stats,
    )
