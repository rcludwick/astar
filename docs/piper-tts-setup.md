# Piper TTS setup (node voice announcements)

`astar-server` can speak voice announcements (station ID, the on-join
greeting) by shelling out to [piper](https://github.com/rhasspy/piper), a small
neural text-to-speech engine. The node's `PiperEngine` invokes a single binary:

```
<binary> --output_file - --model <voice.onnx>
```

with the announcement text on **stdin** and expects a **WAV stream on stdout**.
This guide sets that up on macOS (Apple Silicon). TTS is **optional** — without
it the node still runs; text announcements simply fall back or are skipped, and
CW (Morse) ID needs no TTS at all.

## macOS (Apple Silicon)

> **Why not the prebuilt binary?** The official macOS piper release
> (`piper_macos_aarch64.tar.gz`, 2023.11.14-2) ships **without its dylibs**
> (`libespeak-ng`, `libpiper_phonemize`, `libonnxruntime` are missing — only a
> `.dSYM` is present), so the binary fails with `Library not loaded:
> @rpath/libespeak-ng.1.dylib`. Use the self-contained `piper-tts` Python
> package instead, behind a tiny wrapper.

### 1. Install the engine (venv)

```sh
python3 -m venv ~/.local/share/piper-tts/venv
~/.local/share/piper-tts/venv/bin/pip install --upgrade pip
~/.local/share/piper-tts/venv/bin/pip install piper-tts
```

### 2. Install the wrapper

The node calls one executable and expects WAV on stdout. But `piper-tts`
interprets `--output_file -` as a file literally named `-` (it does **not**
stream to stdout). The wrapper strips the caller's output-file flag, renders to a
temp WAV, and cats it to stdout. Save as `~/.local/bin/piper` and `chmod +x`:

```bash
#!/bin/bash
PY="$HOME/.local/share/piper-tts/venv/bin/python"
tmp="$(mktemp -t piper).wav"
trap 'rm -f "$tmp"' EXIT
args=()
skip=false
for a in "$@"; do
  if $skip; then skip=false; continue; fi
  case "$a" in
    -f|--output_file|--output-file) skip=true; continue ;;  # drop flag + its value
    *) args+=("$a") ;;
  esac
done
"$PY" -m piper "${args[@]}" -f "$tmp" || exit 1
cat "$tmp"
```

### 3. Download a voice

Voices live at [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices).
Each voice is an `.onnx` model plus its `.onnx.json` config — download **both**.
Example: British English **Cori**, medium quality:

```sh
V=~/.local/share/piper/voices; mkdir -p "$V"
BASE=https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/cori/medium
curl -fsSL "$BASE/en_GB-cori-medium.onnx"      -o "$V/en_GB-cori-medium.onnx"
curl -fsSL "$BASE/en_GB-cori-medium.onnx.json" -o "$V/en_GB-cori-medium.onnx.json"
```

Swap `en/en_GB/cori/medium` + the filenames for any other voice/quality
(`x_low`, `low`, `medium`, `high`). Higher quality = larger model + more CPU.

### 4. Verify

```sh
echo "Connected to node 7 7 7 7 7" | ~/.local/bin/piper \
  --output_file - --model ~/.local/share/piper/voices/en_GB-cori-medium.onnx \
  > /tmp/test.wav
afplay /tmp/test.wav     # should speak "...seven seven seven seven seven"
```

### 5. Point the node at it

In `node.toml`:

```toml
[announce]
enabled = true
id_mode = "tts"          # spoken via piper (vs "cw" Morse)

[announce.tts]
binary     = "/Users/<you>/.local/bin/piper"
voice      = "/Users/<you>/.local/share/piper/voices/en_GB-cori-medium.onnx"
timeout_ms = 5000
```

Then restart the daemon. `binary`/`voice` must be absolute paths (the node
spawns the binary directly, independent of `PATH`).

## Digit-by-digit numbers

Piper normalizes `77777` to the cardinal "sixty-nine thousand…". For node
numbers and call signs you want digit-by-digit. The node expands its
`{server-node-number}` announcement token to space-separated digits
(`77777` → `7 7 7 7 7`), which piper reads as "seven seven seven seven seven". If you
author custom announcement text, space the digits yourself for the same effect.

## Linux

The official prebuilt binary generally works on Linux — extract
`piper_linux_<arch>.tar.gz`, point `[announce.tts].binary` at the extracted
`piper`, and no wrapper is needed (it streams WAV to stdout for `--output_file -`
correctly). The venv approach above also works and is a fine fallback.

## Troubleshooting

- **`Library not loaded: @rpath/...`** — the broken macOS prebuilt binary; use the venv + wrapper above.
- **Empty / 0-byte audio on macOS** — you pointed the node at raw `piper-tts` instead of the wrapper; `--output_file -` wrote to a file named `-`. Use the wrapper.
- **Number read as a cardinal** — space the digits (see above).
- **No sound but no error** — confirm `[announce].enabled = true` and `id_mode = "tts"`, and that `voice` points at the `.onnx` (with its `.onnx.json` beside it).
