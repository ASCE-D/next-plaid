# =============================================================================
# ColGREP Dockerfile
# =============================================================================
# Supports two build variants using --target:
#   1. runtime-cpu (default): ColGREP daemon on CPU
#   2. runtime-cuda: GPU-accelerated encoding with CUDA
#
# Build examples:
#   docker build -t colgrep -f colgrep/Dockerfile .
#   docker build -t colgrep:cuda -f colgrep/Dockerfile --target runtime-cuda .
#
# Run examples:
#   # Index a repo (mount it in + mount persistent index storage)
#   docker run -v /path/to/repo:/repo -v colgrep-indices:/data/indices \
#     colgrep init -y /repo --chunked --chunk-files 10000
#
#   # Start daemon (serve mode)
#   docker run -p 8080:8080 -v /path/to/repo:/repo -v colgrep-indices:/data/indices \
#     colgrep serve --port 8080 --index /repo --sessions 4
#
#   # With CUDA GPU
#   docker run --gpus all -p 8080:8080 -v /path/to/repo:/repo -v colgrep-indices:/data/indices \
#     colgrep:cuda serve --port 8080 --index /repo --sessions 4
#
# Environment variables:
#   XDG_DATA_HOME: Override index storage location (default: /data/indices)
#   COLGREP_FORCE_CPU: Force CPU execution provider
#   COLGREP_FORCE_GPU: Force CUDA execution provider (cuda build only)
# =============================================================================

# =============================================================================
# Chef stage - CPU
# =============================================================================
FROM debian:bookworm-slim AS chef-cpu

RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    wget \
    && if [ "$(uname -m)" = "aarch64" ]; then apt-get install -y libopenblas-dev; fi \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-chef

# Download ONNX Runtime CPU
ARG ORT_VERSION=1.23.0
RUN mkdir -p /opt/ort_cpu && \
    ARCH=$(uname -m) && \
    if [ "$ARCH" = "aarch64" ]; then ORT_ARCH="aarch64"; else ORT_ARCH="x64"; fi && \
    wget -q https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}.tgz && \
    tar -xzf onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}.tgz && \
    cp -r onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}/lib/* /opt/ort_cpu/ && \
    rm -rf onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}*

ENV ORT_DYLIB_PATH=/opt/ort_cpu/libonnxruntime.so.${ORT_VERSION}
ENV LD_LIBRARY_PATH=/opt/ort_cpu:${LD_LIBRARY_PATH}

WORKDIR /app

# =============================================================================
# Planner - CPU
# =============================================================================
FROM chef-cpu AS planner-cpu

COPY Cargo.toml Cargo.lock ./
COPY next-plaid ./next-plaid
COPY next-plaid-api ./next-plaid-api
COPY next-plaid-onnx ./next-plaid-onnx
COPY colgrep ./colgrep

RUN cargo chef prepare --recipe-path recipe.json

# =============================================================================
# Builder - CPU
# =============================================================================
FROM chef-cpu AS builder-cpu

COPY --from=planner-cpu /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json --package colgrep

COPY Cargo.toml Cargo.lock ./
COPY next-plaid ./next-plaid
COPY next-plaid-api ./next-plaid-api
COPY next-plaid-onnx ./next-plaid-onnx
COPY colgrep ./colgrep

RUN cargo build --release --package colgrep

# =============================================================================
# Chef stage - CUDA
# =============================================================================
FROM nvidia/cuda:12.4.1-cudnn-devel-ubuntu22.04 AS chef-cuda

RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    libopenblas-dev \
    wget \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-chef

# Download ONNX Runtime GPU
ARG ORT_VERSION=1.23.0
RUN mkdir -p /opt/ort_gpu && \
    wget -q https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-x64-gpu-${ORT_VERSION}.tgz && \
    tar -xzf onnxruntime-linux-x64-gpu-${ORT_VERSION}.tgz && \
    cp -r onnxruntime-linux-x64-gpu-${ORT_VERSION}/lib/* /opt/ort_gpu/ && \
    rm -rf onnxruntime-linux-x64-gpu-${ORT_VERSION}*

ENV ORT_DYLIB_PATH=/opt/ort_gpu/libonnxruntime.so.${ORT_VERSION}
ENV LD_LIBRARY_PATH=/opt/ort_gpu:${LD_LIBRARY_PATH}

WORKDIR /app

# =============================================================================
# Planner - CUDA
# =============================================================================
FROM chef-cuda AS planner-cuda

COPY Cargo.toml Cargo.lock ./
COPY next-plaid ./next-plaid
COPY next-plaid-api ./next-plaid-api
COPY next-plaid-onnx ./next-plaid-onnx
COPY colgrep ./colgrep

RUN cargo chef prepare --recipe-path recipe.json

# =============================================================================
# Builder - CUDA
# =============================================================================
FROM chef-cuda AS builder-cuda

COPY --from=planner-cuda /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json --package colgrep --features cuda

COPY Cargo.toml Cargo.lock ./
COPY next-plaid ./next-plaid
COPY next-plaid-api ./next-plaid-api
COPY next-plaid-onnx ./next-plaid-onnx
COPY colgrep ./colgrep

RUN cargo build --release --package colgrep --features cuda

# =============================================================================
# Runtime - CPU (default target)
# =============================================================================
FROM debian:bookworm-slim AS runtime-cpu

WORKDIR /app

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    libsqlite3-0 \
    curl \
    && if [ "$(uname -m)" = "aarch64" ]; then apt-get install -y libopenblas0; fi \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash colgrep

RUN mkdir -p /data/indices && chown -R colgrep:colgrep /data

COPY --from=builder-cpu /opt/ort_cpu /opt/ort_cpu
COPY --from=builder-cpu /app/target/release/colgrep /usr/local/bin/colgrep

ARG ORT_VERSION=1.23.0
ENV ORT_DYLIB_PATH=/opt/ort_cpu/libonnxruntime.so.${ORT_VERSION}
ENV LD_LIBRARY_PATH=/opt/ort_cpu:${LD_LIBRARY_PATH}
ENV XDG_DATA_HOME=/data/indices
ENV OPENBLAS_NUM_THREADS=1

USER colgrep

EXPOSE 8080

HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -f --max-time 5 http://localhost:8080/health || exit 1

ENTRYPOINT ["colgrep"]
CMD ["serve", "--host", "0.0.0.0", "--port", "8080", "--sessions", "4"]

# =============================================================================
# Runtime - CUDA
# =============================================================================
FROM nvidia/cuda:12.4.1-cudnn-runtime-ubuntu22.04 AS runtime-cuda

WORKDIR /app

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    libsqlite3-0 \
    libopenblas0 \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash colgrep

RUN mkdir -p /data/indices && chown -R colgrep:colgrep /data

COPY --from=builder-cuda /opt/ort_gpu /opt/ort_gpu
COPY --from=builder-cuda /app/target/release/colgrep /usr/local/bin/colgrep

ARG ORT_VERSION=1.23.0
ENV ORT_DYLIB_PATH=/opt/ort_gpu/libonnxruntime.so.${ORT_VERSION}
ENV LD_LIBRARY_PATH=/opt/ort_gpu:/usr/local/cuda/lib64:${LD_LIBRARY_PATH}
ENV XDG_DATA_HOME=/data/indices
ENV OPENBLAS_NUM_THREADS=1

USER colgrep

EXPOSE 8080

HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -f --max-time 5 http://localhost:8080/health || exit 1

ENTRYPOINT ["colgrep"]
CMD ["serve", "--host", "0.0.0.0", "--port", "8080", "--sessions", "4"]
