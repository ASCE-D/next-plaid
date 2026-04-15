# ColGREP Daemon (`colgrep serve`) Design Spec

## Problem Statement

ColGREP currently operates as a single-shot CLI tool. Every search invocation:

1. Spawns a new process
2. Loads the ONNX model (~2-3s)
3. Opens and maps the index (~1-2s)
4. Scans the entire filesystem and hashes every file to check for changes (~10-20s for 720k code units)
5. If any file changed, blocks on re-encoding and re-indexing (potentially minutes)
6. Encodes the query and searches (~1-3s)

For a 720k code unit / 6GB index codebase, this results in 30-40s per search even with warm OS cache, and unpredictable 4+ minute stalls when files change. This makes ColGREP unusable as a production search backend for AI agents.

## Goals

- **Consistent search latency of 2-5 seconds** for a 720k code unit / 6GB index
- **10-50 concurrent searches** without degradation
- **Clear health signaling** so AI agents can fall back to ripgrep when the daemon is unavailable
- **No repeated work** -- model, index, and connections loaded once, reused across all requests
- **Non-UTF8 encoding support** -- legacy Windows-1252 files indexed properly, not skipped
- **XML file support** -- structural parsing of XML/XSD/XSLT/XAML files
- **Hybrid search** -- BM25 keyword + ColBERT semantic via Reciprocal Rank Fusion (upstream v1.2.0)
- **Deployable as a K8s pod** or sidecar binary

## Non-Goals

- The daemon does NOT handle indexing triggers. External orchestration (your code, git hooks, CI) decides when to index.
- The daemon does NOT watch the filesystem.
- No authentication/authorization (runs inside trusted K8s network).
- No multi-tenancy (one daemon per codebase).

## Architecture

```
colgrep serve --port 8080 --index /path/to/index [--alpha 0.75]

+-----------------------------------------------+
|              colgrep serve                     |
|                                                |
|  +----------+   +--------------------------+  |
|  |  Axum    |   |  Shared State            |  |
|  |  HTTP    |-->|  +- ONNX Model Pool      |  |
|  |  Server  |   |  +- MmapIndex (ArcSwap)  |  |
|  +----------+   |  +- SQLite conn pool     |  |
|                  |  +- FTS5 index (hybrid)  |  |
|                  +--------------------------+  |
|                                                |
|  Endpoints:                                    |
|  POST /search     <-- AI agents call this      |
|  POST /reload     <-- after external indexing   |
|  GET  /health     <-- K8s probes               |
+-----------------------------------------------+
```

## Startup Sequence

Startup is blocking -- the daemon is not ready until all steps complete successfully. If any step fails, the daemon stays unhealthy with a clear error.

| Step | Action | Duration | Failure Mode |
|------|--------|----------|--------------|
| 1 | Load ONNX model, create N sessions (N=num configured) | ~2-3s | 503 + "model load failed: {reason}" |
| 2 | Open MmapIndex (mmap files, load IVF/codec into RAM) | ~1-2s | 503 + "index load failed: {reason}" |
| 3 | Open SQLite connection pool (filtering DB + FTS5 index) | ~100ms | 503 + "database open failed: {reason}" |
| 4 | Pre-warm mmap pages (sequential read of index files) | ~5-10s for 6GB | Non-fatal, log warning |
| 5 | Run throwaway search to warm ONNX runtime | ~1s | Non-fatal, log warning |
| 6 | Mark healthy, start accepting HTTP requests | immediate | -- |

### Pre-warming

Pre-warming is critical for consistent latency. Without it, the first searches trigger OS page faults as mmap'd data is pulled from disk, causing unpredictable latency spikes.

Implementation: sequential `madvise(MADV_SEQUENTIAL)` + read pass over `merged_codes.npy` and `merged_residuals.npy` at startup. This forces all index pages into OS cache.

### Fault Tolerance

If startup fails at any step, the daemon:
- Logs the error with full context
- Responds to `GET /health` with `503 {"status": "error", "reason": "..."}`
- Does NOT crash -- stays alive so K8s doesn't restart-loop
- Retries on the next `POST /reload` call

## API

### POST /search

Primary endpoint for AI agents.

**Request:**
```json
{
  "query": "authentication handler middleware",
  "top_k": 10,
  "alpha": 0.75,
  "include_patterns": ["*.py", "*.rs"],
  "exclude_patterns": ["*_test.py"],
  "exclude_dirs": ["vendor", "node_modules"],
  "text_pattern": "async def",
  "code_only": false
}
```

All fields except `query` are optional. Defaults:
- `top_k`: 10
- `alpha`: 0.75 (hybrid search balance, 0.0 = pure BM25, 1.0 = pure semantic)
- `include_patterns`: [] (all files)
- `exclude_patterns`: [] (none excluded)
- `text_pattern`: null (no regex filter)
- `code_only`: false

