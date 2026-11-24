#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# make-golden.sh: write the IMBE golden vectors in tests/golden/.
#
# The Rust port in this crate is checked bit for bit against Pavel Yazev's
# C++ imbe_vocoder from OP25. This script is how those reference answers are
# made. It fetches OP25 at the pinned commit into a temporary directory,
# compiles the vocoder there together with a small driver (written below as a
# heredoc, so no C++ file ever enters the repository), runs it, and writes:
#
#   tests/golden/encode.bin  160 input samples -> 11 output bytes, per frame
#   tests/golden/decode.bin  11 input bytes -> 160 output samples, per frame
#   tests/golden/MANIFEST    the commit, compiler, date and SHA-256 of both
#
# It takes no arguments, needs git, a C++ compiler and network access, and is
# never run by CI: CI only reads the committed .bin files.
set -euo pipefail

PIN=38a37491df4532d932f9f1d5634617af410cb3c9
UPSTREAM=https://github.com/boatbod/op25
SUBDIR=op25/gr-op25_repeater/lib/imbe_vocoder

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$here/../tests/golden"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git clone --quiet --filter=blob:none "$UPSTREAM" "$work/op25"
git -C "$work/op25" -c advice.detachedHead=false checkout --quiet "$PIN"
src="$work/op25/$SUBDIR"

cat >"$work/driver.cc" <<'DRIVER'
// astar's golden-vector driver for Pavel Yazev's imbe_vocoder.
//
//   driver encode <encode.bin>
//       Builds the seven test signals, encodes them with one imbe_vocoder
//       instance, and writes encode.bin.
//   driver decode <encode.bin> <decode.bin>
//       Reads the frames back out of encode.bin, appends the hostile frames,
//       decodes everything with one imbe_vocoder instance, and writes
//       decode.bin.
//
// The two halves run as separate processes on purpose. rand_gen.cc keeps its
// seed in a file-static variable, so it is shared by every imbe_vocoder in a
// process; a decoder constructed after an encoder would start from a
// different seed than a decoder constructed alone. A fresh process gives the
// decoder the seed a fresh Rust ImbeDecoder has.
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

#include "imbe_vocoder.h"

static const int kFrames = 100;      // per test signal, 2 s
static const int kSamples = 160;     // per frame
static const int kBytes = 11;        // per packed frame
static const double kFs = 8000.0;
static const double kPi = 3.14159265358979323846;

static uint32_t lcg_next(uint32_t &x) {
    x = x * 1664525u + 1013904223u;
    return x;
}

static int16_t clamp16(double v) {
    double r = std::floor(v + 0.5);
    if (r > 32767.0) r = 32767.0;
    if (r < -32768.0) r = -32768.0;
    return (int16_t)r;
}

static double rms_of(const std::vector<double> &v) {
    double s = 0.0;
    for (double x : v) s += x * x;
    return std::sqrt(s / (double)v.size());
}

// Scales a float signal to the given RMS in dBFS (0 dBFS = 32768) and rounds.
static void push_scaled(std::vector<int16_t> &pcm, const std::vector<double> &v, double dbfs) {
    double want = 32768.0 * std::pow(10.0, dbfs / 20.0);
    double gain = want / rms_of(v);
    for (double x : v) pcm.push_back(clamp16(x * gain));
}

