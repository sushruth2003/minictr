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
- Bind-mount `--rootfs` onto itself, pivot it into `/`, then detach and remove
  `/oldroot` so the previous host root is not pathname-accessible.
- Start the user command at `/` inside that rootfs instead of inheriting the
  host working directory.
- Mount a fresh procfs whose process entries are scoped to the container's PID
  namespace.
- Bind-mount host files or directories into the container with repeatable
  `--mount host_path:container_path` options.
- Assign an isolated hostname without modifying the host hostname.
- Replace the namespace setup process with the user command so it runs as PID 1.
- Wait for the container's PID 1 process and propagate its exit status.
- Demonstrate host and namespace PID mapping through `/proc/<pid>/status` and
  `NSpid`.
- Verify that the container's PID 1 process is gone after the runtime exits.

The user command currently owns Linux PID 1 responsibilities, including
handling signals and reaping any descendants it creates. An optional init shim
is intentionally left for a follow-up milestone.

## Usage

Building can happen as a normal user, but creating namespaces and changing the
process root require suitable Linux privileges (normally root in the current
version).

```sh
cargo build --release
sudo ./target/release/minictr run --rootfs ./rootfs --hostname demo -- /bin/sh -c \
  'printf "pid=%s parent=%s hostname=%s\n" "$$" "$PPID" "$(hostname)"'
```

Bind-mount a host directory at an absolute path inside the container:

```sh
sudo ./target/release/minictr run --rootfs ./rootfs \
  --mount /host/data:/data -- /bin/sh
```

Pass `--mount` more than once to add multiple bind mounts. Missing destination
directories or files are created inside the rootfs before mounting. `/` and
the runtime-reserved `/oldroot` tree cannot be used as mount destinations.

## Tests

The integration tests exercise Linux namespace behavior and require root:

```sh
./scripts/test --all-features
```

The suite covers command execution, argument and stream preservation, exit
status propagation, UTS isolation, user-command PID 1 ownership, `NSpid`
mapping, root-pivot isolation, rootfs `/tmp` isolation, bind mounts, and cleanup.
It also includes a combined M3 acceptance test for PID, hostname, rootfs,
procfs, temporary-file, and bind-mount behavior.

## Roadmap

1. Add an optional init shim for workloads that need descendant and orphan
   reaping.
2. Add cgroup-based resource controls.
3. Add signal forwarding and shutdown semantics.
4. Harden cleanup across normal exits and failures.
