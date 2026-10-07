// LD_PRELOAD fault injector for libasound, used by the ALSA soak harness
// (`src/audio/alsa_soak.rs`). It sits between cpal's ALSA worker and the real
// libasound and reproduces PCM failure shapes that need real hardware, a
// suspend/resume cycle, or a wedged sound server to trigger naturally.
//
// The mode is read from the file named by CHATT_ALSA_FAULT_FILE (re-read at
// most every 20 ms), so a running process can be flipped between modes:
//
//   none     pass everything through
//   wedge    the PCM's poll descriptors never become ready and no error is
//            raised: a dmix timer or sound-server eventfd that stopped firing
//   suspend  the PCM reports SUSPENDED / -ESTRPIPE and snd_pcm_resume()
//            answers -EAGAIN forever: a resume that never completes
//   ebadfd   every PCM transfer and status call fails with -EBADFD: a PCM left
//            in a state prepare() cannot fix
//   hang     PCM transfer, status, drop and close calls block until the mode
//            changes: a driver or sound server stuck inside a call, which
//            pins the cpal worker and anything that joins it
//
// CHATT_ALSA_FAULT_DIR selects which PCMs are affected: `playback`, `capture`
// or `both` (the default).
//
// Build: cc -O2 -shared -fPIC -o alsa_fault.so alsa_fault.c -ldl
#define _GNU_SOURCE
#include <alsa/asoundlib.h>
#include <dlfcn.h>
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

enum mode { MODE_NONE, MODE_WEDGE, MODE_SUSPEND, MODE_EBADFD, MODE_HANG };

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static enum mode cached_mode = MODE_NONE;
static struct timespec last_read;

#define MAX_TRACKED 64
static struct {
    int fd;
    snd_pcm_stream_t stream;
} tracked[MAX_TRACKED];
static int tracked_len;

static void *real(const char *name) {
    void *sym = dlsym(RTLD_NEXT, name);
    if (!sym) {
        fprintf(stderr, "alsa_fault: missing %s\n", name);
        abort();
    }
    return sym;
}

static enum mode current_mode(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    pthread_mutex_lock(&lock);
    long elapsed_ms = (now.tv_sec - last_read.tv_sec) * 1000 +
                      (now.tv_nsec - last_read.tv_nsec) / 1000000;
    if (elapsed_ms >= 20 || (last_read.tv_sec == 0 && last_read.tv_nsec == 0)) {
        last_read = now;
        enum mode mode = MODE_NONE;
        const char *path = getenv("CHATT_ALSA_FAULT_FILE");
        FILE *file = path ? fopen(path, "r") : NULL;
        if (file) {
            char buf[32] = {0};
            if (fgets(buf, sizeof buf, file)) {
                if (!strncmp(buf, "wedge", 5)) mode = MODE_WEDGE;
                else if (!strncmp(buf, "suspend", 7)) mode = MODE_SUSPEND;
                else if (!strncmp(buf, "ebadfd", 6)) mode = MODE_EBADFD;
                else if (!strncmp(buf, "hang", 4)) mode = MODE_HANG;
            }
            fclose(file);
        }
        cached_mode = mode;
    }
    enum mode mode = cached_mode;
    pthread_mutex_unlock(&lock);
    return mode;
}

static int direction_selected(snd_pcm_stream_t stream) {
    const char *dir = getenv("CHATT_ALSA_FAULT_DIR");
    if (!dir || !strcmp(dir, "both")) return 1;
    if (!strcmp(dir, "playback")) return stream == SND_PCM_STREAM_PLAYBACK;
    if (!strcmp(dir, "capture")) return stream == SND_PCM_STREAM_CAPTURE;
    return 1;
}

static int pcm_faulted(snd_pcm_t *pcm, enum mode want) {
    if (current_mode() != want) return 0;
    return direction_selected(snd_pcm_stream(pcm));
}

static int fd_faulted(int fd, enum mode want) {
    if (current_mode() != want) return 0;
    pthread_mutex_lock(&lock);
    int hit = 0;
    for (int i = 0; i < tracked_len; i++) {
        if (tracked[i].fd == fd && direction_selected(tracked[i].stream)) {
            hit = 1;
            break;
        }
    }
    pthread_mutex_unlock(&lock);
    return hit;
}

// Blocks the calling thread while `pcm` is hung.
static void hang_while_faulted(snd_pcm_t *pcm) {
    struct timespec step = {0, 10 * 1000 * 1000};
    while (pcm_faulted(pcm, MODE_HANG)) nanosleep(&step, NULL);
}

int snd_pcm_poll_descriptors(snd_pcm_t *pcm, struct pollfd *pfds, unsigned int space) {
    static int (*next)(snd_pcm_t *, struct pollfd *, unsigned int);
    if (!next) next = real("snd_pcm_poll_descriptors");
    int filled = next(pcm, pfds, space);
    pthread_mutex_lock(&lock);
    for (int i = 0; i < filled; i++) {
        int known = 0;
        for (int j = 0; j < tracked_len; j++) {
            if (tracked[j].fd == pfds[i].fd) {
                tracked[j].stream = snd_pcm_stream(pcm);
                known = 1;
            }
        }
        if (!known && tracked_len < MAX_TRACKED) {
            tracked[tracked_len].fd = pfds[i].fd;
            tracked[tracked_len].stream = snd_pcm_stream(pcm);
            tracked_len++;
        }
    }
    pthread_mutex_unlock(&lock);
    return filled;
}

