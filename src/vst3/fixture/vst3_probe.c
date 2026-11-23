/* Minimal pure-C VST3 fixture plugin bundle (`Kabelsalat Probe Synth` +
 * `Kabelsalat Probe Gain`) for headless unit tests & `kabelsalat vst3-test`.
 * Zero external headers: matches Steinberg VST3 C COM ABI on Linux LP64. */
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef int32_t tresult;
#define kResultOk 0
#define kResultTrue 0
#define kResultFalse 1
#define kInvalidArgument 2
#define kNotImplemented 3
#define kNoInterface (-1)

typedef struct { int32_t left, top, right, bottom; } ViewRect;

typedef struct {
    char vendor[64];
    char url[256];
    char email[128];
    int32_t flags;
} PFactoryInfo;

typedef struct {
    uint8_t cid[16];
    int32_t cardinality;
    char category[32];
    char name[64];
} PClassInfo;

typedef struct {
    uint8_t cid[16];
    int32_t cardinality;
    char category[32];
    char name[64];
    uint32_t classFlags;
    char subCategories[128];
    char vendor[64];
    char version[64];
    char sdkVersion[64];
} PClassInfo2;

typedef struct {
    int32_t mediaType;
    int32_t direction;
    int32_t channelCount;
    uint16_t name[128];
    int32_t busType;
    uint32_t flags;
} BusInfo;

typedef struct {
    uint32_t id;
    uint16_t title[128];
    uint16_t shortTitle[128];
    uint16_t units[128];
    int32_t stepCount;
    double defaultNormalizedValue;
    int32_t unitId;
    int32_t flags;
} ParameterInfo;

typedef struct {
    int32_t processMode;
    int32_t symbolicSampleSize;
    int32_t maxSamplesPerBlock;
    double sampleRate;
} ProcessSetup;

typedef struct {
    int32_t numChannels;
    uint64_t silenceFlags;
    union {
        float **channelBuffers32;
        double **channelBuffers64;
    };
} AudioBusBuffers;

typedef struct { int16_t channel, pitch; float tuning, velocity; int32_t length, noteId; } NoteOnEvent;
typedef struct { int16_t channel, pitch; float velocity; int32_t noteId; float tuning; } NoteOffEvent;
typedef struct { int16_t channel, pitch; float pressure; int32_t noteId; } PolyPressureEvent;
typedef struct { int16_t r, b, m; uint16_t len; const uint16_t *t; } ChordAlignDummy;

typedef struct {
    int32_t busIndex;
    int32_t sampleOffset;
    double ppqPosition;
    uint16_t flags;
    uint16_t type;
    union {
        NoteOnEvent noteOn;
        NoteOffEvent noteOff;
        PolyPressureEvent polyPressure;
        ChordAlignDummy _align;
    };
} Event;

/* Forward-declared COM vtable wrappers */
typedef struct IBStream IBStream;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*read)(IBStream *, void *, int32_t, int32_t *);
    tresult (*write)(IBStream *, void *, int32_t, int32_t *);
    tresult (*seek)(IBStream *, int64_t, int32_t, int64_t *);
    tresult (*tell)(IBStream *, int64_t *);
} IBStreamVtbl;
struct IBStream { const IBStreamVtbl *vtbl; };

typedef struct IEventList IEventList;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    int32_t (*getEventCount)(IEventList *);
    tresult (*getEvent)(IEventList *, int32_t, Event *);
    tresult (*addEvent)(IEventList *, Event *);
} IEventListVtbl;
struct IEventList { const IEventListVtbl *vtbl; };

typedef struct IParamValueQueue IParamValueQueue;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    uint32_t (*getParameterId)(IParamValueQueue *);
    int32_t (*getPointCount)(IParamValueQueue *);
    tresult (*getPoint)(IParamValueQueue *, int32_t, int32_t *, double *);
    tresult (*addPoint)(IParamValueQueue *, int32_t, double, int32_t *);
} IParamValueQueueVtbl;
struct IParamValueQueue { const IParamValueQueueVtbl *vtbl; };

typedef struct IParameterChanges IParameterChanges;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    int32_t (*getParameterCount)(IParameterChanges *);
    IParamValueQueue *(*getParameterData)(IParameterChanges *, int32_t);
    IParamValueQueue *(*addParameterData)(IParameterChanges *, const uint32_t *, int32_t *);
} IParameterChangesVtbl;
struct IParameterChanges { const IParameterChangesVtbl *vtbl; };

typedef struct {
    int32_t processMode;
    int32_t symbolicSampleSize;
    int32_t numSamples;
    int32_t numInputs;
    int32_t numOutputs;
    AudioBusBuffers *inputs;
    AudioBusBuffers *outputs;
    IParameterChanges *inputParameterChanges;
    IParameterChanges *outputParameterChanges;
    IEventList *inputEvents;
    IEventList *outputEvents;
    void *processContext;
} ProcessData;

