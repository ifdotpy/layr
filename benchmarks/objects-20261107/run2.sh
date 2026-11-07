#!/bin/bash
# Store read/write measurements, second run: states named (path order in packs), fixed arguments.
set -u
B=$TMPDIR/ob-bin/objects-bench; R=$TMPDIR/ob-bin/objects-read
O=$TMPDIR/ob; rm -rf "$O"; mkdir -p "$O"
now() { perl -MTime::HiRes=time -e 'printf "%.3f", time'; }
since() { perl -e "printf '%.2f', $(now)-$1"; }
guard() { local f; f=$(df -k "$O" | awk 'NR==2{print int($4/1048576)}'); echo "free ${f} GiB, load $(uptime | sed 's/.*averages: //')"; [ "$f" -ge 25 ] || { echo "STOP: under 25 GiB"; rm -rf "$O"; exit 1; }; }
G=/Users/igor/Projects/layr-store/projects/layr/lines/main
history() { # history <label> <use-deltas>
  local H=$O/h-$1 prev="" n=0 t m args out
  : > "$O/htrees"; : > "$O/htimes"
  echo "--- history, $1"
  local s; s=$(now)
  for c in $(git -C "$G" log -60 --format=%H --reverse); do
    n=$((n+1)); rm -rf "$O/src"; mkdir -p "$O/src"; git -C "$G" archive "$c" | tar -x -C "$O/src"
    args=(); if [ "$2" = yes ] && [ -n "$prev" ]; then args=(--previous "$prev"); fi
    out=$("$B" "$H" import "$O/src" cold ${args[@]+"${args[@]}"} --state "s$(printf %03d $n)")
    prev=$(echo "$out" | awk '{print $2}'); echo "$prev" >> "$O/htrees"; echo "$out" | awk '{print $4}' >> "$O/htimes"
  done
  echo "import 60 states: $(since "$s") s"; "$B" "$H" stat | tail -1
  s=$(now); "$B" "$H" repack | tail -1; echo "repack $(since "$s") s"; "$B" "$H" stat | tail -1
  local tot=0 worst=0 d
  while read -r t m; do rm -rf "$O/co"; s=$(now); "$B" "$H" checkout "$t" "$O/co" --times "$m" >/dev/null; d=$(since "$s"); tot=$(perl -e "print $tot+$d"); worst=$(perl -e "print $d>$worst?$d:$worst"); done < <(paste "$O/htrees" "$O/htimes")
  echo "checkout of 60 states from packs: total $tot s, worst $worst s"
  rm -rf "$O/src"; mkdir -p "$O/src"; git -C "$G" archive "$c" | tar -x -C "$O/src"; "$B" "$H" verify "$O/src" "$O/co" | tail -1
  "$R" "$H" sample "$O/htrees" 300; "$R" "$H" decode "$prev"
  rm -rf "$O/co" "$O/src" "$H"
}
chromium() { # chromium <label> <block bytes or empty>
  local C=$TMPDIR/layr-chromium/src S=$O/c-$1 out ct cm s
  echo "--- Chromium, $1"; guard
  s=$(now); out=$("$B" "$S" import "$C" cold --state head); ct=$(echo "$out" | awk '{print $2}'); cm=$(echo "$out" | awk '{print $4}'); echo "cold write $(since "$s") s"
  s=$(now); if [ -n "$2" ]; then "$B" "$S" repack --block "$2" | tail -1; else "$B" "$S" repack | tail -1; fi; echo "repack $(since "$s") s"; guard
  s=$(now); "$B" "$S" checkout "$ct" "$O/cco" --times "$cm" | tail -1; echo "checkout from packs $(since "$s") s"
  s=$(now); "$B" "$S" verify "$C" "$O/cco" --times | tail -2; echo "verify $(since "$s") s"; rm -rf "$O/cco"
  echo "$ct" > "$O/ctrees"; "$R" "$S" sample "$O/ctrees" 300; s=$(now); "$R" "$S" decode "$ct"
  "$R" "$S" phases "$ct" "$cm"
  rm -rf "$S"; guard
}
guard


chromium block-56m ""
chromium block-4m 4194304
rm -rf "$O"; echo done
