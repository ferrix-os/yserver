/* selection-record-probe — who gets ConvertSelection's SelectionNotify(None),
 *                          and whether a released selection keeps its
 *                          lastTimeChanged.
 *
 *   ./selection-record-probe convert | none | close | destroy | fresh
 *
 * `a` owns window A, `b` owns window B; every case uses a fresh selection
 * atom. `now` is the server time, read from a PropertyNotify.
 * `convert`: `b` converts the unowned selection with requestor A; then `a`
 *            sets it on A and releases it (None), and `b` converts again.
 * `none`:    `a` sets the owner A at `now`, releases it (None) at `now`, then
 *            `b` sets B at `now - 10`.
 * `close`:   `a` sets A at `now` and disconnects; `b` sets B at `now - 10`.
 * `destroy`: `a` sets A at `now` and destroys A; `b` sets B at `now - 10`.
 * `fresh`:   `a` releases the never-set selection (None) at `now`; `b` sets B
 *            at `now - 10`.
 * Every event `a` and `b` receive is printed, then GetSelectionOwner.
 *
 *   cc -O1 -o selection-record-probe selection-record-probe.c -lxcb
 */
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/xcb.h>

static xcb_connection_t *a, *b;
static xcb_window_t root, A, B;

static const char *nm(uint32_t id)
{
    if (id == A)
        return "A";
    if (id == B)
        return "B";
    if (id == 0)
        return "None";
    static char buf[16];
    snprintf(buf, 16, "0x%x", id);
    return buf;
}

static void rt(xcb_connection_t *c) { free(xcb_get_input_focus_reply(c, xcb_get_input_focus(c), NULL)); }

static void drain(xcb_connection_t *c, const char *who)
{
    if (!c)
        return;
    struct pollfd p = {xcb_get_file_descriptor(c), POLLIN, 0};
    for (;;) {
        xcb_generic_event_t *ev = xcb_poll_for_event(c);
        if (!ev) {
            xcb_flush(c);
            if (poll(&p, 1, 200) <= 0)
                return;
            ev = xcb_poll_for_event(c);
            if (!ev)
                return;
        }
        uint8_t t = ev->response_type & 0x7f;
        printf("  %s: ", who);
        if (t == 0) {
            xcb_generic_error_t *e = (void *)ev;
            printf("Error(%u op%u)", e->error_code, e->major_code);
        } else if (t == XCB_SELECTION_CLEAR) {
            xcb_selection_clear_event_t *e = (void *)ev;
            printf("SelectionClear(owner=%s)", nm(e->owner));
        } else if (t == XCB_SELECTION_REQUEST) {
            xcb_selection_request_event_t *e = (void *)ev;
            printf("SelectionRequest(owner=%s requestor=%s)", nm(e->owner), nm(e->requestor));
        } else if (t == XCB_SELECTION_NOTIFY) {
            xcb_selection_notify_event_t *e = (void *)ev;
            printf("SelectionNotify(requestor=%s property=%u)", nm(e->requestor), e->property);
        } else {
            printf("event%u", t);
        }
        printf("\n");
        free(ev);
    }
}

static xcb_connection_t *conn(void)
{
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    if (xcb_connection_has_error(c)) {
        fprintf(stderr, "cannot connect\n");
        exit(1);
    }
    return c;
}

static xcb_window_t window(xcb_connection_t *c, uint32_t mask)
{
    xcb_window_t w = xcb_generate_id(c);
    xcb_create_window(c, XCB_COPY_FROM_PARENT, w, root, 0, 0, 10, 10, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                      XCB_COPY_FROM_PARENT, XCB_CW_EVENT_MASK, &mask);
    return w;
}

/* The server time: B selects PropertyChange, `b` touches a property on it. */
static xcb_timestamp_t now(void)
{
    xcb_change_property(b, XCB_PROP_MODE_REPLACE, B, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, 1, "x");
    xcb_flush(b);
    for (;;) {
        xcb_generic_event_t *ev = xcb_wait_for_event(b);
        if (!ev)
            exit(1);
        if ((ev->response_type & 0x7f) == XCB_PROPERTY_NOTIFY) {
            xcb_timestamp_t t = ((xcb_property_notify_event_t *)ev)->time;
            free(ev);
            return t;
        }
        free(ev);
    }
}

static void step(const char *what)
{
    if (a)
        rt(a);
    rt(b);
    printf("%s\n", what);
    drain(a, "a");
    drain(b, "b");
}

static void owner(xcb_atom_t sel)
{
    xcb_get_selection_owner_reply_t *r =
        xcb_get_selection_owner_reply(b, xcb_get_selection_owner(b, sel), NULL);
    printf("selection owner: %s\n", r ? nm(r->owner) : "?");
    free(r);
}

int main(int argc, char **argv)
{
    const char *how = argc > 1 ? argv[1] : "convert";
    a = conn();
    b = conn();
    root = xcb_setup_roots_iterator(xcb_get_setup(a)).data->root;
    A = window(a, 0);
    B = window(b, XCB_EVENT_MASK_PROPERTY_CHANGE);
    rt(a);
    rt(b);

    char name[64];
    snprintf(name, sizeof name, "SELECTION_RECORD_PROBE_%s_%d", how, (int)getpid());
    xcb_intern_atom_reply_t *r =
        xcb_intern_atom_reply(b, xcb_intern_atom(b, 0, strlen(name), name), NULL);
    xcb_atom_t sel = r->atom;
    free(r);

    xcb_timestamp_t t = now();
    if (strcmp(how, "convert") == 0) {
        xcb_convert_selection(b, A, sel, XCB_ATOM_STRING, XCB_ATOM_STRING, XCB_CURRENT_TIME);
        step("b ConvertSelection requestor=A (never owned)");
        xcb_set_selection_owner(a, A, sel, t);
        xcb_set_selection_owner(a, XCB_NONE, sel, t);
        step("a SetSelectionOwner A, then None");
        owner(sel);
        xcb_convert_selection(b, A, sel, XCB_ATOM_STRING, XCB_ATOM_STRING, XCB_CURRENT_TIME);
        step("b ConvertSelection requestor=A (released)");
        return 0;
    }
    if (strcmp(how, "fresh") == 0) {
        xcb_set_selection_owner(a, XCB_NONE, sel, t);
        step("a SetSelectionOwner None (never owned) at now");
    } else {
        xcb_set_selection_owner(a, A, sel, t);
        step("a SetSelectionOwner A at now");
        owner(sel);
        if (strcmp(how, "none") == 0) {
            xcb_set_selection_owner(a, XCB_NONE, sel, t);
            step("a SetSelectionOwner None at now");
        } else if (strcmp(how, "close") == 0) {
            xcb_disconnect(a);
            a = NULL;
            usleep(200000);
            step("a disconnects");
        } else {
            xcb_destroy_window(a, A);
            step("a DestroyWindow A");
        }
    }
    owner(sel);
    xcb_set_selection_owner(b, B, sel, t - 10);
    step("b SetSelectionOwner B at now-10");
    owner(sel);
    xcb_set_selection_owner(b, B, sel, t);
    step("b SetSelectionOwner B at now");
    owner(sel);
    return 0;
}
