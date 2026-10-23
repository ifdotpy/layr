# Layr and Layr Cloud: implementation plan

Source of truth for decisions: `docs/cloud-rfc.md` (RFC 0.9), overridden where stated by the lead's design files (`docs/remote-design.md`). This file assigns the work.

Layr Desk will be deleted and replaced by Layr Cloud: no code in Layr may depend on the `layr-desk` repository; port what is needed.

The word for the old whole-project copy command must not appear anywhere in Layr (user decision): remove the command, its code, tests, docs and messages.

## Scope

In scope: everything in the RFC for Layr (the CLI and the service) and Layr Cloud (a server),
without a graphical UI.

Out of scope for now:
- Graphical UI of any kind (web review, Desk site changes).
- Chromium-specific work: RFC part II sections 16, 17, 18.2 (REAPI instances), 22, 24, 25,
  Buildbarn, clangd, rustc. The generic parts of part II stay in scope: state kinds `cache` and
  `derived`, refs with TTL, attestations.
- Studio changes (the per-tool-call hook, RFC 5.5). Layr provides the commands Studio needs.
- Database adapters (RFC 9.2, 29.8).
- New file systems (RFC 11).

## Rules for every worker

- Base: branch `cloud/integration`. Work on your own branch `cloud/<workstream>`. Commit there.
  Only the lead merges into `cloud/integration`.
- Command names: RFC 12.4 and 12.6. No aliases: an old name fails with a message that names the
  new command.
- Layr Core knows no agents, reviews or deploys (RFC 1.5). Names in Core are generic.
- No own cryptography (RFC 6.3): use `age`, Ed25519, HMAC-SHA256, HKDF from existing crates.
- Every new command prints machine-readable output with `--json` where the existing command
  family does.
- Tests: unit tests (`cargo test`) and an integration test file `tests/tNN-<name>.sh` that uses
  `tests/lib.sh`. Integration tests need Linux and btrfs: use the OrbStack machine `layr-dev`
  (`orb start layr-dev`, then `orb -m layr-dev -u root bash -lc '...'`). Build and run in a
  copy inside the machine (for example `/root/work/<workstream>`), not on the shared Mac folder,
  so that workers do not share a `target` folder. Run `sudo tests/run.sh <file>` for your file
  and the existing files your change can affect.
- Keep the code style of the repository: comment density, naming, error messages.
- Update `README.md` (commands table and the section of your feature) in the same branch.
- Report: what works, the exact test commands and their results, what is not done, and every
  deviation from the RFC with the reason.

## Shared contracts

- Reserved revision prefixes (RFC 12.6): `env/`, `release/`, `pins/`, `holds/`, `leases/`,
  `caches/`, `derived/`, `changes/`, `views/`, `attestations/`. A line name may not start with
  them. Workstream core owns the resolver; others call `revs::resolve`.
- Attestation kinds (RFC 12.5): `check`, `approval`, `origin`, `import`, `quarantine`, `revoke`.
- State kinds (RFC 3.0): existing `commit`, `auto`, `stash`, `import`, plus `cache`, `derived`
  with `derived_from` and `context`.
- `ChangeId`: field `change_id` of the state record (RFC 3.1, 12.2 invariant 12).

## Workstreams

| Id | Branch | Owns | RFC |
|---|---|---|---|
| core | `cloud/core` | refs, attestations, ChangeId, state kinds, `change` commands, GC roots, `model.rs`, `db.rs`, `revs.rs`, new `cmd/refs.rs` | 3.0, 3.1, 6.9, 12.2, 12.5, 12.6 |
| merge | `cloud/merge` | `derive` rules, `branch --count`, conflict radar in `status`, fast `reset --hard` | 4.6, 4.8, 4.10 |
| remote-store | `cloud/remote-store` | store format v2, `file://` and `s3://`, slots, push/fetch/clone of stores, `fsck/log/gc --remote`, Desk v1 import | `docs/remote-design.md` |
| remote-cli | `cloud/remote-cli` | `remote add/list/remove/trust`, peer `ssh://` push/fetch/pull/sync, `layr git …`, removal of `machines` and of the old copy command, invariant tests | `docs/remote-design.md` |
| oci | `cloud/oci` | `layr.toml` runnables, deterministic OCI export, `oci://` remote push, registry in the service (`registry.listen`), Helm chart | 30, 31 |
| core-2 | `cloud/core-2` | protection rules for refs, merge queue | `docs/wave2-design.md` 1–2 |
| guard | `cloud/guard` | `bisect run`, secret detector | `docs/wave2-design.md` 3–4 |
| gateway, disk, infra | `cloud/gateway`, `cloud/disk`, `cloud/infra` | Layr Cloud V1 | `docs/cloud-design.md` |
| later | – | probes and behavior diff (RFC 29, Change instead of PR) | 29 |

Wave 1: core, merge, remote-store, remote-cli, oci. Wave 2 starts from their results.
