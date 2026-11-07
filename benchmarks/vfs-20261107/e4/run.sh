#!/bin/bash
S="$(cd "$(dirname "$0")" && pwd)"
export PATH="$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin:$PATH"
build() { local d=$S/$1; shift; rm -rf $d/target; (cd $d && env CARGO_TARGET_DIR=$d/target LINE_DIR=$d "$@" cargo build -q --offline 2>&1 | tail -3); }
variant() {
  local v=$1; shift
  build lineA "$@"; build lineB "$@"
  local same=0 diff=0 dl=""
  for f in $(cd $S/lineA/target/debug && find deps -type f \( -name '*.rlib' -o -name '*.rmeta' \)) app; do
    if cmp -s $S/lineA/target/debug/$f $S/lineB/target/debug/$f; then same=$((same+1)); else diff=$((diff+1)); dl="$dl $(basename $f)"; fi
  done
  local leaks=$(strings -a $S/lineA/target/debug/app | grep -c "$S/lineA")
  local oso=$(dsymutil -s $S/lineA/target/debug/app 2>/dev/null | grep -c "N_OSO.*$S/lineA")
  local dbg=no; (cd /tmp && lldb -b -o 'b libby::hello' -o 'run' -o 'frame info' $S/lineA/target/debug/app 2>&1 | grep -q 'lib.rs:1') && dbg=yes
  echo "$v: same=$same diff=$diff [$dl ] abs-path-strings=$leaks oso-abs=$oso lldb-line=$dbg"
}





variant V5-remap+oso+noincr RUSTC_WRAPPER=$S/wrap.sh OSO=1 CARGO_INCREMENTAL=0
variant V6-remap+oso+packed+noincr RUSTC_WRAPPER=$S/wrap.sh OSO=1 CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=packed CARGO_INCREMENTAL=0
variant V7-remap+noincr RUSTC_WRAPPER=$S/wrap.sh CARGO_INCREMENTAL=0
build lineA RUSTC_WRAPPER=$S/wrap.sh OSO=1
(cd $S/lineA && lldb -b -o 'b libby::hello' -o run -o 'frame info' target/debug/app 2>&1 | grep -E 'frame #0' | head -1)
