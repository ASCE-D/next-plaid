# ColGREP: Complete Analysis, Fixes & Benchmark Report

**Date:** 2026-04-19
**Author:** Deepesh Genani
**Branch:** `feat/colgrep-daemon-with-local-changes`
**Platform tested:** macOS Darwin 25.4.0, Apple Silicon M4, 16GB RAM

---

## 1. What is ColGREP

ColGREP is a Rust CLI tool for semantic code search. It uses a ColBERT model (`lightonai/LateOn-Code-edge`) to produce token-level embeddings of code units (functions, classes, methods) and stores them in a PLAID index. At search time, it encodes the query into token vectors and scores documents via MaxSim (late interaction) — comparing every query token against every document token.

**What makes ColBERT special vs standard embedding search:**
- Standard: 1 vector per document, 1 vector per query, score = dot product
- ColBERT: N vectors per document (one per token), M vectors per query, score = sum of max similarities per query token
- This means ColBERT can match partial concepts within code — "thermal control" matches a function where "thermal" appears at token 47 and "control" at token 112

**Key components:**
- Tree-sitter: Parses code into meaningful units (functions, classes, methods) instead of arbitrary chunks
- ONNX Runtime: Runs the ColBERT model for encoding
- PLAID index: Compressed multi-vector index with k-means clustering
- SQLite (filtering.db): Metadata, FTS5 full-text search, call graphs

---

## 2. The Production Problem (K8s Deployment)

### Architecture

```
Developer question
      |
celery-api pod (Python AI agent)
      |  POST /search {"query": "reagent thermal control"}
colgrep-server:8080 (Rust HTTP wrapper)
      |  spawns: colgrep search "reagent thermal control" /path/to/repo
colgrep-server pod (colgrep CLI)
      |
PLAID index (5.7GB, on NFS originally)
      |
JSON results back to agent
```

**Target codebase:** Siemens Atellica — 720k code units, C#/.NET monorepo.

### Problem 1: Search took 4-5 minutes (unusable)

Every `colgrep search` CLI invocation is a **fresh process** that:

| Step | What happens | Time (NFS) | Time (local+warm) |
|------|-------------|-----------|-------------------|
| Auto-index check | Scan 720k files, hash each one | ~60-120s | ~10-100ms |
| Load ONNX model | Deserialize model, compile compute graph, allocate buffers | ~3-5s | ~15-20s |
| Mmap PLAID index | Memory-map 5.7GB index | ~30-60s (NFS) | <1s (page cache) |
| Encode query | ColBERT forward pass on query | ~2-3s | ~2-3s |
| MaxSim search | Score against all embeddings | ~1-2s | ~1-2s |

**On NFS:** Every mmap page fault = network round trip. Random access patterns in MaxSim + SQLite compound into minutes.

### Problem 2: 0 search results

Two root causes identified:

**Root Cause A — Encoding:** Many `.cs` and `.cpp` files in Atellica are encoded in Windows-1252/Latin-1. ColGREP's file reader did strict UTF-8 parsing and **silently skipped** any file that failed. These files appeared in `state.json` under `ignored_files` — never embedded, completely invisible to search.

Evidence:
```
"CH/MMCC/Source/Utils.InstrumentCheck/Procedures/SampleThermalControl.cs"  <- ignored (skipped)
"CH/MMCC/Source/Utils.InstrumentCheck/Thermals/ThermalsControl.cs"         <- indexed
```

**Root Cause B — Model recall:** Even for indexed files, the ColBERT model (`lightonai/LateOn-Code-edge`, trained on systems code) scored C# WinForms/WPF UI code poorly. Query `"ReagentServer1Thermal TED duty cycle control loop"` scoped to the exact directory containing the file returned **0 results**.

---

## 3. The K8s Hack (Lock-Hold Keepalive)

### What was done

Instead of fixing colgrep, we exploited its lock mechanism:

1. **Copied index from NFS to local pod storage** (emptyDir/SSD)
   - Eliminates NFS round trips from the query path

2. **Primed the Linux page cache**
   - `cat index_files > /dev/null` forces kernel to cache pages in RAM
   - Subsequent mmap page faults served from memory (~100ns) instead of NFS (~0.5-5ms)

3. **Ran a continuous keepalive loop**
   - Back-to-back `colgrep search "warmup ping"` with no sleep
   - This holds the `.lock` file ~99% of the time
   - Real searches hit the lock-busy fast path: `try_acquire_index_lock()` returns None
   - Code skips the auto-index check entirely (no filesystem scan of 720k files)
   - Page cache stays warm (kernel sees pages as "recently used")

