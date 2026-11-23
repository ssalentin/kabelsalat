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

typedef struct {
    void *handle;
    const char *(*unmap)(void *handle, LV2_URID urid);
} LV2_URID_Unmap;

typedef struct {
    uint32_t context;
    uint32_t subject;
    uint32_t key;
    uint32_t size;
    uint32_t type;
    const void *value;
} LV2_Options_Option;

typedef struct {
    const void *(*data_access)(const char *uri);
} LV2_Extension_Data_Feature;

typedef struct {
    uint32_t size;
    uint32_t type;
} LV2_Atom;

typedef struct {
    LV2_Atom atom;
    struct {
        uint32_t unit;
        uint32_t pad;
    } body;
} LV2_Atom_Sequence;

typedef struct {
    uint32_t (*magic)(LV2_Handle h);
    uint32_t (*feature_flags)(LV2_Handle h);
    uint32_t (*atom_out_cap)(LV2_Handle h);
} FixtureExt;

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
    float *meter;
    const LV2_Atom_Sequence *events_in;
    LV2_Atom_Sequence *events_out;
    float state_bias;
    uint32_t state_key;
    uint32_t chunk_type;
    uint32_t seq_type;
    uint32_t ping_type;
    uint32_t pong_type;
    uint32_t feature_flags;
    uint32_t atom_out_cap;
} GainPlugin;

