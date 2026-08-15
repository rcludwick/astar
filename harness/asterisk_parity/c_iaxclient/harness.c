/*
 * Patched-C iaxclient harness for iax-7022 ground-truth captures.
 *
 * Mirrors the Rust scenarios in crates/astar-conformance/src/scenarios.rs so
 * the captured pcaps under fixtures/c-iaxclient/ are directly comparable
 * to fixtures/asterisk/ (the Rust side).
 *
 * Usage:
 *   iax-harness <scenario>
 *
 * Scenarios:
 *   register         REGREQ + AUTH + REGACK + REGREL (no-CALLTOKEN peer)
 *   register_reject  REGREQ against a requirecalltoken=yes peer; expects
 *                    REGREJ because libiax2's iax_register does not append
 *                    the empty CALLTOKEN opt-in IE.
 *   call_notoken     NEW (no CALLTOKEN) -> ACCEPT -> brief hold -> HANGUP
 *   call_token     NEW with CALLTOKEN -> resent NEW -> AUTHREQ/REP -> ACCEPT -> HANGUP
 *   call_ulaw      call_token + 100 frames of mu-law silence
 *   peer_hangup    NEW to "bye" extension; wait for peer-initiated HANGUP
 *
 * Env:
 *   IAX_PEER     host[:port]   default "asterisk:4569"
 *   IAX_USER                   default "astartest"
 *   IAX_SECRET                 default "astartest-pass"
 *   IAX_DEST                   default "s"
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <sys/select.h>
#include <sys/time.h>

#include <iax/iax-client.h>
#include <iax/frame.h>

#define DEFAULT_PEER   "asterisk:4569"
#define DEFAULT_USER   "astartest"
#define DEFAULT_SECRET "astartest-pass"
#define DEFAULT_DEST   "s"

/* mu-law silence byte. 0xff in mu-law decodes to 0 PCM. */
#define ULAW_SILENCE 0xff

static const char *env_or(const char *name, const char *fallback)
{
    const char *v = getenv(name);
    return (v && *v) ? v : fallback;
}

/* Pump the iaxclient event loop until `until_ms` wall-clock has elapsed,
 * OR until the supplied event-predicate returns nonzero. Returns the
 * triggering event (caller must iax_event_free), or NULL on timeout.
 * `match` may be NULL to just spin for `until_ms`. */
typedef int (*event_pred_t)(struct iax_event *);

static long now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static struct iax_event *pump_until(long timeout_ms, event_pred_t match)
{
    long deadline = now_ms() + timeout_ms;
    for (;;) {
        long remaining = deadline - now_ms();
        if (remaining <= 0) return NULL;

        long next = iax_time_to_next_event();
        if (next < 0 || next > remaining) next = remaining;

        fd_set rfds;
        FD_ZERO(&rfds);
        int fd = iax_get_fd();
        FD_SET(fd, &rfds);

        struct timeval tv;
        tv.tv_sec = next / 1000;
        tv.tv_usec = (next % 1000) * 1000;
        int sel = select(fd + 1, &rfds, NULL, NULL, &tv);
        if (sel < 0 && errno != EINTR) {
            fprintf(stderr, "harness: select error: %s\n", strerror(errno));
            return NULL;
        }

        struct iax_event *e;
        while ((e = iax_get_event(0)) != NULL) {
            fprintf(stderr, "harness: event etype=%d subclass=%d ts=%u\n",
                    e->etype, e->subclass, e->ts);
            if (match && match(e)) return e;
            iax_event_free(e);
        }
    }
}

/* Build an `ich` string of the form "username:secret@host/exten". */
static void build_ich(char *out, size_t outlen,
                      const char *user, const char *secret,
                      const char *peer, const char *exten)
{
    if (secret && *secret) {
        snprintf(out, outlen, "%s:%s@%s/%s", user, secret, peer, exten);
    } else {
        snprintf(out, outlen, "%s@%s/%s", user, peer, exten);
    }
}

static int pred_regack_or_rej(struct iax_event *e)
{
    return e->etype == IAX_EVENT_REGACK || e->etype == IAX_EVENT_REGREJ;
}

static int pred_accept_or_reject(struct iax_event *e)
{
    return e->etype == IAX_EVENT_ACCEPT
        || e->etype == IAX_EVENT_REJECT
        || e->etype == IAX_EVENT_HANGUP
        || e->etype == IAX_EVENT_TIMEOUT;
}

static int pred_hangup(struct iax_event *e)
{
    return e->etype == IAX_EVENT_HANGUP || e->etype == IAX_EVENT_TIMEOUT;
}

