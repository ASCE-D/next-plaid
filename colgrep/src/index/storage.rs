use anyhow::{Context, Result};
use std::path::Path;

/// Verify write access to a directory by writing and reading a probe file.
pub fn verify_write_access(dir: &Path) -> Result<()> {
    if !dir.exists() {
        anyhow::bail!(
            "Cannot write to {}\nReason: Directory does not exist\n\n\
             Fix: Create the directory:\n  mkdir -p {}",
            dir.display(),
            dir.display()
        );
    }

    let pid = std::process::id();
    let probe_path = dir.join(format!(".colgrep_write_probe_{}", pid));

    // Write
    std::fs::write(&probe_path, b"probe")
        .with_context(|| format!(
            "Cannot write to {}\nReason: Permission denied\n\n\
             Fix: Ensure the directory is writable:\n  chmod 775 {}\n  \
             # or in K8s, set fsGroup in pod securityContext",
            dir.display(),
            dir.display()
        ))?;

    // Fsync
    let file = std::fs::File::open(&probe_path)?;
    file.sync_all()?;

    // Read back
    let content = std::fs::read(&probe_path)?;
    if content != b"probe" {
        anyhow::bail!("Write verification failed: read-back mismatch at {}", dir.display());
    }

    // Cleanup
    let _ = std::fs::remove_file(&probe_path);

    Ok(())
}

/// Create directory if it doesn't exist, then verify write access.
pub fn init_output_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create output directory: {}", dir.display()))?;
    verify_write_access(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_verify_write_access_succeeds_on_writable_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(verify_write_access(tmp.path()).is_ok());
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
        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert!(entries.iter().all(|e| {
            !e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("colgrep_write_probe")
        }));
    }

    #[test]
    fn test_init_output_dir_creates_nested() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c");
        assert!(!nested.exists());
        init_output_dir(&nested).unwrap();
        assert!(nested.exists());
    }
}
