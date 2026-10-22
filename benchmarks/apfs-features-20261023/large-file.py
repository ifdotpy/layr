import os, resource, subprocess, sys, hashlib, json
root = sys.argv[1]; bins = {"before": sys.argv[2], "after": sys.argv[3]}
os.mkdir(root); src = os.path.join(root, "src"); os.makedirs(os.path.join(src, "d"))
for i in range(1000): open(os.path.join(src, "d", f"f{i}"), "wb").write(os.urandom(4096))
with open(os.path.join(src, "large.bin"), "wb") as f:
    for n in range(1024): f.write(hashlib.shake_256(f"l{n}".encode()).digest(1 << 20))
def run(b, env, cwd, *a):
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    p = subprocess.run([b, *a], cwd=cwd, env=env, capture_output=True, text=True); assert p.returncode == 0, p.stderr
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    return round((r1.ru_utime - r0.ru_utime) + (r1.ru_stime - r0.ru_stime), 3), p.stdout
out = {}
for who, b in bins.items():
    env = dict(os.environ, LAYR_ROOT=os.path.join(root, "store-" + who), LAYR_AUTHOR_NAME="B", LAYR_AUTHOR_EMAIL="b@example.invalid")
    for k in ["LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE", "LAYR_TRACE"]: env.pop(k, None)
    run(b, env, root, "init", "p", "--from", src, "-q")
    main = os.path.join(env["LAYR_ROOT"], "projects/p/lines/main"); run(b, env, main, "branch", "-q", "t")
    w = os.path.join(env["LAYR_ROOT"], "projects/p/lines/t")
    with open(os.path.join(w, "large.bin"), "r+b") as f: f.seek(512 << 20); f.write(b"x" * 4096)
    row = {}
    for op in [("status", "--porcelain"), ("diff", "--stat"), ("add", "-A"), ("commit", "-qm", "c"), ("diff", "--stat", "HEAD~1", "HEAD")]:
        row[" ".join(op)], text = run(b, env, w, *op)
    out[who] = row
print(json.dumps(out, indent=1))
subprocess.run(["chflags", "-R", "nouchg", root]); subprocess.run(["rm", "-rf", root])
