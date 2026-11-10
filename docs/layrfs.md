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

Each layr version has its own server and socket, `layrfs-<version>.sock` in
`~/Library/Caches/layr/` (macOS) or `$XDG_RUNTIME_DIR/layr/` (Linux; `/tmp/layr-<uid>/` without
it), so two layr builds on one machine do not stop each other. The socket has mode 0600, and the
server answers only its own user. The client talks only to its own user's server: the socket's
folder must belong to the user and not be writable by others, and the uid of the process behind
the socket (`getpeereid`, `SO_PEERCRED`) must be the user's. Otherwise another user could listen
first in `/tmp/layr-<uid>/` and print anything into an agent's context; the client then runs the
real tool. The server's log is `layrfs.log` next to the socket.

## What is served

A tree is the layr line around the working folder (a folder `<store>/projects/<project>/lines/<line>`,
or a line moved out of the store), or else the git work tree around it (the nearest folder with a
`.git` entry). rg itself applies `.gitignore` files only inside a git repository; the index uses
the same rules, so a line without `.git` is served the same way.

| Tool | Served | Output |
|---|---|---|
| `rg` | the pattern(s) with `-e`; paths that are folders rg walks or files; `--files`; `-i -s -S -F -w -x -v -c --count-matches -l --files-without-match -n -N -H -I -o -q -A -B -C -m`; `-t`, `-T`; negated globs `-g '!glob'`; `--sort path`, `--color never`, `--no-heading`, `--no-messages`, `--no-config`, `-j` | byte for byte the output of `rg --sort path` |
| `find` | start paths in the tree; `-type f\|d\|l`, `-name`, `-iname`, `-path`, `-ipath`, `-not`/`!`, `-a`, `-maxdepth`, `-mindepth`, `-print` | the lines of `find`, in sorted pre-order |

The real tool runs (a fallback) when:

- stdout is a terminal, or `LAYR_FS_OFF` is set;
- the folder is neither in a layr line nor in a git work tree;
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

The server watches each indexed tree: FSEvents with file events on macOS, inotify on Linux. A
creation, removal or rename refreshes that path. A changed `.gitignore`, `.ignore` or `.rgignore`
refreshes its folder; a changed `.git/info/exclude` or lost events rebuild the tree. A rebuild
walks the tree without holding the index, and queries use the old index until it is replaced.

Events can arrive after a query: FSEvents passes through `fseventsd`, and
`FSEventStreamFlushSync` does not wait for events that `fseventsd` has not received yet. So before
it answers, the server compares the modification time of every folder in the query's scope (and
of the ignore files that apply) with the index, in parallel, and applies what changed. A creation,
removal or rename changes its folder's time inside the system call, so every call that returned
before the query is seen, without writing anything into the tree (as the cookie files of other
watchers do). A folder changed within the last two seconds is listed again on each query, because
a coarse clock may give two changes the same time (git's rule for racy timestamps).

On Linux, inotify queues each event when the change happens, and the server drains the queue
before a query. Only folders that rg enters are watched, so ignored folders (`target/`,
`node_modules`, `.git`) do not count against `fs.inotify.max_user_watches`; `find`, which reaches
them, compares folder times as on macOS. A tree that still needs more watches than the limit is
not indexed, and its tools fall back.

`tests/t229-layrfs-tools.sh` writes a file and searches for it at once 1,000 times while another
process creates and removes files, with and without a watcher.

## Limits

- Memory: the index holds paths and kinds, about 200 bytes per entry (Chromium `src`, 516,265
  files: about 120 MiB). `LAYR_FS_IDLE` (default 1800 s) drops the index of an idle tree;
  `LAYR_FS_EXIT` (default 7200 s) stops an idle server.
- `find` prints entries in sorted order, not in directory order.
- The global git excludes file is read when the index is built; a later change of it needs
  `layr fs stop`.
- Each layr version starts its own server, which indexes the trees it is asked about.

## Measurements

Release build on a Mac (M5 Max, a loaded host: load average 47 to 106), Chromium `src` with
516,265 files, median of five runs. The native runs had a warm page cache. The index was built in
2.3 s; the server used 156 MB.

| Command | Native folder | LayrFS |
|---|---|---|
| `find . -type f` | 1.31 s | 0.23 s |
| `find . -name '*.py'` | 1.97 s | 0.22 s |
| `rg --files` | 1.86 s | 0.45 s |
| `rg --files -t py` | 1.49 s | 0.13 s |
| `rg -n TODO base` | 0.19 s | 0.15 s |
| `rg -l kMaxLength` | 22.8 s | 16.5 s |

The output of `rg -l` and `rg --files` was byte-identical to `rg --sort path`. Content search is
bound by reading the files, so it gains least. With a cold page cache, the native `find` took 15 s.
