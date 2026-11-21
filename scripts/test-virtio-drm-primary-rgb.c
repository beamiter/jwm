/* Read the active primary framebuffer in an owned virtio VirGL test guest. */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/dma-buf.h>
#include <linux/virtio_gpu.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sysexits.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include <drm/virtgpu_drm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#define ARRAY_LEN(array) (sizeof(array) / sizeof((array)[0]))

struct property {
    const char *name;
    uint64_t value;
    bool found;
};

struct sample {
    uint32_t x, y;
    uint8_t r, g, b;
};

static _Noreturn void fail(const char *message)
{
    fprintf(stderr, "virtio-primary-rgb: %s\n", message);
    exit(EXIT_FAILURE);
}

static _Noreturn void fail_errno(const char *message)
{
    perror(message);
    exit(EXIT_FAILURE);
}

static _Noreturn void scanout_changed(void)
{
    fputs("virtio-primary-rgb: active scanout changed during readback\n", stderr);
    exit(EX_TEMPFAIL);
}

static void read_properties(int fd, uint32_t object, uint32_t type,
                            struct property *wanted, size_t count)
{
    drmModeObjectPropertiesPtr all = drmModeObjectGetProperties(fd, object, type);
    if (!all)
        fail_errno("drmModeObjectGetProperties");

    for (uint32_t i = 0; i < all->count_props; ++i) {
        drmModePropertyPtr prop = drmModeGetProperty(fd, all->props[i]);
        if (!prop)
            fail_errno("drmModeGetProperty");

        for (size_t j = 0; j < count; ++j) {
            if (strcmp(wanted[j].name, prop->name) == 0) {
                wanted[j].value = all->prop_values[i];
                wanted[j].found = true;
                break;
            }
        }

        drmModeFreeProperty(prop);
    }
    drmModeFreeObjectProperties(all);
}

static uint32_t active_connector(int fd, const char *name)
{
    drmModeResPtr resources = drmModeGetResources(fd);
    if (!resources)
        fail_errno("drmModeGetResources");

    uint32_t result = 0;
    for (int i = 0; i < resources->count_connectors; ++i) {
        drmModeConnectorPtr connector = drmModeGetConnectorCurrent(fd, resources->connectors[i]);
        if (!connector)
            fail_errno("drmModeGetConnectorCurrent");

        char expected[32];
        snprintf(expected, sizeof(expected), "Virtual-%" PRIu32, connector->connector_type_id);
        bool matches = connector->connector_type == DRM_MODE_CONNECTOR_VIRTUAL &&
                       strcmp(expected, name) == 0;
        if (matches && connector->connection == DRM_MODE_CONNECTED) {
            if (result)
                fail("connector name is ambiguous");
            result = connector->connector_id;
        }
        drmModeFreeConnector(connector);
    }
    drmModeFreeResources(resources);
    if (!result)
        fail("no connected virtio head has the requested name");
    return result;
}

static uint32_t connector_crtc(int fd, uint32_t connector)
{
    struct property crtc = {.name = "CRTC_ID"};
    read_properties(fd, connector, DRM_MODE_OBJECT_CONNECTOR, &crtc, 1);
    if (!crtc.found || crtc.value > UINT32_MAX)
        fail("the requested connector has an invalid CRTC_ID property");
    return (uint32_t)crtc.value;
}

static drmModePlanePtr active_primary(int fd, uint32_t crtc)
{
    drmModePlaneResPtr planes = drmModeGetPlaneResources(fd);
    if (!planes)
        fail_errno("drmModeGetPlaneResources");

    drmModePlanePtr result = NULL;
    for (uint32_t i = 0; i < planes->count_planes; ++i) {
        drmModePlanePtr plane = drmModeGetPlane(fd, planes->planes[i]);
        if (!plane)
            fail_errno("drmModeGetPlane");

        if (plane->crtc_id == crtc && plane->fb_id != 0) {
            struct property type = {.name = "type"};
            read_properties(fd, plane->plane_id, DRM_MODE_OBJECT_PLANE, &type, 1);
            if (type.found && type.value == DRM_PLANE_TYPE_PRIMARY) {
                if (result)
                    fail("multiple active primary planes for the CRTC");
                result = plane;
                continue;
            }
        }
        drmModeFreePlane(plane);
    }
    drmModeFreePlaneResources(planes);
    if (!result)
        fail("the CRTC has no active primary plane");
    return result;
}

