/* A latent LV2 plugin for kabelsalat's own tests: it delays stereo audio
 * by `delay` frames and reports that as its latency, so the host has
 * something to compensate and something that changes while it runs. No LV2
 * headers needed: the descriptor is the ABI of lv2/core/lv2.h. */
#include <stdint.h>
#include <stdlib.h>

#define RING 32768
#define MAX_DELAY 16384

typedef struct {
    const float *in[2];
    float *out[2];
    const float *delay;
    float *latency;
    float ring[2][RING];
    uint32_t pos;
} Latent;

typedef struct {
    const char *uri;
    void *(*instantiate)(const void *, double, const char *, const void *const *);
    void (*connect_port)(void *, uint32_t, void *);
    void (*activate)(void *);
    void (*run)(void *, uint32_t);
    void (*deactivate)(void *);
    void (*cleanup)(void *);
    const void *(*extension_data)(const char *);
} Descriptor;

static void *instantiate(const void *d, double rate, const char *path, const void *const *features) {
    (void)d; (void)rate; (void)path; (void)features;
    return calloc(1, sizeof(Latent));
}

static void connect_port(void *h, uint32_t port, void *data) {
    Latent *p = h;
    switch (port) {
    case 0: case 1: p->in[port] = data; break;
    case 2: case 3: p->out[port - 2] = data; break;
    case 4: p->delay = data; break;
    case 5: p->latency = data; break;
    }
}

static void run(void *h, uint32_t frames) {
    Latent *p = h;
    float want = p->delay ? *p->delay : 0.0f;
    uint32_t delay = want <= 0.0f ? 0 : want >= MAX_DELAY ? MAX_DELAY : (uint32_t)(want + 0.5f);
    for (uint32_t f = 0; f < frames; f++) {
        uint32_t at = (p->pos + f) & (RING - 1);
        uint32_t from = (p->pos + f - delay) & (RING - 1);
        for (int c = 0; c < 2; c++) {
            p->ring[c][at] = p->in[c][f];
            p->out[c][f] = p->ring[c][from];
        }
    }
    p->pos = (p->pos + frames) & (RING - 1);
    /* Also for a run of 0 frames, as lv2core asks of a latent plugin. */
    if (p->latency) *p->latency = (float)delay;
}

static void cleanup(void *h) { free(h); }

static const Descriptor descriptor = {
    "urn:kabelsalat:fixture:latent", instantiate, connect_port, 0, run, 0, cleanup, 0,
};

__attribute__((visibility("default")))
const Descriptor *lv2_descriptor(uint32_t index) {
    return index == 0 ? &descriptor : 0;
}
