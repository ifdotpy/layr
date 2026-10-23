import ctypes, os, sys, time, json, shutil, stat

libc = ctypes.CDLL(None, use_errno=True)
libc.clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint32]
libc.setxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
XATTR_NOFOLLOW = 1
UF_IMMUTABLE = 0x2
ROOT = sys.argv[1]
COPIES = 4


def free():
    os.sync()
    time.sleep(1.5)
    s = os.statvfs(ROOT)
    return s.f_bavail * s.f_frsize


def clone(a, b):
    if libc.clonefile(a.encode(), b.encode(), 0) != 0:
        raise OSError(ctypes.get_errno(), b)


def walk(root):
    for d, dirs, files in os.walk(root):
        yield d
        for f in files:
            yield os.path.join(d, f)


def setx(path, value=b"16777234:123456789:1700000000:123456789"):
    if libc.setxattr(path.encode(), b"com.layr.origin", value, len(value), 0, XATTR_NOFOLLOW) != 0:
        raise OSError(ctypes.get_errno(), path)


def fixture(path, size, dirs=100, per=100):
    os.mkdir(path)
    data = os.urandom(size) if size else b""
    for i in range(dirs):
        d = os.path.join(path, f"d{i:03}")
        os.mkdir(d)
        for j in range(per):
            with open(os.path.join(d, f"file-{j:04}.txt"), "wb") as f:
                f.write(data if not size else os.urandom(size))


def v_clone(src, dst):
    clone(src, dst)


def v_clone_xattr(src, dst):
    clone(src, dst)
    for p in walk(dst):
        setx(p)


def v_clone_seal(src, dst):
    clone(src, dst)
    for p in walk(dst):
        os.lchflags(p, UF_IMMUTABLE)


def v_hardlink(src, dst):
    os.mkdir(dst)
    for d in sorted(os.listdir(src)):
        os.mkdir(os.path.join(dst, d))
        for f in os.listdir(os.path.join(src, d)):
            os.link(os.path.join(src, d, f), os.path.join(dst, d, f))


def v_clone_files(src, dst):
    os.mkdir(dst)
    for d in sorted(os.listdir(src)):
        os.mkdir(os.path.join(dst, d))
        for f in os.listdir(os.path.join(src, d)):
            clone(os.path.join(src, d, f), os.path.join(dst, d, f))


VARIANTS = {"clone_dir": v_clone, "clone_dir+xattr": v_clone_xattr, "clone_dir+seal": v_clone_seal,
            "clone_per_file": v_clone_files, "hardlink_farm": v_hardlink}
out = {}
for size in (0, 4096):
    for name, fn in VARIANTS.items():
        base = os.path.join(ROOT, f"run-{size}-{name}")
        os.mkdir(base)
        src = os.path.join(base, "src")
        fixture(src, size)
        entries = sum(1 for _ in walk(src))
        f0 = free()
        times = []
        for c in range(COPIES):
            t = time.perf_counter()
            fn(src, os.path.join(base, f"c{c}"))
            times.append(time.perf_counter() - t)
        f1 = free()
        per = (f0 - f1) / COPIES
        out[f"{size}:{name}"] = {"entries": entries, "bytes_per_copy": per, "bytes_per_entry": per / entries,
                                 "seconds": sorted(times)[len(times) // 2]}
        print(size, name, f"{per/2**20:.2f} MiB/copy", f"{per/entries:.0f} B/entry", f"{out[f'{size}:{name}']['seconds']:.3f}s", flush=True)
        for p in walk(base):
            try:
                os.lchflags(p, 0)
            except OSError:
                pass
        shutil.rmtree(base)
json.dump(out, open(os.path.join(ROOT, "clone-cost.json"), "w"), indent=1)