**Response (200):**
```json
{
  "results": [
    {
      "file": "src/auth/middleware.rs",
      "line": 42,
      "end_line": 78,
      "score": 0.847,
      "unit_type": "function",
      "signature": "pub async fn auth_middleware(req: Request) -> Response",
      "code": "pub async fn auth_middleware(req: Request) -> Response {\n    ...\n}",
      "language": "rust"
    }
  ],
  "search_time_ms": 2340,
  "index_doc_count": 720000,
  "hybrid_mode": true
}
```

**Response (503 -- daemon unhealthy):**
```json
{
  "status": "error",
  "reason": "index not loaded"
}
```

The AI agent receives 503 and falls back to ripgrep.

### POST /reload

Trigger index reload after external indexing completes.

**Request:**
```json
{}
```

**Response (200):**
```json
{
  "status": "reloaded",
  "doc_count": 720000,
  "reload_time_ms": 3200
}
```

**Behavior:**
1. Load new MmapIndex in background thread
2. Pre-warm new index pages
3. Atomically swap via ArcSwap (lock-free)
4. In-flight searches finish on old index; new searches use new index
5. Drop old index reference (OS reclaims pages)

### GET /health

K8s readiness/liveness probe.

**Response (200 -- healthy):**
```json
{
  "status": "ready",
  "index_doc_count": 720000,
  "model": "lightonai/LateOn-Code-edge",
  "uptime_seconds": 3600,
  "last_reload": "2026-04-15T10:30:00Z"
}
```

**Response (503 -- unhealthy):**
```json
{
  "status": "error",
  "reason": "model load failed: ONNX runtime not found"
}
```

## Search Hot Path

What happens on every `POST /search` -- no filesystem access, no model loading, no reindexing:

```
Request received
  |
  v
1. Query encoding                    (~0.5-1s)
   Acquire ONNX session from pool.
   Tokenize query -> ONNX inference -> query embeddings.
   Release session back to pool.
  |
  v
2. Hybrid search (if alpha < 1.0)    (~10-50ms)
   FTS5 BM25 keyword search in parallel with semantic.
   Fuse results via Reciprocal Rank Fusion (RRF).
  |
  v
3. Centroid scoring                   (~1-2ms)
   query.dot(centroids) -- centroids already in RAM.
   Batched via rayon if >100k centroids.
  |
  v
4. IVF probe + approximate scoring   (~10-50ms)
   Select top centroids per query token.
   Score candidates using centroid codes (mmap, pages warm).
  |
  v
5. Decompression + exact scoring      (~50-200ms)
   Decompress top ~1024 candidates in chunks of 128 (rayon parallel).
   MaxSim scoring with SIMD (AVX2/NEON).
  |
  v
6. SQLite metadata fetch              (~10-50ms)
   Fetch file paths, signatures, code for top_k results.
   Connection from pool -- no open/close overhead.
  |
  v
Response returned                     Total: ~1-3s warm
```

### Why This Is Consistent

- No filesystem scanning (eliminated the 10-20s `compute_update_plan` that hashes every file)
- No model loading (eliminated 2-3s ONNX session creation)
- No surprise mid-search reindexing (indexing is external; reload is explicit)
- Mmap pages pre-warmed at startup (no random page fault latency)
- Only variable is step 5 (decompression count), bounded by `DECOMPRESS_CHUNK_SIZE=128`

## Concurrency Model

### Shared State (Read-Only, No Contention)

| Resource | Sharing Mechanism | Notes |
|----------|-------------------|-------|
| MmapIndex | ArcSwap | Lock-free reads; atomic swap on reload |
| Centroids | mmap'd, read-only | OS handles concurrent page reads |
| IVF data | In RAM, immutable | Loaded once at startup |
| FTS5 index | SQLite WAL mode | Concurrent reads safe |

### Per-Request State (Isolated)

| Resource | Mechanism | Notes |
|----------|-----------|-------|
| ONNX session | Pool of N sessions, mutex-guarded | Bottleneck for throughput |
| Query embedding buffer | Stack-allocated per request | Dropped after response |
| Scoring buffers | Rayon thread-local | No cross-request sharing |
| SQLite connection | Connection pool (r2d2) | One connection per query |

### Thread Budget

```
ONNX model sessions:    4-8 (configurable via --sessions)
  Each session: single-threaded inference
  Bottleneck: with 8 sessions, 8 queries encode in parallel
  Queue of 50 requests drains in ~7-12s

Rayon thread pool:      num_cpus (shared across all requests)
  Used for: centroid batch scoring, approximate scoring, decompression
  OPENBLAS_NUM_THREADS=1 (critical: prevents contention within rayon tasks)

SQLite connections:     16-32 (connection pool)
  Used for: metadata fetch + FTS5 queries
  SQLite WAL mode for concurrent reads
```

