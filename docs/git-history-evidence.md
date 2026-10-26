# Git history validation

Measured on 2026-10-26 in the dedicated `layr-gh` OrbStack machine (Ubuntu,
AArch64, Btrfs). Source repositories were copied without hardlinks; the source
branches were read only. Mirrors are local bare repositories. No production
Git mirror or AWS store was modified.

## Correctness

`t49-git-history.sh` passes 28 checks. Its five-commit merge fixture includes raw
commit headers and message bytes, a tracked ignored file, binary content,
executable mode, symlink, and Gitlink. It checks all original commit IDs after an
encrypted store transfer and fresh clone, the exact tree, a new child's parent,
repeat import, refused non-fast-forward pushes, and explicit force.

The suite also kills a 64-state push before the first metadata checkpoint,
resumes it, and verifies that existing ciphertext objects are unchanged and all
64 states are complete. It checks incremental file-type changes and corrupts a
loose Git blob before exporting the original IDs again.

`cargo test --workspace --locked`: 228 passed, 7 intentionally ignored.
The requested store suites pass: t19 (25), t22 (7), t23 (4), t35 (49), t42 (99).
Git suites t03 (57), t08 (28), t17 (9) also pass. Counts are assertions, with zero
failures. The lead owns the full-suite check after integration.

## Initial measurements and profile

For layr's 392 commits at `c1d7229a30aa808fedc0274e961b2f38e7cc2f7d`, the first
correct implementation took 402.3 seconds to push and 275.7 seconds to clone in
a debug build. Its release build took 26.1 seconds to push and 65.7 seconds to
clone. The release push spent 12.34 seconds rewriting the complete pushed-state
cache, 8.66 seconds uploading states, and 1.57 seconds rebuilding content indexes.

Incremental hashes, parent snapshots, bounded local tree caches and metadata
checkpoints reduced the 392-commit release push to 11.34 seconds. Cloning parents
also reduced fresh Btrfs space used from 1,756,188,672 to 183,930,880 bytes.
The first clone profile then exposed 82.93 seconds in 4,240 individual Git object
publications, dominated by filesystem flushes. Atomic publication of rebuildable
cache objects reduced a subsequent clone to 19.59 seconds. These samples were
collected during development and include competing VM builds in some runs.

For the 7,965-commit layr-studio copy, import took 558.5 seconds, with maximum RSS
377,820 KiB. The first store-cache optimization still took 1,103.7 seconds to
push. A partial profile at 5,859 states showed 193.3 seconds decoding parent
cache files, 212.5 seconds encoding trees, 109.6 seconds persisting cache files,
and 63.4 seconds checking secrets. The next revision keeps the preceding parent
in memory and emits identical canonical tree bytes directly from typed entries.
The same encoder is used by the reader; canonicality, path, frame, and signature
checks remain in place.

A full Git cache scan on the 7,965-commit source took 13.01 seconds in one sample
and 9.35 seconds in another. Normal bridge commands therefore hash objects they
read. Full scanning and cache reconstruction occur after a failed read or hash
check. The corrupted-cache regression verifies exact IDs after repair.

## Back-to-back transfer sample under host contention

The next release run pushed and cloned 392 commits, then pushed 7,965 commits.
The 392-commit round trip retained every original ID and tree. Push took 61.89
seconds; clone took 62.70 seconds. The 7,965-commit push took 1,651.76 seconds.
These absolute times do not establish quiet-machine performance: at 13:45 UTC,
the 18-CPU host load averages were 80.39 / 97.71 / 157.17, while the guest load
was 2.55 / 2.05 / 1.92. Historical CPU samples for the completed pushes were not
recorded. Later clone and gateway measurements include a sampled load log.

| Push phase | 392 commits (s) | 7,965 commits (s) | Ratio |
| --- | ---: | ---: | ---: |
| Secret checks | 15.32 | 395.05 | 25.8 |
| Parent selection | 0.38 | 114.02 | 297.5 |
| Walk and file frames | 15.64 | 340.05 | 21.7 |
| Canonical tree encoding | 0.91 | 138.26 | 152.5 |
| Tree publication | 8.36 | 207.82 | 24.9 |
| Local cache persistence | 20.43 | 378.60 | 18.5 |
| Complete push | 61.83 | 1,650.79 | 26.7 |

Commit count grows 20.3 times. The tree encoding and parent-selection phases grow
faster than commit count in this comparison. This compares different repositories:
the final source trees contain 378 and 3,132 files, and their branch structure
differs. Tree encoding necessarily visits each current file entry, while branch
switches load and validate the selected parent tree. The implementation no longer
revisits all preceding cached trees for every state. A same-repository prefix
study would be needed to isolate those effects from workload width and host load.

The source-index rebuild accounted for much of the secret-check constant cost.
The following patch prepares it once per push while retaining checks on changed
files. It also removes complete historical-tree hydration and hash-index loading
from ordinary incremental pushes. Gateway measurements below use that patch.

A tree-ID query using the final selective reader took 0.031 seconds and returned
`f73d8a3b174dae65889605159ff2e9d98657bb57`. A baseline query without explicit
cache verification took 0.195 seconds in an earlier sample; full scans took
9.35–13.01 seconds. These are wall-clock samples, not a controlled speedup claim.

