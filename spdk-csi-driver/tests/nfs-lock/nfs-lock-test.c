/*
 * Simple NFS Lock Test
 *
 * Compile: gcc -o nfs-lock-test nfs-lock-test.c
 * Usage: nfs-lock-test <nfs-mounted-path>
 *
 * Tests basic file locking over NFS using fcntl().
 * This should be run from a REMOTE machine that has the NFS server mounted.
 */

#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <sys/types.h>
#include <sys/wait.h>

void test_exclusive_lock(const char *filepath) {
    int fd;
    struct flock lock;

    printf("\n=== Test 1: Exclusive Lock ===\n");

    // Open test file
    fd = open(filepath, O_RDWR | O_CREAT, 0644);
    if (fd < 0) {
        perror("open");
        exit(1);
    }

    // Set up exclusive write lock
    memset(&lock, 0, sizeof(lock));
    lock.l_type = F_WRLCK;    // Exclusive lock
    lock.l_whence = SEEK_SET;
    lock.l_start = 0;
    lock.l_len = 0;           // Lock entire file

    printf("Acquiring exclusive lock...\n");
    if (fcntl(fd, F_SETLK, &lock) < 0) {
        perror("fcntl F_SETLK");
        exit(1);
    }
    printf("✓ Exclusive lock acquired\n");

    // Test lock - should succeed (we own it)
    lock.l_type = F_WRLCK;
    if (fcntl(fd, F_GETLK, &lock) < 0) {
        perror("fcntl F_GETLK");
        exit(1);
    }

    if (lock.l_type == F_UNLCK) {
        printf("✓ Lock test passed (no conflict)\n");
    } else {
        printf("✗ Unexpected lock conflict\n");
        exit(1);
    }

    // Release lock
    lock.l_type = F_UNLCK;
    if (fcntl(fd, F_SETLK, &lock) < 0) {
        perror("fcntl F_UNLCK");
        exit(1);
    }
    printf("✓ Lock released\n");

    close(fd);
}

void test_lock_conflict(const char *filepath) {
    int fd;
    struct flock lock;
    pid_t pid;

    printf("\n=== Test 2: Lock Conflict Detection ===\n");

    fd = open(filepath, O_RDWR | O_CREAT, 0644);
    if (fd < 0) {
        perror("open");
        exit(1);
    }

    // Parent acquires lock
    memset(&lock, 0, sizeof(lock));
    lock.l_type = F_WRLCK;
    lock.l_whence = SEEK_SET;
    lock.l_start = 0;
    lock.l_len = 100;  // Lock first 100 bytes

    printf("Parent: Acquiring lock on bytes 0-100...\n");
    if (fcntl(fd, F_SETLK, &lock) < 0) {
        perror("fcntl F_SETLK");
        exit(1);
    }
    printf("✓ Parent: Lock acquired\n");

    // Fork child to test conflict
    pid = fork();
    if (pid < 0) {
        perror("fork");
        exit(1);
    }

    if (pid == 0) {
        // Child process
        int child_fd = open(filepath, O_RDWR);
        if (child_fd < 0) {
            perror("child: open");
            exit(1);
        }

        // Try to acquire conflicting lock (should fail)
        struct flock child_lock;
        memset(&child_lock, 0, sizeof(child_lock));
        child_lock.l_type = F_WRLCK;
        child_lock.l_whence = SEEK_SET;
        child_lock.l_start = 50;   // Overlaps parent's lock
        child_lock.l_len = 100;

        printf("  Child: Testing for lock conflict on bytes 50-150...\n");
        if (fcntl(child_fd, F_GETLK, &child_lock) < 0) {
            perror("child: fcntl F_GETLK");
            exit(1);
        }

        if (child_lock.l_type != F_UNLCK) {
            printf("  ✓ Child: Conflict detected (lock held by PID %d)\n", child_lock.l_pid);
            exit(0);
        } else {
            printf("  ✗ Child: ERROR - No conflict detected (locking may not be working!)\n");
            exit(1);
        }
    } else {
        // Parent waits for child
        int status;
        waitpid(pid, &status, 0);

        if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
            printf("✓ Lock conflict detection working correctly\n");
        } else {
            printf("✗ Lock conflict test failed\n");
            exit(1);
        }

        // Release parent's lock
        lock.l_type = F_UNLCK;
        fcntl(fd, F_SETLK, &lock);
    }

    close(fd);
}

void test_shared_lock(const char *filepath) {
    int fd1, fd2;
    struct flock lock;

    printf("\n=== Test 3: Shared (Read) Locks ===\n");

    fd1 = open(filepath, O_RDONLY);
    fd2 = open(filepath, O_RDONLY);
    if (fd1 < 0 || fd2 < 0) {
        perror("open");
        exit(1);
    }

    // Acquire shared lock on fd1
    memset(&lock, 0, sizeof(lock));
    lock.l_type = F_RDLCK;
    lock.l_whence = SEEK_SET;
    lock.l_start = 0;
    lock.l_len = 0;

    printf("Acquiring shared lock (fd1)...\n");
    if (fcntl(fd1, F_SETLK, &lock) < 0) {
        perror("fcntl F_SETLK fd1");
        exit(1);
    }
    printf("✓ Shared lock acquired (fd1)\n");

    // Acquire another shared lock on fd2 (should succeed)
    printf("Acquiring second shared lock (fd2)...\n");
    if (fcntl(fd2, F_SETLK, &lock) < 0) {
        perror("fcntl F_SETLK fd2");
        exit(1);
    }
    printf("✓ Second shared lock acquired (fd2)\n");
    printf("✓ Multiple shared locks working correctly\n");

    // Release locks
    lock.l_type = F_UNLCK;
    fcntl(fd1, F_SETLK, &lock);
    fcntl(fd2, F_SETLK, &lock);

    close(fd1);
    close(fd2);
}

int main(int argc, char *argv[]) {
    char filepath[1024];

    if (argc != 2) {
        fprintf(stderr, "Usage: %s <nfs-mounted-directory>\n", argv[0]);
        fprintf(stderr, "\nExample:\n");
        fprintf(stderr, "  # On remote machine:\n");
        fprintf(stderr, "  mount -t nfs -o vers=3,tcp server.example.com:/ /mnt/nfs\n");
        fprintf(stderr, "  %s /mnt/nfs\n", argv[0]);
        exit(1);
    }

    snprintf(filepath, sizeof(filepath), "%s/lock-test.dat", argv[1]);

    printf("╔═══════════════════════════════════════════════════════════╗\n");
    printf("║            NFS File Locking Test Suite                   ║\n");
    printf("╚═══════════════════════════════════════════════════════════╝\n");
    printf("\nTest file: %s\n", filepath);

    test_exclusive_lock(filepath);
    test_lock_conflict(filepath);
    test_shared_lock(filepath);

    printf("\n╔═══════════════════════════════════════════════════════════╗\n");
    printf("║                  ALL TESTS PASSED ✓                       ║\n");
    printf("╚═══════════════════════════════════════════════════════════╝\n");
    printf("\nNLM (Network Lock Manager) is working correctly!\n");

    // Cleanup
    unlink(filepath);

    return 0;
}
