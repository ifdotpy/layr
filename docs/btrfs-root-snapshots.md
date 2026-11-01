# Btrfs root snapshots

This page is part of the [Layr documentation](../README.md#documentation).

Layr keeps each line and each state as a btrfs subvolume under the store (default `/var/lib/layr`).
This page explains what goes wrong when the store sits inside the root subvolume, how layr detects
it, and how to put the store in its own subvolume.

## The problem

Some distributions snapshot `/` with snapper and restore a snapshot from the boot menu.
Omarchy does this with Limine and limine-snapper-sync. A restore replaces the root subvolume `@`.

btrfs snapshots are not recursive. After a rollback of `@`:

- The plain files of the store (records, references, folders that are not subvolumes) return to the
  snapshot time.
- Every line and state subvolume appears as an empty folder.
- The data of those subvolumes stays in the old `@`.

The store looks normal, but its lines and states are empty. Layr cannot repair this by itself.

## What layr checks

Layr checks at every store open (`init`, `daemon`, and every other command). The check is cheap:
it reads the subvolume id of a few folders and the snapper config files.

- The store root, the projects folder and each moved line folder (`layr line move`) must not be in
  the same subvolume as `/`. Layr compares the device and the subvolume id. The folder to compare
  with is `/` by default.
- No snapper config may cover them. A config covers the store when its `SUBVOLUME=` value is an
  ancestor of the store and is the same subvolume.

If a check fails, layr refuses the command and prints the reason and the fix.

Layr also checks at every project open:

- The working folder of each line of this machine must be a subvolume.
- The 8 newest local state folders must be subvolumes.

An empty plain folder in one of these places means the store was rolled back. Layr refuses every
command of that project and prints the recovery steps. Do not run `gc` or `save` on that store.
The override below does not skip this check.

Override the root subvolume and snapper checks:

- `--allow-root-snapshots` on `layr init` and `layr daemon`.
- `LAYR_ALLOW_ROOT_SNAPSHOTS=1` in the environment of any command.

The flag applies to the process that opens the store. A command that a user without root sends to
the service runs in the service process. For that command, start the service with
`layr daemon --allow-root-snapshots`.

On macOS (APFS) and with the OverlayFS build, these checks do nothing.

Test hooks: `LAYR_ROOT_SUBVOLUME_PATH` replaces the folder compared with the store (default `/`).
`LAYR_SNAPPER_CONFIGS` replaces the snapper config folder (default `/etc/snapper/configs`).

## Fix: a store in its own subvolume

Use a subvolume `@layr` at the top level of the root file system, and mount it at the store path.
Snapper and limine-snapper-sync snapshot `@` only, so they do not contain `@layr`.

Run these commands as root. Replace `<uuid>` with the UUID of the root file system:

```sh
findmnt -no UUID /                          # prints the UUID to use
mkdir -p /mnt/btrfs
mount -o subvolid=5 UUID=<uuid> /mnt/btrfs  # the top-level subvolume (id 5)
btrfs subvolume create /mnt/btrfs/@layr
umount /mnt/btrfs
```

Add a line to `/etc/fstab`. Copy the other options of your root line, for example `noatime`
or `compress=zstd:1`:

```fstab
UUID=<uuid> /var/lib/layr btrfs subvol=/@layr,<options> 0 0
```

Mount the store and check the subvolume:

```sh
mkdir -p /var/lib/layr
mount /var/lib/layr
findmnt /var/lib/layr                       # the SOURCE column shows subvol=/@layr
```

Then install the service unit (see the [README](../README.md#linux-service)). Its
`RequiresMountsFor=/var/lib/layr` line makes systemd wait for the mount before it starts layr.
Keep that line when the store is a separate mount.

### Existing data

Layr does not move an existing store. A plain copy flattens its subvolumes (`cp -a` copies the
contents of a subvolume as files), and `mv` fails across subvolumes. Set up `@layr` before the first
`layr init` when you can.

For a store that already has data, export each project with a layr store remote (`file:///` in
[remotes](remotes.md)) to a path on another subvolume, then clone it into the new store. Test the
clone on a copy before you remove the old data. This procedure is not tested by the automated
suite.

### Backups

A snapshot of `@` does not contain `@layr`. Keep a copy of the store outside the machine, for
example with a layr remote or a git push of each line.

## Recover a rolled back store

Layr prints these steps when it finds an emptied folder:

```sh
findmnt -no SOURCE /                        # the device of the root file system
mount -o subvolid=5 <device> /mnt           # the top-level subvolume
ls /mnt                                     # find the previous @ (for example @.bak or a snapper snapshot)
```

Copy the store folder back from the previous `@`. Then start layr again.
