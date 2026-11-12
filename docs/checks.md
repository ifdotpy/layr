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
| `checks.changes` | `[]` | Checks for the newest version of each open change: not yet in a protected line, its line not deleted. Newer states run first |
| `checks.lines` | `{}` | Protected line names mapped to check name arrays |
| `checks.privileged` | `[]` | Explicit names permitted to execute as root |
| `checks.platforms` | `{}` | Check names mapped to a platform or a list of platforms: `linux`, `macos` (any architecture), or `linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`. A check must pass once on each entry; an unset name runs once on any platform. A merge into a protected line needs a passing run on each entry: this machine runs its own platform, and a queue entry waits (stays pending) for a runner of another one; a failed run there fails the entry with the reason |
| `checks.after` | `{}` | Check names mapped to the checks they need, as `{"deploy": ["fmt", "clippy", "test@linux-aarch64"]}`. A check runs only after each need passed on the same state (on that platform when the entry names one). A pass runs the needed checks first. A failed need skips the check and a need with no result makes it wait, both with no attestation |
| `checks.timeout` | `900` | Wall timeout and per-process CPU seconds; 1–86400 |
| `checks.memory` | `4294967296` | Linux per-process limit of writable private memory (`RLIMIT_DATA`, not the address space: V8, Go and the JVM reserve address space they never write); maximum file size on either platform |
| `checks.processes` | `4096` | Hard process limit for the sandbox user (`RLIMIT_NPROC`; on Linux it counts threads, and a browser test runs hundreds) |
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
`privileged`, `platform` (`<os>-<arch>` of the machine that ran it), and `log_ref`. A result
counts for a platform entry that its platform meets, from any trusted machine: a check that a
laptop ran on `macos-aarch64` is not run again on another `macos-aarch64` machine, and a
`linux-x86_64` entry still needs its own run.

`layr checks pending [--state <rev>] [--platform <os>-<arch>] [--json]` lists the check jobs that
no active executor result covers for the platform (default: this machine's). `--state` limits
the list to one state and adds the change checks (`checks.changes`) for it. A CI job resolves its
commit with `layr rev-parse <commit>`, fetches the project records, and runs only these checks.

In JSON output, `change` is true when the check applies to the newest version of an open change.
It is false for other checks. A check can apply to both an open change and a protected line.

With `--passing`, the list also includes checks whose active results failed. Use this option for
merge and deployment gates. The default keeps failed results out of the executor's pending list,
so a watcher does not repeatedly run the same failed check.
`--required-checks <name,...>` also plans those checks for the selected state, even when they are
absent from `checks.changes`. It requires `--state` and `--passing`. A missing check definition is
an error. This prevents an incomplete check selection from appearing to satisfy a merge gate.
JSON output also includes `required_definitions`, with the creator's command and platform setting
for each required check. A gate must compare these to the settings it uses to merge.

A service that checks a queue entry before merging can bind the merge to that decision with
`layr queue run --checked-entry <id> --checked-head <state> --checked-state <state>`. It processes
only that entry and refuses a changed target head, entry state or non-pending entry. It never runs a missing check.
The entry stays pending when a check is missing. This mode also waits when queue probes are enabled,
because those probes execute commands during the merge. It requires an untitled fast-forward entry:
a squash or other merge creates a different state that the service has not checked. Pass all three
options together.

`layr checks run --state <rev> [--privileged-only] [--json]` is one executor pass over the jobs of
that one state: the same jobs as `checks pending --state`, run with the same sandbox, approval and
lock rules as `watch`, attested in the same way. Its JSON line and exit code are those of
`watch --once`. A CI job for a Git commit fetches it (`layr git fetch <remote> <branch>`), runs
`layr checks run --state <commit>`, and pushes the results.

`layr checks run --state <rev> --planned-only` runs only jobs that a watcher already plans for that
state. It does not add change checks to an unrelated state. It can restore the selected state from the
remote. It cannot be combined with `--required-checks`.

A check starts warm. Its copy of the state gets the cache of each cache overlay by the rules of
`layr line new <state>` ([cache overlays](cache-overlays.md)): the newest cache saved with the state
or its nearest ancestor that has one, of this platform and with a fitting key, from the caller's
own caches or those of a protected line. The check can write in the copy, cache folders included.
Nothing goes back: the copy and its caches are gone after the check, and the copy writes no
overlay line. The key uses the environment of the caller of `checks`, not the empty environment of
the sandbox, so pin key variables in the project (`[workspace.metadata.layr-retest] env` for cargo).
The copy's caches are relocated to the place where the check sees them (`/work` on Linux, the
private copy on macOS), so a unit with a compiled-in path of another line builds again.

A passing check of a head of a protected line in `checks.lines` gives its caches back. The executor
saves the cache folders of the copy as cache states saved with that head, on the line
`<line>.checks-<os>-<arch>` (for example `main.checks-linux-aarch64`). The first such check makes
the line at its head, and its head stays there. Each line keeps its newest cache (`history=last`),
and the next push deletes the older cache states on the remote. Another machine of the platform then
starts warm from that head (`layr line new X <state>`, and `layr line new X main` when main's own
cache is of an older head): one build per head warms the next check and every machine. A plain
`layr fetch <remote>` on that machine takes the caches of save-back lines ([remotes](remotes.md)). A protected
head is reviewed code built in the sandbox, so these caches count as caches of a protected line for
other users. A pending fast-forward entry of the queue into such a line counts as its next head: a server
queue (Layr Cloud) merges an entry only after its checks passed, so no check runs on the head it becomes, and the
entry's passing check gives the caches back instead. A titled entry merges as a new squash state and gives nothing
back. A check of any other state, such as a pull request, gives nothing back. A privileged
check gives nothing back either: its copy is a plain folder of root. A runner that pushes after its
checks publishes the caches: `layr fetch <remote>`, `layr checks run --state <head>` (or
`checks watch --once`), `layr push <remote> --caches`.

