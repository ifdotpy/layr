#!/bin/bash
# Cold read latency and the cost of a save, by pack block size, on Chromium src.
set -u
B=$TMPDIR/ob-bin/objects-bench; R=$TMPDIR/ob-bin/objects-read; O=$TMPDIR/ob; C=$TMPDIR/layr-chromium/src
rm -rf "$O"; mkdir -p "$O"
guard() { local f; f=$(df -k "$O" | awk 'NR==2{print int($4/1048576)}'); echo "free ${f} GiB, load $(uptime | sed 's/.*averages: //')"; [ "$f" -ge 25 ] || { echo "STOP: under 25 GiB"; rm -rf "$O"; exit 1; }; }
f1=$(cd "$C" && git ls-files | awk 'NR==200000'); f10=$(cd "$C" && git ls-files | awk 'NR%50000==7' | head -10)
for block in 56 4 1; do
  S=$O/c; echo "--- block $block MiB"; guard
  out=$("$B" "$S" import "$C" cold --state head); ct=$(echo "$out" | awk '{print $2}'); cm=$(echo "$out" | awk '{print $4}')
  "$B" "$S" repack --block $((block << 20)) | tail -1
  echo "$ct" > "$O/t"; "$R" "$S" sample "$O/t" 100
  for n in 1 10; do [ $n = 1 ] && files=$f1 || files=$f10; ms=""; for r in 1 2 3 4 5; do x=$("$B" "$S" changed "$C" "$ct" "$cm" $files | grep -oE 'in [0-9.]+ ms' | awk '{print $2}'); ms="$ms $x"; done; echo "save of $n files, ms:$ms"; done
  "$R" "$S" phases "$ct" "$cm"
  rm -rf "$S"
done
guard; rm -rf "$O"; echo done
