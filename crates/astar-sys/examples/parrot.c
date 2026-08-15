/*
 * parrot.c — a minimal C consumer of the astar-sys poll+snapshot C-ABI.
 *
 * Demonstrates the full lifecycle with NO callbacks: new -> connect (the
 * generic, vendor-neutral path) -> poll snapshot/next_event in a loop ->
 * disconnect -> free.
 *
 * Secret-free by construction: the guest secret is passed as a connect()
 * IN-param ("allstar" here) and is never read back out of any struct.
 *
 * Build + link: see examples/build.sh and examples/README.md.
 *
 * Running it for real dials the AllStar parrot 55553 (audio loopback). The CI
 * job only compiles + links this file; it does not run it (no network in CI).
 * Define IAX_PARROT_DRYRUN to build a self-contained offline smoke that just
 * exercises new -> snapshot -> set_ptt(NOT_CONNECTED) -> free.
 */

#include "astar.h"

#include <stdio.h>
#include <string.h>

#if defined(_WIN32)
#  include <windows.h>
static void sleep_ms(int ms) { Sleep(ms); }
#else
#  include <unistd.h>
static void sleep_ms(int ms) { usleep((useconds_t)ms * 1000); }
#endif

static const char *status_name(IaxStatus s) {
    switch (s) {
        case IaxStatus_Idle:     return "idle";
        case IaxStatus_Dialing:  return "dialing";
        case IaxStatus_Answered: return "answered";
        case IaxStatus_Hangup:   return "hangup";
        default:                 return "?";
    }
}

static void drain_events(IaxStation *st) {
    for (;;) {
        IaxEvent ev;
        int rc = iax_station_next_event(st, &ev);
        if (rc != IAX_OK || ev.kind == IaxEventKind_None) {
            return;
        }
        switch (ev.kind) {
            case IaxEventKind_Answered:
                printf("  event: answered\n");
                break;
            case IaxEventKind_RemotePtt:
                printf("  event: remote_ptt=%d\n", ev.remote_ptt ? 1 : 0);
                break;
            case IaxEventKind_Hangup:
                printf("  event: hangup\n");
                break;
            default:
                break;
        }
    }
}

int main(void) {
    /* All NULL: system default devices, no portal (manual/parrot path), and
     * the default guest secret. */
    IaxConfig cfg;
    memset(&cfg, 0, sizeof cfg);

    IaxStation *st = iax_station_new(&cfg);
    if (!st) {
        fprintf(stderr, "iax_station_new failed\n");
        return 1;
    }

#ifdef IAX_PARROT_DRYRUN
    /* Offline smoke: no network. Prove the poll surface and the secret-free
     * not-connected guard, then free. */
    IaxState s;
    int rc = iax_station_snapshot(st, &s);
    if (rc != IAX_OK || s.status != IaxStatus_Idle) {
        fprintf(stderr, "expected idle snapshot, rc=%d status=%d\n", rc, s.status);
        iax_station_free(st);
        return 1;
    }
    rc = iax_station_set_ptt(st, true);
    if (rc != IAX_ERR_NOT_CONNECTED) {
        fprintf(stderr, "expected NOT_CONNECTED, got %d (%s)\n", rc, iax_error_text(rc));
        iax_station_free(st);
        return 1;
    }
    /* send_dtmf smoke: an off-pad character is rejected as an invalid digit
     * (before any call check), and a valid key with no call is not-connected. */
    rc = iax_station_send_dtmf(st, 'A');
    if (rc != IAX_ERR_INVALID_DIGIT) {
        fprintf(stderr, "expected INVALID_DIGIT, got %d (%s)\n", rc, iax_error_text(rc));
        iax_station_free(st);
        return 1;
    }
    rc = iax_station_send_dtmf(st, '5');
    if (rc != IAX_ERR_NOT_CONNECTED) {
        fprintf(stderr, "expected NOT_CONNECTED for dtmf, got %d (%s)\n", rc, iax_error_text(rc));
        iax_station_free(st);
        return 1;
    }
    printf("dry run ok: idle snapshot + not-connected guard + dtmf guards\n");
    iax_station_free(st);
    return 0;
#else
    /* Generic IAX2 connect (vendor-neutral): dial the parrot. secret is an
     * in-param, consumed and never echoed. */
    int rc = iax_station_connect(st, "55553", "55553", "allstar", "astar");
    if (rc != IAX_OK) {
        fprintf(stderr, "connect failed: %s (%d)\n", iax_error_text(rc), rc);
        iax_station_free(st);
        return 1;
    }
    printf("connected; polling ~8s...\n");

    for (int i = 0; i < 80; i++) {
        IaxState s;
        if (iax_station_snapshot(st, &s) == IAX_OK) {
            printf("status=%-8s tx=%.1fdB rx=%.1fdB ptt=%d rtt=%dms\n",
                   status_name(s.status), s.tx_db, s.rx_db, s.ptt ? 1 : 0, s.rtt_ms);
        }
        drain_events(st);
        sleep_ms(100);
    }

    iax_station_disconnect(st);
    iax_station_free(st);
    printf("done\n");
    return 0;
#endif
}
