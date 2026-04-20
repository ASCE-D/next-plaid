# ColGREP Daemon Plan — Progress Tracker

**Plan:** `docs/superpowers/plans/2026-04-16-colgrep-daemon.md`
**Branch:** `feat/colgrep-daemon-with-local-changes`

---

## Original 14-Task Plan Status

### Phase A: Non-UTF8 Encoding Detection

| Task | Description | Status | Commit |
|------|------------|--------|--------|
| **Task 1** | Add chardetng and encoding_rs dependencies | **DONE** | `890298f` |
| **Task 2** | Upgrade DecodeStatus and read_source_file | **DONE** | `8f1306c` |

### Phase B: XML Structural Parsing

| Task | Description | Status | Commit |
|------|------------|--------|--------|
| **Task 3** | Add tree-sitter-xml dependency and Language::Xml variant | **DONE** | `c337e32` |
| **Task 4** | Write XML parser with tests | **DONE** | `bff8661` |

### Phase C: Daemon Server (`colgrep serve`)

| Task | Description | Status | Commit |
|------|------------|--------|--------|
| **Task 5** | Add daemon dependencies (axum, tokio, arc-swap) | **DONE** | `88583b6` |
| **Task 6** | Add Serve subcommand to CLI | **DONE** | `3712b81` |
| **Task 7** | Implement AppState and /health endpoint | **DONE** | `68f33df` |
| **Task 8** | Implement /search endpoint | **DONE** | `68f33df` |
| **Task 9** | Implement /reload endpoint | **DONE** | `68f33df` |
| **Task 10** | Add pre-warming at startup | **DONE** | `68f33df` |
| **Task 11** | Add backpressure middleware | **NOT DONE** | — |
| **Task 12** | Manual integration test | **DONE** (via real-world testing) | — |

### Phase D: Resumable Encoding + Output Directory

| Task | Description | Status | Commit |
|------|------------|--------|--------|
| **Task 13** | Write access verification for output directories | **DONE** | `4536060` |
| **Task 14** | Encoding checkpoint (save/load/resume) | **PLANNED** | — |

---

## Additional Work Done (Beyond Original Plan)

These were discovered during real-world testing on the Chromium repo (164k files, Mac M4, 16GB RAM).

| Feature | Description | Commits |
|---------|------------|---------|
| **Chunked indexing** | `--chunked --chunk-files N` flag for bounded-memory indexing. Processes files in waves to prevent OOM on large repos. | `f3ba9e1`, `2061f2b`, `bd2ccc7` |
| **Encoding progress bar ETA** | Added `{eta}` to encoding progress bar template | `310922b` |
| **Wave progress reporting** | Encoding progress updates the wave bar message in chunked mode (no flickering) | `5cde019` |
| **Integration tests** | Chunked indexing integration tests (2 tests) | `33bd245` |
| **Integration test report** | Real-world test results from Chromium repo | `24bce08` |
| **CUDA GPU test guide** | Step-by-step guide for testing on Linux with NVIDIA GPU | `24bce08` |
| **Chunked resume plan** | Plan for checkpoint/resume support in chunked mode | `24bce08` |

---

## What's Left

### From Original Plan

| # | Item | Priority | Notes |
|---|------|----------|-------|
| **Task 11** | Backpressure middleware | Low | Tower middleware to limit concurrent requests. Daemon works without it — just no protection against overload. |
| **Task 14** | Encoding checkpoint/resume | High | Plan written at `docs/superpowers/plans/2026-04-16-chunked-resume.md`. 4 tasks. Saves checkpoint after each wave so interrupted indexing can resume. |

### Discovered During Testing (New)

| # | Item | Priority | Notes |
|---|------|----------|-------|
| **A** | Daemon `/index` endpoint | Medium | Currently no way to trigger indexing via HTTP API. Users must run `colgrep init` from CLI, then `/reload`. Suggested: `POST /index` (background job) + `GET /index/status` (progress). Documented in test report. |
| **B** | `--force-gpu` on Apple Silicon | Low | `--force-gpu` maps to CUDA only. On macOS, CoreML is used via auto-detect but can't be forced. Consider `--force-coreml` or making `--force-gpu` platform-aware. |
| **C** | CoreML speedup investigation | Low | CPU and CoreML indexing times are identical (~7.5 min for 3.5k files). CoreML EP may be silently falling back to CPU. |
| **D** | Compiler warning | Low | Unused variable `force_cpu` in `next-plaid-onnx/src/lib.rs:319`. |
| **E** | Full Chromium validation | Medium | Full 164k-file index running with `--chunked`. When complete, re-run daemon tests on full index. |
| **F** | CUDA GPU validation | Medium | Test guide written. Needs to be executed on Linux machine with NVIDIA GPU. |

---

## Commit History (This Branch)

```
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

## Summary

- **12 of 14 original tasks done** (86%)
- **Task 11** (backpressure) is low priority — skippable for now
- **Task 14** (resume) has a plan ready, waiting for execution
- **6 additional items** discovered during real-world testing, documented above
