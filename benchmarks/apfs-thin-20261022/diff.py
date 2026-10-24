import json, os, resource, subprocess, sys, statistics, hashlib
base, cand, root = sys.argv[1], sys.argv[2], sys.argv[3]
os.mkdir(root); src = os.path.join(root, "src")
for i in range(10000):
    d = os.path.join(src, f"d{i//100:05}"); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, f"f{i:07}"), "wb").write(hashlib.shake_256(str(i).encode()).digest(4096))
def run(b, env, cwd, *args):
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    p = subprocess.run([b, *args], cwd=cwd, env=env, capture_output=True, text=True)
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    assert p.returncode == 0, p.stderr
    return (r1.ru_utime - r0.ru_utime) + (r1.ru_stime - r0.ru_stime), p.stdout
out = {}
for who, b in (("base", base), ("cand", cand)):
    store = os.path.join(root, f"store-{who}")
    env = dict(os.environ, LAYR_ROOT=store, LAYR_AUTHOR_NAME="B", LAYR_AUTHOR_EMAIL="b@example.invalid")
    for k in ["LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE", "LAYR_TRACE"]: env.pop(k, None)
    run(b, env, root, "init", "p", "--from", src, "-q")
    main = os.path.join(store, "projects/p/lines/main"); run(b, env, main, "branch", "-q", "t")
    work = os.path.join(store, "projects/p/lines/t")
    for k in range(10):
        open(os.path.join(work, f"d00000/f{k:07}"), "r+b").write(b"x" * 4096)
    times = []; texts = set()
    for _ in range(3):
        t, text = run(b, env, work, "diff", "--stat"); times.append(t); texts.add(text)
    out[who] = {"cpu": times, "median": statistics.median(times)}
    out[who + "_text"] = sorted(texts)
assert out["base_text"] == out["cand_text"], "diff output differs"
print(json.dumps({k: v for k, v in out.items() if not k.endswith("_text")}))
subprocess.run(["chflags", "-R", "nouchg", root]); subprocess.run(["rm", "-rf", root])
