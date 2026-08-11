#![allow(clippy::expect_used)]

use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn run_runtime(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_minictr"))
        .args(args)
        .output()
        .expect("runtime should be executable")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "runtime failed with status {:?}\nstdout: {:?}\nstderr: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_exec_failure(output: &Output, start: Instant) {
    assert_eq!(output.status.code(), Some(1));
    assert!(start.elapsed() < Duration::from_secs(2));
}

fn unique_temp_path(label: &str) -> std::path::PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "minictr-{label}-{}-{timestamp}",
        std::process::id()
    ))
}

#[test]
fn exit_code_is_propagated_from_child() {
    let output = run_runtime(&["run", "--hostname", "testbox", "/bin/sh", "-c", "exit 37"]);

    assert_eq!(output.status.code(), Some(37));
}

#[test]
fn command_arguments_are_passed_without_rearrangement() {
    let output = run_runtime(&[
        "run",
        "--hostname",
        "testbox",
        "/bin/sh",
        "-c",
        "printf '%s|%s|%s' \"$1\" \"$2\" \"$3\"",
        "argument-script-name",
        "first",
        "second",
        "third",
    ]);

    assert_success(&output);
    assert_eq!(output.stdout, b"first|second|third");
}

#[test]
fn stdout_and_stderr_are_passed_through_separately() {
    let output = run_runtime(&[
        "run",
        "--hostname",
        "testbox",
        "/bin/sh",
        "-c",
        "printf stdout-marker; printf stderr-marker >&2",
    ]);

    assert_success(&output);
    assert_eq!(output.stdout, b"stdout-marker");
    assert!(String::from_utf8_lossy(&output.stderr).contains("stderr-marker"));
}

#[test]
fn nonexistent_executable_fails_quickly_without_reporting_a_child() {
    let start = Instant::now();
    let output = run_runtime(&[
        "run",
        "--hostname",
        "testbox",
        "/definitely/not/a/minictr-test-executable",
    ]);

    assert_exec_failure(&output, start);
}

#[test]
fn non_executable_child_file_fails_during_exec() {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::temp_dir().join(format!("minictr-non-executable-{}", std::process::id()));
    fs::write(&path, b"#!/bin/sh\nprintf should-not-run\n").expect("test file should be writable");
    let mut permissions = fs::metadata(&path)
        .expect("test file should exist")
        .permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&path, permissions).expect("test file permissions should be set");

    let path_string = path.to_string_lossy().into_owned();
    let start = Instant::now();
    let output = run_runtime(&["run", "--hostname", "testbox", &path_string]);
    let _ = fs::remove_file(&path);

    assert_exec_failure(&output, start);
}

#[test]
fn child_sees_configured_uts_hostname_and_host_is_unchanged() {
    let before = Command::new("/bin/hostname")
        .output()
        .expect("host hostname should be readable");
    assert!(before.status.success());

    let output = run_runtime(&["run", "--hostname", "minictr-testbox", "/bin/hostname"]);
    assert_success(&output);
    assert_eq!(output.stdout, b"minictr-testbox\n");

    let after = Command::new("/bin/hostname")
        .output()
        .expect("host hostname should still be readable");
    assert!(after.status.success());
    assert_eq!(before.stdout, after.stdout);
}

#[test]
fn ordinary_command_still_works_with_uts_isolation() {
    let output = run_runtime(&[
        "run",
        "--hostname",
        "testbox",
        "/bin/sh",
        "-c",
        "printf hello",
    ]);

    assert_success(&output);
    assert_eq!(output.stdout, b"hello");
}

#[test]
fn runtime_init_is_pid_one_and_user_command_is_pid_two() {
    let output = run_runtime(&[
        "run",
        "--hostname",
        "testbox",
        "/bin/sh",
        "-c",
        "printf 'command-pid=%s command-ppid=%s\\n' \"$$\" \"$PPID\"; exec awk '/^NSpid:/ { print }' /proc/self/status",
    ]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("inside child: 1"), "stderr was: {stderr}");
    assert!(
        stdout.contains("command-pid=2 command-ppid=1"),
        "stdout was: {stdout}"
    );

    let nspid_line = stdout
        .lines()
        .find(|line| line.starts_with("NSpid:"))
        .expect("user command should expose an NSpid line");
    let nspids = nspid_line
        .split_whitespace()
        .skip(1)
        .map(|pid| pid.parse::<u32>().expect("NSpid values should be numeric"))
        .collect::<Vec<_>>();
    assert!(nspids.len() >= 2, "NSpid line was: {nspid_line}");
    assert!(
        nspids[0] > 1,
        "host PID should be greater than init: {nspid_line}"
    );
    assert_eq!(
        nspids.last(),
        Some(&2),
        "namespace PID should be 2: {nspid_line}"
    );
}

#[test]
fn runtime_init_reaps_single_user_child() {
    let pid_file = unique_temp_path("nspid");
    let pid_file_string = pid_file.to_string_lossy().into_owned();
    let command = format!(
        r#"while IFS= read -r line; do
    case "$line" in
        NSpid:*) set -- $line; printf '%s\n' "$2" > "{pid_file_string}"; break ;;
    esac
done < /proc/self/status"#
    );

    let output = run_runtime(&["run", "--hostname", "testbox", "/bin/sh", "-c", &command]);
    assert_success(&output);

    let host_pid = fs::read_to_string(&pid_file)
        .expect("user command should record its host PID")
        .trim()
        .parse::<u32>()
        .expect("recorded host PID should be numeric");
    let _ = fs::remove_file(&pid_file);

    assert!(!Path::new(&format!("/proc/{host_pid}")).exists());
}
