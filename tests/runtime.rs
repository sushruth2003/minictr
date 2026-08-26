#![allow(clippy::expect_used)]

mod common;

use common::{assert_exec_failure, assert_success, create_basic_rootfs, run_runtime};
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

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
fn user_command_executes_as_pid_one() {
    let output = run_runtime(&[
        "run",
        "--hostname",
        "testbox",
        "/bin/sh",
        "-c",
        "printf 'command-pid=%s command-ppid=%s\\n' \"$$\" \"$PPID\"",
    ]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("inside child: 1"), "stderr was: {stderr}");
    assert!(
        stdout.contains("command-pid=1 command-ppid=0"),
        "stdout was: {stdout}"
    );
}

#[test]
fn init_flag_runs_user_command_as_child_of_container_init() {
    let output = run_runtime(&[
        "run",
        "--init",
        "/bin/sh",
        "-c",
        "printf 'command-pid=%s command-ppid=%s\\n' \"$$\" \"$PPID\"",
    ]);

    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "command-pid=2 command-ppid=1\n"
    );
}

#[test]
fn init_flag_preserves_workload_exit_status() {
    let output = run_runtime(&["run", "--init", "/bin/sh", "-c", "sleep 1 & exit 37"]);

    assert_eq!(output.status.code(), Some(37));
}

#[test]
fn init_exec_failure_returns_command_not_found_status() {
    let output = run_runtime(&["run", "--init", "/definitely/not/a/minictr-test-executable"]);

    assert_eq!(output.status.code(), Some(127));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to execute command"),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn init_reaps_orphan_without_leaving_zombie() {
    let (fixture, rootfs) = create_basic_rootfs("init-reaping");
    let control = fixture.join("control");
    fs::create_dir_all(&control).expect("control directory should be created");

    let command = r#"
( trap '' HUP
  while [ ! -s /control/orphan.pid ]; do
    sleep 0.01
  done
  read orphan_pid < /control/orphan.pid
  sleep 0.5
  state=gone
  if [ -e "/proc/$orphan_pid/status" ]; then
    while IFS= read -r line; do
      case "$line" in
        State:*) state="$line"; break ;;
      esac
    done < "/proc/$orphan_pid/status"
  fi
  printf '%s\n' "$state" > /control/orphan.state
) &

( trap '' HUP
  ( trap '' HUP; sleep 0.2 ) &
  printf '%s\n' "$!" > /control/orphan.pid
) &
orphan_parent_pid=$!
wait "$orphan_parent_pid"
exit 37
"#;

    let child = Command::new(env!("CARGO_BIN_EXE_minictr"))
        .args(["run", "--rootfs"])
        .arg(&rootfs)
        .args(["--init", "--mount"])
        .arg(format!("{}:/control", control.display()))
        .args(["/bin/sh", "-c", command])
        .spawn()
        .expect("runtime should start for init reaping test");

    let deadline = Instant::now() + Duration::from_secs(2);
    let state_path = control.join("orphan.state");
    while !state_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    let output = child
        .wait_with_output()
        .expect("runtime should finish init reaping test");
    let state = fs::read_to_string(&state_path).expect("checker should report orphan state");

    assert_eq!(output.status.code(), Some(37));
    assert_eq!(state.trim(), "gone", "orphan state was: {state}");

    fs::remove_dir_all(fixture).expect("init reaping fixture should be removed");
}

#[test]
fn procfs_only_exposes_processes_in_the_child_pid_namespace() {
    let host_test_pid = std::process::id();
    let command = format!(
        r#"while IFS= read -r line; do
    case "$line" in
        NSpid:*) printf '%s\n' "$line"; break ;;
    esac
done < /proc/self/status
printf 'proc-pids='
for path in /proc/[0-9]*; do
    printf '%s,' "$path"
done
printf '\n'
[ ! -e /proc/{host_test_pid} ]"#
    );

    let output = run_runtime(&["run", "/bin/sh", "-c", &command]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let nspid_line = stdout
        .lines()
        .find(|line| line.starts_with("NSpid:"))
        .expect("user command should expose an NSpid line");
    let nspids = nspid_line
        .split_whitespace()
        .skip(1)
        .map(|pid| pid.parse::<u32>().expect("NSpid values should be numeric"))
        .collect::<Vec<_>>();

    assert_eq!(nspids, [1], "NSpid line was: {nspid_line}");
    assert!(
        stdout.contains("proc-pids=/proc/1,"),
        "stdout was: {stdout}"
    );
}

#[test]
fn runtime_waits_for_container_pid_one() {
    let start = Instant::now();
    let output = run_runtime(&["run", "/bin/sleep", "0.1"]);

    assert_success(&output);
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "runtime exited before its user child"
    );
}
