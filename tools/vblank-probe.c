/* vblank-probe — which vblank interfaces a DRM device supports, per active
 *                CRTC: CRTC_GET_SEQUENCE, CRTC_QUEUE_SEQUENCE (+event),
 *                legacy WAIT_VBLANK query and WAIT_VBLANK event, and whether
 *                each counter advances over ~1 s. None of these ioctls needs
 *                DRM master, so it runs next to a live X server.
 *
 *   cc -o target/vblank-probe tools/vblank-probe.c \
 *       $(pkg-config --cflags --libs libdrm)
 *   target/vblank-probe /dev/dri/card0
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static int got_event;

static double now_mono(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void on_vblank(int fd, unsigned seq, unsigned sec, unsigned usec, void *data) {
    (void)fd;
    printf("      event: WAIT_VBLANK seq=%u time=%u.%06u (mono now %.6f) user_data=%lu\n",
           seq, sec, usec, now_mono(), (unsigned long)(uintptr_t)data);
    got_event = 1;
}

static void on_sequence(int fd, uint64_t seq, uint64_t ns, uint64_t user_data) {
    (void)fd;
    printf("      event: CRTC_SEQUENCE seq=%" PRIu64 " time=%.6f (mono now %.6f) user_data=%" PRIu64 "\n",
           seq, ns / 1e9, now_mono(), user_data);
    got_event = 1;
}

/* Read DRM events for up to `ms`; returns 1 once a handled event arrived. */
static int wait_event(int fd, int ms) {
    drmEventContext ev = {0};
    ev.version = 4;
    ev.vblank_handler = on_vblank;
    ev.sequence_handler = on_sequence;
    got_event = 0;
    double end = now_mono() + ms / 1000.0;
    while (!got_event) {
        int left = (int)((end - now_mono()) * 1000);
        if (left <= 0) break;
        struct pollfd p = { fd, POLLIN, 0 };
        int r = poll(&p, 1, left);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) break;
        if (drmHandleEvent(fd, &ev) != 0) { printf("      drmHandleEvent failed: %s\n", strerror(errno)); break; }
    }
    return got_event;
}

/* Xorg modesetting's pipe encoding for the legacy ioctl. */
static unsigned pipe_bits(int pipe) {
    if (pipe > 1) return (unsigned)pipe << DRM_VBLANK_HIGH_CRTC_SHIFT;
    if (pipe > 0) return DRM_VBLANK_SECONDARY;
    return 0;
}

static int get_sequence(int fd, uint32_t crtc_id, uint64_t *seq, uint64_t *ns) {
    return drmCrtcGetSequence(fd, crtc_id, seq, ns) ? -errno : 0;
}

static int wait_vblank_query(int fd, int pipe, unsigned *seq, long *sec, long *usec) {
    drmVBlank vbl;
    memset(&vbl, 0, sizeof vbl);
    vbl.request.type = DRM_VBLANK_RELATIVE | pipe_bits(pipe);
    vbl.request.sequence = 0;
    if (drmWaitVBlank(fd, &vbl)) return -errno;
    *seq = vbl.reply.sequence;
    *sec = vbl.reply.tval_sec;
    *usec = vbl.reply.tval_usec;
    return 0;
}

