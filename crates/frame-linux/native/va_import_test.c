/* Short hardware-free boundary checks. GStreamer memory/metadata/fd ownership
 * is real; only DMA reservation fences, PRIME import and GPU sync are mocked.
 * Link with --export-dynamic,--wrap=ioctl,
 * --wrap=gst_va_dmabuf_memories_setup,--wrap=vaSyncSurface2. */
#define _GNU_SOURCE
#define GST_USE_UNSTABLE_API
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/dma-buf.h>
#include <stdarg.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <unistd.h>
#include <gst/allocators/gstdmabuf.h>
#include <gst/va/gstvaallocator.h>
#include <gst/va/gstvadisplay.h>
#include <va/va_drmcommon.h>

int freym_va_import_buffer(GstObject *, GstCaps *, GstBuffer *, GstBuffer **);
int freym_va_sync_buffer(GstObject *, GstBuffer *);
static gboolean import_ok = TRUE, fence_ok = TRUE;
static VAStatus sync_status = VA_STATUS_SUCCESS;
static int last_fence = -1;
static guint imports;
static guint destroyed_surfaces;
static guint64 imported_modifier;

int __wrap_ioctl(int fd, unsigned long request, ...) {
    assert(fd >= 0 && request == DMA_BUF_IOCTL_EXPORT_SYNC_FILE);
    if (!fence_ok) { errno = ENOTTY; return -1; }
    va_list args;
    va_start(args, request);
    struct dma_buf_export_sync_file *fence = va_arg(args, void *);
    va_end(args);
    assert(fence->flags == DMA_BUF_SYNC_READ);
    int event = eventfd(1, EFD_CLOEXEC);
    assert(event >= 0);
    // Keep the fence number outside the duplicate allocator's normal range
    // so a newly duplicated DMA-BUF cannot mask the fence close check.
    last_fence = fcntl(event, F_DUPFD_CLOEXEC, 512);
    close(event);
    assert(last_fence >= 0);
    fence->fd = last_fence;
    return 0;
}

gboolean __real_gst_va_dmabuf_memories_setup(GstVaDisplay *, GstVideoInfoDmaDrm *,
    GstMemory *[GST_VIDEO_MAX_PLANES], uintptr_t [GST_VIDEO_MAX_PLANES],
    gsize [GST_VIDEO_MAX_PLANES], guint);

gboolean __wrap_gst_va_dmabuf_memories_setup(GstVaDisplay *display,
    GstVideoInfoDmaDrm *info, GstMemory *mem[GST_VIDEO_MAX_PLANES],
    uintptr_t fds[GST_VIDEO_MAX_PLANES], gsize offsets[GST_VIDEO_MAX_PLANES], guint usage) {
    assert(display && gst_va_memory_peek_display(mem[0]) == display);
    assert(fcntl((int)fds[0], F_GETFD) & FD_CLOEXEC);
    assert(offsets[0] == 0 && usage == VA_SURFACE_ATTRIB_USAGE_HINT_VPP_READ);
    if (GST_VIDEO_INFO_N_PLANES(&info->vinfo) == 2) {
        assert(mem[0] != mem[1]);
        assert(fds[0] == fds[1] && offsets[1] == 4096);
        assert(info->vinfo.stride[0] == 64 && info->vinfo.stride[1] == 64);
    }
    imported_modifier = info->drm_modifier;
    imports++;
    // Exercise the library's real surface qdata/reference/destruction code.
    return __real_gst_va_dmabuf_memories_setup(display, info, mem, fds, offsets, usage);
}

VAStatus vaCreateSurfaces(VADisplay display, unsigned int format, unsigned int width,
    unsigned int height, VASurfaceID *surfaces, unsigned int count,
    VASurfaceAttrib *attributes, unsigned int n_attributes) {
    (void)display; (void)format;
    assert(width == 64 && height == 64 && count == 1);
    gboolean saw_prime = FALSE;
    for (guint i = 0; i < n_attributes; i++) {
        if (attributes[i].type == VASurfaceAttribExternalBufferDescriptor) {
            VADRMPRIMESurfaceDescriptor *descriptor = attributes[i].value.value.p;
            assert(descriptor->num_objects == 1 && descriptor->num_layers == 1);
            assert(descriptor->objects[0].drm_format_modifier == imported_modifier);
            if (descriptor->layers[0].num_planes == 2) {
                assert(descriptor->layers[0].object_index[1] == 0);
                assert(descriptor->layers[0].offset[1] == 4096);
            }
            saw_prime = TRUE;
        }
    }
    assert(saw_prime);
    if (!import_ok) return VA_STATUS_ERROR_OPERATION_FAILED;
    surfaces[0] = 42;
    return VA_STATUS_SUCCESS;
}

VAStatus vaDestroySurfaces(VADisplay display, VASurfaceID *surfaces, int count) {
    (void)display;
    assert(count == 1 && surfaces[0] == 42);
    destroyed_surfaces++;
    return VA_STATUS_SUCCESS;
}

VAStatus __wrap_vaSyncSurface2(VADisplay display, VASurfaceID surface, uint64_t timeout) {
    (void)display;
    assert(surface == 42 && timeout == UINT64_C(1000000000));
    return sync_status;
}

static void mark_freed(gpointer data, GstMiniObject *object) {
    (void)object;
    *(gboolean *)data = TRUE;
}

