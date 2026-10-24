import sys, os, time, shutil, json
sys.argv = sys.argv[:2]
exec(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "clone-cost.py")).read().split("VARIANTS =")[0])

def prep_x(src, sealed):
    for p in walk(src):
        setx(p)
    if sealed:
        for p in walk(src):
            os.lchflags(p, UF_IMMUTABLE)

def unseal(dst):
    for p in walk(dst):
        if os.lstat(p).st_flags & UF_IMMUTABLE:
            os.lchflags(p, 0)

def r_clone(src, dst): clone(src, dst)
def r_clone_rewrite(src, dst):
    clone(src, dst)
    for p in walk(dst): setx(p)
def r_sealed_unseal(src, dst):
    clone(src, dst); unseal(dst)
def r_sealed_unseal_rewrite(src, dst):
    clone(src, dst); unseal(dst)
    for p in walk(dst): setx(p)
def r_clone_seal(src, dst):
    clone(src, dst)
    for p in walk(dst): os.lchflags(p, UF_IMMUTABLE)

CASES = [("x:clone", False, r_clone), ("x:clone+rewrite", False, r_clone_rewrite),
         ("x:clone+seal", False, r_clone_seal),
         ("xsealed:clone", True, r_clone), ("xsealed:clone+unseal", True, r_sealed_unseal),
         ("xsealed:clone+unseal+rewrite", True, r_sealed_unseal_rewrite)]
out = {}
for size in (0, 4096):
    for name, sealed, fn in CASES:
        base = os.path.join(ROOT, f"r-{size}-{name.replace(':','_').replace('+','_')}")
        os.mkdir(base); src = os.path.join(base, "src")
        fixture(src, size); prep_x(src, sealed)
        entries = sum(1 for _ in walk(src))
        f0 = free(); times = []
        for c in range(COPIES):
            t = time.perf_counter(); fn(src, os.path.join(base, f"c{c}")); times.append(time.perf_counter() - t)
        per = (f0 - free()) / COPIES
        out[f"{size}:{name}"] = {"bytes_per_entry": per / entries, "seconds": sorted(times)[len(times)//2]}
        print(size, name, f"{per/2**20:.2f} MiB/copy", f"{per/entries:.0f} B/entry", f"{sorted(times)[len(times)//2]:.3f}s", flush=True)
        for p in walk(base):
            try: os.lchflags(p, 0)
            except OSError: pass
        shutil.rmtree(base)
json.dump(out, open(os.path.join(ROOT, "clone-cost-attributes.json"), "w"), indent=1)
