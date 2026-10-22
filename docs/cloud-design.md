# Layr Cloud V1: design decisions (lead)

Status: decided. No graphical UI. Layr Desk (`~/Projects/layr-desk`) is deleted after this lands:
everything it does except its web site moves here.

## What Layr Cloud is

Layr Cloud is not a second version control system. It is Layr deployed in the user's cloud:

| Part | What | Where it runs |
|---|---|---|
| Store | an S3 bucket per project with store format v2 (`docs/remote-format.md`) | AWS account of the project owner |
| Gateway | `layr gateway`: HTTPS access to stores for clients without AWS credentials; auth; immutable writes | small Linux host (Desk's `t4g.nano` model), tailnet only |
| Executor | an ordinary Layr service on a Linux host with btrfs, with a machine slot of the store: fetches, runs the merge queue (`queue.run`), checks, the registry (`registry.listen`), pushes results back | EC2 in the same account |
| Disk | `layr mount`: read-only WebDAV of the newest states of stores, decrypted on the user's machine | user's Mac or Linux |
| Infra | CloudFormation templates and a written runbook | `infra/aws/` in this repository |

All of it is the `layr` binary plus templates. One binary; the gateway and the disk do not need
btrfs, a project store or root.

## Decisions

1. **One binary.** `layr gateway --config <file>` and `layr mount` are commands of `layr`; they
   use `src/store_remote/` directly. No new crate, no workspace split now.
2. **Gateway backend for stores.** New store URL scheme `layr+https://<host>/<project>`:
   the same store format through the gateway's HTTP API instead of S3. `store_remote` gets a third
   backend next to `file://` and `s3://`. The gateway only forwards objects; it never decrypts.
3. **Gateway API** (JSON, under `/v1/p/<project>/`): `GET store.json`, `GET slots`,
   `GET challenge`, `POST token`, `GET objects?prefix=`, `GET objects/<name>` (with `Range`),
   `PUT objects/<name>` (create only: the gateway sends `If-None-Match: *` to S3 and returns 412
   when the object exists; it refuses any name outside `records/`, `states/`, `slots/`),
   `DELETE objects/<name>` only for `gc --remote` callers with the delete grant.
4. **Auth.**
   - Read: Desk's challenge: the gateway returns a random token encrypted to the store recipient;
     a client that opened a slot decrypts it and gets a bearer token valid for 10 minutes (an HMAC
     with a key that lives only in gateway memory). No user database.
   - Write: bearer tokens issued by `layr gateway token <project> --write [--until <dur>]` on the
     gateway host; the gateway keeps only SHA-256 hashes in its config. A write-only machine needs
     no slot (Desk's write-only property is kept).
   - Delete: a separate token kind `--gc`.
5. **AWS access of the gateway:** as Desk: instance role, STS assume of a per-project reader/writer
   role, no SDK, SigV4 from `store_remote`'s S3 code. Writer role: put only with the conditional
   header, no delete. GC role: delete only on `states/*` data parts.
6. **TLS and network:** as Desk: listen on the tailnet address only; certificate files or
   `tailscale cert`; `--tls none` only on loopback for tests.
7. **Executor:** no new code beyond what workstreams already build: a documented config profile
   (`remote add origin layr+https://…`, `queue.run main`, `registry.listen …`, a systemd timer
   for `fetch` and `push`), plus the infra template for a btrfs data volume.
8. **Disk:** `layr mount <remote> [--listen 127.0.0.1:8095]` serves read-only WebDAV (OPTIONS,
   PROPFIND, GET, HEAD with ranges) of the newest state of each line the remote has, decrypting
   locally with the machine slot or the paper key; cache by SHA-256 in the user cache folder.
   On macOS `layr mount --install` writes a launchd agent and mounts at `~/Layr` (Desk's
   `ws.layr.desk` behavior). Port from Desk `disk.rs`, `webdav.rs`, `keystore.rs`.
9. **Infra:** port Desk's `template.yaml` (bucket, versioning, writer and reader roles,
   conditional-put policy, multipart cleanup), `gateway.yaml`, `bootstrap.yaml`, `dns.yaml`, and
   add `executor.yaml` (EC2 Graviton, gp3 data volume formatted btrfs, IMDSv2, instance role with
   the store's reader/writer roles). Deployment: written `aws cloudformation deploy` commands in
   `infra/aws/README.md`; no Node.js scripts.
10. **Not in V1:** the web site, passkeys (they need a browser), multi-tenant hosted service,
    billing, per-layer keys (RFC 7.2), compute scheduler (RFC stage 3).

## Gateway configuration (contract between gateway and infra)

`/etc/layr-gateway/config.json`, written by the host boot script, read again when it changes:

```json
{
  "host": "desk.example.com",
  "listen": "<tailnet address>:443",
  "projects": {
    "<project id>": {
      "bucket": "…", "region": "eu-central-1", "prefix": "<project id>/",
      "readerRoleArn": "arn:…", "writerRoleArn": "arn:…", "gcRoleArn": "arn:…",
      "recipient": "age1…",
      "writeTokenHashes": ["<sha256 hex>"], "gcTokenHashes": ["<sha256 hex>"]
    }
  }
}
```

- Token fields hold SHA-256 hashes only, never tokens. `layr gateway token` prints a new token
  once and adds its hash to this file.
- TLS: systemd `LoadCredential=tls.crt` and `tls.key` (Desk's model), or `--tls tailscale`.
- Config keys of the Layr service used by the executor profile: `queue.run` (workstream core-2)
  and `registry.listen`, `registry.user` (workstream oci).

## Work split

| Task | Owner | Depends on |
|---|---|---|
| gateway: `layr gateway`, `layr+https://` backend in store_remote, tokens, tests with MinIO | new worker `gateway` | remote-store merged |
| disk: `layr mount` WebDAV, launchd install, tests | new worker `disk` | remote-store merged |
| infra: CloudFormation templates, runbook, cfn-lint, executor profile docs | new worker `infra` | none (can start now) |