typedef struct IComponentHandler IComponentHandler;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*beginEdit)(IComponentHandler *, uint32_t);
    tresult (*performEdit)(IComponentHandler *, uint32_t, double);
    tresult (*endEdit)(IComponentHandler *, uint32_t);
    tresult (*restartComponent)(IComponentHandler *, int32_t);
} IComponentHandlerVtbl;
struct IComponentHandler { const IComponentHandlerVtbl *vtbl; };

typedef struct IPlugFrame IPlugFrame;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*resizeView)(IPlugFrame *, void *, ViewRect *);
} IPlugFrameVtbl;
struct IPlugFrame { const IPlugFrameVtbl *vtbl; };

typedef struct IRunLoop IRunLoop;
typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*registerEventHandler)(IRunLoop *, void *, int);
    tresult (*unregisterEventHandler)(IRunLoop *, void *);
    tresult (*registerTimer)(IRunLoop *, void *, uint64_t);
    tresult (*unregisterTimer)(IRunLoop *, void *);
} IRunLoopVtbl;
struct IRunLoop { const IRunLoopVtbl *vtbl; };

/* Exact VST3 Linux big-endian TUIDs matching bindings.rs */
static const uint8_t IID_FUNKNOWN[16]     = {0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xC0,0x00,0x00,0x00,0x00,0x00,0x00,0x46};
static const uint8_t IID_FACTORY[16]      = {0x7A,0x4D,0x81,0x1C,0x52,0x11,0x4A,0x1F,0xAE,0xD9,0xD2,0xEE,0x0B,0x43,0xBF,0x9F};
static const uint8_t IID_FACTORY2[16]     = {0x00,0x07,0xB6,0x50,0xF2,0x4B,0x4C,0x0B,0xA4,0x64,0xED,0xB9,0xF0,0x0B,0x2A,0xBB};
static const uint8_t IID_PLUGINBASE[16]   = {0x22,0x88,0x8D,0xDB,0x15,0x6E,0x45,0xAE,0x83,0x58,0xB3,0x48,0x08,0x19,0x06,0x25};
static const uint8_t IID_COMPONENT[16]    = {0xE8,0x31,0xFF,0x31,0xF2,0xD5,0x43,0x01,0x92,0x8E,0xBB,0xEE,0x25,0x69,0x78,0x02};
static const uint8_t IID_PROCESSOR[16]    = {0x42,0x04,0x3F,0x99,0xB7,0xDA,0x45,0x3C,0xA5,0x69,0xE7,0x9D,0x9A,0xAE,0xC3,0x3D};
static const uint8_t IID_EDITCTRL[16]     = {0xDC,0xD7,0xBB,0xE3,0x77,0x42,0x44,0x8D,0xA8,0x74,0xAA,0xCC,0x97,0x9C,0x75,0x9E};
static const uint8_t IID_MIDIMAP[16]      = {0xDF,0x0F,0xF9,0xF7,0x49,0xB7,0x46,0x69,0xB6,0x3A,0xB7,0x32,0x7A,0xDB,0xF5,0xE5};
static const uint8_t IID_PLUGVIEW[16]     = {0x5B,0xC3,0x25,0x07,0xD0,0x60,0x49,0xEA,0xA6,0x15,0x1B,0x52,0x2B,0x75,0x5B,0x29};
static const uint8_t IID_RUNLOOP[16]      = {0x18,0xC3,0x53,0x66,0x97,0x76,0x4F,0x1A,0x9C,0x5B,0x83,0x85,0x7A,0x87,0x13,0x89};
static const uint8_t IID_TIMERHANDLER[16] = {0x10,0xBD,0xD9,0x4F,0x41,0x42,0x47,0x74,0x82,0x1F,0xAD,0x8F,0xEC,0xA7,0x2C,0xA9};

static const uint8_t CID_SYNTH[16] = {0x4B,0x53,0x56,0x33,0x00,0x01,0x40,0x00,0x80,0x00,0x00,0x00,0x00,0x00,0x00,0x01};
static const uint8_t CID_GAIN[16]  = {0x4B,0x53,0x56,0x33,0x00,0x01,0x40,0x00,0x80,0x00,0x00,0x00,0x00,0x00,0x00,0x02};

static int iid_eq(const void *a, const void *b) { return memcmp(a, b, 16) == 0; }

static void utf16_copy(uint16_t *dst, size_t cap, const char *src) {
    size_t i = 0;
    while (src[i] && i + 1 < cap) { dst[i] = (uint8_t)src[i]; i++; }
    dst[i] = 0;
}

