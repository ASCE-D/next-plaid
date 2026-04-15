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
  "target_paths": ["src/auth", "src/middleware"],
  "include_patterns": ["*.py", "*.rs"],
  "exclude_patterns": ["*_test.py"],
  "exclude_dirs": ["vendor", "node_modules"],
  "text_pattern": "async def",
  "code_only": false
}
```

All fields except `query` are optional. Defaults:
- `top_k`: 10 (capped at 1000 to prevent abuse)
- `alpha`: 0.75 (hybrid search balance, 0.0 = pure BM25, 1.0 = pure semantic; must be 0.0-1.0)
- `target_paths`: [] (search entire index)
- `include_patterns`: [] (all file types)
- `exclude_patterns`: [] (none excluded)
- `exclude_dirs`: [] (no directories excluded)
- `text_pattern`: null (no regex filter)
- `code_only`: false

#### Targeted Path Search

`target_paths` is the key field for agent-driven search. When the AI agent knows or suspects the relevant code is in a specific module/directory, it passes one or more paths:

```json
{"query": "database connection pooling", "target_paths": ["src/db", "src/infra/persistence"]}
```

**How it works under the hood (already implemented in the search engine):**

1. Each `target_path` calls `Searcher::filter_by_path_prefix()` → SQL `WHERE file LIKE 'src/auth%'`
2. Doc IDs from all target paths are unioned into a `subset: Vec<i64>`
3. The subset is passed to `search_one_mmap()` in `next-plaid/src/search.rs`
4. The search engine already has built-in subset optimization (lines 324-414):
   - **Centroid pruning**: reads centroid codes for subset docs from `mmap_codes`, builds `eligible_centroids: HashSet<usize>` — only these centroids are scored during IVF probe
   - **Probe scaling**: `n_ivf_probe` is scaled up by `total_docs / subset_docs` to maintain recall
   - **Candidate filtering**: after probe, candidates are filtered to subset doc IDs only
   - **Decompression**: only subset candidates go through exact scoring
5. Both semantic search and FTS5 BM25 accept the same subset parameter

**Why this is fast:** For a 720k code unit index, targeting `src/auth/` (say 2k units) means:
- Centroid pruning: only centroids containing embeddings from those 2k docs are scored (not all centroids)
- IVF probe runs on a smaller centroid pool
- Approximate scoring runs on ~2k candidates instead of ~50k
- Decompression + exact scoring on ~50 candidates instead of ~1024
- Estimated speedup: **5-10x** (sub-second search for targeted queries)

`target_paths` composes with all other filters (`include_patterns`, `exclude_patterns`, `exclude_dirs`, `text_pattern`). All are intersected into a single subset before search.

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

**Behavior (synchronous — blocks until reload completes):**
1. Load new MmapIndex in a `spawn_blocking` task
2. Pre-warm new index pages (sequential read)
3. Atomically swap via ArcSwap (lock-free)
4. In-flight searches finish on old index; new searches use new index
5. Old index files must NOT be deleted until all references are dropped (the external indexer should write to a staging directory and atomic-rename, or the daemon watches a versioned path)
6. Return response with timing

Note: concurrent `/reload` calls are serialized by a reload mutex. A second reload arriving during an active reload waits for the first to complete.

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
2. Parallel search fork:
   |                                     |
   v                                     v
   2a. Semantic search (~100-300ms)      2b. BM25 keyword search (~10-50ms)
   Centroid scoring (query.dot)          FTS5 query on SQLite (spawn_blocking)
   IVF probe + approx scoring           Returns ranked doc IDs
   Decompression + MaxSim exact scoring
   |                                     |
   +------------------+------------------+
                      |
                      v
3. Reciprocal Rank Fusion              (~1ms, if alpha < 1.0)
   Fuse semantic + BM25 results by rank.
   alpha=0.75 means 75% semantic, 25% keyword weight.
   (Skipped if alpha=1.0 pure semantic or alpha=0.0 pure BM25)
  |
  v
4. SQLite metadata fetch              (~10-50ms)
   Fetch file paths, signatures, code for top_k results.
   spawn_blocking + Connection::open (matches next-plaid-api pattern).
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
| SQLite connection | `spawn_blocking` + `Connection::open` (matches next-plaid-api pattern) | One connection per query |

### Thread Budget

```
ONNX model sessions:    4-8 (configurable via --sessions)
  Each session: single-threaded inference
  Bottleneck: with 8 sessions, 8 queries encode in parallel
  Queue of 50 requests drains in ~7-12s

Rayon thread pool:      num_cpus (shared across all requests)
  Used for: centroid batch scoring, approximate scoring, decompression
  OPENBLAS_NUM_THREADS=1 (critical: prevents contention within rayon tasks)

SQLite:                 spawn_blocking per query (no pool needed)
  Used for: metadata fetch + FTS5 queries + subset filtering
  SQLite WAL mode for concurrent reads
  Matches next-plaid-api pattern (handlers/metadata.rs, handlers/documents.rs)
