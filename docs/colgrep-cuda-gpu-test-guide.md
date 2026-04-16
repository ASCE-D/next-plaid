# ColGREP CUDA GPU Integration Test Guide

**Target machine:** Linux Mint, NVIDIA GPU with CUDA, 16GB RAM
**Purpose:** Validate colgrep build, indexing, search, and daemon with CUDA GPU acceleration
**Repo:** `https://github.com/ASCE-D/next-plaid.git`
**Branch:** `feat/colgrep-daemon-with-local-changes`

---

## Prerequisites

Before starting, ensure the following are installed:

- Rust toolchain (rustup + cargo)
- NVIDIA driver (nvidia-smi should work)
- CUDA toolkit (nvcc should work)
- cuDNN library
- git
- curl
- python3 (for json formatting)

---

## Phase 0: Environment Verification

Verify CUDA is accessible before building anything.

```bash
# 1. Check NVIDIA driver
nvidia-smi
# Expected: Shows GPU info, driver version, CUDA version
# Record: GPU model, driver version, CUDA version

# 2. Check CUDA compiler
nvcc --version
# Expected: Shows CUDA compilation tools version

# 3. Check cuDNN
# Look for libcudnn in standard paths
find /usr/lib /usr/local/lib /usr/lib/x86_64-linux-gnu -name "libcudnn*" 2>/dev/null | head -5
# Expected: At least one libcudnn.so file found
# If not found, install cuDNN: https://developer.nvidia.com/cudnn

# 4. Check CUDA libraries are linkable
ldconfig -p | grep -i cuda | head -10
ldconfig -p | grep -i cudnn | head -5
# Expected: Shows libcudart, libcublas, libcudnn entries

# 5. Check available GPU memory
nvidia-smi --query-gpu=memory.total,memory.free --format=csv,noheader
# Record the output

# 6. Verify Rust is installed
rustc --version
cargo --version
# If not installed: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**STOP if any of steps 1-4 fail.** Fix CUDA/cuDNN installation before proceeding.

---

## Phase 1: Clone and Build

```bash
# 1. Clone the repo
git clone https://github.com/ASCE-D/next-plaid.git
cd next-plaid
git checkout feat/colgrep-daemon-with-local-changes

# 2. Verify branch
git log --oneline -5
# Expected: Latest commit should be "feat: show encoding progress on wave bar in chunked mode"

# 3. Build with CUDA + OpenBLAS features
#    This will take a few minutes on first build
time cargo build --release -p colgrep --features "cuda,openblas" 2>&1 | tee /tmp/build-cuda.log
# Expected: "Finished release profile" with no errors
# Record: build time

# 4. If build fails with CUDA linking errors, try setting paths:
# export CUDA_PATH=/usr/local/cuda
# export LD_LIBRARY_PATH=/usr/local/cuda/lib64:$LD_LIBRARY_PATH
# Then retry the build

# 5. Copy binary for convenience
cp target/release/colgrep /tmp/colgrep-gpu

# 6. Verify binary
/tmp/colgrep-gpu --version
# Expected: colgrep 1.2.0

/tmp/colgrep-gpu serve --help
# Expected: Shows serve subcommand help with all options

# 7. Record binary size
ls -lh /tmp/colgrep-gpu
```

---

## Phase 2: CUDA Runtime Verification

Test that CUDA is actually being used during inference, not silently falling back to CPU.

```bash
# 1. Create a small test project
mkdir -p /tmp/cuda-test-project/src
for i in $(seq 1 20); do
  cat > /tmp/cuda-test-project/src/mod_${i}.rs << 'RUSTEOF'
/// Memory allocation helper
pub fn allocate(size: usize) -> Vec<u8> {
    vec![0u8; size]
}
RUSTEOF
done

# 2. Index with --force-gpu (will FAIL if CUDA is not working)
/tmp/colgrep-gpu --force-gpu init -y /tmp/cuda-test-project 2>&1 | tee /tmp/cuda-verify.log
# Expected: Should succeed WITHOUT "CUDA support not compiled" error
# If you see "CUDA support not compiled" -> binary was not built with cuda feature
# If you see "GPU encoding failed" -> CUDA libs not found at runtime

