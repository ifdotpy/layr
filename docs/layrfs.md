# LayrFS: tree tools from an in-memory index

Agents walk their working tree all the time: `rg`, `rg --files` and `find` make up about a fifth
of their shell calls. On a large tree each walk reads every folder again. LayrFS keeps the tree in
the memory of one server process per user and answers these tools from it. The answer is the same
as the real tool's; when it cannot be the same, the real tool runs.

This is the first part of the LayrFS design: the index and the tool substitution, over the normal
folder of a line. The virtual folder for agent lines (an FSKit or FUSE view of the same index,
with changes in memory) comes later and uses the same server.

## Use it

Put links named `rg` and `find` to the layr binary first on the agent's PATH:

```sh
mkdir -p ~/.local/layrfs-bin
ln -sf "$(command -v layr)" ~/.local/layrfs-bin/rg
ln -sf "$(command -v layr)" ~/.local/layrfs-bin/find
export PATH="$HOME/.local/layrfs-bin:$PATH"
```

`layr tool rg <args>` and `layr tool find <args>` do the same without links.

The first call in a tree starts the server (`layr fs serve`) and the build of the index, and runs
the real tool. Later calls are answered from the index. Build the index ahead of time with
`layr fs warm [<dir>] [--wait]`. `layr fs status` lists the indexed trees and their size;
`layr fs stop` stops the server.

The server listens on `~/Library/Caches/layr/layrfs.sock` (macOS) or
`$XDG_RUNTIME_DIR/layr/layrfs.sock` (Linux; `/tmp/layr-<uid>/` without it), mode 0600, and
answers only its own user. Its log is `layrfs.log` next to the socket.

## What is served

A tree is the git work tree around the working folder: the nearest folder with a `.git` entry.

| Tool | Served | Output |
|---|---|---|
| `rg` | the pattern(s) with `-e`; paths that are folders rg walks or files; `--files`; `-i -s -S -F -w -x -v -c --count-matches -l --files-without-match -n -N -H -I -o -q -A -B -C -m`; `-t`, `-T`; negated globs `-g '!glob'`; `--sort path`, `--color never`, `--no-heading`, `--no-messages`, `--no-config`, `-j` | byte for byte the output of `rg --sort path` |
| `find` | start paths in the tree; `-type f\|d\|l`, `-name`, `-iname`, `-path`, `-ipath`, `-not`/`!`, `-a`, `-maxdepth`, `-mindepth`, `-print` | the lines of `find`, in sorted pre-order |

The real tool runs (a fallback) when:

- stdout is a terminal, or `LAYR_FS_OFF` is set;
- the folder is not in a git work tree;
- a flag or expression is not in the table, for example `--hidden`, `-u`, a whitelist glob
  (`-g '*.rs'`, which overrides ignore rules), `-o` in `find`, `-exec`;
- `rg` has no path and its standard input is a file, a pipe or a socket (rg searches stdin then);
- a path is outside the tree, uses `..`, is absolute, is missing, is a link, or is a folder that
  rg does not walk (ignored or hidden) — rg would search it, the index does not list it;
- `RIPGREP_CONFIG_PATH` is set without `--no-config`;
- the server is starting or still building the index of this tree.

`LAYR_FS_STRICT=1` turns a fallback into exit code 125 with the reason on stderr (tests use it).
`LAYR_REAL_RG` and `LAYR_REAL_FIND` name the real tools; otherwise the first `rg` or `find` on
PATH that is not the layr binary runs.

## How it stays current

The server watches each indexed tree: FSEvents with file events on macOS, inotify with a watch per
folder on Linux. Before it answers, it waits until every change made before the call is applied
(`FSEventStreamFlushSync` on macOS; on Linux the kernel queues events when the change happens, and
the server drains the queue). An agent that writes a file and then searches finds it.

A creation, removal or rename refreshes that path. A changed `.gitignore`, `.ignore` or
`.rgignore` refreshes its folder; a changed `.git/info/exclude` or lost events rebuild the tree.
A file written below a folder that rg does not walk (`target/`) costs no visibility check.

On Linux, the number of folders is limited by `fs.inotify.max_user_watches`. A tree that needs
more is not indexed, and its tools fall back.

## Limits

- Memory: the index holds paths and kinds, about 200 bytes per entry (Chromium `src`, 516,265
  files: about 120 MiB). `LAYR_FS_IDLE` (default 1800 s) drops the index of an idle tree;
  `LAYR_FS_EXIT` (default 7200 s) stops an idle server.
- `find` prints entries in sorted order, not in directory order.
- The global git excludes file is read when the index is built; a later change of it needs
  `layr fs stop`.
- A client and a server of different layr builds do not talk: the server exits, the client falls
  back, and the next call starts the new server.

## Measurements

Prototype of this design on this Mac (M5 Max, a loaded host), median of three runs; raw data and
scripts are in `benchmarks/vfs-20261107/` of the line `vfs-bench`.

| Tree | Tool | Native folder | Index |
|---|---|---|---|
| layr-studio, 43,191 files | `rg -c fn` | 0.233 s | 0.160 s |
| | `rg --files` | 0.051 s | 0.033 s |
| | `find . -type f` | 2.535 s | 0.066 s |
| Chromium `src`, 516,265 files | `find . -type f` | 15.1 s | 0.40 s |
| | `rg --files` | 1.75 s | 0.17 s |
| | `rg -c TODO` | 22.9 s | 13.5 s |

Content search is bound by reading the files, so it gains least.
