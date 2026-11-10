/* ListFonts and ListFontsWithInfo replies, and what OpenFont of the pattern
 * opens, for each "max<TAB>pattern" line of a pattern file, on the
 * font path given as arguments (SetFontPath; "built-ins" is an element).
 *
 *   ./font-list-probe patterns.txt /usr/share/fonts/misc built-ins ...
 *
 *   cc -O1 -o font-list-probe font-list-probe.c -lxcb
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/xcb.h>

static xcb_connection_t *c;

static xcb_connection_t *connect_retry(void)
{
    xcb_connection_t *conn;
    /* Xorg resets when the previous run's last client leaves. */
    for (int tries = 0; (conn = xcb_connect(NULL, NULL)) && xcb_connection_has_error(conn) && tries < 50;
         tries++) {
        xcb_disconnect(conn);
        usleep(100000);
    }
    return conn;
}

static char *atom_name(xcb_atom_t a)
{
    static char buf[512];
    xcb_get_atom_name_reply_t *r = xcb_get_atom_name_reply(c, xcb_get_atom_name(c, a), NULL);
    if (!r) {
        snprintf(buf, sizeof buf, "<atom %u?>", a);
        return buf;
    }
    snprintf(buf, sizeof buf, "%.*s", xcb_get_atom_name_name_length(r), xcb_get_atom_name_name(r));
    free(r);
    return buf;
}

/* Properties whose value is an atom (XLFD string fields and friends). */
static int string_prop(const char *name)
{
    static const char *const names[] = {
        "FONT", "FOUNDRY", "FAMILY_NAME", "WEIGHT_NAME", "SLANT", "SETWIDTH_NAME",
        "ADD_STYLE_NAME", "SPACING", "CHARSET_REGISTRY", "CHARSET_ENCODING", "COPYRIGHT",
        "NOTICE", "FONTNAME_REGISTRY", "FACE_NAME", "FONT_TYPE", "RASTERIZER_NAME", "FULL_NAME",
        "_DEC_DEVICE_FONTNAMES", "_GBDFED_INFO", "_XMBDFED_INFO", NULL,
    };
    for (int i = 0; names[i]; i++)
        if (!strcmp(names[i], name))
            return 1;
    return 0;
}

static void charinfo(const char *what, const xcb_charinfo_t *ci)
{
    printf(" %s=%d,%d,%d,%d,%d,%u", what, ci->left_side_bearing, ci->right_side_bearing,
           ci->character_width, ci->ascent, ci->descent, ci->attributes);
}

static void list_fonts(unsigned max, const char *pat)
{
    xcb_list_fonts_reply_t *r =
        xcb_list_fonts_reply(c, xcb_list_fonts(c, max, strlen(pat), pat), NULL);
    if (!r) {
        printf("LF max=%u '%s': error\n", max, pat);
        return;
    }
    printf("LF max=%u '%s' -> %u\n", max, pat, r->names_len);
    xcb_str_iterator_t it = xcb_list_fonts_names_iterator(r);
    for (; it.rem; xcb_str_next(&it))
        printf("  %.*s\n", xcb_str_name_length(it.data), xcb_str_name(it.data));
    free(r);
    /* OpenFont of the pattern itself, as toolkits do with an XLFD pattern. */
    xcb_font_t f = xcb_generate_id(c);
    xcb_generic_error_t *e = xcb_request_check(c, xcb_open_font_checked(c, f, strlen(pat), pat));
    if (e) {
        printf("  open: error %u\n", e->error_code);
        free(e);
        return;
    }
    xcb_query_font_reply_t *q = xcb_query_font_reply(c, xcb_query_font(c, f), NULL);
    if (q) {
        printf("  open: asc=%d desc=%d", q->font_ascent, q->font_descent);
        charinfo("max", &q->max_bounds);
        printf("\n");
        free(q);
    }
    xcb_close_font(c, f);
}

