// Test-only interposition on ALSA's null PCM; never loaded by production.
#define _GNU_SOURCE
#include <alsa/asoundlib.h>
#include <dlfcn.h>
#include <errno.h>
#include <poll.h>
#include <stdatomic.h>
static _Atomic(snd_pcm_t *) target;
static atomic_int mode, prepares, resumes, events;
void tune_4295_arm(int value) {
    atomic_store(&prepares, 0); atomic_store(&resumes, 0);
    atomic_store(&events, 0); atomic_store(&mode, value);
}
int tune_4295_count(int which) {
    return atomic_load(which == 0 ? &prepares : which == 1 ? &resumes : &events);
}
int snd_pcm_open(snd_pcm_t **pcm, const char *name, snd_pcm_stream_t stream, int flags) {
    int (*real)(snd_pcm_t **, const char *, snd_pcm_stream_t, int) = dlsym(RTLD_NEXT, "snd_pcm_open");
    int rc = real(pcm, name, stream, flags);
    if (rc == 0) atomic_store(&target, *pcm);
    return rc;
}
int snd_pcm_poll_descriptors_revents(snd_pcm_t *pcm, struct pollfd *fds, unsigned nfds, unsigned short *revents) {
    int (*real)(snd_pcm_t *, struct pollfd *, unsigned, unsigned short *) =
        dlsym(RTLD_NEXT, "snd_pcm_poll_descriptors_revents");
    int rc = real(pcm, fds, nfds, revents);
    int m = atomic_load(&mode);
    if (pcm == atomic_load(&target) && m != 0) {
        atomic_fetch_add(&events, 1);
        *revents = m == 5 ? POLLHUP : POLLERR;
        return 0;
    }
    return rc;
}
snd_pcm_state_t snd_pcm_state(snd_pcm_t *pcm) {
    snd_pcm_state_t (*real)(snd_pcm_t *) = dlsym(RTLD_NEXT, "snd_pcm_state");
    if (pcm == atomic_load(&target)) {
        switch (atomic_load(&mode)) {
            case 1: return SND_PCM_STATE_XRUN;
            case 2: case 3: case 4: return SND_PCM_STATE_SUSPENDED;
            case 6: return SND_PCM_STATE_DISCONNECTED;
            case 7: case 8: return SND_PCM_STATE_RUNNING;
        }
    }
    return real(pcm);
}
int snd_pcm_prepare(snd_pcm_t *pcm) {
    int (*real)(snd_pcm_t *) = dlsym(RTLD_NEXT, "snd_pcm_prepare");
    if (pcm == atomic_load(&target) && atomic_load(&mode) != 0) {
        atomic_fetch_add(&prepares, 1);
        if (atomic_load(&mode) == 1 || atomic_load(&mode) == 3 || atomic_load(&mode) == 8)
            atomic_store(&mode, 0);
    }
    return real(pcm);
}
int snd_pcm_resume(snd_pcm_t *pcm) {
    int (*real)(snd_pcm_t *) = dlsym(RTLD_NEXT, "snd_pcm_resume");
    if (pcm == atomic_load(&target)) {
        int m = atomic_load(&mode);
        atomic_fetch_add(&resumes, 1);
        if (m == 2) { atomic_store(&mode, 0); return 0; }
        if (m == 3) return -ENOSYS;
        if (m == 4) return -EAGAIN;
    }
    return real(pcm);
}

snd_pcm_sframes_t snd_pcm_avail(snd_pcm_t *pcm) {
    snd_pcm_sframes_t (*real)(snd_pcm_t *) = dlsym(RTLD_NEXT, "snd_pcm_avail");
    // Arm EPIPE only after delivering POLLERR; do not hit an older poll cycle.
    if (pcm == atomic_load(&target) && atomic_load(&mode) == 8 && atomic_load(&events) > 0)
        return -EPIPE;
    return real(pcm);
}
