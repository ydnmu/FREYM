/* Linux VA boundary. GstBuffer/GstMemory own every DMA-BUF descriptor and
 * imported surface. This file never maps pixels or takes ownership of those
 * descriptors. Only acquire sync-file descriptors created here are closed. */
#define _GNU_SOURCE
#define GST_USE_UNSTABLE_API
#include <errno.h>
#include <fcntl.h>
#include <linux/dma-buf.h>
#include <poll.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>
#include <gst/allocators/gstdmabuf.h>
#include <gst/va/gstvaallocator.h>
#include <gst/va/gstvadisplay.h>
#include <gst/video/video-info-dma.h>

enum { IMPORT_OK, IMPORT_LAYOUT, IMPORT_MEMORY, IMPORT_FENCE, IMPORT_FAILED,
       IMPORT_DEVICE, IMPORT_SYNC };

static int64_t monotonic_ms(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return (int64_t) now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

static gboolean wait_acquire(int fd, int64_t deadline) {
#ifdef DMA_BUF_IOCTL_EXPORT_SYNC_FILE
    struct dma_buf_export_sync_file fence = { .flags = DMA_BUF_SYNC_READ, .fd = -1 };
    if (ioctl(fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &fence) != 0) return FALSE;
    if (fence.fd < 0) return FALSE;
    struct pollfd pfd = { .fd = fence.fd, .events = POLLIN };
    gboolean ready = FALSE;
    for (;;) {
        int64_t now = monotonic_ms();
        if (now < 0 || now >= deadline) break;
        int result = poll(&pfd, 1, (int)(deadline - now));
        if (result < 0 && errno == EINTR) continue;
        ready = result == 1 && (pfd.revents & POLLIN) &&
            !(pfd.revents & (POLLERR | POLLNVAL));
        break;
    }
    close(fence.fd);
    return ready;
#else
    (void) fd; (void) deadline;
    return FALSE;
#endif
}

int freym_va_import_buffer(GstObject *object, GstCaps *caps, GstBuffer *buffer,
                          GstBuffer **imported) {
    if (!imported) return IMPORT_MEMORY;
    *imported = NULL;
    if (!GST_IS_VA_DISPLAY(object) || !caps || !buffer) return IMPORT_DEVICE;
    GstVaDisplay *display = GST_VA_DISPLAY(object);
    GstVideoInfoDmaDrm drm;
    GstVideoInfo layout;
    gst_video_info_dma_drm_init(&drm);
    if (!gst_video_info_dma_drm_from_caps(&drm, caps) ||
        !gst_video_info_dma_drm_to_video_info(&drm, &layout)) return IMPORT_LAYOUT;
    GstVideoMeta *meta = gst_buffer_get_video_meta(buffer);
    guint planes = GST_VIDEO_INFO_N_PLANES(&layout);
    if (!meta || planes == 0 || planes > GST_VIDEO_MAX_PLANES ||
        meta->n_planes != planes || meta->width != (guint)layout.width ||
        meta->height != (guint)layout.height || meta->format != layout.finfo->format)
        return IMPORT_LAYOUT;
    /* DMA_DRM's generic format info does not describe all colour planes.
     * VA needs the concrete format's plane count, including NV12 chroma. */
    drm.vinfo = layout;
    guint memories = gst_buffer_n_memory(buffer);
    if (memories == 0 || memories > GST_VIDEO_MAX_PLANES ||
        !gst_buffer_get_meta(buffer, GST_PARENT_BUFFER_META_API_TYPE))
        return IMPORT_MEMORY;
    GstMemory *mem[GST_VIDEO_MAX_PLANES] = { NULL };
    uintptr_t fds[GST_VIDEO_MAX_PLANES] = { 0 };
    gsize offsets[GST_VIDEO_MAX_PLANES] = { 0 };
    int64_t now = monotonic_ms();
    if (now < 0) return IMPORT_FENCE;
    for (guint i = 0; i < planes; i++) {
        guint index, length;
        gsize skip;
        if (meta->stride[i] <= 0 ||
            !gst_buffer_find_memory(buffer, meta->offset[i], 1, &index, &length, &skip) ||
            length != 1) return IMPORT_LAYOUT;
        mem[i] = gst_buffer_peek_memory(buffer, index);
        if (!gst_is_dmabuf_memory(mem[i])) return IMPORT_MEMORY;
        int fd = gst_dmabuf_memory_get_fd(mem[i]);
        if (fd < 0 || mem[i]->offset > G_MAXSIZE - skip) return IMPORT_LAYOUT;
        offsets[i] = mem[i]->offset + skip;
        if (offsets[i] > G_MAXUINT32) return IMPORT_LAYOUT;
        if (drm.drm_modifier == 0) {
            guint rows = 0;
            for (guint component = 0; component < layout.finfo->n_components; component++) {
                if (layout.finfo->plane[component] == i)
                    rows = MAX(rows, (guint)GST_VIDEO_FORMAT_INFO_SCALE_HEIGHT(layout.finfo,
                                                                             component, layout.height));
            }
            /* Linear planes must fit the visible memory. For non-linear
             * layouts size/padding is modifier-specific and VA validates the
             * complete PRIME descriptor instead of guessing row geometry. */
            guint64 row_bytes = (guint64)layout.stride[i];
            guint64 bytes = rows == 0 ? 0 :
                (guint64)(rows - 1) * (guint64)meta->stride[i] + row_bytes;
            if (rows == 0 || meta->stride[i] < layout.stride[i] ||
                skip > mem[i]->size || bytes > mem[i]->size - skip)
                return IMPORT_LAYOUT;
        }
        fds[i] = (uintptr_t)fd;
        /* No explicit SyncTimeline metadata is requested from PipeWire. The
         * producer must publish implicit DMA reservation fences. Export and
         * wait for the write fence before VA reads, without mapping storage. */
        gboolean seen = FALSE;
        for (guint j = 0; j < i; j++) seen |= fds[j] == fds[i];
        if (!seen && !wait_acquire(fd, now + 1000)) return IMPORT_FENCE;
    }
    /* Wrap duplicated descriptors with a display-aware VA DMABuf allocator.
     * Generic PipeWire allocators cannot expose the imported display to VPP;
     * importing into them would make VPP reimport or use its CPU copy path.
     * The new buffer retains the complete original, including its pool parent.
     * These are descriptor/reference operations, never pixel copies. */
    GstAllocator *allocator = gst_va_dmabuf_allocator_new(display);
    if (!allocator) return IMPORT_MEMORY;
    GstBuffer *native = gst_buffer_new();
    GstBuffer *plane_holders = gst_buffer_new();
    int result = IMPORT_MEMORY;
    if (!gst_buffer_copy_into(native, buffer, GST_BUFFER_COPY_METADATA, 0, -1) ||
        !gst_buffer_add_parent_buffer_meta(native, buffer)) goto done;
    for (guint i = 0; i < memories; i++) {
        GstMemory *original = gst_buffer_peek_memory(buffer, i);
        if (!gst_is_dmabuf_memory(original)) goto done;
        int fd = fcntl(gst_dmabuf_memory_get_fd(original), F_DUPFD_CLOEXEC, 0);
        if (fd < 0) goto done;
        GstMemory *wrapped = gst_dmabuf_allocator_alloc(allocator, fd, original->maxsize);
        if (!wrapped) { close(fd); goto done; }
        gst_memory_resize(wrapped, original->offset, original->size);
        gst_buffer_append_memory(native, wrapped);
    }
    for (guint i = 0; i < planes; i++) {
        guint index, length;
        gsize skip;
        if (!gst_buffer_find_memory(native, meta->offset[i], 1, &index, &length, &skip) ||
            length != 1) { result = IMPORT_LAYOUT; goto done; }
        mem[i] = gst_buffer_peek_memory(native, index);
        fds[i] = (uintptr_t)gst_dmabuf_memory_get_fd(mem[i]);
        offsets[i] = mem[i]->offset + skip;
        for (guint j = 0; j < i; j++) {
            if (mem[i] == mem[j]) {
                /* GStreamer attaches one surface qdata ref per plane. Giving
                 * it the same GstMemory twice can replace that qdata while
                 * setup is still running. A distinct shared view retains the
                 * same descriptor/parent without duplicating pixel storage. */
                GstMemory *view = gst_memory_share(mem[i], 0, -1);
                if (!view) goto done;
                mem[i] = view;
                gst_buffer_append_memory(plane_holders, view);
                break;
            }
        }
        drm.vinfo.offset[i] = meta->offset[i];
        drm.vinfo.stride[i] = meta->stride[i];
    }
    if (!gst_buffer_add_parent_buffer_meta(native, plane_holders)) goto done;
    /* A successful PRIME import on this exact VA display proves that the
     * kernel/driver accepts this buffer, including its actual modifier and
     * object/plane layout. An exporter vendor/name is never device evidence.
     * The imported surface is attached to the GstMemory by GStreamer. */
    if (!gst_va_dmabuf_memories_setup(display, &drm, mem, fds, offsets,
                                    VA_SURFACE_ATTRIB_USAGE_HINT_VPP_READ)) {
        result = IMPORT_FAILED;
        goto done;
    }
    if (gst_va_buffer_peek_display(native) != display) {
        result = IMPORT_DEVICE;
        goto done;
    }
    *imported = native;
    native = NULL;
    result = IMPORT_OK;
done:
    if (native) gst_buffer_unref(native);
    gst_buffer_unref(plane_holders);
    gst_object_unref(allocator);
    return result;
}

int freym_va_sync_buffer(GstObject *object, GstBuffer *buffer) {
    if (!GST_IS_VA_DISPLAY(object) || !buffer) return IMPORT_DEVICE;
    GstVaDisplay *display = GST_VA_DISPLAY(object);
    if (gst_va_buffer_peek_display(buffer) != display) return IMPORT_DEVICE;
    VASurfaceID surface = gst_va_buffer_get_surface(buffer);
    if (surface == VA_INVALID_ID) return IMPORT_MEMORY;
    /* Waiting for the VPP destination proves that its reads of the retained
     * PipeWire input have finished. This is a GPU fence wait, not readback. */
    return vaSyncSurface2(gst_va_display_get_va_dpy(display), surface,
                         UINT64_C(1000000000)) == VA_STATUS_SUCCESS ? IMPORT_OK : IMPORT_SYNC;
}