```

### Expected Latency Under Load

| Concurrent Requests | Sessions | Encoding Wait | Total p95 |
|---------------------|----------|---------------|-----------|
| 1-8 | 8 | 0s | 2-5s |
| 10-20 | 8 | 1-2s | 3-7s |
| 30-50 | 8 | 3-6s | 5-11s |

The 2-5s target applies when concurrency is at or below the session count. Beyond that, requests queue for ONNX encoding. With `target_paths` narrowing search, the actual search time drops to sub-second, so even queued requests complete faster.

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
5. Fall back to lossy UTF-8 if detection returns `windows-1252` with fewer than 16 non-ASCII bytes (too little signal to distinguish from binary garbage). For files with 16+ non-ASCII bytes, trust the detection result. Note: `chardetng` cannot distinguish Windows-1252 from ISO-8859-1 for bytes 0x80-0x9F (the only differing range) — both decodings are acceptable

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
    metadata:
      labels:
        app: colgrep-daemon
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

### Indexing Pod (Separate from Serving)

Indexing runs in a different pod/job than serving. The indexing pod needs:
- **CPU**: As many as available (encoding is CPU-bound)
- **RAM**: 2-4GB (encoding pipeline peak)
- **Disk**: Index size + checkpoint storage (~6GB index + ~35GB checkpoint = **~45GB PVC**)
- Checkpoint directory is temporary — cleaned up after successful index build

### Storage: Custom Output Directory

Both the daemon and the indexing CLI accept `--output-dir` to specify where index artifacts and checkpoints are written. This enables NFS/shared storage workflows where indexing and serving pods share a mount.

**CLI:**
```
colgrep index /path/to/project --output-dir /mnt/nfs/colgrep-indices/my-project
colgrep serve --index /mnt/nfs/colgrep-indices/my-project
```

**Startup write access verification:**

At startup (both `colgrep index` and `colgrep serve`), the output directory is verified:

1. Check directory exists (or create it if `--output-dir` specified)
2. Write a temp probe file (`{dir}/.colgrep_write_probe_{pid}`)
3. Fsync + read back to verify write succeeded
4. Delete probe file
5. If any step fails → clear error with actionable message:
   ```
   Error: Cannot write to /mnt/nfs/colgrep-indices/my-project
   Reason: Permission denied

   Fix: Ensure the directory exists and is writable:
     chmod 775 /mnt/nfs/colgrep-indices/my-project
     # or in K8s, set fsGroup in pod securityContext
   ```

**For the daemon (`colgrep serve`):**
- `--output-dir` is where it reads the index from (same as `--index`)
- Write access is only needed if `/reload` needs to stage temporary files during atomic swap
- If read-only mount: `/reload` still works but uses in-memory staging instead of disk staging

**For indexing (`colgrep index`):**
- `--output-dir` is where it writes index + checkpoints
- Write access is mandatory — fail fast at startup if not writable
- Default: `{project_root}/.colgrep/` (existing behavior)

**NFS workflow example:**
```yaml
# Shared NFS PVC mounted in both indexing job and serving deployment
volumes:
- name: colgrep-index
  persistentVolumeClaim:
    claimName: colgrep-nfs-pvc  # ReadWriteMany NFS volume

# Indexing job writes to it:
command: ["colgrep", "index", "/src", "--output-dir", "/data/index", "--resume"]

# Serving pod reads from it:
command: ["colgrep", "serve", "--index", "/data/index"]

# After indexing completes, call POST /reload on the daemon
# to pick up the new index without restart
```

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

colgrep index [OPTIONS] <PROJECT_PATH>

OPTIONS:
  --output-dir <PATH>        Write index + checkpoints here [default: {project}/.colgrep/]
  --resume                   Resume from checkpoint if one exists

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
| Encoding checkpoint | `colgrep/src/index/checkpoint.rs` | Save/load/resume embedding chunks to disk |
| Resume CLI flag | `colgrep/src/cli.rs` | Add `--resume` flag to index/init commands |
| Output dir CLI flag | `colgrep/src/cli.rs` | Add `--output-dir` to index/init/serve commands |
| Write access verification | `colgrep/src/index/storage.rs` | Probe file write + fsync + cleanup with clear error messages |

### New Dependencies

| Crate | Purpose | Size Impact |
|-------|---------|-------------|
| `axum` | HTTP server | Already in workspace (next-plaid-api) |
| `tokio` | Async runtime | Already in workspace |
| `tower` | Middleware (timeout, rate limit) | Already in workspace |
| `arc-swap` | Lock-free atomic pointer swap | ~20KB |
| (none — use `tokio::task::spawn_blocking`) | SQLite access pattern (matches next-plaid-api) | 0 |
| `chardetng` | Encoding detection | ~50KB |
| `encoding_rs` | Encoding transcoding | ~200KB |
| `tree-sitter-xml` | XML structural parsing | ~100KB |

### Modified Code

| File | Change |
|------|--------|
| `colgrep/src/index/mod.rs` | Upgrade `read_source_file()` to use `encoding_rs` |
| `colgrep/src/parser/language.rs` | Map XML extensions to `Language::Xml` instead of `Language::Text` |
| `colgrep/src/parser/mod.rs` | Route `Language::Xml` to new XML parser |
| `colgrep/src/index/mod.rs` | Insert checkpoint stage in `run_chunk_pipeline()` between pool and index |
| `colgrep/Cargo.toml` | Add new dependencies |

## Test-Driven Development Plan

All tests are written RED (failing) before implementation. Each section below maps to an implementation phase. Tests serve as the specification — if the tests pass, the feature is done.

### Phase 1: Non-UTF8 Encoding Detection

Tests written in `colgrep/src/index/mod.rs` (unit) and `colgrep/src/index/tests/` (integration).

```rust
// --- Unit tests for read_source_file() upgrade ---

#[test]
fn test_read_source_file_utf8_unchanged() {
    // Valid UTF-8 file returns DecodeStatus::Utf8, text matches exactly
    let tmp = write_temp_file(b"fn main() { println!(\"hello\"); }");
    let result = read_source_file(&tmp).unwrap();
    assert_eq!(result.decode_status, DecodeStatus::Utf8);
    assert_eq!(result.text, "fn main() { println!(\"hello\"); }");
}

#[test]
fn test_read_source_file_windows_1252_detected() {
    // Windows-1252 encoded file with accented chars (e.g., 0xe9 = é)
    // Should detect encoding and transcode correctly, NOT produce U+FFFD
    let tmp = write_temp_file(b"// R\xe9sum\xe9 handler\nfn process() {}");
    let result = read_source_file(&tmp).unwrap();
    assert!(result.text.contains("Résumé"));
    assert!(!result.text.contains('\u{fffd}'));
    assert!(matches!(result.decode_status, DecodeStatus::Transcoded { .. }));
}

