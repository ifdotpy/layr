// Probe APFS features: bulk attributes, physical extents of clones, document IDs.
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/attr.h>
#include <sys/clonefile.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

static double now(void) { struct timeval t; gettimeofday(&t, 0); return t.tv_sec + t.tv_usec / 1e6; }

// 1. getattrlistbulk with gen count and clone id on one folder.
static void bulk(const char *dir) {
    int fd = open(dir, O_RDONLY | O_DIRECTORY);
    struct attrlist al = {0};
    al.bitmapcount = ATTR_BIT_MAP_COUNT;
    al.commonattr = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_GEN_COUNT;
    al.forkattr = ATTR_CMNEXT_CLONEID;
    char buf[65536];
    int n, total = 0, withgen = 0, withclone = 0;
    while ((n = getattrlistbulk(fd, &al, buf, sizeof buf, FSOPT_PACK_INVAL_ATTRS | FSOPT_ATTR_CMN_EXTENDED)) > 0) {
        char *p = buf;
        for (int i = 0; i < n; i++) {
            uint32_t len = *(uint32_t *)p;
            char *q = p + 4;
            attribute_set_t ret = *(attribute_set_t *)q; q += sizeof(attribute_set_t);
            q += sizeof(attrreference_t); // name
            q += sizeof(fsobj_type_t);
            uint32_t gen = *(uint32_t *)q; q += 4;
            uint64_t clone; memcpy(&clone, q, 8);
            total++;
            if ((ret.commonattr & ATTR_CMN_GEN_COUNT) && gen) withgen++;
            if ((ret.forkattr & ATTR_CMNEXT_CLONEID) && clone) withclone++;
            p += len;
        }
    }
    if (n < 0) perror("getattrlistbulk");
    printf("bulk: entries=%d gen=%d clone=%d\n", total, withgen, withclone);
    close(fd);
}

// 3. Physical block of each 4 KiB block of a file.
static int64_t phys(int fd, off_t off) {
    struct log2phys l = {0};
    l.l2p_contigbytes = 4096;
    l.l2p_devoffset = off;
    if (fcntl(fd, F_LOG2PHYS_EXT, &l) < 0) return -errno;
    return l.l2p_devoffset;
}

static void extents(const char *dir) {
    char a[1024], b[1024];
    snprintf(a, sizeof a, "%s/big", dir);
    snprintf(b, sizeof b, "%s/big-clone", dir);
    int fd = open(a, O_CREAT | O_RDWR | O_TRUNC, 0644);
    char block[4096];
    for (int i = 0; i < 4096; i++) { memset(block, i & 255, sizeof block); write(fd, block, sizeof block); } // 16 MiB
    fsync(fd); close(fd);
    unlink(b);
    if (clonefile(a, b, 0)) { perror("clonefile"); return; }
    int fb = open(b, O_RDWR);
    memset(block, 'x', sizeof block);
    pwrite(fb, block, sizeof block, 4096 * 100);
    pwrite(fb, block, sizeof block, 4096 * 3000);
    fsync(fb);
    int fa = open(a, O_RDONLY);
    double t = now();
    int same = 0, diff = 0, err = 0, first = -1;
    for (off_t off = 0; off < 4096 * 4096; off += 4096) {
        int64_t pa = phys(fa, off), pb = phys(fb, off);
        if (pa < 0 || pb < 0) { err++; if (first < 0) first = (int)-pa; continue; }
        if (pa == pb) same++; else diff++;
    }
    printf("log2phys: same=%d differ=%d errors=%d (errno %d) in %.3f s\n", same, diff, err, first, now() - t);
    // The same file read in full for comparison.
    t = now();
    char x[4096], y[4096]; int d2 = 0;
    for (off_t off = 0; off < 4096 * 4096; off += 4096) { pread(fa, x, 4096, off); pread(fb, y, 4096, off); if (memcmp(x, y, 4096)) d2++; }
    printf("read compare: differ=%d in %.3f s\n", d2, now() - t);
    close(fa); close(fb);
}

// 7. Document ID: set UF_TRACKED, then clone, rename, and safe-save.
static uint32_t docid(const char *p) {
    struct attrlist al = {0};
    al.bitmapcount = ATTR_BIT_MAP_COUNT;
    al.commonattr = ATTR_CMN_DOCUMENT_ID;
    struct { uint32_t len; uint32_t id; } __attribute__((packed)) r;
    if (getattrlist(p, &al, &r, sizeof r, FSOPT_NOFOLLOW)) return 0;
    return r.id;
}

static void documents(const char *dir) {
    char a[1024], b[1024], c[1024], tmp[1024];
    snprintf(a, sizeof a, "%s/doc", dir); snprintf(b, sizeof b, "%s/doc-clone", dir);
    snprintf(c, sizeof c, "%s/doc-moved", dir); snprintf(tmp, sizeof tmp, "%s/doc-tmp", dir);
    int fd = open(a, O_CREAT | O_RDWR | O_TRUNC, 0644); write(fd, "v1", 2); close(fd);
    struct stat st; stat(a, &st);
    if (chflags(a, st.st_flags | UF_TRACKED)) perror("chflags");
    uint32_t id = docid(a);
    unlink(b);
    clonefile(a, b, 0);
    rename(a, c);
    printf("docid: original=%u clone=%u after-rename=%u\n", id, docid(b), docid(c));
    fd = open(tmp, O_CREAT | O_RDWR | O_TRUNC, 0644); write(fd, "v2", 2); close(fd);
    rename(tmp, c); // safe save
    printf("docid: after safe-save=%u\n", docid(c));
}

int main(int argc, char **argv) {
    bulk(argv[1]);
    extents(argv[2]);
    documents(argv[2]);
    return 0;
}
