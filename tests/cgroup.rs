#![allow(clippy::expect_used)]

mod common;

use common::{
    assert_success, create_basic_rootfs, run_runtime, runtime_cgroup_directories, write_config,
};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn no_config_preserves_unlimited_default_behavior() {
    let output = run_runtime(&[
        "run",
        "--init",
        "/bin/sh",
        "-c",
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do /bin/sleep 0.1 & done; wait",
    ]);

    assert_success(&output);
}

#[test]
fn invalid_config_fails_before_container_start() {
    let config = write_config("invalid-config", r#"{"resources":{"pid":{"max":4}}}"#);
    let config_arg = config.to_string_lossy().into_owned();
    let output = run_runtime(&[
        "run",
        "--config",
        &config_arg,
        "/bin/sh",
        "-c",
        "printf should-not-run",
    ]);
    fs::remove_file(config).expect("invalid config fixture should be removed");

    assert_eq!(output.status.code(), Some(125));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field `pid`"),
        "stderr was: {stderr}"
    );
    assert!(!stderr.contains("inside child"), "stderr was: {stderr}");
    assert!(output.stdout.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn configured_pid_limit_is_enforced_and_cgroup_is_removed() {
    let before = runtime_cgroup_directories();
    let config = write_config("pids-limit", r#"{"resources":{"pids":{"max":4}}}"#);
    let config_arg = config.to_string_lossy().into_owned();
    let output = run_runtime(&[
        "run",
        "--config",
        &config_arg,
        "--init",
        "/bin/sh",
        "-c",
        "for i in 1 2 3 4 5 6 7 8; do /bin/sleep 1 & done; wait",
    ]);
    fs::remove_file(config).expect("PID config fixture should be removed");

    assert!(
        !output.status.success(),
        "the workload unexpectedly exceeded pids.max"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("can't fork")
            || stderr.contains("Cannot fork")
            || stderr.contains("Resource temporarily unavailable"),
        "the workload forked without hitting pids.max; stderr was: {stderr}"
    );
    assert_eq!(runtime_cgroup_directories(), before);
}

#[cfg(target_os = "linux")]
#[test]
fn terminating_host_runtime_forwards_signal_and_removes_cgroup() {
    let (fixture, rootfs) = create_basic_rootfs("signal-cleanup");
    let control = fixture.join("control");
    fs::create_dir_all(&control).expect("control directory should be created");
    let config = write_config("signal-cleanup", r#"{"resources":{"pids":{"max":16}}}"#);

    let mut child = Command::new(env!("CARGO_BIN_EXE_minictr"))
        .args(["run", "--rootfs"])
        .arg(&rootfs)
        .args(["--config"])
        .arg(&config)
        .args(["--init", "--mount"])
        .arg(format!("{}:/control", control.display()))
        .args([
            "/bin/sh",
            "-c",
            "trap 'printf terminated > /control/terminated; exit 42' TERM; printf ready > /control/ready; while :; do /bin/sleep 1; done",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("runtime should start for signal cleanup test");
    let runtime_cgroup_prefix = format!("runtime-{}-", child.id());

    let deadline = Instant::now() + Duration::from_secs(5);
    while !control.join("ready").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        control.join("ready").exists(),
        "workload did not become ready"
    );
    assert!(
        runtime_cgroup_directories()
            .iter()
            .any(|name| name.to_string_lossy().starts_with(&runtime_cgroup_prefix)),
        "runtime-specific cgroup was not created"
    );

    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM)
        .expect("host runtime should accept SIGTERM");

    let deadline = Instant::now() + Duration::from_secs(5);
    while child
        .try_wait()
        .expect("runtime status should be readable")
        .is_none()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    if child
        .try_wait()
        .expect("runtime status should be readable")
        .is_none()
    {
        child.kill().expect("timed out runtime should be killed");
        panic!("runtime did not exit after SIGTERM");
    }

    let output = child
        .wait_with_output()
        .expect("runtime output should be collected");
    assert_eq!(
        output.status.code(),
        Some(42),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(control.join("terminated").exists());
    assert!(
        !runtime_cgroup_directories()
            .iter()
            .any(|name| name.to_string_lossy().starts_with(&runtime_cgroup_prefix)),
        "runtime-specific cgroup was not removed"
    );

    fs::remove_file(config).expect("signal config fixture should be removed");
    fs::remove_dir_all(fixture).expect("signal fixture should be removed");
}
