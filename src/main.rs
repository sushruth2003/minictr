use clap::{Parser, error::ErrorKind};
use nix::{
    mount::{MntFlags, MsFlags, mount, umount2},
    sched::{CloneFlags, clone},
    sys::{
        signal::Signal,
        wait::{WaitStatus, waitpid},
    },
    unistd::{gethostname, pivot_root, sethostname},
};
use std::{
    ffi::OsString,
    fs, io,
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

const MAX_HOSTNAME_LEN: usize = 64;
const OLD_ROOT_PATH: &str = "/oldroot";

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(author, version, about)]
struct Cli {
    operation: String,
    #[arg(long, value_name = "path")]
    rootfs: PathBuf,
    #[arg(long, value_name = "hostname", value_parser = validate_hostname)]
    hostname: Option<String>,
    #[arg(
        long = "mount",
        value_name = "host_path:container_path",
        value_parser = parse_bind_mount
    )]
    bind_mounts: Vec<BindMount>,
    #[arg(num_args = 1.., value_name = "command", allow_hyphen_values = true)]
    command_tokens: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BindMount {
    source: PathBuf,
    destination: PathBuf,
}

fn parse_bind_mount(value: &str) -> Result<BindMount, String> {
    let (source, destination) = value
        .rsplit_once(':')
        .ok_or_else(|| "mount must use host_path:container_path syntax".to_owned())?;
    if source.is_empty() {
        return Err("mount host path must not be empty".to_owned());
    }

    let destination = PathBuf::from(destination);
    if !destination.is_absolute() {
        return Err("mount container path must be absolute".to_owned());
    }
    if destination
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err("mount container path must not contain '..'".to_owned());
    }
    if destination == Path::new("/") {
        return Err("mount container path must not replace the container root".to_owned());
    }
    if destination.starts_with(OLD_ROOT_PATH) {
        return Err(format!(
            "mount container path must not use reserved path {OLD_ROOT_PATH}"
        ));
    }

    Ok(BindMount {
        source: PathBuf::from(source),
        destination,
    })
}

fn validate_hostname(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("hostname must not be empty".to_owned());
    }
    if value.len() > MAX_HOSTNAME_LEN {
        return Err(format!("hostname must be at most {MAX_HOSTNAME_LEN} bytes"));
    }
    if value
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.'))
    {
        return Err("hostname may contain only letters, digits, '-' and '.'".to_owned());
    }
    for label in value.split('.') {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return Err("hostname labels must not be empty or start/end with '-'".to_owned());
        }
    }
    Ok(value.to_owned())
}

fn parse_cli<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    if cli.operation != "run" {
        return Err(clap::Error::raw(
            ErrorKind::ValueValidation,
            "only supported operation is run",
        ));
    }
    Ok(cli)
}

fn init_container(args: &Cli) -> isize {
    if let Err(error) = make_mount_tree_private() {
        eprintln!("failed to make mount tree private: {error}");
        return 1;
    }

    if let Err(error) = make_rootfs_mount_point(&args.rootfs) {
        eprintln!("failed to make rootfs a mount point: {error}");
        return 1;
    }

    for bind_mount in &args.bind_mounts {
        if let Err(error) = mount_bind(&args.rootfs, bind_mount) {
            eprintln!(
                "failed to bind mount {} at {}: {error}",
                bind_mount.source.display(),
                bind_mount.destination.display()
            );
            return 1;
        }
    }

    if let Err(error) = pivot_into_rootfs(&args.rootfs) {
        eprintln!("failed to pivot into rootfs: {error}");
        return 1;
    }

    if let Err(error) = mount_procfs() {
        eprintln!("failed to mount procfs: {error}");
        return 1;
    }

    if let Some(hostname) = &args.hostname {
        if let Err(error) = sethostname(hostname).map_err(nix_error_to_io) {
            eprintln!("failed to set hostname: {error}");
            return 1;
        }

        match gethostname() {
            Ok(name) => eprintln!("runtime process hostname = {}", name.to_string_lossy()),
            Err(error) => {
                eprintln!("failed to read hostname: {error}");
                return 1;
            }
        }
    }

    eprintln!("inside child: {}", std::process::id());
    let error = exec_command(&args.command_tokens);
    eprintln!("failed to execute command: {error}");
    1
}

