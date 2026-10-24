"""Paired CPU and space comparison of two Layr binaries on one APFS volume.
CPU is user+sys time of the command process, which is less sensitive to host load than wall time."""
import json, os, resource, subprocess, sys, time, statistics, hashlib

base, cand, root, trials = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]); files = int(sys.argv[5]) if len(sys.argv) > 5 else 10000
os.mkdir(root)
src = os.path.join(root, "src")
for i in range(files):
    d = os.path.join(src, f"d{i//100:05}")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"f{i:07}"), "wb") as f:
        f.write(hashlib.shake_256(str(i).encode()).digest(4096))


def free():
    os.sync(); time.sleep(1)
    s = os.statvfs(root)
    return s.f_bavail * s.f_frsize


def run(binary, env, cwd, *args):
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t = time.perf_counter()
    p = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True)
    wall = time.perf_counter() - t
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    if p.returncode:
        raise RuntimeError((args, p.stdout, p.stderr))
    return {"cpu": (r1.ru_utime - r0.ru_utime) + (r1.ru_stime - r0.ru_stime), "wall": wall}


results = []
names = {"base": base, "cand": cand}
for trial in range(trials):
    order = ["base", "cand"] if trial % 2 == 0 else ["cand", "base"]
    for who in order:
        store = os.path.join(root, f"store-{who}-{trial}")
        env = dict(os.environ, LAYR_ROOT=store, LAYR_AUTHOR_NAME="B", LAYR_AUTHOR_EMAIL="b@example.invalid")
        for k in ["LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE", "LAYR_TEST_DIRECT_MAIN", "LAYR_TRACE"]:
            env.pop(k, None)
        b = names[who]
        row = {"who": who, "trial": trial}
        f = free(); row["init"] = run(b, env, root, "init", "p", "--from", src, "-q"); row["init"]["bytes"] = f - (f := free())
        main = os.path.join(store, "projects/p/lines/main")
        row["branch"] = run(b, env, main, 'line', 'new', "-q", "b1"); row["branch"]["bytes"] = f - (f := free())
        work = os.path.join(store, "projects/p/lines/b1")
        for k in range(10):
            with open(os.path.join(work, f"d00000/f{k:07}"), "r+b") as out:
                out.write(bytes([trial + 1]) * 4096)
        row["save"] = run(b, env, work, "save", "-q"); row["save"]["bytes"] = f - (f := free())
        row["add"] = run(b, env, work, "add", "-A"); row["add"]["bytes"] = f - (f := free())
        row["commit"] = run(b, env, work, 'save', "-q", "-m", "c"); row["commit"]["bytes"] = f - (f := free())
        row["status"] = run(b, env, work, "status", "--porcelain")
        row["diff"] = run(b, env, work, "diff", "--stat", 'head~1')
        row["branch2"] = run(b, env, work, 'line', 'new', "-q", "b2"); row["branch2"]["bytes"] = f - (f := free())
        run(b, env, work, "fsck")
        results.append(row)
        print(json.dumps({k: (v if not isinstance(v, dict) else {x: round(y, 3) for x, y in v.items()}) for k, v in row.items()}), flush=True)
        subprocess.run(["chflags", "-R", "nouchg", store], check=True)
        subprocess.run(["rm", "-rf", store], check=True)

summary = {}
for op in ["init", "branch", "save", "add", "commit", "status", "diff", "branch2"]:
    for who in names:
        rows = [r[op] for r in results if r["who"] == who]
        summary[f"{op}:{who}"] = {k: statistics.median(r[k] for r in rows) for k in rows[0]}
json.dump({"results": results, "summary": summary}, open(os.path.join(root, "paired.json"), "w"), indent=1)
for op in ["init", "branch", "save", "add", "commit", "status", "diff", "branch2"]:
    a, c = summary[f"{op}:base"], summary[f"{op}:cand"]
    print(f"{op:8} cpu {a['cpu']:.3f} -> {c['cpu']:.3f}  wall {a['wall']:.3f} -> {c['wall']:.3f}  "
          + (f"MiB {a['bytes']/2**20:.2f} -> {c['bytes']/2**20:.2f}" if "bytes" in a else ""))
