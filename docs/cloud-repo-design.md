# Layr and Layr Cloud repositories, and parity with Layr Desk (lead)

Status: decided 2026-10-23. Overrides `docs/cloud-design.md` decision 1 ("one binary").

## Repositories

| Repository | Holds | Built artifact |
|---|---|---|
| `ifdotpy/layr` | CLI and service; store client (`file://`, `s3://`, `layr+https://`); `layr mount`; OCI registry in the daemon; library crate `layr-store` | `layr` |
| `ifdotpy/layr-cloud` (renamed from `layr-desk`, history kept) | `layr-gateway` server and its token command; `infra/aws/` templates and runbook; host boot logic; `web/` as groundwork for the future UI | `layr-gateway` |

## The shared crate

`layr` becomes a Cargo workspace. New crate `crates/layr-store` (no dependency on the rest of
Layr): canonical JSON, store format v2 types and names, keys and slots, zstd frames, AWS
credentials and SigV4, the S3 client, and `http.rs` (bounded HTTP/1). `src/store_remote/` keeps
everything that needs a `Repo` or `Ctx` and uses `layr-store`. The `layr` binary behaves exactly
as before; all existing tests pass unchanged.

`layr-cloud` depends on `layr-store` by git with a pinned revision. Until the `layr` branches are
on GitHub, a path dependency on a sibling checkout (`../layr/crates/layr-store`) is allowed, as
one marked line in `Cargo.toml`.

## Moves

- `layr` → `layr-cloud`: `src/gateway/` (server, config, auth, TLS, its tests), `layr gateway`
  and `layr gateway token` become `layr-gateway serve` and `layr-gateway token`; `infra/aws/`
  with its Python tests; `tests/t27-gateway.sh` (rewritten to run `layr-gateway` from
  `layr-cloud` and `layr` from a given path).
- Removed from `layr-cloud`: `crates/desk-core`, `crates/layr-desk`, `python/`, `js/vault`.
  Kept: `web/` with a README line "groundwork for the Layr Cloud UI; reads Desk v1 and must be
  ported to `layr-store` v2 before use". `web/` is not built in CI until then.

## Parity with the current Layr Desk (origin/main 31610ce)

The earlier port read Desk at f4cb324. These later Desk features must exist in Layr Cloud:

1. **Recipient pin (security defect today).** `remote add` stores the store recipient and project
   id in the local remote config; every push, fetch, clone and mount compares them with
   `store.json` and refuses a difference, naming both values. A changed recipient needs an
   explicit `layr remote trust-store <name> --recipient <age1…>`.
2. **Desk v1 import of signed vaults.** The v1 reader verifies tree signatures as Desk does
   (`vault.json` `writer`, `signedFrom`, message `layr-desk tree v1` …) and refuses invalid ones;
   unsigned states before `signedFrom` import with an `import` attestation marked unsigned.
3. **Person keys.** One age identity per person opens every store the person may read:
   - store slot `slots/person-<login>.age` (vault identity sealed to the person recipient);
   - `layr person keys [--paper-key-out <path>]`, `layr person show`;
   - `layr remote grant <remote> --person <login> --recipient <age1…>` (needs the vault identity:
     machine slot, paper key or the owner's person slot);
   - the person's own openers: paper key (24 words) now; passkeys later with the web UI;
   - person index with pins (recipient, project), MAC as in Desk;
   - gateway records `people/<login>/` (person.json, slots, index, inbox of invites) and the
     `person.json`/index/inbox API in `layr-gateway`.
4. **Gateway hardening from Desk:** a store with person slots is visible only to logins that have
   one; any route of a hidden or unknown store answers 404; API writes from pages of other sites
   are refused (Origin/Sec-Fetch checks).

## Work split

| Task | Repository | Depends on |
|---|---|---|
| split: workspace, `layr-store` crate, move gateway and infra out, `layr` tests green | `layr` | none |
| cloud-repo: clean `layr-cloud`, add `layr-gateway` crate and infra, t27 green against `layr` | `layr-cloud` | split |
| pin + desk-sig: recipient pin and Desk v1 signature checks | `layr` | split |
| person: person keys, grant, index and pins in `layr`; people API in `layr-gateway` | both | split, cloud-repo |

## More Desk features for Layr Cloud (found 2026-10-24)

The `cloud-repo` port found these Desk features that Layr Cloud does not have yet. Desk is
removed, so each one becomes a Layr or Layr Cloud task after the person task:

5. **Keychain on macOS:** `layr mount` keeps the store identity in the Keychain; `unlock`,
   `lock` and `status` commands for it.
6. **One disk for all stores:** one Finder disk that shows every store the person may read,
   with an installer for the background service.
7. **AWS login:** use an AWS profile or SSO login for `s3://` stores, as Desk did.
8. **Deploy with one command:** one command that creates or updates the CloudFormation stacks
   of Layr Cloud from the release archives.
