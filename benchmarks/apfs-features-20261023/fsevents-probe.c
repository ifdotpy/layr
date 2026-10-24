// Can a short-lived process read file events since a stored event ID (no daemon)?
#include <CoreServices/CoreServices.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int n = 0, done = 0, scan = 0;
static void cb(ConstFSEventStreamRef s, void *info, size_t count, void *paths, const FSEventStreamEventFlags flags[],
               const FSEventStreamEventId ids[]) {
    char **p = paths;
    for (size_t i = 0; i < count; i++) {
        if (flags[i] & kFSEventStreamEventFlagHistoryDone) { done = 1; continue; }
        if (flags[i] & (kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagUserDropped | kFSEventStreamEventFlagKernelDropped | kFSEventStreamEventFlagEventIdsWrapped)) scan = 1;
        n++;
        if (n <= 5) printf("  %s flags=%x id=%llu\n", p[i], flags[i], ids[i]);
    }
}

int main(int argc, char **argv) {
    if (argc == 2) { printf("%llu\n", FSEventsGetCurrentEventId()); return 0; }
    FSEventStreamEventId since = strtoull(argv[2], 0, 10);
    CFStringRef path = CFStringCreateWithCString(0, argv[1], kCFStringEncodingUTF8);
    CFArrayRef arr = CFArrayCreate(0, (const void **)&path, 1, 0);
    FSEventStreamRef s = FSEventStreamCreate(0, cb, 0, arr, since, 0, kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer);
    dispatch_queue_t q = dispatch_queue_create("ev", 0);
    FSEventStreamSetDispatchQueue(s, q);
    FSEventStreamStart(s);
    for (int i = 0; i < 200 && !done; i++) usleep(10000);
    FSEventStreamFlushSync(s);
    FSEventStreamStop(s);
    printf("events=%d history_done=%d must_scan=%d now=%llu\n", n, done, scan, FSEventsGetCurrentEventId());
    return 0;
}