# 3. Check the log for runtime info
grep -i "runtime\|cuda\|gpu\|cpu\|provider" /tmp/cuda-verify.log
# Expected: Should mention CUDA or GPU runtime, NOT "CPU"

# 4. Search to verify the index works
/tmp/colgrep-gpu "memory allocation" /tmp/cuda-test-project -k 3
# Expected: Returns results from the test files

# 5. Clean up test project index
/tmp/colgrep-gpu clear /tmp/cuda-test-project
```

**If step 2 fails with CUDA errors:** The binary was built with CUDA support but the runtime can't find the libraries. Try:
```bash
export LD_LIBRARY_PATH=/usr/local/cuda/lib64:/usr/lib/x86_64-linux-gnu:$LD_LIBRARY_PATH
```
Then retry.

---

## Phase 3: GPU vs CPU Indexing Benchmark

Compare GPU and CPU indexing performance on a real codebase. Use the next-plaid repo itself as the test corpus.

```bash
# Use the repo itself as test data (~500 source files)
PROJECT_DIR=$(pwd)

# --- CPU Baseline ---
echo "=== CPU Indexing ==="
/tmp/colgrep-gpu clear "$PROJECT_DIR" 2>/dev/null
time COLGREP_FORCE_CPU=1 /tmp/colgrep-gpu init -y "$PROJECT_DIR" 2>&1 | tee /tmp/index-cpu.log
# Record: time, file count, unit count

/tmp/colgrep-gpu status "$PROJECT_DIR"

# Quick search test
/tmp/colgrep-gpu "memory allocation" "$PROJECT_DIR" -k 3

# --- GPU ---
echo "=== GPU Indexing ==="
/tmp/colgrep-gpu clear "$PROJECT_DIR"
time /tmp/colgrep-gpu --force-gpu init -y "$PROJECT_DIR" 2>&1 | tee /tmp/index-gpu.log
# Record: time, file count, unit count

/tmp/colgrep-gpu status "$PROJECT_DIR"

# Quick search test
/tmp/colgrep-gpu "memory allocation" "$PROJECT_DIR" -k 3

echo ""
echo "=== Compare ==="
echo "CPU time: $(grep real /tmp/index-cpu.log 2>/dev/null || echo 'check above')"
echo "GPU time: $(grep real /tmp/index-gpu.log 2>/dev/null || echo 'check above')"
```

---

## Phase 4: Chunked Indexing Test (Memory Bounded)

Test the new `--chunked` flag to verify it works with CUDA.

```bash
PROJECT_DIR=$(pwd)

# Clear and re-index with chunked mode
/tmp/colgrep-gpu clear "$PROJECT_DIR"
time /tmp/colgrep-gpu --force-gpu init -y --chunked --chunk-files 100 "$PROJECT_DIR" 2>&1 | tee /tmp/index-chunked-gpu.log
# Expected: Shows "Wave N:" progress messages
# Should complete without OOM

# Verify search
/tmp/colgrep-gpu "thread safety" "$PROJECT_DIR" -k 5
# Expected: Returns relevant results

