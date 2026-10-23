# Probe workstream evidence

Branch: `cloud/probes`.
Base: `64c11f7` (accepted Views merge into `cloud/integration`).
Contract: `docs/wave2-design.md`, section 6, plus the lead's recorded decisions.

## Implementation

- A typed `[probe.<name>]` schema in the shared `layr.toml` parser.
- Explicit `probe run`, with human output and `--json`.
- Exact state and view inputs through `revs::resolve_view`.
- The accepted `admin::with_try_copy` and `try_command` helpers provide execution, identity, environment, and cleanup.
- Private overlay outputs are refused by name. Every project reader, including line owners, must be an overlay reader.
- Declared outputs are captured from a frozen copy. Links and special files are refused.
- Detached output states use `kind = derived`, `context = probe-v1/<name>`, and exact subject provenance.
- Refs `derived/probe-v1/<name>/<subject>` retain outputs. A local execution lock prevents duplicate concurrent runs of one key.
- A signed check includes exact argv, exit code, output state, subject, and probe definitions.
- Core `Db::check_exit` gates reuse. A separate check requires an active probe attestation, matching definition, retained ref, and local output state.
- Stored comparisons show text patches, binary sizes and SHA-256 hashes, JSON Pointer changes, or relative numeric changes with noise suppression.
- `change show` adds stored state-subject behavior against the merge base with the source line's upstream, or `main`.
- View-only results produce explicit ViewId hints. The command never selects a view implicitly.
- A ViewId in `derived_from` is provenance. It does not retain the view's sources.
- Comparisons use signed definitions and retained outputs after source collection. They report source availability separately.
- Only explicit `probe run` executes code. Queue integration remains with the queue workstream.

No new dependencies.
The model validator accepts a full lowercase ViewId only for `Derived` provenance.
Cache provenance still requires a StateId.

## Verification commands

Native:

```sh
cargo test probes -- --nocapture
cargo test model::tests::derived_sources_allow_exact_views_but_cache_sources_remain_states
rustfmt --edition 2021 --config skip_children=true --check src/probes.rs src/cmd/probe.rs src/cmd/change.rs src/cmd/mod.rs src/main.rs src/model.rs src/oci.rs
git diff --check
bash -n tests/t32-probes.sh
```

Linux uses the owned copy `/root/work/probes` in OrbStack `layr-dev`.
The source timestamps are refreshed after each copy so Cargo cannot reuse an older binary after a rebase.
The probe suite checks the new source-status JSON fields before it trusts the binary.

```sh
COPYFILE_DISABLE=1 tar --no-xattrs --exclude=target --exclude=.git --exclude=__pycache__ -cf - . |
  orb -m layr-dev -u root bash -lc 'tar -xf - -C /root/work/probes && cd /root/work/probes && find src tests -type f -exec touch {} + && touch Cargo.toml Cargo.lock && cargo test && cargo build && sudo tests/run.sh t32-probes t23-views t18-core t20-guard t13-access t07-service t30-oci'
```

## Limits

- The existing `try` copy is the isolation mechanism. This task adds no network or system-call sandbox.
- Screenshots compare as binary files. There is no image comparison.
- A private overlay has no shared output storage in this step.
- Reader containment uses permanent overlay roles for the project reader principals. Temporary overlay grants do not qualify.
- Commands that need collected source files fail with `source collected`. Retained output comparisons still work.

## Recorded checks

Native macOS, 2026-10-23 UTC:

- Probe unit tests: 5 passed, 0 failed.
- Derived provenance validator test: 1 passed, 0 failed.
- Changed-file Rust format check, `git diff --check`, and shell syntax check: passed.

Native monitor: `33f23678-02cb-5aa4-a867-718dcb5f4bd0`.
Format monitor: `25a19100-b552-5ef0-af9c-53b45e5c8796`.

Linux final results, 2026-10-23 UTC:

| Check | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| `cargo test` | 63 | 0 | 1 |
| `cargo build` | 1 | 0 | 0 |
| `t32-probes.sh` | 34 | 0 | 0 |
| `t23-views.sh` | 55 | 0 | 0 |
| `t18-core.sh` | 72 | 0 | 0 |
| `t20-guard.sh` | 38 | 0 | 0 |
| `t13-access.sh` | 78 | 0 | 0 |
| `t07-service.sh` | 31 | 0 | 0 |
| `t30-oci.sh` | 42 | 0 | 0 |

The existing ignored peer transfer test requires its own root Btrfs fixture and a peer binary.
Full build and suite monitor: `acfc0d3a-461a-55b5-92dc-2ba3fd3b48b0`.
That run had one probe fixture failure: only root could read the project, so its root-owned overlay was readable by every project reader.
The fixture now grants a project reader role before it asserts private-layer refusal.
The refusal assertion and runtime code are unchanged.
The corrected probe suite passed with the same freshly built binary:

```sh
COPYFILE_DISABLE=1 tar --no-xattrs -cf - tests/t32-probes.sh |
  orb -m layr-dev -u root bash -lc 'tar -xf - -C /root/work/probes && cd /root/work/probes && touch tests/t32-probes.sh && sha256sum target/debug/layr && sudo tests/run.sh t32-probes'
```

Probe rerun monitor: `b433cba1-2ffb-50be-8a6e-e398b1aade7f`, exit zero.
Binary SHA-256: `2f0b06f64a1cc499511ee507c4d76fa3ca12381aedc5a46511ffa1de555b7508`.
The corrected probe suite covers exact view bytes, private-layer refusal, view-subject provenance, view-result hints, source collection, retained output comparison, caller identity, environment removal, and access denial.
The only Rust source difference after the full VM copy is a module declaration order from Rust formatting; runtime code is unchanged.
