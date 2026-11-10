/* present-watch — count PresentCompleteNotify modes (copy/flip/skip) on the
 *                 composite overlay window and the root for N seconds, to see
 *                 whether a compositor's Present is flipped or copied.
 *
 *   cc -o target/present-watch tools/present-watch.c \
 *       $(pkg-config --cflags --libs xcb xcb-composite xcb-present)
 *   DISPLAY=:7 target/present-watch 10
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <poll.h>
#include <xcb/xcb.h>
#include <xcb/composite.h>
#include <xcb/present.h>

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 10;
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) { fprintf(stderr, "no display\n"); return 1; }
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    xcb_composite_query_version_reply_t *cv = xcb_composite_query_version_reply(c, xcb_composite_query_version(c, 0, 4), NULL);
    free(cv);
    xcb_composite_get_overlay_window_reply_t *ow = xcb_composite_get_overlay_window_reply(c, xcb_composite_get_overlay_window(c, s->root), NULL);
    if (!ow) { fprintf(stderr, "no COW\n"); return 1; }
    xcb_window_t cow = ow->overlay_win;
    const xcb_query_extension_reply_t *pe = xcb_get_extension_data(c, &xcb_present_id);
    if (!pe || !pe->present) { fprintf(stderr, "no Present\n"); return 1; }
    free(xcb_present_query_version_reply(c, xcb_present_query_version(c, 1, 2), NULL));
    xcb_window_t targets[2] = { cow, s->root };
    for (int i = 0; i < 2; i++)
        xcb_present_select_input(c, xcb_generate_id(c), targets[i], XCB_PRESENT_EVENT_MASK_COMPLETE_NOTIFY);
    xcb_flush(c);
    printf("cow=0x%x root=0x%x\n", cow, s->root);
    long mode[2][4] = {{0}}, kind[2] = {0};
    time_t end = time(NULL) + secs;
    struct pollfd pfd = { xcb_get_file_descriptor(c), POLLIN, 0 };
    while (time(NULL) < end) {
        poll(&pfd, 1, 200);
        xcb_generic_event_t *e;
        while ((e = xcb_poll_for_event(c))) {
            if ((e->response_type & 0x7f) == XCB_GE_GENERIC) {
                xcb_present_complete_notify_event_t *n = (void *)e;
                if (n->extension == pe->major_opcode && n->event_type == XCB_PRESENT_COMPLETE_NOTIFY) {
                    int w = n->window == cow ? 0 : 1;
                    if (n->kind == XCB_PRESENT_COMPLETE_KIND_PIXMAP && n->mode < 4) mode[w][n->mode]++;
                    else kind[w]++;
                }
            }
            free(e);
        }
    }
    const char *names[4] = { "copy", "flip", "skip", "suboptimal_copy" };
    for (int w = 0; w < 2; w++) {
        printf("%s:", w ? "root" : "cow");
        for (int m = 0; m < 4; m++) printf(" %s=%ld", names[m], mode[w][m]);
        printf(" msc_notify=%ld\n", kind[w]);
    }
    return 0;
}