typedef struct ProbePlugin ProbePlugin;
typedef struct { const void *vtbl; ProbePlugin *self; } SubIface;

typedef struct {
    const void *vtbl;
    void *parent_impl;
} ViewSub;

typedef struct ProbePlugView {
    ViewSub iview;
    ViewSub itimer;
    uint32_t refcnt;
    ProbePlugin *plugin;
    IPlugFrame *frame;
    IRunLoop *run_loop;
    int timer_ticks;
    int resized;
} ProbePlugView;

struct ProbePlugin {
    SubIface icomp;
    SubIface iproc;
    SubIface iedit;
    SubIface imidi;
    uint32_t refcnt;
    int is_effect;
    double sample_rate;
    double gain;
    double mode_stepped;
    double program;
    int note_active;
    int note_pitch;
    double phase;
    IComponentHandler *handler;
};

/* --- PlugView + Timer --- */
static void view_unregister_timer(ProbePlugView *v) {
    if (v->run_loop) {
        v->run_loop->vtbl->unregisterTimer(v->run_loop, &v->itimer);
        v->run_loop->vtbl->release(v->run_loop);
        v->run_loop = NULL;
    }
}

static uint32_t view_addref(void *this_) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    return ++v->refcnt;
}

static uint32_t view_release(void *this_) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    if (--v->refcnt == 0) {
        view_unregister_timer(v);
        if (v->frame) { v->frame->vtbl->release(v->frame); v->frame = NULL; }
        free(v);
        return 0;
    }
    return v->refcnt;
}

static tresult view_qi(void *this_, const uint8_t *iid, void **obj) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    if (iid_eq(iid, IID_FUNKNOWN) || iid_eq(iid, IID_PLUGVIEW)) {
        v->refcnt++; *obj = &v->iview; return kResultOk;
    }
    if (iid_eq(iid, IID_TIMERHANDLER)) {
        v->refcnt++; *obj = &v->itimer; return kResultOk;
    }
    *obj = NULL; return kNoInterface;
}

static tresult view_is_platform_supported(void *this_, const char *type) {
    (void)this_;
    return (type && strcmp(type, "X11EmbedWindowID") == 0) ? kResultOk : kResultFalse;
}
static tresult view_attached(void *this_, void *parent, const char *type) {
    (void)this_; (void)parent; (void)type; return kResultOk;
}
static tresult view_removed(void *this_) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    view_unregister_timer(v);
    return kResultOk;
}
static tresult view_on_wheel(void *this_, float d) { (void)this_; (void)d; return kResultOk; }
static tresult view_key_down(void *this_, uint16_t k, int16_t kc, int16_t m) { (void)this_; (void)k; (void)kc; (void)m; return kResultOk; }
static tresult view_key_up(void *this_, uint16_t k, int16_t kc, int16_t m) { (void)this_; (void)k; (void)kc; (void)m; return kResultOk; }
static tresult view_get_size(void *this_, ViewRect *size) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    if (!size) return kInvalidArgument;
    size->left = 0; size->top = 0;
    size->right = v->resized ? 640 : 480;
    size->bottom = v->resized ? 360 : 320;
    return kResultOk;
}
static tresult view_on_size(void *this_, ViewRect *r) { (void)this_; (void)r; return kResultOk; }
static tresult view_on_focus(void *this_, uint8_t st) { (void)this_; (void)st; return kResultOk; }
static tresult view_can_resize(void *this_) { (void)this_; return kResultOk; }
static tresult view_check_size(void *this_, ViewRect *r) { (void)this_; (void)r; return kResultOk; }

static tresult view_set_frame(void *this_, void *frame_ptr) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    view_unregister_timer(v);
    if (v->frame) { v->frame->vtbl->release(v->frame); v->frame = NULL; }
    if (!frame_ptr) return kResultOk;
    IPlugFrame *f = (IPlugFrame *)frame_ptr;
    f->vtbl->addRef(f);
    v->frame = f;
    void *rl_raw = NULL;
    if (f->vtbl->queryInterface(f, IID_RUNLOOP, &rl_raw) == kResultOk && rl_raw) {
        v->run_loop = (IRunLoop *)rl_raw;
        v->run_loop->vtbl->registerTimer(v->run_loop, &v->itimer, 5);
    }
    return kResultOk;
}

