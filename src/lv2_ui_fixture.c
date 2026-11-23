/* Minimal LV2 plugin + UI fixture for headless lv2_ui unit tests and lv2-ui-test.
 * No external LV2 headers needed: matches lv2/core/lv2.h & lv2/ui/ui.h ABIs. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef void *LV2_Handle;
typedef void *LV2UI_Handle;
typedef void *LV2UI_Controller;
typedef void *LV2UI_Widget;

typedef struct {
    const char *URI;
    const void *data;
} LV2_Feature;

typedef void (*LV2UI_Write_Function)(
    LV2UI_Controller controller,
    uint32_t port_index,
    uint32_t buffer_size,
    uint32_t port_protocol,
    const void *buffer);

typedef struct {
    const char *URI;
    LV2_Handle (*instantiate)(const void *, double, const char *, const LV2_Feature *const *);
    void (*connect_port)(LV2_Handle, uint32_t, void *);
    void (*activate)(LV2_Handle);
    void (*run)(LV2_Handle, uint32_t);
    void (*deactivate)(LV2_Handle);
    void (*cleanup)(LV2_Handle);
    const void *(*extension_data)(const char *);
} LV2_Descriptor;

typedef struct LV2UI_Descriptor {
    const char *URI;
    LV2UI_Handle (*instantiate)(
        const struct LV2UI_Descriptor *descriptor,
        const char *plugin_uri,
        const char *bundle_path,
        LV2UI_Write_Function write_function,
        LV2UI_Controller controller,
        LV2UI_Widget *widget,
        const LV2_Feature *const *features);
    void (*cleanup)(LV2UI_Handle ui);
    void (*port_event)(
        LV2UI_Handle ui,
        uint32_t port_index,
        uint32_t buffer_size,
        uint32_t format,
        const void *buffer);
    const void *(*extension_data)(const char *uri);
} LV2UI_Descriptor;

typedef struct {
    int (*idle)(LV2UI_Handle ui);
} LV2UI_Idle_Interface;

typedef struct {
    int (*show)(LV2UI_Handle ui);
    int (*hide)(LV2UI_Handle ui);
} LV2UI_Show_Interface;

typedef struct {
    void *handle;
    uint32_t (*port_index)(void *handle, const char *symbol);
} LV2UI_Port_Map;

typedef uint32_t LV2_URID;

typedef struct {
    void *handle;
    LV2_URID (*map)(void *handle, const char *uri);
} LV2_URID_Map;

typedef int (*LV2_State_Store_Function)(
    void *handle,
    uint32_t key,
    const void *value,
    size_t size,
    uint32_t type,
    uint32_t flags);

typedef const void *(*LV2_State_Retrieve_Function)(
    void *handle,
    uint32_t key,
    size_t *size,
    uint32_t *type,
    uint32_t *flags);

typedef struct {
    int (*save)(
        LV2_Handle instance,
        LV2_State_Store_Function store,
        void *handle,
        uint32_t flags,
        const LV2_Feature *const *features);
    int (*restore)(
        LV2_Handle instance,
        LV2_State_Retrieve_Function retrieve,
        void *handle,
        uint32_t flags,
        const LV2_Feature *const *features);
} LV2_State_Interface;

/* --- DSP plugin --- */
typedef struct {
    const float *in[2];
    float *out[2];
    const float *gain;
    float state_bias;
    uint32_t state_key;
    uint32_t chunk_type;
} GainPlugin;

static LV2_Handle dsp_inst(const void *d, double sr, const char *p, const LV2_Feature *const *f) {
    (void)d; (void)sr; (void)p;
    GainPlugin *gp = (GainPlugin *)calloc(1, sizeof(GainPlugin));
    if (!gp) return 0;
    gp->state_bias = 1.0f;
    for (int i = 0; f && f[i]; i++) {
        if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/urid#map") == 0 && f[i]->data) {
            const LV2_URID_Map *m = (const LV2_URID_Map *)f[i]->data;
            if (m->map) {
                gp->state_key = m->map(m->handle, "urn:kabelsalat:fixture:state_bias");
                gp->chunk_type = m->map(m->handle, "http://lv2plug.in/ns/ext/atom#Chunk");
            }
        }
    }
    return gp;
}
static void dsp_conn(LV2_Handle h, uint32_t port, void *data) {
    GainPlugin *p = (GainPlugin *)h;
    if (port < 2) p->in[port] = (const float *)data;
    else if (port < 4) p->out[port - 2] = (float *)data;
    else if (port == 4) p->gain = (const float *)data;
}
static void dsp_run(LV2_Handle h, uint32_t frames) {
    GainPlugin *p = (GainPlugin *)h;
    float g = (p->gain ? *p->gain : 1.0f) * p->state_bias;
    for (uint32_t i = 0; i < frames; i++) {
        p->out[0][i] = p->in[0] ? p->in[0][i] * g : 0.0f;
        p->out[1][i] = p->in[1] ? p->in[1][i] * g : 0.0f;
    }
}
static void dsp_cleanup(LV2_Handle h) { free(h); }

static int dsp_state_save(
    LV2_Handle h,
    LV2_State_Store_Function store,
    void *handle,
    uint32_t flags,
    const LV2_Feature *const *features) {
    (void)flags; (void)features;
    GainPlugin *p = (GainPlugin *)h;
    if (!p || !store || p->state_key == 0) return 1;
    return store(handle, p->state_key, &p->state_bias, sizeof(float), p->chunk_type, 1 | 2);
}