static GstBuffer *input_buffer(int *fd, gboolean *freed) {
    *fd = memfd_create("va-import-test", MFD_CLOEXEC);
    assert(*fd >= 0 && ftruncate(*fd, 16384) == 0);
    GstAllocator *allocator = gst_dmabuf_allocator_new();
    GstBuffer *buffer = gst_buffer_new();
    gst_buffer_append_memory(buffer, gst_dmabuf_allocator_alloc(allocator, *fd, 16384));
    gst_object_unref(allocator);
    GstBuffer *parent = gst_buffer_new();
    assert(gst_buffer_add_parent_buffer_meta(buffer, parent));
    gst_buffer_unref(parent);
    gsize offsets[GST_VIDEO_MAX_PLANES] = { 0 };
    gint strides[GST_VIDEO_MAX_PLANES] = { 256, 0, 0, 0 };
    assert(gst_buffer_add_video_meta_full(buffer, GST_VIDEO_FRAME_FLAG_NONE,
        GST_VIDEO_FORMAT_BGRA, 64, 64, 1, offsets, strides));
    GST_BUFFER_PTS(buffer) = 123;
    gst_mini_object_weak_ref(GST_MINI_OBJECT(buffer), mark_freed, freed);
    return buffer;
}

int main(void) {
    gst_init(NULL, NULL);
    GstVaDisplay *display = g_object_new(GST_TYPE_VA_DISPLAY, NULL);
    gst_object_ref_sink(display);
    GstVaDisplay *other = g_object_new(GST_TYPE_VA_DISPLAY, NULL);
    gst_object_ref_sink(other);
    GstCaps *caps = gst_caps_from_string("video/x-raw(memory:DMABuf),format=DMA_DRM,"
        "drm-format=AR24:0x0100000000000002,width=64,height=64,framerate=30/1");
    int fd;
    gboolean freed = FALSE;
    GstBuffer *input = input_buffer(&fd, &freed), *native = NULL;
    assert(freym_va_import_buffer(GST_OBJECT(display), caps, input, &native) == 0);
    assert(native && imports == 1 && imported_modifier == UINT64_C(0x0100000000000002));
    assert(GST_BUFFER_PTS(native) == 123);
    int duplicate = gst_dmabuf_memory_get_fd(gst_buffer_peek_memory(native, 0));
    assert(duplicate != fd && fcntl(last_fence, F_GETFD) == -1 && errno == EBADF);
    assert(freym_va_sync_buffer(GST_OBJECT(other), native) == 5);
    sync_status = VA_STATUS_ERROR_TIMEDOUT;
    assert(freym_va_sync_buffer(GST_OBJECT(display), native) == 6);
    sync_status = VA_STATUS_SUCCESS;
    assert(freym_va_sync_buffer(GST_OBJECT(display), native) == 0);
    gst_buffer_unref(input);
    assert(!freed && fcntl(fd, F_GETFD) >= 0);
    gst_buffer_unref(native);
    assert(freed && fcntl(fd, F_GETFD) == -1 && fcntl(duplicate, F_GETFD) == -1);
    assert(destroyed_surfaces == 1);

    freed = FALSE;
    input = input_buffer(&fd, &freed);
    import_ok = FALSE;
    assert(freym_va_import_buffer(GST_OBJECT(display), caps, input, &native) == 4 && !native);
    assert(!freed && fcntl(fd, F_GETFD) >= 0);
    import_ok = TRUE;
    fence_ok = FALSE;
    assert(freym_va_import_buffer(GST_OBJECT(display), caps, input, &native) == 3 && !native);
    fence_ok = TRUE;
    gst_buffer_get_video_meta(input)->n_planes = 2;
    assert(freym_va_import_buffer(GST_OBJECT(display), caps, input, &native) == 1 && !native);
    gst_buffer_unref(input);
    assert(freed);
    gst_caps_unref(caps);

    // Shared-object NV12 must preserve both colour planes and their explicit
    // offsets, even though there is only one descriptor/memory in the buffer.
    caps = gst_caps_from_string("video/x-raw(memory:DMABuf),format=DMA_DRM,"
        "drm-format=NV12,width=64,height=64,framerate=30/1");
    freed = FALSE;
    input = input_buffer(&fd, &freed);
    GstVideoMeta *meta = gst_buffer_get_video_meta(input);
    meta->format = GST_VIDEO_FORMAT_NV12;
    meta->n_planes = 2;
    meta->offset[1] = 4096;
    meta->stride[0] = meta->stride[1] = 64;
    meta->offset[1] = 16383;
    assert(freym_va_import_buffer(GST_OBJECT(display), caps, input, &native) == 1 && !native);
    meta->offset[1] = 4096;
    assert(freym_va_import_buffer(GST_OBJECT(display), caps, input, &native) == 0);
    assert(imported_modifier == 0 && native);
    gst_buffer_unref(input);
    assert(!freed);
    gst_buffer_unref(native);
    assert(freed && fcntl(fd, F_GETFD) == -1);
    assert(destroyed_surfaces == 2);
    gst_caps_unref(caps);
    gst_object_unref(other);
    gst_object_unref(display);
    g_print("VA import boundary: modifier, device, fence, failure and descriptor/parent lifetime checks passed.\n");
    return 0;
}
