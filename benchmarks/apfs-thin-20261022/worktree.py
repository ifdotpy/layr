#!/usr/bin/env python3
"""Compare git worktrees with Layr lines on one APFS volume: space and CPU.

Usage: worktree.py LAYR ROOT [LARGE_MIB]
Each case: 10,000 files of 4 KiB (and one large file if LARGE_MIB > 0), then 4 worktrees or
lines, 10 changed files in each, status, add, and commit. CPU is user + system time.
"""
import hashlib, json, os, resource, subprocess, sys, time

layr, root = sys.argv[1], sys.argv[2]
large = int(sys.argv[3]) if len(sys.argv) > 3 else 0
os.mkdir(root)


def free():
    os.sync()
    time.sleep(1)
    s = os.statvfs(root)
    return s.f_bavail * s.f_frsize


def run(cmd, cwd, env=None):
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    assert p.returncode == 0, (cmd, p.stdout, p.stderr)
    return (r1.ru_utime - r0.ru_utime) + (r1.ru_stime - r0.ru_stime)


def fixture(path):
    os.mkdir(path)
    for i in range(10000):
        d = os.path.join(path, f"d{i // 100:05}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i:07}"), "wb") as f:
            f.write(hashlib.shake_256(str(i).encode()).digest(4096))
    for n in range(large):
        with open(os.path.join(path, "large.bin"), "ab") as f:
            f.write(hashlib.shake_256(f"l{n}".encode()).digest(1 << 20))


def edit(work, k):
    for n in range(10):
        with open(os.path.join(work, f"d00000/f{n:07}"), "r+b") as f:
            f.write(bytes([k + 1]) * 4096)


out = {}
env = dict(os.environ, GIT_AUTHOR_NAME="B", GIT_AUTHOR_EMAIL="b@example.invalid",
           GIT_COMMITTER_NAME="B", GIT_COMMITTER_EMAIL="b@example.invalid")

# git: one repository, then 4 worktrees.
g = os.path.join(root, "git")
fixture(g)
run(["git", "init", "-q"], g, env)
f0 = free()
run(["git", "add", "-A"], g, env)
run(["git", "commit", "-qm", "init"], g, env)
f1 = free()
rows = []
for k in range(4):
    w = os.path.join(root, f"wt{k}")
    row = {"branch_cpu": run(["git", "worktree", "add", "-q", "-b", f"b{k}", w], g, env)}
    f2 = free()
    row["branch_mib"] = (f1 - f2) / 2**20
    edit(w, k)
    row["status_cpu"] = run(["git", "status", "--porcelain"], w, env)
    row["add_cpu"] = run(["git", "add", "-A"], w, env)
    row["commit_cpu"] = run(["git", "commit", "-qm", "c"], w, env)
    f1 = free()
    row["commit_mib"] = (f2 - f1) / 2**20
    rows.append(row)
out["git"] = {"repo_mib": (f0 - free()) / 2**20, "rows": rows}

# Layr: one project, then 4 lines.
src = os.path.join(root, "src")
fixture(src)
lenv = dict(os.environ, LAYR_ROOT=os.path.join(root, "store"), LAYR_AUTHOR_NAME="B", LAYR_AUTHOR_EMAIL="b@example.invalid")
for k in ["LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE", "LAYR_TRACE"]:
    lenv.pop(k, None)
f0 = free()
run([layr, "init", "p", "--from", src, "-q"], root, lenv)
f1 = free()
init_mib = (f0 - f1) / 2**20
main = os.path.join(root, "store/projects/p/lines/main")
rows = []
for k in range(4):
    row = {"branch_cpu": run([layr, 'line', 'new', "-q", f"b{k}"], main, lenv)}
    f2 = free()
    row["branch_mib"] = (f1 - f2) / 2**20
    w = os.path.join(root, f"store/projects/p/lines/b{k}")
    edit(w, k)
    row["status_cpu"] = run([layr, "status", "--porcelain"], w, lenv)
    row["add_cpu"] = run([layr, "add", "-A"], w, lenv)
    row["commit_cpu"] = run([layr, 'save', "-qm", "c"], w, lenv)
    f1 = free()
    row["commit_mib"] = (f2 - f1) / 2**20
    rows.append(row)
out["layr"] = {"init_mib": init_mib, "rows": rows}
json.dump(out, open(os.path.join(root, "worktree.json"), "w"), indent=1)
for who in ("git", "layr"):
    r = out[who]["rows"]
    med = lambda k: sorted(x[k] for x in r)[len(r) // 2]
    print(who, {k: round(med(k), 3) for k in r[0]})
print("git repository MiB", round(out["git"]["repo_mib"], 2), "layr init MiB", round(init_mib, 2))