fn make_mount_tree_private() -> io::Result<()> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        private_mount_flags(),
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

fn private_mount_flags() -> MsFlags {
    MsFlags::MS_PRIVATE | MsFlags::MS_REC
}

fn make_rootfs_mount_point(rootfs: &Path) -> io::Result<()> {
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

fn mount_bind(rootfs: &Path, bind_mount: &BindMount) -> io::Result<()> {
    let target = prepare_bind_target(rootfs, bind_mount)?;
    let mut flags = MsFlags::MS_BIND;
    if bind_mount.source.is_dir() {
        flags |= MsFlags::MS_REC;
    }

    mount(
        Some(bind_mount.source.as_path()),
        target.as_path(),
        None::<&str>,
        flags,
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

fn prepare_bind_target(rootfs: &Path, bind_mount: &BindMount) -> io::Result<PathBuf> {
    let relative_destination = bind_mount
        .destination
        .strip_prefix("/")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mount target is not absolute"))?;
    let target = rootfs.join(relative_destination);
    ensure_existing_ancestor_is_in_rootfs(rootfs, &target)?;

    if bind_mount.source.is_dir() {
        fs::create_dir_all(&target)?;
    } else {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "mount target has no parent")
        })?;
        fs::create_dir_all(parent)?;
        if !target.exists() {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
        }
    }

    ensure_existing_ancestor_is_in_rootfs(rootfs, &target)?;
    Ok(target)
}

fn ensure_existing_ancestor_is_in_rootfs(rootfs: &Path, target: &Path) -> io::Result<()> {
    let mut existing = target;
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "mount target has no existing ancestor",
            )
        })?;
    }

    let resolved = fs::canonicalize(existing)?;
    if !resolved.starts_with(rootfs) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mount target resolves outside rootfs through {}",
                existing.display()
            ),
        ));
    }
    Ok(())
}

fn pivot_into_rootfs(rootfs: &Path) -> io::Result<()> {
    let put_old = rootfs.join(OLD_ROOT_PATH.trim_start_matches('/'));
    prepare_old_root_directory(&put_old)?;

    std::env::set_current_dir(rootfs)?;
    pivot_root(Path::new("."), Path::new("oldroot")).map_err(nix_error_to_io)?;
    std::env::set_current_dir("/")?;
    umount2(OLD_ROOT_PATH, MntFlags::MNT_DETACH).map_err(nix_error_to_io)?;
    fs::remove_dir(OLD_ROOT_PATH)
}

fn prepare_old_root_directory(put_old: &Path) -> io::Result<()> {
    match fs::symlink_metadata(put_old) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if fs::read_dir(put_old)?.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "reserved old-root directory is not empty: {}",
                        put_old.display()
                    ),
                ));
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "reserved old-root path is not a directory: {}",
                put_old.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(put_old),
        Err(error) => Err(error),
    }
}

fn mount_procfs() -> io::Result<()> {
    fs::create_dir_all("/proc")?;
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

fn namespace_clone_flags() -> CloneFlags {
    CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS
}

#[allow(unsafe_code)]
fn initialize_namespaced_runtime(args: Cli) -> io::Result<i32> {
    let rootfs = fs::canonicalize(&args.rootfs)?;
    if !rootfs.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("rootfs is not a directory: {}", rootfs.display()),
        ));
    }
    let mut args = args;
    args.rootfs = rootfs;
    for bind_mount in &mut args.bind_mounts {
        bind_mount.source = fs::canonicalize(&bind_mount.source).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to resolve bind mount source {}: {error}",
                    bind_mount.source.display()
                ),
            )
        })?;
    }

    let mut stack = vec![0u8; 1024 * 1024];
    let flags = namespace_clone_flags();

    // SAFETY: the child stack remains alive until the cloned runtime exits,
    // and the callback only captures data owned by this function.
    let runtime_pid = unsafe {
        clone(
            Box::new(|| init_container(&args)),
            &mut stack,
            flags,
            Some(Signal::SIGCHLD as i32),
        )
    }?;
    match waitpid(runtime_pid, None)? {
        WaitStatus::Exited(_, code) => Ok(code),
        WaitStatus::Signaled(_, signal, _) => {
            // PID 1 was killed by a signal
            Ok(128 + signal as i32)
        }

        other => {
            // Unexpected for a blocking waitpid with no special flags
            Err(io::Error::other(format!(
                "unexpected wait status: {other:?}"
            )))
        }
    }
}