static void check_full_plane(int fd, const drmModePlane *plane, const drmModeFB2 *fb)
{
    struct property expected[] = {
        {"type", DRM_PLANE_TYPE_PRIMARY, false},
        {"CRTC_ID", plane->crtc_id, false},
        {"FB_ID", fb->fb_id, false},
        {"CRTC_X", 0, false}, {"CRTC_Y", 0, false},
        {"CRTC_W", fb->width, false}, {"CRTC_H", fb->height, false},
        {"SRC_X", 0, false}, {"SRC_Y", 0, false},
        {"SRC_W", (uint64_t)fb->width << 16, false},
        {"SRC_H", (uint64_t)fb->height << 16, false},
        {"rotation", DRM_MODE_ROTATE_0, false}
    };
    struct property state[ARRAY_LEN(expected)];
    memcpy(state, expected, sizeof(state));
    read_properties(fd, plane->plane_id, DRM_MODE_OBJECT_PLANE, state, ARRAY_LEN(state));
    for (size_t i = 0; i < ARRAY_LEN(state); ++i) {
        if (!state[i].found && strcmp(state[i].name, "rotation") == 0)
            continue;
        if (!state[i].found || state[i].value != expected[i].value) {
            if (state[i].found &&
                (strcmp(state[i].name, "FB_ID") == 0 || strcmp(state[i].name, "CRTC_ID") == 0))
                scanout_changed();
            fprintf(stderr, "virtio-primary-rgb: plane %" PRIu32 " %s: expected %" PRIu64
                    ", got %s%" PRIu64 "\n", plane->plane_id, state[i].name, expected[i].value,
                    state[i].found ? "" : "missing ", state[i].value);
            exit(EXIT_FAILURE);
        }
    }
}

static const char *rgb_format(uint32_t fourcc, bool *rgb_bytes)
{
    *rgb_bytes = false;
    switch (fourcc) {
    case DRM_FORMAT_XRGB8888: return "XRGB8888";
    case DRM_FORMAT_ARGB8888: return "ARGB8888";
    case DRM_FORMAT_XBGR8888: *rgb_bytes = true; return "XBGR8888";
    case DRM_FORMAT_ABGR8888: *rgb_bytes = true; return "ABGR8888";
    default:
        fprintf(stderr, "virtio-primary-rgb: unsupported scanout fourcc 0x%08" PRIx32 "\n",
                fourcc);
        exit(EXIT_FAILURE);
    }
}

static bool virtgpu_param(int fd, uint64_t parameter, uint32_t *value)
{
    *value = 0;
    struct drm_virtgpu_getparam query = {
        .param = parameter,
        .value = (uintptr_t)value
    };
    if (drmIoctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &query) == 0)
        return true;
    if (errno == EINVAL || errno == ENOTTY)
        return false;
    fail_errno("DRM_IOCTL_VIRTGPU_GETPARAM");
}

static void initialize_virgl_readback(int fd)
{
    uint32_t value;
    if (!virtgpu_param(fd, VIRTGPU_PARAM_3D_FEATURES, &value) || !value)
        fail("the virtio device has no VirGL 3D transfer support");
    if (!virtgpu_param(fd, VIRTGPU_PARAM_CONTEXT_INIT, &value) || !value)
        return; // A legacy driver creates the context when GETFB2 creates the handle.

    uint32_t supported;
    bool has_caps = virtgpu_param(fd, VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs, &supported);
    uint32_t capset = VIRTIO_GPU_CAPSET_VIRGL;
    if (supported & (UINT32_C(1) << VIRTIO_GPU_CAPSET_VIRGL2))
        capset = VIRTIO_GPU_CAPSET_VIRGL2;
    else if (has_caps && !(supported & (UINT32_C(1) << VIRTIO_GPU_CAPSET_VIRGL)))
        fail("the virtio device advertises no VirGL context type");

    struct drm_virtgpu_context_set_param param = {
        .param = VIRTGPU_CONTEXT_PARAM_CAPSET_ID,
        .value = capset
    };
    struct drm_virtgpu_context_init context = {
        .num_params = 1,
        .ctx_set_params = (uintptr_t)&param
    };
    // Initialize before GETFB2. The driver's GEM open hook then attaches the
    // existing scanout resource to this file descriptor's readback context.
    if (drmIoctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &context) != 0)
        fail_errno("DRM_IOCTL_VIRTGPU_CONTEXT_INIT");
}