static std::vector<int16_t> make_signals() {
    const int n = kFrames * kSamples;
    std::vector<int16_t> pcm;
    std::vector<double> v(n);

    // Tone1k: 1 kHz sine, -20 dBFS RMS.
    for (int i = 0; i < n; i++) v[i] = std::sin(2.0 * kPi * 1000.0 * i / kFs);
    push_scaled(pcm, v, -20.0);

    // Tone400: 400 Hz sine, -20 dBFS RMS.
    for (int i = 0; i < n; i++) v[i] = std::sin(2.0 * kPi * 400.0 * i / kFs);
    push_scaled(pcm, v, -20.0);

    // Vowel: a 125 Hz pulse train through formant resonators at 700, 1220
    // and 2600 Hz (bandwidths 80, 100, 120 Hz), -20 dBFS RMS.
    {
        for (int i = 0; i < n; i++) v[i] = (i % 64 == 0) ? 1.0 : 0.0;
        const double f[3] = {700.0, 1220.0, 2600.0};
        const double bw[3] = {80.0, 100.0, 120.0};
        for (int k = 0; k < 3; k++) {
            double r = std::exp(-kPi * bw[k] / kFs);
            double a1 = 2.0 * r * std::cos(2.0 * kPi * f[k] / kFs);
            double a2 = -r * r;
            double y1 = 0.0, y2 = 0.0;
            for (int i = 0; i < n; i++) {
                double y = v[i] + a1 * y1 + a2 * y2;
                y2 = y1;
                y1 = y;
                v[i] = y;
            }
        }
        push_scaled(pcm, v, -20.0);
    }

    // Noise: a 32-bit LCG (x = x * 1664525 + 1013904223), seed 1, uniform,
    // -30 dBFS RMS.
    {
        uint32_t x = 1;
        for (int i = 0; i < n; i++) v[i] = (double)(int32_t)lcg_next(x) / 2147483648.0;
        push_scaled(pcm, v, -30.0);
    }

    // Silence.
    for (int i = 0; i < n; i++) pcm.push_back(0);

    // Chirp: 200 -> 3400 Hz linear over the 2 s, -20 dBFS RMS.
    {
        const double t_end = n / kFs;
        const double k = (3400.0 - 200.0) / t_end;
        for (int i = 0; i < n; i++) {
            double t = i / kFs;
            v[i] = std::sin(2.0 * kPi * (200.0 * t + 0.5 * k * t * t));
        }
        push_scaled(pcm, v, -20.0);
    }

    // FullScale: a 1 kHz square wave at +/-32767.
    for (int i = 0; i < n; i++) pcm.push_back((i % 8) < 4 ? 32767 : -32767);

    return pcm;
}

// MMDVMHost's 88-bit form: u0..u3 at 12 bits, u4..u6 at 11 bits, u7 at 7
// bits, each field most significant bit first.
static const int kWidths[8] = {12, 12, 12, 12, 11, 11, 11, 7};

static void pack(const int16_t u[8], uint8_t out[kBytes]) {
    memset(out, 0, kBytes);
    int bit = 0;
    for (int f = 0; f < 8; f++) {
        for (int b = kWidths[f] - 1; b >= 0; b--, bit++) {
            if ((u[f] >> b) & 1) out[bit / 8] |= (uint8_t)(0x80 >> (bit % 8));
        }
    }
}

static void unpack(const uint8_t in[kBytes], int16_t u[8]) {
    int bit = 0;
    for (int f = 0; f < 8; f++) {
        int v = 0;
        for (int b = 0; b < kWidths[f]; b++, bit++) {
            v = (v << 1) | ((in[bit / 8] >> (7 - bit % 8)) & 1);
        }
        u[f] = (int16_t)v;
    }
}

static void put_u32(FILE *f, uint32_t v) {
    uint8_t b[4] = {(uint8_t)v, (uint8_t)(v >> 8), (uint8_t)(v >> 16), (uint8_t)(v >> 24)};
    fwrite(b, 1, 4, f);
}

static void put_pcm(FILE *f, const int16_t *s) {
    for (int i = 0; i < kSamples; i++) {
        uint16_t v = (uint16_t)s[i];
        uint8_t b[2] = {(uint8_t)v, (uint8_t)(v >> 8)};
        fwrite(b, 1, 2, f);
    }
}

static FILE *open_or_die(const char *path, const char *mode) {
    FILE *f = fopen(path, mode);
    if (!f) {
        perror(path);
        exit(1);
    }
    return f;
}

static int do_encode(const char *path) {
    std::vector<int16_t> pcm = make_signals();
    const uint32_t records = (uint32_t)(pcm.size() / kSamples);
    imbe_vocoder enc;
    FILE *f = open_or_die(path, "wb");
    fwrite("IMBEGLD1", 1, 8, f);
    put_u32(f, records);
    for (uint32_t r = 0; r < records; r++) {
        int16_t snd[kSamples];
        memcpy(snd, &pcm[r * kSamples], sizeof(snd));
        put_pcm(f, snd);
        int16_t u[8];
        enc.imbe_encode(u, snd);
        uint8_t bytes[kBytes];
        pack(u, bytes);
        fwrite(bytes, 1, kBytes, f);
    }
    fclose(f);
    return 0;
}

