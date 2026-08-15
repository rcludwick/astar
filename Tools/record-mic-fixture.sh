#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Record N seconds of a named CoreAudio input to a 48 kHz mono 16-bit PCM WAV —
# the format the characterize test fixtures use (read_wav_mono_f32 reads i16
# mono; the test 6:1-decimates 48 kHz → the 8 kHz pipeline rate).
#
# Capture mic NOISE/whine for a characterization fixture: STAY SILENT while it
# records. The point is the steady noise floor + any whine (e.g. the fake-Icom
# 588 Hz fundamental + harmonics), not speech.
#
# Usage:  Tools/record-mic-fixture.sh "<device name or index>" <out.wav> [seconds]
# List devices:  ffmpeg -f avfoundation -list_devices true -i ""
set -euo pipefail

DEV="${1:?device name or avfoundation index (e.g. \"USB Audio Device\" or 2)}"
OUT="${2:?output .wav path}"
SECS="${3:-10}"

mkdir -p "$(dirname "$OUT")"
echo ">> Recording ${SECS}s from '${DEV}' → ${OUT}"
echo ">> STAY SILENT until it finishes."
ffmpeg -hide_banner -loglevel warning \
  -f avfoundation -i ":${DEV}" \
  -t "${SECS}" -ar 48000 -ac 1 -c:a pcm_s16le -y "${OUT}"
echo ">> done:"
ffprobe -hide_banner "${OUT}" 2>&1 | grep -iE "Duration|Audio:"
