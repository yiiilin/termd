use std::process::Command;

#[test]
fn installer_helper_bridge_runs_through_embedded_bash_without_python() {
    let output = Command::new(env!("CARGO_BIN_EXE_termrelay"))
        .args(["install", "--dry-run", "--web"])
        .env("PATH", "/nonexistent/caller-path-without-python")
        .output()
        .expect("termrelay dry-run must start");

    assert!(
        output.status.success(),
        "real embedded helper bridge failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Dry run"));
}
