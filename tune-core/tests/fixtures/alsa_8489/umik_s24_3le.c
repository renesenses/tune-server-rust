// Test-only (#8489), never loaded by production: makes ALSA's null PCM look
// like a miniDSP UMIK-1, i.e. hardware that offers ONLY S24_3LE, 2 channels,
// 48 kHz. Every configuration space CPAL starts from (snd_pcm_hw_params_any)
// is restricted to that, so test_format / get_rate_min / get_channels_max and
// the final hw_params all see the device as the microphone exposes it.
#define _GNU_SOURCE
#include <alsa/asoundlib.h>
#include <dlfcn.h>
int snd_pcm_hw_params_any(snd_pcm_t *pcm, snd_pcm_hw_params_t *params) {
    int (*real)(snd_pcm_t *, snd_pcm_hw_params_t *) = dlsym(RTLD_NEXT, "snd_pcm_hw_params_any");
    int rc = real(pcm, params);
    if (rc < 0) return rc;
    if ((rc = snd_pcm_hw_params_set_format(pcm, params, SND_PCM_FORMAT_S24_3LE)) < 0) return rc;
    if ((rc = snd_pcm_hw_params_set_channels(pcm, params, 2)) < 0) return rc;
    return snd_pcm_hw_params_set_rate(pcm, params, 48000, 0);
}
