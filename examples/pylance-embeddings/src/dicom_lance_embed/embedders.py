"""Multimodal 768-d embedders for dicom-lance datasets."""

from __future__ import annotations

import hashlib
import itertools
import json
import re
import urllib.error
import urllib.request
from typing import Any

import numpy as np

from dicom_lance_embed.loader import window_to_uint8

EMBEDDING_DIM = 768
TEXT_DIMS = 512
PIXEL_DIMS = 256
_GRID = 64
_TOKEN_RE = re.compile(r"[A-Za-z0-9]+")


def l2_normalize(vec: np.ndarray) -> np.ndarray:
    """Sanitize non-finite entries and normalize to a float32 unit vector."""
    arr = np.asarray(vec, dtype=np.float64).reshape(-1)
    if arr.size == 0:
        raise ValueError("embedding vector must not be empty")
    arr = np.where(np.isfinite(arr), arr, 0.0)
    norm = float(np.linalg.norm(arr))
    if norm > 1e-12 and np.isfinite(norm):
        out = (arr / norm).astype(np.float32)
        return np.where(np.isfinite(out), out, np.float32(0.0))
    fallback = np.zeros(arr.shape[0], dtype=np.float32)
    fallback[0] = 1.0
    return fallback


def _fit_768(vec: np.ndarray) -> np.ndarray:
    arr = np.asarray(vec, dtype=np.float64).reshape(-1)
    if arr.size == 0:
        raise ValueError("embedding vector must not be empty")
    out = np.zeros(EMBEDDING_DIM, dtype=np.float64)
    n = min(arr.size, EMBEDDING_DIM)
    out[:n] = arr[:n]
    return l2_normalize(out)


def _hash_feature(out: np.ndarray, namespace: str, token: str, weight: float) -> None:
    digest = hashlib.sha256(f"{namespace}:{token}".encode()).digest()
    bucket = int.from_bytes(digest[:2], "little") % TEXT_DIMS
    out[bucket] += weight


def hash_text_features(text: str) -> np.ndarray:
    """512-d non-negative SHA-256 n-gram/prefix projection matching Rust LocalMultimodalEmbedder."""
    out = np.zeros(TEXT_DIMS, dtype=np.float64)
    tokens = [m.group(0).lower() for m in _TOKEN_RE.finditer(text)]
    if not tokens:
        return out.astype(np.float32)

    for tok in tokens:
        _hash_feature(out, "u", tok, 1.0)
        if len(tok) >= 3:
            _hash_feature(out, "p3", tok[:3], 0.45)
        if len(tok) >= 4:
            _hash_feature(out, "p4", tok[:4], 0.65)
            for i in range(len(tok) - 3):
                _hash_feature(out, "c4", tok[i : i + 4], 0.30)

    for left, right in itertools.pairwise(tokens):
        _hash_feature(out, "b", f"{left}_{right}", 0.75)

    norm = float(np.linalg.norm(out))
    if norm > 1e-12 and np.isfinite(norm):
        out = out / norm
    return out.astype(np.float32)


