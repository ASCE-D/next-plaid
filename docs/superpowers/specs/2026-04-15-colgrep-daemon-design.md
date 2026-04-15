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
- `top_k`: 10
- `alpha`: 0.75 (hybrid search balance, 0.0 = pure BM25, 1.0 = pure semantic)
- `target_paths`: [] (search entire index)
- `include_patterns`: [] (all file types)
- `exclude_patterns`: [] (none excluded)
- `text_pattern`: null (no regex filter)
- `code_only`: false

#### Targeted Path Search

`target_paths` is the key field for agent-driven search. When the AI agent knows or suspects the relevant code is in a specific module/directory, it passes one or more paths:

```json
{"query": "database connection pooling", "target_paths": ["src/db", "src/infra/persistence"]}
```

**How it works under the hood:**

1. Each `target_path` is resolved relative to the indexed project root
2. The filtering DB is queried for all code units whose `file` column starts with any target path
3. The resulting doc IDs form a `subset` that is passed to the search engine
4. Both semantic search and BM25 search only score documents in this subset
5. Centroid probing is proportionally scaled up (fewer eligible centroids need more probes to maintain recall)

**Why this is fast:** For a 720k code unit index, targeting `src/auth/` (say 2k units) means:
- IVF probe only checks centroids containing those 2k docs (not all 720k)
- Approximate scoring runs on ~2k candidates instead of ~50k
- Decompression + exact scoring on ~50 candidates instead of ~1024
- Estimated speedup: **5-10x** (sub-second search for targeted queries)

`target_paths` composes with all other filters. If both `target_paths` and `include_patterns` are set, results must match both (intersection).

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
    assert!(time_targeted < time_full, "Targeted {}ms should be faster than full {}ms", time_targeted, time_full);
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
Phase 4: Concurrency              → cargo test -p colgrep concurrent
Phase 5: Reload                   → cargo test -p colgrep reload
Phase 6: Pre-warming              → cargo test -p colgrep prewarm
Phase 7: Hybrid search            → cargo test -p colgrep hybrid
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
9. All TDD tests pass green before feature is considered complete