A build outside `checks run`, such as a CI job, gives its caches back the same way with
`layr save --overlays --save-back <line> [--json]` in a line made from the head of `<line>`: the cache
folders of the current line go to `<line>.checks-<os>-<arch>` with the line's head as their source
state, and no source state is made (a cache writer machine may run it, docs/ci-trust.md). Then
`layr push <remote> --caches` publishes them.
Owner decision 2026-11-11: a cache is an ordinary overlay, so these cache flags
(`--caches`, `--caches-of`) go away; a fetch and a push name lines and overlays
([overlays](overlays.md) section 1). The commands above are what is built.

The referenced derived state contains `check.log`.
The ref retains logs through GC and syncs with ordinary project refs/states. Read
logs with `layr show <log_ref>:check.log`; inspect evidence with
`layr attestations <state> --kind check --json`. The site uses these attestations.
Output above the cap is discarded while the command keeps running; `truncated`
records that fact. Timeout records exit 124. Signals record 128 plus the signal.
A crash before the signed result commits can rerun that job: execution is at least
once, and completed results are deduplicated. Check commands must tolerate retries.

## Cloud runners

A Mac can join a Layr Cloud gateway as a check runner of chosen projects (platform
macos-aarch64). A gateway administrator makes a one-time join code on the site (valid 1 hour).
On the Mac, as the user who runs the checks:

```sh
layr checks join https://cloud.layr.ws --code <code> --project <a,b>
```

`--project` is required: the person at the Mac chooses the projects. `join` sends the list with
the request and refuses a gateway reply that names any other project; the runner refuses work of
a project that is not in its list. A change of projects needs a new join (remove
`<store>/runner` first); a gateway message never changes it.

`join` registers the machine key of the store with the gateway, keeps the runner token in
`<store>/runner/token` (0600) and the settings in `<store>/runner/config.json`, and installs the
user daemon LaunchAgent (`ws.layr.daemon`) when it is missing. No sudo is used.

The daemon (`src/service.rs`) keeps one long-lived child for the runner (`src/runner.rs`): a
WebSocket to `/v1/runners/connect`, authenticated by the runner token. The gateway pushes work that
names a project only. The runner clones the project once (`--heads --line main`, then
`config --local checks.runner true`), fetches it, runs `checks watch --once --json` and pushes the
results and caches with the runner token. The commands come only from the project's check
settings; the gateway never sends a command. A job reports pass, fail, error or waiting (the
project does not open yet: a private project opens after the creator trusts the machine with
`layr remote trust`).

Two transports carry the same messages. The join reply may name a WebTransport endpoint (for
example `https://rt.layr.ws/v1/runners/connect`, HTTP/3 over QUIC on UDP 443, one bidirectional
stream of JSON lines). `join` keeps it in `config.json` only when it is https, has the runner
path, and its host is the join gateway's host or a host of the same domain (the last two labels).
A welcome only switches it on or off; it never names where the token goes. When it is on, the
runner tries it first on the next connect; a failed
attempt (for example a network that blocks UDP) falls back to the WebSocket at once and skips
WebTransport for 10 minutes. The connection pings every 20 seconds (a WebSocket ping frame, or a
`{"type":"ping"}` line) and reconnects with backoff (1 to 60 seconds). On each
connect it sends its cursor (gateway epoch and last sequence, kept in `<store>/runner/cursor.json`)
and the project that it still checks, so the gateway resumes without losing work. Close code 4001
or HTTP 401 renews the token with a signature of the machine key; 4003 or HTTP 403 means the
runner was removed. The gateway gives a project to one runner of a platform at a time, so two
cloud runners do not run the same check at once. The local flock still covers one machine.

Jobs run as the joining user, inside sandbox-exec, with the macOS watcher rules (only main heads
and exact states that a maintainer approved). The residual risk is that of
[laptop-runner.md](laptop-runner.md): the process limit covers all processes of that user. A
dedicated runner account (a LaunchDaemon with its own `UserName`) is the next step.

The daemon restarts a runner child that ended after 10 s, and doubles the delay up to 60 s while
each run lasts less than a minute (for example, an invalid `config.json`).

To leave: remove the runner on the site, then remove `<store>/runner`.

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
override `checks.memory` after configuring it; this is a hard ceiling of the writable
private memory of each process (`RLIMIT_DATA`), and does not reserve that amount of host RAM.
On macOS the creator's platform map selects `apfs`, the nonprivileged APFS workflow
equivalent. Its watcher uses sandbox-exec and requires a current maintainer's
exact-state approval for change versions; main heads can run without it. Its
machine-local opt-in belongs to the user who owns the project's metadata.
The sandbox permits Unix sockets inside the private check copy. It denies TCP and Unix sockets
outside that copy, including paths through symlinks.
Privileged execution remains unavailable on macOS. The separate root-only service
test is an operator check. See [laptop-runner.md](laptop-runner.md) for machine
provisioning, volume setup, services, verification and residual host risk.

### Headless runner setup

Use `layr checks join <gateway> --code-file <path> --project <projects> --no-start` on a headless
Mac when a system service starts the daemon. Store the code in a private file owned by the runner.
The command saves the runner identity and token but
does not install or start a LaunchAgent. Start `layr daemon` as the same non-root user after the
service has applied its network restrictions.
