"""Unit and Rust <-> Python Lance v2.2 Blob V2 interoperability tests."""

from __future__ import annotations

import http.server
import json
import os
import shutil
import socket
import struct
import subprocess
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

import lance
import numpy as np
import pytest

from dicom_lance_embed import (
    EMBEDDING_DIM,
    BioMedClipEmbedder,
    OpenAICompatibleEmbedder,
    RadiomicsEmbedder,
    decode_frame_pixels,
    l2_normalize,
    update_embeddings,
    window_to_uint8,
)

CT_STUDY_UID = "1.2.826.0.1.3680043.10.5432.1001"
CT_SERIES_UID = "1.2.826.0.1.3680043.10.5432.1001.1"
CT_DIM = 192


def _find_dicom_lance_binary() -> Path:
    env_bin = os.environ.get("DICOM_LANCE_BIN")
    if env_bin:
        candidate = Path(env_bin)
        if candidate.is_file():
            return candidate

    repo_root = Path(__file__).resolve().parents[3]
    exe = "dicom-lance.exe" if os.name == "nt" else "dicom-lance"
    for profile in ("debug", "release"):
        candidate = repo_root / "target" / profile / exe
        if candidate.is_file():
            return candidate

    which_bin = shutil.which("dicom-lance")
    if which_bin:
        return Path(which_bin)

    raise FileNotFoundError(
        "dicom-lance binary not found in target/debug or target/release; "
        "run `cargo build --locked` from the repository root first"
    )


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait_health(base_url: str, proc: subprocess.Popen[bytes], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"dicom-lance exited early with code {proc.returncode}")
        try:
            with urllib.request.urlopen(f"{base_url}/healthz", timeout=1.0) as resp:
                if resp.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            time.sleep(0.1)
    raise TimeoutError(f"Timed out waiting for {base_url}/healthz")


def _get_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10.0) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _extract_first_multipart_part(body: bytes, boundary: str = "dicom-lance-boundary") -> bytes:
    delim = f"--{boundary}".encode("ascii")
    start = body.find(delim)
    assert start != -1, "opening multipart boundary missing"
    after = start + len(delim)
    if body[after : after + 2] == b"\r\n":
        after += 2
    hdr_end = body.find(b"\r\n\r\n", after)
    assert hdr_end != -1, "multipart header terminator missing"
    payload_start = hdr_end + 4
    end_delim = f"\r\n--{boundary}".encode("ascii")
    payload_end = body.find(end_delim, payload_start)
    assert payload_end != -1, "closing multipart boundary missing"
    return body[payload_start:payload_end]


def test_decode_frame_pixels_and_windowing() -> None:
    raw_u16 = struct.pack("<4H", 0, 1024, 1062, 244)
    hu = decode_frame_pixels(
        raw_bytes=raw_u16,
        rows=2,
        columns=2,
        samples_per_pixel=1,
        bits_allocated=16,
        pixel_representation=0,
        rescale_slope=1.0,
        rescale_intercept=-1024.0,
    )
    assert hu.shape == (2, 2)
    np.testing.assert_allclose(hu, np.array([[-1024.0, 0.0], [38.0, -780.0]], dtype=np.float32))

    raw_i16 = struct.pack("<2h", -100, 100)
    signed = decode_frame_pixels(raw_i16, 1, 2, 1, 16, 1)
    np.testing.assert_allclose(signed, np.array([[-100.0, 100.0]], dtype=np.float32))

    raw_u8 = bytes([10, 128, 250, 0])
    u8_arr = decode_frame_pixels(raw_u8, 2, 2, 1, 8, 0)
    assert u8_arr.shape == (2, 2)

    u8_win = window_to_uint8(hu, window_center=-600.0, window_width=1500.0)
    assert u8_win.dtype == np.uint8
    assert u8_win[0, 0] == 55
    assert u8_win[1, 0] > u8_win[1, 1]

    clamp_check = window_to_uint8(
        np.array([[-2000.0, 500.0]], dtype=np.float32),
        window_center=-600.0,
        window_width=1500.0,
    )
    assert clamp_check[0, 0] == 0
    assert clamp_check[0, 1] == 255

    with pytest.raises(ValueError, match="truncated"):
        decode_frame_pixels(b"\x00\x01", 2, 2, 1, 16, 0)
    with pytest.raises(ValueError, match="unsupported"):
        decode_frame_pixels(b"\x00" * 16, 2, 2, 1, 24, 0)


