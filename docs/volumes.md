# Volumes

A volume is a file system that holds projects. The store root is the volume `main`. Other
volumes let one store place projects on several disks, for example active projects on a fast
SSD and archives on a large HDD.

Part A (this document, `layr volume` and `init --volume`) places projects. Part B
(`layr project move`) moves a project between volumes. Part B is not built yet.

## Model

- A volume is a name and a path. It is machine configuration and never replicates.
- A project lives on one volume. Its folder is `<volume path>/projects/<project>`.
- A project without a record lives on `main`, so a store without volumes works as before.
- The default volume receives new projects when no `--volume` is given.
- A delete stays a rename: each volume has its own `trash/` on the same file system.
- Do not pool an SSD and an HDD into one volume (btrfs `device add`). btrfs places new chunks
  where the most space is free, with no tiering. Builds and saves then slow down unpredictably.
  Use one volume per disk and move projects with `project move` instead.

Files (never replicated):

| File | Content |
|---|---|
| `<root>/volumes.json` | `{format, default, volumes: {name: {path}}}`. `main` is not listed. |
| `<root>/project-volumes.json` | `{format, projects: {project: volume}}`. Absent entry means `main`. |
| `<volume>/layr-volume.json` | Identity: `{format, store, volume}`. `store` is the machine id. |
| `<volume>/projects/`, `<volume>/trash/` | Project folders and trash of the volume. |

All three files carry `format` (1). A reader refuses another format. The store format
(`store-version.json`) stays 1: a store without volume files opens without migration.

Writers hold `<root>/.volumes.lock` and publish with `fsutil::atomic_write`.

## Resolver

`volumes::Layout` is the only code that builds a project folder:

- `Layout::project_dir(name)`: folder of a project or an overlay (`<project>%<overlay>`).
  Refused with the volume name and path when the volume is not usable.
- `Layout::ready(volume)`: the volume path, when its identity file names this store and this volume.
- `Layout::projects_dirs()` and `ready_projects_dirs()`: for walks (gc, du, follow, daemon).
- `Layout::trash_of(volume)`: trash folder for deletes.
- `Store::project_dir`, `Store::project_names`, `Project::trash_dir`: wrappers for the store.

## Identity and missing volumes

- A volume is usable only when `<path>/layr-volume.json` exists and names this store and this volume.
- A missing identity file means: not mounted, or the folder is a plain folder. Layr never treats
  it as its own and never creates a layout over it.
- A volume that is not usable refuses its projects with the volume name and path.
- Other projects keep working.
- `gc` (check and `--repair`), `du`, the daemon and `layr follow` skip the projects of that volume.
  They delete nothing because of it. `gc --all-projects` prints the skipped projects.
- Store open does not fail for an unusable volume.

## Commands

- `layr volume add <name> <path>`: the path must exist. It must be empty, apart from hidden
  entries, `lost+found` and empty `projects`/`trash` folders. It must not overlap the store or
  another volume. It must pass the file system check of the build. Then it writes the identity
  file and creates `projects/` and `trash/`.
- `layr volume list [--json]`: name, file system, used and free bytes, projects, state.
- `layr volume remove <name>`: only when no project lives on it and it is not the default. Empties
  its trash when it is mounted. Its files stay on the disk.
- `layr volume default <name>`: the volume for new projects. Existing projects do not move.
- `layr init <project> --volume <name>`, `layr clone <url> [<project>] --volume <name>`,
  `layr git clone <url> [<project>] --volume <name>`: place a new project.
- `layr du --volumes [--json]`: project bytes and project count per volume.
- `layr du --store` lists the projects and trash of every usable volume.

## Checks per volume

The same checks as for the store root, once per volume path:

- btrfs (Linux): not in the root subvolume and not covered by a snapper config
  (`rootcheck::check_store`). Checked on `volume add` and on every store open.
- APFS (macOS): the volume must be APFS.
- OverlayFS build: ext4 or XFS.
- All volumes of a store use the backend of the build.

## Part B: project move (how it uses this)

Part B needs these from part A:

- `volumes::Layout` for the source and target folders, and `Layout::ready` for both volumes.
- `volumes::assign`-style write of `project-volumes.json` under `.volumes.lock`, as the last
  step of the move. The switch is one atomic write.
- The move closes the project for every writer, the daemon included: `layr follow`, the free
  queue, pins and `gc --repair`. A pass that starts during the copy is copied again before the
  switch, or the move refuses and retries.
- Absolute paths that follow the move: the moved-line index (`meta/line-paths.json` and the
  store index `line-paths.json`), the Git bridge store, `.git` folders of lines and cache overlay
  folders. The project file and the index are rewritten in the same switch.
- Free space: the target needs the project size after the copy (shared extents counted once),
  plus the source trash until its delete finishes.
- Tests: a move between two file systems runs in the Linux CI job (root, loop devices) and on
  two APFS images on macOS.
