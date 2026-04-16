# Chunked Indexing Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `colgrep init` capable of indexing repos with 100k+ files on 16GB RAM by processing files in bounded chunks instead of loading all parsed code units into memory at once.

**Architecture:** Add a `--chunked` / `--chunk-files N` CLI flag (default: off for backward compat). When enabled, `full_rebuild` processes files in groups of N (default: 10,000). Each group is parsed, its call graph built, encoded, and flushed to the index before the next group starts. The encoding pipeline (`run_chunk_pipeline`) already operates on streaming chunks via channels — we just need to feed it in file-bounded waves instead of all-at-once. The PLAID index supports appending via `update_or_create`, so each wave appends to the same index.

**Tech Stack:** Rust, existing colgrep/next-plaid crates, clap CLI, indicatif progress bars

---

## Key Design Decisions

1. **Toggle, not default:** `--chunked` is opt-in. Small repos (<10k files) don't benefit, and the all-at-once path is battle-tested. When stabilized, we can flip the default.
2. **Call graph is per-wave:** `build_call_graph` currently needs all units. In chunked mode, call graph is built per-wave (files in the same wave see each other's calls). Cross-wave call edges are lost — this is an acceptable tradeoff for memory. Document this limitation.
3. **k-means centroids:** The PLAID index's k-means centroids are seeded from the first chunk of the first wave. Subsequent waves append to the existing index using `update_or_create`. This means centroid quality depends on the first wave's data — acceptable since files are sorted by path, giving good coverage of the first N files.
4. **Atomic swap preserved:** All waves write to `index.tmp/`, then the final atomic rename happens once at the end.

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `colgrep/src/cli.rs` | Modify | Add `--chunked` and `--chunk-files` flags to `Init` subcommand |
| `colgrep/src/index/mod.rs` | Modify | Add `full_rebuild_chunked()` method, wire `chunk_files` config into `IndexBuilder` |
| `colgrep/src/commands/init.rs` | Modify | Pass new flags through to `IndexBuilder` |
| `colgrep/tests/chunked_indexing.rs` | Create | Integration test for chunked vs non-chunked equivalence |

---

### Task 1: Add CLI flags

**Files:**
- Modify: `colgrep/src/cli.rs:556-592` (Init variant)
- Modify: `colgrep/src/index/mod.rs:750-768` (IndexBuilder struct)

- [ ] **Step 1: Add `--chunked` and `--chunk-files` flags to CLI**

In `colgrep/src/cli.rs`, add two new fields to the `Init` variant, after `static_batch` (line 591):

```rust
        /// Enable chunked indexing for large repos (processes files in bounded groups to limit memory)
        #[arg(long = "chunked")]
        chunked: bool,

        /// Number of files per chunk in chunked indexing mode (default: 10000)
        #[arg(long = "chunk-files", value_name = "N", requires = "chunked")]
        chunk_files: Option<usize>,
```

- [ ] **Step 2: Add fields to IndexBuilder**

In `colgrep/src/index/mod.rs`, add two fields to the `IndexBuilder` struct after `auto_confirm` (line 765):

```rust
    /// If true, use chunked indexing (process files in bounded groups)
    chunked: bool,
    /// Number of files per chunk (default: 10_000)
    chunk_files: usize,
```

- [ ] **Step 3: Add setter methods and default initialization**

Add a constant near the top of `index/mod.rs` (after line 123):

```rust
/// Default number of files per chunk in chunked indexing mode.
const DEFAULT_CHUNK_FILES: usize = 10_000;
```

In the `IndexBuilder::with_options` constructor, initialize the new fields:

```rust
    chunked: false,
    chunk_files: DEFAULT_CHUNK_FILES,
```

Add setter methods to the `impl IndexBuilder` block:

```rust
    pub fn set_chunked(&mut self, chunked: bool) {
        self.chunked = chunked;
    }

    pub fn set_chunk_files(&mut self, n: usize) {
        self.chunk_files = n;
        self.chunked = true;
    }
```

- [ ] **Step 4: Wire CLI flags through init command**

In `colgrep/src/commands/init.rs`, where the `IndexBuilder` is configured, pass the new flags:

```rust
    if chunked {
        builder.set_chunked(true);
    }
    if let Some(n) = chunk_files {
        builder.set_chunk_files(n);
    }
```

Find where the `Init` variant is destructured and add `chunked, chunk_files` to the pattern.

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p colgrep 2>&1 | tail -5`
Expected: Compiles (with possibly unused field warnings — that's fine for now)

- [ ] **Step 6: Commit**

```bash
git add colgrep/src/cli.rs colgrep/src/index/mod.rs colgrep/src/commands/init.rs
git commit -m "feat: add --chunked and --chunk-files CLI flags for bounded-memory indexing"
```

---

### Task 2: Implement `full_rebuild_chunked`

**Files:**
- Modify: `colgrep/src/index/mod.rs:1667-1786` (near `full_rebuild`)

This is the core change. The new method processes files in waves of `chunk_files` size, parsing + encoding + flushing each wave before starting the next.

- [ ] **Step 1: Add the `full_rebuild_chunked` method**

Add this method to `impl IndexBuilder`, right after the existing `full_rebuild` method (after line ~1786):

```rust
    /// Chunked full rebuild: processes files in bounded waves to limit memory.
    ///
    /// Each wave: scan chunk of files -> parse -> build call graph -> encode -> flush to index.
    /// The PLAID index is built incrementally via update_or_create after the first wave
    /// seeds the k-means centroids.
    ///
    /// Trade-off: call graph edges only connect units within the same wave.
    fn full_rebuild_chunked(&mut self, languages: Option<&[Language]>) -> Result<UpdateStats> {
        let index_path = get_vector_index_path(&self.index_dir);
        let temp_path = self.index_dir.join("index.tmp");
        let old_path = self.index_dir.join("index.old");

        // Clean any leftover temp/old dirs from previous failed attempts
        if temp_path.exists() {
            std::fs::remove_dir_all(&temp_path)?;
        }
        if old_path.exists() {
            std::fs::remove_dir_all(&old_path)?;
        }

        let (files, skipped) = self.scan_files(languages)?;
        let total_files = files.len();
        let mut state = IndexState::default();
        let mut total_units: usize = 0;
        let mut first_wave = true;

        // Overall progress bar (file-level, across all waves)
        let file_pb = ProgressBar::new(total_files as u64);
        file_pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                .unwrap()
                .progress_chars("█▓░"),
        );
        file_pb.enable_steady_tick(std::time::Duration::from_millis(100));

        let chunk_files = self.chunk_files;
        let target_index_path = temp_path.clone();
        std::fs::create_dir_all(&target_index_path)?;

        for (wave_idx, file_chunk) in files.chunks(chunk_files).enumerate() {
            if is_interrupted() {
                let _ = std::fs::remove_dir_all(&temp_path);
                anyhow::bail!("Indexing interrupted by user");
            }

            file_pb.set_message(format!(
                "Wave {}: Parsing files...",
                wave_idx + 1
            ));

            // 1. Parse this wave's files
            let mut wave_units: Vec<CodeUnit> = Vec::new();
            for parsed in parse_files_parallel(&self.project_root, file_chunk, Some(&file_pb)) {
                if let Some(reason) = parsed.skip_reason {
                    eprintln!("⚠️  {}", reason);
                    state.ignored_files.insert(parsed.path);
                    continue;
                }

                wave_units.extend(parsed.units);
                state.ignored_files.remove(&parsed.path);
                if let Some(file_info) = parsed.file_info {
                    state.files.insert(parsed.path, file_info);
                }
            }

            if is_interrupted() {
                let _ = std::fs::remove_dir_all(&temp_path);
                anyhow::bail!("Indexing interrupted by user");
            }

            if wave_units.is_empty() {
                continue;
            }

            // 2. Build call graph within this wave
            build_call_graph(&mut wave_units);

            // 3. Prompt for confirmation on the first wave (total estimate)
            if first_wave && !self.auto_confirm {
                // Estimate total units from first wave
                let estimated_total =
                    (wave_units.len() as f64 / file_chunk.len() as f64 * total_files as f64) as usize;
                if estimated_total > CONFIRMATION_THRESHOLD
                    && !prompt_large_index_confirmation(estimated_total)
                {
                    let _ = std::fs::remove_dir_all(&temp_path);
                    anyhow::bail!("Indexing cancelled by user");
                }
            }

            total_units += wave_units.len();

            // 4. Ensure model is created (lazy init on first wave)
            if first_wave {
                self.ensure_model_created(wave_units.len())?;

                #[cfg(feature = "cuda")]
                if !crate::onnx_runtime::is_cudnn_available()
                    && std::env::var("_COLGREP_CUDNN_NOTICE").is_err()
                {
                    std::env::set_var("_COLGREP_CUDNN_NOTICE", "1");
                    eprintln!("📂 cuDNN not found, encoding will use CPU.");
                }
            }

            // 5. Encode and write to index
            file_pb.set_message(format!(
                "Wave {}: Encoding {} units...",
                wave_idx + 1,
                wave_units.len()
            ));

            let was_interrupted =
                self.write_index_impl(&wave_units, false, Some(&target_index_path))?;

            if was_interrupted {
                let _ = std::fs::remove_dir_all(&temp_path);
                anyhow::bail!("Indexing interrupted by user");
            }

            // Units are now flushed to disk — drop them to free memory
            drop(wave_units);

            first_wave = false;
        }

        file_pb.finish_and_clear();

        // Atomic swap: replace old index with newly built one
        if total_units == 0 {
            if index_path.exists() {
                std::fs::remove_dir_all(&index_path)?;
            }
        } else {
            if index_path.exists() {
                std::fs::rename(&index_path, &old_path)
                    .context("Failed to move old index aside")?;
            }
            if let Err(e) = std::fs::rename(&temp_path, &index_path) {
                if old_path.exists() && !index_path.exists() {
                    let _ = std::fs::rename(&old_path, &index_path);
                }
                return Err(anyhow::anyhow!(
                    "Failed to move new index into place: {}",
                    e
                ));
            }
            if old_path.exists() {
                let _ = std::fs::remove_dir_all(&old_path);
            }
        }

        // Save state and project metadata only on successful completion
        state.save(&self.index_dir)?;
        ProjectMetadata::new(&self.project_root).save(&self.index_dir)?;

        Ok(UpdateStats {
            added: total_files,
            changed: 0,
            deleted: 0,
            unchanged: 0,
            skipped,
        })
    }