static int pred_connect(struct iax_event *e)
{
    return e->etype == IAX_EVENT_CONNECT || e->etype == IAX_EVENT_TIMEOUT;
}

/* ---- scenarios ---- */

/* Drive a registration attempt, returning the received event type.
 * Used by both scenario_register (expects REGACK) and
 * scenario_register_reject (expects REGREJ). */
static int run_register(const char *peer_host, const char *user, const char *secret,
                        int *got_etype)
{
    struct iax_session *s = iax_session_new();
    if (!s) { fprintf(stderr, "register: iax_session_new failed\n"); return -1; }

    fprintf(stderr, "register: iax_register peer=%s user=%s\n", peer_host, user);
    if (iax_register(s, peer_host, user, secret, 60) < 0) {
        fprintf(stderr, "register: iax_register returned -1 (iax_errstr=\"%s\")\n", iax_errstr);
        return -1;
    }

    struct iax_event *e = pump_until(5000, pred_regack_or_rej);
    if (!e) { fprintf(stderr, "register: timeout waiting REGACK/REGREJ\n"); return -1; }
    *got_etype = e->etype;
    fprintf(stderr, "register: got %s\n",
            e->etype == IAX_EVENT_REGACK ? "REGACK" : "REGREJ");
    iax_event_free(e);

    /* Brief steady-state — REGACK path additionally sends REGREL below. */
    pump_until(200, NULL);
    if (*got_etype == IAX_EVENT_REGACK) {
        iax_unregister(s, peer_host, user, secret, "test complete");
        pump_until(500, NULL);
    }
    return 0;
}

static int scenario_register(const char *peer_host, const char *user, const char *secret)
{
    int et = 0;
    if (run_register(peer_host, user, secret, &et) < 0) return 1;
    return et == IAX_EVENT_REGACK ? 0 : 1;
}

static int scenario_register_reject(const char *peer_host, const char *user, const char *secret)
{
    int et = 0;
    if (run_register(peer_host, user, secret, &et) < 0) return 1;
    /* Negative scenario: REGREJ is success. Anything else (REGACK,
     * timeout) is a failure because we expected the libiax2 register
     * gap (no CALLTOKEN IE on REGREQ) to trip a requirecalltoken=yes
     * peer. */
    return et == IAX_EVENT_REGREJ ? 0 : 1;
}

static int scenario_call(const char *peer_host, const char *user, const char *secret,
                         const char *dest, int hold_ms, int send_voice)
{
    struct iax_session *s = iax_session_new();
    if (!s) { fprintf(stderr, "call: iax_session_new failed\n"); return 1; }

    char ich[512];
    build_ich(ich, sizeof ich, user, secret, peer_host, dest);
    fprintf(stderr, "call: iax_call ich=%s\n", ich);

    /* format/capability: G711_ULAW. AST_FORMAT_ULAW = 4 in vendored frame.h. */
    if (iax_call(s, user, user, ich, NULL, 0, AST_FORMAT_ULAW, AST_FORMAT_ULAW) < 0) {
        fprintf(stderr, "call: iax_call returned -1 (iax_errstr=\"%s\")\n", iax_errstr);
        return 1;
    }

    struct iax_event *e = pump_until(8000, pred_accept_or_reject);
    if (!e) { fprintf(stderr, "call: timeout waiting ACCEPT\n"); return 1; }
    int et = e->etype;
    iax_event_free(e);
    if (et != IAX_EVENT_ACCEPT) {
        fprintf(stderr, "call: got etype=%d (expected ACCEPT=1)\n", et);
        return 1;
    }

    if (send_voice) {
        unsigned char silence[160];
        memset(silence, ULAW_SILENCE, sizeof silence);
        for (int i = 0; i < 100; i++) {
            iax_send_voice(s, AST_FORMAT_ULAW, silence, sizeof silence, 160);
            pump_until(20, NULL);
        }
    } else if (hold_ms > 0) {
        pump_until(hold_ms, NULL);
    }

    iax_hangup(s, "harness done");
    pump_until(1000, pred_hangup);
    return 0;
}

