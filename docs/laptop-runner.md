# Laptop check runners

The dedicated Lima VZ VM `layr-runner-vz` runs btrfs and OverlayFS checks. The Mac
user runs APFS checks. Both receive immutable states, run `layr checks watch`,
and publish signed check attestations and retained logs through ordinary fetch
and push. AWS executes no test code. See [checks.md](checks.md) for result and
deduplication semantics. Check policy comes only from the verified project creator; runner flags remain machine-local.

## Linux VM

The isolated OrbStack machine `layr-runner` was created first. Its device policy
refused loop setup and even `mknod /dev/loop-control` with EPERM. Its watcher is
stopped. Do not remove that isolation or enable Mac mounts. Use the full VM:

```sh
limactl create --yes --name=layr-runner-vz tools/checks/layr-runner-vz.yaml
limactl start --yes layr-runner-vz
git archive HEAD -o /tmp/layr-runner-source.tar
limactl copy /tmp/layr-runner-source.tar layr-runner-vz:/tmp/source.tar
limactl shell --workdir=/tmp layr-runner-vz sudo bash
mkdir -p /opt/layr-runner /var/log/layr-runner
tar -xf /tmp/source.tar -C /opt/layr-runner
cd /opt/layr-runner
setsid nohup bash tools/checks/install-linux.sh > /var/log/layr-runner/install.log 2>&1 </dev/null & disown
```

The tracked VZ config has `mounts: []` and disables SSH agent forwarding. Before
allowing privileged checks, verify `/Users` and `/mnt/mac` do not exist inside,
and attach/detach a disposable image with `losetup --find --show` / `losetup -d`.
Never stop or restart the shared development machine or OrbStack itself.

The installer prepares the pinned MinIO source release, Helm 3.19.0, filesystem
tools, bubblewrap, and a credential-free Rust toolchain/cache under `/usr`.
It enables unprivileged user namespaces for bubblewrap on Ubuntu 24.04.
Inspect the install log before continuing. Keep one build target, remove the
installer's `target/` after installing the binary, and remove Go build/download
caches after MinIO builds. Checks compile in private copies deleted after each
run; they share only the read-only toolchain. Keep at least 15 GiB free on the
Mac backing volume. A VM's reported free space does not measure this limit.

Run `bash tools/checks/prepare-linux-store.sh`. It uses the existing btrfs root
when present, or creates a persistent btrfs loop mount at `/var/lib/layr`.
Start the VM from a user LaunchAgent so its host agent does not inherit a Studio
command's lifecycle. At a safe point, stop this VM, then run on the Mac:

```sh
limactl stop layr-runner-vz
python3 tools/checks/install-vm-agent.py
```

The agent runs `limactl start --foreground` at login. It refuses to replace a
running VM. Stop it with `launchctl bootout gui/$(id -u)/ws.layr.runner-vm` before
planned VM maintenance. After a manual stop, use
`launchctl kickstart gui/$(id -u)/ws.layr.runner-vm` to start it again.

Obtain this machine's public identity with `layr remote self --json`.
On an existing project owner's machine, enroll it with:

```sh
layr remote trust origin RUNNER_MACHINE_ID --public-key RUNNER_PUBLIC_KEY
layr push origin
```

On the creator's machine, configure the project with `tools/checks/configure.sh`
and push that signed policy. Clone the existing remote on the runner using its own machine slot. Never put a
real project's paper key on a runner. Give it only its own machine identity, an owner-issued project write grant with the maintainer role, and
the transport credential needed for that project. Transport credentials come
from the environment (`LAYR_GATEWAY_TOKEN_FILE` or the relevant storage variables),
never project config or check commands. A root-owned `/etc/layr-runner.env` may
provide those variable names/paths to the daemon; keep that file mode 0600.
For a transient credential, use a root-owned mode 0700 directory such as
`/run/layr-runner-credentials`, with the token file mode 0600. Keep it outside
`/run/layr-runner`: systemd removes that daemon runtime directory on restart.
A tmpfs credential must be provisioned again after a VM reboot.
After cloning, as root in the project:

