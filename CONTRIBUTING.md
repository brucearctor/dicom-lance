# Contributing to `dicom-lance`

Thank you for your interest in contributing to `dicom-lance`!

## Prerequisites

- **Rust** stable (`1.86+`) with `cargo` and `clippy`
- **Protocol Buffers compiler** (`protoc`, needed by `lance` build scripts)
- **Node.js** `22+` and `npm`

> Note: Keep pure-Rust dependencies in `Cargo.toml` so contributors do not need `cmake` or native C/C++ toolchains installed locally.

## Local Development Workflow

```bash
# Build the frontend bundle
cd web && npm install && npm run build && cd ..

# Run the backend serving web/dist on http://127.0.0.1:8090
cargo run -- --seed-on-start --port 8090 --data-dir ./data
```

For hot-reloading UI work, run `cargo run -- --seed-on-start` in one terminal and `cd web && npm run dev` in a second terminal (`vite` proxies `/api`, `/dicomweb`, and `/healthz` to `:8090` with COOP/COEP headers enabled).

## Test Suites

Run the full verification stack before opening a pull request:

```bash
# 1. Rust Hegel PBT (hegeltest) + integration + error-path tests
cargo test --locked
cargo clippy --all-targets --locked

# 2. Frontend TypeScript check, production build, and Vitest + @hegeldev/hegel PBT
cd web
npm run typecheck
npm run build
npm test

# 3. Hermetic Playwright E2E + axe-core WCAG 2.2 AA
npm run test:e2e

# 4. Live Rust + Lance v2.2 + Cornerstone3D WebGL2 E2E (uses compiled binary from ../target/debug)
cd .. && cargo build --locked
cd web && npm run test:e2e:live
```

## Coding & Commit Guidelines

- **Never commit real PHI/PII or patient DICOM datasets.** Use the built-in synthetic phantom generator (`src/phantom.rs`) or de-identified synthetic test objects.
- **No browser storage**: keep all viewer state and cached pixels strictly in memory (zero `localStorage`, `sessionStorage`, `IndexedDB`, or Cache Storage).
- Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `test:`, `docs:`, `ci:`, `chore:`).
