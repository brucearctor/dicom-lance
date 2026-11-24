"""Python / AI ecosystem interop package for dicom-lance datasets."""

from dicom_lance_embed.embedders import (
    EMBEDDING_DIM,
    PIXEL_DIMS,
    TEXT_DIMS,
    BioMedClipEmbedder,
    OpenAICompatibleEmbedder,
    RadiomicsEmbedder,
    l2_normalize,
)
from dicom_lance_embed.loader import (
    decode_frame_pixels,
    iter_lance_frames,
    window_to_uint8,
)
from dicom_lance_embed.writer import (
    UpdateReport,
    invalidate_server_cache,
    update_embeddings,
)

__version__ = "0.2.0"

__all__ = [
    "EMBEDDING_DIM",
    "PIXEL_DIMS",
    "TEXT_DIMS",
    "BioMedClipEmbedder",
    "OpenAICompatibleEmbedder",
    "RadiomicsEmbedder",
    "UpdateReport",
    "__version__",
    "decode_frame_pixels",
    "invalidate_server_cache",
    "iter_lance_frames",
    "l2_normalize",
    "update_embeddings",
    "window_to_uint8",
]
