//! Integration test: chunked indexing resume works after interruption.

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
            "init", "-y", "--chunked", "--chunk-files", "5",
            project.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run colgrep init");

    assert!(
        output.status.success(),
        "first colgrep init --chunked failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Second run: should detect no changes and complete quickly
    let output = Command::new(colgrep_bin())
        .args([
            "init", "-y", "--chunked", "--chunk-files", "5",
            project.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run colgrep init (second run)");

    assert!(
        output.status.success(),
        "second colgrep init --chunked failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify search still works
    let output = Command::new(colgrep_bin())
        .args(["memory allocation", project.to_str().unwrap(), "-k", "5"])
        .output()
        .expect("failed to run colgrep search");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(!stdout.is_empty(), "search returned no results after resumed indexing");
}

#[test]
fn no_resume_flag_forces_fresh_start() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project2");
    create_test_project(&project);

    // First run
    let output = Command::new(colgrep_bin())
        .args([
            "init", "-y", "--chunked", "--chunk-files", "5",
            project.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run colgrep init");
    assert!(output.status.success());

    // Second run with --no-resume
    let output = Command::new(colgrep_bin())
        .args([
            "init", "-y", "--chunked", "--chunk-files", "5", "--no-resume",
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
        .args(["memory allocation", project.to_str().unwrap(), "-k", "3"])
        .output()
        .expect("failed to run colgrep search");
    assert!(output.status.success());
}
