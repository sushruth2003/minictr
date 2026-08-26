#![allow(clippy::expect_used)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn run_runtime(args: &[&str]) -> Output {
    if args.first() != Some(&"run") {
        return Command::new(env!("CARGO_BIN_EXE_minictr"))
            .args(args)
            .output()
            .expect("runtime should be executable");
    }

    let (fixture, rootfs) = create_basic_rootfs("runtime");
    let output = Command::new(env!("CARGO_BIN_EXE_minictr"))
        .arg("run")
        .args(["--rootfs"])
        .arg(&rootfs)
        .args(&args[1..])
        .output()
        .expect("runtime should be executable");
    fs::remove_dir_all(fixture).expect("runtime fixture should be removed");
    output
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

fn create_basic_rootfs(label: &str) -> (PathBuf, PathBuf) {
    let fixture = unique_temp_path(label);
    let rootfs = fixture.join("rootfs");
    fs::create_dir_all(rootfs.join("tmp")).expect("basic rootfs should be created");
    fs::create_dir_all(rootfs.join("dev")).expect("basic rootfs dev directory should be created");
    fs::write(rootfs.join("dev/null"), b"").expect("basic rootfs null placeholder should exist");
    for binary in ["/bin/sh", "/bin/hostname", "/bin/sleep"] {
        install_binary_with_dependencies(Path::new(binary), &rootfs);
    }
    (fixture, rootfs)
}

fn copy_into_rootfs(source: &Path, rootfs: &Path) {
    let relative = source
        .strip_prefix("/")
        .expect("rootfs fixtures must use absolute source paths");
    let destination = rootfs.join(relative);
    fs::create_dir_all(
        destination
            .parent()
            .expect("rootfs fixture destination should have a parent"),
    )
    .expect("rootfs fixture parent should be created");
    fs::copy(source, destination).expect("rootfs fixture should be copied");
}

fn install_binary_with_dependencies(binary: &Path, rootfs: &Path) {
    copy_into_rootfs(binary, rootfs);

    let output = Command::new("ldd")
        .arg(binary)
        .output()
        .expect("ldd should inspect the rootfs fixture binary");
    assert!(output.status.success(), "ldd failed: {output:?}");

    for token in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        if token.starts_with('/') {
            copy_into_rootfs(Path::new(token), rootfs);
        }
    }
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

#[cfg(target_os = "linux")]
#[test]
fn child_cannot_exceed_the_default_pid_limit() {
    let output = run_runtime(&[
        "run",
        "--init",
        "/bin/sh",
        "-c",
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do /bin/sleep 1 & done; wait",
    ]);

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
}