# Check index size
du -sh ~/Library/Application\ Support/colgrep/indices/* 2>/dev/null || \
du -sh ~/.local/share/colgrep/indices/* 2>/dev/null || \
du -sh "${XDG_DATA_HOME:-$HOME/.local/share}/colgrep/indices/"* 2>/dev/null
```

---

## Phase 5: Daemon Testing with CUDA

```bash
PROJECT_DIR=$(pwd)

# Make sure index exists (from Phase 3 or 4)
/tmp/colgrep-gpu status "$PROJECT_DIR"

# Start daemon with GPU
/tmp/colgrep-gpu serve \
  --port 9090 \
  --index "$PROJECT_DIR" \
  --sessions 4 \
  --force-gpu \
  2>&1 > /tmp/serve-cuda.log &
SERVE_PID=$!
echo "Daemon PID: $SERVE_PID"
sleep 15  # wait for model load + prewarm

# --- Health ---
echo "=== Health ==="
curl -s http://localhost:9090/health | python3 -m json.tool
# Expected: status: "ready", non-zero index_doc_count

# --- Basic search ---
echo "=== Basic Search ==="
curl -s -X POST http://localhost:9090/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "memory allocation", "top_k": 5}' | python3 -m json.tool
# Expected: results array with 5 items, search_time_ms present

# --- Target paths ---
echo "=== Target Paths ==="
curl -s -X POST http://localhost:9090/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "embedding model", "top_k": 5, "target_paths": ["next-plaid-onnx/src"]}' | python3 -m json.tool
# Expected: all results from next-plaid-onnx/src/

# --- Include patterns ---
echo "=== Include Patterns ==="
curl -s -X POST http://localhost:9090/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "test", "top_k": 5, "include_patterns": ["*.rs"]}' | python3 -m json.tool
# Expected: all results are .rs files

# --- Code only ---
echo "=== Code Only ==="
curl -s -X POST http://localhost:9090/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "error handling", "top_k": 5, "code_only": true}' | python3 -m json.tool

# --- Error cases ---
echo "=== Error: Empty Query ==="
curl -s -w "\nHTTP_CODE: %{http_code}\n" -X POST http://localhost:9090/search \
  -H 'Content-Type: application/json' \
  -d '{"query": ""}'
# Expected: HTTP 400, "query must not be empty"

echo ""
echo "=== Error: Missing Query ==="
curl -s -w "\nHTTP_CODE: %{http_code}\n" -X POST http://localhost:9090/search \
  -H 'Content-Type: application/json' \
  -d '{}'
# Expected: HTTP 400, "missing required field: query"

# --- Latency benchmark (10 runs) ---
echo "=== Latency Benchmark (10 runs) ==="
for i in $(seq 1 10); do
  curl -s -o /dev/null -w "%{time_total}\n" -X POST http://localhost:9090/search \
    -H 'Content-Type: application/json' \
    -d '{"query": "file system operations", "top_k": 10}'
done | tee /tmp/latency-cuda.txt

echo ""
echo "=== Latency Stats ==="
awk '{sum+=$1; if(NR==1||$1<min)min=$1; if($1>max)max=$1} END{printf "min=%.3fs max=%.3fs avg=%.3fs\n", min, max, sum/NR}' /tmp/latency-cuda.txt

# --- Reload ---
echo "=== Reload ==="
curl -s -X POST http://localhost:9090/reload \
  -H 'Content-Type: application/json' | python3 -m json.tool
# Expected: status: "reloaded", doc_count > 0

# Stop daemon
kill $SERVE_PID 2>/dev/null
echo "Daemon stopped"
```

---

## Phase 6: NFS Storage Redirect

```bash
mkdir -p /tmp/colgrep-nfs-test
PROJECT_DIR=$(pwd)

# Index to alternate location
time XDG_DATA_HOME=/tmp/colgrep-nfs-test /tmp/colgrep-gpu --force-gpu init -y "$PROJECT_DIR" 2>&1 | tee /tmp/index-nfs-cuda.log

# Verify index location
ls -la /tmp/colgrep-nfs-test/colgrep/indices/
du -sh /tmp/colgrep-nfs-test/colgrep/indices/*

# Search from redirected index
XDG_DATA_HOME=/tmp/colgrep-nfs-test /tmp/colgrep-gpu "memory allocation" "$PROJECT_DIR" -k 3

# Daemon from redirected index
XDG_DATA_HOME=/tmp/colgrep-nfs-test /tmp/colgrep-gpu serve \
  --port 9091 \
  --index "$PROJECT_DIR" \
  --sessions 2 \
  --force-gpu \
  2>&1 &
NFS_PID=$!
sleep 10

curl -s http://localhost:9091/health | python3 -m json.tool
curl -s -X POST http://localhost:9091/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "memory allocation", "top_k": 3}' | python3 -m json.tool

kill $NFS_PID 2>/dev/null

# Cleanup
rm -rf /tmp/colgrep-nfs-test
```

---

## Phase 7: Run Unit Tests

```bash
# Run all colgrep tests
cargo test -p colgrep 2>&1 | tee /tmp/test-results.log
# Expected: All tests pass

# Run chunked indexing integration tests specifically
cargo test -p colgrep --test chunked_indexing -- --nocapture 2>&1
# Expected: 2 tests pass
```

---

## Phase 8: Generate Results Report

After completing all phases, collect results into a report.

```bash
echo "========================================="
echo "  ColGREP CUDA GPU Test Results"
echo "========================================="
echo ""
echo "Date: $(date)"
echo "Hostname: $(hostname)"
echo "OS: $(cat /etc/os-release | grep PRETTY_NAME | cut -d= -f2)"
echo "Kernel: $(uname -r)"
echo ""
echo "--- GPU Info ---"
nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
echo ""
echo "--- CUDA Version ---"
nvcc --version | tail -1
echo ""
echo "--- cuDNN ---"
find /usr/lib /usr/local/lib /usr/lib/x86_64-linux-gnu -name "libcudnn.so.*" 2>/dev/null | head -1
echo ""
echo "--- Binary ---"
/tmp/colgrep-gpu --version
ls -lh /tmp/colgrep-gpu | awk '{print "Size:", $5}'
echo ""
echo "--- Build Log (last 5 lines) ---"
tail -5 /tmp/build-cuda.log
echo ""
echo "--- CPU Indexing ---"
tail -3 /tmp/index-cpu.log
echo ""
echo "--- GPU Indexing ---"
tail -3 /tmp/index-gpu.log
echo ""
echo "--- Chunked GPU Indexing ---"
tail -3 /tmp/index-chunked-gpu.log
echo ""
echo "--- Daemon Latency (GPU) ---"
cat /tmp/latency-cuda.txt 2>/dev/null
awk '{sum+=$1; if(NR==1||$1<min)min=$1; if($1>max)max=$1} END{printf "min=%.3fs max=%.3fs avg=%.3fs\n", min, max, sum/NR}' /tmp/latency-cuda.txt 2>/dev/null
echo ""
echo "--- Test Results ---"
tail -5 /tmp/test-results.log
echo ""
echo "========================================="
```

---

## Verification Checklist

| # | Check | Expected | Pass/Fail |
|---|-------|----------|-----------|
| 1 | `nvidia-smi` works | Shows GPU info | |
| 2 | Build with `cuda,openblas` succeeds | No errors | |
| 3 | `--force-gpu init` succeeds | No CUDA errors | |
| 4 | GPU indexing faster than CPU | GPU time < CPU time | |
| 5 | `--chunked --force-gpu` works | Completes, shows waves | |
| 6 | Daemon `/health` returns 200 | `status: ready` | |
| 7 | Daemon `/search` returns results | `search_time_ms` present | |
| 8 | Error cases return 400 not 500 | HTTP 400 on empty/missing query | |
| 9 | NFS redirect works | Index at alternate path | |
| 10 | All unit tests pass | `cargo test` green | |
| 11 | Search latency < 1s (GPU daemon) | avg < 1000ms | |

---

## Troubleshooting

**"CUDA support not compiled"**
Binary was built without `--features cuda`. Rebuild with `cargo build --release -p colgrep --features "cuda,openblas"`.

**"GPU encoding failed" / CUDA runtime error**
- Check `LD_LIBRARY_PATH` includes CUDA lib dir
- Run `ldconfig -p | grep cuda` to verify libraries are findable
- Ensure NVIDIA driver version matches CUDA toolkit version

**"cuDNN not found, encoding will use CPU"**
- Install cuDNN matching your CUDA version
- Ensure libcudnn.so is in a path ldconfig knows about
- Run `sudo ldconfig` after installing cuDNN

**Build fails with linker errors**
```bash
export CUDA_PATH=/usr/local/cuda
export LD_LIBRARY_PATH=$CUDA_PATH/lib64:$LD_LIBRARY_PATH
export PATH=$CUDA_PATH/bin:$PATH
```

**OOM during indexing**
Use chunked mode: `--chunked --chunk-files 5000`. With 16GB RAM + GPU, each wave should stay under 4GB.
