/*
 * bento-run: every `bento exec` runs its command through this (contracts.md,
 * `exec`).
 *
 * msb's agent SIGKILLs an exec's process group at once when the caller's
 * connection drops. bento promises SIGHUP first, then SIGKILL after 5 s, so:
 *
 *   - the command runs in its own process group (the foreground one when
 *     there is a terminal), out of reach of that SIGKILL;
 *   - a watcher, in a third group, waits on a pipe from this process: EOF
 *     without a "done" byte means this process was killed (the caller is
 *     gone), and the watcher hangs up the command's group, then kills it
 *     after 5 s;
 *   - signals bento forwards (INT, TERM, HUP, QUIT, USR1, USR2) go on to
 *     the command's group;
 *   - the exit status is the command's, 128+n when signal n killed it,
 *     127 / 126 when it can't be found / run.
 *
 * Processes the command leaves behind keep running (until `bento stop`).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#ifndef BENTO_RUN_GRACE_MS
#define BENTO_RUN_GRACE_MS 5000
#endif

static volatile sig_atomic_t command_pgid = 0;

static void forward(int sig) {
    if (command_pgid > 0)
        kill(-command_pgid, sig);
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

static int group_alive(pid_t pgid) {
    return kill(-pgid, 0) == 0 || errno == EPERM;
}

static void watch(int rfd, pid_t pgid) {
    int null = open("/dev/null", O_RDWR);
    if (null >= 0) {
        dup2(null, 0);
        dup2(null, 1);
        dup2(null, 2);
        if (null > 2)
            close(null);
    }
    setpgid(0, 0);
    for (int s = 1; s < NSIG; s++)
        if (s != SIGKILL && s != SIGSTOP && s != SIGCHLD)
            signal(s, SIG_IGN);
    char c;
    ssize_t n;
    do {
        n = read(rfd, &c, 1);
    } while (n < 0 && errno == EINTR);
    if (n == 1)
        _exit(0); /* the command ended normally */
    if (!group_alive(pgid))
        _exit(0);
    kill(-pgid, SIGHUP);
    kill(-pgid, SIGCONT);
    for (long waited = 0; waited < BENTO_RUN_GRACE_MS; waited += 50) {
        if (!group_alive(pgid))
            _exit(0);
        sleep_ms(50);
    }
    kill(-pgid, SIGKILL);
    _exit(0);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: bento-run <command> [args...]\n");
        return 125;
    }
    int tty = isatty(0) && tcgetpgrp(0) == getpgrp();
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC) < 0) {
        perror("bento-run: pipe");
        return 125;
    }
    signal(SIGTTOU, SIG_IGN);

    pid_t cmd = fork();
    if (cmd < 0) {
        perror("bento-run: fork");
        return 125;
    }
    if (cmd == 0) {
        setpgid(0, 0);
        if (tty)
            tcsetpgrp(0, getpid());
        signal(SIGTTOU, SIG_DFL);
        execvp(argv[1], argv + 1);
        int e = errno;
        fprintf(stderr, "bento: %s: %s\n", argv[1], strerror(e));
        _exit(e == ENOENT || e == ENOTDIR ? 127 : 126);
    }
    setpgid(cmd, cmd);
    command_pgid = cmd;
    if (tty)
        tcsetpgrp(0, cmd);

    pid_t w = fork();
    if (w == 0) {
        close(pipefd[1]);
        watch(pipefd[0], cmd);
    }
    close(pipefd[0]);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = forward;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    int sigs[] = {SIGHUP, SIGINT, SIGTERM, SIGQUIT, SIGUSR1, SIGUSR2};
    for (unsigned i = 0; i < sizeof sigs / sizeof *sigs; i++)
        sigaction(sigs[i], &sa, NULL);

    int status = 0;
    while (waitpid(cmd, &status, 0) < 0) {
        if (errno != EINTR) {
            perror("bento-run: waitpid");
            return 125;
        }
    }
    if (write(pipefd[1], "x", 1) < 0) {
        /* the watcher is gone: nothing to tell */
    }
    if (WIFEXITED(status))
        return WEXITSTATUS(status);
    if (WIFSIGNALED(status))
        return 128 + WTERMSIG(status);
    return 125;
}