#[test]
fn test_read_source_file_iso_8859_1_detected() {
    // ISO-8859-1 file with German umlauts (0xfc = ü, 0xf6 = ö)
    let tmp = write_temp_file(b"// \xdcber die Br\xfccke\nfn walk() {}");
    let result = read_source_file(&tmp).unwrap();
    assert!(result.text.contains("Über"));
    assert!(result.text.contains("Brücke"));
    assert!(!result.text.contains('\u{fffd}'));
}

#[test]
fn test_read_source_file_truly_binary_falls_back_lossy() {
    // Binary garbage that no encoding can decode meaningfully
    let tmp = write_temp_file(&[0x00, 0x01, 0x02, 0x80, 0x81, 0xff, 0xfe, 0x00]);
    let result = read_source_file(&tmp).unwrap();
    assert_eq!(result.decode_status, DecodeStatus::Lossy);
}

#[test]
fn test_read_source_file_io_error_propagated() {
    // Non-existent file returns Err, not Ok with empty text
    let result = read_source_file(Path::new("/nonexistent/file.rs"));
    assert!(result.is_err());
}

#[test]
fn test_decode_status_transcoded_stores_encoding_name() {
    let tmp = write_temp_file(b"// caf\xe9\n");
    let result = read_source_file(&tmp).unwrap();
    if let DecodeStatus::Transcoded { encoding } = &result.decode_status {
        // Should be "windows-1252" or "iso-8859-1" — both are valid for 0xe9
        assert!(!encoding.is_empty());
    } else {
        panic!("Expected Transcoded, got {:?}", result.decode_status);
    }
}

// --- Integration: indexed content is searchable ---

#[test]
fn test_windows_1252_file_indexed_and_searchable() {
    // Create a temp project with a Windows-1252 encoded file
    // Index it with IndexBuilder
    // Search for the accented term
    // Verify it appears in results with correct text
    let project = create_temp_project_with_file(
        "legacy.cs",
        b"// M\xe9thode de v\xe9rification\npublic void V\xe9rifier() { }",
    );
    let mut builder = IndexBuilder::new(&project.path).unwrap();
    builder.index(None, false).unwrap();

    let searcher = Searcher::load(&project.path).unwrap();
    let results = searcher.search("Vérifier method", 5, None).unwrap();
    assert!(!results.is_empty());
    assert!(results[0].unit.code.contains("Vérifier"));
}
```

### Phase 2: XML Structural Parsing

Tests written in `colgrep/src/parser/xml.rs` and `colgrep/src/parser/tests/test_xml.rs`.

```rust
// --- Unit tests for XML parser ---

#[test]
fn test_xml_language_detection() {
    assert_eq!(detect_language(Path::new("config.xml")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("schema.xsd")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("transform.xsl")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("style.xslt")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("view.xaml")), Some(Language::Xml));
}

#[test]
fn test_xml_extract_named_elements() {
    let source = r#"<?xml version="1.0"?>
<beans>
  <bean id="userService" class="com.app.UserService">
    <property name="repo" ref="userRepo"/>
  </bean>
  <bean id="orderService" class="com.app.OrderService"/>
</beans>"#;
    let units = extract_units(Path::new("app-context.xml"), source, Language::Xml);
    // Should extract 2 units (one per bean), not 1 whole-file unit
    assert_eq!(units.len(), 2);
    assert!(units[0].signature.contains("userService"));
    assert!(units[1].signature.contains("orderService"));
    assert_eq!(units[0].unit_type, UnitType::Element);
}

#[test]
fn test_xml_extract_xsd_types() {
    let source = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="AddressType">
    <xs:sequence>
      <xs:element name="street" type="xs:string"/>
      <xs:element name="city" type="xs:string"/>
    </xs:sequence>
  </xs:complexType>
  <xs:simpleType name="ZipCode">
    <xs:restriction base="xs:string">
      <xs:pattern value="[0-9]{5}"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;
    let units = extract_units(Path::new("types.xsd"), source, Language::Xml);
    assert_eq!(units.len(), 2);
    assert!(units[0].signature.contains("AddressType"));
    assert!(units[1].signature.contains("ZipCode"));
}

#[test]
fn test_xml_extract_xslt_templates() {
    let source = r#"<?xml version="1.0"?>
<xsl:stylesheet xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
  <xsl:template match="/">
    <html><body><xsl:apply-templates/></body></html>
  </xsl:template>
  <xsl:template name="header">
    <h1>Title</h1>
  </xsl:template>
</xsl:stylesheet>"#;
    let units = extract_units(Path::new("transform.xsl"), source, Language::Xml);
    assert_eq!(units.len(), 2);
    assert!(units[0].signature.contains("match=/"));
    assert!(units[1].signature.contains("name=header"));
}

#[test]
fn test_xml_preserves_line_numbers() {
    let source = "<?xml version=\"1.0\"?>\n<root>\n  <item id=\"a\"/>\n  <item id=\"b\"/>\n</root>";
    let units = extract_units(Path::new("items.xml"), source, Language::Xml);
    // First item starts at line 3, second at line 4
    for unit in &units {
        assert!(unit.line >= 3);
        assert!(unit.end_line >= unit.line);
    }
}

#[test]
fn test_xml_small_file_single_unit() {
    // Trivial XML with no named elements falls back to single document unit
    let source = r#"<?xml version="1.0"?><root><a>1</a></root>"#;
    let units = extract_units(Path::new("tiny.xml"), source, Language::Xml);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].unit_type, UnitType::Document);
}

#[test]
fn test_xml_xaml_ui_elements() {
    let source = r#"<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        x:Name="MainWindow" Title="My App">
  <Grid x:Name="rootGrid">
    <Button x:Name="submitBtn" Content="Submit"/>
    <TextBox x:Name="inputField"/>
  </Grid>
</Window>"#;
    let units = extract_units(Path::new("MainWindow.xaml"), source, Language::Xml);
    // Should extract named UI elements
    assert!(units.iter().any(|u| u.signature.contains("MainWindow")));
    assert!(units.iter().any(|u| u.signature.contains("submitBtn")));
}
```

### Phase 3: Daemon Core — Startup, Health, Shutdown

Tests written in `colgrep/src/commands/serve.rs` (unit) and `colgrep/tests/serve_integration.rs`.

```rust
// --- Unit tests for AppState ---

