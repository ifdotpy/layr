#!/bin/bash
# Line operations on three Linux backends with the same tree (docs/layrfs-fuse.md):
#   fuse    the FUSE view (`layr fs view`), served by the user `layr` (no root at run time)
#   btrfs   a writable snapshot of a base subvolume; save = read-only snapshot + btrfs send --no-data
#   overlay an OverlayFS mount on XFS; save = a scan of the upper folder
# Every save writes the same v2 objects (layr-objects), so the tree ids of equal edits must be equal.
# usage: bench.sh <tree name> <source folder> [build]
# Needs: B (bench folder, setup.sh done), LF (the bench helpers of this user), LAYR (layr built with
# the feature layrfs-fuse; setup.sh installs a copy for the user layr). ROUND names a repetition:
# its results go to results/<tree>-<round>.jsonl.
set -euo pipefail
: "${B:?}" "${LF:?}" "${LAYR:?}"
TREE=$1 SRC=$2 BUILD=${3:-}
LFL=/home/layr/layrfs-fuse
VL=/home/layr/layr
X=$B/xfs
me=$(id -u); mygid=$(id -g)
mkdir -p "$B/results" "$B/tmp"
R=$B/results/$TREE${ROUND:+-$ROUND}.jsonl
REPS=${REPS:-3}

now() { date +%s%N; }
# ONLY_BUILD=1 runs only the builds.
rec() { echo "{\"tree\":\"$TREE\",\"backend\":\"$1\",\"step\":\"$2\",\"ms\":$3${4:+,$4}}" >> "$R"; printf '%-8s %-8s %-22s %8s ms %s\n' "$TREE" "$1" "$2" "$3" "${4:-}"; }
timed() { local b=$1 s=$2; shift 2; local t0 t1; t0=$(now); "$@"; t1=$(now); rec "$b" "$s" $(( (t1 - t0) / 1000000 )); }

# ---- inputs ----------------------------------------------------------------------------------
LIST=$B/tmp/$TREE.files
(cd "$SRC" && find . -type f -not -path './.git/*' -size +0 -printf '%P\n' | LC_ALL=C sort) > "$LIST"
NFILES=$(wc -l < "$LIST")
head -n 10 "$LIST" > "$B/tmp/$TREE.edit10"
if [ "$NFILES" -ge 10010 ]; then
    sed -n '11,10010p' "$LIST" > "$B/tmp/$TREE.edit10k"
    KIND=modify
else
    KIND=create
fi
edit10() { while read -r f; do printf '// layrfs bench\n' >> "$1/$f"; done < "$B/tmp/$TREE.edit10"; }
edit10k() {
    if [ "$KIND" = modify ]; then
        while read -r f; do printf '// layrfs bench\n' >> "$1/$f"; done < "$B/tmp/$TREE.edit10k"
    else
        local d i j
        for i in $(seq -w 0 99); do
            d=$1/bench-new/d$i; mkdir -p "$d"
            for j in $(seq -w 0 99); do printf 'file %s %s\n' "$i" "$j" > "$d/f$j.txt"; done
        done
    fi
}

# ---- the base state in each store --------------------------------------------------------------
if [ ! -e "$B/tmp/$TREE.prepared" ]; then
    t0=$(now); sudo -u layr "$LFL" import "$SRC" /home/layr/store/objects "$TREE-base" > "$B/tmp/$TREE.import.fuse"; t1=$(now)
    rec fuse import $(( (t1 - t0) / 1000000 ))
    # Btrfs: a base subvolume with a checkout of the state, and its read-only snapshot (the send parent).
    "$LF" import "$SRC" "$B/btrfs/objects" "$TREE-base" > "$B/tmp/$TREE.import.btrfs"
    sudo btrfs subvolume create "$B/btrfs/$TREE-base" >/dev/null
    sudo chown "$me:$mygid" "$B/btrfs/$TREE-base"
    "$LF" checkout "$B/btrfs/objects" "$TREE-base" "$B/btrfs/$TREE-base" >/dev/null
    sudo btrfs subvolume snapshot -r "$B/btrfs/$TREE-base" "$B/btrfs/states/$TREE-base" >/dev/null
    # OverlayFS: a lower folder on XFS.
    "$LF" import "$SRC" "$X/objects" "$TREE-base" > "$B/tmp/$TREE.import.overlay"
    "$LF" checkout "$X/objects" "$TREE-base" "$X/$TREE-lower" >/dev/null
    touch "$B/tmp/$TREE.prepared"
fi
for b in fuse btrfs overlay; do grep -o '"tree":"[0-9a-f]*"' "$B/tmp/$TREE.import.$b"; done | sort -u | wc -l | grep -qx 1 ||
    { echo "the three imports give different trees" >&2; exit 1; }

