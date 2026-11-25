# Security Policy

## Scope & Threat Model

`dicom-lance` is an experimental **research and development reference implementation** — it is **not** FDA-cleared, CE-marked, or intended for primary clinical diagnosis, and it **does not terminate TLS or emit external audit logs**.

By default, the binary binds strictly to loopback (`127.0.0.1:8090`) and leaves authentication disabled for local Cornerstone3D development.

## Built-in Security Controls (v0.4.0-dev+)

When deploying beyond `localhost` or handling sensitive imaging workflows, enable the built-in hardening options alongside an upstream TLS reverse proxy:

- **Bearer Authentication (`--api-token` / `API_TOKEN`)**: Enforces fixed-time SHA-256 `Authorization: Bearer <token>` authentication across all `/api/*` and `/dicomweb/*` routes while keeping `/healthz` and the static SPA shell accessible. The web UI stores the unlock token strictly in RAM (Zero Browser Storage).
- **CORS Allowlist (`--cors-origin` / `CORS_ORIGINS`)**: Restricts cross-origin API/DICOMweb access to explicit `http://` or `https://` origins (`AllowOrigin::list`) instead of wildcard `*`.
- **Directory Ingestion Guard (`--disable-directory-ingest` / `ALLOW_DIRECTORY_INGEST=false`)**: `/api/ingest/directory` reads `.dcm` files from server-side paths. It is automatically **disabled (`403 Forbidden`)** whenever binding a non-loopback `--host` (such as `0.0.0.0`) unless explicitly re-enabled via `--allow-directory-ingest` / `ALLOW_DIRECTORY_INGEST=true`.
- **PS3.15 De-identification (`--deidentify` / `DEIDENTIFY=true`, `--deidentify-salt` / `DEIDENTIFY_SALT`, or `?deidentify=true`)**: Strips private tags, removes direct PHI / observer attributes, generalizes study/series dates, pseudonymizes patient IDs/names/accessions, and deterministically remaps Study/Series/SOP/FrameOfReference UIDs across multi-series `CT`/`SR`/`SEG` studies.
- **Deletion & Physical Purge (`DELETE /api/studies/{study}`, `POST /api/maintenance/compact`)**: Soft-deletes studies across both Lance datasets and compacts/cleans up old manifests and unreferenced `Blob V2` data files from disk. Note that stored Lance tables remain unencrypted at rest under `DATA_DIR`; use encrypted volumes/buckets and never place un-anonymized PHI on shared/untrusted hosts. On the client side, Cornerstone3D caches remain strictly in RAM (`localStorage`, `sessionStorage`, and `IndexedDB` are never used).

## Reporting a Vulnerability

Please do **not** open a public GitHub issue for suspected security vulnerabilities.

Instead, use **GitHub Private Vulnerability Reporting** (`Security` tab -> `Report a vulnerability`) or contact the repository maintainers directly with:

1. A description of the issue and potential impact.
2. Steps to reproduce or a minimal proof of concept.
3. Any affected versions/commits and suggested mitigations, if known.

We aim to acknowledge reports within **5 business days** and coordinate disclosure once a fix is verified.