static void wait_for_gem(int fd, uint32_t handle)
{
    struct drm_virtgpu_3d_wait wait = {.handle = handle};
    if (drmIoctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &wait) != 0)
        fail_errno("DRM_IOCTL_VIRTGPU_WAIT");
}

static void transfer_from_host(int fd, const drmModePlane *plane, const drmModeFB2 *fb)
{
    struct drm_virtgpu_resource_info resource = {.bo_handle = fb->handles[0]};
    if (drmIoctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &resource) != 0)
        fail_errno("DRM_IOCTL_VIRTGPU_RESOURCE_INFO");
    if (!resource.res_handle || resource.blob_mem != 0 ||
        resource.size < (uint64_t)fb->pitches[0] * fb->height)
        fail("host transfer requires a complete, non-blob VirGL resource");

    wait_for_gem(fd, fb->handles[0]);
    check_full_plane(fd, plane, fb);
    struct drm_virtgpu_3d_transfer_from_host transfer = {
        .bo_handle = fb->handles[0],
        .box = {.w = fb->width, .h = fb->height, .d = 1}
    };
    if (drmIoctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST, &transfer) != 0)
        fail_errno("DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST");
    wait_for_gem(fd, fb->handles[0]);
}

static int export_linear_rgb(int fd, const drmModePlane *plane, const drmModeFB2 *fb,
                              size_t *map_size)
{
    // VirGL's non-blob host transfer uses the resource's implicit stride.
    if (fb->width == 0 || fb->height == 0 || fb->modifier != DRM_FORMAT_MOD_LINEAR ||
        fb->offsets[0] != 0 || fb->pitches[0] != (uint64_t)fb->width * 4)
        fail("expected a tightly packed linear 32-bit framebuffer at offset zero");

    for (size_t i = 1; i < ARRAY_LEN(fb->handles); ++i) {
        if (fb->handles[i] || fb->pitches[i] || fb->offsets[i])
            fail("expected one packed RGB plane");
    }
    if (!fb->handles[0])
        fail("DRM GETFB2 did not return a handle; run in the guest with CAP_SYS_ADMIN");
    transfer_from_host(fd, plane, fb);

    int prime_fd = -1;
    if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC, &prime_fd) != 0)
        fail_errno("drmPrimeHandleToFD");
    if (drmCloseBufferHandle(fd, fb->handles[0]) != 0)
        fail_errno("drmCloseBufferHandle");

    uint64_t end = (uint64_t)fb->pitches[0] * fb->height;
    off_t dma_size = lseek(prime_fd, 0, SEEK_END);
    if (dma_size < 0)
        fail_errno("lseek PRIME allocation size");
    if (end > (uint64_t)dma_size || end > SIZE_MAX)
        fail("framebuffer byte range exceeds the PRIME allocation");

    *map_size = (size_t)end;
    return prime_fd;
}

static void synchronize_cpu_read(int prime_fd, uint64_t stage)
{
    struct dma_buf_sync sync = {.flags = stage | DMA_BUF_SYNC_READ};
    int rc;
    do {
        rc = ioctl(prime_fd, DMA_BUF_IOCTL_SYNC, &sync);
    } while (rc < 0 && (errno == EINTR || errno == EAGAIN));
    if (rc != 0)
        fail_errno("DMA_BUF_IOCTL_SYNC");
}

static void collect_samples(int fd, const drmModePlane *plane, const drmModeFB2 *fb,
                            bool rgb_bytes, struct sample samples[4])
{
    size_t map_size;
    int prime_fd = export_linear_rgb(fd, plane, fb, &map_size);
    const uint8_t *pixels = mmap(NULL, map_size, PROT_READ, MAP_SHARED, prime_fd, 0);
    if (pixels == MAP_FAILED)
        fail_errno("mmap PRIME framebuffer (read-only)");
    synchronize_cpu_read(prime_fd, DMA_BUF_SYNC_START);

    const uint32_t xs[] = {(uint64_t)fb->width * 3 / 8, (uint64_t)fb->width * 5 / 8};
    const uint32_t ys[] = {fb->height / 3, (uint64_t)fb->height * 2 / 3};
    size_t index = 0;
    for (size_t y = 0; y < ARRAY_LEN(ys); ++y) {
        for (size_t x = 0; x < ARRAY_LEN(xs); ++x) {
            size_t offset = (size_t)ys[y] * fb->pitches[0] + (size_t)xs[x] * 4;
            const uint8_t *pixel = pixels + offset;
            samples[index++] = (struct sample){
                xs[x], ys[y], pixel[rgb_bytes ? 0 : 2], pixel[1], pixel[rgb_bytes ? 2 : 0]
            };
        }
    }
    synchronize_cpu_read(prime_fd, DMA_BUF_SYNC_END);
    if (munmap((void *)pixels, map_size) != 0)
        fail_errno("munmap PRIME framebuffer");
    close(prime_fd);
}

