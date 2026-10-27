#!/bin/bash
# Run only in a disposable Linux environment without host mounts.
. "$(dirname "$0")/lib.sh"
chmod 755 "$T" "$LAYR_ROOT"
for u in layr-native-owner layr-native-other; do id "$u" >/dev/null 2>&1 || useradd -m "$u"; done
sample "$T/src"
chown -R layr-native-owner "$T/src"
l init native --from "$T/src" --owner layr-native-owner -q >/dev/null || exit 1
l --project native protect main --remove || exit 1
export LAYR_SOCKET=$T/native.sock
"$LAYR" daemon --socket "$LAYR_SOCKET" --tick 60 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'if [ "$FAIL" -ne 0 ]; then cat "$T/daemon.log"; fi; kill "$DPID" 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
as() { local user=$1; shift; runuser -u "$user" -- env LAYR_SOCKET="$LAYR_SOCKET" PATH=/usr/bin:/bin "$@"; }
ok "owner attaches through service" as layr-native-owner "$LAYR" workspace attach native --path "$T/src"
cd "$T/src" || exit 1
ok "owner reads the input folder through service" as layr-native-owner "$LAYR" status -s
as layr-native-owner sh -c 'printf "owner change\n" > owner.txt' || exit 1
ok "owner stages through service" as layr-native-owner "$LAYR" add owner.txt
ok "owner commits through service" as layr-native-owner "$LAYR" commit -qm owner
no "another user cannot read the project history" as layr-native-other env LAYR_PROJECT=native LAYR_LINE=main "$LAYR" log
no "owner cannot read private stored states" as layr-native-owner ls "$LAYR_ROOT/projects/native/states"
printf 'root secret\n' > locked
chmod 600 locked
no "capture does not read a root-only file" as layr-native-owner "$LAYR" status -s
rm locked
ok "capture works after unreadable file removal" as layr-native-owner "$LAYR" status -s
ok "service history passes verification" as layr-native-owner "$LAYR" fsck
sample "$T/studio-source"
chown -R layr-native-owner "$T/studio-source"
as layr-native-owner git -C "$T/studio-source" init -q || exit 1
as layr-native-owner git -C "$T/studio-source" add -A || exit 1
as layr-native-owner git -C "$T/studio-source" commit -qm original || exit 1
ok "Studio user imports through service" as layr-native-owner "$LAYR" init studio-native --from "$T/studio-source" -q
ok "Studio user removes main protection" as layr-native-owner "$LAYR" --project studio-native protect main --remove
ok "Studio user attaches the imported project" as layr-native-owner "$LAYR" workspace attach studio-native --path "$T/studio-source"
sample "$T/unreadable-source"
chown -R layr-native-owner "$T/unreadable-source"
printf 'root secret\n' > "$T/unreadable-source/secret"
chmod 600 "$T/unreadable-source/secret"
no "service import cannot read a root-only file" as layr-native-owner "$LAYR" init unreadable-native --from "$T/unreadable-source" -q
finish
