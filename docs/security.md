# Security and storage

This page is part of the [Layr documentation](../README.md#documentation).

## Security model

- The service runs as root; every other user is a client (`SO_PEERCRED`). Clients cannot read
  `states/` or the database; they read history through layr.
- The socket (`/run/layr/layr.sock`, mode 0666 in a folder of mode 0755) is open to every local
  user: the service decides each request by the caller's uid, groups and role, so a reader runs
  every read command. A connection refused with "Operation not permitted" (EPERM) never comes
  from layr: a sandbox or seccomp filter of the calling process refused the `connect()` system
  call (for example a filter that denies `connect` for every socket family when network access is
  off). Such a process needs `connect()` to the socket allowed, or must run layr outside that
  sandbox; the client error says so.
- The git object store (`git/`) holds history too. Its folder alone decides who reads it: open
  to the users and groups with a role and the owners of lines (an access list, or `0755` when
  everyone may read). git writes below it with `core.sharedRepository=0644`, so a strict umask
  of the service (such as `0077`) does not close the objects that the `.git` folder of a line
  uses through `alternates`. A store of an earlier layr gets read access below its folder once:
  at the next new line or role change, or at the first periodic run after the service starts.
- A line belongs to its owner. All writes of the service into a line go through descriptors
  (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`); a link in a line never redirects a
  root write. New working folders are prepared in a private folder and appear when ready.
- Commands for users (git network access, `try`, checks, `sync`, merge drivers, `apply` in a
  line) run with that user's uid, groups and a clean environment: a command never runs as
  another user. Paths named by a user outside the store (export, remote copies, patch files) are accessed with the user's file permissions, and so
  is a file such a command leaves for the service (the result of a merge driver).
- Overlays (docs/overlays.md): an overlay is read only through its own project, by its readers;
  the working content of a line with overlays is open to its owner and to readers of all its
  overlays; automatic states leave overlay files out, so no state of the project holds one.
- Permissions (docs/design.md, section 3): roles per project and machine (`layr access`), deny
  rules that win over any role, owners of lines, and protection rules of line patterns
  (`layr protect`). A new project has one admin (its creator) and a protected `main`. Stores of
  older versions keep their `admin` (or `lead`) and `members` settings as roles until `access`
  is set. Root may do everything, and every record names who acted.
- Replication takes records only from trusted machines (`layr sync` trusts the peer the admin
  chose once its records passed the checks; `layr remote trust <name> <machine>`), checks the project id,
  signatures and sequence numbers, and refuses a changed copy of a known record. Roles, deny
  and protection rules, merge commands and remotes are per machine; other
  machines' records never change a local line.
- A btrfs stream from a peer passes a check on its way to `btrfs receive`, which
  would otherwise find snapshot and clone sources by UUID anywhere on the file system: one
  subvolume with the name and UUID its signed record names, a parent only from the same
  project, and clone sources limited to that parent, the new subvolume, or explicitly verified states of the same project.
  Extra sources require an exact stream UUID and transaction ID. Their file paths cannot traverse symbolic links. After the receive, the new subvolume must be a snapshot of exactly that parent.
- Files larger than 64 MiB count as binary for diff, grep, blame, rename detection and merge:
  the service never reads such a file into memory (`show` and merge drivers copy it in
  pieces). Patches, messages and records named by a user are limited in size too.
- The service limits running requests per user and in total, and drops a client that does not
  send its request within 10 seconds.

## Storage and consistency

```text
$LAYR_ROOT/machine/{id,name,key}       machine identity, Ed25519 key (0700)
$LAYR_ROOT/projects/<p>/
  states/<state-id>                    read-only snapshots (0700)
  lines/<line>                         working folders (0711; each line 0700, its owner)
  git/                                 git object store for the remote bridge
  meta/db/layr.sqlite                  signed records + derived tables (own subvolume, 0700)
  meta/stage, meta/scan, meta/work     staging lines and temporary snapshots
  meta/base/<line>@<key>               full tree of a line with overlays (derived)
  meta/overlays/<overlay>/             an overlay: the internal project <p>%<overlay>, same layout
```

An operation writes its snapshots first, then one SQLite transaction (synchronous=FULL) with
its record and the derived rows. After a crash only a snapshot without a record can exist;
`layr fsck --repair` deletes it. Derived rows keep the order key of their record, so a rebuild
(`layr reindex`) gives the same result as incremental imports.

