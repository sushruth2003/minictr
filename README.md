# minictr

`minictr` is a small, Linux-only container runtime written in Rust. It exposes
the kernel mechanisms behind containers directly so the implementation remains
useful as a systems-programming learning project.

## What it supports

- UTS, PID, and mount namespaces
- An isolated hostname and process view
- A private mount tree, rootfs pivot, and fresh `/proc`
- Repeatable host-to-container bind mounts
- Direct execution as namespace PID 1 by default
- An optional `--init` shim that reaps adopted descendants
- Optional cgroup v2 PID limits loaded from a JSON resource configuration
- Workload exit-status propagation
- Forwarding of `SIGHUP`, `SIGINT`, `SIGQUIT`, and `SIGTERM` through the host
  supervisor and optional init shim

Namespaces control what the workload can see. Cgroups control how much the
workload and all of its descendants can consume.

## Requirements

- Linux; cgroup v2 is additionally required when a resource configuration is
  supplied
- A Rust toolchain
- Sufficient privileges for namespaces, mounts, `pivot_root`, and cgroup
  management; the current version is normally run as root
- A prepared root filesystem containing the requested workload and its runtime
  dependencies

## Build and run

Build normally:

```sh
cargo build --release
```

Run a shell directly as PID 1 in the container:

```sh
sudo ./target/release/minictr run \
  --rootfs ./rootfs \
  --hostname demo \
  -- /bin/sh -c 'printf "pid=%s parent=%s hostname=%s\n" "$$" "$PPID" "$(hostname)"'
```

Use the init shim when a workload may orphan descendants:

```sh
sudo ./target/release/minictr run \
  --rootfs ./rootfs \
  --init \
  -- /bin/sh -c 'sleep 1 & exit 0'
```

The shim remains PID 1, runs the workload as PID 2, reaps adopted descendants,
forwards termination signals to the workload process group, and returns the
workload's exit status after every descendant has exited.

Without `--init`, the workload remains namespace PID 1 and therefore owns PID
1's special Linux signal semantics. Use `--init` when predictable signal
forwarding and descendant reaping are required.

`minictr` uses conventional container CLI status codes: `125` for a runtime or
container-setup failure, `126` when a command exists but cannot be executed,
and `127` when the command cannot be found. A workload's own exit status is
otherwise preserved; signal deaths are represented as `128 + signal`.

### Bind mounts

Expose a host directory at an absolute path inside the container:

```sh
sudo ./target/release/minictr run \
  --rootfs ./rootfs \
  --mount /host/data:/data \
  -- /bin/sh
```

Pass `--mount` more than once for multiple mounts. Missing destination files or
directories are created inside the rootfs. `/` and the reserved `/oldroot`
tree cannot be mount destinations.

### Resource configuration

Resource controls are opt-in. Without `--config`, `minictr` creates no cgroup
and applies no implicit PID limit.

Create `resources.json`:

```json
{
  "resources": {
    "pids": {
      "max": 16
    }
  }
}
```

Run with that policy:

```sh
sudo ./target/release/minictr run \
  --config ./resources.json \
  --rootfs ./rootfs \
  --init \
  -- /bin/sh
```

`minictr` creates a unique cgroup under `/sys/fs/cgroup/minictr`, writes the
configured value to `pids.max`, and has container PID 1 join before rootfs
setup or workload creation. Forked workloads inherit membership automatically.
The per-container cgroup is removed after the container exits.

Configuration is strict: unknown fields, malformed JSON, and a PID limit of
zero are rejected before the container process starts. CPU and memory resource
objects will be added in later M5 slices.

## How execution fits together

```text
host minictr
  ├── parse CLI and optional resource JSON
  ├── create and configure an optional cgroup
  ├── clone namespace PID 1
  │    ├── join the cgroup
  │    ├── prepare mounts, rootfs, /proc, and hostname
  │    └── exec the workload directly, or fork it under --init
  ├── forward catchable termination signals and wait for namespace PID 1
  ├── remove the per-container cgroup
  └── return the workload status
```

The implementation is split by responsibility:

- `src/main.rs`: namespace setup and top-level orchestration
- `src/process.rs`: signal forwarding, child outcomes, parent-death behavior,
  and runtime/exec status conventions
- `src/cli.rs`: command-line parsing and validation
- `src/config.rs`: strict JSON parsing and resource-policy validation
- `src/cgroup.rs`: cgroup creation, controller configuration, membership, and
  cleanup
- `src/rootfs.rs`: mount namespace, bind-mount, root-pivot, and procfs setup

## Tests

The namespace and cgroup integration tests require Linux and normally require
root:

```sh
./scripts/test --all-features
```

The suite covers CLI/config validation, command and stream preservation,
exit-status propagation, UTS/PID/mount isolation, rootfs and `/proc` isolation,
bind mounts, init topology and orphan reaping, PID-limit enforcement, no-config
compatibility, signal propagation, and cgroup cleanup.

## Roadmap

1. Complete M5 with memory and CPU cgroup v2 controls plus lifecycle hardening.
2. Add configurable shutdown timeouts and escalation policy.
3. Add structured host/PID-1 error reporting and recovery of stale cgroups
   after an uncatchable host failure such as `SIGKILL`.
