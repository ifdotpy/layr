# Automatic checks

`layr checks watch` detects pending checks when sync introduces a change version
or a head of a selected protected line. It executes them only on a machine that
the machine administrator has opted in with `layr config --local checks.runner true`. Otherwise it
only reports pending work. It scans the existing project database every
five seconds; it does not fetch or push. Use the normal sync service to receive
source states and publish attestations and logs. The first scan includes available
older versions. Intermediate protected-line heads received in one sync are included.
Collected source states are reported on stderr and skipped, never marked passing.

Commands use the existing `check.<name>` settings, just like `layr try --check`,
protected refs and the merge queue. For example, as a project maintainer:

```sh
layr config check.unit 'cargo test --locked --offline'
layr config checks.changes '["unit"]'
layr config checks.lines '{"main":["unit"]}'
```

`checks.lines` selects protected line names, each with its check names. It is
independent of the line's required merge checks. All `checks.*` project settings,
like `check.*`, require the existing `access` action. Automatic checks read commands, selection, resource limits and protection from
the verified project creator's perspective: the machine that signed the unique
`project` record. Set and push that policy on the creator's machine. Runner-local
`check.*` and `checks.*` values never change what the watcher executes. Missing or
ambiguous creator records refuse execution. Other CLI operations retain their
local policy; a cloning OS user receives signed local admin access for that copy
only. Fetch into an existing project never grants local access.

| Key | Default | Meaning |
| --- | --- | --- |
| `checks.changes` | `[]` | Checks for every immutable logical change version |
| `checks.lines` | `{}` | Protected line names mapped to check name arrays |
| `checks.privileged` | `[]` | Explicit names permitted to execute as root |
| `checks.platforms` | `{}` | Check names mapped to `linux` or `macos`; unset names run on either |
| `checks.timeout` | `900` | Wall timeout and per-process CPU seconds; 1–86400 |
| `checks.memory` | `4294967296` | Linux per-process address-space limit; maximum file size on either platform |
| `checks.processes` | `256` (Linux), `4096` (Mac) | Hard process limit for the sandbox user |
| `checks.log-bytes` | `1048576` | Retained combined output bytes; at most 16 MiB |
| `checks.runner` | `false` | Machine-local opt-in to execute checks; root on Linux, metadata owner on macOS, using `config --local` |
| `checks.privileged-runner` | `false` | Additional machine-local opt-in to execute explicitly privileged checks |

Each project has one watcher, protected by a local `flock`. Jobs run sequentially.
An active signed executor check of the exact state and exact argv prevents
another run, including after failure or restart. Renaming a check with identical
argv reuses its evidence. A command change reruns it; an attestation revocation
permits a retry. The privileged mode must also match. Manual `try` results lack
the executor marker and cannot suppress executor work. Returned executor evidence
from other trusted machines does count. The small executor reports pending work;
an opted-in runner executes all checks. A Linux laptop VM can also be a runner.
The lock is local, not a distributed lease: run one active runner per project and platform.

Each result is the existing `check` attestation with state, command, argv, exit,
`privileged`, and `log_ref`. The referenced derived state contains `check.log`.
The ref retains logs through GC and syncs with ordinary project refs/states. Read
logs with `layr show <log_ref>:check.log`; inspect evidence with
`layr attestations <state> --kind check --json`. The site uses these attestations.
Output above the cap is discarded while the command keeps running; `truncated`
records that fact. Timeout records exit 124. Signals record 128 plus the signal.
A crash before the signed result commits can rerun that job: execution is at least
once, and completed results are deduplicated. Check commands must tolerate retries.

## Service and sandbox

Install Linux `bubblewrap` and run the watcher as a dedicated non-root account
with project read/try access through the existing root Layr daemon. Root invocation
of the watcher is refused. The daemon prepares the same private writable state copy
as `try`, then drops to the service account for normal checks. Bubblewrap gives
the command private user, PID, mount, IPC, UTS and network namespaces; only the copy
at `/work`, read-only system tools/libraries, and private `/proc`, `/dev`, `/tmp`
and HOME are visible. Host homes, store metadata, tokens and sockets are absent.
No caller environment reaches the command. Missing bubblewrap is a fatal error;
namespace setup failure is visible as a failed check, with its stderr retained.
There is no unsandboxed fallback. Child processes get hard memory, CPU, process,
file descriptor and core dump limits. The per-file size limit equals `checks.memory`.
Descendants are killed on completion/timeout.

On a runner, root first enables `layr config --local checks.runner true`.
Leave this off on an executor that only reports work. A service entry after sync
uses the same command on either machine:

```sh
runuser -u layr-checks -- env LAYR_SOCKET=/run/layr.sock \
  layr --project layr checks watch --once --json
```

For continuous watching omit `--once`. A systemd service should additionally set
`MemoryMax` and `TasksMax` for the daemon's check requests or a dedicated executor
daemon, because address-space/CPU limits apply per process. Keep credential-free
toolchains and offline dependency caches under `/usr`, readable in the sandbox.
Checks that require network access must use prepared inputs or the explicit
privileged runner.

## Privileged filesystem suites