```sh
layr access grant layr-checks reader
layr config --local checks.runner true
layr config --local checks.privileged-runner true
bash tools/checks/install-services.sh PROJECT
```

The daemon has cgroup memory/process ceilings. `layr-checks@PROJECT` runs the
watcher as the non-root service user and restarts on failure or VM boot.
`layr-sync@PROJECT.timer` fetches then pushes results and the site index every
120 seconds with up to 30 seconds of jitter. Stop this project's units before replacing its binary or policy.
An executor elsewhere keeps both machine-local execution flags off.
A shared gateway limit also applies to runners behind the same public IP.
Respect HTTP 429 `Retry-After`; pause transport while an owner publishes a large
fixture, then resume the timers. Check watchers can remain active during the pause.

```sh
systemctl status layr-runner-daemon layr-checks@PROJECT layr-sync@PROJECT.timer
journalctl -u layr-checks@PROJECT -u layr-sync@PROJECT --since today
limactl stop layr-runner-vz && limactl start layr-runner-vz  # Mac; verify the units again
```

## Mac user runner

Install coreutils, gnu-sed and zstd through Homebrew. In a trusted checkout run
`bash tools/checks/prepare-macos.sh` as the runner user. It prepares two dedicated
sparse APFS disk images, mounts them at `/Volumes/LayrRunner` (case-sensitive)
and `/Volumes/LayrRunnerCase` (case-insensitive), and prepares an APFS-cloned
toolchain/cache seed without credentials. Keep these images mounted while the
watcher runs. After a Mac restart, rerun the preparation command to attach them.
Do not store personal data on these volumes. Refresh the seed after Cargo.lock
changes. The seed lives in `~/Library/Application Support/Layr/checks`.
The case suite uses private folders on these images and verifies both case modes;
automatic jobs do not create or attach additional images through macOS disk services.

There is one separate operator check, outside automatic execution. After building
the binary, the owner runs this once from the trusted checkout:

```sh
sudo bash tools/checks/macos-operator.sh "$PWD/target/debug/layr" igor
```

This enables volume ownership and executes the root-only `tests/apfs-service.sh`.
Its result is operator evidence, separate from the nonprivileged APFS attestation.
Automatic commands never invoke sudo. Separately, as the Mac user (without sudo),
run `bash tools/checks/macos-operator.sh --keychain` to exercise the three login
Keychain fixtures. Report each operator step as unverified until it has completed
successfully. Automatic checks never receive login Keychain access.

Enroll the Mac's own public machine identity from the project owner's machine,
then clone into a dedicated user-owned store. In that clone:

```sh
layr config --local checks.runner true
python3 tools/checks/install-macos-agent.py PROJECT \
  --binary /absolute/path/to/installed/layr --root /absolute/path/to/runner/store
```

Use a stable installed binary path. The user LaunchAgents restart the watcher
and fetch/push every 120 seconds, with logs under `~/Library/Logs/Layr`.

For a gateway remote, add `--gateway-token-file /private/path/to/token` to the
installer command. The file must be owned by the runner user and mode 0600;
only its path is written to the LaunchAgent environment. Refresh expiring grants
in that file outside the automatic check process.
Execution opt-in is a private machine-local file owned by the project metadata owner on
macOS. Privileged opt-in is always refused on macOS. Linux retains its root-owned
opt-in policy. Stop with `launchctl bootout gui/$(id -u)/ws.layr.checks.PROJECT`
(and the matching `fetch` and `push` agents).

Mac execution permits main heads and exact change versions with an active approval
from a current maintainer. Amending a change requires a new approval; withdrawing
an approval blocks unexecuted work. Other states stay pending. Local uid approvals
count only on their own machine; pinned person approvals survive transport.
The watcher consumes only creator-side check policy and protection. Local admin
access on a clone permits local operations and transport configuration; its own
check definitions cannot select or replace automatic jobs.