#[test]
fn test_app_state_healthy_after_init() {
    let state = AppState::new(test_index_path(), test_model_config()).unwrap();
    let health = state.health();
    assert_eq!(health.status, "ready");
    assert!(health.index_doc_count > 0);
}

#[test]
fn test_app_state_unhealthy_with_missing_index() {
    let result = AppState::new(Path::new("/nonexistent/index"), test_model_config());
    assert!(result.is_err());
}

#[test]
fn test_app_state_unhealthy_with_corrupted_index() {
    let tmp = create_corrupted_index();
    let result = AppState::new(&tmp, test_model_config());
    assert!(result.is_err());
}

// --- Integration tests for HTTP endpoints ---
// Uses axum::test helpers (no real network, in-process)

#[tokio::test]
async fn test_health_returns_200_when_ready() {
    let app = create_test_app().await;
    let response = app.oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["status"], "ready");
}

#[tokio::test]
async fn test_health_returns_503_before_init() {
    let app = create_test_app_uninitialized().await;
    let response = app.oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["status"], "error");
    assert!(body["reason"].as_str().unwrap().len() > 0);
}

#[tokio::test]
async fn test_search_returns_503_when_unhealthy() {
    let app = create_test_app_uninitialized().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"test"}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_search_returns_results() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"authentication handler","top_k":5}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = parse_body(response).await;
    assert!(body["results"].as_array().unwrap().len() > 0);
    assert!(body["search_time_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_search_with_include_patterns() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","include_patterns":["*.rs"]}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        assert!(result["file"].as_str().unwrap().ends_with(".rs"));
    }
}

#[tokio::test]
async fn test_search_invalid_request_returns_400() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"not_query":"missing field"}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_search_empty_query_returns_400() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":""}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_search_response_includes_metadata() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"test","top_k":3}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    // Response must include timing, doc count, hybrid mode flag
    assert!(body.get("search_time_ms").is_some());
    assert!(body.get("index_doc_count").is_some());
    assert!(body.get("hybrid_mode").is_some());
}

// --- Validation & edge cases ---

#[tokio::test]
async fn test_search_top_k_capped_at_1000() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"test","top_k":999999}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    // Should succeed but cap results
    assert!(body["results"].as_array().unwrap().len() <= 1000);
}

#[tokio::test]
async fn test_search_with_text_pattern_filter() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","text_pattern":"async"}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        assert!(result["code"].as_str().unwrap().contains("async"));
    }
}

#[tokio::test]
async fn test_search_with_exclude_patterns() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","exclude_patterns":["*_test.rs"]}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        assert!(!result["file"].as_str().unwrap().ends_with("_test.rs"));
    }
}

#[tokio::test]
async fn test_search_with_exclude_dirs() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","exclude_dirs":["vendor"]}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        assert!(!result["file"].as_str().unwrap().starts_with("vendor/"));
    }
}

#[tokio::test]
async fn test_search_code_only_filters_non_code() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","code_only":true}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        let unit_type = result["unit_type"].as_str().unwrap();
        assert!(unit_type != "document" && unit_type != "comment");
    }
}

#[tokio::test]
async fn test_search_empty_index_returns_empty_results() {
    let app = create_test_app_empty_index().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"anything"}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_health_empty_index_still_ready() {
    let app = create_test_app_empty_index().await;
    let response = app.oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(parse_body(response).await["status"], "ready");
}

// --- Safety ---

#[tokio::test]
async fn test_target_path_traversal_rejected() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"test","target_paths":["../../etc/passwd"]}"#))
            .unwrap()
    ).await.unwrap();
    // Either 400 (rejected) or 200 with empty results (path doesn't exist in index)
    let status = response.status();
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::OK);
    if status == StatusCode::OK {
        let body: serde_json::Value = parse_body(response).await;
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
    }
}

// --- Graceful shutdown ---

#[tokio::test]
async fn test_graceful_shutdown_completes_inflight() {
    let app = create_test_app_shared().await;
    let shutdown_tx = app.shutdown_handle();

    // Start a search
    let search_handle = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(
                Request::post("/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"test"}"#))
                    .unwrap()
            ).await.unwrap().status()
        }
    });

    // Signal shutdown after brief delay
    tokio::time::sleep(Duration::from_millis(10)).await;
    shutdown_tx.send(()).unwrap();

    // In-flight search should complete, not get dropped
    let status = search_handle.await.unwrap();
    assert_eq!(status, StatusCode::OK);
}
```

### Phase 3b: Targeted Path Search

Tests for `target_paths` — the agent narrows search to specific modules/directories.

```rust
#[tokio::test]
async fn test_search_with_single_target_path() {
    // Test fixture has files in src/auth/ and src/db/
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","target_paths":["src/auth"]}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        let file = result["file"].as_str().unwrap();
        assert!(file.starts_with("src/auth"), "Expected src/auth, got {}", file);
    }
}

#[tokio::test]
async fn test_search_with_multiple_target_paths() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"query":"handler","target_paths":["src/auth","src/middleware"]}"#
            ))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        let file = result["file"].as_str().unwrap();
        assert!(
            file.starts_with("src/auth") || file.starts_with("src/middleware"),
            "Result outside target paths: {}", file
        );
    }
}

#[tokio::test]
async fn test_search_target_path_combined_with_include_pattern() {
    // target_paths + include_patterns = intersection
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"query":"handler","target_paths":["src/auth"],"include_patterns":["*.rs"]}"#
            ))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    for result in body["results"].as_array().unwrap() {
        let file = result["file"].as_str().unwrap();
        assert!(file.starts_with("src/auth"));
        assert!(file.ends_with(".rs"));
    }
}