static int scenario_incoming(const char *peer_host, const char *user, const char *secret)
{
    /* Step 1: register so the peer knows where to dial us. We bypass
     * run_register's unregister-on-success path — the call comes after. */
    struct iax_session *reg = iax_session_new();
    if (!reg) { fprintf(stderr, "incoming: iax_session_new failed\n"); return 1; }

    fprintf(stderr, "incoming: iax_register peer=%s user=%s\n", peer_host, user);
    if (iax_register(reg, peer_host, user, secret, 60) < 0) {
        fprintf(stderr, "incoming: iax_register failed (iax_errstr=\"%s\")\n", iax_errstr);
        return 1;
    }

    struct iax_event *e = pump_until(5000, pred_regack_or_rej);
    if (!e || e->etype != IAX_EVENT_REGACK) {
        fprintf(stderr, "incoming: registration did not succeed\n");
        if (e) iax_event_free(e);
        return 1;
    }
    iax_event_free(e);
    fprintf(stderr, "incoming: registered, waiting for inbound CONNECT...\n");

    /* Step 2: idle waiting for the peer's NEW. Asterisk originates from
     * outside (run.sh fires `channel originate IAX2/<user> ...` once it
     * sees the harness emit the "registered" message). 15s gives plenty
     * of slack. */
    e = pump_until(15000, pred_connect);
    if (!e || e->etype != IAX_EVENT_CONNECT) {
        fprintf(stderr, "incoming: timeout waiting CONNECT\n");
        if (e) iax_event_free(e);
        return 1;
    }
    struct iax_session *call = e->session;
    fprintf(stderr, "incoming: got CONNECT; accepting + answering\n");
    iax_event_free(e);

    iax_accept(call, AST_FORMAT_ULAW);
    iax_answer(call);

    /* Step 3: hold the call until peer hangs up or 8s elapses. */
    e = pump_until(8000, pred_hangup);
    if (e) {
        fprintf(stderr, "incoming: got %s\n",
                e->etype == IAX_EVENT_HANGUP ? "HANGUP" : "TIMEOUT");
        iax_event_free(e);
    } else {
        fprintf(stderr, "incoming: no HANGUP within hold window\n");
    }
    return 0;
}

static int scenario_peer_hangup(const char *peer_host, const char *user, const char *secret)
{
    struct iax_session *s = iax_session_new();
    if (!s) { fprintf(stderr, "peer_hangup: iax_session_new failed\n"); return 1; }

    char ich[512];
    build_ich(ich, sizeof ich, user, secret, peer_host, "bye");
    fprintf(stderr, "peer_hangup: iax_call ich=%s\n", ich);
    if (iax_call(s, user, user, ich, NULL, 0, AST_FORMAT_ULAW, AST_FORMAT_ULAW) < 0) {
        fprintf(stderr, "peer_hangup: iax_call returned -1 (iax_errstr=\"%s\")\n", iax_errstr);
        return 1;
    }

    /* Wait for the peer to drive HANGUP. */
    struct iax_event *e = pump_until(8000, pred_hangup);
    if (!e) { fprintf(stderr, "peer_hangup: timeout waiting peer HANGUP\n"); return 1; }
    fprintf(stderr, "peer_hangup: got etype=%d\n", e->etype);
    iax_event_free(e);
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s <register|register_reject|call_notoken|call_token"
                "|call_ulaw|peer_hangup|incoming>\n",
                argv[0]);
        return 2;
    }
    const char *scenario = argv[1];

    const char *peer   = env_or("IAX_PEER",   DEFAULT_PEER);
    const char *user   = env_or("IAX_USER",   DEFAULT_USER);
    const char *secret = env_or("IAX_SECRET", DEFAULT_SECRET);
    const char *dest   = env_or("IAX_DEST",   DEFAULT_DEST);

    /* iax_init returns the port it actually bound on (we don't care). */
    if (iax_init(0) < 0) {
        fprintf(stderr, "iax_init failed: %s\n", iax_errstr);
        return 1;
    }

    int rc;
    if (!strcmp(scenario, "register")) {
        rc = scenario_register(peer, user, secret);
    } else if (!strcmp(scenario, "register_reject")) {
        rc = scenario_register_reject(peer, user, secret);
    } else if (!strcmp(scenario, "call_notoken") || !strcmp(scenario, "call_token")) {
        rc = scenario_call(peer, user, secret, dest, 500, 0);
    } else if (!strcmp(scenario, "call_ulaw")) {
        rc = scenario_call(peer, user, secret, dest, 0, 1);
    } else if (!strcmp(scenario, "peer_hangup")) {
        rc = scenario_peer_hangup(peer, user, secret);
    } else if (!strcmp(scenario, "incoming")) {
        rc = scenario_incoming(peer, user, secret);
    } else {
        fprintf(stderr, "unknown scenario: %s\n", scenario);
        rc = 2;
    }

    fprintf(stderr, "harness: scenario %s rc=%d\n", scenario, rc);
    return rc;
}