## Real-size round-trip result

The file-store round trip preserved every one of layr-studio's 7,965 original
commit IDs. Both tips are `eec4c6d136da146a5d3d659575c1baf2d93849f8`; both final
trees are `f73d8a3b174dae65889605159ff2e9d98657bb57`. Mirror `git fsck --full`
passed. Clone took 1,808.32 seconds under the recorded host contention, with
maximum RSS 1,414,556 KiB. Git mirror export took 99.59 seconds and fsck 7.09
seconds. The encrypted store contains 15,005 objects totaling 367,301,433 bytes.
The 392-commit store contains 759 objects totaling 6,099,639 bytes.

[Raw file-store measurements](../benchmarks/git-history-20261026/file-results.jsonl)
include counts, IDs, tree hashes, timings, sizes and peak process RSS.

## Gateway and incremental requests

The gateway benchmark uses a local TLS fixture matching the private object,
proof and token routes, with 5 ms added to every request. It measures client
transport behavior; it does not establish AWS or deployed-gateway performance.
The 392 and 7,965 tests ran back to back. The full gateway pushes took 17.89 and
974.48 seconds after preparing the secret detector once per push.

The first 392-commit incremental push took 0.414 seconds: 18 GET, one POST and
three PUT requests. Its fresh clone took 32.36 seconds and retained all original
IDs plus the new commit with the expected parent and tree.

The 7,965-commit fixture exposed the old 10,000-object listing limit after the
full push published 15,000 objects. The client now requests pages of 1,000
objects, bounds each response to 8 MiB and the aggregate to 100,000 objects, and
rejects duplicate keys and repeated cursors. Public and private routes retain
their authentication behavior. Server pagination is a separately owned change.

A regular writer now trusts its metadata for previously completed immutable
uploads and lists only new state prefixes. The collector never removes complete
state trees. Missing parent caches are hydrated and verified when needed. The
file backend starts listing in the requested prefix directory, which avoids
scanning sibling states for every new state.

Resuming the failed one-commit gateway push took 2.607 seconds: 17 GET, one POST,
and two PUT requests. Its record segment had already published before the old
limit failed, so the retry uploaded only the file part and state tree. Listing
prefixes were `keys/`, the writer's own record prefix, and the single new state
prefix. It did not list the complete state history.

The paginated fresh clone of the 7,965-commit fixture plus its first new commit
passed: 7,966 Git commits, all 7,965 original IDs unchanged, the new commit's
parent correct, and tree `29217a1cfa33fc428f17a49d90a7b74f9e0903e1` matching
the writer. It took 1,072.13 seconds, with maximum RSS 1,429,836 KiB. It used
31,177 GET, four POST and two PUT requests, including 15 state-listing pages.
The PUTs enroll the reader; they do not replace existing state objects.

After that clone, a clean additional one-commit push on the same history took
**3.584 seconds**, with **17 GET, one POST and three PUT requests**. All three
publications were required: new signed records, one file part and one state tree.
It listed `keys/` twice, its own record prefix once and the new state prefix once.
It did not list or download old state trees. The preceding 392-commit sample used
22 requests; the final large-history push used 21. The client still verifies its
local signed record history and reads bounded metadata; this measurement does
not claim constant CPU cost for arbitrarily large local databases.

| Gateway operation | Seconds | Guest load at start/end | Sampled host load range |
| --- | ---: | ---: | ---: |
| 392-commit full push | 17.89 | 3.13 / 2.87 | 23.88–24.50 |
| 393-commit fresh clone | 32.36 | 2.87 / 3.08 | 26.68–28.55 |
| 7,965-commit full push | 974.48 | 3.08 / 4.00 | 21.39–78.98 |
| 7,966-commit fresh clone | 1,072.13 | 1.73 / 1.90 | 12.85–29.53 (partial window) |
| Clean daily one-commit push | 3.58 | 0.34 / 0.47 | 29.19 immediately afterward |

Both host and guest expose 18 CPUs. Values are one-minute load averages, not CPU
utilization percentages. The clone's host sampler covers only part of its run;
there is no overlapping host sample for the short daily push. Raw timestamps
make these limits explicit. The source-index preparation phases took 0.026 and
0.052 seconds in the final full pushes, compared with repeated preparation of
15.32 and 395.05 seconds in the earlier file samples.

Raw [small gateway results](../benchmarks/git-history-20261026/layr-gateway.json),
[large gateway results](../benchmarks/git-history-20261026/layr-studio-gateway.json),
[phase totals](../benchmarks/git-history-20261026/phases.json), and
[load samples](../benchmarks/git-history-20261026/load.jsonl) retain timings,
request counts, listing prefixes and correctness results. The large result file
also retains the failed old-limit attempt and the successful resume.

The absolute ten-minute full-transfer target was not established on this busy
host. The complete history round trip, daily push latency and bounded daily
request count were established. Production server pagination was delivered in
the separate layr-cloud change; no AWS operation was performed by this task.