#[tokio::test]
async fn test_search_target_path_nonexistent_returns_empty() {
    // Targeting a path with no indexed files returns empty results, not an error
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"query":"handler","target_paths":["nonexistent/module"]}"#
            ))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_search_empty_target_paths_searches_everything() {
    // Empty array = no restriction (same as omitting the field)
    let app = create_test_app().await;
    let resp_no_target = app.clone().oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","top_k":5}"#))
            .unwrap()
    ).await.unwrap();
    let resp_empty_target = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","top_k":5,"target_paths":[]}"#))
            .unwrap()
    ).await.unwrap();
    let body1: serde_json::Value = parse_body(resp_no_target).await;
    let body2: serde_json::Value = parse_body(resp_empty_target).await;
    assert_eq!(body1["results"].as_array().unwrap().len(), body2["results"].as_array().unwrap().len());
}

#[tokio::test]
async fn test_search_target_path_is_faster_than_full() {
    // Targeted search on a small subdirectory should be measurably faster
    let app = create_test_app_large().await; // larger fixture
    let resp_full = app.clone().oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","top_k":5}"#))
            .unwrap()
    ).await.unwrap();
    let resp_targeted = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","top_k":5,"target_paths":["src/auth"]}"#))
            .unwrap()
    ).await.unwrap();
    let body_full: serde_json::Value = parse_body(resp_full).await;
    let body_targeted: serde_json::Value = parse_body(resp_targeted).await;
    let time_full = body_full["search_time_ms"].as_u64().unwrap();
    let time_targeted = body_targeted["search_time_ms"].as_u64().unwrap();
    // NOTE: This test may be flaky in CI under load. Consider running as a
    // benchmark (`cargo bench`) rather than a hard assertion in CI.
    // Use generous threshold: targeted should be at least 2x faster.
    assert!(time_targeted * 2 < time_full, "Targeted {}ms should be >2x faster than full {}ms", time_targeted, time_full);
}
```

### Phase 4: Concurrency & Backpressure

```rust
#[tokio::test]
async fn test_concurrent_searches_all_succeed() {
    let app = create_test_app_shared().await;
    let mut handles = Vec::new();
    for i in 0..20 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let response = app.oneshot(
                Request::post("/search")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"query":"test query {}","top_k":3}}"#, i)))
                    .unwrap()
            ).await.unwrap();
            response.status()
        }));
    }
    for handle in handles {
        let status = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn test_max_concurrent_requests_returns_429() {
    // Configure app with max_concurrent=2
    let app = create_test_app_with_config(ServeConfig { max_concurrent: 2, ..default() }).await;
    // Send 5 requests simultaneously, at least some should get 429
    let mut handles = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let response = app.oneshot(
                Request::post("/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"test","top_k":1}"#))
                    .unwrap()
            ).await.unwrap();
            response.status()
        }));
    }
    let statuses: Vec<StatusCode> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert!(statuses.contains(&StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_request_timeout_returns_408() {
    // Configure app with timeout=1s, use a query that would take longer
    // (mock the model pool to sleep)
    let app = create_test_app_with_slow_model(Duration::from_secs(5)).await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"slow query"}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
}

#[tokio::test]
async fn test_onnx_encoding_failure_returns_500_not_crash() {
    // Mock model that returns error on encode
    let app = create_test_app_with_failing_model().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"test"}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = parse_body(response).await;
    assert!(body["reason"].as_str().unwrap().contains("encoding"));

    // Daemon should still be healthy for subsequent requests
    let health = app.oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}
```

### Phase 5: Reload

```rust
#[tokio::test]
async fn test_reload_returns_200_with_stats() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/reload")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["status"], "reloaded");
    assert!(body["doc_count"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_reload_does_not_interrupt_inflight_search() {
    let app = create_test_app_shared().await;

    // Start a slow search (mock model latency)
    let search_app = app.clone();
    let search_handle = tokio::spawn(async move {
        let response = search_app.oneshot(
            Request::post("/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"inflight test"}"#))
                .unwrap()
        ).await.unwrap();
        response.status()
    });

    // Immediately trigger reload
    tokio::time::sleep(Duration::from_millis(10)).await;
    let reload_response = app.clone().oneshot(
        Request::post("/reload")
            .body(Body::from("{}"))
            .unwrap()
    ).await.unwrap();
    assert_eq!(reload_response.status(), StatusCode::OK);

    // In-flight search should still complete successfully
    let search_status = search_handle.await.unwrap();
    assert_eq!(search_status, StatusCode::OK);
}

#[tokio::test]
async fn test_reload_with_missing_index_returns_error() {
    let app = create_test_app().await;
    // Delete the index directory between reload calls
    remove_test_index();
    let response = app.oneshot(
        Request::post("/reload")
            .body(Body::from("{}"))
            .unwrap()
    ).await.unwrap();
    // Should fail but daemon stays healthy with old index
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Health should still report ready (old index still loaded)
    let health = app.oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_concurrent_reloads_serialized() {
    let app = create_test_app_shared().await;
    // Fire 3 reloads simultaneously — all should succeed (serialized by mutex)
    let mut handles = Vec::new();
    for _ in 0..3 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let response = app.oneshot(
                Request::post("/reload")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap()
            ).await.unwrap();
            response.status()
        }));
    }
    let statuses: Vec<StatusCode> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    // All should succeed (serialized), none should 500
    assert!(statuses.iter().all(|s| *s == StatusCode::OK));
}
```

### Phase 6: Pre-warming & Startup Performance

```rust
#[test]
fn test_prewarm_reads_all_index_pages() {
    // Create a small test index with known size
    let index_path = create_test_index(1000); // 1000 code units
    let codes_path = index_path.join("index/merged_codes.npy");
    let residuals_path = index_path.join("index/merged_residuals.npy");

    // Pre-warm should not panic and should read all bytes
    prewarm_index(&index_path).unwrap();

    // Verify files are in OS page cache (platform-specific)
    // On Linux: check /proc/self/smaps for referenced pages
    // On macOS: this is best-effort, just verify no errors
}