`sandbox-exec` denies network access and permits writes only in the private check
folder and the two dedicated test volumes. Its private source copy lives under `/private/var/tmp`, outside the personal home.
The check gets a clean environment,
private HOME, timeout, process limits and capped logs. Tools and the prepared
cache are read-only. Darwin does not enforce the Linux address-space limit;
`checks.memory` still bounds output files there. Missing or refused sandbox
execution fails visibly.
The private Cargo home links to the prepared read-only registry; its lock files
and build outputs stay private. This avoids copying every cached crate on each run.
The Mac process limit counts all processes of the runner user, including desktop
applications; its default is 4096. Linux uses a dedicated service account.
Residual risk: approved repository code executes on the personal Mac with access
to macOS system interfaces and read-only system/Homebrew tools. The sandbox is
not a VM isolation boundary; kernel or system-service defects may escape it.
Approval grants trust to run that exact version. Linux privileged code has full
VM root access and must never share the personal Mac's files or credentials.

The automatic Mac command omits these exact unit test names only:

| Test | Reason and separate coverage |
| --- | --- |
| `store_remote::backend::tests::reader_hooks_use_s3_caller_credentials_and_leave_file_reads_local` | Local HTTP server; full Linux suite on the same state |
| `store_remote::gateway::tests::epoch_slot_staging_uses_the_project_grant_before_a_new_proof_exists` | Local HTTP server; full Linux suite on the same state |
| `store_remote::gateway::tests::missing_descriptor_keeps_optional_person_reads_and_hints_machine_grants` | Local HTTP server; full Linux suite on the same state |
| `cmd::checks::macos_tests::sandbox_refuses_network_and_writes_outside_work` | Needs an outside-sandbox TCP baseline; run separately on the Mac |
| `disk::catalog::tests::status_reports_the_keychain_and_service_without_the_identity` | Login Keychain fixture; manual `--keychain` operator step |
| `disk::keystore::tests::login_keychain_roundtrip` | Login Keychain fixture; manual `--keychain` operator step |
| `disk::tests::keychain_lock_hides_files_and_blocks_webdav_until_unlock` | Login Keychain fixture; manual `--keychain` operator step |

These omissions were confirmed by the actual sandboxed run (117 passed, 19 failed
before the filesystem profile correction). All APFS storage/thin tests remain in
the automatic suite. Network access remains blocked, including localhost services.

## Verification

Run `cargo test --locked --bin layr macos_tests -- --nocapture` for actual sandbox
network/write refusal, and `LAYR="$PWD/target/debug/layr" bash tests/apfs-checks.sh`
for main execution, approval/amend/withdrawal gates, failed checks and retained
signed logs. Linux's `tests/t43-checks.sh` covers remote arrivals and attestation
deduplication. The configured filesystem scripts run the workflow suites.

For acceptance, push a new change version to a disposable file/MinIO project,
fetch on each runner, and verify the btrfs, ext4, xfs and APFS results with
`layr attestations STATE --kind check --json`. Read each result's retained log
with `layr show LOG_REF:check.log`. Trigger one deliberate failure and verify its
nonzero result/log. Restart the VM and verify the watcher resumes. Finally repeat
against a disposable project on `cloud.layr.ws`, then inspect its change/main
pages. A local run alone does not prove site delivery. A disposable test store's
paper key stays only in its owner's temporary folder and is deleted with that
store; it never enters the runner VM.

The [measured acceptance evidence](laptop-runner-evidence.json) records the local
and cloud runs, VM restart, isolation checks, installed binaries and cleanup.
The cloud run on source `e74ceff` passed all five jobs and showed five green rows
on the published site. The earlier APFS failure exposed a concurrent file-store
publication defect; its signed result is retained with the five passing results
in [the signed envelopes](laptop-runner-signed-checks.jsonl). Their original logs
and SHA-256 manifest are in [the log archive](laptop-runner-check-logs.tar.gz).
The root APFS service and three login Keychain fixtures remain unverified until
Igor runs the separate operator steps above.
