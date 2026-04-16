# Chunked Indexing Resume Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow `colgrep init --chunked` to resume from the last completed wave after interruption, avoiding re-encoding hours of already-processed data.

**Architecture:** After each wave completes in `full_rebuild_chunked`, save a checkpoint file (`checkpoint.json`) alongside the index in `index.tmp/`. The checkpoint records: completed wave count, accumulated IndexState, total file count, and chunk_files size. On the next `colgrep init --chunked` run, if a matching checkpoint exists (same files, same chunk size), skip completed waves and resume from where it left off. A `--no-resume` flag forces a fresh start. Checkpoint is deleted after successful completion.

**Tech Stack:** Rust, serde_json, existing colgrep types (IndexState, paths module)

---

## Key Design Decisions

1. **Automatic resume:** When `--chunked` is used and a checkpoint exists with matching parameters, resume automatically. No separate `--resume` flag needed — it just works. `--no-resume` overrides this to force a fresh start.
2. **Checkpoint granularity is per-wave:** We don't checkpoint within a wave. If a wave is interrupted mid-encoding, that wave is re-done. This is simple and safe — a wave of 10k files takes ~30 min max.
3. **Checkpoint validity:** A checkpoint is valid only if (a) the file list hash matches (same files scanned), (b) `chunk_files` matches, and (c) the `index.tmp/` directory exists with the partial index. If any differ, the checkpoint is stale and ignored.
4. **No CLI flag for resume:** Resume is implicit with `--chunked`. The only new flag is `--no-resume` to opt out.

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `colgrep/src/index/checkpoint.rs` | Create | Checkpoint data structure, save/load/validate/cleanup functions |
| `colgrep/src/index/mod.rs` | Modify | Wire checkpoint save/load into `full_rebuild_chunked`, add `pub mod checkpoint` |
| `colgrep/src/cli.rs` | Modify | Add `--no-resume` flag to `Init` variant |
| `colgrep/src/commands/init.rs` | Modify | Pass `no_resume` flag to IndexBuilder |
| `colgrep/src/main.rs` | Modify | Pass `no_resume` through InitOptions |
| `colgrep/tests/chunked_resume.rs` | Create | Integration test for resume behavior |

---

### Task 1: Checkpoint data structure and persistence

**Files:**
- Create: `colgrep/src/index/checkpoint.rs`
- Modify: `colgrep/src/index/mod.rs` (add `pub mod checkpoint;`)

- [ ] **Step 1: Create the checkpoint module**

Create `colgrep/src/index/checkpoint.rs`:

```rust
//! Wave-level checkpoint for chunked indexing resume support.
//!
//! After each wave completes, a checkpoint is saved alongside the partial
//! index in `index.tmp/`. On the next `--chunked` run, if the checkpoint
//! matches (same files, same chunk size), completed waves are skipped.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::state::IndexState;

const CHECKPOINT_FILENAME: &str = "chunked_checkpoint.json";

/// Checkpoint saved after each completed wave during chunked indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkedCheckpoint {
    /// Number of waves fully completed (0-indexed: if 3 waves done, this is 3)
    pub completed_waves: usize,
    /// The chunk_files parameter used — must match to resume
    pub chunk_files: usize,
    /// Hash of the sorted file list — must match to resume
    /// (ensures the same files are being indexed)
    pub file_list_hash: u64,
    /// Total number of files in the scan
    pub total_files: usize,
    /// Accumulated IndexState from completed waves
    pub state: IndexState,
    /// Total code units encoded so far
    pub total_units: usize,
}

impl ChunkedCheckpoint {
    /// Compute a hash of the sorted file list for validity checking.
    pub fn hash_file_list(files: &[PathBuf]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        files.len().hash(&mut hasher);
        for f in files {
            f.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Save checkpoint to the given directory (typically `index.tmp/`).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CHECKPOINT_FILENAME);
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to save checkpoint to {}", path.display()))?;
        Ok(())
    }

    /// Load checkpoint from the given directory, if it exists.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(CHECKPOINT_FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read checkpoint from {}", path.display()))?;
        let checkpoint: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse checkpoint from {}", path.display()))?;
        Ok(Some(checkpoint))
    }

    /// Check if this checkpoint is valid for the given parameters.
    pub fn is_valid_for(&self, files: &[PathBuf], chunk_files: usize) -> bool {
        self.chunk_files == chunk_files
            && self.total_files == files.len()
            && self.file_list_hash == Self::hash_file_list(files)
    }

    /// Remove the checkpoint file from the given directory.
    pub fn cleanup(dir: &Path) {
        let path = dir.join(CHECKPOINT_FILENAME);
        let _ = std::fs::remove_file(&path);
    }
}
```