### Backpressure

If all ONNX sessions are busy, incoming search requests queue at the session pool. To prevent unbounded queuing:

- Request timeout: 30s (configurable via `--timeout`)
- Max concurrent requests: 64 (configurable via `--max-concurrent`)
- Axum tower middleware enforces both limits
- Requests exceeding limits get `429 Too Many Requests`

## Non-UTF8 Encoding Support

### Current State (Already Implemented)

The `read_source_file()` function in `colgrep/src/index/mod.rs` handles non-UTF8 files via lossy decoding:

```rust
pub fn read_source_file(path: &Path) -> Result<DecodedSourceFile> {
    let bytes = std::fs::read(path)?;
    match std::str::from_utf8(&bytes) {
        Ok(valid) => Ok(DecodedSourceFile { text: valid.to_owned(), decode_status: DecodeStatus::Utf8 }),
        Err(_) => Ok(DecodedSourceFile { text: String::from_utf8_lossy(&bytes).into_owned(), decode_status: DecodeStatus::Lossy }),
    }
}
```

This replaces invalid bytes with U+FFFD replacement characters, which is functional but degrades search quality for legacy Windows-1252 files (accented characters, smart quotes become replacement chars).

### Improvement: Encoding Detection

Add encoding detection so Windows-1252 and other legacy files are properly decoded:

