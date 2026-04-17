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
