# APFS model and person-request integration checks

Task `work-1793001475891119872-8` starts at Layr main
`b41642ab1f4ba4591072886f7ea8153ffced2c57` on `cloud/apfs-fixes`.
The failing checks were reproduced before editing, as a native store owner with
user ID 501 on a separate case-sensitive APFS volume.

## Causes and corrections

The assertion lines below name the original test entry points.

| Assertion                                          | Cause                                                                                                                                                                                                                    | Correction                                                                                                                                                                                        |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `t37-model.sh:232`, merge required-check refusal   | The fixture protects main, then merges without `--expect`. A non-root user receives the reviewed-state refusal before the check runs. The root runner bypasses that condition.                                           | Supply the already-resolved `MERGE_SOURCE` with `--expect`, so the unchanged check-failure assertion reaches its intended boundary.                                                               |
| `t37-model.sh:274`, overlay transfer IDs           | The independent Python inspector opens a checkpointed WAL-mode database with `mode=ro` and no WAL/SHM files. The native SQLite opener fails with `unable to open database file` when it needs to initialize those files. | Use `mode=ro&immutable=1` for this static inspection after every CLI process has exited and checkpointed its database. Keep the transfer-ID assertions unchanged.                                 |
| `t37-model.sh:379`, overlay signed records         | The same read-only SQLite initialization issue prevents the independent database query.                                                                                                                                  | Use the same immutable read mode for the static overlay database. Keep the signed-record/golden comparison unchanged.                                                                             |
| `t37-model.sh:417`, verified page metadata         | The fixture hardcodes `user:0` although `init` makes the actual native caller the admin. The failure is `KeyError: 'user:0'`.                                                                                            | Assert that `user:<os.getuid()>` is admin, including the original root case. Keep all other page assertions unchanged.                                                                            |
| `t41-requests.sh:182`, replica request application | Store clone adopts local lines and records its remote and pin, but creates no local access policy. A non-root cloning user falls back to writer and cannot perform the admin-only sync needed by `requests apply`.       | Apply the approved clone-owner policy: append a signed local access setting granting the cloning caller admin of its new copy. The change applies to all store URL types and public local copies. |

The clone fix is the laptop-runner owner's commit
`c92d71c3cc44cfbfaf0a2249b44edc80951f0b9e`, cherry-picked as `a09e90c`.
It is not a parallel implementation. The lead approved the policy and all-clone
scope before it was applied. Git clones already use `init`, which grants their
owner local admin. Remote policy, signing trust, gateway grants, and the
creator-selected automatic check commands are unchanged.

The person-request suite also directly checks the cloned project's local access
setting. That assertion catches a missing grant even on the root runner, where
an ordinary permission check would bypass the defect. The original CLI also
fails this new witness because `config --get access` returns exit code 1; the
fixed CLI passes it.

The SQLite mode applies only to sequential fixture inspection. These are not
live application reads: the test starts no daemon, every command has returned,
and its WAL is checkpointed. A standalone copy of an affected database
reproduced the distinction:

```text
mode=ro OperationalError unable to open database file
mode=ro&immutable=1 (8,)
```

No assertion was weakened and no APFS filesystem implementation was changed.
Linux root behavior is preserved; the tests now reach the same intended
boundaries for a native non-root owner. The lead owns the Linux full-suite gate
before integration.

## Commands and results

```sh
cargo build -j 4
LAYR_APFS_TEST_ROOT=/Users/igor/.local/state/layr-m6/volume \
LAYR_TEST_DIRECT_MAIN=1 CARGO_BUILD_JOBS=4 \
PATH=/opt/homebrew/opt/gnu-sed/libexec/gnubin:/opt/homebrew/opt/coreutils/libexec/gnubin:$PATH \
bash tests/t37-model.sh
LAYR_APFS_TEST_ROOT=/Users/igor/.local/state/layr-m6/volume \
LAYR_TEST_DIRECT_MAIN=1 \
PATH=/opt/homebrew/opt/gnu-sed/libexec/gnubin:/opt/homebrew/opt/coreutils/libexec/gnubin:$PATH \
bash tests/t41-requests.sh
cargo test --workspace -j 4
```

- Model integration: **47 passed, 0 failed**, including the four original failed assertions.
- Person requests: **60 passed, 0 failed**, including the explicit clone-policy witness.
- Workspace tests: **227 passed, 0 failed, 32 ignored**. Ignored checks retain their existing environment/explicit-run requirements.
- Shell syntax checks and `git diff --check`: passed.

The original diagnostic run exposed all four model failures and retained its
scratch store. Local logs:

- Before model corrections: `/Users/igor/.local/state/codex-agents/monitor-logs/967fb152-da20-5e7e-96f5-1e7cf4be398f.log`.
- Passing model suite: `/Users/igor/.local/state/codex-agents/monitor-logs/3e07c29f-9e77-5a4c-9be1-dee0d3364d5d.log`.
- Passing person suite including the clone-policy witness: `/Users/igor/.local/state/codex-agents/monitor-logs/164fc2e9-92bd-58f5-a98d-0ba69b19a2b1.log`.
- Original CLI failing the direct clone-policy witness: `/Users/igor/.local/state/codex-agents/monitor-logs/5122c0a2-7b49-51d9-b3fe-bc215dd2b8f5.log`.
- Workspace tests: `/Users/igor/.local/state/codex-agents/monitor-logs/d4ff352e-2cf3-5e71-934e-65917c95c2a1.log`.

All runtime state and diagnostic files stayed outside the checkout. No main
branch was merged, no deployment was changed, and README files were not edited.