static void timer_on_timer(void *this_) {
    ViewSub *s = (ViewSub *)this_;
    ProbePlugView *v = (ProbePlugView *)s->parent_impl;
    v->timer_ticks++;
    if (v->timer_ticks == 1 && v->plugin && v->plugin->handler) {
        v->plugin->handler->vtbl->performEdit(v->plugin->handler, 0, 0.75);
        v->plugin->gain = 0.75;
    }
    if (v->timer_ticks == 2 && v->frame) {
        v->resized = 1;
        ViewRect nr = {0, 0, 640, 360};
        v->frame->vtbl->resizeView(v->frame, &v->iview, &nr);
    }
}

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*isPlatformTypeSupported)(void *, const char *);
    tresult (*attached)(void *, void *, const char *);
    tresult (*removed)(void *);
    tresult (*onWheel)(void *, float);
    tresult (*onKeyDown)(void *, uint16_t, int16_t, int16_t);
    tresult (*onKeyUp)(void *, uint16_t, int16_t, int16_t);
    tresult (*getSize)(void *, ViewRect *);
    tresult (*onSize)(void *, ViewRect *);
    tresult (*onFocus)(void *, uint8_t);
    tresult (*setFrame)(void *, void *);
    tresult (*canResize)(void *);
    tresult (*checkSizeConstraint)(void *, ViewRect *);
} IPlugViewVtbl;

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    void (*onTimer)(void *);
} ITimerHandlerVtbl;

static const IPlugViewVtbl PLUGVIEW_VTBL = {
    view_qi, view_addref, view_release,
    view_is_platform_supported, view_attached, view_removed,
    view_on_wheel, view_key_down, view_key_up,
    view_get_size, view_on_size, view_on_focus,
    view_set_frame, view_can_resize, view_check_size
};

static const ITimerHandlerVtbl TIMER_VTBL = {
    view_qi, view_addref, view_release, timer_on_timer
};

/* --- ProbePlugin COM --- */
static uint32_t plug_addref(void *this_) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    return ++p->refcnt;
}

static uint32_t plug_release(void *this_) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (--p->refcnt == 0) {
        if (p->handler) { p->handler->vtbl->release(p->handler); p->handler = NULL; }
        free(p);
        return 0;
    }
    return p->refcnt;
}

static tresult plug_qi(void *this_, const uint8_t *iid, void **obj) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (iid_eq(iid, IID_FUNKNOWN) || iid_eq(iid, IID_PLUGINBASE) || iid_eq(iid, IID_COMPONENT)) {
        p->refcnt++; *obj = &p->icomp; return kResultOk;
    }
    if (iid_eq(iid, IID_PROCESSOR)) {
        p->refcnt++; *obj = &p->iproc; return kResultOk;
    }
    if (iid_eq(iid, IID_EDITCTRL)) {
        p->refcnt++; *obj = &p->iedit; return kResultOk;
    }
    if (iid_eq(iid, IID_MIDIMAP)) {
        p->refcnt++; *obj = &p->imidi; return kResultOk;
    }
    *obj = NULL; return kNoInterface;
}

static tresult comp_init(void *this_, void *ctx) { (void)this_; (void)ctx; return kResultOk; }
static tresult comp_term(void *this_) { (void)this_; return kResultOk; }
static tresult comp_ctrl_cid(void *this_, uint8_t *cid) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    memcpy(cid, p->is_effect ? CID_GAIN : CID_SYNTH, 16);
    return kResultOk;
}
static tresult comp_set_io(void *this_, int32_t mode) { (void)this_; (void)mode; return kResultOk; }
static int32_t comp_bus_count(void *this_, int32_t media_type, int32_t dir) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (media_type == 0) { /* kAudio */
        if (dir == 0) return p->is_effect ? 1 : 0;
        return 1;
    }
    if (media_type == 1 && dir == 0 && !p->is_effect) return 1; /* kEvent input */
    return 0;
}
static tresult comp_bus_info(void *this_, int32_t media_type, int32_t dir, int32_t index, BusInfo *bus) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (!bus || index != 0) return kInvalidArgument;
    memset(bus, 0, sizeof(*bus));
    bus->mediaType = media_type;
    bus->direction = dir;
    bus->busType = 0; /* kMain */
    bus->flags = 1;   /* kDefaultActive */
    if (media_type == 0) {
        if (dir == 0 && !p->is_effect) return kInvalidArgument;
        bus->channelCount = 2;
        utf16_copy(bus->name, 128, dir == 0 ? "Stereo In" : "Stereo Out");
        return kResultOk;
    }
    if (media_type == 1 && dir == 0 && !p->is_effect) {
        bus->channelCount = 1;
        utf16_copy(bus->name, 128, "MIDI In");
        return kResultOk;
    }
    return kInvalidArgument;
}
static tresult comp_routing(void *this_, void *a, void *b) { (void)this_; (void)a; (void)b; return kNotImplemented; }
static tresult comp_act_bus(void *this_, int32_t t, int32_t d, int32_t idx, uint8_t st) {
    (void)this_; (void)t; (void)d; (void)idx; (void)st; return kResultOk;
}
static tresult comp_set_active(void *this_, uint8_t st) { (void)this_; (void)st; return kResultOk; }

