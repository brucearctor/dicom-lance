# Stage 1: Build TypeScript + React 19 + Cornerstone3D frontend
FROM node:22-bookworm-slim AS web-builder
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# Stage 2: Prepare cargo-chef dependency recipe
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Cache-cook dependencies and build release Rust binary
FROM chef AS rust-builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json --bin dicom-lance
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin dicom-lance

# Stage 3: Minimal non-root runtime image
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 10001 -s /usr/sbin/nologin dicom \
    && mkdir -p /data /app/web/dist \
    && chown -R dicom:dicom /data /app

COPY --from=rust-builder /build/target/release/dicom-lance /usr/local/bin/dicom-lance
COPY --from=web-builder /build/web/dist /app/web/dist

USER dicom
WORKDIR /app

ENV HOST=0.0.0.0 \
    PORT=8090 \
    DATA_DIR=/data \
    STATIC_DIR=/app/web/dist

VOLUME ["/data"]
EXPOSE 8090

ENTRYPOINT ["/usr/local/bin/dicom-lance"]