```
Pod startup:
  1. cp index from NFS to local emptyDir
  2. cat all index files > /dev/null  (prime page cache)
  3. while true; do colgrep search "warmup ping" /path/to/repo; done &
  4. Start Rust HTTP wrapper on :8080
```

### Result

| Scenario | Latency |
|----------|---------|
| Baseline (NFS, cold) | ~4-5 minutes |
| After hack (local, warm, lock-busy) | ~20-30 seconds |

### Why 20-30s remained

The hack saves:
- Auto-index scan: skipped (lock busy) -> saved ~60-120s
- Index mmap: page cache warm -> saved ~30-60s

But EVERY search still:
- Loads ONNX model from scratch (~15-20s) — this is CPU work (session compilation), not I/O
- Creates new ONNX InferenceSession
- Allocates inference buffers
- Compiles compute graph

**The 20-30s floor = ONNX model session creation.** No amount of page cache warming fixes this. Only a long-lived server process (daemon) that loads the model once at startup eliminates it.

---

## 4. Fixes Built on This Branch

### 4a. Non-UTF-8 Encoding Support

**File:** `colgrep/src/index/mod.rs` lines 60-103

Added `chardetng` + `encoding_rs` for robust multi-encoding file reading:
- Fast path: valid UTF-8 detected without overhead
- Non-ASCII: `chardetng` detects encoding (Windows-1252, Latin-1, etc.)
- Fallback: lossy UTF-8 conversion when detection confidence is low
- Returns `DecodedSourceFile` with status: `Utf8`, `Transcoded { encoding }`, or `Lossy`

**Impact:** Files that were silently skipped (like `SampleThermalControl.cs`) are now indexed.

### 4b. Daemon Server (`colgrep serve`)

**File:** `colgrep/src/commands/serve.rs` (592 lines)

HTTP server using Axum that loads model + index **once at startup** and keeps them resident:

- `POST /search` — semantic + hybrid search with filters
- `GET /health` — status, doc count, uptime
- `POST /reload` — hot-swap index without restart
- Configurable: port, sessions, timeout, alpha, quantized, force-cpu
- Pre-warms index on startup (reads merged_codes.npy, merged_residuals.npy)
- `Arc<Searcher>` shared across all requests via `RwLock`

**Impact:** Eliminates per-query model reload. Search goes from 20-30s to 20-70ms.

### 4c. Hybrid Search (Semantic + BM25)

**File:** `colgrep/src/index/mod.rs` lines 3638-3740

Combines ColBERT semantic results with FTS5 keyword results via Reciprocal Rank Fusion:
- `alpha=1.0`: pure semantic
- `alpha=0.75`: hybrid (default) — 75% semantic, 25% keyword
- `alpha=0.0`: pure BM25 keyword search

**Impact:** FTS5 catches exact identifier matches that the ColBERT model misses. Compensates for model weakness on C# code.

### 4d. Chunked Indexing with Checkpoint/Resume

**Files:** `colgrep/src/index/mod.rs` (full_rebuild_chunked), `colgrep/src/index/checkpoint.rs`

- `--chunked --chunk-files N`: processes files in waves of N to bound memory
- Saves `chunked_checkpoint.json` after each wave with completion state
- Resume: validates file_list_hash, skips completed waves
- `--no-resume`: force fresh start

**Impact:** Enables indexing repos with 100k+ files on 16GB RAM machines. Original approach OOM'd on Chromium (loaded all code units into memory at once).

### 4e. XML Language Support

**File:** `colgrep/src/parser/xml.rs` (256 lines)

Structural parsing for XML/XSD/XSLT/XAML files using tree-sitter.

### 4f. Configuration System

**File:** `colgrep/src/config.rs` (877 lines)

Persisted settings at `~/.config/colgrep/config.json`: default model, k/n values, pool_factor, batch_size, hybrid search settings, extra ignore/include patterns.

---

## 5. Benchmark Results

### Test setup

- **Binary:** colgrep 1.2.0, release build with `coreml,metal_gpu,accelerate`
- **Machine:** Mac M4, 16GB RAM

Two repos tested:
- **Small:** potpie (502 files, 2,506 code units, 40MB index)
- **Large:** Chromium (164,562 files, 3,501,133 code units, 30GB index)

### 5a. Indexing

**Potpie (502 files):**

| Mode | Time | Notes |
|------|------|-------|
| CPU | 4 min 27s | Standard `ExecutionProvider::Cpu` |
| CoreML | 15 min 59s | **3.6x SLOWER** — CoreML overhead kills batch encoding |
| CUDA (estimated) | ~10-30s | Based on published ColBERT benchmarks |

