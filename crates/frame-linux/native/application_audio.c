// SPDX-License-Identifier: GPL-3.0-or-later
// Optional PulseAudio protocol adapter, also usable with PipeWire-Pulse.
// All calls run on the recording control thread; no media callback allocates.
#define _POSIX_C_SOURCE 200809L
#include <pulse/pulseaudio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdio.h>

typedef struct {
    uint32_t index, cookie, client, pid, channels;
    char name[256];
} FreymApp;
typedef struct {
    pa_mainloop *loop;
    pa_context *context;
    pa_stream *stream;
    FreymApp *apps;
    size_t capacity, count;
    uint32_t selected, monitor;
    uint32_t sink_index;
    FreymApp expected;
    pa_channel_map map;
    int done, failed, found, draining, corked, has_selection, info_pending;
    int64_t first_ns;
    uint64_t frames;
} FreymPulse;

static int64_t monotonic_ns(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (int64_t)now.tv_sec * 1000000000 + now.tv_nsec;
}
static int pump(FreymPulse *p) {
    for (int i = 0; i < 32; ++i) {
        if (pa_mainloop_prepare(p->loop, 0) < 0 || pa_mainloop_poll(p->loop) < 0)
            return -1;
        int dispatched = pa_mainloop_dispatch(p->loop);
        if (dispatched < 0) return -1;
        if (!dispatched) break;
    }
    pa_context_state_t state = pa_context_get_state(p->context);
    return PA_CONTEXT_IS_GOOD(state) ? 0 : -1;
}
static int wait_ready(FreymPulse *p, int stream) {
    int64_t deadline = monotonic_ns() + 3000000000;
    do {
        if (pump(p) < 0 || p->failed) return -1;
        if (stream) {
            pa_stream_state_t state = pa_stream_get_state(p->stream);
            if (state == PA_STREAM_READY) return 0;
            if (!PA_STREAM_IS_GOOD(state)) return -1;
        } else if (pa_context_get_state(p->context) == PA_CONTEXT_READY) return 0;
        struct timespec delay = {0, 1000000};
        nanosleep(&delay, NULL);
    } while (monotonic_ns() < deadline);
    return -1;
}
void freym_pulse_close(FreymPulse *p) {
    if (!p) return;
    if (p->stream) { pa_stream_disconnect(p->stream); pa_stream_unref(p->stream); }
    if (p->context) { pa_context_disconnect(p->context); pa_context_unref(p->context); }
    if (p->loop) pa_mainloop_free(p->loop);
    free(p);
}
static FreymPulse *open_context(void) {
    FreymPulse *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->monitor = PA_INVALID_INDEX;
    p->sink_index = PA_INVALID_INDEX;
    p->first_ns = -1;
    p->loop = pa_mainloop_new();
    if (!p->loop) goto fail;
    p->context = pa_context_new(pa_mainloop_get_api(p->loop), "FREYM application capture");
    if (!p->context || pa_context_connect(p->context, NULL, PA_CONTEXT_NOAUTOSPAWN, NULL) < 0 || wait_ready(p, 0) < 0) goto fail;
    return p;
fail:
    freym_pulse_close(p);
    return NULL;
}
static void input_info(pa_context *c, const pa_sink_input_info *info, int eol, void *data) {
    (void)c;
    FreymPulse *p = data;
    if (eol) { p->done = 1; if (eol < 0) p->failed = 1; return; }
    if (!info || info->client == PA_INVALID_INDEX || info->sample_spec.channels > 2 || !info->sample_spec.channels) return;
    pa_channel_map supported_map;
    if (info->sample_spec.channels == 1) pa_channel_map_init_mono(&supported_map);
    else pa_channel_map_init_stereo(&supported_map);
    if (!pa_channel_map_equal(&info->channel_map, &supported_map)) {
        if (p->has_selection && info->index == p->selected) p->failed = 1;
        return;
    }
    const char *pid = pa_proplist_gets(info->proplist, PA_PROP_APPLICATION_PROCESS_ID);
    if (!pid) return;
    char *end;
    unsigned long parsed = strtoul(pid, &end, 10);
    if (*end || !parsed || parsed > UINT32_MAX) return;
    if (p->has_selection && info->index == p->selected &&
        (info->client != p->expected.client || parsed != p->expected.pid ||
         info->sample_spec.channels != p->expected.channels ||
         (p->sink_index != PA_INVALID_INDEX && info->sink != p->sink_index))) {
        p->failed = 1; return;
    }
    if (p->apps) {
        if (p->count == p->capacity) { p->failed = 1; return; }
        FreymApp *a = &p->apps[p->count++];
        a->index = info->index; a->client = info->client; a->pid = (uint32_t)parsed;
        a->cookie = 0; // filled with the server cookie after enumeration
        a->channels = info->sample_spec.channels;
        const char *name = pa_proplist_gets(info->proplist, PA_PROP_APPLICATION_NAME);
        if (!name) name = info->name;
        if (name) { strncpy(a->name, name, sizeof(a->name)-1); a->name[255] = 0; }
    }
    if (info->index == p->selected) { p->found = 1; p->monitor = info->sink; p->map = info->channel_map; }
}
static void server_info(pa_context *c, const pa_server_info *info, void *data) {
    (void)c;
    FreymPulse *p = data;
    if (!info) { p->failed = 1; } else p->selected = info->cookie;
    p->done = 1;
}
static void sink_info(pa_context *c, const pa_sink_info *info, int eol, void *data) {
    (void)c;
    FreymPulse *p = data;
    if (eol) { p->done = 1; if (eol < 0) p->failed = 1; return; }
    if (info) p->monitor = info->monitor_source;
}
static int operation(FreymPulse *p, pa_operation *op) {
    if (!op) return -1;
    int64_t deadline = monotonic_ns() + 3000000000;
    while (!p->done && !p->failed && monotonic_ns() < deadline) {
        if (pump(p) < 0) { p->failed = 1; break; }
        struct timespec delay = {0, 1000000}; nanosleep(&delay, NULL);
    }
    pa_operation_cancel(op); pa_operation_unref(op);
    return p->done && !p->failed ? 0 : -1;
}
static int enumerate(FreymPulse *p, FreymApp *apps, size_t capacity) {
    p->done = 0;
    if (operation(p, pa_context_get_server_info(p->context, server_info, p)) < 0) return -1;
    uint32_t cookie = p->selected;
    p->done = 0; p->apps = apps; p->capacity = capacity;
    int result = operation(p, pa_context_get_sink_input_info_list(p->context, input_info, p));
    for (size_t i=0; i<p->count; ++i) apps[i].cookie = cookie;
    p->apps = NULL;
    return result;
}
int freym_pulse_list(FreymApp *apps, size_t capacity) {
    FreymPulse *p = open_context(); if (!p) return -1;
    int result = enumerate(p, apps, capacity);
    int count = (int)p->count;
    freym_pulse_close(p);
    return result < 0 ? -1 : count;
}
static void changed_input(pa_context *c, const pa_sink_input_info *info, int eol, void *data) {
    FreymPulse *p = data;
    if (eol) {
        p->info_pending = 0;
        if (eol < 0 || !p->found) p->failed = 1;
        return;
    }
    input_info(c, info, 0, data);
}
static void subscribed(pa_context *c, int success, void *data) {
    (void)c;
    FreymPulse *p = data;
    p->done = 1;
    if (!success) p->failed = 1;
}
static void overflow(pa_stream *stream, void *data) {
    (void)stream;
    // Never bridge an unreported lost interval with a fabricated contiguous clock.
    ((FreymPulse *)data)->failed = 1;
}
static void subscription(pa_context *c, pa_subscription_event_type_t event, uint32_t index, void *data) {
    (void)c;
    FreymPulse *p=data;
    if ((event & PA_SUBSCRIPTION_EVENT_FACILITY_MASK) == PA_SUBSCRIPTION_EVENT_SINK_INPUT && index == p->selected) {
        // Removal ends this exact generation. A changed sink also ends capture;
        // changes to playback volume/mute must not terminate independent capture.
        if ((event & PA_SUBSCRIPTION_EVENT_TYPE_MASK) == PA_SUBSCRIPTION_EVENT_REMOVE)
            p->failed = 1;
        else if (!p->info_pending) {
            p->info_pending = 1;
            p->found = 0;
            pa_operation *op = pa_context_get_sink_input_info(c, index, changed_input, p);
            if (op) pa_operation_unref(op); else p->failed = 1;
        }
    }
}
FreymPulse *freym_pulse_open(const FreymApp *selected) {
    FreymPulse *p = open_context(); if (!p) return NULL;
    FreymApp *apps = calloc(1, 1024*1024);
    if (!apps) goto fail;
    int found=0;
    if (enumerate(p, apps, (1024*1024)/sizeof(*apps)) == 0) {
        for (size_t i=0;i<p->count;++i) if (apps[i].index==selected->index && apps[i].cookie==selected->cookie && apps[i].client==selected->client && apps[i].pid==selected->pid && apps[i].channels==selected->channels) found=1;
    }
    free(apps); if (!found) goto fail;
    p->selected=selected->index; p->expected=*selected; p->has_selection=1;
    p->done=0;
    pa_context_set_subscribe_callback(p->context,subscription,p);
    if (operation(p, pa_context_subscribe(p->context,PA_SUBSCRIPTION_MASK_SINK_INPUT,subscribed,p)) < 0) goto fail;
    p->done=0; p->found=0;
    if (operation(p, pa_context_get_sink_input_info(p->context, p->selected, input_info, p)) < 0 || !p->found) goto fail;
    uint32_t sink=p->monitor; p->sink_index=sink; p->done=0; p->monitor=PA_INVALID_INDEX;
    if (operation(p, pa_context_get_sink_info_by_index(p->context, sink, sink_info, p)) < 0 || p->monitor==PA_INVALID_INDEX) goto fail;
    pa_sample_spec spec={ PA_SAMPLE_FLOAT32LE, 48000, (uint8_t)selected->channels };
    p->stream=pa_stream_new(p->context,"Isolated application PCM",&spec,&p->map);
    if (!p->stream || pa_stream_set_monitor_stream(p->stream, p->selected)<0) goto fail;
    pa_stream_set_overflow_callback(p->stream,overflow,p);
    char device[32]; snprintf(device,sizeof(device),"%u",p->monitor);
    pa_buffer_attr attr={ .maxlength=131072, .tlength=(uint32_t)-1, .prebuf=(uint32_t)-1, .minreq=(uint32_t)-1, .fragsize=480*selected->channels*4 };
    if (pa_stream_connect_record(p->stream,device,&attr,PA_STREAM_DONT_MOVE|PA_STREAM_AUTO_TIMING_UPDATE)<0 || wait_ready(p,1)<0) goto fail;
    return p;
fail: freym_pulse_close(p); return NULL;
}
// 0=no packet, 1=PCM, 2=drained, -1=lost/error. Caller owns fixed scratch.
int freym_pulse_read(FreymPulse *p,float *samples,size_t capacity,size_t *count,int64_t *pts_ns) {
    if (pump(p)<0 || p->failed || pa_stream_get_state(p->stream)!=PA_STREAM_READY) return -1;
    size_t available=pa_stream_readable_size(p->stream);
    if (available==(size_t)-1) return -1;
    if (!available) return p->draining && p->corked ? 2 : 0;
    const void *data; size_t bytes;
    if (pa_stream_peek(p->stream,&data,&bytes)<0) return -1;
    const pa_sample_spec *spec=pa_stream_get_sample_spec(p->stream);
    if (!bytes || bytes%((size_t)spec->channels*4) || bytes/4>capacity) return -1;
    uint64_t frames=bytes/(spec->channels*4);
    if (p->first_ns<0) {
        pa_usec_t latency; int negative=0;
        if (pa_stream_get_latency(p->stream,&latency,&negative)<0) return 0;
        if (latency > INT64_MAX / 1000) return -1;
        p->first_ns=monotonic_ns()-(negative ? 0 : (int64_t)latency*1000);
    }
    // Divide first so recordings beyond four days cannot overflow the frame clock.
    uint64_t seconds=p->frames/48000;
    if (seconds > INT64_MAX/1000000000) return -1;
    uint64_t elapsed=seconds*1000000000+(p->frames%48000)*1000000000/48000;
    if (p->first_ns < 0 || elapsed > (uint64_t)(INT64_MAX-p->first_ns)) return -1;
    *pts_ns=p->first_ns+(int64_t)elapsed;
    *count=bytes/4;
    if (data) memcpy(samples,data,bytes); else memset(samples,0,bytes);
    if (pa_stream_drop(p->stream)<0) return -1;
    p->frames+=frames;
    return 1;
}
int64_t freym_pulse_now(void) { return monotonic_ns(); }
static void cork_done(pa_stream *s, int success, void *data) {
    (void)s;
    FreymPulse *p=data;
    if (success) p->corked=1; else p->failed=1;
}
void freym_pulse_drain(FreymPulse *p) {
    if (p->draining) return;
    p->draining=1;
    pa_operation *op=pa_stream_cork(p->stream,1,cork_done,p);
    if (op) pa_operation_unref(op); else p->failed=1;
}
