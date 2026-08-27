use nix::{
    errno::Errno,
    libc,
    sys::{
        prctl,
        signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction},
        wait::{WaitStatus, waitpid},
    },
    unistd::Pid,
};
use std::{
    io,
    sync::atomic::{AtomicI32, Ordering},
};

pub(crate) const RUNTIME_ERROR_EXIT_CODE: i32 = 125;
pub(crate) const CANNOT_EXEC_EXIT_CODE: i32 = 126;
pub(crate) const COMMAND_NOT_FOUND_EXIT_CODE: i32 = 127;

const FORWARDED_SIGNALS: [Signal; 4] = [
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGTERM,
];

static FORWARD_TARGET: AtomicI32 = AtomicI32::new(0);

#[allow(unsafe_code)]
extern "C" fn forward_signal(signal: libc::c_int) {
    let target = FORWARD_TARGET.load(Ordering::Relaxed);
    if target != 0 {
        // SAFETY: kill(2) is async-signal-safe, and the handler intentionally
        // ignores errors because the target process or process group may have
        // exited concurrently.
        unsafe {
            libc::kill(target, signal);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessOutcome {
    Exited(i32),
    Signaled(Signal),
}

impl ProcessOutcome {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled(signal) => 128 + signal as i32,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SignalForwarder {
    mask: SigSet,
    original_mask: SigSet,
    original_actions: Vec<(Signal, SigAction)>,
}

impl SignalForwarder {
    #[allow(unsafe_code)]
    pub(crate) fn install() -> io::Result<Self> {
        let mut mask = SigSet::empty();
        for signal in FORWARDED_SIGNALS {
            mask.add(signal);
        }

        let original_mask = SigSet::thread_get_mask().map_err(io::Error::from)?;
        mask.thread_block().map_err(io::Error::from)?;

        let action = SigAction::new(
            SigHandler::Handler(forward_signal),
            SaFlags::empty(),
            SigSet::empty(),
        );
        let mut original_actions = Vec::with_capacity(FORWARDED_SIGNALS.len());
        for signal in FORWARDED_SIGNALS {
            // SAFETY: forward_signal has the required C signal-handler ABI.
            match unsafe { sigaction(signal, &action) } {
                Ok(original) => original_actions.push((signal, original)),
                Err(error) => {
                    restore_actions(&original_actions);
                    let _ = original_mask.thread_set_mask();
                    return Err(io::Error::from(error));
                }
            }
        }

        Ok(Self {
            mask,
            original_mask,
            original_actions,
        })
    }

    pub(crate) fn activate_process(&self, target: Pid) -> io::Result<()> {
        self.activate_target(target.as_raw())
    }

    pub(crate) fn activate_process_group(&self, process_group: Pid) -> io::Result<()> {
        self.activate_target(-process_group.as_raw())
    }

    fn activate_target(&self, target: libc::pid_t) -> io::Result<()> {
        FORWARD_TARGET.store(target, Ordering::Relaxed);
        if let Err(error) = self.mask.thread_unblock() {
            FORWARD_TARGET.store(0, Ordering::Relaxed);
            return Err(io::Error::from(error));
        }
        Ok(())
    }

    pub(crate) fn clear_target(&self) {
        FORWARD_TARGET.store(0, Ordering::Relaxed);
    }

    pub(crate) fn restore(&self) -> io::Result<()> {
        self.mask.thread_block().map_err(io::Error::from)?;
        self.clear_target();
        restore_actions(&self.original_actions);
        self.original_mask
            .thread_set_mask()
            .map_err(io::Error::from)
    }
}

pub(crate) fn set_parent_death_signal() -> io::Result<()> {
    prctl::set_pdeathsig(Signal::SIGKILL).map_err(io::Error::from)
}

pub(crate) fn wait_for_process(pid: Pid) -> io::Result<ProcessOutcome> {
    loop {
        match waitpid(pid, None) {
            Ok(status) => match process_outcome(status, pid)? {
                Some(outcome) => return Ok(outcome),
                None => continue,
            },
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

pub(crate) fn process_outcome(
    status: WaitStatus,
    expected_pid: Pid,
) -> io::Result<Option<ProcessOutcome>> {
    match status {
        WaitStatus::Exited(pid, code) if pid == expected_pid => {
            Ok(Some(ProcessOutcome::Exited(code)))
        }
        WaitStatus::Signaled(pid, signal, _) if pid == expected_pid => {
            Ok(Some(ProcessOutcome::Signaled(signal)))
        }
        WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _) => Ok(None),
        other => Err(io::Error::other(format!(
            "unexpected wait status: {other:?}"
        ))),
    }
}

pub(crate) fn exec_failure_exit_code(error: &io::Error) -> i32 {
    if error.raw_os_error() == Some(libc::ENOENT) {
        COMMAND_NOT_FOUND_EXIT_CODE
    } else {
        CANNOT_EXEC_EXIT_CODE
    }
}

#[allow(unsafe_code)]
fn restore_actions(actions: &[(Signal, SigAction)]) {
    for (signal, action) in actions.iter().rev() {
        // SAFETY: each action was returned by sigaction for this signal.
        let _ = unsafe { sigaction(*signal, action) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_outcomes_have_shell_compatible_exit_codes() {
        assert_eq!(ProcessOutcome::Exited(37).exit_code(), 37);
        assert_eq!(ProcessOutcome::Signaled(Signal::SIGTERM).exit_code(), 143);
    }

    #[test]
    fn missing_command_and_other_exec_failures_are_distinct() {
        let missing = io::Error::from_raw_os_error(libc::ENOENT);
        let denied = io::Error::from_raw_os_error(libc::EACCES);

        assert_eq!(
            exec_failure_exit_code(&missing),
            COMMAND_NOT_FOUND_EXIT_CODE
        );
        assert_eq!(exec_failure_exit_code(&denied), CANNOT_EXEC_EXIT_CODE);
    }
}