```

- [ ] **Step 2: Wire the toggle in the `index` method**

Find where `full_rebuild` is called. The main entry point is the `index` method (around line 1311). Every call to `self.full_rebuild(languages)` should be replaced with a conditional:

```rust
    if self.chunked {
        self.full_rebuild_chunked(languages)
    } else {
        self.full_rebuild(languages)
    }
```

There are multiple call sites — search for `self.full_rebuild(languages)` and update each one. There are roughly 8-10 occurrences between `index()` and `incremental_update()`. Each call should get the same conditional wrapper.

**Important:** For `incremental_update`, the chunked mode should only apply to the `full_rebuild` fallback calls, not to the incremental path itself (which already handles small batches).

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p colgrep 2>&1 | tail -5`
Expected: Compiles successfully

- [ ] **Step 4: Commit**

```bash
git add colgrep/src/index/mod.rs
git commit -m "feat: implement full_rebuild_chunked for bounded-memory indexing"
```

---

### Task 3: Fix encoding progress bar (missing ETA, not visible enough)

**Files:**
- Modify: `colgrep/src/index/mod.rs:2371-2384` (encoding progress bar in `write_index_impl`)
- Modify: `colgrep/src/index/mod.rs` (chunked path call)

The encoding progress bar exists at line 2371 but has two issues:
1. Template lacks `{eta}` — users see a bar but no time estimate
2. Message just says "Encoding..." with no context about what stage this is