#[test]
fn test_prewarm_skipped_with_no_prewarm_flag() {
    let config = ServeConfig { no_prewarm: true, ..default() };
    let state = AppState::new_with_config(test_index_path(), config).unwrap();
    // Should succeed without pre-warming
    assert_eq!(state.health().status, "ready");
}

#[test]
fn test_prewarm_failure_is_non_fatal() {
    // Prewarm on a path where mmap files are missing/unreadable
    // Should log warning but not prevent startup
    let config = ServeConfig { no_prewarm: false, ..default() };
    let state = AppState::new_with_config(test_index_path_partial(), config).unwrap();
    assert_eq!(state.health().status, "ready");
}

#[tokio::test]
async fn test_startup_completes_within_timeout() {
    let start = std::time::Instant::now();
    let _app = create_test_app().await; // includes model load, index load, prewarm
    let elapsed = start.elapsed();
    // For test fixtures (small index), should be well under 30s
    assert!(elapsed < Duration::from_secs(30));
}
```

### Phase 7: Hybrid Search

```rust
#[tokio::test]
async fn test_search_hybrid_default_alpha() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"function handler"}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["hybrid_mode"], true);
}

#[tokio::test]
async fn test_search_pure_semantic_with_alpha_1() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","alpha":1.0}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["hybrid_mode"], false);
}

#[tokio::test]
async fn test_search_pure_bm25_with_alpha_0() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler","alpha":0.0}"#))
            .unwrap()
    ).await.unwrap();
    let body: serde_json::Value = parse_body(response).await;
    // Pure keyword search — results should contain the literal term
    for result in body["results"].as_array().unwrap() {
        assert!(result["code"].as_str().unwrap().to_lowercase().contains("handler"));
    }
}

