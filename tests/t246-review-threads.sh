#!/bin/bash
# ci: macos overlayfs
# Review of changes (docs/review.md, "Review threads, reactions and reviewers"): comment threads on a line range,
# replies, resolve and unresolve, reactions, and reviewer requests with their status from approvals and comments.
# Two machines then write at the same time; after a sync both show the same threads, reactions and reviewers.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
state_id() { l log "$1" -n 1 --json | json 'd["id"]'; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main"
l line new feature -q
cd "$P/feature"
printf 'one\ntwo\nthree\nfour\n' > feature.txt
l add feature.txt
l save -qm feature
V1=$(state_id head)
CHANGE=$(l change list --json | json 'next(c["change_id"] for c in d if c["message"] == "feature")')
echo five >> feature.txt
l save -q --amend -am feature
V2=$(state_id head)
eq "the amend is the second version" 2 "$(l change show "$CHANGE" --json | json 'len(d["versions"])')"

# 1. A thread on a line range of a version, replies, and the old positional form.
OUT=$(l comment "$CHANGE" -m 'Why four lines?' --file feature.txt --line 2-4 --state "$V1" --json)
C1=$(echo "$OUT" | json 'd["id"]')
eq "a range comment records its end line" "2 4" "$(echo "$OUT" | json '"%s %s" % (d["line"], d["line_end"])')"
C2=$(l comment "$CHANGE" 'Positional text still works.' --version v2 | awk '{print $2}')
no "a range that ends before it starts is refused" l comment "$CHANGE" -m x --file feature.txt --line 4-2
no "a line without a file is refused" l comment "$CHANGE" -m x --line 2
no "a state of another change is refused" l comment "$CHANGE" -m x --state "$(cd "$P/main" && state_id head)"
no "--state and --version together are refused" l comment "$CHANGE" -m x --state "$V1" --version v1
R1=$(l comment reply "$C1" -m 'To test the range.' | awk '{print $2}')
R2=$(l comment reply "$R1" 'A reply to a reply.' --json | json 'd["id"]')
SHOW=$(l change show "$CHANGE" --json)
eq "a reply to a reply joins the thread of the first comment" "$R1 $R2" \
    "$(echo "$SHOW" | json '" ".join(r["id"] for t in d["threads"] if t["id"] == "'"$C1"'" for r in t["replies"])')"
eq "the thread names its version, file and range" "v1 feature.txt 2 4 False" \
    "$(echo "$SHOW" | json '" ".join(str(x) for t in d["threads"] if t["id"] == "'"$C1"'" for x in (t["version_label"], t["path"], t["line"], t["line_end"], t["resolved"]))')"
eq "each comment that replies to none starts one thread" 2 "$(echo "$SHOW" | json 'len(d["threads"])')"
eq "the flat comment list keeps every comment" 4 "$(echo "$SHOW" | json 'len(d["comments"])')"
eq "a reply has the version of its thread" "$V1" "$(echo "$SHOW" | json 'next(c["version"] for c in d["comments"] if c["id"] == "'"$R2"'")')"

# 2. Resolve and unresolve. A reply id names its thread. A repeated command writes no record.
resolves() { l op log --all --json | python3 -c 'import json,sys; print(sum(1 for line in sys.stdin if json.loads(line)["op"] in ("comment.resolve", "comment.unresolve")))'; }
has "resolve through a reply resolves the thread" "thread $C1 resolved" "$(l comment resolve "$R2")"
eq "change show lists the thread as resolved, with who resolved it" "True $(id -un)" \
    "$(l change show "$CHANGE" --json | json '" ".join(str(x) for t in d["threads"] if t["id"] == "'"$C1"'" for x in (t["resolved"], t["resolved_by"]["name"]))')"
has "a second resolve changes nothing" "already resolved" "$(l comment resolve "$C1")"
eq "and writes no record" 1 "$(resolves)"
has "unresolve opens the thread again" "thread $C1 open" "$(l comment unresolve "$C1")"
eq "the thread is open" "False" "$(l change show "$CHANGE" --json | json 'next(t["resolved"] for t in d["threads"] if t["id"] == "'"$C1"'")')"
no "an unknown comment is refused" l comment resolve 0123456789

# 3. Reactions: one per writer, target and name.
l react "$C1" +1 >/dev/null
has "the same reaction again changes nothing" "is already there" "$(l react "$C1" +1)"
l react "$CHANGE" heart >/dev/null
l react "$R1" eyes >/dev/null
SHOW=$(l change show "$CHANGE" --json)
eq "a comment shows its reaction count" 1 "$(echo "$SHOW" | json 'next(t["comment"]["reactions"]["+1"] for t in d["threads"] if t["id"] == "'"$C1"'")')"
eq "a reply shows its reactions" '{"eyes": 1}' "$(echo "$SHOW" | json 'json.dumps(next(r["reactions"] for t in d["threads"] for r in t["replies"] if r["id"] == "'"$R1"'"))')"
eq "the change shows its reactions" '{"heart": 1}' "$(echo "$SHOW" | json 'json.dumps(d["reactions"])')"
has "remove takes the reaction away" "removed" "$(l react "$R1" eyes --remove)"
eq "the reply has no reaction" '{}' "$(l change show "$CHANGE" --json | json 'json.dumps(next(r["reactions"] for t in d["threads"] for r in t["replies"] if r["id"] == "'"$R1"'"))')"
no "a bad reaction name is refused" l react "$C1" 'Thumbs Up'
no "an unknown target is refused" l react feedface +1

# 4. Reviewer requests. The caller has commented; an approval of the newest version marks the request done.
ME=user:$(id -u)
SELF=$(l remote self --json | json 'd["id"]')
l review request "$CHANGE" "$(id -un)" person:ann@example.com "machine:$SELF" >/dev/null
has "a second request changes nothing" "is already requested" "$(l review request "$CHANGE" "$ME")"
no "a group is no reviewer" l review request "$CHANGE" group:0
no "an unknown machine name is refused" l review request "$CHANGE" machine:no-such-machine
reviewers() { l change show "$CHANGE" --json | json '" ".join(r["reviewer"] + "=" + r["status"] for r in d["reviewers"])'; }
eq "a reviewer who commented is listed as commented" "machine:$SELF=commented person:ann@example.com=pending $ME=commented" "$(reviewers)"
l approve -q "$V2"
eq "an approval of the newest version marks the request done" "machine:$SELF=approved person:ann@example.com=pending $ME=approved" "$(reviewers)"
echo six >> feature.txt
l save -q --amend -am feature
eq "a new version makes the approval outdated" "machine:$SELF=outdated person:ann@example.com=pending $ME=outdated" "$(reviewers)"
eq "the outdated approval names its version" v2 "$(l change show "$CHANGE" --json | json 'next(r["approved_version"] for r in d["reviewers"] if r["reviewer"] == "'"$ME"'")')"
has "cancel removes a request" "canceled" "$(l review cancel "$CHANGE" person:ann@example.com)"
has "a second cancel changes nothing" "was not requested" "$(l review cancel "$CHANGE" person:ann@example.com)"
eq "the canceled reviewer is not listed" "machine:$SELF=outdated $ME=outdated" "$(reviewers)"
TEXT=$(l change show "$CHANGE")
has "human change show lists threads" "thread $C1 open, v1 feature.txt:2-4" "$TEXT"
has "with reactions on the comment" "comment $C1 by $(id -un): Why four lines? [+1 1]" "$TEXT"
has "and reviewers" "$ME	outdated v2" "$TEXT"
has "and change reactions" "Reactions: heart 1" "$TEXT"
ok "undo skips review records" bash -c "! '$LAYR' undo -n 2>&1 | grep -q review"
has "review records verify" "signatures ok" "$(l fsck)"

# 5. Two machines write at the same time. Machine c is trusted and clones the project.
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
c=$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json)
l remote trust archive "$(echo "$c" | json 'd["id"]')" --public-key "$(echo "$c" | json 'd["public_key"]')" >/dev/null
l comment resolve "$C1" >/dev/null
l review request "$CHANGE" machine:"$(echo "$c" | json 'd["id"]')" >/dev/null
C_ID=machine:$(echo "$c" | json 'd["id"]')
l push archive -q >/dev/null 2>&1
env LAYR_ROOT="$T/c/store" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
cl() { (cd "$T/c/store/projects/p/lines/main" && env LAYR_ROOT="$T/c/store" "$LAYR" "$@"); }
eq "machine c sees the resolved thread" True "$(cl change show "$CHANGE" --json | json 'next(t["resolved"] for t in d["threads"] if t["id"] == "'"$C1"'")')"
# Thread: a reopens and resolves again, c reopens. Request: a cancels and requests again, c cancels.
l comment unresolve "$C1" >/dev/null; l comment resolve "$C1" >/dev/null
cl comment unresolve "$C1" >/dev/null
l review cancel "$CHANGE" "$C_ID" >/dev/null; l review request "$CHANGE" "$C_ID" >/dev/null
cl review cancel "$CHANGE" "$C_ID" >/dev/null
# Both add the same reaction; c also replies.
l react "$C2" +1 >/dev/null
cl react "$C2" +1 >/dev/null
cl comment reply "$C1" -m 'Seen on machine c.' >/dev/null
cl push origin -q >/dev/null 2>&1
l fetch archive -q >/dev/null 2>&1
l push archive -q >/dev/null 2>&1
cl fetch origin -q >/dev/null 2>&1
view() { json 'json.dumps({"threads":[(t["id"],t["resolved"],[r["id"] for r in t["replies"]],t["comment"]["reactions"]) for t in d["threads"]],"reactions":d["reactions"],"reviewers":sorted(r["reviewer"] for r in d["reviewers"])}, sort_keys=True)'; }
A=$(l change show "$CHANGE" --json | view)
C=$(cl change show "$CHANGE" --json | view)
eq "both machines show the same review after the sync" "$A" "$C"
eq "a resolve and an unresolve at the same time leave the thread open" False \
    "$(l change show "$CHANGE" --json | json 'next(t["resolved"] for t in d["threads"] if t["id"] == "'"$C1"'")')"
has "a request and a cancel at the same time keep the request" "$C_ID" "$(l change show "$CHANGE" --json | json '[r["reviewer"] for r in d["reviewers"]]')"
eq "a reaction of each machine counts once" 2 "$(l change show "$CHANGE" --json | json 'next(t["comment"]["reactions"]["+1"] for t in d["threads"] if t["id"] == "'"$C2"'")')"
eq "the reply of machine c is in the thread" 3 "$(l change show "$CHANGE" --json | json 'next(len(t["replies"]) for t in d["threads"] if t["id"] == "'"$C1"'")')"
# A record that saw both heads decides.
l comment resolve "$C1" >/dev/null
l push archive -q >/dev/null 2>&1
cl fetch origin -q >/dev/null 2>&1
eq "a resolve after the sync resolves the thread on both machines" "True True" \
    "$(l change show "$CHANGE" --json | json 'next(t["resolved"] for t in d["threads"] if t["id"] == "'"$C1"'")') $(cl change show "$CHANGE" --json | json 'next(t["resolved"] for t in d["threads"] if t["id"] == "'"$C1"'")')"
finish
