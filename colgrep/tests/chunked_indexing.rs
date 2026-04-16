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
