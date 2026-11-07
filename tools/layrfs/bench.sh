#!/bin/zsh
# LayrFS (FSKit passthrough) benchmark.
#
# Usage: bench.sh <source-dir> <mount-point> [rust-subdir]
#   source-dir   backing directory (for example a git checkout with a Cargo workspace)
#   mount-point  empty directory owned by you
#   rust-subdir  path of the Cargo project inside source-dir (default ".")
#
# Environment:
#   RUNS=1            cargo build repetitions per side
#   WORK=<dir>        results and CARGO_TARGET_DIR (default: $TMPDIR/layrfs-bench-<time>), must be on the native SSD
#   SKIP_CARGO=1      skip the cargo test
#   NEVER_INDEX=1     create <source-dir>/.metadata_never_index for the run (default 1; removed at the end)
#
# Each mount test runs on a fresh mount (cold FSKit caches), then once more (warm).
# The backing APFS caches are not purged (that needs sudo), so "cold" means cold FSKit and kernel
# vnode/name caches for the mount, not a cold disk.
# Requirements: the LayrFS extension is installed and enabled; run as a normal user (not root).

set -u
setopt pipefail

if [[ $# -lt 2 ]]; then
  sed -n '2,20p' "$0"; exit 2
fi
if [[ $(id -u) -eq 0 ]]; then
  echo "Do not run as root: FSKit mounts of third-party modules fail for root." >&2; exit 2
fi

SRC=${1:A}
MNT=${2:A}
RUST_SUB=${3:-.}
RUNS=${RUNS:-1}
HERE=${0:A:h}
WORK=${WORK:-${TMPDIR:-/tmp}/layrfs-bench-$(date +%Y%m%d-%H%M%S)}
CONTAINER_TMP=$HOME/Library/Containers/dev.layr.fshost.fs/Data/tmp
mkdir -p "$WORK" "$MNT"
MNT_REAL=$(cd "$MNT" && pwd -P)
WATCH=$WORK/fsevents-watch
TARGET=$WORK/target
typeset -A T   # results: T[name]=seconds

log() { print -r -- "[bench] $*" >&2 }

[[ -d $SRC ]] || { log "no source dir $SRC"; exit 2 }
if [[ -n $(ls -A "$MNT" 2>/dev/null) ]]; then log "mount point $MNT is not empty"; exit 2; fi

# ---------- helpers ----------

is_mounted() { mount | grep -qF " on $MNT_REAL (" }

ext_pid() { pgrep -nx LayrFS }

do_mount() {
  is_mounted && do_umount
  if ! mount -F -t layrfs "$SRC" "$MNT" 2>"$WORK/mount.err"; then
    log "mount -F -t layrfs <dir> failed ($(cat "$WORK/mount.err")); trying the file:// form"
    mount -t layrfs "file://$SRC" "$MNT" 2>>"$WORK/mount.err" || { log "mount failed: $(cat "$WORK/mount.err")"; exit 1 }
  fi
  for i in {1..50}; do is_mounted && break; sleep 0.1; done
  is_mounted || { log "mount did not appear"; exit 1 }
  mount | grep -F " on $MNT_REAL (" >> "$WORK/mounts.txt"
}

do_umount() {
  is_mounted || return 0
  umount "$MNT" 2>/dev/null || diskutil unmount "$MNT" >/dev/null 2>&1 || umount -f "$MNT"
  for i in {1..50}; do is_mounted || break; sleep 0.1; done
  sleep 0.5   # deactivate writes its stats dump
}

stats_reset() { local p=$(ext_pid); [[ -n $p ]] && kill -USR2 $p; sleep 0.2 }

stats_save() {   # stats_save <name>
  local p=$(ext_pid)
  [[ -n $p ]] && kill -USR1 $p
  sleep 0.4
  if [[ -r $CONTAINER_TMP/layrfs-stats-latest.json ]]; then
    cp "$CONTAINER_TMP/layrfs-stats-latest.json" "$WORK/stats-$1.json"
  else
    log "no stats file in $CONTAINER_TMP (Terminal may need access to app containers)"
  fi
}

timed() {   # timed <name> <dir> <command string>
  local name=$1 dir=$2 cmd=$3
  ( cd "$dir" && /usr/bin/time -p -o "$WORK/time-$name.txt" /bin/sh -c "$cmd" ) > "$WORK/out-$name.txt" 2> "$WORK/err-$name.txt"
  T[$name]=$(awk '/^real/ {print $2}' "$WORK/time-$name.txt")
  log "$name: ${T[$name]} s ($(head -c 80 "$WORK/out-$name.txt" | tr '\n' ' '))"
}

# One mount test: native (twice, report second), mount cold (fresh mount), mount warm.
run_test() {   # run_test <key> <command string>
  local key=$1 cmd=$2
  timed "$key-native-1" "$SRC" "$cmd"
  timed "$key-native" "$SRC" "$cmd"
  do_mount
  stats_reset
  timed "$key-cold" "$MNT" "$cmd"
  stats_save "$key-cold"
  stats_reset
  timed "$key-warm" "$MNT" "$cmd"
  stats_save "$key-warm"
}

cleanup() {
  do_umount
  [[ ${CREATED_NEVER_INDEX:-0} == 1 ]] && rm -f "$SRC/.metadata_never_index"
  rm -f "$SRC/.layrfs-fsevents-"*(N)
}
trap cleanup EXIT INT TERM

# ---------- setup ----------

log "work dir: $WORK"
xcrun swiftc -O -o "$WATCH" "$HERE/tools/fsevents-watch.swift" || { log "cannot build fsevents-watch"; exit 1 }
if [[ ${NEVER_INDEX:-1} == 1 && ! -e $SRC/.metadata_never_index ]]; then
  touch "$SRC/.metadata_never_index" && CREATED_NEVER_INDEX=1
fi
( cd "$SRC" && find . -type f -print0 > "$WORK/filelist0" )
NFILES=$(tr -cd '\0' < "$WORK/filelist0" | wc -c | tr -d ' ')
log "files in source: $NFILES"

# ---------- 1-2: metadata and read tests ----------

run_test find  'find . -type f | wc -l'
run_test stat  "xargs -0 stat -f '%z %m' < '$WORK/filelist0' > /dev/null"
run_test rg    'rg --no-messages -c fn . > /dev/null; true'

# ---------- 3: cargo build ----------

CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
if [[ ${SKIP_CARGO:-0} != 1 && -x $CARGO && -f $SRC/$RUST_SUB/Cargo.toml ]]; then
  log "cargo fetch (native)"
  ( cd "$SRC/$RUST_SUB" && "$CARGO" fetch ) > "$WORK/cargo-fetch.log" 2>&1 || log "cargo fetch failed, see $WORK/cargo-fetch.log"
  export CARGO_TARGET_DIR=$TARGET
  for r in $(seq 1 $RUNS); do
    rm -rf "$TARGET"
    timed "cargo-native-$r" "$SRC/$RUST_SUB" "'$CARGO' build --offline 2>&1 | tail -1"
    rm -rf "$TARGET"
    do_mount
    stats_reset
    timed "cargo-mount-$r" "$MNT/$RUST_SUB" "'$CARGO' build --offline 2>&1 | tail -1"
    stats_save "cargo-mount-$r"
  done
  T[cargo-native]=${T[cargo-native-$RUNS]}
  T[cargo-mount]=${T[cargo-mount-$RUNS]}
  unset CARGO_TARGET_DIR
else
  log "cargo test skipped (SKIP_CARGO, no cargo, or no $RUST_SUB/Cargo.toml)"
fi

# ---------- 4: FSEvents ----------

is_mounted || do_mount
typeset -A EV
ev_test() {   # ev_test <key> <watch-dir> <file-to-write>
  local out
  out=$("$WATCH" "$2" "$3" 2 2>&1)
  EV[$1]=$out
  log "fsevents $1: $out"
}
ev_test via-mount_watch-mount "$MNT" "$MNT/.layrfs-fsevents-1"
ev_test via-backing_watch-mount "$MNT" "$SRC/.layrfs-fsevents-2"
ev_test via-mount_watch-backing "$SRC" "$MNT/.layrfs-fsevents-3"
stats_save "fsevents"
rm -f "$SRC/.layrfs-fsevents-"*(N)

# ---------- 5: unmount and final stats ----------

stats_save "final-before-unmount"
do_umount
[[ -r $CONTAINER_TMP/layrfs-stats-latest.json ]] && cp "$CONTAINER_TMP/layrfs-stats-latest.json" "$WORK/stats-after-unmount.json"

# ---------- report ----------

ratio() { [[ -n ${1:-} && -n ${2:-} && $2 != 0 && $2 != 0.00 ]] && awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2fx", a / b }' || print -n "-" }

{
  print "LayrFS benchmark  $(date)"
  print "source: $SRC ($NFILES files)   mount: $MNT_REAL"
  print "stats JSON: $WORK/stats-*.json"
  print
  printf "%-10s %10s %12s %12s %12s\n" test native_s mount_cold_s mount_warm_s warm/native
  for k in find stat rg; do
    printf "%-10s %10s %12s %12s %12s\n" $k "${T[$k-native]:-}" "${T[$k-cold]:-}" "${T[$k-warm]:-}" "$(ratio "${T[$k-warm]:-}" "${T[$k-native]:-}")"
  done
  if [[ -n ${T[cargo-native]:-} ]]; then
    printf "%-10s %10s %12s %12s %12s\n" cargo "${T[cargo-native]}" "${T[cargo-mount]}" "-" "$(ratio "${T[cargo-mount]}" "${T[cargo-native]}")"
  fi
  print
  print "FSEvents (2 s timeout):"
  for k in via-mount_watch-mount via-backing_watch-mount via-mount_watch-backing; do
    printf "  %-26s %s\n" $k "${EV[$k]:-}"
  done
} | tee "$WORK/summary.txt"