typedef struct {
    char magic[4];
    float gain;
    float mode;
} ProbeStateBlob;

static tresult comp_set_state(void *this_, IBStream *stream) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (!stream) return kInvalidArgument;
    ProbeStateBlob blob;
    int32_t read_n = 0;
    if (stream->vtbl->read(stream, &blob, sizeof(blob), &read_n) != kResultOk || read_n != sizeof(blob)) {
        return kResultFalse;
    }
    if (memcmp(blob.magic, "KPS1", 4) != 0) return kResultFalse;
    p->gain = blob.gain;
    p->mode_stepped = blob.mode;
    return kResultOk;
}

static tresult comp_get_state(void *this_, IBStream *stream) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (!stream) return kInvalidArgument;
    ProbeStateBlob blob = {{'K','P','S','1'}, (float)p->gain, (float)p->mode_stepped};
    int32_t wrote = 0;
    return stream->vtbl->write(stream, &blob, sizeof(blob), &wrote);
}

static tresult proc_set_arr(void *this_, uint64_t *ins, int32_t ni, uint64_t *outs, int32_t no) {
    (void)this_; (void)ins; (void)ni; (void)outs; (void)no; return kResultOk;
}
static tresult proc_get_arr(void *this_, int32_t dir, int32_t idx, uint64_t *arr) {
    (void)this_; (void)dir; (void)idx;
    if (arr) *arr = 3; /* Stereo */
    return kResultOk;
}
static tresult proc_can_size(void *this_, int32_t sz) { (void)this_; return sz == 0 ? kResultOk : kResultFalse; }
static uint32_t proc_latency(void *this_) { (void)this_; return 0; }
static tresult proc_setup(void *this_, ProcessSetup *setup) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (setup) p->sample_rate = setup->sampleRate > 0 ? setup->sampleRate : 48000.0;
    return kResultOk;
}
static tresult proc_set_processing(void *this_, uint8_t st) { (void)this_; (void)st; return kResultOk; }
static uint32_t proc_tail(void *this_) { (void)this_; return 0; }

static tresult proc_process(void *this_, ProcessData *data) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (!data || data->numSamples <= 0) return kResultOk;

    if (data->inputParameterChanges) {
        int32_t pc = data->inputParameterChanges->vtbl->getParameterCount(data->inputParameterChanges);
        for (int32_t i = 0; i < pc; i++) {
            IParamValueQueue *q = data->inputParameterChanges->vtbl->getParameterData(data->inputParameterChanges, i);
            if (!q) continue;
            int32_t pts = q->vtbl->getPointCount(q);
            if (pts > 0) {
                int32_t off = 0; double val = 0.0;
                if (q->vtbl->getPoint(q, pts - 1, &off, &val) == kResultOk) {
                    uint32_t id = q->vtbl->getParameterId(q);
                    if (id == 0) {
                        if (fabs(val - 0.421875) < 1e-6 && data->outputParameterChanges) {
                            double out_val = 0.625;
                            uint32_t out_id = 0;
                            int32_t q_idx = 0;
                            IParamValueQueue *oq = data->outputParameterChanges->vtbl->addParameterData(
                                data->outputParameterChanges, &out_id, &q_idx);
                            if (oq) {
                                int32_t p_idx = 0;
                                oq->vtbl->addPoint(oq, 0, out_val, &p_idx);
                            }
                            p->gain = out_val;
                        } else {
                            p->gain = val;
                        }
                    }
                    else if (id == 1) p->mode_stepped = val;
                    else if (id == 2) p->program = val;
                }
            }
        }
    }

    if (data->inputEvents && !p->is_effect) {
        int32_t ec = data->inputEvents->vtbl->getEventCount(data->inputEvents);
        for (int32_t i = 0; i < ec; i++) {
            Event ev;
            if (data->inputEvents->vtbl->getEvent(data->inputEvents, i, &ev) == kResultOk) {
                if (ev.type == 0) { /* kNoteOnEvent */
                    p->note_active = ev.noteOn.velocity > 0.0f ? 1 : 0;
                    p->note_pitch = ev.noteOn.pitch;
                } else if (ev.type == 1) { /* kNoteOffEvent */
                    p->note_active = 0;
                }
            }
        }
    }

    if (data->numOutputs < 1 || !data->outputs || data->outputs[0].numChannels < 2) return kResultOk;
    float *out_l = data->outputs[0].channelBuffers32[0];
    float *out_r = data->outputs[0].channelBuffers32[1];
    float g = (float)p->gain;

    if (p->is_effect) {
        if (data->numInputs >= 1 && data->inputs && data->inputs[0].numChannels >= 2) {
            const float *in_l = data->inputs[0].channelBuffers32[0];
            const float *in_r = data->inputs[0].channelBuffers32[1];
            for (int32_t i = 0; i < data->numSamples; i++) {
                out_l[i] = in_l[i] * g;
                out_r[i] = in_r[i] * g;
            }
        } else {
            memset(out_l, 0, (size_t)data->numSamples * sizeof(float));
            memset(out_r, 0, (size_t)data->numSamples * sizeof(float));
        }
    } else if (p->note_active) {
        double freq = 440.0 * pow(2.0, ((double)p->note_pitch - 69.0) / 12.0);
        double step = 2.0 * 3.141592653589793 * freq / p->sample_rate;
        for (int32_t i = 0; i < data->numSamples; i++) {
            float s = (float)sin(p->phase) * 0.5f * g;
            p->phase += step;
            out_l[i] = s;
            out_r[i] = s;
        }
    } else {
        memset(out_l, 0, (size_t)data->numSamples * sizeof(float));
        memset(out_r, 0, (size_t)data->numSamples * sizeof(float));
    }
    return kResultOk;
}