def test_l2_normalize_and_radiomics_embedder() -> None:
    deg = l2_normalize(np.array([np.nan, np.inf, 0.0, 0.0], dtype=np.float32))
    assert deg.shape == (4,)
    assert np.isclose(np.linalg.norm(deg), 1.0)
    assert deg[0] == 1.0

    embedder = RadiomicsEmbedder()
    pixels = np.linspace(-900.0, 200.0, 32 * 32, dtype=np.float32).reshape((32, 32))
    meta = {
        "window_center": -600.0,
        "window_width": 1500.0,
        "slice_thickness": 2.5,
        "pixel_spacing_row_mm": 1.25,
        "pixel_spacing_col_mm": 1.25,
    }
    v1 = embedder.embed_frame(pixels, meta, "CT Chest Pulmonary Nodule")
    v2 = embedder.embed_frame(pixels, meta, "CT Chest Pulmonary Nodule")
    assert v1.shape == (EMBEDDING_DIM,)
    assert v1.dtype == np.float32
    np.testing.assert_array_equal(v1, v2)
    assert np.isclose(np.linalg.norm(v1), 1.0, atol=1e-4)

    q = embedder.embed_text("nodular lung lesion")
    assert q.shape == (EMBEDDING_DIM,)
    assert np.isclose(np.linalg.norm(q), 1.0, atol=1e-4)


def test_openai_compatible_and_biomedclip_guards() -> None:
    captured: dict[str, object] = {}

    class _MockHandler(http.server.BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length).decode("utf-8"))
            captured["auth"] = self.headers.get("Authorization")
            captured["model"] = body.get("model")
            vec = [0.0] * 16
            vec[0] = 3.0
            vec[1] = 4.0
            out = json.dumps({"data": [{"embedding": vec}]}).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)

        def log_message(self, format: str, *args: object) -> None:
            return

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _MockHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        host, port = server.server_address
        client = OpenAICompatibleEmbedder(
            endpoint_url=f"http://{host}:{port}/v1/embeddings",
            api_key="py-secret",
            model="biomed-embed",
        )
        emb = client.embed_text("spiculated nodule")
        assert captured["auth"] == "Bearer py-secret"
        assert captured["model"] == "biomed-embed"
        assert emb.shape == (EMBEDDING_DIM,)
        assert np.isclose(np.linalg.norm(emb), 1.0, atol=1e-4)
        assert np.isclose(emb[0], 0.6, atol=1e-4)
        assert np.isclose(emb[1], 0.8, atol=1e-4)
    finally:
        server.shutdown()
        server.server_close()

    try:
        import open_clip  # noqa: F401
    except ImportError:
        with pytest.raises(ImportError, match="biomedclip"):
            BioMedClipEmbedder()