static void probe_crtc(int fd, int pipe, uint32_t crtc_id) {
    uint64_t seq_a = 0, ns_a = 0, seq_b = 0, ns_b = 0;
    unsigned vseq_a = 0, vseq_b = 0;
    long sec = 0, usec = 0;
    int r;

    printf("  (a) CRTC_GET_SEQUENCE: ");
    r = get_sequence(fd, crtc_id, &seq_a, &ns_a);
    if (r) printf("errno %d (%s)\n", -r, strerror(-r));
    else printf("seq=%" PRIu64 " time=%.6f (mono now %.6f)\n", seq_a, ns_a / 1e9, now_mono());
    int have_get = r == 0;

    printf("  (b) CRTC_QUEUE_SEQUENCE relative +1: ");
    uint64_t queued = 0;
    if (drmCrtcQueueSequence(fd, crtc_id, DRM_CRTC_SEQUENCE_RELATIVE, 1, &queued, crtc_id)) {
        printf("errno %d (%s)\n", errno, strerror(errno));
    } else {
        printf("queued for seq=%" PRIu64 "\n", queued);
        if (!wait_event(fd, 500)) printf("      no event within 500 ms\n");
    }

    printf("  (c) WAIT_VBLANK query (relative 0, pipe %d): ", pipe);
    r = wait_vblank_query(fd, pipe, &vseq_a, &sec, &usec);
    if (r) printf("errno %d (%s)\n", -r, strerror(-r));
    else printf("seq=%u time=%ld.%06ld (mono now %.6f)\n", vseq_a, sec, usec, now_mono());
    int have_wait = r == 0;

    printf("  (d) WAIT_VBLANK event (relative +1): ");
    drmVBlank vbl;
    memset(&vbl, 0, sizeof vbl);
    vbl.request.type = DRM_VBLANK_RELATIVE | DRM_VBLANK_EVENT | pipe_bits(pipe);
    vbl.request.sequence = 1;
    vbl.request.signal = crtc_id;
    if (drmWaitVBlank(fd, &vbl)) {
        printf("errno %d (%s)\n", errno, strerror(errno));
    } else {
        printf("queued for seq=%u\n", vbl.reply.sequence);
        if (!wait_event(fd, 500)) printf("      no event within 500 ms\n");
    }

    if (!have_get && !have_wait) return;
    double t0 = now_mono();
    if (have_get) get_sequence(fd, crtc_id, &seq_a, &ns_a);
    if (have_wait) wait_vblank_query(fd, pipe, &vseq_a, &sec, &usec);
    usleep(1000000);
    double dt = now_mono() - t0;
    if (have_get) {
        if (get_sequence(fd, crtc_id, &seq_b, &ns_b) == 0)
            printf("  advance GET_SEQUENCE: %" PRId64 " vblanks in %.3f s (%.2f Hz)\n",
                   (int64_t)(seq_b - seq_a), dt, (double)(seq_b - seq_a) / dt);
    }
    if (have_wait) {
        if (wait_vblank_query(fd, pipe, &vseq_b, &sec, &usec) == 0)
            printf("  advance WAIT_VBLANK:  %d vblanks in %.3f s (%.2f Hz)\n",
                   (int)(vseq_b - vseq_a), dt, (double)(vseq_b - vseq_a) / dt);
    }
}

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s /dev/dri/cardN\n", argv[0]); return 2; }
    int fd = open(argv[1], O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror(argv[1]); return 1; }
    drmVersionPtr v = drmGetVersion(fd);
    if (v) { printf("driver %s %d.%d.%d (%s)\n", v->name, v->version_major, v->version_minor, v->version_patchlevel, v->date); drmFreeVersion(v); }
    printf("DRM master: %s\n", drmIsMaster(fd) ? "yes" : "no");
    uint64_t cap = 0;
    if (drmGetCap(fd, DRM_CAP_CRTC_IN_VBLANK_EVENT, &cap) == 0) printf("DRM_CAP_CRTC_IN_VBLANK_EVENT=%" PRIu64 "\n", cap);
    else printf("DRM_CAP_CRTC_IN_VBLANK_EVENT: errno %d (%s)\n", errno, strerror(errno));
    if (drmGetCap(fd, DRM_CAP_TIMESTAMP_MONOTONIC, &cap) == 0) printf("DRM_CAP_TIMESTAMP_MONOTONIC=%" PRIu64 "\n", cap);
    drmModeResPtr res = drmModeGetResources(fd);
    if (!res) { fprintf(stderr, "drmModeGetResources: %s (not a KMS node?)\n", strerror(errno)); return 1; }
    int active = 0;
    for (int i = 0; i < res->count_crtcs; i++) {
        drmModeCrtcPtr c = drmModeGetCrtc(fd, res->crtcs[i]);
        if (!c) continue;
        if (c->mode_valid) {
            active++;
            printf("\nCRTC %u (pipe %d): %ux%u@%u mode=%s\n", c->crtc_id, i, c->mode.hdisplay, c->mode.vdisplay, c->mode.vrefresh, c->mode.name);
            probe_crtc(fd, i, c->crtc_id);
        }
        drmModeFreeCrtc(c);
    }
    if (!active) printf("no active CRTC\n");
    drmModeFreeResources(res);
    close(fd);
    return 0;
}
