#!/bin/bash
# Time the commit split, the assembly of a line, and the owner classification of a change list.
# Usage (root, after setup.sh): bench.sh <runs> <folder>...
# The folders become folder overlays (nested subvolumes). Output: TSV rows
# "experiment  size  run  step  microseconds" on stdout; summary.py prints the table.
# Every timed step ends with `btrfs filesystem sync`, so the time includes the transaction commit.
set -eu
R=${1:?usage: bench.sh <runs> <folder>...}
shift
W=${W:-/root/ovl-bench}
M=$W/mnt
B=$(cd "$(dirname "$0")" && pwd)
cd "$M" || exit 1

row() { printf '%s\t%s\t%s\t%s\t%s\n' "$@"; }
now() { T=${EPOCHREALTIME/./}; } # microseconds
sync_fs() { btrfs -q filesystem sync "$M"; }
# Reflink copies of the listed files (relative paths on stdin) from folder $1 into folder $2.
rcopy() { (cd "$1" || exit 1; xargs -d '\n' cp -a --reflink=always --parents -t "$2"); }
clean() { # delete the work subvolumes and wait for the cleaner
    local d
    for d in "$M"/work/*; do [ -e "$d" ] && btrfs -q subvolume delete -R "$d"; done
    btrfs -q subvolume sync "$M"
}
dest() { if [ "$3" = 1 ]; then echo "states/$1-$2"; else echo "work/$1-$2-$3"; fi; }

# Reference: today's commit is one read-only snapshot; with one staging line per owner
# (shared set and three overlays) a commit is four.
for r in $(seq "$R"); do
    sync_fs; now; a=$T
    btrfs -q subvolume snapshot -r line "work/ref1-$r"
    sync_fs; now; row ref-1-snapshot 0 "$r" total $((T - a))
    sync_fs; now; a=$T
    for k in 1 2 3 4; do btrfs -q subvolume snapshot -r line "work/ref4-$r-$k"; done
    sync_fs; now; row ref-4-snapshots 0 "$r" total $((T - a))
done
clean

for n in 10 1000 10000; do
    list=$W/ovl-$n.txt
    # 1a. The shared state: snapshot the line, delete the overlay files, make it read-only.
    for r in $(seq "$R"); do
        d=$(dest shared "$n" "$r")
        sync_fs; now; a=$T
        btrfs -q subvolume snapshot line "$d"
        now; b=$T
        (cd "$d" || exit 1; xargs -d '\n' rm -f -- <"$list")
        now; c=$T
        btrfs property set -ts "$d" ro true
        sync_fs; now
        row split-shared "$n" "$r" snapshot $((b - a))
        row split-shared "$n" "$r" delete $((c - b))
        row split-shared "$n" "$r" ro+sync $((T - c))
        row split-shared "$n" "$r" total $((T - a))
    done
    # 1b. The overlay state: an empty subvolume plus reflink copies of its files.
    for r in $(seq "$R"); do
        d=$(dest ovl "$n" "$r")
        sync_fs; now; a=$T
        btrfs -q subvolume create "$d"
        now; b=$T
        rcopy line "$M/$d" <"$list"
        now; c=$T
        btrfs property set -ts "$d" ro true
        sync_fs; now
        row split-overlay "$n" "$r" create $((b - a))
        row split-overlay "$n" "$r" reflink $((c - b))
        row split-overlay "$n" "$r" ro+sync $((T - c))
        row split-overlay "$n" "$r" total $((T - a))
    done
    # 1c. The next overlay state from the previous one: a snapshot plus 10 changed files.
    for r in $(seq "$R"); do
        d=work/inc-$n-$r
        sync_fs; now; a=$T
        btrfs -q subvolume snapshot "states/ovl-$n" "$d"
        head -n 10 "$list" | rcopy line "$M/$d"
        btrfs property set -ts "$d" ro true
        sync_fs; now; row overlay-next-10 "$n" "$r" total $((T - a))
    done
    # 2. Assemble a line: a writable snapshot of the shared state plus reflink copies of the
    # overlay files.
    for r in $(seq "$R"); do
        d=work/asm-$n-$r
        sync_fs; now; a=$T
        btrfs -q subvolume snapshot "states/shared-$n" "$d"
        now; b=$T
        rcopy "states/ovl-$n" "$M/$d" <"$list"
        sync_fs; now
        row assemble "$n" "$r" snapshot $((b - a))
        row assemble "$n" "$r" reflink+sync $((T - b))
        row assemble "$n" "$r" total $((T - a))
    done
    clean
done

# 3. Folder overlays: the folder is a nested subvolume of the line. A snapshot of the line
# leaves it out, so the split and the assembly are snapshots only.
i=0
for f in "$@"; do
    i=$((i + 1))
    n=$(grep -c "^$f/" "$W/all.txt")
    btrfs -q subvolume snapshot line "nested-$i"
    mv "nested-$i/$f" "nested-$i/$f.old"
    btrfs -q subvolume create "nested-$i/$f"
    cp -a --reflink=always "nested-$i/$f.old/." "nested-$i/$f/"
    rm -rf "nested-$i/$f.old"
    for r in $(seq "$R"); do
        sync_fs; now; a=$T
        btrfs -q subvolume snapshot -r "nested-$i" "$(dest fshared "$i" "$r")"
        btrfs -q subvolume snapshot -r "nested-$i/$f" "$(dest fovl "$i" "$r")"
        sync_fs; now; row folder-split "$n" "$r" total $((T - a))
    done
    for r in $(seq "$R"); do
        d=work/fasm-$i-$r
        sync_fs; now; a=$T
        btrfs -q subvolume snapshot "states/fshared-$i" "$d"
        rmdir "$d/$f"
        btrfs -q subvolume snapshot "states/fovl-$i" "$d/$f"
        sync_fs; now; row folder-assemble "$n" "$r" total $((T - a))
    done
    btrfs -q subvolume delete -R "nested-$i"
    clean
done

# 4. Classify a change list by owner. A typical commit: 40 changed files, 5 new (2 of them
# match a routing pattern), 5 deleted, 2 moved. A large one: 1000 changed files.
make_commit() { # make_commit <name> <changed>
    local d=work/$1 i=0 p
    btrfs -q subvolume snapshot line "$d"
    while IFS= read -r p; do
        i=$((i + 1))
        if [ "$i" -le "$2" ]; then
            echo "changed line" >>"$d/$p"
        elif [ "$i" -le $(($2 + 5)) ]; then
            rm -f -- "$d/$p"
        elif [ "$i" -le $(($2 + 7)) ]; then
            mv -- "$d/$p" "$d/$p.moved"
        else
            echo new >"$(dirname "$d/$p")/new-$i.txt"
        fi
    done < <(shuf -n $(($2 + 10)) --random-source=<(yes "$1") "$W/all.txt")
    mkdir -p "$d/secrets"
    echo "KEY=1" >"$d/secrets/.env"
    echo key >"$d/secrets/deploy.key"
    btrfs -q subvolume snapshot -r "$d" "states/$1"
}
make_commit commit-52 40
make_commit commit-1000 1000
for c in commit-52 commit-1000; do
    for r in $(seq "$R"); do
        sync_fs; now; a=$T
        btrfs send -q --no-data -p states/full "states/$c" >"$W/$c.stream"
        now; row send-no-data "$c" "$r" total $((T - a))
    done
    btrfs receive --dump <"$W/$c.stream" >"$W/$c.dump"
    python3 "$B/classify.py" "$W/$c.dump" "$R" "$c" "states/$c" states/ovl-10 states/ovl-1000 states/ovl-10000
done
clean
