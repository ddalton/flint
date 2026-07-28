/* F52 reproducer: prove that open_by_handle_at() on a cold dcache
 * (freshly mounted fs) returns a DISCONNECTED dentry whose
 * /proc/self/fd readlink is "/" — the path KernelFh::resolve trusts.
 *
 *   fhtest mint <file> <handle-out>        name_to_handle_at -> hex file
 *   fhtest resolve <mountdir> <handle-in>  open_by_handle_at + readlink
 *
 * resolve also fstats the fd and re-reads content via /proc/self/fd
 * reopen, to show the FD ITSELF is correct even when the path is junk.
 * Raw syscalls: musl has no libc wrappers for these.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/syscall.h>

#ifndef O_PATH
#define O_PATH 010000000
#endif

struct fhbuf {
    unsigned int handle_bytes;
    int handle_type;
    unsigned char f_handle[128];
};

static int mint(const char *file, const char *out) {
    struct fhbuf fh; memset(&fh, 0, sizeof fh);
    fh.handle_bytes = 128;
    int mount_id = 0;
    if (syscall(SYS_name_to_handle_at, AT_FDCWD, file, &fh, &mount_id, 0)) {
        perror("name_to_handle_at"); return 1;
    }
    FILE *f = fopen(out, "w");
    fprintf(f, "%d %u ", fh.handle_type, fh.handle_bytes);
    for (unsigned i = 0; i < fh.handle_bytes; i++) fprintf(f, "%02x", fh.f_handle[i]);
    fprintf(f, "\n");
    fclose(f);
    printf("minted: type=%d bytes=%u\n", fh.handle_type, fh.handle_bytes);
    return 0;
}

static int resolve(const char *mountdir, const char *in) {
    struct fhbuf fh; memset(&fh, 0, sizeof fh);
    FILE *f = fopen(in, "r");
    if (!f) { perror("open handle file"); return 1; }
    if (fscanf(f, "%d %u ", &fh.handle_type, &fh.handle_bytes) != 2) { fprintf(stderr, "bad handle file\n"); return 1; }
    for (unsigned i = 0; i < fh.handle_bytes; i++) {
        unsigned b; if (fscanf(f, "%02x", &b) != 1) { fprintf(stderr, "bad hex\n"); return 1; }
        fh.f_handle[i] = (unsigned char)b;
    }
    fclose(f);

    /* O_RDONLY|O_DIRECTORY, NOT O_PATH — same as KernelFh::try_new */
    int mfd = open(mountdir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (mfd < 0) { perror("open mountdir"); return 1; }

    int fd = (int)syscall(SYS_open_by_handle_at, mfd, &fh, O_PATH | O_CLOEXEC);
    if (fd < 0) { printf("open_by_handle_at FAILED: %s\n", strerror(errno)); return 2; }

    char proc[64], path[4096];
    snprintf(proc, sizeof proc, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(proc, path, sizeof path - 1);
    if (n < 0) { perror("readlink"); return 1; }
    path[n] = 0;
    printf("readlink(/proc/self/fd) = \"%s\"\n", path);

    struct stat st;
    if (fstat(fd, &st) == 0)
        printf("fstat: ino=%llu size=%lld mode=%o  <- fd itself is CORRECT\n",
               (unsigned long long)st.st_ino, (long long)st.st_size, (unsigned)st.st_mode);

    /* the fix path: reopen through /proc/self/fd works regardless */
    int rfd = open(proc, O_RDONLY);
    if (rfd >= 0) {
        char buf[64]; ssize_t r = read(rfd, buf, sizeof buf - 1);
        if (r > 0) { buf[r] = 0; printf("reopen-via-procfd read: \"%s\"  <- fd-based I/O works\n", buf); }
        close(rfd);
    }
    close(fd); close(mfd);
    return 0;
}

/* The FIX's algorithm (mirrors KernelFh::resolve + IdentityResolver):
 * trust readlink only if it lies under the mount root AND lstats back
 * to the fd's (dev,ino); otherwise walk the export by inode identity. */
#include <dirent.h>

static int walk_find(const char *dir, dev_t dev, ino_t ino, char *out, size_t outsz) {
    DIR *d = opendir(dir);
    struct dirent *e; struct stat st; char p[4096];
    if (!d) return 0;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        snprintf(p, sizeof p, "%s/%s", dir, e->d_name);
        if (lstat(p, &st) || st.st_dev != dev) continue;
        if (st.st_ino == ino) { snprintf(out, outsz, "%s", p); closedir(d); return 1; }
        if (S_ISDIR(st.st_mode) && walk_find(p, dev, ino, out, outsz)) { closedir(d); return 1; }
    }
    closedir(d);
    return 0;
}

static int resolve_fixed(const char *mountdir, const char *in) {
    struct fhbuf fh; memset(&fh, 0, sizeof fh);
    FILE *f = fopen(in, "r");
    if (!f) { perror("open handle file"); return 1; }
    if (fscanf(f, "%d %u ", &fh.handle_type, &fh.handle_bytes) != 2) return 1;
    for (unsigned i = 0; i < fh.handle_bytes; i++) {
        unsigned b; if (fscanf(f, "%02x", &b) != 1) return 1;
        fh.f_handle[i] = (unsigned char)b;
    }
    fclose(f);

    int mfd = open(mountdir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (mfd < 0) { perror("open mountdir"); return 1; }
    int fd = (int)syscall(SYS_open_by_handle_at, mfd, &fh, O_PATH | O_CLOEXEC);
    if (fd < 0) { printf("FIXED: STALE (%s)\n", strerror(errno)); return 2; }

    struct stat fst;
    if (fstat(fd, &fst)) { perror("fstat"); return 1; }

    char proc[64], path[4096];
    snprintf(proc, sizeof proc, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(proc, path, sizeof path - 1);
    close(fd); close(mfd);
    if (n < 0) { perror("readlink"); return 1; }
    path[n] = 0;

    /* trust gate */
    struct stat vst;
    size_t rl = strlen(mountdir);
    if (!strncmp(path, mountdir, rl) && !lstat(path, &vst)
        && vst.st_dev == fst.st_dev && vst.st_ino == fst.st_ino) {
        printf("FIXED: trusted readlink -> \"%s\"\n", path);
        return 0;
    }
    /* identity recovery */
    char found[4096];
    if (walk_find(mountdir, fst.st_dev, fst.st_ino, found, sizeof found)) {
        printf("FIXED: readlink \"%s\" UNTRUSTED -> recovered by identity walk -> \"%s\"\n",
               path, found);
        return 0;
    }
    printf("FIXED: readlink \"%s\" UNTRUSTED and ino %llu not under export -> STALE\n",
           path, (unsigned long long)fst.st_ino);
    return 2;
}

int main(int argc, char **argv) {
    if (argc == 4 && !strcmp(argv[1], "mint"))          return mint(argv[2], argv[3]);
    if (argc == 4 && !strcmp(argv[1], "resolve"))       return resolve(argv[2], argv[3]);
    if (argc == 4 && !strcmp(argv[1], "resolve-fixed")) return resolve_fixed(argv[2], argv[3]);
    fprintf(stderr, "usage: fhtest mint|resolve|resolve-fixed ...\n");
    return 64;
}