# ---- backends ----------------------------------------------------------------------------------
path_of() {
    case $1 in
        fuse) echo "/home/layr/lines/$2" ;;
        btrfs) echo "$B/btrfs/lines/$2" ;;
        overlay) echo "$X/lines/$2" ;;
    esac
}
create() {
    local b=$1 l=$2
    case $b in
        fuse)
            # One command as the service user: it makes the mount folder and forks the server.
            # The agent's cache folder is made outside the timed step (prep).
            sudo -u layr $VL fs view start --objects /home/layr/store/objects \
                --state $TREE-base --line /home/layr/store/lines/$l --mnt /home/layr/lines/$l \
                --sock /home/layr/run/$l.sock --cache target=$B/caches/$l/target --allow-other --allow $me --owner $me ${LAYRFS_EXTRA:-}
            ;;
        btrfs) sudo btrfs subvolume snapshot "$B/btrfs/$TREE-base" "$B/btrfs/lines/$l" >/dev/null ;;
        overlay)
            mkdir -p "$X/up/$l/upper" "$X/up/$l/work" "$X/lines/$l"
            sudo mount -t overlay overlay -o "lowerdir=$X/$TREE-lower,upperdir=$X/up/$l/upper,workdir=$X/up/$l/work" "$X/lines/$l"
            ;;
    esac
}
delete() {
    local b=$1 l=$2
    case $b in
        fuse)
            # The server unmounts, removes its line folder and the mount folder, then answers.
            "$LAYR" fs view ctl "/home/layr/run/$l.sock" stop remove >/dev/null
            ;;
        btrfs) sudo btrfs subvolume delete "$B/btrfs/lines/$l" >/dev/null ;;
        overlay) sudo sh -c "umount $X/lines/$l && rm -rf $X/up/$l && rmdir $X/lines/$l" ;;
    esac
}
# save <backend> <line> <base state> <new state>: prints the json of the object write.
save() {
    local b=$1 l=$2 base=$3 new=$4 p
    case $b in
        fuse) "$LAYR" fs view ctl "/home/layr/run/$l.sock" save "$new" ;;
        btrfs)
            sudo btrfs subvolume snapshot -r "$B/btrfs/lines/$l" "$B/btrfs/states/$new" >/dev/null
            sudo btrfs send -q --no-data -p "$B/btrfs/states/$base" "$B/btrfs/states/$new" | btrfs receive --dump > "$B/tmp/$new.dump"
            # The paths of the stream that are not folders (a folder's own entry would re-read it all).
            p=$B/btrfs/states/$new
            awk '{print $2; for (i = 3; i <= NF; i++) if ($i ~ /^dest=/) print substr($i, 6)}' "$B/tmp/$new.dump" |
                sed 's|^\./||; s|^[^/]*/||' | grep -v '^o[0-9]*-[0-9]*-0$' | LC_ALL=C sort -u |
                while read -r f; do [ -d "$p/$f" ] || echo "$f"; done > "$B/tmp/$new.paths"
            "$LF" write-changed "$B/btrfs/objects" "$base" "$p" "$B/tmp/$new.paths" "$new"
            ;;
        overlay)
            (cd "$X/up/$l/upper" && find . -mindepth 1 -not -type d -printf '%P\n') > "$B/tmp/$new.paths"
            "$LF" write-changed "$X/objects" "$base" "$X/lines/$l" "$B/tmp/$new.paths" "$new"
            ;;
    esac
}
# The agent's native cache folder of a FUSE line: made before and removed after the timed steps.
prep() { [ "$1" = fuse ] && mkdir -p "$B/caches/$2/target" || true; }
unprep() { [ "$1" = fuse ] && rm -rf "$B/caches/$2" || true; }
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
rep_timed() { # rep_timed <backend> <step> cmd...: the median of REPS runs
    local b=$1 s=$2 i t0 t1; shift 2
    # The command's output must not reach the pipe of the timings.
    for i in $(seq "$REPS"); do t0=$(now); "$@" >/dev/null; t1=$(now); echo $(( (t1 - t0) / 1000000 )); done | median | { read -r m; rec "$b" "$s" "$m"; }
}

