#![allow(clippy::expect_used)]

mod common;

use common::{assert_success, run_runtime, runtime_cgroup_directories, write_config};
use std::fs;

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

    assert_eq!(output.status.code(), Some(1));
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
