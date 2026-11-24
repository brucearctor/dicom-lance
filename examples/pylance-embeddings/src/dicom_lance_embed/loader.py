"""Zero-copy Lance v2.2 Blob V2 frame reader and DICOM pixel decoder."""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any

import lance
import numpy as np

FRAME_METADATA_COLUMNS: tuple[str, ...] = (
    "study_instance_uid",
    "series_instance_uid",
    "sop_instance_uid",
    "instance_number",
    "frame_number",
    "slice_sort_index",
    "rows",
    "columns",
    "samples_per_pixel",
    "bits_allocated",
    "pixel_representation",
    "rescale_slope",
    "rescale_intercept",
    "window_center",
    "window_width",
)

_DTYPE_BY_FORMAT: dict[tuple[int, bool], np.dtype[Any]] = {
    (8, False): np.dtype("<u1"),
    (8, True): np.dtype("<i1"),
    (16, False): np.dtype("<u2"),
    (16, True): np.dtype("<i2"),
    (32, False): np.dtype("<u4"),
    (32, True): np.dtype("<i4"),
}


def decode_frame_pixels(
    raw_bytes: bytes,
    rows: int,
    columns: int,
    samples_per_pixel: int,
    bits_allocated: int,
    pixel_representation: int,
    rescale_slope: float = 1.0,
    rescale_intercept: float = 0.0,
) -> np.ndarray:
    """Decode raw Explicit VR Little Endian pixel bytes into float32 rescaled intensities."""
    if rows <= 0 or columns <= 0 or samples_per_pixel <= 0:
        raise ValueError(
            f"invalid frame dimensions: rows={rows}, columns={columns}, spp={samples_per_pixel}"
        )
    dtype = _DTYPE_BY_FORMAT.get((bits_allocated, pixel_representation == 1))
    if dtype is None:
        raise ValueError(
            f"unsupported pixel format: bits_allocated={bits_allocated}, "
            f"pixel_representation={pixel_representation}"
        )

    bytes_per_sample = bits_allocated // 8
    sample_count = rows * columns * samples_per_pixel
    expected_bytes = sample_count * bytes_per_sample
    if len(raw_bytes) < expected_bytes:
        raise ValueError(
            f"pixel buffer truncated: got {len(raw_bytes)} bytes, expected at least {expected_bytes}"
        )

    # Zero-copy view over raw frame bytes, reshaped into native image axes.
    arr = np.frombuffer(raw_bytes, dtype=dtype, count=sample_count)
    if samples_per_pixel == 1:
        arr = arr.reshape((rows, columns))
    else:
        arr = arr.reshape((rows, columns, samples_per_pixel))

    slope = float(rescale_slope) if np.isfinite(rescale_slope) else 1.0
    intercept = float(rescale_intercept) if np.isfinite(rescale_intercept) else 0.0
    return arr.astype(np.float32) * np.float32(slope) + np.float32(intercept)


def window_to_uint8(
    values: np.ndarray,
    window_center: float,
    window_width: float,
) -> np.ndarray:
    """Apply DICOM VOI Window Center/Width and quantize cleanly to uint8 [0, 255]."""
    arr = np.asarray(values, dtype=np.float32)
    if arr.ndim == 3:
        arr = arr.mean(axis=2)

    finite = np.isfinite(arr)
    safe = np.where(finite, arr, 0.0)

    if np.isfinite(window_center) and np.isfinite(window_width) and window_width > 1.0:
        wc = float(window_center)
        ww = float(window_width)
    elif np.any(finite):
        min_v = float(np.min(safe[finite]))
        max_v = float(np.max(safe[finite]))
        if max_v > min_v:
            wc = (min_v + max_v) * 0.5
            ww = max(max_v - min_v, 1.0)
        else:
            wc, ww = 40.0, 400.0
    else:
        wc, ww = 40.0, 400.0

    lower = wc - ww * 0.5
    scaled = np.clip((safe - lower) * (255.0 / max(ww, 1.0)), 0.0, 255.0)
    return scaled.astype(np.uint8)


def iter_lance_frames(
    frames_ds: lance.LanceDataset,
    batch_size: int = 32,
) -> Iterator[tuple[dict[str, Any], np.ndarray]]:
    """Stream metadata rows + decoded pixel arrays via Lance take_blobs(ids=...)."""
    scanner = frames_ds.scanner(
        columns=list(FRAME_METADATA_COLUMNS),
        with_row_id=True,
        batch_size=max(1, batch_size),
    )
    for batch in scanner.to_batches():
        row_ids = batch["_rowid"].to_pylist()
        blobs = frames_ds.take_blobs("pixel_frame_blob", ids=row_ids)
        meta_rows = batch.to_pylist()

        for meta, blob in zip(meta_rows, blobs, strict=True):
            if blob is None:
                raise ValueError(
                    f"null pixel_frame_blob for SOP {meta['sop_instance_uid']} "
                    f"frame {meta['frame_number']} (rowid={meta['_rowid']})"
                )
            with blob as handle:
                raw_bytes = handle.read()
            pixels = decode_frame_pixels(
                raw_bytes=raw_bytes,
                rows=int(meta["rows"]),
                columns=int(meta["columns"]),
                samples_per_pixel=int(meta["samples_per_pixel"]),
                bits_allocated=int(meta["bits_allocated"]),
                pixel_representation=int(meta["pixel_representation"]),
                rescale_slope=float(meta["rescale_slope"]),
                rescale_intercept=float(meta["rescale_intercept"]),
            )
            yield meta, pixels
