/*
 * jitter_parity C harness.
 *
 * Reads a trace from stdin, drives Steve Kann's reference jitterbuf,
 * writes a result trace to stdout. Comment lines (starting with #) and
 * blank lines are passed through silently. Output is intended to be
 * golden for cross-checking the Rust port in
 * crates/astar-codec/src/jitter.rs.
 *
 * Trace ops:
 *   config <max_jitterbuf> <resync_threshold> <max_contig_interp> <target_extra>
 *   put <now> <ts> <ms> <voice|control|silence|video> <hex-payload>
 *   get <now> <interpl>
 *   next <now-ignored>
 *   reset
 *
 * Results:
 *   config ok
 *   put ok | put drop | put sched
 *   get ok ts=<ts> ms=<ms> ftype=<...> payload=<hex>
 *   get interpolate | get drop ts=... | get empty | get noframe | get scheduled
 *   next none | next at=<ts>
 *   reset ok
 */

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "jitterbuf.h"

/* JB_LONGMAX as defined in jitterbuf.c (private). */
#define HARNESS_LONGMAX 2147483647L

/*
 * The C jb stores `data` as a void*. We allocate small heap blobs so that
 * the jb can hand back the same pointer later; the harness owns them and
 * frees on reset/exit.
 */
typedef struct payload {
    unsigned char *bytes;
    size_t len;
    struct payload *next;
} payload;

static payload *payload_head = NULL;

static payload *payload_new(const unsigned char *bytes, size_t len)
{
    payload *p = (payload *)malloc(sizeof(*p));
    p->bytes = (unsigned char *)malloc(len ? len : 1);
    if (len) {
        memcpy(p->bytes, bytes, len);
    }
    p->len = len;
    p->next = payload_head;
    payload_head = p;
    return p;
}

static void payload_free_all(void)
{
    payload *p = payload_head;
    while (p) {
        payload *next = p->next;
        free(p->bytes);
        free(p);
        p = next;
    }
    payload_head = NULL;
}

static const char *ftype_name(enum jb_frame_type t)
{
    switch (t) {
    case JB_TYPE_CONTROL: return "control";
    case JB_TYPE_VOICE:   return "voice";
    case JB_TYPE_VIDEO:   return "video";
    case JB_TYPE_SILENCE: return "silence";
    default:              return "unknown";
    }
}

static int parse_ftype(const char *s, enum jb_frame_type *out)
{
    if (strcmp(s, "control") == 0)  { *out = JB_TYPE_CONTROL; return 1; }
    if (strcmp(s, "voice") == 0)    { *out = JB_TYPE_VOICE;   return 1; }
    if (strcmp(s, "video") == 0)    { *out = JB_TYPE_VIDEO;   return 1; }
    if (strcmp(s, "silence") == 0)  { *out = JB_TYPE_SILENCE; return 1; }
    return 0;
}