/* names_only: the reply names, without metrics and properties. */
static void list_fonts_with_info(unsigned max, const char *pat, int names_only)
{
    xcb_list_fonts_with_info_cookie_t ck = xcb_list_fonts_with_info(c, max, strlen(pat), pat);
    printf("LFWI max=%u '%s'\n", max, pat);
    for (;;) {
        xcb_generic_error_t *e = NULL;
        xcb_list_fonts_with_info_reply_t *r = xcb_list_fonts_with_info_reply(c, ck, &e);
        if (!r) {
            printf("  error %u\n", e ? e->error_code : 0);
            free(e);
            return;
        }
        if (r->name_len == 0) {
            free(r);
            return;
        }
        printf("  %.*s\n", xcb_list_fonts_with_info_name_length(r),
               xcb_list_fonts_with_info_name(r));
        if (names_only) {
            free(r);
            continue;
        }
        printf("   asc=%d desc=%d", r->font_ascent, r->font_descent);
        charinfo("min", &r->min_bounds);
        charinfo("max", &r->max_bounds);
        printf(" dc=%u cols=%u-%u rows=%u-%u all=%u dir=%u\n", r->default_char,
               r->min_char_or_byte2, r->max_char_or_byte2, r->min_byte1, r->max_byte1,
               r->all_chars_exist, r->draw_direction);
        xcb_fontprop_t *p = xcb_list_fonts_with_info_properties(r);
        for (int i = 0; i < xcb_list_fonts_with_info_properties_length(r); i++) {
            char name[512];
            snprintf(name, sizeof name, "%s", atom_name(p[i].name));
            if (string_prop(name))
                printf("   %s=%s\n", name, atom_name(p[i].value));
            else
                printf("   %s=%d\n", name, (int32_t)p[i].value);
        }
        free(r);
    }
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s patterns.txt font-path-element...\n", argv[0]);
        return 2;
    }
    c = connect_retry();
    if (!c || xcb_connection_has_error(c)) {
        fprintf(stderr, "cannot connect\n");
        return 1;
    }
    /* SetFontPath: each element is a length-prefixed string. */
    size_t len = 0;
    for (int i = 2; i < argc; i++)
        len += 1 + strlen(argv[i]);
    xcb_str_t *path = calloc(1, len);
    char *w = (char *)path;
    for (int i = 2; i < argc; i++) {
        *w++ = (char)strlen(argv[i]);
        memcpy(w, argv[i], strlen(argv[i]));
        w += strlen(argv[i]);
    }
    xcb_generic_error_t *e = xcb_request_check(c, xcb_set_font_path_checked(c, argc - 2, path));
    printf("font path:");
    for (int i = 2; i < argc; i++)
        printf(" %s", argv[i]);
    printf(e ? " -> error %u\n" : "\n", e ? e->error_code : 0);
    free(e);
    free(path);

    FILE *in = fopen(argv[1], "r");
    if (!in) {
        perror(argv[1]);
        return 1;
    }
    char line[512];
    while (fgets(line, sizeof line, in)) {
        line[strcspn(line, "\n")] = 0;
        char *tab = strchr(line, '\t');
        if (!tab || line[0] == '#')
            continue;
        *tab = 0;
        unsigned max = (unsigned)strtoul(line, NULL, 10);
        /* A third field: "lf" for ListFonts only, "names" for ListFontsWithInfo
         * names without metrics and properties. */
        char *only = strchr(tab + 1, '\t');
        if (only)
            *only++ = 0;
        list_fonts(max, tab + 1);
        if (!only || strcmp(only, "lf"))
            list_fonts_with_info(max, tab + 1, only && !strcmp(only, "names"));
    }
    fclose(in);
    /* Back to the default path so a later scenario starts clean. */
    xcb_request_check(c, xcb_set_font_path_checked(c, 0, NULL));
    xcb_disconnect(c);
    return 0;
}
