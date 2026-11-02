#!/bin/bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# nodebench ASL3 entrypoint: render iax.conf with the chosen codec set, then
# run Asterisk in the foreground so podman owns its lifetime.
#
#   NODEBENCH_CODECS  comma list, offered in order (default "slin16,slin,ulaw";
#                     "ulaw" is the ClearNode-like ulaw-only variant)
#   NODEBENCH_DEBUG   1 turns on "iax2 set debug on" and "rpt debug level 7"
#                     as soon as Asterisk answers the CLI
set -euo pipefail

codecs="${NODEBENCH_CODECS:-slin16,slin,ulaw}"
block="; codecs from NODEBENCH_CODECS=${codecs}"$'\n'"disallow = all"
IFS=',' read -ra list <<<"$codecs"
for c in "${list[@]}"; do
    case "$c" in
        ulaw | alaw | slin | slin16 | gsm | adpcm) block+=$'\n'"allow = $c" ;;
        *)
            echo "nodebench-asl3: unknown codec '$c'" >&2
            exit 2
            ;;
    esac
done
while IFS= read -r line; do
    if [ "$line" = "@CODECS@" ]; then printf '%s\n' "$block"; else printf '%s\n' "$line"; fi
done </etc/asterisk/iax.conf.in >/etc/asterisk/iax.conf
echo "nodebench-asl3: codecs = ${codecs}"

if [ "${NODEBENCH_DEBUG:-0}" = "1" ]; then
    (
        for _ in $(seq 1 120); do
            if asterisk -rx 'core show version' >/dev/null 2>&1; then
                asterisk -rx 'core set debug 2' >/dev/null || true
                asterisk -rx 'iax2 set debug on' || true
                asterisk -rx 'rpt debug level 7' || true
                exit 0
            fi
            sleep 0.5
        done
    ) &
fi

mkdir -p /var/run/asterisk /var/log/asterisk
chown -R asterisk:asterisk /var/run/asterisk /var/log/asterisk /var/lib/asterisk \
    /var/spool/asterisk /etc/asterisk
exec /usr/sbin/asterisk -f -U asterisk -G asterisk -vvv
