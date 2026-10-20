use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_kashmir")
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kashmir-cli-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn discover_emits_machine_readable_profile() {
    let output = Command::new(binary())
        .args(["discover", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with('{'));
    assert!(stdout.contains("\"hardware_fingerprint\"") || stdout.contains("\"fingerprint\""));
    assert!(stdout.trim_end().ends_with('}'));
}

#[test]
fn spin_and_weave_install_a_temporal_policy() {
    let root = scratch("weave");
    let target = root.join("target-project");
    let spin = root.join("cpu.spin");
    fs::create_dir_all(&target).unwrap();

    let spin_status = Command::new(binary())
        .args(["spin", "cpu", "--ttl", "60", "--output"])
        .arg(&spin)
        .status()
        .unwrap();
    assert!(spin_status.success());

    let weave_status = Command::new(binary())
        .arg("weave")
        .arg(&spin)
        .arg(&target)
        .status()
        .unwrap();
    assert!(weave_status.success());
    assert!(target.join(".kashmir/active.spin").is_file());
    let environment = fs::read_to_string(target.join(".kashmir/active.env")).unwrap();
    assert!(environment.contains("KASHMIR_WORKLOAD=cpu-bound"));
    assert!(environment.contains("RAYON_NUM_THREADS="));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn pinned_run_preserves_executor_environment() {
    let status = Command::new(binary())
        .args([
            "run",
            "cpu",
            "--pin",
            "--",
            "/bin/sh",
            "-c",
            "test -n \"$RAYON_NUM_THREADS\" && test -n \"$TOKIO_WORKER_THREADS\"",
        ])
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(target_os = "linux")]
#[test]
fn weave_runs_a_command_with_the_temporal_environment() {
    let root = scratch("command");
    let target = root.join("target-project");
    let spin = root.join("cpu.spin");
    fs::create_dir_all(&target).unwrap();
    assert!(
        Command::new(binary())
            .args(["spin", "cpu", "--output"])
            .arg(&spin)
            .status()
            .unwrap()
            .success()
    );

    let output = Command::new(binary())
        .arg("weave")
        .arg(&spin)
        .arg(&target)
        .args(["--", "/bin/sh", "-c", "printf %s \"$KASHMIR_WORKLOAD\""])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("cpu-bound"));
    fs::remove_dir_all(root).unwrap();
}
