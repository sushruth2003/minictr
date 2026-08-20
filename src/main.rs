use clap::{Parser, error::ErrorKind};
use nix::{
    mount::{MsFlags, mount},
    sched::{CloneFlags, clone},
    sys::{
        signal::Signal,
        wait::{WaitStatus, waitpid},
    },
    unistd::{gethostname, sethostname},
};
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

const MAX_HOSTNAME_LEN: usize = 64;

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(author, version, about)]
struct Cli {
    operation: String,
    #[arg(long, value_name = "path")]
    rootfs: PathBuf,
    #[arg(long, value_name = "hostname", value_parser = validate_hostname)]
    hostname: Option<String>,
    #[arg(num_args = 1.., value_name = "command", allow_hyphen_values = true)]
    command_tokens: Vec<String>,
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

    if let Err(error) = enter_rootfs(&args.rootfs) {
        eprintln!("failed to enter rootfs: {error}");
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

    match run_command(&args.command_tokens) {
        Ok(status) => status.code().unwrap_or(1) as isize,
        Err(error) => {
            eprintln!("failed to execute command: {error}");
            1
        }
    }
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

fn enter_rootfs(rootfs: &Path) -> io::Result<()> {
    nix::unistd::chroot(rootfs).map_err(nix_error_to_io)?;
    std::env::set_current_dir("/")
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
    let args = Cli { rootfs, ..args };

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

fn run_command(command_tokens: &[String]) -> io::Result<ExitStatus> {
    let command = &command_tokens[0];
    let mut command_builder = Command::new(command);
    command_builder.args(&command_tokens[1..]);
    let mut child_process = command_builder.spawn()?;
    eprintln!("child pid = {}", child_process.id());
    child_process.wait()
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
            "--",
            "sh",
            "-c",
            "printf hello",
        ])
        .unwrap();

        assert_eq!(cli.operation, "run");
        assert_eq!(cli.rootfs, PathBuf::from("./rootfs"));
        assert_eq!(cli.hostname.as_deref(), Some("testbox"));
        assert_eq!(cli.command_tokens, ["sh", "-c", "printf hello"]);
    }

    #[test]
    fn rootfs_is_parsed_before_the_command_separator() {
        let cli = parse_cli(["minictr", "run", "--rootfs", "./rootfs", "--", "/bin/sh"]).unwrap();

        assert_eq!(cli.rootfs, PathBuf::from("./rootfs"));
        assert_eq!(cli.hostname, None);
        assert_eq!(cli.command_tokens, ["/bin/sh"]);
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