static int dsp_state_restore(
    LV2_Handle h,
    LV2_State_Retrieve_Function retrieve,
    void *handle,
    uint32_t flags,
    const LV2_Feature *const *features) {
    (void)flags; (void)features;
    GainPlugin *p = (GainPlugin *)h;
    if (!p || !retrieve || p->state_key == 0) return 1;
    size_t size = 0;
    uint32_t type = 0;
    uint32_t rflags = 0;
    const void *val = retrieve(handle, p->state_key, &size, &type, &rflags);
    if (val && size == sizeof(float)) {
        memcpy(&p->state_bias, val, sizeof(float));
        return 0;
    }
    return 1;
}

static const LV2_State_Interface dsp_state_iface = { dsp_state_save, dsp_state_restore };

static const void *dsp_ext(const char *uri) {
    if (uri && strcmp(uri, "http://lv2plug.in/ns/ext/state#interface") == 0) {
        return &dsp_state_iface;
    }
    return 0;
}

static const LV2_Descriptor dsp_desc = {
    "urn:kabelsalat:fixture:ui_gain",
    dsp_inst,
    dsp_conn,
    0,
    dsp_run,
    0,
    dsp_cleanup,
    dsp_ext,
};

__attribute__((visibility("default")))
const LV2_Descriptor *lv2_descriptor(uint32_t index) {
    return index == 0 ? &dsp_desc : 0;
}

/* --- Headless floating UI (showInterface + idleInterface) --- */
typedef struct {
    LV2UI_Write_Function write;
    LV2UI_Controller ctrl;
    uint32_t mapped_port;
    float last_gain;
    int shown;
    int idle_ticks;
    int close_after;
} TestUi;

static LV2UI_Handle ui_inst(
    const LV2UI_Descriptor *d,
    const char *plugin_uri,
    const char *bundle_path,
    LV2UI_Write_Function write_function,
    LV2UI_Controller controller,
    LV2UI_Widget *widget,
    const LV2_Feature *const *features) {
    (void)d; (void)plugin_uri; (void)bundle_path;
    TestUi *ui = (TestUi *)calloc(1, sizeof(TestUi));
    if (!ui) return 0;
    ui->write = write_function;
    ui->ctrl = controller;
    ui->mapped_port = 4;
    ui->close_after = 0; /* >0 only when plugin drives auto-close via port_event */
    for (int i = 0; features && features[i]; i++) {
        if (strcmp(features[i]->URI, "http://lv2plug.in/ns/extensions/ui#portMap") == 0 && features[i]->data) {
            const LV2UI_Port_Map *pm = (const LV2UI_Port_Map *)features[i]->data;
            if (pm->port_index) {
                uint32_t idx = pm->port_index(pm->handle, "gain");
                if (idx != (uint32_t)-1) ui->mapped_port = idx;
            }
        }
    }
    if (widget) *widget = ui;
    return ui;
}

static void ui_cleanup(LV2UI_Handle h) {
    free(h);
}

static void ui_port_event(LV2UI_Handle h, uint32_t port_index, uint32_t buffer_size, uint32_t format, const void *buffer) {
    TestUi *ui = (TestUi *)h;
    if (format == 0 && buffer_size == sizeof(float) && buffer && port_index == ui->mapped_port) {
        ui->last_gain = *(const float *)buffer;
        /* When host sends >= 1.9, echo 0.25 back via write_function and close after 2 idles. */
        if (ui->last_gain > 1.8f && ui->write) {
            float echoed = 0.25f;
            ui->write(ui->ctrl, ui->mapped_port, sizeof(float), 0, &echoed);
            ui->close_after = 2;
        }
    }
}

static int ui_show(LV2UI_Handle h) {
    TestUi *ui = (TestUi *)h;
    ui->shown = 1;
    if (ui->write) {
        float init_val = 0.75f;
        ui->write(ui->ctrl, ui->mapped_port, sizeof(float), 0, &init_val);
    }
    return 0;
}

static int ui_hide(LV2UI_Handle h) {
    TestUi *ui = (TestUi *)h;
    ui->shown = 0;
    return 0;
}

static int ui_idle(LV2UI_Handle h) {
    TestUi *ui = (TestUi *)h;
    ui->idle_ticks++;
    if (ui->close_after > 0 && ui->idle_ticks >= ui->close_after) {
        return 1; /* closed */
    }
    return 0;
}

static const LV2UI_Show_Interface show_iface = { ui_show, ui_hide };
static const LV2UI_Idle_Interface idle_iface = { ui_idle };

static const void *ui_ext(const char *uri) {
    if (strcmp(uri, "http://lv2plug.in/ns/extensions/ui#showInterface") == 0) return &show_iface;
    if (strcmp(uri, "http://lv2plug.in/ns/extensions/ui#idleInterface") == 0) return &idle_iface;
    return 0;
}

static const LV2UI_Descriptor ui_desc = {
    "urn:kabelsalat:fixture:ui_gain#ui",
    ui_inst,
    ui_cleanup,
    ui_port_event,
    ui_ext,
};

__attribute__((visibility("default")))
const LV2UI_Descriptor *lv2ui_descriptor(uint32_t index) {
    return index == 0 ? &ui_desc : 0;
}
