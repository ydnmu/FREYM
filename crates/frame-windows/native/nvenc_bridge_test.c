/* Standalone fault harness: includes the production bridge; no GPU or DLL loaded.
 * Run with python crates/frame-windows/native/verify_bridge.py. */
#include "nvenc_bridge.c"
#include <assert.h>
static int fail_unmap, fail_registration, fail_buffer, fail_event, fail_destroy, fail_unlock;
static unsigned unmaps, registrations, buffers, events, destroys, locks, unlocks;
static NVENCSTATUS NVENCAPI test_unmap(void *e, NV_ENC_INPUT_PTR p) { (void)e;(void)p; ++unmaps; return fail_unmap ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI test_registration(void *e, NV_ENC_REGISTERED_PTR p) { (void)e;(void)p; ++registrations; return fail_registration ? NV_ENC_ERR_INVALID_PARAM : NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI test_buffer(void *e, NV_ENC_OUTPUT_PTR p) { (void)e;(void)p; ++buffers; return fail_buffer ? NV_ENC_ERR_OUT_OF_MEMORY : NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI test_event(void *e, NV_ENC_EVENT_PARAMS *p) { (void)e;(void)p; ++events; return fail_event ? NV_ENC_ERR_INVALID_EVENT : NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI test_destroy(void *e) { (void)e; ++destroys; return fail_destroy ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI test_lock(void *e, NV_ENC_LOCK_BITSTREAM *p) { (void)e;(void)p; ++locks; return NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI test_unlock(void *e, NV_ENC_OUTPUT_PTR p) { (void)e;(void)p; ++unlocks; return fail_unlock ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS; }
static FrameNvencSession *make_session(void) {
    FrameNvencSession *s = calloc(1, sizeof(*s)); assert(s);
    s->library = &frame_nvenc_library; s->encoder = (void *)(uintptr_t)1; s->initialized = 1;
    s->witness_id = frame_witness_create("test/session"); assert(s->witness_id);
    s->outputs[0] = (void *)(uintptr_t)2;
    s->events[0] = CreateEventW(NULL, TRUE, FALSE, NULL); assert(s->events[0]);
    s->event_registered[0] = 1;
    s->eos_event = CreateEventW(NULL, TRUE, FALSE, NULL); assert(s->eos_event); s->eos_registered = 1;
    s->inputs[0].used = 1; s->inputs[0].registered = (void *)(uintptr_t)3;
    return s;
}
static void clear_counts(void) { unmaps=registrations=buffers=events=destroys=locks=unlocks=0; }
static int live(void *event) { DWORD flags; return GetHandleInformation(event, &flags) != 0; }
int main(void) {
    FrameNvencSession *s; FrameNvencOutcome result; uint64_t started; void *event, *eos;
    frame_nvenc_library.functions.nvEncUnmapInputResource=test_unmap;
    frame_nvenc_library.functions.nvEncUnregisterResource=test_registration;
    frame_nvenc_library.functions.nvEncDestroyBitstreamBuffer=test_buffer;
    frame_nvenc_library.functions.nvEncUnregisterAsyncEvent=test_event;
    frame_nvenc_library.functions.nvEncDestroyEncoder=test_destroy;
    frame_nvenc_library.functions.nvEncLockBitstream=test_lock;
    frame_nvenc_library.functions.nvEncUnlockBitstream=test_unlock;
    /* Two never-signaled outputs share one real 5s monotonic deadline. */
    s=make_session(); s->output_in_use[0]=1; s->output_in_use[1]=1;
    s->events[1]=CreateEventW(NULL,TRUE,FALSE,NULL); s->event_registered[1]=1;
    s->outputs[1]=(void *)(uintptr_t)4;
    started=GetTickCount64();
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(GetTickCount64()-started < 8000u);
    assert(!registrations && !buffers && !events && !destroys && live(s->events[0]));
    SetEvent(s->events[0]); SetEvent(s->events[1]);
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_OK);
    assert(locks==2 && unlocks==2 && destroys==1);
    puts("PASS two pending outputs: one deadline, no early release, delayed recovery");
    /* Orphan mapping left by failed submission must survive a failed unmap. */
    clear_counts(); s=make_session(); s->inputs[0].mapped=(void *)(uintptr_t)5; fail_unmap=1;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(s->inputs[0].mapped && !registrations && !buffers && !destroys);
    fail_unmap=0; assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_OK);
    assert(unmaps==2 && registrations==1 && destroys==1);
    puts("PASS failed unmap retains mapping and dependencies");
    clear_counts(); s=make_session(); fail_registration=1;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(s->inputs[0].used && !buffers && !events && !destroys);
    fail_registration=0; assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_OK);
    assert(registrations==2 && destroys==1);
    puts("PASS failed registration retains dependency graph");
    /* Unequal child errors and destroy errors, released stages never repeat. */
    clear_counts(); s=make_session(); event=s->events[0]; eos=s->eos_event;
    fail_buffer=fail_event=fail_destroy=1;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(result.status==NV_ENC_ERR_OUT_OF_MEMORY && s->outputs[0] && live(event) && live(eos) && !events && !destroys);
    fail_buffer=0;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(result.status==NV_ENC_ERR_INVALID_EVENT && !s->outputs[0] && live(event) && live(eos) && !destroys);
    fail_event=0;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(result.status==NV_ENC_ERR_GENERIC && s->encoder && !live(event) && !live(eos));
    fail_destroy=0; assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_OK);
    assert(registrations==1 && buffers==2 && events==4 && destroys==2);
    puts("PASS combined failures: buffer/event/EOS/encoder handles, statuses and exactly-once successful release");
    clear_counts(); s=make_session(); s->output_in_use[0]=1; SetEvent(s->events[0]); fail_unlock=1;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(result.status==NV_ENC_ERR_GENERIC && s->output_locked[0] && !buffers && !destroys);
    fail_unlock=0; assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_OK);
    assert(locks==1 && unlocks==2 && destroys==1);
    puts("PASS failed bitstream unlock retries unlock without locking twice");
    clear_counts(); s=make_session(); s->output_in_use[0]=1; s->inputs[0].mapped=(void *)(uintptr_t)5; SetEvent(s->events[0]); fail_unmap=1;
    assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_CLEANUP_UNSAFE);
    assert(result.status==NV_ENC_ERR_GENERIC && s->inputs[0].mapped && s->output_complete[0] && !registrations);
    fail_unmap=0; assert(frame_nvenc_session_close(s,&result)==FRAME_NVENC_OK);
    assert(locks==1 && unlocks==1 && unmaps==2 && destroys==1);
    puts("PASS completed output with failed unmap retains its status and retries only unmap");
    return 0;
}