#[tokio::test]
async fn test_search_alpha_out_of_range_returns_400() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"test","alpha":1.5}"#))
            .unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_hybrid_search_fallback_when_fts5_missing() {
    // Create app with index that has no FTS5 tables (pre-v1.2.0 index)
    let app = create_test_app_no_fts5().await;
    let response = app.oneshot(
        Request::post("/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"handler"}"#))
            .unwrap()
    ).await.unwrap();
    // Should still succeed with pure semantic search, not 500
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = parse_body(response).await;
    assert_eq!(body["hybrid_mode"], false); // fell back to semantic-only
}
```

### Test Infrastructure

Each phase needs shared test helpers:

```rust
// colgrep/tests/common/mod.rs

/// Create a temporary project directory with source files, run indexing,
/// and return the path. Used by all integration tests.
fn create_test_project() -> TempProject { ... }

/// Create an Axum test app with a real index (small fixture).
/// Uses axum::Router directly — no TCP, no ports.
async fn create_test_app() -> Router { ... }

/// Same as above but shared (Arc) for concurrent test cases.
async fn create_test_app_shared() -> Router { ... }

/// Create an app that hasn't finished initialization.
async fn create_test_app_uninitialized() -> Router { ... }

/// Create an app with custom config overrides.
async fn create_test_app_with_config(config: ServeConfig) -> Router { ... }

/// Parse response body as JSON.
async fn parse_body(response: Response) -> serde_json::Value { ... }

/// Write bytes to a temp file and return the path.
fn write_temp_file(content: &[u8]) -> PathBuf { ... }
```

### Test Execution Order

Tests are written and run in phase order. Each phase must be GREEN before moving to the next:

```
Phase 1: Non-UTF8 encoding       → cargo test -p colgrep encoding
Phase 2: XML parsing              → cargo test -p colgrep xml
Phase 3: Daemon core              → cargo test -p colgrep serve
Phase 3b: Targeted path search    → cargo test -p colgrep target_path
Phase 4: Concurrency              → cargo test -p colgrep concurrent
Phase 5: Reload                   → cargo test -p colgrep reload
Phase 6: Pre-warming              → cargo test -p colgrep prewarm
Phase 7: Hybrid search            → cargo test -p colgrep hybrid
Phase 8: Resumable encoding       → cargo test -p colgrep checkpoint
Phase 9: Output dir & write access → cargo test -p colgrep output_dir
```

## Resumable Encoding

### Problem

The CLI shows two steps:

```
⠋ [████████████████████████████████████░░░░░] 18432/22000 (3m) Parsing files...
⠋ [██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░] 52000/720000 Encoding...
                                                              ^^^^^^^^
                                                              29 hours on 3 CPUs
```

Step 1 (parsing) is fast. Step 2 (encoding) runs the pipelined `tokenize → encode (ONNX) → pool → index → metadata` pipeline for 29 hours. If the process crashes, is OOM-killed, or the K8s pod is evicted at hour 25, all progress is lost — the pipeline holds everything in memory until the final atomic write.

### How the Pipeline Works Today

```
run_chunk_pipeline():

  for each chunk of 1024 code units:
    main thread → tokenize_tx ──→ [tokenize] ──→ [encode (ONNX)] ──→ [pool] ──→ [index_stage] ──→ [metadata]
                                                   ^^^^^^^^^^^^^^^
                                                   29 hours total
                                                   ~2.5 min/chunk

  index_stage (initial create):
    1. First chunk → spawn k-means in background
    2. All chunks buffered in memory via coding channel
    3. After k-means done → compress all chunks → atomic write
    4. Nothing logged after "Encoding..." finishes
```

The silent step after encoding (k-means + compression + write) takes minutes, not hours.

### Solution: Checkpoint After Pool Stage

Insert a disk checkpoint between the pool stage and the index stage. Each chunk's embeddings are saved to disk after ONNX encoding + pooling, so a crash only loses the current in-flight chunk (~2.5 minutes of work).

```
New pipeline with checkpointing:

  [tokenize] → [encode (ONNX)] → [pool] → [checkpoint] → [index_stage] → [metadata]
                                            ^^^^^^^^^^^
                                            NEW: save to disk

  Checkpoint directory: {index_dir}/encoding_checkpoint/
    chunk_0000.embeddings.npy    (pooled embeddings, ~50MB per chunk)
    chunk_0000.units.json        (CodeUnit metadata for this chunk)
    checkpoint.json              {"completed_chunks": 42, "total_chunks": 704}
```

### Resume Flow

```
colgrep index --resume /path/to/project

  1. Load checkpoint.json → last_completed = 42
  2. Skip parsing + encoding for chunks 0..42 (already on disk)
  3. Resume pipeline from chunk 43
  4. After all chunks encoded:
     a. Load all saved embeddings from disk
     b. K-means on heldout sample → centroids
     c. Compress all chunks → codes + residuals
     d. Write final PLAID index atomically
     e. Delete checkpoint directory
```

### Implementation

The change is in `run_chunk_pipeline()` and `run_index_stage()`:

1. **Before the main loop** in `run_chunk_pipeline()`: check for `encoding_checkpoint/checkpoint.json`. If found, load `completed_chunks` count. Skip that many chunks in the `sorted_units.chunks()` iterator.

2. **New checkpoint stage** between pool and index: after `run_pool_stage` produces a `PooledChunkForIndex`, serialize the embeddings (`Vec<Array2<f32>>`) and unit metadata to disk, then update `checkpoint.json` atomically (write to tmp, rename).

3. **In `run_index_stage()`** (initial create mode): instead of receiving chunks from the pool channel, load all checkpoint files from disk. Feed them to k-means + compression as before.

4. **New CLI flag**: `colgrep index --resume` enables checkpoint loading. Without it, any existing checkpoint directory is cleared on start (fresh index).

5. **Cleanup**: after successful index write, delete `encoding_checkpoint/`.

### Disk Space

| Codebase | Chunks | Checkpoint Size | Final Index |
|----------|--------|-----------------|-------------|
| 720k units | ~704 chunks | ~35GB | 6GB |
| 100k units | ~98 chunks | ~5GB | 800MB |

The checkpoint is temporary — deleted after index build. For the user's case, 35GB temporary disk during a 29-hour encoding is acceptable.

### What Stays Fast

- K-means training: uses heldout sample from first chunk (~minutes)
- Compression: processes all chunks through codec (~minutes)
- Index write: single atomic operation (~seconds)
- Metadata DB: already written incrementally via metadata_stage

Only the ONNX encoding is slow. Everything after it is minutes at most. The checkpoint captures exactly the expensive work.

### TDD Tests (Phase 8)

```rust
// --- Unit tests for checkpoint ---

#[test]
fn test_checkpoint_save_and_load() {
    let tmp = TempDir::new().unwrap();
    let checkpoint_dir = tmp.path().join("encoding_checkpoint");

    let embeddings = vec![Array2::<f32>::zeros((10, 128))]; // 1 doc, 10 tokens, 128 dim
    let units = vec![create_test_code_unit("src/main.rs", 1, 10)];

    save_chunk_checkpoint(&checkpoint_dir, 0, &embeddings, &units).unwrap();

    let checkpoint = load_checkpoint(&checkpoint_dir).unwrap();
    assert_eq!(checkpoint.completed_chunks, 1);
}

#[test]
fn test_checkpoint_resumes_from_last_completed() {
    let tmp = TempDir::new().unwrap();
    let checkpoint_dir = tmp.path().join("encoding_checkpoint");

    // Save 3 chunks
    for i in 0..3 {
        let embeddings = vec![Array2::<f32>::zeros((10, 128))];
        let units = vec![create_test_code_unit(&format!("src/file_{}.rs", i), 1, 10)];
        save_chunk_checkpoint(&checkpoint_dir, i, &embeddings, &units).unwrap();
    }

    let checkpoint = load_checkpoint(&checkpoint_dir).unwrap();
    assert_eq!(checkpoint.completed_chunks, 3);
    // Pipeline should skip first 3 chunks on resume
}

#[test]
fn test_checkpoint_atomic_write_survives_crash() {
    let tmp = TempDir::new().unwrap();
    let checkpoint_dir = tmp.path().join("encoding_checkpoint");

    // Save chunk 0 successfully
    save_chunk_checkpoint(&checkpoint_dir, 0, &test_embeddings(), &test_units()).unwrap();

    // Simulate crash during chunk 1: write embeddings but not checkpoint.json update
    let chunk_path = checkpoint_dir.join("chunk_0001.embeddings.npy");
    std::fs::write(&chunk_path, b"partial data").unwrap();
    // Don't update checkpoint.json

    // On resume, should see only 1 completed chunk (chunk 0)
    let checkpoint = load_checkpoint(&checkpoint_dir).unwrap();
    assert_eq!(checkpoint.completed_chunks, 1);
    // Partial chunk_0001 file should be ignored/overwritten
}

#[test]
fn test_checkpoint_cleanup_after_index_build() {
    let tmp = TempDir::new().unwrap();
    let checkpoint_dir = tmp.path().join("encoding_checkpoint");

    save_chunk_checkpoint(&checkpoint_dir, 0, &test_embeddings(), &test_units()).unwrap();
    assert!(checkpoint_dir.exists());

    cleanup_checkpoint(&checkpoint_dir).unwrap();
    assert!(!checkpoint_dir.exists());
}

#[test]
fn test_no_checkpoint_without_resume_flag() {
    // Without --resume, existing checkpoint dir is cleared
    let tmp = TempDir::new().unwrap();
    let checkpoint_dir = tmp.path().join("encoding_checkpoint");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::write(checkpoint_dir.join("checkpoint.json"), r#"{"completed_chunks":5}"#).unwrap();

    // init_checkpoint with resume=false should clear it
    init_checkpoint(&checkpoint_dir, false).unwrap();
    assert!(!checkpoint_dir.join("checkpoint.json").exists());
}

#[test]
fn test_load_all_checkpoint_embeddings() {
    let tmp = TempDir::new().unwrap();
    let checkpoint_dir = tmp.path().join("encoding_checkpoint");

    // Save 3 chunks with known dimensions
    for i in 0..3 {
        let embeddings = vec![
            Array2::<f32>::ones((5, 128)),  // 1 doc, 5 tokens
            Array2::<f32>::ones((8, 128)),  // 1 doc, 8 tokens
        ];
        save_chunk_checkpoint(&checkpoint_dir, i, &embeddings, &test_units_n(2)).unwrap();
    }

    let all_embeddings = load_all_checkpoint_embeddings(&checkpoint_dir, 3).unwrap();
    // 3 chunks × 2 docs = 6 doc embeddings total
    assert_eq!(all_embeddings.len(), 6);
}

// --- Integration test ---

#[test]
fn test_interrupted_encoding_resumes_and_completes() {
    let project = create_test_project_with_n_files(50); // small but multi-chunk

    // First run: index with simulated interrupt after 2 chunks
    let mut builder = IndexBuilder::new(&project.path).unwrap();
    builder.set_interrupt_after_chunks(2); // test hook
    let result = builder.index(None, false);
    assert!(result.is_err() || result.unwrap().added < 50);

    // Checkpoint should exist
    let checkpoint_dir = get_checkpoint_dir(&project.path);
    assert!(checkpoint_dir.join("checkpoint.json").exists());

    // Second run with --resume: should complete
    let mut builder = IndexBuilder::new(&project.path).unwrap();
    builder.set_resume(true);
    let stats = builder.index(None, false).unwrap();
    assert_eq!(stats.added, 50);

    // Checkpoint should be cleaned up
    assert!(!checkpoint_dir.exists());

    // Index should be searchable
    let searcher = Searcher::load(&project.path).unwrap();
    let results = searcher.search("test function", 5, None).unwrap();
    assert!(!results.is_empty());
}
```

### TDD Tests (Phase 9): Output Directory & Write Access

```rust
#[test]
fn test_verify_write_access_succeeds_on_writable_dir() {
    let tmp = TempDir::new().unwrap();
    let result = verify_write_access(tmp.path());
    assert!(result.is_ok());
}

#[test]
fn test_verify_write_access_fails_on_readonly_dir() {
    let tmp = TempDir::new().unwrap();
    let readonly = tmp.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    // Make read-only
    let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&readonly, perms).unwrap();

    let result = verify_write_access(&readonly);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("Permission denied") || err_msg.contains("cannot write"));

    // Cleanup: restore write permission so TempDir can delete
    let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&readonly, perms).unwrap();
}

