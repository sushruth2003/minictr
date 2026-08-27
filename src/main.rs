mod cgroup;
mod cli;
mod config;
mod process;
mod rootfs;

use crate::{
    cgroup::Cgroup,
    cli::{Cli, parse_cli},
    config::RuntimeConfig,
    process::{
        ProcessOutcome, RUNTIME_ERROR_EXIT_CODE, SignalForwarder, exec_failure_exit_code,
        process_outcome, set_parent_death_signal, wait_for_process,
    },
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
        signal::{Signal, kill},
        wait::waitpid,
    },
    unistd::{ForkResult, Pid, fork, gethostname, sethostname, setpgid},
};
use std::{fs, io, os::unix::process::CommandExt, process::Command};

fn init_container(args: &Cli, cgroup: Option<&Cgroup>, signals: &SignalForwarder) -> isize {
    match run_container(args, cgroup, signals) {
        Ok(code) => code as isize,
        Err(error) => {
            eprintln!("{error}");
            RUNTIME_ERROR_EXIT_CODE as isize
        }
    }
}

#[allow(unsafe_code)]
fn run_container(
    args: &Cli,
    cgroup: Option<&Cgroup>,
    signals: &SignalForwarder,
) -> Result<i32, String> {
    set_parent_death_signal()
        .map_err(|error| format!("failed to configure parent-death signal: {error}"))?;

    if let Some(cgroup) = cgroup {
        cgroup
            .join_current()
            .map_err(|error| format!("failed to join cgroup: {error}"))?;
    }

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
                if let Err(error) = setpgid(Pid::from_raw(0), Pid::from_raw(0)) {
                    eprintln!("failed to create workload process group: {error}");
                    // SAFETY: this is the post-fork workload child.
                    unsafe { libc::_exit(RUNTIME_ERROR_EXIT_CODE) };
                }
                if let Err(error) = signals.restore() {
                    eprintln!("failed to restore workload signal state: {error}");
                    // SAFETY: this is the post-fork workload child.
                    unsafe { libc::_exit(RUNTIME_ERROR_EXIT_CODE) };
                }
                eprintln!("inside child: {}", std::process::id());
                let error = exec_command(&args.command_tokens);
                eprintln!("failed to execute command: {error}");
                // SAFETY: exec failed in the post-fork child, so terminate
                // immediately without running cleanup inherited from PID 1.
                unsafe { libc::_exit(exec_failure_exit_code(&error)) };
            }
            Ok(ForkResult::Parent { child }) => child,
            Err(error) => {
                return Err(format!("failed to fork workload: {error}"));
            }
        };
        match setpgid(workload_pid, workload_pid) {
            Ok(()) | Err(Errno::EACCES) | Err(Errno::ESRCH) => {}
            Err(error) => {
                let _ = kill(workload_pid, Signal::SIGKILL);
                let _ = waitpid(workload_pid, None);
                return Err(format!("failed to create workload process group: {error}"));
            }
        }
        if let Err(error) = signals.activate_process_group(workload_pid) {
            let _ = kill(workload_pid, Signal::SIGKILL);
            let _ = waitpid(workload_pid, None);
            return Err(format!("failed to activate signal forwarding: {error}"));
        }
        let mut workload_status = None;

        Ok(loop {
            match waitpid(Pid::from_raw(-1), None) {
                Ok(status) => {
                    if let Some(outcome) = process_outcome(status, workload_pid)
                        .map_err(|error| format!("failed to interpret workload status: {error}"))?
                    {
                        workload_status = Some(outcome.exit_code());
                    }
                }
                Err(Errno::EINTR) => continue,
                Err(Errno::ECHILD) => {
                    signals.clear_target();
                    break workload_status.unwrap_or(RUNTIME_ERROR_EXIT_CODE);
                }
                Err(error) => {
                    eprintln!("failed to wait for workload: {error}");
                    break RUNTIME_ERROR_EXIT_CODE;
                }
            }
        })
    } else {
        signals
            .restore()
            .map_err(|error| format!("failed to restore workload signal state: {error}"))?;
        eprintln!("inside child: {}", std::process::id());
        let error = exec_command(&args.command_tokens);
        eprintln!("failed to execute command: {error}");
        Ok(exec_failure_exit_code(&error))
    }
}

fn namespace_clone_flags() -> CloneFlags {
    CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS
}

#[allow(unsafe_code)]
fn initialize_namespaced_runtime(args: Cli) -> io::Result<ProcessOutcome> {
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
    let signals = SignalForwarder::install()?;
    let cgroup = Cgroup::create(&runtime_config.resources)?;

    // SAFETY: the child stack remains alive until the cloned runtime exits,
    // and the callback only captures data owned by this function.
    let runtime_pid = unsafe {
        clone(
            Box::new(|| init_container(&args, cgroup.as_ref(), &signals)),
            &mut stack,
            flags,
            Some(Signal::SIGCHLD as i32),
        )
    };
    let runtime_result = match runtime_pid {
        Ok(runtime_pid) => match signals.activate_process(runtime_pid) {
            Ok(()) => wait_for_process(runtime_pid),
            Err(error) => {
                let _ = kill(runtime_pid, Signal::SIGKILL);
                let _ = wait_for_process(runtime_pid);
                Err(error)
            }
        },
        Err(error) => Err(io::Error::from(error)),
    };

    let cleanup_result = match cgroup {
        Some(cgroup) => cgroup.remove(),
        None => Ok(()),
    };
    if let Err(error) = signals.restore() {
        eprintln!("failed to restore host signal state: {error}");
    }
    resolve_runtime_and_cleanup(runtime_result, cleanup_result)
}

fn resolve_runtime_and_cleanup(
    runtime_result: io::Result<ProcessOutcome>,
    cleanup_result: io::Result<()>,
) -> io::Result<ProcessOutcome> {
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
        Ok(outcome) => {
            std::process::exit(outcome.exit_code());
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(RUNTIME_ERROR_EXIT_CODE);
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
        let result = resolve_runtime_and_cleanup(
            Ok(ProcessOutcome::Exited(37)),
            Err(io::Error::other("simulated cleanup failure")),
        );

        assert_eq!(result.ok(), Some(ProcessOutcome::Exited(37)));
    }
}