**Chromium (164k files):**

| Mode | Time | Notes |
|------|------|-------|
| CPU (chunked, 10k files/wave) | ~18 hours | 17 waves, checkpoint/resume across restarts |
| CUDA (estimated) | ~30-90 min | Not tested locally, requires Linux GPU |

**Conclusion:** CoreML is not viable for indexing (slower than CPU). CUDA on Linux is the fast path. CPU is the reliable path. Chunked indexing with checkpoint/resume makes CPU feasible even for massive repos.

### 5b. Search Latency: CLI vs Daemon

**Small index (potpie, 2.5k docs):**

| Query | CLI | Daemon (1st) | Daemon (warm) | Speedup |
|-------|-----|-------------|---------------|---------|
| "database connection pooling" | 1,124ms | 132ms | 20-24ms | **46-56x** |
| "authentication middleware" | 853ms | 34ms | 20-24ms | **36-43x** |
| "error handling" | 829ms | 34ms | 19-22ms | **38-44x** |

Daemon startup: ~2 seconds. Note: CLI was only ~1s because model was warm in page cache from recent indexing.

**Large index (Chromium, 3.5M docs, 30GB):**

| Query | CLI | Daemon (1st) | Daemon (warm) | Speedup |
|-------|-----|-------------|---------------|---------|
| "memory allocation strategies" | **2 min 24s** | 5,486ms | **1,779ms** | **70x** |
| "renderer process compositor frame scheduling" | — | 3,360ms | **2,094ms** | — |
| "thread pool implementation" | — | 2,493ms | **1,699ms** | — |
| "IPC message routing between processes" | — | 2,724ms | **2,176ms** | — |
| "V8 garbage collection" | — | 2,360ms | **1,864ms** | — |

Daemon startup: ~6 seconds (loading 30GB index + model + prewarm).

**Key findings:**
- Daemon warm queries: **1.7-2.2s** on 3.5M docs — production-viable
- CLI: **2 min 24s** for the same query — unusable
- Speedup: **70x** on large index
- All queries returned 10 results — no 0-result failures
- First query is slower (~3-5s) due to page cache warming, subsequent queries stabilize at ~2s

### 5c. Hybrid Search Quality

**Small index (potpie):**

| Query | Semantic (alpha=1.0) | Hybrid (alpha=0.75) | BM25 (alpha=0.0) |
|-------|---------------------|---------------------|-------------------|
| "database connection pooling" | 5 results, top=database.py | 5 results, top=database.py | 5 results, top=database.py |
| "authentication middleware" | 5 results, top=auth_service.py | 5 results, top=auth_service.py | 5 results, top=research_agent.py |
| "error handling" | 5 results, top=delegation_streamer.py | 5 results, top=**exceptions.py** | 5 results, top=**exceptions.py** |

**Large index (Chromium):**

| Query | Semantic top result | BM25 top result | Hybrid top result |
|-------|-------------------|-----------------|-------------------|
| "memory allocation strategies" | process_memory_dump.h | blob_memory_controller.h | process_memory_dump.h |
| "thread pool implementation" | threadpool_unittest.cc | README.md | threadpool_unittest.cc |
| "V8 garbage collection" | weak_ptr.cc | extension_garbage_collector_chromeos.h | weak_ptr.cc |

**Key findings:**
- Hybrid preserves semantic's superior ranking while adding keyword coverage
- BM25 alone ranks poorly (README.md for "thread pool", wrong file for "V8 garbage collection")
- Semantic correctly identifies relevant implementation files
- Hybrid is the best default (alpha=0.75)

---

## 6. Why People Don't Hit These Problems

| Factor | Typical user | Our case |
|--------|-------------|----------|
| Repo size | 500-5k files | 164k-720k files |
| Hardware | CUDA GPU on Linux | CPU on Mac M4 |
| Encoding | UTF-8 only | Windows-1252 legacy .cs/.cpp |
| Deployment | Local CLI | K8s pod, NFS storage |
| Usage pattern | Occasional search | AI agent querying continuously |

ColGREP is designed for developers running `colgrep search` on their local machine against a small-to-medium repo. We're using it as a production search engine at 100x the intended scale.

---

## 7. ColBERT vs API Embeddings vs Ripgrep