#[test]
fn test_verify_write_access_fails_on_nonexistent_dir() {
    let result = verify_write_access(Path::new("/nonexistent/path/xyz"));
    assert!(result.is_err());
}

#[test]
fn test_verify_write_access_probe_file_cleaned_up() {
    let tmp = TempDir::new().unwrap();
    verify_write_access(tmp.path()).unwrap();
    // No probe files left behind
    let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
    assert!(entries.iter().all(|e| {
        !e.as_ref().unwrap().file_name().to_string_lossy().contains("colgrep_write_probe")
    }));
}

#[test]
fn test_output_dir_creates_directory_if_missing() {
    let tmp = TempDir::new().unwrap();
    let new_dir = tmp.path().join("new/nested/output");
    assert!(!new_dir.exists());

    init_output_dir(&new_dir).unwrap();
    assert!(new_dir.exists());
    verify_write_access(&new_dir).unwrap();
}

#[test]
fn test_index_with_custom_output_dir() {
    let project = create_test_project_with_n_files(10);
    let output = TempDir::new().unwrap();

    let mut builder = IndexBuilder::new(&project.path).unwrap();
    builder.set_output_dir(output.path());
    let stats = builder.index(None, false).unwrap();
    assert_eq!(stats.added, 10);

    // Index files should be in the custom output dir, not project dir
    assert!(output.path().join("index/metadata.json").exists());

    // Searchable from the custom location
    let searcher = Searcher::load_from(output.path()).unwrap();
    let results = searcher.search("test", 5, None).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn test_checkpoint_written_to_output_dir() {
    let project = create_test_project_with_n_files(50);
    let output = TempDir::new().unwrap();

    let mut builder = IndexBuilder::new(&project.path).unwrap();
    builder.set_output_dir(output.path());
    builder.set_interrupt_after_chunks(1);
    let _ = builder.index(None, false);

    // Checkpoint should be in output dir, not project dir
    assert!(output.path().join("encoding_checkpoint/checkpoint.json").exists());
    assert!(!project.path.join(".colgrep/encoding_checkpoint").exists());
}

#[tokio::test]
async fn test_serve_startup_verifies_index_dir_readable() {
    // Point daemon at a directory that exists but has no index
    let tmp = TempDir::new().unwrap();
    let result = AppState::new(tmp.path(), test_model_config());
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("index") || err.contains("metadata"));
}
```

## Success Criteria

1. `colgrep serve` starts and reaches healthy state within 30s for a 6GB index
2. `POST /search` returns results in 2-5s consistently (p95) under 10-50 concurrent load
3. `GET /health` returns 503 with clear reason when any component fails
4. Non-UTF8 files (Windows-1252) are indexed with correct characters, not replacement chars
5. XML files produce structural code units (not just whole-file documents)
6. Hybrid search (BM25 + semantic) works via `alpha` parameter
7. `POST /reload` swaps index without disrupting in-flight searches
8. No search request ever triggers filesystem scanning or model reloading
9. `colgrep index --resume` recovers from a crash and completes encoding without re-doing finished chunks
10. `--output-dir` writes index/checkpoints to custom path; write access verified at startup with clear error if not writable
11. All TDD tests pass green before feature is considered complete