for b in ${BACKENDS:-fuse btrfs overlay}; do
  if [ -z "${ONLY_BUILD:-}" ]; then
    # Empty lines: create and delete.
    c=(); d=()
    for i in $(seq "$REPS"); do
        l=$TREE-c$i-$$
        prep "$b" "$l"
        t0=$(now); create "$b" "$l"; t1=$(now); c+=($(( (t1 - t0) / 1000000 )))
        t0=$(now); delete "$b" "$l"; t1=$(now); d+=($(( (t1 - t0) / 1000000 )))
        unprep "$b" "$l"
    done
    rec "$b" "line new" "$(printf '%s\n' "${c[@]}" | median)"
    rec "$b" "line delete (clean)" "$(printf '%s\n' "${d[@]}" | median)"

    l=$TREE-main-$$
    prep "$b" "$l"
    create "$b" "$l"
    P=$(path_of "$b" "$l")
    # The index for `layr tool rg|find`: the view builds its own at the mount; on the native
    # backends the user's `layr fs serve` builds it.
    t0=$(now)
    if [ "$b" = fuse ]; then
        until "$LAYR" fs view ctl "/home/layr/run/$l.sock" stats | grep -q '"index_entries":[0-9]'; do sleep 0.02; done
    else
        "$LAYR" fs warm "$P" --wait >/dev/null
    fi
    t1=$(now); rec "$b" "tool index ready" $(( (t1 - t0) / 1000000 ))
    if [ -d "$SRC/.git" ]; then
        if [ "$b" = fuse ]; then
            # git with the view's fsmonitor hook and the untracked cache (given with -c: no change to .git/config).
            G=(git -c "core.fsmonitor=$LAYR fs view fsmonitor" -c core.untrackedCache=true -C "$P")
            timed "$b" "git status (cold)" "${G[@]}" status --porcelain >/dev/null
            rep_timed "$b" "git status (warm)" "${G[@]}" status --porcelain >/dev/null
            rep_timed "$b" "git status, no fsmonitor (warm)" git -c core.fsmonitor= -C "$P" status --porcelain >/dev/null
        else
            timed "$b" "git status (cold)" git -C "$P" status --porcelain >/dev/null
            rep_timed "$b" "git status (warm)" git -C "$P" status --porcelain >/dev/null
        fi
    fi
    timed "$b" "rg -l TODO (first)" sh -c "cd '$P' && rg -l TODO </dev/null >/dev/null || true"
    rep_timed "$b" "rg -l TODO (warm)" sh -c "cd '$P' && rg -l TODO </dev/null >/dev/null || true"
    rep_timed "$b" "layr tool rg -l TODO (warm)" sh -c "cd '$P' && { LAYR_FS_STRICT=1 '$LAYR' tool rg -l TODO </dev/null >/dev/null; [ \$? -le 1 ]; }"
    rep_timed "$b" "find -type f (warm)" sh -c "cd '$P' && find . -type f | wc -l >/dev/null"
    rep_timed "$b" "layr tool find -type f (warm)" sh -c "cd '$P' && LAYR_FS_STRICT=1 '$LAYR' tool find . -type f >/dev/null"
    timed "$b" "edit 10 files" edit10 "$P"
    t0=$(now); out=$(save "$b" "$l" "$TREE-base" "$TREE-$b-s10-$$"); t1=$(now)
    rec "$b" "save 10" $(( (t1 - t0) / 1000000 )) "$(echo "$out" | grep -o '"tree":"[0-9a-f]*"')"
    timed "$b" "edit 10000 files ($KIND)" edit10k "$P"
    t0=$(now); out=$(save "$b" "$l" "$TREE-$b-s10-$$" "$TREE-$b-s10k-$$"); t1=$(now)
    rec "$b" "save 10000" $(( (t1 - t0) / 1000000 )) "$(echo "$out" | grep -o '"tree":"[0-9a-f]*"')"
    # The user's index server may walk the line at this moment; an open folder makes umount fail.
    [ "$b" = fuse ] || "$LAYR" fs stop >/dev/null 2>&1 || true
    timed "$b" "line delete (10010 changes)" delete "$b" "$l"
    unprep "$b" "$l"
  fi

    if [ -n "$BUILD" ]; then
        l=$TREE-build-$$
        prep "$b" "$l"
        create "$b" "$l"
        P=$(path_of "$b" "$l")
        timed "$b" "cargo build (full)" sh -c "cd '$P' && cargo build --offline -q 2>/dev/null"
        touch "$P/src/main.rs"
        timed "$b" "cargo build (touch main.rs)" sh -c "cd '$P' && cargo build --offline -q 2>/dev/null"
        printf '\n// layrfs bench\n' >> "$P/src/views.rs"
        timed "$b" "cargo build (edit views.rs)" sh -c "cd '$P' && cargo build --offline -q 2>/dev/null"
        [ "$b" = fuse ] || "$LAYR" fs stop >/dev/null 2>&1 || true
        delete "$b" "$l"
        unprep "$b" "$l"
    fi
done
