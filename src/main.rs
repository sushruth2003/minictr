mod cgroup;
mod cli;
mod config;
mod rootfs;

use crate::{
    cgroup::Cgroup,
    cli::{Cli, parse_cli},
    config::RuntimeConfig,
    rootfs::{
        make_mount_tree_private, make_rootfs_mount_point, mount_bind, mount_procfs,
        pivot_into_rootfs,
    },
};
use nix::{
    errno::Errno,
    libc,
    sched::{CloneFlags, clone},
    sys::{
        signal::Signal,
        wait::{WaitStatus, waitpid},
    },
    unistd::{ForkResult, Pid, fork, gethostname, sethostname},
};
use std::{fs, io, os::unix::process::CommandExt, process::Command};

fn init_container(args: &Cli, cgroup: Option<&Cgroup>) -> isize {
    if let Some(cgroup) = cgroup
        && let Err(error) = cgroup.join_current()
    {
        eprintln!("failed to join cgroup: {error}");
        return 1;
    }

    match run_container(args) {
        Ok(code) => code as isize,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[allow(unsafe_code)]
fn run_container(args: &Cli) -> Result<i32, String> {
    if let Err(error) = make_mount_tree_private() {
        return Err(format!("failed to make mount tree private: {error}"));
    }

    if let Err(error) = make_rootfs_mount_point(&args.rootfs) {
        return Err(format!("failed to make rootfs a mount point: {error}"));
    }

    for bind_mount in &args.bind_mounts {
        if let Err(error) = mount_bind(&args.rootfs, bind_mount) {
            return Err(format!(
                "failed to bind mount {} at {}: {error}",
                bind_mount.source.display(),
                bind_mount.destination.display()
            ));
        }
    }

    if let Err(error) = pivot_into_rootfs(&args.rootfs) {
        return Err(format!("failed to pivot into rootfs: {error}"));
    }

    if let Err(error) = mount_procfs() {
        return Err(format!("failed to mount procfs: {error}"));
    }

    if let Some(hostname) = &args.hostname {
        if let Err(error) = sethostname(hostname).map_err(nix_error_to_io) {
            return Err(format!("failed to set hostname: {error}"));
        }

        match gethostname() {
            Ok(name) => eprintln!("runtime process hostname = {}", name.to_string_lossy()),
            Err(error) => {
                return Err(format!("failed to read hostname: {error}"));
            }
        }
    }

    if args.init {
        let workload_pid = match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                eprintln!("inside child: {}", std::process::id());
                let error = exec_command(&args.command_tokens);
                eprintln!("failed to execute command: {error}");
                // SAFETY: exec failed in the post-fork child, so terminate
                // immediately without running cleanup inherited from PID 1.
                unsafe { libc::_exit(127) };
            }
            Ok(ForkResult::Parent { child }) => child,
            Err(error) => {
                return Err(format!("failed to fork workload: {error}"));
            }
        };
        let mut workload_status = None;

        Ok(loop {
            match waitpid(Pid::from_raw(-1), None) {
                Ok(WaitStatus::Exited(pid, code)) if pid == workload_pid => {
                    workload_status = Some(code);
                }
                Ok(WaitStatus::Signaled(pid, signal, _)) if pid == workload_pid => {
                    workload_status = Some(128 + signal as i32);
                }
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(Errno::ECHILD) => break workload_status.unwrap_or(1),
                Err(error) => {
                    eprintln!("failed to wait for workload: {error}");
                    break 1;
                }
            }
        })
    } else {
        eprintln!("inside child: {}", std::process::id());
        Err(format!(
            "failed to execute command: {}",
            exec_command(&args.command_tokens)
        ))
    }
}

fn namespace_clone_flags() -> CloneFlags {
    CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS
}

#[allow(unsafe_code)]
fn initialize_namespaced_runtime(args: Cli) -> io::Result<i32> {
    let runtime_config = RuntimeConfig::load(args.config.as_deref())?;
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
    let cgroup = Cgroup::create(&runtime_config.resources)?;

    // SAFETY: the child stack remains alive until the cloned runtime exits,
    // and the callback only captures data owned by this function.
    let runtime_pid = unsafe {
        clone(
            Box::new(|| init_container(&args, cgroup.as_ref())),
            &mut stack,
            flags,
            Some(Signal::SIGCHLD as i32),
        )
    };
    let runtime_result = runtime_pid
        .map_err(io::Error::from)
        .and_then(wait_for_runtime);

    let cleanup_result = match cgroup {
        Some(cgroup) => cgroup.remove(),
        None => Ok(()),
    };
    resolve_runtime_and_cleanup(runtime_result, cleanup_result)
}

fn wait_for_runtime(runtime_pid: Pid) -> io::Result<i32> {
    loop {
        match waitpid(runtime_pid, None) {
            Ok(WaitStatus::Exited(_, code)) => return Ok(code),
            Ok(WaitStatus::Signaled(_, signal, _)) => return Ok(128 + signal as i32),
            Ok(other) => {
                return Err(io::Error::other(format!(
                    "unexpected wait status: {other:?}"
                )));
            }
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

fn resolve_runtime_and_cleanup(
    runtime_result: io::Result<i32>,
    cleanup_result: io::Result<()>,
) -> io::Result<i32> {
    match (runtime_result, cleanup_result) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(code), Err(error)) => {
            eprintln!("failed to remove cgroup: {error}");
            Ok(code)
        }
        (Err(error), Ok(())) => Err(error),
        (Err(runtime_error), Err(cleanup_error)) => Err(io::Error::other(format!(
            "{runtime_error}; also failed to remove cgroup: {cleanup_error}"
        ))),
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
mod tests {
    use super::*;

    #[test]
    fn namespace_configuration_includes_pid_uts_and_mount() {
        let flags = namespace_clone_flags();

        assert!(flags.contains(CloneFlags::CLONE_NEWUTS));
        assert!(flags.contains(CloneFlags::CLONE_NEWPID));
        assert!(flags.contains(CloneFlags::CLONE_NEWNS));
    }

    #[test]
    fn cleanup_failure_does_not_replace_workload_status() {
        let result =
            resolve_runtime_and_cleanup(Ok(37), Err(io::Error::other("simulated cleanup failure")));

        assert_eq!(result.ok(), Some(37));
    }
}