1. Try UTF-8 (fast path -- vast majority of files)
2. If UTF-8 fails, use `chardetng` crate to detect the encoding (Mozilla's encoding detector, used in Firefox)
3. Transcode to UTF-8 using `encoding_rs` (Mozilla's transcoding library):
   - Windows-1252 (most common legacy encoding in Western codebases)
   - ISO-8859-1 (Latin-1)
   - Shift_JIS, EUC-KR, GB2312 (if CJK codebases are in scope)
4. Store detected encoding in `DecodeStatus` for metadata tracking
5. Fall back to lossy UTF-8 if detection fails or confidence is too low

`chardetng` detects the encoding, `encoding_rs` does the transcoding. Both are from Mozilla, well-tested, and add ~250KB combined to binary size.

**Where this applies:** At indexing time only. The daemon searches whatever is in the index. But `read_source_file_for_preview()` in the search response path also benefits (showing correct characters in code previews).

### Updated Types

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeStatus {
    Utf8,
    Transcoded { encoding: String },  // e.g., "windows-1252"
    Lossy,  // fallback: replacement characters used
}
```

## XML File Support

### Current State

XML files (.xml, .xsd, .xsl, .xslt, .xaml) are detected and routed to `Language::Text`, which creates a single `CodeUnit` per file with `UnitType::Document`. No structural parsing.

### Improvement: Structural XML Parsing

Add a dedicated XML parser that extracts meaningful code units from XML files:

**Option A: Tree-sitter XML** (recommended)
- Add `tree-sitter-xml` dependency
- Add `Language::Xml` variant to the Language enum
- Extract structural units:
  - Top-level elements with significant content
  - Named elements (id, name attributes) as individual code units
  - Schema definitions (xsd:element, xsd:complexType)
  - XSLT templates (xsl:template match/name)
- Each unit gets: file path, line range, signature (element path), code content

**Option B: Custom regex-based parser** (simpler, less accurate)
- Use regex to extract elements, attributes, and text content
- Lower quality but no new dependency

**Recommendation:** Option A. Tree-sitter provides reliable structural parsing and follows the existing pattern used for all other languages in colgrep. The `tree-sitter-xml` crate is well-maintained.

### Unit Extraction Examples

For a Spring XML config:
```xml
<bean id="userService" class="com.app.UserService">
  <property name="repository" ref="userRepo"/>
</bean>
```

Extracted unit:
- `unit_type`: Element
- `signature`: `bean#userService (com.app.UserService)`
- `line`: 1, `end_line`: 3
- `code`: full element text

For an XSD schema:
```xml
<xs:complexType name="AddressType">
  <xs:sequence>
    <xs:element name="street" type="xs:string"/>
  </xs:sequence>
</xs:complexType>
```

Extracted unit:
- `unit_type`: TypeDefinition
- `signature`: `complexType AddressType`
- `line`: 1, `end_line`: 5

## Deployment

### As a Standalone K8s Pod

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: colgrep-daemon
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: colgrep
        image: colgrep:1.2.0
        command: ["colgrep", "serve"]
        args:
          - "--port=8080"
          - "--index=/data/index"
          - "--sessions=8"
          - "--alpha=0.75"
        ports:
        - containerPort: 8080
        resources:
          requests:
            cpu: "2"
            memory: "4Gi"
          limits:
            cpu: "4"
            memory: "6Gi"
        readinessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 30
          periodSeconds: 10
        livenessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 60
          periodSeconds: 30
        volumeMounts:
        - name: index-data
          mountPath: /data/index
          readOnly: true
      volumes:
      - name: index-data
        persistentVolumeClaim:
          claimName: colgrep-index-pvc
---
apiVersion: v1
kind: Service
metadata:
  name: colgrep
spec:
  selector:
    app: colgrep-daemon
  ports:
  - port: 80
    targetPort: 8080
```

### Resource Requirements (720k Code Units, 6GB Index)

| Component | RAM |
|-----------|-----|
| ONNX model (8 sessions) | ~400MB |
| MmapIndex resident (pre-warmed) | ~2-4GB under load |
| IVF + codec in RAM | ~150MB |
| SQLite + FTS5 | ~50MB |
| Per-search temp allocations (50 concurrent) | ~500MB peak |
| **Total recommended** | **4-6GB** |

CPU: 2-4 cores. Searches use rayon (parallel scoring) and ONNX (inference). Idle between queries.

### As a Binary in Celery Pods

Not recommended for the 6GB index case due to memory overhead competing with Celery workers. Use the standalone K8s pod approach and have Celery workers call it over HTTP.

## Configuration

```
colgrep serve [OPTIONS]

OPTIONS:
  --port <PORT>              Listen port [default: 8080]
  --host <HOST>              Listen address [default: 0.0.0.0]
  --index <PATH>             Path to colgrep index directory [required]
  --model <NAME>             Model name [default: lightonai/LateOn-Code-edge]
  --sessions <N>             Number of ONNX sessions [default: 4]
  --timeout <SECONDS>        Request timeout [default: 30]
  --max-concurrent <N>       Max concurrent requests [default: 64]
  --alpha <FLOAT>            Default hybrid search alpha [default: 0.75]
  --no-hybrid-search         Disable FTS5 hybrid, use pure semantic
  --no-prewarm               Skip index pre-warming at startup
  --quantized                Use INT8 quantized model
  --force-cpu                Force CPU execution (no GPU)

ENVIRONMENT:
  OPENBLAS_NUM_THREADS=1     Required: prevent BLAS thread contention
  RAYON_NUM_THREADS=<N>      Optional: control rayon pool size
  COLGREP_LOG=info           Log level (trace/debug/info/warn/error)
```

## Implementation Scope

### New Code

| Component | Location | Description |
|-----------|----------|-------------|
| `colgrep serve` subcommand | `colgrep/src/commands/serve.rs` | Axum HTTP server, endpoint handlers |
| CLI definition | `colgrep/src/cli.rs` | Add `Serve` variant to Commands enum |
| Shared state | `colgrep/src/commands/serve.rs` | AppState with ArcSwap index, model pool, SQLite pool |
| Pre-warming | `colgrep/src/commands/serve.rs` | Sequential mmap page read at startup |
| Encoding detection | `colgrep/src/index/mod.rs` | Upgrade `read_source_file()` with `encoding_rs` |
| XML parser | `colgrep/src/parser/xml.rs` | Tree-sitter XML structural extraction |
| XML language variant | `colgrep/src/parser/types.rs` | Add `Language::Xml` |

### New Dependencies

| Crate | Purpose | Size Impact |
|-------|---------|-------------|
| `axum` | HTTP server | Already in workspace (next-plaid-api) |
| `tokio` | Async runtime | Already in workspace |
| `tower` | Middleware (timeout, rate limit) | Already in workspace |
| `arc-swap` | Lock-free atomic pointer swap | ~20KB |
| `r2d2` + `r2d2_sqlite` | SQLite connection pool | ~50KB |
| `chardetng` | Encoding detection | ~50KB |
| `encoding_rs` | Encoding transcoding | ~200KB |
| `tree-sitter-xml` | XML structural parsing | ~100KB |

### Modified Code

| File | Change |
|------|--------|
| `colgrep/src/index/mod.rs` | Upgrade `read_source_file()` to use `encoding_rs` |
| `colgrep/src/parser/language.rs` | Map XML extensions to `Language::Xml` instead of `Language::Text` |
| `colgrep/src/parser/mod.rs` | Route `Language::Xml` to new XML parser |
| `colgrep/Cargo.toml` | Add new dependencies |

## Success Criteria

1. `colgrep serve` starts and reaches healthy state within 30s for a 6GB index
2. `POST /search` returns results in 2-5s consistently (p95) under 10-50 concurrent load
3. `GET /health` returns 503 with clear reason when any component fails
4. Non-UTF8 files (Windows-1252) are indexed with correct characters, not replacement chars
5. XML files produce structural code units (not just whole-file documents)
6. Hybrid search (BM25 + semantic) works via `alpha` parameter
7. `POST /reload` swaps index without disrupting in-flight searches
8. No search request ever triggers filesystem scanning or model reloading
