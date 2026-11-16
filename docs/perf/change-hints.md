# Filesystem change hints

`ChangeHints` supplies observed candidates relative to a specific native base. Only
`Complete` can prune the comparison. `Partial`, provider errors, and unavailable
capabilities select the full comparison; native hints never apply to object-only
bases. Partial evidence is deliberately discarded rather than mixed with a new scan.
The full scan propagates observation errors and retains block ranges. Without a
proved shared native identity domain it keeps Add/Remove; merge applies ordinary
content rename detection and confirms candidate modifications before consuming them.
OverlayFS identity pairing remains inside its proved shared-layer comparison.

Native status captures the working tree before requesting hints. It avoids the
status cache's eager per-file stat pass when the selected index has a native base
and the driver offers hints. Local ignore rules are captured separately so nested
`.git` subvolume placeholders cannot hide them. Object-only bases retain the shared
status cache.
APFS does not advertise hints: its per-entry counters do not yet prove a complete
set of changed paths. Its existing comparison is preserved.

Btrfs requires two read-only snapshots and a successful metadata send. Unknown send
commands, non-UTF-8 paths, failed metadata observations, and changed hard-linked
inodes fall back. The latter needs an inode-reference expansion before it can be
accelerated safely. OverlayFS reuses enumeration of unshared layers, including
whiteouts and opaque/replaced subtrees. Unknown ancestry or hard-link materialization
selects the full comparison.

## Structural measurement (normal user, no service)

Use a native scratch volume, never a VM sharing the host home. Run from this line:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export TMPDIR=/var/lib/layr-test/tmp-fs-change-hints
mkdir -p "$TMPDIR"
cargo +1.100.0 test --locked --bin layr change_hints_50k_metadata_measurement \
  -- --ignored --nocapture
```

The fake provider makes real metadata observations and the source adapters count
logical metadata lookups. This measures the shared comparison's traversal budget,
not kernel syscalls, capture cost, provider enumeration, or end-to-end status time.
For 50,000 entries in each tree, complete clean hints require zero entry observations;
ten dirty paths require twenty. Partial and failed hints require 100,000 in both
cases, and confirmed content changes must match the complete result.

## Native 50k A/B for the lead's privileged harness

Btrfs send requires privileges; do not run Layr or tests as root. Extend the existing
`perf/caller-ab.sh` harness in the issue scratchpad: only its isolated service runs
as root, while all clients remain UID 1000. Build baseline state
`cb0e68b6b4827493a0f201a1daaf8257` and this line as UID 1000, in release mode, and
pin each binary's SHA-256 as the caller A/B driver does. Run the harness's dry mode
before handing its privileged wrapper to the lead.

1. Keep the existing isolated namespace, dedicated filesystem, socket, store,
   caller identity, binary checksum validation, and cleanup rules. Do not migrate
   or retire native history for this measurement. Select btrfs and OverlayFS in
   separate runs with their corresponding binaries.
2. Create 50,000 files of 4 KiB and one 32 MiB file. Initialize a fresh project per
   variant/repetition; remove main protection as the normal caller. Record that
   the selected index is native and retain the native parent.
3. Warm `status --porcelain`, then measure clean status. Edit ten tracked files,
   preserving their lengths, and measure dirty status twice. Require empty clean
   output and exactly ten ` M` dirty paths; compare the exact sorted porcelain
   output across baseline and candidate.
4. Alternate baseline/candidate for at least five timing trials. Collect wall,
   user, and system time separately from tracing. In a separate trial attach
   `strace -f -c` to the isolated daemon (not just the client), tracing `statx`,
   `newfstatat`, `fstat`, `openat`, `read`, `getdents64`, `lgetxattr`, and `llistxattr`.
   Keep raw summaries, client stderr with `LAYR_TRACE=1`, backend, state IDs,
   binary hashes, kernel version, fixture size, and native-retention mode.
5. Expect candidate native status to avoid the status cache's two full fingerprint
   scans. Do not expect literally zero syscalls: snapshot creation, registry,
   ignore rules, directory metadata, and the native provider still do I/O. Btrfs
   must show the send span; OverlayFS must show its candidates span.
6. Repeat with a changed hard-linked file and verify full-scan fallback reports
   every changed alias. Also verify directory renames, whiteouts, opaque directory
   replacement, xattrs, chmod, additions, and deletions against the full result.

Direct object-only 50k status remains a useful fallback/control measurement. Retire
native parents as in `tests/status-stat-cache.py`, warm the persistent cache, and
trace the normal-user client with `LAYR_DIRECT=1` and a nonexistent `LAYR_SOCKET`.
It should still stat the tree: there is no proved native base for these hints.

## Local results (2026-11-14)

Normal-user btrfs scratch volume, Rust 1.100.0, debug binaries, direct execution:

- The 50k structural test passed: complete clean/ten-dirty hints observed 0/20
  entries; partial and failed hints each observed 100,000. All confirmed deltas
  agreed (zero or ten files).
- Retired-native, object-only CLI control: baseline and candidate each made
  100,061 `statx`, 59 `newfstatat`, and 91 `fstat` calls per warmed status, for
  both clean and ten-dirty trees. Exact porcelain output agreed. This unchanged
  fallback is expected; these hints require a native base.
- Native btrfs send and mounted OverlayFS status measurements remain for the
  lead's isolated service harness. No privileged Layr/test process was run here.

The control used 50,000 files of 4 KiB without the extra large file. Raw local
artifacts and its normal-user driver are under
`/var/lib/layr-test/tmp-fs-change-hints/` (`direct-measure.log`,
`direct-50k/*.strace`, `direct-50k/results.json`, `measure-direct.py`). The native
harness above retains the issue's additional 32 MiB file for comparable timing.
