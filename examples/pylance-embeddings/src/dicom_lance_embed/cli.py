"""CLI entry point for updating dicom-lance embeddings from Python."""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import sys

from dicom_lance_embed.embedders import (
    BioMedClipEmbedder,
    OpenAICompatibleEmbedder,
    RadiomicsEmbedder,
)
from dicom_lance_embed.writer import update_embeddings


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="dicom-lance-embed",
        description="Compute 768-d embeddings over Lance v2.2 DICOM Blob V2 frames via pylance.",
    )
    parser.add_argument(
        "--data-dir",
        default="./data",
        help="Root dicom-lance data directory containing lance/dicom_series.lance and dicom_frames.lance",
    )
    parser.add_argument(
        "--backend",
        choices=("radiomics", "openai", "biomedclip"),
        default="radiomics",
        help="Embedding backend (default: pure-NumPy radiomics)",
    )
    parser.add_argument(
        "--embedder-url",
        default=os.environ.get("EMBEDDER_URL", "http://127.0.0.1:11434/v1/embeddings"),
        help="OpenAI-compatible /v1/embeddings URL (used when --backend openai)",
    )
    parser.add_argument(
        "--embedder-api-key",
        default=os.environ.get("EMBEDDER_API_KEY"),
        help="Optional Bearer API key for --backend openai",
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("EMBEDDER_MODEL", "text-embedding-3-small"),
        help="Model identifier for openai or biomedclip backends",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=32,
        help="Rows per Lance scan / take_blobs batch",
    )
    parser.add_argument(
        "--reload-url",
        default=None,
        help="Optional running dicom-lance URL (e.g. http://127.0.0.1:8090) to invalidate cached handles",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.backend == "radiomics":
        embedder = RadiomicsEmbedder()
    elif args.backend == "openai":
        embedder = OpenAICompatibleEmbedder(
            endpoint_url=args.embedder_url,
            api_key=args.embedder_api_key,
            model=args.model,
        )
    else:
        embedder = BioMedClipEmbedder(
            model_name=args.model
            if args.model != "text-embedding-3-small"
            else "hf-hub:microsoft/BiomedCLIP-PubMedBERT_256-vit_base_patch16_224"
        )

    report = update_embeddings(
        data_dir=args.data_dir,
        embedder=embedder,
        batch_size=args.batch_size,
        reload_url=args.reload_url,
    )
    json.dump(dataclasses.asdict(report), sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
