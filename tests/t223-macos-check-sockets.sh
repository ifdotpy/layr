#!/bin/bash
# ci: macos
# The macOS check sandbox permits private Unix sockets with a short temporary path.
. "$(dirname "$0")/lib.sh"
[ "$(uname -s)" = Darwin ] && [ "$(id -u)" != 0 ] || { echo "$(basename "$0"): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
cp tools/checks/environment.sh "$T/src/check-environment.sh"
mkdir -p "$T/src/fake-seed/cargo/registry" "$T/src/fake-seed/cargo/bin" "$T/src/fake-seed/rustup"
touch "$T/src/fake-seed/cargo/registry/keep" "$T/src/fake-seed/cargo/bin/keep" "$T/src/fake-seed/rustup/keep"
printf 'stub\n' > "$T/src/fake-seed/toolchain"
printf '/usr/bin/clang\n' > "$T/src/fake-seed/clang"
printf '/\n' > "$T/src/fake-seed/sdk"
printf '/Applications/Xcode.app/Contents/Developer/usr/bin/git\n' > "$T/src/fake-seed/git"
if [ -n "${LAYR_SANDBOX_TEST_BINARY:-}" ]; then
  test -x "$LAYR_SANDBOX_TEST_BINARY" || exit 1
  cp -p "$LAYR_SANDBOX_TEST_BINARY" "$T/src/unit-test-bin" || exit 1
fi
cat > "$T/src/socket-check.sh" <<'CHECK'
#!/bin/bash
set -eu
export LAYR_CHECK_SEED="$PWD/fake-seed"
. ./check-environment.sh
printf '%s\n' "$TMPDIR"
test "$TMPDIR" = "$LAYR_CHECK_TMPDIR"
/usr/bin/ruby --disable-gems -rsocket -e 'p=File.join(ENV.fetch("TMPDIR"),"check.sock"); s=UNIXServer.new(p); c=UNIXSocket.new(p); c.close; s.close'
if [ -x ./unit-test-bin ]; then
  ./unit-test-bin repo::cache_copy_tests::incomplete_cache_directories_are_private --exact
  ./unit-test-bin service::request_tests::client_trace_is_isolated_and_stdout_stays_json --exact
  ./unit-test-bin service::request_tests::incompatible_client_schemas_are_refused_before_opening_the_store --exact
  ./unit-test-bin service::request_tests::malformed_environment_is_an_error --exact
  ./unit-test-bin ctx::author_tests
  ./unit-test-bin store_remote::gateway::tests --skip epoch_slot_staging_uses_the_project_grant_before_a_new_proof_exists --skip missing_descriptor_keeps_optional_person_reads_and_hints_machine_grants
fi
CHECK
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l config check.socket 'bash socket-check.sh' >/dev/null
l config checks.changes '["socket"]' >/dev/null
l config --local checks.runner true >/dev/null
state=$(l log --json | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
l approve -q "$state" >/dev/null
out=$(l checks run --state "$state" --json 2>"$T/error"); code=$?
eq "the sandboxed check completes" 0 "$code"
check_exit=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["checks"][0]["exit"])' "$out" 2>/dev/null)
eq "the private Unix socket works" 0 "$check_exit"
log_state=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["checks"][0]["output_state"])' "$out" 2>/dev/null)
short=$(head -1 "$LAYR_ROOT/projects/p/states/$log_state/check.log" 2>/dev/null)
has "the check gets a short temporary path" "/tmp/lct-" "$short"
no "the temporary link is removed after the check" test -L "$short"
if [ -n "${LAYR_SANDBOX_TEST_BINARY:-}" ]; then
  ok "socket, author and gateway unit tests pass in the sandbox" python3 -c 'import pathlib,sys; s=pathlib.Path(sys.argv[1]).read_text(); assert s.count("test result: ok. 1 passed;")==4 and "test result: ok. 3 passed;" in s and "test result: ok. 18 passed;" in s' "$LAYR_ROOT/projects/p/states/$log_state/check.log"
fi
echo 'exit 17' >> socket-check.sh
l save -qam 'fail after socket' >/dev/null
state=$(l log --json | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
l approve -q "$state" >/dev/null
out=$(l checks run --state "$state" --json 2>"$T/error"); code=$?
eq "a failed check remains a failure" 1 "$code"
log_state=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["checks"][0]["output_state"])' "$out" 2>/dev/null)
short=$(head -1 "$LAYR_ROOT/projects/p/states/$log_state/check.log" 2>/dev/null)
has "the failed check used the short path" "/tmp/lct-" "$short"
no "the temporary link is removed after failure" test -L "$short"
finish