int main(int argc, char **argv)
{
    if (argc != 3) {
        fprintf(stderr, "usage: %s /dev/dri/cardN Virtual-N\n", argv[0]);
        return EXIT_FAILURE;
    }

    const char *connector_name = argv[2];
    int fd = open(argv[1], O_RDWR | O_CLOEXEC);
    if (fd < 0)
        fail_errno("open DRM card");

    drmVersionPtr driver = drmGetVersion(fd);
    if (!driver)
        fail_errno("drmGetVersion");
    bool virtio = driver->name && strcmp(driver->name, "virtio_gpu") == 0;
    drmFreeVersion(driver);
    if (!virtio)
        fail("only a virtio_gpu guest DRM card is supported");

    // Atomic properties expose the actual plane source and destination. Setting
    // the client capability affects this file descriptor and performs no modeset.
    if (drmSetClientCap(fd, DRM_CLIENT_CAP_ATOMIC, 1) != 0)
        fail_errno("drmSetClientCap DRM_CLIENT_CAP_ATOMIC");

    initialize_virgl_readback(fd);
    uint32_t connector_id = active_connector(fd, connector_name);
    uint32_t crtc_id = connector_crtc(fd, connector_id);
    if (!crtc_id)
        fail("the requested connector has no active CRTC");
    drmModeCrtcPtr crtc = drmModeGetCrtc(fd, crtc_id);
    if (!crtc)
        fail_errno("drmModeGetCrtc");
    if (!crtc->mode_valid)
        fail("the selected CRTC has no valid mode");

    drmModePlanePtr plane = active_primary(fd, crtc_id);
    drmModeFB2Ptr fb = drmModeGetFB2(fd, plane->fb_id);
    if (!fb) {
        int error = errno;
        drmModePlanePtr current = drmModeGetPlane(fd, plane->plane_id);
        if (current) {
            bool changed = current->crtc_id != plane->crtc_id || current->fb_id != plane->fb_id;
            drmModeFreePlane(current);
            if (changed)
                scanout_changed();
        }
        errno = error;
        fail_errno("drmModeGetFB2");
    }
    if (fb->width != crtc->mode.hdisplay || fb->height != crtc->mode.vdisplay)
        fail("primary framebuffer dimensions differ from the active CRTC mode");
    check_full_plane(fd, plane, fb);

    bool rgb_bytes;
    const char *format = rgb_format(fb->pixel_format, &rgb_bytes);
    struct sample samples[4];
    collect_samples(fd, plane, fb, rgb_bytes, samples);

    if (connector_crtc(fd, connector_id) != crtc_id)
        scanout_changed();
    check_full_plane(fd, plane, fb);

    printf("{\"connector\":\"%s\",\"connector_id\":%" PRIu32 ",\"crtc_id\":%" PRIu32
           ",\"plane_id\":%" PRIu32 ",\"framebuffer_id\":%" PRIu32
           ",\"width\":%" PRIu32 ",\"height\":%" PRIu32 ",\"format\":\"%s\""
           ",\"modifier\":\"0x%016" PRIx64 "\",\"pitch\":%" PRIu32
           ",\"offset\":%" PRIu32 ",\"host_transfer\":true,\"samples\":[",
           connector_name, connector_id, crtc_id, plane->plane_id, fb->fb_id, fb->width,
           fb->height, format, fb->modifier, fb->pitches[0], fb->offsets[0]);
    for (size_t i = 0; i < ARRAY_LEN(samples); ++i) {
        const struct sample sample = samples[i];
        printf("%s{\"x\":%" PRIu32 ",\"y\":%" PRIu32 ",\"rgb\":[%u,%u,%u]}",
               i ? "," : "", sample.x, sample.y, (unsigned)sample.r, (unsigned)sample.g,
               (unsigned)sample.b);
    }
    puts("]}");

    drmModeFreeFB2(fb);
    drmModeFreePlane(plane);
    drmModeFreeCrtc(crtc);
    close(fd);
    return EXIT_SUCCESS;
}