static LV2_Handle dsp_inst(const void *d, double sr, const char *p, const LV2_Feature *const *f) {
    (void)d; (void)sr; (void)p;
    GainPlugin *gp = (GainPlugin *)calloc(1, sizeof(GainPlugin));
    if (!gp) return 0;
    gp->state_bias = 1.0f;
    const LV2_URID_Map *umap = 0;
    const LV2_URID_Unmap *uunmap = 0;
    const LV2_Options_Option *opts = 0;
    for (int i = 0; f && f[i]; i++) {
        if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/urid#map") == 0 && f[i]->data) {
            umap = (const LV2_URID_Map *)f[i]->data;
            gp->feature_flags |= (1u << 0);
            if (umap->map) {
                gp->state_key = umap->map(umap->handle, "urn:kabelsalat:fixture:state_bias");
                gp->chunk_type = umap->map(umap->handle, "http://lv2plug.in/ns/ext/atom#Chunk");
                gp->seq_type = umap->map(umap->handle, "http://lv2plug.in/ns/ext/atom#Sequence");
                gp->ping_type = umap->map(umap->handle, "urn:kabelsalat:fixture:ping");
                gp->pong_type = umap->map(umap->handle, "urn:kabelsalat:fixture:pong");
            }
        } else if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/urid#unmap") == 0 && f[i]->data) {
            uunmap = (const LV2_URID_Unmap *)f[i]->data;
        } else if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/options#options") == 0 && f[i]->data) {
            opts = (const LV2_Options_Option *)f[i]->data;
        } else if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength") == 0) {
            gp->feature_flags |= (1u << 3);
        } else if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/state#mapPath") == 0 && f[i]->data) {
            gp->feature_flags |= (1u << 4);
        } else if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/state#makePath") == 0 && f[i]->data) {
            gp->feature_flags |= (1u << 5);
        } else if (strcmp(f[i]->URI, "http://lv2plug.in/ns/ext/state#freePath") == 0 && f[i]->data) {
            gp->feature_flags |= (1u << 6);
        }
    }
    if (umap && uunmap && uunmap->unmap && gp->state_key) {
        const char *back = uunmap->unmap(uunmap->handle, gp->state_key);
        if (back && strcmp(back, "urn:kabelsalat:fixture:state_bias") == 0) {
            gp->feature_flags |= (1u << 1);
        }
    }
    if (umap && umap->map && opts) {
        uint32_t sr_k = umap->map(umap->handle, "http://lv2plug.in/ns/ext/parameters#sampleRate");
        uint32_t min_k = umap->map(umap->handle, "http://lv2plug.in/ns/ext/buf-size#minBlockLength");
        uint32_t nom_k = umap->map(umap->handle, "http://lv2plug.in/ns/ext/buf-size#nominalBlockLength");
        uint32_t max_k = umap->map(umap->handle, "http://lv2plug.in/ns/ext/buf-size#maxBlockLength");
        int found_mask = 0;
        for (int i = 0; opts[i].key != 0; i++) {
            if (opts[i].key == sr_k) found_mask |= 1;
            if (opts[i].key == min_k) found_mask |= 2;
            if (opts[i].key == nom_k) found_mask |= 4;
            if (opts[i].key == max_k) found_mask |= 8;
        }
        if (found_mask == 15) {
            gp->feature_flags |= (1u << 2);
        }
    }
    return gp;
}
static void dsp_conn(LV2_Handle h, uint32_t port, void *data) {
    GainPlugin *p = (GainPlugin *)h;
    if (port < 2) p->in[port] = (const float *)data;
    else if (port < 4) p->out[port - 2] = (float *)data;
    else if (port == 4) p->gain = (const float *)data;
    else if (port == 5) p->meter = (float *)data;
    else if (port == 6) p->events_in = (const LV2_Atom_Sequence *)data;
    else if (port == 7) p->events_out = (LV2_Atom_Sequence *)data;
}
static void dsp_run(LV2_Handle h, uint32_t frames) {
    GainPlugin *p = (GainPlugin *)h;
    float g = (p->gain ? *p->gain : 1.0f) * p->state_bias;
    float peak = 0.0f;
    for (uint32_t i = 0; i < frames; i++) {
        float l = p->in[0] ? p->in[0][i] * g : 0.0f;
        float r = p->in[1] ? p->in[1][i] * g : 0.0f;
        p->out[0][i] = l;
        p->out[1][i] = r;
        float al = l < 0.0f ? -l : l;
        if (al > peak) peak = al;
    }
    if (p->meter) *p->meter = peak;

    if (p->events_out) {
        uint32_t cap = p->events_out->atom.size;
        if (cap > p->atom_out_cap) p->atom_out_cap = cap;
        p->events_out->atom.size = 8;
        p->events_out->atom.type = p->seq_type;
        p->events_out->body.unit = 0;
        p->events_out->body.pad = 0;

        if (p->events_in && p->events_in->atom.type == p->seq_type && p->events_in->atom.size >= 8) {
            const uint8_t *base = (const uint8_t *)p->events_in;
            uint32_t in_end = 8 + p->events_in->atom.size;
            uint32_t at = 16;
            uint32_t out_used = 8;
            while (at + 16 <= in_end) {
                uint32_t bsize = *(const uint32_t *)(base + at + 8);
                uint32_t btype = *(const uint32_t *)(base + at + 12);
                uint32_t body = at + 16;
                if (body + bsize > in_end) break;
                if (btype == p->ping_type && p->pong_type) {
                    uint32_t ev_padded = (16 + 4 + 7) & ~7u;
                    if (out_used + ev_padded <= cap + 8) {
                        uint8_t *dst = ((uint8_t *)p->events_out) + 8 + out_used;
                        memset(dst, 0, ev_padded);
                        uint32_t one = 1u;
                        *(uint32_t *)(dst + 8) = 4u;
                        *(uint32_t *)(dst + 12) = p->pong_type;
                        memcpy(dst + 16, &one, 4);
                        out_used += ev_padded;
                        p->events_out->atom.size = out_used;
                    }
                }
                at = body + ((bsize + 7) & ~7u);
            }
        }
    }
}
static void dsp_cleanup(LV2_Handle h) { free(h); }