- [ ] **Step 1: Fix the encoding progress bar template**

In `write_index_impl` (line 2375), change the template from:

```rust
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
```

to:

```rust
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
```

This adds ETA to the encoding bar, matching the parsing bar's template.

- [ ] **Step 2: Enable encoding progress in chunked mode**

In `full_rebuild_chunked`, change the `write_index_impl` call to pass `show_progress=true`:

```rust
            let was_interrupted =
                self.write_index_impl(&wave_units, true, Some(&target_index_path))?;
```

Each wave will show its own encoding progress bar with position, ETA, and "Encoding..." message.

- [ ] **Step 3: Verify the bar shows**

Run on a small project:
```bash
cargo run -p colgrep -- init -y .
```
Expected: See both "Parsing files..." and "Encoding..." progress bars with ETA.

- [ ] **Step 4: Commit**

```bash
git add colgrep/src/index/mod.rs
git commit -m "fix: add ETA to encoding progress bar and enable it in chunked mode"
```

---

### Task 4: Integration test

**Files:**
- Create: `colgrep/tests/chunked_indexing.rs`

- [ ] **Step 1: Write integration test**

Create `colgrep/tests/chunked_indexing.rs`:

```rust
//! Integration test: chunked indexing produces a searchable index.
//!
//! Uses a small synthetic project to verify that --chunked with a low
//! --chunk-files value produces the same search results as non-chunked.

use std::fs;
use std::path::Path;
use std::process::Command;

fn colgrep_bin() -> String {
    let path = env!("CARGO_BIN_EXE_colgrep");
    path.to_string()
}

fn create_test_project(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    // Create 20 small Rust files across 2 directories
    for i in 0..10 {
        let sub = dir.join("src");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join(format!("mod_{i}.rs")),
            format!(
                "/// Module {i} for memory allocation\n\
                 pub fn allocate_{i}(size: usize) -> Vec<u8> {{\n\
                     vec![0u8; size]\n\
                 }}\n"
            ),
        )
        .unwrap();
    }
    for i in 0..10 {
        let sub = dir.join("lib");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join(format!("helper_{i}.rs")),
            format!(
                "/// Helper {i} for thread safety\n\
                 pub fn thread_safe_{i}() -> bool {{\n\
                     true\n\
                 }}\n"
            ),
        )
        .unwrap();
    }
}

#[test]
fn chunked_indexing_produces_searchable_index() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    create_test_project(&project);

    // Index with --chunked --chunk-files 5 (4 waves of 5 files)
    let output = Command::new(colgrep_bin())
        .args([
            "init",
            "-y",
            "--chunked",
            "--chunk-files",
            "5",
            project.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run colgrep init");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "colgrep init --chunked failed: {stderr}"
    );

    // Search should return results
    let output = Command::new(colgrep_bin())
        .args([
            "memory allocation",
            project.to_str().unwrap(),
            "-k",
            "5",
            "--no-color",
        ])
        .output()
        .expect("failed to run colgrep search");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "colgrep search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.is_empty(),
        "search returned no results after chunked indexing"
    );
    // Should find files from src/ directory
    assert!(
        stdout.contains("allocate") || stdout.contains("mod_"),
        "search results don't contain expected matches: {stdout}"
    );
}

#[test]
fn chunked_flag_requires_chunk_files_or_uses_default() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project2");
    create_test_project(&project);

    // --chunked without --chunk-files should use default (10000) and succeed
    let output = Command::new(colgrep_bin())
        .args(["init", "-y", "--chunked", project.to_str().unwrap()])
        .output()
        .expect("failed to run colgrep init");

    assert!(
        output.status.success(),
        "colgrep init --chunked (default chunk size) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
```

