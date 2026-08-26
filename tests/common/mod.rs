#![allow(dead_code)]

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) fn run_runtime(args: &[&str]) -> Output {
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

pub(crate) fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "runtime failed with status {:?}\nstdout: {:?}\nstderr: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub(crate) fn assert_exec_failure(output: &Output, start: Instant) {
    assert_eq!(output.status.code(), Some(1));
    assert!(start.elapsed() < Duration::from_secs(2));
}

pub(crate) fn unique_temp_path(label: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "minictr-{label}-{}-{timestamp}",
        std::process::id()
    ))
}

pub(crate) fn write_config(label: &str, contents: &str) -> PathBuf {
    let path = unique_temp_path(label).with_extension("json");
    fs::write(&path, contents).expect("runtime config should be written");
    path
}

#[cfg(target_os = "linux")]
pub(crate) fn runtime_cgroup_directories() -> BTreeSet<OsString> {
    let parent = Path::new("/sys/fs/cgroup/minictr");
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeSet::new(),
        Err(error) => panic!("minictr cgroup should be readable: {error}"),
    };

    entries
        .map(|entry| entry.expect("cgroup entry should be readable"))
        .filter(|entry| {
            entry
                .file_type()
                .expect("cgroup entry type should be readable")
                .is_dir()
        })
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with("runtime-"))
        .collect()
}

pub(crate) fn create_basic_rootfs(label: &str) -> (PathBuf, PathBuf) {
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

pub(crate) fn install_binary_with_dependencies(binary: &Path, rootfs: &Path) {
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
