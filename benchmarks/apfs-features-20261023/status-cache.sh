#!/bin/bash
# usage: watchbench.sh BINARY ROOT_ON_LOGGED_VOLUME FILES
B=$1; R=$2/wb-$(date +%s); N=$3; mkdir -p $R
python3 -I -c "
import os,sys
s,n=sys.argv[1],int(sys.argv[2])
for i in range(n):
  d=f'{s}/d{i//500:04}/e{i//50%10}'; os.makedirs(d,exist_ok=True); open(f'{d}/f{i:06}','wb').write(b'x'*100)
" $R/src $N
export LAYR_ROOT=$R/store LAYR_AUTHOR_NAME=B LAYR_AUTHOR_EMAIL=b@example.invalid; unset LAYR_SOCKET LAYR_PROJECT LAYR_LINE
cd $R && $B init p --from $R/src -q; cd $R/store/projects/p/lines/main && $B protect main --remove >/dev/null; $B branch -q t; cd ../t
$B status --porcelain >/dev/null   # first: full, saves the cache
cpu(){ /usr/bin/time -p "$@" 2>&1 >/dev/null | awk '/real/{r=$2}/user/{u=$2}/sys/{s=$2}END{printf "real %.2f cpu %.2f\n", r, u+s}'; }
for k in 1 2 3; do
  echo y$k > d0001/e1/f000560
  echo "fast:  $(cpu $B status --porcelain)  $(LAYR_TRACE=1 $B status --porcelain 2>&1 >/dev/null | grep -E "saved status used" | head -1)"
  echo z$k > d0002/e1/f001060
  echo "full:  $(LAYR_NO_STATUS_CACHE=1 cpu $B status --porcelain)"
done