static int hex_nibble(int c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static int parse_hex(const char *s, unsigned char **out, size_t *outlen)
{
    size_t n = strlen(s);
    if (n % 2 != 0) return 0;
    *outlen = n / 2;
    *out = (unsigned char *)malloc(*outlen ? *outlen : 1);
    for (size_t i = 0; i < *outlen; i++) {
        int hi = hex_nibble((unsigned char)s[2 * i]);
        int lo = hex_nibble((unsigned char)s[2 * i + 1]);
        if (hi < 0 || lo < 0) {
            free(*out);
            *out = NULL;
            return 0;
        }
        (*out)[i] = (unsigned char)((hi << 4) | lo);
    }
    return 1;
}

static void print_hex(FILE *f, const unsigned char *bytes, size_t len)
{
    if (len == 0) {
        fputc('-', f);
        return;
    }
    for (size_t i = 0; i < len; i++) {
        fprintf(f, "%02x", bytes[i]);
    }
}

static void print_frame(FILE *f, const jb_frame *fr)
{
    payload *p = (payload *)fr->data;
    fprintf(f, "ts=%ld ms=%ld ftype=%s payload=",
            fr->ts, fr->ms, ftype_name(fr->type));
    if (p) {
        print_hex(f, p->bytes, p->len);
    } else {
        fputc('-', f);
    }
}

static void rstrip(char *s)
{
    size_t n = strlen(s);
    while (n > 0 && (s[n - 1] == '\n' || s[n - 1] == '\r' ||
                     s[n - 1] == ' ' || s[n - 1] == '\t')) {
        s[--n] = '\0';
    }
}

int main(void)
{
    jitterbuf *jb = jb_new();
    if (!jb) {
        fprintf(stderr, "jb_new failed\n");
        return 1;
    }

    char line[4096];
    while (fgets(line, sizeof(line), stdin)) {
        rstrip(line);
        /* strip leading whitespace */
        char *p = line;
        while (*p == ' ' || *p == '\t') p++;
        /* skip comments and blanks (no output) */
        if (*p == '\0' || *p == '#') {
            continue;
        }

        /* tokenize: op + args */
        char *saveptr = NULL;
        char *op = strtok_r(p, " \t", &saveptr);
        if (!op) continue;

        if (strcmp(op, "config") == 0) {
            char *t1 = strtok_r(NULL, " \t", &saveptr);
            char *t2 = strtok_r(NULL, " \t", &saveptr);
            char *t3 = strtok_r(NULL, " \t", &saveptr);
            char *t4 = strtok_r(NULL, " \t", &saveptr);
            if (!t1 || !t2 || !t3 || !t4) {
                printf("config error\n");
                continue;
            }
            jb_conf c;
            memset(&c, 0, sizeof(c));
            c.max_jitterbuf    = atol(t1);
            c.resync_threshold = atol(t2);
            c.max_contig_interp = atol(t3);
            c.target_extra     = atol(t4);
            jb_setconf(jb, &c);
            printf("config ok\n");
        } else if (strcmp(op, "put") == 0) {
            char *now_s  = strtok_r(NULL, " \t", &saveptr);
            char *ts_s   = strtok_r(NULL, " \t", &saveptr);
            char *ms_s   = strtok_r(NULL, " \t", &saveptr);
            char *typ_s  = strtok_r(NULL, " \t", &saveptr);
            char *hex_s  = strtok_r(NULL, " \t", &saveptr);
            if (!now_s || !ts_s || !ms_s || !typ_s) {
                printf("put error\n");
                continue;
            }
            long now = atol(now_s);
            long ts  = atol(ts_s);
            long ms  = atol(ms_s);
            enum jb_frame_type ftype;
            if (!parse_ftype(typ_s, &ftype)) {
                printf("put error\n");
                continue;
            }
            unsigned char *bytes = NULL;
            size_t blen = 0;
            if (hex_s) {
                if (!parse_hex(hex_s, &bytes, &blen)) {
                    printf("put error\n");
                    continue;
                }
            }
            payload *pl = payload_new(bytes, blen);
            free(bytes);
            enum jb_return_code rc = jb_put(jb, pl, ftype, ms, ts, now);
            switch (rc) {
            case JB_OK:   printf("put ok\n"); break;
            case JB_DROP: printf("put drop\n"); break;
            case JB_SCHED: printf("put sched\n"); break;
            default:      printf("put rc=%d\n", rc); break;
            }
        } else if (strcmp(op, "get") == 0) {
            char *now_s    = strtok_r(NULL, " \t", &saveptr);
            char *interpl_s = strtok_r(NULL, " \t", &saveptr);
            if (!now_s || !interpl_s) {
                printf("get error\n");
                continue;
            }
            long now = atol(now_s);
            long interpl = atol(interpl_s);
            jb_frame fr;
            memset(&fr, 0, sizeof(fr));
            enum jb_return_code rc = jb_get(jb, &fr, now, interpl);
            switch (rc) {
            case JB_OK:
                printf("get ok ");
                print_frame(stdout, &fr);
                fputc('\n', stdout);
                break;
            case JB_DROP:
                printf("get drop ");
                print_frame(stdout, &fr);
                fputc('\n', stdout);
                break;
            case JB_INTERP:   printf("get interpolate\n"); break;
            case JB_EMPTY:    printf("get empty\n"); break;
            case JB_NOFRAME:  printf("get noframe\n"); break;
            case JB_SCHED:    printf("get scheduled\n"); break;
            default:          printf("get rc=%d\n", rc); break;
            }
        } else if (strcmp(op, "next") == 0) {
            /* the trace argument is ignored but kept for symmetry */
            (void)strtok_r(NULL, " \t", &saveptr);
            long n = jb_next(jb);
            if (n == HARNESS_LONGMAX) {
                printf("next none\n");
            } else {
                printf("next at=%ld\n", n);
            }
        } else if (strcmp(op, "reset") == 0) {
            jb_reset(jb);
            payload_free_all();
            printf("reset ok\n");
        } else {
            printf("unknown op=%s\n", op);
        }
    }

    jb_destroy(jb);
    payload_free_all();
    return 0;
}