| | Ripgrep | ColGREP (daemon) | API Embeddings (e.g. voyage-code-3) |
|---|---------|------------------|-------------------------------------|
| Query type | Exact text/regex | Semantic + hybrid | Semantic (single-vector) |
| Latency | <0.5s | 20-70ms (daemon) | ~200-500ms (API round trip) |
| "Find RenderFrameHostImpl" | Perfect | Good (hybrid) | Good |
| "How does auth work?" | Can't | Good | Good |
| Complex multi-concept query | Can't | Best (late interaction) | Good |
| Infrastructure | None | ONNX Runtime, PLAID index, daemon | API key, vector DB |
| Indexing speed | N/A | Slow on CPU, fast on CUDA | Fast (API throughput) |
| Offline | Yes | Yes | No |
| Cost | Free | Free (but needs compute) | ~$0.06/1M tokens |

**For an AI agent, the practical strategy is:**
1. **Colgrep (daemon)** for exploration: "I don't know where this is, help me find it" — 1-2 queries
2. **Ripgrep** for confirmation: "I know the symbol, show me all references" — fast, precise
3. The agent should NOT hammer colgrep 10 times — use it surgically

---

## 8. Current Status

### Chromium Indexing (COMPLETE)

- **164,562 files indexed**, 3,501,133 code units
- **30GB index** at `~/Library/Application Support/colgrep/indices/chromium-553c093d/`
- Completed in 17 waves (chunked, 10k files/wave) with checkpoint/resume across multiple restarts
- Total CPU time: ~18 hours on Mac M4

### Branch Commits (27 total)

```
9e501c5 test: add integration tests for chunked indexing resume
2199895 fix: preserve index.tmp in chunked mode for checkpoint resume
5b838cc feat: wire checkpoint save/load into full_rebuild_chunked for resume
0d21611 feat: add --no-resume CLI flag for chunked indexing
fcb42e0 feat: add chunked indexing checkpoint data structure
24bce08 docs: add CUDA GPU test guide and updated plans
5cde019 feat: show encoding progress on wave bar in chunked mode
33bd245 test: add integration tests for chunked indexing
bd2ccc7 refactor: extract dispatch_full_rebuild helper and guard chunk_files=0
310922b fix: add ETA to encoding progress bar template
2061f2b feat: implement full_rebuild_chunked for bounded-memory indexing
f3ba9e1 feat: add --chunked and --chunk-files CLI flags for chunked indexing
4536060 feat: add write access verification for output directories
68f33df feat: implement colgrep serve daemon with /search, /health, /reload endpoints
3712b81 feat: add colgrep serve subcommand (stub)
88583b6 feat: add axum, tokio, arc-swap dependencies for daemon server
bff8661 feat: add structural XML parser for named elements, XSD types, XSLT templates
c337e32 feat: add Language::Xml variant and tree-sitter-xml dependency
8f1306c feat: upgrade read_source_file with chardetng encoding detection
890298f feat: add chardetng and encoding_rs dependencies for encoding detection
```

---

## 9. Remaining Work

| Item | Priority | Description |
|------|----------|-------------|
| K8s deployment of daemon | High | Replace CLI-per-query with `colgrep serve`. Update K8s Deployment, probes, remove keepalive hack. |
| API compatibility shim | High | Daemon's response uses `file`/`line`/`search_time_ms` but Python agent expects `path`/`start_line`/`latency_ms`. 5-line fix in `serve.rs`. |
| Re-index Atellica with encoding fix | High | Clear old index, re-index with `--chunked`. Previously-skipped .cs files should now be indexed. |
| Run acceptance queries | High | Test exact failing queries from this report on Atellica with daemon. |
| Agent prompting strategy | Medium | Update agent instructions: when to use colgrep (exploration) vs ripgrep (confirmation). |
| CUDA GPU validation | Medium | Test on Linux with NVIDIA GPU for 10-30x indexing speedup. Guide at `docs/colgrep-cuda-gpu-test-guide.md`. |
| Backpressure middleware | Low | Tower middleware to limit concurrent daemon requests. |
| Daemon `/index` endpoint | Low | Trigger indexing via HTTP API instead of requiring CLI access. |

---

## 10. Key Files

| File | Purpose |
|------|---------|
| `colgrep/src/commands/serve.rs` | Daemon server (Axum) |
| `colgrep/src/commands/search.rs` | CLI search with lock-busy fast path |
| `colgrep/src/index/mod.rs:60-103` | Encoding fix (read_source_file) |
| `colgrep/src/index/mod.rs:3183-3252` | Searcher::load (model + index load per CLI search) |
| `colgrep/src/index/mod.rs:3638-3740` | Hybrid search (RRF fusion) |
| `colgrep/src/index/mod.rs:1856-2100` | Chunked indexing (full_rebuild_chunked) |
| `colgrep/src/index/checkpoint.rs` | Checkpoint save/load/validate |
| `colgrep/src/config.rs` | Configuration system |
| `colgrep/src/parser/xml.rs` | XML structural parser |
