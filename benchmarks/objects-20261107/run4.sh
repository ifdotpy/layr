#!/bin/bash
# Final store measurements with the default settings: block 4 MiB, lazy indexes, per-folder times.
set -u
B=$TMPDIR/ob-bin/objects-bench; R=$TMPDIR/ob-bin/objects-read; O=$TMPDIR/ob; C=$TMPDIR/layr-chromium/src
G=/Users/igor/Projects/layr-store/projects/layr/lines/main
rm -rf "$O"; mkdir -p "$O"
now() { perl -MTime::HiRes=time -e 'printf "%.3f", time'; }
since() { perl -e "printf '%.2f', $(now)-$1"; }
free() { df -k "$O" | awk 'NR==2{print int($4/1048576)}'; }
guard() { echo "free $(free) GiB, load $(uptime | sed 's/.*averages: //')"; [ "$(free)" -ge 25 ] || { echo "STOP: under 25 GiB"; git -C "$C" checkout -q HEAD -- . ; rm -rf "$O"; exit 1; }; }
echo "--- history of layr, 60 commits, with deltas"; guard
H=$O/h; prev=""; n=0; : > "$O/ht"; : > "$O/hm"; s=$(now)
for c in $(git -C "$G" log -60 --format=%H --reverse); do n=$((n+1)); rm -rf "$O/src"; mkdir -p "$O/src"; git -C "$G" archive "$c" | tar -x -C "$O/src"
  args=(); [ -n "$prev" ] && args=(--previous "$prev"); out=$("$B" "$H" import "$O/src" cold ${args[@]+"${args[@]}"} --state "s$(printf %03d $n)"); prev=$(echo "$out" | awk '{print $2}'); echo "$prev" >> "$O/ht"; echo "$out" | awk '{print $4}' >> "$O/hm"; done
echo "import $(since "$s") s"; "$B" "$H" stat | grep -E 'times|total bytes' | tail -4
s=$(now); "$B" "$H" repack | tail -1; echo "repack $(since "$s") s"; "$B" "$H" stat | tail -1
tot=0; worst=0; while read -r t m; do rm -rf "$O/co"; s=$(now); "$B" "$H" checkout "$t" "$O/co" --times "$m" >/dev/null; d=$(since "$s"); tot=$(perl -e "print $tot+$d"); worst=$(perl -e "print $d>$worst?$d:$worst"); done < <(paste "$O/ht" "$O/hm")
echo "checkout of 60 states: total $tot s, worst $worst s"; "$B" "$H" verify "$O/src" "$O/co" | tail -1
"$R" "$H" sample "$O/ht" 100; "$R" "$H" decode "$prev"; rm -rf "$O/co" "$O/src" "$H"
echo "--- Chromium"; guard; S=$O/c
s=$(now); out=$("$B" "$S" import "$C" cold --state head); ct=$(echo "$out" | awk '{print $2}'); cm=$(echo "$out" | awk '{print $4}'); echo "cold write $(since "$s") s"; echo "$out" | cut -c130-220
s=$(now); "$B" "$S" repack | tail -1; echo "repack $(since "$s") s"; "$B" "$S" stat | grep -E 'times|total bytes'; guard
echo "$ct" > "$O/t"; "$R" "$S" sample "$O/t" 100; "$R" "$S" decode "$ct"; "$R" "$S" phases "$ct" "$cm"
list=$(cd "$C" && git ls-files | awk 'NR%50000==7' | head -10); one=$(echo "$list" | head -1)
for k in 1 10; do [ $k = 1 ] && files=$one || files=$list; bt=$ct; bm=$cm; ms=""
  for r in 1 2 3 4 5 6; do for f in $files; do printf '// %s %s\n' "$k" "$r" >> "$C/$f"; done
    o=$("$B" "$S" changed "$C" "$bt" "$bm" $files); ms="$ms $(echo "$o" | grep -oE 'in [0-9.]+ ms' | awk '{print $2}')"; bt=$(echo "$o" | awk '{print $2}'); bm=$(echo "$o" | awk '{print $4}'); done
  echo "save of $k changed files, ms:$ms"; done
git -C "$C" checkout -q HEAD -- .
guard
if [ "$(free)" -ge 32 ]; then s=$(now); "$B" "$S" checkout "$ct" "$O/cco" --times "$cm" | tail -1 | cut -c1-140; echo "checkout from packs $(since "$s") s"; s=$(now); "$B" "$S" verify "$C" "$O/cco" --times | tail -2; echo "verify $(since "$s") s"; rm -rf "$O/cco"; else echo "checkout skipped: disk"; fi
rm -rf "$O"; guard; echo done
