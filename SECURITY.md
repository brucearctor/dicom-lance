# Security Policy

## Scope & Threat Model

`dicom-lance` is an experimental **research and development reference implementation** — it is not a certified medical device and **does not include built-in authentication, authorization, TLS termination, or audit logging**.

By default, the binary binds strictly to loopback (`127.0.0.1:8090`) and configures permissive CORS headers to support local DICOMweb + Cornerstone3D development.

If you deploy `dicom-lance` beyond `localhost` or point it at sensitive datasets:

- **Terminate TLS and enforce authentication/authorization** at an upstream reverse proxy or API gateway before exposing any `/api` or `/dicomweb` routes.
- **Restrict CORS origins** appropriately for your deployment domain.
- **Protect filesystem ingestion endpoints**: `/api/ingest/directory` reads `.dcm` files from server-side filesystem paths and must never be exposed to untrusted clients.
- **Do not use real Protected Health Information (PHI)** without appropriate HIPAA/GDPR safeguards, access controls, and de-identification pipelines.
- Note that DICOM structured reports (`dicom_series.lance`), headers (`dicom_json_header`), and pixel blobs (`pixel_frame_blob`) are persisted unencrypted under `DATA_DIR`. On the client side, Cornerstone3D caches remain strictly in RAM (`localStorage`, `sessionStorage`, and `IndexedDB` are never used).

## Reporting a Vulnerability

Please do **not** open a public GitHub issue for suspected security vulnerabilities.

Instead, use **GitHub Private Vulnerability Reporting** (`Security` tab -> `Report a vulnerability`) or contact the repository maintainers directly with:

1. A description of the issue and potential impact.
2. Steps to reproduce or a minimal proof of concept.
3. Any affected versions/commits and suggested mitigations, if known.

We aim to acknowledge reports within **5 business days** and coordinate disclosure once a fix is verified.
