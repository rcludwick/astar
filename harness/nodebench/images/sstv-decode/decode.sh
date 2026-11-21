#!/bin/sh
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# decode <in.wav> <out.png>: run the outside decoders, unmodified, on one
# recording, and print "Detected SSTV mode <name>" for the mode found.
#
# colaclanth/sstv goes first; it has no PD modes and stops on a VIS code it
# does not know, and then ta2ldv/sstv-decoder gets the recording.
set -eu
[ $# -eq 2 ] || { echo "usage: decode <in.wav> <out.png>" >&2; exit 2; }
in=$1
out=$2

# colaclanth/sstv sizes its progress bar from the terminal and dies without
# one, so it gets a pseudo-terminal (a width, nothing else). script exits with
# the command's status.
if script -qec "stty cols 100 rows 24; sstv -d '$in' -o '$out'" /dev/null; then
    exit 0
fi
echo "decode: colaclanth/sstv could not read it; trying ta2ldv/sstv-decoder" >&2

# ta2ldv/sstv-decoder writes ./data/<name>_sstv.png and prints the mode on a
# "VIS=<n> -> <mode>" line.
scratch=$(mktemp -d)
cd "$scratch"
python /usr/local/lib/sstv_decoder.py "$in" >log.txt 2>&1 || { cat log.txt >&2; exit 1; }
cat log.txt
png=$(ls data/*_sstv.png 2>/dev/null | head -n 1)
[ -n "$png" ] || { echo "decode: no picture was decoded" >&2; exit 1; }
mode=$(sed -n 's/.*VIS=[0-9]* -> //p' log.txt | head -n 1)
cp "$png" "$out"
echo "Detected SSTV mode $mode"