static tresult edit_set_comp_state(void *this_, IBStream *st) { return comp_set_state(this_, st); }
static tresult edit_set_state(void *this_, IBStream *st) { (void)this_; (void)st; return kResultOk; }
static tresult edit_get_state(void *this_, IBStream *st) { (void)this_; (void)st; return kResultOk; }
static int32_t edit_param_count(void *this_) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    return p->is_effect ? 1 : 3;
}
static tresult edit_param_info(void *this_, int32_t idx, ParameterInfo *info) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (!info || idx < 0 || idx >= (p->is_effect ? 1 : 3)) return kInvalidArgument;
    memset(info, 0, sizeof(*info));
    if (idx == 0) {
        info->id = 0;
        utf16_copy(info->title, 128, "Gain");
        utf16_copy(info->shortTitle, 128, "Gain");
        utf16_copy(info->units, 128, "x");
        info->stepCount = 0;
        info->defaultNormalizedValue = 1.0;
        info->flags = 1; /* kCanAutomate */
    } else if (idx == 1) {
        info->id = 1;
        utf16_copy(info->title, 128, "Wave");
        utf16_copy(info->shortTitle, 128, "Wave");
        info->stepCount = 2;
        info->defaultNormalizedValue = 0.0;
        info->flags = 1 | 8; /* kCanAutomate | kIsList */
    } else {
        info->id = 2;
        utf16_copy(info->title, 128, "Program");
        utf16_copy(info->shortTitle, 128, "Prog");
        info->stepCount = 3;
        info->defaultNormalizedValue = 0.0;
        info->flags = 1 | 8 | (1 << 15); /* kCanAutomate | kIsList | kIsProgramChange */
    }
    return kResultOk;
}
static tresult edit_val_to_str(void *this_, uint32_t id, double val, uint16_t str[128]) {
    (void)this_;
    if (id == 1) {
        if (val < 0.25) utf16_copy(str, 128, "Sine");
        else if (val < 0.75) utf16_copy(str, 128, "Tri");
        else utf16_copy(str, 128, "Saw");
        return kResultOk;
    }
    if (id == 2) {
        int idx = (int)llround(val * 3.0);
        if (idx <= 0) utf16_copy(str, 128, "Init");
        else if (idx == 1) utf16_copy(str, 128, "Brass");
        else if (idx == 2) utf16_copy(str, 128, "Pad");
        else utf16_copy(str, 128, "Lead");
        return kResultOk;
    }
    /* Format integer percent cleanly without needing stdio float locale issues */
    int pct = (int)llround(val * 100.0);
    char buf[32];
    int n = 0;
    if (pct < 0) { buf[n++] = '-'; pct = -pct; }
    if (pct >= 100) buf[n++] = (char)('0' + (pct / 100) % 10);
    if (pct >= 10) buf[n++] = (char)('0' + (pct / 10) % 10);
    buf[n++] = (char)('0' + pct % 10);
    buf[n++] = '%';
    buf[n] = 0;
    utf16_copy(str, 128, buf);
    return kResultOk;
}
static tresult edit_str_to_val(void *this_, uint32_t id, uint16_t *str, double *val) {
    (void)this_; (void)id; (void)str; (void)val; return kResultFalse;
}
static double edit_norm_to_plain(void *this_, uint32_t id, double v) { (void)this_; (void)id; return v; }
static double edit_plain_to_norm(void *this_, uint32_t id, double v) { (void)this_; (void)id; return v; }
static double edit_get_param(void *this_, uint32_t id) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (id == 0) return p->gain;
    if (id == 1) return p->mode_stepped;
    if (id == 2) return p->program;
    return 0.0;
}
static tresult edit_set_param(void *this_, uint32_t id, double v) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (id == 0) { p->gain = v; return kResultOk; }
    if (id == 1) { p->mode_stepped = v; return kResultOk; }
    if (id == 2) { p->program = v; return kResultOk; }
    return kInvalidArgument;
}
static tresult edit_set_handler(void *this_, IComponentHandler *h) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (p->handler) p->handler->vtbl->release(p->handler);
    p->handler = h;
    if (h) h->vtbl->addRef(h);
    return kResultOk;
}
static void *edit_create_view(void *this_, const char *name) {
    ProbePlugin *p = ((SubIface *)this_)->self;
    if (!name || strcmp(name, "editor") != 0) return NULL;
    ProbePlugView *v = (ProbePlugView *)calloc(1, sizeof(ProbePlugView));
    v->iview.vtbl = &PLUGVIEW_VTBL;
    v->iview.parent_impl = v;
    v->itimer.vtbl = &TIMER_VTBL;
    v->itimer.parent_impl = v;
    v->refcnt = 1;
    v->plugin = p;
    return &v->iview;
}
static tresult midi_get_assign(void *this_, int32_t bus, int16_t ch, int16_t cc, uint32_t *id) {
    (void)this_;
    if (bus == 0 && ch == 0 && cc == 7 && id) { *id = 0; return kResultOk; }
    return kResultFalse;
}

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*initialize)(void *, void *);
    tresult (*terminate)(void *);
    tresult (*getControllerClassId)(void *, uint8_t *);
    tresult (*setIoMode)(void *, int32_t);
    int32_t (*getBusCount)(void *, int32_t, int32_t);
    tresult (*getBusInfo)(void *, int32_t, int32_t, int32_t, BusInfo *);
    tresult (*getRoutingInfo)(void *, void *, void *);
    tresult (*activateBus)(void *, int32_t, int32_t, int32_t, uint8_t);
    tresult (*setActive)(void *, uint8_t);
    tresult (*setState)(void *, IBStream *);
    tresult (*getState)(void *, IBStream *);
} IComponentVtbl;

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*setBusArrangements)(void *, uint64_t *, int32_t, uint64_t *, int32_t);
    tresult (*getBusArrangement)(void *, int32_t, int32_t, uint64_t *);
    tresult (*canProcessSampleSize)(void *, int32_t);
    uint32_t (*getLatencySamples)(void *);
    tresult (*setupProcessing)(void *, ProcessSetup *);
    tresult (*setProcessing)(void *, uint8_t);
    tresult (*process)(void *, ProcessData *);
    uint32_t (*getTailSamples)(void *);
} IAudioProcessorVtbl;

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*initialize)(void *, void *);
    tresult (*terminate)(void *);
    tresult (*setComponentState)(void *, IBStream *);
    tresult (*setState)(void *, IBStream *);
    tresult (*getState)(void *, IBStream *);
    int32_t (*getParameterCount)(void *);
    tresult (*getParameterInfo)(void *, int32_t, ParameterInfo *);
    tresult (*getParamStringByValue)(void *, uint32_t, double, uint16_t *);
    tresult (*getParamValueByString)(void *, uint32_t, uint16_t *, double *);
    double (*normalizedParamToPlain)(void *, uint32_t, double);
    double (*plainParamToNormalized)(void *, uint32_t, double);
    double (*getParamNormalized)(void *, uint32_t);
    tresult (*setParamNormalized)(void *, uint32_t, double);
    tresult (*setComponentHandler)(void *, IComponentHandler *);
    void *(*createView)(void *, const char *);
} IEditControllerVtbl;

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*getMidiControllerAssignment)(void *, int32_t, int16_t, int16_t, uint32_t *);
} IMidiMappingVtbl;