- [ ] **Step 2: Add module declaration**

In `colgrep/src/index/mod.rs`, add after line 3 (`pub mod storage;`):

```rust
pub mod checkpoint;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p colgrep 2>&1 | tail -5`
Expected: Compiles successfully

- [ ] **Step 4: Commit**

```bash
git add colgrep/src/index/checkpoint.rs colgrep/src/index/mod.rs
git commit -m "feat: add chunked indexing checkpoint data structure"
```

---

### Task 2: Add `--no-resume` CLI flag

**Files:**
- Modify: `colgrep/src/cli.rs` (Init variant)
- Modify: `colgrep/src/commands/init.rs` (InitOptions + wiring)
- Modify: `colgrep/src/main.rs` (passthrough)
- Modify: `colgrep/src/index/mod.rs` (IndexBuilder field + setter)

- [ ] **Step 1: Add `--no-resume` flag to CLI**

In `colgrep/src/cli.rs`, add to the `Init` variant after the `chunk_files` field:

```rust
        /// Skip resuming from a previous chunked indexing checkpoint (force fresh start)
        #[arg(long = "no-resume")]
        no_resume: bool,
```

- [ ] **Step 2: Add field to IndexBuilder**

In `colgrep/src/index/mod.rs`, add to the `IndexBuilder` struct after `chunk_files`:

```rust
    /// If true, ignore any existing checkpoint and start fresh
    no_resume: bool,
```

Initialize it in `IndexBuilder::with_options`:

```rust
    no_resume: false,
```

Add setter:

```rust
    pub fn set_no_resume(&mut self, no_resume: bool) {
        self.no_resume = no_resume;
    }
```

- [ ] **Step 3: Wire through init command**

In `colgrep/src/commands/init.rs`, add `no_resume` to `InitOptions` struct:

```rust
    pub no_resume: bool,
```

And in the wiring section where `set_chunked` is called:

```rust
    if opts.no_resume {
        builder.set_no_resume(true);
    }
```

In `colgrep/src/main.rs`, add `no_resume` to the `Init` match arm destructuring and pass it into `InitOptions`.

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p colgrep 2>&1 | tail -5`
Expected: Compiles (possibly with unused field warnings)

- [ ] **Step 5: Commit**

```bash
git add colgrep/src/cli.rs colgrep/src/commands/init.rs colgrep/src/main.rs colgrep/src/index/mod.rs
git commit -m "feat: add --no-resume CLI flag for chunked indexing"
```

---

### Task 3: Wire checkpoint into `full_rebuild_chunked`

**Files:**
- Modify: `colgrep/src/index/mod.rs:1840-1975` (`full_rebuild_chunked`)

This is the core change. Add checkpoint save after each wave, and checkpoint load + wave skipping at the start.

- [ ] **Step 1: Add checkpoint load and resume logic**

In `full_rebuild_chunked`, after the `scan_files` call and before the wave loop, add checkpoint loading. Replace the section from `let (files, skipped) = self.scan_files(languages)?;` through the start of the `for` loop with:

```rust
        let (files, skipped) = self.scan_files(languages)?;
        let total_files = files.len();
        let mut state = IndexState::default();
        let mut total_units: usize = 0;
        let mut first_wave = true;
        let mut start_wave: usize = 0;

        let chunk_files = self.chunk_files;
        let target_index_path = temp_path.clone();

        // Check for existing checkpoint to resume from
        if !self.no_resume {
            if let Ok(Some(ckpt)) = checkpoint::ChunkedCheckpoint::load(&target_index_path) {
                if ckpt.is_valid_for(&files, chunk_files) && ckpt.completed_waves > 0 {
                    eprintln!(
                        "📂 Resuming from wave {} ({} files, {} units already encoded)",
                        ckpt.completed_waves + 1,
                        ckpt.completed_waves * chunk_files,
                        ckpt.total_units,
                    );
                    start_wave = ckpt.completed_waves;
                    state = ckpt.state;
                    total_units = ckpt.total_units;
                    first_wave = false;
                } else {
                    eprintln!("⚠️  Stale checkpoint found (file list changed), starting fresh");
                    checkpoint::ChunkedCheckpoint::cleanup(&target_index_path);
                }
            }
        }

        // Only clean temp dir if starting fresh (no valid checkpoint)
        if start_wave == 0 {
            if temp_path.exists() {
                std::fs::remove_dir_all(&temp_path)?;
            }
        }
        std::fs::create_dir_all(&target_index_path)?;
