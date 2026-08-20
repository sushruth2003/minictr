# minictr

`minictr` is a small, Linux-only container runtime built in Rust to explore the
kernel primitives behind containers without hiding them behind a production
runtime.

The overall goal is to build a mini container runtime that covers:

- Linux namespaces
- Process creation and lifecycle management
- Root filesystem isolation
- Cgroups and resource controls
- Signal handling and forwarding
- Deterministic process and resource cleanup

This is mostly a toy project to see how i can build these things/learn some rust. 

## Current capabilities

- Run a command with its arguments and propagate its exit status.
- Create combined UTS, PID, and mount namespaces.
- Make the container mount tree recursively private so mount events cannot
  propagate back to the host.
- Change the runtime root to `--rootfs` before launching the user command.
- Start the user command at `/` inside that rootfs instead of inheriting the
  host working directory.
- Mount a fresh procfs whose process entries are scoped to the container's PID
  namespace.
- Assign an isolated hostname without modifying the host hostname.
- Start a runtime-owned init process as PID 1 in the new PID namespace.
- Start the user command as the init process's direct child, normally PID 2.
- Wait for and reap the direct user process.
- Demonstrate host and namespace PID mapping through `/proc/<pid>/status` and
  `NSpid`.
- Verify that the direct user process is gone after the runtime exits.

Reaping multiple descendants and orphaned process trees is intentionally left
for a follow-up milestone. The current lifecycle model supports one direct user
process.

## Usage

Building can happen as a normal user, but creating namespaces and changing the
process root require suitable Linux privileges (normally root in the current
version).

```sh
cargo build --release
sudo ./target/release/minictr run --rootfs ./rootfs --hostname demo -- /bin/sh -c \
  'printf "pid=%s parent=%s hostname=%s\n" "$$" "$PPID" "$(hostname)"'
```

## Tests

The integration tests exercise Linux namespace behavior and require root:

```sh
./scripts/test --all-features
```

The suite covers command execution, argument and stream preservation, exit
status propagation, UTS isolation, PID 1/PID 2 ownership, `NSpid` mapping,
single-child reaping, and cleanup.

## Roadmap

1. Complete runtime-owned init behavior for multiple descendants and orphan
   reaping.
2. Replace the initial `chroot` boundary with bind-mounted `pivot_root`
   lifecycle management.
3. Add cgroup-based resource controls.
4. Add signal forwarding and shutdown semantics.
5. Harden cleanup across normal exits and failures.