A maintainer must list the check in `checks.privileged`, and root on that machine
must separately opt in with `layr config --local checks.privileged-runner true`,
in addition to `checks.runner true`. The two local settings default to false.
They use a root-owned mode-600 file in project metadata; it is never
a signed project setting and never syncs. It can be disabled or unset with the
same `--local` option. Without privileged opt-in, those jobs remain pending.
Privileged jobs additionally require a state that was a main head on the creator's
machine, or an active exact-state approval from a current creator-side maintainer
or a pinned maintainer person. A runner-local main movement cannot bypass this gate.
The runner invokes `checks watch --once --json` to run all selected checks.
The optional `--privileged-only` filter skips normal jobs and requires both opts.
Run this caller
as a non-root service user through the root daemon as above.

Privileged checks execute repository code as root with host access, including loop
devices and mounts. They are not sandboxed. They use a fresh private
`/var/tmp/layr-check-*` copy and HOME, removed after execution, with the same timeout
and output cap; attestations include `privileged: true`. A private mount namespace
releases test mounts on timeout; it does not prevent root access to host files.
Root can evade process
limits or access the runner's machine key. Use a dedicated, replaceable machine
holding only its own Layr identity and the minimum project role/transport credential
needed to receive source and publish its own results, with no other secrets.
Do not enable this on a personal or shared executor.

The deployment supervisor owns machine start/stop and fetch→watch→push. The CLI
contains no EC2 or SSH orchestration. `--once --json` writes one JSON line:

```json
{"work_done":true,"count":1,"pending":0,"pending_privileged":0,"checks":[{"name":"unit","state":"...","argv":["sh","-c","..."],"exit":0,"log_ref":"derived/check-v1/..."}]}
```

The complete result has additional fields, including `privileged`, `output_state`
and `truncated`. `work_done` is true whenever a check executed, even if it failed.
An idle scan returns false, count 0 and an empty array. Exit 0 means scan complete
with no newly failed check (including idle); 1 means at least one newly executed
check failed; 128 means configuration, permission or operational failure. Errors
go to stderr and do not produce a success JSON line. A supervisor must inspect
both exit 0 and exit 1 JSON when updating its last-work timestamp.
`pending` counts all available state/argv jobs with no active signed executor
result; `pending_privileged` is its privileged subset. An executor with both local
flags off only reports these counts and never launches checks. Its supervisor can
wake the runner when `pending` is nonzero. Returned results, including failures, clear the pending count
after sync. This count is advisory; it does not authorize execution.

## Layr's Linux workflow equivalents

On the creator's machine, retain the required main protection and run
`bash tools/checks/configure.sh`, then push. On Linux runner copies, provision the
local service account's reader role (`layr access grant layr-checks reader`). The
creator's platform map selects Linux or APFS checks without runner-local overrides.
The configuration defines:

| Check | Mode | GitHub workflow equivalent |
| --- | --- | --- |
| `unit` | Sandbox | Locked offline workspace unit tests |
| `integration` | Privileged | `ci.yml`: build, unit tests, full btrfs integration suite |
| `overlayfs-ext4` | Privileged | `overlayfs.yml`: ext4, unit/local/cache/selected integration tests and casefold rejection |
| `overlayfs-xfs` | Privileged | Same OverlayFS tests on xfs |
| `apfs` | macOS sandbox | `ci.yml` APFS suite, excluding the separate root service operator check |

The scripts keep the filesystem matrix explicit and unmount loop files on exit.
Provision btrfs-progs, xfsprogs, e2fsprogs, zstd, git, python3, bubblewrap,
docker-registry, skopeo, apache2-utils and helm, as in the GitHub Linux jobs.
Install MinIO built from `RELEASE.2025-10-15T17-29-55Z` at
`/usr/local/lib/layr-checks/minio`. A fresh Rust HOME uses a credential-free seed
at `/usr/local/share/layr-checks/{cargo,rustup}`. Prepare the pinned lockfile's
dependencies with `cargo fetch --locked` in a trusted checkout before making
the seed read-only. Copy only registry/cache and the Rust tools into the seed;
exclude Cargo credentials, Git credentials and SSH configuration. Each check
copies this seed into its private HOME, with `RUSTUP_HOME` using the read-only
toolchain, and uses offline Cargo. Maintain the seed when Cargo.lock changes.
If caches live on a data volume, bind-mount them under the seed directory instead
of symlinking to host paths outside `/usr`, which the sandbox cannot see.
The unit script sets one Cargo build job for small executors. Each machine may
override `checks.memory` after configuring it; this is a hard address-space
ceiling, and does not reserve that amount of host RAM.
On macOS the creator's platform map selects `apfs`, the nonprivileged APFS workflow
equivalent. Its watcher uses sandbox-exec and requires a current maintainer's
exact-state approval for change versions; main heads can run without it. Its
machine-local opt-in belongs to the user who owns the project's metadata.
Privileged execution remains unavailable on macOS. The separate root-only service
test is an operator check. See [laptop-runner.md](laptop-runner.md) for machine
provisioning, volume setup, services, verification and residual host risk.
