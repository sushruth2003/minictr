#![allow(clippy::expect_used)]

mod common;

use common::{assert_success, install_binary_with_dependencies, unique_temp_path};
use std::{fs, path::Path, process::Command};

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

    fs::remove_dir_all(&fixture).expect("rootfs fixture should be removed");
}