```

Note: the existing temp_path cleanup at the top of the method (lines 1852-1858) should be moved to be conditional — only clean when `start_wave == 0`. Remove the unconditional cleanup at lines 1852-1858 and replace with the code above.

- [ ] **Step 2: Skip completed waves in the loop**

Change the wave loop from:

```rust
        for (wave_idx, file_chunk) in files.chunks(chunk_files).enumerate() {
```

to:

```rust
        for (wave_idx, file_chunk) in files.chunks(chunk_files).enumerate() {
            // Skip waves already completed in a previous run
            if wave_idx < start_wave {
                continue;
            }
```

- [ ] **Step 3: Save checkpoint after each wave**

After `drop(wave_units);` and before `first_wave = false;`, add:

```rust
            // Save checkpoint so this wave can be skipped on resume
            let ckpt = checkpoint::ChunkedCheckpoint {
                completed_waves: wave_idx + 1,
                chunk_files,
                file_list_hash: checkpoint::ChunkedCheckpoint::hash_file_list(&files),
                total_files,
                state: state.clone(),
                total_units,
            };
            if let Err(e) = ckpt.save(&target_index_path) {
                eprintln!("⚠️  Failed to save checkpoint: {e}");
                // Non-fatal — indexing continues, just can't resume
            }
```

- [ ] **Step 4: Clean up checkpoint on successful completion**

After the atomic swap succeeds (after `state.save(&self.index_dir)?;`), add:

```rust
        // Checkpoint is no longer needed — index is complete
        checkpoint::ChunkedCheckpoint::cleanup(&target_index_path);
```

Note: Since `target_index_path` was renamed to `index_path` by the atomic swap, and the checkpoint is inside `index.tmp/` which was renamed, the cleanup target should be the final `index_path`. Actually — the checkpoint file lives inside `index.tmp/` which gets renamed to the final index path. After the rename, the checkpoint is inside the live index directory. Clean it from there:

```rust
        checkpoint::ChunkedCheckpoint::cleanup(&index_path);
```

- [ ] **Step 5: Update interrupt handlers to NOT delete temp_path**

Currently, interrupt handlers in the wave loop call `std::fs::remove_dir_all(&temp_path)`. This destroys the partial index + checkpoint. Change these to just bail without cleanup, so the checkpoint survives:

Replace all occurrences of:
```rust
            if is_interrupted() {
                let _ = std::fs::remove_dir_all(&temp_path);
                anyhow::bail!("Indexing interrupted by user");
            }
```

with:
```rust
            if is_interrupted() {
                anyhow::bail!("Indexing interrupted by user");
            }
```

There are 3 such blocks in `full_rebuild_chunked`. Remove the `remove_dir_all` from all of them. The `index.tmp/` directory with the checkpoint should survive interruption — that's the whole point.

Also remove the cleanup from the confirmation cancel:
```rust
                {
                    let _ = std::fs::remove_dir_all(&temp_path);
                    anyhow::bail!("Indexing cancelled by user");
                }
```
Change to:
```rust
                {
                    anyhow::bail!("Indexing cancelled by user");
                }
```

- [ ] **Step 6: Also move the old_path cleanup to be conditional**

The `old_path` cleanup at the top should also be conditional on `start_wave == 0`:

```rust
        if start_wave == 0 {
            if temp_path.exists() {
                std::fs::remove_dir_all(&temp_path)?;
            }
            if old_path.exists() {
                std::fs::remove_dir_all(&old_path)?;
            }
        }
```

- [ ] **Step 7: Update progress bar to reflect resume**

When resuming, the progress bar should start from the already-completed position:

After creating the progress bar and before the loop, add:
```rust
        if start_wave > 0 {
            let completed_files = (start_wave * chunk_files).min(total_files);
            file_pb.set_position(completed_files as u64);
        }
```

- [ ] **Step 8: Verify it compiles**

Run: `cargo build -p colgrep 2>&1 | tail -5`
Expected: Compiles successfully

- [ ] **Step 9: Run existing tests**

Run: `cargo test -p colgrep 2>&1 | tail -10`
Expected: All tests pass

- [ ] **Step 10: Commit**

```bash
git add colgrep/src/index/mod.rs
git commit -m "feat: wire checkpoint save/load into full_rebuild_chunked for resume support"
```

---

### Task 4: Integration test for resume

**Files:**
- Create: `colgrep/tests/chunked_resume.rs`

- [ ] **Step 1: Write integration test**

Create `colgrep/tests/chunked_resume.rs`:

```rust
//! Integration test: chunked indexing resume works after interruption.
//!
//! Strategy: index a project with --chunked --chunk-files 5, then clear
//! only some of the state to simulate a partial run, and verify re-running
//! produces a searchable index.

use std::fs;
use std::path::Path;
use std::process::Command;

fn colgrep_bin() -> String {
    env!("CARGO_BIN_EXE_colgrep").to_string()
}

fn create_test_project(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..15 {
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
}

#[test]
fn chunked_indexing_resumes_from_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    create_test_project(&project);

    // First run: full chunked indexing (3 waves of 5 files)
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

    assert!(
        output.status.success(),
        "first colgrep init --chunked failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Second run: should detect no changes and skip quickly
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
        .expect("failed to run colgrep init (second run)");

    assert!(
        output.status.success(),
        "second colgrep init --chunked failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify search still works after double-init
    let output = Command::new(colgrep_bin())
        .args([
            "memory allocation",
            project.to_str().unwrap(),
            "-k",
            "5",
        ])
        .output()
        .expect("failed to run colgrep search");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.is_empty(),
        "search returned no results after resumed indexing"
    );
}

#[test]
fn no_resume_flag_forces_fresh_start() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project2");
    create_test_project(&project);

    // First run
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

    assert!(output.status.success());

    // Second run with --no-resume should still succeed
    let output = Command::new(colgrep_bin())
        .args([
            "init",
            "-y",
            "--chunked",
            "--chunk-files",
            "5",
            "--no-resume",
            project.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run colgrep init --no-resume");

    assert!(
        output.status.success(),
        "colgrep init --no-resume failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify search works
    let output = Command::new(colgrep_bin())
        .args([
            "memory allocation",
            project.to_str().unwrap(),
            "-k",
            "3",
        ])
        .output()
        .expect("failed to run colgrep search");

    assert!(
        output.status.success(),
        "search failed after --no-resume: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p colgrep --test chunked_resume -- --nocapture 2>&1 | tail -20`
Expected: Both tests pass

- [ ] **Step 3: Run full test suite**

Run: `cargo test -p colgrep 2>&1 | tail -10`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add colgrep/tests/chunked_resume.rs
git commit -m "test: add integration tests for chunked indexing resume"
```

---

## Important Notes for Implementer

1. **The checkpoint lives inside `index.tmp/`:** This is the temp directory used during `full_rebuild_chunked`. On successful completion, it's atomically renamed to the final index path. On interruption, it persists for resume.

2. **Checkpoint is JSON:** Simple `serde_json` serialization. The `IndexState` is already `Serialize/Deserialize`. No binary formats needed — the checkpoint is tiny (a few KB even for 164k files).

3. **`index.tmp/` cleanup behavior changes:** Currently, `full_rebuild_chunked` deletes `index.tmp/` on interruption. With resume support, it must NOT delete it — that's where the checkpoint + partial index live. Only delete `index.tmp/` when starting fresh (no valid checkpoint) or after successful completion.

4. **File list ordering matters:** `scan_files` must return files in a deterministic order for `hash_file_list` to produce consistent hashes. Check that `scan_files` sorts its output. If it doesn't, sort the files before hashing and before chunking.

5. **The `first_wave` flag on resume:** When resuming, `first_wave` should be `false` (model was already created in the previous run, but we need to re-create it since it's not persisted). However, `ensure_model_created` is idempotent, so just call it on the first wave we actually process. The current code already handles this correctly since `first_wave` is set to `false` when `start_wave > 0`.

6. **Don't checkpoint the model:** The ONNX model is loaded fresh each run. Only the index data and IndexState are checkpointed. The model initialization overhead (~10-20s) is negligible compared to hours of encoding.
