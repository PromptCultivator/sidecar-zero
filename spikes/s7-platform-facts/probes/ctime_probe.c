// S7 probe: does unlinking a file, or renaming another file over it, alter the change
// time of the file as seen through a descriptor that is still open?
//
// The supervisor keeps a model's descriptor open and compares the file's identity
// before loading from it again. The design removed the change time from that identity
// on the belief that publishing a new model (unlink, rename-over, or a Kubernetes
// symlink swap followed by cleanup) changes it. If that belief is wrong, the change
// time could go back in; if it is right, comparing it would refuse every rollback.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static void nap(void) { struct timespec t = {0, 30 * 1000 * 1000}; nanosleep(&t, NULL); }

static int make_file(const char *path, const char *text) {
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0) { perror(path); exit(2); }
    if (write(fd, text, strlen(text)) < 0) perror("write");
    fsync(fd);
    close(fd);
    return open(path, O_RDONLY);
}

static void report(const char *fs, const char *what, const struct stat *a, const struct stat *b) {
    int ctime_changed = a->st_ctim.tv_sec != b->st_ctim.tv_sec || a->st_ctim.tv_nsec != b->st_ctim.tv_nsec;
    int mtime_changed = a->st_mtim.tv_sec != b->st_mtim.tv_sec || a->st_mtim.tv_nsec != b->st_mtim.tv_nsec;
    printf("%-10s %-44s ctime %-9s mtime %-9s size %-9s inode %-5s nlink %lu->%lu\n", fs, what,
           ctime_changed ? "CHANGED" : "same", mtime_changed ? "CHANGED" : "same",
           a->st_size != b->st_size ? "CHANGED" : "same", a->st_ino != b->st_ino ? "DIFF" : "same",
           (unsigned long)a->st_nlink, (unsigned long)b->st_nlink);
}

static void run_dir(const char *dir) {
    char fs[64], p[512], q[512], d1[512], d2[512], link[512], tmp[512];
    struct stat a, b;
    snprintf(fs, sizeof fs, "%s", dir);

    // 1. Unlink while a descriptor is open.
    snprintf(p, sizeof p, "%s/model-1", dir);
    int fd = make_file(p, "old model bytes");
    fstat(fd, &a); nap();
    unlink(p);
    fstat(fd, &b);
    report(fs, "unlink while open", &a, &b);
    close(fd);

    // 2. Another file renamed over it: the usual "write temp, rename into place".
    snprintf(p, sizeof p, "%s/model-2", dir);
    snprintf(q, sizeof q, "%s/model-2.tmp", dir);
    fd = make_file(p, "old model bytes");
    close(make_file(q, "new model bytes, longer"));
    fstat(fd, &a); nap();
    rename(q, p);
    fstat(fd, &b);
    report(fs, "another file renamed over it", &a, &b);
    close(fd); unlink(p);

    // 3. The file itself renamed to another name (a link count that does not change).
    snprintf(p, sizeof p, "%s/model-3", dir);
    snprintf(q, sizeof q, "%s/model-3.bak", dir);
    fd = make_file(p, "old model bytes");
    fstat(fd, &a); nap();
    rename(p, q);
    fstat(fd, &b);
    report(fs, "file renamed to a new name", &a, &b);
    close(fd); unlink(q);

    // 4. Kubernetes volume style: a symlink is swapped to a new directory, then the old
    //    directory is removed.
    snprintf(d1, sizeof d1, "%s/..v1", dir);
    snprintf(d2, sizeof d2, "%s/..v2", dir);
    snprintf(link, sizeof link, "%s/..data", dir);
    snprintf(tmp, sizeof tmp, "%s/..data_tmp", dir);
    mkdir(d1, 0755); mkdir(d2, 0755);
    snprintf(p, sizeof p, "%s/model", d1);
    snprintf(q, sizeof q, "%s/model", d2);
    close(make_file(p, "old model bytes"));
    close(make_file(q, "new model bytes, longer"));
    unlink(link);
    if (symlink("..v1", link)) perror("symlink");
    char via[1024];
    snprintf(via, sizeof via, "%s/model", link);
    fd = open(via, O_RDONLY);
    fstat(fd, &a); nap();
    if (symlink("..v2", tmp)) perror("symlink");
    rename(tmp, link);
    fstat(fd, &b);
    report(fs, "symlink swapped to a new directory", &a, &b);
    nap();
    unlink(p); rmdir(d1);
    fstat(fd, &b);
    report(fs, "then the old directory removed", &a, &b);
    close(fd); unlink(q); rmdir(d2); unlink(link);

    // Controls: things that must change it, and the write the check exists to catch.
    snprintf(p, sizeof p, "%s/model-5", dir);
    fd = make_file(p, "old model bytes");
    fstat(fd, &a); nap();
    chmod(p, 0600);
    fstat(fd, &b);
    report(fs, "control: chmod", &a, &b);
    fstat(fd, &a); nap();
    int w = open(p, O_WRONLY);
    if (pwrite(w, "NEW", 3, 0) < 0) perror("pwrite");
    close(w);
    fstat(fd, &b);
    report(fs, "control: bytes overwritten in place", &a, &b);
    close(fd); unlink(p);
    printf("\n");
}

int main(int argc, char **argv) {
    printf("%-10s %-44s %s\n", "directory", "operation", "what the open descriptor then reports");
    for (int i = 1; i < argc; i++) run_dir(argv[i]);
    return 0;
}