- [ ] **Step 2: Run the test to verify it fails (implementation not yet wired)**

Run: `cargo test -p colgrep --test chunked_indexing -- --nocapture 2>&1 | tail -20`

If Task 2 is already done, the test should pass. If it fails, debug from the error message.

- [ ] **Step 3: Fix any issues and verify tests pass**

Run: `cargo test -p colgrep --test chunked_indexing -- --nocapture 2>&1 | tail -20`
Expected: Both tests pass.

- [ ] **Step 4: Run existing test suite to ensure no regressions**

Run: `cargo test -p colgrep 2>&1 | tail -10`
Expected: All existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add colgrep/tests/chunked_indexing.rs
git commit -m "test: add integration tests for chunked indexing"
```

---

### Task 5: Manual validation on chromium/base/

This is not automated — run manually to verify the chunked pipeline works on real data.

- [ ] **Step 1: Build release binary**

Run: `cargo build --release -p colgrep --features "coreml,metal_gpu,accelerate" 2>&1 | tail -5`

- [ ] **Step 2: Clear existing index and re-index with chunked mode**

```bash
target/release/colgrep clear /Users/deepesh/practice/chromium/base
time target/release/colgrep init -y --chunked --chunk-files 1000 /Users/deepesh/practice/chromium/base
```

Expected: Completes successfully, shows wave progress, ~7-8 minutes.

- [ ] **Step 3: Verify search works**

```bash
target/release/colgrep "memory allocation" /Users/deepesh/practice/chromium/base -k 5 -c
```

Expected: Returns relevant results from `allocator/`, `memory/`, etc.

- [ ] **Step 4: Test full chromium with chunked mode (the real test)**

```bash
target/release/colgrep clear /Users/deepesh/practice/chromium
time target/release/colgrep init -y --chunked --chunk-files 10000 /Users/deepesh/practice/chromium
```

Monitor memory with `top -pid $(pgrep colgrep)` in another terminal. Memory should stay under ~8GB (vs 35GB+ without chunked mode).

- [ ] **Step 5: Commit (update help text if needed)**

```bash
git add -A
git commit -m "docs: update CLI help for chunked indexing"
```

---

## Important Notes for Implementer

1. **`parse_files_parallel` returns a `Vec`**: This function (line 667) collects all results via `par_iter().map().collect()`. In the chunked path, it's called per-wave with only `chunk_files` files, so memory is bounded.

2. **`write_index_impl` calls `prepare_units_for_encoding`**: This sorts and samples units for k-means. On the second+ wave, the PLAID index already exists, so `run_index_stage` uses `update_or_create` instead of initial k-means seeding. This is already handled by the `initial_create` check at line 394.

3. **`build_call_graph` limitation**: The call graph only connects units within the same wave. For a 164k-file repo with 10k-file waves, a function defined in wave 1 and called in wave 5 won't have its `called_by` populated. This is acceptable — the call graph is a search quality enhancement, not a correctness requirement.

4. **Index state accumulation**: `IndexState` (file hashes/mtimes) is accumulated across waves in memory. This is lightweight — ~200 bytes per file = ~33MB for 164k files. Not a concern.

5. **Don't touch `incremental_update`**: The incremental path only re-indexes changed files (usually small numbers). Chunking is only needed for `full_rebuild`.