static int do_decode(const char *enc_path, const char *path) {
    std::vector<std::vector<uint8_t>> frames;
    FILE *in = open_or_die(enc_path, "rb");
    char magic[8];
    uint8_t cnt[4];
    if (fread(magic, 1, 8, in) != 8 || memcmp(magic, "IMBEGLD1", 8) != 0 || fread(cnt, 1, 4, in) != 4) {
        fprintf(stderr, "bad encode.bin\n");
        return 1;
    }
    uint32_t n = cnt[0] | (cnt[1] << 8) | (cnt[2] << 16) | ((uint32_t)cnt[3] << 24);
    for (uint32_t r = 0; r < n; r++) {
        uint8_t skip[kSamples * 2];
        std::vector<uint8_t> fr(kBytes);
        if (fread(skip, 1, sizeof(skip), in) != sizeof(skip) || fread(fr.data(), 1, kBytes, in) != (size_t)kBytes) {
            fprintf(stderr, "short encode.bin\n");
            return 1;
        }
        frames.push_back(fr);
    }
    fclose(in);

    // 50 frames of LCG bytes (seed 2), one byte per step, from the top byte.
    uint32_t x = 2;
    for (int r = 0; r < 50; r++) {
        std::vector<uint8_t> fr(kBytes);
        for (int i = 0; i < kBytes; i++) fr[i] = (uint8_t)(lcg_next(x) >> 24);
        frames.push_back(fr);
    }
    const uint8_t null_frame[kBytes] = {0x04, 0x0C, 0xFD, 0x7B, 0xFB, 0x7D, 0xF2, 0x7B, 0x3D, 0x9E, 0x45};
    for (int r = 0; r < 10; r++) frames.push_back(std::vector<uint8_t>(null_frame, null_frame + kBytes));
    for (int r = 0; r < 10; r++) frames.push_back(std::vector<uint8_t>(kBytes, 0x00));
    for (int r = 0; r < 10; r++) frames.push_back(std::vector<uint8_t>(kBytes, 0xFF));

    imbe_vocoder dec;
    FILE *f = open_or_die(path, "wb");
    fwrite("IMBEGLD1", 1, 8, f);
    put_u32(f, (uint32_t)frames.size());
    for (const std::vector<uint8_t> &fr : frames) {
        int16_t u[8];
        unpack(fr.data(), u);
        uint8_t check[kBytes];
        pack(u, check);
        if (memcmp(check, fr.data(), kBytes) != 0) {
            fprintf(stderr, "pack/unpack disagree\n");
            return 1;
        }
        int16_t snd[kSamples];
        dec.imbe_decode(u, snd);
        fwrite(fr.data(), 1, kBytes, f);
        put_pcm(f, snd);
    }
    fclose(f);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "encode") == 0) return do_encode(argv[2]);
    if (argc == 4 && strcmp(argv[1], "decode") == 0) return do_decode(argv[2], argv[3]);
    fprintf(stderr, "usage: driver encode OUT | driver decode ENCODE_BIN OUT\n");
    return 2;
}
DRIVER

CXX="${CXX:-c++}"
"$CXX" -std=c++11 -O0 -w -I"$src" -o "$work/driver" "$src"/*.cc "$work/driver.cc"

# Runs the driver with its stderr kept in a log. The vocoder prints a licence
# banner there on every run; anything else the driver says (a pack/unpack
# mismatch, a bad input file, a failed open) is shown, and on failure the
# whole log is, before the script stops.
run_driver() {
    local log="$work/driver-$1.log"
    if ! "$work/driver" "$@" 2>"$log"; then
        echo "make-golden.sh: driver $1 failed:" >&2
        cat "$log" >&2
        exit 1
    fi
    grep -v -e '^Project 25 IMBE Encoder/Decoder' -e '^Developed by Pavel Yazev' \
        -e '^Version 1.0 (c) Copyright 2009' -e '^This program comes with ABSOLUTELY NO WARRANTY' \
        -e '^This is free software, and you are welcome' -e '^under certain conditions' \
        "$log" >&2 || true
}

run_driver encode "$work/encode.bin"
run_driver decode "$work/encode.bin" "$work/decode.bin"
cp "$work/encode.bin" "$work/decode.bin" "$out/"

sha() { shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1 || sha256sum "$1" | cut -d' ' -f1; }

{
    echo "upstream: $UPSTREAM"
    echo "commit: $PIN"
    echo "directory: $SUBDIR"
    echo "compiler: $CXX -std=c++11 -O0"
    echo "compiler-version: $("$CXX" --version | head -n 1)"
    echo "date: $(date -u '+%Y-%m-%d %H:%M UTC')"
    echo "sha256 decode.bin: $(sha "$out/decode.bin")"
    echo "sha256 encode.bin: $(sha "$out/encode.bin")"
} >"$out/MANIFEST"

grep '^sha256' "$out/MANIFEST"