fn nix_error_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn exec_command(command_tokens: &[String]) -> io::Error {
    let command = &command_tokens[0];
    let mut command_builder = Command::new(command);
    command_builder.args(&command_tokens[1..]);
    command_builder.exec()
}

fn main() {
    let args = match parse_cli(std::env::args_os()) {
        Ok(args) => args,
        Err(error) => error.exit(),
    };

    match initialize_namespaced_runtime(args) {
        Ok(code) => {
            std::process::exit(code);
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cli_args_become_runtime_configuration() {
        let cli = parse_cli([
            "minictr",
            "run",
            "--rootfs",
            "./rootfs",
            "--hostname",
            "testbox",
            "--mount",
            "/host/data:/data",
            "--",
            "sh",
            "-c",
            "printf hello",
        ])
        .unwrap();

        assert_eq!(cli.operation, "run");
        assert_eq!(cli.rootfs, PathBuf::from("./rootfs"));
        assert_eq!(cli.hostname.as_deref(), Some("testbox"));
        assert_eq!(
            cli.bind_mounts,
            [BindMount {
                source: PathBuf::from("/host/data"),
                destination: PathBuf::from("/data"),
            }]
        );
        assert_eq!(cli.command_tokens, ["sh", "-c", "printf hello"]);
    }

    #[test]
    fn rootfs_is_parsed_before_the_command_separator() {
        let cli = parse_cli(["minictr", "run", "--rootfs", "./rootfs", "--", "/bin/sh"]).unwrap();

        assert_eq!(cli.rootfs, PathBuf::from("./rootfs"));
        assert_eq!(cli.hostname, None);
        assert!(cli.bind_mounts.is_empty());
        assert_eq!(cli.command_tokens, ["/bin/sh"]);
    }

    #[test]
    fn bind_mount_parser_requires_an_absolute_container_path() {
        assert_eq!(
            parse_bind_mount("/host/data:/data").unwrap(),
            BindMount {
                source: PathBuf::from("/host/data"),
                destination: PathBuf::from("/data"),
            }
        );
        assert!(parse_bind_mount("/host/data:data").is_err());
        assert!(parse_bind_mount("/host/data").is_err());
        assert!(parse_bind_mount(":/data").is_err());
        assert!(parse_bind_mount("/host/data:/../outside").is_err());
        assert!(parse_bind_mount("/host/data:/").is_err());
        assert!(parse_bind_mount("/host/data:/oldroot").is_err());
        assert!(parse_bind_mount("/host/data:/oldroot/nested").is_err());
        assert!(parse_bind_mount("/host/data:/oldroot-safe").is_ok());
    }

    #[test]
    fn hostname_validation_rejects_invalid_values() {
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("has space").is_err());
        assert!(validate_hostname("-starts-with-dash").is_err());
        assert!(validate_hostname(&"x".repeat(MAX_HOSTNAME_LEN + 1)).is_err());
        assert_eq!(
            validate_hostname("testbox-1.example").unwrap(),
            "testbox-1.example"
        );
    }

    #[test]
    fn namespace_configuration_includes_pid_uts_and_mount() {
        let flags = namespace_clone_flags();

        assert!(flags.contains(CloneFlags::CLONE_NEWUTS));
        assert!(flags.contains(CloneFlags::CLONE_NEWPID));
        assert!(flags.contains(CloneFlags::CLONE_NEWNS));
    }

    #[test]
    fn mount_tree_is_configured_as_recursively_private() {
        let flags = private_mount_flags();

        assert!(flags.contains(MsFlags::MS_PRIVATE));
        assert!(flags.contains(MsFlags::MS_REC));
    }
}
