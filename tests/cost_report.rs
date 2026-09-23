use std::{path::PathBuf, process::Command};

#[test]
fn metadata_crosses_crates_and_compilation_prints_one_numeric_bound() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO"))
        .args(["check", "--offline", "--manifest-path"])
        .arg(root.join("tests/fixtures/cost-consumer/Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target/cost-report-fixture"))
        .output()
        .expect("run the cross-crate cost-check fixture");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // Cargo replays procedural macro stderr for a cached build as well.
    assert!(
        stderr.contains("Abacus cost for external_read: 1"),
        "{stderr}"
    );
    assert!(!stderr.contains("external_read: ("), "{stderr}");
}
