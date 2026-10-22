# Layr and Layr Cloud: implementation plan

Source of truth for decisions: `docs/cloud-rfc.md` (RFC 0.9). This file assigns the work.

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
| remote | `cloud/remote` | storage format, `remote add`, `push`/`pull`/`fetch`/`clone` for `file://`, `ssh://`, `s3://`; `layr git …`; `sync`; `backup` removal after its checks | 6, 7, 12.3, 12.4, 13 (0B, 1A–1D) |
| oci | `cloud/oci` | `layr.toml` runnables, deterministic OCI export, `oci://` remote push, registry in the service (`registry.listen`), Helm chart | 30, 31 |
| later | – | secret detector (4.7), `bisect run` (4.9), branch from `attestations/<id>` (4.11), probes and behavior diff, Layr Cloud server | 4.7, 4.9, 4.11, 29, Cloud |

Wave 1: core, merge, remote, oci. Wave 2 starts from their results.