#[test]
fn init_reaps_orphan_without_leaving_zombie() {
    let (fixture, rootfs) = create_basic_rootfs("init-reaping");
    let control = fixture.join("control");
    fs::create_dir_all(&control).expect("control directory should be created");
    fs::create_dir_all(rootfs.join("dev")).expect("rootfs dev directory should be created");
    fs::write(rootfs.join("dev/null"), b"").expect("rootfs null device placeholder should exist");

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

#[test]
fn child_starts_at_root_and_cannot_see_outside_rootfs() {
    let fixture = unique_temp_path("rootfs");
    let rootfs = fixture.join("rootfs");
    let outside_marker = fixture.join("host-only-marker");
    fs::create_dir_all(&rootfs).expect("rootfs fixture should be created");
    fs::write(rootfs.join("inside-marker"), b"inside").expect("inside marker should be created");
    fs::write(&outside_marker, b"outside").expect("outside marker should be created");
    install_binary_with_dependencies(Path::new("/bin/sh"), &rootfs);

    let command = format!(
        "pwd; [ -f /inside-marker ] && [ ! -e /oldroot ] && [ ! -e '{}' ]",
        outside_marker.display()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_minictr"))
        .args(["run", "--rootfs"])
        .arg(&rootfs)
        .args(["--", "/bin/sh", "-c", &command])
        .output()
        .expect("runtime should execute against the fixture rootfs");

    assert_success(&output);
    assert_eq!(output.stdout, b"/\n");
    assert!(
        !rootfs.join("oldroot").exists(),
        "temporary old-root directory should be removed"
    );

    fs::remove_dir_all(&fixture).expect("rootfs fixture should be removed");
}

#[test]
fn bind_mount_exposes_a_host_directory_at_the_container_path() {
    let fixture = unique_temp_path("bind-mount");
    let rootfs = fixture.join("rootfs");
    let host_data = fixture.join("host-data");
    fs::create_dir_all(&rootfs).expect("rootfs fixture should be created");
    fs::create_dir_all(&host_data).expect("host data fixture should be created");
    fs::write(host_data.join("input"), b"from-host").expect("host input should be created");
    install_binary_with_dependencies(Path::new("/bin/sh"), &rootfs);

    let mount = format!("{}:/data", host_data.display());
    let output = Command::new(env!("CARGO_BIN_EXE_minictr"))
        .args(["run", "--rootfs"])
        .arg(&rootfs)
        .args(["--mount", &mount, "--", "/bin/sh", "-c"])
        .arg("read value < /data/input; printf '%s' \"$value\"; printf mounted > /data/output")
        .output()
        .expect("runtime should execute with a bind mount");

    assert_success(&output);
    assert_eq!(output.stdout, b"from-host");
    assert_eq!(
        fs::read(host_data.join("output")).expect("container output should reach the host"),
        b"mounted"
    );

    fs::remove_dir_all(&fixture).expect("bind mount fixture should be removed");
}

#[test]
fn m3_acceptance_covers_pid_hostname_rootfs_proc_tmp_and_bind_mount() {
    let fixture = unique_temp_path("m3-acceptance");
    let rootfs = fixture.join("rootfs");
    let host_data = fixture.join("data");
    let host_tmp_path = unique_temp_path("container-only-file");
    let container_tmp_name = host_tmp_path
        .file_name()
        .expect("unique temp path should have a file name")
        .to_string_lossy();

    fs::create_dir_all(rootfs.join("etc")).expect("rootfs etc should be created");
    fs::create_dir_all(rootfs.join("tmp")).expect("rootfs tmp should be created");
    fs::create_dir_all(&host_data).expect("bind source should be created");
    fs::write(rootfs.join("etc/hostname"), b"rootfs-hostname\n")
        .expect("rootfs hostname should be created");
    fs::write(host_data.join("x"), b"hello\n").expect("bind source file should be created");
    for binary in [
        "/bin/sh",
        "/bin/hostname",
        "/bin/cat",
        "/bin/touch",
        "/bin/ps",
    ] {
        install_binary_with_dependencies(Path::new(binary), &rootfs);
    }

    assert!(
        !host_tmp_path.exists(),
        "host-side tmp marker must not exist before the test"
    );

    let mount = format!("{}:/data", host_data.display());
    let script = format!(
        r#"printf 'PID=%s\n' "$$"
printf 'UTS='; /bin/hostname
printf 'ROOTFS_HOSTNAME='; /bin/cat /etc/hostname
printf 'PS_BEGIN\n'; /bin/ps -e -o pid=,comm=; printf 'PS_END\n'
/bin/touch /tmp/{container_tmp_name}
printf 'BIND='; /bin/cat /data/x"#
    );
    let output = Command::new(env!("CARGO_BIN_EXE_minictr"))
        .args(["run", "--rootfs"])
        .arg(&rootfs)
        .args([
            "--hostname",
            "mini",
            "--mount",
            &mount,
            "--",
            "/bin/sh",
            "-c",
        ])
        .arg(&script)
        .output()
        .expect("M3 acceptance container should execute");

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PID=1\n"), "stdout was: {stdout}");
    assert!(stdout.contains("UTS=mini\n"), "stdout was: {stdout}");
    assert!(
        stdout.contains("ROOTFS_HOSTNAME=rootfs-hostname\n"),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("BIND=hello\n"), "stdout was: {stdout}");

    let ps_output = stdout
        .split_once("PS_BEGIN\n")
        .and_then(|(_, output)| output.split_once("PS_END\n"))
        .map(|(output, _)| output)
        .expect("ps output should be delimited");
    let pids = ps_output
        .lines()
        .map(|line| {
            line.split_whitespace()
                .next()
                .expect("ps line should include a PID")
                .parse::<u32>()
                .expect("ps PID should be numeric")
        })
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2, "ps output was: {ps_output}");
    assert_eq!(pids[0], 1, "ps output was: {ps_output}");
    assert!(pids[1] > 1, "ps output was: {ps_output}");

    assert!(
        rootfs
            .join("tmp")
            .join(container_tmp_name.as_ref())
            .exists(),
        "container tmp marker should exist in the rootfs"
    );
    assert!(
        !host_tmp_path.exists(),
        "container tmp marker must not appear in host /tmp"
    );
    assert_eq!(
        fs::read(host_data.join("x")).expect("bind source should remain readable"),
        b"hello\n"
    );

    fs::remove_dir_all(&fixture).expect("M3 fixture should be removed");
}