int poll(struct pollfd *fds, nfds_t nfds, int timeout) {
    static int (*next)(struct pollfd *, nfds_t, int);
    if (!next) next = real("poll");
    short saved[16];
    int masked = 0, errored = 0;
    if (nfds <= 16 && current_mode() != MODE_NONE) {
        for (nfds_t i = 0; i < nfds; i++) {
            saved[i] = fds[i].events;
            if (fd_faulted(fds[i].fd, MODE_WEDGE)) {
                fds[i].events = 0;
                masked = 1;
            } else if (fd_faulted(fds[i].fd, MODE_SUSPEND) || fd_faulted(fds[i].fd, MODE_EBADFD)) {
                errored = 1;
            }
        }
    }
    if (errored) {
        // The kernel reports POLLERR immediately for a PCM outside
        // PREPARED/RUNNING; mirror that without touching the real device.
        for (nfds_t i = 0; i < nfds; i++) {
            fds[i].revents = 0;
            if (fd_faulted(fds[i].fd, MODE_SUSPEND) || fd_faulted(fds[i].fd, MODE_EBADFD))
                fds[i].revents = POLLERR;
        }
        int ready = 0;
        for (nfds_t i = 0; i < nfds; i++) ready += fds[i].revents != 0;
        if (ready) return ready;
    }
    int result = next(fds, nfds, timeout);
    if (masked) {
        for (nfds_t i = 0; i < nfds; i++) fds[i].events = saved[i];
    }
    return result;
}

int snd_pcm_poll_descriptors_revents(snd_pcm_t *pcm, struct pollfd *pfds, unsigned int nfds,
                                     unsigned short *revents) {
    static int (*next)(snd_pcm_t *, struct pollfd *, unsigned int, unsigned short *);
    if (!next) next = real("snd_pcm_poll_descriptors_revents");
    if (pcm_faulted(pcm, MODE_SUSPEND) || pcm_faulted(pcm, MODE_EBADFD)) {
        *revents = POLLERR;
        return 0;
    }
    return next(pcm, pfds, nfds, revents);
}

snd_pcm_state_t snd_pcm_state(snd_pcm_t *pcm) {
    static snd_pcm_state_t (*next)(snd_pcm_t *);
    if (!next) next = real("snd_pcm_state");
    if (pcm_faulted(pcm, MODE_SUSPEND)) return SND_PCM_STATE_SUSPENDED;
    if (pcm_faulted(pcm, MODE_EBADFD)) return SND_PCM_STATE_SETUP;
    return next(pcm);
}

int snd_pcm_avail_delay(snd_pcm_t *pcm, snd_pcm_sframes_t *avail, snd_pcm_sframes_t *delay) {
    static int (*next)(snd_pcm_t *, snd_pcm_sframes_t *, snd_pcm_sframes_t *);
    if (!next) next = real("snd_pcm_avail_delay");
    hang_while_faulted(pcm);
    if (pcm_faulted(pcm, MODE_SUSPEND)) return -ESTRPIPE;
    if (pcm_faulted(pcm, MODE_EBADFD)) return -EBADFD;
    return next(pcm, avail, delay);
}

snd_pcm_sframes_t snd_pcm_readi(snd_pcm_t *pcm, void *buffer, snd_pcm_uframes_t size) {
    static snd_pcm_sframes_t (*next)(snd_pcm_t *, void *, snd_pcm_uframes_t);
    if (!next) next = real("snd_pcm_readi");
    hang_while_faulted(pcm);
    if (pcm_faulted(pcm, MODE_SUSPEND)) return -ESTRPIPE;
    if (pcm_faulted(pcm, MODE_EBADFD)) return -EBADFD;
    return next(pcm, buffer, size);
}

snd_pcm_sframes_t snd_pcm_writei(snd_pcm_t *pcm, const void *buffer, snd_pcm_uframes_t size) {
    static snd_pcm_sframes_t (*next)(snd_pcm_t *, const void *, snd_pcm_uframes_t);
    if (!next) next = real("snd_pcm_writei");
    hang_while_faulted(pcm);
    if (pcm_faulted(pcm, MODE_SUSPEND)) return -ESTRPIPE;
    if (pcm_faulted(pcm, MODE_EBADFD)) return -EBADFD;
    return next(pcm, buffer, size);
}

int snd_pcm_resume(snd_pcm_t *pcm) {
    static int (*next)(snd_pcm_t *);
    if (!next) next = real("snd_pcm_resume");
    if (pcm_faulted(pcm, MODE_SUSPEND)) return -EAGAIN;
    return next(pcm);
}

int snd_pcm_prepare(snd_pcm_t *pcm) {
    static int (*next)(snd_pcm_t *);
    if (!next) next = real("snd_pcm_prepare");
    if (pcm_faulted(pcm, MODE_EBADFD)) return -EBADFD;
    return next(pcm);
}

int snd_pcm_start(snd_pcm_t *pcm) {
    static int (*next)(snd_pcm_t *);
    if (!next) next = real("snd_pcm_start");
    if (pcm_faulted(pcm, MODE_EBADFD)) return -EBADFD;
    return next(pcm);
}

int snd_pcm_drop(snd_pcm_t *pcm) {
    static int (*next)(snd_pcm_t *);
    if (!next) next = real("snd_pcm_drop");
    hang_while_faulted(pcm);
    return next(pcm);
}

int snd_pcm_close(snd_pcm_t *pcm) {
    static int (*next)(snd_pcm_t *);
    if (!next) next = real("snd_pcm_close");
    hang_while_faulted(pcm);
    return next(pcm);
}