def extract_pixel_radiomics(pixels: np.ndarray, meta: dict[str, Any]) -> np.ndarray:
    """Extract deterministic 256-d histogram, spatial, radial, and gradient radiomics."""
    out = np.zeros(PIXEL_DIMS, dtype=np.float64)
    arr = np.asarray(pixels, dtype=np.float64)
    if arr.ndim == 3:
        arr = arr[:, :, 0]
    if arr.ndim != 2 or arr.shape[0] <= 0 or arr.shape[1] <= 0:
        return out.astype(np.float32)

    arr = np.where(np.isfinite(arr), arr, 0.0)
    rows, cols = arr.shape
    ys = (np.arange(_GRID) * rows) // _GRID
    xs = (np.arange(_GRID) * cols) // _GRID
    raw_grid = arr[np.ix_(ys, xs)]

    wc = float(meta.get("window_center", 40.0))
    ww = float(meta.get("window_width", 400.0))
    if not (np.isfinite(wc) and np.isfinite(ww) and ww > 1.0):
        min_v = float(np.min(raw_grid))
        max_v = float(np.max(raw_grid))
        if max_v > min_v:
            wc = (min_v + max_v) * 0.5
            ww = max(max_v - min_v, 1.0)
        else:
            wc, ww = 40.0, 400.0

    norm_grid = np.clip((raw_grid - (wc - ww * 0.5)) / max(ww, 1.0), 0.0, 1.0)

    # 0..64: 64-bin intensity histogram
    bins = np.clip(np.rint(norm_grid.ravel() * 63.0).astype(np.int64), 0, 63)
    out[:64] = np.bincount(bins, minlength=64).astype(np.float64) / (_GRID * _GRID)

    # 64..128: 4x4 spatial blocks (mean, std, min, max)
    blocks4 = norm_grid.reshape(4, _GRID // 4, 4, _GRID // 4).swapaxes(1, 2).reshape(16, -1)
    stats4 = np.stack(
        [blocks4.mean(axis=1), blocks4.std(axis=1), blocks4.min(axis=1), blocks4.max(axis=1)],
        axis=1,
    ).ravel()
    out[64:128] = stats4

    # 128..192: 8x8 spatial mean map
    blocks8 = norm_grid.reshape(8, _GRID // 8, 8, _GRID // 8).swapaxes(1, 2).reshape(64, -1)
    out[128:192] = blocks8.mean(axis=1)

    # 192..224: 16 radial ring means and stds
    center = (_GRID - 1) * 0.5
    max_r = max(float(np.sqrt(2.0 * center * center)), 1.0)
    gy, gx = np.indices((_GRID, _GRID), dtype=np.float64)
    r_bins = np.clip(
        np.floor((np.sqrt((gx - center) ** 2 + (gy - center) ** 2) / max_r) * 15.0).astype(
            np.int64
        ),
        0,
        15,
    )
    for r_idx in range(16):
        mask = r_bins == r_idx
        if np.any(mask):
            vals = norm_grid[mask]
            out[192 + r_idx] = float(vals.mean())
            out[208 + r_idx] = float(vals.std())

    # 224..240: 16-bin finite-difference gradient orientation histogram
    gx_grad = norm_grid[1:-1, 2:] - norm_grid[1:-1, :-2]
    gy_grad = norm_grid[2:, 1:-1] - norm_grid[:-2, 1:-1]
    mag = np.sqrt(gx_grad * gx_grad + gy_grad * gy_grad)
    valid_mag = mag > 1e-6
    grad_total = float(np.sum(mag[valid_mag]))
    if grad_total > 1e-9:
        ang = np.mod(np.arctan2(gy_grad[valid_mag], gx_grad[valid_mag]), 2.0 * np.pi)
        abins = np.clip(np.floor((ang / (2.0 * np.pi)) * 16.0).astype(np.int64), 0, 15)
        out[224:240] = np.bincount(abins, weights=mag[valid_mag], minlength=16) / grad_total

    # 240..256: global quantiles & acquisition geometry
    flat = norm_grid.ravel()
    out[240] = float(flat.mean())
    out[241] = float(flat.std())
    out[242:247] = np.quantile(flat, [0.10, 0.25, 0.50, 0.75, 0.90])
    out[247] = float(np.mean(flat > 0.05))
    out[248] = float(np.tanh(raw_grid.mean() / 1000.0))
    out[249] = float(np.tanh(raw_grid.std() / 1000.0))
    out[250] = float(np.tanh(wc / 1000.0))
    out[251] = float(np.tanh(ww / 2000.0))
    out[252] = float(np.tanh(float(meta.get("slice_thickness", 1.0)) / 10.0))
    out[253] = float(np.tanh(float(meta.get("pixel_spacing_row_mm", 1.0)) / 5.0))
    out[254] = float(np.tanh(float(meta.get("pixel_spacing_col_mm", 1.0)) / 5.0))
    out[255] = float(np.tanh(rows / 512.0))

    norm = float(np.linalg.norm(out))
    if norm > 1e-12 and np.isfinite(norm):
        out = out / norm
    return np.where(np.isfinite(out), out, 0.0).astype(np.float32)


class RadiomicsEmbedder:
    """Pure-NumPy deterministic 768-d multimodal DICOM + text embedder."""

    name = "python-radiomics-768"

    def embed_text(self, text: str) -> np.ndarray:
        combined = np.zeros(EMBEDDING_DIM, dtype=np.float32)
        combined[:TEXT_DIMS] = hash_text_features(text)
        return l2_normalize(combined)

    def embed_frame(self, pixels: np.ndarray, meta: dict[str, Any], series_text: str) -> np.ndarray:
        combined = np.zeros(EMBEDDING_DIM, dtype=np.float32)
        combined[:TEXT_DIMS] = hash_text_features(series_text) * np.float32(0.4359)
        combined[TEXT_DIMS:] = extract_pixel_radiomics(pixels, meta) * np.float32(0.90)
        return l2_normalize(combined)

    def embed_series(self, searchable_text: str, frame_embeddings: list[np.ndarray]) -> np.ndarray:
        combined = np.zeros(EMBEDDING_DIM, dtype=np.float32)
        combined[:TEXT_DIMS] = hash_text_features(searchable_text) * np.float32(0.85)
        if frame_embeddings:
            pixel_parts = np.mean([np.asarray(v[TEXT_DIMS:], dtype=np.float64) for v in frame_embeddings], axis=0)
            pnorm = float(np.linalg.norm(pixel_parts))
            if pnorm > 1e-12:
                combined[TEXT_DIMS:] = (pixel_parts / pnorm).astype(np.float32) * np.float32(0.5268)
        return l2_normalize(combined)


class OpenAICompatibleEmbedder:
    """OpenAI-compatible /v1/embeddings HTTP embedder fused with pixel radiomics."""

    name = "python-openai-http-768"

    def __init__(
        self,
        endpoint_url: str,
        api_key: str | None = None,
        model: str = "text-embedding-3-small",
        timeout: float = 30.0,
    ) -> None:
        self.endpoint_url = endpoint_url
        self.api_key = api_key
        self.model = model
        self.timeout = timeout
        self._text_cache: dict[str, np.ndarray] = {}

    def embed_text(self, text: str) -> np.ndarray:
        if text in self._text_cache:
            return self._text_cache[text]

        payload = json.dumps({"input": text, "model": self.model}).encode("utf-8")
        headers = {"Content-Type": "application/json"}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"

        req = urllib.request.Request(self.endpoint_url, data=payload, headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw_json = resp.read().decode("utf-8")
        except urllib.error.URLError as exc:
            raise RuntimeError(f"embedding request to {self.endpoint_url} failed: {exc}") from exc

        body = json.loads(raw_json)
        data = body.get("data") or []
        if not data or not isinstance(data[0].get("embedding"), list) or not data[0]["embedding"]:
            raise ValueError("embedding endpoint response missing non-empty data[0].embedding")

        raw_vec = np.asarray(data[0]["embedding"], dtype=np.float64)
        if not np.any(np.isfinite(raw_vec) & (raw_vec != 0.0)):
            raise ValueError("embedding endpoint returned degenerate all-zero/NaN vector")

        normed = _fit_768(raw_vec)
        self._text_cache[text] = normed
        return normed

    def embed_frame(self, pixels: np.ndarray, meta: dict[str, Any], series_text: str) -> np.ndarray:
        text_vec = self.embed_text(series_text) if series_text.strip() else np.zeros(EMBEDDING_DIM, dtype=np.float32)
        text_slice = text_vec[:TEXT_DIMS].astype(np.float64)
        tnorm = float(np.linalg.norm(text_slice))
        if tnorm > 1e-12:
            text_slice = text_slice / tnorm

        combined = np.zeros(EMBEDDING_DIM, dtype=np.float64)
        combined[:TEXT_DIMS] = text_slice * 0.4359
        combined[TEXT_DIMS:] = extract_pixel_radiomics(pixels, meta).astype(np.float64) * 0.90
        return l2_normalize(combined)

    def embed_series(self, searchable_text: str, frame_embeddings: list[np.ndarray]) -> np.ndarray:
        text_vec = self.embed_text(searchable_text)
        if not frame_embeddings:
            return text_vec
        mean_frame = np.mean([np.asarray(v, dtype=np.float64) for v in frame_embeddings], axis=0)
        return l2_normalize(0.75 * text_vec.astype(np.float64) + 0.25 * mean_frame)


class BioMedClipEmbedder:
    """Optional BioMedCLIP vision-language embedder (`uv sync --extra biomedclip`)."""

    name = "python-biomedclip-768"

    def __init__(
        self,
        model_name: str = "hf-hub:microsoft/BiomedCLIP-PubMedBERT_256-vit_base_patch16_224",
        device: str | None = None,
    ) -> None:
        try:
            import open_clip
            import torch
            from PIL import Image
        except ImportError as exc:
            raise ImportError(
                "BioMedClipEmbedder requires the optional 'biomedclip' extra. "
                "Install it with: uv sync --extra biomedclip"
            ) from exc

        self._torch = torch
        self._image_cls = Image
        self._device = device or ("cuda" if torch.cuda.is_available() else "cpu")
        self._model, _, self._preprocess = open_clip.create_model_and_transforms(
            model_name,
            device=self._device,
        )
        self._model.eval()
        self._tokenizer = open_clip.get_tokenizer(model_name)

    def embed_text(self, text: str) -> np.ndarray:
        tokens = self._tokenizer([text]).to(self._device)
        with self._torch.no_grad():
            feat = self._model.encode_text(tokens)[0].detach().cpu().numpy()
        return _fit_768(feat)

    def embed_frame(self, pixels: np.ndarray, meta: dict[str, Any], series_text: str) -> np.ndarray:
        gray = window_to_uint8(
            pixels,
            float(meta.get("window_center", 40.0)),
            float(meta.get("window_width", 400.0)),
        )
        img = self._image_cls.fromarray(gray, mode="L").convert("RGB")
        tensor = self._preprocess(img).unsqueeze(0).to(self._device)
        with self._torch.no_grad():
            img_feat = self._model.encode_image(tensor)[0].detach().cpu().numpy()
        if series_text.strip():
            txt_feat = self.embed_text(series_text)
            return l2_normalize(0.85 * _fit_768(img_feat).astype(np.float64) + 0.15 * txt_feat)
        return _fit_768(img_feat)

    def embed_series(self, searchable_text: str, frame_embeddings: list[np.ndarray]) -> np.ndarray:
        txt = self.embed_text(searchable_text)
        if not frame_embeddings:
            return txt
        mean_frame = np.mean([np.asarray(v, dtype=np.float64) for v in frame_embeddings], axis=0)
        return l2_normalize(0.60 * txt.astype(np.float64) + 0.40 * mean_frame)
