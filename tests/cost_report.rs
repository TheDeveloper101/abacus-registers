use std::{path::PathBuf, process::Command};
#[test]
fn cross_crate_api_prints_a_single_integer_bound() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO"))
        .args(["check", "--offline", "--manifest-path"])
        .arg(root.join("tests/fixtures/cost-consumer/Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target/cost-report-fixture"))
        .output()
        .expect("compile cost report fixture");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Heliod cost for external_read: 3"),
        "{stderr}"
    );
    assert!(!stderr.contains("external_read: ("), "{stderr}");
}
