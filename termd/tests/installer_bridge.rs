use std::process::Command;

#[test]
fn installer_helper_bridge_runs_through_embedded_bash_without_python() {
    let output = Command::new(env!("CARGO_BIN_EXE_termd"))
        .args(["install", "--dry-run", "--web"])
        .env("PATH", "/nonexistent/caller-path-without-python")
        .output()
        .expect("termd dry-run must start");

    assert!(
        output.status.success(),
        "real embedded helper bridge failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Dry run"));
}

#[test]
fn installer_helper_bridge_rejects_forged_shell_parent_without_disclosure() {
    const NONCE: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let binary = env!("CARGO_BIN_EXE_termd");
    let script = r#"
exec {binary_fd}<"$1"
pinned="/proc/$$/fd/${binary_fd}"
TERMD_INSTALL_HELPER_NONCE="$2" \
TERMD_INSTALL_HELPER_SELF_BINARY="$pinned" \
  "$1" __terminstall-helper-v1 self-check
"#;
    let output = Command::new("/bin/bash")
        .args(["-c", script, "installer-forgery", binary, NONCE])
        .env_clear()
        .output()
        .expect("forged helper invocation must start");

    assert!(!output.status.success());
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(rendered.contains("invalid internal installer helper request"));
    assert!(!rendered.contains(NONCE));
    assert!(!rendered.contains(binary));
    assert!(!rendered.contains("/proc/"));
}