def test_rust_seed_python_blob_read_embed_write_and_live_reload(tmp_path: Path) -> None:
    bin_path = _find_dicom_lance_binary()
    port = _free_port()
    base_url = f"http://127.0.0.1:{port}"
    proc = subprocess.Popen(
        [
            str(bin_path),
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--data-dir",
            str(tmp_path),
            "--seed-on-start",
        ]
    )
    try:
        _wait_health(base_url, proc)
        stats_before = _get_json(f"{base_url}/api/stats")
        v_series_before = stats_before["series_dataset"]["version"]
        v_frames_before = stats_before["frames_dataset"]["version"]
        assert stats_before["frames_dataset"]["storage_version"] == "2.2"
        assert stats_before["blob_v2_kind_sample"] == "Packed"

        # 1. Read Rust-written Lance v2.2 Blob V2 frame directly from Python via pylance
        frames_ds = lance.dataset(tmp_path / "lance" / "dicom_frames.lance")
        slice23_meta = (
            frames_ds.scanner(
                columns=[
                    "rows",
                    "columns",
                    "samples_per_pixel",
                    "bits_allocated",
                    "pixel_representation",
                    "rescale_slope",
                    "rescale_intercept",
                ],
                filter=(
                    f"study_instance_uid = '{CT_STUDY_UID}' "
                    f"AND series_instance_uid = '{CT_SERIES_UID}' "
                    "AND instance_number = 23"
                ),
                with_row_id=True,
            )
            .to_table()
            .to_pylist()
        )
        assert len(slice23_meta) == 1
        meta23 = slice23_meta[0]
        blob = frames_ds.take_blobs("pixel_frame_blob", ids=[int(meta23["_rowid"])])[0]
        assert blob is not None
        with blob as f:
            raw23 = f.read()

        hu23 = decode_frame_pixels(
            raw_bytes=raw23,
            rows=int(meta23["rows"]),
            columns=int(meta23["columns"]),
            samples_per_pixel=int(meta23["samples_per_pixel"]),
            bits_allocated=int(meta23["bits_allocated"]),
            pixel_representation=int(meta23["pixel_representation"]),
            rescale_slope=float(meta23["rescale_slope"]),
            rescale_intercept=float(meta23["rescale_intercept"]),
        )
        assert hu23.shape == (CT_DIM, CT_DIM)
        assert hu23[86, 51] == pytest.approx(38.0)
        assert hu23[96, 64] == pytest.approx(-780.0)

        # 2. Compute & write 768-d embeddings from Python and notify Rust server to reload handles
        report = update_embeddings(
            data_dir=tmp_path,
            embedder=RadiomicsEmbedder(),
            batch_size=32,
            reload_url=base_url,
        )
        assert report.series_updated == 3
        assert report.frames_updated == 112
        assert report.reloaded_stats is not None
        assert report.reloaded_stats["series_dataset"]["version"] > v_series_before
        assert report.reloaded_stats["frames_dataset"]["version"] > v_frames_before
        assert report.reloaded_stats["frames_dataset"]["storage_version"] == "2.2"
        assert report.reloaded_stats["blob_v2_kind_sample"] == "Packed"

        # 3. Verify live Rust WADO-RS, Cosine KNN Similar Slices, and Vector search use updated data
        nodule_sop = f"{CT_SERIES_UID}.23"
        wado_url = (
            f"{base_url}/dicomweb/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}"
            f"/instances/{nodule_sop}/frames/1"
        )
        with urllib.request.urlopen(wado_url, timeout=10.0) as resp:
            wado_body = resp.read()
        part_bytes = _extract_first_multipart_part(wado_body)
        assert len(part_bytes) == CT_DIM * CT_DIM * 2
        stored_core = int.from_bytes(
            part_bytes[(86 * CT_DIM + 51) * 2 : (86 * CT_DIM + 51) * 2 + 2],
            "little",
        )
        assert stored_core - 1024 == 38

        sim_json = _get_json(
            f"{base_url}/api/studies/{CT_STUDY_UID}/series/{CT_SERIES_UID}"
            f"/instances/{nodule_sop}/frames/1/similar?limit=5"
        )
        hits = sim_json["hits"]
        assert len(hits) == 5
        for hit in hits:
            assert hit["series_instance_uid"] == CT_SERIES_UID
            assert hit["sop_instance_uid"] != nodule_sop
            assert hit["similarity"] > 0.85

        vec_json = _get_json(f"{base_url}/api/search?q=nodular&mode=vector")
        assert vec_json["studies"]
        assert vec_json["studies"][0]["patient_name"] == "Rivera^Elena"
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
