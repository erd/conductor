use std::process::Command;

#[test]
fn version_prints_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
        .arg("--version")
        .output()
        .expect("run conductor --version");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("conductor {}", env!("CARGO_PKG_VERSION"))
    );
}