static const IComponentVtbl COMP_VTBL = {
    plug_qi, plug_addref, plug_release, comp_init, comp_term,
    comp_ctrl_cid, comp_set_io, comp_bus_count, comp_bus_info,
    comp_routing, comp_act_bus, comp_set_active, comp_set_state, comp_get_state
};
static const IAudioProcessorVtbl PROC_VTBL = {
    plug_qi, plug_addref, plug_release, proc_set_arr, proc_get_arr,
    proc_can_size, proc_latency, proc_setup, proc_set_processing,
    proc_process, proc_tail
};
static const IEditControllerVtbl EDIT_VTBL = {
    plug_qi, plug_addref, plug_release, comp_init, comp_term,
    edit_set_comp_state, edit_set_state, edit_get_state, edit_param_count,
    edit_param_info, edit_val_to_str, edit_str_to_val, edit_norm_to_plain,
    edit_plain_to_norm, edit_get_param, edit_set_param, edit_set_handler, edit_create_view
};
static const IMidiMappingVtbl MIDI_VTBL = {
    plug_qi, plug_addref, plug_release, midi_get_assign
};

/* --- Factory --- */
typedef struct {
    const void *vtbl;
    uint32_t refcnt;
} ProbeFactory;

static tresult fac_qi(void *this_, const uint8_t *iid, void **obj) {
    ProbeFactory *f = (ProbeFactory *)this_;
    if (iid_eq(iid, IID_FUNKNOWN) || iid_eq(iid, IID_FACTORY) || iid_eq(iid, IID_FACTORY2)) {
        f->refcnt++; *obj = f; return kResultOk;
    }
    *obj = NULL; return kNoInterface;
}
static uint32_t fac_addref(void *this_) { return ++((ProbeFactory *)this_)->refcnt; }
static uint32_t fac_release(void *this_) { return --((ProbeFactory *)this_)->refcnt; }
static tresult fac_info(void *this_, PFactoryInfo *info) {
    (void)this_;
    memset(info, 0, sizeof(*info));
    strcpy(info->vendor, "Kabelsalat");
    strcpy(info->url, "https://github.com/ssalentin/kabelsalat");
    return kResultOk;
}
static int32_t fac_count(void *this_) { (void)this_; return 2; }
static tresult fac_class_info2(void *this_, int32_t idx, PClassInfo2 *info) {
    (void)this_;
    memset(info, 0, sizeof(*info));
    info->cardinality = 0x7FFFFFFF;
    strcpy(info->category, "Audio Module Class");
    strcpy(info->vendor, "Kabelsalat");
    strcpy(info->version, "1.0.0");
    strcpy(info->sdkVersion, "VST 3.8.0");
    if (idx == 0) {
        memcpy(info->cid, CID_SYNTH, 16);
        strcpy(info->name, "Kabelsalat Probe Synth");
        strcpy(info->subCategories, "Instrument|Synth");
        return kResultOk;
    }
    if (idx == 1) {
        memcpy(info->cid, CID_GAIN, 16);
        strcpy(info->name, "Kabelsalat Probe Gain");
        strcpy(info->subCategories, "Fx|Tools");
        return kResultOk;
    }
    return kInvalidArgument;
}
static tresult fac_class_info(void *this_, int32_t idx, PClassInfo *info) {
    PClassInfo2 i2;
    if (fac_class_info2(this_, idx, &i2) != kResultOk) return kInvalidArgument;
    memcpy(info->cid, i2.cid, 16);
    info->cardinality = i2.cardinality;
    memcpy(info->category, i2.category, 32);
    memcpy(info->name, i2.name, 64);
    return kResultOk;
}
static tresult fac_create(void *this_, const char *cid, const char *iid, void **obj) {
    (void)this_;
    int is_fx = -1;
    if (iid_eq(cid, CID_SYNTH)) is_fx = 0;
    else if (iid_eq(cid, CID_GAIN)) is_fx = 1;
    else { *obj = NULL; return kNoInterface; }

    ProbePlugin *p = (ProbePlugin *)calloc(1, sizeof(ProbePlugin));
    p->icomp.vtbl = &COMP_VTBL; p->icomp.self = p;
    p->iproc.vtbl = &PROC_VTBL; p->iproc.self = p;
    p->iedit.vtbl = &EDIT_VTBL; p->iedit.self = p;
    p->imidi.vtbl = &MIDI_VTBL; p->imidi.self = p;
    p->refcnt = 1;
    p->is_effect = is_fx;
    p->sample_rate = 48000.0;
    p->gain = 1.0;

    tresult r = plug_qi(&p->icomp, (const uint8_t *)iid, obj);
    plug_release(&p->icomp);
    return r;
}

typedef struct {
    tresult (*queryInterface)(void *, const uint8_t *, void **);
    uint32_t (*addRef)(void *);
    uint32_t (*release)(void *);
    tresult (*getFactoryInfo)(void *, PFactoryInfo *);
    int32_t (*countClasses)(void *);
    tresult (*getClassInfo)(void *, int32_t, PClassInfo *);
    tresult (*createInstance)(void *, const char *, const char *, void **);
    tresult (*getClassInfo2)(void *, int32_t, PClassInfo2 *);
} IPluginFactory2Vtbl;

static const IPluginFactory2Vtbl FACTORY_VTBL = {
    fac_qi, fac_addref, fac_release,
    fac_info, fac_count, fac_class_info, fac_create, fac_class_info2
};
static ProbeFactory G_FACTORY = { &FACTORY_VTBL, 1 };

__attribute__((visibility("default")))
int ModuleEntry(void *lib) { (void)lib; return 1; }

__attribute__((visibility("default")))
int ModuleExit(void) { return 1; }

__attribute__((visibility("default")))
void *GetPluginFactory(void) {
    G_FACTORY.refcnt++;
    return &G_FACTORY;
}