static uint32_t ext_magic(LV2_Handle h) { return h ? 0x4B534C54u : 0u; }
static uint32_t ext_feature_flags(LV2_Handle h) { return h ? ((const GainPlugin *)h)->feature_flags : 0u; }
static uint32_t ext_atom_out_cap(LV2_Handle h) { return h ? ((const GainPlugin *)h)->atom_out_cap : 0u; }
static const FixtureExt fixture_ext_iface = { ext_magic, ext_feature_flags, ext_atom_out_cap };

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
    if (uri && strcmp(uri, "urn:kabelsalat:fixture:ext") == 0) {
        return &fixture_ext_iface;
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
    uint32_t meter_port;
    uint32_t events_in_port;
    uint32_t events_out_port;
    uint32_t event_transfer;
    uint32_t ping_type;
    uint32_t pong_type;
    float last_gain;
    int shown;
    int idle_ticks;
    int close_after;
    int direct_ok;
    int saw_meter;
    int saw_pong;
    int emitted_direct_gain;
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
    ui->meter_port = 5;
    ui->events_in_port = 6;
    ui->events_out_port = 7;
    ui->close_after = 0; /* >0 only when plugin drives auto-close via port_event */
    LV2_Handle inst_handle = 0;
    const LV2_Extension_Data_Feature *data_access = 0;
    for (int i = 0; features && features[i]; i++) {
        if (strcmp(features[i]->URI, "http://lv2plug.in/ns/extensions/ui#portMap") == 0 && features[i]->data) {
            const LV2UI_Port_Map *pm = (const LV2UI_Port_Map *)features[i]->data;
            if (pm->port_index) {
                uint32_t idx = pm->port_index(pm->handle, "gain");
                if (idx != (uint32_t)-1) ui->mapped_port = idx;
                idx = pm->port_index(pm->handle, "meter");
                if (idx != (uint32_t)-1) ui->meter_port = idx;
                idx = pm->port_index(pm->handle, "events_in");
                if (idx != (uint32_t)-1) ui->events_in_port = idx;
                idx = pm->port_index(pm->handle, "events_out");
                if (idx != (uint32_t)-1) ui->events_out_port = idx;
            }
        } else if (strcmp(features[i]->URI, "http://lv2plug.in/ns/ext/urid#map") == 0 && features[i]->data) {
            const LV2_URID_Map *um = (const LV2_URID_Map *)features[i]->data;
            if (um->map) {
                ui->event_transfer = um->map(um->handle, "http://lv2plug.in/ns/ext/atom#eventTransfer");
                ui->ping_type = um->map(um->handle, "urn:kabelsalat:fixture:ping");
                ui->pong_type = um->map(um->handle, "urn:kabelsalat:fixture:pong");
            }
        } else if (strcmp(features[i]->URI, "http://lv2plug.in/ns/ext/instance-access") == 0) {
            inst_handle = (LV2_Handle)features[i]->data;
        } else if (strcmp(features[i]->URI, "http://lv2plug.in/ns/ext/data-access") == 0 && features[i]->data) {
            data_access = (const LV2_Extension_Data_Feature *)features[i]->data;
        }
    }
    if (inst_handle && data_access && data_access->data_access) {
        const FixtureExt *ext = (const FixtureExt *)data_access->data_access("urn:kabelsalat:fixture:ext");
        if (ext && ext->magic && ext->magic(inst_handle) == 0x4B534C54u) {
            ui->direct_ok = 1;
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
    } else if (format == 0 && buffer_size == sizeof(float) && buffer && port_index == ui->meter_port) {
        if (*(const float *)buffer > 0.01f) {
            ui->saw_meter = 1;
        }
    } else if (ui->event_transfer && format == ui->event_transfer && buffer && buffer_size >= 8 && port_index == ui->events_out_port) {
        const LV2_Atom *atom = (const LV2_Atom *)buffer;
        if (atom->type == ui->pong_type) {
            ui->saw_pong = 1;
        }
    }
    if (ui->direct_ok && ui->saw_meter && ui->saw_pong && !ui->emitted_direct_gain && ui->write) {
        ui->emitted_direct_gain = 1;
        float val = 1.25f;
        ui->write(ui->ctrl, ui->mapped_port, sizeof(float), 0, &val);
    }
}

static int ui_show(LV2UI_Handle h) {
    TestUi *ui = (TestUi *)h;
    ui->shown = 1;
    if (ui->write) {
        float init_val = 0.75f;
        ui->write(ui->ctrl, ui->mapped_port, sizeof(float), 0, &init_val);
        if (ui->direct_ok && ui->event_transfer && ui->ping_type) {
            uint8_t atom[12];
            uint32_t payload = 0x12345678u;
            *(uint32_t *)(atom + 0) = 4u;
            *(uint32_t *)(atom + 4) = ui->ping_type;
            memcpy(atom + 8, &payload, 4);
            ui->write(ui->ctrl, ui->events_in_port, sizeof(atom), ui->event_transfer, atom);
        }
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
